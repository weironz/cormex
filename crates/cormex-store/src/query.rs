//! The read path.
//!
//! Every piece of SQL here is a literal (sqlx 0.9's `SqlSafeStr` rejects
//! concatenated query strings at the type level), and column names are written
//! out one by one rather than using `SELECT *` — `tsv` is a purely derived
//! column with no Rust type and no reason to be read back, and should not eat
//! bandwidth for nothing.

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::model::{
    Blob, BlobTranscript, CanonicalEntity, Entity, EntityMerge, Episode, EpisodeAttachment,
    EpisodeToolCall, Fact, FactEvent, FactStatus, InjectedMemory, Redaction, RedactionTarget,
    Summary, SummaryScope, fact_columns,
};
use crate::recall::MAX_LIMIT;
use crate::store::Store;

/// The pagination cursor for session messages: `(occurred_at, id)`.
///
/// # Why `occurred_at` alone will not do
///
/// `occurred_at` comes from the client's clock, and two rows landing in the
/// same microsecond is routine (one tool call writing a user row and a tool row
/// back to back is enough). A single-column cursor either skips or repeats the
/// rows sharing that instant — and both symptoms show up in the UI merely as
/// "the history looks a bit off", with nobody suspecting pagination.
///
/// `id` is a ULID (`COLLATE "C"`, byte-comparable), serving as a deterministic
/// tiebreaker within the same instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeCursor {
    pub occurred_at: DateTime<Utc>,
    pub id: String,
}

impl Store {
    // ── L0: episodes ───────────────────────────────────────

    pub async fn episode(&self, id: &str) -> Result<Option<Episode>> {
        let row = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// The messages within one session, ascending by occurrence time (uses
    /// `idx_episodes_sess`).
    ///
    /// **Only suitable where the row count is known to be bounded** (tests,
    /// exporting a small session). The session-detail path always goes through
    /// [`Self::episodes_by_session_page`] — on a long session, ascending +
    /// LIMIT truncates the **newest** messages, so the user sees a conversation
    /// with no ending and cannot tell it was truncated.
    pub async fn episodes_by_session(&self, session_id: &str, limit: i64) -> Result<Vec<Episode>> {
        let rows = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes WHERE session_id = $1
              ORDER BY occurred_at ASC, id ASC
              LIMIT $2",
        )
        .bind(session_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// One page of session messages, **newest to oldest** (uses
    /// `idx_episodes_sess_desc`).
    ///
    /// When `before` is `None` this returns the newest page; otherwise the page
    /// strictly earlier than that cursor. The returned order is **descending**
    /// (newest first) — the natural direction for pagination; a caller that
    /// wants to render in forward order reverses it itself (which is what
    /// cormexd does; see the comments over there for why).
    ///
    /// # Why the cursor spans two columns
    ///
    /// See [`EpisodeCursor`]. The comparison here is written as
    /// `occurred_at < $2 OR (occurred_at = $2 AND id < $3)` rather than the row
    /// comparison `(occurred_at, id) < ($2, $3)`: the latter requires the
    /// collation on both sides to be determinable, and `id` is a domain with
    /// `COLLATE "C"` while the bind parameter carries the default collation, so
    /// on some Postgres versions the comparison is rejected outright. Splitting
    /// it up leaves the collation determined by the column side alone.
    pub async fn episodes_by_session_page(
        &self,
        session_id: &str,
        limit: i64,
        before: Option<&EpisodeCursor>,
    ) -> Result<Vec<Episode>> {
        let rows = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes
              WHERE session_id = $1
                AND ($2::timestamptz IS NULL
                     OR occurred_at < $2::timestamptz
                     OR (occurred_at = $2::timestamptz AND id < $3))
              ORDER BY occurred_at DESC, id DESC
              LIMIT $4",
        )
        .bind(session_id)
        .bind(before.map(|c| c.occurred_at))
        .bind(before.map(|c| c.id.as_str()))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The temporal recency retrieval channel: the most recent N (uses
    /// `idx_episodes_time`).
    pub async fn recent_episodes(&self, limit: i64) -> Result<Vec<Episode>> {
        let rows = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes ORDER BY occurred_at DESC, id DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Look back over a half-open interval of event time, returned ascending.
    pub async fn episodes_in_range(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Episode>> {
        let rows = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes
              WHERE occurred_at >= $1 AND occurred_at < $2
              ORDER BY occurred_at ASC, id ASC
              LIMIT $3",
        )
        .bind(from)
        .bind(to)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── L0: blobs ──────────────────────────────────────────

    pub async fn blob(&self, hash: &str) -> Result<Option<Blob>> {
        let row = sqlx::query_as::<_, Blob>(
            "SELECT hash, mime, size_bytes, storage_key, created_at FROM blobs WHERE hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// One message's attachments, **along with their content metadata** (mime /
    /// size).
    ///
    /// An inner join on `blobs` rather than a left join:
    /// `episode_blobs.blob_hash` has a foreign key onto `blobs.hash`, so a
    /// missing row is impossible; writing it as a LEFT JOIN would only make
    /// callers write a branch for an `Option` that can never occur.
    pub async fn episode_attachments(&self, episode_id: &str) -> Result<Vec<EpisodeAttachment>> {
        let rows = sqlx::query_as::<_, EpisodeAttachment>(
            "SELECT eb.episode_id, eb.blob_hash, eb.kind, eb.filename, b.mime, b.size_bytes
               FROM episode_blobs eb
               JOIN blobs b ON b.hash = eb.blob_hash
              WHERE eb.episode_id = $1 ORDER BY eb.blob_hash ASC",
        )
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Whether the blob is still referenced by another episode — a question
    /// that must be asked before purging.
    pub async fn blob_reference_count(&self, blob_hash: &str) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM episode_blobs WHERE blob_hash = $1")
                .bind(blob_hash)
                .fetch_one(self.pool())
                .await?;
        Ok(count)
    }

    /// Media transcripts, ascending by segment offset (images / OCR have a NULL
    /// offset and sort last).
    pub async fn blob_transcripts(&self, blob_hash: &str) -> Result<Vec<BlobTranscript>> {
        let rows = sqlx::query_as::<_, BlobTranscript>(
            "SELECT id, blob_hash, kind, text, embedding, span_start_ms, span_end_ms,
                    transcribed_by, embedding_model, created_at
               FROM blob_transcripts WHERE blob_hash = $1
              ORDER BY span_start_ms ASC NULLS LAST, id ASC",
        )
        .bind(blob_hash)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Blobs awaiting transcription: MIME prefix matches and no
    /// `blob_transcripts` row exists yet.
    ///
    /// On the presigned direct-upload path the server never handles the bytes,
    /// so there is no way to trigger transcription at upload time — large files
    /// become pure archive that retrieval can never hit. A background rescanner
    /// treats the `blobs` table as the authoritative inventory and picks this
    /// batch back up to run them.
    ///
    /// # Why already-redacted blobs must be excluded in SQL
    ///
    /// Without the exclusion, the rescanner would **periodically** pull
    /// redacted blobs out of object storage into memory and feed them to the
    /// vision model — and even though the pipeline's own
    /// [`crate::Store::is_redacted`] recheck eventually discards the result, the
    /// secret has by then left the process once and gone over the network once.
    /// Redact's promise of destruction does not survive that.
    ///
    /// `mime_prefixes` are SQL `LIKE` patterns (e.g. `image/%`). They are only a
    /// **coarse filter**; whether a blob is actually claimed is decided by the
    /// transcription pipeline — the coarse filter exists so that the whole table
    /// is not pulled into memory.
    pub async fn blobs_missing_transcript(
        &self,
        mime_prefixes: &[String],
        limit: i64,
    ) -> Result<Vec<Blob>> {
        let rows = sqlx::query_as::<_, Blob>(
            "SELECT b.hash, b.mime, b.size_bytes, b.storage_key, b.created_at
               FROM blobs b
              WHERE b.mime LIKE ANY($1)
                AND NOT EXISTS (
                    SELECT 1 FROM blob_transcripts t WHERE t.blob_hash = b.hash)
                AND NOT EXISTS (
                    SELECT 1 FROM redactions r
                     WHERE r.target_kind = 'blob' AND r.target_id = b.hash)
              ORDER BY b.created_at ASC, b.hash ASC
              LIMIT $2",
        )
        .bind(mime_prefixes)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── the replay drawer ──────────────────────────────────

    /// The memories injected on one conversational turn, in injection order.
    ///
    /// The table holds only `fact_id`; `statement` is the **present state**
    /// joined in here — so once a fact is redacted the drawer follows suit and
    /// shows a placeholder, with no need to sweep this table too. The price is
    /// losing verbatim fidelity; what it buys is that redact does not have to
    /// erase the same secret in several places.
    ///
    /// Invalidated facts are **still returned**, merely carrying
    /// `invalidated = true`: "the thing this was based on no longer holds" is
    /// exactly what an audit most wants to see, and hiding it would be
    /// falsifying the replay.
    pub async fn injected_memories(&self, episode_id: &str) -> Result<Vec<InjectedMemory>> {
        let rows = sqlx::query_as::<_, InjectedMemory>(
            "SELECT m.episode_id, m.fact_id, m.ordinal, m.channels, m.score,
                    f.statement, f.domain, f.source_episode_id,
                    coalesce(s.op = 'invalidate', false) AS invalidated
               FROM episode_memories m
               LEFT JOIN facts f       ON f.id = m.fact_id
               LEFT JOIN fact_status s ON s.fact_id = m.fact_id
              WHERE m.episode_id = $1
              ORDER BY m.ordinal ASC",
        )
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The bulk version. Session detail has to hang drawers off a whole page of
    /// messages at once, and querying one at a time is N+1.
    pub async fn injected_memories_bulk(
        &self,
        episode_ids: &[String],
    ) -> Result<Vec<InjectedMemory>> {
        if episode_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, InjectedMemory>(
            "SELECT m.episode_id, m.fact_id, m.ordinal, m.channels, m.score,
                    f.statement, f.domain, f.source_episode_id,
                    coalesce(s.op = 'invalidate', false) AS invalidated
               FROM episode_memories m
               LEFT JOIN facts f       ON f.id = m.fact_id
               LEFT JOIN fact_status s ON s.fact_id = m.fact_id
              WHERE m.episode_id = ANY($1)
              ORDER BY m.episode_id ASC, m.ordinal ASC",
        )
        .bind(episode_ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The tool calls of one conversational turn, in call order.
    pub async fn episode_tool_calls(&self, episode_id: &str) -> Result<Vec<EpisodeToolCall>> {
        let rows = sqlx::query_as::<_, EpisodeToolCall>(
            "SELECT id, episode_id, ordinal, name, path, summary, ok, device_id, diff, created_at
               FROM episode_tool_calls WHERE episode_id = $1 ORDER BY ordinal ASC",
        )
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The bulk version, for the same reason as
    /// [`Self::injected_memories_bulk`].
    pub async fn episode_tool_calls_bulk(
        &self,
        episode_ids: &[String],
    ) -> Result<Vec<EpisodeToolCall>> {
        if episode_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, EpisodeToolCall>(
            "SELECT id, episode_id, ordinal, name, path, summary, ok, device_id, diff, created_at
               FROM episode_tool_calls WHERE episode_id = ANY($1)
              ORDER BY episode_id ASC, ordinal ASC",
        )
        .bind(episode_ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The most recent tool calls (across episodes, newest first).
    ///
    /// For trajectory extraction to judge "the same command keeps recurring =
    /// this is a convention of this project", it needs a view beyond the
    /// current turn, and that criterion simply cannot be computed within one
    /// turn.
    ///
    /// **Descending by `id` rather than `created_at`**: `id` is a ULID, so
    /// lexicographic order is time order, and it is the primary key with an
    /// index by nature; `created_at` has no index at all, so sorting by it on
    /// this append-only table means a full table scan plus a sort.
    pub async fn recent_tool_calls(&self, limit: i64) -> Result<Vec<EpisodeToolCall>> {
        let rows = sqlx::query_as::<_, EpisodeToolCall>(
            "SELECT id, episode_id, ordinal, name, path, summary, ok, device_id, diff, created_at
               FROM episode_tool_calls ORDER BY id DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── L1: entities ───────────────────────────────────────

    pub async fn entity(&self, id: &str) -> Result<Option<Entity>> {
        let row = sqlx::query_as::<_, Entity>(
            "SELECT id, kind, name, summary, embedding, embedding_model, device_id, created_at
               FROM entities WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// The entity resolution hot path: exact match on (kind, name) (uses
    /// `idx_entities_kn`).
    pub async fn entities_named(&self, kind: &str, name: &str) -> Result<Vec<Entity>> {
        let rows = sqlx::query_as::<_, Entity>(
            "SELECT id, kind, name, summary, embedding, embedding_model, device_id, created_at
               FROM entities WHERE kind = $1 AND name = $2 ORDER BY created_at ASC, id ASC",
        )
        .bind(kind)
        .bind(name)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The final owner reached by walking the alias merge chain to its end. A
    /// chained merge A→B→C resolves to A→C.
    ///
    /// Use it for cycle detection before writing `entity_merges`: if the
    /// target's owner resolves back to the source being merged, this edge would
    /// form a cycle and must be rejected.
    pub async fn canonical_entity(&self, id: &str) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT canonical FROM canonical_entities WHERE id = $1")
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(canonical,)| canonical))
    }

    /// Resolve owners in bulk.
    pub async fn canonical_entities(&self, ids: &[String]) -> Result<Vec<CanonicalEntity>> {
        let rows = sqlx::query_as::<_, CanonicalEntity>(
            "SELECT id, canonical FROM canonical_entities WHERE id = ANY($1) ORDER BY id ASC",
        )
        .bind(ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// An entity's outgoing edge (at most one — `UNIQUE(from_entity)`).
    pub async fn entity_merge_from(&self, from_entity: &str) -> Result<Option<EntityMerge>> {
        let row = sqlx::query_as::<_, EntityMerge>(
            "SELECT id, from_entity, into_entity, reason, source_episode_id, device_id, created_at
               FROM entity_merges WHERE from_entity = $1",
        )
        .bind(from_entity)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    // ── L1: facts ──────────────────────────────────────────

    /// Fetch a fact by primary key, **without filtering out invalidated ones**
    /// (the audit view has to be able to see what was deleted).
    pub async fn fact(&self, id: &str) -> Result<Option<Fact>> {
        let row = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM facts WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// The currently valid facts — the entry point for everyday retrieval.
    pub async fn active_facts_by_subject(&self, subject_id: &str, limit: i64) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts WHERE subject_id = $1
              ORDER BY created_at DESC, id DESC LIMIT $2"
        ))
        .bind(subject_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The contradiction detection hot path: the valid facts that currently
    /// exist under one (subject, predicate) pair (uses `idx_facts_sp`).
    pub async fn active_facts_by_subject_predicate(
        &self,
        subject_id: &str,
        predicate: &str,
    ) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts WHERE subject_id = $1 AND predicate = $2
              ORDER BY created_at DESC, id DESC"
        ))
        .bind(subject_id)
        .bind(predicate)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The reverse hop of graph traversal: valid facts pointing at this entity
    /// (uses `idx_facts_object`).
    pub async fn active_facts_by_object(
        &self,
        object_entity_id: &str,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts WHERE object_entity_id = $1
              ORDER BY created_at DESC, id DESC LIMIT $2"
        ))
        .bind(object_entity_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Redact cascade: locate derived facts by their provenance (uses
    /// `idx_facts_source`).
    pub async fn facts_by_source_episode(&self, episode_id: &str) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM facts WHERE source_episode_id = $1 ORDER BY created_at ASC, id ASC"
        ))
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// A fact's most recent **status** event (`flag` is an annotation and takes
    /// no part in the decision).
    /// Returning `None` means it has never been invalidated.
    pub async fn fact_status(&self, fact_id: &str) -> Result<Option<FactStatus>> {
        let row = sqlx::query_as::<_, FactStatus>(
            "SELECT fact_id, op, kind, invalid_at, superseded_by, actor, decided_at
               FROM fact_status WHERE fact_id = $1",
        )
        .bind(fact_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// A fact's complete lifecycle timeline, including `flag`. The UI's "how
    /// did this change over time".
    pub async fn fact_events(&self, fact_id: &str) -> Result<Vec<FactEvent>> {
        let rows = sqlx::query_as::<_, FactEvent>(
            "SELECT id, fact_id, op, kind, invalid_at, superseded_by, actor, reason,
                    source_episode_id, device_id, created_at
               FROM fact_events WHERE fact_id = $1 ORDER BY created_at ASC, id ASC",
        )
        .bind(fact_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── L2: summaries ──────────────────────────────────────

    pub async fn summary(&self, id: &str) -> Result<Option<Summary>> {
        let row = sqlx::query_as::<_, Summary>(
            "SELECT id, scope, scope_key, text, embedding, embedding_model,
                    covers_from, covers_to, device_id, created_at
               FROM summaries WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    pub async fn summaries_for_scope(
        &self,
        scope: SummaryScope,
        scope_key: &str,
        limit: i64,
    ) -> Result<Vec<Summary>> {
        let rows = sqlx::query_as::<_, Summary>(
            "SELECT id, scope, scope_key, text, embedding, embedding_model,
                    covers_from, covers_to, device_id, created_at
               FROM summaries WHERE scope = $1 AND scope_key = $2
              ORDER BY created_at DESC, id DESC LIMIT $3",
        )
        .bind(scope)
        .bind(scope_key)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── redaction tombstones ───────────────────────────────

    pub async fn redactions_for_target(
        &self,
        target_kind: RedactionTarget,
        target_id: &str,
    ) -> Result<Vec<Redaction>> {
        let rows = sqlx::query_as::<_, Redaction>(
            "SELECT id, target_kind, target_id, mode, reason, actor, device_id, created_at
               FROM redactions WHERE target_kind = $1 AND target_id = $2
              ORDER BY created_at ASC, id ASC",
        )
        .bind(target_kind)
        .bind(target_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// In-flight task protection: the extraction / transcription pipeline must
    /// ask this **before** writing any derived row, or an async task that was
    /// already in flight when redact ran will write the erased content straight
    /// back (docs/memory.md §11).
    pub async fn is_redacted(&self, target_kind: RedactionTarget, target_id: &str) -> Result<bool> {
        let (exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM redactions WHERE target_kind = $1 AND target_id = $2)",
        )
        .bind(target_kind)
        .bind(target_id)
        .fetch_one(self.pool())
        .await?;
        Ok(exists)
    }

    // ─────── switching embedding models: census and backlog ───────

    /// Which embedding models exist in the database, and how many **valid**
    /// facts each covers.
    ///
    /// # Why this is needed
    ///
    /// After a model switch the old vectors are still there, but all four
    /// vector queries filter by `embedding_model` (see
    /// [`Store::recall_vector`]), so the old facts **drop out of vector
    /// retrieval that same day**. Meanwhile BM25 / graph / recency carry on as
    /// usual, so it does not look broken, only like "semantic retrieval seems a
    /// bit worse lately" — and nobody was told.
    ///
    /// This query is the basis for that warning cormexd prints at startup. **It
    /// only counts; it does not judge**: which model is the current one is up
    /// to the caller.
    ///
    /// What it counts is the **union** of the `facts` column and the
    /// `fact_embeddings` side table: one fact counts once under each of two
    /// models, which is exactly what we want — "how many rows does model M
    /// cover" is asking precisely that.
    ///
    /// Descending by coverage count, so the largest tier comes first (the
    /// warning only has room for a few lines).
    pub async fn embedding_census(&self) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT embedding_model, count(DISTINCT fact_id) AS n
               FROM (
                        SELECT id AS fact_id, embedding_model
                          FROM active_facts
                         WHERE embedding IS NOT NULL
                        UNION ALL
                        SELECT fe.fact_id, fe.embedding_model
                          FROM fact_embeddings fe
                          JOIN active_facts f ON f.id = fe.fact_id
                    ) t
              GROUP BY embedding_model
              ORDER BY n DESC, embedding_model ASC",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// How many valid facts the current model covers, and how many it does not.
    ///
    /// # Why you cannot just subtract using [`Store::embedding_census`]
    ///
    /// During a backfill, one fact has vectors under two models **at the same
    /// time**, so the census counts it twice. A stale count computed as "total
    /// − current model's count" comes out too small, and it is too small by
    /// exactly the number of rows already backfilled — meaning the further the
    /// backfill runs, the closer the number gets to 0, so it looks "nearly
    /// done" while bearing no relation to actual progress.
    ///
    /// The `covered` predicate is **word-for-word identical** to the union in
    /// [`Store::recall_vector`]: the column is non-null and under the current
    /// model, **or** the side table has a row for the current model. The
    /// symptom of the two predicates drifting apart is `/health` reporting
    /// "fully covered" while retrieval still comes back empty.
    pub async fn embedding_coverage(&self, model: &str) -> Result<(i64, i64)> {
        let (covered, stale): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE ok), count(*) FILTER (WHERE NOT ok)
               FROM (
                 SELECT (f.embedding IS NOT NULL AND f.embedding_model = $1)
                        OR EXISTS (
                             SELECT 1 FROM fact_embeddings fe
                              WHERE fe.fact_id = f.id AND fe.embedding_model = $1
                           ) AS ok
                   FROM active_facts f
               ) t",
        )
        .bind(model)
        .fetch_one(self.pool())
        .await?;
        Ok((covered, stale))
    }

    /// The **valid** facts that have no vector in `model`'s space yet, for the
    /// backfiller to take a batch of.
    ///
    /// Ordered by `created_at DESC`: **newest first**. Recent memories are far
    /// more likely to be retrieved, so starting from the oldest means that
    /// halfway through the backfill, the very ones the user is most likely to
    /// ask about are still missing.
    ///
    /// Returns `(id, statement)` rather than a whole [`Fact`] row: the backfill
    /// only needs the text, and hauling back a 1024-dimensional vector and the
    /// tsv alongside it is wasted bandwidth — and this query runs for many
    /// rounds.
    ///
    /// # The predicate is "has no usable vector", not "has a different model
    /// name"
    ///
    /// Writing it as `embedding_model <> $1` misses a category: rows whose
    /// model name **is** the current one but whose `embedding` is NULL
    /// (vectorization failed at write time). Such facts likewise cannot enter
    /// vector retrieval, and the backfiller would never pick them up — they do
    /// not exist in the backlog, yet they count toward the stale figure in
    /// [`Store::embedding_coverage`], so the number on `/health` would **stick
    /// at a value that never goes down**, looking as though the backfill were
    /// broken. Both places must use the same predicate.
    pub async fn facts_missing_embedding(
        &self,
        model: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT f.id, f.statement
               FROM active_facts f
              WHERE NOT (f.embedding IS NOT NULL AND f.embedding_model = $1)
                AND NOT EXISTS (
                        SELECT 1 FROM fact_embeddings fe
                         WHERE fe.fact_id = f.id AND fe.embedding_model = $1
                    )
              ORDER BY f.created_at DESC, f.id DESC
              LIMIT $2",
        )
        .bind(model)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The same, on the entity side. Entity vectors take no part in four-way
    /// retrieval and are used only for **entity resolution during
    /// extraction** — but the way they fail is more insidious: a failed
    /// resolution creates a second entity for the same person, that person's
    /// facts are split across two nodes from then on, and graph traversal can
    /// no longer get through.
    pub async fn entities_missing_embedding(
        &self,
        model: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT e.id, e.name
               FROM entities e
              WHERE NOT (e.embedding IS NOT NULL AND e.embedding_model = $1)
                -- Entities that never had a vector are not backfilled: entity
                -- vectors are for resolution during extraction, and never
                -- having had one means that path was never taken back then, so
                -- adding one now has no corresponding semantics
                AND e.embedding IS NOT NULL
                AND NOT EXISTS (
                        SELECT 1 FROM entity_embeddings ee
                         WHERE ee.entity_id = e.id AND ee.embedding_model = $1
                    )
              ORDER BY e.created_at DESC, e.id DESC
              LIMIT $2",
        )
        .bind(model)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }
}

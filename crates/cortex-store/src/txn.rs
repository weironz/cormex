//! Write transactions: turning the write discipline of docs/memory.md §9 into a
//! **raw transaction nobody can get their hands on**.
//!
//! ## The discipline
//!
//! 1. The transaction opens with `SELECT pg_advisory_xact_lock(4272)`, which
//!    serializes concurrent write transactions and guarantees that the order of
//!    `sync_log.seq` == the order in which rows become visible to readers.
//!    A bare `BIGSERIAL` cannot do this: the sequence value is allocated at
//!    INSERT time, but rows become visible in commit order — T1 takes 100 and
//!    has not committed, T2 takes 101 and commits first, the client advances
//!    its cursor past 101, and the 100 that T1 later commits is permanently
//!    invisible. Silent data loss, unrecoverable short of a full resync.
//! 2. For every business row written, append one `sync_log` row in the same
//!    transaction.
//! 3. Sequence-allocating transactions must be **short and write-only**: LLM
//!    calls and embedding computation always happen outside the transaction.
//!
//! ## How the discipline is enforced
//!
//! Rule 2 does not rely on callers remembering it. The raw
//! [`sqlx::Transaction`] is sealed inside the private module `guarded`, its
//! field invisible to the rest of this file, so the **only** path to the
//! database is `Guarded::insert_row` — and that binds "write the business row"
//! and "append to sync_log" into a single indivisible action. Whoever adds a
//! new write method to [`WriteTxn`] in the future cannot write code that skips
//! the append, even if they have no idea sync_log exists.
//!
//! Rule 3 is structural in the same way: [`WriteTxn`] **offers no read methods
//! at all**. Flows that need to read before writing (contradiction resolution,
//! entity resolution, merge-cycle detection) should do their reading and
//! computing outside the transaction, then enter it to write.

use sqlx::{PgPool, Postgres, postgres::PgArguments, query::Query};

use cortex_core::Id;

use crate::error::{Result, StoreError};
use crate::model::{
    DerivedKind, NewBlob, NewBlobTranscript, NewEntity, NewEntityEmbedding, NewEntityMerge,
    NewEpisode, NewEpisodeBlob, NewEpisodeMemory, NewEpisodeToolCall, NewFact, NewFactEmbedding,
    NewFactEvent, NewProjectEvent, NewRedaction, NewSessionEvent, NewSummary, ProvenanceRef, table,
};

/// The advisory lock key for sync sequence allocation.
///
/// The value 4272 means nothing in particular; it only has to be unique
/// database-wide and not collide with anyone else's. It matches the header
/// comment in `migrations/20260807000001_init.sql`.
///
/// Since multi-tenancy it serves as the **classid of the two-argument form**,
/// with the second argument derived from the tenant schema (see
/// `SchemaName::lock_key`). That is why the type is `i32` and not `i64`: the
/// two-argument overload of `pg_advisory_xact_lock` takes two int4s.
pub const SYNC_ADVISORY_LOCK_KEY: i32 = 4272;

mod guarded {
    use sqlx::{PgPool, Postgres, Transaction, postgres::PgArguments, query::Query};

    use super::SYNC_ADVISORY_LOCK_KEY;
    use crate::error::Result;

    /// The guard around the raw transaction. Its field is private to this
    /// module — this is the enforcement mechanism for the discipline.
    pub(super) struct Guarded {
        tx: Transaction<'static, Postgres>,
    }

    impl Guarded {
        /// Begin the transaction and take the advisory lock **immediately**.
        ///
        /// The lock has to be the first statement in the transaction: it must
        /// precede any `nextval` for "allocation order == commit order" to
        /// hold.
        pub(super) async fn begin(pool: &PgPool, tenant_key: i32) -> Result<Self> {
            let mut tx = pool.begin().await?;
            // The **two-argument form**, with the second derived from the
            // tenant schema.
            //
            // This used to be a single-argument global key, which serialized
            // the write transactions of **every tenant** — one person importing
            // three years of history and nobody else can write a single
            // sentence. What this lock guarantees is "the order of
            // `sync_log.seq` == the order rows become visible to readers", and
            // that is a per-schema matter to begin with.
            //
            // The classid is still SYNC_ADVISORY_LOCK_KEY, so that in
            // `pg_locks` this is recognizable at a glance as the sync
            // sequence-allocation lock and not something else
            sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
                .bind(SYNC_ADVISORY_LOCK_KEY)
                .bind(tenant_key)
                .execute(&mut *tx)
                .await?;
            Ok(Self { tx })
        }

        /// The one and only business write channel: run the business INSERT and
        /// append to sync_log within the same transaction.
        ///
        /// Returns the row's `seq` in the total sync order.
        pub(super) async fn insert_row<'q>(
            &mut self,
            table: &'static str,
            record_id: &str,
            stmt: Query<'q, Postgres, PgArguments>,
        ) -> Result<i64> {
            stmt.execute(&mut *self.tx).await?;

            let (seq,): (i64,) = sqlx::query_as(
                "INSERT INTO sync_log (table_name, record_id) VALUES ($1, $2) RETURNING seq",
            )
            .bind(table)
            .bind(record_id)
            .fetch_one(&mut *self.tx)
            .await?;

            Ok(seq)
        }

        /// The channel reserved for **derived rows**: run the INSERT and write
        /// **no sync_log entry**.
        ///
        /// # Only one class of thing earns this path
        ///
        /// Vectors backfilled when the embedding model changes
        /// (`fact_embeddings` / `entity_embeddings`). They satisfy all three of
        /// the following, and missing any one disqualifies a table:
        ///
        /// 1. **Purely derived**: uniquely determined by `statement` + the
        ///    model, and recomputable if lost
        /// 2. **The client cannot receive them anyway**: `sync_payload.rs`
        ///    states outright that "tsv and embedding are server-derived
        ///    columns and do not enter the sync payload" — routing them through
        ///    sync_log would only push out a pile of rows whose payload carries
        ///    no useful field at all
        /// 3. **The volume is database-wide**: one backfill over a hundred
        ///    thousand facts is a hundred thousand sync_log rows. That is
        ///    exactly what the comment in `sync_payload.rs` means by "changes
        ///    wholesale when the embedding model changes, **blowing up
        ///    incremental sync for nothing**"
        ///
        /// # Think it through before adding a new table here
        ///
        /// Business rows go through [`Self::insert_row`]. Putting a business
        /// row on this path means **other devices will never see it**, with no
        /// symptom whatsoever — which is precisely why the sync_log enforcement
        /// mechanism exists. When in doubt use `insert_row`: one sync_log row
        /// too many is visible waste, one too few is invisible loss.
        pub(super) async fn insert_derived_row<'q>(
            &mut self,
            stmt: Query<'q, Postgres, PgArguments>,
        ) -> Result<()> {
            stmt.execute(&mut *self.tx).await?;
            Ok(())
        }

        pub(super) async fn commit(self) -> Result<()> {
            self.tx.commit().await?;
            Ok(())
        }

        pub(super) async fn rollback(self) -> Result<()> {
            self.tx.rollback().await?;
            Ok(())
        }
    }
}

/// A write transaction. It can only reach a caller via
/// [`crate::Store::write_txn`].
///
/// Every write method returns the row's `seq` in the total sync order
/// (`sync_log.seq`) — the per-record ack for upstream ingest uses it directly.
pub struct WriteTxn {
    inner: guarded::Guarded,
}

impl WriteTxn {
    pub(crate) async fn begin(pool: &PgPool, tenant_key: i32) -> Result<Self> {
        Ok(Self {
            inner: guarded::Guarded::begin(pool, tenant_key).await?,
        })
    }

    pub(crate) async fn commit(self) -> Result<()> {
        self.inner.commit().await
    }

    pub(crate) async fn rollback(self) -> Result<()> {
        self.inner.rollback().await
    }

    async fn insert_row<'q>(
        &mut self,
        table: &'static str,
        record_id: &str,
        stmt: Query<'q, Postgres, PgArguments>,
    ) -> Result<i64> {
        self.inner.insert_row(table, record_id, stmt).await
    }

    // ── L0 raw layer ───────────────────────────────────────

    /// Persist one raw message. `tsv_source` is written in the same transaction
    /// as the main row, never backfilled asynchronously.
    pub async fn insert_episode(&mut self, new: &NewEpisode) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO episodes
                 (id, session_id, role, content, text, tsv, domain, device_id, occurred_at)
             VALUES ($1, $2, $3, $4, $5, to_tsvector('simple', $6), $7, $8, $9)",
        )
        .bind(&id)
        .bind(&new.session_id)
        .bind(new.role)
        .bind(sqlx::types::Json(&new.content))
        .bind(&new.text)
        .bind(&new.tsv_source)
        .bind(&new.domain)
        .bind(&new.device_id)
        .bind(new.occurred_at);

        self.insert_row(table::EPISODES, &id, stmt).await
    }

    /// Register a content-addressed binary object.
    ///
    /// Step two of a fixed three-step order: upload to object storage → the
    /// `blobs` row → the episode plus its link rows.
    pub async fn insert_blob(&mut self, new: &NewBlob) -> Result<i64> {
        let stmt = sqlx::query(
            "INSERT INTO blobs (hash, mime, size_bytes, storage_key) VALUES ($1, $2, $3, $4)",
        )
        .bind(&new.hash)
        .bind(&new.mime)
        .bind(new.size_bytes)
        .bind(&new.storage_key);

        self.insert_row(table::BLOBS, &new.hash, stmt).await
    }

    /// Attach a blob to an episode. `sync_log.record_id` uses the composite key
    /// `episode_id:blob_hash`.
    ///
    /// `filename` lives here rather than in `blobs`: under content addressing
    /// the same bytes can carry several filenames, and putting it in blobs
    /// would mean "whoever uploads first names it". See
    /// `migrations/20260807000004_attachment_filename.sql`.
    pub async fn link_episode_blob(&mut self, new: &NewEpisodeBlob) -> Result<i64> {
        let record_id = new.record_id();
        let stmt = sqlx::query(
            "INSERT INTO episode_blobs (episode_id, blob_hash, kind, filename)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(new.episode_id.to_string())
        .bind(&new.blob_hash)
        .bind(&new.kind)
        .bind(&new.filename);

        self.insert_row(table::EPISODE_BLOBS, &record_id, stmt)
            .await
    }

    // ── the replay drawer ──────────────────────────────────

    /// Record which memory was injected on this turn.
    ///
    /// Only the `fact_id` is stored, never a snapshot of the text as it was
    /// injected — text inside a snapshot cannot be wiped, which would turn
    /// redact's promise of destruction into a lie (see the table header of
    /// migration 20260807000005).
    pub async fn insert_episode_memory(&mut self, new: &NewEpisodeMemory) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO episode_memories
                 (id, episode_id, fact_id, ordinal, channels, score, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(new.episode_id.to_string())
        .bind(new.fact_id.to_string())
        .bind(new.ordinal)
        .bind(&new.channels)
        .bind(new.score)
        .bind(&new.device_id);

        self.insert_row(table::EPISODE_MEMORIES, &id, stmt).await
    }

    /// Record one tool call from this turn.
    ///
    /// `path` gets its own column instead of being stuffed into `summary`:
    /// summary is natural language written for humans and its wording can
    /// change at any time, so once a client starts regexing the path out of it,
    /// a single rewording silently shows the wrong file.
    pub async fn insert_episode_tool_call(&mut self, new: &NewEpisodeToolCall) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO episode_tool_calls
                 (id, episode_id, ordinal, name, path, summary, ok, device_id, diff)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&id)
        .bind(new.episode_id.to_string())
        .bind(new.ordinal)
        .bind(&new.name)
        .bind(&new.path)
        .bind(&new.summary)
        .bind(new.ok)
        .bind(&new.device_id)
        .bind(&new.diff);

        self.insert_row(table::EPISODE_TOOL_CALLS, &id, stmt).await
    }

    /// Write a media transcript.
    ///
    /// The caller must have checked [`crate::Store::is_redacted`] before
    /// writing — this keeps an async task that was already in flight when
    /// redact ran from backfilling erased content after the fact
    /// (docs/memory.md §11).
    pub async fn insert_blob_transcript(&mut self, new: &NewBlobTranscript) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO blob_transcripts
                 (id, blob_hash, kind, text, tsv, embedding,
                  span_start_ms, span_end_ms, transcribed_by, embedding_model)
             VALUES ($1, $2, $3, $4, to_tsvector('simple', $5), $6, $7, $8, $9, $10)",
        )
        .bind(&id)
        .bind(&new.blob_hash)
        .bind(new.kind)
        .bind(&new.text)
        .bind(&new.tsv_source)
        .bind(&new.embedding)
        .bind(new.span_start_ms)
        .bind(new.span_end_ms)
        .bind(&new.transcribed_by)
        .bind(&new.embedding_model);

        self.insert_row(table::BLOB_TRANSCRIPTS, &id, stmt).await
    }

    // ── L1 fact layer ──────────────────────────────────────

    pub async fn insert_entity(&mut self, new: &NewEntity) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO entities (id, kind, name, summary, embedding, embedding_model, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(&new.kind)
        .bind(&new.name)
        .bind(&new.summary)
        .bind(&new.embedding)
        .bind(&new.embedding_model)
        .bind(&new.device_id);

        self.insert_row(table::ENTITIES, &id, stmt).await
    }

    /// Append one alias merge.
    ///
    /// `UNIQUE(from_entity)` makes concurrent forks first-writer-wins at the
    /// physical level: whoever arrives second gets a unique-constraint conflict
    /// (`sqlx::Error::Database`, SQLSTATE 23505), on which the caller turns the
    /// operation into a no-op and acks the client.
    ///
    /// Cycles (A→B together with B→A) are not caught by the database; the
    /// caller must detect them **outside the transaction** by walking the
    /// `into_entity` chain to its end (`canonical_entity` exists for exactly
    /// this).
    pub async fn insert_entity_merge(&mut self, new: &NewEntityMerge) -> Result<i64> {
        let id = new.id.to_string();
        let source_episode_id = new.source_episode_id.map(|v| v.to_string());
        let stmt = sqlx::query(
            "INSERT INTO entity_merges
                 (id, from_entity, into_entity, reason, source_episode_id, device_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(new.from_entity.to_string())
        .bind(new.into_entity.to_string())
        .bind(&new.reason)
        .bind(&source_episode_id)
        .bind(&new.device_id);

        self.insert_row(table::ENTITY_MERGES, &id, stmt).await
    }

    /// Persist one fact.
    ///
    /// `source` also produces `trust_tier`: the correspondence between the two
    /// columns is nailed down by the schema's
    /// `facts_trust_tier_matches_channel`, and all we do here is compute it —
    /// letting callers fill in both columns themselves would sooner or later
    /// produce self-contradictory rows like "channel says web page, tier says
    /// the user said it in person", and such a row only surfaces during an
    /// actual security incident, when you need to invalidate in bulk by source.
    ///
    /// Every source in `derived_from` gets one `derivations` row appended in
    /// the **same transaction** (each with its own sync_log entry). For
    /// single-source derivation leave it empty; `source_episode_id` is its
    /// provenance.
    pub async fn insert_fact(&mut self, new: &NewFact) -> Result<i64> {
        let id = new.id.to_string();
        let object_entity_id = new.object_entity_id.map(|v| v.to_string());
        let stmt = sqlx::query(
            "INSERT INTO facts
                 (id, subject_id, predicate, object_text, object_entity_id, statement, tsv,
                  embedding, embedding_model, domain, confidence, valid_at,
                  source_episode_id, source_channel, trust_tier, extracted_by, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, to_tsvector('simple', $7),
                     $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(&id)
        .bind(new.subject_id.to_string())
        .bind(&new.predicate)
        .bind(&new.object_text)
        .bind(&object_entity_id)
        .bind(&new.statement)
        .bind(&new.tsv_source)
        .bind(&new.embedding)
        .bind(&new.embedding_model)
        .bind(&new.domain)
        .bind(new.confidence)
        .bind(new.valid_at)
        .bind(new.source_episode_id.to_string())
        .bind(new.source.channel())
        .bind(new.source.trust_tier())
        .bind(&new.extracted_by)
        .bind(&new.device_id);

        let seq = self.insert_row(table::FACTS, &id, stmt).await?;

        for source in &new.derived_from {
            self.insert_derivation(DerivedKind::Fact, new.id, *source, &new.device_id)
                .await?;
        }

        Ok(seq)
    }

    /// Add one vector to an existing fact when switching models.
    ///
    /// # Why not `UPDATE facts SET embedding = …`
    ///
    /// Two reasons; the second is the hard one:
    ///
    /// 1. CLAUDE.md: "no table does UPDATE / DELETE". This crate contains zero
    ///    UPDATEs to date, and the existing precedent is `blob_transcripts` —
    ///    derived data gets new rows in a separate table
    /// 2. "Dual-model retrieval during backfill" (memory.md §7, step 3)
    ///    requires one fact to hold vectors from two spaces **at the same
    ///    time**. One column cannot hold two
    ///
    /// # Idempotency
    ///
    /// `ON CONFLICT DO NOTHING`: a process restart, two backfill passes hitting
    /// the same row, or two devices backfilling simultaneously all end up here.
    /// Let the unique constraint be the backstop rather than making the
    /// backfiller keep its own books — bookkeeping either misses rows or
    /// duplicates them, and when it goes wrong it raises no error.
    ///
    /// **Does not enter sync_log** (it goes through `insert_derived_row`). For
    /// the reasoning see the comment on `table::FACT_EMBEDDINGS` in `model.rs`.
    pub async fn insert_fact_embedding(&mut self, new: &NewFactEmbedding) -> Result<()> {
        let stmt = sqlx::query(
            "INSERT INTO fact_embeddings (fact_id, embedding_model, embedding)
             VALUES ($1, $2, $3)
             ON CONFLICT (fact_id, embedding_model) DO NOTHING",
        )
        .bind(new.fact_id.to_string())
        .bind(&new.embedding_model)
        .bind(&new.embedding);

        self.inner.insert_derived_row(stmt).await
    }

    /// The same, on the entity side.
    ///
    /// Entity vectors take no part in four-way retrieval; they are used only
    /// for **entity resolution during extraction**. The way they fail is more
    /// insidious than for facts: a failed resolution creates a second entity
    /// for the same person, that person's facts are split across two nodes from
    /// then on, and graph traversal can no longer get through — while all of
    /// this shows up in the UI merely as "it seems to have forgotten some
    /// things I told it".
    pub async fn insert_entity_embedding(&mut self, new: &NewEntityEmbedding) -> Result<()> {
        let stmt = sqlx::query(
            "INSERT INTO entity_embeddings (entity_id, embedding_model, embedding)
             VALUES ($1, $2, $3)
             ON CONFLICT (entity_id, embedding_model) DO NOTHING",
        )
        .bind(new.entity_id.to_string())
        .bind(&new.embedding_model)
        .bind(&new.embedding);

        self.inner.insert_derived_row(stmt).await
    }

    /// Append one lifecycle event (invalidate / restore / mark).
    ///
    /// Invalidation is expressed by appending rather than by an in-place UPDATE
    /// — that is the premise of both append-only storage and conflict-free
    /// multi-device operation.
    pub async fn insert_fact_event(&mut self, new: &NewFactEvent) -> Result<i64> {
        let id = new.id.to_string();
        let superseded_by = new.superseded_by.map(|v| v.to_string());
        let source_episode_id = new.source_episode_id.map(|v| v.to_string());
        let stmt = sqlx::query(
            "INSERT INTO fact_events
                 (id, fact_id, op, kind, invalid_at, superseded_by,
                  actor, reason, source_episode_id, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(&id)
        .bind(new.fact_id.to_string())
        .bind(new.op)
        .bind(new.kind)
        .bind(new.invalid_at)
        .bind(&superseded_by)
        .bind(new.actor)
        .bind(&new.reason)
        .bind(&source_episode_id)
        .bind(&new.device_id);

        self.insert_row(table::FACT_EVENTS, &id, stmt).await
    }

    // ── L2 summary layer ───────────────────────────────────

    /// Write a summary **together with its provenance**.
    ///
    /// # Why the two are bound into one method
    ///
    /// The same technique by which [`Guarded::insert_row`](guarded) welds
    /// "business row + sync_log" together. A summary has no fallback column
    /// like `source_episode_id NOT NULL`; its provenance lives entirely in
    /// `derivations`. Splitting this into two calls would mean the gap between
    /// "write the summary" and "write the edges" can be interrupted, and the
    /// symptom of an interruption is a summary whose source **can never be
    /// found** — one that keeps leaking content after its source is redacted,
    /// and that cannot be repaired after the fact (who summarized it, and from
    /// what, is known only to the step that produced it).
    ///
    /// The returned seq is the **summary row's**; its provenance edges follow
    /// immediately after with larger seqs. A client replaying in sync_log order
    /// therefore sees the summary before the edges, which is exactly the FK
    /// direction.
    ///
    /// # Errors
    /// Returns [`StoreError::MissingProvenance`] when `new.sources` is empty,
    /// and **writes no rows at all**.
    pub async fn insert_summary(&mut self, new: &NewSummary) -> Result<i64> {
        if new.sources.is_empty() {
            return Err(StoreError::MissingProvenance {
                kind: table::SUMMARIES,
                id: new.id.to_string(),
            });
        }

        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO summaries
                 (id, scope, scope_key, text, tsv, embedding, embedding_model,
                  covers_from, covers_to, device_id)
             VALUES ($1, $2, $3, $4, to_tsvector('simple', $5), $6, $7, $8, $9, $10)",
        )
        .bind(&id)
        .bind(new.scope)
        .bind(&new.scope_key)
        .bind(&new.text)
        .bind(&new.tsv_source)
        .bind(&new.embedding)
        .bind(&new.embedding_model)
        .bind(new.covers_from)
        .bind(new.covers_to)
        .bind(&new.device_id);

        let seq = self.insert_row(table::SUMMARIES, &id, stmt).await?;

        for source in &new.sources {
            self.insert_derivation(DerivedKind::Summary, new.id, *source, &new.device_id)
                .await?;
        }

        Ok(seq)
    }

    // ── derivation provenance ──────────────────────────────

    /// Append one "derived record ← source" provenance edge.
    ///
    /// Private: a provenance edge should only be written from the same method
    /// that writes the derived row it describes (see [`Self::insert_summary`]).
    /// Making it public would amount to permitting "write the derived record
    /// first, come back and add the edges later" — which is precisely the kind
    /// of repair §5.3 calls impossible after the fact.
    async fn insert_derivation(
        &mut self,
        derived_kind: DerivedKind,
        derived_id: Id,
        source: ProvenanceRef,
        device_id: &str,
    ) -> Result<i64> {
        let id = Id::new().to_string();
        let stmt = sqlx::query(
            "INSERT INTO derivations
                 (id, derived_kind, derived_id, source_kind, source_id, device_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(derived_kind)
        .bind(derived_id.to_string())
        .bind(source.kind)
        .bind(source.id.to_string())
        .bind(device_id);

        self.insert_row(table::DERIVATIONS, &id, stmt).await
    }

    // ── session lifecycle ──────────────────────────────────

    /// Append one session lifecycle event (rename / archive / bind workspace).
    ///
    /// Like [`Self::insert_fact_event`], this appends rather than modifying in
    /// place: a session's final state is determined by the last event along
    /// each dimension (the `session_state` view). This is not only about
    /// upholding append-only — it makes concurrent renames from several devices
    /// deterministic: replaying in `sync_log` total order, every device
    /// computes the same title, whereas under UPDATE semantics the later write
    /// overwrites the earlier one, each of two devices is "the later one" from
    /// its own vantage point, and which value it finally converges on comes
    /// down to network jitter.
    pub async fn insert_session_event(&mut self, new: &NewSessionEvent) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO session_events
                 (id, session_id, op, title, workspace, project_id, runtime, actor, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&id)
        .bind(&new.session_id)
        .bind(new.op)
        .bind(&new.title)
        .bind(&new.workspace)
        .bind(&new.project_id)
        .bind(new.runtime)
        .bind(new.actor)
        .bind(&new.device_id);

        self.insert_row(table::SESSION_EVENTS, &id, stmt).await
    }

    // ── project lifecycle ──────────────────────────────────

    /// Append one project lifecycle event (create / rename / delete).
    ///
    /// Same shape as [`Self::insert_session_event`]: the final state is
    /// determined by the last event of each state machine (the `project_state`
    /// view) rather than by modifying a projects table in place. Deleting a
    /// project here merely **appends a delete**, touching none of the sessions
    /// inside it — a cascading rewrite would make "undo an accidental deletion"
    /// impossible.
    pub async fn insert_project_event(&mut self, new: &NewProjectEvent) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO project_events (id, project_id, op, name, actor, device_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(&new.project_id)
        .bind(new.op)
        .bind(&new.name)
        .bind(new.actor)
        .bind(&new.device_id);

        self.insert_row(table::PROJECT_EVENTS, &id, stmt).await
    }

    // ── redaction tombstones ───────────────────────────────

    /// Write one redaction tombstone.
    ///
    /// Just the tombstone: the actual cascading purge (clearing episode bodies,
    /// the statement / tsv / embedding of derived facts, blob_transcripts, and
    /// regenerating summaries) is the redact executor's job, and it is the only
    /// place in the entire system where UPDATE is permitted. The tombstone
    /// itself ships through sync_log, and on receiving it a client must
    /// idempotently perform the equivalent local purge.
    pub async fn insert_redaction(&mut self, new: &NewRedaction) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO redactions (id, target_kind, target_id, mode, reason, actor, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(new.target_kind)
        .bind(&new.target_id)
        .bind(new.mode)
        .bind(&new.reason)
        .bind(&new.actor)
        .bind(&new.device_id);

        self.insert_row(table::REDACTIONS, &id, stmt).await
    }
}

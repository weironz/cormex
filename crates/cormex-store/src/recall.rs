//! The query side of multi-channel retrieval.
//!
//! Each channel returns candidates **already sorted by that channel's own
//! notion of relevance**, and fusion is left to the RRF in
//! `cormex-memory::fusion` — the storage layer implements no ranking policy, it
//! only pulls each channel's raw results out. Which channels are used and which
//! are not is decided by `cormex-memory::retrieval` on the basis of evaluation
//! data, not judged here.
//!
//! Everyday retrieval queries only the `active_facts` view (invalidated facts
//! should not enter everyday retrieval). The exception is the `facts_as_of*`
//! group: what they want is precisely "what I believed to be true at that
//! moment", so they must go back to `facts` + `fact_events` and replay. The
//! three replay retrievals share one snapshot predicate (`as_of_snapshot!`) and
//! differ only in **what they sort by on top of that snapshot**.

use chrono::{DateTime, Utc};
use sqlx::{FromRow as _, Row as _};

use crate::error::Result;
use crate::model::{Entity, Fact, fact_columns};
use crate::store::Store;

/// The sane range for the cap on how many rows one retrieval returns.
/// Too few leaves RRF no room to fuse; too many slows down the graph traversal
/// channel.
///
/// `pub(crate)`: the backfill-todo query used when switching models
/// (`query.rs`) shares the same cap. Copying the constant would let the two
/// slowly drift apart, and the symptom of that drift is "some path quietly
/// returns more/fewer rows than the others", with no error at all.
pub(crate) const MAX_LIMIT: i64 = 200;

/// The snapshot of "what Cormex still believed to be true at the instant `$1`".
///
/// The three replay retrievals (recency / BM25 / vector) share this one
/// predicate. It **must query `facts` and not `active_facts`**: the latter is
/// the **current** valid set, and using it for replay would prematurely erase
/// "conclusions that were only overturned later" — which is exactly what replay
/// exists to look at.
///
/// # The state machine, not "has it ever been invalidated"
///
/// `fact_events` is a state machine: `invalidate` makes a fact stop holding,
/// `revoke` withdraws that invalidation, and **the last state event wins** —
/// that is what `fact_status` (the `DISTINCT ON … ORDER BY created_at DESC,
/// id DESC` view) encodes, and what `active_facts` filters on.
///
/// The predicate used to be `NOT EXISTS (… op = 'invalidate' AND created_at <=
/// $1)`, i.e. "has it ever been invalidated by now". That is not the same
/// question, and the difference is invisible until somebody actually revokes:
/// a fact that was invalidated and later restored **never comes back** in
/// replay, at any instant, including instants after the restore. The symptom is
/// "replay says I never believed this, but I did — I even re-confirmed it",
/// with no error anywhere.
///
/// So take the last state event at or before `$1` and require it not to be an
/// `invalidate`. The ordering is the same one `fact_status` uses, `id DESC`
/// included: `created_at` is `clock_timestamp()`, and two events in the same
/// microsecond are tie-broken by the ULID so that two replicas agree.
macro_rules! as_of_snapshot {
    () => {
        concat!(
            "SELECT ",
            fact_columns!(),
            " FROM facts f
              WHERE f.created_at <= $1
                AND coalesce((
                    SELECT e.op FROM fact_events e
                     WHERE e.fact_id = f.id
                       AND e.op IN ('invalidate', 'revoke')
                       AND e.created_at <= $1
                     ORDER BY e.created_at DESC, e.id DESC
                     LIMIT 1
                ), 'revoke') <> 'invalidate'"
        )
    };
}

/// The **union** for vector retrieval: the fact's own column ∪ the
/// `fact_embeddings` side table.
///
/// # Why a union
///
/// After a model switch, "the vectors under model M" are scattered across two
/// places: the ones written with M from the start sit in `facts.embedding`,
/// while the ones the backfiller filled in later sit in `fact_embeddings`.
/// Querying only one place produces **silent partial amnesia** — the missing
/// half is still covered by BM25 and graph traversal, so it does not look
/// broken (see the comments in migrations/20260808000001).
///
/// # Why it is written as two branches, each with its own `ORDER BY … LIMIT`
///
/// So that each side gets a **chance** to use its own vector index. Written as
/// a `JOIN` followed by a single `ORDER BY`, or with `LATERAL`, the index never
/// even gets the chance — that kind of query is blazingly fast on a small
/// database and only surfaces once the database grows, at which point it looks
/// like "the database got slower" rather than "the index isn't being used".
///
/// Both sides genuinely use their index now — but branch one was **only fixed
/// later**, and that history is worth keeping:
///
/// `active_facts` used to be `facts LEFT JOIN fact_status`, and the outer join
/// has to complete before anything can be sorted by distance, so `idx_facts_vec`
/// had gone unused since day one of this project. Measured over 50,000 facts:
/// 289.7ms, 198,000 buffer hits, 43,000 distance computations over 1024
/// dimensions; switching to an anti-join brought it to 0.5ms and 531 buffer
/// hits. The view definition is in migrations/20260810000001, which has a full
/// side-by-side comparison of the three formulations.
///
/// **The lesson is that it raises no error**: the results were always entirely
/// correct, merely slow, and on a small database that is invisible. So before
/// reshaping `active_facts`, take a look at that migration — one carelessly
/// reintroduced join quietly puts this back the way it was.
///
/// # Why `$base` is written out twice instead of being factored into a CTE
///
/// `WITH base AS (…)` would look nicer, but **it defeats the HNSW index in
/// branch one**. Since PG 12 a CTE is inlined only when it is referenced
/// **once**; here each of the two branches references it once, so it gets
/// materialized into a temporary result and `ORDER BY embedding <=> $1` can
/// only sort a pile of already-materialized rows.
///
/// This is not speculation — the first version was a CTE, and `EXPLAIN` printed:
///
/// ```text
///   ->  Sort  (Sort Key: b.embedding <=> …)
///         ->  CTE Scan on base b          ← branch one, no index
///   ->  Index Scan using idx_fact_embeddings_vec on fact_embeddings  ← branch two has one
/// ```
///
/// With the CTE removed, both sides are `Index Scan`. Repeating that piece of
/// SQL is the price.
///
/// # The `DISTINCT ON` is defensive
///
/// Under normal circumstances the two sides do not collide: the backfiller only
/// picks facts whose `embedding_model <> the current one` (see
/// `Store::facts_missing_embedding`), so a fact cannot be on both sides at
/// once. But a path like "switch away and then switch back" can manufacture an
/// overlap, and the symptom of an overlap is the same memory appearing twice in
/// retrieval and RRF giving it a double ranking — a quietly amplifying bias.
macro_rules! vector_union {
    (base = $base:expr, vec = $v:literal, model = $m:literal, limit = $l:literal) => {
        concat!(
            "SELECT ",
            fact_columns!("d."),
            ", d.distance
               FROM (
                 SELECT DISTINCT ON (u.id) ",
            fact_columns!("u."),
            ", u.distance
                   FROM (
                     (SELECT ",
            fact_columns!("b."),
            ", b.embedding <=> ",
            $v,
            " AS distance
                        FROM ",
            $base,
            " b
                       WHERE b.embedding IS NOT NULL AND b.embedding_model = ",
            $m,
            "        ORDER BY b.embedding <=> ",
            $v,
            "
                       LIMIT ",
            $l,
            ")
                     UNION ALL
                     (SELECT ",
            fact_columns!("b."),
            ", fe.distance
                        FROM (SELECT fact_id, embedding <=> ",
            $v,
            " AS distance
                                FROM fact_embeddings
                               WHERE embedding_model = ",
            $m,
            "                ORDER BY embedding <=> ",
            $v,
            "
                               LIMIT ",
            $l,
            ") fe
                        JOIN ",
            $base,
            " b ON b.id = fe.fact_id)
                   ) u
                  ORDER BY u.id, u.distance
               ) d
              ORDER BY d.distance
              LIMIT ",
            $l
        )
    };
}

/// The SQL used by live vector retrieval.
///
/// It is lifted into a constant for exactly one reason: so that
/// `tests/embedding_migration.rs` can run `EXPLAIN` against **the query that
/// actually runs**. Hand-copying the SQL into the test would let it slowly
/// drift, and after drifting the test guards a query that no longer exists —
/// while still passing.
#[doc(hidden)]
pub const RECALL_VECTOR_SQL: &str = vector_union!(
    base = "active_facts",
    vec = "$1",
    model = "$2",
    limit = "$3"
);

impl Store {
    /// ① BM25 full-text retrieval — proper nouns and exact matches.
    ///
    /// `tsquery_input` is produced by
    /// `cormex-memory::tokenize::to_tsquery_input` (jieba segmentation joined
    /// with OR). This channel does the heavy lifting for Chinese retrieval:
    /// vectors perform poorly on proper nouns like "张伟", and only full text
    /// hits them exactly.
    pub async fn recall_bm25(&self, tsquery_input: &str, limit: i64) -> Result<Vec<Fact>> {
        if tsquery_input.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts
              WHERE tsv @@ to_tsquery('simple', $1)
              ORDER BY ts_rank(tsv, to_tsquery('simple', $1)) DESC, created_at DESC
              LIMIT $2"
        ))
        .bind(tsquery_input)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// ② Vector cosine retrieval — semantic proximity.
    ///
    /// Filtered by `embedding_model`: during a model switch the old and new
    /// vectors do not live in the same semantic space, so their distances are
    /// not comparable. Querying them mixed together yields a meaningless
    /// ranking (see memory.md §7).
    pub async fn recall_vector(
        &self,
        embedding: &pgvector::Vector,
        embedding_model: &str,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        // Shares the same SQL as recall_vector_scored: the extra distance
        // column that comes back is simply ignored when `FromRow for Fact`
        // picks columns by name.
        // Two hand-copied versions of the SQL would drift apart sooner or
        // later, and the symptom of that drift is "the scored path and the
        // unscored path retrieve different results" -- with no error at all
        let rows = sqlx::query_as::<_, Fact>(RECALL_VECTOR_SQL)
            .bind(embedding)
            .bind(embedding_model)
            .bind(limit.clamp(1, MAX_LIMIT))
            .fetch_all(self.pool())
            .await?;
        Ok(rows)
    }

    /// The scored version of ② — returns the **cosine distance** (not the
    /// similarity) alongside each fact.
    ///
    /// # Why the scores are pulled out at all
    ///
    /// RRF uses only ranks, which is its virtue (immunity to differing scales),
    /// but it also means the fused score **carries no absolute relevance
    /// information whatsoever**: for a question the database simply has no
    /// answer to, all four channels still return their own best few, those
    /// still corroborate each other, and the fused score is still high.
    /// Measured in evaluation: sweeping the abstention threshold from 0 to
    /// 0.045 left the false-recall rate frozen at 0.370 — that gate controls
    /// **how many memories get injected**, not **whether they are relevant**.
    ///
    /// The only signal that still carries an absolute scale is the cosine
    /// distance here: it is calibrated by the model and independent of corpus
    /// size. Similarity = 1 - distance.
    ///
    /// The unscored [`Store::recall_vector`] is kept because the retrieval path
    /// itself does not need scores, and returning an extra column would make
    /// that hot path shuttle an f64 around on every call.
    pub async fn recall_vector_scored(
        &self,
        embedding: &pgvector::Vector,
        embedding_model: &str,
        limit: i64,
    ) -> Result<Vec<(Fact, f64)>> {
        let rows = sqlx::query(RECALL_VECTOR_SQL)
            .bind(embedding)
            .bind(embedding_model)
            .bind(limit.clamp(1, MAX_LIMIT))
            .fetch_all(self.pool())
            .await?;

        rows.into_iter()
            .map(|r| {
                let distance: f64 = r.try_get("distance")?;
                Ok((Fact::from_row(&r)?, distance))
            })
            .collect()
    }

    /// ③ Graph traversal retrieval — expand 1–2 hops of related facts outward
    /// from the entities that were hit.
    ///
    /// Every row of `subject_id → object_entity_id` in the `facts` table is an
    /// edge of the graph, so this is an ordinary recursive CTE with no need for
    /// a graph database.
    ///
    /// Entities are first collapsed through `canonical_entities`, or aliases
    /// would split one entity's neighbors apart. Results are **ascending by hop
    /// count** — nearer is more relevant than farther.
    ///
    /// # Why the extra nested subquery
    ///
    /// `DISTINCT ON (f.id)` forces `ORDER BY` to lead with `f.id`, so the outer
    /// sort could only be `ORDER BY f.id, hop` — which is **sorting by fact
    /// id**, and the id is a ULID (creation order), utterly unrelated to
    /// relevance. Worse, the `LIMIT` would also truncate by id, so as the
    /// corpus grows, near-hop facts get squeezed out by older far-hop ones.
    ///
    /// So the inner level uses `DISTINCT ON` to do the "keep only the smallest
    /// hop count per fact" deduplication, and the outer level sorts and
    /// truncates by hop count. Evaluation found this bug: for one question the
    /// gold answer was the most recent fact in the entire database, yet it
    /// ranked 6th out of 7 graph retrieval results.
    pub async fn recall_graph(
        &self,
        seed_entity_ids: &[String],
        max_hops: i32,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        if seed_entity_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "WITH RECURSIVE seeds AS (
                 SELECT DISTINCT COALESCE(c.canonical, s.id) AS id
                   FROM unnest($1::text[]) AS s(id)
                   LEFT JOIN canonical_entities c ON c.id = s.id
             ),
             reach(entity_id, hop) AS (
                 SELECT id, 0 FROM seeds
               UNION
                 SELECT COALESCE(f.object_entity_id, f.subject_id), r.hop + 1
                   FROM reach r
                   JOIN active_facts f
                     ON f.subject_id = r.entity_id
                  WHERE r.hop < $2
                    AND f.object_entity_id IS NOT NULL
             ),
             nearest AS (
                 SELECT DISTINCT ON (f.id) ",
            fact_columns!("f."),
            ", r.hop
                   FROM active_facts f
                   JOIN reach r
                     ON f.subject_id = r.entity_id OR f.object_entity_id = r.entity_id
                  ORDER BY f.id, r.hop
             )
             SELECT ",
            fact_columns!(),
            "  FROM nearest
              ORDER BY hop, created_at DESC
              LIMIT $3"
        ))
        .bind(seed_entity_ids)
        .bind(max_hops.clamp(1, 3))
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// ④ Temporal recency retrieval — contextual continuity.
    ///
    /// Sorted by **system time** (`created_at`, i.e. when Cormex learned it)
    /// rather than event time: "what have we talked about lately" asks about
    /// the freshness of knowledge, not about the world's timeline.
    pub async fn recall_recent(&self, within_days: i64, limit: i64) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts
              WHERE created_at > now() - make_interval(days => $1::int)
              ORDER BY created_at DESC
              LIMIT $2"
        ))
        .bind(within_days.clamp(1, 3650) as i32)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// ⑤ Episode full-text retrieval — work backwards from the **raw
    /// conversation** to the facts extracted from it.
    ///
    /// The first four channels all look only at `facts.statement`, which is a
    /// sentence the extractor rewrote. In **provenance follow-ups** like "that
    /// thing we discussed last time" or "where did you learn that", the user is
    /// paraphrasing what was actually said at the time, and the original
    /// wording often did not make it into the statement at all — so the BM25
    /// channel finds no such term and the vector channel does not line up
    /// either. The GIN index on `episodes.tsv` had been built but never used;
    /// this channel is what finally uses it.
    ///
    /// Sorted by the episode's `ts_rank`, and within one episode by fact
    /// recency. What is returned is still a `Fact` — the injection contract
    /// recognizes facts only, and L0 raw text never enters the prompt.
    pub async fn recall_via_episodes(
        &self,
        tsquery_input: &str,
        episode_limit: i64,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        if tsquery_input.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "WITH hit AS (
                 SELECT e.id, ts_rank(e.tsv, to_tsquery('simple', $1)) AS ep_rank
                   FROM episodes e
                  WHERE e.tsv @@ to_tsquery('simple', $1)
                  ORDER BY ep_rank DESC, e.occurred_at DESC
                  LIMIT $2
             )
             SELECT ",
            fact_columns!("f."),
            "   FROM active_facts f
                JOIN hit ON hit.id = f.source_episode_id
               ORDER BY hit.ep_rank DESC, f.created_at DESC
               LIMIT $3"
        ))
        .bind(tsquery_input)
        .bind(episode_limit.clamp(1, MAX_LIMIT))
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Replay by **system time**: which facts Cormex believed to be true at the
    /// instant `as_of`.
    ///
    /// This is the core bitemporal capability, answering "what did I believe
    /// three months ago". Note that it cannot query the `active_facts` view —
    /// that is the **current** valid set; it must go back to `facts` +
    /// `fact_events` and replay along time.
    ///
    /// Sorted by `created_at` descending only — this is "the whole picture as
    /// it stood then". Replay with a query term goes through
    /// [`Store::facts_as_of_bm25`] and [`Store::facts_as_of_vector`], and the
    /// three are then RRF'd together on the `cormex-memory` side.
    pub async fn facts_as_of(&self, as_of: DateTime<Utc>, limit: i64) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            as_of_snapshot!(),
            " ORDER BY f.created_at DESC LIMIT $2"
        ))
        .bind(as_of)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// BM25 ranking on top of the replay snapshot.
    ///
    /// # Why not "fetch the whole picture and sort it in memory"
    ///
    /// At a scale of a hundred thousand rows the whole picture cannot be
    /// fetched back at all. Relevance ranking for replay must be **pushed down
    /// into SQL** so that the `idx_facts_tsv` GIN index does the work;
    /// otherwise, as the corpus grows, this path degrades from "the ordering is
    /// wrong" into "the query won't run".
    pub async fn facts_as_of_bm25(
        &self,
        as_of: DateTime<Utc>,
        tsquery_input: &str,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        if tsquery_input.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, Fact>(concat!(
            as_of_snapshot!(),
            "   AND f.tsv @@ to_tsquery('simple', $2)
             ORDER BY ts_rank(f.tsv, to_tsquery('simple', $2)) DESC, f.created_at DESC
             LIMIT $3"
        ))
        .bind(as_of)
        .bind(tsquery_input)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Vector ranking on top of the replay snapshot. Same reasoning as
    /// [`Store::facts_as_of_bm25`].
    ///
    /// Likewise filtered by `embedding_model`: distances across models are not
    /// comparable, and replay is no exception.
    pub async fn facts_as_of_vector(
        &self,
        as_of: DateTime<Utc>,
        embedding: &pgvector::Vector,
        embedding_model: &str,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        // The base set for replay is the as_of snapshot (**it must query facts
        // and not active_facts**; for why see the comments on
        // as_of_snapshot!), and the union layer's logic is identical to live
        // retrieval
        let rows = sqlx::query_as::<_, Fact>(vector_union!(
            base = concat!("(", as_of_snapshot!(), ")"),
            vec = "$2",
            model = "$3",
            limit = "$4"
        ))
        .bind(as_of)
        .bind(embedding)
        .bind(embedding_model)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Look candidate entities up from text, to seed graph traversal.
    ///
    /// Exact name match first, then falling back to prefix match — entity names
    /// are usually very short, and full-text search is actually less accurate
    /// than direct matching here.
    pub async fn entities_matching(&self, terms: &[String], limit: i64) -> Result<Vec<String>> {
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query(
            "SELECT DISTINCT COALESCE(c.canonical, e.id) AS id
               FROM entities e
               LEFT JOIN canonical_entities c ON c.id = e.id
              WHERE e.name = ANY($1)
                 OR EXISTS (
                     SELECT 1 FROM unnest($1::text[]) AS t(term)
                      WHERE e.name ILIKE t.term || '%'
                 )
              LIMIT $2",
        )
        .bind(terms)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|r| r.get::<String, _>("id")).collect())
    }
}

impl Store {
    /// Find entities by vector nearest neighbor, for **alias resolution**
    /// during extraction.
    ///
    /// What separates this from [`Store::recall_vector`] is purpose: that one
    /// is retrieval, this one decides before writing whether "cormex" and "the
    /// Cormex project" are the same thing.
    ///
    /// Returns candidates ascending by cosine distance along with their
    /// **distance** (not similarity) — the threshold decision is left to the
    /// caller, because "how close counts as the same" depends heavily on the
    /// embedding model and the storage layer should not decide it on the
    /// caller's behalf. Similarity = 1 - distance.
    ///
    /// Filtered by `kind`: a person and a project should not be merged even if
    /// they share a name. Filtered by `embedding_model`: distances across
    /// models are not comparable.
    pub async fn entities_near(
        &self,
        kind: &str,
        embedding: &pgvector::Vector,
        embedding_model: &str,
        limit: i64,
    ) -> Result<Vec<(Entity, f64)>> {
        // The same union as on the fact side: the ones written with the current
        // model sit in entities.embedding, the backfilled ones in
        // entity_embeddings. Querying only one place makes resolution silently
        // stop working after a model switch -- and the way it stops working is
        // by creating a second entity for the same person, splitting their
        // facts across two nodes from then on so that graph traversal can no
        // longer get through
        let rows = sqlx::query(
            // Three levels: the inner two branches each use their own HNSW →
            // the middle DISTINCT ON deduplicates (which forces sorting by id)
            // → **the outer level must re-sort by distance**, or the caller
            // receives results ordered by ULID, and ULIDs being ordered means
            // "ordered by creation time" -- which looks like a reasonable list,
            // only it has nothing to do with relevance
            "SELECT id, kind, name, summary, embedding, embedding_model,
                    device_id, created_at, distance
               FROM (
                 SELECT DISTINCT ON (u.id) u.*
                   FROM (
                     (SELECT id, kind, name, summary, embedding, embedding_model,
                             device_id, created_at, embedding <=> $2 AS distance
                        FROM entities
                       WHERE kind = $1
                         AND embedding IS NOT NULL
                         AND embedding_model = $3
                       ORDER BY embedding <=> $2
                       LIMIT $4)
                     UNION ALL
                     (SELECT e.id, e.kind, e.name, e.summary, e.embedding, e.embedding_model,
                             e.device_id, e.created_at, ee.distance
                        FROM (SELECT entity_id, embedding <=> $2 AS distance
                                FROM entity_embeddings
                               WHERE embedding_model = $3
                               ORDER BY embedding <=> $2
                               LIMIT $4) ee
                        JOIN entities e ON e.id = ee.entity_id
                       WHERE e.kind = $1)
                   ) u
                  ORDER BY u.id, u.distance
               ) d
              ORDER BY distance
              LIMIT $4",
        )
        .bind(kind)
        .bind(embedding)
        .bind(embedding_model)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;

        rows.into_iter()
            .map(|r| {
                let distance: f64 = r.try_get("distance")?;
                let entity = Entity::from_row(&r)?;
                Ok((entity, distance))
            })
            .collect()
    }
}

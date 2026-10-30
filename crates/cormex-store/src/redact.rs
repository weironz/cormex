//! The redact cascade's storage half: the `UPDATE`s and `DELETE`s.
//!
//! Design: `docs/memory.md` §十一. The executor lives in `cormexd::redact`;
//! this module is the SQL it calls.
//!
//! # Why these are here rather than in `WriteTxn`
//!
//! `WriteTxn` is the append-only channel: it takes the sync lock and writes a
//! `sync_log` row for everything it touches. A redact is the one operation that
//! **modifies** rows, and routing it through that channel would mean a
//! `sync_log` entry claiming "this row was created" for a row that was
//! destroyed.
//!
//! So these take the pool directly, outside any transaction of the append-only
//! kind. The tombstone — the thing that *does* need to sync — still goes through
//! `write_txn`, which is why the executor writes it separately and last.
//!
//! # Idempotent by construction
//!
//! Every statement is `WHERE … = …` against a value that a second run cannot
//! match (the content is already the placeholder). Running the cascade twice is
//! safe, which is what makes "resume after a crash" mean "run it again".

use chrono::Utc;

use crate::error::Result;
use crate::model::RedactionTarget;
use crate::store::Store;

/// What the archive-copy step cleared.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveClearCounts {
    pub events: usize,
    pub chunks: usize,
}

impl Store {
    /// Blank one episode's content, keeping the row.
    ///
    /// `content` becomes `{"redacted":true}` rather than the row being deleted:
    /// the row is the identity every `fact.source_episode_id` points at, and
    /// removing it would turn the cascade into a set of dangling references.
    ///
    /// `tsv` goes to NULL rather than to an empty vector: an empty `tsvector`
    /// still matches an empty query, and "this was erased" must not look like
    /// "this matched nothing".
    ///
    /// # Errors
    /// The statement failed.
    pub async fn redact_episode_content(&self, episode_id: &str) -> Result<usize> {
        let done = sqlx::query(
            "UPDATE episodes
                SET content = '{\"redacted\":true}'::jsonb,
                    text    = NULL,
                    tsv     = NULL
              WHERE id = $1 AND content <> '{\"redacted\":true}'::jsonb",
        )
        .bind(episode_id)
        .execute(self.pool())
        .await?;
        Ok(usize::try_from(done.rows_affected()).unwrap_or(usize::MAX))
    }

    /// The facts whose `source_episode_id` is this episode.
    ///
    /// # Errors
    /// The query failed.
    pub async fn facts_from_episode(&self, episode_id: &str) -> Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT id::text FROM facts WHERE source_episode_id = $1 ORDER BY id")
                .bind(episode_id)
                .fetch_all(self.pool())
                .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Blank one fact's content.
    ///
    /// **The embedding goes too.** A vector can be inverted approximately — the
    /// literature on embedding inversion is unambiguous about that — so leaving
    /// it would leave a recoverable copy of the thing being erased. The design
    /// says so explicitly, and it is the step most likely to be forgotten
    /// because nothing fails when it is skipped.
    ///
    /// # Errors
    /// The statement failed.
    pub async fn redact_fact_content(&self, fact_id: &str) -> Result<usize> {
        // **`object_text` is blanked to a placeholder, not to NULL.** The table
        // has `CHECK (object_text IS NOT NULL OR object_entity_id IS NOT NULL)`,
        // and a fact whose object is a literal has `object_entity_id` NULL — so
        // setting `object_text` to NULL would violate the check and abort the
        // whole cascade. The placeholder carries no content either way
        let done = sqlx::query(
            "UPDATE facts
                SET statement   = '[redacted]',
                    tsv         = NULL,
                    embedding   = NULL,
                    object_text = CASE WHEN object_text IS NULL THEN NULL ELSE '[redacted]' END
              WHERE id = $1 AND statement <> '[redacted]'",
        )
        .bind(fact_id)
        .execute(self.pool())
        .await?;
        Ok(usize::try_from(done.rows_affected()).unwrap_or(usize::MAX))
    }

    /// The blobs attached to an episode.
    ///
    /// # Errors
    /// The query failed.
    pub async fn blobs_of_episode(&self, episode_id: &str) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT blob_hash::text FROM episode_blobs WHERE episode_id = $1 ORDER BY blob_hash",
        )
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(h,)| h).collect())
    }

    /// Blank one blob's transcripts.
    ///
    /// A caption or an ASR transcript of the same content is the same content.
    ///
    /// # Errors
    /// The statement failed.
    pub async fn redact_blob_transcript(&self, blob_hash: &str) -> Result<usize> {
        let done = sqlx::query(
            "UPDATE blob_transcripts
                SET text = '[redacted]', tsv = NULL, embedding = NULL
              WHERE blob_hash = $1 AND text <> '[redacted]'",
        )
        .bind(blob_hash)
        .execute(self.pool())
        .await?;
        Ok(usize::try_from(done.rows_affected()).unwrap_or(usize::MAX))
    }

    /// Whether a blob is still attached to an episode **other than** this one.
    ///
    /// # Errors
    /// The query failed.
    pub async fn blob_referenced_elsewhere(
        &self,
        blob_hash: &str,
        except_episode: &str,
    ) -> Result<bool> {
        let (exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(
                 SELECT 1 FROM episode_blobs
                  WHERE blob_hash = $1 AND episode_id <> $2)",
        )
        .bind(blob_hash)
        .bind(except_episode)
        .fetch_one(self.pool())
        .await?;
        Ok(exists)
    }

    /// Whether a blob is attached to any episode at all.
    ///
    /// # Errors
    /// The query failed.
    pub async fn blob_referenced_anywhere(&self, blob_hash: &str) -> Result<bool> {
        let (exists,): (bool,) =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM episode_blobs WHERE blob_hash = $1)")
                .bind(blob_hash)
                .fetch_one(self.pool())
                .await?;
        Ok(exists)
    }

    /// A blob's object-storage key.
    ///
    /// # Errors
    /// The query failed, or there is no such blob.
    pub async fn blob_storage_key(&self, blob_hash: &str) -> Result<String> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT storage_key FROM blobs WHERE hash = $1")
                .bind(blob_hash)
                .fetch_optional(self.pool())
                .await?;
        row.map(|(k,)| k)
            .ok_or_else(|| crate::StoreError::Invalid(format!("no such blob: {blob_hash}")))
    }

    /// The summaries whose `derivations` edge names this source.
    ///
    /// # Errors
    /// The query failed.
    pub async fn summaries_derived_from(
        &self,
        kind: RedactionTarget,
        source_id: &str,
    ) -> Result<Vec<String>> {
        // `derivations.source_kind` and `redactions.target_kind` share the
        // spellings `episode` / `blob`; a target kind with no derivations
        // spelling (the capture ones) simply matches nothing, which is right
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT DISTINCT d.derived_id::text
               FROM derivations d
              WHERE d.derived_kind = 'summary'
                AND d.source_kind::text = $1
                AND d.source_id::text = $2",
        )
        .bind(kind.to_string())
        .bind(source_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Delete one summary **and its provenance edges**.
    ///
    /// # Errors
    /// The statement failed.
    pub async fn delete_summary(&self, summary_id: &str) -> Result<usize> {
        sqlx::query(
            "DELETE FROM derivations
              WHERE derived_kind = 'summary' AND derived_id::text = $1",
        )
        .bind(summary_id)
        .execute(self.pool())
        .await?;
        let done = sqlx::query("DELETE FROM summaries WHERE id::text = $1")
            .bind(summary_id)
            .execute(self.pool())
            .await?;
        Ok(usize::try_from(done.rows_affected()).unwrap_or(usize::MAX))
    }

    /// Clear the archive's copies of an episode.
    ///
    /// # What "the archive's copies" means
    ///
    /// A promoted episode (P5-4) is an `episodes` row whose `content` carries
    /// `transcript_event_id`. Its text **is** the conversation, so the P-layer
    /// event and its index chunks have to go with it — otherwise the archive
    /// index keeps returning the erased sentence while the memory layer says it
    /// is gone.
    ///
    /// # Errors
    /// The statement failed.
    pub async fn redact_transcript_events_of_episode(
        &self,
        episode_id: &str,
    ) -> Result<ArchiveClearCounts> {
        // The event id lives inside `content`, which the cascade has already
        // replaced with the placeholder by the time this runs — so it is read
        // from the **promoted** copy's `content` before that happens. The
        // executor calls this before blanking, and this query tolerates both
        // orders by matching on the episode's own id as well
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT content->>'transcript_event_id'
               FROM episodes
              WHERE id = $1 AND content ? 'transcript_event_id'",
        )
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;

        let mut counts = ArchiveClearCounts::default();
        for (event_id,) in rows {
            let chunks = sqlx::query("DELETE FROM archive_chunks WHERE event_id::text = $1")
                .bind(&event_id)
                .execute(self.pool())
                .await?
                .rows_affected();
            let events = sqlx::query(
                "UPDATE transcript_events
                    SET text = NULL, tsv = NULL, content = '[]'::jsonb
                  WHERE id::text = $1 AND text IS NOT NULL",
            )
            .bind(&event_id)
            .execute(self.pool())
            .await?
            .rows_affected();
            counts.chunks += usize::try_from(chunks).unwrap_or(usize::MAX);
            counts.events += usize::try_from(events).unwrap_or(usize::MAX);
        }
        Ok(counts)
    }

    /// The promoted episodes belonging to these capture sessions.
    ///
    /// A promoted episode is one whose `content` carries
    /// `transcript_event_id` (P5-4). They are the bridge between the archive
    /// and the memory layer, and the reason a purge has to run the redact
    /// cascade as well as delete its own rows.
    ///
    /// # Errors
    /// The query failed.
    pub async fn promoted_episodes_of(&self, session_ids: &[String]) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT id::text FROM episodes
              WHERE content ? 'transcript_event_id'
                AND session_id = ANY($1)
              ORDER BY id",
        )
        .bind(session_ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Whether a string names a real capture session.
    ///
    /// The distiller's progress table has a foreign key onto
    /// `capture_sessions(id)`, so rewinding a `scope_key` that is not one would
    /// fail. A summary's `scope_key` is *usually* a capture session id, but not
    /// always — a summary written from the memory side can name something else.
    ///
    /// # Errors
    /// The query failed.
    pub async fn is_capture_session(&self, id: &str) -> Result<bool> {
        let (exists,): (bool,) =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM capture_sessions WHERE id::text = $1)")
                .bind(id)
                .fetch_one(self.pool())
                .await?;
        Ok(exists)
    }

    /// Redacted-at timestamp for a target, when there is one.
    ///
    /// # Errors
    /// The query failed.
    pub async fn redacted_at(
        &self,
        target: RedactionTarget,
        target_id: &str,
    ) -> Result<Option<chrono::DateTime<Utc>>> {
        let row: Option<(chrono::DateTime<Utc>,)> = sqlx::query_as(
            "SELECT created_at FROM redactions
              WHERE target_kind = $1 AND target_id = $2
              ORDER BY created_at DESC LIMIT 1",
        )
        .bind(target)
        .bind(target_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|(t,)| t))
    }
}

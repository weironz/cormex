//! Read-only lookups over the archive tables.
//!
//! These are the reads the capture API needs **before** it enters a write
//! transaction: which file is this, which generation is current, which chunks
//! cover this byte range. They deliberately live outside [`super::ArchiveTxn`]
//! for the same reason [`crate::txn`] has no read methods — a transaction that
//! holds a lock stays short and write-only.

use crate::archive::model::{
    CaptureChunk, CaptureFile, CaptureMask, Generation, Projection, TranscriptEvent,
};
use crate::error::Result;
use crate::store::Store;

impl Store {
    /// One logical file, by the key that identifies it.
    pub async fn capture_file(
        &self,
        device_id: &str,
        agent: &str,
        logical_path: &str,
    ) -> Result<Option<CaptureFile>> {
        let row = sqlx::query_as::<_, CaptureFile>(
            "SELECT id, session_id, device_id, agent, logical_path, role, framing,
                    cwd, project_key, first_seen_at
               FROM capture_files
              WHERE device_id = $1 AND agent = $2 AND logical_path = $3",
        )
        .bind(device_id)
        .bind(agent)
        .bind(logical_path)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// Generations that have bytes the masker has not scanned yet.
    ///
    /// The todo is derived from the data rather than kept in a queue: a
    /// generation is done when its end is at or below the scanner's progress,
    /// so a restarted process loses nothing and there is no job table to fall
    /// out of step with the rows.
    pub async fn generations_needing_masks(
        &self,
        scanner: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT g.id, coalesce(max(c.byte_offset + c.byte_length), 0) AS end
               FROM capture_generations g
               JOIN capture_chunks c ON c.generation_id = g.id
               LEFT JOIN LATERAL (
                    SELECT p.through_offset
                      FROM capture_mask_progress p
                     WHERE p.generation_id = g.id AND p.scanner = $1
                     ORDER BY p.created_at DESC, p.id DESC
                     LIMIT 1
               ) prog ON true
              GROUP BY g.id, prog.through_offset
             HAVING coalesce(max(c.byte_offset + c.byte_length), 0) > coalesce(prog.through_offset, 0)
              ORDER BY g.id
              LIMIT $2",
        )
        .bind(scanner)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Sessions that have bytes the projector has not projected yet.
    ///
    /// Returns `(session_id, agent)` — the agent picks the parser.
    pub async fn sessions_needing_projection(
        &self,
        parser_version_filter: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        // A session is done when every one of its generations is fully
        // projected. `through` is a JSON map from generation id to a **record**
        // — `{offset, parser}` — not a bare number: the offset alone cannot
        // resume a parser, because the parser holds a line buffer and feeding
        // it a chunk that starts mid-file fails its contiguity check.
        //
        // `-> g.id ->> 'offset'` reaches the number; `coalesce(…, 0)` treats
        // "never projected" as 0.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT s.id, s.agent
               FROM capture_sessions s
               JOIN capture_files f ON f.session_id = s.id
               JOIN capture_generations g ON g.file_id = f.id
               JOIN capture_chunks c ON c.generation_id = g.id
               LEFT JOIN LATERAL (
                    SELECT p.through
                      FROM transcript_projections p
                     WHERE p.session_id = s.id AND p.parser_version = $1
                     ORDER BY p.created_at DESC, p.id DESC
                     LIMIT 1
               ) prog ON true
              GROUP BY s.id, s.agent, g.id, prog.through
             HAVING coalesce(max(c.byte_offset + c.byte_length), 0)
                    > coalesce(
                        ((prog.through -> g.id::text) ->> 'offset')::bigint,
                        0
                      )
              ORDER BY s.id
              LIMIT $2",
        )
        .bind(parser_version_filter)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Every file of a session.
    ///
    /// The projector needs all of them: a session's conversation may be spread
    /// across the main transcript, a sub-agent's own file, and the host's
    /// memory pseudo-session.
    pub async fn capture_files_of_session(&self, session_id: &str) -> Result<Vec<CaptureFile>> {
        let rows = sqlx::query_as::<_, CaptureFile>(
            "SELECT id, session_id, device_id, agent, logical_path, role, framing,
                    cwd, project_key, first_seen_at
               FROM capture_files WHERE session_id = $1
              ORDER BY id",
        )
        .bind(session_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// One capture file by its id.
    ///
    /// Used by the capture API to answer "does this file belong to the device
    /// asking" before it writes anything into it.
    pub async fn capture_file_by_id(&self, file_id: &str) -> Result<Option<CaptureFile>> {
        let row = sqlx::query_as::<_, CaptureFile>(
            "SELECT id, session_id, device_id, agent, logical_path, role, framing,
                    cwd, project_key, first_seen_at
               FROM capture_files WHERE id = $1",
        )
        .bind(file_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// One generation by its id.
    pub async fn generation_by_id(&self, generation_id: &str) -> Result<Option<Generation>> {
        let row = sqlx::query_as::<_, Generation>(
            "SELECT id, file_id, ordinal, reason, created_at
               FROM capture_generations WHERE id = $1",
        )
        .bind(generation_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// The newest generation of a file, and how far it currently extends.
    ///
    /// `None` means the file has no generation at all, which the registration
    /// path guarantees cannot happen — but the signature must not claim what it
    /// cannot know.
    pub async fn latest_generation(&self, file_id: &str) -> Result<Option<(Generation, i64)>> {
        let row = sqlx::query_as::<_, Generation>(
            "SELECT id, file_id, ordinal, reason, created_at
               FROM capture_generations WHERE file_id = $1
              ORDER BY ordinal DESC LIMIT 1",
        )
        .bind(file_id)
        .fetch_optional(self.pool())
        .await?;
        let Some(generation) = row else {
            return Ok(None);
        };
        let end = self.generation_end(&generation.id).await?;
        Ok(Some((generation, end)))
    }

    /// How many logical bytes a generation holds.
    ///
    /// `max(byte_offset + byte_length)` rather than `sum(byte_length)`: the
    /// contiguity invariant makes them equal, and the max is the one that still
    /// gives a sane answer if a gap ever appears — a sum would silently report
    /// a total that no offset can be appended at.
    pub async fn generation_end(&self, generation_id: &str) -> Result<i64> {
        let (end,): (i64,) = sqlx::query_as(
            "SELECT coalesce(max(byte_offset + byte_length), 0)
               FROM capture_chunks WHERE generation_id = $1",
        )
        .bind(generation_id)
        .fetch_one(self.pool())
        .await?;
        Ok(end)
    }

    /// One page of a session's transcript events, ascending by `seq`.
    ///
    /// `after_seq` is exclusive: pass the last `seq` of the previous page. The
    /// cursor is `seq` rather than an offset because `seq` is the projection's
    /// own order, and it is unique per `(session, parser_version)` — an offset
    /// cursor would skip or repeat rows when two events share a timestamp.
    pub async fn transcript_page(
        &self,
        session_id: &str,
        parser_version: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<TranscriptEvent>> {
        let rows = sqlx::query_as::<_, TranscriptEvent>(
            "SELECT id, session_id, parser_version, seq, kind, occurred_at, content,
                    text, tool_name, tool_call_id,
                    src_generation_id, src_offset, src_length, created_at
               FROM transcript_events
              WHERE session_id = $1 AND parser_version = $2 AND seq > $3
              ORDER BY seq ASC
              LIMIT $4",
        )
        .bind(session_id)
        .bind(parser_version)
        .bind(after_seq)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The newest projection for a session, or `None` if it has never been
    /// projected.
    pub async fn latest_projection(
        &self,
        session_id: &str,
        parser_version: &str,
    ) -> Result<Option<Projection>> {
        let row = sqlx::query_as::<_, Projection>(
            "SELECT id, session_id, parser_version, through, next_seq, status, error, created_at
               FROM transcript_projections
              WHERE session_id = $1 AND parser_version = $2
              ORDER BY created_at DESC, id DESC
              LIMIT 1",
        )
        .bind(session_id)
        .bind(parser_version)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// How far the masker has scanned a generation.
    ///
    /// `0` when it has never run: the scanner starts at the beginning, which is
    /// the right answer for "nothing has been masked yet".
    pub async fn mask_progress(&self, generation_id: &str, scanner: &str) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT through_offset FROM capture_mask_progress
              WHERE generation_id = $1 AND scanner = $2
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(generation_id)
        .bind(scanner)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map_or(0, |(n,)| n))
    }

    /// The mask intervals overlapping `[offset, offset + len)`.
    ///
    /// Used by the raw view: the bytes come from object storage unchanged, and
    /// the intervals are replaced with `[masked:<rule>]` unless the caller has
    /// `raw:read` **and** asked for `unmask=true`.
    pub async fn masks_in_range(
        &self,
        generation_id: &str,
        offset: i64,
        len: i64,
    ) -> Result<Vec<CaptureMask>> {
        let rows = sqlx::query_as::<_, CaptureMask>(
            "SELECT id, generation_id, byte_offset, byte_length, rule, scanner, created_at
               FROM capture_masks
              WHERE generation_id = $1
                AND byte_offset < $2 + $3
                AND byte_offset + byte_length > $2
              ORDER BY byte_offset ASC",
        )
        .bind(generation_id)
        .bind(offset)
        .bind(len)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The chunks covering `[offset, offset + len)`, ascending by offset.
    ///
    /// Used to serve a byte range of the original file (the raw view, and
    /// restore). It returns whole chunks, so the caller may receive bytes on
    /// either side of the range it asked for — slicing them is the caller's
    /// job, because only the caller knows whether it wants the surrounding
    /// context.
    pub async fn chunks_overlapping(
        &self,
        generation_id: &str,
        offset: i64,
        len: i64,
    ) -> Result<Vec<CaptureChunk>> {
        let rows = sqlx::query_as::<_, CaptureChunk>(
            "SELECT id, generation_id, byte_offset, byte_length, content_sha256,
                    storage_key, encoding, observed_at, created_at
               FROM capture_chunks
              WHERE generation_id = $1
                AND byte_offset < $2 + $3
                AND byte_offset + byte_length > $2
              ORDER BY byte_offset ASC",
        )
        .bind(generation_id)
        .bind(offset)
        .bind(len)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }
}

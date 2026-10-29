//! Read-only lookups over the archive tables.
//!
//! These are the reads the capture API needs **before** it enters a write
//! transaction: which file is this, which generation is current, which chunks
//! cover this byte range. They deliberately live outside [`super::ArchiveTxn`]
//! for the same reason [`crate::txn`] has no read methods — a transaction that
//! holds a lock stays short and write-only.

use crate::archive::model::{CaptureChunk, CaptureFile, Generation};
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

//! The **archive** write path: the server-only capture tables, and the
//! transaction discipline they need.
//!
//! # Why this is a second write channel rather than a flag on `write_txn`
//!
//! [`crate::Store::write_txn`] exists to make one guarantee: the order of
//! `sync_log.seq` equals the order rows become visible to readers. It pays for
//! that with a per-tenant advisory lock and a `sync_log` row per business row.
//!
//! The archive tables need **neither** (D3, D23):
//!
//! - they are never synced to devices, so there is no cursor that could skip
//!   an uncommitted row
//! - their downstream consumers (projector / masker / indexer) keep explicit
//!   progress tables (`capture_mask_progress`, `transcript_projections`) rather
//!   than a global cursor
//!
//! Reusing `write_txn` would mean every captured chunk takes the tenant's sync
//! lock — so a machine uploading a year of history would block every other
//! write from that account — and appends a `sync_log` row for data that is
//! deliberately never synced.
//!
//! # What is enforced here, and how
//!
//! The raw transaction lives in the private module [`guarded`], exactly as it
//! does in [`crate::txn`]. There is no accessor, no `Deref`, and no escape hatch
//! other than the methods below — so "somebody wrote an archive row without the
//! discipline" is not a mistake that can be made by accident.
//!
//! Two disciplines are specific to this layer:
//!
//! 1. **Contiguity.** `append_chunk` takes
//!    `pg_advisory_xact_lock(4273, hashtext(generation_id))` and then requires
//!    `offset == current end`. Without the lock two concurrent uploads both read
//!    the same end and both insert; `UNIQUE (generation_id, byte_offset)` turns
//!    that into an error for one of them, but the error arrives after the object
//!    has already been written to storage, and the client has no way to tell
//!    "you raced" from "you are wrong".
//! 2. **Short and write-only.** Reading object storage, decompressing, parsing
//!    and calling a model all happen outside the transaction; one transaction
//!    writes at most a few hundred rows.

pub mod model;
mod query;

use sqlx::PgPool;

use crate::error::{Result, StoreError};
use crate::model::table;
use crate::store::Store;

pub use model::{
    AppendOutcome, CaptureChunk, CaptureFile, CaptureSession, ChunkEncoding, FileEventOp, FileRole,
    Framing, Generation, GenerationReason, NewCaptureChunk, NewCaptureFile, NewCaptureFileEvent,
    NewCaptureMask, NewCaptureSession, NewMaskProgress, RegisteredFile,
};

/// The advisory lock class for archive-generation appends.
///
/// 4273 rather than [`crate::SYNC_ADVISORY_LOCK_KEY`] (4272): the two locks
/// guard different things and must never collide. 4272 serializes *sync
/// sequence allocation* per tenant — it is held for the length of a business
/// write transaction and is what makes the cursor correct. 4273 serializes
/// *appends to one generation* — a much finer key (the second argument is the
/// generation's hash, not the tenant's), because two uploads to different files
/// have no reason to wait for each other.
///
/// Sharing 4272 would mean a large capture upload blocks ordinary memory
/// writes; sharing 4273 for sync would mean the cursor guarantee silently
/// stops holding.
pub const ARCHIVE_GENERATION_LOCK_CLASS: i32 = 4273;

mod guarded {
    use sqlx::{PgPool, Postgres, Transaction};

    use super::ARCHIVE_GENERATION_LOCK_CLASS;
    use crate::error::Result;

    /// The guard around the raw transaction. Its field is private to this
    /// module — that is the enforcement mechanism.
    pub(super) struct Guarded {
        pub(super) tx: Transaction<'static, Postgres>,
    }

    impl Guarded {
        /// Begin a transaction. **No advisory lock is taken here**: the only
        /// lock this layer needs is per generation, and it is taken by
        /// `append_chunk` once it knows which generation is being written.
        pub(super) async fn begin(pool: &PgPool) -> Result<Self> {
            Ok(Self {
                tx: pool.begin().await?,
            })
        }

        /// Serialize appends to one generation.
        ///
        /// `hashtext` rather than a stored integer: the second lock argument
        /// has to be an `int4`, and hashing the generation's ULID gives a
        /// stable value without adding a column. A hash collision between two
        /// generations only costs them waiting for each other.
        pub(super) async fn lock_generation(&mut self, generation_id: &str) -> Result<()> {
            sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
                .bind(ARCHIVE_GENERATION_LOCK_CLASS)
                .bind(generation_id)
                .execute(&mut *self.tx)
                .await?;
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

/// A write transaction over the archive tables.
///
/// Reachable only through [`Store::archive_txn`]. It offers **no method that
/// writes `sync_log`** — that is not an oversight, it is D3 — and no read
/// methods beyond what appending genuinely requires (the current end of a
/// generation, which cannot be computed outside the transaction without
/// reintroducing the race the lock exists to prevent).
pub struct ArchiveTxn {
    inner: guarded::Guarded,
}

impl ArchiveTxn {
    pub(crate) async fn begin(pool: &PgPool) -> Result<Self> {
        Ok(Self {
            inner: guarded::Guarded::begin(pool).await?,
        })
    }

    pub(crate) async fn commit(self) -> Result<()> {
        self.inner.commit().await
    }

    pub(crate) async fn rollback(self) -> Result<()> {
        self.inner.rollback().await
    }

    // ── sessions and files ─────────────────────────────────

    /// Register a session, or find the one that is already there.
    ///
    /// `ON CONFLICT (agent, native_session_id) DO NOTHING` followed by a read
    /// back, rather than `DO UPDATE … RETURNING`: there is nothing to update
    /// (the row is append-only) and an upsert that writes would take a lock on
    /// a row every other device is also registering.
    pub async fn upsert_capture_session(&mut self, new: &NewCaptureSession) -> Result<String> {
        sqlx::query(
            "INSERT INTO capture_sessions (id, agent, native_session_id, parent_session_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (agent, native_session_id) DO NOTHING",
        )
        .bind(new.id.to_string())
        .bind(&new.agent)
        .bind(&new.native_session_id)
        .bind(new.parent_session_id.map(|v| v.to_string()))
        .execute(&mut *self.inner.tx)
        .await?;

        let (id,): (String,) = sqlx::query_as(
            "SELECT id FROM capture_sessions WHERE agent = $1 AND native_session_id = $2",
        )
        .bind(&new.agent)
        .bind(&new.native_session_id)
        .fetch_one(&mut *self.inner.tx)
        .await?;
        Ok(id)
    }

    /// Register a logical file and make sure it has a generation.
    ///
    /// One call does what the client needs before its first append: session
    /// row, file row, and (when the file is new) generation 0. Splitting these
    /// into three requests would leave a window in which the file exists with
    /// no generation, and an append during that window has nowhere to go.
    ///
    /// **Idempotent**: registering the same file twice returns the same ids and
    /// does not open a second generation.
    pub async fn register_capture_file(&mut self, new: &NewCaptureFile) -> Result<RegisteredFile> {
        // The parent has to exist before the child references it, so it is
        // registered first. A missing parent is not an error: the host may
        // have deleted the parent session, and refusing the child's file would
        // lose data that still exists on disk
        if let Some(parent_native) = &new.parent_native_session_id {
            self.upsert_capture_session(&NewCaptureSession {
                id: crate::archive::session_id_for(&new.agent, parent_native),
                agent: new.agent.clone(),
                native_session_id: parent_native.clone(),
                parent_session_id: None,
            })
            .await?;
        }
        let session_id = self
            .upsert_capture_session(&NewCaptureSession {
                // The session id is derived from its unique key so that two
                // devices registering the same session concurrently converge
                // instead of inserting twice
                id: crate::archive::session_id_for(&new.agent, &new.native_session_id),
                agent: new.agent.clone(),
                native_session_id: new.native_session_id.clone(),
                parent_session_id: new
                    .parent_native_session_id
                    .as_ref()
                    .map(|p| crate::archive::session_id_for(&new.agent, p)),
            })
            .await?;

        sqlx::query(
            "INSERT INTO capture_files
                 (id, session_id, device_id, agent, logical_path, role, framing, cwd, project_key)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
             ON CONFLICT (device_id, agent, logical_path) DO NOTHING",
        )
        .bind(new.id.to_string())
        .bind(&session_id)
        .bind(&new.device_id)
        .bind(&new.agent)
        .bind(&new.logical_path)
        .bind(new.role)
        .bind(new.framing)
        .bind(&new.cwd)
        .bind(&new.project_key)
        .execute(&mut *self.inner.tx)
        .await?;

        let file = self
            .file_by_key(&new.device_id, &new.agent, &new.logical_path)
            .await?;
        let session = self.session(&file.session_id).await?;

        // A file always has at least one generation. Without this, the client's
        // first append would have to know the generation id from somewhere —
        // and "there is none yet" is not an id it can send
        let generation = match self.latest_generation(&file.id).await? {
            Some((g, _)) => g,
            None => {
                self.open_generation(&file.id, GenerationReason::Initial)
                    .await?
            }
        };
        let end_offset = self.generation_end(&generation.id).await?;
        Ok(RegisteredFile {
            session,
            file,
            generation,
            end_offset,
        })
    }

    /// Open the next generation of a file.
    ///
    /// The ordinal is computed inside the transaction (`max(ordinal) + 1`)
    /// rather than passed in: two clients that both notice a rewrite would
    /// otherwise both claim ordinal 1, and `UNIQUE (file_id, ordinal)` would
    /// turn that into an error for one of them — after it had already uploaded
    /// the bytes.
    pub async fn open_generation(
        &mut self,
        file_id: &str,
        reason: GenerationReason,
    ) -> Result<Generation> {
        let (next,): (i32,) = sqlx::query_as(
            "SELECT coalesce(max(ordinal) + 1, 0) FROM capture_generations WHERE file_id = $1",
        )
        .bind(file_id)
        .fetch_one(&mut *self.inner.tx)
        .await?;

        let id = cormex_core::Id::new().to_string();
        sqlx::query(
            "INSERT INTO capture_generations (id, file_id, ordinal, reason)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&id)
        .bind(file_id)
        .bind(next)
        .bind(reason)
        .execute(&mut *self.inner.tx)
        .await?;

        self.generation(&id).await
    }

    // ── chunks ─────────────────────────────────────────────

    /// Append one chunk to a generation.
    ///
    /// The whole point of this method is the three-way answer in
    /// [`AppendOutcome`]. The rules, in order:
    ///
    /// ```text
    /// offset == end  → INSERT                      → Appended
    /// offset <  end  → same offset, same sha256?   → AlreadyPresent
    ///                  otherwise                   → OffsetMismatch { expected: end }
    /// offset >  end  → OffsetMismatch { expected: end }
    /// ```
    ///
    /// `AlreadyPresent` is what makes a retry after a crash idempotent. It is
    /// deliberately keyed on **both** the offset and the content hash: the same
    /// offset with different bytes is not a replay, it is a client whose idea
    /// of the file has diverged — and answering "already present" there would
    /// silently keep the wrong bytes.
    pub async fn append_chunk(&mut self, new: &NewCaptureChunk) -> Result<AppendOutcome> {
        let generation_id = new.generation_id.to_string();
        // **The lock comes before the read of the end.** Reading first and
        // locking later is the same as not locking: two transactions read the
        // same end and both decide they may append
        self.inner.lock_generation(&generation_id).await?;
        let end = self.generation_end(&generation_id).await?;

        if new.byte_offset < end {
            let existing: Option<(String, i64)> = sqlx::query_as(
                "SELECT content_sha256, byte_length FROM capture_chunks
                  WHERE generation_id = $1 AND byte_offset = $2",
            )
            .bind(&generation_id)
            .bind(new.byte_offset)
            .fetch_optional(&mut *self.inner.tx)
            .await?;
            return match existing {
                Some((sha, len)) if sha == new.content_sha256 && len == new.byte_length => {
                    Ok(AppendOutcome::AlreadyPresent { end_offset: end })
                }
                _ => Ok(AppendOutcome::OffsetMismatch { expected: end }),
            };
        }
        if new.byte_offset > end {
            return Ok(AppendOutcome::OffsetMismatch { expected: end });
        }

        sqlx::query(
            "INSERT INTO capture_chunks
                 (id, generation_id, byte_offset, byte_length, content_sha256,
                  storage_key, encoding, observed_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(new.id.to_string())
        .bind(&generation_id)
        .bind(new.byte_offset)
        .bind(new.byte_length)
        .bind(&new.content_sha256)
        .bind(&new.storage_key)
        .bind(new.encoding)
        .bind(new.observed_at)
        .execute(&mut *self.inner.tx)
        .await?;

        Ok(AppendOutcome::Appended {
            end_offset: end + new.byte_length,
        })
    }

    /// Record one observation of a file's lifecycle.
    pub async fn insert_file_event(&mut self, new: &NewCaptureFileEvent) -> Result<()> {
        sqlx::query(
            "INSERT INTO capture_file_events (id, file_id, op, detail) VALUES ($1, $2, $3, $4)",
        )
        .bind(new.id.to_string())
        .bind(new.file_id.to_string())
        .bind(new.op)
        .bind(&new.detail)
        .execute(&mut *self.inner.tx)
        .await?;
        Ok(())
    }

    // ── reads that appending genuinely needs ───────────────

    /// The current end of a generation: `max(byte_offset + byte_length)`, or 0
    /// when it holds nothing yet.
    pub async fn generation_end(&mut self, generation_id: &str) -> Result<i64> {
        let (end,): (i64,) = sqlx::query_as(
            "SELECT coalesce(max(byte_offset + byte_length), 0)
               FROM capture_chunks WHERE generation_id = $1",
        )
        .bind(generation_id)
        .fetch_one(&mut *self.inner.tx)
        .await?;
        Ok(end)
    }

    /// The newest generation of a file, with the offset it currently ends at.
    pub async fn latest_generation(&mut self, file_id: &str) -> Result<Option<(Generation, i64)>> {
        let row = sqlx::query_as::<_, Generation>(
            "SELECT id, file_id, ordinal, reason, created_at
               FROM capture_generations WHERE file_id = $1
              ORDER BY ordinal DESC LIMIT 1",
        )
        .bind(file_id)
        .fetch_optional(&mut *self.inner.tx)
        .await?;
        match row {
            None => Ok(None),
            Some(g) => {
                let end = self.generation_end(&g.id).await?;
                Ok(Some((g, end)))
            }
        }
    }

    async fn generation(&mut self, id: &str) -> Result<Generation> {
        let row = sqlx::query_as::<_, Generation>(
            "SELECT id, file_id, ordinal, reason, created_at
               FROM capture_generations WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut *self.inner.tx)
        .await?;
        Ok(row)
    }

    async fn session(&mut self, id: &str) -> Result<CaptureSession> {
        let row = sqlx::query_as::<_, CaptureSession>(
            "SELECT id, agent, native_session_id, parent_session_id, first_seen_at
               FROM capture_sessions WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&mut *self.inner.tx)
        .await?;
        Ok(row)
    }

    async fn file_by_key(
        &mut self,
        device_id: &str,
        agent: &str,
        logical_path: &str,
    ) -> Result<CaptureFile> {
        let row = sqlx::query_as::<_, CaptureFile>(
            "SELECT id, session_id, device_id, agent, logical_path, role, framing,
                    cwd, project_key, first_seen_at
               FROM capture_files
              WHERE device_id = $1 AND agent = $2 AND logical_path = $3",
        )
        .bind(device_id)
        .bind(agent)
        .bind(logical_path)
        .fetch_optional(&mut *self.inner.tx)
        .await?
        .ok_or_else(|| {
            StoreError::Invalid(format!(
                "capture file {agent}:{logical_path} on device {device_id} vanished between \
                 registration and read-back"
            ))
        })?;
        Ok(row)
    }
}

impl Store {
    /// Run `f` inside a transaction over the **archive** tables.
    ///
    /// No advisory lock, no `sync_log` (D3, D23). See the module docs for why
    /// this is a second channel rather than a flag on
    /// [`Store::write_txn`].
    ///
    /// Commits when `f` returns `Ok`, rolls back on `Err`.
    pub async fn archive_txn<T, F>(&self, f: F) -> Result<T>
    where
        F: AsyncFnOnce(&mut ArchiveTxn) -> Result<T>,
    {
        let mut txn = ArchiveTxn::begin(self.pool()).await?;
        match f(&mut txn).await {
            Ok(value) => {
                txn.commit().await?;
                Ok(value)
            }
            Err(err) => {
                if let Err(rollback_err) = txn.rollback().await {
                    tracing::warn!(
                        error = %rollback_err,
                        "rolling back the archive transaction failed"
                    );
                }
                Err(err)
            }
        }
    }
}

/// The session id for an `(agent, native_session_id)` pair.
///
/// Derived rather than random so that two devices registering the same session
/// at the same time produce the same id: the `ON CONFLICT` then converges on
/// one row instead of one device winning and the other reading back a
/// different id than it sent.
fn session_id_for(agent: &str, native_session_id: &str) -> cormex_core::Id {
    cormex_core::Id::derived(
        chrono::DateTime::from_timestamp(0, 0).expect("the epoch is a valid timestamp"),
        &format!("capture_session|{agent}|{native_session_id}"),
    )
}

/// The archive tables, and the test that keeps them out of `table::ALL`.
///
/// `table::ALL` is defined as "every table name that can appear in
/// `sync_log.table_name`", and the archive tables never do (D3). Keeping them
/// in a separate constant — rather than adding them to `ALL` with a comment
/// saying "not really" — is what makes the disjointness something a test can
/// assert.
pub const ARCHIVE_TABLES: &[&str] = table::ARCHIVE;

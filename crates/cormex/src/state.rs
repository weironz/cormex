//! The local state database, `state.db` (D22).
//!
//! # Why SQLite rather than a JSON file
//!
//! Several hook processes write here **concurrently**: Claude Code fires
//! `Stop`, `SubagentStop`, `PreCompact` and `SessionEnd`, and a scheduler runs
//! `sync --once` on top. A JSON file needs a lock and a read-modify-write
//! cycle, and the failure mode of getting that wrong is a **lost update** — a
//! file marked as uploaded that was not, or a queue entry dropped. SQLite gives
//! cross-process transactions for free: WAL plus `busy_timeout` plus
//! `BEGIN IMMEDIATE`.
//!
//! # Why `user_version` rather than a migration table
//!
//! The schema is four tables and changes rarely. `PRAGMA user_version` is one
//! integer, needs no table, and cannot get out of step with itself. A migration
//! table would be more machinery than the thing it manages.

use std::path::Path;

use anyhow::{Context as _, Result};
use rusqlite::{Connection, OptionalExtension as _, params};

/// The schema version this build expects.
///
/// Bump it and add an arm to [`migrate`] when the schema changes. A database
/// **newer** than this build is an error rather than an attempt to use it: the
/// user downgraded, and reading a schema we do not know is how a column gets
/// silently misread.
const SCHEMA_VERSION: i32 = 1;

/// One logical file's upload progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileState {
    /// `agent + logical_path`. The primary key.
    pub local_key: String,
    pub agent: String,
    /// The physical path as it was last seen.
    pub path: String,
    /// The server's file id. `None` = never registered.
    pub file_id: Option<String>,
    pub generation_id: Option<String>,
    pub generation_ordinal: Option<i32>,
    /// How many logical bytes have been accepted by the server.
    pub uploaded_offset: i64,
    /// The sha256 of the last 4 KiB ending at `uploaded_offset`.
    ///
    /// This is what catches an in-place rewrite: the file is the same length,
    /// but the bytes before the upload point changed. Comparing the whole file
    /// would mean re-reading all of it on every run.
    pub tail_sha256: Option<String>,
    /// The whole-file hash, for snapshot sources.
    pub whole_sha256: Option<String>,
    pub last_size: Option<i64>,
    pub last_mtime_ns: Option<i64>,
    /// Whether a `missing` event has already been sent. Sent once, not every
    /// run.
    pub missing_reported: bool,
    pub last_ok_at: Option<String>,
}

/// One queued "this file changed" note from a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: i64,
    pub agent: String,
    pub path: String,
    /// The `cwd` from the hook's input, which is more accurate than decoding it
    /// out of the path.
    pub cwd: Option<String>,
    pub event: Option<String>,
    pub enqueued_at: String,
    pub attempts: i64,
}

/// The local state database.
pub struct StateDb {
    conn: Connection,
}

impl StateDb {
    /// Open (and migrate) `<data_dir>/state.db`.
    ///
    /// # Errors
    /// The directory cannot be created, the database cannot be opened, or its
    /// schema is newer than this build understands.
    pub fn open(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("could not create {}", data_dir.display()))?;
        let path = data_dir.join("state.db");
        let conn = Connection::open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;

        // WAL: readers do not block the writer, which is what makes "a hook
        // writes while a sync reads" work at all
        conn.pragma_update(None, "journal_mode", "WAL")
            .context("could not switch to WAL")?;
        // 5 s: long enough for a sync to finish a transaction, short enough
        // that a hook does not hang its host
        conn.pragma_update(None, "busy_timeout", 5000)
            .context("could not set busy_timeout")?;
        // NORMAL rather than FULL: a crash can lose the last few transactions,
        // and the consequence is re-uploading a chunk — which is idempotent.
        // FULL would fsync on every hook invocation, and the hook has a
        // 1.5-second budget on SessionEnd
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context("could not set synchronous")?;

        let mut db = Self { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&mut self) -> Result<()> {
        let version: i32 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .context("could not read user_version")?;

        if version > SCHEMA_VERSION {
            anyhow::bail!(
                "state.db is at schema version {version}, but this build understands \
                 {SCHEMA_VERSION}. That means an older `cormex` is running against a database a \
                 newer one created — upgrade rather than downgrade, or the columns will be \
                 misread"
            );
        }
        if version == SCHEMA_VERSION {
            return Ok(());
        }

        // v0 -> v1: the initial schema
        let tx = self.conn.transaction().context("could not begin")?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                 local_key         TEXT PRIMARY KEY,
                 agent             TEXT NOT NULL,
                 path              TEXT NOT NULL,
                 file_id           TEXT,
                 generation_id     TEXT,
                 generation_ordinal INTEGER,
                 uploaded_offset   INTEGER NOT NULL DEFAULT 0,
                 tail_sha256       TEXT,
                 whole_sha256      TEXT,
                 last_size         INTEGER,
                 last_mtime_ns     INTEGER,
                 missing_reported  INTEGER NOT NULL DEFAULT 0,
                 last_ok_at        TEXT
             );
             CREATE TABLE IF NOT EXISTS queue (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 agent TEXT NOT NULL,
                 path TEXT NOT NULL,
                 cwd TEXT,
                 event TEXT,
                 enqueued_at TEXT NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE IF NOT EXISTS runs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 trigger TEXT NOT NULL,
                 started_at TEXT,
                 finished_at TEXT,
                 files INTEGER,
                 bytes INTEGER,
                 errors INTEGER,
                 last_error TEXT
             );
             CREATE TABLE IF NOT EXISTS installs (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 kind TEXT NOT NULL,
                 target TEXT NOT NULL,
                 backup_path TEXT,
                 installed_at TEXT NOT NULL
             );",
        )
        .context("could not create the schema")?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)
            .context("could not set user_version")?;
        tx.commit().context("could not commit the migration")?;
        Ok(())
    }

    /// One file's state, or `None` if it has never been seen.
    ///
    /// # Errors
    /// The query failed.
    pub fn file(&self, local_key: &str) -> Result<Option<FileState>> {
        let row = self
            .conn
            .query_row(
                "SELECT local_key, agent, path, file_id, generation_id, generation_ordinal,
                        uploaded_offset, tail_sha256, whole_sha256, last_size, last_mtime_ns,
                        missing_reported, last_ok_at
                   FROM files WHERE local_key = ?1",
                params![local_key],
                |r| {
                    Ok(FileState {
                        local_key: r.get(0)?,
                        agent: r.get(1)?,
                        path: r.get(2)?,
                        file_id: r.get(3)?,
                        generation_id: r.get(4)?,
                        generation_ordinal: r.get(5)?,
                        uploaded_offset: r.get(6)?,
                        tail_sha256: r.get(7)?,
                        whole_sha256: r.get(8)?,
                        last_size: r.get(9)?,
                        last_mtime_ns: r.get(10)?,
                        missing_reported: r.get::<_, i64>(11)? != 0,
                        last_ok_at: r.get(12)?,
                    })
                },
            )
            .optional()
            .context("could not read the file state")?;
        Ok(row)
    }

    /// Insert or replace one file's state.
    ///
    /// # Errors
    /// The write failed.
    pub fn put_file(&self, f: &FileState) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO files
                     (local_key, agent, path, file_id, generation_id, generation_ordinal,
                      uploaded_offset, tail_sha256, whole_sha256, last_size, last_mtime_ns,
                      missing_reported, last_ok_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
                 ON CONFLICT(local_key) DO UPDATE SET
                     path = excluded.path,
                     file_id = excluded.file_id,
                     generation_id = excluded.generation_id,
                     generation_ordinal = excluded.generation_ordinal,
                     uploaded_offset = excluded.uploaded_offset,
                     tail_sha256 = excluded.tail_sha256,
                     whole_sha256 = excluded.whole_sha256,
                     last_size = excluded.last_size,
                     last_mtime_ns = excluded.last_mtime_ns,
                     missing_reported = excluded.missing_reported,
                     last_ok_at = excluded.last_ok_at",
                params![
                    f.local_key,
                    f.agent,
                    f.path,
                    f.file_id,
                    f.generation_id,
                    f.generation_ordinal,
                    f.uploaded_offset,
                    f.tail_sha256,
                    f.whole_sha256,
                    f.last_size,
                    f.last_mtime_ns,
                    i64::from(f.missing_reported),
                    f.last_ok_at,
                ],
            )
            .context("could not write the file state")?;
        Ok(())
    }

    /// Append one queue entry.
    ///
    /// # Errors
    /// The write failed. The hook's caller turns that into a spool file rather
    /// than an error, because a hook must never fail its host.
    pub fn enqueue(
        &self,
        agent: &str,
        path: &str,
        cwd: Option<&str>,
        event: Option<&str>,
        at: &str,
    ) -> Result<i64> {
        self.conn
            .execute(
                "INSERT INTO queue (agent, path, cwd, event, enqueued_at) VALUES (?1,?2,?3,?4,?5)",
                params![agent, path, cwd, event, at],
            )
            .context("could not enqueue")?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Drain the queue, oldest first.
    ///
    /// # Errors
    /// The query failed.
    pub fn queue(&self) -> Result<Vec<QueueEntry>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, agent, path, cwd, event, enqueued_at, attempts
                   FROM queue ORDER BY id ASC",
            )
            .context("could not prepare the queue query")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(QueueEntry {
                    id: r.get(0)?,
                    agent: r.get(1)?,
                    path: r.get(2)?,
                    cwd: r.get(3)?,
                    event: r.get(4)?,
                    enqueued_at: r.get(5)?,
                    attempts: r.get(6)?,
                })
            })
            .context("could not read the queue")?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.context("could not read a queue row")?);
        }
        Ok(out)
    }

    /// Remove one queue entry, after it has been handled.
    ///
    /// # Errors
    /// The delete failed.
    pub fn dequeue(&self, id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM queue WHERE id = ?1", params![id])
            .context("could not dequeue")?;
        Ok(())
    }

    /// Record one sync run, for `doctor`.
    ///
    /// # Errors
    /// The write failed.
    #[expect(
        clippy::too_many_arguments,
        reason = "these are the columns of one row; a struct would merely move the list, and \
                  there is one caller"
    )]
    pub fn record_run(
        &self,
        trigger: &str,
        started_at: &str,
        finished_at: &str,
        files: i64,
        bytes: i64,
        errors: i64,
        last_error: Option<&str>,
    ) -> Result<()> {
        self.conn
            .execute(
                "INSERT INTO runs
                     (trigger, started_at, finished_at, files, bytes, errors, last_error)
                 VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    trigger,
                    started_at,
                    finished_at,
                    files,
                    bytes,
                    errors,
                    last_error
                ],
            )
            .context("could not record the run")?;
        Ok(())
    }

    /// The most recent successful run's finish time.
    ///
    /// # Errors
    /// The query failed.
    pub fn last_successful_run(&self) -> Result<Option<String>> {
        let row = self
            .conn
            .query_row(
                "SELECT finished_at FROM runs WHERE errors = 0 ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()
            .context("could not read the last run")?;
        Ok(row)
    }

    /// How many queue entries are waiting.
    ///
    /// # Errors
    /// The query failed.
    pub fn queue_len(&self) -> Result<i64> {
        let n = self
            .conn
            .query_row("SELECT count(*) FROM queue", [], |r| r.get(0))
            .context("could not count the queue")?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, StateDb) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let db = StateDb::open(dir.path()).expect("open");
        (dir, db)
    }

    /// A fresh database migrates to the current version, and reopening it is a
    /// no-op.
    #[test]
    fn a_fresh_database_migrates_and_reopening_is_a_no_op() {
        let dir = tempfile::tempdir().expect("a temp dir");
        {
            let db = StateDb::open(dir.path()).expect("open");
            let v: i32 = db
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .expect("version");
            assert_eq!(v, SCHEMA_VERSION);
        }
        // Reopening must not re-run the migration and must keep the data
        {
            let db = StateDb::open(dir.path()).expect("reopen");
            db.enqueue(
                "claude_code",
                "/p/s.jsonl",
                None,
                None,
                "2026-10-29T00:00:00Z",
            )
            .expect("enqueue");
        }
        let db = StateDb::open(dir.path()).expect("reopen again");
        assert_eq!(
            db.queue_len().expect("len"),
            1,
            "reopening lost the queue -- the migration ran again and recreated the table"
        );
    }

    /// A database from a **newer** build is refused.
    ///
    /// Reading a schema we do not know is how a column gets silently misread,
    /// and the symptom would be "my uploads restart from zero every time".
    #[test]
    fn a_newer_schema_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        {
            let db = StateDb::open(dir.path()).expect("open");
            db.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .expect("bump");
        }
        let err = match StateDb::open(dir.path()) {
            Ok(_) => panic!("a newer schema must be refused"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("downgrade"),
            "the error should explain that this is a downgrade, got: {err}"
        );
    }

    /// A file's state round-trips, including the `Option` fields.
    #[test]
    fn the_file_state_round_trips() {
        let (_dir, db) = db();
        assert!(db.file("claude_code|/p/s.jsonl").expect("read").is_none());

        let state = FileState {
            local_key: "claude_code|/p/s.jsonl".into(),
            agent: "claude_code".into(),
            path: "/p/s.jsonl".into(),
            file_id: Some("01ABC".into()),
            generation_id: Some("01DEF".into()),
            generation_ordinal: Some(2),
            uploaded_offset: 4096,
            tail_sha256: Some("ab".repeat(32)),
            whole_sha256: None,
            last_size: Some(8192),
            last_mtime_ns: Some(123),
            missing_reported: true,
            last_ok_at: Some("2026-10-29T00:00:00Z".into()),
        };
        db.put_file(&state).expect("write");
        assert_eq!(db.file(&state.local_key).expect("read"), Some(state));
    }

    /// The queue is FIFO and drains one entry at a time.
    #[test]
    fn the_queue_is_fifo() {
        let (_dir, db) = db();
        db.enqueue("a", "/1", None, Some("Stop"), "t1").expect("e1");
        db.enqueue("a", "/2", None, Some("Stop"), "t2").expect("e2");
        let q = db.queue().expect("queue");
        assert_eq!(
            q.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(),
            ["/1", "/2"]
        );
        db.dequeue(q[0].id).expect("dequeue");
        assert_eq!(db.queue_len().expect("len"), 1);
        assert_eq!(db.queue().expect("queue")[0].path, "/2");
    }

    /// `last_successful_run` skips runs that had errors.
    #[test]
    fn the_last_successful_run_ignores_failed_ones() {
        let (_dir, db) = db();
        db.record_run("manual", "t0", "t1", 1, 10, 0, None)
            .expect("ok run");
        db.record_run("manual", "t2", "t3", 1, 10, 2, Some("boom"))
            .expect("failed run");
        assert_eq!(
            db.last_successful_run().expect("read").as_deref(),
            Some("t1"),
            "a run with errors must not count as the last success -- `doctor` would then say \
             everything is fine while nothing has uploaded for a week"
        );
    }
}

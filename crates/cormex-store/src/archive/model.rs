//! The archive layer's rows.
//!
//! Field for field with `migrations/20261101000001_capture_archive.sql`. The
//! mapping is deliberate and mechanical: a new column means a new field here,
//! and the compiler then finds every INSERT that has to be updated. The
//! alternative — building the SQL from `serde_json::Value` — would make a
//! missing column a runtime error that only fires when that particular path is
//! exercised.

use chrono::{DateTime, Utc};
use cormex_core::Id;

use crate::model::text_enum;

text_enum! {
    /// `capture_files.role` — what kind of file this is within its session.
    pub enum FileRole {
        /// The main conversation log.
        Transcript => "transcript",
        /// A sub-agent's own log, hanging off the parent session.
        Subagent => "subagent",
        /// Tool output that the host spilled out of the main log into its own
        /// file (Claude Code's `tool-results/`).
        Sidecar => "sidecar",
        /// A binary the conversation referred to.
        Attachment => "attachment",
        /// The host's own memory pseudo-session (Claude Code's `memory/`).
        Memory => "memory",
        /// One row of a SQLite source, treated as a logical file of its own.
        Row => "row",
    }
}

text_enum! {
    /// `capture_files.framing` — how the file's bytes behave over time.
    ///
    /// This is what decides whether an append is "more bytes at the end" or
    /// "the whole thing again". Getting it wrong for a snapshot source produces
    /// a generation per sync, which is not wrong so much as unbounded.
    pub enum Framing {
        /// Only ever grows (the hosts' `*.jsonl` logs).
        Append => "append",
        /// Rewritten wholesale each time (export files, SQLite rows).
        Snapshot => "snapshot",
    }
}

text_enum! {
    /// `capture_generations.reason` — why a new generation was opened.
    ///
    /// Recorded rather than inferred: "the file was truncated" and "the file
    /// was rewritten in place" are indistinguishable after the fact from the
    /// bytes alone, and they mean different things about what the host was
    /// doing.
    pub enum GenerationReason {
        /// The first time this file was seen.
        Initial => "initial",
        /// Same length, different content — the client caught it with a
        /// trailing hash.
        Rewrite => "rewrite",
        /// The file got shorter.
        Truncate => "truncate",
        /// A snapshot source, captured again.
        Snapshot => "snapshot",
    }
}

text_enum! {
    /// `capture_file_events.op` — what the client observed about the file on
    /// disk.
    ///
    /// "Cormex is the only copy" is derived from this: every device's latest
    /// event for the file is `missing`.
    pub enum FileEventOp {
        Missing => "missing",
        Reappeared => "reappeared",
        Excluded => "excluded",
        Included => "included",
    }
}

// ══════════════════════════════════════════════════════════
//  Sessions and files
// ══════════════════════════════════════════════════════════

/// A session to register. `id` is derived from `(agent, native_session_id)`
/// rather than random, so that two devices registering the same session at the
/// same time converge on one row instead of racing.
#[derive(Debug, Clone)]
pub struct NewCaptureSession {
    pub id: Id,
    pub agent: String,
    pub native_session_id: String,
    pub parent_session_id: Option<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CaptureSession {
    pub id: String,
    pub agent: String,
    pub native_session_id: String,
    pub parent_session_id: Option<String>,
    pub first_seen_at: DateTime<Utc>,
}

/// A logical file to register.
#[derive(Debug, Clone)]
pub struct NewCaptureFile {
    pub id: Id,
    /// The host's own session id. The session row is found or created from
    /// `(agent, native_session_id)` — the caller does not have to register the
    /// session first, because that would leave a window in which the file
    /// exists and its session does not.
    pub native_session_id: String,
    /// Set when this is a sub-agent's file, so the session row points at its
    /// parent. Ignored when the session already exists.
    pub parent_native_session_id: Option<String>,
    /// `cormex_auth.device_keys.id`. Cross-schema, so no foreign key.
    pub device_id: String,
    pub agent: String,
    /// Relative to the agent's home, forward slashes. Always the **plaintext**
    /// name: a `.jsonl.zst` and a `.jsonl` are the same logical file.
    pub logical_path: String,
    pub role: FileRole,
    pub framing: Framing,
    pub cwd: Option<String>,
    pub project_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CaptureFile {
    pub id: String,
    pub session_id: String,
    pub device_id: String,
    pub agent: String,
    pub logical_path: String,
    pub role: FileRole,
    pub framing: Framing,
    pub cwd: Option<String>,
    pub project_key: Option<String>,
    pub first_seen_at: DateTime<Utc>,
}

// ══════════════════════════════════════════════════════════
//  Generations and chunks
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Generation {
    pub id: String,
    pub file_id: String,
    pub ordinal: i32,
    pub reason: GenerationReason,
    pub created_at: DateTime<Utc>,
}

/// A chunk to append.
#[derive(Debug, Clone)]
pub struct NewCaptureChunk {
    pub id: Id,
    pub generation_id: Id,
    pub byte_offset: i64,
    pub byte_length: i64,
    /// SHA-256 of the **logical (uncompressed)** bytes. This is the idempotency
    /// key: the same file as plaintext and as `.jsonl.zst` has to hash the same.
    pub content_sha256: String,
    /// The object-storage key. Stored rather than derived, because the layout
    /// will change and old rows must keep pointing where they point.
    pub storage_key: String,
    pub encoding: ChunkEncoding,
    /// When the client read these bytes (client clock). The `at` axis.
    pub observed_at: DateTime<Utc>,
}

text_enum! {
    /// `capture_chunks.encoding` — how the object's bytes are stored.
    pub enum ChunkEncoding {
        /// A zstd frame.
        Zstd => "zstd",
        /// Stored as-is.
        Identity => "identity",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CaptureChunk {
    pub id: String,
    pub generation_id: String,
    pub byte_offset: i64,
    pub byte_length: i64,
    pub content_sha256: String,
    pub storage_key: String,
    pub encoding: ChunkEncoding,
    pub observed_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
}

/// What [`super::ArchiveTxn::append_chunk`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// Written at the end of the generation.
    Appended { end_offset: i64 },
    /// The same `(offset, content_sha256)` was already there: an idempotent
    /// replay, not an error.
    ///
    /// This is a **normal** path, not an exceptional one: a client that died
    /// between "the server accepted it" and "I recorded it as sent" retries on
    /// the next run. Answering 409 would force it to tell a success apart from
    /// a failure for no reason.
    AlreadyPresent { end_offset: i64 },
    /// The offset is not where this generation currently ends. The client
    /// resumes from `expected`.
    OffsetMismatch { expected: i64 },
}

// ══════════════════════════════════════════════════════════
//  File lifecycle
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct NewCaptureFileEvent {
    pub id: Id,
    pub file_id: Id,
    pub op: FileEventOp,
    pub detail: Option<String>,
}

// ══════════════════════════════════════════════════════════
//  Masks (wired up in P2-2; the types live here so the DDL has a counterpart)
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone)]
pub struct NewCaptureMask {
    pub id: Id,
    pub generation_id: Id,
    pub byte_offset: i64,
    pub byte_length: i32,
    /// `redaction::Rule::name()`.
    pub rule: String,
    /// The scanner version. Rules can be upgraded and the results re-scanned;
    /// old and new coexist rather than overwrite.
    pub scanner: String,
}

#[derive(Debug, Clone)]
pub struct NewMaskProgress {
    pub id: Id,
    pub generation_id: Id,
    pub scanner: String,
    pub through_offset: i64,
}

/// What [`super::ArchiveTxn::register_capture_file`] found or created.
#[derive(Debug, Clone)]
pub struct RegisteredFile {
    pub session: CaptureSession,
    pub file: CaptureFile,
    /// The newest generation, and how many logical bytes it holds.
    ///
    /// A file always has at least one: registration creates generation 0 with
    /// `reason = initial` when there is none. That is what lets the client
    /// append to a brand-new file without a second round trip.
    pub generation: Generation,
    pub end_offset: i64,
}

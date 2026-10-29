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

// ══════════════════════════════════════════════════════════
//  The P layer (phase 2)
// ══════════════════════════════════════════════════════════

text_enum! {
    /// `transcript_events.kind`.
    ///
    /// The spellings match `cormex_proto::transcript::TranscriptKind`; the
    /// conversion is one `match` in the projector, where a new variant is a
    /// compile error rather than a silently missing branch.
    pub enum TranscriptEventKind {
        UserMessage => "user_message",
        AgentMessage => "agent_message",
        AgentThought => "agent_thought",
        ToolCall => "tool_call",
        ToolResult => "tool_result",
        Plan => "plan",
        System => "system",
        Meta => "meta",
        Unknown => "unknown",
    }
}

text_enum! {
    /// `transcript_projections.status`.
    pub enum ProjectionStatus {
        Ok => "ok",
        /// Some files are projected and others are not yet.
        Partial => "partial",
        /// The parser failed on this session. Not retried for an hour.
        Failed => "failed",
    }
}

/// One normalized event to insert.
#[derive(Debug, Clone)]
pub struct NewTranscriptEvent {
    pub id: Id,
    pub session_id: Id,
    pub parser_version: String,
    pub seq: i64,
    pub kind: TranscriptEventKind,
    pub occurred_at: Option<DateTime<Utc>>,
    /// The ACP-aligned content blocks, already serialized.
    pub content: serde_json::Value,
    /// The plain text, **already masked**.
    pub text: Option<String>,
    /// The jieba-segmented form, for `to_tsvector('simple', $n)`.
    ///
    /// Passed in rather than computed here: the tokenizer lives in
    /// `cormex-memory`, and `cortex-store` must not depend on it. The caller
    /// computes it outside the transaction, which is also where the rule "the
    /// tsv is written with the main row" is satisfied — same INSERT.
    pub tsv_source: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub src_generation_id: Id,
    pub src_offset: i64,
    pub src_length: i64,
}

/// One row of `transcript_events`.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct TranscriptEvent {
    pub id: String,
    pub session_id: String,
    pub parser_version: String,
    pub seq: i64,
    pub kind: TranscriptEventKind,
    pub occurred_at: Option<DateTime<Utc>>,
    pub content: serde_json::Value,
    pub text: Option<String>,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    pub src_generation_id: String,
    pub src_offset: i64,
    pub src_length: i64,
    pub created_at: DateTime<Utc>,
}

/// One projection progress row to insert.
#[derive(Debug, Clone)]
pub struct NewProjection {
    pub id: Id,
    pub session_id: Id,
    pub parser_version: String,
    /// `{generation_id: offset projected to}`.
    pub through: serde_json::Value,
    pub next_seq: i64,
    pub status: ProjectionStatus,
    pub error: Option<String>,
}

/// The newest projection for a session.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Projection {
    pub id: String,
    pub session_id: String,
    pub parser_version: String,
    pub through: serde_json::Value,
    pub next_seq: i64,
    pub status: ProjectionStatus,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// One archive-index chunk to insert.
#[derive(Debug, Clone)]
pub struct NewArchiveChunk {
    pub id: Id,
    pub session_id: Id,
    pub event_id: Id,
    pub part: i32,
    pub text: String,
    /// The jieba-segmented form, like [`NewTranscriptEvent::tsv_source`].
    pub tsv_source: String,
}

/// One access-log row to insert.
#[derive(Debug, Clone)]
pub struct NewAccessLog {
    pub id: Id,
    /// Who read it: `user:<id>`, `device:<id>`, `delegated:<session>`.
    pub principal: String,
    pub generation_id: Id,
    pub byte_offset: i64,
    pub byte_length: i64,
    /// Whether the read bypassed the mask overlay.
    pub unmasked: bool,
}

/// One row of the archive's session list.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ArchiveSessionRow {
    pub id: String,
    pub agent: String,
    pub first_at: Option<DateTime<Utc>>,
    pub last_at: Option<DateTime<Utc>>,
    pub event_count: i64,
    /// The first user message's text, for deriving a title.
    pub title_source: Option<String>,
    pub project_key: Option<String>,
    pub devices: Option<Vec<String>>,
}

/// One hit from the conversation search.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ConversationHit {
    pub session_id: String,
    pub event_id: String,
    pub occurred_at: Option<DateTime<Utc>>,
    pub text: Option<String>,
    pub score: f32,
}

/// One hit from the archive search.
#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ArchiveHit {
    pub session_id: String,
    pub event_id: String,
    pub part: i32,
    pub text: String,
    pub occurred_at: Option<DateTime<Utc>>,
    pub score: f32,
}

/// One wrapped data key for a capture file (P6-1).
#[derive(Debug, Clone)]
pub struct NewCaptureKey {
    pub id: cormex_core::Id,
    pub file_id: cormex_core::Id,
    pub wrapped_key: Vec<u8>,
}

/// One conversation turn's vector (P5-6).
#[derive(Debug, Clone)]
pub struct NewTranscriptEmbedding {
    pub event_id: cormex_core::Id,
    pub embedding: crate::Vector,
    pub embedding_model: String,
}

/// One exclusion / inclusion of a project's archive index.
#[derive(Debug, Clone)]
pub struct NewArchiveIndexEvent {
    pub id: Id,
    pub project_key: String,
    pub excluded: bool,
}

/// One mask interval.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct CaptureMask {
    pub id: String,
    pub generation_id: String,
    pub byte_offset: i64,
    pub byte_length: i32,
    pub rule: String,
    pub scanner: String,
    pub created_at: DateTime<Utc>,
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

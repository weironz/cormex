//! The capture wire protocol: what the `cormex` CLI sends to `POST /capture/*`.
//!
//! Design: docs/design/session-archive-architecture.md §7.1.
//!
//! # Why the DTOs live here rather than in the server
//!
//! The CLI and the server are separate binaries on separate release lines, and
//! the CLI is the one that ships to other people's machines. A field that
//! silently goes missing between them is not a compile error — `serde` ignores
//! unknown fields, so the server simply records a file with no `cwd`, and the
//! symptom appears much later as "the project filter does not work". One shared
//! crate turns that into a compile error on both sides.
//!
//! # The two headers that are not in the body
//!
//! `X-Content-SHA256` and `X-Observed-At` travel as headers on
//! `PUT …/chunks/{offset}` because the body is raw bytes: there is nowhere to
//! put metadata without framing it, and framing it would mean the server has to
//! parse an envelope before it can hash what it received — with the hash then
//! covering the envelope rather than the bytes.

use serde::{Deserialize, Serialize};

/// The header carrying the SHA-256 of the **logical (uncompressed)** bytes.
///
/// Logical, not stored: the client may send `Content-Encoding: zstd`, and the
/// server decompresses before checking. A hash of the compressed bytes would
/// make the same content hash differently depending on whether the client
/// compressed it, and then "is this chunk already there" has no stable answer.
pub const HEADER_CONTENT_SHA256: &str = "x-content-sha256";

/// The header carrying when the client read these bytes (client clock).
///
/// It is a header rather than a query parameter because it belongs to the body
/// it describes, and a query parameter is the part that ends up in access logs
/// and proxy logs.
pub const HEADER_OBSERVED_AT: &str = "x-observed-at";

/// The largest logical chunk the server accepts, in bytes.
///
/// 4 MiB matches the client's chunking (`docs/design/…` §5.3) with headroom:
/// the client's blocks are 4 MiB, so anything meaningfully larger is either a
/// client bug or an attempt to make the server buffer a lot of memory per
/// request. The server's own ceiling is higher (32 MiB, the DDL's
/// `byte_length` bound) to allow a single over-long line, but the default
/// request limit is this.
pub const MAX_CHUNK_BYTES: usize = 4 << 20;

/// `capture_files.role`, on the wire.
///
/// Duplicated from `cormex_store::FileRole` on purpose: this crate must not
/// depend on the storage layer (a Rust client depends on *just* this), and the
/// conversion is one `match` in cormexd where a new variant is a compile error
/// rather than a silently missing branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRole {
    Transcript,
    Subagent,
    Sidecar,
    Attachment,
    Memory,
    Row,
}

/// `capture_files.framing`, on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Framing {
    /// Only ever grows.
    Append,
    /// Rewritten wholesale each time.
    Snapshot,
}

/// `capture_generations.reason`, on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GenerationReason {
    Initial,
    Rewrite,
    Truncate,
    Snapshot,
}

/// `POST /capture/files`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterFileRequest {
    /// The catalog's agent id (`claude_code`, `codex`, …).
    pub agent: String,
    /// The host's own session id. Sessions are merged across devices on this.
    pub native_session_id: String,
    /// Set for a sub-agent's file, so its session points at the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_native_session_id: Option<String>,
    /// Relative to the agent's home, forward slashes, **plaintext** name (a
    /// `.jsonl.zst` and a `.jsonl` are the same logical file).
    pub logical_path: String,
    pub role: FileRole,
    pub framing: Framing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// `host/owner/repo`, from `git remote get-url origin`. `None` when the
    /// client has git metadata turned off (D7).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
}

/// The generation a file is currently on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationDto {
    pub id: String,
    pub ordinal: i32,
    /// How many logical bytes the generation holds. The client appends here.
    pub end_offset: i64,
}

/// `POST /capture/files` response.
///
/// **Idempotent**: registering a file that already exists returns the same ids
/// and the current newest generation, not an error. The client cannot know
/// whether it has registered this file before — it keeps that in a local SQLite
/// database that can be lost, and "I lost my state" must not mean "I cannot
/// upload".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterFileResponse {
    pub file_id: String,
    pub session_id: String,
    pub generation: GenerationDto,
}

/// `POST /capture/files/{id}/generations`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewGenerationRequest {
    pub reason: GenerationReason,
}

/// `PUT …/chunks/{offset}` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkAck {
    /// Where the generation now ends. The client's next append goes here.
    pub end_offset: i64,
    /// This exact chunk was already stored: a replay, not an error.
    ///
    /// A client that died between "the server accepted it" and "I recorded it
    /// as sent" retries on the next run. Answering 409 there would force it to
    /// tell a success apart from a failure for no reason.
    #[serde(default)]
    pub already_present: bool,
}

/// The 409 body for `PUT …/chunks/{offset}`.
///
/// # Why this is a type rather than an ad-hoc `json!`
///
/// The client **branches on it**: `expected_offset` greater than its own means
/// the server already has more than the client thought (skip ahead); less means
/// the client's state is ahead and it must rewind. Getting either direction
/// wrong is silent — the upload loop either stalls or re-sends forever — so the
/// shape is pinned by a test on both sides of the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OffsetMismatchBody {
    /// Always `"offset_mismatch"`. Present so that a client can tell this body
    /// apart from any other 409 without inspecting the status code alone.
    pub error: String,
    pub expected_offset: i64,
}

impl OffsetMismatchBody {
    /// The only value `error` ever takes.
    pub const ERROR: &'static str = "offset_mismatch";

    #[must_use]
    pub fn new(expected_offset: i64) -> Self {
        Self {
            error: Self::ERROR.to_owned(),
            expected_offset,
        }
    }
}

/// `POST /capture/files/{id}/events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEventRequest {
    pub op: FileEventOp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `capture_file_events.op`, on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileEventOp {
    Missing,
    Reappeared,
    Excluded,
    Included,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 409 body's JSON shape is a contract with the client.
    ///
    /// The client branches on `expected_offset` to decide whether to skip ahead
    /// or rewind, and both wrong directions are silent: the upload loop stalls,
    /// or it re-sends the same chunk forever. Pinning the exact shape here means
    /// a rename shows up as a test failure rather than as an upload that never
    /// finishes on somebody else's machine.
    #[test]
    fn the_offset_mismatch_body_has_a_stable_shape() {
        let json = serde_json::to_string(&OffsetMismatchBody::new(20_000)).expect("serialize");
        assert_eq!(
            json, r#"{"error":"offset_mismatch","expected_offset":20000}"#,
            "the 409 body changed shape -- clients branch on expected_offset"
        );
        let back: OffsetMismatchBody = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, OffsetMismatchBody::new(20_000));
    }

    /// Every DTO survives a round trip, and the enums use the same spellings as
    /// the database CHECKs.
    #[test]
    fn dtos_round_trip_with_the_database_spellings() {
        let req = RegisterFileRequest {
            agent: "claude_code".into(),
            native_session_id: "abc".into(),
            parent_native_session_id: None,
            logical_path: "projects/-home-u-proj/s.jsonl".into(),
            role: FileRole::Transcript,
            framing: Framing::Append,
            cwd: Some("/home/u/proj".into()),
            project_key: Some("github.com/u/proj".into()),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(
            json.contains(r#""role":"transcript""#) && json.contains(r#""framing":"append""#),
            "the wire spellings must match the migration's CHECK constraints, got {json}"
        );
        assert!(
            !json.contains("parent_native_session_id"),
            "an absent optional field must be omitted rather than sent as null: the server's \
             serde default handles both, but a null in the log is noise"
        );
        let back: RegisterFileRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.role, FileRole::Transcript);

        let ack = ChunkAck {
            end_offset: 23456,
            already_present: false,
        };
        let back: ChunkAck = serde_json::from_str(&serde_json::to_string(&ack).expect("serialize"))
            .expect("deserialize");
        assert_eq!(back.end_offset, 23456);

        for reason in [
            GenerationReason::Initial,
            GenerationReason::Rewrite,
            GenerationReason::Truncate,
            GenerationReason::Snapshot,
        ] {
            let json = serde_json::to_string(&reason).expect("serialize");
            assert_eq!(
                serde_json::from_str::<GenerationReason>(&json).expect("deserialize"),
                reason,
                "{json} did not round trip"
            );
        }
    }

    /// The header names are lowercase, as HTTP/2 requires and as axum's header
    /// map normalises to. A constant spelled `X-Content-SHA256` would still work
    /// for lookups (the map is case-insensitive) but would read wrong at every
    /// call site and in every log line.
    #[test]
    fn header_names_are_lowercase() {
        for h in [HEADER_CONTENT_SHA256, HEADER_OBSERVED_AT] {
            assert_eq!(
                h,
                h.to_ascii_lowercase(),
                "{h} must be lowercase: HTTP/2 requires it, and a mixed-case constant reads \
                 wrong everywhere it is used"
            );
        }
    }
}

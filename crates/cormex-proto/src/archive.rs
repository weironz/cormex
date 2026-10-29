//! The archive browsing wire types.
//!
//! Design: docs/design/session-archive-architecture.md §7.2.
//!
//! # Why the DTOs live here rather than in the server
//!
//! The same reason as every other wire type: the Web client and the CLI both
//! read these, and a field that silently goes missing between the server and a
//! client is not a compile error — `serde` ignores unknown fields, so the UI
//! simply draws one row fewer.

use serde::{Deserialize, Serialize};

use crate::transcript::TranscriptEventDto;

/// One row of `GET /archive/sessions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveSessionDto {
    pub id: String,
    pub agent: String,
    /// The first user message's opening, until phase 5's summaries exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    /// Every device that has contributed a file to this session.
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<String>,
    pub event_count: i64,
    /// One line, once phase 5's summaries exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// **Cormex holds the only copy.**
    ///
    /// True when every file of this session, on every device, has `missing` as
    /// its newest lifecycle event — the host deleted them and this is all that
    /// is left. The UI has to say so: a user about to delete a session from
    /// here is deleting the last copy.
    pub sole_copy: bool,
}

/// `GET /archive/sessions`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveSessionsResponse {
    pub sessions: Vec<ArchiveSessionDto>,
    /// The cursor for the next page. `None` = this is the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// One file of a session, with its generations.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveFileDto {
    pub id: String,
    pub role: String,
    pub device_id: String,
    pub logical_path: String,
    pub generations: Vec<ArchiveGenerationDto>,
}

/// One generation of a file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveGenerationDto {
    pub id: String,
    pub ordinal: i32,
    pub reason: String,
    /// How many logical bytes it holds.
    pub end_offset: i64,
    pub created_at: String,
}

/// `GET /archive/sessions/{id}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveSessionDetail {
    pub session: ArchiveSessionDto,
    pub files: Vec<ArchiveFileDto>,
    /// Which parser produced the events the caller will see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parser_version: Option<String>,
    /// `ok` / `partial` / `failed`, from the newest projection row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_status: Option<String>,
    /// The error a `failed` projection recorded, so the UI can say why rather
    /// than showing an empty conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection_error: Option<String>,
}

/// `GET /archive/sessions/{id}/events`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArchiveEventsResponse {
    pub events: Vec<TranscriptEventDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

/// How much of a tool result the `digest` view shows.
///
/// 2 KiB is roughly a screenful of monospace text. The point of the digest view
/// is to skim a conversation, and a 40 KB build log defeats that — the full
/// text is one click away in `transcript`, and the original is always in `raw`.
pub const DIGEST_TOOL_RESULT_BYTES: usize = 2 * 1024;

/// How long a derived session title is.
pub const TITLE_CHARS: usize = 80;

#[cfg(test)]
mod tests {
    use super::*;

    /// Absent optional fields are omitted rather than sent as null.
    #[test]
    fn absent_optional_fields_are_omitted() {
        let s = ArchiveSessionDto {
            id: "01S".into(),
            agent: "claude_code".into(),
            title: None,
            project_key: None,
            devices: vec![],
            first_at: None,
            last_at: None,
            event_count: 0,
            summary: None,
            sole_copy: false,
        };
        let json = serde_json::to_string(&s).expect("serialize");
        assert!(
            !json.contains("null"),
            "an absent field was sent as null: {json}"
        );
        assert!(
            json.contains(r#""sole_copy":false"#),
            "sole_copy is not optional -- a client must not have to guess whether the archive \
             is the only copy: {json}"
        );
    }

    /// The digest's tool-result cap is the one the design names.
    #[test]
    fn the_digest_cap_is_two_kib() {
        assert_eq!(DIGEST_TOOL_RESULT_BYTES, 2048);
        assert_eq!(TITLE_CHARS, 80);
    }
}

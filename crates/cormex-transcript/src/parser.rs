//! The parser interface and the shared event shape.
//!
//! # Why `feed` takes an offset rather than a stream
//!
//! The projector reads chunks from the raw layer, and a chunk is whatever the
//! client uploaded — 4 MiB, or less if the file is smaller. A parser that
//! consumed a stream would have to be handed the whole file, which is exactly
//! what the chunked design exists to avoid.
//!
//! Taking the offset also makes the chunk-invariance property **expressible**:
//! "feeding the same bytes split at any position gives the same events" is a
//! test that a stream-based interface cannot even state.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};

/// One event a parser produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedEvent {
    pub kind: TranscriptKind,
    /// `user` / `assistant` / …, when the host's record names one.
    pub role: Option<String>,
    /// The event-time axis (`at`). `None` when the record carried no timestamp.
    pub occurred_at: Option<DateTime<Utc>>,
    pub content: Vec<ContentBlock>,
    /// Plain text for display and search.
    pub text: String,
    pub tool_name: Option<String>,
    pub tool_call_id: Option<String>,
    /// Where in the logical file this record came from.
    pub src_offset: u64,
    pub src_length: u64,
}

impl ParsedEvent {
    /// An event with no content, for a record the parser did not recognise.
    ///
    /// **Not dropped.** In a real session, `attachment` / `last-prompt` /
    /// `atis-latch` / `mode` / … make up more than half the lines; discarding
    /// them would make the projection a summary of a conversation rather than a
    /// record of it.
    #[must_use]
    pub fn unknown(raw_type: &str, offset: u64, length: u64) -> Self {
        Self {
            kind: TranscriptKind::Unknown,
            role: None,
            occurred_at: None,
            content: vec![ContentBlock::Unknown {
                raw_type: raw_type.to_owned(),
            }],
            text: String::new(),
            tool_name: None,
            tool_call_id: None,
            src_offset: offset,
            src_length: length,
        }
    }

    /// An event for a line that is not valid JSON at all.
    ///
    /// Kept with its offset so the raw view can show what it was: a truncated
    /// write, a host that changed its format mid-session, or a genuinely
    /// corrupt byte. Silently skipping it would make the projection shorter
    /// than the file with nothing to say why.
    #[must_use]
    pub fn malformed(offset: u64, length: u64) -> Self {
        Self {
            kind: TranscriptKind::Unknown,
            role: None,
            occurred_at: None,
            content: vec![ContentBlock::Unknown {
                raw_type: "malformed-json".to_owned(),
            }],
            text: String::new(),
            tool_name: None,
            tool_call_id: None,
            src_offset: offset,
            src_length: length,
        }
    }
}

/// A parser for one agent's session format.
///
/// One instance per file: the state (the line buffer, the tool-name map) is
/// per-file, and sharing it across files would mix two conversations' ids.
pub trait Parser: Send + Sync {
    /// The version string, e.g. `claude_code@1`.
    ///
    /// It is stored with every projected event, so a parser change means a new
    /// projection rather than an in-place edit — the raw bytes are kept, so the
    /// old one can always be recomputed.
    fn version(&self) -> &'static str;

    /// Feed the next contiguous logical bytes starting at `offset`.
    ///
    /// Returns the events that are now complete. A line that is not finished
    /// yet stays buffered; the caller feeds the rest later.
    ///
    /// # Errors
    /// The bytes do not start where the parser expects, which means the caller
    /// skipped a chunk.
    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String>;

    /// The state needed to resume.
    ///
    /// A `serde_json::Value` rather than a typed struct: the projector stores
    /// it in `transcript_projections.through`, and a typed struct would make
    /// every parser's state a schema migration.
    fn checkpoint(&self) -> serde_json::Value;
}

/// The parser for an agent and file role, or `None` when there is none.
///
/// `None` is not an error: an agent the catalog knows about may have a file
/// role no parser handles yet, and the projector records "no parser" rather
/// than failing the session.
#[must_use]
pub fn parser_for(agent: &str, role: cormex_proto::capture::FileRole) -> Option<Box<dyn Parser>> {
    use cormex_proto::capture::FileRole;
    match (agent, role) {
        // Only the conversation-bearing roles have parsers. A sidecar is a
        // tool's spilled output, which the archive index covers; an attachment
        // is bytes with no structure to project.
        ("claude_code", FileRole::Transcript | FileRole::Subagent) => {
            Some(Box::new(crate::claude_code::ClaudeCodeParser::new()))
        }
        ("codex", FileRole::Transcript) => Some(Box::new(crate::codex::CodexParser::new())),
        ("gemini_cli", FileRole::Transcript | FileRole::Subagent) => {
            Some(Box::new(crate::gemini::GeminiParser::new()))
        }
        // Cline and Roo Code are the same fork; one parser serves both
        ("cline" | "roo_code", FileRole::Transcript) => {
            Some(Box::new(crate::cline::ClineParser::new()))
        }
        // SQLite rows (P4-5). One row is one logical file, so the role is `Row`
        ("opencode", FileRole::Row) => Some(Box::new(crate::rows::RowParser::new(
            crate::rows::RowFormat::OpenCode,
        ))),
        ("goose", FileRole::Row) => Some(Box::new(crate::rows::RowParser::new(
            crate::rows::RowFormat::Goose,
        ))),
        // VS Code Copilot Chat (P4-3): a mutation log, replayed before it can
        // be projected
        ("copilot_chat", FileRole::Transcript) => {
            Some(Box::new(crate::copilot_chat::CopilotChatParser::new()))
        }
        // Qwen Code (P4-6). Same family as Gemini CLI, different record shape
        ("qwen_code", FileRole::Transcript) => Some(Box::new(crate::qwen::QwenParser::new())),
        // Export files (P4-7). The client splits `conversations.json` into one
        // file per conversation before uploading, so one file is one session
        ("chatgpt_export", FileRole::Transcript) => Some(Box::new(
            crate::export::ExportParser::new(crate::export::ExportPlatform::ChatGpt),
        )),
        ("claude_export", FileRole::Transcript) => Some(Box::new(
            crate::export::ExportParser::new(crate::export::ExportPlatform::Claude),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cormex_proto::capture::FileRole;

    #[test]
    fn the_known_agents_have_parsers_for_their_conversation_roles() {
        assert!(parser_for("claude_code", FileRole::Transcript).is_some());
        assert!(parser_for("claude_code", FileRole::Subagent).is_some());
        assert!(parser_for("codex", FileRole::Transcript).is_some());
        assert!(parser_for("gemini_cli", FileRole::Transcript).is_some());
        assert!(parser_for("gemini_cli", FileRole::Subagent).is_some());
    }

    /// A role with no parser is `None`, not a panic.
    ///
    /// An attachment is bytes with no structure; a sidecar is tool output the
    /// archive index covers. The projector records "no parser" rather than
    /// failing the session.
    #[test]
    fn a_role_with_no_parser_is_none() {
        assert!(parser_for("claude_code", FileRole::Attachment).is_none());
        assert!(parser_for("codex", FileRole::Sidecar).is_none());
        assert!(parser_for("unknown_agent", FileRole::Transcript).is_none());
    }

    #[test]
    fn an_unknown_event_keeps_its_original_type() {
        let e = ParsedEvent::unknown("atis-latch", 100, 20);
        assert_eq!(e.kind, TranscriptKind::Unknown);
        assert_eq!(
            e.content,
            vec![ContentBlock::Unknown {
                raw_type: "atis-latch".into()
            }],
            "the original type name must survive: it is what makes the record searchable and \
             what tells the next person which record types still need a parser"
        );
        assert_eq!(e.src_offset, 100);
        assert_eq!(e.src_length, 20);
    }
}

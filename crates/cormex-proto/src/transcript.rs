//! The P layer's wire types: a normalized conversation event.
//!
//! Design: docs/design/session-archive-architecture.md §7.2 (D11).
//!
//! # Aligned with ACP's content blocks, not with any one host's format
//!
//! Twenty agents write twenty different shapes. The projection's job is to
//! reduce them to one, and the one it reduces to is the Agent Client Protocol's
//! content blocks — a shape that already exists in the world, is documented, and
//! is what other tools are converging on. Inventing a private one would mean
//! every future host needs a translation written twice.
//!
//! # `Unknown` is a variant, not a reason to drop a record
//!
//! In a real Claude Code session, `user` and `assistant` are **less than half**
//! the lines: `attachment`, `last-prompt`, `atis-latch`, `mode`, `tools-used`,
//! `queue-operation`, `cost-state` and more make up the rest. A parser that
//! only recognises the two conversation types would treat more than half its
//! input as garbage — and the fixture would then be a test of the error path.
//! So an unrecognised record becomes [`ContentBlock::Unknown`] with its original
//! type name, which keeps it visible and searchable.

use serde::{Deserialize, Serialize};

/// What kind of event this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    /// Something the person said.
    UserMessage,
    /// Something the agent said.
    AgentMessage,
    /// The agent's reasoning, where the host exposes it.
    AgentThought,
    /// A tool invocation.
    ToolCall,
    /// A tool's output.
    ToolResult,
    /// A plan or todo list.
    Plan,
    /// A system-level record (compaction, a mode change, a summary).
    System,
    /// A record that carries structure the UI does not render, but that is part
    /// of the session.
    Meta,
    /// A record the parser does not understand.
    Unknown,
}

impl TranscriptKind {
    /// The spelling stored in `transcript_events.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::AgentMessage => "agent_message",
            Self::AgentThought => "agent_thought",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Plan => "plan",
            Self::System => "system",
            Self::Meta => "meta",
            Self::Unknown => "unknown",
        }
    }
}

/// One piece of an event's content.
///
/// `serde`'s internal tagging (`type`) matches ACP's own encoding, so a client
/// that already speaks ACP can read these without a translation layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text.
    Text { text: String },
    /// The agent's reasoning.
    Thought { text: String },
    /// A tool invocation.
    ToolCall {
        tool_call_id: String,
        name: String,
        /// The arguments, as the host recorded them. Kept as JSON rather than
        /// a string: the UI renders different tools differently, and a string
        /// would force it to re-parse.
        input: serde_json::Value,
    },
    /// A tool's output.
    ToolResult {
        tool_call_id: String,
        output: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A file edit.
    Diff {
        path: String,
        old: String,
        new: String,
    },
    /// A plan.
    Plan { entries: Vec<String> },
    /// An image, by the blob it was stored as.
    Image { blob_ref: String },
    /// A block too large to inline: a pointer back to the raw layer.
    ///
    /// The raw bytes are always kept, so a large tool output is a reference
    /// rather than a copy. That is what makes "the original is always readable"
    /// true without storing everything twice.
    ResourceRef {
        generation_id: String,
        offset: i64,
        length: i64,
        mime: String,
    },
    /// A record the parser did not understand, with its original type name.
    Unknown { raw_type: String },
}

/// Where an event came from in the raw layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub generation_id: String,
    pub offset: i64,
    pub length: i64,
}

/// One normalized event, as `GET /archive/sessions/{id}/events` returns it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEventDto {
    pub id: String,
    /// Position within the session's projection. Monotonic per
    /// `(session, parser_version)`.
    pub seq: i64,
    pub kind: TranscriptKind,
    /// The event-time axis (`at`). `None` when the host's record carried no
    /// timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub content: Vec<ContentBlock>,
    /// Plain text for display and search. The masked form: suspected secrets
    /// have already been replaced (D5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub source: SourceSpan,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The content blocks use ACP's `type` tag.
    #[test]
    fn content_blocks_are_tagged_the_way_acp_tags_them() {
        let block = ContentBlock::ToolCall {
            tool_call_id: "call-1".into(),
            name: "Bash".into(),
            input: serde_json::json!({ "command": "ls" }),
        };
        let json = serde_json::to_string(&block).expect("serialize");
        assert!(
            json.contains(r#""type":"tool_call""#),
            "the tag must be `type`, matching ACP: {json}"
        );
        let back: ContentBlock = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, block);
    }

    /// Every block variant round-trips.
    #[test]
    fn every_block_variant_round_trips() {
        let blocks = vec![
            ContentBlock::Text {
                text: "hello".into(),
            },
            ContentBlock::Thought {
                text: "thinking".into(),
            },
            ContentBlock::ToolCall {
                tool_call_id: "c".into(),
                name: "Read".into(),
                input: serde_json::json!({}),
            },
            ContentBlock::ToolResult {
                tool_call_id: "c".into(),
                output: "out".into(),
                is_error: true,
            },
            ContentBlock::Diff {
                path: "/p".into(),
                old: "a".into(),
                new: "b".into(),
            },
            ContentBlock::Plan {
                entries: vec!["one".into()],
            },
            ContentBlock::Image {
                blob_ref: "abc".into(),
            },
            ContentBlock::ResourceRef {
                generation_id: "01G".into(),
                offset: 0,
                length: 10,
                mime: "text/plain".into(),
            },
            ContentBlock::Unknown {
                raw_type: "atis-latch".into(),
            },
        ];
        for b in blocks {
            let json = serde_json::to_string(&b).expect("serialize");
            let back: ContentBlock = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, b, "{json} did not round trip");
        }
    }

    /// The `kind` spellings match the migration's CHECK.
    #[test]
    fn the_kind_spellings_match_the_ddl() {
        // `transcript_events.kind` accepts exactly these
        let expected = [
            "user_message",
            "agent_message",
            "agent_thought",
            "tool_call",
            "tool_result",
            "plan",
            "system",
            "meta",
            "unknown",
        ];
        for k in [
            TranscriptKind::UserMessage,
            TranscriptKind::AgentMessage,
            TranscriptKind::AgentThought,
            TranscriptKind::ToolCall,
            TranscriptKind::ToolResult,
            TranscriptKind::Plan,
            TranscriptKind::System,
            TranscriptKind::Meta,
            TranscriptKind::Unknown,
        ] {
            assert!(
                expected.contains(&k.as_str()),
                "{} is not in the DDL's CHECK list",
                k.as_str()
            );
        }
    }

    /// Absent optional fields are omitted rather than sent as null.
    #[test]
    fn absent_optional_fields_are_omitted() {
        let e = TranscriptEventDto {
            id: "01E".into(),
            seq: 0,
            kind: TranscriptKind::Meta,
            occurred_at: None,
            role: None,
            content: vec![],
            text: None,
            tool_name: None,
            tool_call_id: None,
            source: SourceSpan {
                generation_id: "01G".into(),
                offset: 0,
                length: 5,
            },
        };
        let json = serde_json::to_string(&e).expect("serialize");
        assert!(
            !json.contains("null"),
            "an absent field was sent as null: {json}"
        );
    }
}

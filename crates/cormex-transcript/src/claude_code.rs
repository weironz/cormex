//! The Claude Code transcript parser.
//!
//! Design: docs/design/session-archive-architecture.md §7.2, plan P2-1.
//!
//! # The record types, and why most of them are not messages
//!
//! A real session (1455 lines, counted 2026-10-29) is:
//!
//! ```text
//! assistant 487 · attachment 375 · user 287 · last-prompt 103 · atis-latch 102
//! mode 62 · tools-used 16 · queue-operation 14 · system 6 · cost-state 3
//! ```
//!
//! Recognising only `user` / `assistant` would treat more than half the input
//! as garbage. Every type therefore lands somewhere: a message, a thought, a
//! tool call, a system record, or [`TranscriptKind::Meta`] — and a type this
//! parser has never seen becomes `Unknown` **with its name**, so the next
//! person can see which types still need handling.
//!
//! # `tool_result` arrives in a `user` message
//!
//! Claude Code records a tool's output as a `user` message whose content is a
//! `tool_result` block. It is not something the person said, so it becomes a
//! [`TranscriptKind::ToolResult`] event rather than a user message — and the
//! tool's **name** is not in that record, only its id. The name is recovered
//! from the `tool_use` that preceded it, which is why the parser keeps a map
//! and why that map is in the checkpoint.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct ClaudeCodeParser {
    lines: LineBuffer,
    /// `tool_use_id` → tool name, so a `tool_result` can carry the name its own
    /// record does not have.
    tool_names: std::collections::BTreeMap<String, String>,
}

impl ClaudeCodeParser {
    /// A parser starting at offset 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
            tool_names: std::collections::BTreeMap::new(),
        }
    }

    /// Parse one complete line.
    fn parse_line(&mut self, offset: u64, length: u64, bytes: &[u8]) -> ParsedEvent {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            return ParsedEvent::malformed(offset, length);
        };
        let record_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        let occurred_at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));

        match record_type {
            "user" | "assistant" => self.parse_message(&v, offset, length, occurred_at),
            "system" | "summary" => ParsedEvent {
                kind: TranscriptKind::System,
                role: None,
                occurred_at,
                content: text_blocks(v.get("content").or_else(|| v.get("summary"))),
                text: flatten_text(v.get("content").or_else(|| v.get("summary"))),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            },
            // Everything else is a record the UI does not render but that is
            // part of the session. `Meta` rather than `Unknown`: these are
            // types this parser knows about and deliberately does not turn into
            // messages. A type it does *not* know becomes `Unknown`, below
            "attachment"
            | "last-prompt"
            | "atis-latch"
            | "mode"
            | "tools-used"
            | "queue-operation"
            | "cost-state"
            | "ai-title"
            | "file-history-delta"
            | "file-history-snapshot"
            | "permission-mode"
            | "pr-link"
            | "api-request"
            | "api-request-blob"
            | "api-request-shape" => ParsedEvent {
                kind: TranscriptKind::Meta,
                role: None,
                occurred_at,
                content: vec![ContentBlock::Unknown {
                    raw_type: record_type.to_owned(),
                }],
                text: String::new(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            },
            other => ParsedEvent::unknown(other, offset, length),
        }
    }

    fn parse_message(
        &mut self,
        v: &Value,
        offset: u64,
        length: u64,
        occurred_at: Option<DateTime<Utc>>,
    ) -> ParsedEvent {
        let role = v
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .or_else(|| v.get("type").and_then(Value::as_str))
            .map(str::to_owned);
        let content = v.get("message").and_then(|m| m.get("content"));

        // `content` is a string in some records and an array of blocks in
        // others. Both are real; a parser that assumes one drops the other
        let (blocks, tool_name, tool_call_id, kind) = match content {
            Some(Value::String(s)) => (
                vec![ContentBlock::Text { text: s.clone() }],
                None,
                None,
                message_kind(role.as_deref()),
            ),
            Some(Value::Array(items)) => self.parse_blocks(items),
            _ => (Vec::new(), None, None, message_kind(role.as_deref())),
        };

        ParsedEvent {
            kind,
            role,
            occurred_at,
            text: flatten_text(content),
            content: blocks,
            tool_name,
            tool_call_id,
            src_offset: offset,
            src_length: length,
        }
    }

    /// Turn a content-block array into blocks, recording tool names.
    fn parse_blocks(
        &mut self,
        items: &[Value],
    ) -> (
        Vec<ContentBlock>,
        Option<String>,
        Option<String>,
        TranscriptKind,
    ) {
        let mut blocks = Vec::new();
        let mut first_tool_name = None;
        let mut first_tool_id = None;
        let mut kind = TranscriptKind::AgentMessage;

        for item in items {
            match item.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    let text = item
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    blocks.push(ContentBlock::Text { text });
                }
                "thinking" => {
                    let text = item
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    blocks.push(ContentBlock::Thought { text });
                    kind = TranscriptKind::AgentThought;
                }
                "tool_use" => {
                    let id = item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let input = item.get("input").cloned().unwrap_or(Value::Null);
                    // Record the name so the matching `tool_result` can carry
                    // it: that record has only the id
                    self.tool_names.insert(id.clone(), name.clone());
                    if first_tool_name.is_none() {
                        first_tool_name = Some(name.clone());
                        first_tool_id = Some(id.clone());
                    }
                    blocks.push(ContentBlock::ToolCall {
                        tool_call_id: id,
                        name,
                        input,
                    });
                    kind = TranscriptKind::ToolCall;
                }
                "tool_result" => {
                    let id = item
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let output = flatten_value(item.get("content"));
                    let is_error = item
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if first_tool_name.is_none() {
                        first_tool_name = self.tool_names.get(&id).cloned();
                        first_tool_id = Some(id.clone());
                    }
                    blocks.push(ContentBlock::ToolResult {
                        tool_call_id: id,
                        output,
                        is_error,
                    });
                    kind = TranscriptKind::ToolResult;
                }
                "image" => {
                    let blob_ref = item
                        .get("source")
                        .and_then(|s| s.get("data"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    blocks.push(ContentBlock::Image { blob_ref });
                }
                other => blocks.push(ContentBlock::Unknown {
                    raw_type: other.to_owned(),
                }),
            }
        }
        (blocks, first_tool_name, first_tool_id, kind)
    }
}

impl Default for ClaudeCodeParser {
    fn default() -> Self {
        Self::new()
    }
}

/// `user` → a user message; anything else → an agent message.
fn message_kind(role: Option<&str>) -> TranscriptKind {
    match role {
        Some("user") => TranscriptKind::UserMessage,
        _ => TranscriptKind::AgentMessage,
    }
}

fn text_blocks(content: Option<&Value>) -> Vec<ContentBlock> {
    match content {
        Some(Value::String(s)) => vec![ContentBlock::Text { text: s.clone() }],
        _ => Vec::new(),
    }
}

/// Flatten a content value into plain text.
///
/// `text` is what the search index and the UI's one-line preview use, so it has
/// to be the readable form rather than a JSON dump.
fn flatten_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                Some("text") => i.get("text").and_then(Value::as_str).map(str::to_owned),
                Some("thinking") => i.get("thinking").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn flatten_value(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| {
                i.get("text")
                    .and_then(Value::as_str)
                    .or_else(|| i.as_str())
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

impl Parser for ClaudeCodeParser {
    fn version(&self) -> &'static str {
        "claude_code@1"
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        let lines = self.lines.feed(offset, bytes).map_err(|e| e.to_string())?;
        Ok(lines
            .into_iter()
            .map(|l| {
                let len = u64::try_from(l.bytes.len()).unwrap_or(0);
                self.parse_line(l.offset, len, &l.bytes)
            })
            .collect())
    }

    fn checkpoint(&self) -> Value {
        let (next, start, partial) = self.lines.state();
        serde_json::json!({
            "next_offset": next,
            "partial_start": start,
            // The partial line as a lossy string: a checkpoint is stored in
            // JSON, and a byte array would be a base64 blob nobody can read
            "partial": String::from_utf8_lossy(&partial),
            "tool_names": self.tool_names,
        })
    }
}

impl ClaudeCodeParser {
    /// Resume from a checkpoint.
    ///
    /// # Errors
    /// The checkpoint is not one this parser wrote.
    pub fn restore(state: &Value) -> Result<Self, String> {
        let next = state
            .get("next_offset")
            .and_then(Value::as_u64)
            .ok_or("the checkpoint has no next_offset")?;
        let start = state
            .get("partial_start")
            .and_then(Value::as_u64)
            .unwrap_or(next);
        let partial = state
            .get("partial")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        let tool_names = state
            .get("tool_names")
            .and_then(Value::as_object)
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            lines: LineBuffer::from_state(next, start, partial),
            tool_names,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(body: &[u8]) -> Vec<ParsedEvent> {
        let mut p = ClaudeCodeParser::new();
        p.feed(0, body).expect("feed")
    }

    #[test]
    fn a_user_message_becomes_a_user_event() {
        let body = br#"{"type":"user","message":{"role":"user","content":"hello"},"timestamp":"2026-10-29T08:00:00Z"}
"#;
        let events = feed_all(body);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[0].role.as_deref(), Some("user"));
        assert_eq!(events[0].text, "hello");
        assert!(events[0].occurred_at.is_some());
    }

    /// A `tool_result` in a user message is a tool result, not something the
    /// person said.
    #[test]
    fn a_tool_result_is_not_a_user_message() {
        let body = br#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"file contents"}]}}
"#;
        let events = feed_all(body);
        assert_eq!(
            events[0].kind,
            TranscriptKind::ToolResult,
            "a tool's output recorded in a user message is not something the person said"
        );
        assert_eq!(events[0].tool_call_id.as_deref(), Some("t1"));
    }

    /// The tool name is recovered from the earlier `tool_use`.
    ///
    /// The `tool_result` record carries only the id, so without the map the UI
    /// would show "tool result" with no idea which tool.
    #[test]
    fn a_tool_result_carries_the_name_from_its_call() {
        let body = br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/p"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
"#;
        let events = feed_all(body);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, TranscriptKind::ToolCall);
        assert_eq!(events[0].tool_name.as_deref(), Some("Read"));
        assert_eq!(
            events[1].tool_name.as_deref(),
            Some("Read"),
            "the tool result lost the tool's name; the record itself has only the id, so the \
             name has to come from the call"
        );
    }

    #[test]
    fn thinking_becomes_a_thought() {
        let body = br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"hmm"}]}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::AgentThought);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Thought { text: "hmm".into() }]
        );
    }

    /// The non-message record types become `Meta`, not `Unknown`.
    ///
    /// They are more than half of a real session, and they are types this
    /// parser knows about and deliberately does not turn into messages.
    #[test]
    fn the_known_non_message_types_become_meta() {
        for t in [
            "attachment",
            "last-prompt",
            "atis-latch",
            "mode",
            "tools-used",
            "queue-operation",
            "cost-state",
        ] {
            let body = format!("{{\"type\":\"{t}\"}}\n");
            let events = feed_all(body.as_bytes());
            assert_eq!(
                events[0].kind,
                TranscriptKind::Meta,
                "{t} should be Meta, not Unknown"
            );
        }
    }

    /// A type this parser has never seen becomes `Unknown` **with its name**.
    #[test]
    fn an_unrecognised_type_keeps_its_name() {
        let body = br#"{"type":"some-future-record"}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "some-future-record".into()
            }]
        );
    }

    /// A line that is not JSON becomes an `Unknown` event, not a dropped line.
    #[test]
    fn a_malformed_line_is_kept() {
        let body = b"not json at all\n";
        let events = feed_all(body);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        assert_eq!(
            events[0].src_offset, 0,
            "the offset is what lets the raw view show what the line was"
        );
    }

    /// Splitting at every position gives the same events.
    ///
    /// The chunk-invariance property. A 4 MiB chunk boundary lands wherever it
    /// lands, so a parser that is correct for one split and wrong for another
    /// has a bug that appears only on some sessions.
    #[test]
    fn every_split_position_gives_the_same_events() {
        let body = br#"{"type":"user","message":{"role":"user","content":"a"},"timestamp":"2026-10-29T08:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"b"}]}}
{"type":"mode"}
"#;
        let expected = feed_all(body);
        for split in 0..=body.len() {
            let mut p = ClaudeCodeParser::new();
            let mut got = p.feed(0, &body[..split]).expect("feed");
            got.extend(p.feed(split as u64, &body[split..]).expect("feed"));
            assert_eq!(
                got, expected,
                "splitting at byte {split} changed the events"
            );
        }
    }

    /// A checkpoint restores enough to continue identically.
    #[test]
    fn a_checkpoint_resumes_identically() {
        let body = br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Read","input":{}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
"#;
        let expected = feed_all(body);

        // Feed the first line, checkpoint, restore, feed the rest
        let first_end = body.iter().position(|b| *b == b'\n').expect("a newline") + 1;
        let mut p = ClaudeCodeParser::new();
        let first = p.feed(0, &body[..first_end]).expect("feed");
        let checkpoint = p.checkpoint();
        let mut resumed = ClaudeCodeParser::restore(&checkpoint).expect("restore");
        let mut got = first;
        got.extend(
            resumed
                .feed(first_end as u64, &body[first_end..])
                .expect("feed"),
        );

        assert_eq!(got, expected);
        assert_eq!(
            got[1].tool_name.as_deref(),
            Some("Read"),
            "the tool-name map must be in the checkpoint: without it a resumed projection loses \
             the name on every tool result after the resume point"
        );
    }

    /// A checkpoint taken mid-line resumes mid-line.
    #[test]
    fn a_checkpoint_taken_mid_line_resumes_mid_line() {
        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let expected = feed_all(body);
        let split = 20;

        let mut p = ClaudeCodeParser::new();
        assert!(p.feed(0, &body[..split]).expect("feed").is_empty());
        let checkpoint = p.checkpoint();
        let mut resumed = ClaudeCodeParser::restore(&checkpoint).expect("restore");
        let got = resumed.feed(split as u64, &body[split..]).expect("feed");
        assert_eq!(got, expected);
    }
}

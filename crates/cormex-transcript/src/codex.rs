//! The Codex rollout parser.
//!
//! Design: docs/design/session-archive-architecture.md §7.2, plan P2-1.
//!
//! # The shape
//!
//! A rollout is JSONL of `{ timestamp, type, payload }`, where `type` is one of
//! `session_meta`, `turn_context`, `response_item`, `event_msg`, `compacted`.
//! The conversation itself is in `response_item`: `message`, `reasoning`,
//! `function_call`, `function_call_output`, `local_shell_call`.
//!
//! # `event_msg` is a duplicate, and is deliberately `Meta`
//!
//! Codex writes both a `response_item` and an `event_msg` for the same thing —
//! the user's message appears as both. Projecting both would show every message
//! twice, which reads as a parser bug and is not one. `event_msg` therefore
//! becomes `Meta`: kept, searchable, not rendered.
//!
//! # Field names follow `openai/codex`'s `codex-rs/protocol`
//!
//! Apache-2.0, and the types are read from upstream rather than guessed. The
//! parser is written here rather than copied: the upstream types are deeply
//! coupled to `codex-protocol`, and vendoring them would drag that in for five
//! structs.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct CodexParser {
    lines: LineBuffer,
}

impl CodexParser {
    /// A parser starting at offset 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
        }
    }

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
        let payload = v.get("payload").cloned().unwrap_or(Value::Null);

        match record_type {
            "response_item" => self.parse_response_item(&payload, offset, length, occurred_at),
            "compacted" => ParsedEvent {
                kind: TranscriptKind::System,
                role: None,
                occurred_at,
                content: vec![ContentBlock::Text {
                    text: payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }],
                text: payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            },
            // `session_meta` / `turn_context` / `event_msg`: structure the
            // session needs but the UI does not render. `event_msg` in
            // particular duplicates a `response_item`, and projecting both
            // would show every message twice
            "session_meta" | "turn_context" | "event_msg" => ParsedEvent {
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

    fn parse_response_item(
        &mut self,
        payload: &Value,
        offset: u64,
        length: u64,
        occurred_at: Option<DateTime<Utc>>,
    ) -> ParsedEvent {
        let item_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let mut event = ParsedEvent {
            kind: TranscriptKind::Meta,
            role: None,
            occurred_at,
            content: Vec::new(),
            text: String::new(),
            tool_name: None,
            tool_call_id: None,
            src_offset: offset,
            src_length: length,
        };

        match item_type {
            "message" => {
                let role = payload
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or("assistant")
                    .to_owned();
                let text = flatten_content(payload.get("content"));
                event.kind = if role == "user" {
                    TranscriptKind::UserMessage
                } else {
                    TranscriptKind::AgentMessage
                };
                event.content = vec![ContentBlock::Text { text: text.clone() }];
                event.text = text;
                event.role = Some(role);
            }
            "reasoning" => {
                let text = flatten_content(payload.get("summary"))
                    .is_empty()
                    .then(|| flatten_content(payload.get("content")))
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| flatten_content(payload.get("summary")));
                event.kind = TranscriptKind::AgentThought;
                event.content = vec![ContentBlock::Thought { text: text.clone() }];
                event.text = text;
            }
            "function_call" => {
                let id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = payload
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                // `arguments` is a JSON **string** in Codex's protocol, not an
                // object. Passing it through as a string would make the UI
                // re-parse it; parsing it here would fail on a malformed one.
                // It is kept as a string inside the JSON value, which is honest
                let input = payload.get("arguments").cloned().unwrap_or(Value::Null);
                event.kind = TranscriptKind::ToolCall;
                event.content = vec![ContentBlock::ToolCall {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    input,
                }];
                event.tool_name = Some(name);
                event.tool_call_id = Some(id);
            }
            "function_call_output" => {
                let id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let output = payload.get("output").map(flatten_value).unwrap_or_default();
                event.kind = TranscriptKind::ToolResult;
                event.content = vec![ContentBlock::ToolResult {
                    tool_call_id: id.clone(),
                    output: output.clone(),
                    is_error: false,
                }];
                event.text = output;
                event.tool_call_id = Some(id);
            }
            "local_shell_call" => {
                let id = payload
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let action = payload.get("action").cloned().unwrap_or(Value::Null);
                event.kind = TranscriptKind::ToolCall;
                event.content = vec![ContentBlock::ToolCall {
                    tool_call_id: id.clone(),
                    name: "local_shell".to_owned(),
                    input: action,
                }];
                event.tool_name = Some("local_shell".to_owned());
                event.tool_call_id = Some(id);
            }
            other => {
                event.kind = TranscriptKind::Unknown;
                event.content = vec![ContentBlock::Unknown {
                    raw_type: format!("response_item:{other}"),
                }];
            }
        }
        event
    }
}

impl Default for CodexParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Flatten a Codex content array (`input_text` / `output_text` / `summary_text`).
fn flatten_content(v: Option<&Value>) -> String {
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
        _ => String::new(),
    }
}

fn flatten_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

impl Parser for CodexParser {
    fn version(&self) -> &'static str {
        "codex@1"
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
            "partial": String::from_utf8_lossy(&partial),
        })
    }
}

impl CodexParser {
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
        Ok(Self {
            lines: LineBuffer::from_state(next, start, partial),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(body: &[u8]) -> Vec<ParsedEvent> {
        let mut p = CodexParser::new();
        p.feed(0, body).expect("feed")
    }

    #[test]
    fn a_user_message_becomes_a_user_event() {
        let body = br#"{"timestamp":"2026-10-29T08:00:00Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[0].text, "hello");
        assert_eq!(events[0].role.as_deref(), Some("user"));
    }

    #[test]
    fn an_assistant_message_becomes_an_agent_event() {
        let body = br#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hi"}]}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::AgentMessage);
    }

    #[test]
    fn reasoning_becomes_a_thought() {
        let body = br#"{"type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"hmm"}]}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::AgentThought);
        assert_eq!(events[0].text, "hmm");
    }

    #[test]
    fn a_function_call_and_its_output_are_linked() {
        let body = br#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"c1"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"a.txt"}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::ToolCall);
        assert_eq!(events[0].tool_name.as_deref(), Some("shell"));
        assert_eq!(events[0].tool_call_id.as_deref(), Some("c1"));
        assert_eq!(events[1].kind, TranscriptKind::ToolResult);
        assert_eq!(
            events[1].tool_call_id.as_deref(),
            Some("c1"),
            "the output must carry the id its call had, or the UI cannot pair them"
        );
    }

    #[test]
    fn a_local_shell_call_becomes_a_tool_call() {
        let body = br#"{"type":"response_item","payload":{"type":"local_shell_call","call_id":"c2","action":{"type":"exec","command":["pwd"]}}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::ToolCall);
        assert_eq!(events[0].tool_name.as_deref(), Some("local_shell"));
    }

    /// `event_msg` is `Meta`, because it duplicates a `response_item`.
    ///
    /// Codex writes both for the same message; projecting both would show every
    /// message twice, which reads as a parser bug.
    #[test]
    fn event_msg_is_meta_not_a_duplicate_message() {
        let body = br#"{"type":"event_msg","payload":{"type":"user_message","message":"hello"}}
"#;
        let events = feed_all(body);
        assert_eq!(
            events[0].kind,
            TranscriptKind::Meta,
            "event_msg duplicates a response_item; projecting it as a message would show every \
             message twice"
        );
    }

    #[test]
    fn session_meta_and_turn_context_are_meta() {
        for t in ["session_meta", "turn_context"] {
            let body = format!("{{\"type\":\"{t}\",\"payload\":{{}}}}\n");
            let events = feed_all(body.as_bytes());
            assert_eq!(events[0].kind, TranscriptKind::Meta, "{t}");
        }
    }

    #[test]
    fn compacted_is_system() {
        let body = br#"{"type":"compacted","payload":{"message":"summary of what happened"}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::System);
        assert_eq!(events[0].text, "summary of what happened");
    }

    #[test]
    fn an_unknown_record_type_keeps_its_name() {
        let body = br#"{"type":"some-future-thing","payload":{}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "some-future-thing".into()
            }]
        );
    }

    #[test]
    fn an_unknown_response_item_keeps_its_name() {
        let body = br#"{"type":"response_item","payload":{"type":"web_search_call"}}
"#;
        let events = feed_all(body);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "response_item:web_search_call".into()
            }],
            "the name should say which level it was unknown at"
        );
    }

    #[test]
    fn every_split_position_gives_the_same_events() {
        let body = br#"{"type":"session_meta","payload":{"id":"x"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{}","call_id":"c1"}}
"#;
        let expected = feed_all(body);
        for split in 0..=body.len() {
            let mut p = CodexParser::new();
            let mut got = p.feed(0, &body[..split]).expect("feed");
            got.extend(p.feed(split as u64, &body[split..]).expect("feed"));
            assert_eq!(
                got, expected,
                "splitting at byte {split} changed the events"
            );
        }
    }

    #[test]
    fn a_checkpoint_resumes_identically() {
        let body = br#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"a"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"b"}]}}
"#;
        let expected = feed_all(body);
        let split = body.iter().position(|b| *b == b'\n').expect("newline") + 1;
        let mut p = CodexParser::new();
        let first = p.feed(0, &body[..split]).expect("feed");
        let mut resumed = CodexParser::restore(&p.checkpoint()).expect("restore");
        let mut got = first;
        got.extend(resumed.feed(split as u64, &body[split..]).expect("feed"));
        assert_eq!(got, expected);
    }
}

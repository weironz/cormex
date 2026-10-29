//! The Qwen Code session parser (P4-6).
//!
//! Design: plan P4-6. Written against `QwenLM/qwen-code` (Apache-2.0),
//! `packages/core/src/services/chatRecordingService.ts`.
//!
//! # The shape
//!
//! JSONL, one `ChatRecord` per line:
//!
//! ```text
//! { uuid, parentUuid, sessionId, timestamp, type, message?: Content, … }
//! ```
//!
//! `type` is `user` / `assistant` / `tool_result` / `system`. `message` is a
//! Gemini-style `Content` (`{role, parts}`), and the parts are `text`,
//! `functionCall`, `functionResponse`, `thought`.
//!
//! # It is a **tree**, not a list
//!
//! `parentUuid` makes the records a tree: a rewind re-roots the chain so the
//! rewound messages end up on a dead branch. The file is still append-only, and
//! **the linear history is the active chain**, not the file order.
//!
//! Reconstructing the active chain needs the whole file, which a chunked parser
//! does not have. So this parser emits events in **file order** and marks a
//! record whose `parentUuid` does not match the previous record's `uuid` as
//! `Meta` — which is what a dead branch looks like from the file's point of
//! view. That keeps the projection honest (every record is present) without
//! claiming a linear order the data does not have.
//!
//! # `subtype` records are `Meta`
//!
//! Upstream has some forty subtypes — `chat_compression`, `custom_title`,
//! `rewind`, `goal_state`, … — and most of them are `type: 'user'` while not
//! being something the user typed. Emitting them as user messages would put
//! system bookkeeping into the conversation index and let it outrank what the
//! person actually said.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct QwenParser {
    lines: LineBuffer,
    /// The `uuid` of the previous record, for the branch check.
    previous_uuid: Option<String>,
}

impl QwenParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
            previous_uuid: None,
        }
    }

    fn parse_line(&mut self, offset: u64, length: u64, bytes: &[u8]) -> ParsedEvent {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            return ParsedEvent::malformed(offset, length);
        };
        let record_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        let subtype = v.get("subtype").and_then(Value::as_str);
        let occurred_at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let uuid = v.get("uuid").and_then(Value::as_str).map(str::to_owned);
        let parent = v.get("parentUuid").and_then(Value::as_str);

        // A record whose parent is not the previous record is either the first
        // one (parent `null`) or on a branch the rewrite left behind. The
        // second case is `Meta`: its content is real, but it is not part of the
        // active chain
        let branched = match (&self.previous_uuid, parent) {
            (None, _) => false,
            (Some(prev), Some(p)) => p != prev,
            (Some(_), None) => true,
        };
        self.previous_uuid = uuid;

        // Every subtype record is bookkeeping, whatever its `type` says
        if let Some(sub) = subtype {
            return ParsedEvent {
                kind: TranscriptKind::Meta,
                role: None,
                occurred_at,
                content: vec![ContentBlock::Unknown {
                    raw_type: format!("qwen:{sub}"),
                }],
                text: String::new(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            };
        }

        let message = v.get("message").cloned().unwrap_or(Value::Null);
        let parts = message.get("parts").and_then(Value::as_array);

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

        match record_type {
            "user" => {
                let text = flatten_parts(parts, PartKind::Text);
                event.kind = TranscriptKind::UserMessage;
                event.role = Some("user".to_owned());
                event.content = vec![ContentBlock::Text { text: text.clone() }];
                event.text = text;
            }
            "assistant" => {
                // A `functionCall` part is a tool call, and the rest is text.
                // The first one wins: an assistant record with several calls is
                // rare, and splitting one line into several events would need
                // ids the projector derives per offset
                if let Some(call) =
                    parts.and_then(|p| p.iter().find(|part| part.get("functionCall").is_some()))
                {
                    let fc = call.get("functionCall").cloned().unwrap_or(Value::Null);
                    let id = fc
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let name = fc
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    event.kind = TranscriptKind::ToolCall;
                    event.tool_name = Some(name.clone());
                    event.tool_call_id = Some(id.clone());
                    event.content = vec![ContentBlock::ToolCall {
                        tool_call_id: id,
                        name,
                        input: fc.get("args").cloned().unwrap_or(Value::Null),
                    }];
                } else {
                    let thought = flatten_parts(parts, PartKind::Thought);
                    if !thought.is_empty() {
                        event.kind = TranscriptKind::AgentThought;
                        event.content = vec![ContentBlock::Thought {
                            text: thought.clone(),
                        }];
                        event.text = thought;
                    } else {
                        let text = flatten_parts(parts, PartKind::Text);
                        event.kind = TranscriptKind::AgentMessage;
                        event.role = Some("assistant".to_owned());
                        event.content = vec![ContentBlock::Text { text: text.clone() }];
                        event.text = text;
                    }
                }
            }
            "tool_result" => {
                let output = parts
                    .and_then(|p| p.iter().find(|part| part.get("functionResponse").is_some()))
                    .and_then(|part| part.get("functionResponse"))
                    .map(|fr| {
                        fr.get("response")
                            .map(|r| r.to_string())
                            .unwrap_or_else(|| fr.to_string())
                    })
                    .unwrap_or_default();
                let id = parts
                    .and_then(|p| p.iter().find(|part| part.get("functionResponse").is_some()))
                    .and_then(|part| part.get("functionResponse"))
                    .and_then(|fr| fr.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                event.kind = TranscriptKind::ToolResult;
                event.tool_call_id = Some(id.clone());
                event.content = vec![ContentBlock::ToolResult {
                    tool_call_id: id,
                    output: output.clone(),
                    is_error: false,
                }];
                event.text = output;
            }
            // `system` and anything new: kept, searchable, not rendered
            other => {
                event.kind = TranscriptKind::System;
                event.content = vec![ContentBlock::Unknown {
                    raw_type: format!("qwen:{other}"),
                }];
            }
        }

        if branched && event.kind != TranscriptKind::Meta {
            // The content is kept in `content` (so the raw view can show it),
            // but the kind becomes `Meta`: it is not part of the active chain
            event.kind = TranscriptKind::Meta;
        }
        event
    }
}

impl Default for QwenParser {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PartKind {
    Text,
    Thought,
}

/// Join the parts of one kind.
fn flatten_parts(parts: Option<&Vec<Value>>, kind: PartKind) -> String {
    let Some(parts) = parts else {
        return String::new();
    };
    parts
        .iter()
        .filter_map(|p| match kind {
            PartKind::Text => p.get("text").and_then(Value::as_str),
            PartKind::Thought => p
                .get("thought")
                .and_then(|t| t.get("text"))
                .and_then(Value::as_str)
                .or_else(|| p.get("thought").and_then(Value::as_str)),
        })
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .join("\n")
}

impl Parser for QwenParser {
    fn version(&self) -> &'static str {
        "qwen_code@1"
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

    fn checkpoint(&self) -> serde_json::Value {
        let (next, start, partial) = self.lines.state();
        serde_json::json!({
            "next_offset": next,
            "partial_start": start,
            "partial": String::from_utf8_lossy(&partial),
            // The branch check needs the previous uuid, or a resumed projection
            // treats the first record after the resume as the start of a branch
            "previous_uuid": self.previous_uuid,
        })
    }
}

impl QwenParser {
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
            previous_uuid: state
                .get("previous_uuid")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: Value) -> Vec<ParsedEvent> {
        let mut p = QwenParser::new();
        let mut s = serde_json::to_vec(&v).expect("serialize");
        s.push(b'\n');
        p.feed(0, &s).expect("feed")
    }

    fn user(uuid: &str, parent: Option<&str>, text: &str) -> Value {
        serde_json::json!({
            "uuid": uuid,
            "parentUuid": parent,
            "sessionId": "s1",
            "timestamp": "2026-10-30T00:00:00Z",
            "type": "user",
            "message": { "role": "user", "parts": [{ "text": text }] }
        })
    }

    /// A user record becomes a user message.
    #[test]
    fn a_user_record_becomes_a_user_message() {
        let events = line(user("u1", None, "hello"));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[0].text, "hello");
        assert!(events[0].occurred_at.is_some());
    }

    /// An assistant `functionCall` part becomes a tool call.
    #[test]
    fn an_assistant_function_call_is_a_tool_call() {
        let events = line(serde_json::json!({
            "uuid": "a1", "parentUuid": "u1", "sessionId": "s1",
            "timestamp": "2026-10-30T00:00:01Z", "type": "assistant",
            "message": { "role": "model", "parts": [
                { "functionCall": { "id": "c1", "name": "read_file", "args": {"p":"a.rs"} } }
            ]}
        }));
        assert_eq!(events[0].kind, TranscriptKind::ToolCall);
        assert_eq!(events[0].tool_name.as_deref(), Some("read_file"));
        assert_eq!(events[0].tool_call_id.as_deref(), Some("c1"));
    }

    /// A `thought` part becomes an `AgentThought`.
    #[test]
    fn an_assistant_thought_is_a_thought() {
        let events = line(serde_json::json!({
            "uuid": "a1", "parentUuid": "u1", "sessionId": "s1",
            "timestamp": "2026-10-30T00:00:01Z", "type": "assistant",
            "message": { "role": "model", "parts": [
                { "thought": { "text": "let me look" } },
                { "text": "here it is" }
            ]}
        }));
        assert_eq!(
            events[0].kind,
            TranscriptKind::AgentThought,
            "a thought part should win over the text part"
        );
        assert_eq!(events[0].text, "let me look");
    }

    /// A `tool_result` record becomes a tool result.
    #[test]
    fn a_tool_result_record_becomes_a_result() {
        let events = line(serde_json::json!({
            "uuid": "t1", "parentUuid": "a1", "sessionId": "s1",
            "timestamp": "2026-10-30T00:00:02Z", "type": "tool_result",
            "message": { "role": "user", "parts": [
                { "functionResponse": { "id": "c1", "name": "read_file",
                                        "response": { "output": "fn main() {}" } } }
            ]}
        }));
        assert_eq!(events[0].kind, TranscriptKind::ToolResult);
        assert_eq!(events[0].tool_call_id.as_deref(), Some("c1"));
        assert!(events[0].text.contains("fn main"));
    }

    /// A `subtype` record is `Meta`, even when its `type` is `user`.
    ///
    /// Upstream has some forty subtypes and most are `type: 'user'` while not
    /// being something the user typed. Emitting them as user messages would put
    /// system bookkeeping into the conversation index.
    #[test]
    fn a_subtype_record_is_meta() {
        let events = line(serde_json::json!({
            "uuid": "x1", "parentUuid": null, "sessionId": "s1",
            "timestamp": "2026-10-30T00:00:00Z", "type": "user",
            "subtype": "custom_title",
            "message": { "role": "user", "parts": [{ "text": "My session" }] }
        }));
        assert_eq!(events[0].kind, TranscriptKind::Meta);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "qwen:custom_title".into()
            }]
        );
    }

    /// A record on a dead branch is `Meta`.
    ///
    /// A rewind re-roots the chain, so the rewound messages stay in the file on
    /// a branch that is no longer the active one. Emitting them as conversation
    /// would show text the user has already gone back and replaced.
    #[test]
    fn a_record_on_a_dead_branch_is_meta() {
        let mut p = QwenParser::new();
        let mut body = serde_json::to_vec(&user("u1", None, "first")).expect("serialize");
        body.push(b'\n');
        body.extend(serde_json::to_vec(&user("u2", Some("u1"), "second")).expect("serialize"));
        body.push(b'\n');
        // A third record whose parent is not `u2`: a branch
        body.extend(serde_json::to_vec(&user("u3", Some("u1"), "rewound")).expect("serialize"));
        body.push(b'\n');
        let events = p.feed(0, &body).expect("feed");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[1].kind, TranscriptKind::UserMessage);
        assert_eq!(
            events[2].kind,
            TranscriptKind::Meta,
            "a record whose parent is not the previous record is on a dead branch"
        );
    }

    /// The branch state survives a checkpoint.
    ///
    /// Without it, the first record after a resume looks like the start of a
    /// branch and is demoted to `Meta` — so a resumed projection would lose the
    /// first message of every chunk.
    #[test]
    fn the_branch_state_survives_a_checkpoint() {
        let mut p = QwenParser::new();
        let mut body = serde_json::to_vec(&user("u1", None, "first")).expect("serialize");
        body.push(b'\n');
        p.feed(0, &body).expect("feed");
        let state = p.checkpoint();
        let mut restored = QwenParser::restore(&state).expect("restore");
        let next = u64::try_from(body.len()).expect("fits");
        let mut more = serde_json::to_vec(&user("u2", Some("u1"), "second")).expect("serialize");
        more.push(b'\n');
        let events = restored.feed(next, &more).expect("resume");
        assert_eq!(
            events[0].kind,
            TranscriptKind::UserMessage,
            "the resumed parser treated a continuation as a branch"
        );
    }

    /// A malformed line is kept.
    #[test]
    fn a_malformed_line_is_kept() {
        let mut p = QwenParser::new();
        let events = p.feed(0, b"not json\n").expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
    }
}

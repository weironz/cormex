//! The Grok Build session parser (`~/.grok/sessions/<cwd>/<id>/updates.jsonl`).
//!
//! # Where the format came from
//!
//! `xai-org/grok-build`'s user guide (`docs/user-guide/17-sessions.md`) names
//! `updates.jsonl` the authoritative conversation log: one JSON-RPC
//! notification per line, carrying an ACP `session/update`, plus xAI's own
//! `_x.ai/session/update` for turn bookkeeping. Checked against sessions
//! `grok` 1.0.77 wrote on a real run.
//!
//! ```text
//! {"timestamp":…,"method":"session/update","params":{"sessionId":"…","update":
//!     {"sessionUpdate":"user_message_chunk","content":{"type":"text","text":"…"}},
//!  "_meta":{"agentTimestampMs":…}}}
//! {… "update":{"sessionUpdate":"agent_thought_chunk","content":{…}}}
//! {… "update":{"sessionUpdate":"agent_message_chunk","content":{…}}}
//! {… "update":{"sessionUpdate":"tool_call","toolCallId":"call_…","title":"read_file","rawInput":{…}}}
//! {… "update":{"sessionUpdate":"tool_call_update","toolCallId":"call_…","status":"completed",
//!     "content":[{"type":"content","content":{"type":"text","text":"…"}}],"rawOutput":{…}}}
//! {"method":"_x.ai/session/update", … "update":{"sessionUpdate":"turn_completed",…}}
//! ```
//!
//! # Chunks are merged
//!
//! ACP streams a message as any number of `*_chunk` updates. One event per
//! chunk would show an answer as dozens of fragments, so consecutive chunks of
//! the same kind become one event, spanning their lines in the raw file. The
//! message being assembled is part of the checkpoint: the projector can stop
//! between two chunks of one answer.
//!
//! A merged event is emitted when anything else arrives (another kind, a tool
//! call, a turn boundary). The last message of a file that is still being
//! written stays pending until the next update or the turn's
//! `turn_completed`, which Grok writes at the end of every turn.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// A message still being assembled from chunks.
#[derive(Debug, Clone, PartialEq)]
struct Pending {
    kind: TranscriptKind,
    text: String,
    start: u64,
    end: u64,
    at: Option<DateTime<Utc>>,
}

/// The parser.
pub struct GrokBuildParser {
    lines: LineBuffer,
    pending: Option<Pending>,
}

impl GrokBuildParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
            pending: None,
        }
    }

    /// Resume from [`Parser::checkpoint`].
    ///
    /// # Errors
    /// The checkpoint has no `next_offset`.
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
        let pending = state.get("pending").and_then(|p| {
            Some(Pending {
                kind: kind_from_str(p.get("kind")?.as_str()?)?,
                text: p.get("text")?.as_str()?.to_owned(),
                start: p.get("start")?.as_u64()?,
                end: p.get("end")?.as_u64()?,
                at: p
                    .get("at")
                    .and_then(Value::as_i64)
                    .and_then(DateTime::<Utc>::from_timestamp_millis),
            })
        });
        Ok(Self {
            lines: LineBuffer::from_state(next, start, partial),
            pending,
        })
    }

    fn flush(&mut self, out: &mut Vec<ParsedEvent>) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let (role, block) = match p.kind {
            TranscriptKind::UserMessage => (
                "user",
                ContentBlock::Text {
                    text: p.text.clone(),
                },
            ),
            TranscriptKind::AgentThought => (
                "assistant",
                ContentBlock::Thought {
                    text: p.text.clone(),
                },
            ),
            _ => (
                "assistant",
                ContentBlock::Text {
                    text: p.text.clone(),
                },
            ),
        };
        out.push(ParsedEvent {
            kind: p.kind,
            role: Some(role.to_owned()),
            occurred_at: p.at,
            content: vec![block],
            text: p.text,
            tool_name: None,
            tool_call_id: None,
            src_offset: p.start,
            src_length: p.end - p.start,
        });
    }

    fn parse_line(&mut self, offset: u64, length: u64, bytes: &[u8]) -> Vec<ParsedEvent> {
        let mut out = Vec::new();
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            self.flush(&mut out);
            out.push(ParsedEvent::malformed(offset, length));
            return out;
        };
        let at = v
            .pointer("/params/_meta/agentTimestampMs")
            .and_then(Value::as_i64)
            .and_then(DateTime::<Utc>::from_timestamp_millis)
            .or_else(|| {
                v.get("timestamp")
                    .and_then(Value::as_i64)
                    .and_then(|s| DateTime::<Utc>::from_timestamp(s, 0))
            });
        let update = v.pointer("/params/update");
        let kind = update
            .and_then(|u| u.get("sessionUpdate"))
            .and_then(Value::as_str)
            .unwrap_or("");

        let chunk_kind = match kind {
            "user_message_chunk" => Some(TranscriptKind::UserMessage),
            "agent_message_chunk" => Some(TranscriptKind::AgentMessage),
            "agent_thought_chunk" => Some(TranscriptKind::AgentThought),
            _ => None,
        };
        if let Some(chunk_kind) = chunk_kind {
            let piece = update
                .and_then(|u| u.get("content"))
                .map(content_text)
                .unwrap_or_default();
            match &mut self.pending {
                Some(p) if p.kind == chunk_kind => {
                    p.text.push_str(&piece);
                    p.end = offset + length;
                }
                _ => {
                    self.flush(&mut out);
                    self.pending = Some(Pending {
                        kind: chunk_kind,
                        text: piece,
                        start: offset,
                        end: offset + length,
                        at,
                    });
                }
            }
            return out;
        }

        // Anything that is not a chunk ends the message being assembled
        self.flush(&mut out);
        let u = update.cloned().unwrap_or(Value::Null);
        match kind {
            "tool_call" => {
                let id = str_field(&u, "toolCallId");
                let name = u
                    .pointer("/_meta/x.ai~1tool/name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| str_field(&u, "title"));
                let input = u.get("rawInput").cloned().unwrap_or(Value::Null);
                out.push(ParsedEvent {
                    kind: TranscriptKind::ToolCall,
                    role: Some("assistant".to_owned()),
                    occurred_at: at,
                    content: vec![ContentBlock::ToolCall {
                        tool_call_id: id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    }],
                    text: input.to_string(),
                    tool_name: Some(name),
                    tool_call_id: Some(id),
                    src_offset: offset,
                    src_length: length,
                });
            }
            "tool_call_update"
                if matches!(
                    u.get("status").and_then(Value::as_str),
                    Some("completed" | "failed")
                ) =>
            {
                let id = str_field(&u, "toolCallId");
                let output = u
                    .get("content")
                    .map(content_text)
                    .filter(|t| !t.is_empty())
                    .or_else(|| {
                        u.get("rawOutput").map(|r| match r {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                    })
                    .unwrap_or_default();
                out.push(ParsedEvent {
                    kind: TranscriptKind::ToolResult,
                    role: Some("tool".to_owned()),
                    occurred_at: at,
                    content: vec![ContentBlock::ToolResult {
                        tool_call_id: id.clone(),
                        output: output.clone(),
                        is_error: u.get("status").and_then(Value::as_str) == Some("failed"),
                    }],
                    text: output,
                    tool_name: None,
                    tool_call_id: Some(id),
                    src_offset: offset,
                    src_length: length,
                });
            }
            "plan" => {
                let entries: Vec<String> = u
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|es| {
                        es.iter()
                            .filter_map(|e| e.get("content").and_then(Value::as_str))
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(ParsedEvent {
                    kind: TranscriptKind::Plan,
                    role: Some("assistant".to_owned()),
                    occurred_at: at,
                    text: entries.join("\n"),
                    content: vec![ContentBlock::Plan { entries }],
                    tool_name: None,
                    tool_call_id: None,
                    src_offset: offset,
                    src_length: length,
                });
            }
            // An in-progress tool update, turn bookkeeping, retries, command
            // lists, …
            other => {
                let raw = if other.is_empty() {
                    "grok:untyped".to_owned()
                } else {
                    format!("grok:{other}")
                };
                let mut e = ParsedEvent::unknown(&raw, offset, length);
                e.kind = TranscriptKind::Meta;
                e.occurred_at = at;
                out.push(e);
            }
        }
        out
    }
}

fn kind_from_str(s: &str) -> Option<TranscriptKind> {
    match s {
        "user" => Some(TranscriptKind::UserMessage),
        "agent" => Some(TranscriptKind::AgentMessage),
        "thought" => Some(TranscriptKind::AgentThought),
        _ => None,
    }
}

const fn kind_to_str(k: TranscriptKind) -> &'static str {
    match k {
        TranscriptKind::UserMessage => "user",
        TranscriptKind::AgentThought => "thought",
        _ => "agent",
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The text of an ACP content value: a block `{type:"text",text}`, a wrapper
/// `{type:"content",content:{…}}`, or an array of either.
fn content_text(v: &Value) -> String {
    match v {
        Value::Array(items) => items
            .iter()
            .map(content_text)
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => v
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| v.get("content").map(content_text))
            .unwrap_or_default(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

impl Default for GrokBuildParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser for GrokBuildParser {
    fn version(&self) -> &'static str {
        "grok_build@1"
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        let lines = self.lines.feed(offset, bytes).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for l in lines {
            let len = u64::try_from(l.bytes.len()).unwrap_or(0);
            out.extend(self.parse_line(l.offset, len, &l.bytes));
        }
        Ok(out)
    }

    fn checkpoint(&self) -> Value {
        let (next, start, partial) = self.lines.state();
        serde_json::json!({
            "next_offset": next,
            "partial_start": start,
            "partial": String::from_utf8_lossy(&partial),
            "pending": self.pending.as_ref().map(|p| serde_json::json!({
                "kind": kind_to_str(p.kind),
                "text": p.text,
                "start": p.start,
                "end": p.end,
                "at": p.at.map(|t| t.timestamp_millis()),
            })),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(kind: &str, text: &str) -> String {
        format!(
            r#"{{"timestamp":1794037966,"method":"session/update","params":{{"sessionId":"s","update":{{"sessionUpdate":"{kind}","content":{{"type":"text","text":"{text}"}}}},"_meta":{{"agentTimestampMs":1794037966160}}}}}}"#
        ) + "\n"
    }

    const DONE: &str = r#"{"timestamp":1794037969,"method":"_x.ai/session/update","params":{"sessionId":"s","update":{"sessionUpdate":"turn_completed","stop_reason":"end_turn"}}}
"#;

    #[test]
    fn streamed_chunks_become_one_message() {
        let body = chunk("user_message_chunk", "hi")
            + &chunk("agent_message_chunk", "Hel")
            + &chunk("agent_message_chunk", "lo")
            + DONE;
        let events = GrokBuildParser::new()
            .feed(0, body.as_bytes())
            .expect("feed");
        let said: Vec<_> = events
            .iter()
            .filter(|e| e.kind != TranscriptKind::Meta)
            .map(|e| (e.kind, e.text.as_str()))
            .collect();
        assert_eq!(
            said,
            vec![
                (TranscriptKind::UserMessage, "hi"),
                (TranscriptKind::AgentMessage, "Hello"),
            ],
            "two chunks of one answer must read as one message"
        );
        let answer = &events[1];
        let first_chunk = chunk("user_message_chunk", "hi").len() as u64;
        assert_eq!(answer.src_offset, first_chunk);
        // Both lines, minus the final newline (a line's length excludes it)
        assert_eq!(
            answer.src_length,
            (chunk("agent_message_chunk", "Hel").len() + chunk("agent_message_chunk", "lo").len()
                - 1) as u64,
            "the merged event points at both chunk lines in the raw file"
        );
    }

    #[test]
    fn a_message_split_across_a_checkpoint_is_still_one_message() {
        let a = chunk("agent_message_chunk", "Hel");
        let b = chunk("agent_message_chunk", "lo");
        let mut first = GrokBuildParser::new();
        assert!(
            first.feed(0, a.as_bytes()).expect("feed").is_empty(),
            "an unfinished message is held back, not emitted as a fragment"
        );
        let mut resumed = GrokBuildParser::restore(&first.checkpoint()).expect("restore");
        let rest = b.clone() + DONE;
        let events = resumed.feed(a.len() as u64, rest.as_bytes()).expect("feed");
        assert_eq!(events[0].text, "Hello");
        assert_eq!(events[0].src_offset, 0);
    }

    #[test]
    fn a_tool_call_and_its_result_pair_by_id() {
        let call = r#"{"timestamp":1,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"c1","title":"list_dir","rawInput":{"target_directory":"/r"},"_meta":{"x.ai/tool":{"name":"list_dir"}}}}}
"#;
        let result = r#"{"timestamp":2,"method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"c1","status":"completed","content":[{"type":"content","content":{"type":"text","text":"app.py"}}]}}}
"#;
        let body = call.to_owned() + result;
        let events = GrokBuildParser::new()
            .feed(0, body.as_bytes())
            .expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::ToolCall);
        assert_eq!(events[0].tool_name.as_deref(), Some("list_dir"));
        assert_eq!(events[1].kind, TranscriptKind::ToolResult);
        assert_eq!(events[1].text, "app.py");
        assert_eq!(events[1].tool_call_id.as_deref(), Some("c1"));
    }
}

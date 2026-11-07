//! The DeepSeek Harness (`dsh`) session parser.
//!
//! # Where the format came from
//!
//! `deepseek-ai/deepseek-harness` (MIT) persists a session as an append-only
//! event log (`packages/session/session-persistence-jsonl`). This parser was
//! written against logs that `dsh` 0.2.2-rc.1 wrote on a real run, decompressed
//! — on disk the log is a series of appended zstd frames, which the client
//! reads as one logical file (catalog: `also_compressed = ".zstd"`).
//!
//! ```text
//! {"type":"session","version":4,"id":"session-…","createdAt":…,"cwd":"/repo"}
//! {"type":"user/message","seq":9,"time":…,"data":{"content":[{"type":"text","text":"…"}],"role":"user"}}
//! {"type":"assistant/message","seq":17,"time":…,"data":{"message":{"role":"assistant","content":[
//!     {"type":"reasoning","text":"…"},{"type":"text","text":"…"},
//!     {"type":"tool-call","id":"call_…","name":"bash","arguments":"{\"command\":…}"}]}}}
//! {"type":"tool/call","seq":18,"data":{"callId":"call_…","name":"bash","arguments":"…"}}
//! {"type":"tool/result","seq":21,"data":{"message":{"role":"tool","toolCallId":"call_…",
//!     "content":[{"type":"text","text":"…"}],"isError":false}}}
//! ```
//!
//! Everything else (`turn/start`, `request/header`, `job/settled`,
//! `session/title`, delivery receipts, …) is the runtime's own bookkeeping and
//! is projected as `Meta`.
//!
//! # Two things a naive parser gets wrong
//!
//! 1. **A tool call is logged twice**: once inside `assistant/message`, once as
//!    `tool/call`. Projecting both shows every call twice; `tool/call` is
//!    `Meta` here.
//! 2. **`tool/result` does not name the tool.** The name is remembered from
//!    the call, and the map is part of the checkpoint: a result can arrive in
//!    a later chunk than its call.
//!
//! `agent/inbox/spliced` carries the user's message before it is delivered, and
//! `user/message` carries it again when it is; only the second is the message.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// How many call-id → tool-name entries to remember. A long session makes
/// thousands of calls; a result arrives within a few lines of its call, so the
/// map only needs the recent ones, and an unbounded map would grow every
/// checkpoint without limit.
const TOOL_NAMES_KEPT: usize = 256;

/// The parser.
pub struct DshParser {
    lines: LineBuffer,
    /// call id → (sequence it was seen at, tool name).
    tool_names: BTreeMap<String, (u64, String)>,
    seen: u64,
}

impl DshParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
            tool_names: BTreeMap::new(),
            seen: 0,
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
        let mut tool_names = BTreeMap::new();
        if let Some(map) = state.get("tool_names").and_then(Value::as_object) {
            for (id, v) in map {
                if let (Some(seq), Some(name)) = (
                    v.get(0).and_then(Value::as_u64),
                    v.get(1).and_then(Value::as_str),
                ) {
                    tool_names.insert(id.clone(), (seq, name.to_owned()));
                }
            }
        }
        Ok(Self {
            lines: LineBuffer::from_state(next, start, partial),
            seen: state.get("seen").and_then(Value::as_u64).unwrap_or(0),
            tool_names,
        })
    }

    fn remember(&mut self, id: &str, name: &str) {
        if id.is_empty() {
            return;
        }
        self.seen += 1;
        self.tool_names
            .insert(id.to_owned(), (self.seen, name.to_owned()));
        if self.tool_names.len() > TOOL_NAMES_KEPT
            && let Some(oldest) = self
                .tool_names
                .iter()
                .min_by_key(|(_, (seq, _))| *seq)
                .map(|(k, _)| k.clone())
        {
            self.tool_names.remove(&oldest);
        }
    }

    fn parse_line(&mut self, offset: u64, length: u64, bytes: &[u8]) -> Vec<ParsedEvent> {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            return vec![ParsedEvent::malformed(offset, length)];
        };
        let at = v
            .get("time")
            .or_else(|| v.get("createdAt"))
            .and_then(Value::as_i64)
            .and_then(DateTime::<Utc>::from_timestamp_millis);
        let record_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        let data = v.get("data");
        let event = |kind, role: &str, content: Vec<ContentBlock>, text: String| ParsedEvent {
            kind,
            role: Some(role.to_owned()),
            occurred_at: at,
            content,
            text,
            tool_name: None,
            tool_call_id: None,
            src_offset: offset,
            src_length: length,
        };

        match record_type {
            "user/message" => {
                let text = text_of(data.and_then(|d| d.get("content")));
                if text.trim().is_empty() {
                    return vec![meta("dsh:user/message-empty", offset, length, at)];
                }
                // dsh delivers its own runtime-context snapshot (cwd, sandbox
                // and approval policy) as a `user/message` too, marked by
                // `source.kind`. Only `user` is something a person typed
                let typed = data
                    .and_then(|d| d.pointer("/source/kind"))
                    .and_then(Value::as_str)
                    .is_none_or(|k| k == "user");
                let (kind, role) = if typed {
                    (TranscriptKind::UserMessage, "user")
                } else {
                    (TranscriptKind::System, "system")
                };
                vec![event(
                    kind,
                    role,
                    vec![ContentBlock::Text { text: text.clone() }],
                    text,
                )]
            }
            "system/message" => {
                let text = text_of(
                    data.and_then(|d| d.get("message"))
                        .and_then(|m| m.get("content")),
                );
                vec![event(
                    TranscriptKind::System,
                    "system",
                    vec![ContentBlock::Text { text: text.clone() }],
                    text,
                )]
            }
            "assistant/message" => {
                let blocks = data
                    .and_then(|d| d.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut out = Vec::new();
                for block in &blocks {
                    match block.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" => {
                            let text = str_field(block, "text");
                            if !text.trim().is_empty() {
                                out.push(event(
                                    TranscriptKind::AgentMessage,
                                    "assistant",
                                    vec![ContentBlock::Text { text: text.clone() }],
                                    text,
                                ));
                            }
                        }
                        // DeepSeek often returns an empty reasoning block; it
                        // says nothing and is skipped
                        "reasoning" => {
                            let text = str_field(block, "text");
                            if !text.trim().is_empty() {
                                out.push(event(
                                    TranscriptKind::AgentThought,
                                    "assistant",
                                    vec![ContentBlock::Thought { text: text.clone() }],
                                    text,
                                ));
                            }
                        }
                        "tool-call" => {
                            let id = str_field(block, "id");
                            let name = str_field(block, "name");
                            self.remember(&id, &name);
                            let input = arguments(block.get("arguments"));
                            let mut e = event(
                                TranscriptKind::ToolCall,
                                "assistant",
                                vec![ContentBlock::ToolCall {
                                    tool_call_id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                }],
                                input.to_string(),
                            );
                            e.tool_name = Some(name);
                            e.tool_call_id = Some(id);
                            out.push(e);
                        }
                        other => {
                            out.push(meta(&format!("dsh:block:{other}"), offset, length, at));
                        }
                    }
                }
                if out.is_empty() {
                    out.push(meta("dsh:assistant/message-empty", offset, length, at));
                }
                out
            }
            "tool/result" => {
                let message = data.and_then(|d| d.get("message"));
                let id = message
                    .and_then(|m| m.get("toolCallId"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let output = text_of(message.and_then(|m| m.get("content")));
                let is_error = message
                    .and_then(|m| m.get("isError"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut e = event(
                    TranscriptKind::ToolResult,
                    "tool",
                    vec![ContentBlock::ToolResult {
                        tool_call_id: id.clone(),
                        output: output.clone(),
                        is_error,
                    }],
                    output,
                );
                e.tool_name = self.tool_names.get(&id).map(|(_, n)| n.clone());
                e.tool_call_id = Some(id);
                vec![e]
            }
            // The duplicate of the call inside `assistant/message`; the name is
            // still worth remembering in case that message was cut off
            "tool/call" => {
                if let Some(d) = data {
                    self.remember(&str_field(d, "callId"), &str_field(d, "name"));
                }
                vec![meta("dsh:tool/call", offset, length, at)]
            }
            "" => vec![meta("dsh:untyped", offset, length, at)],
            other => vec![meta(&format!("dsh:{other}"), offset, length, at)],
        }
    }
}

fn meta(raw: &str, offset: u64, length: u64, at: Option<DateTime<Utc>>) -> ParsedEvent {
    let mut e = ParsedEvent::unknown(raw, offset, length);
    e.kind = TranscriptKind::Meta;
    e.occurred_at = at;
    e
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Tool arguments arrive as a JSON **string**; decode it so the UI gets an
/// object, and keep the string when it does not decode (a call the model cut
/// off mid-argument).
fn arguments(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::String(s)) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.clone()))
        }
        Some(other) => other.clone(),
        None => Value::Null,
    }
}

impl Default for DshParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser for DshParser {
    fn version(&self) -> &'static str {
        "deepseek_harness@1"
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
        let names: serde_json::Map<String, Value> = self
            .tool_names
            .iter()
            .map(|(k, (seq, name))| (k.clone(), serde_json::json!([seq, name])))
            .collect();
        serde_json::json!({
            "next_offset": next,
            "partial_start": start,
            "partial": String::from_utf8_lossy(&partial),
            "tool_names": names,
            "seen": self.seen,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURN: &str = concat!(
        r#"{"type":"session","version":4,"id":"session-1","createdAt":1794037763356,"cwd":"/home/user/r"}"#,
        "\n",
        r#"{"type":"agent/inbox/spliced","seq":4,"time":1794037763367,"data":{"inserted":[{"content":[{"type":"text","text":"run it"}],"role":"user"}]}}"#,
        "\n",
        r#"{"type":"user/message","seq":9,"time":1794037764310,"data":{"content":[{"type":"text","text":"run it"}],"role":"user"}}"#,
        "\n",
        r#"{"type":"assistant/message","seq":17,"time":1794037782903,"data":{"message":{"role":"assistant","content":[{"type":"reasoning","text":""},{"type":"text","text":"I'll run it."},{"type":"tool-call","id":"call_1","name":"bash","arguments":"{\"command\":\"ls\"}"}]}}}"#,
        "\n",
        r#"{"type":"tool/call","seq":18,"time":1794037782906,"data":{"callId":"call_1","name":"bash","arguments":"{\"command\":\"ls\"}"}}"#,
        "\n",
        r#"{"type":"tool/result","seq":21,"time":1794037783236,"data":{"message":{"role":"tool","toolCallId":"call_1","content":[{"type":"text","text":"app.py\n"}],"isError":false}}}"#,
        "\n",
    );

    #[test]
    fn a_turn_projects_each_thing_once() {
        let events = DshParser::new().feed(0, TURN.as_bytes()).expect("feed");
        let said: Vec<_> = events
            .iter()
            .filter(|e| e.kind != TranscriptKind::Meta)
            .map(|e| e.kind)
            .collect();
        assert_eq!(
            said,
            vec![
                TranscriptKind::UserMessage,
                TranscriptKind::AgentMessage,
                TranscriptKind::ToolCall,
                TranscriptKind::ToolResult,
            ],
            "the inbox copy of the prompt, the empty reasoning block and the tool/call duplicate \
             must not show up as conversation"
        );
        let call = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolCall)
            .expect("a call");
        assert_eq!(
            call.content,
            vec![ContentBlock::ToolCall {
                tool_call_id: "call_1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            }],
            "the arguments string is decoded into an object"
        );
        let result = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolResult)
            .expect("a result");
        assert_eq!(
            result.tool_name.as_deref(),
            Some("bash"),
            "tool/result does not name the tool; the name comes from the call"
        );
    }

    #[test]
    fn the_tool_name_survives_a_checkpoint_between_call_and_result() {
        let split = TURN.find(r#"{"type":"tool/result""#).expect("result line");
        let mut first = DshParser::new();
        first.feed(0, &TURN.as_bytes()[..split]).expect("feed");
        let mut resumed = DshParser::restore(&first.checkpoint()).expect("restore");
        let events = resumed
            .feed(split as u64, &TURN.as_bytes()[split..])
            .expect("feed");
        assert_eq!(
            events[0].tool_name.as_deref(),
            Some("bash"),
            "the projector stops between chunks; a result in the next chunk must still know its \
             tool"
        );
    }
}

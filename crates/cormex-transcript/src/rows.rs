//! The SQLite-row parsers: OpenCode and Goose (P4-5).
//!
//! Design: plan P4-5, over P4-0's row reader.
//!
//! # Why these are row parsers rather than file parsers
//!
//! Both hosts keep their history in SQLite, and P4-0 turns each row into a
//! logical file whose bytes are either one text column or the whole row as a
//! JSON object. So the input here is **one row's JSON**, not a file — and the
//! parser's job is to turn that object into events.
//!
//! # OpenCode
//!
//! `sst/opencode` (MIT), `packages/schema/src/session-message.ts` and
//! `packages/core/src/session/sql.ts`:
//!
//! ```text
//! session_message(id, session_id, type, seq, data JSON)
//! session_part(message_id, session_id, ordinal, type, call_id, data JSON)
//! ```
//!
//! `session_message.type` is `user` / `assistant` / `system` / `shell` /
//! `synthetic` / …; an `assistant` message's content is in `session_part`,
//! which is a **separate row**. P4-0's reader selects one row at a time, so the
//! catalog's query for OpenCode joins the parts into the message row — the
//! alternative is the parser reaching back into the database, which it cannot
//! do.
//!
//! # Goose
//!
//! `block/goose` (Apache-2.0), `crates/goose/src/session/session_manager.rs`:
//!
//! ```text
//! sessions(id, name, description, working_dir, created_at, …)
//! messages(id, message_id, session_id, role, content_json, created_timestamp, …)
//! ```
//!
//! `content_json` is goose's own `MessageContent` array. The catalog's query
//! selects `messages` rows, so one row is one message and the parser is a thin
//! mapping.
//!
//! # Both are `snapshot` sources
//!
//! A row is rewritten in place. There is nothing to append to, and the client
//! compares whole-row hashes rather than offsets.

use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::parser::{ParsedEvent, Parser};

/// Which host's row this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowFormat {
    OpenCode,
    Goose,
}

/// A parser over one row's JSON bytes.
///
/// The whole row is fed in one call (P4-0 reads a row whole), so this does not
/// buffer across chunks. The `Parser` trait's contract still holds: `feed`
/// returns the events for what it was given, and a second feed of the same
/// bytes returns nothing.
pub struct RowParser {
    format: RowFormat,
    /// Whether the row has already been turned into events.
    ///
    /// A snapshot source is re-fed on every run; re-emitting would duplicate
    /// them, and the projector's `ON CONFLICT` would hide that behind an id
    /// collision rather than an error.
    emitted: bool,
}

impl RowParser {
    #[must_use]
    pub fn new(format: RowFormat) -> Self {
        Self {
            format,
            emitted: false,
        }
    }
}

impl Parser for RowParser {
    fn version(&self) -> &'static str {
        match self.format {
            RowFormat::OpenCode => "opencode@1",
            RowFormat::Goose => "goose@1",
        }
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        if self.emitted {
            return Ok(Vec::new());
        }
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            return Ok(vec![ParsedEvent::malformed(
                offset,
                u64::try_from(bytes.len()).unwrap_or(0),
            )]);
        };
        self.emitted = true;
        let len = u64::try_from(bytes.len()).unwrap_or(0);
        Ok(match self.format {
            RowFormat::OpenCode => parse_opencode(&v, offset, len),
            RowFormat::Goose => parse_goose(&v, offset, len),
        })
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::json!({ "emitted": self.emitted })
    }
}

impl RowParser {
    /// Resume from a checkpoint.
    ///
    /// # Errors
    /// The checkpoint is not one this parser wrote.
    pub fn restore(format: RowFormat, state: &Value) -> Result<Self, String> {
        Ok(Self {
            format,
            emitted: state
                .get("emitted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// One `session_message` row (with its parts joined in by the catalog query).
///
/// The expected columns are `type`, `data` and — when the catalog's query joins
/// them — `parts`, an array of `{type, data}` objects in `ordinal` order.
fn parse_opencode(row: &Value, offset: u64, len: u64) -> Vec<ParsedEvent> {
    let message_type = row.get("type").and_then(Value::as_str).unwrap_or("");
    let data = row.get("data").cloned().unwrap_or(Value::Null);
    let occurred_at = data
        .get("time")
        .and_then(|t| t.get("created"))
        .and_then(Value::as_i64)
        .and_then(ms_to_time);

    let mut out = Vec::new();
    match message_type {
        "user" => {
            let text = data
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            out.push(text_event(
                TranscriptKind::UserMessage,
                "user",
                text,
                occurred_at,
                offset,
                len,
            ));
        }
        "assistant" => {
            // The parts are the content; `data.content` is absent on a
            // `session_message` row because the parts live in their own table
            let parts = row.get("parts").and_then(Value::as_array);
            match parts {
                Some(parts) if !parts.is_empty() => {
                    for part in parts {
                        out.extend(opencode_part(part, occurred_at, offset, len));
                    }
                }
                _ => {
                    // A message with no parts joined in is a row the catalog's
                    // query did not cover. `Unknown` with its type keeps the
                    // offset visible rather than projecting an empty session
                    out.push(ParsedEvent::unknown(
                        "opencode:assistant-no-parts",
                        offset,
                        len,
                    ));
                }
            }
        }
        // `system` / `synthetic` / `shell` / the switch records: the host
        // talking about the session. `System` keeps them visible without
        // putting them in the conversation index
        other => {
            let text = data
                .get("text")
                .and_then(Value::as_str)
                .or_else(|| data.get("command").and_then(Value::as_str))
                .unwrap_or_default()
                .to_owned();
            out.push(text_event(
                TranscriptKind::System,
                "",
                if text.is_empty() {
                    format!("[{other}]")
                } else {
                    text
                },
                occurred_at,
                offset,
                len,
            ));
        }
    }
    out
}

/// One `session_part` row.
fn opencode_part(
    part: &Value,
    occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    offset: u64,
    len: u64,
) -> Vec<ParsedEvent> {
    let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");
    let data = part.get("data").cloned().unwrap_or(Value::Null);
    match part_type {
        "text" => vec![text_event(
            TranscriptKind::AgentMessage,
            "assistant",
            data.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            occurred_at,
            offset,
            len,
        )],
        "reasoning" => vec![ParsedEvent {
            kind: TranscriptKind::AgentThought,
            role: Some("assistant".to_owned()),
            occurred_at,
            content: vec![ContentBlock::Thought {
                text: data
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }],
            text: data
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            tool_name: None,
            tool_call_id: None,
            src_offset: offset,
            src_length: len,
        }],
        "tool" => {
            // The tool's state is a tagged union: `streaming` has no result,
            // `completed` carries `content`, `error` carries `error`
            let id = data
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let name = data
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let state = data.get("state").cloned().unwrap_or(Value::Null);
            let status = state.get("status").and_then(Value::as_str).unwrap_or("");
            let mut out = vec![ParsedEvent {
                kind: TranscriptKind::ToolCall,
                role: Some("assistant".to_owned()),
                occurred_at,
                content: vec![ContentBlock::ToolCall {
                    tool_call_id: id.clone(),
                    name: name.clone(),
                    input: state.get("input").cloned().unwrap_or(Value::Null),
                }],
                text: String::new(),
                tool_name: Some(name),
                tool_call_id: Some(id.clone()),
                src_offset: offset,
                src_length: len,
            }];
            // Only a settled call has a result. Emitting one for `streaming`
            // would show a tool that never returned
            if status == "completed" || status == "error" {
                let output = if status == "error" {
                    state
                        .get("error")
                        .map(|e| e.to_string())
                        .unwrap_or_default()
                } else {
                    flatten_content(state.get("content"))
                };
                out.push(ParsedEvent {
                    kind: TranscriptKind::ToolResult,
                    role: None,
                    occurred_at,
                    content: vec![ContentBlock::ToolResult {
                        tool_call_id: id.clone(),
                        output: output.clone(),
                        is_error: status == "error",
                    }],
                    text: output,
                    tool_name: None,
                    tool_call_id: Some(id),
                    src_offset: offset,
                    src_length: len,
                });
            }
            out
        }
        other => vec![ParsedEvent::unknown(
            &format!("opencode:part:{other}"),
            offset,
            len,
        )],
    }
}

/// One `messages` row from Goose.
///
/// The catalog's query selects `role`, `content_json` and `created_timestamp`.
fn parse_goose(row: &Value, offset: u64, len: u64) -> Vec<ParsedEvent> {
    let role = row.get("role").and_then(Value::as_str).unwrap_or("");
    let occurred_at = row
        .get("created_timestamp")
        .and_then(Value::as_i64)
        .and_then(|t| {
            // Goose stores **seconds** on current versions and milliseconds on
            // some older rows; the upstream code has the same threshold
            const MILLISECOND_THRESHOLD: i64 = 10_000_000_000;
            if t > MILLISECOND_THRESHOLD {
                ms_to_time(t)
            } else {
                chrono::DateTime::from_timestamp(t, 0)
            }
        });

    // `content_json` arrives as a **string** (the column is TEXT holding JSON)
    // when the catalog's query takes it verbatim, or as an array when the query
    // uses `json(content_json)`. Both are handled
    let content = match row.get("content_json") {
        Some(Value::String(s)) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        Some(v) => v.clone(),
        None => Value::Null,
    };

    let items = content.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    let mut pending = String::new();

    for item in &items {
        match item.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(t) = item.get("text").and_then(Value::as_str) {
                    if !pending.is_empty() {
                        pending.push('\n');
                    }
                    pending.push_str(t);
                }
            }
            "toolRequest" | "tool_request" => {
                if !pending.is_empty() {
                    out.push(message_event(
                        role,
                        std::mem::take(&mut pending),
                        occurred_at,
                        offset,
                        len,
                    ));
                }
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = item
                    .get("toolCall")
                    .and_then(|c| c.get("value"))
                    .and_then(|v| v.get("name"))
                    .and_then(Value::as_str)
                    .or_else(|| item.get("name").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned();
                out.push(ParsedEvent {
                    kind: TranscriptKind::ToolCall,
                    role: Some(role.to_owned()),
                    occurred_at,
                    content: vec![ContentBlock::ToolCall {
                        tool_call_id: id.clone(),
                        name: name.clone(),
                        input: item
                            .get("toolCall")
                            .and_then(|c| c.get("value"))
                            .and_then(|v| v.get("arguments"))
                            .cloned()
                            .unwrap_or(Value::Null),
                    }],
                    text: String::new(),
                    tool_name: Some(name),
                    tool_call_id: Some(id),
                    src_offset: offset,
                    src_length: len,
                });
            }
            "toolResponse" | "tool_response" => {
                if !pending.is_empty() {
                    out.push(message_event(
                        role,
                        std::mem::take(&mut pending),
                        occurred_at,
                        offset,
                        len,
                    ));
                }
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let output = item
                    .get("toolResult")
                    .and_then(|r| r.get("value"))
                    .map(|v| {
                        v.get("content")
                            .map(|c| flatten_content(Some(c)))
                            .unwrap_or_else(|| v.to_string())
                    })
                    .unwrap_or_default();
                out.push(ParsedEvent {
                    kind: TranscriptKind::ToolResult,
                    role: None,
                    occurred_at,
                    content: vec![ContentBlock::ToolResult {
                        tool_call_id: id.clone(),
                        output: output.clone(),
                        is_error: item
                            .get("toolResult")
                            .and_then(|r| r.get("value"))
                            .and_then(|v| v.get("isError"))
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }],
                    text: output,
                    tool_name: None,
                    tool_call_id: Some(id),
                    src_offset: offset,
                    src_length: len,
                });
            }
            // `thinking` / `reasoning` blocks, and anything else with no text
            other => {
                if let Some(t) = item.get("thinking").and_then(Value::as_str) {
                    if !pending.is_empty() {
                        pending.push('\n');
                    }
                    pending.push_str(t);
                } else if !other.is_empty() && item.get("text").is_none() {
                    // Nothing to contribute; the raw row keeps it
                }
            }
        }
    }
    if !pending.is_empty() {
        out.push(message_event(role, pending, occurred_at, offset, len));
    }
    if out.is_empty() {
        out.push(ParsedEvent::unknown(
            &format!("goose:{role}-no-content"),
            offset,
            len,
        ));
    }
    out
}

fn message_event(
    role: &str,
    text: String,
    occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    offset: u64,
    len: u64,
) -> ParsedEvent {
    let kind = match role {
        "user" => TranscriptKind::UserMessage,
        "assistant" => TranscriptKind::AgentMessage,
        _ => TranscriptKind::System,
    };
    text_event(kind, role, text, occurred_at, offset, len)
}

fn text_event(
    kind: TranscriptKind,
    role: &str,
    text: String,
    occurred_at: Option<chrono::DateTime<chrono::Utc>>,
    offset: u64,
    len: u64,
) -> ParsedEvent {
    ParsedEvent {
        kind,
        role: if role.is_empty() {
            None
        } else {
            Some(role.to_owned())
        },
        occurred_at,
        content: vec![ContentBlock::Text { text: text.clone() }],
        text,
        tool_name: None,
        tool_call_id: None,
        src_offset: offset,
        src_length: len,
    }
}

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
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn ms_to_time(ms: i64) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp_millis(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opencode_row() -> Value {
        // The catalog's query joins the parts in as an array of
        // `{type, data}` objects, in `ordinal` order
        serde_json::json!({
            "type": "assistant",
            "data": { "time": { "created": 1_760_000_000_000i64 } },
            "parts": [
                { "type": "reasoning", "data": { "text": "let me look" } },
                { "type": "text", "data": { "text": "I will read the file." } },
                { "type": "tool", "data": {
                    "id": "call_1", "name": "read",
                    "state": { "status": "completed", "input": {"path": "a.rs"},
                               "content": [{"type": "text", "text": "fn main() {}"}] }
                } }
            ]
        })
    }

    fn parse(f: RowFormat, v: &Value) -> Vec<ParsedEvent> {
        let mut p = RowParser::new(f);
        let bytes = serde_json::to_vec(v).expect("serialize");
        p.feed(0, &bytes).expect("feed")
    }

    /// An OpenCode assistant row yields a thought, a message, a call and a
    /// result.
    #[test]
    fn an_opencode_assistant_row_yields_its_parts() {
        let events = parse(RowFormat::OpenCode, &opencode_row());
        assert!(
            events
                .iter()
                .any(|e| e.kind == TranscriptKind::AgentThought)
        );
        assert!(
            events
                .iter()
                .any(|e| e.kind == TranscriptKind::AgentMessage)
        );
        let call = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolCall)
            .expect("a tool call");
        assert_eq!(call.tool_name.as_deref(), Some("read"));
        let result = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolResult)
            .expect("a tool result");
        assert_eq!(result.text, "fn main() {}");
    }

    /// A `streaming` tool has no result yet.
    ///
    /// Emitting one would show a tool that never returned, and the trace
    /// pipeline reads results as the hard signal.
    #[test]
    fn a_streaming_tool_has_no_result() {
        let row = serde_json::json!({
            "type": "assistant",
            "data": {},
            "parts": [{ "type": "tool", "data": {
                "id": "c1", "name": "bash",
                "state": { "status": "streaming", "input": "{}" }
            } }]
        });
        let events = parse(RowFormat::OpenCode, &row);
        assert!(events.iter().any(|e| e.kind == TranscriptKind::ToolCall));
        assert!(
            !events.iter().any(|e| e.kind == TranscriptKind::ToolResult),
            "a streaming tool produced a result"
        );
    }

    /// An OpenCode user row is a user message.
    #[test]
    fn an_opencode_user_row_is_a_user_message() {
        let row = serde_json::json!({ "type": "user", "data": { "text": "hello" } });
        let events = parse(RowFormat::OpenCode, &row);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[0].text, "hello");
    }

    /// An assistant row with no parts joined in is `Unknown`, not silence.
    #[test]
    fn an_opencode_assistant_without_parts_is_unknown() {
        let row = serde_json::json!({ "type": "assistant", "data": {} });
        let events = parse(RowFormat::OpenCode, &row);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
    }

    /// A Goose row with a text block becomes a message.
    #[test]
    fn a_goose_row_with_text_becomes_a_message() {
        let row = serde_json::json!({
            "role": "user",
            "content_json": "[{\"type\":\"text\",\"text\":\"hello\"}]",
            "created_timestamp": 1_760_000_000i64
        });
        let events = parse(RowFormat::Goose, &row);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[0].text, "hello");
        assert!(events[0].occurred_at.is_some(), "the timestamp was dropped");
    }

    /// A Goose `toolRequest` / `toolResponse` pair becomes a call and a result.
    #[test]
    fn a_goose_tool_pair_becomes_call_and_result() {
        let row = serde_json::json!({
            "role": "assistant",
            "content_json": serde_json::to_string(&serde_json::json!([
                {"type":"toolRequest","id":"t1","toolCall":{"value":{"name":"bash","arguments":{"cmd":"ls"}}}},
                {"type":"toolResponse","id":"t1","toolResult":{"value":{"content":[{"type":"text","text":"a.rs"}],"isError":false}}}
            ])).expect("string"),
            "created_timestamp": 1_760_000_000i64
        });
        let events = parse(RowFormat::Goose, &row);
        let call = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolCall)
            .expect("a call");
        assert_eq!(call.tool_name.as_deref(), Some("bash"));
        let result = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolResult)
            .expect("a result");
        assert_eq!(result.text, "a.rs");
    }

    /// Goose's millisecond timestamps are recognised.
    ///
    /// Current versions store seconds and some older rows store milliseconds;
    /// reading a millisecond value as seconds puts the session in the year
    /// 57700.
    #[test]
    fn a_goose_millisecond_timestamp_is_recognised() {
        let row = serde_json::json!({
            "role": "user",
            "content_json": "[{\"type\":\"text\",\"text\":\"x\"}]",
            "created_timestamp": 1_760_000_000_000i64
        });
        let events = parse(RowFormat::Goose, &row);
        let at = events[0].occurred_at.expect("a timestamp");
        assert_eq!(at.format("%Y").to_string(), "2025", "{at}");
    }

    /// A row fed twice emits nothing the second time.
    #[test]
    fn feeding_twice_does_not_duplicate() {
        let mut p = RowParser::new(RowFormat::Goose);
        let bytes = serde_json::to_vec(&serde_json::json!({
            "role": "user",
            "content_json": "[{\"type\":\"text\",\"text\":\"x\"}]"
        }))
        .expect("serialize");
        assert!(!p.feed(0, &bytes).expect("first").is_empty());
        assert!(p.feed(0, &bytes).expect("second").is_empty());
    }

    /// A row that is not JSON is one malformed event.
    #[test]
    fn a_non_json_row_is_malformed() {
        let mut p = RowParser::new(RowFormat::Goose);
        let events = p.feed(0, b"not json").expect("feed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
    }

    /// A checkpoint round-trips.
    #[test]
    fn the_checkpoint_round_trips() {
        let mut p = RowParser::new(RowFormat::Goose);
        let bytes = serde_json::to_vec(&serde_json::json!({
            "role": "user",
            "content_json": "[{\"type\":\"text\",\"text\":\"x\"}]"
        }))
        .expect("serialize");
        p.feed(0, &bytes).expect("first");
        let state = p.checkpoint();
        let mut restored = RowParser::restore(RowFormat::Goose, &state).expect("restore");
        assert!(
            restored.feed(0, &bytes).expect("again").is_empty(),
            "a restored parser re-emitted a row it had already emitted"
        );
    }
}

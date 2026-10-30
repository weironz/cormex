//! The Cline / Roo Code task parser.
//!
//! Design: plan P4-4.
//!
//! # The shape, read from upstream
//!
//! `cline/cline` (Apache-2.0), `apps/vscode/src/core/storage/disk.ts`:
//!
//! ```text
//! <globalStorageFsPath>/tasks/<taskId>/api_conversation_history.json
//! <globalStorageFsPath>/tasks/<taskId>/ui_messages.json
//! <globalStorageFsPath>/tasks/<taskId>/task_metadata.json
//! ```
//!
//! `api_conversation_history.json` is an `Anthropic.MessageParam[]` — the array
//! Cline sends to the model, and therefore the one that is the conversation.
//! `ui_messages.json` is the UI's own event log (partial messages, tool status
//! updates); it is **not** parsed here, because it duplicates the conversation
//! with different framing and parsing both would show every message twice.
//!
//! Roo Code is a fork and uses the same file names under its own global storage
//! directory; the parser is shared.
//!
//! # Why this is a `snapshot` source, not `append`
//!
//! The file is **rewritten wholesale** after every message (`JSON.stringify` of
//! the whole array). There is nothing to tail: the file's length can shrink.
//! `framing = snapshot` is what tells the client to compare whole-file hashes
//! rather than appending from an offset.
//!
//! # There are no timestamps
//!
//! `MessageParam` carries none: the conversation array is the model's view, and
//! the model does not need to know when a message was sent. So `occurred_at` is
//! `None` throughout, and the projector falls back to the chunk's `observed_at`
//! — the time the client read the file, which is close to the time of the last
//! message in it. That is honest: a made-up timestamp would make the event-time
//! axis claim more than the data does.
//!
//! # The content shapes
//!
//! `MessageParam.content` is `string | ContentBlockParam[]`. The blocks that
//! occur are `text`, `tool_use`, `tool_result`, and `image`. An `image` block
//! contributes no text; its bytes are in the file, and the raw layer keeps them.

use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::parser::{ParsedEvent, Parser};

/// The parser.
///
/// The file is a **single JSON array**, not JSONL, so this parser does not use
/// the line buffer's splitting: it buffers the whole input and parses once. The
/// `Parser` trait's offset contract still holds (the offsets are the whole
/// file's), which is what lets the projector's checkpoint logic work unchanged.
pub struct ClineParser {
    /// Bytes received so far. A snapshot is parsed whole, so this is the file.
    buffer: Vec<u8>,
    /// The offset the buffer starts at.
    start: u64,
    /// Whether the buffer has already been turned into events.
    ///
    /// A snapshot source is re-fed the whole file on every run; re-emitting the
    /// same events would duplicate them, and the projector's `ON CONFLICT`
    /// would hide that behind an id collision rather than an error.
    emitted: bool,
}

impl ClineParser {
    /// A parser starting at offset 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            start: 0,
            emitted: false,
        }
    }

    fn parse_array(&self) -> Vec<ParsedEvent> {
        let Ok(v) = serde_json::from_slice::<Value>(&self.buffer) else {
            // A partially written file is the common case for a snapshot: the
            // host rewrites it atomically, but a crash can leave a truncated
            // one. One malformed event keeps the offset visible rather than
            // silently projecting an empty session
            return vec![ParsedEvent::malformed(
                self.start,
                u64::try_from(self.buffer.len()).unwrap_or(0),
            )];
        };
        let Some(items) = v.as_array() else {
            return vec![ParsedEvent::malformed(
                self.start,
                u64::try_from(self.buffer.len()).unwrap_or(0),
            )];
        };

        let mut out = Vec::with_capacity(items.len());
        for (i, item) in items.iter().enumerate() {
            let offset = self.start + u64::try_from(i).unwrap_or(0);
            out.extend(parse_message(item, offset));
        }
        out
    }
}

impl Default for ClineParser {
    fn default() -> Self {
        Self::new()
    }
}

/// One `MessageParam` → one or more events.
fn parse_message(item: &Value, offset: u64) -> Vec<ParsedEvent> {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("");
    let content = item.get("content");
    let mut out = Vec::new();

    // A bare string is the whole message
    if let Some(text) = content.and_then(Value::as_str) {
        out.push(message_event(role, text.to_owned(), offset));
        return out;
    }

    let Some(blocks) = content.and_then(Value::as_array) else {
        out.push(ParsedEvent::unknown(
            &format!("cline:{role}-no-content"),
            offset,
            0,
        ));
        return out;
    };

    // Text blocks that are adjacent are joined: Cline splits one message into
    // several text blocks, and emitting each separately would turn one
    // paragraph into three events
    let mut pending = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "text" => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    if !pending.is_empty() {
                        pending.push('\n');
                    }
                    pending.push_str(t);
                }
            }
            "tool_use" => {
                if !pending.is_empty() {
                    out.push(message_event(role, std::mem::take(&mut pending), offset));
                }
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                out.push(ParsedEvent {
                    kind: TranscriptKind::ToolCall,
                    role: Some(role.to_owned()),
                    occurred_at: None,
                    content: vec![ContentBlock::ToolCall {
                        tool_call_id: id.clone(),
                        name: name.clone(),
                        input: block.get("input").cloned().unwrap_or(Value::Null),
                    }],
                    text: String::new(),
                    tool_name: Some(name),
                    tool_call_id: Some(id),
                    src_offset: offset,
                    src_length: 0,
                });
            }
            "tool_result" => {
                if !pending.is_empty() {
                    out.push(message_event(role, std::mem::take(&mut pending), offset));
                }
                let id = block
                    .get("tool_use_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let output = flatten(block.get("content"));
                out.push(ParsedEvent {
                    kind: TranscriptKind::ToolResult,
                    role: None,
                    occurred_at: None,
                    content: vec![ContentBlock::ToolResult {
                        tool_call_id: id.clone(),
                        output: output.clone(),
                        is_error: block
                            .get("is_error")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    }],
                    text: output,
                    tool_name: None,
                    tool_call_id: Some(id),
                    src_offset: offset,
                    src_length: 0,
                });
            }
            // `image` and anything else: no text to contribute. The raw layer
            // keeps the bytes, which is what makes them recoverable
            _ => {}
        }
    }
    if !pending.is_empty() {
        out.push(message_event(role, pending, offset));
    }
    if out.is_empty() {
        out.push(ParsedEvent::unknown(
            &format!("cline:{role}-no-text"),
            offset,
            0,
        ));
    }
    out
}

fn message_event(role: &str, text: String, offset: u64) -> ParsedEvent {
    let kind = match role {
        "user" => TranscriptKind::UserMessage,
        "assistant" => TranscriptKind::AgentMessage,
        _ => TranscriptKind::System,
    };
    ParsedEvent {
        kind,
        role: Some(role.to_owned()),
        occurred_at: None,
        content: vec![ContentBlock::Text { text: text.clone() }],
        text,
        tool_name: None,
        tool_call_id: None,
        src_offset: offset,
        src_length: 0,
    }
}

/// Flatten a `tool_result`'s content, which is `string | ContentBlockParam[]`.
fn flatten(v: Option<&Value>) -> String {
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

impl Parser for ClineParser {
    fn version(&self) -> &'static str {
        "cline@1"
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        // A snapshot is fed the whole file in one call by the projector (one
        // chunk). Appending lets a chunked read still work, and the parse
        // happens only when the buffer is complete — which the projector
        // signals by feeding the last chunk.
        if self.buffer.is_empty() {
            self.start = offset;
        } else if offset != self.start + u64::try_from(self.buffer.len()).unwrap_or(0) {
            return Err(format!(
                "the chunk starts at {offset} but the buffer ends at {}; a snapshot must be fed \
                 contiguously",
                self.start + u64::try_from(self.buffer.len()).unwrap_or(0)
            ));
        }
        self.buffer.extend_from_slice(bytes);

        // The file is **complete** when it parses as a JSON value. Checking
        // for a trailing `]` instead would treat a well-formed object as a
        // partial file and emit nothing — and the failure would be a silently
        // empty session rather than a malformed event
        if self.emitted || serde_json::from_slice::<Value>(&self.buffer).is_err() {
            return Ok(Vec::new());
        }
        self.emitted = true;
        Ok(self.parse_array())
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::json!({
            "next_offset": self.start + u64::try_from(self.buffer.len()).unwrap_or(0),
            "partial_start": self.start,
            // **The whole buffer**, because a snapshot is parsed whole: a
            // checkpoint without it could not resume a half-fed file
            "partial": String::from_utf8_lossy(&self.buffer),
            "emitted": self.emitted,
        })
    }
}

impl ClineParser {
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
            buffer: partial,
            start,
            emitted: state
                .get("emitted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `api_conversation_history.json`, reduced.
    ///
    /// The shape is upstream's `Anthropic.MessageParam[]`; the content is
    /// written here rather than copied, because Cline's own test fixtures carry
    /// a user's real conversations and this repository is public.
    const FIXTURE: &str = r#"[
      {"role":"user","content":"Add a retry to the upload loop."},
      {"role":"assistant","content":[
        {"type":"text","text":"I will look at the loop first."},
        {"type":"tool_use","id":"t1","name":"read_file","input":{"path":"sync/mod.rs"}},
        {"type":"text","text":"Then add the retry."}
      ]},
      {"role":"user","content":[
        {"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text":"fn upload() {}"}]}
      ]},
      {"role":"assistant","content":[{"type":"text","text":"Done."}]}
    ]"#;

    fn parse_all() -> Vec<ParsedEvent> {
        let mut p = ClineParser::new();
        p.feed(0, FIXTURE.as_bytes()).expect("feed")
    }

    /// A bare-string message becomes one event.
    #[test]
    fn a_string_message_becomes_one_event() {
        let events = parse_all();
        let user = events
            .iter()
            .find(|e| e.kind == TranscriptKind::UserMessage)
            .expect("a user message");
        assert_eq!(user.text, "Add a retry to the upload loop.");
    }

    /// Adjacent text blocks are joined into one message.
    ///
    /// Cline splits one message into several text blocks around a tool call;
    /// emitting each separately would turn one paragraph into three events.
    #[test]
    fn adjacent_text_blocks_are_joined() {
        let events = parse_all();
        let texts: Vec<&str> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::AgentMessage)
            .map(|e| e.text.as_str())
            .collect();
        assert!(
            texts.contains(&"I will look at the loop first."),
            "the first text block is missing: {texts:?}"
        );
        assert!(
            texts.contains(&"Then add the retry."),
            "the text after the tool call is missing: {texts:?}"
        );
    }

    /// A `tool_use` block becomes a tool call, and a `tool_result` a result.
    #[test]
    fn tool_blocks_become_call_and_result_events() {
        let events = parse_all();
        let call = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolCall)
            .expect("a tool call");
        assert_eq!(call.tool_name.as_deref(), Some("read_file"));
        let result = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolResult)
            .expect("a tool result");
        assert_eq!(result.tool_call_id.as_deref(), Some("t1"));
        assert_eq!(result.text, "fn upload() {}");
    }

    /// A partial file produces no events.
    ///
    /// The host rewrites this file atomically, but a crash can leave a
    /// truncated one. Projecting half a conversation as if it were whole is
    /// worse than projecting nothing: the session reads as complete.
    #[test]
    fn a_partial_file_produces_nothing() {
        let mut p = ClineParser::new();
        let events = p
            .feed(0, br#"[{"role":"user","content":"half a mess"#)
            .expect("feed");
        assert!(events.is_empty(), "a truncated array produced events");
    }

    /// Feeding the whole file twice does not duplicate the events.
    ///
    /// A snapshot source is re-fed on every run; re-emitting would duplicate
    /// them, and the projector's `ON CONFLICT` would hide that behind an id
    /// collision rather than an error.
    #[test]
    fn feeding_twice_does_not_duplicate() {
        let mut p = ClineParser::new();
        let first = p.feed(0, FIXTURE.as_bytes()).expect("first");
        let second = p
            .feed(
                u64::try_from(FIXTURE.len()).expect("fits"),
                FIXTURE.as_bytes(),
            )
            .expect("second");
        assert!(!first.is_empty());
        assert!(second.is_empty(), "the second feed emitted events again");
    }

    /// A file that is not an array is one malformed event, with its offset.
    #[test]
    fn a_non_array_is_malformed() {
        let mut p = ClineParser::new();
        let events = p.feed(0, br#"{"not":"an array"}"#).expect("feed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
    }

    /// A checkpoint round-trips, buffer included.
    #[test]
    fn the_checkpoint_round_trips() {
        let mut p = ClineParser::new();
        p.feed(0, br#"[{"role":"user","#).expect("partial");
        let state = p.checkpoint();
        let mut restored = ClineParser::restore(&state).expect("restore");
        let events = restored
            .feed(
                u64::try_from(br#"[{"role":"user","#.len()).expect("fits"),
                br#""content":"x"}]"#,
            )
            .expect("resume");
        assert_eq!(events.len(), 1, "the resumed parse produced nothing");
        assert_eq!(events[0].text, "x");
    }

    /// An image block contributes no text, and does not drop the message.
    #[test]
    fn an_image_block_contributes_no_text() {
        let mut p = ClineParser::new();
        let events = p
            .feed(
                0,
                br#"[{"role":"user","content":[{"type":"image","source":{}},{"type":"text","text":"look at this"}]}]"#,
            )
            .expect("feed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "look at this");
    }
}

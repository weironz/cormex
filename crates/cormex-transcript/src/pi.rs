//! The pi / omp session parser.
//!
//! # Where the format came from
//!
//! pi documents it (`packages/coding-agent/docs/session-format.md` in
//! `earendil-works/pi`, MIT), and this parser was checked against sessions the
//! real CLIs wrote (pi 1.5.0, omp 18.18.0). omp is a fork of pi and writes the
//! same file; one parser serves both.
//!
//! ```text
//! {"type":"session","version":3,"id":"…","timestamp":"…","cwd":"/repo"}
//! {"type":"model_change","id":"…","parentId":null,"provider":"…","modelId":"…"}
//! {"type":"message","id":"…","parentId":"…","timestamp":"…",
//!  "message":{"role":"user","content":[{"type":"text","text":"…"}]}}
//! {"type":"message",…,"message":{"role":"assistant","content":[
//!     {"type":"thinking","thinking":"…"},
//!     {"type":"text","text":"…"},
//!     {"type":"toolCall","id":"call_…","name":"read","arguments":{…}}]}}
//! {"type":"message",…,"message":{"role":"toolResult","toolCallId":"call_…",
//!  "toolName":"read","content":[{"type":"text","text":"…"}],"isError":false}}
//! ```
//!
//! # Branches are kept, not resolved
//!
//! Entries form a tree through `id` / `parentId`: `/tree` in pi rewinds to an
//! earlier entry and continues from there **in the same file**. This parser
//! emits every entry in file order and does not pick a branch. The archive
//! never drops what was said, and choosing "the" branch is a reading decision
//! the raw bytes keep available.
//!
//! # omp's first line is rewritten in place
//!
//! omp starts the file with `{"type":"title",…,"pad":"   …"}` and later
//! overwrites that line with the generated title, padded to the same byte
//! length so nothing after it moves. Capture is append-only and does not see
//! the rewrite (the bytes before the uploaded offset are not re-read), so the
//! archived copy keeps the first, usually empty, title. It is projected as
//! `Meta` and never used as the session's title; the list takes the title from
//! the first question instead.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct PiParser {
    lines: LineBuffer,
}

impl PiParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
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
        Ok(Self {
            lines: LineBuffer::from_state(next, start, partial),
        })
    }

    fn parse_line(offset: u64, length: u64, bytes: &[u8]) -> Vec<ParsedEvent> {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            return vec![ParsedEvent::malformed(offset, length)];
        };
        let at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let record_type = v.get("type").and_then(Value::as_str).unwrap_or("");
        let event =
            |kind, role: Option<&str>, content: Vec<ContentBlock>, text: String| ParsedEvent {
                kind,
                role: role.map(str::to_owned),
                occurred_at: at,
                content,
                text,
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            };

        if record_type != "message" {
            // `custom_message` is what an extension shows in the conversation;
            // everything else (`session`, `model_change`, `compaction`,
            // `label`, omp's `title` and `custom`, …) is bookkeeping
            if record_type == "custom_message"
                && let Some(text) = text_of(v.get("content"))
                && !text.trim().is_empty()
            {
                return vec![event(
                    TranscriptKind::System,
                    None,
                    vec![ContentBlock::Text { text: text.clone() }],
                    text,
                )];
            }
            let raw = if record_type.is_empty() {
                "pi:untyped".to_owned()
            } else {
                format!("pi:{record_type}")
            };
            let mut e = ParsedEvent::unknown(&raw, offset, length);
            e.kind = TranscriptKind::Meta;
            e.occurred_at = at;
            return vec![e];
        }

        let Some(message) = v.get("message") else {
            return vec![ParsedEvent::unknown(
                "pi:message-without-body",
                offset,
                length,
            )];
        };
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        let content = message.get("content");

        match role {
            "user" => {
                let mut out = Vec::new();
                for block in blocks(content) {
                    match block.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" => {
                            let text = str_field(block, "text");
                            if !text.trim().is_empty() {
                                out.push(event(
                                    TranscriptKind::UserMessage,
                                    Some("user"),
                                    vec![ContentBlock::Text { text: text.clone() }],
                                    text,
                                ));
                            }
                        }
                        // Images are kept in the raw bytes; the projection does
                        // not inline base64
                        other => out.push(unknown_block(other, offset, length, at)),
                    }
                }
                if out.is_empty() {
                    // A plain-string content, which older sessions used
                    if let Some(text) = content.and_then(Value::as_str)
                        && !text.trim().is_empty()
                    {
                        out.push(event(
                            TranscriptKind::UserMessage,
                            Some("user"),
                            vec![ContentBlock::Text {
                                text: text.to_owned(),
                            }],
                            text.to_owned(),
                        ));
                    }
                }
                or_empty(out, "pi:empty-user-message", offset, length, at)
            }
            "assistant" => {
                let mut out = Vec::new();
                for block in blocks(content) {
                    match block.get("type").and_then(Value::as_str).unwrap_or("") {
                        "text" => {
                            let text = str_field(block, "text");
                            if !text.trim().is_empty() {
                                out.push(event(
                                    TranscriptKind::AgentMessage,
                                    Some("assistant"),
                                    vec![ContentBlock::Text { text: text.clone() }],
                                    text,
                                ));
                            }
                        }
                        "thinking" => {
                            let text = str_field(block, "thinking");
                            if !text.trim().is_empty() {
                                out.push(event(
                                    TranscriptKind::AgentThought,
                                    Some("assistant"),
                                    vec![ContentBlock::Thought { text: text.clone() }],
                                    text,
                                ));
                            }
                        }
                        "toolCall" => {
                            let id = str_field(block, "id");
                            let name = str_field(block, "name");
                            let input = block.get("arguments").cloned().unwrap_or(Value::Null);
                            let mut e = event(
                                TranscriptKind::ToolCall,
                                Some("assistant"),
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
                        other => out.push(unknown_block(other, offset, length, at)),
                    }
                }
                // An assistant entry with no blocks is a turn that was cut off
                // (an error or an abort): kept, so the gap is visible
                or_empty(out, "pi:empty-assistant-message", offset, length, at)
            }
            "toolResult" => {
                let id = str_field(message, "toolCallId");
                let name = str_field(message, "toolName");
                let output = text_of(content).unwrap_or_default();
                let is_error = message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut e = event(
                    TranscriptKind::ToolResult,
                    Some("tool"),
                    vec![ContentBlock::ToolResult {
                        tool_call_id: id.clone(),
                        output: output.clone(),
                        is_error,
                    }],
                    output,
                );
                e.tool_name = (!name.is_empty()).then_some(name);
                e.tool_call_id = Some(id);
                vec![e]
            }
            // A `!command` the user ran in the TUI, recorded with its output
            "bashExecution" => {
                let command = str_field(message, "command");
                let output = str_field(message, "output");
                let text = format!("$ {command}\n{output}");
                let mut e = event(
                    TranscriptKind::ToolResult,
                    Some("user"),
                    vec![ContentBlock::ToolResult {
                        tool_call_id: String::new(),
                        output: text.clone(),
                        is_error: message
                            .get("exitCode")
                            .and_then(Value::as_i64)
                            .is_some_and(|c| c != 0),
                    }],
                    text,
                );
                e.tool_name = Some("bash".to_owned());
                vec![e]
            }
            // The system prompt. Empty `content` with the prompt split into
            // `sections` is how pi 1.x writes it
            "system" => {
                let text = text_of(content)
                    .filter(|t| !t.trim().is_empty())
                    .or_else(|| {
                        message.get("sections").and_then(Value::as_object).map(|s| {
                            s.values()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\n\n")
                        })
                    })
                    .unwrap_or_default();
                vec![event(
                    TranscriptKind::System,
                    Some("system"),
                    vec![ContentBlock::Text { text: text.clone() }],
                    text,
                )]
            }
            // `custom` (pi 3+) and `hookMessage` (before): what an extension
            // injected
            _ => {
                let text = text_of(content).unwrap_or_default();
                if text.trim().is_empty() {
                    let mut e = ParsedEvent::unknown(&format!("pi:role:{role}"), offset, length);
                    e.occurred_at = at;
                    return vec![e];
                }
                vec![event(
                    TranscriptKind::System,
                    Some(role),
                    vec![ContentBlock::Text { text: text.clone() }],
                    text,
                )]
            }
        }
    }
}

fn blocks(content: Option<&Value>) -> &[Value] {
    content.and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// The text of a content value: a string, or the `text` blocks of an array.
fn text_of(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn unknown_block(
    block_type: &str,
    offset: u64,
    length: u64,
    at: Option<DateTime<Utc>>,
) -> ParsedEvent {
    let mut e = ParsedEvent::unknown(&format!("pi:block:{block_type}"), offset, length);
    e.occurred_at = at;
    e
}

fn or_empty(
    mut out: Vec<ParsedEvent>,
    raw: &str,
    offset: u64,
    length: u64,
    at: Option<DateTime<Utc>>,
) -> Vec<ParsedEvent> {
    if out.is_empty() {
        let mut e = ParsedEvent::unknown(raw, offset, length);
        e.occurred_at = at;
        out.push(e);
    }
    out
}

impl Default for PiParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser for PiParser {
    fn version(&self) -> &'static str {
        "pi@1"
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        let lines = self.lines.feed(offset, bytes).map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        for l in lines {
            let len = u64::try_from(l.bytes.len()).unwrap_or(0);
            out.extend(Self::parse_line(l.offset, len, &l.bytes));
        }
        Ok(out)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(body: &str) -> Vec<ParsedEvent> {
        PiParser::new().feed(0, body.as_bytes()).expect("feed")
    }

    #[test]
    fn a_turn_with_thinking_and_a_tool_call_becomes_ordered_events() {
        let body = concat!(
            r#"{"type":"session","version":3,"id":"s","timestamp":"2026-11-07T07:39:32.807Z","cwd":"/home/user/r"}"#,
            "\n",
            r#"{"type":"message","id":"a","parentId":null,"timestamp":"2026-11-07T07:39:33Z","message":{"role":"user","content":[{"type":"text","text":"读一下 app.py"}]}}"#,
            "\n",
            r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-11-07T07:39:40Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"read it"},{"type":"text","text":"I'll read the file."},{"type":"toolCall","id":"call_1","name":"read","arguments":{"path":"app.py"}}]}}"#,
            "\n",
            r#"{"type":"message","id":"c","parentId":"b","timestamp":"2026-11-07T07:39:41Z","message":{"role":"toolResult","toolCallId":"call_1","toolName":"read","content":[{"type":"text","text":"def add(): pass"}],"isError":false}}"#,
            "\n",
        );
        let kinds: Vec<_> = parse(body).iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                TranscriptKind::Meta,
                TranscriptKind::UserMessage,
                TranscriptKind::AgentThought,
                TranscriptKind::AgentMessage,
                TranscriptKind::ToolCall,
                TranscriptKind::ToolResult,
            ],
            "one event per block, in file order, with the session header kept as Meta"
        );
        let events = parse(body);
        assert_eq!(events[4].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            events[5].tool_call_id.as_deref(),
            Some("call_1"),
            "the result carries the call's id, so the UI can pair them"
        );
        assert_eq!(events[5].tool_name.as_deref(), Some("read"));
    }

    #[test]
    fn the_system_prompt_split_into_sections_is_rejoined() {
        let body = r#"{"type":"message","id":"x","parentId":null,"timestamp":"2026-11-07T07:39:32Z","message":{"role":"system","content":"","sections":{"preamble":"You are pi","tools":"<tools/>"}}}
"#;
        let e = &parse(body)[0];
        assert_eq!(e.kind, TranscriptKind::System);
        assert!(
            e.text.contains("You are pi") && e.text.contains("<tools/>"),
            "pi 1.x writes an empty content and the prompt in sections; showing nothing would \
             hide what the model was told: {:?}",
            e.text
        );
    }

    #[test]
    fn omp_title_and_custom_records_are_bookkeeping() {
        let body = concat!(
            r#"{"type":"title","v":1,"title":"","updatedAt":"2026-11-07T07:46:41.222Z","pad":"    "}"#,
            "\n",
            r#"{"type":"custom","customType":"date-cwd-reminder","data":{},"id":"c","parentId":null,"timestamp":"2026-11-07T07:46:41.629Z"}"#,
            "\n",
        );
        let events = parse(body);
        assert!(
            events.iter().all(|e| e.kind == TranscriptKind::Meta),
            "omp's title line and its injected reminders are not things anyone said"
        );
    }

    #[test]
    fn a_broken_line_is_kept_as_malformed() {
        let events = parse("{not json\n");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0].content[0],
            ContentBlock::Unknown { raw_type } if raw_type == "malformed-json"
        ));
    }
}

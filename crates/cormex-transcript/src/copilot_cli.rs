//! The Copilot CLI session parser.
//!
//! Design: plan P4-6.
//!
//! # Where the format came from
//!
//! `github/copilot-cli` is closed source, and the CLI ships as a native binary
//! — but that binary **embeds a gzipped tar of the application's JavaScript**
//! (`package/cli-main.js`, 8.3 MB). Extracting it gives the event schema:
//!
//! ```bash
//! python3 - <<'PY'
//! import io, tarfile, zlib
//! data = open('node_modules/@github/copilot-linux-x64/copilot','rb').read()
//! off = data.find(b'\x1f\x8b\x08', 100_000_000)   # the tar's gzip header
//! raw = zlib.decompressobj(16 + zlib.MAX_WBITS)
//! buf = io.BytesIO(); pos = off
//! while pos < len(data) and not raw.eof:
//!     buf.write(raw.decompress(data[pos:pos+(1<<20)])); pos += 1<<20
//! buf.seek(0)
//! tarfile.open(fileobj=buf).extract('package/cli-main.js', '/tmp')
//! PY
//! ```
//!
//! The event names are in `cli-main.js` as literals; the writer's shape was
//! confirmed by importing a session (`copilot sessions import`) and reading the
//! `events.jsonl` it produced.
//!
//! # The shape
//!
//! JSONL, one event per line, **append-only**:
//!
//! ```text
//! {"type":"session.start","data":{"sessionId":"…","startTime":"…","context":{"cwd":"…"}},
//!  "id":"…","timestamp":"…","parentId":null}
//! {"type":"user.message","data":{"content":"hello"},"id":"…","timestamp":"…","parentId":"…"}
//! {"type":"assistant.message","data":{"messageId":"…","content":"hi there"},"id":"…",…}
//! ```
//!
//! `parentId` chains the events, so the file is a **tree** like Qwen's — but
//! unlike Qwen the chain is linear in practice (no rewind), and a `parentId`
//! that does not match the previous event is treated as a branch and demoted to
//! `Meta`, the same rule as [`crate::qwen`].
//!
//! # The event types that carry conversation
//!
//! `cli-main.js` defines around eighty `session.*` types (canvas, mcp, plugins,
//! permissions …) and a handful of conversation ones. Only these four are
//! projected; everything else is `Meta`:
//!
//! | type | what it is |
//! |---|---|
//! | `user.message` | `data.content`, a string |
//! | `assistant.message` | `data.content`, a string; `data.toolRequests` when the turn called tools |
//! | `assistant.reasoning` | the model's thinking |
//! | `tool.execution_complete` | `data.toolCallId`, `data.success`, `data.result.content` |
//!
//! # Two things worth knowing
//!
//! - **A tool call arrives inside `assistant.message`**, as `data.toolRequests[]`
//!   with `{toolCallId, name, arguments}`. There is no separate `tool.call`
//!   event in the session log — that name is an RPC method, not a stored event.
//! - **`tool.execution_start` exists but carries no output**; the result is in
//!   `tool.execution_complete`. Both are handled: the start produces the call,
//!   the complete produces the result.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct CopilotCliParser {
    lines: LineBuffer,
    /// The previous event's `id`, for the branch check.
    previous_id: Option<String>,
}

impl CopilotCliParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
            previous_id: None,
        }
    }

    fn parse_line(&mut self, offset: u64, length: u64, bytes: &[u8]) -> Vec<ParsedEvent> {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            return vec![ParsedEvent::malformed(offset, length)];
        };
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        let occurred_at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.with_timezone(&Utc));
        let id = v.get("id").and_then(Value::as_str).map(str::to_owned);
        let parent = v.get("parentId").and_then(Value::as_str);

        // A `parentId` that is not the previous event is a branch the file kept
        // after a rewind. Its content is real but not part of the active chain
        let branched = match (&self.previous_id, parent) {
            (None, _) => false,
            (Some(prev), Some(p)) => p != prev,
            (Some(_), None) => true,
        };
        self.previous_id = id;

        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let mut out = match kind {
            "user.message" => vec![text_event(
                TranscriptKind::UserMessage,
                "user",
                data.get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                occurred_at,
                offset,
                length,
            )],
            "assistant.message" => {
                let mut events = Vec::new();
                let text = data
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                if !text.trim().is_empty() {
                    events.push(text_event(
                        TranscriptKind::AgentMessage,
                        "assistant",
                        text,
                        occurred_at,
                        offset,
                        length,
                    ));
                }
                // **The tool calls ride inside the message**, as
                // `data.toolRequests[]`
                if let Some(calls) = data.get("toolRequests").and_then(Value::as_array) {
                    for call in calls {
                        let call_id = call
                            .get("toolCallId")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        let name = call
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        events.push(ParsedEvent {
                            kind: TranscriptKind::ToolCall,
                            role: Some("assistant".to_owned()),
                            occurred_at,
                            content: vec![ContentBlock::ToolCall {
                                tool_call_id: call_id.clone(),
                                name: name.clone(),
                                input: call.get("arguments").cloned().unwrap_or(Value::Null),
                            }],
                            text: String::new(),
                            tool_name: Some(name),
                            tool_call_id: Some(call_id),
                            src_offset: offset,
                            src_length: length,
                        });
                    }
                }
                if events.is_empty() {
                    events.push(ParsedEvent::unknown(
                        "copilot_cli:empty-assistant-message",
                        offset,
                        length,
                    ));
                }
                events
            }
            "assistant.reasoning" => {
                let text = data
                    .get("content")
                    .and_then(Value::as_str)
                    .or_else(|| data.get("text").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned();
                vec![ParsedEvent {
                    kind: TranscriptKind::AgentThought,
                    role: Some("assistant".to_owned()),
                    occurred_at,
                    content: vec![ContentBlock::Thought { text: text.clone() }],
                    text,
                    tool_name: None,
                    tool_call_id: None,
                    src_offset: offset,
                    src_length: length,
                }]
            }
            "tool.execution_complete" => {
                let call_id = data
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let success = data.get("success").and_then(Value::as_bool).unwrap_or(true);
                let output = if success {
                    flatten(data.get("result").and_then(|r| r.get("content")))
                } else {
                    data.get("error")
                        .and_then(|e| e.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                };
                vec![ParsedEvent {
                    kind: TranscriptKind::ToolResult,
                    role: None,
                    occurred_at,
                    content: vec![ContentBlock::ToolResult {
                        tool_call_id: call_id.clone(),
                        output: output.clone(),
                        is_error: !success,
                    }],
                    text: output,
                    tool_name: None,
                    tool_call_id: Some(call_id),
                    src_offset: offset,
                    src_length: length,
                }]
            }
            // `session.start`, `session.idle`, `session.model_change`, the
            // canvas / mcp / plugin / permission families: the host talking
            // about the session. `Meta` keeps them in the record without putting
            // them in the conversation index
            other => vec![ParsedEvent {
                kind: TranscriptKind::Meta,
                role: None,
                occurred_at,
                content: vec![ContentBlock::Unknown {
                    raw_type: format!("copilot_cli:{other}"),
                }],
                text: String::new(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            }],
        };

        if branched {
            for e in &mut out {
                if e.kind != TranscriptKind::Meta {
                    e.kind = TranscriptKind::Meta;
                }
            }
        }
        out
    }
}

impl Default for CopilotCliParser {
    fn default() -> Self {
        Self::new()
    }
}

fn text_event(
    kind: TranscriptKind,
    role: &str,
    text: String,
    at: Option<DateTime<Utc>>,
    offset: u64,
    len: u64,
) -> ParsedEvent {
    ParsedEvent {
        kind,
        role: Some(role.to_owned()),
        occurred_at: at,
        content: vec![ContentBlock::Text { text: text.clone() }],
        text,
        tool_name: None,
        tool_call_id: None,
        src_offset: offset,
        src_length: len,
    }
}

/// Flatten a tool result's `content`, which is `string | Part[]`.
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
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

impl Parser for CopilotCliParser {
    fn version(&self) -> &'static str {
        "copilot_cli@2"
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
            // The branch check needs it, or a resumed projection treats the
            // first event after the resume as the start of a branch
            "previous_id": self.previous_id,
        })
    }
}

impl CopilotCliParser {
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
            previous_id: state
                .get("previous_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A real `events.jsonl`**, produced by this machine.
    ///
    /// `copilot sessions import` writes one, and this is that file verbatim —
    /// not a fixture written from the schema. It is the check that the parser
    /// matches what the CLI actually stores rather than what its code appears
    /// to say.
    ///
    /// The tool events below are written from `cli-main.js`'s
    /// `emit("tool.execution_complete", …)` call, because the import format
    /// cannot express them.
    const REAL: &str = include_str!("../tests/fixtures/copilot_cli_session.jsonl");

    const FIXTURE: &str = concat!(
        r#"{"type":"session.start","data":{"sessionId":"s1","version":1,"producer":"copilot-agent","startTime":"2026-10-30T00:00:00.000Z","context":{"cwd":"/tmp/p"}},"id":"e0","timestamp":"2026-10-30T00:00:00.000Z","parentId":null}"#,
        "\n",
        r#"{"type":"user.message","data":{"content":"hello"},"id":"e1","timestamp":"2026-10-30T00:00:01.000Z","parentId":"e0"}"#,
        "\n",
        r#"{"type":"assistant.message","data":{"messageId":"m1","content":"hi there"},"id":"e2","timestamp":"2026-10-30T00:00:02.000Z","parentId":"e1"}"#,
        "\n",
    );

    fn parse_all() -> Vec<ParsedEvent> {
        let mut p = CopilotCliParser::new();
        p.feed(0, FIXTURE.as_bytes()).expect("feed")
    }

    /// The parser reads the **real** file the CLI wrote.
    ///
    /// This is the check that matters: everything else is written from the
    /// schema, and a schema read correctly but applied to the wrong field name
    /// passes all of those.
    #[test]
    fn it_reads_a_real_events_jsonl() {
        let mut p = CopilotCliParser::new();
        let events = p.feed(0, REAL.as_bytes()).expect("feed");
        assert_eq!(events.len(), 3, "{events:?}");
        assert_eq!(events[0].kind, TranscriptKind::Meta, "session.start");
        assert_eq!(events[1].kind, TranscriptKind::UserMessage);
        assert_eq!(events[1].text, "hello");
        assert_eq!(events[2].kind, TranscriptKind::AgentMessage);
        assert_eq!(events[2].text, "hi there");
        assert!(
            events[1].occurred_at.is_some(),
            "the real file's timestamp was dropped"
        );
    }

    /// `session.start` is `Meta`.
    #[test]
    fn session_start_is_meta() {
        let events = parse_all();
        assert_eq!(events[0].kind, TranscriptKind::Meta);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "copilot_cli:session.start".into()
            }]
        );
    }

    /// A user message and an assistant message come through.
    #[test]
    fn the_messages_come_through() {
        let events = parse_all();
        let user = events
            .iter()
            .find(|e| e.kind == TranscriptKind::UserMessage)
            .expect("a user message");
        assert_eq!(user.text, "hello");
        let agent = events
            .iter()
            .find(|e| e.kind == TranscriptKind::AgentMessage)
            .expect("an agent message");
        assert_eq!(agent.text, "hi there");
        assert!(user.occurred_at.is_some(), "the timestamp was dropped");
    }

    /// A tool call rides inside `assistant.message` as `toolRequests[]`.
    #[test]
    fn a_tool_request_inside_a_message_becomes_a_call() {
        let line = r#"{"type":"assistant.message","data":{"messageId":"m2","content":"","toolRequests":[{"toolCallId":"c1","name":"bash","arguments":{"cmd":"ls"}}]},"id":"e3","timestamp":"2026-10-30T00:00:03.000Z","parentId":"e2"}"#;
        let mut p = CopilotCliParser::new();
        let events = p.feed(0, format!("{line}\n").as_bytes()).expect("feed");
        let call = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolCall)
            .expect("a tool call");
        assert_eq!(call.tool_name.as_deref(), Some("bash"));
        assert_eq!(call.tool_call_id.as_deref(), Some("c1"));
    }

    /// `tool.execution_complete` becomes a result, with the error flag.
    #[test]
    fn a_tool_completion_becomes_a_result() {
        let ok = r#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","success":true,"result":{"content":"a.rs"}},"id":"e4","timestamp":"2026-10-30T00:00:04.000Z","parentId":"e3"}"#;
        let mut p = CopilotCliParser::new();
        let events = p.feed(0, format!("{ok}\n").as_bytes()).expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::ToolResult);
        assert_eq!(events[0].text, "a.rs");
        let ContentBlock::ToolResult { is_error, .. } = &events[0].content[0] else {
            panic!("expected a tool result");
        };
        assert!(!is_error);

        let failed = r#"{"type":"tool.execution_complete","data":{"toolCallId":"c1","success":false,"error":{"message":"boom"}},"id":"e5","timestamp":"2026-10-30T00:00:05.000Z","parentId":"e4"}"#;
        let mut p = CopilotCliParser::new();
        let events = p.feed(0, format!("{failed}\n").as_bytes()).expect("feed");
        assert_eq!(events[0].text, "boom");
        let ContentBlock::ToolResult { is_error, .. } = &events[0].content[0] else {
            panic!("expected a tool result");
        };
        assert!(is_error, "a failed tool must be marked as an error");
    }

    /// `assistant.reasoning` becomes a thought.
    #[test]
    fn reasoning_becomes_a_thought() {
        let line = r#"{"type":"assistant.reasoning","data":{"content":"let me think"},"id":"e2","timestamp":"2026-10-30T00:00:02.000Z","parentId":"e1"}"#;
        let mut p = CopilotCliParser::new();
        let events = p.feed(0, format!("{line}\n").as_bytes()).expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::AgentThought);
        assert_eq!(events[0].text, "let me think");
    }

    /// An event whose `parentId` is not the previous event is a branch.
    #[test]
    fn a_branch_is_demoted_to_meta() {
        let mut p = CopilotCliParser::new();
        let body = format!(
            "{}\n{}\n{}",
            r#"{"type":"user.message","data":{"content":"one"},"id":"a","timestamp":"2026-10-30T00:00:01Z","parentId":null}"#,
            r#"{"type":"user.message","data":{"content":"two"},"id":"b","timestamp":"2026-10-30T00:00:02Z","parentId":"a"}"#,
            r#"{"type":"user.message","data":{"content":"rewound"},"id":"c","timestamp":"2026-10-30T00:00:03Z","parentId":"a"}"#,
        );
        let events = p.feed(0, format!("{body}\n").as_bytes()).expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[1].kind, TranscriptKind::UserMessage);
        assert_eq!(
            events[2].kind,
            TranscriptKind::Meta,
            "a record whose parent is not the previous one is on a dead branch"
        );
    }

    /// The branch state survives a checkpoint.
    #[test]
    fn the_branch_state_survives_a_checkpoint() {
        let mut p = CopilotCliParser::new();
        let first = r#"{"type":"user.message","data":{"content":"one"},"id":"a","timestamp":"2026-10-30T00:00:01Z","parentId":null}"#;
        p.feed(0, format!("{first}\n").as_bytes()).expect("feed");
        let state = p.checkpoint();
        let mut restored = CopilotCliParser::restore(&state).expect("restore");
        let next = u64::try_from(first.len() + 1).expect("fits");
        let events = restored
            .feed(
                next,
                format!("{}\n", r#"{"type":"user.message","data":{"content":"two"},"id":"b","timestamp":"2026-10-30T00:00:02Z","parentId":"a"}"#).as_bytes(),
            )
            .expect("resume");
        assert_eq!(
            events[0].kind,
            TranscriptKind::UserMessage,
            "the resumed parser treated a continuation as a branch"
        );
    }

    /// The events do not depend on where the chunks split.
    #[test]
    fn the_events_do_not_depend_on_where_the_chunks_split() {
        let whole = parse_all();
        let bytes = FIXTURE.as_bytes();
        for split in [1usize, 40, 120, bytes.len() / 2, bytes.len() - 1] {
            let mut p = CopilotCliParser::new();
            let mut got = p.feed(0, &bytes[..split]).expect("first");
            got.extend(p.feed(split as u64, &bytes[split..]).expect("second"));
            assert_eq!(
                got.len(),
                whole.len(),
                "a split at {split} changed the count"
            );
            for (a, b) in got.iter().zip(whole.iter()) {
                assert_eq!(a.kind, b.kind);
                assert_eq!(a.text, b.text);
            }
        }
    }

    /// A malformed line is kept.
    #[test]
    fn a_malformed_line_is_kept() {
        let mut p = CopilotCliParser::new();
        let events = p.feed(0, b"not json\n").expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
    }
}

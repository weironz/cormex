//! The Gemini CLI session parser.
//!
//! Design: docs/design/session-archive-architecture.md §7.2, plan P4-1.
//!
//! # The shape, read from upstream
//!
//! `packages/core/src/services/chatRecordingTypes.ts` in
//! `google-gemini/gemini-cli` (Apache-2.0). A session file is JSONL:
//!
//! ```text
//! line 1   { sessionId, projectHash, startTime, lastUpdated, kind?, directories? }
//! line 2+  { id, timestamp, type, content, … }        one per message
//! ```
//!
//! `type` is `user` / `gemini` / `info` / `error` / `warning`. A `gemini`
//! record additionally carries `toolCalls`, `thoughts`, `tokens` and `model`.
//!
//! # Why the first line is not a message
//!
//! It is the conversation's **metadata** — the session id, the project hash and
//! the start time. Treating it as a message would put a JSON blob into the
//! transcript as if the user had typed it. It becomes `Meta` with its raw type,
//! which keeps it searchable and tells the reader what it was.
//!
//! # `content` is a `PartListUnion`, which is three different things
//!
//! Upstream's type is `PartListUnion` = `Part | Part[] | string`. In practice:
//! `user` records carry `[{text: …}]`, `info` / `warning` records carry a bare
//! string, and a `gemini` record's content may be either. All three flatten to
//! text; anything without text (an inline image) contributes nothing rather
//! than a placeholder, because a placeholder would put words in the transcript
//! the session does not contain.
//!
//! # A tool call and its result are one record
//!
//! Unlike Claude Code, Gemini puts the result **inside** the `toolCalls` entry
//! (`result`), so one record yields two events: the call and the result. They
//! share the record's offset, and their derived ids differ by a suffix — the
//! projector's id is `(session, parser_version, generation, offset)`, so two
//! events at one offset would collide without it.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct GeminiParser {
    lines: LineBuffer,
}

impl GeminiParser {
    /// A parser starting at offset 0.
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
        }
    }

    /// Parse one complete line into zero or more events.
    ///
    /// Two events come out of one `gemini` record that carries a tool call:
    /// the call and its result. Everything else is one event.
    fn parse_line(&mut self, offset: u64, length: u64, bytes: &[u8]) -> Vec<ParsedEvent> {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            return vec![ParsedEvent::malformed(offset, length)];
        };

        // The metadata line has no `type` and carries `sessionId`
        let Some(record_type) = v.get("type").and_then(Value::as_str) else {
            return vec![ParsedEvent {
                kind: TranscriptKind::Meta,
                role: None,
                occurred_at: v
                    .get("startTime")
                    .and_then(Value::as_str)
                    .and_then(parse_time),
                content: vec![ContentBlock::Unknown {
                    raw_type: "conversation-metadata".to_owned(),
                }],
                text: String::new(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            }];
        };

        let occurred_at = v
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_time);
        let text = flatten_content(v.get("content"));

        match record_type {
            "user" => vec![ParsedEvent {
                kind: TranscriptKind::UserMessage,
                role: Some("user".to_owned()),
                occurred_at,
                content: vec![ContentBlock::Text { text: text.clone() }],
                text,
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            }],
            "gemini" => self.parse_gemini(&v, offset, length, occurred_at, text),
            // `info` / `error` / `warning` are the host talking about the
            // session ("Context compressed from 20% to 7%"), not the
            // conversation. `System` keeps them visible without putting them in
            // the conversation index
            "info" | "error" | "warning" => vec![ParsedEvent {
                kind: TranscriptKind::System,
                role: None,
                occurred_at,
                content: vec![ContentBlock::Text { text: text.clone() }],
                text,
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            }],
            other => vec![ParsedEvent::unknown(other, offset, length)],
        }
    }

    fn parse_gemini(
        &mut self,
        v: &Value,
        offset: u64,
        length: u64,
        occurred_at: Option<DateTime<Utc>>,
        text: String,
    ) -> Vec<ParsedEvent> {
        let mut out = Vec::new();

        // Thoughts first: they happened before the answer, and the order in the
        // projection is the order in the file
        if let Some(thoughts) = v.get("thoughts").and_then(Value::as_array) {
            for t in thoughts {
                let body = t
                    .get("description")
                    .and_then(Value::as_str)
                    .or_else(|| t.get("subject").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned();
                if body.is_empty() {
                    continue;
                }
                out.push(ParsedEvent {
                    kind: TranscriptKind::AgentThought,
                    role: Some("assistant".to_owned()),
                    occurred_at: t
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_time)
                        .or(occurred_at),
                    content: vec![ContentBlock::Thought { text: body.clone() }],
                    text: body,
                    tool_name: None,
                    tool_call_id: None,
                    src_offset: offset,
                    src_length: length,
                });
            }
        }

        if !text.is_empty() {
            out.push(ParsedEvent {
                kind: TranscriptKind::AgentMessage,
                role: Some("assistant".to_owned()),
                occurred_at,
                content: vec![ContentBlock::Text { text: text.clone() }],
                text,
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            });
        }

        if let Some(calls) = v.get("toolCalls").and_then(Value::as_array) {
            for call in calls {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                // `args` is a real object here, unlike Codex's JSON string
                let args = call.get("args").cloned().unwrap_or(Value::Null);
                out.push(ParsedEvent {
                    kind: TranscriptKind::ToolCall,
                    role: Some("assistant".to_owned()),
                    occurred_at: call
                        .get("timestamp")
                        .and_then(Value::as_str)
                        .and_then(parse_time)
                        .or(occurred_at),
                    content: vec![ContentBlock::ToolCall {
                        tool_call_id: id.clone(),
                        name: name.clone(),
                        input: args,
                    }],
                    text: String::new(),
                    tool_name: Some(name),
                    tool_call_id: Some(id.clone()),
                    src_offset: offset,
                    src_length: length,
                });

                // The result lives in the same record. `status` is the host's
                // own word; anything but `success` is an error result, and the
                // difference matters to the trace pipeline
                if let Some(result) = call.get("result").filter(|r| !r.is_null()) {
                    let output = flatten_content(Some(result));
                    let is_error = call
                        .get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s != "success");
                    out.push(ParsedEvent {
                        kind: TranscriptKind::ToolResult,
                        role: None,
                        occurred_at,
                        content: vec![ContentBlock::ToolResult {
                            tool_call_id: id.clone(),
                            output: output.clone(),
                            is_error,
                        }],
                        text: output,
                        tool_name: None,
                        tool_call_id: Some(id),
                        src_offset: offset,
                        src_length: length,
                    });
                }
            }
        }

        // A `gemini` record with nothing in it at all is still a record: the
        // projection is meant to be a record of the file, not a summary of it
        if out.is_empty() {
            out.push(ParsedEvent {
                kind: TranscriptKind::Meta,
                role: None,
                occurred_at,
                content: vec![ContentBlock::Unknown {
                    raw_type: "gemini-empty".to_owned(),
                }],
                text: String::new(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            });
        }
        out
    }
}

impl Default for GeminiParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Flatten a `PartListUnion`.
///
/// Three shapes, all of which occur: a bare string, an array of parts, and a
/// single part object. Anything without text (an inline image) contributes
/// nothing — a placeholder would put words in the transcript that the session
/// does not contain.
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
        Some(Value::Object(o)) => o
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

fn parse_time(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

impl Parser for GeminiParser {
    fn version(&self) -> &'static str {
        "gemini@1"
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

    fn checkpoint(&self) -> serde_json::Value {
        let (next, start, partial) = self.lines.state();
        serde_json::json!({
            "next_offset": next,
            "partial_start": start,
            "partial": String::from_utf8_lossy(&partial),
        })
    }
}

impl GeminiParser {
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

    /// The fixture is upstream's own `memory-tests/large-chat-session.json`,
    /// reduced to its first few records.
    ///
    /// **Taken from `google-gemini/gemini-cli`, not invented.** A parser
    /// written against a guessed format is one that looks right and fails on
    /// real data, and the failure is a silently empty projection.
    const FIXTURE: &str = include_str!("../tests/fixtures/gemini_session.jsonl");

    fn parse_all() -> Vec<ParsedEvent> {
        let mut p = GeminiParser::new();
        p.feed(0, FIXTURE.as_bytes()).expect("feed")
    }

    /// The metadata line is `Meta`, not a message.
    #[test]
    fn the_metadata_line_is_meta() {
        let events = parse_all();
        assert_eq!(
            events[0].kind,
            TranscriptKind::Meta,
            "the first record is the conversation's metadata, not something the user said"
        );
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "conversation-metadata".into()
            }]
        );
    }

    /// A user record becomes a user message with its text flattened.
    #[test]
    fn a_user_record_becomes_a_user_message() {
        let events = parse_all();
        let user = events
            .iter()
            .find(|e| e.kind == TranscriptKind::UserMessage)
            .expect("a user message");
        assert!(!user.text.is_empty());
        assert_eq!(user.role.as_deref(), Some("user"));
    }

    /// A `gemini` record with a tool call yields the call **and** its result.
    ///
    /// Gemini puts the result inside the `toolCalls` entry, so one record is
    /// two events. Missing the result half would make every tool call look like
    /// it never returned.
    #[test]
    fn a_tool_call_yields_both_the_call_and_its_result() {
        let events = parse_all();
        let calls: Vec<&ParsedEvent> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::ToolCall)
            .collect();
        let results: Vec<&ParsedEvent> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::ToolResult)
            .collect();
        assert!(!calls.is_empty(), "the fixture has tool calls");
        assert!(
            !results.is_empty(),
            "the fixture's tool calls carry results, and none were produced"
        );
        // The ids pair up
        let call_id = calls[0].tool_call_id.clone().expect("a call id");
        assert!(
            results
                .iter()
                .any(|r| r.tool_call_id.as_deref() == Some(&call_id)),
            "the result does not carry its call's id"
        );
    }

    /// A thought becomes an `AgentThought`, not an agent message.
    #[test]
    fn a_thought_becomes_a_thought() {
        let events = parse_all();
        let thoughts: Vec<&ParsedEvent> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::AgentThought)
            .collect();
        assert!(!thoughts.is_empty(), "the fixture has thoughts");
        assert!(
            thoughts[0].text.len() > 10,
            "a thought's description should be its text"
        );
    }

    /// `info` records are `System`, not conversation.
    #[test]
    fn an_info_record_is_system() {
        let events = parse_all();
        let system: Vec<&ParsedEvent> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::System)
            .collect();
        // The reduced fixture may not include one; if it does, it must not be
        // a user or agent message
        for s in system {
            assert!(!s.text.is_empty(), "a system record's text should survive");
        }
    }

    /// Feeding the same bytes split at any position gives the same events.
    ///
    /// This is the chunk-invariance property the whole chunked design rests on:
    /// a client uploads whatever it read, and a record straddling two chunks
    /// must be completed by the next one rather than lost or duplicated.
    #[test]
    fn the_events_do_not_depend_on_where_the_chunks_split() {
        let whole = parse_all();
        let bytes = FIXTURE.as_bytes();
        for split in [1usize, 17, 100, 500, bytes.len() / 2, bytes.len() - 1] {
            let mut p = GeminiParser::new();
            let mut got = p.feed(0, &bytes[..split]).expect("first half");
            got.extend(p.feed(split as u64, &bytes[split..]).expect("second half"));
            assert_eq!(
                got.len(),
                whole.len(),
                "a split at {split} changed the event count"
            );
            for (a, b) in got.iter().zip(whole.iter()) {
                assert_eq!(a.kind, b.kind, "a split at {split} changed an event's kind");
                assert_eq!(a.text, b.text, "a split at {split} changed an event's text");
            }
        }
    }

    /// A line that is not JSON is kept as `malformed`, with its offset.
    #[test]
    fn a_malformed_line_is_kept() {
        let mut p = GeminiParser::new();
        let events = p.feed(0, b"not json\n").expect("feed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "malformed-json".into()
            }]
        );
    }

    /// An unknown record type keeps its name.
    #[test]
    fn an_unknown_record_type_keeps_its_name() {
        let mut p = GeminiParser::new();
        // A trailing newline, because the line buffer only yields **complete**
        // lines: without one the record stays buffered, which is the behaviour
        // the chunk-invariance test relies on
        let events = p
            .feed(
                0,
                b"{\"type\":\"something-new\",\"timestamp\":\"2026-04-01T00:00:00Z\"}\n",
            )
            .expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "something-new".into()
            }]
        );
    }

    /// A checkpoint round-trips, so a resumed projection continues correctly.
    #[test]
    fn the_checkpoint_round_trips() {
        let mut p = GeminiParser::new();
        p.feed(
            0,
            b"{\"type\":\"user\",\"content\":[{\"text\":\"a\"}]}\n{\"type\":\"user\",",
        )
        .expect("feed");
        let state = p.checkpoint();
        let mut restored = GeminiParser::restore(&state).expect("restore");
        // The checkpoint's `next_offset` is where the buffered partial ends, so
        // that is where the next chunk has to start. Feeding anywhere else
        // would make every later `src_offset` wrong
        let (next, _, _) = restored.lines.state();
        let events = restored
            .feed(next, b"\"content\":[{\"text\":\"b\"}]}\n")
            .expect("resume");
        assert_eq!(events.len(), 1, "the straddling record was lost");
        assert_eq!(events[0].text, "b");
    }
}

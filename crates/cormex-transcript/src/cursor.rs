//! The Cursor CLI transcript parser.
//!
//! Design: plan P4-2.
//!
//! # Where the format came from
//!
//! Cursor's **public docs** do not describe the transcript — the hook reference
//! says only "Path to the main conversation transcript file". But the CLI ships
//! as a bundled JavaScript application, and the serialiser is in it:
//! `agent-transcript/dist/index.js`, functions `U` and `W`.
//!
//! This parser is written against that code, not against a guess. The shape:
//!
//! ```text
//! {"role":"user","message":{"content":[{"type":"text","text":"…"}]}}
//! {"role":"assistant","message":{"content":[
//!     {"type":"text","text":"…"},
//!     {"type":"tool_use","name":"bash","input":{…}}]}}
//! {"type":"turn_ended","status":"success"}          ← only when a turn ended
//! {"type":"metadata","metadata":{"overview":"…"}}   ← only when an overview exists
//! ```
//!
//! Three things the serialiser does that a naive parser gets wrong:
//!
//! 1. **`thinking` is merged into the `text` block.** `U` pushes the message's
//!    `text` and its `thinking` into the same array and joins them with a blank
//!    line, so a message that had both arrives as one `text` block. There is no
//!    way to tell them apart afterwards, and this parser does not pretend to.
//! 2. **`tool_result` is dropped.** `R` records `toolResult` as
//!    `{toolName}` — the name only, no output — and `U` never emits it. So a
//!    tool call's **output is not in this file at all**; it lives in the blob
//!    store the transcript writer reads from.
//! 3. **The first of two consecutive user messages is dropped.** `W` removes it
//!    when the first two messages are both `user`, because Cursor writes its
//!    system prompt as a `user` message and the real prompt right after it.
//!
//! # Why `turn_ended` and `metadata` are `Meta`
//!
//! Neither is something either party said. `turn_ended` carries the turn's
//! outcome and `metadata` an overview; both are useful to have in the record and
//! wrong to put in the conversation index.
//!
//! # Where the file lives
//!
//! `~/.cursor/projects/<sanitised workspace path>/agent-transcripts/<id>.jsonl`
//! — `O2 = "agent-transcripts"` in `utils/dist/workspace-paths.js`, and the
//! workspace segment is the path with every non-alphanumeric run collapsed to
//! `-`.

use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct CursorParser {
    lines: LineBuffer,
}

impl CursorParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
        }
    }

    fn parse_line(&mut self, offset: u64, length: u64, bytes: &[u8]) -> Vec<ParsedEvent> {
        let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
            return vec![ParsedEvent::malformed(offset, length)];
        };

        // The two non-message line kinds
        if let Some(kind) = v.get("type").and_then(Value::as_str) {
            return vec![ParsedEvent {
                kind: TranscriptKind::Meta,
                role: None,
                occurred_at: None,
                content: vec![ContentBlock::Unknown {
                    raw_type: format!("cursor:{kind}"),
                }],
                text: String::new(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            }];
        }

        let role = v.get("role").and_then(Value::as_str).unwrap_or("");
        let Some(blocks) = v
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            return vec![ParsedEvent::unknown("cursor:no-content", offset, length)];
        };

        let mut out = Vec::new();
        for block in blocks {
            match block.get("type").and_then(Value::as_str).unwrap_or("") {
                "text" => {
                    let text = block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if text.trim().is_empty() {
                        continue;
                    }
                    // **`user` and `assistant` are the only roles the serialiser
                    // writes**, so anything else is the host's own bookkeeping
                    let kind = match role {
                        "user" => TranscriptKind::UserMessage,
                        "assistant" => TranscriptKind::AgentMessage,
                        _ => TranscriptKind::System,
                    };
                    out.push(ParsedEvent {
                        kind,
                        role: Some(role.to_owned()),
                        occurred_at: None,
                        content: vec![ContentBlock::Text { text: text.clone() }],
                        text,
                        tool_name: None,
                        tool_call_id: None,
                        src_offset: offset,
                        src_length: length,
                    });
                }
                "tool_use" => {
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    // **No id in this format.** `U` emits `{type, name, input}`
                    // and nothing else, so the call cannot be paired with a
                    // result — which is consistent, because the result is not in
                    // this file either
                    out.push(ParsedEvent {
                        kind: TranscriptKind::ToolCall,
                        role: Some(role.to_owned()),
                        occurred_at: None,
                        content: vec![ContentBlock::ToolCall {
                            tool_call_id: String::new(),
                            name: name.clone(),
                            input: block.get("input").cloned().unwrap_or(Value::Null),
                        }],
                        text: String::new(),
                        tool_name: Some(name),
                        tool_call_id: None,
                        src_offset: offset,
                        src_length: length,
                    });
                }
                other => out.push(ParsedEvent::unknown(
                    &format!("cursor:block:{other}"),
                    offset,
                    length,
                )),
            }
        }
        if out.is_empty() {
            out.push(ParsedEvent::unknown("cursor:empty-message", offset, length));
        }
        out
    }
}

impl Default for CursorParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser for CursorParser {
    fn version(&self) -> &'static str {
        "cursor@2"
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

impl CursorParser {
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

    /// A transcript in the shape `agent-transcript`'s `U`/`W` write.
    ///
    /// Written from the serialiser's code rather than copied from a real
    /// session: a real one carries somebody's conversation, and this repository
    /// is public.
    const FIXTURE: &str = concat!(
        r#"{"role":"user","message":{"content":[{"type":"text","text":"add a retry to the upload loop"}]}}"#,
        "\n",
        r#"{"role":"assistant","message":{"content":[{"type":"text","text":"I will read the loop first.\n\nThen add the retry."},{"type":"tool_use","name":"read_file","input":{"path":"sync/mod.rs"}}]}}"#,
        "\n",
        r#"{"role":"user","message":{"content":[{"type":"text","text":"thanks"}]}}"#,
        "\n",
        r#"{"type":"turn_ended","status":"success"}"#,
        "\n",
    );

    fn parse_all() -> Vec<ParsedEvent> {
        let mut p = CursorParser::new();
        p.feed(0, FIXTURE.as_bytes()).expect("feed")
    }

    /// A user line becomes a user message.
    #[test]
    fn a_user_line_becomes_a_user_message() {
        let events = parse_all();
        let user = events
            .iter()
            .find(|e| e.kind == TranscriptKind::UserMessage)
            .expect("a user message");
        assert_eq!(user.text, "add a retry to the upload loop");
        assert_eq!(user.role.as_deref(), Some("user"));
    }

    /// A `text` block becomes an agent message; `thinking` is already merged in.
    ///
    /// `U` joins a message's `text` and its `thinking` into one block with a
    /// blank line, so the two cannot be told apart afterwards — and this parser
    /// does not pretend to.
    #[test]
    fn the_text_block_carries_the_thinking_too() {
        let events = parse_all();
        let agent = events
            .iter()
            .find(|e| e.kind == TranscriptKind::AgentMessage)
            .expect("an agent message");
        assert!(
            agent.text.contains("I will read the loop first."),
            "{}",
            agent.text
        );
        assert!(
            agent.text.contains("Then add the retry."),
            "the merged thinking/text was split: {}",
            agent.text
        );
    }

    /// A `tool_use` block becomes a tool call, with no id.
    ///
    /// `U` emits `{type, name, input}` and nothing else, so there is no id to
    /// pair with a result — consistent, because the result is not in this file.
    #[test]
    fn a_tool_use_block_becomes_a_call_without_an_id() {
        let events = parse_all();
        let call = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolCall)
            .expect("a tool call");
        assert_eq!(call.tool_name.as_deref(), Some("read_file"));
        assert!(
            call.tool_call_id.is_none(),
            "this format has no tool-call id, so inventing one would make a result \
             look pairable when it is not in the file at all"
        );
    }

    /// `turn_ended` is `Meta`, not a message.
    #[test]
    fn turn_ended_is_meta() {
        let events = parse_all();
        let meta = events
            .iter()
            .find(|e| {
                matches!(
                    e.content.first(),
                    Some(ContentBlock::Unknown { raw_type }) if raw_type == "cursor:turn_ended"
                )
            })
            .expect("the turn_ended line");
        assert_eq!(meta.kind, TranscriptKind::Meta);
        assert!(meta.text.is_empty());
    }

    /// A `metadata` line is `Meta` too.
    #[test]
    fn metadata_is_meta() {
        let mut p = CursorParser::new();
        let events = p
            .feed(
                0,
                b"{\"type\":\"metadata\",\"metadata\":{\"overview\":\"a summary\"}}\n",
            )
            .expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::Meta);
    }

    /// The events do not depend on where the chunks split.
    #[test]
    fn the_events_do_not_depend_on_where_the_chunks_split() {
        let whole = parse_all();
        let bytes = FIXTURE.as_bytes();
        for split in [1usize, 40, 100, bytes.len() / 2, bytes.len() - 1] {
            let mut p = CursorParser::new();
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
        let mut p = CursorParser::new();
        let events = p.feed(0, b"not json\n").expect("feed");
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
    }

    /// The checkpoint round-trips.
    #[test]
    fn the_checkpoint_round_trips() {
        let mut p = CursorParser::new();
        let first =
            b"{\"role\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"a\"}]}}\n";
        p.feed(0, first).expect("feed");
        let state = p.checkpoint();
        let mut restored = CursorParser::restore(&state).expect("restore");
        let events = restored
            .feed(
                u64::try_from(first.len()).expect("fits"),
                b"{\"role\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"b\"}]}}\n",
            )
            .expect("resume");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].text, "b");
    }
}

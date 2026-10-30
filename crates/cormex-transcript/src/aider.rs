//! The Aider chat-history parser (P4-6).
//!
//! Design: plan P4-6. `.aider.chat.history.md` lives **in the project
//! directory**, which D7 forbids capturing by default — the client reads the
//! agent's own homes and nothing else. So this is only ever reached through a
//! source the user added by hand (`cormex sources add`).
//!
//! # The shape
//!
//! Aider writes a Markdown transcript, not JSON:
//!
//! ```markdown
//! # aider chat started at 2026-10-30 09:12:04
//!
//! #### we use RustFS for object storage
//! > Tokens: 1.2k sent, 84 received.
//!
//! Sure — noted.
//! ```
//!
//! The user's turns start with `#### `; everything between one `####` line and
//! the next is the assistant's answer. That is the whole grammar, and it is
//! why this parser is line-based rather than JSON-based.
//!
//! # Why the `####` prefix and not "any heading"
//!
//! Aider uses `#` for its own banner and `####` for the user's prompt. Treating
//! every heading as a turn boundary would split a Markdown answer that happens
//! to contain a heading into two turns.
//!
//! # No timestamps in the body
//!
//! The file's first line carries a start time; the turns do not. `occurred_at`
//! is `None` and the projector falls back to the chunk's `observed_at`, which
//! is honest: inventing a time per turn would make the event-time axis claim
//! more than the file does.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};

use crate::jsonl::LineBuffer;
use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct AiderParser {
    lines: LineBuffer,
    /// Whether the current turn is the user's (`####` seen) or the agent's.
    in_user_turn: bool,
    /// The session's start time, from the banner line.
    started_at: Option<DateTime<Utc>>,
}

impl AiderParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            lines: LineBuffer::new(),
            in_user_turn: false,
            started_at: None,
        }
    }

    fn parse_line(&mut self, offset: u64, length: u64, bytes: &[u8]) -> Option<ParsedEvent> {
        let line = String::from_utf8_lossy(bytes);
        let trimmed = line.trim_end_matches(['\r', '\n']);

        // The banner: `# aider chat started at 2026-10-30 09:12:04`
        if self.started_at.is_none()
            && let Some(rest) = trimmed.strip_prefix("# aider chat started at ")
        {
            // **A naive datetime**, so `NaiveDateTime` rather than
            // `DateTime::parse_from_str`: the latter requires an offset and
            // fails on Aider's banner, which has none
            self.started_at =
                chrono::NaiveDateTime::parse_from_str(rest.trim(), "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .map(|t| t.and_utc());
            return Some(ParsedEvent {
                kind: TranscriptKind::Meta,
                role: None,
                occurred_at: self.started_at,
                content: vec![ContentBlock::Unknown {
                    raw_type: "aider:session-start".to_owned(),
                }],
                text: String::new(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            });
        }

        // A user turn begins
        if let Some(prompt) = trimmed.strip_prefix("#### ") {
            self.in_user_turn = true;
            return Some(text_event(
                TranscriptKind::UserMessage,
                "user",
                prompt.to_owned(),
                self.started_at,
                offset,
                length,
            ));
        }

        // Aider's own accounting line. `Meta` rather than a message: it is not
        // something either party said
        if trimmed.starts_with("> Tokens:") || trimmed.starts_with("> Aider v") {
            return Some(ParsedEvent {
                kind: TranscriptKind::Meta,
                role: None,
                occurred_at: self.started_at,
                content: vec![ContentBlock::Unknown {
                    raw_type: "aider:accounting".to_owned(),
                }],
                text: String::new(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: length,
            });
        }

        // Everything else is answer text — but only once a user turn has
        // started. Before that it is the file's preamble
        if !self.in_user_turn || trimmed.trim().is_empty() {
            return None;
        }
        Some(text_event(
            TranscriptKind::AgentMessage,
            "assistant",
            trimmed.to_owned(),
            self.started_at,
            offset,
            length,
        ))
    }
}

impl Default for AiderParser {
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

impl Parser for AiderParser {
    fn version(&self) -> &'static str {
        "aider@1"
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        let lines = self.lines.feed(offset, bytes).map_err(|e| e.to_string())?;
        Ok(lines
            .into_iter()
            .filter_map(|l| {
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
            // **Both pieces of state**, or a resume puts the rest of an answer
            // into the user's turn (or vice versa)
            "in_user_turn": self.in_user_turn,
            "started_at": self.started_at.map(|t| t.to_rfc3339()),
        })
    }
}

impl AiderParser {
    /// Resume from a checkpoint.
    ///
    /// # Errors
    /// The checkpoint is not one this parser wrote.
    pub fn restore(state: &serde_json::Value) -> Result<Self, String> {
        let next = state
            .get("next_offset")
            .and_then(serde_json::Value::as_u64)
            .ok_or("the checkpoint has no next_offset")?;
        let start = state
            .get("partial_start")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(next);
        let partial = state
            .get("partial")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        Ok(Self {
            lines: LineBuffer::from_state(next, start, partial),
            in_user_turn: state
                .get("in_user_turn")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
            started_at: state
                .get("started_at")
                .and_then(serde_json::Value::as_str)
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.with_timezone(&Utc)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = "\
# aider chat started at 2026-10-30 09:12:04

> Aider v0.60.0
> Model: gpt-4o with diff edit format

#### we use RustFS for object storage
> Tokens: 1.2k sent, 84 received.

Sure — noted.

#### and the health check?
> Tokens: 1.3k sent, 60 received.

It binds to loopback.
";

    fn parse_all() -> Vec<ParsedEvent> {
        let mut p = AiderParser::new();
        p.feed(0, FIXTURE.as_bytes()).expect("feed")
    }

    /// The banner becomes `Meta` and carries the session's start time.
    #[test]
    fn the_banner_is_meta_with_a_time() {
        let events = parse_all();
        assert_eq!(events[0].kind, TranscriptKind::Meta);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "aider:session-start".into()
            }]
        );
        assert!(
            events[0].occurred_at.is_some(),
            "the start time was dropped"
        );
    }

    /// `####` lines are the user's turns.
    #[test]
    fn a_hash_line_is_a_user_turn() {
        let events = parse_all();
        let users: Vec<&str> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::UserMessage)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(
            users,
            vec!["we use RustFS for object storage", "and the health check?"]
        );
    }

    /// The text after a `####` line is the agent's answer.
    #[test]
    fn the_text_after_a_prompt_is_the_answer() {
        let events = parse_all();
        let answers: Vec<&str> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::AgentMessage)
            .map(|e| e.text.as_str())
            .collect();
        assert!(answers.contains(&"Sure — noted."), "{answers:?}");
        assert!(answers.contains(&"It binds to loopback."), "{answers:?}");
    }

    /// Aider's accounting lines are `Meta`, not messages.
    #[test]
    fn the_accounting_lines_are_meta() {
        let events = parse_all();
        assert!(
            !events
                .iter()
                .any(|e| e.text.starts_with("> Tokens:") || e.text.starts_with("> Aider v")),
            "an accounting line became a message"
        );
    }

    /// Text before the first `####` is the preamble, not an answer.
    #[test]
    fn the_preamble_is_not_an_answer() {
        let events = parse_all();
        assert!(
            !events
                .iter()
                .any(|e| e.kind == TranscriptKind::AgentMessage && e.text.contains("Model:")),
            "the preamble became an answer"
        );
    }

    /// The checkpoint carries both pieces of state.
    ///
    /// Without `in_user_turn`, a resume puts the rest of an answer into the
    /// user's turn — and `started_at` would be lost, so every later event would
    /// have no time.
    #[test]
    fn the_checkpoint_carries_both_pieces_of_state() {
        let mut p = AiderParser::new();
        let first = "# aider chat started at 2026-10-30 09:12:04\n#### hello\n";
        p.feed(0, first.as_bytes()).expect("feed");
        let state = p.checkpoint();
        let restored = AiderParser::restore(&state).expect("restore");
        assert!(restored.in_user_turn);
        assert!(restored.started_at.is_some());

        let mut restored = restored;
        let events = restored
            .feed(u64::try_from(first.len()).expect("fits"), b"the answer\n")
            .expect("resume");
        assert_eq!(
            events[0].kind,
            TranscriptKind::AgentMessage,
            "the resumed parser lost which turn it was in"
        );
    }

    /// A malformed checkpoint is refused.
    #[test]
    fn a_malformed_checkpoint_is_refused() {
        assert!(AiderParser::restore(&serde_json::json!({})).is_err());
    }
}

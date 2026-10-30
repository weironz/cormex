//! The Cursor transcript parser — **a placeholder that says so**.
//!
//! Design: plan P4-2.
//!
//! # Why this exists at all when the format is unknown
//!
//! Cursor's hooks hand us `transcript_path`, and the file behind it is the
//! conversation. **Upstream does not document its format**: the hook reference
//! says only "Path to the main conversation transcript file", and nothing in
//! Cursor's public material describes the bytes.
//!
//! The choice is between two bad options and one good one:
//!
//! | | Consequence |
//! |---|---|
//! | Do not capture | The bytes are lost. Cursor deletes its own history, and unlike a parser bug, **a missing file cannot be fixed retroactively** |
//! | Capture, pretend to parse | A parser written against a guessed format produces wrong events, and wrong is harder to notice than absent |
//! | **Capture, say plainly that the format is unknown** | The bytes are safe in the R layer; the projection says why it is empty; when a parser lands, re-projection works on the stored bytes |
//!
//! This is the third. It emits **one** event per file, of kind
//! [`TranscriptKind::Unknown`], whose `raw_type` names the situation. That
//! makes the session visible in the archive with an honest label rather than
//! either vanishing or showing fabricated turns.
//!
//! # What has to happen for this to be replaced
//!
//! Someone runs `cormex hook cursor` on a machine with Cursor and looks at the
//! `transcript_path` in its stdin. One real file is enough: the R layer already
//! holds every byte, so the replacement parser can be tested against the same
//! file and re-projection fills in the events.

use cormex_proto::transcript::{ContentBlock, TranscriptKind};

use crate::parser::{ParsedEvent, Parser};

/// The parser.
///
/// Stateless: it emits one event for the whole file on the first feed and
/// nothing afterwards, so a re-run of the projector produces the same result.
pub struct CursorParser {
    emitted: bool,
}

impl CursorParser {
    #[must_use]
    pub fn new() -> Self {
        Self { emitted: false }
    }
}

impl Default for CursorParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser for CursorParser {
    fn version(&self) -> &'static str {
        // **The version names the situation, not a parser revision.** When a
        // real parser lands it becomes `cursor@2`, and the projector then
        // rewrites the projection from the stored bytes — which is exactly what
        // `parser_version` exists for
        "cursor@1-undocumented"
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        if self.emitted {
            return Ok(Vec::new());
        }
        self.emitted = true;
        Ok(vec![ParsedEvent {
            kind: TranscriptKind::Unknown,
            role: None,
            occurred_at: None,
            content: vec![ContentBlock::Unknown {
                raw_type: "cursor:format-undocumented".to_owned(),
            }],
            // No text: the file's bytes are not the conversation's text, and
            // putting them here would put a JSON blob into the conversation
            // index where a search could return it as if somebody had said it
            text: String::new(),
            tool_name: None,
            tool_call_id: None,
            src_offset: offset,
            src_length: u64::try_from(bytes.len()).unwrap_or(0),
        }])
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::json!({ "emitted": self.emitted })
    }
}

impl CursorParser {
    /// Resume from a checkpoint.
    ///
    /// # Errors
    /// The checkpoint is not one this parser wrote.
    pub fn restore(state: &serde_json::Value) -> Result<Self, String> {
        Ok(Self {
            emitted: state
                .get("emitted")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One event, naming the situation, with no text.
    ///
    /// The absence of text is deliberate: the file's bytes are not the
    /// conversation's text, and putting them in `text` would put a JSON blob
    /// into the conversation index where a search could return it as if
    /// somebody had said it.
    #[test]
    fn it_emits_one_event_that_says_the_format_is_unknown() {
        let mut p = CursorParser::new();
        let events = p.feed(0, br#"{"whatever":"cursor writes"}"#).expect("feed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "cursor:format-undocumented".into()
            }]
        );
        assert!(
            events[0].text.is_empty(),
            "the raw bytes must not become conversation text"
        );
    }

    /// The version names the situation.
    ///
    /// When a real parser lands it becomes `cursor@2`, and the projector
    /// rewrites the projection from the stored bytes — which is what
    /// `parser_version` exists for.
    #[test]
    fn the_version_says_the_format_is_undocumented() {
        assert_eq!(CursorParser::new().version(), "cursor@1-undocumented");
    }

    /// Feeding twice does not duplicate.
    #[test]
    fn feeding_twice_does_not_duplicate() {
        let mut p = CursorParser::new();
        assert!(!p.feed(0, b"anything").expect("first").is_empty());
        assert!(p.feed(0, b"anything").expect("second").is_empty());
    }

    /// A checkpoint round-trips.
    #[test]
    fn the_checkpoint_round_trips() {
        let mut p = CursorParser::new();
        p.feed(0, b"anything").expect("feed");
        let state = p.checkpoint();
        let mut restored = CursorParser::restore(&state).expect("restore");
        assert!(restored.feed(0, b"anything").expect("again").is_empty());
    }
}

//! The Copilot CLI transcript parser — **a placeholder that says so**.
//!
//! Design: plan P4-6.
//!
//! # Why the format is still unknown
//!
//! `github/copilot-cli` is a **closed-source repository** — its GitHub repo
//! holds a README, a changelog and an install script, no code. The CLI is
//! distributed as a native binary (`@github/copilot-linux-x64`, 198 MB,
//! stripped), and unlike Cursor's — which ships as bundled JavaScript whose
//! serialiser can be read — its strings carry no transcript schema.
//!
//! Installing it and running a conversation was tried: it refuses to start
//! without a **fine-grained** GitHub PAT or OAuth (`ghp_` classic tokens are
//! rejected by name), so no `events.jsonl` was produced to inspect.
//!
//! # What this does instead
//!
//! **Captures the bytes and labels the projection honestly.** See
//! [`crate::cursor`]'s module docs for the reasoning: a file never captured is
//! gone for good, because the host cleans up its own session state, while a
//! parser bug is fixable by re-projection.
//!
//! The version is `copilot_cli@1-undocumented`. When a real parser lands it
//! becomes `copilot_cli@2`, and the projector rewrites the projection from the
//! stored bytes.

use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::parser::{ParsedEvent, Parser};

/// The parser.
///
/// Stateless beyond `emitted`: it emits one event for the whole file on the
/// first feed and nothing afterwards, so a re-run of the projector produces the
/// same result.
pub struct CopilotCliParser {
    emitted: bool,
}

impl CopilotCliParser {
    #[must_use]
    pub fn new() -> Self {
        Self { emitted: false }
    }
}

impl Default for CopilotCliParser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser for CopilotCliParser {
    fn version(&self) -> &'static str {
        "copilot_cli@1-undocumented"
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
                raw_type: "copilot_cli:format-undocumented".to_owned(),
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

    fn checkpoint(&self) -> Value {
        serde_json::json!({ "emitted": self.emitted })
    }
}

impl CopilotCliParser {
    /// Resume from a checkpoint.
    ///
    /// # Errors
    /// The checkpoint is not one this parser wrote.
    pub fn restore(state: &Value) -> Result<Self, String> {
        Ok(Self {
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

    /// One event, naming the situation, with no text.
    #[test]
    fn it_emits_one_event_that_says_the_format_is_unknown() {
        let mut p = CopilotCliParser::new();
        let events = p
            .feed(0, br#"{"whatever":"copilot writes"}"#)
            .expect("feed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        assert_eq!(
            events[0].content,
            vec![ContentBlock::Unknown {
                raw_type: "copilot_cli:format-undocumented".into()
            }]
        );
        assert!(
            events[0].text.is_empty(),
            "the raw bytes must not become conversation text"
        );
    }

    /// The version says the format is undocumented.
    #[test]
    fn the_version_says_the_format_is_undocumented() {
        assert_eq!(
            CopilotCliParser::new().version(),
            "copilot_cli@1-undocumented"
        );
    }

    /// Feeding twice does not duplicate.
    #[test]
    fn feeding_twice_does_not_duplicate() {
        let mut p = CopilotCliParser::new();
        assert!(!p.feed(0, b"anything").expect("first").is_empty());
        assert!(p.feed(0, b"anything").expect("second").is_empty());
    }

    /// A checkpoint round-trips.
    #[test]
    fn the_checkpoint_round_trips() {
        let mut p = CopilotCliParser::new();
        p.feed(0, b"anything").expect("feed");
        let state = p.checkpoint();
        let mut restored = CopilotCliParser::restore(&state).expect("restore");
        assert!(restored.feed(0, b"anything").expect("again").is_empty());
    }
}

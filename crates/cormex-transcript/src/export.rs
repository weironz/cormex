//! The ChatGPT / Claude export parser (P4-7).
//!
//! Design: plan P4-7. `cormex import <export>` already reads these files; this
//! makes the same parse reachable from the archive's projector, so an export
//! lands in the R layer and is browsable like any other session.
//!
//! # Why the parse is reused rather than reimplemented
//!
//! `cormex-import` already knows this format's two shapes (`mapping` for
//! ChatGPT, `chat_messages` for Claude), including the drift handling that
//! names the keys it actually saw when the shape changes. A second parser here
//! would be a second thing to update when the vendors change their exports, and
//! the one that is not being tested is the one that rots.
//!
//! # Why one file is many sessions
//!
//! An export's `conversations.json` holds **every conversation** the account
//! ever had. The archive's model is one logical file per session, so a single
//! export file cannot be one `capture_file` — the projector would put a hundred
//! conversations into one session.
//!
//! So the client splits the export **before** uploading: one logical file per
//! conversation, each a one-element JSON array. That is what
//! [`split_export`] does, and it is why this parser receives one conversation
//! per file.
//!
//! # Timestamps
//!
//! Both vendors carry them, and they are the event-time axis. `cormex-import`
//! already normalises them; this parser passes them through rather than
//! inventing anything.

use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::Value;

use crate::parser::{ParsedEvent, Parser};

/// Which vendor's export this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportPlatform {
    ChatGpt,
    Claude,
}

impl ExportPlatform {
    /// The `agent` id this platform's sessions are captured under.
    #[must_use]
    pub const fn agent(self) -> &'static str {
        match self {
            Self::ChatGpt => "chatgpt_export",
            Self::Claude => "claude_export",
        }
    }
}

/// A parser over one conversation's JSON.
///
/// The bytes are a **one-element array** — the shape [`split_export`] produces,
/// and the same shape `cormex-import::parse` expects. Keeping that shape means
/// the same function reads both, with no second entry point to keep in step.
pub struct ExportParser {
    platform: ExportPlatform,
    emitted: bool,
}

impl ExportParser {
    #[must_use]
    pub fn new(platform: ExportPlatform) -> Self {
        Self {
            platform,
            emitted: false,
        }
    }
}

impl Parser for ExportParser {
    fn version(&self) -> &'static str {
        match self.platform {
            ExportPlatform::ChatGpt => "chatgpt_export@1",
            ExportPlatform::Claude => "claude_export@1",
        }
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        if self.emitted {
            return Ok(Vec::new());
        }
        let len = u64::try_from(bytes.len()).unwrap_or(0);

        // `cormex-import`'s platform detection is used rather than a second
        // sniff: the export format is what drifts, and its error names the keys
        // it saw
        let platform = match self.platform {
            ExportPlatform::ChatGpt => cormex_import::Platform::ChatGpt,
            ExportPlatform::Claude => cormex_import::Platform::Claude,
        };
        let parsed = match cormex_import::parse(bytes, Some(platform)) {
            Ok((_, conversations)) => conversations,
            Err(e) => {
                // **One malformed event, with its offset.** Returning `Err`
                // would record the whole session as `failed`, which reads as
                // "the parser is broken" rather than "this file is not an
                // export"
                return Ok(vec![ParsedEvent {
                    kind: TranscriptKind::Unknown,
                    role: None,
                    occurred_at: None,
                    content: vec![ContentBlock::Unknown {
                        raw_type: format!("export-unparseable: {e}"),
                    }],
                    text: String::new(),
                    tool_name: None,
                    tool_call_id: None,
                    src_offset: offset,
                    src_length: len,
                }]);
            }
        };
        self.emitted = true;

        let Some(conversation) = parsed.into_iter().next() else {
            return Ok(vec![ParsedEvent::malformed(offset, len)]);
        };

        let mut out = Vec::with_capacity(conversation.messages.len());
        for m in &conversation.messages {
            let kind = match m.role {
                cormex_import::Role::User => TranscriptKind::UserMessage,
                cormex_import::Role::Assistant => TranscriptKind::AgentMessage,
            };
            out.push(ParsedEvent {
                kind,
                role: Some(m.role.wire().to_owned()),
                occurred_at: Some(m.at),
                content: vec![ContentBlock::Text {
                    text: m.text.clone(),
                }],
                text: m.text.clone(),
                tool_name: None,
                tool_call_id: None,
                src_offset: offset,
                src_length: len,
            });
        }
        Ok(out)
    }

    fn checkpoint(&self) -> serde_json::Value {
        serde_json::json!({ "emitted": self.emitted })
    }
}

impl ExportParser {
    /// Resume from a checkpoint.
    ///
    /// # Errors
    /// The checkpoint is not one this parser wrote.
    pub fn restore(platform: ExportPlatform, state: &Value) -> Result<Self, String> {
        Ok(Self {
            platform,
            emitted: state
                .get("emitted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// One conversation out of an export file, as a standalone one-element array.
#[derive(Debug, Clone)]
pub struct SplitConversation {
    /// The vendor's own conversation id. Stable across runs, so it is the
    /// session id.
    pub session_id: String,
    /// The conversation's title, for the session list.
    pub title: String,
    /// A one-element JSON array holding this conversation.
    pub bytes: Vec<u8>,
}

/// Split an export's `conversations.json` into one file per conversation.
///
/// # Errors
/// The file is not an export, or its shape is not recognised. The error names
/// the keys it saw — format drift is this format's most likely failure mode,
/// and "parsed 0 conversations and reported success" would leave someone
/// believing their history is empty.
pub fn split_export(
    bytes: &[u8],
    platform: Option<ExportPlatform>,
) -> anyhow::Result<Vec<SplitConversation>> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| anyhow::anyhow!("this file is not valid JSON: {e}"))?;
    let items = root.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "the top level of an export has to be an array, but this one is: {}",
            match &root {
                Value::Object(_) => "an object",
                Value::String(_) => "a string",
                Value::Number(_) => "a number",
                Value::Bool(_) => "a boolean",
                Value::Null => "null",
                Value::Array(_) => "an array",
            }
        )
    })?;

    let platform = match platform {
        Some(p) => p,
        None => {
            let first = items
                .first()
                .ok_or_else(|| anyhow::anyhow!("the export contains no conversations at all"))?;
            if first.get("mapping").is_some() {
                ExportPlatform::ChatGpt
            } else if first.get("chat_messages").is_some() {
                ExportPlatform::Claude
            } else {
                return Err(anyhow::anyhow!(
                    "cannot tell which vendor this export came from: the first record has \
                     neither `mapping` (ChatGPT) nor `chat_messages` (Claude). The keys it does \
                     have: {}",
                    cormex_import::keys_of(first)
                ));
            }
        }
    };

    // The vendor's id field differs; both are `id` in practice, and the fallback
    // is a positional name so a single missing id does not lose the conversation
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}-{i}", platform.agent()));
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        // **A one-element array**, so `cormex-import::parse` reads it with no
        // second entry point
        let bytes = serde_json::to_vec(&Value::Array(vec![item.clone()]))
            .map_err(|e| anyhow::anyhow!("could not re-serialize a conversation: {e}"))?;
        out.push(SplitConversation {
            session_id: id,
            title,
            bytes,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ChatGPT export record: `mapping` is what identifies it.
    fn chatgpt_export() -> Value {
        serde_json::json!([{
            "id": "conv-1",
            "title": "Storage decision",
            "create_time": 1_760_000_000.0,
            "mapping": {
                "a": { "id": "a", "message": {
                    "id": "m1", "author": {"role": "user"},
                    "content": {"content_type": "text", "parts": ["we use RustFS"]},
                    "create_time": 1_760_000_000.0
                }},
                "b": { "id": "b", "parent": "a", "message": {
                    "id": "m2", "author": {"role": "assistant"},
                    "content": {"content_type": "text", "parts": ["noted"]},
                    "create_time": 1_760_000_010.0
                }}
            }
        }])
    }

    /// A minimal Claude export record: `chat_messages` is what identifies it.
    fn claude_export() -> Value {
        serde_json::json!([{
            "uuid": "conv-2",
            "name": "Deploy notes",
            "created_at": "2026-10-30T00:00:00Z",
            "chat_messages": [
                { "uuid": "m1", "sender": "human",
                  "created_at": "2026-10-30T00:00:00Z",
                  "text": "the health check times out" },
                { "uuid": "m2", "sender": "assistant",
                  "created_at": "2026-10-30T00:00:10Z",
                  "text": "it binds to loopback" }
            ]
        }])
    }

    fn parse(p: ExportPlatform, v: &Value) -> Vec<ParsedEvent> {
        let mut parser = ExportParser::new(p);
        let bytes = serde_json::to_vec(v).expect("serialize");
        parser.feed(0, &bytes).expect("feed")
    }

    /// A ChatGPT conversation becomes user and assistant events, with times.
    #[test]
    fn a_chatgpt_conversation_becomes_events() {
        let events = parse(ExportPlatform::ChatGpt, &chatgpt_export());
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[0].text, "we use RustFS");
        assert_eq!(events[1].kind, TranscriptKind::AgentMessage);
        assert!(
            events[0].occurred_at.is_some(),
            "the export's timestamp was dropped"
        );
    }

    /// A Claude conversation becomes events too.
    #[test]
    fn a_claude_conversation_becomes_events() {
        let events = parse(ExportPlatform::Claude, &claude_export());
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[1].kind, TranscriptKind::AgentMessage);
    }

    /// Splitting an export yields one file per conversation, each a one-element
    /// array.
    ///
    /// The archive's model is one logical file per session; a whole export in
    /// one file would put every conversation into one session.
    #[test]
    fn splitting_yields_one_file_per_conversation() {
        let two = serde_json::json!([
            chatgpt_export()[0].clone(),
            {
                "id": "conv-9",
                "title": "Another",
                "mapping": {}
            }
        ]);
        let bytes = serde_json::to_vec(&two).expect("serialize");
        let split = split_export(&bytes, None).expect("split");
        assert_eq!(split.len(), 2);
        assert_eq!(split[0].session_id, "conv-1");
        assert_eq!(split[1].session_id, "conv-9");

        // Each is a one-element array that the parser reads
        let one: Value = serde_json::from_slice(&split[0].bytes).expect("JSON");
        assert_eq!(one.as_array().map(Vec::len), Some(1));
        let events = parse(ExportPlatform::ChatGpt, &one);
        assert_eq!(events.len(), 2);
    }

    /// The platform is sniffed from the shape when not given.
    #[test]
    fn the_platform_is_sniffed() {
        let bytes = serde_json::to_vec(&claude_export()).expect("serialize");
        let split = split_export(&bytes, None).expect("split");
        assert_eq!(split.len(), 1);
        // And the sniffed platform parses it
        let one: Value = serde_json::from_slice(&split[0].bytes).expect("JSON");
        assert_eq!(parse(ExportPlatform::Claude, &one).len(), 2);
    }

    /// A file that is not an export is one `Unknown` event naming the keys.
    ///
    /// Returning `Err` would record the whole session as `failed`, which reads
    /// as "the parser is broken" rather than "this file is not an export".
    #[test]
    fn a_non_export_is_one_unknown_event() {
        let mut p = ExportParser::new(ExportPlatform::ChatGpt);
        let events = p.feed(0, br#"[{"something":"else"}]"#).expect("feed");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
        let ContentBlock::Unknown { raw_type } = &events[0].content[0] else {
            panic!("expected an Unknown block");
        };
        assert!(
            raw_type.contains("something"),
            "the error should name the keys it saw: {raw_type}"
        );
    }

    /// Feeding twice does not duplicate.
    #[test]
    fn feeding_twice_does_not_duplicate() {
        let bytes = serde_json::to_vec(&chatgpt_export()).expect("serialize");
        let mut p = ExportParser::new(ExportPlatform::ChatGpt);
        assert!(!p.feed(0, &bytes).expect("first").is_empty());
        assert!(p.feed(0, &bytes).expect("second").is_empty());
    }

    /// An empty export is refused rather than reported as success.
    #[test]
    fn an_empty_export_is_refused() {
        let err = split_export(b"[]", None).expect_err("an empty export must fail");
        assert!(err.to_string().contains("no conversations"), "{err}");
    }
}

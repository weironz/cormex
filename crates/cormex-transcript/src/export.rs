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
    Grok,
    Gemini,
}

impl ExportPlatform {
    /// The `agent` id this platform's sessions are captured under.
    #[must_use]
    pub const fn agent(self) -> &'static str {
        match self {
            Self::ChatGpt => "chatgpt_export",
            Self::Claude => "claude_export",
            Self::Grok => "grok_export",
            Self::Gemini => "gemini_takeout",
        }
    }

    /// The platform an `agent` id was captured under.
    #[must_use]
    pub fn from_agent(agent: &str) -> Option<Self> {
        match agent {
            "chatgpt_export" => Some(Self::ChatGpt),
            "claude_export" => Some(Self::Claude),
            "grok_export" => Some(Self::Grok),
            "gemini_takeout" => Some(Self::Gemini),
            _ => None,
        }
    }

    const fn import(self) -> cormex_import::Platform {
        match self {
            Self::ChatGpt => cormex_import::Platform::ChatGpt,
            Self::Claude => cormex_import::Platform::Claude,
            Self::Grok => cormex_import::Platform::Grok,
            Self::Gemini => cormex_import::Platform::Gemini,
        }
    }

    const fn from_import(p: cormex_import::Platform) -> Self {
        match p {
            cormex_import::Platform::ChatGpt => Self::ChatGpt,
            cormex_import::Platform::Claude => Self::Claude,
            cormex_import::Platform::Grok => Self::Grok,
            cormex_import::Platform::Gemini => Self::Gemini,
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
            ExportPlatform::ChatGpt => "chatgpt_export@2",
            ExportPlatform::Claude => "claude_export@2",
            ExportPlatform::Grok => "grok_export@1",
            ExportPlatform::Gemini => "gemini_takeout@1",
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
        let platform = self.platform.import();
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

/// Split an export into one file per conversation.
///
/// Accepts every shape [`cormex_import::parse`] does: a top-level array
/// (ChatGPT, Claude, Gemini Takeout) or Grok's account object.
///
/// # Ids come from the parser, not from a second rule
///
/// The session id comes from [`cormex_import::identity`], the function the
/// vendor's parser itself uses. This used to read `id` and fall back to the record's
/// **position**; Claude names its id `uuid`, so every Claude conversation got
/// `claude_export-<n>`, and a newer export with one more conversation shifted
/// every one of them onto its neighbour's session.
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
    Ok(split_export_detected(bytes, platform)?.1)
}

/// [`split_export`], also saying which platform the file turned out to be.
///
/// # Errors
/// As [`split_export`].
pub fn split_export_detected(
    bytes: &[u8],
    platform: Option<ExportPlatform>,
) -> anyhow::Result<(ExportPlatform, Vec<SplitConversation>)> {
    let root: Value = serde_json::from_slice(bytes)
        .map_err(|e| anyhow::anyhow!("this file is not valid JSON: {e}"))?;
    let items = cormex_import::export_items(&root)?;
    let first = items
        .first()
        .ok_or_else(|| anyhow::anyhow!("the export contains no conversations at all"))?;
    let platform = match platform {
        Some(p) => p,
        None => ExportPlatform::from_import(cormex_import::sniff_platform(first)?),
    };

    let mut out = Vec::with_capacity(items.len());
    for item in items {
        // **A one-element array**, so `cormex-import::parse` reads it with no
        // second entry point, here and in the projector
        let bytes = serde_json::to_vec(&Value::Array(vec![item.clone()]))
            .map_err(|e| anyhow::anyhow!("could not re-serialize a conversation: {e}"))?;
        let Some((session_id, title)) = cormex_import::identity(platform.import(), item) else {
            // A Gemini activity that is not a prompt with an answer is not a
            // conversation. A record of the other vendors with no id at all
            // cannot be imported idempotently, and is refused rather than given
            // a positional id that would move to another conversation next time
            if platform == ExportPlatform::Gemini {
                continue;
            }
            return Err(anyhow::anyhow!(
                "a conversation in this export has no id; the keys it does have: {}",
                cormex_import::keys_of(item)
            ));
        };
        out.push(SplitConversation {
            session_id,
            title,
            bytes,
        });
    }
    Ok((platform, out))
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

    /// Claude names its conversation id `uuid`. The splitter used to read `id`
    /// and fall back to the position in the file, so a later export with one
    /// more conversation moved every conversation onto its neighbour's session.
    #[test]
    fn a_claude_conversation_keeps_its_uuid_whatever_its_position() {
        let one = claude_export();
        let mut two = vec![serde_json::json!({
            "uuid": "conv-new", "name": "Newer", "created_at": "2026-11-01T00:00:00Z",
            "chat_messages": [{"uuid": "n1", "sender": "human",
                               "created_at": "2026-11-01T00:00:00Z", "text": "hi"}]
        })];
        two.extend(one.as_array().expect("array").iter().cloned());

        let first = split_export(&serde_json::to_vec(&one).expect("bytes"), None).expect("split");
        let second = split_export(
            &serde_json::to_vec(&Value::Array(two)).expect("bytes"),
            None,
        )
        .expect("split");
        assert_eq!(first[0].session_id, "conv-2");
        assert_eq!(
            second
                .iter()
                .find(|c| c.title == "Deploy notes")
                .map(|c| c.session_id.as_str()),
            Some("conv-2"),
            "the same conversation is the same session in both exports"
        );
    }

    /// Grok's file is the whole account: an object with `conversations`.
    #[test]
    fn a_grok_account_export_splits_into_conversations() {
        let account = serde_json::json!({
            "conversations": [{
                "conversation": {"id": "g1", "title": "Rust", "create_time": "2026-01-02T03:04:05Z"},
                "responses": [
                    {"response": {"_id": "r1", "message": "what is a lifetime", "sender": "human",
                                  "create_time": "2026-01-02T03:04:05Z"}},
                    {"response": {"_id": "r2", "message": "a scope a reference is valid in",
                                  "sender": "ASSISTANT", "create_time": "2026-01-02T03:04:09Z"}}
                ]
            }],
            "projects": []
        });
        let (platform, split) =
            split_export_detected(&serde_json::to_vec(&account).expect("bytes"), None)
                .expect("split");
        assert_eq!(platform, ExportPlatform::Grok);
        assert_eq!(split[0].session_id, "g1");
        let events = ExportParser::new(ExportPlatform::Grok)
            .feed(0, &split[0].bytes)
            .expect("feed");
        let kinds: Vec<_> = events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![TranscriptKind::UserMessage, TranscriptKind::AgentMessage]
        );
    }

    /// A Takeout activity log: one prompt and answer per session; activities
    /// with no answer are not sessions.
    #[test]
    fn a_gemini_takeout_log_splits_into_prompts() {
        let log = serde_json::json!([
            {"header": "Gemini Apps", "title": "Prompted 什么是幂等", "time": "2026-01-02T03:04:05Z",
             "products": ["Gemini Apps"], "safeHtmlItem": [{"html": "<p>执行多次与一次效果相同。</p>"}]},
            {"header": "Gemini Apps", "title": "Used Gemini Apps", "time": "2026-01-02T03:05:00Z",
             "products": ["Gemini Apps"]}
        ]);
        let (platform, split) =
            split_export_detected(&serde_json::to_vec(&log).expect("bytes"), None).expect("split");
        assert_eq!(platform, ExportPlatform::Gemini);
        assert_eq!(split.len(), 1, "the answerless activity is skipped");
        assert_eq!(split[0].title, "什么是幂等");
        let events = ExportParser::new(ExportPlatform::Gemini)
            .feed(0, &split[0].bytes)
            .expect("feed");
        assert_eq!(events.len(), 2);
    }
}

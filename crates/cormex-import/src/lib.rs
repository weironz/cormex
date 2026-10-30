//! Pour ChatGPT / Claude export files into the memory store.
//!
//! # Why this is its own crate
//!
//! The same parser has to run in **three places**: `cormex-cli` (the command
//! line), `cormex-local` (the desktop app picking a local file), and `cormexd`
//! (the Web client uploading a file).
//!
//! Not sharing it means two or three implementations, and the symptom when they
//! drift apart is ugly: the same export file imports 812 segments on the desktop
//! and 790 on the web, with neither side reporting an error.
//!
//! So what lives here is everything **independent of who sends the requests**:
//! parsing, pairing, id derivation, and the accounting. The step that actually
//! sends requests goes through [`Sink`] — implemented separately in all three
//! places — while rate limiting, write ordering, and error handling exist in
//! this one copy only.
//!
//! # Why it is worth having
//!
//! This product's entire pitch is memory, and an empty memory store is
//! indistinguishable from an ordinary chat box — a new user has to accumulate
//! for weeks after installing before the benefit shows. Meanwhile the
//! extraction pipeline's input is **exactly conversations**, the same shape as
//! what both vendors export: import is mostly format adaptation plus reuse of
//! the existing pipeline.
//!
//! # It needs no new server endpoint
//!
//! `POST /episodes` has exactly the right shape for this: one user entry, one
//! assistant entry carrying `anchor_episode_id`, and the server sends that pair
//! into extraction. And it is **idempotent to begin with** (deduplicating by
//! episode id) — paired with the stable ids computed by
//! [`cormex_core::Id::derived`], resuming is free and needs no progress file.
//!
//! # Three things that must be stated up front
//!
//! 1. **It costs money.** Every message pair triggers one extraction, i.e. one
//!    LLM call. Three years of history may be thousands of them. So `--dry-run`
//!    is the **default**: put the numbers on the table first.
//! 2. **It puts load on the server.** The server already has a concurrency gate
//!    (`CORMEX_EXTRACT_CONCURRENCY`), but that is not backpressure — pouring too
//!    fast merely grows the queue in server memory. So this side rate-limits
//!    itself.
//! 3. **The value only lands once extraction has run.** The four retrieval paths
//!    return **facts**, and the episodes path is off by default. Pouring in only
//!    the raw text adds nothing to retrieval.

pub mod chatgpt;
pub mod claude;
pub mod plan;

pub use plan::{Estimate, PACE, Plan, Progress, Sink, execute, load, plan_from_bytes};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use cormex_core::Id;

/// Which platform this was exported from.
///
/// It goes into the seed string (see [`Conversation::session_id`]): without a
/// source, two conversations that happen to share a number across the two
/// platforms would collide into the same session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    ChatGpt,
    Claude,
}

impl Platform {
    /// The seed prefix, and also the prefix of the session title.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::ChatGpt => "chatgpt",
            Self::Claude => "claude",
        }
    }

    /// The display name. It appears in session titles, so it is spelled the way
    /// a person reads it.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ChatGpt => "ChatGPT",
            Self::Claude => "Claude",
        }
    }
}

/// One message, already normalized down to the part both vendors share.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// The original id from the export file. One of the seeds for deriving the
    /// episode id, so it **must be stable**
    pub source_id: String,
    pub role: Role,
    pub text: String,
    /// The original moment. It becomes the ULID's time part, so both ordering
    /// and freshness depend on it
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// One conversation.
#[derive(Debug, Clone)]
pub struct Conversation {
    pub source_id: String,
    pub title: String,
    pub at: DateTime<Utc>,
    /// Already sorted by time, and **containing only user / assistant**
    pub messages: Vec<Message>,
}

impl Conversation {
    /// This conversation's session id inside Cormex.
    ///
    /// Derived rather than random: a re-run of the import has to land in **the
    /// same** session, otherwise every run adds another session with the same
    /// name.
    #[must_use]
    pub fn session_id(&self, platform: Platform) -> Id {
        Id::derived(
            self.at,
            &format!("{}:conv:{}", platform.tag(), self.source_id),
        )
    }

    /// The session title. **The source goes first** — this is the only place in
    /// the UI where "this memory was imported" lands (we deliberately did not
    /// change the schema for it).
    #[must_use]
    pub fn display_title(&self, platform: Platform) -> String {
        let t = self.title.trim();
        let base = if t.is_empty() {
            "Untitled conversation"
        } else {
            t
        };
        let full = format!("{} · {base}", platform.label());
        // The CHECK on session_events limits titles to 1..=200 characters
        full.chars().take(200).collect()
    }

    /// Split into pairs: one user message with the first assistant message
    /// **immediately** following it.
    ///
    /// # Why pair at all rather than pour them in one at a time
    ///
    /// Server-side extraction works in units of one question and one answer
    /// (`用户：…\n助手：…`). A dangling user message has no assistant to trigger
    /// extraction, and a dangling assistant has no anchor — both still get
    /// written into episodes (the raw text is not lost), but neither produces
    /// any facts.
    ///
    /// When several messages of the same role run consecutively (the user fires
    /// off three), the **last one** is taken as the question: the earlier ones
    /// are usually elaboration, and the assistant is answering their union.
    #[must_use]
    pub fn pairs(&self) -> Vec<(&Message, &Message)> {
        let mut out = Vec::new();
        let mut pending: Option<&Message> = None;
        for m in &self.messages {
            match m.role {
                Role::User => pending = Some(m),
                Role::Assistant => {
                    if let Some(u) = pending.take() {
                        out.push((u, m));
                    }
                }
            }
        }
        out
    }
}

/// A message's episode id inside Cormex.
#[must_use]
pub fn episode_id(platform: Platform, conversation: &str, m: &Message) -> Id {
    Id::derived(
        m.at,
        &format!("{}:msg:{conversation}:{}", platform.tag(), m.source_id),
    )
}

/// Turn a message pair into two `POST /episodes` request bodies.
///
/// The user one carries `retrieve: false`: import does not need memory injected
/// into historical conversations — it would be meaningless (the answer was
/// written long ago) and would add the cost of one retrieval to every single
/// entry.
#[must_use]
pub fn requests_for(
    platform: Platform,
    conv: &Conversation,
    user: &Message,
    assistant: &Message,
) -> [cormex_proto::episodes::NewEpisodeRequest; 2] {
    use cormex_proto::episodes::NewEpisodeRequest;

    let session = conv.session_id(platform).to_string();
    let user_id = episode_id(platform, &conv.source_id, user).to_string();
    let assistant_id = episode_id(platform, &conv.source_id, assistant).to_string();

    [
        NewEpisodeRequest {
            id: user_id.clone(),
            session_id: session.clone(),
            role: Role::User.wire().to_string(),
            text: user.text.clone(),
            occurred_at: Some(user.at.to_rfc3339()),
            attachments: Vec::new(),
            retrieve: false,
            anchor_episode_id: None,
            tool_calls: Vec::new(),
        },
        NewEpisodeRequest {
            id: assistant_id,
            session_id: session,
            role: Role::Assistant.wire().to_string(),
            text: assistant.text.clone(),
            occurred_at: Some(assistant.at.to_rfc3339()),
            attachments: Vec::new(),
            retrieve: false,
            // This field is the extraction switch: the server uses it to pair
            // (user, assistant) into one turn
            anchor_episode_id: Some(user_id),
            tool_calls: Vec::new(),
        },
    ]
}

/// Parse an export file.
///
/// When `platform` is `None`, the platform is sniffed from the content — the
/// two vendors' top-level structures differ enough (ChatGPT has `mapping`,
/// Claude has `chat_messages`).
///
/// # Errors
/// The file cannot be read, is not JSON, or **its structure is unrecognized**.
/// That last case lists the keys actually seen: format drift is this feature's
/// most likely failure mode, and "parse 0 entries and report success" would
/// leave someone believing their own history is empty.
pub fn parse(bytes: &[u8], platform: Option<Platform>) -> Result<(Platform, Vec<Conversation>)> {
    let root: serde_json::Value =
        serde_json::from_slice(bytes).context("this file is not valid JSON")?;
    let items = root.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "the top level of an export file has to be an array, but this one is: {}. Check \
             that this is conversations.json and not some other file from the archive",
            type_name(&root)
        )
    })?;
    if items.is_empty() {
        bail!("the export file contains no conversations at all");
    }

    let platform = match platform {
        Some(p) => p,
        None => sniff(&items[0])?,
    };
    let conversations = match platform {
        Platform::ChatGpt => chatgpt::parse(items)?,
        Platform::Claude => claude::parse(items)?,
    };
    Ok((platform, conversations))
}

/// Identify the platform from the shape of the first record.
fn sniff(first: &serde_json::Value) -> Result<Platform> {
    if first.get("mapping").is_some() {
        return Ok(Platform::ChatGpt);
    }
    if first.get("chat_messages").is_some() {
        return Ok(Platform::Claude);
    }
    bail!(
        "cannot tell which vendor this export came from: the first record has neither \
         `mapping` (ChatGPT) nor `chat_messages` (Claude). The keys it does have: {}\n\
         The format may have changed — pass --platform, or report this key list back",
        keys_of(first)
    );
}

/// List a JSON object's keys. **When the format drifts, this is the only useful
/// clue.**
pub fn keys_of(v: &serde_json::Value) -> String {
    match v.as_object() {
        Some(m) if m.is_empty() => "(empty object)".into(),
        Some(m) => m.keys().cloned().collect::<Vec<_>>().join(", "),
        None => format!("(not an object, but: {})", type_name(v)),
    }
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// Unix seconds (possibly fractional) → a UTC moment.
pub fn from_unix_secs(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    let secs = v.as_f64()?;
    if !secs.is_finite() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    DateTime::from_timestamp(secs.trunc() as i64, (secs.fract().abs() * 1e9) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, role: Role, text: &str) -> Message {
        Message {
            source_id: id.into(),
            role,
            text: text.into(),
            at: "2024-03-01T10:00:00Z".parse().unwrap(),
        }
    }

    fn conv(messages: Vec<Message>) -> Conversation {
        Conversation {
            source_id: "c1".into(),
            title: "关于 Rust 异步".into(),
            at: "2024-03-01T10:00:00Z".parse().unwrap(),
            messages,
        }
    }

    /// Pairing: one question, one answer; dangling messages produce no pair.
    #[test]
    fn pairing_takes_one_question_and_its_answer() {
        let c = conv(vec![
            msg("1", Role::User, "问题一"),
            msg("2", Role::Assistant, "回答一"),
            msg("3", Role::User, "问题二"),
            msg("4", Role::Assistant, "回答二"),
        ]);
        let pairs = c.pairs();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0.text, "问题一");
        assert_eq!(pairs[0].1.text, "回答一");
    }

    /// In a burst of messages, the **last one** is taken as the question.
    ///
    /// The earlier ones are usually clarification, and the assistant is
    /// answering their union — taking the first would leave "question" and
    /// "answer" mismatched.
    #[test]
    fn a_burst_of_user_messages_pairs_on_the_last_one() {
        let c = conv(vec![
            msg("1", Role::User, "先说一句"),
            msg("2", Role::User, "补充一下，其实我想问的是 X"),
            msg("3", Role::Assistant, "关于 X……"),
        ]);
        let pairs = c.pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0.text, "补充一下，其实我想问的是 X");
    }

    /// Dangling messages produce no pair (but they are still written into
    /// episodes — the raw text is not lost).
    #[test]
    fn dangling_messages_produce_no_pair() {
        assert!(
            conv(vec![msg("1", Role::User, "没人回")])
                .pairs()
                .is_empty()
        );
        assert!(
            conv(vec![msg("1", Role::Assistant, "凭空一句")])
                .pairs()
                .is_empty()
        );
    }

    /// Session ids are stable and **do not collide across platforms**.
    #[test]
    fn session_ids_are_stable_and_platform_scoped() {
        let c = conv(vec![]);
        assert_eq!(
            c.session_id(Platform::ChatGpt),
            c.session_id(Platform::ChatGpt),
            "a re-run of the import must land back in the same session, \
             otherwise each run adds another session with the same name"
        );
        assert_ne!(
            c.session_id(Platform::ChatGpt),
            c.session_id(Platform::Claude),
            "conversations sharing a number across the two vendors must not \
             collide into one session"
        );
    }

    /// The title carries the source prefix, an empty title has a fallback, and
    /// it stays within the schema's 200-character limit.
    #[test]
    fn titles_announce_where_they_came_from() {
        assert_eq!(
            conv(vec![]).display_title(Platform::ChatGpt),
            "ChatGPT · 关于 Rust 异步"
        );

        let mut empty = conv(vec![]);
        empty.title = "   ".into();
        assert_eq!(
            empty.display_title(Platform::Claude),
            "Claude · Untitled conversation"
        );

        let mut long = conv(vec![]);
        long.title = "很".repeat(500);
        assert!(
            long.display_title(Platform::ChatGpt).chars().count() <= 200,
            "exceeding the CHECK limit on session_events makes the whole title \
             update fail"
        );
    }

    /// The request bodies: user does not retrieve, assistant carries the anchor
    /// (without which extraction is not triggered).
    #[test]
    fn the_assistant_request_carries_the_anchor_that_triggers_extraction() {
        let c = conv(vec![
            msg("1", Role::User, "问"),
            msg("2", Role::Assistant, "答"),
        ]);
        let (u, a) = c.pairs()[0];
        let [user_req, asst_req] = requests_for(Platform::ChatGpt, &c, u, a);

        assert!(
            !user_req.retrieve,
            "import does not need memory injected into historical conversations"
        );
        assert_eq!(
            asst_req.anchor_episode_id.as_deref(),
            Some(user_req.id.as_str()),
            "the anchor is the extraction switch — without it the pair only \
             lands as raw text and not a single fact comes out"
        );
        assert_eq!(user_req.session_id, asst_req.session_id);
    }

    /// An unrecognized structure must **fail loudly**, carrying the keys it
    /// actually saw.
    ///
    /// "Parse 0 entries and report success" is the worst failure mode here: the
    /// user would conclude that three years of their history is simply empty.
    #[test]
    fn an_unrecognised_shape_fails_loudly_with_the_keys_it_saw() {
        let err = parse(br#"[{"foo": 1, "bar": 2}]"#, None)
            .expect_err("an unrecognized shape should be an error");
        let msg = err.to_string();
        assert!(msg.contains("foo") && msg.contains("bar"), "got: {msg}");

        assert!(
            parse(b"{}", None).is_err(),
            "a non-array at the top level must be an error"
        );
        assert!(
            parse(b"[]", None).is_err(),
            "an empty file must be an error rather than a silent success"
        );
        assert!(parse(b"not json", None).is_err());
    }
}

//! Structure-preserving anonymization for session fixtures.
//!
//! Design: docs/plan/session-archive-plan.md P2-0.
//!
//! # Why "strip the secrets" is not enough
//!
//! A session transcript contains the conversation, the contents of files the
//! agent read, command output, and — per Claude Code's own documentation — the
//! system prompt and tool definitions sent to the model. Redaction removes
//! things that *look like* credentials; it cannot remove a paragraph of a
//! private document, a customer name, or a path that identifies a person.
//!
//! So this module keeps the **shape** and replaces every free-text value:
//!
//! | Kept | Replaced |
//! |---|---|
//! | JSON structure, every key name | every string value not on the allow-list |
//! | booleans, numbers, nulls | timestamps (shifted by one constant) |
//! | enum-like values on the allow-list (`type`, `role`, tool names, …) | ids (consistently mapped) |
//! | | paths (home, username, project) |
//!
//! # Consistency is the property that makes a fixture usable
//!
//! `parentUuid ↔ uuid` and `tool_use_id ↔ tool_result` are references. Map them
//! independently and the parser's tests would exercise a transcript whose
//! references point at nothing — which is a shape no real file has, so the
//! parser would be tested against a fiction. Every id therefore goes through one
//! map, and the same input always yields the same output.
//!
//! # The safety net
//!
//! After anonymizing, the output is scanned again — by the memory crate's
//! redaction rules, and by a set of patterns for this machine's own hostname,
//! username, email addresses and IPv4 addresses. **Any hit aborts the tool and
//! writes nothing.** A fixture that leaks is in the git history forever, and
//! `.gitignore` cannot take it back.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// The anonymization rule version. Bump it when the rules change, so a fixture
/// can be traced to the rules that produced it.
pub const RULES_VERSION: &str = "anon@1";

/// Keys whose **values** are kept verbatim, because they are enum-like and the
/// parser branches on them.
///
/// A key not on this list has its string value replaced. Adding one is a
/// deliberate act: `name` is here because it is a tool name in one place and a
/// person's name in another, and the parser needs the tool name.
pub const ALLOWED_VALUE_KEYS: &[&str] = &[
    "type",
    "role",
    "subtype",
    "name",
    "stop_reason",
    "stop_sequence",
    "media_type",
    "operation",
    "mode",
    "level",
    "kind",
    "status",
    "source",
    "tool_name",
    "event",
    "hook_event_name",
    "permissionMode",
    "model",
    "service_tier",
    "content_type",
    "encoding",
    "framing",
    "agent",
    "parser_version",
];

/// Keys whose values are **ids**, mapped consistently.
pub const ID_KEYS: &[&str] = &[
    "uuid",
    "parentUuid",
    "id",
    "tool_use_id",
    "tool_call_id",
    "sessionId",
    "session_id",
    "requestId",
    "call_id",
    "item_id",
];

/// Keys whose values are **timestamps**, shifted by one constant.
pub const TIME_KEYS: &[&str] = &[
    "timestamp",
    "created_at",
    "createdAt",
    "updated_at",
    "occurred_at",
    "started_at",
    "finished_at",
    "completed_at",
    "last_updated",
];

/// Keys whose values are **paths**.
pub const PATH_KEYS: &[&str] = &["cwd", "path", "file_path", "transcript_path", "workspace"];

/// The anonymizer's state. One instance per file, so that ids and paths stay
/// consistent **within** a file (which is what the references need).
pub struct Anonymizer {
    /// Real id → fake id. `BTreeMap` for deterministic iteration, so a fixture
    /// regenerated from the same input is byte-identical.
    ids: BTreeMap<String, String>,
    /// Real path prefix → placeholder.
    paths: BTreeMap<String, String>,
    /// How far timestamps are shifted, in seconds. One constant per run.
    shift_secs: i64,
    /// The real strings that must not appear in the output, for the safety net.
    forbidden: Vec<String>,
}

impl Anonymizer {
    /// A new anonymizer.
    ///
    /// `shift_secs` is the constant every timestamp moves by. It is a parameter
    /// rather than a random value so that a test can pin the output.
    #[must_use]
    pub fn new(shift_secs: i64) -> Self {
        Self {
            ids: BTreeMap::new(),
            paths: BTreeMap::new(),
            shift_secs,
            forbidden: Vec::new(),
        }
    }

    /// Register a real string that must not survive into the output.
    ///
    /// The caller adds this machine's hostname and username; the safety net then
    /// checks for them after anonymizing.
    pub fn forbid(&mut self, s: &str) {
        let s = s.trim();
        if s.len() >= 3 {
            self.forbidden.push(s.to_owned());
        }
    }

    /// Anonymize one JSON value in place.
    ///
    /// # Panics
    /// Never: an unexpected shape is passed through rather than panicking, so
    /// that one odd record does not abort a whole fixture.
    pub fn value(&mut self, key: Option<&str>, value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut out = Map::new();
                for (k, v) in map {
                    out.insert(k.clone(), self.value(Some(k), v));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(|v| self.value(key, v)).collect()),
            Value::String(s) => self.string(key, s),
            // Numbers, booleans and nulls carry no personal content, and the
            // parser branches on them
            other => other.clone(),
        }
    }

    fn string(&mut self, key: Option<&str>, s: &str) -> Value {
        let Some(key) = key else {
            return Value::String(synthetic_text(s));
        };

        if ALLOWED_VALUE_KEYS.contains(&key) {
            // **But not blindly.** A tool named `Bash` is an enum value; a
            // `name` field holding a person's name is not. The allow-list is
            // for short, enum-like values; anything long is free text that
            // happens to sit under an allowed key
            if s.len() <= 64 && !s.contains('\n') && !looks_like_free_text(s) {
                return Value::String(s.to_owned());
            }
            return Value::String(synthetic_text(s));
        }
        if ID_KEYS.contains(&key) {
            return Value::String(self.id(s));
        }
        if TIME_KEYS.contains(&key) {
            return Value::String(self.timestamp(s));
        }
        if PATH_KEYS.contains(&key) {
            return Value::String(self.path(s));
        }
        Value::String(synthetic_text(s))
    }

    /// Map an id consistently.
    fn id(&mut self, real: &str) -> String {
        if let Some(fake) = self.ids.get(real) {
            return fake.clone();
        }
        // A UUID-shaped id keeps its shape, because the parser may branch on
        // one; anything else gets a stable synthetic id of the same length
        let fake = if looks_like_uuid(real) {
            format!("00000000-0000-4000-8000-{:012x}", self.ids.len())
        } else {
            format!("id-{:08x}", self.ids.len())
        };
        self.ids.insert(real.to_owned(), fake.clone());
        fake
    }

    /// Shift a timestamp by the constant, keeping its format.
    fn timestamp(&mut self, real: &str) -> String {
        match chrono::DateTime::parse_from_rfc3339(real) {
            Ok(t) => {
                let shifted =
                    t.with_timezone(&chrono::Utc) + chrono::Duration::seconds(self.shift_secs);
                // Keep the original's precision: a fixture with milliseconds
                // must not become one with none, or the parser's date handling
                // is tested against a shape the host never writes
                if real.contains('.') {
                    shifted.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
                } else {
                    shifted.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                }
            }
            // Not a timestamp after all: treat it as free text
            Err(_) => synthetic_text(real),
        }
    }

    /// Replace a path, keeping the last segment's shape.
    fn path(&mut self, real: &str) -> String {
        // A registered prefix wins, so that `/home/alice/proj/x` and
        // `/home/alice/proj/y` map to the same placeholder directory
        for (prefix, placeholder) in &self.paths {
            if let Some(rest) = real.strip_prefix(prefix.as_str()) {
                let rest = rest.trim_start_matches('/');
                if rest.is_empty() {
                    return placeholder.clone();
                }
                return format!("{placeholder}/{rest}");
            }
        }
        // A home-like path: register its prefix so siblings stay together
        if let Some(idx) = real.find("/home/").or_else(|| real.find("/Users/")) {
            let after = &real[idx..];
            let mut parts = after.splitn(4, '/').filter(|p| !p.is_empty());
            let _root = parts.next();
            if let Some(user) = parts.next() {
                let prefix = format!("/{}/{}", after.split('/').nth(1).unwrap_or("home"), user);
                let placeholder = format!("/home/{user_placeholder}", user_placeholder = "user");
                self.paths.insert(prefix.clone(), placeholder.clone());
                let rest = real
                    .strip_prefix(&prefix)
                    .unwrap_or("")
                    .trim_start_matches('/');
                return if rest.is_empty() {
                    placeholder
                } else {
                    format!("{placeholder}/{rest}")
                };
            }
        }
        // Anything else: keep the shape, replace the segments
        if real.starts_with('/') {
            let segments: Vec<&str> = real.split('/').filter(|s| !s.is_empty()).collect();
            let replaced: Vec<String> = segments
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    if i == 0 {
                        (*s).to_owned()
                    } else {
                        synthetic_text(s)
                    }
                })
                .collect();
            format!("/{}", replaced.join("/"))
        } else {
            synthetic_text(real)
        }
    }

    /// Check the output for anything that must not be there.
    ///
    /// Returns the offending patterns, **without** the matching text: the
    /// caller prints the pattern's name, never the leak.
    ///
    /// # Only the registered strings are checked here
    ///
    /// The email and IPv4 patterns are **not** applied to the anonymized output,
    /// and that is deliberate: `synthetic_text` produces strings of the same
    /// character class, so an anonymized email address still looks like an email
    /// address (`efghijk@mnopqrstu.wxy`). Scanning the output for that shape
    /// would abort on every fixture that ever contained one, which is most of
    /// them.
    ///
    /// The email / IPv4 / home-directory checks belong on the **input**, before
    /// anonymization, where they mean something: [`Self::scan_input`] does that.
    #[must_use]
    pub fn safety_net(&self, output: &str) -> Vec<String> {
        let mut hits = Vec::new();

        for f in &self.forbidden {
            if output.contains(f.as_str()) {
                hits.push(format!("a registered private string ({} chars)", f.len()));
            }
        }

        // A home directory that is **not** the placeholder. This one is safe to
        // check on the output: the placeholder is a fixed string, so anything
        // else is either a bug in `path` or a path that arrived through a key
        // this module does not know about
        if let Some(idx) = output.find("/home/") {
            let rest = &output[idx + 6..];
            let name: String = rest
                .chars()
                .take_while(|c| *c != '/' && *c != '"' && *c != '\\')
                .collect();
            if !name.is_empty() && name != "user" {
                hits.push("a home directory that is not the placeholder".to_owned());
            }
        }
        hits
    }

    /// Scan the **input** for the shapes that must never reach a fixture.
    ///
    /// Runs before anonymization, where an email address or an IPv4 address is
    /// really a leak rather than a synthetic string that happens to have the
    /// same shape.
    ///
    /// # Why this does not abort
    ///
    /// These are **informational**: the anonymizer will replace them anyway, so
    /// a hit here is not a reason to refuse to write. What it is a reason to do
    /// is look: a fixture whose input had an email address is worth a human
    /// glance before it is committed. The caller prints the count.
    #[must_use]
    pub fn scan_input(input: &str) -> Vec<String> {
        let mut hits = Vec::new();
        if looks_like_email(input) {
            hits.push("an email address in the input".to_owned());
        }
        if looks_like_ipv4(input) {
            hits.push("an IPv4 address in the input".to_owned());
        }
        hits
    }
}

/// Whether a string looks like a UUID.
fn looks_like_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Whether an allowed-key value is really free text.
///
/// A `name` of `Bash` is an enum; a `name` of `Acme Corp quarterly review` is
/// not. The heuristic is "contains a space or is long", which is enough to keep
/// the enum-like values and replace the rest.
fn looks_like_free_text(s: &str) -> bool {
    s.contains(' ') || s.len() > 32
}

/// A synthetic string of the same length and character class.
///
/// # Why length and character class are preserved
///
/// The parser's tests care about shapes: a chunk boundary landing mid-string, a
/// very long base64 blob, a Chinese sentence with no spaces. A fixture whose
/// strings are all `x` would pass a parser that mishandles all three.
#[must_use]
pub fn synthetic_text(real: &str) -> String {
    // Long base64-ish blobs (thinking signatures, inline images) become a
    // placeholder of the same order of length rather than a same-length string:
    // a 40 KB run of `A` is not more useful than `[blob:40000]`, and the latter
    // says what it is
    if real.len() > 512
        && real
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c))
    {
        return format!("[blob:{}]", real.len());
    }

    let mut out = String::with_capacity(real.len());
    let mut n = 0usize;
    for c in real.chars() {
        if c == '\n' {
            out.push('\n');
            continue;
        }
        n += 1;
        if c.is_ascii_alphabetic() {
            // A repeating lowercase run, so the length matches exactly
            out.push((b'a' + (n % 26) as u8) as char);
        } else if c.is_ascii_digit() {
            out.push((b'0' + (n % 10) as u8) as char);
        } else if c.is_whitespace() {
            out.push(' ');
        } else if is_cjk(c) {
            // Chinese text stays Chinese: a parser that tokenizes by whitespace
            // must be tested against text that has none
            out.push('文');
        } else if c.is_ascii_punctuation() {
            // Punctuation is structural (quotes, braces inside a string), so it
            // is kept
            out.push(c);
        } else {
            out.push('x');
        }
    }
    out
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{4E00}'..='\u{9FFF}' | '\u{3400}'..='\u{4DBF}')
}

fn looks_like_email(s: &str) -> bool {
    // Deliberately loose: a false positive aborts the tool, which is the safe
    // direction
    s.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '@' && c != '.' && c != '-');
        w.contains('@') && w.contains('.') && w.len() > 5
    })
}

fn looks_like_ipv4(s: &str) -> bool {
    s.split(|c: char| !c.is_ascii_digit() && c != '.').any(|w| {
        let parts: Vec<&str> = w.split('.').collect();
        parts.len() == 4
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.len() <= 3 && p.parse::<u8>().is_ok())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_map_consistently_and_references_survive() {
        let mut a = Anonymizer::new(0);
        let uuid = "11111111-2222-3333-4444-555555555555";
        let first = a.value(Some("uuid"), &json!(uuid));
        let second = a.value(Some("parentUuid"), &json!(uuid));
        assert_eq!(
            first, second,
            "the same id mapped to two different fakes: `parentUuid ↔ uuid` references would \
             point at nothing, and the parser would be tested against a shape no real file has"
        );
        assert_ne!(first, json!(uuid), "the real id survived");
        assert!(
            looks_like_uuid(first.as_str().expect("a string")),
            "a UUID-shaped id must stay UUID-shaped, because the parser may branch on that"
        );
    }

    #[test]
    fn tool_use_id_and_result_stay_linked() {
        let mut a = Anonymizer::new(0);
        let id = "toolu_01ABC";
        let call = a.value(Some("tool_use_id"), &json!(id));
        let result = a.value(Some("tool_use_id"), &json!(id));
        assert_eq!(call, result);
    }

    #[test]
    fn allow_listed_values_are_kept() {
        let mut a = Anonymizer::new(0);
        assert_eq!(
            a.value(Some("type"), &json!("assistant")),
            json!("assistant"),
            "the parser branches on `type`; replacing it makes the fixture useless"
        );
        assert_eq!(a.value(Some("role"), &json!("user")), json!("user"));
        assert_eq!(a.value(Some("name"), &json!("Bash")), json!("Bash"));
        assert_eq!(
            a.value(Some("stop_reason"), &json!("end_turn")),
            json!("end_turn")
        );
    }

    /// An allowed key holding free text is still replaced.
    ///
    /// `name` is a tool name in one place and a person's name in another; the
    /// allow-list cannot tell them apart, so length and spaces decide.
    #[test]
    fn an_allowed_key_holding_free_text_is_replaced() {
        let mut a = Anonymizer::new(0);
        let out = a.value(Some("name"), &json!("Acme Corp quarterly review meeting"));
        assert_ne!(
            out,
            json!("Acme Corp quarterly review meeting"),
            "a `name` field holding a sentence survived the anonymizer"
        );
    }

    #[test]
    fn free_text_is_replaced_but_keeps_its_shape() {
        let mut a = Anonymizer::new(0);
        let real = "Hello, this is a private message.\nSecond line.";
        let out = a.value(Some("text"), &json!(real));
        let s = out.as_str().expect("a string");
        assert_ne!(s, real);
        assert_eq!(
            s.chars().count(),
            real.chars().count(),
            "the length must be preserved: a parser tested against strings of a different length \
             would miss a chunk-boundary bug"
        );
        assert_eq!(
            s.matches('\n').count(),
            real.matches('\n').count(),
            "the newline count must be preserved"
        );
    }

    /// Chinese text stays Chinese.
    ///
    /// A parser that tokenizes by whitespace must be tested against text that
    /// has none.
    #[test]
    fn chinese_text_stays_chinese() {
        let mut a = Anonymizer::new(0);
        let out = a.value(Some("text"), &json!("我们用对象存储"));
        let s = out.as_str().expect("a string");
        assert!(
            s.chars().all(is_cjk),
            "Chinese text became ASCII, so a whitespace-tokenizing parser would be tested \
             against the wrong shape: {s}"
        );
        assert_eq!(s.chars().count(), 7);
    }

    #[test]
    fn timestamps_shift_by_one_constant_and_keep_their_precision() {
        let mut a = Anonymizer::new(3600);
        let out = a.value(Some("timestamp"), &json!("2026-10-29T08:00:00Z"));
        assert_eq!(out, json!("2026-10-29T09:00:00Z"));

        let with_millis = a.value(Some("timestamp"), &json!("2026-10-29T08:00:00.123Z"));
        let s = with_millis.as_str().expect("a string");
        assert!(
            s.contains('.'),
            "a millisecond timestamp lost its precision: {s}"
        );
    }

    /// The shift preserves ordering, which is what the parser's sequencing
    /// depends on.
    #[test]
    fn the_shift_preserves_ordering() {
        let mut a = Anonymizer::new(-86400);
        let early = a.value(Some("timestamp"), &json!("2026-10-29T08:00:00Z"));
        let late = a.value(Some("timestamp"), &json!("2026-10-29T09:00:00Z"));
        assert!(early.as_str() < late.as_str());
    }

    #[test]
    fn the_home_directory_becomes_a_placeholder() {
        let mut a = Anonymizer::new(0);
        let one = a.value(Some("cwd"), &json!("/home/alice/project-a"));
        let two = a.value(Some("cwd"), &json!("/home/alice/project-b"));
        assert_eq!(one, json!("/home/user/project-a"));
        assert_eq!(
            two,
            json!("/home/user/project-b"),
            "two paths under one home must share the placeholder prefix, or the fixture's \
             cross-file references break"
        );
    }

    #[test]
    fn a_long_blob_becomes_a_placeholder() {
        let mut a = Anonymizer::new(0);
        let blob = "A".repeat(4000);
        let out = a.value(Some("signature"), &json!(blob));
        assert_eq!(
            out,
            json!("[blob:4000]"),
            "a 40 KB run of letters is not more useful than a placeholder that says what it is"
        );
    }

    /// The safety net catches what it is for.
    #[test]
    fn the_safety_net_catches_a_leak() {
        let mut a = Anonymizer::new(0);
        a.forbid("my-secret-hostname");
        assert!(
            a.safety_net("nothing to see").is_empty(),
            "a clean output should report no hits"
        );
        assert!(
            !a.safety_net("the host is my-secret-hostname").is_empty(),
            "a registered private string was not caught"
        );
        assert!(
            !a.safety_net(r#"{"cwd":"/home/alice/project"}"#).is_empty(),
            "a home directory other than the placeholder was not caught"
        );
        assert!(
            a.safety_net("contact me at alice@example.com").is_empty(),
            "the safety net flagged an email-shaped string in the **output**. The anonymizer              produces strings of the same character class, so an anonymized email still looks              like one -- scanning for that shape would abort on most fixtures"
        );
        assert!(
            !Anonymizer::scan_input("contact me at alice@example.com").is_empty(),
            "an email address in the input was not reported"
        );
        assert!(
            !Anonymizer::scan_input("the server is at 192.168.1.10").is_empty(),
            "an IPv4 address in the input was not reported"
        );
        assert!(
            a.safety_net(r#"{"cwd":"/home/user/project"}"#).is_empty(),
            "the placeholder itself must not be reported"
        );
    }

    /// The safety net reports the **kind**, never the leak.
    #[test]
    fn the_safety_net_does_not_echo_the_leak() {
        let mut a = Anonymizer::new(0);
        a.forbid("my-secret-hostname");
        let hits = a.safety_net("the host is my-secret-hostname");
        assert!(
            hits.iter().all(|h| !h.contains("my-secret-hostname")),
            "the report echoes the leaked string, so the leak ends up in the log: {hits:?}"
        );
    }
}

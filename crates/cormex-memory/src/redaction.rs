//! Write-side negative filtering — credentials are blocked **before extraction
//! begins**.
//!
//! One of the deliberate differences from the mainstream memory systems, and it
//! comes down to **where** the filter sits: ahead of the extraction call rather
//! than inside it (rule 2 below has the counterexample).
//!
//! Previously this layer consisted of one sentence in the extract prompt ("the
//! literal values of credentials and secrets … do not extract"), and that is a
//! **soft constraint**: the model can see the raw text, and the raw text has
//! already crossed the network in full.
//!
//! # Three fixed rules, each matching the document
//!
//! ## 1. What gets blocked is the derived layer, not L0
//!
//! `episodes` is the single source of truth and **is not allowed to have holes**
//! — and the conversation body is exactly where secrets most often appear. So
//! what is blocked here is "entering extraction, being embedded, entering the
//! retrieval index", **not "don't write episodes"**. The raw text is persisted as
//! usual.
//!
//! Actually erasing raw text that has already been persisted is `redact`, and
//! that requires an explicit trigger, a second confirmation, and a tombstone
//! (`docs/memory.md` §11). Automatic destruction before the write is not covered
//! by that exception.
//!
//! ## 2. It must come before the LLM call
//!
//! This is the lesson Mem0 teaches by counterexample: its filtering sits at the
//! extraction **layer**, which means the raw text is sent out in full first and
//! filtered afterwards. That is why this function is called on the first line of
//! [`crate::extract::Extractor::extract`], not in `write_candidates`.
//!
//! ## 3. Grade A only — the handful of categories with zero ambiguity
//!
//! Email addresses, URLs, and generic high-entropy strings are all left alone.
//! The document has measured numbers: SecretBench's "Other" category is
//! **99.29% false positives**, and the true-positive rate for database/server
//! URLs is only **1.6%**. Hard-blocking those would mean ordinary technical
//! discussion produces no memory across the board — and that loss is silent,
//! because nobody ever comes to report "my memory is missing things".
//!
//! # Why block **the whole turn** rather than redact the secret and carry on
//!
//! Redacting is less "wasteful", but it stakes correctness on the regex being
//! complete: if the pattern matches one character short, what stays in the text
//! is part of the secret, and it proceeds into the model, into the embedding, and
//! into the index as usual — a filter that **looks like it is working**.
//!
//! Blocking the whole turn fails in the opposite direction: the worst case is
//! that one turn's facts don't get extracted, and that has a log entry, is
//! explainable, and can be re-extracted afterwards. Grade A is by definition the
//! tier where you would rather go without.

use std::sync::LazyLock;

use regex::Regex;

/// Which rule matched.
///
/// **Deliberately carries only the rule name, never the matched content.** The
/// only consumer of this type is the log, and putting the matched fragment into
/// the log moves the secret from "only in the conversation body" to "also in the
/// container logs" — and container logs get pasted wholesale into tickets, CI
/// output, and chat windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// A PEM private key block
    PrivateKey,
    /// A key with a vendor prefix (AWS / GitHub / Slack / Stripe / OpenAI /
    /// Google …)
    VendorKey,
    /// `Authorization: Bearer …`
    BearerHeader,
    /// A JWT (three segments; the payload often carries identity information)
    Jwt,
    /// A `.env`-style assignment whose key name says outright that this is a
    /// credential
    EnvAssignment,
}

impl Rule {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PrivateKey => "private_key",
            Self::VendorKey => "vendor_key",
            Self::BearerHeader => "bearer_header",
            Self::Jwt => "jwt",
            Self::EnvAssignment => "env_assignment",
        }
    }
}

// ── The patterns, as constants ─────────────────────────────
//
// **One source per rule.** The `regex::Regex` and the `regex::bytes::Regex` that
// `scan_spans_bytes` uses are both compiled from these, so a rule cannot drift
// between the text path and the byte path — and a drift there would mean the
// masker and the write-side filter disagree about what a secret is, with no
// error anywhere.

/// A PEM private key block.
const PATTERN_PRIVATE_KEY: &str = r"-----BEGIN [A-Z ]*PRIVATE KEY-----";

/// Keys with a vendor prefix.
const PATTERN_VENDOR_KEY: &str = concat!(
    r"AKIA[0-9A-Z]{16}",               // AWS access key id
    r"|gh[pousr]_[A-Za-z0-9]{20,}",    // GitHub PAT / OAuth / server / refresh
    r"|xox[abposr]-[A-Za-z0-9-]{10,}", // Slack
    r"|sk_live_[A-Za-z0-9]{16,}",      // Stripe (live only; test keys are ignored)
    r"|sk-[A-Za-z0-9_-]{20,}",         // The OpenAI / Anthropic family
    r"|AIza[0-9A-Za-z_-]{35}",         // Google API key
    r"|glpat-[A-Za-z0-9_-]{16,}",      // GitLab PAT
);

/// `Authorization: Bearer …`.
const PATTERN_BEARER: &str = r"(?i)authorization\s*:\s*bearer\s+[A-Za-z0-9._~+/=-]{8,}";

/// The three-segment JWT shape.
const PATTERN_JWT: &str = r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}";

/// A `.env`-style assignment. Capture group 1 = the key name, group 2 = the value.
const PATTERN_ENV_ASSIGN: &str =
    r#"(?im)^[ \t]*(?:export[ \t]+)?([A-Za-z][A-Za-z0-9_]*)[ \t]*=[ \t]*['\x22]?([^\s'\x22]+)"#;

/// A PEM private key block. Zero regex ambiguity; the best signal-to-noise ratio
/// of any rule here.
static PRIVATE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PATTERN_PRIVATE_KEY).expect("constant regex"));

/// Keys with a vendor prefix. This category hits 87.7% on SecretBench.
///
/// Every alternative demands the prefix **plus** a sufficiently long body: matching
/// the prefix alone would kill sentences like "we're using AWS", and the prefix on
/// its own contributes nothing to the decision.
static VENDOR_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PATTERN_VENDOR_KEY).expect("constant regex"));

/// `Authorization: Bearer …`。
static BEARER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PATTERN_BEARER).expect("constant regex"));

/// The three-segment JWT shape. The payload segment almost always starts with
/// `eyJ` (the base64 of `{"`).
static JWT: LazyLock<Regex> = LazyLock::new(|| Regex::new(PATTERN_JWT).expect("constant regex"));

/// A `.env`-style assignment. Capture group 1 = the key name, group 2 = the value.
///
/// The single most frequent way a coding agent walks into this, with precedent in
/// this very repository.
static ENV_ASSIGN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(PATTERN_ENV_ASSIGN).expect("constant regex"));

/// A key name containing any of these as a **whole segment** (split on `_`) is
/// judged to be a credential.
///
/// Requiring a whole segment rather than a substring blocks a real class of false
/// positives: `KEY` as a substring matches `MONKEY`, `KEYBOARD`, and `KEYWORDS`;
/// and `TOKENS` (plural) is a different segment from `TOKEN` — so
/// `MAX_TOKENS=100000` is not taken for a credential.
const SECRET_SEGMENTS: &[&str] = &[
    "SECRET",
    "SECRETS",
    "PASSWORD",
    "PASSWD",
    "PWD",
    "TOKEN",
    "APIKEY",
    "CREDENTIAL",
    "CREDENTIALS",
];

/// A key name also counts when one of these appears immediately before a `KEY`
/// segment (`API_KEY` / `PRIVATE_KEY` / `ACCESS_KEY`).
///
/// A lone `KEY` segment is not enough: `SORT_KEY`, `PRIMARY_KEY`, and `CACHE_KEY`
/// are everywhere in technical discussion, and what follows them is a column
/// name, not a credential.
const KEY_QUALIFIERS: &[&str] = &["API", "PRIVATE", "ACCESS", "SECRET", "AUTH", "SIGNING"];

/// Scan the text once and return the **first** rule that matches.
///
/// It does not return every match: the only decision the caller has to make is
/// "extract or not", and listing more rule names would only make the log longer.
/// The first one is enough to locate the problem.
#[must_use]
pub fn scan(text: &str) -> Option<Rule> {
    if PRIVATE_KEY.is_match(text) {
        return Some(Rule::PrivateKey);
    }
    if VENDOR_KEY.is_match(text) {
        return Some(Rule::VendorKey);
    }
    if BEARER.is_match(text) {
        return Some(Rule::BearerHeader);
    }
    if JWT.is_match(text) {
        return Some(Rule::Jwt);
    }
    for c in ENV_ASSIGN.captures_iter(text) {
        let (Some(key), Some(value)) = (c.get(1), c.get(2)) else {
            continue;
        };
        if is_secret_key(key.as_str()) && looks_assigned(value.as_str()) {
            return Some(Rule::EnvAssignment);
        }
    }
    None
}

// ── Spans: where the secrets are, not just whether there are any ──
//
// `scan` answers "is there a secret in here", which is what the write-side
// filter needs. The masker needs a different answer: **which byte ranges**, so
// that the raw layer can show the original bytes with those ranges replaced and
// nothing else touched (D5). That is a separate function rather than a flag on
// `scan`, because the two have different shapes — one returns a rule, the other
// a list of ranges — and folding them together would make the simple caller pay
// for the complex one's allocations.

use std::ops::Range;

/// A matched range and the rule that matched it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub range: Range<usize>,
    pub rule: Rule,
}

/// Every match of every rule, sorted by start, with overlaps merged.
///
/// # Why overlaps are merged
///
/// `sk-...` can appear inside a `Bearer` header, and a JWT can appear inside an
/// env assignment. Reporting both ranges would make `mask_text` replace the
/// inner one first and leave a fragment of the outer one — and the fragment is
/// exactly the part that was secret. Merging first means one contiguous
/// replacement per region.
///
/// The rule kept for a merged region is the **earliest-starting** one, which is
/// what a human reading the mask would name it by.
#[must_use]
pub fn scan_spans(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    for (re, rule) in [
        (&*PRIVATE_KEY, Rule::PrivateKey),
        (&*VENDOR_KEY, Rule::VendorKey),
        (&*BEARER, Rule::BearerHeader),
        (&*JWT, Rule::Jwt),
    ] {
        for m in re.find_iter(text) {
            spans.push(Span {
                range: m.start()..m.end(),
                rule,
            });
        }
    }
    // The env-assignment rule needs the key-name judgement, so it cannot be a
    // plain `find_iter`
    for c in ENV_ASSIGN.captures_iter(text) {
        let (Some(key), Some(value)) = (c.get(1), c.get(2)) else {
            continue;
        };
        if is_secret_key(key.as_str()) && looks_assigned(value.as_str()) {
            // The whole assignment, not just the value: the key name is part of
            // what identifies it, and masking only the value would leave
            // `OPENAI_API_KEY=` in the clear, which tells an attacker where to
            // look
            let whole = c.get(0).expect("group 0 always exists");
            spans.push(Span {
                range: whole.start()..whole.end(),
                rule: Rule::EnvAssignment,
            });
        }
    }
    merge(spans)
}

/// The same, over bytes.
///
/// # Why this exists at all
///
/// The masker works on the **raw layer's bytes**, which are not necessarily
/// UTF-8: a transcript can contain any bytes a tool wrote. Running the text
/// version would require a lossy conversion first, and the conversion changes
/// the byte offsets — so the ranges would point at the wrong place in the
/// original, which is the one thing the overlay must not do.
///
/// # Why the results agree on UTF-8 input
///
/// Both are compiled from the same pattern constants, and the byte version
/// operates on the same bytes the text version sees. `the_two_scanners_agree`
/// pins it.
#[must_use]
pub fn scan_spans_bytes(bytes: &[u8]) -> Vec<Span> {
    static BYTES_PRIVATE_KEY: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(PATTERN_PRIVATE_KEY).expect("constant regex"));
    static BYTES_VENDOR_KEY: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(PATTERN_VENDOR_KEY).expect("constant regex"));
    static BYTES_BEARER: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(PATTERN_BEARER).expect("constant regex"));
    static BYTES_JWT: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(PATTERN_JWT).expect("constant regex"));
    static BYTES_ENV_ASSIGN: LazyLock<regex::bytes::Regex> =
        LazyLock::new(|| regex::bytes::Regex::new(PATTERN_ENV_ASSIGN).expect("constant regex"));

    let mut spans = Vec::new();
    for (re, rule) in [
        (&*BYTES_PRIVATE_KEY, Rule::PrivateKey),
        (&*BYTES_VENDOR_KEY, Rule::VendorKey),
        (&*BYTES_BEARER, Rule::BearerHeader),
        (&*BYTES_JWT, Rule::Jwt),
    ] {
        for m in re.find_iter(bytes) {
            spans.push(Span {
                range: m.start()..m.end(),
                rule,
            });
        }
    }
    for c in BYTES_ENV_ASSIGN.captures_iter(bytes) {
        let (Some(key), Some(value)) = (c.get(1), c.get(2)) else {
            continue;
        };
        // The key-name judgement is on `&str`; a key name is ASCII by the
        // pattern's own definition, so the conversion cannot fail in a way that
        // matters
        let key = String::from_utf8_lossy(key.as_bytes()).into_owned();
        let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
        if is_secret_key(&key) && looks_assigned(&value) {
            let whole = c.get(0).expect("group 0 always exists");
            spans.push(Span {
                range: whole.start()..whole.end(),
                rule: Rule::EnvAssignment,
            });
        }
    }
    merge(spans)
}

/// Sort by start and merge overlapping or adjacent ranges.
fn merge(mut spans: Vec<Span>) -> Vec<Span> {
    spans.sort_by_key(|s| (s.range.start, s.range.end));
    let mut out: Vec<Span> = Vec::with_capacity(spans.len());
    for s in spans {
        match out.last_mut() {
            Some(last) if s.range.start <= last.range.end => {
                // Overlapping or touching: extend, keeping the earlier rule
                last.range.end = last.range.end.max(s.range.end);
            }
            _ => out.push(s),
        }
    }
    out
}

/// Replace every matched range with `[masked:<rule>]`.
///
/// Returns `Cow::Borrowed` when nothing matched, so the common case — a text
/// with no secrets, which is almost all of them — costs no allocation.
#[must_use]
pub fn mask_text(text: &str) -> std::borrow::Cow<'_, str> {
    let spans = scan_spans(text);
    if spans.is_empty() {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for s in spans {
        out.push_str(&text[cursor..s.range.start]);
        out.push_str("[masked:");
        out.push_str(s.rule.name());
        out.push(']');
        cursor = s.range.end;
    }
    out.push_str(&text[cursor..]);
    std::borrow::Cow::Owned(out)
}

/// Does this key name announce "what follows me is a credential"?
fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    let segs: Vec<&str> = upper.split('_').filter(|s| !s.is_empty()).collect();
    if segs.iter().any(|s| SECRET_SEGMENTS.contains(s)) {
        return true;
    }
    // `..._KEY` counts only when the segment before it is specific enough
    segs.windows(2)
        .any(|w| w[1] == "KEY" && KEY_QUALIFIERS.contains(&w[0]))
}

/// Whether the value looks like **something real was assigned**, rather than a
/// number or a placeholder.
///
/// At least one letter is required: config entries like `TOKEN_LIMIT=4096` and
/// `MAX_RETRIES=3` should not block a whole turn's extraction even when the key
/// name collides. Passwords are a mix of letters and digits; the all-digits one
/// is not a password.
///
/// Placeholders are excluded separately: `PASSWORD=<your-password>` is extremely
/// common in documentation and examples, and treating it as a credential would
/// mean "how do I configure this" conversations never produce any memory at all.
fn looks_assigned(value: &str) -> bool {
    if !value.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let v = value.trim_matches(['<', '>', '{', '}', '$', '(', ')']);
    let lower = v.to_ascii_lowercase();
    const PLACEHOLDERS: &[&str] = &[
        "your",
        "yourpassword",
        "xxx",
        "xxxx",
        "changeme",
        "placeholder",
        "redacted",
        "example",
        "todo",
        "fixme",
        "null",
        "none",
        "empty",
    ];
    if PLACEHOLDERS
        .iter()
        .any(|p| lower == *p || lower.starts_with(p))
    {
        return false;
    }
    // Leftovers from something already redacted, like `***` or `…`
    !v.chars().all(|c| c == '*' || c == '.' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assemble a synthetic secret. **Never leave a contiguous stretch of
    /// key-shaped text in the source.**
    ///
    /// These strings are all made up, but **the shape is real** — real enough that
    /// scanners take them for actual keys. This repository has hit that once:
    /// GitHub's push protection caught the fake Stripe key below and rejected the
    /// entire push. gitleaks, trufflehog and friends behave the same way.
    ///
    /// So the prefix and the body are written separately and concatenated at
    /// runtime. All the tests need is the shape, and the shape only comes into
    /// existence after the concatenation.
    fn synthetic(prefix: &str, body: &str) -> String {
        format!("{prefix}{body}")
    }

    /// Every category of grade-A credential has to be blocked.
    #[test]
    fn every_a_level_credential_is_blocked() {
        let vendor_keys = [
            synthetic("AKIA", "IOSFODNN7EXAMPLE"),
            synthetic("ghp", "_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
            synthetic("xoxb", "-1234567890-abcdefghijkl"),
            synthetic("sk", "_live_aBcDeFgHiJkLmNoPqRsTuVwX"),
            synthetic("sk", "-proj-aBcDeFgHiJkLmNoPqRsTuVwXyZ01"),
            synthetic("AIza", "SyA1234567890abcdefghijklmnopqrstuv"),
        ];
        for k in &vendor_keys {
            assert_eq!(
                scan(&format!("我的 key 是 {k}")),
                Some(Rule::VendorKey),
                "a vendor-prefixed key got through — this text would be sent to the extraction model verbatim"
            );
        }

        let bearer = synthetic("Authorization: Bearer ", "abcdef1234567890");
        assert_eq!(
            scan(&format!("curl -H '{bearer}' https://x")),
            Some(Rule::BearerHeader)
        );

        let cases: &[(&str, Rule)] = &[
            (
                "帮我看看这个\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow…\n",
                Rule::PrivateKey,
            ),
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA…",
                Rule::PrivateKey,
            ),
            (
                "token 是 eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
                Rule::Jwt,
            ),
            ("CORMEXD_TOKEN=s3cr3tvalue123", Rule::EnvAssignment),
            ("export DB_PASSWORD=hunter2", Rule::EnvAssignment),
            ("CORMEX_EMBED_API_KEY=abc123def456", Rule::EnvAssignment),
        ];
        for (text, want) in cases {
            assert_eq!(
                scan(text),
                Some(*want),
                "got through: {text}\n— this text would be sent to the extraction model verbatim, then on into the embedding and the index"
            );
        }
    }

    /// **Not one piece of ordinary technical discussion may be killed by
    /// mistake.**
    ///
    /// This matters more than the test above it. Missing a credential is visible
    /// (when something actually goes wrong, it can be traced), whereas killing
    /// something by mistake is **silent**: the user merely feels that "nothing
    /// seems to have been remembered from that conversation", and nobody ever
    /// comes to report it. This is exactly what the two numbers in the document
    /// are about — SecretBench's "Other" category is 99.29% false positives, and
    /// the true-positive rate for DB/server URLs is only 1.6%.
    #[test]
    fn ordinary_technical_talk_is_never_killed() {
        let benign = [
            "我们用 AWS 的 S3，bucket 叫 cormex-blobs",
            "GitHub Actions 里配一个 secrets.GITHUB_TOKEN 就行", // a reference, not a literal value
            "数据库连到 postgres://localhost:5442/cormex",
            "MAX_TOKENS=100000 这个上限要调高",
            "TOKEN_LIMIT=4096",
            "RUST_LOG=cormexd=info,warn",
            "CORMEX_BACKEND=mock",
            "PRIMARY_KEY 用 ULID，SORT_KEY 用时间戳",
            "缓存的 CACHE_KEY 是 session_id 拼上日期",
            "把 PASSWORD=<your-password> 换成你自己的",
            "DB_PASSWORD=***",
            "这个 commit 的 sha 是 a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
            "文件哈希 sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "他的邮箱是 zhang@example.com，电话 13800138000",
            "我们决定生产用 Postgres 17 作为数据库",
        ];
        for text in benign {
            assert_eq!(
                scan(text),
                None,
                "killed ordinary content by mistake: {text}\n— this turn's memory would silently fail to be extracted, and nobody would ever report it"
            );
        }
    }

    /// Key names are judged by segment: whole segments, not substrings.
    #[test]
    fn key_names_are_matched_by_whole_segments() {
        assert!(is_secret_key("CORMEXD_TOKEN"));
        assert!(is_secret_key("token"));
        assert!(is_secret_key("AWS_SECRET_ACCESS_KEY"));
        assert!(is_secret_key("MY_API_KEY"));
        assert!(is_secret_key("SSH_PRIVATE_KEY"));

        assert!(
            !is_secret_key("MAX_TOKENS"),
            "the plural is a different segment, not TOKEN"
        );
        assert!(
            !is_secret_key("MONKEY"),
            "KEY as a substring would match this"
        );
        assert!(!is_secret_key("KEYBOARD_LAYOUT"));
        assert!(
            !is_secret_key("PRIMARY_KEY"),
            "a column name, not a credential"
        );
        assert!(!is_secret_key("SORT_KEY"));
        assert!(!is_secret_key("CACHE_KEY"));
    }

    /// Judging the value: neither all-digits nor placeholders count.
    #[test]
    fn a_value_must_look_like_it_was_actually_set() {
        assert!(looks_assigned("hunter2"));
        assert!(looks_assigned("s3cr3t"));

        assert!(!looks_assigned("100000"), "all digits is not a password");
        assert!(
            !looks_assigned("<your-password>"),
            "a documentation placeholder"
        );
        assert!(!looks_assigned("changeme"));
        assert!(
            !looks_assigned("***"),
            "leftovers from something already redacted"
        );
        assert!(!looks_assigned("..."));
    }

    /// The rule name goes into the log; **the matched content never does**.
    ///
    /// This guards a constraint that is very easy to break in the moment someone
    /// thinks "let me print the context too, it'll make debugging easier". Do
    /// that and the secret goes from "only in the conversation body" to "also in
    /// the container logs" — and container logs get pasted wholesale into tickets
    /// and chat windows.
    #[test]
    fn the_rule_carries_a_name_and_nothing_else() {
        let hit = scan("AKIAIOSFODNN7EXAMPLE").expect("should match");
        assert_eq!(hit.name(), "vendor_key");
        // Rule is a fieldless enum — at the type level it cannot carry content.
        // The line below asserts "it isn't in the Debug output either", because
        // logs frequently use {:?}
        assert!(
            !format!("{hit:?}").contains("AKIA"),
            "the Debug output leaked the matched content"
        );
    }

    // ── spans ──────────────────────────────────────────────

    /// Every rule has a positive case, and `scan_spans` finds it.
    #[test]
    fn every_rule_produces_a_span() {
        let cases = [
            ("-----BEGIN RSA PRIVATE KEY-----", Rule::PrivateKey),
            ("AKIAIOSFODNN7EXAMPLE", Rule::VendorKey),
            ("Authorization: Bearer abcdefghijklmnop", Rule::BearerHeader),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop",
                Rule::Jwt,
            ),
            (
                "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz",
                Rule::EnvAssignment,
            ),
        ];
        for (text, want) in cases {
            let spans = scan_spans(text);
            assert!(
                !spans.is_empty(),
                "no span for {text:?}, which the {} rule should match",
                want.name()
            );
            assert!(
                spans.iter().any(|s| s.rule == want),
                "{text:?} matched {:?}, not {}",
                spans.iter().map(|s| s.rule).collect::<Vec<_>>(),
                want.name()
            );
        }
    }

    /// A text with no secret produces no spans, and `mask_text` borrows.
    #[test]
    fn a_clean_text_produces_no_spans() {
        let text = "we use RustFS for object storage";
        assert!(scan_spans(text).is_empty());
        assert!(
            matches!(mask_text(text), std::borrow::Cow::Borrowed(_)),
            "a clean text must not be copied: almost every text is clean, and copying all of              them is pure waste"
        );
    }

    /// Overlapping matches merge into one range.
    ///
    /// `sk-...` inside a `Bearer` header, or a JWT inside an env assignment.
    /// Replacing the inner range first would leave a fragment of the outer one —
    /// and the fragment is the part that was secret.
    #[test]
    fn overlapping_matches_merge() {
        let text = "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop";
        let spans = scan_spans(text);
        assert_eq!(
            spans.len(),
            1,
            "the bearer header and the JWT overlap and must merge into one range, got {spans:?}"
        );
        assert_eq!(spans[0].range, 0..text.len());
    }

    /// The text and byte scanners agree on UTF-8 input.
    ///
    /// They are compiled from the same constants; a drift would mean the masker
    /// and the write-side filter disagree about what a secret is, with no error
    /// anywhere.
    #[test]
    fn the_two_scanners_agree() {
        let cases = [
            "plain text with nothing in it",
            "AKIAIOSFODNN7EXAMPLE",
            "对象存储用的是 RustFS，密钥是 sk-abcdefghijklmnopqrstuvwxyz",
            "-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----",
            "TOKEN=abc123",
            "MAX_TOKENS=100000",
        ];
        for text in cases {
            assert_eq!(
                scan_spans(text),
                scan_spans_bytes(text.as_bytes()),
                "the text and byte scanners disagree on {text:?}"
            );
        }
    }

    /// After masking, `scan` finds nothing.
    ///
    /// This is the property the write-side filter depends on: a masked text must
    /// not trip the rule that caught it, or the filter would reject its own
    /// output.
    #[test]
    fn a_masked_text_no_longer_trips_the_scanner() {
        let cases = [
            "AKIAIOSFODNN7EXAMPLE",
            "Authorization: Bearer abcdefghijklmnop",
            "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop",
        ];
        for text in cases {
            let masked = mask_text(text);
            assert!(
                scan(&masked).is_none(),
                "masking {text:?} produced {masked:?}, which the scanner still flags as {:?}",
                scan(&masked)
            );
        }
    }

    /// The mask names the rule, so a reader knows what was there.
    #[test]
    fn the_mask_names_the_rule() {
        let masked = mask_text("AKIAIOSFODNN7EXAMPLE");
        assert_eq!(masked, "[masked:vendor_key]");
        assert!(
            !masked.contains("AKIA"),
            "the mask must not leave a fragment of the secret: {masked}"
        );
    }

    /// Masking preserves the text around the match.
    #[test]
    fn masking_preserves_the_surrounding_text() {
        let masked = mask_text("before AKIAIOSFODNN7EXAMPLE after");
        assert_eq!(masked, "before [masked:vendor_key] after");
    }

    /// A non-secret env assignment produces no span.
    ///
    /// `MAX_TOKENS=100000` is the case the key-name judgement exists for: `TOKENS`
    /// is a different segment from `TOKEN`.
    #[test]
    fn a_non_secret_env_assignment_produces_no_span() {
        for text in ["MAX_TOKENS=100000", "SORT_KEY=name", "PRIMARY_KEY=id"] {
            assert!(
                scan_spans(text).is_empty(),
                "{text:?} was flagged; the key-name judgement is too loose"
            );
        }
    }

    /// The byte scanner works on bytes that are not valid UTF-8.
    ///
    /// The raw layer holds whatever a tool wrote; converting to UTF-8 first would
    /// change the offsets, and the ranges would then point at the wrong place in
    /// the original.
    #[test]
    fn the_byte_scanner_handles_invalid_utf8() {
        let mut bytes = vec![0xff, 0xfe]; // not valid UTF-8
        bytes.extend_from_slice(b" AKIAIOSFODNN7EXAMPLE ");
        let spans = scan_spans_bytes(&bytes);
        assert_eq!(spans.len(), 1);
        assert_eq!(
            &bytes[spans[0].range.clone()],
            b"AKIAIOSFODNN7EXAMPLE",
            "the range must point at the secret in the original bytes"
        );
    }
}

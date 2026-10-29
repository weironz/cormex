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

/// A PEM private key block. Zero regex ambiguity; the best signal-to-noise ratio
/// of any rule here.
static PRIVATE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").expect("constant regex"));

/// Keys with a vendor prefix. This category hits 87.7% on SecretBench.
///
/// Every alternative demands the prefix **plus** a sufficiently long body: matching
/// the prefix alone would kill sentences like "we're using AWS", and the prefix on
/// its own contributes nothing to the decision.
static VENDOR_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"AKIA[0-9A-Z]{16}",               // AWS access key id
        r"|gh[pousr]_[A-Za-z0-9]{20,}",    // GitHub PAT / OAuth / server / refresh
        r"|xox[abposr]-[A-Za-z0-9-]{10,}", // Slack
        r"|sk_live_[A-Za-z0-9]{16,}",      // Stripe (live only; test keys are ignored)
        r"|sk-[A-Za-z0-9_-]{20,}",         // The OpenAI / Anthropic family
        r"|AIza[0-9A-Za-z_-]{35}",         // Google API key
        r"|glpat-[A-Za-z0-9_-]{16,}",      // GitLab PAT
    ))
    .expect("constant regex")
});

/// `Authorization: Bearer …`。
static BEARER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)authorization\s*:\s*bearer\s+[A-Za-z0-9._~+/=-]{8,}").expect("constant regex")
});

/// The three-segment JWT shape. The payload segment almost always starts with
/// `eyJ` (the base64 of `{"`).
static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}")
        .expect("constant regex")
});

/// A `.env`-style assignment. Capture group 1 = the key name, group 2 = the value.
///
/// The single most frequent way a coding agent walks into this, with precedent in
/// this very repository.
static ENV_ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^[ \t]*(?:export[ \t]+)?([A-Za-z][A-Za-z0-9_]*)[ \t]*=[ \t]*['\x22]?([^\s'\x22]+)",
    )
    .expect("constant regex")
});

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
}

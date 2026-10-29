//! The source catalog: where each agent keeps its session files, and how to
//! recognise a session id from a path.
//!
//! Design: docs/design/session-archive-architecture.md §5.2 (D8).
//!
//! # Why this is data
//!
//! "Which file belongs to which agent, and what is its session id" changes
//! whenever an upstream agent changes its storage layout — which is often, and
//! never on our schedule. Written as code, every such change is a release of
//! the CLI; written as data, it is a TOML file that the server can hand out a
//! newer copy of (`GET /capture/catalog`) and the client can pick up without
//! being rebuilt.
//!
//! The built-in copy is compiled in with `include_str!`, so a fresh install
//! works with no network at all, and the server's copy is an **override**, not
//! a requirement.
//!
//! # Everything here works from the path alone
//!
//! The `cwd` decode and the `session_id` rules read the path and nothing else
//! (D2). Reading a file's contents to find its session id would mean the
//! discovery step — which is supposed to be cheap enough to run on every hook —
//! opens every transcript in the directory.

use std::sync::LazyLock;

use serde::{Deserialize, Serialize};

/// The built-in catalog, compiled into the binary.
const BUILTIN_TOML: &str = include_str!("../catalog/agents.toml");

/// The parsed built-in catalog.
///
/// `LazyLock` rather than parsing per call: discovery runs on every hook, and
/// re-parsing a few kilobytes of TOML each time is pure waste — but more to the
/// point, a parse error would then surface at a random moment during a sync
/// rather than at the first use.
pub static BUILTIN: LazyLock<Catalog> = LazyLock::new(|| {
    Catalog::parse(BUILTIN_TOML).expect(
        "the built-in catalog is compiled in and must parse; a failure here is a build-time \
         mistake in crates/cormex-proto/catalog/agents.toml, not a runtime condition",
    )
});

/// The whole catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// The catalog format version. Bumping it means a client that does not
    /// understand the new shape should refuse the file rather than guess.
    pub schema: u32,
    /// The data's own version, for "is the server's copy newer than mine".
    pub version: String,
    /// The agents.
    ///
    /// `agents` in both the TOML and the JSON: the wire shape in the design is
    /// `{"schema":1,"version":…,"agents":[…]}` (`GET /capture/catalog`), and
    /// having the TOML spell it `[[agent]]` would mean two names for one thing —
    /// with the JSON one silently wrong, since nothing deserialises it back.
    #[serde(default)]
    pub agents: Vec<AgentEntry>,
}

/// One agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentEntry {
    /// Lowercase identifier, matching `capture_sessions.agent`'s CHECK.
    pub id: String,
    /// What to show a human.
    pub name: String,
    /// Where the agent keeps its files, in the order to try them.
    #[serde(default)]
    pub home: Vec<HomeRoot>,
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HookSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention: Option<RetentionSpec>,
}

/// One candidate home directory.
///
/// An environment variable first, a literal path second: `CLAUDE_CONFIG_DIR`
/// wins when set, because that is how a user says "my config is not in the
/// default place", and ignoring it would capture the wrong directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HomeRoot {
    Env { env: String },
    Path { path: String },
}

/// One kind of file within an agent's home.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpec {
    pub role: super::capture::FileRole,
    /// Relative to the home, forward slashes, globset syntax.
    pub glob: String,
    pub framing: super::capture::Framing,
    /// When set, a sibling with this suffix is the **same logical file**
    /// (Codex's `.jsonl.zst`). The client keeps one of them — plaintext wins —
    /// and the logical path is always the plaintext name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub also_compressed: Option<String>,
    pub session_id: SessionIdRule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionIdRule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<CwdRule>,
}

/// How to derive a session id from a path.
///
/// The variants are the ones the surveyed hosts actually need; a new host that
/// needs something else is a new variant here plus one arm in the CLI, which is
/// still much cheaper than a per-agent code path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "from", rename_all = "snake_case")]
pub enum SessionIdRule {
    /// The file stem, minus any compression suffix.
    ///
    /// A struct variant with no fields rather than a unit variant: serde's
    /// internally-tagged representation cannot express a unit variant, and the
    /// TOML spelling `{ from = "stem" }` is what the catalog uses.
    Stem {},
    /// A capture group named `id` in a regex applied to the file stem.
    StemRegex { regex: String },
    /// The `index`-th segment of the logical path, counting from zero.
    ///
    /// Negative indices count from the end. **Only positive indices are
    /// meaningful when the glob contains `**`**: the depth is not fixed, so
    /// "third from the end" addresses a different segment depending on how deep
    /// the match went.
    PathSegment {
        index: i32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// A fixed value.
    ///
    /// `per_device = true` appends `@<device_id>`: the host keeps one global
    /// memory file per machine, and without the suffix two machines' memories
    /// would merge into one session — whose contents do not agree.
    Const {
        value: String,
        #[serde(default)]
        per_device: bool,
    },
}

/// How to derive a working directory from a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CwdRule {
    /// `parent_dir`: the directory containing the file.
    pub from: String,
    /// `claude_project_dir`: Claude Code encodes a path by replacing `/` with
    /// `-`. The decode is **lossy** (a directory name containing a literal `-`
    /// is ambiguous), so the result is a fallback for when a hook did not
    /// supply the cwd, and the client marks it as guessed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode: Option<String>,
}

/// The host's hook mechanism.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookSpec {
    /// Which installer to use: `claude_code_settings` (merge into
    /// `settings.json`), `codex_hooks_json` (write `hooks.json`).
    pub kind: String,
    /// The lifecycle events to hook.
    #[serde(default)]
    pub events: Vec<String>,
}

/// The host's own session-file retention, which the CLI offers to raise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionSpec {
    pub setting: String,
    pub file: String,
    pub default_days: u32,
}

impl Catalog {
    /// Parse a catalog from TOML.
    ///
    /// # Errors
    /// The TOML is malformed, or a field has the wrong shape. The message is
    /// the parser's, which names the line — that is the whole reason a catalog
    /// author gets anything useful out of a typo.
    pub fn parse(toml_text: &str) -> Result<Self, String> {
        toml::from_str(toml_text).map_err(|e| e.to_string())
    }

    /// The built-in catalog.
    #[must_use]
    pub fn builtin() -> &'static Catalog {
        &BUILTIN
    }

    /// One agent by id.
    #[must_use]
    pub fn agent(&self, id: &str) -> Option<&AgentEntry> {
        self.agents.iter().find(|a| a.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The built-in catalog parses, and its ids match the database's CHECK.
    ///
    /// `capture_sessions.agent` is constrained to `^[a-z][a-z0-9_]{1,31}$`. An
    /// id outside that shape is rejected by Postgres at insert time — after the
    /// client has already discovered and uploaded the file — so the check
    /// belongs here, where the message can say which agent and why.
    #[test]
    fn the_builtin_catalog_parses_and_its_ids_match_the_ddl() {
        let c = Catalog::builtin();
        assert_eq!(c.schema, 1, "the built-in catalog's schema version");
        assert!(
            !c.agents.is_empty(),
            "the built-in catalog has no agents -- the include_str! is pointing at the wrong file"
        );

        let mut seen = std::collections::BTreeSet::new();
        for a in &c.agents {
            assert!(
                seen.insert(a.id.clone()),
                "agent id {:?} appears twice; discovery would then register the same file under \
                 two agents",
                a.id
            );
            let ok = (2..=32).contains(&a.id.len())
                && a.id.starts_with(|c: char| c.is_ascii_lowercase())
                && a.id
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
            assert!(
                ok,
                "agent id {:?} does not match ^[a-z][a-z0-9_]{{1,31}}$ -- capture_sessions.agent \
                 has that CHECK, so the insert would be rejected after the upload",
                a.id
            );
            assert!(
                !a.home.is_empty(),
                "agent {:?} has no home; discovery would find nothing",
                a.id
            );
            assert!(!a.sources.is_empty(), "agent {:?} has no sources", a.id);
        }
    }

    /// Every regex in the catalog compiles.
    ///
    /// A bad regex would otherwise surface as a panic or a skipped file during
    /// discovery — on somebody's machine, at hook time, where the output is
    /// discarded.
    #[test]
    fn every_regex_compiles() {
        for a in &Catalog::builtin().agents {
            for s in &a.sources {
                for rule in std::iter::once(&s.session_id).chain(s.parent_session_id.as_ref()) {
                    if let SessionIdRule::StemRegex { regex } = rule {
                        regex::Regex::new(regex).unwrap_or_else(|e| {
                            panic!(
                                "agent {:?} has a session_id regex that does not compile: {e}\n\
                                 the pattern is {regex:?}",
                                a.id
                            )
                        });
                        assert!(
                            regex.contains("(?P<id>"),
                            "agent {:?}'s regex {regex:?} has no named `id` group, so it can \
                             never produce a session id",
                            a.id
                        );
                    }
                }
            }
        }
    }

    /// Claude Code's memory pseudo-sessions are **per project**, Codex's are
    /// **per device**.
    ///
    /// Both are easy to get backwards, and both failures are silent: the first
    /// merges every project's memory into one session; the second merges two
    /// machines' memories into one, and their contents do not agree.
    #[test]
    fn the_two_memory_pseudo_sessions_are_scoped_differently() {
        let c = Catalog::builtin();

        let claude = c
            .agent("claude_code")
            .expect("claude_code is in the catalog");
        let memory = claude
            .sources
            .iter()
            .find(|s| s.role == super::super::capture::FileRole::Memory)
            .expect("Claude Code has a memory source");
        match &memory.session_id {
            SessionIdRule::PathSegment { index, prefix } => {
                assert_eq!(
                    *index, 1,
                    "Claude Code's memory glob is projects/*/memory/**, so the project segment is \
                     index 1"
                );
                assert_eq!(
                    prefix.as_deref(),
                    Some("memory:"),
                    "the pseudo-session id needs a prefix, or it collides with a real session id \
                     that happens to equal the project directory name"
                );
            }
            other => {
                panic!("Claude Code's memory session_id should be a path segment, got {other:?}")
            }
        }

        let codex = c.agent("codex").expect("codex is in the catalog");
        let memory = codex
            .sources
            .iter()
            .find(|s| s.role == super::super::capture::FileRole::Memory)
            .expect("Codex has a memory source");
        match &memory.session_id {
            SessionIdRule::Const { value, per_device } => {
                assert_eq!(value, "memory");
                assert!(
                    *per_device,
                    "Codex's memory is one file per machine; without per_device the two machines' \
                     memories merge into one session whose contents do not agree"
                );
            }
            other => panic!("Codex's memory session_id should be a constant, got {other:?}"),
        }
    }

    /// Codex's `.jsonl.zst` is the same logical file as the `.jsonl`.
    #[test]
    fn codex_declares_its_compressed_sibling() {
        let codex = Catalog::builtin().agent("codex").expect("codex");
        let transcript = codex
            .sources
            .iter()
            .find(|s| s.role == super::super::capture::FileRole::Transcript)
            .expect("Codex has a transcript source");
        assert_eq!(
            transcript.also_compressed.as_deref(),
            Some(".zst"),
            "without this, a cold Codex rollout would be captured twice -- once as .jsonl.zst and \
             once as .jsonl -- and the two would be different sessions' worth of duplicate bytes"
        );
    }

    /// The catalog survives a round trip through JSON, which is how the server
    /// hands it out (`GET /capture/catalog` returns "the TOML's JSON form").
    #[test]
    fn the_catalog_round_trips_through_json() {
        let c = Catalog::builtin();
        let json = serde_json::to_string(c).expect("serialize");
        let back: Catalog = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(
            &back, c,
            "the catalog changed shape on the way through JSON"
        );
    }

    /// A malformed catalog is an error, not a panic.
    #[test]
    fn a_malformed_catalog_is_an_error() {
        assert!(
            Catalog::parse("schema = ").is_err(),
            "truncated TOML must be reported, not accepted"
        );
        assert!(
            Catalog::parse("schema = 1\nversion = \"x\"\n[[agents]]\nid = 3").is_err(),
            "an agent id of the wrong type must be reported"
        );
    }
}

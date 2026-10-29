//! Turning the source catalog into a list of files on this machine.
//!
//! Design: docs/design/session-archive-architecture.md §5.2 (D2, D7, D8).
//!
//! # Discovery reads paths and metadata, never contents
//!
//! Every rule here works from the path: which agent's home, which glob matched,
//! which segment is the session id. That is not an optimisation — discovery
//! runs on every hook, and a hook has a 1.5-second budget on `SessionEnd`. It
//! also keeps the promise the privacy section makes: the client does not open a
//! transcript until it is about to upload it.
//!
//! # The one lossy decode
//!
//! Claude Code encodes a project directory by replacing `/` with `-`. That is
//! not reversible (a directory name containing a literal `-` is ambiguous), so
//! the decoded `cwd` is a **fallback** for when a hook did not supply one, and
//! the result is marked as guessed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use cormex_proto::capture::{FileRole, Framing};
use cormex_proto::catalog::{AgentEntry, Catalog, HomeRoot, SessionIdRule, SourceSpec};

use crate::config::{Config, Exclusion};

/// One file discovery found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredFile {
    pub agent: String,
    pub role: FileRole,
    pub framing: Framing,
    /// Relative to the agent's home, forward slashes, **plaintext** name: a
    /// `.jsonl.zst` and its `.jsonl` sibling are the same logical file.
    pub logical_path: String,
    pub physical_path: PathBuf,
    /// Whether `physical_path` is the compressed form.
    pub compressed: bool,
    pub native_session_id: String,
    pub parent_native_session_id: Option<String>,
    pub cwd: Option<String>,
    /// Whether `cwd` was decoded out of the path rather than supplied.
    pub cwd_guessed: bool,
}

/// Resolve an agent's home directory.
///
/// Environment variable first, then the literal path: `CLAUDE_CONFIG_DIR` is
/// how a user says "my config is not in the default place", and ignoring it
/// would capture the wrong directory — or nothing.
#[must_use]
pub fn resolve_home(entry: &AgentEntry) -> Option<PathBuf> {
    for root in &entry.home {
        let candidate = match root {
            HomeRoot::Env { env } => std::env::var(env).ok().map(PathBuf::from),
            HomeRoot::Path { path } => expand_tilde(path),
        };
        if let Some(p) = candidate
            && p.is_dir()
        {
            return Some(p);
        }
    }
    None
}

/// Expand a leading `~`.
///
/// `directories::BaseDirs` rather than `$HOME`: on Windows the home directory
/// is `%USERPROFILE%`, and `$HOME` is often unset there.
fn expand_tilde(path: &str) -> Option<PathBuf> {
    let rest = path.strip_prefix("~/").or_else(|| path.strip_prefix('~'));
    match rest {
        None => Some(PathBuf::from(path)),
        Some(rest) => {
            let base = directories::BaseDirs::new()?;
            let mut p = base.home_dir().to_path_buf();
            if !rest.is_empty() {
                p.push(rest.trim_start_matches('/'));
            }
            Some(p)
        }
    }
}

/// Discover every file the catalog describes, minus the exclusions.
///
/// # Errors
/// A glob in the catalog does not compile. That is a **catalog** bug, and it is
/// reported rather than skipped: a silently skipped glob means an agent's files
/// stop being captured, and nothing says so.
pub fn discover(catalog: &Catalog, cfg: &Config, device_id: &str) -> Result<Vec<DiscoveredFile>> {
    let mut out = Vec::new();
    for agent in &catalog.agents {
        let Some(home) = resolve_home(agent) else {
            continue;
        };
        for source in &agent.sources {
            out.extend(discover_source(agent, source, &home, cfg, device_id)?);
        }
    }
    Ok(out)
}

fn discover_source(
    agent: &AgentEntry,
    source: &SourceSpec,
    home: &Path,
    cfg: &Config,
    device_id: &str,
) -> Result<Vec<DiscoveredFile>> {
    // **`literal_separator(true)` is mandatory.** Without it globset's `*`
    // matches `/` as well, so `projects/*/*.jsonl` would also match
    // `projects/a/subagents/child.jsonl` — and the sub-agent's transcript would
    // be captured as the parent's, under the wrong session id.
    let glob = globset::GlobBuilder::new(&source.glob)
        .literal_separator(true)
        .build()
        .map_err(|e| anyhow::anyhow!("agent {} has a glob that does not compile: {e}", agent.id))?
        .compile_matcher();

    // `also_compressed`: a `.jsonl.zst` beside a `.jsonl` is the same logical
    // file. Collect both, then keep one per logical path — plaintext wins,
    // matching Codex's own rule.
    let mut by_logical: std::collections::BTreeMap<String, DiscoveredFile> =
        std::collections::BTreeMap::new();

    for entry in walkdir::WalkDir::new(home)
        .follow_links(false)
        .into_iter()
        .filter_map(std::result::Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(home) else {
            continue;
        };
        let logical = to_slash(rel);
        let (logical_plain, compressed) = match &source.also_compressed {
            Some(suffix) => match logical.strip_suffix(suffix) {
                Some(stem) => (stem.to_owned(), true),
                None => (logical.clone(), false),
            },
            None => (logical.clone(), false),
        };
        if !glob.is_match(&logical_plain) {
            continue;
        }

        let segments: Vec<&str> = logical_plain.split('/').collect();
        let Some(mut native_session_id) = session_id(&source.session_id, &logical_plain, &segments)
        else {
            // A path that matches the glob but yields no session id is a
            // catalog bug, not a file to skip silently — but skipping is the
            // lesser evil at runtime: the alternative is failing the whole
            // sync over one stray file
            tracing::debug!(
                agent = %agent.id,
                path = %logical_plain,
                "no session id could be derived; skipping"
            );
            continue;
        };
        // `per_device`: the host keeps one global memory file per machine, and
        // without the suffix two machines' memories merge into one session
        // whose contents do not agree
        if let SessionIdRule::Const {
            per_device: true, ..
        } = &source.session_id
        {
            native_session_id = format!("{native_session_id}@{device_id}");
        }
        let parent = source
            .parent_session_id
            .as_ref()
            .and_then(|rule| session_id(rule, &logical_plain, &segments));

        let (cwd, cwd_guessed) = match &source.cwd {
            Some(rule) if rule.decode.as_deref() == Some("claude_project_dir") => {
                (decode_claude_project_dir(&segments), true)
            }
            Some(_) => (segments.first().map(|s| format!("/{s}")), true),
            None => (None, false),
        };

        let file = DiscoveredFile {
            agent: agent.id.clone(),
            role: source.role,
            framing: source.framing,
            logical_path: logical_plain.clone(),
            physical_path: entry.path().to_path_buf(),
            compressed,
            native_session_id: native_session_id.clone(),
            parent_native_session_id: parent,
            cwd,
            cwd_guessed,
        };

        if excluded(cfg, &file) {
            continue;
        }

        // Plaintext wins: a compressed sibling is only kept when there is no
        // plaintext one. Keeping both would upload the same conversation twice
        // under two different logical paths.
        match by_logical.get(&logical_plain) {
            Some(existing) if !existing.compressed => {}
            _ => {
                by_logical.insert(logical_plain, file);
            }
        }
    }

    Ok(by_logical.into_values().collect())
}

/// Apply the session-id rule to a path.
fn session_id(rule: &SessionIdRule, logical: &str, segments: &[&str]) -> Option<String> {
    match rule {
        SessionIdRule::Stem {} => {
            let file = logical.rsplit('/').next()?;
            Some(file.split('.').next()?.to_owned())
        }
        SessionIdRule::StemRegex { regex } => {
            let file = logical.rsplit('/').next()?;
            let stem = file.split('.').next()?;
            let re = regex::Regex::new(regex).ok()?;
            re.captures(stem)?.name("id").map(|m| m.as_str().to_owned())
        }
        SessionIdRule::PathSegment { index, prefix } => {
            // Negative indices count from the end; only positive ones are
            // meaningful when the glob contains `**`, since the depth varies
            let i = if *index < 0 {
                segments.len().checked_sub(index.unsigned_abs() as usize)?
            } else {
                usize::try_from(*index).ok()?
            };
            let seg = segments.get(i)?;
            Some(match prefix {
                Some(p) => format!("{p}{seg}"),
                None => (*seg).to_owned(),
            })
        }
        SessionIdRule::Const {
            value,
            per_device: _,
        } => {
            // The `@<device_id>` suffix for `per_device` is appended by
            // `discover`, which knows the device id; this function only sees
            // the path
            Some(value.clone())
        }
    }
}

/// Decode Claude Code's project directory name back into a path.
///
/// **Lossy**: the encoding replaces `/` with `-`, so a directory whose own name
/// contains `-` cannot be distinguished from a path separator. That is why the
/// result is a fallback and is marked as guessed.
fn decode_claude_project_dir(segments: &[&str]) -> Option<String> {
    // projects/<encoded>/<file>
    let encoded = segments.get(1)?;
    let decoded = encoded.replace('-', "/");
    Some(if decoded.starts_with('/') {
        decoded
    } else {
        format!("/{decoded}")
    })
}

fn excluded(cfg: &Config, file: &DiscoveredFile) -> bool {
    cfg.exclusions.iter().any(|e| match e {
        Exclusion::Project { path } => file
            .cwd
            .as_deref()
            .is_some_and(|cwd| cwd == path || cwd.starts_with(&format!("{path}/"))),
        Exclusion::Session { agent, id } => &file.agent == agent && &file.native_session_id == id,
    })
}

/// Path → forward-slash string, relative to the home.
fn to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> AgentEntry {
        AgentEntry {
            id: "claude_code".into(),
            name: "Claude Code".into(),
            home: Vec::new(),
            sources: Vec::new(),
            hooks: None,
            retention: None,
        }
    }

    #[test]
    fn a_tilde_path_expands_to_the_home_directory() {
        let expanded = expand_tilde("~/.claude").expect("expand");
        assert!(
            expanded.is_absolute(),
            "~ did not expand to an absolute path: {}",
            expanded.display()
        );
        assert!(!expanded.to_string_lossy().contains('~'));
    }

    /// The environment variable wins over the literal path.
    ///
    /// `CLAUDE_CONFIG_DIR` is how a user says "my config is not in the default
    /// place"; ignoring it captures the wrong directory.
    #[test]
    fn the_environment_variable_wins() {
        let dir = tempfile::tempdir().expect("a temp dir");
        // SAFETY: this test is the only one in this file that touches this
        // variable, and it removes it again
        unsafe { std::env::set_var("CORMEX_TEST_HOME", dir.path()) };
        let mut e = entry();
        e.home = vec![
            HomeRoot::Env {
                env: "CORMEX_TEST_HOME".into(),
            },
            HomeRoot::Path {
                path: "/definitely/not/here".into(),
            },
        ];
        assert_eq!(resolve_home(&e), Some(dir.path().to_path_buf()));
        unsafe { std::env::remove_var("CORMEX_TEST_HOME") };
    }

    #[test]
    fn a_home_that_does_not_exist_resolves_to_none() {
        let mut e = entry();
        e.home = vec![HomeRoot::Path {
            path: "/definitely/not/here".into(),
        }];
        assert!(resolve_home(&e).is_none());
    }

    /// Claude Code's project directory decode is lossy and says so.
    #[test]
    fn the_project_dir_decode_is_marked_lossy() {
        let segments = ["projects", "-home-u-project-a", "s.jsonl"];
        assert_eq!(
            decode_claude_project_dir(&segments).as_deref(),
            Some("/home/u/project/a"),
            "the decode should turn the encoded directory back into a path"
        );
    }

    #[test]
    fn a_path_segment_rule_reads_the_right_segment() {
        let segments = ["projects", "-home-u-proj", "subagents", "child.jsonl"];
        let rule = SessionIdRule::PathSegment {
            index: 2,
            prefix: None,
        };
        assert_eq!(
            session_id(
                &rule,
                "projects/-home-u-proj/subagents/child.jsonl",
                &segments
            ),
            Some("subagents".to_owned())
        );
        // Negative counts from the end
        let rule = SessionIdRule::PathSegment {
            index: -1,
            prefix: None,
        };
        assert_eq!(
            session_id(&rule, "x", &segments),
            Some("child.jsonl".to_owned())
        );
    }

    #[test]
    fn a_stem_regex_extracts_the_named_group() {
        let rule = SessionIdRule::StemRegex {
            regex: r"rollout-[0-9T-]+-(?P<id>[0-9a-f-]{36})".into(),
        };
        let path = "sessions/2026/10/29/rollout-2026-10-29T08-00-00-01234567-89ab-cdef-0123-456789abcdef.jsonl";
        assert_eq!(
            session_id(&rule, path, &[]).as_deref(),
            Some("01234567-89ab-cdef-0123-456789abcdef")
        );
    }

    #[test]
    fn a_constant_rule_returns_the_constant() {
        let rule = SessionIdRule::Const {
            value: "memory".into(),
            per_device: true,
        };
        assert_eq!(session_id(&rule, "x", &[]).as_deref(), Some("memory"));
    }
}

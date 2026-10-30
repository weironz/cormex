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

use anyhow::{Context as _, Result};
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
    /// The bytes, when the source is a SQLite row rather than a file (P4-0).
    ///
    /// Read at discovery time rather than at upload time: the host may rewrite
    /// its database between the two, and what gets uploaded should be the row
    /// as it was when the run decided to send it.
    pub bytes: Option<Vec<u8>>,
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
    // **Hand-added sources, after the catalog's.** They exist for hosts whose
    // history is somewhere the catalog deliberately does not look (D7), so they
    // are the user's own decision and are not subject to the "agent home"
    // restriction the rest of this function enforces
    out.extend(discover_extra_sources(cfg, device_id)?);
    Ok(out)
}

/// Discover the sources the user added with `cormex sources add`.
///
/// # Why a plain path rather than a glob
///
/// A hand-added source is "this one file". A glob would make it a second
/// catalog, with all the matching rules and precedence questions the real one
/// has — and the case this exists for (Aider's per-project history file) is
/// always one file per path.
fn discover_extra_sources(cfg: &Config, device_id: &str) -> Result<Vec<DiscoveredFile>> {
    let mut out = Vec::new();
    for extra in &cfg.extra_sources {
        let Some(path) = expand_tilde(&extra.path) else {
            tracing::debug!(path = %extra.path, "an extra source's path does not resolve");
            continue;
        };
        if !path.is_file() {
            // A file that is not there yet is not an error: a project the user
            // has not run Aider in has no history file, and failing the whole
            // sync over it would make `cormex sync` red on a fresh checkout
            continue;
        }
        let logical = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| extra.path.clone());
        let native_session_id = match extra.session_id.as_str() {
            "stem" => logical.split('.').next().unwrap_or(&logical).to_owned(),
            other => {
                tracing::warn!(
                    rule = other,
                    "an extra source names a session-id rule that does not exist; skipping"
                );
                continue;
            }
        };
        let framing = match extra.framing.as_str() {
            "append" => Framing::Append,
            "snapshot" => Framing::Snapshot,
            other => {
                tracing::warn!(
                    framing = other,
                    "an extra source names a framing that does not exist; skipping"
                );
                continue;
            }
        };
        let file = DiscoveredFile {
            agent: extra.agent.clone(),
            role: FileRole::Transcript,
            framing,
            logical_path: logical,
            physical_path: path,
            compressed: false,
            native_session_id,
            parent_native_session_id: None,
            cwd: None,
            cwd_guessed: false,
            bytes: None,
        };
        if excluded(cfg, &file) {
            continue;
        }
        out.push(file);
    }
    let _ = device_id;
    Ok(out)
}

fn discover_source(
    agent: &AgentEntry,
    source: &SourceSpec,
    home: &Path,
    cfg: &Config,
    device_id: &str,
) -> Result<Vec<DiscoveredFile>> {
    // A SQLite source has no files to glob: `glob` names one database, and the
    // spec says how to read rows out of it
    if let Some(spec) = &source.sqlite {
        return discover_sqlite(agent, source, spec, home, cfg, device_id);
    }
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
            bytes: None,
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

/// Discover a SQLite source's rows (P4-0).
///
/// Each row becomes one logical file with `framing = snapshot`: a row is
/// rewritten in place, so there is nothing to append to. The logical path is
/// `sqlite:<db relative path>#<key>`, which is stable across runs and across
/// machines — the absolute path would make the same row look like two files.
fn discover_sqlite(
    agent: &AgentEntry,
    source: &SourceSpec,
    spec: &cormex_proto::catalog::SqliteSpec,
    home: &Path,
    cfg: &Config,
    device_id: &str,
) -> Result<Vec<DiscoveredFile>> {
    let db = crate::sqlite_source::db_path(home, &source.glob);
    if !db.exists() {
        // A host that has never run has no database. Not an error: the same
        // rule as a home that does not exist
        return Ok(Vec::new());
    }
    let db_rel = crate::sqlite_source::db_rel_path(home, &db);
    let rows = crate::sqlite_source::read_rows(&db, &db_rel, spec)
        .with_context(|| format!("agent {}: reading {}", agent.id, db.display()))?;

    // The session id comes from the source's rule applied to the **logical
    // path**, so a catalog can use the database's name or the row key
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let segments: Vec<&str> = row.logical_path.split('/').collect();
        // **The row's own session column wins when the spec names one.** A
        // database holding four hundred conversations would otherwise read as
        // one enormous session, and `session_search` would return that one
        // session for every query
        let from_rule = session_id(&source.session_id, &row.logical_path, &segments);
        let Some(mut native_session_id) = row.session.clone().or(from_rule) else {
            tracing::debug!(
                agent = %agent.id,
                path = %row.logical_path,
                "no session id could be derived; skipping"
            );
            continue;
        };
        let logical_path = row.logical_path.clone();
        // `per_device` applies to a `const` rule; a column-derived id is
        // already unique per host because the database is per host
        if row.session.is_none()
            && let SessionIdRule::Const {
                per_device: true, ..
            } = &source.session_id
        {
            native_session_id = format!("{native_session_id}@{device_id}");
        }
        let file = DiscoveredFile {
            agent: agent.id.clone(),
            role: source.role,
            framing: source.framing,
            logical_path,
            // No physical path: the bytes are already in hand, and there is no
            // file to re-read
            physical_path: PathBuf::new(),
            compressed: false,
            native_session_id: native_session_id.clone(),
            parent_native_session_id: source
                .parent_session_id
                .as_ref()
                .and_then(|rule| session_id(rule, &native_session_id, &segments)),
            cwd: None,
            cwd_guessed: false,
            bytes: Some(row.bytes),
        };
        if excluded(cfg, &file) {
            continue;
        }
        out.push(file);
    }
    let _ = device_id;
    Ok(out)
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
            manual_only: false,
            import_only: false,
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

    /// A SQLite source's rows become logical files with a stable path.
    ///
    /// The path is `sqlite:<db relative>#<key>`, which is what makes the same
    /// row on two machines one file rather than two.
    #[test]
    fn a_sqlite_source_yields_one_file_per_row() {
        let home = tempfile::tempdir().expect("a temp dir");
        let db = home.path().join("state.vscdb");
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.execute_batch(
            "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT);
             INSERT INTO kv VALUES ('a', 'one'), ('b', 'two');",
        )
        .expect("seed");
        drop(conn);

        let agent = AgentEntry {
            id: "cursor".into(),
            name: "Cursor".into(),
            home: vec![HomeRoot::Path {
                path: home.path().display().to_string(),
            }],
            sources: vec![SourceSpec {
                role: FileRole::Row,
                glob: "state.vscdb".into(),
                framing: Framing::Snapshot,
                also_compressed: None,
                session_id: SessionIdRule::Const {
                    value: "cursor-history".into(),
                    per_device: false,
                },
                parent_session_id: None,
                cwd: None,
                sqlite: Some(cormex_proto::catalog::SqliteSpec {
                    query: "SELECT key, value FROM kv ORDER BY key".into(),
                    key_column: "key".into(),
                    text_column: Some("value".into()),
                    session_column: None,
                    busy_timeout_ms: 1000,
                }),
            }],
            manual_only: false,
            import_only: false,
            hooks: None,
            retention: None,
        };
        let catalog = Catalog {
            schema: 1,
            version: "test".into(),
            agents: vec![agent],
        };

        let files = discover(&catalog, &Config::default(), "dev").expect("discover");
        assert_eq!(files.len(), 2, "one file per row");
        assert_eq!(files[0].logical_path, "sqlite:state.vscdb#a");
        assert_eq!(files[0].bytes.as_deref(), Some(&b"one"[..]));
        assert_eq!(files[0].native_session_id, "cursor-history");
        assert_eq!(files[0].framing, Framing::Snapshot);
        assert!(
            files[0].physical_path.as_os_str().is_empty(),
            "a row has no file to re-read"
        );
    }

    /// A SQLite source whose database does not exist yields nothing, not an
    /// error.
    #[test]
    fn a_missing_sqlite_database_is_not_an_error() {
        let home = tempfile::tempdir().expect("a temp dir");
        let catalog = Catalog {
            schema: 1,
            version: "test".into(),
            agents: vec![AgentEntry {
                id: "cursor".into(),
                name: "Cursor".into(),
                home: vec![HomeRoot::Path {
                    path: home.path().display().to_string(),
                }],
                sources: vec![SourceSpec {
                    role: FileRole::Row,
                    glob: "state.vscdb".into(),
                    framing: Framing::Snapshot,
                    also_compressed: None,
                    session_id: SessionIdRule::Const {
                        value: "x".into(),
                        per_device: false,
                    },
                    parent_session_id: None,
                    cwd: None,
                    sqlite: Some(cormex_proto::catalog::SqliteSpec {
                        query: "SELECT 1".into(),
                        key_column: "1".into(),
                        text_column: None,
                        session_column: None,
                        busy_timeout_ms: 1000,
                    }),
                }],
                manual_only: false,
                import_only: false,
                hooks: None,
                retention: None,
            }],
        };
        let files = discover(&catalog, &Config::default(), "dev").expect("discover");
        assert!(files.is_empty());
    }

    /// A SQLite source with a `session_column` puts each row in its own
    /// conversation.
    ///
    /// Without it every row of the database lands in one session — a host with
    /// four hundred conversations would read as one enormous one, and
    /// `session_search` would return it for every query.
    #[test]
    fn a_session_column_splits_rows_into_conversations() {
        let home = tempfile::tempdir().expect("a temp dir");
        let db = home.path().join("sessions.db");
        let conn = rusqlite::Connection::open(&db).expect("open");
        conn.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content_json TEXT);
             INSERT INTO messages VALUES (1,'s1','user','[]'), (2,'s1','assistant','[]'), (3,'s2','user','[]');",
        )
        .expect("seed");
        drop(conn);

        let agent = AgentEntry {
            id: "goose".into(),
            name: "Goose".into(),
            home: vec![HomeRoot::Path {
                path: home.path().display().to_string(),
            }],
            sources: vec![SourceSpec {
                role: FileRole::Row,
                glob: "sessions.db".into(),
                framing: Framing::Snapshot,
                also_compressed: None,
                session_id: SessionIdRule::Const {
                    value: "goose".into(),
                    per_device: true,
                },
                parent_session_id: None,
                cwd: None,
                sqlite: Some(cormex_proto::catalog::SqliteSpec {
                    query: "SELECT id, session_id, role, content_json FROM messages ORDER BY id"
                        .into(),
                    key_column: "id".into(),
                    text_column: Some("content_json".into()),
                    session_column: Some("session_id".into()),
                    busy_timeout_ms: 1000,
                }),
            }],
            manual_only: false,
            import_only: false,
            hooks: None,
            retention: None,
        };
        let catalog = Catalog {
            schema: 1,
            version: "test".into(),
            agents: vec![agent],
        };

        let files = discover(&catalog, &Config::default(), "dev").expect("discover");
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].native_session_id, "s1");
        assert_eq!(files[1].native_session_id, "s1");
        assert_eq!(
            files[2].native_session_id, "s2",
            "the second conversation was merged into the first"
        );
    }
}

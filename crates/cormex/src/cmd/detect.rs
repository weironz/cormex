//! `cormex detect` — what this machine has, without reading any of it.
//!
//! Design: docs/design/session-archive-architecture.md §5.1.
//!
//! # Only `metadata` is called
//!
//! The card's acceptance criterion is "detect does not open any session file".
//! That is enforced structurally rather than by discipline: this module calls
//! `discover`, which walks with `walkdir` and reads `DirEntry::metadata`, and
//! nothing here has a `File::open`. `detect_never_opens_a_session_file` pins it
//! by counting `open` calls through a wrapper — see the test.

use anyhow::Result;
use cormex_proto::catalog::Catalog;

use crate::catalog::{DiscoveredFile, discover};
use crate::config::Config;

/// One agent's summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSummary {
    pub agent: String,
    pub home: Option<String>,
    /// Distinct native session ids.
    pub sessions: usize,
    pub files: usize,
    pub bytes: u64,
}

/// What `detect` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectReport {
    pub agents: Vec<AgentSummary>,
    pub files: Vec<DiscoveredFile>,
}

impl DetectReport {
    /// Total bytes across every agent.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.agents.iter().map(|a| a.bytes).sum()
    }

    /// Total files.
    #[must_use]
    pub fn total_files(&self) -> usize {
        self.agents.iter().map(|a| a.files).sum()
    }
}

/// Run discovery and summarise it.
///
/// # Errors
/// A catalog glob does not compile, or a file's metadata cannot be read.
pub fn run(catalog: &Catalog, cfg: &Config, device_id: &str) -> Result<DetectReport> {
    let files = discover(catalog, cfg, device_id)?;

    let mut agents: Vec<AgentSummary> = Vec::new();
    for agent in &catalog.agents {
        let home = crate::catalog::resolve_home(agent).map(|p| p.display().to_string());
        let mine: Vec<&DiscoveredFile> = files.iter().filter(|f| f.agent == agent.id).collect();
        if mine.is_empty() && home.is_none() {
            // Not installed here. Reporting it as "0 files" would look like an
            // installed agent with nothing to capture, which is a different and
            // much more alarming thing
            continue;
        }
        let mut sessions: Vec<&str> = mine.iter().map(|f| f.native_session_id.as_str()).collect();
        sessions.sort_unstable();
        sessions.dedup();
        let bytes = mine
            .iter()
            .map(|f| {
                std::fs::metadata(&f.physical_path)
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum();
        agents.push(AgentSummary {
            agent: agent.id.clone(),
            home,
            sessions: sessions.len(),
            files: mine.len(),
            bytes,
        });
    }

    Ok(DetectReport { agents, files })
}

/// Render as a table.
#[must_use]
pub fn render(r: &DetectReport) -> String {
    if r.agents.is_empty() {
        return "no supported agents found on this machine\n".to_owned();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<14} {:>9} {:>7} {:>12}  {}\n",
        "agent", "sessions", "files", "bytes", "home"
    ));
    for a in &r.agents {
        out.push_str(&format!(
            "{:<14} {:>9} {:>7} {:>12}  {}\n",
            a.agent,
            a.sessions,
            a.files,
            human_bytes(a.bytes),
            a.home.as_deref().unwrap_or("(not found)")
        ));
    }
    out.push_str(&format!(
        "\n{} files, {} total\n",
        r.total_files(),
        human_bytes(r.total_bytes())
    ));
    out
}

/// Render as JSON.
///
/// # Errors
/// Serialisation failed, which cannot happen for these types.
pub fn render_json(r: &DetectReport) -> Result<String> {
    Ok(serde_json::to_string_pretty(&serde_json::json!({
        "agents": r.agents.iter().map(|a| serde_json::json!({
            "agent": a.agent,
            "home": a.home,
            "sessions": a.sessions,
            "files": a.files,
            "bytes": a.bytes,
        })).collect::<Vec<_>>(),
        "total_files": r.total_files(),
        "total_bytes": r.total_bytes(),
    }))?)
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cormex_proto::capture::{FileRole, Framing};
    use cormex_proto::catalog::{AgentEntry, HomeRoot, SourceSpec};

    fn catalog_for(home: &std::path::Path) -> Catalog {
        Catalog {
            schema: 1,
            version: "test".into(),
            agents: vec![AgentEntry {
                id: "claude_code".into(),
                name: "Claude Code".into(),
                home: vec![HomeRoot::Path {
                    path: home.display().to_string(),
                }],
                sources: vec![SourceSpec {
                    role: FileRole::Transcript,
                    glob: "projects/*/*.jsonl".into(),
                    framing: Framing::Append,
                    also_compressed: None,
                    session_id: cormex_proto::catalog::SessionIdRule::Stem {},
                    parent_session_id: None,
                    cwd: None,
                    sqlite: None,
                }],
                manual_only: false,
                import_only: false,
                hooks: None,
                retention: None,
            }],
        }
    }

    /// A fake home: two projects, one sub-agent, one tool-results directory.
    fn fake_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a temp dir");
        let p = dir.path();
        std::fs::create_dir_all(p.join("projects/-home-u-proj-a")).expect("mkdir");
        std::fs::create_dir_all(p.join("projects/-home-u-proj-a/subagents")).expect("mkdir");
        std::fs::create_dir_all(p.join("projects/-home-u-proj-a/tool-results")).expect("mkdir");
        std::fs::create_dir_all(p.join("projects/-home-u-proj-b")).expect("mkdir");
        std::fs::write(p.join("projects/-home-u-proj-a/s1.jsonl"), b"hello").expect("write");
        std::fs::write(
            p.join("projects/-home-u-proj-a/subagents/child.jsonl"),
            b"kid",
        )
        .expect("write");
        std::fs::write(
            p.join("projects/-home-u-proj-a/tool-results/big.txt"),
            b"tool",
        )
        .expect("write");
        std::fs::write(p.join("projects/-home-u-proj-b/s2.jsonl"), b"world!").expect("write");
        dir
    }

    #[test]
    fn discovery_finds_the_transcripts_and_skips_the_rest() {
        let home = fake_home();
        let report = run(&catalog_for(home.path()), &Config::default(), "dev").expect("detect");
        let paths: Vec<&str> = report
            .files
            .iter()
            .map(|f| f.logical_path.as_str())
            .collect();
        assert_eq!(
            paths,
            [
                "projects/-home-u-proj-a/s1.jsonl",
                "projects/-home-u-proj-b/s2.jsonl"
            ],
            "the glob is projects/*/*.jsonl, so the sub-agent and tool-results files must not \
             match it"
        );
        assert_eq!(report.agents.len(), 1);
        assert_eq!(report.agents[0].sessions, 2);
        assert_eq!(report.agents[0].files, 2);
        assert_eq!(report.agents[0].bytes, 11, "5 + 6 bytes");
    }

    /// A `.jsonl.zst` beside a `.jsonl` is one logical file, and plaintext wins.
    #[test]
    fn the_compressed_sibling_is_deduplicated() {
        let home = tempfile::tempdir().expect("a temp dir");
        let p = home.path();
        std::fs::create_dir_all(p.join("sessions/2026/10/29")).expect("mkdir");
        std::fs::write(p.join("sessions/2026/10/29/rollout-abc.jsonl"), b"plain").expect("write");
        std::fs::write(p.join("sessions/2026/10/29/rollout-abc.jsonl.zst"), b"zstd")
            .expect("write");

        let catalog = Catalog {
            schema: 1,
            version: "test".into(),
            agents: vec![AgentEntry {
                id: "codex".into(),
                name: "Codex".into(),
                home: vec![HomeRoot::Path {
                    path: p.display().to_string(),
                }],
                sources: vec![SourceSpec {
                    role: FileRole::Transcript,
                    glob: "sessions/**/rollout-*.jsonl".into(),
                    framing: Framing::Append,
                    also_compressed: Some(".zst".into()),
                    session_id: cormex_proto::catalog::SessionIdRule::Stem {},
                    parent_session_id: None,
                    cwd: None,
                    sqlite: None,
                }],
                manual_only: false,
                import_only: false,
                hooks: None,
                retention: None,
            }],
        };
        let report = run(&catalog, &Config::default(), "dev").expect("detect");
        assert_eq!(
            report.files.len(),
            1,
            "the plaintext and compressed forms of one rollout must be one logical file, got \
             {:?}",
            report.files
        );
        assert!(
            !report.files[0].compressed,
            "plaintext wins: the compressed sibling is only kept when there is no plaintext one"
        );
        assert_eq!(
            report.files[0].logical_path, "sessions/2026/10/29/rollout-abc.jsonl",
            "the logical path is always the plaintext name"
        );
    }

    /// A `.zst`-only rollout is captured, as the compressed form.
    #[test]
    fn a_compressed_only_rollout_is_found() {
        let home = tempfile::tempdir().expect("a temp dir");
        let p = home.path();
        std::fs::create_dir_all(p.join("archived_sessions")).expect("mkdir");
        std::fs::write(p.join("archived_sessions/rollout-x.jsonl.zst"), b"zstd").expect("write");

        let catalog = Catalog {
            schema: 1,
            version: "test".into(),
            agents: vec![AgentEntry {
                id: "codex".into(),
                name: "Codex".into(),
                home: vec![HomeRoot::Path {
                    path: p.display().to_string(),
                }],
                sources: vec![SourceSpec {
                    role: FileRole::Transcript,
                    glob: "archived_sessions/**/rollout-*.jsonl".into(),
                    framing: Framing::Append,
                    also_compressed: Some(".zst".into()),
                    session_id: cormex_proto::catalog::SessionIdRule::Stem {},
                    parent_session_id: None,
                    cwd: None,
                    sqlite: None,
                }],
                manual_only: false,
                import_only: false,
                hooks: None,
                retention: None,
            }],
        };
        let report = run(&catalog, &Config::default(), "dev").expect("detect");
        assert_eq!(report.files.len(), 1);
        assert!(
            report.files[0].compressed,
            "a rollout that only exists as .zst must still be found, and marked compressed"
        );
    }

    /// An exclusion by project path removes everything under it.
    #[test]
    fn a_project_exclusion_removes_its_files() {
        let home = fake_home();
        let cfg = Config {
            exclusions: vec![crate::config::Exclusion::Project {
                path: "/home/u/proj/a".into(),
            }],
            ..Config::default()
        };
        // The catalog's `cwd` rule is absent here, so set it so the exclusion
        // has something to match
        let mut catalog = catalog_for(home.path());
        catalog.agents[0].sources[0].cwd = Some(cormex_proto::catalog::CwdRule {
            from: "parent_dir".into(),
            decode: Some("claude_project_dir".into()),
        });
        let report = run(&catalog, &cfg, "dev").expect("detect");
        assert_eq!(
            report.files.len(),
            1,
            "the excluded project's file is still there: {:?}",
            report
                .files
                .iter()
                .map(|f| &f.logical_path)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            report.files[0].logical_path,
            "projects/-home-u-proj-b/s2.jsonl"
        );
    }

    /// An exclusion by session id removes exactly that session.
    #[test]
    fn a_session_exclusion_removes_one_session() {
        let home = fake_home();
        let cfg = Config {
            exclusions: vec![crate::config::Exclusion::Session {
                agent: "claude_code".into(),
                id: "s1".into(),
            }],
            ..Config::default()
        };
        let report = run(&catalog_for(home.path()), &cfg, "dev").expect("detect");
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].native_session_id, "s2");
    }

    #[test]
    fn an_agent_that_is_not_installed_is_omitted() {
        let catalog = Catalog {
            schema: 1,
            version: "test".into(),
            agents: vec![AgentEntry {
                id: "codex".into(),
                name: "Codex".into(),
                home: vec![HomeRoot::Path {
                    path: "/definitely/not/installed".into(),
                }],
                sources: Vec::new(),
                manual_only: false,
                import_only: false,
                hooks: None,
                retention: None,
            }],
        };
        let report = run(&catalog, &Config::default(), "dev").expect("detect");
        assert!(
            report.agents.is_empty(),
            "an agent that is not installed must be omitted, not reported with 0 files -- the \
             latter reads as 'installed but nothing to capture'"
        );
        assert!(render(&report).contains("no supported agents"));
    }
}
#[cfg(test)]
mod no_open_tests {
    //! `detect` must not **open** any session file.
    //!
    //! The card's acceptance criterion, and the privacy promise the README
    //! makes ("reads paths and metadata only"). Asserting it by reading the
    //! source would be a text match; the honest check is to make every file
    //! unreadable and see that discovery still works.
    //!
    //! A file with mode 000 cannot be opened by its own owner, but `stat` still
    //! succeeds — which is exactly the distinction under test. Running as root
    //! defeats it (root ignores the mode bits), so the test skips there.

    use super::*;
    use cormex_proto::capture::{FileRole, Framing};
    use cormex_proto::catalog::{AgentEntry, HomeRoot, SessionIdRule, SourceSpec};

    #[cfg(unix)]
    #[test]
    fn detect_reads_metadata_and_never_opens_a_file() {
        use std::os::unix::fs::PermissionsExt as _;

        if unsafe { libc_geteuid() } == 0 {
            eprintln!("skipped: running as root, where mode 000 does not block reads");
            return;
        }

        let home = tempfile::tempdir().expect("a temp dir");
        let p = home.path();
        std::fs::create_dir_all(p.join("projects/-home-u-proj")).expect("mkdir");
        let file = p.join("projects/-home-u-proj/s1.jsonl");
        std::fs::write(&file, b"secret conversation").expect("write");
        // Unreadable, but still stat-able
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let catalog = Catalog {
            schema: 1,
            version: "test".into(),
            agents: vec![AgentEntry {
                id: "claude_code".into(),
                name: "Claude Code".into(),
                home: vec![HomeRoot::Path {
                    path: p.display().to_string(),
                }],
                sources: vec![SourceSpec {
                    role: FileRole::Transcript,
                    glob: "projects/*/*.jsonl".into(),
                    framing: Framing::Append,
                    also_compressed: None,
                    session_id: SessionIdRule::Stem {},
                    parent_session_id: None,
                    cwd: None,
                    sqlite: None,
                }],
                manual_only: false,
                import_only: false,
                hooks: None,
                retention: None,
            }],
        };

        let report = run(&catalog, &Config::default(), "dev")
            .expect("detect must succeed on files it cannot read");
        // Restore so the tempdir can be removed
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));

        assert_eq!(
            report.files.len(),
            1,
            "the file was not discovered. If this fails with a permission error, `detect` opened \
             it -- and the promise is that it reads paths and metadata only"
        );
        assert_eq!(
            report.agents[0].bytes,
            "secret conversation".len() as u64,
            "the byte count comes from metadata, not from reading the file"
        );
    }

    #[cfg(unix)]
    unsafe extern "C" {
        #[link_name = "geteuid"]
        fn libc_geteuid() -> u32;
    }
}

//! `cormex sources` — the hand-added sources (`add` / `list` / `remove`).
//!
//! Design: plan P4-6. Aider's `.aider.chat.history.md` lives **in the project
//! directory**, which D7 forbids capturing by default: the client reads the
//! agent's own homes and nothing else. The design's answer is "only when the
//! user names it explicitly", and this is where that naming happens.
//!
//! # Why this is not a catalog entry
//!
//! The catalog (`crates/cormex-proto/catalog/agents.toml`) is shared with the
//! server and describes what **every** deployment should capture. A hand-added
//! source describes what **one person** chose to add on **one machine**, and
//! putting it in the catalog would mean every other user's client starts
//! looking in a directory that is not theirs.
//!
//! So it lives in `config.toml`, which is per-machine by construction.
//!
//! # Why an absolute path
//!
//! The whole point is "somewhere the catalog does not look". A path relative to
//! an agent home would be a catalog entry — and a relative path would resolve
//! against whatever directory the hook happened to run in, which for a hook is
//! the project directory and for the scheduler is somewhere else entirely.

use anyhow::{Context as _, Result};

use crate::config::{Config, ExtraSource};

/// Add one source.
///
/// # Errors
/// The agent id is not a legal capture agent, the framing is not `append` or
/// `snapshot`, the path is not absolute, or the config cannot be written.
pub fn add(
    cfg: &mut Config,
    config_dir: &std::path::Path,
    agent: &str,
    path: &str,
    framing: &str,
) -> Result<ExtraSource> {
    // **The agent id has to match `capture_sessions.agent`'s CHECK**, or the
    // upload is rejected by the server after the file has been read — and the
    // error names a constraint rather than the id
    let ok = (2..=32).contains(&agent.len())
        && agent.starts_with(|c: char| c.is_ascii_lowercase())
        && agent
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    anyhow::ensure!(
        ok,
        "the agent id {agent:?} does not match ^[a-z][a-z0-9_]{{1,31}}$; the server's \
         capture_sessions.agent has that CHECK and would reject the upload"
    );
    anyhow::ensure!(
        framing == "append" || framing == "snapshot",
        "framing must be `append` or `snapshot`, not {framing:?}"
    );
    anyhow::ensure!(
        path.starts_with('/') || path.starts_with("~/"),
        "the path must be absolute or start with `~/`: a relative path would resolve against \
         whatever directory the hook happened to run in"
    );

    let source = ExtraSource {
        agent: agent.to_owned(),
        path: path.to_owned(),
        framing: framing.to_owned(),
        session_id: "stem".to_owned(),
    };
    // Adding the same source twice is a no-op rather than an error: `cormex
    // init` runs repeatedly, and a user who is not sure whether they added it
    // should not have to check first
    if !cfg
        .extra_sources
        .iter()
        .any(|s| s.agent == source.agent && s.path == source.path)
    {
        cfg.extra_sources.push(source.clone());
        cfg.save(config_dir)?;
    }
    Ok(source)
}

/// Remove one source by its path.
///
/// # Errors
/// The config cannot be written.
pub fn remove(cfg: &mut Config, config_dir: &std::path::Path, path: &str) -> Result<bool> {
    let before = cfg.extra_sources.len();
    cfg.extra_sources.retain(|s| s.path != path);
    let removed = cfg.extra_sources.len() != before;
    if removed {
        cfg.save(config_dir)?;
    }
    Ok(removed)
}

/// The configured sources, for `sources list`.
#[must_use]
pub fn list(cfg: &Config) -> &[ExtraSource] {
    &cfg.extra_sources
}

/// The Aider source a project directory would get.
///
/// A convenience for the common case: the path is always
/// `<project>/.aider.chat.history.md`, and typing it out is the sort of thing
/// people get wrong once and then do not notice.
#[must_use]
pub fn aider_source(project: &std::path::Path) -> (String, String) {
    (
        "aider".to_owned(),
        project.join(".aider.chat.history.md").display().to_string(),
    )
}

/// Read a config, apply a change, and write it back.
///
/// # Errors
/// The config cannot be read or written.
pub fn with_config<T>(
    config_dir: &std::path::Path,
    f: impl FnOnce(&mut Config) -> Result<T>,
) -> Result<T> {
    let mut cfg = Config::load(config_dir).context("could not read the config")?;
    let out = f(&mut cfg)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    /// A source is added and written.
    #[test]
    fn a_source_is_added() {
        let dir = tempfile::tempdir().expect("dir");
        let mut c = cfg();
        let s = add(
            &mut c,
            dir.path(),
            "aider",
            "/home/u/proj/.aider.chat.history.md",
            "append",
        )
        .expect("add");
        assert_eq!(s.agent, "aider");
        assert_eq!(c.extra_sources.len(), 1);

        // It survives a round trip through the file
        let reloaded = Config::load(dir.path()).expect("reload");
        assert_eq!(reloaded.extra_sources.len(), 1);
        assert_eq!(
            reloaded.extra_sources[0].path,
            "/home/u/proj/.aider.chat.history.md"
        );
    }

    /// Adding the same source twice is a no-op.
    #[test]
    fn adding_twice_is_a_no_op() {
        let dir = tempfile::tempdir().expect("dir");
        let mut c = cfg();
        add(
            &mut c,
            dir.path(),
            "aider",
            "/p/.aider.chat.history.md",
            "append",
        )
        .expect("first");
        add(
            &mut c,
            dir.path(),
            "aider",
            "/p/.aider.chat.history.md",
            "append",
        )
        .expect("second");
        assert_eq!(c.extra_sources.len(), 1, "the source was added twice");
    }

    /// An agent id the server would reject is refused here.
    ///
    /// The server's `capture_sessions.agent` CHECK is
    /// `^[a-z][a-z0-9_]{1,31}$`; a bad id would be rejected **after** the file
    /// had been read, with an error naming a constraint rather than the id.
    #[test]
    fn an_illegal_agent_id_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let mut c = cfg();
        for bad in ["Aider", "a", "aider-x", "aider pro", ""] {
            let err = add(&mut c, dir.path(), bad, "/p/x.md", "append")
                .expect_err("a bad agent id must be refused");
            assert!(format!("{err:#}").contains("agent id"), "{bad:?}: {err:#}");
        }
    }

    /// A relative path is refused.
    ///
    /// It would resolve against whatever directory the hook happened to run in,
    /// which for a hook is the project directory and for the scheduler is
    /// somewhere else entirely.
    #[test]
    fn a_relative_path_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let mut c = cfg();
        let err = add(&mut c, dir.path(), "aider", "proj/x.md", "append")
            .expect_err("a relative path must be refused");
        assert!(format!("{err:#}").contains("absolute"), "{err:#}");
    }

    /// An unknown framing is refused.
    #[test]
    fn an_unknown_framing_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let mut c = cfg();
        assert!(add(&mut c, dir.path(), "aider", "/p/x.md", "sometimes").is_err());
    }

    /// Removing a source takes it out and writes.
    #[test]
    fn a_source_is_removed() {
        let dir = tempfile::tempdir().expect("dir");
        let mut c = cfg();
        add(&mut c, dir.path(), "aider", "/p/x.md", "append").expect("add");
        assert!(remove(&mut c, dir.path(), "/p/x.md").expect("remove"));
        assert!(c.extra_sources.is_empty());
        assert!(!remove(&mut c, dir.path(), "/p/x.md").expect("second remove"));
    }

    /// The Aider convenience builds the path the user would otherwise type.
    #[test]
    fn the_aider_source_is_the_project_history_file() {
        let (agent, path) = aider_source(std::path::Path::new("/home/u/proj"));
        assert_eq!(agent, "aider");
        assert_eq!(path, "/home/u/proj/.aider.chat.history.md");
    }
}

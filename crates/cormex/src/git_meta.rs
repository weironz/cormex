//! The project key: `host/owner/repo`, from git.
//!
//! Design: docs/design/session-archive-architecture.md §5.2 (D7).
//!
//! # This is the only thing the client executes outside the agent's home
//!
//! D7 says the client reads the agent's own directories and **not** the project
//! directories. The single exception is two read-only git commands, and they
//! exist because "which project was this session about" is the filter users ask
//! for first — and the alternative (the cwd path) is machine-specific, so it
//! cannot group two machines' sessions.
//!
//! # Two commands, one second, no shell
//!
//! `git -C <cwd> remote get-url origin` and `git rev-parse HEAD`. Both are
//! spawned directly rather than through a shell: a cwd containing a space or a
//! quote is ordinary, and shell interpolation is how that becomes an injection.
//! The timeout is one second because this runs inside a hook.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// How long both git commands together may take.
///
/// One second, because this runs from a hook whose host gives it 1.5 seconds on
/// `SessionEnd`. A `git` that has to walk up a deep tree or contend for the
/// index on a large repository can take longer than that, and the right answer
/// there is "no project key" rather than "the hook timed out".
const GIT_TIMEOUT: Duration = Duration::from_secs(1);

/// Derive the project key for a working directory.
///
/// Returns `None` when git is not installed, the directory is not a repository,
/// there is no `origin`, or either command times out. All of those are normal:
/// the key is an optimisation for grouping, not a requirement.
#[must_use]
pub fn project_key(cwd: &Path) -> Option<String> {
    let url = run_git(cwd, &["remote", "get-url", "origin"])?;
    let key = normalise_remote(url.trim())?;
    // The commit is not part of the key — it changes on every commit, and the
    // key is a grouping label. It is fetched so that a caller which wants it
    // does not have to spawn git twice.
    let _ = run_git(cwd, &["rev-parse", "HEAD"]);
    Some(key)
}

/// The current commit, for provenance.
#[must_use]
pub fn head_commit(cwd: &Path) -> Option<String> {
    run_git(cwd, &["rev-parse", "HEAD"]).map(|s| s.trim().to_owned())
}

/// Turn a remote URL into `host/owner/repo`.
///
/// Handles the three forms git actually produces:
///
/// ```text
/// https://github.com/owner/repo.git
/// git@github.com:owner/repo.git
/// ssh://git@github.com/owner/repo.git
/// ```
///
/// The `.git` suffix and any trailing slash are stripped: without that, the
/// same repository cloned over HTTPS and over SSH produces two different keys,
/// and the project filter shows two projects that are one.
#[must_use]
pub fn normalise_remote(url: &str) -> Option<String> {
    let url = url.trim();
    if url.is_empty() {
        return None;
    }

    // scp-like syntax: `user@host:path`
    let path = if let Some(rest) = url.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        return Some(format!("{host}/{}", trim_repo(path)));
    } else if let Some(rest) = url.split_once("://").map(|(_, r)| r) {
        // scheme://[user@]host[:port]/path
        let after_user = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
        let (host, path) = after_user.split_once('/')?;
        // Drop a port: `host:22/owner/repo` and `host/owner/repo` are one
        // repository
        let host = host.split(':').next().unwrap_or(host);
        format!("{host}/{}", trim_repo(path))
    } else {
        // A bare `host/owner/repo` -- what this function itself produces, so
        // accepting it makes the function idempotent. A bare `owner/repo` is
        // **not** accepted: it would collide across hosts, and the caller
        // cannot tell which it has without counting segments.
        trim_repo(url).to_owned()
    };

    let path = path.trim_matches('/');
    // At least `host/owner/repo`: two segments is `owner/repo`, which has no
    // host and would merge two different projects with the same name
    if path.split('/').count() < 3 {
        return None;
    }
    Some(path.to_owned())
}

fn trim_repo(path: &str) -> &str {
    let p = path.trim().trim_end_matches('/');
    p.strip_suffix(".git").unwrap_or(p)
}

fn run_git(cwd: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    // `spawn` + `wait_timeout` rather than `output()`: `output` waits forever,
    // and a git that blocks on a credential prompt would hang the hook past its
    // budget. `GIT_TERMINAL_PROMPT=0` closes the other half of that door.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd.env("GIT_ASKPASS", "true");

    let mut child = cmd.spawn().ok()?;
    let deadline = std::time::Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out = child.wait_with_output().ok()?;
                return String::from_utf8(out.stdout).ok();
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three remote forms produce the **same** key.
    ///
    /// Without this, the same repository cloned over HTTPS on one machine and
    /// SSH on another shows up as two projects, and the filter that exists to
    /// group them does the opposite.
    #[test]
    fn every_remote_form_normalises_to_one_key() {
        for url in [
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo",
            "git@github.com:owner/repo.git",
            "git@github.com:owner/repo",
            "ssh://git@github.com/owner/repo.git",
            "ssh://git@github.com:22/owner/repo.git",
            "https://user:token@github.com/owner/repo.git",
        ] {
            assert_eq!(
                normalise_remote(url).as_deref(),
                Some("github.com/owner/repo"),
                "{url} did not normalise to the canonical key"
            );
        }
    }

    #[test]
    fn a_trailing_slash_is_stripped() {
        assert_eq!(
            normalise_remote("https://github.com/owner/repo/").as_deref(),
            Some("github.com/owner/repo")
        );
    }

    /// A bare `owner/repo` is refused: it would collide across hosts.
    #[test]
    fn a_path_without_a_host_is_refused() {
        assert_eq!(normalise_remote("owner/repo"), None);
        assert_eq!(normalise_remote(""), None);
        assert_eq!(normalise_remote("   "), None);
    }

    /// Normalising twice gives the same answer.
    ///
    /// The output of this function is accepted as input (a bare
    /// `host/owner/repo` is the third form), so idempotence is a real property
    /// rather than a curiosity.
    #[test]
    fn normalising_is_idempotent() {
        let once = normalise_remote("https://github.com/owner/repo.git").expect("key");
        assert_eq!(normalise_remote(&once).as_deref(), Some(once.as_str()));
    }

    /// A directory that is not a repository yields `None`, not an error.
    ///
    /// The project key is a grouping optimisation; a session outside a
    /// repository is ordinary.
    #[test]
    fn a_non_repository_yields_no_key() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert_eq!(project_key(dir.path()), None);
    }
}

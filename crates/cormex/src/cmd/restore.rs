//! `cormex sessions restore` — put a captured session back on disk.
//!
//! Design: docs/plan/session-archive-plan.md P3-5.
//!
//! # Why this exists at all
//!
//! The host deletes its own sessions after `cleanupPeriodDays`. Once it has,
//! Cormex holds the only copy — and "the only copy" is worth nothing if it
//! cannot be put back where the host will read it. After a restore,
//! `claude --resume <id>` works exactly as it did before the deletion, because
//! the bytes are the bytes.
//!
//! # The one thing it does not rewrite
//!
//! A transcript records the `cwd` it ran in. Restoring into a different
//! directory leaves that field alone: the restored file is a **derivative**, and
//! rewriting the recorded history to match the new location would make it
//! disagree with what actually happened. `--cwd` changes where the file
//! **lands**, not what it says.

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::client::ApiClient;

/// What a restore did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub files: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
}

/// Restore a session into `home`.
///
/// # Errors
/// The server is unreachable, the session has no files, an existing file is in
/// the way and `force` is false, or a write failed.
pub async fn run(
    client: &ApiClient,
    session_id: &str,
    home: &Path,
    cwd: Option<&str>,
    force: bool,
) -> Result<RestoreReport> {
    // The session's metadata says which agent wrote it, and that decides the
    // directory layout
    let detail: cormex_proto::archive::ArchiveSessionDetail = client
        .get_json(&format!("/archive/sessions/{}", urlencode(session_id)))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let bytes = client
        .get_bytes(
            &format!("/archive/sessions/{}/export", urlencode(session_id)),
            &[],
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let tar = zstd::decode_all(bytes.as_slice()).context("the export is not a zstd frame")?;
    let mut archive = tar::Archive::new(tar.as_slice());

    let mut report = RestoreReport::default();
    for entry in archive.entries().context("could not read the tar")? {
        let mut entry = entry.context("could not read a tar entry")?;
        let logical = entry
            .path()
            .context("a tar entry has no path")?
            .to_string_lossy()
            .into_owned();

        let target = destination(home, &detail.session.agent, &logical, cwd, session_id);

        if target.exists() && !force {
            // **Refuse rather than overwrite.** The file on disk may be newer
            // than the one being restored, and a restore that silently replaced
            // it would be the one operation here that loses data
            report.skipped.push(target);
            continue;
        }

        let mut contents = Vec::new();
        entry
            .read_to_end(&mut contents)
            .context("could not read a tar entry's bytes")?;

        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("could not create {}", dir.display()))?;
        }
        // Temp file + rename: a half-written transcript is one the host will
        // read and choke on, and the failure would look like a corrupt session
        // rather than an interrupted restore
        let tmp = target.with_extension("cormex-tmp");
        std::fs::write(&tmp, &contents)
            .with_context(|| format!("could not write {}", tmp.display()))?;
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("could not rename into {}", target.display()))?;
        report.files.push(target);
    }

    Ok(report)
}

/// Where one logical path lands in the host's home.
///
/// # Claude Code
///
/// `<home>/projects/<encoded cwd>/<session>.jsonl`, where the encoding replaces
/// `/` with `-`. That is the host's own rule, and getting it wrong means the
/// session is on disk but `--resume` cannot find it.
///
/// A sub-agent's file lives under the parent's directory, and a sidecar under
/// `tool-results/`; both keep their relative position, so the layout is
/// reproduced rather than guessed.
///
/// # Codex
///
/// `sessions/YYYY/MM/DD/…`, which is the logical path already.
#[must_use]
pub fn destination(
    home: &Path,
    agent: &str,
    logical_path: &str,
    cwd: Option<&str>,
    session_id: &str,
) -> PathBuf {
    match agent {
        "claude_code" => {
            // The logical path is `projects/<encoded>/<rest>`; the encoded
            // directory is the one the host derived from the cwd. With `--cwd`
            // the caller wants the file under a **different** project
            // directory, so the segment is recomputed and the rest kept
            let rest = logical_path
                .strip_prefix("projects/")
                .and_then(|r| r.split_once('/').map(|(_, tail)| tail))
                .unwrap_or(logical_path);
            let dir = match cwd {
                Some(cwd) => encode_claude_project_dir(cwd),
                None => logical_path
                    .strip_prefix("projects/")
                    .and_then(|r| r.split_once('/').map(|(head, _)| head.to_owned()))
                    .unwrap_or_else(|| encode_claude_project_dir(session_id)),
            };
            home.join("projects").join(dir).join(rest)
        }
        // Codex's logical path is already `sessions/YYYY/MM/DD/rollout-….jsonl`
        _ => home.join(logical_path),
    }
}

/// Claude Code's project-directory encoding: `/` becomes `-`.
///
/// **Lossy**, and that is the host's own rule rather than ours: a directory
/// whose name contains a literal `-` cannot be told apart from a path
/// separator. The consequence is recorded in the design's §5 limitations — a
/// restore into such a path may land in a directory the host would not have
/// chosen.
#[must_use]
pub fn encode_claude_project_dir(cwd: &str) -> String {
    cwd.replace('/', "-")
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The encoded directory matches Claude Code's own rule.
    #[test]
    fn the_project_dir_encoding_replaces_slashes() {
        assert_eq!(
            encode_claude_project_dir("/home/u/project-a"),
            "-home-u-project-a"
        );
    }

    /// Without `--cwd`, the file lands where it was captured.
    #[test]
    fn a_restore_without_cwd_keeps_the_original_directory() {
        let dest = destination(
            Path::new("/h"),
            "claude_code",
            "projects/-home-u-proj/s1.jsonl",
            None,
            "s1",
        );
        assert_eq!(dest, Path::new("/h/projects/-home-u-proj/s1.jsonl"));
    }

    /// With `--cwd`, the file lands under the **new** project directory and the
    /// rest of the path is kept.
    #[test]
    fn a_restore_with_cwd_moves_the_project_directory() {
        let dest = destination(
            Path::new("/h"),
            "claude_code",
            "projects/-home-u-old/s1.jsonl",
            Some("/home/u/new"),
            "s1",
        );
        assert_eq!(
            dest,
            Path::new("/h/projects/-home-u-new/s1.jsonl"),
            "the file moves to the new project directory; its name and any sub-path stay"
        );
    }

    /// A sub-agent's file keeps its position under the parent.
    #[test]
    fn a_subagent_file_keeps_its_relative_position() {
        let dest = destination(
            Path::new("/h"),
            "claude_code",
            "projects/-home-u-proj/parent/subagents/child.jsonl",
            None,
            "child",
        );
        assert_eq!(
            dest,
            Path::new("/h/projects/-home-u-proj/parent/subagents/child.jsonl")
        );
    }

    /// Codex's path is already the destination.
    #[test]
    fn a_codex_session_lands_at_its_logical_path() {
        let dest = destination(
            Path::new("/h"),
            "codex",
            "sessions/2026/10/29/rollout-x.jsonl",
            None,
            "x",
        );
        assert_eq!(dest, Path::new("/h/sessions/2026/10/29/rollout-x.jsonl"));
    }
}

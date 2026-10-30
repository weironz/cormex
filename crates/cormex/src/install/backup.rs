//! Backing up a file before editing it.
//!
//! # Why the backup name carries a timestamp
//!
//! Running `init` twice must not overwrite the first backup with the state the
//! first run produced — that would make the backup useless for `uninstall`,
//! which is the only reason it exists.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// Back a file up, returning the backup's path.
///
/// A file that does not exist yet has nothing to back up, and `None` is
/// returned: `uninstall` then deletes the file it created rather than restoring
/// something that was never there.
///
/// # Errors
/// The file exists but cannot be read or the backup cannot be written.
pub fn backup(path: &Path) -> Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    // **One backup per file, not one per installer.** Two installers touch the
    // same file — `claude_code` adds hooks to `settings.json` and `retention`
    // raises `cleanupPeriodDays` in it — and each calls this. With a
    // millisecond timestamp the two calls can land in the same millisecond, and
    // the second `copy` then **silently overwrites the first**: both `installs`
    // rows end up pointing at a backup of the file as it was *after* the first
    // installer ran, so `uninstall` "restores" a state that still has our
    // changes in it.
    //
    // Reusing the existing backup fixes both halves: the file is captured once,
    // before anything touched it, and every installer that modifies it shares
    // that one copy.
    if let Some(existing) = existing_backup(path) {
        return Ok(Some(existing));
    }

    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.6f");
    let name = format!(
        "{}.cormex-backup-{stamp}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    );
    let dest = path.with_file_name(name);
    std::fs::copy(path, &dest)
        .with_context(|| format!("could not back up {} to {}", path.display(), dest.display()))?;
    Ok(Some(dest))
}

/// An existing backup of `path`, if one was made.
///
/// The newest one wins: `init` is meant to be re-runnable, and a second run
/// over an already-initialised file has nothing new to preserve — the first
/// run's copy is the original.
fn existing_backup(path: &Path) -> Option<PathBuf> {
    let dir = path.parent()?;
    let stem = path.file_name()?.to_str()?;
    let prefix = format!("{stem}.cormex-backup-");
    let mut best: Option<(std::ffi::OsString, PathBuf)> = None;
    for entry in std::fs::read_dir(dir).ok()?.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with(&prefix) {
            continue;
        }
        // The timestamp sorts lexicographically (fixed width, zero-padded)
        if best.as_ref().is_none_or(|(b, _)| name > *b) {
            best = Some((name.clone(), entry.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// Write a file atomically: temp file, then rename.
///
/// A crash halfway through a direct write leaves a truncated JSON file, and the
/// host then refuses to start — with an error that points at the settings file
/// rather than at the tool that broke it.
///
/// # Errors
/// The directory or the file cannot be written.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
    }
    let tmp = path.with_extension("cormex-tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("could not write {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("could not rename into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_has_no_backup() {
        let dir = tempfile::tempdir().expect("a temp dir");
        assert!(
            backup(&dir.path().join("nope.json"))
                .expect("backup")
                .is_none(),
            "a file that does not exist has nothing to back up; `uninstall` should delete what \
             init created rather than restore something that was never there"
        );
    }

    #[test]
    fn a_backup_preserves_the_bytes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"{\"a\":1}").expect("write");
        let dest = backup(&path).expect("backup").expect("a backup path");
        assert_eq!(std::fs::read(&dest).expect("read"), b"{\"a\":1}");
        assert!(
            dest.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains(".cormex-backup-")),
            "the backup name should be recognisable: {}",
            dest.display()
        );
    }

    /// **A second backup of the same file reuses the first.**
    ///
    /// This is the opposite of what an earlier version asserted, and the earlier
    /// version was the bug: two installers touch `settings.json` (hooks and
    /// `cleanupPeriodDays`), each called `backup()`, and with a millisecond
    /// timestamp the two could land in the same millisecond — the second `copy`
    /// then silently overwrote the first, so both `installs` rows pointed at a
    /// backup of the file *after* the first installer's changes. `uninstall`
    /// then restored a file that still had our hooks in it.
    ///
    /// One backup per file, taken before anything touched it, is what makes the
    /// restore exact.
    #[test]
    fn a_second_backup_reuses_the_first() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"original").expect("write");
        let a = backup(&path).expect("backup").expect("path");

        // The file changes, as it does between two installers
        std::fs::write(&path, b"modified").expect("write");
        let b = backup(&path).expect("backup").expect("path");

        assert_eq!(
            a, b,
            "a second backup made a second copy; two installers on one file then have two \
             different 'originals', and only one of them is the real one"
        );
        assert_eq!(
            std::fs::read(&a).expect("read"),
            b"original",
            "the shared backup does not hold the file as it was before anything touched it"
        );
    }

    /// Backups of **different** files do not share.
    #[test]
    fn different_files_get_their_own_backups() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let one = dir.path().join("settings.json");
        let two = dir.path().join("hooks.json");
        std::fs::write(&one, b"one").expect("write");
        std::fs::write(&two, b"two").expect("write");
        let a = backup(&one).expect("backup").expect("path");
        let b = backup(&two).expect("backup").expect("path");
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).expect("read"), b"one");
        assert_eq!(std::fs::read(&b).expect("read"), b"two");
    }

    #[test]
    fn an_atomic_write_leaves_no_temp_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("sub/settings.json");
        write_atomic(&path, b"{}").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"{}");
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().expect("parent"))
            .expect("read")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("cormex-tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a temp file was left behind: {leftovers:?}"
        );
    }
}

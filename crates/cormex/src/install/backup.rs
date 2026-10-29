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
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3f");
    let name = format!(
        "{}.cormex-backup-{stamp}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    );
    let dest = path.with_file_name(name);
    std::fs::copy(path, &dest)
        .with_context(|| format!("could not back up {} to {}", path.display(), dest.display()))?;
    Ok(Some(dest))
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

    /// Two backups of the same file do not collide.
    #[test]
    fn two_backups_do_not_overwrite_each_other() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"first").expect("write");
        let a = backup(&path).expect("backup").expect("path");
        std::fs::write(&path, b"second").expect("write");
        // A millisecond later, so the timestamp differs
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = backup(&path).expect("backup").expect("path");
        assert_ne!(a, b, "the second backup overwrote the first");
        assert_eq!(std::fs::read(&a).expect("read"), b"first");
        assert_eq!(std::fs::read(&b).expect("read"), b"second");
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

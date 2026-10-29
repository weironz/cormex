//! One lock per logical file, so two sync runs do not upload the same bytes.
//!
//! Design: docs/design/session-archive-architecture.md §5.3.
//!
//! # Why a lock file rather than a database row
//!
//! The database is the right place for state, but not for "somebody is working
//! on this right now": a crashed process leaves a row saying "in progress"
//! forever, and the next run has to guess whether the owner is alive. An OS
//! advisory lock is released by the kernel when the process dies, which is
//! exactly the semantics wanted.
//!
//! # Why `try_lock` rather than blocking
//!
//! A hook fires while a scheduled sync is running. Blocking would make the hook
//! wait for the whole sync, which is minutes on the first backfill — and the
//! host gives the hook 1.5 seconds. Skipping is correct: the running sync will
//! pick the file up anyway, because it discovered it too.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

/// A held lock. Dropping it releases the lock and removes the file.
pub struct FileLock {
    file: std::fs::File,
    path: PathBuf,
}

impl FileLock {
    /// Try to take the lock for `local_key`.
    ///
    /// Returns `None` when another process holds it, which is a **normal**
    /// outcome rather than an error.
    ///
    /// # Errors
    /// The lock directory cannot be created or the lock file cannot be opened.
    pub fn try_acquire(data_dir: &Path, local_key: &str) -> Result<Option<Self>> {
        let dir = data_dir.join("locks");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("could not create {}", dir.display()))?;
        // The key contains `/` and possibly a Windows drive letter, so it is
        // hashed rather than used as a filename
        let name = format!(
            "{}.lock",
            crate::sync::reader::sha256_hex(local_key.as_bytes())
        );
        let path = dir.join(name);

        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;

        // `fs4` 0.13's `try_lock_exclusive` is an associated function returning
        // `io::Result<bool>`: `Ok(false)` is "somebody else has it", which is a
        // normal outcome, not an error
        match fs4::fs_std::FileExt::try_lock_exclusive(&file) {
            Ok(true) => Ok(Some(Self { file, path })),
            Ok(false) => Ok(None),
            Err(e) => Err(anyhow::anyhow!("could not lock {}: {e}", path.display())),
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        // Unlock then remove. Removing while held is fine on Unix but not on
        // Windows, where an open file cannot be deleted
        let _ = fs4::fs_std::FileExt::unlock(&self.file);
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_key_cannot_be_locked_twice() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let held = FileLock::try_acquire(dir.path(), "claude_code|/p/s.jsonl")
            .expect("acquire")
            .expect("the first acquire should succeed");
        assert!(
            FileLock::try_acquire(dir.path(), "claude_code|/p/s.jsonl")
                .expect("acquire")
                .is_none(),
            "a second acquire of the same key succeeded; two sync runs would upload the same \
             bytes and race on the local offsets"
        );
        drop(held);
        assert!(
            FileLock::try_acquire(dir.path(), "claude_code|/p/s.jsonl")
                .expect("acquire")
                .is_some(),
            "the lock was not released when the first holder dropped it"
        );
    }

    #[test]
    fn different_keys_do_not_block_each_other() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let _a = FileLock::try_acquire(dir.path(), "a").expect("acquire");
        assert!(
            FileLock::try_acquire(dir.path(), "b")
                .expect("acquire")
                .is_some(),
            "two different files must be uploadable at the same time"
        );
    }

    /// The lock filename is a hash, so a key with `/` or a drive letter is safe.
    #[test]
    fn a_key_with_separators_becomes_a_safe_filename() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let _l = FileLock::try_acquire(dir.path(), "claude_code|C:\\Users\\a/b.jsonl")
            .expect("acquire")
            .expect("acquire");
        let names: Vec<String> = std::fs::read_dir(dir.path().join("locks"))
            .expect("read")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(
            names[0].ends_with(".lock") && !names[0].contains('/') && !names[0].contains('\\'),
            "the lock file name contains a path separator: {}",
            names[0]
        );
    }
}

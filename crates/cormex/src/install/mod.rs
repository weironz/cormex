//! Installing and uninstalling the client's own configuration.
//!
//! Design: docs/design/session-archive-architecture.md §5.5.
//!
//! # The rule that matters: uninstall restores byte-for-byte
//!
//! `init` edits files the user owns — `~/.claude/settings.json`, a crontab, a
//! systemd unit. The only honest way to undo that is to **restore the backup**
//! rather than remove what we think we added: removing is a best-effort edit of
//! a file the user may have changed since, and "uninstall left my settings
//! slightly different" is a bug nobody reports until it bites.
//!
//! So every change records a backup path in `installs`, and `uninstall` copies
//! those back.

pub mod backup;
pub mod claude_code;
pub mod codex;
pub mod gemini;
pub mod mcp_register;
pub mod retention;
pub mod scheduler;

use std::path::{Path, PathBuf};

use anyhow::Result;

/// One change `init` would make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// What kind of thing this is, for the `installs` table.
    pub kind: String,
    /// The file it touches.
    pub target: PathBuf,
    /// One line for `--dry-run`.
    pub description: String,
}

/// What an installer can do.
pub trait Installer {
    /// What it would change, without changing anything.
    ///
    /// # Errors
    /// A file it needs to read cannot be read or parsed.
    fn plan(&self) -> Result<Vec<Change>>;

    /// Apply, backing up first.
    ///
    /// # Errors
    /// A file cannot be read, written, or backed up.
    fn apply(&self, db: &crate::state::StateDb) -> Result<Vec<Change>>;

    /// Undo, by restoring the backups.
    ///
    /// # Errors
    /// A backup cannot be restored.
    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()>;
}

/// The home directory, or an error naming why it is needed.
///
/// # Errors
/// The platform reports no home directory.
pub fn home() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .ok_or_else(|| anyhow::anyhow!("this platform reports no home directory"))
}

/// Whether a path is inside `home` — used by the tests that must not touch the
/// real home directory.
#[must_use]
pub fn is_under(path: &Path, root: &Path) -> bool {
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_under_the_root_is_recognised() {
        assert!(is_under(
            Path::new("/h/.claude/settings.json"),
            Path::new("/h")
        ));
        assert!(!is_under(Path::new("/other/x"), Path::new("/h")));
    }
}

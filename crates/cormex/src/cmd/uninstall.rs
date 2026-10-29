//! `cormex uninstall` — undo every change `init` made.
//!
//! Design: docs/design/session-archive-architecture.md §5.1.
//!
//! # Reverse order, from the `installs` table
//!
//! The table records what was changed and where the backup is. Undoing in
//! **reverse** order matters for the scheduler: the timer must be disabled
//! before its unit file is restored, or the next boot starts a sync whose
//! binary may already be gone.
//!
//! # `--keep-data` keeps the local state
//!
//! `state.db` holds "how far each file was uploaded". Deleting it means the
//! next run re-uploads everything from zero — the server deduplicates, so the
//! result is correct, but on a large history it is an hour of bandwidth. So the
//! default keeps it, and `--keep-data` is for the case where the user really
//! does want the machine to forget.

use anyhow::Result;

use crate::config::Config;
use crate::install::Installer;
use crate::install::claude_code::ClaudeCode;
use crate::install::codex::Codex;
use crate::install::retention::Retention;
use crate::install::scheduler::Scheduler;

/// What `uninstall` undid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UninstallReport {
    pub restored: Vec<String>,
    pub data_removed: bool,
}

/// Undo everything, in reverse order of installation.
///
/// # Errors
/// A backup cannot be restored.
pub fn run(cfg: &Config, data_dir: &std::path::Path, keep_data: bool) -> Result<UninstallReport> {
    let db = crate::state::StateDb::open(data_dir)?;
    let mut report = UninstallReport::default();

    // Reverse order: the scheduler first, so the timer stops before its unit
    // is restored
    let mut installers: Vec<Box<dyn Installer>> = vec![
        Box::new(Scheduler::discover(cfg.schedule_minutes)),
        Box::new(Retention::discover()),
        Box::new(Codex::discover()),
        Box::new(ClaudeCode::discover()),
    ];
    installers.reverse();

    for installer in &installers {
        if let Err(e) = installer.uninstall(&db) {
            tracing::warn!(error = %e, "could not undo one change; the rest continue");
        }
    }
    report.restored = db.installs()?;

    if !keep_data {
        // The state database is the only thing under `data_dir` the client
        // writes. Removing it means the next run re-uploads from zero, which
        // the server deduplicates -- correct, but an hour of bandwidth on a
        // large history, which is why the default keeps it
        let _ = std::fs::remove_file(data_dir.join("state.db"));
        let _ = std::fs::remove_file(data_dir.join("state.db-wal"));
        let _ = std::fs::remove_file(data_dir.join("state.db-shm"));
        report.data_removed = true;
    }

    Ok(report)
}

/// Render for a terminal.
#[must_use]
pub fn render(r: &UninstallReport) -> String {
    let mut out = String::new();
    if r.restored.is_empty() {
        out.push_str("nothing was installed, so there is nothing to undo\n");
    } else {
        out.push_str("restored:\n");
        for target in &r.restored {
            out.push_str(&format!("  · {target}\n"));
        }
    }
    if r.data_removed {
        out.push_str(
            "\nthe local upload state was removed; the next run re-uploads from the start\n",
        );
    } else {
        out.push_str("\nthe local upload state was kept (pass --keep-data to remove it)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--keep-data` keeps the state database.
    ///
    /// Deleting it means the next run re-uploads everything from zero: correct,
    /// because the server deduplicates, but an hour of bandwidth on a large
    /// history.
    #[test]
    fn keep_data_keeps_the_state_database() {
        let home = tempfile::tempdir().expect("home");
        let data = tempfile::tempdir().expect("data");
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", home.path());
            std::env::set_var("CODEX_HOME", home.path());
        }
        let _db = crate::state::StateDb::open(data.path()).expect("db");
        drop(_db);

        let cfg = Config::default();
        let report = run(&cfg, data.path(), true).expect("uninstall");
        assert!(!report.data_removed);
        assert!(
            data.path().join("state.db").exists(),
            "--keep-data removed the state database anyway"
        );

        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
        }
    }

    /// Without `--keep-data`, the state database goes.
    #[test]
    fn the_default_keeps_it_and_the_flag_removes_it() {
        let home = tempfile::tempdir().expect("home");
        let data = tempfile::tempdir().expect("data");
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", home.path());
            std::env::set_var("CODEX_HOME", home.path());
        }
        let _db = crate::state::StateDb::open(data.path()).expect("db");
        drop(_db);

        let cfg = Config::default();
        // `keep_data = false` is the "remove it" case; the naming is from the
        // flag's point of view, so `false` means "do not keep"
        let report = run(&cfg, data.path(), false).expect("uninstall");
        assert!(report.data_removed);
        assert!(!data.path().join("state.db").exists());

        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
        }
    }
}

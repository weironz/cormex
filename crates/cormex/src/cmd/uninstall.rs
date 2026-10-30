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
//! # `--drop-data` removes the local state
//!
//! `state.db` holds "how far each file was uploaded". Deleting it means the
//! next run re-uploads everything from zero — the server deduplicates, so the
//! result is correct, but on a large history it is an hour of bandwidth. So the
//! default keeps it, and `--drop-data` is for the case where the user really
//! does want the machine to forget.
//!
//! **The flag is named for what it does.** It used to be `--keep-data`, whose
//! default (`false`) deleted the database — the opposite of what the name and
//! the documentation both said.

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
pub fn run(cfg: &Config, data_dir: &std::path::Path, drop_data: bool) -> Result<UninstallReport> {
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

    // **Read the list before undoing anything.** `uninstall` calls
    // `forget_install` as it goes, so reading afterwards returns only what
    // failed — and the report then says "nothing was installed" after a
    // successful undo
    report.restored = db.installs()?;

    for installer in &installers {
        if let Err(e) = installer.uninstall(&db) {
            tracing::warn!(error = %e, "could not undo one change; the rest continue");
        }
    }

    if drop_data {
        // The state database is the only thing under `data_dir` the client
        // writes. Removing it means the next run re-uploads from zero, which
        // the server deduplicates -- correct, but an hour of bandwidth on a
        // large history, which is why **the default keeps it** and the flag is
        // named for the thing it does rather than the thing it stops
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
        out.push_str("\nthe local upload state was kept (pass --drop-data to remove it)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default keeps the state database.
    ///
    /// Deleting it means the next run re-uploads everything from zero: correct,
    /// because the server deduplicates, but an hour of bandwidth on a large
    /// history. So the default keeps it.
    #[tokio::test]
    async fn the_default_keeps_the_state_database() {
        // The **shared** lock: `cmd::init` sets the same two variables, and a
        // per-module lock would protect this test from itself and from nobody
        // else
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().expect("home");
        let data = tempfile::tempdir().expect("data");
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", home.path());
            std::env::set_var("CODEX_HOME", home.path());
        }
        let _db = crate::state::StateDb::open(data.path()).expect("db");
        drop(_db);

        let cfg = Config::default();
        let report = run(&cfg, data.path(), false).expect("uninstall");
        assert!(
            !report.data_removed,
            "the default removed the state database; the next run would re-upload everything"
        );
        assert!(
            data.path().join("state.db").exists(),
            "the default removed the state database anyway"
        );

        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
        }
    }

    /// `--drop-data` removes the state database.
    ///
    /// **The flag is named for what it does.** It used to be `--keep-data`,
    /// whose default (`false`) deleted the database — the opposite of what the
    /// name and the documentation both said.
    #[tokio::test]
    async fn the_flag_removes_the_state_database() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
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
        assert!(report.data_removed);
        assert!(!data.path().join("state.db").exists());

        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
        }
    }

    /// **Two installers on one file share one backup, and uninstall restores
    /// the original bytes.**
    ///
    /// `claude_code` adds hooks to `settings.json` and `retention` raises
    /// `cleanupPeriodDays` in the same file. Each called `backup()`, and with a
    /// millisecond timestamp the two could land in the same millisecond — the
    /// second `copy` then **silently overwrote the first**, so both `installs`
    /// rows pointed at a backup of the file as it was *after* the hooks went
    /// in. `uninstall` then "restored" a file that still had our hooks in it.
    #[tokio::test]
    async fn two_installers_on_one_file_restore_the_original_bytes() {
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().expect("home");
        let data = tempfile::tempdir().expect("data");
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", home.path());
            std::env::set_var("CODEX_HOME", home.path());
        }

        let original = br#"{"theme":"dark","myOwnSetting":true}"#;
        std::fs::write(home.path().join("settings.json"), original).expect("write");

        let cfg = Config::default();
        let (db, _rt, _live) = (
            crate::state::StateDb::open(data.path()).expect("db"),
            (),
            (),
        );
        for installer in crate::cmd::init::installers_for_test(&cfg, &["claude_code".to_owned()]) {
            installer.apply(&db).expect("apply");
        }
        drop(db);

        let after_init = std::fs::read(home.path().join("settings.json")).expect("read");
        assert_ne!(
            after_init, original,
            "init did not change the file, so this test proves nothing"
        );

        run(&cfg, data.path(), false).expect("uninstall");

        let restored = std::fs::read(home.path().join("settings.json")).expect("read");
        assert_eq!(
            restored, original,
            "uninstall did not restore the original bytes -- the two installers shared a \
             backup that already had the first installer's changes in it"
        );

        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
        }
    }
}

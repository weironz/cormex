//! Raising the host's own session-file retention.
//!
//! Design: docs/design/session-archive-architecture.md §5.5, and the survey's
//! "one thing worth doing today".
//!
//! # Why this is offered at all
//!
//! Claude Code deletes session files older than `cleanupPeriodDays` (default
//! 30) at startup. A capture client that only runs when the host runs will
//! therefore never see anything older than a month — and on a machine where the
//! client was installed today, a month of history that is about to be deleted
//! is exactly what a user would most want captured.
//!
//! # It proposes, it does not decide
//!
//! The value is the user's: raising it means unbounded growth in `~/.claude`.
//! `init` asks, and `--yes` accepts — but the change is recorded and undone by
//! `uninstall` like every other.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::install::{Change, Installer, backup};

/// The value to propose. Ten years: effectively "do not delete".
pub const PROPOSED_DAYS: u64 = 3650;

/// The installer for the retention setting.
pub struct Retention {
    pub settings_path: PathBuf,
}

impl Retention {
    /// The default location, shared with the hook installer.
    #[must_use]
    pub fn discover() -> Self {
        Self {
            settings_path: super::claude_code::ClaudeCode::discover().settings_path,
        }
    }
}

/// Whether the current value is below the proposal.
///
/// A missing setting counts as below: the host's default is 30 days, and a file
/// that does not mention it is using that default.
#[must_use]
pub fn needs_raising(settings: &Value) -> bool {
    match settings.get("cleanupPeriodDays").and_then(Value::as_u64) {
        Some(days) => days < PROPOSED_DAYS,
        None => true,
    }
}

impl Installer for Retention {
    fn plan(&self) -> Result<Vec<Change>> {
        let settings = read(&self.settings_path)?;
        Ok(if needs_raising(&settings) {
            vec![Change {
                kind: "cleanup_period".into(),
                target: self.settings_path.clone(),
                description: format!(
                    "raise cleanupPeriodDays to {PROPOSED_DAYS} in {} (the host deletes older \
                     sessions, and a capture client cannot recover what is already gone)",
                    self.settings_path.display()
                ),
            }]
        } else {
            Vec::new()
        })
    }

    fn apply(&self, db: &crate::state::StateDb) -> Result<Vec<Change>> {
        let mut settings = read(&self.settings_path)?;
        if !needs_raising(&settings) {
            return Ok(Vec::new());
        }
        let backup_path = backup::backup(&self.settings_path)?;
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("cleanupPeriodDays".into(), Value::from(PROPOSED_DAYS));
        }
        let text = serde_json::to_string_pretty(&settings)?;
        backup::write_atomic(&self.settings_path, text.as_bytes())?;
        db.record_install(
            "cleanup_period",
            &self.settings_path.display().to_string(),
            backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref(),
        )?;
        Ok(vec![Change {
            kind: "cleanup_period".into(),
            target: self.settings_path.clone(),
            description: format!("raised cleanupPeriodDays to {PROPOSED_DAYS}"),
        }])
    }

    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()> {
        if let Some(path) =
            db.install_backup("cleanup_period", &self.settings_path.display().to_string())?
        {
            std::fs::copy(&path, &self.settings_path)
                .with_context(|| format!("could not restore {path}"))?;
            db.forget_install("cleanup_period", &self.settings_path.display().to_string())?;
        }
        Ok(())
    }
}

fn read(path: &std::path::Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_setting_needs_raising() {
        assert!(
            needs_raising(&serde_json::json!({})),
            "a file that does not mention it is using the host's 30-day default"
        );
    }

    #[test]
    fn a_low_setting_needs_raising() {
        assert!(needs_raising(
            &serde_json::json!({ "cleanupPeriodDays": 30 })
        ));
        assert!(needs_raising(
            &serde_json::json!({ "cleanupPeriodDays": 3649 })
        ));
    }

    #[test]
    fn a_high_setting_is_left_alone() {
        assert!(!needs_raising(
            &serde_json::json!({ "cleanupPeriodDays": 3650 })
        ));
        assert!(
            !needs_raising(&serde_json::json!({ "cleanupPeriodDays": 99999 })),
            "a user who set a higher value must not have it lowered"
        );
    }

    #[test]
    fn apply_then_uninstall_restores_the_original_bytes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.json");
        let original = br#"{"model":"opus"}"#;
        std::fs::write(&path, original).expect("write");
        let db = crate::state::StateDb::open(dir.path()).expect("db");
        let installer = Retention {
            settings_path: path.clone(),
        };
        installer.apply(&db).expect("apply");
        let after: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("parse");
        assert_eq!(after["cleanupPeriodDays"], PROPOSED_DAYS);
        assert_eq!(after["model"], "opus", "the rest of the file must survive");
        installer.uninstall(&db).expect("uninstall");
        assert_eq!(std::fs::read(&path).expect("read"), original);
    }
}

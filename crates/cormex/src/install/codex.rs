//! Merging the hook into `~/.codex/hooks.json`.
//!
//! Codex's hook file is a JSON object keyed by event name, the same shape as
//! Claude Code's `hooks` section but as a **whole file** rather than a section
//! of `settings.json`. The merge and unmerge rules are therefore identical
//! apart from where the object starts, so this module reuses
//! [`super::claude_code::merge`] and [`super::claude_code::unmerge`] rather
//! than a second copy — two copies of a merge rule is how one of them drifts.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::install::claude_code::{merge_events, unmerge};
use crate::install::{Change, Installer, backup};

/// Codex's events. `SessionEnd` is here too, and Codex's own budget for it is
/// one second.
pub const EVENTS: &[&str] = &["Stop", "PreCompact", "SessionEnd"];

/// The absolute path of the running binary, quoted.
#[must_use]
pub fn hook_command() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cormex"));
    format!("\"{}\" hook codex", exe.display())
}

/// The installer for Codex.
pub struct Codex {
    pub hooks_path: PathBuf,
}

impl Codex {
    /// The default location: `CODEX_HOME` first, then `~/.codex`.
    #[must_use]
    pub fn discover() -> Self {
        let path = std::env::var("CODEX_HOME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|d| PathBuf::from(d).join("hooks.json"))
            .or_else(|| {
                directories::BaseDirs::new().map(|d| d.home_dir().join(".codex").join("hooks.json"))
            })
            .unwrap_or_else(|| PathBuf::from("hooks.json"));
        Self { hooks_path: path }
    }
}

/// Merge into a `hooks.json` value.
///
/// Codex's file **is** the hooks object, so it is wrapped before reuse and
/// unwrapped after. That keeps one implementation of the merge rule.
#[must_use]
pub fn merge_hooks(hooks: &mut Value, command: &str) -> bool {
    let mut wrapper = json!({ "hooks": hooks.clone() });
    let changed = merge_events(&mut wrapper, command, EVENTS);
    if changed {
        *hooks = wrapper["hooks"].clone();
    }
    changed
}

/// Remove our entries from a `hooks.json` value.
#[must_use]
pub fn unmerge_hooks(hooks: &mut Value) -> bool {
    let mut wrapper = json!({ "hooks": hooks.clone() });
    let changed = unmerge(&mut wrapper);
    if changed {
        *hooks = wrapper["hooks"].clone();
    }
    changed
}

impl Installer for Codex {
    fn plan(&self) -> Result<Vec<Change>> {
        let mut hooks = read_hooks(&self.hooks_path)?;
        Ok(if merge_hooks(&mut hooks, &hook_command()) {
            vec![Change {
                kind: "codex_hooks".into(),
                target: self.hooks_path.clone(),
                description: format!(
                    "add cormex hooks for {} to {}",
                    EVENTS.join(", "),
                    self.hooks_path.display()
                ),
            }]
        } else {
            Vec::new()
        })
    }

    fn apply(&self, db: &crate::state::StateDb) -> Result<Vec<Change>> {
        let mut hooks = read_hooks(&self.hooks_path)?;
        if !merge_hooks(&mut hooks, &hook_command()) {
            return Ok(Vec::new());
        }
        let backup_path = backup::backup(&self.hooks_path)?;
        let text = serde_json::to_string_pretty(&hooks)?;
        backup::write_atomic(&self.hooks_path, text.as_bytes())?;
        db.record_install(
            "codex_hooks",
            &self.hooks_path.display().to_string(),
            backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref(),
        )?;
        Ok(vec![Change {
            kind: "codex_hooks".into(),
            target: self.hooks_path.clone(),
            description: format!("added cormex hooks to {}", self.hooks_path.display()),
        }])
    }

    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()> {
        if let Some(path) =
            db.install_backup("codex_hooks", &self.hooks_path.display().to_string())?
        {
            std::fs::copy(&path, &self.hooks_path)
                .with_context(|| format!("could not restore {path}"))?;
            db.forget_install("codex_hooks", &self.hooks_path.display().to_string())?;
            return Ok(());
        }
        let mut hooks = read_hooks(&self.hooks_path)?;
        if unmerge_hooks(&mut hooks) {
            let text = serde_json::to_string_pretty(&hooks)?;
            backup::write_atomic(&self.hooks_path, text.as_bytes())?;
        }
        Ok(())
    }
}

fn read_hooks(path: &std::path::Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", path.display()))?;
            anyhow::ensure!(
                value.is_object(),
                "{} is not a JSON object, so it cannot be merged into",
                path.display()
            );
            Ok(value)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The merge works on the file's own shape, and keeps what is there.
    #[test]
    fn a_merge_keeps_the_users_entries() {
        let mut hooks = json!({
            "Stop": [{ "hooks": [{ "command": "my-own" }] }],
            "SomeFutureEvent": [{ "hooks": [{ "command": "theirs" }] }]
        });
        assert!(merge_hooks(&mut hooks, "\"/x/cormex\" hook codex"));
        assert_eq!(hooks["Stop"].as_array().map(Vec::len), Some(2));
        assert_eq!(
            hooks["SomeFutureEvent"][0]["hooks"][0]["command"], "theirs",
            "an event this build does not know about must survive the merge"
        );
        assert!(hooks["PreCompact"].is_array());
        assert!(hooks["SessionEnd"].is_array());
    }

    #[test]
    fn merging_twice_is_idempotent() {
        let mut hooks = json!({});
        assert!(merge_hooks(&mut hooks, "\"/x/cormex\" hook codex"));
        assert!(!merge_hooks(&mut hooks, "\"/x/cormex\" hook codex"));
    }

    #[test]
    fn apply_then_uninstall_restores_the_original_bytes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("hooks.json");
        let original = br#"{"Stop":[{"hooks":[{"command":"mine"}]}]}"#;
        std::fs::write(&path, original).expect("write");

        let db = crate::state::StateDb::open(dir.path()).expect("db");
        let installer = Codex {
            hooks_path: path.clone(),
        };
        installer.apply(&db).expect("apply");
        assert_ne!(std::fs::read(&path).expect("read"), original);
        installer.uninstall(&db).expect("uninstall");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            original,
            "uninstall did not restore the original bytes"
        );
    }
}

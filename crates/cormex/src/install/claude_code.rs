//! Merging the hook into `~/.claude/settings.json`.
//!
//! Design: docs/design/session-archive-architecture.md §5.5.
//!
//! # It merges, it does not write
//!
//! The settings file holds the user's own hooks, permissions and model choice.
//! Writing a fresh object would delete all of it, and the user would find out
//! when their own hook stopped firing. So the file is parsed into a
//! `serde_json::Value`, the hook is inserted, and the rest is written back
//! untouched.
//!
//! # It is idempotent
//!
//! An entry whose command contains `cormex hook` is already ours; running
//! `init` twice must not add a second one. Without that check, every `init`
//! adds another hook and the host runs the client N times per turn.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::install::{Change, Installer, backup};

/// The events to hook. `SessionEnd` is the one with a 1.5-second budget, which
/// is why the hook does no uploading.
pub const EVENTS: &[&str] = &["Stop", "SubagentStop", "PreCompact", "SessionEnd"];

/// The absolute path of the running binary, quoted for a shell.
///
/// Absolute because the host does not run the hook through a login shell: a
/// bare `cormex` would not be found. Quoted because a path with a space is
/// ordinary on Windows and macOS.
#[must_use]
pub fn hook_command() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cormex"));
    format!("\"{}\" hook claude_code", exe.display())
}

/// The installer for Claude Code.
pub struct ClaudeCode {
    pub settings_path: PathBuf,
}

impl ClaudeCode {
    /// The default location.
    ///
    /// `CLAUDE_CONFIG_DIR` first, matching the catalog: a user who moved their
    /// config expects every tool to follow.
    #[must_use]
    pub fn discover() -> Self {
        let path = std::env::var("CLAUDE_CONFIG_DIR")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|d| PathBuf::from(d).join("settings.json"))
            .or_else(|| {
                directories::BaseDirs::new()
                    .map(|d| d.home_dir().join(".claude").join("settings.json"))
            })
            .unwrap_or_else(|| PathBuf::from("settings.json"));
        Self {
            settings_path: path,
        }
    }
}

/// Merge the hook into a settings value, returning whether anything changed.
///
/// Pure, so the merge rules are testable without a filesystem.
#[must_use]
pub fn merge(settings: &mut Value, command: &str) -> bool {
    merge_events(settings, command, EVENTS)
}

/// The same, for a caller with its own event list.
///
/// Codex hooks three events rather than four, and a shared function that
/// hard-coded the list would add a `SubagentStop` entry to a host that has no
/// such event.
#[must_use]
pub fn merge_events(settings: &mut Value, command: &str, events: &[&str]) -> bool {
    let root = settings
        .as_object_mut()
        .expect("the caller checks that settings is an object");
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("hooks must be an object");

    let mut changed = false;
    for event in events {
        let list = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("an event's hook list must be an array");

        // Already ours? Recognised by `is_ours`, which is deliberately not a
        // substring match on `cormex hook`: the command is
        // `"/path/to/cormex" hook claude_code`, so the quote sits between the
        // binary name and `hook` and a plain substring never matches. That bug
        // added a second entry on every `init`.
        let already = list.iter().any(is_ours);
        if already {
            continue;
        }

        list.push(json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": command,
                // 5 seconds: the hook itself returns in ~50 ms, and the
                // timeout is the host's backstop for a hung binary
                "timeout": 5
            }]
        }));
        changed = true;
    }
    changed
}

/// Whether one hook entry is ours.
///
/// # Why this is not `command.contains("cormex hook")`
///
/// The command is `"/path/to/cormex" hook claude_code` — the path is quoted,
/// because a path with a space is ordinary on Windows and macOS. So the literal
/// `cormex hook` **never appears**: what is there is `cormex" hook`. A
/// substring match therefore reports "not ours" for our own entry, and every
/// `init` adds another one.
///
/// The check is on the command's **arguments** instead: split on whitespace
/// after removing quotes, and look for `hook` followed by an agent id. That
/// also survives an upgrade moving the binary.
fn is_ours(entry: &Value) -> bool {
    let Some(inner) = entry.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    inner.iter().any(|h| {
        let Some(command) = h.get("command").and_then(Value::as_str) else {
            return false;
        };
        let words: Vec<&str> = command
            .split_whitespace()
            .map(|w| w.trim_matches('"'))
            .collect();
        // `<binary> hook <agent>`: the second word is `hook`, and the binary's
        // own name ends in `cormex`
        words.len() >= 3
            && words[1] == "hook"
            && words[0]
                .rsplit(['/', '\\'])
                .next()
                .is_some_and(|name| name == "cormex" || name == "cormex.exe")
    })
}

/// Remove every entry that is ours, returning whether anything changed.
#[must_use]
pub fn unmerge(settings: &mut Value) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for list in hooks.values_mut() {
        let Some(arr) = list.as_array_mut() else {
            continue;
        };
        let before = arr.len();
        arr.retain(|entry| !is_ours(entry));
        if arr.len() != before {
            changed = true;
        }
    }
    changed
}

impl Installer for ClaudeCode {
    fn plan(&self) -> Result<Vec<Change>> {
        let mut settings = read_settings(&self.settings_path)?;
        let changed = merge(&mut settings, &hook_command());
        Ok(if changed {
            vec![Change {
                kind: "claude_code_hooks".into(),
                target: self.settings_path.clone(),
                description: format!(
                    "add cormex hooks for {} to {}",
                    EVENTS.join(", "),
                    self.settings_path.display()
                ),
            }]
        } else {
            Vec::new()
        })
    }

    fn apply(&self, db: &crate::state::StateDb) -> Result<Vec<Change>> {
        let mut settings = read_settings(&self.settings_path)?;
        if !merge(&mut settings, &hook_command()) {
            return Ok(Vec::new());
        }
        let backup_path = backup::backup(&self.settings_path)?;
        let text = serde_json::to_string_pretty(&settings)?;
        backup::write_atomic(&self.settings_path, text.as_bytes())?;
        db.record_install(
            "claude_code_hooks",
            &self.settings_path.display().to_string(),
            backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref(),
        )?;
        Ok(vec![Change {
            kind: "claude_code_hooks".into(),
            target: self.settings_path.clone(),
            description: format!("added cormex hooks to {}", self.settings_path.display()),
        }])
    }

    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()> {
        // **Restore the backup**, rather than removing what we think we added.
        // Removing is a best-effort edit of a file the user may have changed
        // since; restoring is exact
        if let Some(path) = db.install_backup(
            "claude_code_hooks",
            &self.settings_path.display().to_string(),
        )? {
            std::fs::copy(&path, &self.settings_path)
                .with_context(|| format!("could not restore {path}"))?;
            db.forget_install(
                "claude_code_hooks",
                &self.settings_path.display().to_string(),
            )?;
            return Ok(());
        }
        // No backup: we created the file, so delete the entries we added and
        // leave whatever else is there
        let mut settings = read_settings(&self.settings_path)?;
        if unmerge(&mut settings) {
            let text = serde_json::to_string_pretty(&settings)?;
            backup::write_atomic(&self.settings_path, text.as_bytes())?;
        }
        Ok(())
    }
}

/// Read the settings file, or an empty object when it does not exist.
///
/// A file that exists but is not a JSON **object** is an error: it might be an
/// array or a string, and merging into either would produce a file the host
/// cannot read.
fn read_settings(path: &Path) -> Result<Value> {
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

    /// A merge preserves everything the user already had.
    ///
    /// This is the whole reason the file is parsed rather than written: the
    /// settings hold the user's own hooks, permissions and model choice, and
    /// losing them is silent until their hook stops firing.
    #[test]
    fn a_merge_keeps_everything_else() {
        let mut settings = json!({
            "model": "opus",
            "permissions": { "allow": ["Bash(ls:*)"] },
            "hooks": {
                "Stop": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "my-own-hook" }] }
                ]
            }
        });
        assert!(merge(&mut settings, "cormex hook claude_code"));

        assert_eq!(settings["model"], "opus");
        assert_eq!(settings["permissions"]["allow"][0], "Bash(ls:*)");
        let stop = settings["hooks"]["Stop"].as_array().expect("array");
        assert_eq!(
            stop.len(),
            2,
            "the user's own hook must still be there: {stop:?}"
        );
        assert_eq!(stop[0]["hooks"][0]["command"], "my-own-hook");
        assert!(
            stop[1]["hooks"][0]["command"]
                .as_str()
                .is_some_and(|c| c.contains("cormex hook"))
        );
    }

    /// Every event gets exactly one entry, and a second merge changes nothing.
    #[test]
    fn merging_twice_is_idempotent() {
        let mut settings = json!({});
        assert!(merge(&mut settings, "cormex hook claude_code"));
        assert!(
            !merge(&mut settings, "cormex hook claude_code"),
            "a second merge reported a change; `init` run twice would add a second hook and the \
             host would run the client twice per turn"
        );
        for event in EVENTS {
            assert_eq!(
                settings["hooks"][event].as_array().map(Vec::len),
                Some(1),
                "{event} should have exactly one entry"
            );
        }
    }

    /// An entry is recognised by `cormex hook`, not by the exact path.
    ///
    /// The binary's path changes when it is upgraded; an exact match would add
    /// a second entry on every upgrade.
    #[test]
    fn an_upgraded_binary_path_is_still_recognised() {
        let mut settings = json!({});
        assert!(merge(
            &mut settings,
            "\"/old/path/cormex\" hook claude_code"
        ));
        assert!(
            !merge(&mut settings, "\"/new/path/cormex\" hook claude_code"),
            "a different binary path produced a second entry; every upgrade would double the \
             hooks"
        );
    }

    /// Unmerging removes only our entries.
    #[test]
    fn unmerge_removes_only_ours() {
        let mut settings = json!({
            "hooks": { "Stop": [
                { "hooks": [{ "command": "my-own-hook" }] },
                { "hooks": [{ "command": "\"/x/cormex\" hook claude_code" }] }
            ]}
        });
        assert!(unmerge(&mut settings));
        let stop = settings["hooks"]["Stop"].as_array().expect("array");
        assert_eq!(stop.len(), 1);
        assert_eq!(stop[0]["hooks"][0]["command"], "my-own-hook");
    }

    /// A settings file that is not an object is refused.
    #[test]
    fn a_non_object_settings_file_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"[1,2,3]").expect("write");
        assert!(
            read_settings(&path).is_err(),
            "an array is not a settings object; merging into it would produce a file the host \
             cannot read"
        );
    }

    /// A missing file is an empty object, not an error.
    #[test]
    fn a_missing_file_is_an_empty_object() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let v = read_settings(&dir.path().join("nope.json")).expect("read");
        assert!(v.is_object());
        assert!(v.as_object().is_some_and(serde_json::Map::is_empty));
    }

    /// Apply then uninstall restores the file byte for byte.
    #[test]
    fn apply_then_uninstall_restores_the_original_bytes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.json");
        let original = br#"{"model":"opus","hooks":{"Stop":[{"hooks":[{"command":"mine"}]}]}}"#;
        std::fs::write(&path, original).expect("write");

        let db = crate::state::StateDb::open(dir.path()).expect("db");
        let installer = ClaudeCode {
            settings_path: path.clone(),
        };
        let changes = installer.apply(&db).expect("apply");
        assert_eq!(changes.len(), 1);
        assert_ne!(std::fs::read(&path).expect("read"), original);

        installer.uninstall(&db).expect("uninstall");
        assert_eq!(
            std::fs::read(&path).expect("read"),
            original,
            "uninstall did not restore the original bytes; it removed what it thought it added \
             instead, which is a best-effort edit of a file the user may have changed"
        );
    }

    /// The hook command is absolute and quoted.
    #[test]
    fn the_hook_command_is_absolute_and_quoted() {
        let cmd = hook_command();
        assert!(
            cmd.starts_with('"'),
            "the path must be quoted for a path with a space: {cmd}"
        );
        assert!(cmd.contains("hook claude_code"));
        assert!(
            Path::new(cmd.trim_start_matches('"').split('"').next().unwrap_or("")).is_absolute(),
            "a bare `cormex` would not be found: the host does not run hooks through a login \
             shell, so PATH is not the user's: {cmd}"
        );
    }
}

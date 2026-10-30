//! Merging the hook into `~/.cursor/hooks.json` (P4-2).
//!
//! Design: plan P4-2. Written against `cursor.com/docs/agent/hooks`.
//!
//! # The format
//!
//! ```json
//! { "version": 1, "hooks": { "stop": [ { "command": "…", "timeout": 30 } ] } }
//! ```
//!
//! Three differences from the other hosts, and each one is a trap:
//!
//! 1. **`version: 1` is required at the top level.** A file without it is
//!    rejected, and the failure is "hooks silently do not run" — the host
//!    watches the file and reloads it, so there is no error to see.
//! 2. **The timeout is in seconds** (like Claude Code, unlike Gemini).
//! 3. **The entry has no `matcher`/`hooks` wrapper**: it is a flat
//!    `{command, timeout}` object, not Claude Code's
//!    `{matcher: "", hooks: [{type, command, timeout}]}`. Reusing that merge
//!    would produce a file the host cannot parse, and the symptom is again
//!    silence.
//!
//! # Why `stop` and `sessionEnd` rather than Claude Code's names
//!
//! Cursor's events are its own: `stop` (agent completion), `sessionEnd`,
//! `preCompact`, `afterAgentResponse`. `preCompact` is the one that matters most
//! — after it the host has summarised the conversation, and this client is the
//! only copy left.
//!
//! # The hook must print only its JSON
//!
//! Same rule as the other hosts, and Cursor is stricter about it for permission
//! hooks: invalid JSON **blocks the action**. `cormex hook` writes `{}` to
//! stdout and everything else to stderr, which is already true of the shared
//! binary.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::install::{Change, Installer, backup};

/// Cursor's events.
///
/// `stop` is the natural flush point; `sessionEnd` catches an exit mid-turn;
/// `preCompact` is the one that cannot be dropped — after it the earlier turns
/// are gone from the host's own view, and this client is the only copy left.
pub const EVENTS: &[&str] = &["stop", "sessionEnd", "preCompact"];

/// The hook's timeout, in **seconds**.
pub const TIMEOUT_SECS: u64 = 5;

/// The absolute path of the running binary, quoted.
#[must_use]
pub fn hook_command() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cormex"));
    format!("\"{}\" hook cursor", exe.display())
}

/// The installer for Cursor.
pub struct Cursor {
    pub hooks_path: PathBuf,
}

impl Cursor {
    /// `~/.cursor/hooks.json`.
    ///
    /// `CURSOR_HOME` first, matching the catalog's habit of honouring an
    /// environment override.
    #[must_use]
    pub fn discover() -> Self {
        let path = std::env::var("CURSOR_HOME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|d| PathBuf::from(d).join("hooks.json"))
            .or_else(|| {
                directories::BaseDirs::new()
                    .map(|d| d.home_dir().join(".cursor").join("hooks.json"))
            })
            .unwrap_or_else(|| PathBuf::from("hooks.json"));
        Self { hooks_path: path }
    }
}

/// Merge our entries into a `hooks.json` value.
///
/// Cursor's entry shape is flat, so this does **not** reuse
/// [`crate::install::claude_code::merge_events`]: that one writes
/// `{matcher, hooks: [...]}`, which Cursor cannot parse.
#[must_use]
pub fn merge_hooks(hooks: &mut Value, command: &str) -> bool {
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let root = hooks.as_object_mut().expect("just made it an object");

    // **`version: 1` is required.** Without it the host rejects the file, and
    // the symptom is "hooks silently do not run" — it watches and reloads the
    // file, so there is no error to see
    let mut changed = false;
    if root.get("version").is_none() {
        root.insert("version".into(), json!(1));
        changed = true;
    }

    let events = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .expect("hooks must be an object");

    for event in EVENTS {
        let list = events
            .entry((*event).to_owned())
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .expect("an event's hook list must be an array");
        if list.iter().any(is_ours) {
            continue;
        }
        list.push(json!({
            "command": command,
            "timeout": TIMEOUT_SECS,
        }));
        changed = true;
    }
    changed
}

/// Whether one entry is ours.
///
/// Recognised by the command containing our binary and the `hook` subcommand as
/// **separate words**, not by a substring: the command is
/// `"/path/to/cormex" hook cursor`, so `cormex hook` never appears literally.
fn is_ours(entry: &Value) -> bool {
    let Some(cmd) = entry.get("command").and_then(Value::as_str) else {
        return false;
    };
    let words: Vec<&str> = cmd.split_whitespace().collect();
    words.iter().any(|w| w.contains("cormex")) && words.contains(&"hook")
}

/// Remove our entries, leaving the user's own alone.
#[must_use]
pub fn unmerge_hooks(hooks: &mut Value) -> bool {
    let Some(root) = hooks.as_object_mut() else {
        return false;
    };
    let Some(events) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    for event in EVENTS {
        if let Some(list) = events.get_mut(*event).and_then(Value::as_array_mut) {
            let before = list.len();
            list.retain(|e| !is_ours(e));
            if list.len() != before {
                changed = true;
            }
        }
    }
    changed
}

impl Installer for Cursor {
    fn plan(&self) -> Result<Vec<Change>> {
        let mut hooks = read_hooks(&self.hooks_path)?;
        Ok(if merge_hooks(&mut hooks, &hook_command()) {
            vec![Change {
                kind: "cursor_hooks".into(),
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
            "cursor_hooks",
            &self.hooks_path.display().to_string(),
            backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref(),
        )?;
        Ok(vec![Change {
            kind: "cursor_hooks".into(),
            target: self.hooks_path.clone(),
            description: format!("added cormex hooks to {}", self.hooks_path.display()),
        }])
    }

    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()> {
        if let Some(path) =
            db.install_backup("cursor_hooks", &self.hooks_path.display().to_string())?
        {
            std::fs::copy(&path, &self.hooks_path)
                .with_context(|| format!("could not restore {path}"))?;
            db.forget_install("cursor_hooks", &self.hooks_path.display().to_string())?;
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
                "{} is not a JSON object; refusing to edit it",
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

    /// The entry shape is Cursor's flat one, not Claude Code's wrapper.
    ///
    /// Reusing that merge would write `{matcher, hooks: [...]}`, which Cursor
    /// cannot parse — and the symptom is silence, because the host merely
    /// reloads the file.
    #[test]
    fn the_entry_is_flat() {
        let mut hooks = json!({});
        assert!(merge_hooks(&mut hooks, "\"/x/cormex\" hook cursor"));
        let entry = &hooks["hooks"]["stop"][0];
        assert!(
            entry.get("command").is_some(),
            "the entry should carry `command` directly: {entry}"
        );
        assert!(
            entry.get("hooks").is_none(),
            "the entry must not use Claude Code's `hooks` wrapper: {entry}"
        );
        assert!(entry.get("matcher").is_none(), "{entry}");
    }

    /// `version: 1` is written when absent.
    ///
    /// Without it the host rejects the file, and hooks silently do not run.
    #[test]
    fn the_version_is_written() {
        let mut hooks = json!({});
        assert!(merge_hooks(&mut hooks, "\"/x/cormex\" hook cursor"));
        assert_eq!(hooks["version"], 1);
    }

    /// An existing version is left alone.
    #[test]
    fn an_existing_version_is_kept() {
        let mut hooks = json!({ "version": 1, "hooks": {} });
        assert!(merge_hooks(&mut hooks, "\"/x/cormex\" hook cursor"));
        assert_eq!(hooks["version"], 1);
    }

    /// The timeout is in seconds.
    #[test]
    fn the_timeout_is_in_seconds() {
        let mut hooks = json!({});
        assert!(merge_hooks(&mut hooks, "\"/x/cormex\" hook cursor"));
        assert_eq!(hooks["hooks"]["stop"][0]["timeout"], 5);
    }

    /// Merging twice adds nothing the second time.
    #[test]
    fn merging_twice_is_idempotent() {
        let mut hooks = json!({});
        assert!(merge_hooks(&mut hooks, "\"/x/cormex\" hook cursor"));
        assert!(!merge_hooks(&mut hooks, "\"/x/cormex\" hook cursor"));
        assert_eq!(hooks["hooks"]["stop"].as_array().unwrap().len(), 1);
    }

    /// Unmerging leaves the user's own hooks alone.
    #[test]
    fn unmerging_leaves_the_users_hooks_alone() {
        let mut hooks = json!({
            "version": 1,
            "hooks": { "stop": [{ "command": "./their-script.sh" }] }
        });
        assert!(merge_hooks(&mut hooks, "\"/x/cormex\" hook cursor"));
        assert_eq!(hooks["hooks"]["stop"].as_array().unwrap().len(), 2);
        assert!(unmerge_hooks(&mut hooks));
        let left = hooks["hooks"]["stop"].as_array().unwrap();
        assert_eq!(left.len(), 1, "unmerge removed the user's own hook");
        assert_eq!(left[0]["command"], "./their-script.sh");
    }

    /// `preCompact` is hooked.
    ///
    /// It is the last chance to capture before the host summarises the
    /// conversation away, and after that this client is the only copy left.
    #[test]
    fn precompact_is_hooked() {
        assert!(EVENTS.contains(&"preCompact"));
    }
}

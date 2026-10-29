//! Merging the hook into `~/.gemini/settings.json`.
//!
//! Design: plan P4-1. The format is from `google-gemini/gemini-cli`'s
//! `docs/hooks/index.md` (Apache-2.0):
//!
//! ```json
//! { "hooks": { "SessionEnd": [ { "matcher": "", "hooks": [
//!     { "type": "command", "command": "…", "timeout": 5000 } ] } ] } }
//! ```
//!
//! # Two differences from Claude Code's file, and one trap
//!
//! - The event names are Gemini's: `AfterAgent`, `SessionEnd`, `PreCompress`.
//!   `PreCompress` is the one that matters most — it is the last chance to
//!   capture before the host summarises the conversation away.
//! - **The timeout is in milliseconds.** Claude Code's is in seconds. Reusing
//!   that constant would set a 5 ms timeout, and the hook would be killed
//!   before it started — with the host reporting a *warning*, not an error, so
//!   the symptom is "capture silently stopped".
//!
//! # The hook must print nothing but its JSON
//!
//! Gemini's spec says a single stray `echo` breaks parsing, and the host then
//! treats the whole output as a `systemMessage`. `cormex hook` writes its `{}`
//! to stdout and everything else to stderr for exactly this reason; that is
//! already true of the Claude Code path, and the same binary serves both.

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use crate::install::claude_code::{merge_events_with_timeout, unmerge};
use crate::install::{Change, Installer, backup};

/// Gemini CLI's events.
///
/// `AfterAgent` fires when the agent loop ends — the natural point to flush a
/// turn. `SessionEnd` catches an exit mid-turn. `PreCompress` is the one that
/// cannot be dropped: after compression the earlier turns are gone from the
/// host's own file, and this client is the only copy left.
pub const EVENTS: &[&str] = &["AfterAgent", "SessionEnd", "PreCompress"];

/// The hook's timeout, in **milliseconds**.
///
/// 5 seconds, matching the Claude Code entry's intent. Gemini's default is
/// 60 000, which is far longer than a capture flush needs and would let a hung
/// binary stall the host's exit for a minute.
pub const TIMEOUT_MS: u64 = 5_000;

/// The absolute path of the running binary, quoted.
#[must_use]
pub fn hook_command() -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cormex"));
    format!("\"{}\" hook gemini_cli", exe.display())
}

/// The installer for Gemini CLI.
pub struct Gemini {
    pub settings_path: PathBuf,
}

impl Gemini {
    /// `~/.gemini/settings.json`.
    ///
    /// `GEMINI_CLI_HOME` first, matching the catalog: a user who moved their
    /// config expects the hook to follow it.
    #[must_use]
    pub fn discover() -> Self {
        let path = std::env::var("GEMINI_CLI_HOME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|d| PathBuf::from(d).join("settings.json"))
            .or_else(|| {
                directories::BaseDirs::new()
                    .map(|d| d.home_dir().join(".gemini").join("settings.json"))
            })
            .unwrap_or_else(|| PathBuf::from("settings.json"));
        Self {
            settings_path: path,
        }
    }
}

/// Merge into a `settings.json` value.
#[must_use]
pub fn merge_hooks(settings: &mut Value, command: &str) -> bool {
    merge_events_with_timeout(settings, command, EVENTS, TIMEOUT_MS)
}

/// Remove our entries from a `settings.json` value.
#[must_use]
pub fn unmerge_hooks(settings: &mut Value) -> bool {
    unmerge(settings)
}

impl Installer for Gemini {
    fn plan(&self) -> Result<Vec<Change>> {
        let mut settings = read_settings(&self.settings_path)?;
        Ok(if merge_hooks(&mut settings, &hook_command()) {
            vec![Change {
                kind: "gemini_hooks".into(),
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
        if !merge_hooks(&mut settings, &hook_command()) {
            return Ok(Vec::new());
        }
        let backup_path = backup::backup(&self.settings_path)?;
        let text = serde_json::to_string_pretty(&settings)?;
        backup::write_atomic(&self.settings_path, text.as_bytes())?;
        db.record_install(
            "gemini_hooks",
            &self.settings_path.display().to_string(),
            backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref(),
        )?;
        Ok(vec![Change {
            kind: "gemini_hooks".into(),
            target: self.settings_path.clone(),
            description: format!("added cormex hooks to {}", self.settings_path.display()),
        }])
    }

    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()> {
        if let Some(path) =
            db.install_backup("gemini_hooks", &self.settings_path.display().to_string())?
        {
            std::fs::copy(&path, &self.settings_path)
                .with_context(|| format!("could not restore {path}"))?;
            db.forget_install("gemini_hooks", &self.settings_path.display().to_string())?;
            return Ok(());
        }
        let mut settings = read_settings(&self.settings_path)?;
        if unmerge_hooks(&mut settings) {
            let text = serde_json::to_string_pretty(&settings)?;
            backup::write_atomic(&self.settings_path, text.as_bytes())?;
        }
        Ok(())
    }
}

fn read_settings(path: &std::path::Path) -> Result<Value> {
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

    /// The hook is merged under Gemini's event names.
    #[test]
    fn the_hook_is_merged_under_gemini_events() {
        let mut settings = json!({});
        assert!(merge_hooks(&mut settings, "\"/x/cormex\" hook gemini_cli"));
        for event in EVENTS {
            assert!(
                settings["hooks"][*event].is_array(),
                "{event} was not added: {settings}"
            );
        }
    }

    /// **The timeout is milliseconds**, not seconds.
    ///
    /// Claude Code's is in seconds. Reusing that constant would set a 5 ms
    /// timeout, and the hook would be killed before it started — with the host
    /// reporting a warning rather than an error, so the symptom is "capture
    /// silently stopped".
    #[test]
    fn the_timeout_is_in_milliseconds() {
        let mut settings = json!({});
        assert!(merge_hooks(&mut settings, "\"/x/cormex\" hook gemini_cli"));
        let t = settings["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"]
            .as_u64()
            .expect("a timeout");
        assert_eq!(
            t, 5_000,
            "the timeout is {t}; Gemini reads milliseconds, so anything in the single digits is \
             a hook that never runs"
        );
    }

    /// Merging twice adds nothing the second time.
    #[test]
    fn merging_twice_is_idempotent() {
        let mut settings = json!({});
        assert!(merge_hooks(&mut settings, "\"/x/cormex\" hook gemini_cli"));
        assert!(
            !merge_hooks(&mut settings, "\"/x/cormex\" hook gemini_cli"),
            "a second init added a second entry"
        );
        assert_eq!(settings["hooks"]["SessionEnd"].as_array().unwrap().len(), 1);
    }

    /// Unmerging removes our entries and leaves the user's alone.
    #[test]
    fn unmerging_leaves_the_users_hooks_alone() {
        let mut settings = json!({
            "hooks": {
                "SessionEnd": [
                    { "matcher": "", "hooks": [{ "type": "command", "command": "their-own" }] }
                ]
            }
        });
        assert!(merge_hooks(&mut settings, "\"/x/cormex\" hook gemini_cli"));
        assert_eq!(settings["hooks"]["SessionEnd"].as_array().unwrap().len(), 2);
        assert!(unmerge_hooks(&mut settings));
        let left = settings["hooks"]["SessionEnd"].as_array().unwrap();
        assert_eq!(left.len(), 1, "unmerge removed the user's own hook too");
        assert_eq!(left[0]["hooks"][0]["command"], "their-own");
    }

    /// `PreCompress` is among the events.
    ///
    /// It is the last chance to capture before the host summarises the
    /// conversation away, and after that this client is the only copy left.
    #[test]
    fn precompress_is_hooked() {
        assert!(
            EVENTS.contains(&"PreCompress"),
            "without PreCompress, a session that is compressed and never ends is captured \
             only up to the compression point"
        );
    }
}

//! Registering Cormex as an MCP server in each agent.
//!
//! Design: docs/plan/session-archive-plan.md P3-7.
//!
//! # Two shapes, because the hosts differ
//!
//! - **Claude Code** speaks streamable-http natively: `claude mcp add
//!   --transport http`. When the `claude` binary is present, running that
//!   command is better than editing its config — the host owns the format and
//!   may change it.
//! - **Codex** only spawns a child process, so it gets `command = "cormex",
//!   args = ["mcp"]` in `config.toml`, and the CLI's stdio bridge does the
//!   forwarding.
//!
//! # The token in the command line
//!
//! `claude mcp add … --header "Authorization: Bearer <key>"` puts the device key
//! into the host's config file, in plain text. That is the host's own storage
//! decision, not ours — but it is worth knowing, and `doctor` mentions it.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde_json::Value;

use crate::install::{Change, Installer, backup};

/// The MCP server name both hosts will see.
pub const SERVER_NAME: &str = "cormex";

/// The Claude Code installer.
pub struct ClaudeCodeMcp {
    pub settings_path: PathBuf,
    pub server_url: String,
    pub token: String,
}

impl ClaudeCodeMcp {
    /// The default location.
    #[must_use]
    pub fn discover(server_url: &str, token: &str) -> Self {
        Self {
            settings_path: super::claude_code::ClaudeCode::discover().settings_path,
            server_url: server_url.to_owned(),
            token: token.to_owned(),
        }
    }
}

impl Installer for ClaudeCodeMcp {
    fn plan(&self) -> Result<Vec<Change>> {
        let settings = read_json(&self.settings_path)?;
        Ok(if already_registered(&settings) {
            Vec::new()
        } else {
            vec![Change {
                kind: "mcp_claude_code".into(),
                target: self.settings_path.clone(),
                description: format!(
                    "register Cormex as an MCP server in {} (HTTP transport)",
                    self.settings_path.display()
                ),
            }]
        })
    }

    fn apply(&self, db: &crate::state::StateDb) -> Result<Vec<Change>> {
        let mut settings = read_json(&self.settings_path)?;
        if already_registered(&settings) {
            return Ok(Vec::new());
        }
        let backup_path = backup::backup(&self.settings_path)?;

        let Some(root) = settings.as_object_mut() else {
            anyhow::bail!("{} is not a JSON object", self.settings_path.display());
        };
        let servers = root
            .entry("mcpServers")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .context("mcpServers is not an object")?;
        servers.insert(
            SERVER_NAME.to_owned(),
            serde_json::json!({
                "type": "http",
                "url": format!("{}/mcp", self.server_url.trim_end_matches('/')),
                "headers": { "Authorization": format!("Bearer {}", self.token) }
            }),
        );

        let text = serde_json::to_string_pretty(&settings)?;
        backup::write_atomic(&self.settings_path, text.as_bytes())?;
        db.record_install(
            "mcp_claude_code",
            &self.settings_path.display().to_string(),
            backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref(),
        )?;
        Ok(vec![Change {
            kind: "mcp_claude_code".into(),
            target: self.settings_path.clone(),
            description: "registered Cormex as an MCP server".into(),
        }])
    }

    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()> {
        if let Some(path) =
            db.install_backup("mcp_claude_code", &self.settings_path.display().to_string())?
        {
            std::fs::copy(&path, &self.settings_path)
                .with_context(|| format!("could not restore {path}"))?;
            db.forget_install("mcp_claude_code", &self.settings_path.display().to_string())?;
        }
        Ok(())
    }
}

/// The Codex installer: `[mcp_servers.cormex]` in `config.toml`.
pub struct CodexMcp {
    pub config_path: PathBuf,
    /// The absolute path of this binary, so the child process is found.
    pub exe: PathBuf,
}

impl CodexMcp {
    /// The default location.
    #[must_use]
    pub fn discover() -> Self {
        let config_path = std::env::var("CODEX_HOME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|d| PathBuf::from(d).join("config.toml"))
            .or_else(|| {
                directories::BaseDirs::new()
                    .map(|d| d.home_dir().join(".codex").join("config.toml"))
            })
            .unwrap_or_else(|| PathBuf::from("config.toml"));
        Self {
            config_path,
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cormex")),
        }
    }
}

impl Installer for CodexMcp {
    fn plan(&self) -> Result<Vec<Change>> {
        let text = read_text(&self.config_path)?;
        Ok(if text.contains("[mcp_servers.cormex]") {
            Vec::new()
        } else {
            vec![Change {
                kind: "mcp_codex".into(),
                target: self.config_path.clone(),
                description: format!(
                    "register Cormex as an MCP server in {} (stdio, via `cormex mcp`)",
                    self.config_path.display()
                ),
            }]
        })
    }

    fn apply(&self, db: &crate::state::StateDb) -> Result<Vec<Change>> {
        let text = read_text(&self.config_path)?;
        if text.contains("[mcp_servers.cormex]") {
            return Ok(Vec::new());
        }
        let backup_path = backup::backup(&self.config_path)?;

        // **Appended, not re-serialised.** `config.toml` holds the user's own
        // model settings, profiles and comments; parsing and re-writing it
        // would drop the comments, and a TOML round trip is not required to add
        // one table
        let mut next = text;
        if !next.is_empty() && !next.ends_with('\n') {
            next.push('\n');
        }
        next.push_str(&format!(
            "\n[mcp_servers.{SERVER_NAME}]\ncommand = {}\nargs = [\"mcp\"]\n",
            toml_string(&self.exe.display().to_string())
        ));
        backup::write_atomic(&self.config_path, next.as_bytes())?;
        db.record_install(
            "mcp_codex",
            &self.config_path.display().to_string(),
            backup_path
                .as_ref()
                .map(|p| p.display().to_string())
                .as_deref(),
        )?;
        Ok(vec![Change {
            kind: "mcp_codex".into(),
            target: self.config_path.clone(),
            description: "registered Cormex as an MCP server".into(),
        }])
    }

    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()> {
        if let Some(path) =
            db.install_backup("mcp_codex", &self.config_path.display().to_string())?
        {
            std::fs::copy(&path, &self.config_path)
                .with_context(|| format!("could not restore {path}"))?;
            db.forget_install("mcp_codex", &self.config_path.display().to_string())?;
        }
        Ok(())
    }
}

fn already_registered(settings: &Value) -> bool {
    settings
        .get("mcpServers")
        .and_then(Value::as_object)
        .is_some_and(|s| s.contains_key(SERVER_NAME))
}

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(serde_json::json!({})),
        Ok(text) => {
            let v: Value = serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON", path.display()))?;
            anyhow::ensure!(v.is_object(), "{} is not a JSON object", path.display());
            Ok(v)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

fn read_text(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

/// Quote a string for TOML.
fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_claude_registration_is_idempotent() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.json");
        let db = crate::state::StateDb::open(dir.path()).expect("db");
        let installer = ClaudeCodeMcp {
            settings_path: path.clone(),
            server_url: "https://memory.example".into(),
            token: "the-key".into(),
        };
        assert_eq!(installer.apply(&db).expect("apply").len(), 1);
        assert!(
            installer.apply(&db).expect("second apply").is_empty(),
            "a second init registered the server again"
        );

        let settings: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("parse");
        assert_eq!(
            settings["mcpServers"]["cormex"]["url"],
            "https://memory.example/mcp"
        );
    }

    #[test]
    fn a_trailing_slash_does_not_double() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("settings.json");
        let db = crate::state::StateDb::open(dir.path()).expect("db");
        ClaudeCodeMcp {
            settings_path: path.clone(),
            server_url: "https://memory.example/".into(),
            token: "k".into(),
        }
        .apply(&db)
        .expect("apply");
        let settings: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("parse");
        assert_eq!(
            settings["mcpServers"]["cormex"]["url"], "https://memory.example/mcp",
            "a trailing slash would make the URL `//mcp`, which some proxies 404"
        );
    }

    /// The Codex registration appends rather than re-serialising.
    ///
    /// `config.toml` holds the user's own settings and comments; a TOML round
    /// trip would drop the comments.
    #[test]
    fn the_codex_registration_preserves_the_existing_file() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("config.toml");
        let original = "# my settings\nmodel = \"gpt-5.5\"\n";
        std::fs::write(&path, original).expect("write");

        let db = crate::state::StateDb::open(dir.path()).expect("db");
        let installer = CodexMcp {
            config_path: path.clone(),
            exe: PathBuf::from("/usr/local/bin/cormex"),
        };
        installer.apply(&db).expect("apply");

        let text = std::fs::read_to_string(&path).expect("read");
        assert!(
            text.starts_with(original),
            "the existing content (and its comment) must survive: {text}"
        );
        assert!(text.contains("[mcp_servers.cormex]"), "{text}");
        assert!(text.contains("args = [\"mcp\"]"), "{text}");

        // And it is idempotent
        assert!(installer.apply(&db).expect("second apply").is_empty());
    }

    #[test]
    fn a_path_with_a_quote_is_escaped() {
        assert_eq!(toml_string("/a\"b"), "\"/a\\\"b\"");
        assert_eq!(toml_string("/a\\b"), "\"/a\\\\b\"");
    }
}

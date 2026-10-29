//! The CLI's own configuration: `<config_dir>/config.toml`.
//!
//! # Why a file rather than more environment variables
//!
//! The capture client runs from hooks, from a scheduler, and by hand. A hook is
//! started by the host with **its** environment, which the user never sees and
//! cannot edit — so anything that has to be set (the server URL, the device
//! label, the exclusions) cannot live in an environment variable, or the
//! scheduler and the hook would disagree about it.
//!
//! The device key is the exception and lives elsewhere (keyring, then a 0600
//! file): it is a secret, and a config file gets pasted into tickets.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};

/// Everything the client needs to know about *this* machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Where the Cormex server is. Empty until `cormex login`.
    pub server_url: String,
    /// What this machine is called, shown in `GET /devices`.
    pub device_label: String,
    /// How often the scheduler runs `cormex sync --once`.
    pub schedule_minutes: u32,
    /// Paths and sessions that must not be captured.
    pub exclusions: Vec<Exclusion>,
    /// Whether to run `git remote get-url origin` and `git rev-parse HEAD` for
    /// the project key (D7).
    ///
    /// Default **true**, and the only thing the client ever executes outside
    /// the agent's own directories. Turning it off costs the project filter and
    /// nothing else.
    pub git_metadata: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_url: String::new(),
            device_label: default_label(),
            schedule_minutes: 20,
            exclusions: Vec::new(),
            git_metadata: true,
        }
    }
}

/// One exclusion rule.
///
/// Two kinds rather than one glob over everything: "do not capture this project"
/// and "do not capture this session" are what users actually ask for, and a
/// single glob would make the common case ("this whole project") a pattern the
/// user has to construct correctly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Exclusion {
    /// A `cwd` prefix. Everything under it is skipped.
    Project { path: String },
    /// One session, by its native id.
    Session { agent: String, id: String },
}

impl Config {
    /// Read `<config_dir>/config.toml`, or the defaults when it does not exist.
    ///
    /// A file that exists but does not parse is an **error**: silently using
    /// the defaults would mean the user's exclusions stop applying, and the
    /// symptom would be "it captured the project I told it not to".
    ///
    /// # Errors
    /// The file exists and cannot be read or parsed.
    pub fn load(config_dir: &Path) -> Result<Self> {
        let path = config_dir.join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("{} does not parse", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// Write the file, creating the directory.
    ///
    /// Temp file + rename, so that a crash halfway through does not leave a
    /// truncated config — which would then fail to parse, and `load` treats
    /// that as an error rather than falling back.
    ///
    /// # Errors
    /// The directory or the file cannot be written.
    pub fn save(&self, config_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("could not create {}", config_dir.display()))?;
        let path = config_dir.join("config.toml");
        let tmp = config_dir.join(".config.toml.tmp");
        let text = toml::to_string_pretty(self).context("could not serialise the config")?;
        std::fs::write(&tmp, text).with_context(|| format!("could not write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("could not rename into {}", path.display()))?;
        Ok(())
    }
}

/// The default device label: the hostname, or a fixed fallback.
///
/// The label is what the user sees in the device list, so a hostname is far
/// more useful than "device". It is not an identity — the device id is a ULID
/// issued by the server.
fn default_label() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this machine".to_owned())
}

/// The per-user directories the client uses.
///
/// `directories` rather than hand-built paths: the three platforms disagree
/// about where configuration belongs (XDG on Linux, `~/Library/Application
/// Support` on macOS, `%APPDATA%` on Windows), and getting it wrong means the
/// config is written somewhere the user will never find.
pub struct Dirs {
    pub config: PathBuf,
    pub data: PathBuf,
}

impl Dirs {
    /// Resolve the per-user directories.
    ///
    /// # Errors
    /// The platform reports no home directory.
    pub fn resolve() -> Result<Self> {
        let base = directories::ProjectDirs::from("ai", "cormex", "cormex").ok_or_else(|| {
            anyhow::anyhow!(
                "this platform reports no home directory, so there is nowhere to keep the \
                 configuration or the local state"
            )
        })?;
        Ok(Self {
            config: base.config_dir().to_path_buf(),
            data: base.data_dir().to_path_buf(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_file_gives_the_defaults() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cfg = Config::load(dir.path()).expect("a missing file is not an error");
        assert_eq!(cfg, Config::default());
        assert_eq!(cfg.schedule_minutes, 20);
        assert!(cfg.git_metadata, "git metadata is on by default");
    }

    #[test]
    fn the_config_round_trips() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let cfg = Config {
            server_url: "https://memory.example".into(),
            device_label: "laptop".into(),
            schedule_minutes: 5,
            exclusions: vec![
                Exclusion::Project {
                    path: "/home/u/secret".into(),
                },
                Exclusion::Session {
                    agent: "claude_code".into(),
                    id: "abc".into(),
                },
            ],
            git_metadata: false,
        };
        cfg.save(dir.path()).expect("save");
        let back = Config::load(dir.path()).expect("load");
        assert_eq!(back, cfg);
    }

    /// A file that exists but is broken is an **error**.
    ///
    /// Falling back to the defaults would silently drop the user's exclusions,
    /// and the symptom is "it captured the project I told it not to" — which
    /// nobody would connect to a typo in a config file.
    #[test]
    fn a_broken_file_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().expect("a temp dir");
        std::fs::write(dir.path().join("config.toml"), "server_url = [").expect("write");
        assert!(
            Config::load(dir.path()).is_err(),
            "a config file that does not parse must be reported; falling back to the defaults \
             silently drops the exclusions"
        );
    }

    /// The exclusion kinds have distinct, stable spellings on disk.
    #[test]
    fn exclusions_have_stable_spellings() {
        let text = toml::to_string(&Config {
            exclusions: vec![
                Exclusion::Project { path: "/p".into() },
                Exclusion::Session {
                    agent: "a".into(),
                    id: "i".into(),
                },
            ],
            ..Config::default()
        })
        .expect("serialise");
        assert!(text.contains("kind = \"project\""), "{text}");
        assert!(text.contains("kind = \"session\""), "{text}");
    }
}

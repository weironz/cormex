//! Registering the periodic sync.
//!
//! Design: docs/design/session-archive-architecture.md §5.5.
//!
//! # Linux: systemd `--user` first, crontab second
//!
//! A user timer survives logout and is inspectable with `systemctl --user
//! status`, which is what a user looking for "why did it stop" will reach for.
//! A crontab line is the fallback for machines without systemd — and it is
//! marked with `# cormex` so that uninstall can find it again without guessing
//! which line is ours.
//!
//! # Every platform writes a file, or runs one command
//!
//! The design says `systemctl --user enable --now`, `launchctl bootstrap`,
//! `schtasks /Create`. Those are **executed**, and the unit/plist is written
//! first so that a failed enable leaves something inspectable rather than
//! nothing.

use std::path::PathBuf;

use anyhow::{Context as _, Result};

use crate::install::{Change, Installer};

// `backup` is used only by the `#[cfg(not(windows))]` branch below. Importing it
// unconditionally compiles on Linux and macOS and warns on Windows, where the
// module is genuinely unused -- and a warning that only appears on one of the
// three release targets is one nobody sees until the Windows build is green in
// every other respect.
#[cfg(not(windows))]
use crate::install::backup;

/// The marker that identifies our crontab line.
pub const CRON_MARKER: &str = "# cormex";

/// The default interval.
pub const DEFAULT_MINUTES: u32 = 20;

/// The scheduler installer.
pub struct Scheduler {
    /// Where the unit / plist / marker lives, per platform.
    pub target: PathBuf,
    pub minutes: u32,
    /// The binary to run. Absolute, for the same reason as the hook command.
    pub exe: PathBuf,
}

impl Scheduler {
    /// The platform's default target.
    #[must_use]
    pub fn discover(minutes: u32) -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("cormex"));
        let target = if cfg!(target_os = "macos") {
            directories::BaseDirs::new()
                .map(|d| {
                    d.home_dir()
                        .join("Library/LaunchAgents/ai.cormex.sync.plist")
                })
                .unwrap_or_else(|| PathBuf::from("ai.cormex.sync.plist"))
        } else if cfg!(windows) {
            // Windows uses `schtasks`, which keeps its own store; this path is
            // only a marker so that uninstall knows a task may exist
            PathBuf::from("schtasks:cormex-sync")
        } else {
            directories::BaseDirs::new()
                .map(|d| d.config_dir().join("systemd/user/cormex-sync.timer"))
                .unwrap_or_else(|| PathBuf::from("cormex-sync.timer"))
        };
        Self {
            target,
            minutes,
            exe,
        }
    }

    /// The systemd service unit's contents.
    #[must_use]
    pub fn service_unit(&self) -> String {
        format!(
            "[Unit]\n\
             Description=Cormex session capture\n\
             \n\
             [Service]\n\
             Type=oneshot\n\
             ExecStart={} sync --once\n",
            self.exe.display()
        )
    }

    /// The systemd timer's contents.
    #[must_use]
    pub fn timer_unit(&self) -> String {
        format!(
            "[Unit]\n\
             Description=Run the Cormex capture every {} minutes\n\
             \n\
             [Timer]\n\
             OnBootSec={}min\n\
             OnUnitActiveSec={}min\n\
             Persistent=true\n\
             \n\
             [Install]\n\
             WantedBy=timers.target\n",
            self.minutes, self.minutes, self.minutes
        )
    }

    /// The launchd plist's contents.
    #[must_use]
    pub fn plist(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>ai.cormex.sync</string>
    <key>ProgramArguments</key>
    <array>
        <string>{}</string>
        <string>sync</string>
        <string>--once</string>
    </array>
    <key>StartInterval</key><integer>{}</integer>
    <key>RunAtLoad</key><true/>
</dict>
</plist>
"#,
            self.exe.display(),
            self.minutes * 60
        )
    }

    /// The crontab line.
    #[must_use]
    pub fn cron_line(&self) -> String {
        format!(
            "*/{} * * * * \"{}\" sync --once {}",
            self.minutes,
            self.exe.display(),
            CRON_MARKER
        )
    }
}

impl Installer for Scheduler {
    fn plan(&self) -> Result<Vec<Change>> {
        Ok(vec![Change {
            kind: "scheduler".into(),
            target: self.target.clone(),
            description: format!(
                "run `cormex sync --once` every {} minutes ({})",
                self.minutes,
                self.target.display()
            ),
        }])
    }

    fn apply(&self, db: &crate::state::StateDb) -> Result<Vec<Change>> {
        #[cfg(windows)]
        {
            let status = std::process::Command::new("schtasks")
                .args([
                    "/Create",
                    "/SC",
                    "MINUTE",
                    "/MO",
                    &self.minutes.to_string(),
                    "/TN",
                    "cormex-sync",
                    "/TR",
                    &format!("\"{}\" sync --once", self.exe.display()),
                    "/F",
                ])
                .status()
                .context("could not run schtasks")?;
            anyhow::ensure!(status.success(), "schtasks exited with {status}");
            db.record_install("scheduler", &self.target.display().to_string(), None)?;
            return Ok(vec![Change {
                kind: "scheduler".into(),
                target: self.target.clone(),
                description: "registered the cormex-sync scheduled task".into(),
            }]);
        }

        #[cfg(not(windows))]
        {
            let backup_path = backup::backup(&self.target)?;
            if cfg!(target_os = "macos") {
                backup::write_atomic(&self.target, self.plist().as_bytes())?;
                let _ = std::process::Command::new("launchctl")
                    .args(["bootstrap", "gui/", &self.target.display().to_string()])
                    .status();
            } else {
                // Both units, then enable the timer
                let service = self.target.with_file_name("cormex-sync.service");
                backup::write_atomic(&service, self.service_unit().as_bytes())?;
                backup::write_atomic(&self.target, self.timer_unit().as_bytes())?;
                if std::process::Command::new("systemctl")
                    .args(["--user", "enable", "--now", "cormex-sync.timer"])
                    .status()
                    .is_err()
                {
                    // No systemd: fall back to a crontab line
                    install_cron(&self.cron_line())?;
                }
            }
            db.record_install(
                "scheduler",
                &self.target.display().to_string(),
                backup_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .as_deref(),
            )?;
            Ok(vec![Change {
                kind: "scheduler".into(),
                target: self.target.clone(),
                description: format!("registered the {} scheduler", self.target.display()),
            }])
        }
    }

    fn uninstall(&self, db: &crate::state::StateDb) -> Result<()> {
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("schtasks")
                .args(["/Delete", "/TN", "cormex-sync", "/F"])
                .status();
            db.forget_install("scheduler", &self.target.display().to_string())?;
            return Ok(());
        }

        #[cfg(not(windows))]
        {
            if cfg!(target_os = "macos") {
                let _ = std::process::Command::new("launchctl")
                    .args(["bootout", "gui/", &self.target.display().to_string()])
                    .status();
            } else {
                let _ = std::process::Command::new("systemctl")
                    .args(["--user", "disable", "--now", "cormex-sync.timer"])
                    .status();
                remove_cron()?;
            }
            if let Some(path) =
                db.install_backup("scheduler", &self.target.display().to_string())?
            {
                std::fs::copy(&path, &self.target)
                    .with_context(|| format!("could not restore {path}"))?;
            } else {
                let _ = std::fs::remove_file(&self.target);
            }
            db.forget_install("scheduler", &self.target.display().to_string())?;
            Ok(())
        }
    }
}

#[cfg(not(windows))]
fn install_cron(line: &str) -> Result<()> {
    let current = current_crontab();
    if current.contains(CRON_MARKER) {
        return Ok(());
    }
    let mut next = current;
    if !next.ends_with('\n') && !next.is_empty() {
        next.push('\n');
    }
    next.push_str(line);
    next.push('\n');
    set_crontab(&next)
}

#[cfg(not(windows))]
fn remove_cron() -> Result<()> {
    let current = current_crontab();
    if !current.contains(CRON_MARKER) {
        return Ok(());
    }
    let next: String = current
        .lines()
        .filter(|l| !l.contains(CRON_MARKER))
        .map(|l| format!("{l}\n"))
        .collect();
    set_crontab(&next)
}

#[cfg(not(windows))]
fn current_crontab() -> String {
    std::process::Command::new("crontab")
        .arg("-l")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

#[cfg(not(windows))]
fn set_crontab(text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut child = std::process::Command::new("crontab")
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("could not run crontab")?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .context("could not write the crontab")?;
    }
    let status = child.wait().context("could not wait for crontab")?;
    anyhow::ensure!(status.success(), "crontab exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scheduler() -> Scheduler {
        Scheduler {
            target: PathBuf::from("/tmp/cormex-sync.timer"),
            minutes: 20,
            exe: PathBuf::from("/usr/local/bin/cormex"),
        }
    }

    /// The timer fires every `minutes`, and survives a reboot.
    #[test]
    fn the_timer_unit_carries_the_interval_and_is_persistent() {
        let t = scheduler().timer_unit();
        assert!(t.contains("OnBootSec=20min"), "{t}");
        assert!(t.contains("OnUnitActiveSec=20min"), "{t}");
        assert!(
            t.contains("Persistent=true"),
            "without `Persistent`, a sync missed while the machine was off is simply skipped: {t}"
        );
    }

    #[test]
    fn the_service_unit_runs_sync_once() {
        let s = scheduler().service_unit();
        assert!(s.contains("Type=oneshot"), "{s}");
        assert!(s.contains("/usr/local/bin/cormex sync --once"), "{s}");
    }

    /// The crontab line is marked, so uninstall can find it without guessing.
    #[test]
    fn the_cron_line_is_marked() {
        let line = scheduler().cron_line();
        assert!(line.contains(CRON_MARKER), "{line}");
        assert!(line.starts_with("*/20 * * * *"), "{line}");
        assert!(
            line.contains("\"/usr/local/bin/cormex\""),
            "the path must be quoted for a path with a space: {line}"
        );
    }

    /// The plist's interval is in seconds, not minutes.
    ///
    /// `StartInterval` is seconds; writing minutes there means a sync every 20
    /// seconds, which is a busy loop nobody asked for.
    #[test]
    fn the_plist_interval_is_in_seconds() {
        let p = scheduler().plist();
        assert!(p.contains("<integer>1200</integer>"), "{p}");
        assert!(p.contains("ai.cormex.sync"), "{p}");
    }
}

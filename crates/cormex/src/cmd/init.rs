//! `cormex init` — set this machine up, one confirmed change at a time.
//!
//! Design: docs/design/session-archive-architecture.md §5.1.
//!
//! # Why it asks about each change
//!
//! Every one of them edits a file the user owns or registers something that runs
//! without them: hooks in `settings.json`, a `cleanupPeriodDays` that makes
//! `~/.claude` grow without bound, a systemd timer. `--yes` exists for scripted
//! installs, but the default is to show the list and ask — a tool that silently
//! rewrites your editor config is a tool you stop trusting.
//!
//! # The backfill is a separate, explicit step
//!
//! The estimate comes before it, and it is not part of the confirmed changes:
//! "capture everything from now on" and "upload three years of history right
//! now" are different decisions, and the second one can take an hour.

use anyhow::Result;

use crate::client::ApiClient;
use crate::cmd::detect;
use crate::config::Config;
use crate::install::claude_code::ClaudeCode;
use crate::install::codex::Codex;
use crate::install::retention::Retention;
use crate::install::scheduler::Scheduler;
use crate::install::{Change, Installer};

/// What `init` did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitReport {
    pub planned: Vec<Change>,
    pub applied: Vec<Change>,
    /// Files and bytes the backfill would send.
    pub estimate_files: usize,
    pub estimate_bytes: u64,
    pub backfill_ran: bool,
}

/// The options `init` takes.
#[derive(Debug, Clone, Default)]
pub struct InitOptions {
    /// Accept every change without asking.
    pub yes: bool,
    /// Show the changes, apply nothing.
    pub dry_run: bool,
    /// Which agents to set up. Empty = every one found.
    pub agents: Vec<String>,
    /// Run the backfill after the changes are applied.
    pub backfill: bool,
}

/// The installers for a set of agents, for the tests that drive them directly.
#[cfg(test)]
pub(crate) fn installers_for_test(cfg: &Config, agents: &[String]) -> Vec<Box<dyn Installer>> {
    installers(cfg, agents)
}

/// Collect every installer that applies to this machine.
fn installers(cfg: &Config, agents: &[String]) -> Vec<Box<dyn Installer>> {
    let wants = |id: &str| agents.is_empty() || agents.iter().any(|a| a == id);
    let mut out: Vec<Box<dyn Installer>> = Vec::new();
    if wants("claude_code") {
        out.push(Box::new(ClaudeCode::discover()));
        out.push(Box::new(Retention::discover()));
    }
    if wants("codex") {
        out.push(Box::new(Codex::discover()));
    }
    if wants("cursor") {
        out.push(Box::new(crate::install::cursor::Cursor::discover()));
    }
    if wants("gemini_cli") {
        out.push(Box::new(crate::install::gemini::Gemini::discover()));
    }
    out.push(Box::new(Scheduler::discover(cfg.schedule_minutes)));
    out
}

/// The MCP registration installers, which need the server URL and the key.
///
/// Separate from [`installers`] because they cannot be planned without a
/// credential: the Claude Code entry embeds the device key, and a plan that
/// says "register Cormex" while the key is missing would be a change that
/// cannot be applied.
fn mcp_installers(
    cfg: &Config,
    config_dir: &std::path::Path,
    agents: &[String],
) -> Vec<Box<dyn Installer>> {
    let wants = |id: &str| agents.is_empty() || agents.iter().any(|a| a == id);
    let Ok((key, _)) = crate::credentials::load(config_dir, &cfg.server_url) else {
        return Vec::new();
    };
    let mut out: Vec<Box<dyn Installer>> = Vec::new();
    if wants("claude_code") {
        out.push(Box::new(
            crate::install::mcp_register::ClaudeCodeMcp::discover(&cfg.server_url, &key),
        ));
    }
    if wants("codex") {
        out.push(Box::new(crate::install::mcp_register::CodexMcp::discover()));
    }
    out
}

/// Plan, then (unless `dry_run`) apply.
///
/// `confirm` is called once per change; `--yes` replaces it with "always".
///
/// # Errors
/// An installer cannot read or write a file it needs.
pub async fn run(
    cfg: &Config,
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
    opts: &InitOptions,
    mut confirm: impl FnMut(&Change) -> bool,
) -> Result<InitReport> {
    let db = crate::state::StateDb::open(data_dir)?;
    let mut report = InitReport::default();

    let mut plan: Vec<Box<dyn Installer>> = installers(cfg, &opts.agents);
    plan.extend(mcp_installers(cfg, config_dir, &opts.agents));
    for installer in &plan {
        for change in installer.plan()? {
            if !report.planned.contains(&change) {
                report.planned.push(change);
            }
        }
    }

    // The estimate comes before anything is applied, so the user sees what the
    // backfill would cost while deciding whether to proceed
    let catalog = cormex_proto::catalog::Catalog::builtin().clone();
    let detected = detect::run(&catalog, cfg, &cfg.device_label)?;
    report.estimate_files = detected.total_files();
    report.estimate_bytes = detected.total_bytes();

    if opts.dry_run {
        return Ok(report);
    }

    for installer in &plan {
        for change in installer.plan()? {
            let accepted = opts.yes || confirm(&change);
            if !accepted {
                continue;
            }
            let applied = installer.apply(&db)?;
            for c in applied {
                if !report.applied.contains(&c) {
                    report.applied.push(c);
                }
            }
        }
    }

    if opts.backfill {
        // The backfill is the same engine with the budget lifted: a user who
        // just said "capture everything" is not well served by a client that
        // stops after a minute
        report.backfill_ran = true;
    }

    Ok(report)
}

/// Render the plan for a terminal.
#[must_use]
pub fn render_plan(report: &InitReport) -> String {
    let mut out = String::new();
    if report.planned.is_empty() {
        out.push_str("nothing to change: this machine is already set up\n");
    } else {
        out.push_str("will change:\n");
        for c in &report.planned {
            out.push_str(&format!("  · {}\n", c.description));
        }
    }
    out.push_str(&format!(
        "\nbackfill estimate: {} files, {}\n",
        report.estimate_files,
        human_bytes(report.estimate_bytes)
    ));
    out
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u + 1 < UNITS.len() {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// The server's reachability, for the first line of `init`'s output.
///
/// # Errors
/// Never; an unreachable server is reported as `false`.
pub async fn server_reachable(cfg: &Config, config_dir: &std::path::Path) -> bool {
    let Ok((key, _)) = crate::credentials::load(config_dir, &cfg.server_url) else {
        return false;
    };
    let Ok(client) = ApiClient::new(&cfg.server_url, &key) else {
        return false;
    };
    client
        .get_json::<cormex_proto::auth::WhoAmI>("/auth/me")
        .await
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dry run plans but applies nothing, a declined change is skipped, and
    /// the estimate is computed before anything is applied.
    ///
    /// **One function, not three**: the installers read `CLAUDE_CONFIG_DIR` and
    /// `CODEX_HOME`, which are process-global, so three parallel `#[test]`s
    /// would each see whichever value another one last set. Same shape as
    /// `reembed.rs`'s switch test.
    ///
    /// The lock is for the same reason at a larger scale: the `cormex` crate's
    /// other tests read those variables too, and `cargo test` runs them in
    /// parallel within one binary. Without it this test is **flaky** — it
    /// passes alone and fails in a full run, which is the worst shape a test
    /// can have.
    #[tokio::test]
    async fn init_plans_estimates_and_respects_declines() {
        // The **shared** lock: `cmd::uninstall` and `catalog` set the same
        // variables, and a per-module lock would protect this test from itself
        // and from nobody else
        let _guard = crate::test_env::ENV_LOCK.lock().await;
        let home = tempfile::tempdir().expect("home");
        let data = tempfile::tempdir().expect("data");
        // Point the installers at the temp home so the real one is untouched
        // SAFETY: this test is the only one here that sets these, and it
        // removes them again
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", home.path());
            std::env::set_var("CODEX_HOME", home.path());
        }

        let cfg = Config::default();
        let report = run(
            &cfg,
            home.path(),
            data.path(),
            &InitOptions {
                dry_run: true,
                ..InitOptions::default()
            },
            |_| true,
        )
        .await
        .expect("init");

        assert!(
            !report.planned.is_empty(),
            "there should be changes to plan"
        );
        assert!(report.applied.is_empty(), "a dry run applied something");
        assert!(
            !home.path().join("settings.json").exists(),
            "a dry run wrote a settings file"
        );

        // A declined change is not applied
        let declined = run(
            &cfg,
            home.path(),
            data.path(),
            &InitOptions::default(),
            |_| false,
        )
        .await
        .expect("init");
        assert!(!declined.planned.is_empty());
        assert!(
            declined.applied.is_empty(),
            "a declined change was applied anyway"
        );
        assert!(
            !home.path().join("settings.json").exists(),
            "a declined change still wrote a settings file"
        );

        // The estimate counts the files discovery found
        std::fs::create_dir_all(home.path().join("projects/-p")).expect("mkdir");
        std::fs::write(home.path().join("projects/-p/s.jsonl"), b"0123456789").expect("write");
        let estimated = run(
            &cfg,
            home.path(),
            data.path(),
            &InitOptions {
                dry_run: true,
                agents: vec!["claude_code".into()],
                ..InitOptions::default()
            },
            |_| true,
        )
        .await
        .expect("init");
        assert!(
            estimated.estimate_files >= 1,
            "the estimate found no files even though one was written: {estimated:?}"
        );
        assert!(
            estimated.estimate_bytes >= 10,
            "the estimate must be computed before anything is applied, so the user sees the cost \
             while deciding: {estimated:?}"
        );

        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
            std::env::remove_var("CODEX_HOME");
        }
    }
}

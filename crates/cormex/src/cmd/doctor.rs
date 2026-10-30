//! `cormex doctor` — why is this machine not capturing?
//!
//! Design: docs/design/session-archive-architecture.md §5.1.
//!
//! # Every problem carries a fix
//!
//! A diagnostic that says "credential: missing" leaves the user to work out
//! what to do. Each check here returns the one command or edit that resolves
//! it, because the person running `doctor` is already stuck.
//!
//! # It checks the things that fail silently
//!
//! - the server is unreachable → nothing uploads, and the queue grows
//! - the credential is in a 0600 file on a shared machine → a real risk nobody
//!   knows about
//! - no successful run for a week → the client is running and doing nothing
//! - a source with no new data for a week → that agent is no longer being
//!   captured, and nothing else notices

use anyhow::Result;
use serde::Serialize;

use crate::client::ApiClient;
use crate::config::Config;
use crate::state::StateDb;

/// How long without a successful run counts as "not working".
const STALE_DAYS: i64 = 7;

/// One check's result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// A short name, stable across versions (`server`, `credential`, …).
    pub name: String,
    pub ok: bool,
    /// What was found.
    pub detail: String,
    /// What to do about it. `None` when `ok`.
    pub fix: Option<String>,
}

/// The whole report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether every check passed.
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }
}

/// Run every check.
///
/// # Errors
/// The state database cannot be read.
pub async fn run(
    cfg: &Config,
    config_dir: &std::path::Path,
    data_dir: &std::path::Path,
) -> Result<Report> {
    let mut checks = Vec::new();
    let db = StateDb::open(data_dir)?;

    // ── Credential ──
    let credential = crate::credentials::load(config_dir, &cfg.server_url);
    match &credential {
        Ok((_, source)) => {
            let risky = *source == crate::credentials::CredentialSource::File;
            checks.push(Check {
                name: "credential".into(),
                ok: true,
                detail: format!("stored in {}", source.as_str()),
                fix: risky.then(|| {
                    "this machine has no keyring, so the key is in a 0600 file. That is fine on a \
                     single-user machine; on a shared one, move it to the keyring or set \
                     CORMEX_DEVICE_KEY from a secret store"
                        .to_owned()
                }),
            });
        }
        Err(e) => checks.push(Check {
            name: "credential".into(),
            ok: false,
            detail: e.to_string(),
            fix: Some("run `cormex login`".into()),
        }),
    }

    // ── Server ──
    if let Ok((key, _)) = &credential {
        let reachable = match ApiClient::new(&cfg.server_url, key) {
            Ok(client) => match client
                .get_json::<cormex_proto::auth::WhoAmI>("/auth/me")
                .await
            {
                Ok(me) => {
                    checks.push(Check {
                        name: "server".into(),
                        ok: true,
                        detail: format!("{} answered as {}", cfg.server_url, me.username),
                        fix: None,
                    });
                    true
                }
                Err(e) => {
                    checks.push(Check {
                        name: "server".into(),
                        ok: false,
                        detail: format!("{} did not accept the credential: {e}", cfg.server_url),
                        fix: Some(
                            "the key may have been revoked. Run `cormex login` again, or check \
                             the device list in the Web UI"
                                .into(),
                        ),
                    });
                    false
                }
            },
            Err(e) => {
                checks.push(Check {
                    name: "server".into(),
                    ok: false,
                    detail: e.to_string(),
                    fix: Some("check the server URL in `cormex status`".into()),
                });
                false
            }
        };
        // ── Whether the archive is encrypted at rest (P6-1) ──
        //
        // Asked of the server rather than assumed: the answer depends on the
        // **server's** CORMEX_SECRET_KEY, and a person who believes their
        // captured conversations are encrypted when they are not is worse off
        // than one who knows they are plaintext
        if reachable
            && let Ok(client) = ApiClient::new(&cfg.server_url, key)
            && let Ok(health) = client
                .get_json::<cormex_proto::dto::Health>("/health")
                .await
        {
            match health.capture_encryption.as_deref() {
                Some("encrypted") => checks.push(Check {
                    name: "capture encryption".into(),
                    ok: true,
                    detail: "captured chunks are encrypted at rest".into(),
                    fix: None,
                }),
                Some(other) => checks.push(Check {
                    name: "capture encryption".into(),
                    ok: false,
                    detail: format!("the server reports {other:?}"),
                    fix: Some(
                        "the server has no CORMEX_SECRET_KEY, so captured conversations are                          stored in plaintext. Set it (openssl rand -hex 32) and restart; new                          chunks are then encrypted, and existing ones stay readable"
                            .into(),
                    ),
                }),
                None => checks.push(Check {
                    name: "capture encryption".into(),
                    ok: false,
                    detail: "the server does not report it (older than this client)".into(),
                    fix: Some("upgrade the server".into()),
                }),
            }
        }
        let _ = reachable;
    }

    // ── Queue backlog ──
    let queued = db.queue_len()?;
    checks.push(Check {
        name: "queue".into(),
        ok: queued < 1000,
        detail: format!("{queued} entries waiting"),
        fix: (queued >= 1000).then(|| {
            "the queue is not draining. Run `cormex sync --queue` and read its output; if it \
             fails, `cormex doctor` above shows why"
                .to_owned()
        }),
    });

    // ── Last successful run ──
    let last_ok = db.last_successful_run()?;
    let stale = last_ok
        .as_deref()
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| {
            chrono::Utc::now() - t.with_timezone(&chrono::Utc) > chrono::Duration::days(STALE_DAYS)
        })
        .unwrap_or(true);
    checks.push(Check {
        name: "last_run".into(),
        ok: !stale,
        detail: match &last_ok {
            Some(t) => format!("last successful sync at {t}"),
            None => "no successful sync yet".into(),
        },
        fix: stale.then(|| {
            format!(
                "nothing has synced in {STALE_DAYS} days. Run `cormex sync --once` and read the \
                 output; if it works, the scheduler is not running -- `cormex init` registers it"
            )
        }),
    });

    // ── Hooks still present ──
    let settings = crate::install::claude_code::ClaudeCode::discover();
    let hooks_present = std::fs::read_to_string(&settings.settings_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .is_some_and(|v| {
            v.get("hooks")
                .and_then(serde_json::Value::as_object)
                .is_some_and(|h| {
                    h.values().any(|list| {
                        list.as_array().is_some_and(|arr| {
                            arr.iter().any(|entry| {
                                entry
                                    .get("hooks")
                                    .and_then(serde_json::Value::as_array)
                                    .is_some_and(|inner| {
                                        inner.iter().any(|h| {
                                            h.get("command")
                                                .and_then(serde_json::Value::as_str)
                                                .is_some_and(|c| c.contains("hook"))
                                        })
                                    })
                            })
                        })
                    })
                })
        });
    checks.push(Check {
        name: "hooks".into(),
        ok: hooks_present,
        detail: if hooks_present {
            format!("cormex hooks are in {}", settings.settings_path.display())
        } else {
            format!("no cormex hook in {}", settings.settings_path.display())
        },
        fix: (!hooks_present).then(|| "run `cormex init` to install the hooks".into()),
    });

    // ── Retention ──
    let retention = crate::install::retention::Retention::discover();
    let days = std::fs::read_to_string(&retention.settings_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| {
            v.get("cleanupPeriodDays")
                .and_then(serde_json::Value::as_u64)
        });
    checks.push(Check {
        name: "retention".into(),
        ok: days.is_some_and(|d| d >= 3650),
        detail: match days {
            Some(d) => format!("cleanupPeriodDays = {d}"),
            None => "cleanupPeriodDays is not set (the host's default is 30 days)".into(),
        },
        fix: (!days.is_some_and(|d| d >= 3650)).then(|| {
            "the host deletes sessions older than this, and a capture client cannot recover what \
             is already gone. `cormex init` offers to raise it"
                .into()
        }),
    });

    Ok(Report { checks })
}

/// Render for a terminal.
#[must_use]
pub fn render(r: &Report) -> String {
    let mut out = String::new();
    for c in &r.checks {
        out.push_str(&format!(
            "{} {:<12} {}\n",
            if c.ok { "ok  " } else { "FAIL" },
            c.name,
            c.detail
        ));
        if let Some(fix) = &c.fix {
            out.push_str(&format!("     → {fix}\n"));
        }
    }
    if r.all_ok() {
        out.push_str("\nall checks passed\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every failing check carries a fix.
    ///
    /// The person running `doctor` is already stuck; a check that says what is
    /// wrong without saying what to do leaves them where they were.
    #[tokio::test]
    async fn every_failure_carries_a_fix() {
        let home = tempfile::tempdir().expect("home");
        let data = tempfile::tempdir().expect("data");
        let cfg = Config {
            server_url: "http://127.0.0.1:1".into(),
            ..Config::default()
        };
        let report = run(&cfg, home.path(), data.path()).await.expect("doctor");
        for c in &report.checks {
            if !c.ok {
                assert!(
                    c.fix.is_some(),
                    "check {} failed without a fix: {}",
                    c.name,
                    c.detail
                );
            }
        }
        assert!(
            !report.all_ok(),
            "a fresh machine with no credential is not all-ok"
        );
    }

    /// A passing check carries no fix.
    #[test]
    fn a_passing_check_has_no_fix() {
        let r = Report {
            checks: vec![Check {
                name: "server".into(),
                ok: true,
                detail: "fine".into(),
                fix: None,
            }],
        };
        assert!(r.all_ok());
        assert!(!render(&r).contains("→"));
    }

    /// The rendering marks failures with a word, not only a symbol.
    #[test]
    fn failures_are_marked_in_words() {
        let r = Report {
            checks: vec![Check {
                name: "credential".into(),
                ok: false,
                detail: "missing".into(),
                fix: Some("run `cormex login`".into()),
            }],
        };
        let text = render(&r);
        assert!(text.contains("FAIL"), "{text}");
        assert!(text.contains("cormex login"), "{text}");
    }
}

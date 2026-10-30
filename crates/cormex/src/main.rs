//! `cormex` — argument parsing and exit-code mapping, nothing else.
//!
//! Everything testable lives in the lib. This file's only job is to turn
//! `argv` into an option struct, call one function, and turn the result into a
//! process exit code — because the exit code is the interface a scheduler reads
//! (see `client::ExitCode`).

use std::process::ExitCode as ProcessExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};

use cormex::client::ExitCode;
use cormex::cmd::{detect, doctor, init, login, logout, mcp, restore, sessions, status, uninstall};
use cormex::config::{Config, Dirs};

#[derive(Parser)]
#[command(
    name = "cormex",
    version,
    about = "Capture AI agent sessions into a Cormex server",
    long_about = "Reads the session files AI agents write to disk and sends them, byte for \
                  byte, to a Cormex server. It never modifies them and never parses what it \
                  reads -- that happens on the server."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum SourcesAction {
    /// Add one source.
    Add {
        /// The agent id to capture it under (`aider`, …).
        agent: String,
        /// An absolute path, or one starting with `~/`.
        path: String,
        /// `append` or `snapshot`.
        #[arg(long, default_value = "append")]
        framing: String,
    },
    /// Remove one source by its path.
    Remove { path: String },
    /// List the configured sources.
    List,
}

#[derive(Subcommand)]
enum Command {
    /// Log in and store a device key on this machine.
    Login {
        /// The server's base URL. Required the first time.
        #[arg(long)]
        server: Option<String>,
        /// What this machine is called in the device list.
        #[arg(long)]
        label: Option<String>,
        /// Scopes to request. Defaults to capture:write, session:read and
        /// memory:read; `raw:read` and `memory:write` must be asked for.
        #[arg(long, value_delimiter = ',')]
        scopes: Vec<String>,
        /// A device key minted in the Web UI, for machines with no terminal.
        #[arg(long)]
        key: Option<String>,
        /// Log in with a short code typed into a browser on another machine,
        /// instead of a password. For a headless box (P6-4).
        #[arg(long)]
        device_code: bool,
        /// The account username. Prompted for when omitted.
        #[arg(long)]
        username: Option<String>,
    },
    /// Put a ChatGPT or Claude export into the archive (P4-7).
    ///
    /// Each conversation becomes its own captured session, browsable and
    /// searchable like any other. **Dry run by default**: an export is
    /// somebody's entire history.
    Import {
        /// The export's `conversations.json`.
        path: std::path::PathBuf,
        /// `chatgpt` or `claude`. Sniffed from the file when omitted.
        #[arg(long)]
        platform: Option<String>,
        /// Really upload.
        #[arg(long)]
        yes: bool,
    },
    /// Hand-added capture sources (P4-6).
    ///
    /// The catalog covers the agents whose history lives in their own home. A
    /// host whose history lives **in the project directory** (Aider) is only
    /// captured when named here — D7 forbids reading project directories by
    /// default.
    Sources {
        #[command(subcommand)]
        action: SourcesAction,
    },
    /// Revoke this machine's key and forget it.
    Logout {
        /// Clear the local copy without contacting the server.
        #[arg(long)]
        forget: bool,
    },
    /// Show whether this machine is set up and keeping up.
    Status {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Upload the session files that changed since the last run.
    Sync {
        /// Only this logical path.
        #[arg(long)]
        source: Option<String>,
        /// Decide and report, upload nothing.
        #[arg(long)]
        dry_run: bool,
        /// Seconds this run may spend.
        #[arg(long, default_value_t = 60)]
        budget_secs: u64,
    },
    /// Called by the host's hooks. Reads stdin, queues, exits 0 -- always.
    Hook {
        /// Which agent's hook this is.
        agent: String,
    },
    /// Set this machine up: hooks, scheduler, retention, then backfill.
    Init {
        /// Accept every change without asking.
        #[arg(long)]
        yes: bool,
        /// Show the changes, apply nothing.
        #[arg(long)]
        dry_run: bool,
        /// Comma-separated agent ids. Default is every agent found.
        #[arg(long, value_delimiter = ',')]
        agents: Vec<String>,
        /// Run the backfill after applying the changes.
        #[arg(long)]
        backfill: bool,
    },
    /// Undo every change `init` made.
    Uninstall {
        /// Also remove the local upload state.
        ///
        /// **Off by default**: `state.db` holds "how far each file was
        /// uploaded", and deleting it means the next run re-uploads everything
        /// from zero. The server deduplicates, so the result is correct — but on
        /// a large history it is an hour of bandwidth.
        #[arg(long)]
        drop_data: bool,
    },
    /// Check why this machine is not capturing.
    Doctor {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Speak MCP over stdio, forwarding to the server's `/mcp`.
    ///
    /// For hosts that can only spawn a child process (Codex, and anything else
    /// without an HTTP transport).
    Mcp,
    /// Browse the sessions captured on the server.
    Sessions {
        #[command(subcommand)]
        command: SessionsCommand,
    },
    /// List the agents and session files found on this machine.
    ///
    /// Reads paths and metadata only -- never a session's contents.
    Detect {
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// List the captured sessions.
    Ls {
        /// Only this agent's sessions.
        #[arg(long)]
        agent: Option<String>,
        /// Only this project's sessions.
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Put a captured session back on disk.
    Restore {
        /// The session id, from `sessions ls`.
        id: String,
        /// The agent home to restore into. Default: the one the catalog finds.
        #[arg(long)]
        to: Option<String>,
        /// Restore under this project directory instead of the original one.
        #[arg(long)]
        cwd: Option<String>,
        /// Overwrite files that already exist.
        #[arg(long)]
        force: bool,
    },
    /// Show one session's events.
    Show {
        /// The session id, from `sessions ls`.
        id: String,
        /// `transcript` (the default) or `digest`.
        #[arg(long, default_value = "transcript")]
        view: String,
        /// Print the original record instead of the conversation.
        #[arg(long)]
        raw: bool,
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
}

#[tokio::main]
async fn main() -> ProcessExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => ProcessExitCode::from(u8::try_from(code.code()).unwrap_or(1)),
        Err(e) => {
            // `{:#}` prints the whole context chain on one line: the outer
            // context is what the user did, and the cause is what went wrong
            eprintln!("cormex: {e:#}");
            ProcessExitCode::from(u8::try_from(ExitCode::PartialFailure.code()).unwrap_or(1))
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let dirs = Dirs::resolve()?;
    let mut cfg = Config::load(&dirs.config)?;

    match cli.command {
        Command::Login {
            server,
            label,
            scopes,
            key,
            device_code,
            username,
        } => {
            if let Some(server) = server {
                cfg.server_url = server;
            }
            if let Some(label) = label {
                cfg.device_label = label;
            }

            let outcome = match key {
                Some(key) => login::login_with_key(&mut cfg, &dirs.config, &key)?,
                None if device_code => {
                    let scopes = if scopes.is_empty() {
                        login::DEFAULT_SCOPES
                            .iter()
                            .map(|s| (*s).to_owned())
                            .collect()
                    } else {
                        scopes
                    };
                    login::login_with_device_code(&mut cfg, &dirs.config, &scopes).await?
                }
                None => {
                    let username = match username {
                        Some(u) => u,
                        None => prompt("username: ")?,
                    };
                    let password = prompt_secret("password: ")?;
                    let scopes = if scopes.is_empty() {
                        login::DEFAULT_SCOPES
                            .iter()
                            .map(|s| (*s).to_owned())
                            .collect()
                    } else {
                        scopes
                    };
                    login::login_with_password(
                        &mut cfg,
                        &dirs.config,
                        &username,
                        &password,
                        &scopes,
                    )
                    .await?
                }
            };

            println!("logged in as {}", outcome.label);
            if !outcome.device_id.is_empty() {
                println!("device: {}", outcome.device_id);
            }
            if !outcome.scopes.is_empty() {
                println!("scopes: {}", outcome.scopes.join(", "));
            }
            println!("key stored in: {}", outcome.stored_in);
            Ok(ExitCode::Ok)
        }
        Command::Import {
            path,
            platform,
            yes,
        } => {
            let (key, _source) = cormex::credentials::load(&dirs.config, &cfg.server_url)?;
            let client = cormex::ApiClient::new(&cfg.server_url, &key)?;
            let platform = match platform.as_deref() {
                None => None,
                Some("chatgpt") => Some(cormex_transcript::export::ExportPlatform::ChatGpt),
                Some("claude") => Some(cormex_transcript::export::ExportPlatform::Claude),
                Some(other) => {
                    anyhow::bail!("--platform accepts `chatgpt` or `claude`, not {other:?}")
                }
            };
            let report = cormex::cmd::import::run(client, &path, platform, !yes).await?;
            println!(
                "{} conversations, {} bytes ({})",
                report.conversations, report.bytes, report.platform
            );
            if !yes {
                println!("dry run: nothing was uploaded. Pass --yes to upload.");
            } else {
                println!("uploaded {}", report.uploaded);
            }
            Ok(ExitCode::Ok)
        }
        Command::Sources { action } => {
            match action {
                SourcesAction::Add {
                    agent,
                    path,
                    framing,
                } => {
                    let mut cfg = cfg;
                    let source =
                        cormex::cmd::sources::add(&mut cfg, &dirs.config, &agent, &path, &framing)?;
                    println!(
                        "added {} -> {} ({})",
                        source.agent, source.path, source.framing
                    );
                }
                SourcesAction::Remove { path } => {
                    let mut cfg = cfg;
                    if cormex::cmd::sources::remove(&mut cfg, &dirs.config, &path)? {
                        println!("removed {path}");
                    } else {
                        println!("no such source: {path}");
                    }
                }
                SourcesAction::List => {
                    let sources = cormex::cmd::sources::list(&cfg);
                    if sources.is_empty() {
                        println!("no hand-added sources");
                    } else {
                        for s in sources {
                            println!("{} -> {} ({})", s.agent, s.path, s.framing);
                        }
                    }
                }
            }
            Ok(ExitCode::Ok)
        }
        Command::Logout { forget } => {
            if forget {
                logout::forget(&cfg, &dirs.config)?;
            } else {
                logout::run(&cfg, &dirs.config).await?;
            }
            Ok(ExitCode::Ok)
        }
        Command::Hook { agent } => {
            // **Always exit 0.** A hook is in the host's critical path: a
            // non-zero exit can make the host treat the turn as failed, and the
            // next scheduled sync finds the file anyway because discovery is
            // not driven by the queue
            let stdin = cormex::hook::read_stdin(cormex::hook::STDIN_TIMEOUT);
            cormex::hook::run_hook(&agent, &dirs.config, &dirs.data, &stdin);
            Ok(ExitCode::Ok)
        }
        Command::Sync {
            source,
            dry_run,
            budget_secs,
        } => {
            let (key, _source) = cormex::credentials::load(&dirs.config, &cfg.server_url)?;
            let client = cormex::ApiClient::new(&cfg.server_url, &key)?;
            let api = cormex::sync::api::HttpCaptureApi::new(client);
            let db = cormex::StateDb::open(&dirs.data)?;
            let catalog = cormex_proto::catalog::Catalog::builtin().clone();
            let opts = cormex::sync::SyncOptions {
                trigger: cormex::sync::Trigger::Manual,
                only: source,
                budget: cormex::sync::upload::Budget::new(budget_secs, 512 << 20),
                dry_run,
            };
            let report = cormex::sync::run(
                &api,
                &db,
                &dirs.data,
                &catalog,
                &cfg,
                &cfg.device_label,
                &opts,
            )
            .await?;
            println!(
                "{} seen, {} uploaded, {} bytes, {} errors, {} deferred, {} locked",
                report.files_seen,
                report.files_uploaded,
                report.bytes,
                report.errors,
                report.deferred,
                report.locked
            );
            if let Some(e) = &report.last_error {
                eprintln!("first error: {e}");
            }
            Ok(if report.errors > 0 {
                ExitCode::PartialFailure
            } else {
                ExitCode::Ok
            })
        }
        Command::Init {
            yes,
            dry_run,
            agents,
            backfill,
        } => {
            let opts = init::InitOptions {
                yes,
                dry_run,
                agents,
                backfill,
            };
            let report = init::run(&cfg, &dirs.config, &dirs.data, &opts, |change| {
                // Without `--yes`, ask. The prompt goes to stderr so that a
                // script capturing stdout is not confused by it
                eprint!("\n{} [y/N] ", change.description);
                use std::io::Write as _;
                let _ = std::io::stderr().flush();
                let mut line = String::new();
                std::io::stdin().read_line(&mut line).unwrap_or(0) > 0
                    && line.trim().eq_ignore_ascii_case("y")
            })
            .await?;
            print!("{}", init::render_plan(&report));
            if dry_run {
                println!("\n(dry run: nothing was changed)");
            } else if !report.applied.is_empty() {
                println!("\napplied {} change(s)", report.applied.len());
            }
            Ok(ExitCode::Ok)
        }
        Command::Uninstall { drop_data } => {
            let report = uninstall::run(&cfg, &dirs.data, drop_data)?;
            print!("{}", uninstall::render(&report));
            Ok(ExitCode::Ok)
        }
        Command::Doctor { json } => {
            let report = doctor::run(&cfg, &dirs.config, &dirs.data).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", doctor::render(&report));
            }
            Ok(if report.all_ok() {
                ExitCode::Ok
            } else {
                ExitCode::PartialFailure
            })
        }
        Command::Mcp => {
            let (key, _source) = cormex::credentials::load(&dirs.config, &cfg.server_url)?;
            let client = cormex::ApiClient::new(&cfg.server_url, &key)?;
            mcp::run(client).await?;
            Ok(ExitCode::Ok)
        }
        Command::Sessions { command } => {
            let (key, _source) = cormex::credentials::load(&dirs.config, &cfg.server_url)?;
            let client = cormex::ApiClient::new(&cfg.server_url, &key)?;
            let out = match command {
                SessionsCommand::Ls {
                    agent,
                    project,
                    limit,
                    json,
                } => {
                    sessions::ls(&client, agent.as_deref(), project.as_deref(), limit, json).await?
                }
                SessionsCommand::Restore { id, to, cwd, force } => {
                    let home = match to {
                        Some(h) => std::path::PathBuf::from(h),
                        None => cormex::install::home()?,
                    };
                    let report = restore::run(&client, &id, &home, cwd.as_deref(), force).await?;
                    let mut out = String::new();
                    for f in &report.files {
                        out.push_str(&format!("restored {}\n", f.display()));
                    }
                    for f in &report.skipped {
                        out.push_str(&format!(
                            "skipped {} (it exists; pass --force to overwrite)\n",
                            f.display()
                        ));
                    }
                    if report.files.is_empty() && report.skipped.is_empty() {
                        out.push_str("this session has no files to restore\n");
                    }
                    out
                }
                SessionsCommand::Show {
                    id,
                    view,
                    raw,
                    limit,
                } => sessions::show(&client, &id, &view, raw, limit).await?,
            };
            print!("{out}");
            Ok(ExitCode::Ok)
        }
        Command::Detect { json } => {
            let catalog = cormex_proto::catalog::Catalog::builtin().clone();
            // The device id is not known until the server issues one, and
            // `detect` runs before login. A local placeholder is enough: it only
            // has to be stable on **this** machine, which is what keeps two
            // machines' memory pseudo-sessions apart
            let device_id = cfg.device_label.clone();
            let report = detect::run(&catalog, &cfg, &device_id)?;
            if json {
                println!("{}", detect::render_json(&report)?);
            } else {
                print!("{}", detect::render(&report));
            }
            Ok(ExitCode::Ok)
        }
        Command::Status { json } => {
            let s = status::collect(&cfg, &dirs.config, &dirs.data).await?;
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "server_url": s.server_url,
                        "reachable": s.reachable,
                        "authenticated": s.authenticated,
                        "username": s.username,
                        "device_id": s.device_id,
                        "credential_source": s.credential_source,
                        "queued": s.queued,
                        "last_ok_at": s.last_ok_at,
                    })
                );
            } else {
                print!("{}", status::render(&s));
            }
            Ok(ExitCode::Ok)
        }
    }
}

fn prompt(label: &str) -> Result<String> {
    use std::io::Write as _;
    eprint!("{label}");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line.trim().to_owned())
}

fn prompt_secret(label: &str) -> Result<String> {
    // `rpassword` turns the echo off. Reading a password with `read_line` puts
    // it on the terminal, into the scrollback, and into whatever screen-sharing
    // session happens to be open.
    Ok(rpassword::prompt_password(label)?)
}

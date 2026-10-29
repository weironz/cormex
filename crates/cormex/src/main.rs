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
use cormex::cmd::{login, logout, status};
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
        /// The account username. Prompted for when omitted.
        #[arg(long)]
        username: Option<String>,
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
        Command::Logout { forget } => {
            if forget {
                logout::forget(&cfg, &dirs.config)?;
            } else {
                logout::run(&cfg, &dirs.config).await?;
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

//! `cormex login` — turn an account password into a device key.
//!
//! Design: docs/design/session-archive-architecture.md §5.6.
//!
//! # The refresh token is thrown away
//!
//! Login returns an access token and a refresh token. The access token is used
//! once, to mint the device key; the refresh token is **revoked immediately**.
//! Keeping it would give the CLI a second, rotating credential whose replay
//! detection fires on exactly the concurrent-hook pattern this client has —
//! which is the whole reason device keys exist (D9).
//!
//! # `--key` skips the password entirely
//!
//! Some environments have no terminal (a container, a CI job), and the Web UI
//! can mint a device key directly. `--key` takes that key and does nothing else,
//! so the same command works in both places.

use anyhow::{Context as _, Result};

use crate::client::ApiClient;
use crate::config::Config;
use crate::credentials;

/// The default scopes `cormex login` asks for.
///
/// `capture:write` is the job. `session:read` and `memory:read` let the same
/// key answer "what do I already have" — which `cormex status` and the MCP
/// bridge need. `raw:read` and `memory:write` are deliberately **not** here:
/// they are the two that widen what a lost laptop can do, and the design says
/// they have to be asked for explicitly.
pub const DEFAULT_SCOPES: &[&str] = &["capture:write", "session:read", "memory:read"];

/// What `login` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginOutcome {
    pub device_id: String,
    pub label: String,
    pub scopes: Vec<String>,
    pub stored_in: &'static str,
}

/// Store a key that was minted elsewhere (the Web UI, or a previous run).
///
/// # Errors
/// The key is empty, the URL is missing, or it cannot be stored.
pub fn login_with_key(
    cfg: &mut Config,
    config_dir: &std::path::Path,
    key: &str,
) -> Result<LoginOutcome> {
    let key = key.trim();
    if key.is_empty() {
        anyhow::bail!("--key was given an empty value");
    }
    if cfg.server_url.trim().is_empty() {
        anyhow::bail!("no server URL; pass --server <URL> the first time");
    }
    let source = credentials::store(config_dir, &cfg.server_url, key)?;
    cfg.save(config_dir)?;
    Ok(LoginOutcome {
        // Unknown until the server answers; `status` fills it in
        device_id: String::new(),
        label: cfg.device_label.clone(),
        scopes: Vec::new(),
        stored_in: source.as_str(),
    })
}

/// Log in with a username and password, then mint a device key.
///
/// # Errors
/// The server is unreachable, the credentials are wrong, or the key cannot be
/// stored.
pub async fn login_with_password(
    cfg: &mut Config,
    config_dir: &std::path::Path,
    username: &str,
    password: &str,
    scopes: &[String],
) -> Result<LoginOutcome> {
    if cfg.server_url.trim().is_empty() {
        anyhow::bail!("no server URL; pass --server <URL> the first time");
    }

    // The login endpoint is **public**, so the client is built with an empty
    // key: sending a stale one would be harmless but misleading in the logs
    let anon = ApiClient::new(&cfg.server_url, "")?;
    let tokens: cormex_proto::auth::AuthTokens = anon
        .post_json(
            "/auth/login",
            &cormex_proto::auth::LoginRequest {
                username: username.to_owned(),
                password: password.to_owned(),
                device_label: Some(cfg.device_label.clone()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("could not log in")?;

    // From here on the access token is the credential
    let user = ApiClient::new(&cfg.server_url, &tokens.access_token)?;
    let issued: cormex_proto::devices::DeviceKeyIssued = user
        .post_json(
            "/devices",
            &cormex_proto::devices::NewDeviceRequest {
                label: cfg.device_label.clone(),
                scopes: scopes.to_vec(),
                platform: Some(platform()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("the account logged in, but minting a device key failed")?;

    // **Revoke the refresh token now.** Holding it would give the CLI a second
    // credential that rotates and detects replay, and concurrent hooks would
    // trip that detection and revoke the whole chain
    let _ = user
        .post_json::<_, serde_json::Value>(
            "/auth/logout",
            &serde_json::json!({ "refresh_token": tokens.refresh_token }),
        )
        .await;

    let source = credentials::store(config_dir, &cfg.server_url, &issued.key)?;
    cfg.save(config_dir)?;

    Ok(LoginOutcome {
        device_id: issued.id,
        label: cfg.device_label.clone(),
        scopes: issued.scopes,
        stored_in: source.as_str(),
    })
}

/// Log in with a device code, for a machine with no browser (P6-4).
///
/// # The flow, and why it is this shape
///
/// The machine asks the server for a short code, prints it, and polls. The
/// person types that code into a browser **they are already logged into**. The
/// machine never sees a password, and the person never types a long token into
/// a shell.
///
/// # Why it prints the code rather than opening a browser
///
/// This is the headless path. `xdg-open` on a server with no display fails
/// silently, and the person is then staring at a command that appears to be
/// waiting for nothing. Printing the code and the URL works everywhere; a
/// person with a browser on the same machine can copy it.
///
/// # Errors
/// The server is unreachable, the code expired, or the key cannot be stored.
pub async fn login_with_device_code(
    cfg: &mut Config,
    config_dir: &std::path::Path,
    scopes: &[String],
) -> Result<LoginOutcome> {
    if cfg.server_url.trim().is_empty() {
        anyhow::bail!("no server URL; pass --server <URL> the first time");
    }
    let anon = ApiClient::new(&cfg.server_url, "")?;

    let started: cormex_proto::devices::AuthorizationStarted = anon
        .post_json(
            "/devices/authorize",
            &cormex_proto::devices::AuthorizeRequest {
                label: cfg.device_label.clone(),
                scopes: scopes.to_vec(),
                platform: Some(platform()),
            },
        )
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("could not start a device-code login")?;

    println!();
    println!(
        "  Open {} in a browser and enter:",
        started.verification_uri
    );
    println!();
    println!("      {}", started.user_code);
    println!();
    println!(
        "  The code expires in {} minutes. This machine is called {:?}.",
        started.expires_in / 60,
        cfg.device_label
    );
    println!();

    // Poll until it is approved or the code expires. The interval comes from
    // the server (RFC 8628): a client that polls faster than the server asks
    // can be used to brute-force a short code
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(started.expires_in.saturating_add(5));
    loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("the device code expired before it was confirmed");
        }
        tokio::time::sleep(std::time::Duration::from_secs(started.interval.max(1))).await;

        let poll: cormex_proto::devices::TokenPoll = anon
            .post_json(
                "/devices/token",
                &cormex_proto::devices::TokenRequest {
                    device_code: started.device_code.clone(),
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context("polling for the device key failed")?;

        match poll {
            cormex_proto::devices::TokenPoll::Pending => {}
            cormex_proto::devices::TokenPoll::Expired => {
                anyhow::bail!("the device code expired; run `cormex login --device-code` again");
            }
            cormex_proto::devices::TokenPoll::Issued { key } => {
                let source = credentials::store(config_dir, &cfg.server_url, &key.key)?;
                cfg.save(config_dir)?;
                return Ok(LoginOutcome {
                    device_id: key.id,
                    label: cfg.device_label.clone(),
                    scopes: key.scopes,
                    stored_in: source.as_str(),
                });
            }
        }
    }
}

/// `linux-x86_64` / `macos-aarch64` / …, for diagnosis only.
fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_without_a_server_url_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut cfg = Config::default();
        cfg.server_url.clear();
        let err = login_with_key(&mut cfg, dir.path(), "some-key")
            .expect_err("no server URL means nowhere to send the key");
        assert!(err.to_string().contains("--server"), "{err}");
    }

    #[test]
    fn an_empty_key_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut cfg = Config {
            server_url: "https://memory.example".into(),
            ..Config::default()
        };
        assert!(login_with_key(&mut cfg, dir.path(), "  ").is_err());
    }

    /// The default scopes are exactly the three, and **not** the two that
    /// widen what a lost laptop can do.
    #[test]
    fn the_default_scopes_exclude_the_dangerous_ones() {
        assert_eq!(
            DEFAULT_SCOPES,
            &["capture:write", "session:read", "memory:read"],
            "the default scopes changed. raw:read and memory:write are the two the design says \
             must be asked for explicitly -- a lost laptop with raw:read hands over every \
             original conversation"
        );
    }
}

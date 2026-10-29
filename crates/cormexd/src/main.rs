//! cormexd — the Cormex daemon.
//!
//! Single writer, always resident. Hosts the memory engine, the background jobs
//! and multi-device sync. CLI / desktop / mobile / web are all HTTP + WebSocket
//! clients of this process — every one of them speaks the same protocol.

// The modules live in `lib.rs`, so that the end-to-end test can reach the
// router. `main` is the process entry point and nothing else.
use cormexd::{auth, cors, credentials, device_keys, routes, state};

use anyhow::Context as _;
use clap::Parser;
use cormex_core::Config;
use tower_http::trace::TraceLayer;

#[derive(Parser, Debug)]
#[command(name = "cormexd", version, about = "Cormex daemon")]
struct Args {
    /// Listen address.
    #[arg(long, env = "CORMEXD_BIND", default_value = "127.0.0.1:8080")]
    bind: String,

    /// Generate a fresh credential, then exit immediately without starting the
    /// service.
    ///
    /// The plaintext exists only inside this one printout: the server keeps the
    /// digest, the client keeps the plaintext, and no single place ever holds
    /// both. See `src/auth.rs`.
    #[arg(long)]
    generate_token: bool,

    /// Create one account, then exit without starting the service.
    ///
    /// Registration is off by default (see `accounts::OPEN_REGISTRATION_ENV`);
    /// this is the path for adding people without opening it. The password is
    /// read from stdin, **never from a command-line argument** — arguments end
    /// up in shell history, in the output of `ps`, and in the container's
    /// startup record.
    #[arg(long, value_name = "USERNAME")]
    create_user: Option<String>,
}

/// The first administrator, taken from `.env`. See [`ensure_admin`].
const ADMIN_USER_ENV: &str = "CORMEX_ADMIN_USERNAME";
const ADMIN_PASSWORD_ENV: &str = "CORMEX_ADMIN_PASSWORD";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Injected from .env during development; production uses real environment
    // variables, so a missing file is not an error
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cormexd=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();

    // Before anything else: generating a credential must not require the
    // database, an API key, or anything else to be ready. It is precisely the
    // first command you run when bringing this thing up for the first time
    if args.generate_token {
        print_new_token();
        return Ok(());
    }

    let config = Config::from_env().context("could not load the configuration")?;

    if let Some(username) = args.create_user.clone() {
        return create_user(&config, &username).await;
    }
    // Auth and confirmation-loop configuration is fixed here. **Misconfiguration
    // exits**, it never degrades: a cormexd that believes auth is on when it is
    // actually off produces no symptom whatsoever (see auth.rs)
    let mut rt = state::Runtime::from_env()
        .context("could not load the authentication / confirmation-loop configuration")?;
    // The accounts pool needs an await, so it is filled in after from_env.
    //
    // **Failing to connect costs you the accounts feature, not startup**: the
    // old pre-shared token path does not depend on it, and a deployment that
    // refuses to come up at all because the accounts table is unreachable is far
    // worse than one that still serves conversation while the login endpoints
    // answer 501
    match state::Accounts::connect(&config.database_url).await {
        Ok(accounts) => {
            tracing::info!("the accounts system is attached (cormex_auth)");
            // Device keys live in `cormex_auth`, so they exist exactly when the
            // accounts database does
            rt.device_keys = Some(std::sync::Arc::new(device_keys::DeviceKeys::new(
                accounts.pool.clone(),
            )));
            rt.accounts = Some(accounts);
        }
        Err(e) => {
            tracing::warn!(error = %e, "the accounts table is unreachable; /auth/* will answer 501 \
                 (the legacy pre-shared token is unaffected)");
        }
    }

    // Same as above: a malformed allow-list fails here, rather than surfacing
    // later as an inscrutable CORS error in the browser
    let cors = cors::CorsPolicy::from_env().context("could not load the cross-origin policy")?;

    // ── Create the first account from .env ──────────────────
    //
    // This replaces the old special case that let the very first account on a
    // deployment through unconditionally. The price of that special case: on a
    // freshly deployed machine that has no account yet and is exposed to the
    // internet, **whoever reaches /auth/register first becomes the owner** —
    // you can outrun that window once by being quick, but not across repeated
    // deployments.
    //
    // With this instead, the window does not exist: the account is created
    // **before the listener opens**, and the credential comes from .env (where
    // ops already has to write POSTGRES_PASSWORD anyway). Grafana / Nextcloud /
    // Keycloak / Portainer all have this same shape.
    if let Some(acc) = rt.accounts.as_ref() {
        ensure_admin(acc).await;
    }

    tracing::info!("{}", rt.auth.status_line());
    tracing::info!("{}", cors.status_line());
    // This used to print `cormex_agent::status_line()` (whether landlock /
    // seccomp were in place). That line describes the capabilities of an
    // **execution** environment, and this process no longer executes anything —
    // printing it would only suggest cormexd still owns the sandbox. That now
    // lives in `cormex-agentd`.

    // The backend is chosen by CORMEX_BACKEND, and **a failed connection never
    // falls back**.
    //
    // Falling back used to be the default ("during development the database is
    // often down, and exiting is more annoying than degrading"). Its consequence
    // in production: one automatic restart lands in the middle of Postgres
    // recovery and cormexd serves traffic backed by a database of fabricated
    // memory — HTTP 200, status: ok, retrieval returning results. Full
    // reasoning in `state::BackendChoice`.
    //
    // To run without a database during development you now have to **say so**:
    // `CORMEX_BACKEND=mock`.
    let state = match state::BackendChoice::from_env()? {
        state::BackendChoice::Mock => {
            tracing::warn!(
                "CORMEX_BACKEND=mock: **every piece of data is fake**. Sessions, memories \
                 and retrieval results are all made up; this is for development only"
            );
            state::AppState::new_mock(config, rt).await
        }
        state::BackendChoice::Live => {
            let st = state::AppState::new_live(&config, rt.clone())
                .await
                // The error has to say **where to go look**. The most common
                // cause here is an unreachable database, and the connection
                // string carries a password, so it cannot be logged whole
                .with_context(|| {
                    format!(
                        "could not attach the real backend (database {}). Check that it \
                         is running and that the address and credentials match; to \
                         develop without a database, set CORMEX_BACKEND=mock",
                        redact_db_url(&config.database_url)
                    )
                })?;
            tracing::info!(
                "attached the real backend: Postgres + {}",
                config.llm.provider
            );
            st
        }
    };

    // The sandbox background jobs (reaping, snapshots, the OOM watchdog, volume
    // quota) are **no longer here** — they went to `cormex-agentd` along with
    // container orchestration. This process has no docker, so it has no
    // containers to reap.

    let app = routes::router(state)
        // No cross-origin request is allowed by default: production is a single
        // domain with path-based routing, so the web app and cormexd are
        // same-origin. Random ports on a development machine are opened up with
        // CORMEX_CORS_ALLOWED_ORIGINS=loopback. See src/cors.rs — in particular
        // how it relates to the short-lived ?ticket= credential
        .layer(cors.layer())
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("could not bind to {}", args.bind))?;

    tracing::info!(addr = %args.bind, version = cormex_core::VERSION, "cormexd is up");

    axum::serve(listener, app)
        .await
        .context("the HTTP service exited abnormally")?;
    Ok(())
}

/// Print a freshly generated credential.
///
/// Uses `println!` rather than `tracing`: these lines are meant to be copied and
/// pasted by a human. They should not be mixed into log output carrying
/// timestamps and level prefixes, and they must not be filterable away by
/// `RUST_LOG`.
fn print_new_token() {
    let (token, digest) = auth::generate();
    println!("# ── server: put this in .env (gitignored, never committed) ──");
    println!("{}={digest}", auth::TOKEN_SHA256_ENV);
    println!();
    println!("# ── client: the plaintext token, **shown this one time only** ──");
    println!("CORMEXD_TOKEN={token}");
    println!();
    println!("# The server keeps only the digest — .env alone cannot log anyone in.");
    println!("# Lose the plaintext and generate a new one — there is no other copy, by design.");
}

/// Strip the credentials out of a database connection string, keeping only
/// "which host it was pointing at".
///
/// An unreachable database is the most common cause of a failed startup, and if
/// the error does not say **which** address, diagnosis goes from "glance at the
/// log" to "go dig through the environment variables on the server". But that
/// string carries a password and cannot be printed whole — it would land in
/// container logs, in CI output, and in the snippet a user pastes into a chat
/// window.
///
/// Per RFC 3986: the authority is everything between `://` and the next `/`,
/// and the userinfo is everything before the **last** `@` inside the authority.
/// "Last" is not pedantry: a password containing `@` is perfectly legal
/// (`postgres://u:p@ss@host/db`), and splitting on the first `@` would leave
/// `ss` in the output.
fn redact_db_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // Not URL-shaped, so nothing can be guaranteed — give away not one
        // character of it
        return "<unparsable connection string>".into();
    };
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if path.is_empty() {
        format!("{scheme}://{host}")
    } else {
        format!("{scheme}://{host}/{path}")
    }
}

/// `--create-user <username>`: create an account, then exit.
///
/// The password is read from stdin. Passed as an argument it would linger in
/// shell history, in the output of `ps`, and in the container's startup record
/// — and it is the plaintext of a **long-lived** credential.
///
/// Creates the administrator from `CORMEX_ADMIN_USERNAME` /
/// `CORMEX_ADMIN_PASSWORD` in `.env`.
///
/// **Only acts when the database holds no account at all**, which makes it
/// idempotent: a hundred restarts still create it once, and changing the
/// password in .env does not overwrite an existing account (that is "change the
/// password", a different operation).
///
/// # Why failure only logs instead of killing the process
///
/// The common causes of a failed account creation are configuration problems
/// like a password that is too short. Refusing to boot over that would take down
/// a deployment that is **otherwise completely healthy** — while the
/// administrator could still have filled the gap with `--create-user`. Say it
/// clearly in the log, then carry on.
///
/// The one thing that is not acceptable is **silence**: without a log line, ops
/// spends a long time staring at a system they cannot log into.
async fn ensure_admin(acc: &state::Accounts) {
    let username = std::env::var(ADMIN_USER_ENV)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let password = std::env::var(ADMIN_PASSWORD_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());

    let (Some(username), Some(password)) = (username, password) else {
        // Only half of the pair being set is a **configuration error**, not
        // "I don't want this feature". Silently ignoring it leaves someone
        // believing the account was created, then staring at a login screen
        // for a long while
        if std::env::var(ADMIN_USER_ENV).is_ok() || std::env::var(ADMIN_PASSWORD_ENV).is_ok() {
            tracing::error!(
                "{ADMIN_USER_ENV} and {ADMIN_PASSWORD_ENV} must be configured as a pair, \
                 and only one half is set — no account was created"
            );
        }
        return;
    };

    let existing: Result<i64, _> = sqlx::query_scalar("SELECT count(*) FROM cormex_auth.users")
        .fetch_one(&acc.pool)
        .await;
    match existing {
        Ok(0) => {}
        Ok(n) => {
            tracing::debug!(
                users = n,
                "accounts already exist; skipping the .env bootstrap"
            );
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "could not count the users; skipping the .env bootstrap");
            return;
        }
    }

    // The first account's schema is simply public — someone who used the system
    // for months before creating an account should not log in afterwards and
    // find their memory empty. Same rule as `AppState::create_account`; reuse it
    // here rather than restating it
    let st_hash = match credentials::hash_password(&password) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!(error = %e, "{ADMIN_PASSWORD_ENV} does not meet the requirements; no account was created");
            return;
        }
    };
    let user_id = cormex_core::Id::new().to_string();
    let r = sqlx::query(
        "INSERT INTO cormex_auth.users (id, username, password_hash, schema_name)
         VALUES ($1, $2, $3, 'public')
         ON CONFLICT DO NOTHING",
    )
    .bind(&user_id)
    .bind(&username)
    .bind(&st_hash)
    .execute(&acc.pool)
    .await;

    match r {
        Ok(res) if res.rows_affected() == 1 => tracing::info!(
            user = %username,
            "created the administrator account from {ADMIN_USER_ENV} / \
             {ADMIN_PASSWORD_ENV} (schema=public). Removing {ADMIN_PASSWORD_ENV} from \
             .env afterwards is recommended — it is only ever read on the first start"
        ),
        Ok(_) => {
            tracing::warn!(user = %username, "that username already exists; nothing was created")
        }
        Err(e) => tracing::error!(error = %e, "could not create the administrator from .env"),
    }
}

async fn create_user(config: &Config, username: &str) -> anyhow::Result<()> {
    use std::io::Write as _;

    print!(
        "Set a password for {username} (it cannot be hidden from the screen — mind who is nearby): "
    );
    std::io::stdout().flush()?;
    let mut password = String::new();
    std::io::stdin().read_line(&mut password)?;
    let password = password.trim_end();

    cormex_store::Store::migrate_global(&config.database_url)
        .await
        .context("the global schema migration failed")?;
    let pools = std::sync::Arc::new(cormex_store::TenantPools::new(&config.database_url));
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&config.database_url)
        .await
        .context("could not connect to the database")?;

    let hash = credentials::hash_password(password)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("the password does not meet the requirements")?;
    let user_id = cormex_core::Id::new().to_string();
    let schema = cormex_store::SchemaName::derive(&user_id).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Schema first, the users row last — the other order leaves behind an
    // account nobody can log into. Same discipline as AppState::create_account
    cormex_store::provision(&pools, &schema)
        .await
        .context("could not provision the database for the new account")?;
    let inserted = sqlx::query(
        "INSERT INTO cormex_auth.users (id, username, password_hash, schema_name)
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(&user_id)
    .bind(username)
    .bind(&hash)
    .bind(schema.as_str())
    .execute(&pool)
    .await
    .context("could not write the user record")?;

    if inserted.rows_affected() == 0 {
        let _ = pools.drop_schema(&schema).await;
        anyhow::bail!("the username {username} is already taken");
    }
    println!(
        "created: {username} (id {user_id}, schema {})",
        schema.as_str()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After redaction, **not one character of the password may remain**.
    ///
    /// The only reason this function exists is to put the address into the log
    /// without putting the password there. Getting it wrong is not a
    /// functionality bug, it is a password in the container log — and those
    /// logs routinely get pasted wholesale into tickets and chat windows.
    #[test]
    fn credentials_never_survive_redaction() {
        assert_eq!(
            redact_db_url("postgres://cormex:hunter2@db.internal:5442/cormex"),
            "postgres://db.internal:5442/cormex"
        );

        // A password containing `@` is legal, and splitting on the **first** `@`
        // leaves behind whatever follows it. This is the one place in this
        // function that is easy to get wrong
        let tricky = redact_db_url("postgres://u:p@ss:w0rd@db.internal:5442/cormex");
        assert_eq!(tricky, "postgres://db.internal:5442/cormex");
        assert!(
            !tricky.contains("ss") && !tricky.contains("w0rd"),
            "a fragment of the password leaked through: {tricky}"
        );

        // With no credentials present, the address must not be lost as well
        assert_eq!(
            redact_db_url("postgres://db.internal:5442/cormex"),
            "postgres://db.internal:5442/cormex"
        );
        // No path
        assert_eq!(redact_db_url("postgres://u:p@host"), "postgres://host");

        // If the shape is unrecognised, say nothing at all. **Returning it
        // verbatim is not an option** — a string we cannot parse may well be
        // entirely credential
        assert_eq!(
            redact_db_url("not-a-url:p@ssword"),
            "<unparsable connection string>"
        );
    }
}

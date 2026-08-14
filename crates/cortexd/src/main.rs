//! cortexd — the Cortex daemon.
//!
//! Single writer, always resident. Hosts the memory engine, the background jobs
//! and multi-device sync. CLI / desktop / mobile / web are all HTTP + WebSocket
//! clients of this process — every one of them speaks the same protocol.

mod accounts;
mod auth;
mod backfill;
mod blobs;
mod byo_key;
mod cors;
mod credentials;
mod cursor;
mod delegated_token;
mod import;
mod live;
/// Memory's outward-facing MCP frontage. See that module's header comment.
mod mcp;
mod quota;
mod reembed;
mod request_tenant;
mod routes;
mod snapshots;
mod state;
mod sync_notify;
mod sync_payload;
mod ws;

// The wire protocol lives in `cortex-proto`, because the agent side has to
// speak the **same** protocol (see that crate's module comment). Re-exporting
// it here as `crate::dto` keeps every existing call site untouched while
// confining "the protocol is not cortexd's private property" to this one line.
//
// `confirm` is no longer re-exported: that ledger moved to `cortex-local`.
// cortexd does not run an agent, so it has no confirmation loop. A re-export
// that resolves to no call site at all would only make the next person believe
// a copy still lives here.
pub use cortex_proto::dto;

use anyhow::Context as _;
use clap::Parser;
use cortex_core::Config;
use tower_http::trace::TraceLayer;

#[derive(Parser, Debug)]
#[command(name = "cortexd", version, about = "Cortex 守护进程")]
struct Args {
    /// Listen address.
    #[arg(long, env = "CORTEXD_BIND", default_value = "127.0.0.1:8080")]
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
    #[arg(long, value_name = "用户名")]
    create_user: Option<String>,
}

/// The first administrator, taken from `.env`. See [`ensure_admin`].
const ADMIN_USER_ENV: &str = "CORTEX_ADMIN_USERNAME";
const ADMIN_PASSWORD_ENV: &str = "CORTEX_ADMIN_PASSWORD";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Injected from .env during development; production uses real environment
    // variables, so a missing file is not an error
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cortexd=info,tower_http=info".into()),
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

    let config = Config::from_env().context("加载配置失败")?;

    if let Some(username) = args.create_user.clone() {
        return create_user(&config, &username).await;
    }
    // Auth and confirmation-loop configuration is fixed here. **Misconfiguration
    // exits**, it never degrades: a cortexd that believes auth is on when it is
    // actually off produces no symptom whatsoever (see auth.rs)
    let mut rt = state::Runtime::from_env().context("加载认证 / 确认回路配置失败")?;
    // The accounts pool needs an await, so it is filled in after from_env.
    //
    // **Failing to connect costs you the accounts feature, not startup**: the
    // old pre-shared token path does not depend on it, and a deployment that
    // refuses to come up at all because the accounts table is unreachable is far
    // worse than one that still serves conversation while the login endpoints
    // answer 501
    match state::Accounts::connect(&config.database_url).await {
        Ok(accounts) => {
            tracing::info!("账号体系已接入（cortex_auth）");
            rt.accounts = Some(accounts);
        }
        Err(e) => {
            tracing::warn!(error = %e, "账号表连不上，/auth/* 将回 501（老的预共享 token 不受影响）");
        }
    }

    // Same as above: a malformed allow-list fails here, rather than surfacing
    // later as an inscrutable CORS error in the browser
    let cors = cors::CorsPolicy::from_env().context("加载跨源策略失败")?;

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
    // This used to print `cortex_agent::status_line()` (whether landlock /
    // seccomp were in place). That line describes the capabilities of an
    // **execution** environment, and this process no longer executes anything —
    // printing it would only suggest cortexd still owns the sandbox. That now
    // lives in `cortex-agentd`.

    // The backend is chosen by CORTEX_BACKEND, and **a failed connection never
    // falls back**.
    //
    // Falling back used to be the default ("during development the database is
    // often down, and exiting is more annoying than degrading"). Its consequence
    // in production: one automatic restart lands in the middle of Postgres
    // recovery and cortexd serves traffic backed by a database of fabricated
    // memory — HTTP 200, status: ok, retrieval returning results. Full
    // reasoning in `state::BackendChoice`.
    //
    // To run without a database during development you now have to **say so**:
    // `CORTEX_BACKEND=mock`.
    let state = match state::BackendChoice::from_env()? {
        state::BackendChoice::Mock => {
            tracing::warn!(
                "CORTEX_BACKEND=mock：**全部数据都是假的**。\
                 会话、记忆、检索结果都是编出来的，只用于开发"
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
                        "接入真实后端失败（数据库 {}）。\
                         确认它在跑、地址与凭据对得上；\
                         开发时想不带数据库跑就设 CORTEX_BACKEND=mock",
                        redact_db_url(&config.database_url)
                    )
                })?;
            tracing::info!("已接入真实后端：Postgres + {}", config.llm.provider);
            st
        }
    };

    // The sandbox background jobs (reaping, snapshots, the OOM watchdog, volume
    // quota) are **no longer here** — they went to `cortex-agentd` along with
    // container orchestration. This process has no docker, so it has no
    // containers to reap.

    let app = routes::router(state)
        // No cross-origin request is allowed by default: production is a single
        // domain with path-based routing, so the web app and cortexd are
        // same-origin. Random ports on a development machine are opened up with
        // CORTEX_CORS_ALLOWED_ORIGINS=loopback. See src/cors.rs — in particular
        // how it relates to the short-lived ?ticket= credential
        .layer(cors.layer())
        .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&args.bind)
        .await
        .with_context(|| format!("无法绑定到 {}", args.bind))?;

    tracing::info!(addr = %args.bind, version = cortex_core::VERSION, "cortexd 已启动");

    axum::serve(listener, app)
        .await
        .context("HTTP 服务异常退出")?;
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
    println!("# ── 服务端：写进 .env（已被 gitignore 排除，绝不入库）──");
    println!("{}={digest}", auth::TOKEN_SHA256_ENV);
    println!();
    println!("# ── 客户端：明文 token，**只显示这一次** ──");
    println!("CORTEXD_TOKEN={token}");
    println!();
    println!("# 服务端只存摘要：.env 被读走也拿不到能用来登录的东西。");
    println!("# 丢了明文就重新生成一份 —— 它没有别的副本，这是有意的。");
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
        return "<无法解析的连接串>".into();
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
/// Creates the administrator from `CORTEX_ADMIN_USERNAME` /
/// `CORTEX_ADMIN_PASSWORD` in `.env`.
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
                "{ADMIN_USER_ENV} 与 {ADMIN_PASSWORD_ENV} 必须成对配置，现在只有一半 ——                  没有建任何账号"
            );
        }
        return;
    };

    let existing: Result<i64, _> = sqlx::query_scalar("SELECT count(*) FROM cortex_auth.users")
        .fetch_one(&acc.pool)
        .await;
    match existing {
        Ok(0) => {}
        Ok(n) => {
            tracing::debug!(users = n, "已经有账号了，跳过 .env 建号");
            return;
        }
        Err(e) => {
            tracing::error!(error = %e, "数不出用户数，跳过 .env 建号");
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
            tracing::error!(error = %e, "{ADMIN_PASSWORD_ENV} 不合规，没有建账号");
            return;
        }
    };
    let user_id = cortex_core::Id::new().to_string();
    let r = sqlx::query(
        "INSERT INTO cortex_auth.users (id, username, password_hash, schema_name)
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
            "已按 {ADMIN_USER_ENV} / {ADMIN_PASSWORD_ENV} 建好管理员账号（schema=public）。             建议建完之后把 {ADMIN_PASSWORD_ENV} 从 .env 里删掉 ——              它只在第一次启动时被用到"
        ),
        Ok(_) => tracing::warn!(user = %username, "这个用户名已经存在，没有新建"),
        Err(e) => tracing::error!(error = %e, "按 .env 建管理员失败"),
    }
}

async fn create_user(config: &Config, username: &str) -> anyhow::Result<()> {
    use std::io::Write as _;

    print!("给 {username} 设个密码（不回显不了，注意旁边有没有人）：");
    std::io::stdout().flush()?;
    let mut password = String::new();
    std::io::stdin().read_line(&mut password)?;
    let password = password.trim_end();

    cortex_store::Store::migrate_global(&config.database_url)
        .await
        .context("全局 schema 迁移失败")?;
    let pools = std::sync::Arc::new(cortex_store::TenantPools::new(&config.database_url));
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&config.database_url)
        .await
        .context("连不上数据库")?;

    let hash = credentials::hash_password(password)
        .map_err(|e| anyhow::anyhow!("{e}"))
        .context("密码不合要求")?;
    let user_id = cortex_core::Id::new().to_string();
    let schema = cortex_store::SchemaName::derive(&user_id).map_err(|e| anyhow::anyhow!("{e}"))?;

    // Schema first, the users row last — the other order leaves behind an
    // account nobody can log into. Same discipline as AppState::create_account
    cortex_store::provision(&pools, &schema)
        .await
        .context("给新账号开库失败")?;
    let inserted = sqlx::query(
        "INSERT INTO cortex_auth.users (id, username, password_hash, schema_name)
         VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(&user_id)
    .bind(username)
    .bind(&hash)
    .bind(schema.as_str())
    .execute(&pool)
    .await
    .context("写用户记录失败")?;

    if inserted.rows_affected() == 0 {
        let _ = pools.drop_schema(&schema).await;
        anyhow::bail!("用户名 {username} 已经有人用了");
    }
    println!(
        "已创建：{username}（id {user_id}，schema {}）",
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
            redact_db_url("postgres://cortex:hunter2@db.internal:5442/cortex"),
            "postgres://db.internal:5442/cortex"
        );

        // A password containing `@` is legal, and splitting on the **first** `@`
        // leaves behind whatever follows it. This is the one place in this
        // function that is easy to get wrong
        let tricky = redact_db_url("postgres://u:p@ss:w0rd@db.internal:5442/cortex");
        assert_eq!(tricky, "postgres://db.internal:5442/cortex");
        assert!(
            !tricky.contains("ss") && !tricky.contains("w0rd"),
            "a fragment of the password leaked through: {tricky}"
        );

        // With no credentials present, the address must not be lost as well
        assert_eq!(
            redact_db_url("postgres://db.internal:5442/cortex"),
            "postgres://db.internal:5442/cortex"
        );
        // No path
        assert_eq!(redact_db_url("postgres://u:p@host"), "postgres://host");

        // If the shape is unrecognised, say nothing at all. **Returning it
        // verbatim is not an option** — a string we cannot parse may well be
        // entirely credential
        assert_eq!(
            redact_db_url("这不是一个 URL:p@ssword"),
            "<无法解析的连接串>"
        );
    }
}

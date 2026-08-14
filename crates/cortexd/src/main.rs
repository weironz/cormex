//! cortexd —— Cortex 守护进程。
//!
//! 单写者、常驻。承载记忆引擎、后台任务与多端同步。
//! CLI / 桌面 / 移动 / Web 全部是它的 HTTP + WebSocket 客户端 —— 走同一套协议。

mod accounts;
mod auth;
mod backfill;
mod blobs;
mod byo_key;
mod cors;
mod credentials;
mod cursor;
mod import;
mod live;
/// 记忆对外的 MCP 门面。见那个模块的头注释
mod mcp;
mod quota;
mod reembed;
mod request_tenant;
mod routes;
mod sandbox_token;
mod snapshots;
mod state;
mod sync_notify;
mod sync_payload;
mod ws;

// 线协议住在 `cortex-proto`，因为 agent 那一侧要讲**同一套**协议
// （见那个 crate 的模块注释）。这里 re-export 成 `crate::dto`，
// 既保持既有调用点不动，也让「协议不是 cortexd 私有的」这件事只体现在这一行上。
//
// `confirm` 不再 re-export：那本簿子搬去 `cortex-local` 了，cortexd 不跑
// agent，也就没有确认回路。留一个指不到任何调用点的 re-export，
// 只会让下一个人以为这儿还有一份。
pub use cortex_proto::dto;

use anyhow::Context as _;
use clap::Parser;
use cortex_core::Config;
use tower_http::trace::TraceLayer;

#[derive(Parser, Debug)]
#[command(name = "cortexd", version, about = "Cortex 守护进程")]
struct Args {
    /// 监听地址
    #[arg(long, env = "CORTEXD_BIND", default_value = "127.0.0.1:8080")]
    bind: String,

    /// 生成一份新凭据后立即退出，不启动服务。
    ///
    /// 明文只在这一次输出里存在：服务端拿摘要，客户端拿明文，
    /// 没有任何一处同时持有两者。见 `src/auth.rs`。
    #[arg(long)]
    generate_token: bool,

    /// 建一个账号后退出，不启动服务。
    ///
    /// 注册默认是关的（见 `accounts::OPEN_REGISTRATION_ENV`），这是不开注册
    /// 也能加人的那条路。密码从 stdin 读，**不走命令行参数** ——
    /// 参数会进 shell history、进 `ps` 的输出、进容器的启动记录。
    #[arg(long, value_name = "用户名")]
    create_user: Option<String>,
}

/// `.env` 里的第一个管理员。见 [`ensure_admin`]。
const ADMIN_USER_ENV: &str = "CORTEX_ADMIN_USERNAME";
const ADMIN_PASSWORD_ENV: &str = "CORTEX_ADMIN_PASSWORD";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 开发期从 .env 注入；生产环境用真实环境变量，缺失不算错误
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cortexd=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();

    // 先于一切：生成凭据这条路不该要求数据库、API key 或任何别的东西就绪。
    // 它恰恰是「第一次把这个东西跑起来」时要执行的第一条命令
    if args.generate_token {
        print_new_token();
        return Ok(());
    }

    let config = Config::from_env().context("加载配置失败")?;

    if let Some(username) = args.create_user.clone() {
        return create_user(&config, &username).await;
    }
    // 认证与确认回路的配置在这里定型。**配错就退出**，不降级：
    // 一个「以为开着认证其实关着」的 cortexd 不会有任何症状（见 auth.rs）
    let mut rt = state::Runtime::from_env().context("加载认证 / 确认回路配置失败")?;
    // 账号池要 await，所以在 from_env 之后补上。
    //
    // **连不上就没有账号功能，而不是启动失败**：老的预共享 token 那条路
    // 不依赖它，而一个「因为账号表连不上所以整个服务不起来」的部署，
    // 比一个「能对话但登录端点回 501」的部署糟得多
    match state::Accounts::connect(&config.database_url).await {
        Ok(accounts) => {
            tracing::info!("账号体系已接入（cortex_auth）");
            rt.accounts = Some(accounts);
        }
        Err(e) => {
            tracing::warn!(error = %e, "账号表连不上，/auth/* 将回 501（老的预共享 token 不受影响）");
        }
    }

    // 同上：写错的允许列表在这里失败，而不是等浏览器报一句语焉不详的 CORS 错
    let cors = cors::CorsPolicy::from_env().context("加载跨源策略失败")?;

    // ── 按 .env 建第一个账号 ────────────────────────────────
    //
    // 这一步替掉了原先那条「本部署的第一个账号无条件放行」的特例。
    // 那条特例的代价是：一台刚部署好、还没建号、又暴露在公网上的机器，
    // **第一个访问 /auth/register 的人会成为主人** —— 部署一次能靠手快
    // 躲过去，反复部署躲不过去。
    //
    // 换成这条之后那个窗口不存在了：账号在**开始监听之前**就建好了，
    // 而凭据来自 .env（运维本来就要在那里写 POSTGRES_PASSWORD）。
    // Grafana / Nextcloud / Keycloak / Portainer 都是这个形态。
    if let Some(acc) = rt.accounts.as_ref() {
        ensure_admin(acc).await;
    }

    tracing::info!("{}", rt.auth.status_line());
    tracing::info!("{}", cors.status_line());
    // 这里曾经印 `cortex_agent::status_line()`（landlock / seccomp 装没装）。
    // 那一行说的是**执行环境**的能力，而这个进程不执行任何东西了 ——
    // 印出来只会让人以为 cortexd 还管着沙箱。它现在在 `cortex-agentd` 上。

    // 后端由 CORTEX_BACKEND 决定，**连不上不回落**。
    //
    // 回落曾经是默认行为（「开发期数据库经常没起，退出比降级碍事」），
    // 而它在生产上的后果是：一次自动重启撞上 Postgres 恢复期，cortexd 就带着
    // 一库假记忆对外服务，且 HTTP 200、status: ok、检索有结果 —— 完整理由
    // 见 `state::BackendChoice`。
    //
    // 开发期想不带数据库跑，现在要**明说** `CORTEX_BACKEND=mock`。
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
                // 报错要说清**去查哪儿**。这条最常见的原因是数据库连不上，
                // 而连接串里有密码，不能整条打进日志
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

    // 沙箱那几个后台任务（回收、快照、OOM 守望、卷配额）**不在这儿了** ——
    // 它们跟着容器编排一起去了 `cortex-agentd`。这个进程没有 docker，
    // 也就没有容器可回收。

    let app = routes::router(state)
        // 默认一个跨源都不许：生产是单域名路径分流，Web 与 cortexd 同源。
        // 开发机上的随机端口用 CORTEX_CORS_ALLOWED_ORIGINS=loopback 放开。
        // 见 src/cors.rs —— 尤其是它与 ?ticket= 短命票据的关系
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

/// 打印一份新凭据。
///
/// 走 `println!` 而不是 `tracing`：这两行是要被人复制粘贴的东西，
/// 不该混在带时间戳和级别前缀的日志里，也不该被 `RUST_LOG` 过滤掉。
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

/// 把数据库连接串里的凭据抹掉，只留「连的是哪儿」。
///
/// 连不上数据库是启动失败最常见的原因，而报错里不写清是**哪个**地址，
/// 排查就从「看一眼日志」变成「去翻服务器上的环境变量」。可那条串里
/// 有密码，不能整条打出去 —— 它会进容器日志、进 CI 输出、进用户贴到
/// 聊天窗口里的那一段。
///
/// 按 RFC 3986：authority 是 `://` 到下一个 `/` 之间的部分，
/// 而 userinfo 是 authority 里**最后一个** `@` 之前的一切。
/// 「最后一个」这条不是吹毛求疵：密码里带 `@` 完全合法
/// （`postgres://u:p@ss@host/db`），而按第一个 `@` 切会把 `ss` 留在输出里。
fn redact_db_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        // 不是 URL 形状，什么都不敢保证 —— 一个字都不给
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

/// `--create-user <用户名>`：建号后退出。
///
/// 密码从 stdin 读。走参数的话它会留在 shell history、`ps` 的输出、
/// 以及容器的启动记录里 —— 而那是个**长期**凭据的明文。
/// 按 `.env` 里的 `CORTEX_ADMIN_USERNAME` / `CORTEX_ADMIN_PASSWORD` 建管理员。
///
/// **只在库里一个账号都没有时动手**，所以它是幂等的：重启一百次也只建一次，
/// 改了 .env 里的密码也不会覆盖已有账号（那是「改密码」，另一件事）。
///
/// # 为什么失败只记日志，不让进程退出
///
/// 建号失败的常见原因是密码太短这类配置问题。为它拒绝启动，会让一个
/// **其余部分完全健康**的部署整个起不来 —— 而管理员本来还可以用
/// `--create-user` 补上。日志里说清楚，然后继续。
///
/// 唯一不能接受的是**静默**：不打日志的话，运维会对着一个登录不进去的
/// 系统查很久。
async fn ensure_admin(acc: &state::Accounts) {
    let username = std::env::var(ADMIN_USER_ENV)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let password = std::env::var(ADMIN_PASSWORD_ENV)
        .ok()
        .filter(|v| !v.trim().is_empty());

    let (Some(username), Some(password)) = (username, password) else {
        // 只配了一半是**配置错误**，不是「不想用这个功能」。
        // 静默忽略会让人以为账号建好了，然后对着登录界面查半天
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

    // 第一个账号的 schema 就是 public —— 一个先用了几个月才建号的人，
    // 建完号一登录不该发现记忆全空了。与 `AppState::create_account`
    // 同一条规则，这里复用它而不是抄一遍
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

    // schema 先行、users 行最后 —— 反过来会留下一个登不进去的账号。
    // 与 AppState::create_account 同一条纪律
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

    /// 抹掉凭据之后，**一个字符的密码都不能留下**。
    ///
    /// 这个函数存在的唯一理由就是往日志里写地址而不写密码。
    /// 写错的后果不是功能不对，是密码进了容器日志 —— 而那种日志
    /// 经常被整段贴进工单和聊天窗口。
    #[test]
    fn credentials_never_survive_redaction() {
        assert_eq!(
            redact_db_url("postgres://cortex:hunter2@db.internal:5442/cortex"),
            "postgres://db.internal:5442/cortex"
        );

        // 密码里带 `@` 是合法的，而按**第一个** `@` 切会把 `ss@` 之前那段
        // 之外的残片留下来。这是这个函数唯一容易写错的地方
        let tricky = redact_db_url("postgres://u:p@ss:w0rd@db.internal:5442/cortex");
        assert_eq!(tricky, "postgres://db.internal:5442/cortex");
        assert!(
            !tricky.contains("ss") && !tricky.contains("w0rd"),
            "密码残片漏了出来：{tricky}"
        );

        // 没有凭据时不该把地址也弄没
        assert_eq!(
            redact_db_url("postgres://db.internal:5442/cortex"),
            "postgres://db.internal:5442/cortex"
        );
        // 没有路径
        assert_eq!(redact_db_url("postgres://u:p@host"), "postgres://host");

        // 形状不认识就什么都不说。**不能原样返回** —— 一个我们看不懂的串，
        // 完全可能整条都是凭据
        assert_eq!(
            redact_db_url("这不是一个 URL:p@ssword"),
            "<无法解析的连接串>"
        );
    }
}

//! 账号：注册 / 登录 / 刷新 / 登出，以及它们背后的那几张表。
//!
//! 纯逻辑（密码哈希、令牌生成、重放判定）在 [`crate::credentials`]；
//! 这里只做「跟数据库打交道」与「拼成 HTTP」。
//!
//! # 会话为什么能一直用下去
//!
//! 两种令牌，寿命差两个数量级：
//!
//! - **access token**：15 分钟。每次请求都带，泄露的窗口因此很短。
//! - **refresh token**：30 天。只在换 access token 时用一次，用完就轮转。
//!
//! 客户端把 refresh token 存进系统凭据库，于是「关掉应用再打开」不需要
//! 重新输密码 —— 它拿旧的换一对新的就继续用。**这是「不用反复登录」
//! 的全部机制**，而它成立的前提是客户端真的把那串东西存下来
//! （见 `app/lib/auth/`）。服务端发了长效令牌而客户端不存，等于没做。
//!
//! # 轮转与重放
//!
//! 每次刷新都签一个新的、把旧的标记 `rotated_at`。拿一个已经轮转过的
//! 来换 = 它泄露了（合法客户端手上只有最新那个），此时**整条 family
//! 一起作废** —— 只废这一个不够，攻击者手上还有他刚换到的那个。
//!
//! 判定本身在 [`crate::credentials::judge_refresh`]，那里有三态与它们的理由。

use axum::{Json, extract::State};
use chrono::{Duration, Utc};
use cortex_proto::auth::{AuthTokens, LoginRequest, RefreshRequest, RegisterRequest, WhoAmI};
use sqlx::Row as _;

use crate::credentials::{
    RefreshRecord, RefreshVerdict, RejectReason, generate_refresh, hash_password, judge_refresh,
    refresh_digest, verify_password,
};
use crate::routes::ApiError;
use crate::state::AppState;

/// access token 的寿命。
///
/// 短到「泄露也没多大用」，长到「不至于每几分钟就为刷新往返一次」。
/// 15 分钟是这两者的常见折衷，没有更深的理由 —— 真正兜底的是刷新那条链
/// 能被整体作废。
const ACCESS_TTL: Duration = Duration::minutes(15);

/// refresh token 的寿命。
///
/// **这个数就是「多久不用就要重新登录」。** 30 天意味着日常使用的人
/// 永远不会再看到登录框（每次刷新都会续期），而一台放了一个月没开的
/// 机器要重新输密码 —— 那正是我们想要的那条线。
const REFRESH_TTL: Duration = Duration::days(30);

/// 注册默认是**关**的。
///
/// 开放注册意味着任何人都能拿服务端那把 DeepSeek key 烧钱，而生产是
/// 一台 2 核 3.5 GB、已经跑着 19 个容器的机器。
///
/// 取值刻意不是 `1` / `true` 而是这个字面量，与 `CORTEX_AUTH=disabled`
/// 同款：一个手滑设成 `0` 的环境变量不该把门打开。
const OPEN_REGISTRATION_ENV: &str = "CORTEX_OPEN_REGISTRATION";
const OPEN_REGISTRATION_ON: &str = "enabled";

fn open_registration() -> bool {
    std::env::var(OPEN_REGISTRATION_ENV).is_ok_and(|v| v.trim() == OPEN_REGISTRATION_ON)
}

impl AppState {
    /// 建一个账号：开 schema、迁移、写 users 行。
    ///
    /// # 顺序不能反
    ///
    /// 先建 schema 并迁移，**最后**才写 users 那一行。反过来会留下
    /// 「账号存在但库是空的」这个中间态，而那个用户一登录就撞上一片没有
    /// 表的 schema —— 报的是看不懂的 SQL 错误，不是「你的账号还没建好」。
    ///
    /// # Errors
    /// 用户名已存在、密码太短、建 schema 失败、migration 失败。
    pub async fn create_account(&self, username: &str, password: &str) -> Result<String, ApiError> {
        let hash = hash_password(password).map_err(|e| ApiError::bad_request(e.to_string()))?;
        let user_id = cortex_core::Id::new().to_string();

        // **第一个账号落在 `public` 上，不另开 schema。**
        //
        // 那片库里已经有东西了 —— 一个自托管的人是先用了几个月、
        // 后来才建账号的。给他也 derive 一个新 schema，等于他建完号
        // 一登录，几个月的记忆全部消失（库还在，只是没有任何一条路
        // 通向它了）。而这**不报错**：会话列表就是空的，看着像新装的。
        //
        // 端到端跑一次才发现：老 token 建号之后指向了 `u_01kz…`，
        // 而存量数据在 `public`。设计里本来就写着「1 号用户的 schema 名
        // 就叫 public，存量数据不搬家」，是实现漏了这一支。
        let schema = schema_for_new_account(&user_id, self.user_count().await? == 0)?;

        // schema 先行。失败时还没有任何账号记录，重试即可 ——
        // 而反过来会留下一个登不进去的账号
        cortex_store::provision(self.accounts()?.tenants.as_ref(), &schema)
            .await
            .map_err(|e| ApiError::internal(format!("给新账号开库失败：{e}")))?;

        let inserted = sqlx::query(
            "INSERT INTO cortex_auth.users (id, username, password_hash, schema_name)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(&user_id)
        .bind(username)
        .bind(&hash)
        .bind(schema.as_str())
        .execute(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("写用户记录失败：{e}")))?;

        if inserted.rows_affected() == 0 {
            // schema 已经建了，但账号没写成。删掉它，别留孤儿 ——
            // 一片没人认领的 schema 会一直占着 catalog，而且下次用同一个
            // 用户名注册时又会建一片新的
            let _ = self.accounts()?.tenants.as_ref().drop_schema(&schema).await;
            return Err(ApiError::bad_request("这个用户名已经有人用了"));
        }
        Ok(user_id)
    }

    /// 这个部署有几个账号。
    ///
    /// 只用来判断「是不是第一个」，所以不缓存 —— 它每次注册请求跑一次，
    /// 而注册在这个产品里是极低频动作。
    ///
    /// # Errors
    /// 没接数据库，或者查不动。
    pub async fn user_count(&self) -> Result<i64, ApiError> {
        let row = sqlx::query("SELECT count(*) AS n FROM cortex_auth.users")
            .fetch_one(&self.accounts()?.pool)
            .await
            .map_err(|e| ApiError::internal(format!("数不出用户数：{e}")))?;
        Ok(row.get("n"))
    }

    /// 签一对新令牌。`family` 为 `None` 时开一条新链（登录）。
    async fn issue(
        &self,
        user_id: &str,
        family: Option<String>,
        device: Option<&str>,
    ) -> Result<AuthTokens, ApiError> {
        let (plain, digest) = generate_refresh();
        let family = family.unwrap_or_else(|| cortex_core::Id::new().to_string());
        let expires = Utc::now() + REFRESH_TTL;

        sqlx::query(
            "INSERT INTO cortex_auth.auth_tokens
                 (id, user_id, token_sha256, family_id, expires_at, device_label)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(cortex_core::Id::new().to_string())
        .bind(user_id)
        .bind(&digest)
        .bind(&family)
        .bind(expires)
        .bind(device)
        .execute(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("签发令牌失败：{e}")))?;

        Ok(AuthTokens {
            access_token: self.access_book().issue(
                user_id,
                std::time::Duration::from_secs(
                    u64::try_from(ACCESS_TTL.num_seconds()).unwrap_or(900),
                ),
            ),
            access_expires_in_secs: u64::try_from(ACCESS_TTL.num_seconds()).unwrap_or(900),
            refresh_token: plain,
            refresh_expires_in_secs: u64::try_from(REFRESH_TTL.num_seconds()).unwrap_or(2_592_000),
            user_id: user_id.to_owned(),
        })
    }

    /// 用 refresh token 换一对新的，并把旧的标记为已轮转。
    ///
    /// # Errors
    /// 令牌不认识、已过期、已作废，或者**被重放**（此时整条 family 作废）。
    pub async fn rotate(&self, plain: &str) -> Result<AuthTokens, ApiError> {
        let digest = refresh_digest(plain);
        let row = sqlx::query(
            "SELECT id, user_id, family_id, rotated_at IS NOT NULL AS rotated,
                    revoked_at IS NOT NULL AS revoked, expires_at < now() AS expired
               FROM cortex_auth.auth_tokens WHERE token_sha256 = $1",
        )
        .bind(&digest)
        .fetch_optional(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("查令牌失败：{e}")))?;

        // 查不到就是查不到。**不要**告诉客户端「这个令牌不存在」与
        // 「这个令牌过期了」的区别 —— 那等于给爆破者一个进度条
        let Some(row) = row else {
            return Err(ApiError::unauthorized("登录已失效，请重新登录"));
        };

        let family_raw: String = row.get("family_id");
        let record = RefreshRecord {
            family: family_raw
                .parse()
                .map_err(|_| ApiError::internal("库里的 family_id 不是合法 ULID"))?,
            rotated: row.get("rotated"),
            revoked: row.get("revoked"),
            expired: row.get("expired"),
        };
        let user_id: String = row.get("user_id");
        let token_id: String = row.get("id");

        match judge_refresh(&record) {
            RefreshVerdict::RevokeFamily { family } => {
                // 这一条已经被换过了，而合法客户端手上只有最新那个 ——
                // 所以这次请求来自别人。整条链作废，两边都得重新登录
                let family = family.to_string();
                let n = self.revoke_family(&family).await;
                tracing::warn!(
                    user = %user_id,
                    family = %family,
                    revoked = n,
                    "检测到 refresh token 重放，已作废整条链"
                );
                Err(ApiError::unauthorized(
                    "这个登录凭据已经被使用过了。出于安全，该设备上的所有会话都已注销，请重新登录",
                ))
            }
            RefreshVerdict::Reject(why) => Err(ApiError::unauthorized(match why {
                RejectReason::Expired => "登录已过期，请重新登录",
                RejectReason::FamilyRevoked => "登录已注销，请重新登录",
            })),
            RefreshVerdict::Rotate => {
                sqlx::query("UPDATE cortex_auth.auth_tokens SET rotated_at = now() WHERE id = $1")
                    .bind(&token_id)
                    .execute(&self.accounts()?.pool)
                    .await
                    .map_err(|e| ApiError::internal(format!("轮转令牌失败：{e}")))?;
                self.issue(&user_id, Some(record.family.to_string()), None)
                    .await
            }
        }
    }

    /// 作废整条链，返回作废了几条。
    async fn revoke_family(&self, family: &str) -> u64 {
        let Ok(acc) = self.accounts() else { return 0 };
        sqlx::query(
            "UPDATE cortex_auth.auth_tokens SET revoked_at = now()
              WHERE family_id = $1 AND revoked_at IS NULL",
        )
        .bind(family)
        .execute(&acc.pool)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0)
    }
}

/// 已签发的 access token → (用户 id, 过期时刻)。
///
/// # 为什么在内存里，不落库、也不签 JWT
///
/// 落库意味着**每个请求一次数据库往返**，而 access token 每个请求都带。
/// JWT 要引一套签名依赖，还要管密钥轮转 —— 而它换来的「无状态校验」
/// 对一个单进程部署没有价值。
///
/// 代价是**重启之后所有 access token 失效**。这不痛：refresh token 在库里
/// 活得好好的，客户端撞到一次 401 就用它换一对新的，用户什么都感觉不到。
/// 这也正是「两种令牌」这个设计顺带买到的东西。
#[derive(Default)]
pub struct AccessBook {
    inner: std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
}

impl AccessBook {
    fn issue(&self, user_id: &str, ttl: std::time::Duration) -> String {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect("内核熵源不可用，拒绝签发可预测的令牌");
        let token = hex::encode(buf);
        let mut g = self.inner.lock().expect("access 簿子的锁不该中毒");
        // 顺手清一次过期的。挂定时器要多一个后台任务与它的失败路径，
        // 而这条路本来就是「有人登录/刷新」时才走
        let now = std::time::Instant::now();
        g.retain(|_, (_, exp)| *exp > now);
        g.insert(token.clone(), (user_id.to_owned(), now + ttl));
        token
    }

    /// 这个 token 属于谁。过期或不认识都返回 `None`。
    /// 只给测试用的签发口。
    ///
    /// `issue` 是私有的（签发要走完整的登录流程），但认证中间件那条不变式
    /// 需要一张真的、簿子认得的票 —— 手搓一串十六进制测不到任何东西。
    #[cfg(test)]
    pub fn issue_for_test(&self, user_id: &str, ttl: std::time::Duration) -> String {
        self.issue(user_id, ttl)
    }

    pub fn resolve(&self, token: &str) -> Option<String> {
        let g = self.inner.lock().expect("access 簿子的锁不该中毒");
        let (user, exp) = g.get(token)?;
        (*exp > std::time::Instant::now()).then(|| user.clone())
    }
}

/// 新账号落在哪个 schema。
///
/// # 第一个账号必须是 `public`
///
/// 那片库里通常已经有东西 —— 自托管的人是先用了几个月、后来才建账号的。
/// 给他也 derive 一个新 schema，等于建完号一登录，几个月的记忆全部消失：
/// 库还在，只是没有任何一条路通向它了。而这**不报错**，会话列表就是空的，
/// 看着像新装的。
///
/// 端到端跑一次才发现的（老 token 建号之后指向了 `u_01kz…`，
/// 而存量数据在 `public`）。设计里本来就写着「1 号用户的 schema 名就叫
/// public，存量数据不搬家」，是实现漏了这一支。
///
/// # Errors
/// `user_id` 不是合法 ULID。
fn schema_for_new_account(
    user_id: &str,
    is_first: bool,
) -> Result<cortex_store::SchemaName, ApiError> {
    if is_first {
        return Ok(cortex_store::SchemaName::public());
    }
    cortex_store::SchemaName::derive(user_id).map_err(|e| ApiError::internal(e.to_string()))
}

/// `POST /auth/login`
pub async fn login(
    State(st): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthTokens>, ApiError> {
    let row = sqlx::query(
        "SELECT id, password_hash, disabled_at IS NOT NULL AS disabled
           FROM cortex_auth.users WHERE lower(username) = lower($1)",
    )
    .bind(&req.username)
    .fetch_optional(&st.accounts()?.pool)
    .await
    .map_err(|e| ApiError::internal(format!("查用户失败：{e}")))?;

    // 用户名不存在与密码错误回**同一句话**：区分开来等于送人一份用户名字典。
    //
    // 时间侧信道这里不做等时处理：真要防，得对不存在的用户也跑一遍 argon2，
    // 而那会把「随便猜个用户名」变成一次 16ms 的 CPU 消耗 —— 那是个更好用的
    // DoS 放大器。取舍写在这里，别当成漏了
    let deny = || ApiError::unauthorized("用户名或密码不对");
    let Some(row) = row else { return Err(deny()) };
    if row.get::<bool, _>("disabled") {
        return Err(ApiError::unauthorized("这个账号已被停用"));
    }
    if !verify_password(&req.password, &row.get::<String, _>("password_hash")) {
        return Err(deny());
    }

    let user_id: String = row.get("id");
    Ok(Json(
        st.issue(&user_id, None, req.device_label.as_deref())
            .await?,
    ))
}

/// `POST /auth/refresh` —— 客户端**不用重新登录**靠的就是这条。
pub async fn refresh(
    State(st): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<AuthTokens>, ApiError> {
    Ok(Json(st.rotate(&req.refresh_token).await?))
}

/// `POST /auth/logout` —— 作废这条链。
///
/// 作废**整条 family** 而不是只作废递上来的那一个：登出的意图是
/// 「这台设备上的登录不再有效」，而那条链上还有它刚换出来的后继。
pub async fn logout(
    State(st): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let digest = refresh_digest(&req.refresh_token);
    let n = sqlx::query(
        "UPDATE cortex_auth.auth_tokens SET revoked_at = now()
          WHERE family_id = (SELECT family_id FROM cortex_auth.auth_tokens WHERE token_sha256 = $1)
            AND revoked_at IS NULL",
    )
    .bind(&digest)
    .execute(&st.accounts()?.pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);

    // 认不出的令牌也回 200：登出是幂等的，而报 404 只会让「我已经登出了吗」
    // 变成一个客户端要处理的分支
    Ok(Json(serde_json::json!({ "revoked": n })))
}

/// `POST /auth/register` —— **默认 403**。
pub async fn register(
    State(st): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthTokens>, ApiError> {
    // **第一个账号永远放行。**
    //
    // 私有化部署的第一步是「把它跑起来」，而那时既没有邮箱通道、也可能
    // 没有 shell（有人是点一个 compose 就部署完的）。要求先 SSH 上去跑
    // `--create-user` 才能用，等于把一个纯运维的步骤塞进产品的第一分钟。
    //
    // 代价要说清楚：**一台刚部署好、还没建号、又直接暴露在公网上的机器，
    // 第一个访问它的人会成为主人。** 这个窗口只在「部署完成」到「你注册」
    // 之间，通常几秒，而且一旦关上就永远关上。不接受它的人应当先用
    // `--create-user` 建号，再放通网络。
    // **不再有「第一个账号无条件放行」这条特例。**
    //
    // 它曾经的理由是：私有化部署的第一步是把它跑起来，那时既没有邮箱通道、
    // 也可能没有 shell。代价是一台刚部署好、还没建号、又暴露在公网上的机器，
    // 第一个访问它的人会成为主人 —— 对反复部署来说这个窗口躲不过去。
    //
    // 现在第一个账号由 `.env` 里的 CORTEX_ADMIN_USERNAME / _PASSWORD 在
    // **启动时**建好（见 `main::ensure_admin`），或者用 `--create-user`。
    // 两条路都不经过公网，于是这里不需要任何特例 —— 少一条分支，
    // 也少一个窗口。
    if !open_registration() {
        // 说清楚是「这个部署关着」而不是「你填错了」，否则对方会一直重试
        return Err(ApiError::forbidden(format!(
            "这个部署没有开放注册。管理员可以设 {OPEN_REGISTRATION_ENV}={OPEN_REGISTRATION_ON} 打开，             或者用 cortexd --create-user 直接建号"
        )));
    }
    let user_id = st.create_account(&req.username, &req.password).await?;
    Ok(Json(st.issue(&user_id, None, None).await?))
}

/// `GET /auth/usage` —— 我这个窗口用了多少、还剩多少。
///
/// # Errors
/// 没接账号体系，或者算不出用量。
pub async fn usage(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<crate::quota::QuotaView>, ApiError> {
    let user_id = current_user(&st, &headers).await;
    Ok(Json(st.quota_status(&user_id).await?.into()))
}

/// 这个请求是谁发的。
///
/// access token 认不出时回落到 1 号用户 —— 那是老的预共享 token 那条路，
/// 它已经过了入站那道门，否则这个 handler 根本不会被调到。
pub async fn current_user(st: &AppState, headers: &axum::http::HeaderMap) -> String {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    match st.access_book().resolve(bearer) {
        Some(id) => id,
        None => owner_user_id(st).await.unwrap_or_else(|| "owner".into()),
    }
}

/// `GET /auth/me`
///
/// 用请求里那个 access token 反查是谁。**老的预共享 token 也认**，
/// 映射到 1 号用户 —— 过渡期 CLI 与现有桌面端都还在用它，
/// 一次性切换会让它们当天全部失联。
pub async fn whoami(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<WhoAmI>, ApiError> {
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    let user_id = match st.access_book().resolve(bearer) {
        Some(id) => id,
        // 认不出就是老 token 那条路（它已经过了入站那道门，
        // 否则这个 handler 根本不会被调到）
        None => owner_user_id(&st).await.unwrap_or_else(|| "owner".into()),
    };
    let row = sqlx::query("SELECT username, schema_name FROM cortex_auth.users WHERE id = $1")
        .bind(&user_id)
        .fetch_optional(&st.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("查用户失败：{e}")))?;
    match row {
        Some(r) => Ok(Json(WhoAmI {
            user_id,
            username: r.get("username"),
            schema_name: r.get("schema_name"),
        })),
        // 老 token 那条路映射到 1 号用户，而 1 号用户可能还没在 users 表里
        // （部署早于账号体系）。这不是错误，是过渡期的正常状态
        None => Ok(Json(WhoAmI {
            user_id,
            username: "(尚未建账号)".into(),
            schema_name: "public".into(),
        })),
    }
}

/// 1 号用户的 id。老 token 那条路映射到他。
///
/// 按 id 升序取第一个 —— ULID 有序，所以「第一个」就是「最早建的那个」。
async fn owner_user_id(st: &AppState) -> Option<String> {
    let acc = st.accounts().ok()?;
    sqlx::query("SELECT id FROM cortex_auth.users ORDER BY id LIMIT 1")
        .fetch_optional(&acc.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("id"))
}

#[cfg(test)]
mod tests {
    /// **注册再也没有「第一个账号」这条特例了。**
    ///
    /// 它曾经无条件放行，代价是一台刚部署好、还没建号、又暴露在公网上的
    /// 机器，第一个访问 /auth/register 的人会成为主人。部署一次能靠手快
    /// 躲过去，反复部署躲不过去。
    ///
    /// 现在第一个账号在**开始监听之前**就由 `.env` 建好了（见
    /// `main::ensure_admin`），或者用 `--create-user`。两条都不经过公网，
    /// 于是这里不需要特例 —— 而这条测试盯着那条特例别再长回来。
    #[test]
    fn registration_has_no_first_account_exception() {
        let src = include_str!("accounts.rs");
        let gate = src
            .split("pub async fn register")
            .nth(1)
            .expect("register 还在吧");
        // 只看函数体前半段（到密码哈希为止就够了）
        let head = &gate[..gate.len().min(2000)];
        assert!(
            !head.contains("user_count"),
            "register 里又数起用户数了 —— 那通常意味着「第一个账号特殊对待」             这条特例回来了，而它带着一个公网上的抢主人窗口。             第一个账号应当由 .env（main::ensure_admin）或 --create-user 建"
        );
    }

    use super::*;

    /// **默认必须是关的。**
    ///
    /// 忘了配一个环境变量的后果不该是「陌生人可以开号并烧我的 API key」。
    #[test]
    fn registration_is_closed_unless_explicitly_opened() {
        // 这个测试不改进程环境（会影响并行跑的别的用例），
        // 只验判据本身：除了那个字面量，什么都不算开
        for v in [
            "", "0", "1", "true", "TRUE", "yes", "on", "disabled", "Enabled",
        ] {
            assert_ne!(
                v.trim(),
                OPEN_REGISTRATION_ON,
                "{v:?} 不该被当成「开放注册」—— 手滑设成 1 或 true 不能把门打开"
            );
        }
        assert_eq!("enabled", OPEN_REGISTRATION_ON);
    }

    /// refresh 的寿命就是「多久不用要重新登录」，别让它悄悄变短。
    #[test]
    fn a_session_survives_a_month_of_restarts() {
        assert_eq!(
            REFRESH_TTL.num_days(),
            30,
            "这个数就是用户体感上的「多久要重新登录一次」，改它要有意识"
        );
        assert!(
            ACCESS_TTL < REFRESH_TTL,
            "access 必须比 refresh 短得多，否则短命令牌那一层就没意义了"
        );
    }
}

#[cfg(test)]
mod account_schema_tests {
    use super::*;

    /// 第一个账号接管 `public`，其余各自开一片。
    #[test]
    fn the_first_account_takes_over_public() {
        let id = cortex_core::Id::new().to_string();
        assert_eq!(
            schema_for_new_account(&id, true).unwrap().as_str(),
            "public",
            "第一个账号必须落在 public 上。落在别处的话，一个用了几个月才建号的人             建完号一登录，全部记忆消失 —— 而且不报错，会话列表就是空的"
        );
        let second = schema_for_new_account(&id, false).unwrap();
        assert!(
            second.as_str().starts_with("u_"),
            "第二个之后必须各自一片，实际拿到 {}",
            second.as_str()
        );
        assert_ne!(second.as_str(), "public");
    }
}

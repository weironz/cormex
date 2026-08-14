//! 认证 —— 「谁能连上这个 cortexd」。
//!
//! 在此之前谁连上谁就是主人。一个绑到 `0.0.0.0` 的 cortexd、一条随手开的
//! ngrok、一个把端口映射出去的 docker-compose，任何一个都足以让整个记忆库
//! 对公网敞开，而且**不会有任何迹象**。
//!
//! # 方案：预共享 token，服务端只存摘要
//!
//! ```text
//! 运维：cortexd --generate-token
//!        ├─► 明文 token ──────────────► 客户端（CORTEXD_TOKEN，只显示这一次）
//!        └─► SHA-256 摘要 ────────────► 服务端（.env 的 CORTEX_AUTH_TOKEN_SHA256）
//!
//! 客户端：Authorization: Bearer <明文>
//! 服务端：sha256(明文) == 摘要 ?
//! ```
//!
//! ## 为什么不是签发式 / 带过期的会话令牌
//!
//! 那两条都需要一个**签发端点**，也就是需要一份先于它存在的凭据去换令牌 ——
//! 单用户自托管场景下那份凭据只能还是预共享的东西，于是多出来的一层只是把
//! 同一个秘密换了个包装，代价是密钥轮换、时钟偏移、令牌撤销三件事都要处理。
//! 真正需要签发式的是「多用户 + 要能单独踢掉某个人」，而那是多租户的需求，
//! 这一轮明确不做（见本文件末尾）。
//!
//! OAuth 之类更不必谈：自托管的东西不该为了登录去依赖一个外部服务，
//! 那台服务挂了你就进不去自己的记忆库。
//!
//! ## 为什么摘要用 SHA-256 而不是 argon2 / bcrypt
//!
//! 慢 KDF 防的是**低熵秘密被离线爆破**（人选的密码）。这里的 token 是
//! 256 位内核随机数，爆破它需要的次数与 KDF 快慢无关 —— 慢 100 万倍，
//! 爆破时间从「宇宙年龄」变成「宇宙年龄」。付出的是一个新依赖和每次请求
//! 几十毫秒的 CPU。
//!
//! **这个论证依赖一个前提：token 必须是高熵随机的。** 所以
//! [`generate`] 不接受用户自选的口令，只吐随机值 —— 前提一旦被人绕开
//! （拿 `echo -n hunter2 | sha256sum` 填进去），上面这段就不成立了。
//!
//! ## 凭据存在哪
//!
//! - **服务端**：只有摘要，放 `.env`（已被 gitignore 排除）。摘要泄露不能
//!   直接用来登录 —— 它不能被当成 bearer token 提交，服务端会先对收到的
//!   东西做一次 SHA-256 再比。
//! - **不存数据库**：那会让凭据跟着 `sync_log` 同步到每一台设备上，
//!   而且在 append-only 体系里写进去就删不掉 —— 轮换一次旧摘要永远留在库里。
//! - **客户端**：明文，环境变量 `CORTEXD_TOKEN`。同样不进任何被 git 跟踪的文件。
//!
//! # 没配凭据时：拒绝启动
//!
//! 与 agent 那一侧的沙箱降级策略同一类判断，答案也一样：
//! **静默不生效是最危险的失败形态**。开放默认值的代价是「有人在不知情的
//! 情况下把记忆库挂在公网上」，而这件事没有任何症状；拒绝启动的代价是
//! 「第一次跑要多做一步」，而这件事当场就被发现。
//!
//! 逃生口同样刻意不是 `1` / `true`：只有 `CORTEX_AUTH=disabled` 这个字面量
//! 才关得掉它，没法被「顺手设个环境变量」误开。关掉之后 `/health` 会一直报
//! `auth: disabled`，启动日志里也有一行警告 —— 任何时候都必须能回答
//! 「我现在到底有没有被保护」。
//!
//! # 浏览器的真实约束：`EventSource` 与 `WebSocket` 加不了请求头
//!
//! 这不是可以绕过的，是浏览器 API 的硬限制。三条常见出路里：
//!
//! | 出路 | 问题 |
//! |---|---|
//! | 把长期 token 放查询串 | 它会进 access log、进反代日志、进浏览器历史，而且是**长期**凭据 |
//! | Cookie | 要配 CORS credentials，且立刻引入 CSRF —— 而本服务的 CORS 是 permissive |
//! | **短命票据** | 用长期 token 换一张 60 秒的票，票才进 URL |
//!
//! 选第三条。见 [`TicketBook`]。票据同样能解决 `<img src>` 取附件的问题：
//! `Image.network` 一样加不了请求头。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest as _, Sha256};

use crate::state::AppState;

/// 服务端摘要的环境变量。
pub const TOKEN_SHA256_ENV: &str = "CORTEX_AUTH_TOKEN_SHA256";

/// 关闭认证的环境变量。
pub const AUTH_ENV: &str = "CORTEX_AUTH";

/// 关闭认证的**唯一**取值。见模块文档里「为什么不是 `1`」。
pub const DISABLED_VALUE: &str = "disabled";

/// 票据有效期。
///
/// 60 秒够开一条 WebSocket、够加载一屏图片，短到即使整条 URL 被记进日志，
/// 捡到它的人也基本上赶不上。**它不是单次使用的** —— 单次会让一个页面上的
/// 第二张图直接 401，也会让 WS 断线重连时手上那张票作废；用「短」换「一次性」
/// 在这里是划算的，因为要保护的性质是「长期凭据不进 URL」，不是「URL 不可重放」。
pub const TICKET_TTL: Duration = Duration::from_secs(60);

/// 查询串里放票据的参数名。
pub const TICKET_PARAM: &str = "ticket";

/// 本部署的认证形态。
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// 校验 bearer token（与短命票据）
    Token { digest: [u8; 32] },
    /// 显式关闭 —— 只该出现在只听回环的开发机上
    Disabled,
}

impl AuthMode {
    /// 从环境变量加载。**配置缺失是错误，不是「就当没配」。**
    pub fn from_env() -> cortex_core::Result<Self> {
        let disabled = std::env::var(AUTH_ENV).is_ok_and(|v| v.trim() == DISABLED_VALUE);
        let digest_hex = std::env::var(TOKEN_SHA256_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        match (disabled, digest_hex) {
            // 两个都配了：意图不明。安全配置上的「不明」必须是响亮的错误 ——
            // 猜错的两个方向分别是「以为关了其实开着」（只是不好用）和
            // 「以为开着其实关了」（记忆库裸奔），后者不能靠猜
            (true, Some(_)) => Err(cortex_core::CortexError::Config(format!(
                "{AUTH_ENV}={DISABLED_VALUE} 与 {TOKEN_SHA256_ENV} 同时配置了，意图不明。\
                 想关认证就删掉 {TOKEN_SHA256_ENV}；想开认证就删掉 {AUTH_ENV}。"
            ))),
            (true, None) => Ok(Self::Disabled),
            (false, Some(hex_digest)) => {
                let raw = hex::decode(&hex_digest).map_err(|e| {
                    cortex_core::CortexError::Config(format!(
                        "{TOKEN_SHA256_ENV} 不是合法的十六进制：{e}"
                    ))
                })?;
                let digest: [u8; 32] = raw.try_into().map_err(|_| {
                    cortex_core::CortexError::Config(format!(
                        "{TOKEN_SHA256_ENV} 必须是 SHA-256 摘要（64 个十六进制字符）。\
                         注意这里要填的是**摘要**，不是 token 明文 —— \
                         用 `cortexd --generate-token` 一次生成两者。"
                    ))
                })?;
                Ok(Self::Token { digest })
            }
            (false, None) => Err(cortex_core::CortexError::Config(format!(
                "没有配置任何凭据，cortexd 拒绝启动。\n\
                 \n\
                 一个不认证的 cortexd 会把整个记忆库交给任何能连上这个端口的人，\n\
                 而这件事不会有任何症状 —— 所以这里选择当场失败，而不是默认放行。\n\
                 \n\
                 生成一份凭据：  cortexd --generate-token\n\
                 然后把 {TOKEN_SHA256_ENV}=… 写进 .env（明文 token 给客户端）。\n\
                 \n\
                 只听回环的开发机确实不想要认证，就显式写 {AUTH_ENV}={DISABLED_VALUE}。"
            ))),
        }
    }

    /// 给 `/health` 与启动日志的一句话。
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Token { .. } => "token",
            Self::Disabled => "disabled",
        }
    }

    /// 启动时打给运维看的那一行。
    #[must_use]
    pub fn status_line(&self) -> String {
        match self {
            Self::Token { .. } => "认证：已启用（Bearer token）".into(),
            Self::Disabled => format!(
                "认证：⚠ **已关闭**（{AUTH_ENV}={DISABLED_VALUE}）—— \
                 任何能连上这个端口的人都拥有全部记忆。只有在确认监听地址是回环时才可接受"
            ),
        }
    }

    fn verify(&self, presented: &str) -> bool {
        match self {
            Self::Disabled => true,
            Self::Token { digest } => {
                let got = Sha256::digest(presented.as_bytes());
                // 先哈希再比，所以即使这里泄露时序也只泄露摘要的字节 ——
                // 但常数时间比较是白给的，没有理由不做
                constant_time_eq(&got, digest)
            }
        }
    }
}

/// 逐字节全量比较，不提前返回。
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 生成一份新凭据：(明文 token, SHA-256 摘要的十六进制)。
///
/// 明文只在这一刻存在。服务端拿摘要，客户端拿明文，没有任何一处同时持有两者
/// 之外的东西 —— 服务端被读走 `.env` 也拿不到能用来登录的凭据。
#[must_use]
pub fn generate() -> (String, String) {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("内核熵源不可用，拒绝生成可预测的凭据");
    let token = hex::encode(buf);
    let digest = hex::encode(Sha256::digest(token.as_bytes()));
    (token, digest)
}

// ────────────────────────── 短命票据 ──────────────────────────

/// 已签发、尚未过期的票据。
///
/// 存内存不存库：它活不过一分钟，而写进 `sync_log` 的东西会同步到每台设备
/// 并且永远删不掉。进程重启后全部票据作废，客户端重新换一张即可 ——
/// 反正它手上有长期 token。
#[derive(Default)]
pub struct TicketBook {
    inner: Mutex<HashMap<String, Instant>>,
}

impl TicketBook {
    /// 签一张票。调用方必须**已经**通过了 bearer 校验。
    pub fn issue(&self) -> String {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect("内核熵源不可用，拒绝签发可预测的票据");
        let ticket = hex::encode(buf);
        let mut guard = self.inner.lock().expect("票据本的锁不该中毒");
        // 顺手清一遍过期的。没有单独的清理任务：签票是唯一会让这个表长大的
        // 动作，在这里清就不可能出现「只涨不清」
        let now = Instant::now();
        guard.retain(|_, exp| *exp > now);
        guard.insert(ticket.clone(), now + TICKET_TTL);
        ticket
    }

    fn valid(&self, ticket: &str) -> bool {
        let guard = self.inner.lock().expect("票据本的锁不该中毒");
        guard.get(ticket).is_some_and(|exp| *exp > Instant::now())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().expect("票据本的锁不该中毒").len()
    }
}

// ──────────────────────────── 中间件 ───────────────────────────

/// 认证中间件。
///
/// # 为什么不在这里写「这几条路径不用认证」
///
/// 路径白名单是这类中间件最经典的出事点：加一条新路由的人不会想起来去
/// 更新那张表，而漏掉的方向恰好是**默认放行**。所以豁免不在这里表达 ——
/// `/health` 压根不挂这层中间件（见 [`crate::routes::router`]），
/// 新加的路由默认落在受保护那一侧。
pub async fn require(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let mode = st.auth_mode();
    if matches!(mode, AuthMode::Disabled) {
        return next.run(req).await;
    }

    // 1) 常规路径：Authorization: Bearer
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    if let Some(tok) = bearer {
        // 预共享 token（老路，也是自托管的常态）
        if mode.verify(tok) {
            return next.run(req).await;
        }
        // 登录换来的 access token。
        //
        // **这一支曾经不存在**，而缺了它整个账号体系是不通的：注册、登录、
        // 拿到 token，然后每一个请求 401 —— 因为这道门只认那把预共享的。
        // 端到端跑一次才发现，而三类测试都没覆盖到：
        // 服务端测试单独测 `/auth/login`，中间件测试只查公开路由表，
        // 客户端测试打的是 mock。**没有一条从登录走到用它**。
        if st.access_book().resolve(tok).is_some() {
            return next.run(req).await;
        }
        // 沙箱令牌。**与上面两支的差别是它带作用域** —— 认出来还不够，
        // 还要问一句「这条路由它够得着吗」。
        //
        // 为什么不能像另外两把那样认了就放行：容器里跑的是不可信代码，
        // 而 `/episodes` 与 `/memory/search` 是**穿透容器边界的持久通道**
        //（写进去的记忆会在未来所有会话、所有设备上被召回）。
        // 完整论证见 `sandbox_token` 的模块文档。
        if let Some(scope) = st.sandbox_tokens().resolve(tok) {
            let (method, path) = (req.method().clone(), req.uri().path().to_owned());
            if scope.allows(&method, &path) {
                // 作用域塞进扩展，供 handler 做**会话级**收窄
                //（认证只答「够不够得着这条路由」，答不了「够不够得着这条
                // 会话的数据」—— 后者要 handler 拿到 body/参数才判断得了）
                let mut req = req;
                req.extensions_mut().insert(scope);
                return next.run(req).await;
            }
            tracing::warn!(
                owner = %scope.owner, session = %scope.session_id, %method, %path,
                "沙箱令牌够不着这条路由"
            );
            return (
                StatusCode::FORBIDDEN,
                axum::Json(crate::dto::ErrorBody {
                    error: format!("沙箱不能访问 {method} {path}"),
                }),
            )
                .into_response();
        }
    }

    // 2) 加不了请求头的那些客户端：短命票据
    if let Some(ticket) = ticket_from_query(req.uri().query())
        && st.ticket_book().valid(&ticket)
    {
        return next.run(req).await;
    }

    // 不区分「没带凭据」与「凭据不对」：分开报等于给暴力破解一个进度条。
    // WWW-Authenticate 照给 —— 那是 401 的标准形状，客户端据此知道该带什么
    tracing::debug!(path = %req.uri().path(), "未通过认证");
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"cortexd\"")],
        axum::Json(crate::dto::ErrorBody {
            error: "未认证：请带上 Authorization: Bearer <token>，\
                    或对加不了请求头的连接使用 POST /auth/ticket 换到的 ?ticket="
                .into(),
        }),
    )
        .into_response()
}

/// 从查询串里取 `ticket=`。
///
/// 手写而不是走 `Query<T>` 提取器：中间件跑在**所有**路由前面，而各路由的
/// 查询参数五花八门，用强类型提取器会因为「多了个不认识的字段」把请求打回，
/// 那是一个非常难查的 400。
fn ticket_from_query(query: Option<&str>) -> Option<String> {
    let q = query?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == TICKET_PARAM).then(|| v.to_string())
    })
}

// ══════════════════════════════════════════════════════════════
//  多租户：这一轮**不做**，理由与代价写在这里
// ══════════════════════════════════════════════════════════════
//
// 「认证」与「数据按人隔离」是两件事，代价差一个数量级。上面这一整个模块是
// 前者：一层中间件加一份凭据。后者要动的是：
//
// 1. **每张业务表加归属列**。`episodes` / `facts` / `entities` / `blobs` /
//    `summaries` / `session_events` / `fact_events` / `episode_memories` /
//    `episode_tool_calls` / `blob_transcripts` / `redactions` —— 一张都不能漏。
// 2. **`sync_log` 的游标要按租户切**。现在是全库单一全序（一个 BIGSERIAL +
//    advisory lock）。多租户下 A 的写会推高 B 看到的游标，B 拉到的记录里
//    全是被过滤掉的空洞 —— 不算错，但每个客户端都要为别人的写量付带宽。
//    真要分开就得每租户一条序列，而那把「单一全序」这个同步协议的地基换掉了。
// 3. **四路召回的 SQL 全部要加过滤**（BM25 / 向量 / 图 / episode），
//    外加 RRF 融合之后的取回、`as_of` 快照那两条路、以及注入块的渲染。
// 4. **`pg_advisory_xact_lock(4272)` 要不要按租户分**。分了并发上去了，
//    但「取号顺序 == 提交顺序」这个论证是针对单一序列成立的，换成每租户
//    一把锁就得重新论证一遍（而现有的那份反向验证 —— 32 并发写漏 12 条 ——
//    也要重跑）。
// 5. **`blobs` 是内容寻址的**，同一份字节天然被多个租户共享。归属加在
//    `blobs` 上就破坏了去重，加在 `episode_blobs` 上则意味着删最后一个引用
//    时要做引用计数 —— 而这是个 append-only 系统，引用计数没地方放。
//
// 而这是一个 **append-only** 系统：事后给已有数据补归属列，唯一诚实的做法是
// 加一列带默认值（「都归第一个用户」），这一步本身不难；难的是第 2、3、4 条，
// 它们**什么时候做都一样难**，跟现在有多少数据无关。也就是说「趁数据少赶紧
// 加列」买不到什么 —— 真正的成本不在数据迁移上。
//
// 所以这一轮只做认证。**没有加任何归属列**：一个「有列但读侧不过滤」的状态
// 会让下一个人以为隔离已经生效了一半，而事实上四路召回里任何一路漏了过滤，
// 泄露就是全量的。要做就整套做，包括读侧。
//
// 当前的产品形态是单用户自托管，认证这一层已经完整地解决了它面对的威胁
// （「别人能不能连上我的 cortexd」）。做一半的多租户比不做更危险：
// 它会让人以为数据是隔离的。

#[cfg(test)]
mod tests {
    use super::*;

    fn token_mode() -> (String, AuthMode) {
        let (token, digest_hex) = generate();
        let raw = hex::decode(&digest_hex).expect("生成的摘要应当是合法十六进制");
        let digest: [u8; 32] = raw.try_into().expect("SHA-256 应当是 32 字节");
        (token, AuthMode::Token { digest })
    }

    #[test]
    fn the_right_token_passes_and_everything_else_does_not() {
        let (token, mode) = token_mode();
        assert!(mode.verify(&token), "正确的 token 必须通过");
        assert!(!mode.verify(""), "空凭据不能通过");
        assert!(!mode.verify("wrong"), "错误的 token 不能通过");

        // 改一个字符就该失败
        let mut tampered = token.clone();
        let last = tampered.pop().expect("token 非空");
        tampered.push(if last == 'a' { 'b' } else { 'a' });
        assert!(!mode.verify(&tampered), "只差一个字符的 token 不能通过");

        // 摘要本身不能被当成凭据提交：它只是 token 的哈希，
        // 服务端会再哈希一次，对不上
        let (_, digest_hex) = generate();
        assert!(!mode.verify(&digest_hex));
    }

    /// 摘要**开头几个字节相同**的错误 token 必须被拒。
    ///
    /// 上面那条「改一个字符」的断言其实拦不住把比较写成
    /// `got.starts_with(&digest[..1])` 这类退化：随便改一个字符，
    /// 摘要首字节有 255/256 的概率也跟着变，于是那条测试照样绿。
    /// 而这个退化是灾难性的 —— 只比一个字节意味着任意随机 token 有
    /// 1/256 的概率被放行。
    ///
    /// 所以这里现场爆破出一个首字节相同的 token 再去试（SHA-256 算几百次，
    /// 微秒级）。它是**确定性**的：只要比较不是全量的，就一定红。
    #[test]
    fn a_token_whose_digest_merely_starts_the_same_is_rejected() {
        let (real, mode) = token_mode();
        let real_digest = Sha256::digest(real.as_bytes());

        let mut collided = None;
        // 首字节 1/256，两万次里撞不上的概率小到可以忽略；
        // 真撞不上就是 SHA-256 出问题了，此时失败也是对的
        for _ in 0..20_000 {
            let (cand, _) = generate();
            if Sha256::digest(cand.as_bytes())[0] == real_digest[0] && cand != real {
                collided = Some(cand);
                break;
            }
        }
        let collided = collided.expect("应当能爆破出一个首字节相同的 token");
        assert!(
            !mode.verify(&collided),
            "摘要只是开头相同的 token 竟然通过了 —— 凭据比较不是全量的"
        );
    }

    #[test]
    fn the_stored_digest_is_not_the_token() {
        let (token, digest) = generate();
        assert_ne!(token, digest, "存的必须是摘要，不能是明文");
        assert_eq!(token.len(), 64);
        assert_eq!(digest.len(), 64);
        assert_eq!(
            hex::encode(Sha256::digest(token.as_bytes())),
            digest,
            "摘要必须真的是 token 的 SHA-256"
        );
    }

    #[test]
    fn two_generated_tokens_never_collide() {
        let (a, _) = generate();
        let (b, _) = generate();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq_agrees_with_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"), "长度不同必须判否");
    }

    #[test]
    fn a_ticket_is_valid_until_it_is_not_in_the_book() {
        let book = TicketBook::default();
        let t = book.issue();
        assert!(book.valid(&t));
        assert!(!book.valid("deadbeef"), "没签发过的票据不能通过");
        // 可重复使用：一个页面上的第二张图不该 401
        assert!(book.valid(&t));
    }

    #[test]
    fn issuing_prunes_expired_tickets() {
        let book = TicketBook::default();
        {
            let mut g = book.inner.lock().unwrap();
            g.insert("old".into(), Instant::now() - Duration::from_secs(1));
        }
        assert_eq!(book.len(), 1);
        let fresh = book.issue();
        assert_eq!(book.len(), 1, "过期票据应当在签新票时被清掉");
        assert!(book.valid(&fresh));
        assert!(!book.valid("old"));
    }

    #[test]
    fn ticket_is_parsed_from_anywhere_in_the_query_string() {
        assert_eq!(ticket_from_query(Some("ticket=abc")), Some("abc".into()));
        assert_eq!(
            ticket_from_query(Some("since=3&ticket=abc&limit=5")),
            Some("abc".into())
        );
        assert_eq!(ticket_from_query(Some("since=3")), None);
        assert_eq!(ticket_from_query(None), None);
        // 名字相近的参数不能被误认
        assert_eq!(ticket_from_query(Some("myticket=abc")), None);
    }

    /// 关闭开关只认那一个字面量。守的是模块文档里那条论证。
    #[test]
    fn the_disable_switch_only_accepts_the_exact_literal() {
        for v in ["1", "true", "yes", "off", "DISABLED", "disable"] {
            assert_ne!(
                v.trim(),
                DISABLED_VALUE,
                "{v:?} 绝不能关掉认证 —— 开关必须是读懂了才会写下的东西"
            );
        }
        assert_eq!("disabled".trim(), DISABLED_VALUE);
    }

    /// **登录换来的 access token 必须能过这道门。**
    ///
    /// 这条曾经是不成立的：`require` 只调 `mode.verify`，而那只认预共享
    /// token 的摘要。于是账号体系整条链是断的 —— 注册成功、登录成功、
    /// 拿到 token，然后**每一个请求 401**。
    ///
    /// 三类测试都没覆盖到：服务端测试单独测 `/auth/login` 的返回，
    /// 中间件测试只查公开路由表，客户端测试打的是 mock。
    /// 没有一条**从登录走到用它**，端到端跑一次才撞出来。
    ///
    /// 这里不起 axum，只钉住那个不变式：门要认的是两样东西的**并集**。
    #[test]
    fn a_logged_in_access_token_is_not_the_preshared_one() {
        let book = crate::accounts::AccessBook::default();
        let issued = book.issue_for_test("01ABCDEF", std::time::Duration::from_secs(60));

        let mode = AuthMode::Token {
            digest: Sha256::digest(b"the-preshared-one").into(),
        };
        assert!(
            !mode.verify(&issued),
            "刚签发的 access token 与预共享 token 无关，`verify` 当然认不出它 ——              所以 `require` 里**必须**另有一支去问 AccessBook。             这条断言本身不会红；它在这里是为了让下一条的意思清楚"
        );
        assert_eq!(
            book.resolve(&issued).as_deref(),
            Some("01ABCDEF"),
            "簿子必须认得出自己签发的 token。这条红了，说明账号体系又断在同一个地方：             登录成功但每个后续请求 401"
        );
    }
}

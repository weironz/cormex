//! 应用状态与业务分派。
//!
//! 两个后端共用同一套契约：[`Backend::Live`] 接真实的存储层、供应商层与
//! 记忆引擎；[`Backend::Mock`] 是一个**显式选择**的假数据源。
//!
//! 保留 Mock 不是权宜之计 —— 客户端的 CI、离线开发、以及「后端挂了
//! 界面还能不能看」这三件事都需要它，且它强制我们把契约的两个实现
//! 对齐，避免真实实现悄悄偏离文档。
//!
//! **但它不再是回落。** Mock 曾经是「数据库或 API key 不可用时」自动接管的
//! 降级路径，而那让 cortexd 能带着一库假记忆对外服务且看起来完全正常。
//! 现在它只在 `CORTEX_BACKEND=mock` 时出现 —— 完整理由见 [`BackendChoice`]。

use std::ops::Range;
use std::sync::Arc;

use axum::body::Bytes;
use chrono::Utc;
use cortex_core::{Config, CortexError, Id, Result};
use cortex_memory::embed::SharedEmbedder;

use crate::auth::{AuthMode, TicketBook};
use crate::blobs::{MediaStore, PRESIGN_TTL};
use crate::live::Live;
use crate::request_tenant::Tenant;
use crate::sync_notify::SyncBus;
use tokio::sync::broadcast;

use crate::dto::*;
use cortex_llm::MessageStream;
use cortex_proto::episodes::{EpisodeAck, NewEpisodeRequest};
use cortex_proto::llm::LlmStreamRequest;

/// mock 后端的伪游标。见 [`AppState::new_mock`]。
static MOCK_CURSOR: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    #[allow(dead_code)]
    config: Config,
    backend: Backend,
    bus: SyncBus,
    /// 对象存储**两个后端共用**。
    ///
    /// 它与 Postgres 是各自独立的故障域：数据库没起来照样能把图存进 RustFS，
    /// 反之亦然。把它塞进 `Backend::Live` 会让「mock 模式下传不了图」
    /// 变成一条没有理由的限制 —— 而客户端 CI 恰恰要在 mock 上跑上传流程。
    blobs: MediaStore,
    /// 进程级、与后端无关的东西。见 [`Runtime`]。
    rt: Runtime,
}

/// 与后端选择无关的进程级设施。
///
/// 单独一个结构而不是三个字段散在 [`Inner`] 里：它们的共同点是
/// **mock 与 live 必须共用同一份**。认证是全进程的形态，确认簿要能被
/// 任何一条 HTTP 回执找到，票据本同理 —— 任何一个跟着 `Backend` 走，
/// 都会变成「mock 模式下这条路测不了」，而客户端 CI 恰恰跑在 mock 上。
#[derive(Clone)]
pub struct Runtime {
    pub auth: AuthMode,
    pub tickets: Arc<TicketBook>,
    /// 账号那几张表的连接池（`cortex_auth`），以及租户池注册表。
    ///
    /// **与记忆库的池分开**：`cortex-store` 的每条查询都假设自己在某个
    /// 租户的 schema 里，而账号是跨租户的。混用一个池意味着
    /// `search_path` 要来回改，而那正是 [`cortex_store::TenantPools`]
    /// 的文档里说「漏一次就是跨用户泄露」的那种改法。
    ///
    /// `None` = 这个部署还没接数据库（mock 后端）。账号端点此时全部
    /// 报 501 —— 假装能登录比明说不支持糟得多。
    pub accounts: Option<Accounts>,
    /// 已签发的 access token。见 [`crate::accounts::AccessBook`]。
    pub access: Arc<crate::accounts::AccessBook>,
    /// 已签发的**沙箱**令牌。与上面两本簿子并列而不是合并：它是唯一一种
    /// 带**语义作用域**的凭据（见 [`crate::sandbox_token::SandboxScope`]），
    /// 合进 `AccessBook` 会让「这把 token 能干什么」变成一个要在调用点
    /// 到处判断的问题。
    pub sandboxes: Arc<crate::sandbox_token::SandboxTokens>,
}

/// 账号相关的两个句柄。
#[derive(Clone)]
pub struct Accounts {
    /// 直连 `cortex_auth` 的池
    pub pool: sqlx::PgPool,
    /// 按用户取记忆库
    pub tenants: Arc<cortex_store::TenantPools>,
}

impl Accounts {
    /// 连 `cortex_auth`，并把全局那套 migration 跑上。
    ///
    /// 池子只要两条连接：账号相关的查询都是「登录时一次、刷新时一次」，
    /// 而记忆库那边的并发压力走的是租户池。
    ///
    /// # Errors
    /// 连不上，或者全局 migration 跑不过。
    pub async fn connect(database_url: &str) -> Result<Self> {
        cortex_store::Store::migrate_global(database_url)
            .await
            .map_err(|e| cortex_core::CortexError::Store(format!("全局 schema 迁移失败：{e}")))?;

        let options = std::str::FromStr::from_str(database_url)
            .map(|o: sqlx::postgres::PgConnectOptions| {
                // 账号表在 cortex_auth 里，而这个池**只**碰它们。
                // 不带 public 的话 `ulid` / `sha256` 两个 DOMAIN 解析不到
                o.options([("search_path", "cortex_auth,public")])
            })
            .map_err(|e: sqlx::Error| {
                cortex_core::CortexError::Config(format!("DATABASE_URL 不合法：{e}"))
            })?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .map_err(|e| cortex_core::CortexError::Store(format!("连 cortex_auth 失败：{e}")))?;

        Ok(Self {
            pool,
            tenants: Arc::new(cortex_store::TenantPools::new(database_url)),
        })
    }
}

impl Runtime {
    /// 从环境变量装配。任何一项配错都在这里失败，而不是等到第一次请求。
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            auth: AuthMode::from_env()?,
            tickets: Arc::new(TicketBook::default()),
            // 建池要 await，而 from_env 是同步的。由 main 在拿到配置之后
            // 调 `with_accounts` 补上 —— 分两步是为了让「配置错了」
            // 与「数据库连不上」在日志里是两条不同的失败
            accounts: None,
            access: Arc::new(crate::accounts::AccessBook::default()),
            sandboxes: Arc::new(crate::sandbox_token::SandboxTokens::default()),
            // 由 main 在拿到配置后调 `with_sandbox` 补上：连 docker 要
            // 试探一次 daemon，而 from_env 是同步的且不该做 IO
        })
    }
}

impl AppState {
    /// 账号相关的两个句柄。
    ///
    /// # Errors
    /// 这个部署没接数据库（mock 后端）。回 501 而不是 panic ——
    /// 我一开始写的是 `expect`，理由是「路由层会先挡住」，
    /// 但那道闸门当时并不存在，于是一次匿名 `GET /auth/me` 就能让
    /// 这个 handler panic。**「调用方会先检查」不是一种保护。**
    pub fn accounts(&self) -> Result<&Accounts, crate::routes::ApiError> {
        self.inner.rt.accounts.as_ref().ok_or_else(|| {
            crate::routes::ApiError::unsupported(
                "这个部署没有接数据库，账号功能不可用（当前跑在 mock 后端上）",
            )
        })
    }

    /// 已签发的 access token 簿子。
    pub fn access_book(&self) -> &crate::accounts::AccessBook {
        &self.inner.rt.access
    }
}

enum Backend {
    /// 假数据源。契约与 Live 完全一致。
    Mock,
    /// 真实后端：Postgres + LLM + 记忆引擎
    Live(Arc<Live>),
}

/// `CORTEX_BACKEND` —— 跑哪个后端。**默认 live，且不回落。**
///
/// # 为什么不能是「连不上就回落到 mock」
///
/// 那正是它此前的样子，而它在生产上是这样发作的：cortexd 因为崩溃或 OOM
/// **自动重启**（`restart: unless-stopped`）时，compose 的
/// `depends_on: condition: service_healthy` **不生效** —— 那个条件只在
/// `docker compose up` 那一刻管用。于是 Postgres 恢复期间的一次自动重启，
/// 会让 cortexd 带着一库假记忆起来并对外服务。
///
/// 而它看起来完全正常：HTTP 200、`status: "ok"`、对话照常、检索有结果。
/// 只有 `/health` 里 `database: "not_wired"` 一个字段能看出区别，
/// 而生产 compose 里 cortexd **根本没有配 healthcheck**。
///
/// 这与 0.1.2 那次「embeddings 找不到权重、静默变成假向量」是同一个失败
/// 形状：**降级得太安静**。那次写进了 CHANGELOG，这次把它从形状上去掉 ——
/// 起不来的容器会被 docker 反复重启，直到数据库真的回来，这正是想要的。
///
/// mock 仍然保留，但必须**明说**。开发时想不带数据库跑就设
/// `CORTEX_BACKEND=mock`，而那时 `/health` 会如实报 `status: "mock"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    Live,
    Mock,
}

impl BackendChoice {
    /// # Errors
    /// 取值不认识时报错。
    pub fn from_env() -> cortex_core::Result<Self> {
        // 空串按「没设」处理，与 `CORTEX_EMBED_BACKEND` 同一个理由：
        // compose 的 `${VAR:-}` 会把没配的变量**设成空串**，而 `env::var`
        // 对空串返回 `Ok("")`。这个坑在这个仓库里已经出现过三次
        std::env::var("CORTEX_BACKEND")
            .ok()
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or(Ok(Self::Live), Self::parse)
    }

    /// # Errors
    /// 取值不认识时报错 —— **不回落到默认值**。
    ///
    /// 一个打错的 `CORTEX_BACKEND=moc` 悄悄跑成 live 还算幸运；
    /// 反过来（`CORTEX_BACKEND=liv` 跑成 mock）就是这条修复本来要防的事
    /// 又从另一个门进来了。
    pub fn parse(s: &str) -> cortex_core::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "live" | "real" => Ok(Self::Live),
            "mock" | "fake" => Ok(Self::Mock),
            other => Err(cortex_core::CortexError::Config(format!(
                "未知的 CORTEX_BACKEND={other:?}，可选：live / mock"
            ))),
        }
    }
}

impl AppState {
    pub async fn new_mock(config: Config, rt: Runtime) -> Self {
        let blobs = MediaStore::connect(&config).await;
        let bus = SyncBus::inert();

        // 定期伪造一次游标推进。看着像多此一举，但没有它，Flutter/CLI 的
        // 「收到 bump → 去 /sync 补拉 → 推进本地游标」这条主路径在离线开发
        // 与客户端 CI 里根本跑不到，只能等接上数据库才第一次被执行。
        // 与 mock_chat_stream 伪造记忆和工具事件是同一个理由。
        {
            let bus = bus.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
                loop {
                    tick.tick().await;
                    let cursor = MOCK_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    bus.publish(SyncEvent::Bump { cursor });
                }
            });
        }

        Self {
            inner: Arc::new(Inner {
                config,
                backend: Backend::Mock,
                bus,
                blobs,
                rt,
            }),
        }
    }

    /// 接入真实后端。数据库连不上或缺 API key 都会失败，而**调用方不再降级** ——
    /// 它把错误一路带到 `main` 并退出。理由见 [`BackendChoice`]。
    pub async fn new_live(config: &Config, rt: Runtime) -> Result<Self> {
        // 走进程内共享的那一份（`OnceCell`），而不是自己 new 一个：模型即使
        // int8 也有近 600 MB，「daemon 而非嵌入式」这个架构决策的收益之一
        // 就是模型常驻共享，各处各建一份等于把它退回去。
        // 后端由 CORTEX_EMBED_BACKEND 决定，默认真实语义模型；
        // CI 与离线开发显式设 hash。
        let embedder: SharedEmbedder = cortex_memory::embed::shared_embedder().await?;
        let live = Live::new(config, embedder).await?;
        let bus = SyncBus::spawn(live.store().clone());
        let blobs = MediaStore::connect(config).await;

        let live = Arc::new(live);

        let st = Self {
            inner: Arc::new(Inner {
                config: config.clone(),
                backend: Backend::Live(live),
                bus,
                blobs: blobs.clone(),
                rt,
            }),
        };

        // ── 已有租户的 migration ─────────────────────────────
        //
        // 新注册的用户由 `provision` 迁移，而**已经存在的**租户此前一次
        // 都没被迁过：`migrate_all` 写好了、导出了，没有任何一处调用它。
        // 后果是加一条新 migration 之后，老用户的库停在旧结构上 ——
        // 表现是零散的 SQL 错误，不是一条能读的启动失败。
        //
        // 坏掉的单独标出来，不拖垮整个服务：一个用户的库坏了，
        // 其他人照常。
        if let Ok(schemas) = st.tenant_schemas().await
            && let Ok(acc) = st.accounts()
        {
            let others: Vec<_> = schemas
                .into_iter()
                // public 上面刚迁过（`Live::new` 里那一句）
                .filter(|s| s.as_str() != cortex_store::SchemaName::public().as_str())
                .collect();
            if !others.is_empty() {
                let results = cortex_store::migrate_all(acc.tenants.as_ref(), &others).await;
                let bad = results.iter().filter(|r| r.failure.is_some()).count();
                tracing::info!(
                    tenants = results.len(),
                    failed = bad,
                    "已有租户的 migration 跑完"
                );
            }
        }

        // 预签名直传那条路上服务端从没经手字节，转录无从触发 —— 大文件成了
        // 纯归档。回扫器以 blobs 表为权威清单把它们捡回来。
        // 未配 vision 模型时它自己不会启动。
        crate::backfill::spawn(st.clone(), blobs);
        // 换过 embedding 模型时把旧事实的向量补出来。默认开着 ——
        // 关掉的代价（旧记忆永久退出向量召回）没有任何症状，
        // 而开着的代价（一次全库 embedding 调用）是可见的，
        // 且它会先把数字报出来再动手。见 reembed 的模块注释
        crate::reembed::spawn(st.clone());

        Ok(st)
    }

    /// 不碰网络、不碰文件系统的最小状态，专供 crate 内的 HTTP 测试。
    ///
    /// # 为什么不复用 `new_mock`
    ///
    /// 它会 `MediaStore::connect`（一次真实的 S3 往返 + 失败后在工作目录里
    /// 建回落目录），还会 spawn 一个每 10 秒推一次假游标的后台任务。
    /// 一条「没带凭据要拿 401」的测试不该依赖这些东西 —— 这正是
    /// 上一轮认为「路由测试做不了」的那个障碍，而它其实只是一个构造函数
    /// 的问题，不是 bin crate 的问题。
    ///
    /// 契约与 mock 后端完全一致（`Backend::Mock`），所以测试打到的仍然是
    /// **真实的** [`crate::routes::router`] 与真实的处理器。
    #[cfg(test)]
    pub fn for_tests(rt: Runtime) -> Self {
        Self {
            inner: Arc::new(Inner {
                config: Config {
                    database_url: "postgres://tests-never-connect".into(),
                    bind: "127.0.0.1:0".into(),
                    s3: cortex_core::config::S3Config {
                        endpoint: String::new(),
                        bucket: String::new(),
                        region: String::new(),
                        access_key: String::new(),
                        secret_key: String::new(),
                    },
                    llm: cortex_core::config::LlmConfig {
                        provider: "none".into(),
                        model: String::new(),
                        cheap_model: String::new(),
                        base_url: None,
                    },
                    device_id: "test".into(),
                },
                backend: Backend::Mock,
                bus: SyncBus::inert(),
                blobs: crate::blobs::MediaStore::unavailable_for_tests(),
                rt,
            }),
        }
    }

    // ───────────────────────── 认证 ─────────────────────────

    #[must_use]
    pub fn auth_mode(&self) -> &AuthMode {
        &self.inner.rt.auth
    }

    #[must_use]
    pub fn ticket_book(&self) -> &TicketBook {
        &self.inner.rt.tickets
    }

    #[must_use]
    pub fn sandbox_tokens(&self) -> &crate::sandbox_token::SandboxTokens {
        &self.inner.rt.sandboxes
    }

    // ───────────────────────── 多租户 ─────────────────────────

    /// 真实后端的句柄。后台任务用它按租户绑定。
    #[must_use]
    pub fn live(&self) -> Option<&Arc<Live>> {
        match &self.inner.backend {
            Backend::Mock => None,
            Backend::Live(l) => Some(l),
        }
    }

    /// 现在有哪些租户。**每次重新查**，不缓存。
    ///
    /// 后台任务靠它遍历。缓存一份的话，注册之后新用户要等到下次重启才被
    /// 回填覆盖到 —— 而那是静默的：他只是觉得「语义召回好像差一点」。
    ///
    /// 没接账号体系时返回 `[public]` —— 单用户自托管的形态。
    ///
    /// # Errors
    /// 查不动 `cortex_auth.users`。
    pub async fn tenant_schemas(&self) -> Result<Vec<cortex_store::SchemaName>> {
        let Ok(acc) = self.accounts() else {
            return Ok(vec![cortex_store::SchemaName::public()]);
        };
        let names: Vec<String> = sqlx::query_scalar(
            // disabled 的账号不迁、不回填 —— 他登不进来，花在他身上的
            // embedding 钱是纯浪费
            "SELECT schema_name FROM cortex_auth.users WHERE disabled_at IS NULL",
        )
        .fetch_all(&acc.pool)
        .await
        .map_err(|e| CortexError::Store(format!("列不出租户：{e}")))?;

        Ok(names
            .iter()
            .filter_map(|n| match cortex_store::SchemaName::new(n) {
                Ok(s) => Some(s),
                Err(e) => {
                    // 跳过而不是整批失败：一行坏数据不该让所有人的回填停摆
                    tracing::error!(schema = n, error = %e, "跳过一个 schema 名不合法的租户");
                    None
                }
            })
            .collect())
    }

    /// 某个租户的 `Store`。后台任务用。
    ///
    /// # Errors
    /// mock 后端，或者那个 schema 的池建不起来。
    pub async fn tenant_store(
        &self,
        schema: &cortex_store::SchemaName,
    ) -> Result<std::sync::Arc<cortex_store::Store>> {
        if schema.as_str() == cortex_store::SchemaName::public().as_str() {
            return self
                .public_store()
                .ok_or_else(|| CortexError::Unavailable("mock 后端没有库".into()));
        }
        self.accounts()
            .map_err(|_| CortexError::Unavailable("没接账号体系".into()))?
            .tenants
            .get(schema)
            .await
            .map_err(|e| CortexError::Store(format!("连不上 {}：{e}", schema.as_str())))
    }

    /// `public` 那个池 —— 启动时建的那一个。mock 后端下没有。
    ///
    /// **不是「默认租户」**。它只在两种情况下被当作租户用：没接账号体系
    /// （单用户自托管），以及 1 号用户（他的 schema 名就叫 `public`）。
    /// 其余用户一律走 [`crate::request_tenant::Tenant`] 里那条查表的路。
    #[must_use]
    pub fn public_store(&self) -> Option<std::sync::Arc<cortex_store::Store>> {
        match &self.inner.backend {
            Backend::Mock => None,
            Backend::Live(l) => Some(std::sync::Arc::new(l.store().clone())),
        }
    }

    // ───────────────────── 实时同步（/ws）─────────────────────

    #[must_use]
    pub fn subscribe_sync(&self) -> broadcast::Receiver<SyncEvent> {
        self.inner.bus.subscribe()
    }

    /// 服务端当前的同步游标末端。
    ///
    /// 读失败一律返回 0：客户端拿到偏小的游标只会多拉一次（无害），
    /// 拿到偏大的才会永久漏行。
    pub async fn latest_cursor(&self) -> i64 {
        match &self.inner.backend {
            Backend::Mock => MOCK_CURSOR.load(std::sync::atomic::Ordering::Relaxed),
            Backend::Live(l) => l.latest_cursor().await,
        }
    }

    /// `/health` 顶上那一行。
    ///
    /// mock 后端**不报 `ok`**：那个词在探针眼里意味着「这个服务可以承接
    /// 流量」，而一个返回假数据的进程不可以。此前它报 `ok`，
    /// 与「数据是真的」唯一的区别是下面 `database` 那一行。
    ///
    /// 现在 mock 只能是显式选的（见 [`BackendChoice`]），所以这一行主要是
    /// 给开发时一眼确认「我到底连没连上库」—— 但它值得如实说。
    #[must_use]
    pub fn status(&self) -> &'static str {
        match &self.inner.backend {
            Backend::Mock => "mock",
            Backend::Live(_) => "ok",
        }
    }

    pub async fn database_status(&self) -> String {
        match &self.inner.backend {
            Backend::Mock => "not_wired".into(),
            Backend::Live(l) => l.database_status().await,
        }
    }

    #[must_use]
    pub fn blob_backend(&self) -> &'static str {
        self.inner.blobs.backend()
    }

    /// 向量化的覆盖情况，进 `/health`。mock 后端没有数据库可查，返回 `None`。
    pub async fn embedding_health(&self) -> Option<crate::dto::EmbeddingHealth> {
        match &self.inner.backend {
            Backend::Mock => None,
            Backend::Live(l) => Some(l.embedding_health().await),
        }
    }

    /// 本部署能不能签 presigned URL。见 [`MediaStore::supports_presign`]。
    #[must_use]
    pub fn supports_presign(&self) -> bool {
        self.inner.blobs.supports_presign()
    }

    // ───────────────────────── 媒体 ─────────────────────────

    /// 服务端中转上传：对象存储 → `blobs` 行。§九 三步顺序的前两步。
    pub async fn upload_blob(
        &self,
        tenant: &Tenant,
        bytes: Bytes,
        declared_mime: Option<&str>,
    ) -> Result<BlobDto> {
        // 留一份给转录用。内容寻址下这份字节就是最终内容，
        // 不必等落库后再从对象存储取回来一遍
        let for_transcribe = bytes.clone();
        let stored = self.inner.blobs.put(bytes, declared_mime).await?;
        self.register(
            tenant,
            &stored.hash,
            &stored.mime,
            stored.size_bytes,
            stored.deduplicated,
            Some(for_transcribe),
        )
        .await
    }

    /// 一个用户的 `Store`，**入口是 user id 而不是请求头**。
    ///
    /// 后台任务（快照）手上只有沙箱令牌里记着的 owner，没有 access token，
    /// 所以走不了 [`Self::tenant`] 那条路。详见
    /// [`Self::tenant_for_user`]。
    ///
    /// # Errors
    /// 租户解析失败，或 mock 后端下没有库。
    pub async fn tenant_store_for_user(
        &self,
        user_id: &str,
    ) -> Result<std::sync::Arc<cortex_store::Store>> {
        match self
            .tenant_for_user(user_id)
            .await
            .map_err(|e| CortexError::Store(format!("解析 {user_id} 的租户失败：{e:?}")))?
        {
            crate::request_tenant::Tenant::Live { store, .. } => Ok(store),
            crate::request_tenant::Tenant::Mock => Err(CortexError::Unavailable(
                "本实例跑在 mock 后端上，没有可写的库".into(),
            )),
        }
    }

    /// 签一张直传 URL。客户端**先算好哈希**再来要 —— 内容寻址不存在
    /// 「先传上去再定 key」。
    pub async fn presign_upload(&self, hash: &str) -> Result<BlobPresignResponse> {
        // 先探一次：这份内容可能早就在了（转发同一张图、跨设备重传）。
        // 告诉客户端「不用传了」是内容寻址在移动端上最直接的一笔收益
        let already_uploaded = self.inner.blobs.exists(hash).await?;
        let url = self.inner.blobs.presign_put(hash).await?;
        Ok(BlobPresignResponse {
            url,
            method: "PUT",
            expires_in_secs: PRESIGN_TTL.as_secs(),
            already_uploaded,
        })
    }

    /// 直传完成后的登记。
    ///
    /// # 这里对客户端信到什么程度
    ///
    /// - **哈希**：不复核。复核要把整个对象拉下来重算一遍，那正好抵消掉
    ///   直传省下的带宽 —— presign 这条路就白铺了。星型拓扑下上传者就是
    ///   数据的主人，谎报哈希只会污染他自己的记忆库。
    /// - **字节数**：不复核。`BlobStore` 没有 HEAD 能力，唯一的办法同样是整取。
    ///   它只影响 `Content-Length` 与 range 的分母，报错了表现为播放器多要
    ///   一次或少要一次，不会损坏内容。
    /// - **MIME**：**不信**。只拉头几 KB 嗅探，与直传路径的规矩一致 ——
    ///   把 `application/octet-stream` 写进 `blobs.mime`，这份内容对将来的
    ///   转录 pipeline 就是黑洞，而那时它已经沉在库底了。
    ///
    /// 唯一硬性检查是**对象真的在**：不在就绝不写 `blobs` 行，否则会留下
    /// 一条取不回内容的悬空引用，而那是无法自愈的。
    pub async fn commit_blob(&self, tenant: &Tenant, req: BlobCommitRequest) -> Result<BlobDto> {
        if req.size_bytes < 0 {
            return Err(CortexError::Invalid(format!(
                "size_bytes 不能为负：{}",
                req.size_bytes
            )));
        }
        if !self.inner.blobs.exists(&req.hash).await? {
            return Err(CortexError::Invalid(format!(
                "对象 {} 不在对象存储里；请先 PUT 到 presigned URL 再来登记",
                req.hash
            )));
        }

        let mime = self
            .inner
            .blobs
            .sniff_mime(&req.hash, req.mime.as_deref())
            .await?;
        // 这条路上服务端没经手字节，「有没有真的传」只有客户端知道 ——
        // 所以去重与否完全交给 `register` 按 blobs 行的存在性判定
        self.register(tenant, &req.hash, &mime, req.size_bytes, false, None)
            .await
    }

    /// 登记 blob 行。`bytes` 只有直传那条路有 —— presign 直传时服务端
    /// 根本没经手字节，转录要等将来的后台补扫（尚未实现，已记入 roadmap）。
    async fn register(
        &self,
        tenant: &Tenant,
        hash: &str,
        mime: &str,
        size_bytes: i64,
        deduplicated: bool,
        bytes: Option<bytes::Bytes>,
    ) -> Result<BlobDto> {
        let seq = match &self.inner.backend {
            // mock 没有 blobs 表。伪造一次游标推进，让客户端的
            // 「收到 bump → 补拉」这条路径在离线开发时也走得到
            Backend::Mock => {
                let cursor = MOCK_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                self.inner.bus.publish(SyncEvent::Bump { cursor });
                Some(cursor)
            }
            Backend::Live(l) => {
                let seq = l
                    .bind(tenant.store()?)
                    .register_blob(cortex_store::NewBlob {
                        hash: hash.to_owned(),
                        mime: mime.to_owned(),
                        size_bytes,
                        // 与 blobs::MediaStore 用的是同一个前缀 —— 那边写对象，
                        // 这边写指向它的那一行，两者必须一致
                        // 前缀跟着租户走。写死 "public" 会让所有人的对象
                        // 挤在同一个前缀下 —— 那正是「加前缀换掉一类 GC bug」
                        // 想避免的：A 删掉最后一条引用时字节还被 B 引用着
                        storage_key: cortex_blob::hash::storage_key(
                            &cortex_blob::TenantPrefix::new(tenant.schema()).map_err(|e| {
                                CortexError::Invalid(format!("租户前缀不合法：{e}"))
                            })?,
                            hash,
                        )?,
                    })
                    .await?;

                // 只在**首次**登记时转录（seq.is_some()）：内容寻址下重复上传
                // 是同一份字节，转录结果也会一模一样，重转纯属白烧模型钱
                if let (Some(_), Some(b)) = (seq, bytes) {
                    l.spawn_transcription(hash.to_owned(), b, mime.to_owned());
                }
                seq
            }
        };

        Ok(BlobDto {
            hash: hash.to_owned(),
            mime: mime.to_owned(),
            size_bytes,
            // register_blob 返回 None 即「早就登记过」，那比对象存储那一侧的
            // 去重判定更权威 —— 它看的是 blobs 表，而不是「这次有没有传字节」
            deduplicated: deduplicated || seq.is_none(),
            seq,
        })
    }

    /// 取回时要用的元信息：MIME 与总长度。
    ///
    /// 总长度是 `Content-Range` 的分母 —— 没有它就没法回 206，
    /// 而没有 206 播放器就只能从头拉整个文件。
    pub async fn blob_meta(&self, tenant: &Tenant, hash: &str) -> Result<(String, i64)> {
        // 先校验形制再查库。不校验也不会不安全（存储层自己会拦），但畸形的
        // 哈希会先撞上「blobs 表里没有这一行」而变成 404 —— 报给客户端的是
        // 「找不到」，而真相是「你传的根本不是一个哈希」。
        cortex_blob::hash::validate_hash(hash)?;

        match &self.inner.backend {
            // mock 没有 blobs 表，只能从对象本身现算。整取一次是可以接受的：
            // 这条路只在离线开发与客户端 CI 上跑
            Backend::Mock => {
                let bytes = self.inner.blobs.get(hash).await?;
                let mime = cortex_blob::probe_media(&bytes).resolve_mime(None);
                Ok((mime, bytes.len() as i64))
            }
            Backend::Live(l) => {
                let b = l.bind(tenant.store()?).blob_meta(hash).await?;
                Ok((b.mime, b.size_bytes))
            }
        }
    }

    pub async fn blob_bytes(&self, hash: &str, range: Option<Range<u64>>) -> Result<Bytes> {
        match range {
            Some(r) => self.inner.blobs.get_range(hash, r).await,
            None => self.inner.blobs.get(hash).await,
        }
    }

    /// 直下 URL。手机播视频走它，省掉经 cortexd 中转的那一半带宽。
    pub async fn blob_download_url(&self, tenant: &Tenant, hash: &str) -> Result<BlobUrlResponse> {
        // 先确认这个 blob 是**登记过**的，再签 URL。
        // 直接签会把「对象存储里恰好有这个 key」变成可探测的信息，
        // 也会让客户端拿到一条数据库里根本不认识的内容
        let _ = self.blob_meta(tenant, hash).await?;
        Ok(BlobUrlResponse {
            url: self.inner.blobs.presign_get(hash).await?,
            expires_in_secs: PRESIGN_TTL.as_secs(),
        })
    }

    /// 本地 agent 把一轮对话写回记忆库。见 [`Live::write_episode`]。
    ///
    /// mock 后端下**不假装写成功**：本地 agent 会据此以为记忆已经落库，
    /// 于是把队列里那一条划掉 —— 那才是真的丢数据。
    /// `origin` 决定抽出来的 fact 落在哪个信任级 ——
    /// 见 [`cortex_memory::extract::TurnOrigin`]。
    pub async fn write_episode(
        &self,
        tenant: &Tenant,
        req: NewEpisodeRequest,
        origin: cortex_memory::extract::TurnOrigin,
    ) -> Result<EpisodeAck> {
        match &self.inner.backend {
            Backend::Mock => Err(CortexError::Unavailable(
                "本实例跑在 mock 后端上，没有可写入的记忆库".into(),
            )),
            Backend::Live(l) => l.bind(tenant.store()?).write_episode(req, origin).await,
        }
    }

    /// LLM 代理 —— 把一次流式调用原样转给供应商。
    ///
    /// 本地 agent 默认走这条路：API key 只在服务端一处，多设备不用每台配一遍。
    /// 它**不碰记忆、不写库**，纯转发。
    ///
    /// mock 后端下没有可转发的对象，返回 `Unavailable` 而不是伪造一段流：
    /// 假的 token 流会让本地 agent 看着能跑，直到有人问它为什么答非所问。
    pub async fn llm_stream(
        &self,
        req: LlmStreamRequest,
        own: Option<(&str, &str, Option<&str>)>,
    ) -> Result<MessageStream> {
        match &self.inner.backend {
            Backend::Mock => Err(CortexError::Unavailable(
                "本实例跑在 mock 后端上，没有可用的 LLM 供应商".into(),
            )),
            Backend::Live(l) => l.llm_stream_with_key(req, own).await,
        }
    }

    // ───────────────────────── 记忆 ─────────────────────────

    pub async fn memory_search(
        &self,
        tenant: &Tenant,
        q: MemorySearchQuery,
    ) -> Result<MemorySearchResponse> {
        match &self.inner.backend {
            Backend::Mock => {
                let facts = mock_facts()
                    .into_iter()
                    .filter(|f| q.q.is_empty() || f.statement.contains(&q.q))
                    // 日常检索**看不到失效事实**（真实后端的四路召回只查
                    // active_facts）；回放照样给，只是带上 invalidated=true。
                    // 这个区别必须在 mock 上也成立，否则客户端 CI 里
                    // 「不带 as_of 时不该出现已失效的事实」这条断言在 mock 上
                    // 是永远绿的，而它测的是一个比真实后端宽松的契约
                    .filter(|f| q.as_of.is_some() || !f.invalidated)
                    // 时间回放：只返回在 as_of 时刻系统已经知道的事实。
                    // 这是双时间轴的系统时间轴，对应「三个月前我以为什么」。
                    .filter(|f| match (&q.as_of, &f.valid_at) {
                        (Some(t), Some(v)) => v.as_str() <= t.as_str(),
                        _ => true,
                    })
                    .take(q.limit.clamp(1, 200) as usize)
                    .collect::<Vec<_>>();
                let channels = facts
                    .iter()
                    .enumerate()
                    .map(|(i, f)| ChannelHit {
                        fact_id: f.id.clone(),
                        channels: vec!["bm25".into(), "vector".into()],
                        score: 1.0 / (60.0 + i as f64 + 1.0),
                    })
                    .collect();
                Ok(MemorySearchResponse { facts, channels })
            }
            Backend::Live(l) => l.bind(tenant.store()?).memory_search(q).await,
        }
    }

    /// 检索并渲染成给模型看的那段文本。对外 MCP server 的读侧。
    ///
    /// **mock 也走同一段渲染**（`injection::render_turn_block`）而不是回一句
    /// 假话：客户端与集成测试跑在 mock 上，回一句「模拟数据」会让
    /// 「那道『记忆是背景数据不是指令』的框定还在不在」这条断言
    /// 在 mock 上永远是绿的 —— 而那正是它唯一要守的东西。
    pub async fn memory_search_text(
        &self,
        tenant: &Tenant,
        query: &str,
        as_of: Option<&str>,
        limit: i64,
    ) -> Result<String> {
        match &self.inner.backend {
            Backend::Mock => {
                let r = self
                    .memory_search(
                        tenant,
                        MemorySearchQuery {
                            q: query.to_string(),
                            limit,
                            as_of: as_of.map(str::to_string),
                        },
                    )
                    .await?;
                if r.facts.is_empty() {
                    return Ok("没有检索到相关记忆。".into());
                }
                Ok(cortex_core::injection::render_turn_block(
                    &r.facts
                        .iter()
                        .map(cortex_proto::dto::FactDto::to_memory_item)
                        .collect::<Vec<_>>(),
                ))
            }
            Backend::Live(l) => {
                l.bind(tenant.store()?)
                    .memory_search_text(query, as_of, limit)
                    .await
            }
        }
    }

    /// 核心画像块。MCP 的 `cortex://profile` 读的就是它。
    ///
    /// mock 上回空串而不是编一段画像：一个假的「用户偏好」会被第三方 agent
    /// 当真放进上下文，而它在 mock 上分不出真假。
    pub async fn profile_text(&self, tenant: &Tenant, limit: i64) -> Result<String> {
        match &self.inner.backend {
            Backend::Mock => Ok(String::new()),
            Backend::Live(l) => l.bind(tenant.store()?).profile_text(limit).await,
        }
    }

    pub async fn get_episode(&self, tenant: &Tenant, id: &str) -> Result<EpisodeDto> {
        match &self.inner.backend {
            Backend::Mock => Ok(mock_episode(id)),
            Backend::Live(l) => l.bind(tenant.store()?).get_episode(id).await,
        }
    }

    pub async fn list_sessions(
        &self,
        tenant: &Tenant,
        q: &ListSessionsQuery,
    ) -> Result<Vec<SessionDto>> {
        match &self.inner.backend {
            // mock 里没有归档的会话，include_archived 因此不改变结果。
            // 仍然把参数收下：客户端要能在离线开发时验证自己拼对了查询串。
            //
            // project_id 则**照样过滤**：客户端 CI 里「点开项目只看得到
            // 这个项目的会话」这条断言，在一个无视过滤器的 mock 上永远是绿的，
            // 而它测的正是这个查询串有没有拼对
            Backend::Mock => Ok(mock_sessions()
                .into_iter()
                .filter(|s| q.project_id.is_none() || s.project_id == q.project_id)
                .collect()),
            Backend::Live(l) => l.bind(tenant.store()?).list_sessions(q).await,
        }
    }

    // ───────────────────────── 项目 ─────────────────────────

    pub async fn list_projects(&self, tenant: &Tenant) -> Result<Vec<ProjectDto>> {
        match &self.inner.backend {
            Backend::Mock => Ok(mock_projects()),
            Backend::Live(l) => l.bind(tenant.store()?).list_projects().await,
        }
    }

    pub async fn create_project(&self, tenant: &Tenant, name: &str) -> Result<ProjectDto> {
        match &self.inner.backend {
            // mock 不落库，但**照样跑一遍校验**：客户端 CI 里「空名字会被拒」
            // 这条断言必须在 mock 上也成立，否则它测的是一个比真实后端宽松的契约
            Backend::Mock => Ok(ProjectDto {
                id: Id::new().to_string(),
                name: crate::live::validate_project_name(name)?,
                created_at: Utc::now().to_rfc3339(),
                session_count: 0,
            }),
            Backend::Live(l) => l.bind(tenant.store()?).create_project(name).await,
        }
    }

    pub async fn patch_project(
        &self,
        tenant: &Tenant,
        id: &str,
        patch: ProjectPatch,
    ) -> Result<ProjectDto> {
        match &self.inner.backend {
            Backend::Mock => {
                let name = patch.name.as_deref().ok_or_else(|| {
                    CortexError::Invalid("请求体里没有任何要改的字段（name）".into())
                })?;
                let mut p = mock_projects()
                    .into_iter()
                    .find(|p| p.id == id)
                    .unwrap_or_else(|| mock_projects().swap_remove(0));
                p.id = id.to_string();
                p.name = crate::live::validate_project_name(name)?;
                Ok(p)
            }
            Backend::Live(l) => l.bind(tenant.store()?).patch_project(id, patch).await,
        }
    }

    pub async fn delete_project(&self, tenant: &Tenant, id: &str) -> Result<()> {
        match &self.inner.backend {
            Backend::Mock => Ok(()),
            Backend::Live(l) => l.bind(tenant.store()?).delete_project(id).await,
        }
    }

    pub async fn patch_session(
        &self,
        tenant: &Tenant,
        id: &str,
        patch: SessionPatch,
    ) -> Result<SessionDto> {
        match &self.inner.backend {
            // mock 不落库，但**照样跑一遍校验**：客户端 CI 里
            // 「绑到 C:\Windows 会被拒」这条断言必须在 mock 上也成立，
            // 否则它测的是一个比真实后端宽松的契约
            Backend::Mock => {
                let mut s = mock_sessions()
                    .into_iter()
                    .find(|s| s.id == id)
                    .unwrap_or_else(|| mock_sessions().swap_remove(0));
                s.id = id.to_string();
                if let Some(t) = patch.title.as_deref() {
                    if t.trim().is_empty() {
                        return Err(CortexError::Invalid("标题不能为空白".into()));
                    }
                    s.title = t.trim().to_string();
                    s.title_is_custom = true;
                }
                if let Some(a) = patch.archived {
                    s.archived = a;
                }
                // **走 live 那一份判断，不是自己写一遍。**
                //
                // 原先这里是 `cortex_agent::workspace::validate` —— 也就是
                // mock **接受**绑定，而 live 一律拒绝（见
                // `live::workspace_patch` 的那段理由）。契约的两个实现就此
                // 分岔：客户端 CI 跑在 mock 上，于是「服务端不接受绑定」这条
                // 在 CI 里从来没被验证过，反而验证了它的反面。
                if crate::live::workspace_patch(patch.workspace.as_ref().map(Option::as_deref))? {
                    s.workspace = None;
                }
                // 三态照搬：客户端 CI 要能验证「显式 null 是移出、字段缺席是
                // 不动」这条契约，而那件事在一个不理会 project_id 的 mock 上
                // 是测不出来的
                if let Some(p) = &patch.project_id {
                    s.project_id = p.clone();
                }
                Ok(s)
            }
            Backend::Live(l) => l.bind(tenant.store()?).patch_session(id, patch).await,
        }
    }

    pub async fn session_detail(
        &self,
        tenant: &Tenant,
        id: &str,
        q: &SessionDetailQuery,
    ) -> Result<SessionDetail> {
        match &self.inner.backend {
            Backend::Mock => {
                let session = mock_sessions()
                    .into_iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| CortexError::NotFound {
                        kind: "session",
                        id: id.into(),
                    })?;
                // mock 只有一条消息，翻不了页。但游标**照样校验** ——
                // 客户端 CI 里「传了个坏游标会拿到 400」这条断言必须在 mock 上
                // 也成立，否则它测的是一个比真实后端宽松的契约
                if let Some(raw) = q.before.as_deref() {
                    let _ = crate::cursor::decode(raw)?;
                }
                Ok(SessionDetail {
                    episodes: vec![mock_episode("01JEPISODE000000000000001")],
                    session,
                    has_more: false,
                    next_cursor: None,
                })
            }
            Backend::Live(l) => l.bind(tenant.store()?).session_detail(id, q).await,
        }
    }

    // ───────────────────────── 同步 ─────────────────────────

    pub async fn sync_since(&self, tenant: &Tenant, q: SyncQuery) -> Result<SyncResponse> {
        match &self.inner.backend {
            Backend::Mock => {
                // 尚无真实 sync_log，但游标语义先跑通：客户端能验证
                // 「拉到空即已追平」的收敛逻辑，而不是等接线后才发现协议误解。
                let _ = q.limit.clamp(1, 1000);
                Ok(SyncResponse {
                    cursor: q.since,
                    records: vec![],
                    has_more: false,
                })
            }
            Backend::Live(l) => l.bind(tenant.store()?).sync_since(q).await,
        }
    }
}

// ─────────────────────── mock 数据源 ───────────────────────

fn mock_episode(id: &str) -> EpisodeDto {
    EpisodeDto {
        id: id.to_string(),
        session_id: "01JSESSION0000000000000001".into(),
        role: "user".into(),
        text: Some("对象存储先用 RustFS，第一版单卷即可。".into()),
        occurred_at: Utc::now().to_rfc3339(),
        attachments: vec![],
        // 伪造一条注入归因，让客户端能在离线开发时把「为什么记得这个」
        // 抽屉真的画出来 —— 与 mock_chat_stream 伪造记忆事件同一个理由
        memories: vec![InjectedMemoryDto {
            fact_id: "01JFACT00000000000000001".into(),
            statement: Some("Cortex 的对象存储使用 RustFS".into()),
            domain: Some("coding".into()),
            channels: vec!["bm25".into(), "vector".into()],
            score: Some(0.032),
            invalidated: false,
            source_episode_id: Some("01JEPISODE000000000000000".into()),
        }],
        tool_calls: vec![],
    }
}

/// 假项目。**刻意只有一个**，且 mock 的会话里恰好一个属于它、一个不属于 ——
/// 客户端要能在离线开发时同时画出「项目里的会话」与「未分组」两种形态，
/// 而两条样本都在同一个项目里的话，未分组那段 UI 一次都不会被执行到。
fn mock_projects() -> Vec<ProjectDto> {
    vec![ProjectDto {
        id: MOCK_PROJECT.into(),
        name: "Cortex 自研".into(),
        created_at: Utc::now().to_rfc3339(),
        session_count: 1,
    }]
}

/// mock 里那个项目的 id。会话与项目两处都引它，写成字面量迟早对不上，
/// 而对不上的表现是「mock 上项目永远是空的」。
const MOCK_PROJECT: &str = "01JPROJECT0000000000000001";

fn mock_sessions() -> Vec<SessionDto> {
    let now = Utc::now().to_rfc3339();
    vec![
        SessionDto {
            id: "01JSESSION0000000000000001".into(),
            title: "技术选型讨论".into(),
            created_at: now.clone(),
            updated_at: now.clone(),
            message_count: 12,
            preview: Some("那就先按 RustFS 单卷来。".into()),
            title_is_custom: false,
            archived: false,
            workspace: None,
            runtime: cortex_proto::dto::SessionRuntimeDto::Cloud,
            project_id: Some(MOCK_PROJECT.into()),
        },
        SessionDto {
            id: "01JSESSION0000000000000002".into(),
            title: "记忆 schema 设计".into(),
            created_at: now.clone(),
            updated_at: now,
            message_count: 34,
            preview: Some("sync_log 用单游标，跨全部表。".into()),
            title_is_custom: false,
            archived: false,
            // 刻意不编一个假路径：mock 的消费者是客户端 CI，那台机器上
            // 任何编出来的路径都不存在，界面按「不存在即未绑定」渲染的话
            // 反而看不出区别。要试绑定态就 PATCH 一个真实目录上去
            workspace: None,
            runtime: cortex_proto::dto::SessionRuntimeDto::Cloud,
            // 未分组的那一条，理由见 [`mock_projects`]
            project_id: None,
        },
    ]
}

/// 假数据里的**来源通道刻意各不相同**。
///
/// 理由与下面那条「已被推翻」的样本一样：来源角标是客户端要画的东西，
/// 而四条都是 `user_stated` 的话，「亲述 / 推断 / 工具输出长得不一样」
/// 这段 UI 在真实后端接上之前一次都不会被执行到。
/// 最后一条给 `None`，覆盖加列之前的存量行（`unknown_legacy`）。
fn mock_facts() -> Vec<FactDto> {
    vec![
        FactDto {
            id: Id::new().to_string(),
            statement: "Cortex 的对象存储使用 RustFS".into(),
            predicate: Some("uses_object_storage".into()),
            domain: Some("coding".into()),
            confidence: 0.95,
            valid_at: Some("2026-08-06T00:00:00Z".into()),
            created_at: Utc::now().to_rfc3339(),
            invalidated: false,
            source_episode_id: Some(Id::new().to_string()),
            source_channel: Some("user_stated".into()),
            trust_tier: Some(1),
        },
        FactDto {
            id: Id::new().to_string(),
            statement: "全部图形界面统一用 Flutter，六端一套代码".into(),
            predicate: Some("decided".into()),
            domain: Some("coding".into()),
            confidence: 0.9,
            valid_at: Some("2026-08-07T00:00:00Z".into()),
            created_at: Utc::now().to_rfc3339(),
            invalidated: false,
            source_episode_id: Some(Id::new().to_string()),
            source_channel: Some("conversation".into()),
            trust_tier: Some(2),
        },
        // 一条**已被推翻**的事实。与伪造记忆事件、伪造游标推进同一个理由：
        // 「这条现在已经不成立了」那个角标是客户端要画的东西，
        // 没有一条这样的样本，那段 UI 只能等接上真实后端并且真的
        // redact / supersede 过一条事实之后才第一次被执行
        FactDto {
            id: Id::new().to_string(),
            statement: "同步协议采用裸 BIGSERIAL 游标".into(),
            predicate: Some("decided".into()),
            domain: Some("coding".into()),
            confidence: 0.72,
            valid_at: Some("2026-08-05T00:00:00Z".into()),
            created_at: Utc::now().to_rfc3339(),
            invalidated: true,
            source_episode_id: Some(Id::new().to_string()),
            source_channel: Some("tool_output".into()),
            trust_tier: Some(3),
        },
        FactDto {
            id: Id::new().to_string(),
            statement: "同步协议采用 sync_log outbox + advisory lock 串行化".into(),
            predicate: Some("decided".into()),
            domain: Some("coding".into()),
            confidence: 1.0,
            valid_at: Some("2026-08-07T00:00:00Z".into()),
            created_at: Utc::now().to_rfc3339(),
            invalidated: false,
            source_episode_id: Some(Id::new().to_string()),
            source_channel: None,
            trust_tier: None,
        },
    ]
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod backend_choice_tests {
    use super::BackendChoice;

    /// 默认必须是 live，而认不出的取值必须**报错而不是回落**。
    ///
    /// 这条守的是一次修复的完整性。原来的行为是「连不上数据库就悄悄跑
    /// mock」，其后果是 cortexd 可以带着一库假记忆对外服务而看起来完全正常。
    /// 修法是把 mock 变成显式选择 —— 但如果打错的取值会回落到某个默认值，
    /// 那扇门就等于没关：一个 `CORTEX_BACKEND=moc` 会以「我配了 mock」的
    /// 心情跑成别的东西，反之亦然。
    #[test]
    fn mock_must_be_asked_for_by_name() {
        assert_eq!(BackendChoice::parse("live").unwrap(), BackendChoice::Live);
        assert_eq!(BackendChoice::parse("mock").unwrap(), BackendChoice::Mock);
        assert_eq!(BackendChoice::parse("MOCK").unwrap(), BackendChoice::Mock);
        assert_eq!(
            BackendChoice::parse("  mock  ").unwrap(),
            BackendChoice::Mock
        );

        let err = BackendChoice::parse("moc").expect_err("打错的取值必须报错，不能回落");
        assert!(
            err.to_string().contains("CORTEX_BACKEND"),
            "报错要点名是哪个变量，实际：{err}"
        );

        // 空串在 from_env 那侧按「没设」处理（compose 的 `${VAR:-}`），
        // 但直接调 parse 传空串是调用方的错，该报错
        assert!(BackendChoice::parse("").is_err());
    }
}

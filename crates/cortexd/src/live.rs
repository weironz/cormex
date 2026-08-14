//! 真实后端 —— 把存储层、供应商层、记忆引擎接到 HTTP 契约上。
//!
//! 与 [`crate::state::mock`] 的契约完全一致：路由与 DTO 不因后端切换而变。
//!
//! # 这里**没有** agent 循环
//!
//! cortexd 是记忆服务，不跑对话。agent 一律是另一个进程里的 `cortex-local`
//! ——桌面端上是本机那个，云端上是容器里那个（`--exec-env=container`，
//! 由 [`crate::sandbox_proxy`] 逐字节反代过去）。
//!
//! 此前这里还留着一份进程内的循环，只在部署接不上 docker 时走到。删掉它的
//! 理由不是它有 bug，而是**它是第二份实现**：同一个 `Turn::run` 有两处装配，
//! 于是每加一个工具、每改一次注入纪律都要记得改两遍，而漏掉的那一遍没有
//! 任何测试会红——两条路的用户是不同的人群。更要紧的是，把 agent 赶出去
//! 之后，我们自己的 agent 走的就是第三方走的那条路（`/llm/stream` 借模型、
//! `/episodes` 写记忆、`/mcp` 查记忆），那个 API 才不会烂。
//!
//! # 一轮对话落到这里是什么
//!
//! ```text
//! agent 跑完一轮 → POST /episodes
//!   ├─ 1. 落 L0：episodes + sync_log 同事务（advisory lock 串行化）
//!   ├─ 2. 四路召回 + RRF + 预算截断
//!   ├─ 3. 渲染注入块回给 agent（记忆是数据不是指令）
//!   └─ 4. 异步抽取事实（不阻塞回执）
//! ```
//!
//! 第 4 步必须异步：抽取要调一次 LLM，同步做会让每轮对话多等几秒，
//! 而抽取结果对**本轮**毫无用处——它是给下一轮准备的。

use std::sync::Arc;

use chrono::{DateTime, Utc};
use cortex_core::{Config, CortexError, Id, Result};
use cortex_llm::{LlmClient, MessageStream};
use cortex_memory::{
    Retrieved, Retriever,
    embed::SharedEmbedder,
    extract::{ExtractContext, Extractor, TurnOrigin},
    injection,
};
use cortex_store::{NewEpisode, Role, Store};

use crate::dto::*;
use cortex_proto::episodes::{EpisodeAck, NewEpisodeRequest};
use cortex_proto::llm::{LlmStreamRequest, ModelTier};

/// `PATCH /sessions` 里那个 `workspace` 字段该怎么处置。
///
/// 返回 `true` = 要解绑；`false` = 这次没提这个字段。绑定一律报错。
///
/// # 为什么绑定要**拒绝**而不是静默忽略
///
/// 这条路曾经是 Web 端绑工作区的回落（本地 agent 不在时），而它绑的是
/// **服务器上**的一个目录 —— 于是一个远端用户的 `read_file` 动的是生产机的
/// 文件系统，爆炸半径是整台机器加上所有租户的数据。
///
/// 静默忽略的话，客户端会显示「已绑定 D:\myproject」，然后每个文件操作都
/// 失败得莫名其妙 —— 而用户明明看到绑定成功了。报错里直接给出两条走得通的路。
///
/// # 为什么解绑照旧放行
///
/// 老会话上可能还留着一条服务端绑定（这次改动之前存下的）。拒绝解绑等于让
/// 那条记录永远焊在那儿，而用户唯一能做的就是删掉整个会话。
///
/// # 为什么抽成函数
///
/// `patch_session` 要一个真数据库才进得去，于是这条判断在那里是测不到的。
/// 而它恰恰是本次改动里最该有测试的一条 —— 写反了就是**服务端又能绑了**，
/// 且没有任何症状。
pub(crate) fn workspace_patch(field: Option<Option<&str>>) -> Result<bool> {
    match field {
        Some(Some(_)) => Err(CortexError::Invalid(
            "cortexd 进程自己不提供文件执行环境，不能在这里绑定一个宿主机路径。\
             文件与命令有两条路：跑在**你自己的机器**上（桌面端，\
             或在本机运行 cortex-local —— `cortex` 命令行会自己拉起它），\
             或者打开**云沙箱** —— 那一轮的工作区是容器里的 /workspace，\
             由服务端自己管，不需要也不接受外部路径。"
                .into(),
        )),
        Some(None) => Ok(true),
        None => Ok(false),
    }
}

/// 校验一个项目名，返回 trim 之后的那份。
///
/// # 为什么是一个独立的纯函数
///
/// 与 [`workspace_patch`] 同一个理由：真正的项目路径要一个数据库才进得去，
/// 于是这条判断在那里测不到。而它同时被 mock 后端复用 —— 客户端 CI 里
/// 「空名字会被拒」这条断言必须在 mock 上也成立，否则它测的是一个比真实
/// 后端宽松的契约。
///
/// # 为什么 trim 之后判空，而不是直接判空
///
/// 输入框里敲一个空格就能造出一个名字是空白的项目，而它在侧边栏上是一行
/// 什么都没有的可点区域 —— 看起来像界面坏了。
///
/// 上限按**字符**而非字节数：数据库那道 CHECK 用的是 `length()`，
/// 它数的也是字符。两处口径不同的症状是「明明没超，服务端却回 500」。
pub(crate) fn validate_project_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(CortexError::Invalid("项目名不能为空白".into()));
    }
    if name.chars().count() > cortex_store::PROJECT_NAME_MAX_CHARS {
        return Err(CortexError::Invalid(format!(
            "项目名过长（上限 {} 字符）",
            cortex_store::PROJECT_NAME_MAX_CHARS
        )));
    }
    Ok(name.to_owned())
}

/// 同时最多几路抽取。见 [`Live::extract_permit`]。
const EXTRACT_CONCURRENCY_ENV: &str = "CORTEX_EXTRACT_CONCURRENCY";

/// 默认 3。
///
/// 不按 CPU 核数算：抽取的瓶颈是**供应商那一端**，本地几乎不耗 CPU
/// （等一次 HTTP 往返）。按核数算会在大机器上放出几十路并发，撞的是
/// 供应商的速率限制，而那表现为一批 429 —— 每一个 429 都是一轮记忆丢了。
///
/// 3 是个保守值：正常聊天下永远排不上队（人打字比这慢得多），
/// 而批量写入时它把峰值削平。不够用就调大，那是个明确的运维动作。
const DEFAULT_EXTRACT_CONCURRENCY: usize = 3;

/// 读并解析 [`EXTRACT_CONCURRENCY_ENV`]。
fn extract_concurrency() -> Result<usize> {
    parse_extract_concurrency(
        std::env::var(EXTRACT_CONCURRENCY_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .as_deref(),
    )
}

/// 与 `MAX_ROUNDS_ENV` 同样的纪律：取值非法就报错，不悄悄用默认值 ——
/// 配错了却照跑，等于上限失效而运维完全不知情。
///
/// 0 也算非法：那不是「不限制」，是「一路都不许跑」，
/// 而想关掉抽取该有它自己的开关。
///
/// 单独一个纯函数（而不是直接读环境变量）是为了能测：`env::set_var`
/// 改的是进程全局状态，同二进制里的别的测试会跟着一起踩。
fn parse_extract_concurrency(raw: Option<&str>) -> Result<usize> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_EXTRACT_CONCURRENCY);
    };
    match raw.trim().parse::<usize>() {
        Ok(0) | Err(_) => Err(CortexError::Config(format!(
            "{EXTRACT_CONCURRENCY_ENV} 必须是正整数，实际是 {raw:?}"
        ))),
        Ok(n) => Ok(n),
    }
}

/// 在闸门下跑一段抽取。**许可一直持有到它结束。**
///
/// 单独一个函数，是因为这里有一个很容易写错、且写错之后**一切照常绿**
/// 的地方：
///
/// ```ignore
/// let _ = gate.acquire_owned().await;   // ← 许可当场就被丢掉了，闸门失效
/// let _permit = gate.acquire_owned().await;  // ← 才是对的
/// ```
///
/// `_` 是「立刻丢弃」，`_permit` 是「绑定但不用」。两者只差几个字符，
/// 而前者让整个信号量变成一句昂贵的空操作 —— 没有任何测试会因此变红，
/// 除非有人专门去测并发度。收进这一个函数之后，那条测试只要写一次。
async fn under_gate<F: Future>(gate: &Arc<tokio::sync::Semaphore>, fut: F) -> F::Output {
    // acquire 只在信号量被 close 时失败，而我们从不 close 它。
    // 真失败了也照跑：抽取不该因为闸门坏了就整个停摆
    let _permit = Arc::clone(gate).acquire_owned().await.ok();
    fut.await
}

/// 会话列表一次给多少条。
///
/// 上限作用在**会话**上而不是消息上 —— 存储层用一句 SQL 聚合，
/// 不再是「拉最近 N 条消息再归纳」（那样一个话痨会话就能把窗口占满，
/// 别的会话直接从列表里消失）。
const SESSION_LIST_LIMIT: i64 = 200;

/// 会话标题取首条用户消息的前多少字。
const TITLE_CHARS: usize = 24;

/// 克隆是廉价的 —— 每个字段要么是 `Arc`，要么是 `Arc` 语义的句柄
/// （`Store` 内部是连接池），要么是标量。[`Live::bind`] 靠它按请求换库。
#[derive(Clone)]
pub struct Live {
    store: Store,
    llm: LlmClient,
    retriever: Retriever<SharedEmbedder>,
    extractor: Arc<Extractor>,
    /// 媒体转录。未配置 vision 模型时为 None —— 图片照常归档，只是检索不到
    transcribe: Option<Arc<cortex_memory::TranscribePipeline>>,
    device_id: String,
    context_window: usize,
    /// 当前的向量化后端。
    ///
    /// 单独留一份而不是从 `retriever` 里掏：换模型的回填器要用它算向量，
    /// `/health` 的普查要用它的 `model_id()` 做比对。`SharedEmbedder` 是
    /// `Arc`，多一个句柄不多一份模型。
    embedder: SharedEmbedder,
    /// 同时最多几路抽取。用法见 [`under_gate`]。
    ///
    /// # 为什么必须有这个闸门
    ///
    /// 抽取是**裸 `tokio::spawn`**：一次 `/episodes` 一个任务，谁都不看别人。
    /// 正常聊天下这没问题（人打字的速度就是天然的限流），但只要有一个客户端
    /// 连续快速写入 —— 历史导入是最典型的那个 —— 瞬间就是几千个并发任务，
    /// 每个都要调一次 LLM。
    ///
    /// 生产那台机器是 2 核 3.5 GB，上面已经跑着十几个容器。几千路并发抽取
    /// 不是「慢一点」，是把整台机器连同别的服务一起拖死。
    ///
    /// # 为什么在 spawn **之后**才等许可
    ///
    /// 等在 spawn 之前的话，`/episodes` 的响应会被抽取排队卡住 —— 而抽取的
    /// 全部设计前提就是「绝不阻塞调用方，它是给下一轮准备的」。现在排队的是
    /// 那些后台任务自己，调用方照样立刻拿到响应。
    ///
    /// 代价说清楚：**这不是背压**。写得太快时队列会一直长，只是长在内存里的
    /// parked task 上。真正的节流得由客户端自己做（导入器就是这么做的），
    /// 这个闸门保的是 CPU 与供应商配额。
    extract_gate: Arc<tokio::sync::Semaphore>,
}

/// 已经绑定到某个租户的 [`Live`]。
///
/// # 它存在的全部意义：让「忘了绑定」编译不过
///
/// 读写数据的那十个方法在 `Live` 上是**模块私有**的，只有这里转发出去。
/// 于是 `state.rs` 拿着 `Backend::Live(l)` 也调不到 `l.get_episode(…)` ——
/// 必须先 `l.bind(tenant.store()?)`。
///
/// 这不是洁癖。上一版里 `TenantPools` 建好了、隔离测过了、故障注入过了，
/// 唯独**没有一处调用它** —— 请求路径一直握着启动时那个连 `public` 的
/// `Store`，两个登录的用户读写同一片库。而那个状态下所有测试都是绿的：
/// 没有任何测试会自然覆盖到「这个方法用的是哪个连接池」。
///
/// 类型能覆盖到。
pub struct BoundLive(Arc<Live>);

impl BoundLive {
    /// 借出内层。
    ///
    /// 给**后台任务**用：它们要调 `store()` / `embedder()` /
    /// `transcribe_and_store()` 这些与租户无关的公开方法，而那些方法长在
    /// `Live` 上。绑定过的这一份，`store` 字段已经指向那个租户了。
    ///
    /// 请求路径**不要**用它绕过转发层 —— 那等于把「不绑定就调不到」
    /// 这个保证还回去。`state.rs` 那条守卫测试盯着的就是这件事。
    #[must_use]
    pub fn as_live(&self) -> &Arc<Live> {
        &self.0
    }

    // 以下全是转发。签名与 `Live` 上那份逐字相同，只是多了一层。
    //
    // 想过用 `Deref<Target = Live>` 省掉这一坨 —— 不行：那样
    // `Live` 上的方法就又能直接调了，而「不绑定就调不到」正是这个类型
    // 唯一的产出。转发是这个保证的价格。

    pub async fn write_episode(
        &self,
        req: NewEpisodeRequest,
        origin: TurnOrigin,
    ) -> Result<EpisodeAck> {
        self.0.write_episode(req, origin).await
    }

    pub async fn register_blob(&self, new: cortex_store::NewBlob) -> Result<Option<i64>> {
        self.0.register_blob(new).await
    }

    pub async fn blob_meta(&self, hash: &str) -> Result<cortex_store::Blob> {
        self.0.blob_meta(hash).await
    }

    pub async fn memory_search(&self, q: MemorySearchQuery) -> Result<MemorySearchResponse> {
        self.0.memory_search(q).await
    }

    pub async fn memory_search_text(
        &self,
        query: &str,
        as_of: Option<&str>,
        limit: i64,
    ) -> Result<String> {
        self.0.memory_search_text(query, as_of, limit).await
    }

    pub async fn profile_text(&self, limit: i64) -> Result<String> {
        self.0.profile_text(limit).await
    }

    pub async fn get_episode(&self, id: &str) -> Result<EpisodeDto> {
        self.0.get_episode(id).await
    }

    pub async fn list_sessions(&self, q: &ListSessionsQuery) -> Result<Vec<SessionDto>> {
        self.0.list_sessions(q).await
    }

    pub async fn list_projects(&self) -> Result<Vec<ProjectDto>> {
        self.0.list_projects().await
    }

    pub async fn create_project(&self, name: &str) -> Result<ProjectDto> {
        self.0.create_project(name).await
    }

    pub async fn patch_project(&self, id: &str, patch: ProjectPatch) -> Result<ProjectDto> {
        self.0.patch_project(id, patch).await
    }

    pub async fn delete_project(&self, id: &str) -> Result<()> {
        self.0.delete_project(id).await
    }

    pub async fn patch_session(&self, session_id: &str, patch: SessionPatch) -> Result<SessionDto> {
        self.0.patch_session(session_id, patch).await
    }

    pub async fn session_detail(
        &self,
        session_id: &str,
        q: &SessionDetailQuery,
    ) -> Result<SessionDetail> {
        self.0.session_detail(session_id, q).await
    }

    pub async fn sync_since(&self, q: SyncQuery) -> Result<SyncResponse> {
        self.0.sync_since(q).await
    }
}

/// 一次转录的结局。
///
/// 比 [`cortex_memory::transcribe::Outcome`] 多一个 `Disabled`，少了那一大坨
/// 待写行 —— 调用方要的只是「成没成、要不要重试」。回扫器据此区分
/// 「这次失败了」与「这个永远转不了」。
#[derive(Debug)]
pub enum TranscribeOutcome {
    /// 转录完成并已落库
    Stored { segments: usize },
    /// 该 blob 已被抹除，结果整批丢弃。**不是失败，也永远不该重试**
    Redacted,
    /// 没有转录器认领这个 MIME（当前主要是音视频）。
    /// 将来 ASR 落地后重跑即可，但在此之前重试多少次都是同一个结果
    Unsupported { mime: String },
    /// 没配 vision 模型，整条链路关闭
    Disabled,
}

impl TranscribeOutcome {
    fn log(&self, hash: &str) {
        match self {
            Self::Stored { segments } => tracing::info!(hash, segments, "媒体转录已落库"),
            Self::Redacted => tracing::info!(hash, "blob 在转录期间被抹除，结果已丢弃"),
            Self::Unsupported { mime } => tracing::debug!(hash, mime, "没有转录器认领这个类型"),
            Self::Disabled => tracing::debug!(hash, "未配置 vision 模型，跳过转录"),
        }
    }
}

/// 把 `store.is_redacted` 接到转录管线的 [`RedactionGuard`] 上。
///
/// 管线把这个约束做进了签名（要 trait 而非闭包），调用方没有办法漏掉它 ——
/// 漏掉的后果是 redact 执行时正在途中的转录任务，事后把已抹除内容
/// 又写回 `blob_transcripts`（memory.md §十一 的在途任务防护）。
struct StoreRedactionGuard {
    store: Store,
}

#[async_trait::async_trait]
impl cortex_memory::transcribe::RedactionGuard for StoreRedactionGuard {
    async fn is_redacted(&self, blob_hash: &str) -> Result<bool> {
        self.store
            .is_redacted(cortex_store::RedactionTarget::Blob, blob_hash)
            .await
            .map_err(store_err)
    }
}

impl Live {
    pub async fn new(config: &Config, embedder: SharedEmbedder) -> Result<Self> {
        // 全局那套（cortex_auth）先跑：账号表要在任何人能登录之前就位。
        //
        // 它与租户那套是**两套独立的 migration**，版本表也各在各的 schema 里 ——
        // 混成一套会让新租户建号时撞上 `relation "users" already exists`，
        // 而失败点在建号中途。见 migrations-global/ 的文件头
        Store::migrate_global(&config.database_url)
            .await
            .map_err(|e| CortexError::Store(format!("全局 schema 迁移失败：{e}")))?;

        let store = Store::connect(&config.database_url)
            .await
            .map_err(|e| CortexError::Store(format!("连接数据库失败：{e}")))?;

        // 1 号用户的 schema 就是 public，所以这一句同时也是「给他迁移」。
        //
        // 多租户接线之后这里换成 `cortex_store::migrate_all`，逐个迁并把
        // 坏掉的那几个单独标出来 —— 一个用户的库坏了不该让所有人下线
        store
            .migrate()
            .await
            .map_err(|e| CortexError::Store(format!("数据库迁移失败：{e}")))?;

        // `api_key_env` 为空 = 该供应商免鉴权（本机 Ollama、以及任何
        // 不校验 key 的自建端点）。
        //
        // **原先这里没有这一支**，于是 `std::env::var("")` 必然失败，
        // 报「缺少 ollama 的 API key 环境变量」—— 一条读起来像配置漏了、
        // 实际上无论怎么配都过不去的错误。cortexd 因此根本起不来，
        // 而 `cortex-llm::LlmClient::from_config` 那条路一直是对的：
        // 同一个判断在两处，只写对了一处。
        let key_var = cortex_llm::provider::api_key_env(&config.llm.provider)?;
        let api_key = if key_var.is_empty() {
            String::new()
        } else {
            std::env::var(&key_var).map_err(|_| {
                CortexError::Config(format!(
                    "缺少 {} 的 API key 环境变量 {key_var}",
                    config.llm.provider
                ))
            })?
        };
        let llm = LlmClient::from_config(&config.llm, &api_key)?;

        let context_window = llm.model().context_limit();

        // 没配 CORTEX_VISION_PROVIDER 就整条关掉，而不是配了个假的转录器：
        // 后者会往库里写垃圾 caption，而垃圾比空白更难发现
        let transcribe = match cortex_memory::transcribe::VisionTranscriber::from_env()? {
            Some(v) => {
                tracing::info!("媒体转录已启用");
                Some(Arc::new(
                    cortex_memory::TranscribePipeline::new(embedder.clone())
                        .with_transcriber(Arc::new(v)),
                ))
            }
            None => None,
        };

        // 上一个模型。只在**回填期间**有用：让查询也被编到旧空间去，
        // 于是还没被补上的旧事实仍然进得了向量召回。
        //
        // ⚠️ 它救不了「fastembed → api」那一次迁移 —— 旧模型是进程内的
        // fastembed，而它默认不编进二进制。见 ApiEmbedder::legacy_from_env
        let legacy: Option<SharedEmbedder> =
            cortex_memory::embed_api::ApiEmbedder::legacy_from_env()?
                .map(|e| Arc::new(e) as SharedEmbedder);
        if let Some(l) = &legacy {
            tracing::info!(
                legacy = l.model_id(),
                current = embedder.model_id(),
                "挂上了上一个 embedding 模型：回填完成之前，查询会同时在两个空间里捞"
            );
        }

        let extract_permits = extract_concurrency()?;
        tracing::info!(
            concurrent = extract_permits,
            "抽取并发上限（{EXTRACT_CONCURRENCY_ENV} 可调）"
        );

        Ok(Self {
            retriever: Retriever::new(embedder.clone()).with_legacy(legacy),
            extractor: Arc::new(Extractor::new(
                llm.clone(),
                embedder.clone(),
                config.device_id.clone(),
            )),
            embedder,
            transcribe,
            store,
            llm,
            device_id: config.device_id.clone(),
            context_window,
            extract_gate: Arc::new(tokio::sync::Semaphore::new(extract_permits)),
        })
    }

    /// 本地 agent 把一轮对话写回记忆库。
    ///
    /// 这是 [`run_turn`] 的「记忆那一半」被单独拎出来的形态：agent 循环搬到
    /// 本地之后，循环在那边跑，落库与检索留在这边。两条路走的是**同一段**
    /// 逻辑（写 episode → 检索 → 归因 → 工具归因 → 抽取），刻意没有分叉。
    ///
    /// # 幂等，且判重在事务外
    ///
    /// 离线队列重放必然重复投递。判重是先读一次 `episodes`，命中就直接返回 ——
    /// **不能**改成 `INSERT ... ON CONFLICT DO NOTHING`：`Guarded::insert_row`
    /// 无条件往 `sync_log` 追一行，空操作会留下一条指向早已同步的 episode
    /// 的幽灵行，而 `sync_log` 是同步的唯一事实序。
    ///
    /// 读放在事务外还有第二个理由：`write_txn` 刻意不提供读方法
    ///（见 `cortex-store::txn` 的纪律三，取号事务必须短小纯写）。
    ///
    /// 真并发下两个重放同时穿过判重，后一个会撞 PK 唯一约束 ——
    /// 那条路由 [`Self::write_episode`] 的调用方映射成同样的「已存在」。
    async fn write_episode(
        &self,
        req: NewEpisodeRequest,
        origin: TurnOrigin,
    ) -> Result<EpisodeAck> {
        let episode_id: Id = req
            .id
            .parse()
            .map_err(|_| CortexError::Invalid(format!("episode id 不是合法 ULID：{}", req.id)))?;
        let role = match req.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            other => {
                return Err(CortexError::Invalid(format!(
                    "role 只接受 user / assistant，收到 {other}；\
                     tool 与 system 不是「一轮对话」，它们由工具归因与系统提示各自表达"
                )));
            }
        };

        // ── 幂等判重 ──
        if self
            .store
            .episode(&req.id)
            .await
            .map_err(store_err)?
            .is_some()
        {
            tracing::debug!(episode = %req.id, "这条 episode 已经写过，本次是空操作");
            return Ok(EpisodeAck {
                episode_id: req.id,
                memories: Vec::new(),
                already_existed: true,
            });
        }

        let occurred_at = match req.occurred_at.as_deref() {
            None => Utc::now(),
            Some(raw) => chrono::DateTime::parse_from_rfc3339(raw)
                .map(|t| t.with_timezone(&Utc))
                .map_err(|e| {
                    CortexError::Invalid(format!(
                        "occurred_at 不是合法的 RFC 3339 时间：{raw}（{e}）"
                    ))
                })?,
        };

        // ── 附件预检（事务外，理由同 run_turn 步骤 0）──
        let attachments = dedup_attachments(&req.attachments);
        for a in &attachments {
            if self.store.blob(&a.hash).await.map_err(store_err)?.is_none() {
                return Err(CortexError::Invalid(format!(
                    "附件 {} 尚未登记；请先 POST /blobs 上传，或直传后 POST /blobs/commit",
                    a.hash
                )));
            }
        }

        // ── 写 L0。tsv **由服务端算** ──
        //
        // 不接受客户端算好的 tsv：它决定 BM25 那一路能不能召回，算错了
        // 没有任何症状（只是搜不到），而客户端有很多个、版本各不相同。
        // 「tsv 与主行同事务」那条约束也只有在这一侧才保证得了。
        let tsv = cortex_memory::tokenize::to_tsvector_input(&req.text);
        let ep = NewEpisode {
            id: episode_id,
            session_id: req.session_id.clone(),
            role,
            content: serde_json::json!({ "text": req.text }),
            text: Some(req.text.clone()),
            tsv_source: Some(tsv),
            domain: None,
            device_id: self.device_id.clone(),
            occurred_at,
        };
        let links: Vec<cortex_store::NewEpisodeBlob> = attachments
            .iter()
            .map(|a| cortex_store::NewEpisodeBlob {
                episode_id,
                blob_hash: a.hash.clone(),
                kind: a.kind.clone(),
                filename: a
                    .filename
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| clamp_chars(s, cortex_store::ATTACHMENT_FILENAME_MAX_CHARS)),
            })
            .collect();

        match self
            .store
            .write_txn(async |t| {
                let mut last = t.insert_episode(&ep).await?;
                for link in &links {
                    last = t.link_episode_blob(link).await?;
                }
                Ok(last)
            })
            .await
        {
            Ok(_) => {}
            // 判重与写入之间挤进了另一个重放。这是**正常路径**，不是错误 ——
            // 报 500 会让客户端把一次成功的重放当成失败，然后永远重试下去
            Err(e) if e.is_duplicate_key() => {
                tracing::debug!(episode = %req.id, "并发重放撞上唯一约束，按已存在处理");
                return Ok(EpisodeAck {
                    episode_id: req.id,
                    memories: Vec::new(),
                    already_existed: true,
                });
            }
            Err(e) => return Err(store_err(e)),
        }

        // ── 检索 + 归因 ──
        let mut memories = Vec::new();
        if req.retrieve && role == Role::User {
            let retrieved = self
                .retriever
                .retrieve(&self.store, &req.text, None, self.context_window)
                .await?;
            if !retrieved.items.is_empty() {
                memories = retrieved.items.iter().map(fact_dto_of).collect();
                record_injected_memories(&self.store, episode_id, &retrieved, &self.device_id)
                    .await;
            }
        }

        // ── 工具归因（单独事务，理由见 run_turn 步骤 5b）──
        if !req.tool_calls.is_empty() {
            let anchor = req.anchor_episode_id.as_deref().unwrap_or(&req.id);
            let anchor: Id = anchor.parse().map_err(|_| {
                CortexError::Invalid(format!("anchor_episode_id 不是合法 ULID：{anchor}"))
            })?;
            let rows: Vec<cortex_store::NewEpisodeToolCall> = req
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, c)| cortex_store::NewEpisodeToolCall {
                    id: Id::new(),
                    episode_id: anchor,
                    ordinal: i as i32,
                    name: clamp_chars(&c.name, cortex_store::TOOL_NAME_MAX_CHARS),
                    path: c
                        .path
                        .as_deref()
                        .map(|p| clamp_chars(p, cortex_store::TOOL_PATH_MAX_CHARS)),
                    summary: clamp_chars(&c.summary, cortex_store::TOOL_SUMMARY_MAX_CHARS),
                    ok: c.ok,
                    device_id: self.device_id.clone(),
                    // 本地 agent 灌上来的那条路（POST /episodes）。diff 与
                    // path 一样是客户端给的，同样要夹一次上限 —— 入库上限
                    // 是这一层的责任，不能指望上游都夹好了
                    diff: c
                        .diff
                        .as_deref()
                        .map(|d| clamp_chars(d, cortex_store::TOOL_DIFF_MAX_CHARS)),
                })
                .collect();
            let n = rows.len();
            let res = self
                .store
                .write_txn(async |t| {
                    let mut last = 0;
                    for c in &rows {
                        last = t.insert_episode_tool_call(c).await?;
                    }
                    Ok(last)
                })
                .await;
            // 与 run_turn 同样的取舍：对话已经完成，归因写不进去是可观测性的
            // 损失，不该表现为一次失败的写入
            if let Err(e) = res {
                tracing::warn!(error = %e, calls = n, "工具归因落库失败");
            }
        }

        // ── 异步抽取。绝不阻塞调用方 ──
        if role == Role::Assistant {
            self.spawn_extraction(
                req.anchor_episode_id.clone(),
                req.text.clone(),
                occurred_at,
                origin,
            );
        }

        Ok(EpisodeAck {
            episode_id: req.id,
            memories,
            already_existed: false,
        })
    }

    /// 把这一轮送进抽取管线。
    ///
    /// user 那半从库里读回来而不是让客户端再传一遍：同一句话有两个来源，
    /// 就有了「两边不一样」这条要处理的路，而库里那一份才是权威。
    fn spawn_extraction(
        &self,
        anchor: Option<String>,
        reply: String,
        occurred_at: DateTime<Utc>,
        origin: TurnOrigin,
    ) {
        let Some(anchor) = anchor else {
            tracing::debug!("assistant episode 没带 anchor_episode_id，跳过抽取");
            return;
        };
        let Ok(anchor_id) = anchor.parse::<Id>() else {
            tracing::warn!(anchor, "anchor_episode_id 不是合法 ULID，跳过抽取");
            return;
        };
        let store = self.store.clone();
        let extractor = Arc::clone(&self.extractor);
        let gate = Arc::clone(&self.extract_gate);
        tokio::spawn(async move {
            // 排队。**在 spawn 之后才等** —— 调用方早就拿到响应了，
            // 排的是这个后台任务自己。见 `Live::extract_gate` 那个字段的文档
            under_gate(&gate, async move {
                let user_text = match store.episode(&anchor).await {
                    Ok(Some(ep)) => ep.text.unwrap_or_default(),
                    Ok(None) => {
                        tracing::warn!(anchor, "抽取找不到对应的 user episode，跳过");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, anchor, "抽取读 user episode 失败，跳过");
                        return;
                    }
                };
                let text = format!("用户：{user_text}\n助手：{reply}");
                // 信任级**只由来源决定**，而来源只由凭据决定（见
                // `routes::write_episode` 那段）。让请求体说了算，等于让一个
                // 被注入的 agent 自己挑落哪一档
                let ctx = ExtractContext::new(anchor_id, occurred_at).from(origin);
                match extractor.ingest(&store, &text, &ctx).await {
                    Ok(report) => tracing::info!(
                        candidates = report.candidates,
                        written = report.written.len(),
                        superseded = report.superseded.len(),
                        duplicates = report.duplicates,
                        "本轮抽取完成（本地 agent）"
                    ),
                    Err(e) => tracing::warn!(error = %e, "本轮抽取失败（本地 agent）"),
                }
            })
            .await;
        });
    }

    /// LLM 代理：把本地 agent 的一次流式调用原样转给供应商。
    ///
    /// **纯转发，不碰记忆、不写库。** 本地那侧走完这一趟拿到的是与直连
    /// 完全相同的 `MessageStream`（见 `cortex_proto::llm` 的模块注释：
    /// 线上传的就是 goose 的原生类型，没有中间格式）。
    ///
    /// # 模型由服务端选，不由客户端点名
    ///
    /// 请求里只有档位。让客户端指定模型名，等于让任何拿到 token 的人
    /// **拿服务端的 key 跑任意模型** —— 而账单是服务端付。
    /// 同上，但可以指定用**哪一把 key** 调。
    ///
    /// `own` 是用户自己填的那把。给了就现建一个 provider 用它，
    /// 这次调用因此不占配额（也不花服务端的钱）。
    ///
    /// # 为什么每次现建而不是缓存
    ///
    /// 缓存要按 (用户, 供应商, key) 做键并处理失效 —— 而 key 是可以随时
    /// 被换掉或撤下的，一个还活着的缓存项意味着「撤下之后还在用旧 key 扣钱」。
    /// 建一个 provider 只是搭一个 reqwest 客户端，比它随后那次 HTTP
    /// 便宜得多。
    ///
    /// # Errors
    /// 供应商建不起来，或者上游拒绝。
    pub async fn llm_stream_with_key(
        &self,
        req: LlmStreamRequest,
        own: Option<(&str, &str, Option<&str>)>,
    ) -> Result<MessageStream> {
        let Some((provider_name, api_key, base_url)) = own else {
            let model = match req.tier {
                ModelTier::Main => self.llm.model(),
                ModelTier::Cheap => self.llm.cheap_model(),
            };
            return Ok(self
                .llm
                .stream_with(model, &req.system, &req.messages, &req.tools)
                .await?);
        };

        let provider: std::sync::Arc<dyn cortex_llm::Provider> =
            cortex_llm::provider::build_with(provider_name, api_key, base_url)
                .map_err(|e| CortexError::Config(format!("自带 key 建不起供应商：{e}")))?
                .into();
        // 模型配置沿用服务端那份 —— 用户填的是 key，不是模型。
        // 让他连模型一起换需要一整套「这个供应商有哪些模型」的校验，
        // 而填错模型名的表现是每次对话都失败
        let client = cortex_llm::LlmClient::from_provider(
            provider,
            provider_name,
            self.llm.model().clone(),
            self.llm.cheap_model().clone(),
        );
        let model = match req.tier {
            ModelTier::Main => client.model(),
            ModelTier::Cheap => client.cheap_model(),
        };
        Ok(client
            .stream_with(model, &req.system, &req.messages, &req.tools)
            .await?)
    }

    pub async fn database_status(&self) -> String {
        match self.store.ping().await {
            Ok(()) => "ok".into(),
            Err(e) => format!("error: {e}"),
        }
    }

    /// 存储层句柄。给实时推送的监听任务查游标用。
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// 换到某个租户的库上，其余部件全部共享。
    ///
    /// # 为什么是「克隆一个 Live」而不是「给每个方法加一个 store 参数」
    ///
    /// 后者要改 38 处 `self.store` 与十几个方法签名，而每一处都是**手工**
    /// 判断「这里该用租户的库还是 public 的库」—— 判断错了不报错，
    /// 只是把一个人的记忆写进另一个人的 schema。
    ///
    /// 克隆则是整体替换：绑定之后，这个 `Live` 上**所有**方法都落在那个
    /// schema 上，没有一处能例外。代价是每请求多克隆约十个 `Arc`
    /// 与一个 `String`，比它随后要做的那次数据库往返便宜三个数量级。
    ///
    /// 返回 [`BoundLive`] 而不是 `Live`：数据方法只长在它上面，
    /// **不绑定就调不到**（见 `state.rs` 里那条读源码的守卫测试）。
    #[must_use]
    pub fn bind(&self, store: &Store) -> BoundLive {
        BoundLive(Arc::new(Self {
            store: store.clone(),
            ..self.clone()
        }))
    }

    /// 当前的向量化后端。回填器要用它算向量。
    #[must_use]
    pub fn embedder(&self) -> &SharedEmbedder {
        &self.embedder
    }

    /// 向量化的覆盖情况：当前模型覆盖了多少条有效事实、还有多少条是旧模型的。
    ///
    /// # 为什么这个数字必须能被看见
    ///
    /// 换模型之后旧向量还在，但四条向量查询全部按 `embedding_model` 过滤
    /// （见 `cortex_store::Store::recall_vector`），于是旧事实**当天就退出
    /// 向量召回**。而 BM25 / 图遍历 / recency 三路照常，所以它看起来
    /// 不像坏了，只像「语义召回好像变差了点」——
    /// 没有这个数字，没有任何人会被告知。
    pub async fn embedding_health(&self) -> crate::dto::EmbeddingHealth {
        let model = self.embedder.model_id().to_owned();
        let (covered, stale) = self
            .store
            .embedding_coverage(&model)
            .await
            .unwrap_or_else(|e| {
                // 查不到就报 (0, 0) 而不是把整个 /health 打挂：这个端点的
                // 消费者是 HEALTHCHECK 与探针，为一个诊断字段让容器转
                // unhealthy 是本末倒置。错误进日志
                tracing::warn!(error = %e, "统计 embedding 覆盖率失败");
                (0, 0)
            });
        crate::dto::EmbeddingHealth {
            model,
            covered,
            stale,
        }
    }

    /// 启动时报一次向量化的覆盖情况。
    ///
    /// 全覆盖时是一行 INFO；有旧模型的存量时是 **WARN**，并逐档列出。
    /// 这是「换模型导致静默失忆」这件事**唯一**会主动发声的地方 ——
    /// 检索侧不会报错，界面上也看不出来。
    pub async fn report_embedding_state(&self) {
        let model = self.embedder.model_id();
        let census = match self.store.embedding_census().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "embedding 普查失败，跳过启动比对");
                return;
            }
        };

        let others: Vec<String> = census
            .iter()
            .filter(|(m, _)| m != model)
            .map(|(m, n)| format!("{m}={n}"))
            .collect();

        if others.is_empty() {
            let covered = census
                .iter()
                .find(|(m, _)| m == model)
                .map_or(0, |(_, n)| *n);
            tracing::info!(model, covered, "向量化：全部事实都在当前模型下");
            return;
        }

        let (covered, stale) = self.store.embedding_coverage(model).await.unwrap_or((0, 0));
        tracing::warn!(
            current = model,
            covered,
            stale,
            others = others.join(" "),
            "换过 embedding 模型：{stale} 条事实在当前模型下**没有向量**，\
             它们进不了向量召回（BM25 / 图遍历 / 近因三路照常，所以界面上\
             只表现为「语义召回变差了点」）。回填器会补上；\
             要关掉它设 CORTEX_REEMBED=off"
        );
    }

    pub async fn latest_cursor(&self) -> i64 {
        self.store.latest_seq().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "读取 latest_seq 失败，按 0 下发");
            0
        })
    }

    // ─────────────────────────── 检索 ───────────────────────────

    /// 检索的唯一实现。HTTP 端点与 agent 的 `memory_search` 工具都走这里 ——
    /// 两条路要是各写一份，「工具查到的」与「界面上看到的」迟早会对不上，
    /// 而那正是最难解释给用户听的一类不一致。
    async fn retrieve(&self, query: &str, as_of: Option<&str>, limit: i64) -> Result<Retrieved> {
        let at = match as_of {
            Some(t) => Some(
                chrono::DateTime::parse_from_rfc3339(t)
                    .map_err(|e| CortexError::Invalid(format!("as_of 不是合法的 RFC3339：{e}")))?
                    .with_timezone(&Utc),
            ),
            None => None,
        };

        match retrieval_plan(query, at) {
            // 没有查询词时要的就是「那一刻的全貌」，按时间倒序给。
            // `at` 为 None 时那一刻就是现在
            RetrievalPlan::Snapshot(at) => {
                self.retriever
                    .retrieve_as_of(&self.store, at, limit, self.context_window)
                    .await
            }
            // 有查询词就该在**同一份 as-of 快照上**做相关性排序。
            // 快照才是回放的本质，「按时间倒序」只是个跟问题无关的排序 ——
            // 评测实测：换成快照上的三路 RRF 后，时间回放 R@5
            // 0.417 → 0.833，gold 平均名次 11.5 → 2.8。
            RetrievalPlan::SnapshotRanked(at) => {
                self.retriever
                    .retrieve_as_of_ranked(&self.store, query, at, limit, self.context_window)
                    .await
            }
            RetrievalPlan::Ranked => {
                self.retriever
                    .retrieve(&self.store, query, None, self.context_window)
                    .await
            }
        }
    }

    /// 给一批检索结果补上「这条**此刻**还成不成立」。见 [`FactDto::invalidated`]。
    ///
    /// # 为什么只有回放路径要查库
    ///
    /// 不带 `as_of` 的四路召回全部走 `active_facts` 视图，按定义就查不到
    /// 已失效的事实 —— 那条路上恒为 `false`，一次查询都不必发。
    /// 只有 `as_of` 快照会返回「当时有效、现在已被推翻」的行，而
    /// **那正是这个字段唯一有信息量的场合**。
    ///
    /// # 为什么是 N 次小查询而不是一次批量查询
    ///
    /// `cortex-store` 目前只有单条的 `fact_status`，而 cortexd 不写 SQL
    /// （见 `Cargo.toml` 里那段「这里没有 sqlx 而且不该再有」）。加批量接口
    /// 要动存储层，那是另一个 crate 的事。N 的上限是检索的 `limit`
    /// （缺省 20），而回放是用户显式发起的低频操作 —— 这里的取舍是
    /// 「多几次索引命中的点查」换「不在 cortexd 里开 SQL 的口子」。
    ///
    /// 顺序发而不是并发发：并发只会跟正在服务的对话抢同一个连接池，
    /// 而省下的那点墙钟时间对一个用户自己点出来的回放没有意义。
    async fn invalidation_flags(
        &self,
        items: &[cortex_memory::MemoryItem],
        replaying: bool,
    ) -> Vec<bool> {
        if !replaying {
            return vec![false; items.len()];
        }
        let mut out = Vec::with_capacity(items.len());
        for m in items {
            out.push(match self.store.fact_status(&m.id).await {
                Ok(s) => s.is_some_and(|s| s.op == cortex_store::FactOp::Invalidate),
                // 查不出来时报 false：把一条仍然有效的事实标成「已失效」
                // 会让用户以为自己的现行结论被推翻了；反过来只是少一个角标。
                // 两个方向的错误代价不对称，往少说的那边倒
                Err(e) => {
                    tracing::warn!(fact = %m.id, error = %e, "取事实状态失败，按未失效下发");
                    false
                }
            });
        }
        out
    }

    /// 检索并**渲染成给模型看的那段文本**。
    ///
    /// # 为什么这一层要单独存在
    ///
    /// 有两个消费者：agent 自己的 `memory_search` 工具（[`TurnHost`]），
    /// 以及对外的 MCP server（[`crate::mcp`]）。它们各写一份的话，
    /// 「第三方查到的」与「我们自己 agent 查到的」会漂开 —— 而那正是
    /// [`Self::retrieve`] 那条注释已经立过一次的规矩，这里只是把它
    /// 往上再抬一层：**渲染也只有一份**。
    ///
    /// 渲染必须走 `injection::render_turn_block`，不能另写「MCP 结果格式」。
    /// 这段文本同样要进模型上下文，那道「记忆是背景数据不是指令」的框定
    /// 一处都不能少 —— 记忆里可能混着被抽取进来的恶意指令（MemoryGraft），
    /// 从 MCP 通道进来的和从注入通道进来的一样危险。
    pub async fn memory_search_text(
        &self,
        query: &str,
        as_of: Option<&str>,
        limit: i64,
    ) -> Result<String> {
        let r = self.retrieve(query, as_of, limit).await?;
        if r.items.is_empty() {
            return Ok("没有检索到相关记忆。".into());
        }
        Ok(injection::render_turn_block(&r.items))
    }

    /// 核心画像块 —— 这个人身上**不怎么变**的那些事，渲染成给模型看的文本。
    ///
    /// 空查询走的是 `RetrievalPlan::Snapshot(now)`，也就是「此刻的全貌」，
    /// 按时间倒序。那正是画像该有的取法：它不回答某个具体问题。
    ///
    /// 渲染走 `render_profile_block` 而不是 `render_turn_block` —— 两者的
    /// 框定不同，画像那一份是给「每轮都重发的前缀」写的。
    /// **这是它的第一个调用者**：这个函数造出来之后一直没人用。
    pub async fn profile_text(&self, limit: i64) -> Result<String> {
        let r = self.retrieve("", None, limit).await?;
        if r.items.is_empty() {
            return Ok(String::new());
        }
        Ok(injection::render_profile_block(&r.items))
    }

    async fn memory_search(&self, q: MemorySearchQuery) -> Result<MemorySearchResponse> {
        let r = self.retrieve(&q.q, q.as_of.as_deref(), q.limit).await?;
        let invalidated = self.invalidation_flags(&r.items, q.as_of.is_some()).await;

        Ok(MemorySearchResponse {
            // 走 `fact_dto_of` 而不是在这里再拼一遍。此前这里是它的手抄版，
            // 于是「补一个字段」要改两处 —— 而漏改的表现是同一条记忆在
            // 搜索结果里和在对话抽屉里长得不一样，没有任何报错
            facts: r
                .items
                .iter()
                .zip(invalidated)
                .map(|(m, invalidated)| FactDto {
                    invalidated,
                    ..fact_dto_of(m)
                })
                .collect(),
            channels: r
                .attribution
                .into_iter()
                .map(|a| ChannelHit {
                    fact_id: a.fact_id,
                    channels: a.channels.into_iter().map(str::to_string).collect(),
                    score: a.score,
                })
                .collect(),
        })
    }

    async fn get_episode(&self, id: &str) -> Result<EpisodeDto> {
        let e = self
            .store
            .episode(id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CortexError::NotFound {
                kind: "episode",
                id: id.into(),
            })?;
        let replay = Replay {
            attachments: self
                .store
                .episode_attachments(id)
                .await
                .map_err(store_err)?
                .into_iter()
                .map(attachment_of)
                .collect(),
            memories: self
                .store
                .injected_memories(id)
                .await
                .map_err(store_err)?
                .into_iter()
                .map(memory_dto)
                .collect(),
            tool_calls: self
                .store
                .episode_tool_calls(id)
                .await
                .map_err(store_err)?
                .into_iter()
                .map(tool_call_dto)
                .collect(),
        };
        Ok(episode_dto(e, replay))
    }

    async fn list_sessions(&self, q: &ListSessionsQuery) -> Result<Vec<SessionDto>> {
        let digests = self
            .store
            .session_digests(
                SESSION_LIST_LIMIT,
                q.include_archived,
                q.project_id.as_deref(),
            )
            .await
            .map_err(store_err)?;
        Ok(digests.into_iter().map(session_dto).collect())
    }

    // ── 项目 ───────────────────────────────────────────────

    async fn list_projects(&self) -> Result<Vec<ProjectDto>> {
        let rows = self.store.projects().await.map_err(store_err)?;
        Ok(rows.into_iter().map(project_dto).collect())
    }

    /// 建一个项目。id 由**服务端**生成。
    ///
    /// 与会话相反（那边 id 由客户端生成，因为它要在离线时先建起来）：
    /// 项目只在联网时能建 —— 它的全部意义是「服务端知道这个分组存在」，
    /// 离线建一个本地项目再等着合并，会引出「两台设备各建了一个同名项目」
    /// 这种要人工消歧的状态，而分组这件事不值得。
    async fn create_project(&self, name: &str) -> Result<ProjectDto> {
        let name = validate_project_name(name)?;
        let id = Id::new().to_string();
        let event = cortex_store::NewProjectEvent::create(
            &id,
            &name,
            cortex_store::Actor::User,
            &self.device_id,
        );

        self.store
            .write_txn(async |t| t.insert_project_event(&event).await)
            .await
            .map_err(store_err)?;

        // 不拿手上的 name 拼一个 DTO 回去，而是读回末态：并发下另一台设备
        // 可能刚好改了名，此时回显本次请求的名字就是在告诉客户端一件
        // 已经不成立的事。多一次读换掉一整类「界面显示的与库里存的不一样」
        self.project_overview(&id).await
    }

    async fn patch_project(&self, id: &str, patch: ProjectPatch) -> Result<ProjectDto> {
        let Some(raw) = patch.name.as_deref() else {
            return Err(CortexError::Invalid(
                "请求体里没有任何要改的字段（name）".into(),
            ));
        };
        let name = validate_project_name(raw)?;

        // 改名之前先确认它还在。少了这一句，给一个已删除项目改名会**静默成功**
        // （事件照样落库），而列表里什么都不会出现 —— 用户看到的是「改完就没了」
        self.require_project(id).await?;

        let event = cortex_store::NewProjectEvent::rename(
            id,
            &name,
            cortex_store::Actor::User,
            &self.device_id,
        );
        self.store
            .write_txn(async |t| t.insert_project_event(&event).await)
            .await
            .map_err(store_err)?;

        self.project_overview(id).await
    }

    /// 删项目 —— **解散分组，不动内容**。
    ///
    /// 里面的会话变成未分组（`session_state` 把指向已删除项目的归属收敛成
    /// NULL），消息、附件、已抽取的事实一条不少。刻意不级联写一批
    /// `remove_from_project`：那是 N 条写入，而且会让「撤销误删」变成不可能 ——
    /// 现在只要用同一个 id 再 create 一次，所有会话的归属原样回来。
    ///
    /// **重复删是 404 而不是 200**：客户端在两台设备上各删一次是常见的，
    /// 但把「已经不在了」说成成功，会让「我删的是哪一个」这种错误永远查不出来。
    async fn delete_project(&self, id: &str) -> Result<()> {
        self.require_project(id).await?;

        let event =
            cortex_store::NewProjectEvent::delete(id, cortex_store::Actor::User, &self.device_id);
        self.store
            .write_txn(async |t| t.insert_project_event(&event).await)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    /// 项目此刻的末态。不存在（或已删）时 404。
    async fn project_overview(&self, id: &str) -> Result<ProjectDto> {
        Ok(project_dto(self.require_project(id).await?))
    }

    /// 「这个项目现在还在吗」—— 读一次末态，不在就 404。
    ///
    /// 单独抽出来是因为改名与删除都要它，而两处都必须在**进写事务之前**问：
    /// 写事务持着 advisory lock，纪律要求它短小纯写
    /// （`cortex-store::txn` 的第三条）。
    ///
    /// 把会话移进项目那一处**不**走这里：那条路上项目不存在是 400 不是 404，
    /// 理由见 [`Self::patch_session`] 里那段注释。
    async fn require_project(&self, id: &str) -> Result<cortex_store::Project> {
        self.store
            .project(id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CortexError::NotFound {
                kind: "project",
                id: id.into(),
            })
    }

    /// 改名 / 归档 / 绑定工作区。
    ///
    /// # 为什么不要求会话已经有消息
    ///
    /// 用户新建会话 → 选工作区 → 发第一句，这是最自然的顺序。若在这里
    /// 因为「查不到这个会话」返回 404，那个顺序就走不通，客户端只好倒过来
    /// 强迫用户先说一句话。事件表本来就不依赖 episodes 存在，允许它即可 ——
    /// 末态与写入顺序无关，消息一到会话就带着标题和工作区出现在列表里。
    ///
    /// # 为什么全部改动进同一个写事务
    ///
    /// 「改名 + 归档」若分两个事务，别的设备会先拉到「改完名但还没归档」
    /// 那个中间态，列表上闪一下。同事务下它们在 `sync_log` 里连号到达。
    async fn patch_session(&self, session_id: &str, patch: SessionPatch) -> Result<SessionDto> {
        let mut events: Vec<cortex_store::NewSessionEvent> = Vec::new();

        if let Some(raw) = patch.title.as_deref() {
            let title = raw.trim();
            if title.is_empty() {
                return Err(CortexError::Invalid(
                    "标题不能为空白；本版不支持「恢复自动标题」，请给一个非空标题".into(),
                ));
            }
            if title.chars().count() > cortex_store::SESSION_TITLE_MAX_CHARS {
                return Err(CortexError::Invalid(format!(
                    "标题过长（上限 {} 字符）",
                    cortex_store::SESSION_TITLE_MAX_CHARS
                )));
            }
            events.push(cortex_store::NewSessionEvent::rename(
                session_id,
                title,
                cortex_store::Actor::User,
                &self.device_id,
            ));
        }

        if let Some(archived) = patch.archived {
            events.push(if archived {
                cortex_store::NewSessionEvent::archive(
                    session_id,
                    cortex_store::Actor::User,
                    &self.device_id,
                )
            } else {
                cortex_store::NewSessionEvent::unarchive(
                    session_id,
                    cortex_store::Actor::User,
                    &self.device_id,
                )
            });
        }

        // 校验必须在进事务**之前**：写事务持着 advisory lock，
        // 而 canonicalize 要打文件系统的往返（网络盘上能到几十毫秒）。
        // 取号事务短小纯写，这是 cortex-store::txn 的纪律三。
        // ── 绑定工作区：**服务端明确拒绝** ──
        //
        // 这条路曾经是 Web 端绑工作区的回落（本地 agent 不在时），而它绑的是
        // **服务器上**的一个目录 —— 于是一个远端用户的 `read_file` 动的是
        // 生产机的文件系统。爆炸半径是整台机器加上所有租户的数据。
        //
        // 拒绝而不是静默忽略：静默的话客户端会显示「已绑定 D:\myproject」，
        // 然后每个文件操作都失败得莫名其妙 —— 而用户明明看到绑定成功了。
        // 报错里直接给出两条能走通的路。
        //
        // 解绑（`Some(None)`）**照旧放行**：老会话上可能还留着一条服务端绑定，
        // 用户要能清掉它。拒绝解绑等于让那条记录永远焊在那儿。
        if workspace_patch(patch.workspace.as_ref().map(Option::as_deref))? {
            events.push(cortex_store::NewSessionEvent::unbind_workspace(
                session_id,
                cortex_store::Actor::User,
                &self.device_id,
            ));
        }

        // ── 移进 / 移出项目 ──
        //
        // 目标项目的存在性在**进事务之前**查（同上：取号事务短小纯写）。
        // 不查的话，移进一个已删除的项目会静默成功 —— 事件照样落库，而末态
        // 视图把悬挂的归属当作未分组，于是用户看到的是「拖进去又弹回来了」，
        // 且没有任何一处报错。
        match patch.project_id.as_ref().map(Option::as_deref) {
            Some(Some(project_id)) => {
                // 目标项目不存在是 **400 而不是 404**：这条请求的资源是那个会话，
                // 而它好好的。回 404 会让客户端分不清「会话没了」与「项目没了」，
                // 而两者要做的事完全不同（一个是刷新列表，一个是重建分组）
                if self
                    .store
                    .project(project_id)
                    .await
                    .map_err(store_err)?
                    .is_none()
                {
                    return Err(CortexError::Invalid(format!(
                        "目标项目不存在或已被删除：{project_id}"
                    )));
                }
                events.push(cortex_store::NewSessionEvent::move_to_project(
                    session_id,
                    project_id,
                    cortex_store::Actor::User,
                    &self.device_id,
                ));
            }
            Some(None) => events.push(cortex_store::NewSessionEvent::remove_from_project(
                session_id,
                cortex_store::Actor::User,
                &self.device_id,
            )),
            None => {}
        }

        // ── 执行归属 ──
        //
        // 不校验「这台机器上真有那个绑定」：服务端**看不到**客户机的
        // `workspaces.json`，那是这套设计的前提而不是疏漏（见
        // `cortex-local::workspaces` 的模块头）。声明它的是唯一知情的一方。
        if let Some(runtime) = patch.runtime {
            events.push(cortex_store::NewSessionEvent::set_runtime(
                session_id,
                match runtime {
                    SessionRuntimeDto::Local => cortex_store::SessionRuntime::Local,
                    SessionRuntimeDto::Cloud => cortex_store::SessionRuntime::Cloud,
                },
                cortex_store::Actor::User,
                &self.device_id,
            ));
        }

        if events.is_empty() {
            return Err(CortexError::Invalid(
                "请求体里没有任何要改的字段（title / archived / workspace / project_id）".into(),
            ));
        }

        self.store
            .write_txn(async |t| {
                let mut last = 0;
                for e in &events {
                    last = t.insert_session_event(e).await?;
                }
                Ok(last)
            })
            .await
            .map_err(store_err)?;

        self.session_overview(session_id).await
    }

    /// 会话概览。没有消息时仍然返回一条 —— PATCH 之后客户端要拿回末态，
    /// 而此刻会话很可能一条消息都还没有。
    async fn session_overview(&self, session_id: &str) -> Result<SessionDto> {
        // 有消息时聚合查询已经顺带把标题 / 归档 / 工作区一起带回来了
        if let Some(d) = self
            .store
            .session_digest(session_id)
            .await
            .map_err(store_err)?
        {
            return Ok(session_dto(d));
        }

        // 没有消息 —— 用事件末态拼一条空壳。这条路正是「先选工作区、
        // 再发第一句话」那个顺序要求的
        let state = self
            .store
            .session_state(session_id)
            .await
            .map_err(store_err)?;
        let now = Utc::now().to_rfc3339();
        let title = state.as_ref().and_then(|s| s.title.clone());
        Ok(SessionDto {
            id: session_id.to_string(),
            title_is_custom: title.is_some(),
            title: title.unwrap_or_else(|| session_title(None)),
            created_at: now.clone(),
            updated_at: now,
            message_count: 0,
            preview: None,
            archived: state.as_ref().is_some_and(|s| s.archived),
            workspace: state.as_ref().and_then(|s| s.workspace.clone()),
            runtime: state
                .as_ref()
                .map_or(SessionRuntimeDto::Cloud, |s| runtime_dto(s.runtime)),
            project_id: state.and_then(|s| s.project_id),
        })
    }

    /// 一个会话的一页消息，附件与回放抽屉一并挂上。
    ///
    /// # 概览为什么不再从取回的消息现算
    ///
    /// 以前是「消息全在手上了，顺手算一下」，分页之后这条路直接错了：
    /// 手上只有**一页**，算出来的 `message_count` 是页大小、`created_at`
    /// 是这一页最早那条、派生标题取的是这一页里的首条用户消息。
    /// 这些数字在第一页上恰好都对，往回翻一页就全错 —— 而它们不报错。
    ///
    /// 现在概览一律来自 `session_digest` 那句聚合 SQL：它统计的是**全部**消息，
    /// 与翻到第几页无关。多一次往返换掉一整类「翻页之后数字开始漂」的 bug。
    /// 客户端拿 `message_count` 与本页条数比对来判断是否被截断的那套补丁，
    /// 现在有 `has_more` 可用，但旧口径仍然成立（`message_count` 还是总数）。
    async fn session_detail(
        &self,
        session_id: &str,
        q: &SessionDetailQuery,
    ) -> Result<SessionDetail> {
        let digest = self
            .store
            .session_digest(session_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CortexError::NotFound {
                kind: "session",
                id: session_id.into(),
            })?;

        let limit = q
            .limit
            .unwrap_or(DEFAULT_EPISODE_PAGE)
            .clamp(1, MAX_EPISODE_PAGE);
        let before = q.before.as_deref().map(crate::cursor::decode).transpose()?;

        // 多要一条来判断「还有没有更早的」。用 len == limit 推断会在恰好整除时
        // 多骗客户端要一次空页，而客户端多半把空页当成「到头了」——
        // 两种判断混在一起就是「有时候翻不到最早那几条」
        let mut page = self
            .store
            .episodes_by_session_page(session_id, limit + 1, before.as_ref())
            .await
            .map_err(store_err)?;
        let has_more = page.len() as i64 > limit;
        page.truncate(limit as usize);

        // 这里拿到的是降序（新 → 老）。反转成正序再下发，理由见
        // `SessionDetail::episodes` 的文档：下发降序不会报错，只会让老客户端
        // 把整段对话倒着画出来
        let next_cursor = page
            .last()
            .filter(|_| has_more)
            .map(|e| crate::cursor::encode(e.occurred_at, &e.id));
        page.reverse();

        let ids: Vec<String> = page.iter().map(|e| e.id.clone()).collect();
        let mut by_episode = self.replay_bundle(&ids).await?;

        Ok(SessionDetail {
            session: session_dto(digest),
            episodes: page
                .into_iter()
                .map(|e| {
                    let replay = by_episode.remove(&e.id).unwrap_or_default();
                    episode_dto(e, replay)
                })
                .collect(),
            has_more,
            next_cursor,
        })
    }

    /// 一页消息的附件 + 回放抽屉，三次批量查询而不是逐条 N+1。
    async fn replay_bundle(
        &self,
        episode_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Replay>> {
        let mut out: std::collections::HashMap<String, Replay> =
            std::collections::HashMap::with_capacity(episode_ids.len());

        for a in self
            .store
            .episode_attachments_bulk(episode_ids)
            .await
            .map_err(store_err)?
        {
            out.entry(a.episode_id.clone())
                .or_default()
                .attachments
                .push(attachment_of(a));
        }
        for m in self
            .store
            .injected_memories_bulk(episode_ids)
            .await
            .map_err(store_err)?
        {
            out.entry(m.episode_id.clone())
                .or_default()
                .memories
                .push(memory_dto(m));
        }
        for t in self
            .store
            .episode_tool_calls_bulk(episode_ids)
            .await
            .map_err(store_err)?
        {
            out.entry(t.episode_id.clone())
                .or_default()
                .tool_calls
                .push(tool_call_dto(t));
        }

        Ok(out)
    }

    // ─────────────────────────── 媒体 ───────────────────────────

    /// `blobs` 行的元信息。取回字节时要靠它拿 MIME 与总长度
    /// （HTTP Range 的 `Content-Range` 分母就是这个长度）。
    async fn blob_meta(&self, hash: &str) -> Result<cortex_store::Blob> {
        self.store
            .blob(hash)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CortexError::NotFound {
                kind: "blob",
                id: hash.into(),
            })
    }

    /// 登记一个已经落到对象存储里的 blob —— 三步顺序的第二步。
    ///
    /// 走 `write_txn` 而不是裸 INSERT：`blobs` 行必须与 `sync_log` 同事务，
    /// 否则别的设备永远不知道这个对象存在，同步下来的 `episode_blobs`
    /// 就会指向一条它没有的 blob 行。
    ///
    /// 返回 `None` 表示这份内容**早就登记过**，本次没有新增行。这不是错误：
    /// 内容寻址下两条行会逐字节相同，重复登记既没有意义也没有害处。
    async fn register_blob(&self, new: cortex_store::NewBlob) -> Result<Option<i64>> {
        if self
            .store
            .blob(&new.hash)
            .await
            .map_err(store_err)?
            .is_some()
        {
            return Ok(None);
        }

        match self
            .store
            .write_txn(async |t| t.insert_blob(&new).await)
            .await
        {
            Ok(seq) => Ok(Some(seq)),
            Err(e) => {
                // 上面那次存在性检查是 TOCTOU，两个设备同时传同一张图就会撞主键。
                // 回查一次：已经在了就当成功。不去匹配 SQLSTATE ——
                // 那要求 cortexd 认识 sqlx 的错误结构，而这一层不该再有 sqlx
                if self
                    .store
                    .blob(&new.hash)
                    .await
                    .map_err(store_err)?
                    .is_some()
                {
                    tracing::debug!(hash = %new.hash, "并发登记同一份内容，按幂等处理");
                    Ok(None)
                } else {
                    Err(store_err(e))
                }
            }
        }
    }

    /// 排一次媒体转录 —— **不阻塞上传响应**。
    ///
    /// 转录要调一次 vision 模型（实测十几秒），同步做会让「拖一张图进来」
    /// 卡住十几秒，而转录结果对**这次上传**毫无用处：它是给将来的检索准备的。
    /// 与事实抽取同一个理由，也同一个形态。
    ///
    /// 只在 blob **首次**登记时排队：内容寻址下重复上传是同一份字节，
    /// 转录结果也会一模一样，重转纯属白烧模型钱。
    pub fn spawn_transcription(self: &Arc<Self>, hash: String, bytes: bytes::Bytes, mime: String) {
        if self.transcribe.is_none() {
            return; // 没配 vision 模型，图片仍然归档，只是检索不到
        }
        let live = Arc::clone(self);

        tokio::spawn(async move {
            match live.transcribe_and_store(&hash, &bytes, &mime).await {
                Ok(outcome) => outcome.log(&hash),
                // 转录失败不该影响已经成功的上传，记日志即可
                Err(e) => tracing::warn!(hash, error = %e, "媒体转录失败"),
            }
        });
    }

    /// 媒体转录是否启用。后台回扫器据此决定要不要起来 ——
    /// 没配 `CORTEX_VISION_PROVIDER` 时整条链路关闭，与上传路径的行为一致。
    #[must_use]
    pub fn transcription_enabled(&self) -> bool {
        self.transcribe.is_some()
    }

    /// 跑一遍转录并落库。**同步等待结果** —— 上传路径把它丢进
    /// [`Self::spawn_transcription`] 里 fire-and-forget，后台回扫器则要拿到
    /// 结局才能决定「这个 blob 要不要重试」。
    ///
    /// 抹除防护走 [`StoreRedactionGuard`]，两条路都绕不开它：管线把
    /// [`cortex_memory::transcribe::RedactionGuard`] 做进了 `run` 的签名。
    pub async fn transcribe_and_store(
        &self,
        hash: &str,
        bytes: &[u8],
        mime: &str,
    ) -> Result<TranscribeOutcome> {
        let Some(pipeline) = self.transcribe.as_ref() else {
            return Ok(TranscribeOutcome::Disabled);
        };
        let guard = StoreRedactionGuard {
            store: self.store.clone(),
        };
        match pipeline.run(hash, bytes, mime, &guard).await? {
            cortex_memory::transcribe::Outcome::Ready(rows) => {
                let n = rows.len();
                // 一个事务写完全部段落：ASR 的分段之间没有独立意义，
                // 半截的时间轴比没有更糟
                self.store
                    .write_txn(async |t| {
                        for r in &rows {
                            t.insert_blob_transcript(r).await?;
                        }
                        Ok(())
                    })
                    .await
                    .map_err(store_err)?;
                Ok(TranscribeOutcome::Stored { segments: n })
            }
            cortex_memory::transcribe::Outcome::Redacted => Ok(TranscribeOutcome::Redacted),
            cortex_memory::transcribe::Outcome::Unsupported { mime } => {
                Ok(TranscribeOutcome::Unsupported { mime })
            }
        }
    }

    // ─────────────────────────── 同步 ───────────────────────────

    async fn sync_since(&self, q: SyncQuery) -> Result<SyncResponse> {
        let limit = q.limit.clamp(1, 1000);
        let records = self
            .store
            .fetch_since(q.since, limit)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))?;

        let cursor = records.last().map_or(q.since, |r| r.seq);
        let has_more = records.len() as i64 == limit;

        Ok(SyncResponse {
            cursor,
            has_more,
            records: records
                .into_iter()
                .map(|r| SyncRecord {
                    seq: r.seq,
                    table: r.table_name,
                    id: r.record_id,
                    // payload 为 None 说明业务行已不存在（被 purge）。
                    // 仍要下发这条日志：客户端据此推进游标并知道发生过删除
                    payload: r
                        .payload
                        .as_ref()
                        .map_or(serde_json::Value::Null, crate::sync_payload::to_json),
                })
                .collect(),
        })
    }
}

/// 把本轮的检索归因写进 `episode_memories`。
///
/// 失败只记日志：对话已经在跑了，归因是可观测性，不该把一次成功的对话
/// 拖成 500。但**必须记**，否则「抽屉为什么是空的」在生产上无从查起。
async fn record_injected_memories(
    store: &Store,
    episode_id: Id,
    retrieved: &Retrieved,
    device_id: &str,
) {
    // attribution 与 items 等长且同序（见 cortex_memory::Retrieved 的文档），
    // 但不假设它 —— 按 fact_id 建索引，少一条就少一条归因，不至于错位到
    // 「A 的分数记在 B 头上」
    let scores: std::collections::HashMap<&str, &cortex_memory::Attribution> = retrieved
        .attribution
        .iter()
        .map(|a| (a.fact_id.as_str(), a))
        .collect();

    let mut rows: Vec<cortex_store::NewEpisodeMemory> = Vec::with_capacity(retrieved.items.len());
    for (i, item) in retrieved.items.iter().enumerate() {
        // fact id 形制不对就跳过这一条而不是整批放弃：episode_memories.fact_id
        // 是 ulid 域，一个坏 id 会让整个事务回滚，连带丢掉其余全部归因
        let Ok(fact_id) = item.id.parse::<Id>() else {
            tracing::warn!(id = %item.id, "检索结果的 id 不是合法 ULID，跳过这一条归因");
            continue;
        };
        let attribution = scores.get(item.id.as_str());
        rows.push(cortex_store::NewEpisodeMemory {
            id: Id::new(),
            episode_id,
            fact_id,
            ordinal: i as i32,
            channels: attribution.map_or_else(Vec::new, |a| {
                a.channels.iter().map(|c| (*c).to_string()).collect()
            }),
            score: attribution.map(|a| a.score),
            device_id: device_id.to_owned(),
        });
    }

    if rows.is_empty() {
        return;
    }

    let n = rows.len();
    let res = store
        .write_txn(async |t| {
            let mut last = 0;
            for r in &rows {
                last = t.insert_episode_memory(r).await?;
            }
            Ok(last)
        })
        .await;
    match res {
        Ok(_) => tracing::debug!(injected = n, "本轮记忆归因已落库"),
        Err(e) => tracing::warn!(error = %e, injected = n, "记忆归因落库失败，本轮抽屉将是空的"),
    }
}

/// 按**字符**截断到上限。
///
/// 这些上限对应 schema 里的 CHECK。超限不是「这一行写不进去」而是
/// **整个写事务回滚** —— 一个名字特别长的文件能把整轮对话的归因全带走。
/// 按字符而非字节：Postgres 的 `length()` 数的是字符，按字节切还会把
/// 汉字劈成半个。
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

// ───────────────────────── 领域 → DTO ──────────────────────────

fn store_err(e: cortex_store::StoreError) -> CortexError {
    CortexError::Store(e.to_string())
}

/// 检索结果 → 线上事实。
///
/// [`run_turn`]（Web 那条路，走 SSE）与 [`Live::write_episode`]
///（本地 agent 那条路，走 HTTP 响应）共用这一个映射。两处各写一份的话，
/// 漂移的表现是**同一条记忆在两个客户端上长得不一样**，而没有任何报错。
fn fact_dto_of(m: &cortex_memory::MemoryItem) -> FactDto {
    FactDto {
        id: m.id.clone(),
        statement: m.statement.clone(),
        predicate: m.predicate.clone(),
        domain: m.domain.clone(),
        // `confidence` 曾经在这里写死成 1.0，于是抽屉上**每一条**记忆
        // 都显示「100%」—— 一个看起来像数据、实际是常量的数字。
        // 拿不到时给 0.0 而不是 1.0：客户端据此不显示这个角标
        confidence: m.confidence.unwrap_or(0.0),
        valid_at: m.valid_at.clone(),
        created_at: m.known_since.clone(),
        // 本轮注入走的是不带 as_of 的召回，只查 active_facts ——
        // 按定义拿不到已失效的事实。见 `Live::invalidation_flags`
        invalidated: false,
        source_episode_id: m.source_episode_id.clone(),
        source_channel: m.source_channel.clone(),
        trust_tier: m.trust_tier,
    }
}

/// 一条消息的「附加物」：附件 + 回放抽屉。
///
/// 三个 `Vec` 打包成一个结构而不是给 [`episode_dto`] 三个参数：
/// 三个同型参数排在一起，传错顺序编译器一声不吭。
#[derive(Debug, Default)]
struct Replay {
    attachments: Vec<AttachmentDto>,
    memories: Vec<InjectedMemoryDto>,
    tool_calls: Vec<ToolCallDto>,
}

fn episode_dto(e: cortex_store::Episode, replay: Replay) -> EpisodeDto {
    EpisodeDto {
        id: e.id,
        session_id: e.session_id,
        role: e.role.as_str().to_string(),
        text: e.text,
        occurred_at: e.occurred_at.to_rfc3339(),
        attachments: replay.attachments,
        memories: replay.memories,
        tool_calls: replay.tool_calls,
    }
}

fn attachment_of(a: cortex_store::EpisodeAttachment) -> AttachmentDto {
    AttachmentDto {
        hash: a.blob_hash,
        kind: a.kind,
        filename: a.filename,
        mime: a.mime,
        size_bytes: a.size_bytes,
    }
}

fn memory_dto(m: cortex_store::InjectedMemory) -> InjectedMemoryDto {
    InjectedMemoryDto {
        fact_id: m.fact_id,
        statement: m.statement,
        domain: m.domain,
        channels: m.channels.unwrap_or_default(),
        score: m.score,
        invalidated: m.invalidated,
        source_episode_id: m.source_episode_id,
    }
}

fn tool_call_dto(t: cortex_store::EpisodeToolCall) -> ToolCallDto {
    ToolCallDto {
        name: t.name,
        path: t.path,
        summary: t.summary,
        ok: t.ok,
        diff: t.diff,
    }
}

fn session_dto(d: cortex_store::SessionDigest) -> SessionDto {
    SessionDto {
        id: d.session_id,
        title_is_custom: d.title.is_some(),
        // 用户起过名就用他起的，否则从首条用户消息派生。
        // 顺序不能反 —— 反了就是「用户改了名，刷新一下又变回去了」
        title: d
            .title
            .unwrap_or_else(|| session_title(d.first_user_text.as_deref())),
        created_at: d.started_at.to_rfc3339(),
        updated_at: d.updated_at.to_rfc3339(),
        message_count: d.message_count,
        preview: d.last_text,
        archived: d.archived,
        workspace: d.workspace,
        project_id: d.project_id,
        runtime: runtime_dto(d.runtime),
    }
}

/// 存储层的枚举 → 线协议的枚举。
///
/// 两个枚举而不是一个：`cortex-store` 不依赖 `cortex-proto`（依赖方向是
/// `core ← store ← … ← cortexd`），而线协议的取值一旦定下就不能跟着存储层
/// 的重构走。多一个 match 的代价，换的是「改数据库表示不会静默改线协议」。
fn runtime_dto(r: cortex_store::SessionRuntime) -> SessionRuntimeDto {
    match r {
        cortex_store::SessionRuntime::Local => SessionRuntimeDto::Local,
        cortex_store::SessionRuntime::Cloud => SessionRuntimeDto::Cloud,
    }
}

fn project_dto(p: cortex_store::Project) -> ProjectDto {
    ProjectDto {
        id: p.project_id,
        name: p.name,
        created_at: p.created_at.to_rfc3339(),
        session_count: p.session_count,
    }
}

/// 标题由首条用户消息派生。
///
/// 截断放在这一层而不是 SQL 里：它是**展示决策**，存储层不该替 UI 决定
/// 一个标题该有多长。按字符而非字节截断 —— 中文一刀切在字节上会切出乱码。
fn session_title(first_user_text: Option<&str>) -> String {
    let trimmed = first_user_text.map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return "新会话".into();
    }
    trimmed.chars().take(TITLE_CHARS).collect()
}

/// 去掉重复的附件哈希，保持客户端给的顺序。
///
/// `episode_blobs` 的主键是 `(episode_id, blob_hash)`，同一条消息里挂两次
/// 同一张图会直接撞主键把整个写事务弄回滚 —— 而「同一张图被拖进来两次」
/// 在界面上是再正常不过的操作。
fn dedup_attachments(input: &[AttachmentRef]) -> Vec<AttachmentRef> {
    let mut seen = std::collections::HashSet::new();
    input
        .iter()
        .filter(|a| seen.insert(a.hash.clone()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四格摆齐。**少一格的后果是记忆面板打开就是空的，且不报任何错。**
    #[test]
    fn 检索的四格里没有一格是空手而归() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("常量时间可解析")
            .with_timezone(&Utc);

        assert_eq!(
            retrieval_plan("", Some(at)),
            RetrievalPlan::Snapshot(at),
            "回放 + 没有词 = 当时的全貌"
        );
        assert_eq!(
            retrieval_plan("对象存储", Some(at)),
            RetrievalPlan::SnapshotRanked(at),
            "回放 + 有词 = 在那份快照上排序，而不是退化成按时间倒序"
        );
        assert_eq!(
            retrieval_plan("对象存储", None),
            RetrievalPlan::Ranked,
            "此刻 + 有词 = 四路召回"
        );

        // **这一格曾经是 Ranked。** 而四路召回全都要查询词，于是它恒回 0 条。
        // 界面上的表现是「还没有记忆」——对着一个有 159 条事实的库
        match retrieval_plan("   ", None) {
            RetrievalPlan::Snapshot(_) => {}
            other => panic!(
                "此刻 + 没有词必须给「最近的全貌」，实际是 {other:?}。\
                 走四路召回的话空查询恒回 0 条：记忆面板打开就是空的，\
                 而它会说「还没有记忆」——用户据此认为这个功能没用"
            ),
        }
    }

    #[test]
    fn a_project_name_must_survive_trimming() {
        assert_eq!(
            validate_project_name("  记忆引擎  ").expect("两侧空白应当被去掉而不是拒绝"),
            "记忆引擎"
        );

        for bad in ["", "   ", "\t\n"] {
            assert!(
                validate_project_name(bad).is_err(),
                "{bad:?} 应当被拒 —— 一个名字全是空白的项目在侧边栏上是一行\
                 什么都没有的可点区域，看起来像界面坏了"
            );
        }
    }

    #[test]
    fn the_project_name_limit_counts_characters_not_bytes() {
        let max = cortex_store::PROJECT_NAME_MAX_CHARS;

        // 恰好顶格的中文名：按字节算的话它是 3 倍长度，会被误拒
        let full = "记".repeat(max);
        assert_eq!(
            validate_project_name(&full)
                .expect("恰好顶格应当通过 —— 上限按字符数，与数据库 length() 一致"),
            full
        );

        assert!(
            validate_project_name(&"记".repeat(max + 1)).is_err(),
            "超一个字符就该在这里被拒，而不是让数据库的 CHECK 变成一句 500"
        );
    }

    #[test]
    fn session_title_falls_back_and_truncates_by_char() {
        assert_eq!(session_title(None), "新会话");
        assert_eq!(session_title(Some("   ")), "新会话", "全空白也该回落");

        // 按字符截断。按字节切会把一个汉字劈成半个，JSON 序列化直接产出乱码
        let long = "一二三四五六七八九十".repeat(4); // 40 个汉字
        let title = session_title(Some(&long));
        assert_eq!(
            title.chars().count(),
            TITLE_CHARS,
            "标题应截到 {TITLE_CHARS} 个字符：{title}"
        );
        assert!(title.starts_with("一二三"), "截断后开头应保持原样：{title}");
    }

    #[test]
    fn duplicate_attachments_are_collapsed_in_order() {
        // episode_blobs 的主键是 (episode_id, blob_hash)，重复挂同一张图会
        // 撞主键把整个写事务回滚 —— 而「同一张图被拖进来两次」是常规操作
        let a = AttachmentRef {
            hash: "a".repeat(64),
            kind: Some("image".into()),
            filename: Some("设计稿.png".into()),
        };
        let b = AttachmentRef {
            hash: "b".repeat(64),
            kind: None,
            filename: None,
        };
        let got = dedup_attachments(&[a.clone(), b.clone(), a.clone()]);
        assert_eq!(
            got.iter().map(|x| x.hash.clone()).collect::<Vec<_>>(),
            vec![a.hash.clone(), b.hash.clone()],
            "重复的哈希应被去掉，且保持客户端给的顺序"
        );
        assert_eq!(
            got[0].kind.as_deref(),
            Some("image"),
            "保留的应当是第一次出现的那一条，连同它的 kind"
        );
    }

    /// 服务端**不能**绑工作区，但**能**解绑。
    ///
    /// 绑定那一半写反了的后果是「服务端又能绑了」，而它没有任何症状 ——
    /// 直到某天一个远端用户的 `read_file` 读到了生产机上的 `.env`。
    /// 解绑那一半写反了的后果是老会话上的绑定永远清不掉。
    #[test]
    fn the_server_refuses_to_bind_but_still_lets_you_unbind() {
        let msg = workspace_patch(Some(Some("D:/anything")))
            .expect_err("服务端绑定工作区必须被拒绝")
            .to_string();
        assert!(
            msg.contains("你自己的机器") && msg.contains("cortex-local"),
            "拒绝理由要给出走得通的路（桌面端 / 本机跑 cortex-local），而不只是说不行。实际：{msg}"
        );
        // 沙箱落地之后**多了一条走得通的路**，而这条恰恰是 Web 用户唯一能走的
        // —— 前两条都要求他先装个桌面端。漏掉它，报错就等于对 Web 用户说
        // 「你想要的这件事在这里做不到」，而其实开一个开关就有
        assert!(
            msg.contains("云沙箱"),
            "Web 用户走不了前两条路（都要装桌面端）。不提沙箱，这条报错对他就是死路。实际：{msg}"
        );

        assert!(
            workspace_patch(Some(None)).expect("解绑必须放行"),
            "解绑要照旧能用 —— 老会话上可能还留着一条服务端绑定，拒绝解绑等于让它永远焊在那儿"
        );
        assert!(
            !workspace_patch(None).expect("没提这个字段不该报错"),
            "只改标题的请求不该被工作区这一条拦下来"
        );
    }
}

/// 一次检索该走哪条路。**四路召回全都要查询词**，所以「有没有词」与
/// 「回不回放」是两个独立维度，共四格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetrievalPlan {
    /// 那一刻的全貌，按时间倒序。没有查询词时走它。
    Snapshot(chrono::DateTime<Utc>),
    /// 在那一刻的快照**之上**做相关性排序。
    SnapshotRanked(chrono::DateTime<Utc>),
    /// 此刻的四路召回 + RRF。
    Ranked,
}

/// [`Live::retrieve`] 的决策部分，抽成纯函数只为**能把那四格摆出来测**。
///
/// # 这里少过一格
///
/// 「没有词 + 不回放」原来落在 `Ranked` 上。而四路召回全都要词：BM25 没有词、
/// 向量嵌不出方向、图遍历没有种子 —— 于是它恒回 0 条，连 `channels` 都是空的
/// （一路都没跑）。症状是记忆面板**打开就是空的**，配一句「还没有记忆」，
/// 而真机那个库里有 159 条。用户据此得出的结论是「这功能没什么用」。
///
/// 正确答案代码里本来就有（`as_of` + 没有词 → 全貌），缺的只是
/// 「**此刻**也是一个合法的 as_of」。四格摆成一张表，就没有地方藏这种缺口。
fn retrieval_plan(query: &str, as_of: Option<chrono::DateTime<Utc>>) -> RetrievalPlan {
    match (query.trim().is_empty(), as_of) {
        (true, Some(at)) => RetrievalPlan::Snapshot(at),
        (true, None) => RetrievalPlan::Snapshot(Utc::now()),
        (false, Some(at)) => RetrievalPlan::SnapshotRanked(at),
        (false, None) => RetrievalPlan::Ranked,
    }
}

#[cfg(test)]
mod extract_gate_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 取值非法就报错，不悄悄用默认值。
    #[test]
    fn a_bad_concurrency_setting_is_an_error_not_a_fallback() {
        assert_eq!(
            parse_extract_concurrency(None).unwrap(),
            DEFAULT_EXTRACT_CONCURRENCY
        );
        assert_eq!(parse_extract_concurrency(Some("8")).unwrap(), 8);
        assert_eq!(parse_extract_concurrency(Some(" 8 ")).unwrap(), 8);

        for bad in ["0", "abc", "-1", "3.5"] {
            let err = parse_extract_concurrency(Some(bad))
                .expect_err("非法取值必须报错，回落到默认值等于上限失效而运维不知情");
            assert!(
                err.to_string().contains(EXTRACT_CONCURRENCY_ENV),
                "报错要点名是哪个变量，实际：{err}"
            );
        }
    }

    /// **闸门真的把并发压住了。**
    ///
    /// 这条测的不是信号量本身（那是 tokio 的事），而是 [`under_gate`]
    /// 有没有把许可**持有到 future 结束**。写成 `let _ = acquire().await`
    /// 的话许可当场就被丢掉，闸门变成一句昂贵的空操作 —— 而那种改动
    /// 不会让任何别的测试变红，因为功能上一切正常，只是没有上限了。
    ///
    /// 失败的样子很具体：批量导入时几千路抽取一起打出去，把 2 核 3.5G 的
    /// 生产节点连同上面别的服务一起拖死。
    #[tokio::test]
    async fn the_gate_actually_caps_concurrency() {
        const PERMITS: usize = 3;
        const TASKS: usize = 40;

        let gate = Arc::new(tokio::sync::Semaphore::new(PERMITS));
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(TASKS);
        for _ in 0..TASKS {
            let (gate, now, peak) = (Arc::clone(&gate), Arc::clone(&now), Arc::clone(&peak));
            handles.push(tokio::spawn(async move {
                under_gate(&gate, async {
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    // 让出执行权，给别的任务挤进来的机会 —— 不 await 的话
                    // 每个任务都会一口气跑完，峰值恒为 1，测试就没有意义了
                    tokio::task::yield_now().await;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    now.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.expect("任务不该 panic");
        }

        let peak = peak.load(Ordering::SeqCst);
        assert!(
            peak <= PERMITS,
            "同时有 {peak} 路在跑，而上限是 {PERMITS} —— 许可没被持有到结束"
        );
        assert!(
            peak > 1,
            "峰值只有 {peak}，说明这些任务根本没重叠过，这条测试测了个寂寞"
        );
    }
}

//! 进程内 ONNX 推理的 embedding 后端 —— **可选 feature `local-embed`**。
//!
//! 这一整个文件在默认构建里**不编译**。它从 `embed.rs` 里拆出来正是为了
//! 让那个 cfg 只写一次：原来 fastembed 是硬依赖，后果不只是「重」——
//!
//!   1. Intel Mac 上 `ort-sys` 没有预编译产物，**构建期**就失败，
//!      连 `CORTEX_EMBED_BACKEND=hash` 都救不了
//!   2. 每台机器都要能拿到那 590 MB 权重；拿不到时 cortexd 会
//!      **照常启动、`/health` 返回 ok**，只在日志里留一行「回落到 mock」
//!   3. 冷启动 12 秒、常驻 1.0 GiB
//!
//! 默认后端改成了 [`crate::embed_api`]（HTTP，OpenAI 兼容）。
//! 需要「一个进程搞定、不依赖任何外部服务」时才打开这个 feature：
//!
//! ```bash
//! cargo build -p cortexd --features cortex-memory/local-embed
//! ```
//!
//! # 环境变量
//!
//! | 变量 | 取值 | 默认 |
//! |---|---|---|
//! | `CORTEX_EMBED_MODEL` | 见 [`FastModel::parse`] | `bge-m3-int8` |
//! | `CORTEX_MODEL_CACHE` | 模型缓存目录 | 用户 cache 目录下的 `cortex/models` |
//! | `CORTEX_EMBED_BATCH` | 后台回填批大小 | [`DEFAULT_BACKFILL_BATCH`] |

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use cortex_core::{CortexError, Result};
use fastembed::{
    Bgem3Embedding, Bgem3InitOptions, Bgem3Model, EmbeddingModel, TextEmbedding, TextInitOptions,
};

use crate::embed::{EMBEDDING_DIM, Embedder, normalize};

/// 后台回填的批大小。
///
/// 远小于 fastembed 自己的 256：BGE-M3 的 ColBERT 输出是
/// `[batch, seq_len, 1024]` 的 f32，256×512 那一档就要 500 MB 以上的临时张量，
/// 在 architecture.md 划定的 8 GB 内存预算里直接 OOM。
pub const DEFAULT_BACKFILL_BATCH: usize = 32;

/// 序列截断长度。
///
/// BGE-M3 本身支持 8192，但记忆里存的是**单条事实陈述**（几十字量级），
/// 放大窗口只会让 padding 与注意力开销线性上涨而召回毫无变化。
pub const DEFAULT_MAX_LENGTH: usize = 512;

/// 可选的本地模型。**三个都是 1024 维**——这不是巧合，是硬约束：
/// migration 写死 `VECTOR(1024)`，维度不同的模型根本存不进去。
///
/// 三个都支持中文。fastembed 里其余 1024 维模型（`BGELargeENV15`、
/// `MxbaiEmbedLargeV1`、`GTELargeENV15`…）全是纯英文的，对本项目不可用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FastModel {
    /// BAAI/bge-m3 的 int8 量化版（约 600 MB）。
    ///
    /// 默认值。architecture.md 的部署预算写明「8 GB 内存机器上 int8 是必选项」，
    /// fp32 的 2.2 GB 会让最低配 VPS 直接跑不起来。
    #[default]
    BgeM3Int8,
    /// BAAI/bge-m3 原始 fp32（约 2.2 GB）。量化损失不可接受时再用。
    BgeM3Fp32,
    /// intfloat/multilingual-e5-large。
    ///
    /// 留作对照与逃生门：万一 bge-m3 的中文表现不及预期，
    /// 换它不需要动 schema（同为 1024 维），可直接走渐进回填。
    MultilingualE5Large,
}

impl FastModel {
    /// 写进各表 `embedding_model` 列的标识。
    ///
    /// 带上 HF 仓库名而不是简称，因为**量化版与原版是不同的权重**，
    /// 产出的向量不在同一空间。渐进回填靠这个字段区分新旧，简称区分不开。
    #[must_use]
    pub fn model_id(self) -> &'static str {
        match self {
            Self::BgeM3Int8 => "fastembed:gpahal/bge-m3-onnx-int8",
            Self::BgeM3Fp32 => "fastembed:BAAI/bge-m3",
            Self::MultilingualE5Large => "fastembed:Qdrant/multilingual-e5-large-onnx",
        }
    }

    /// 解析 `CORTEX_EMBED_MODEL`。
    ///
    /// # Errors
    ///
    /// 名字不认识时报错而不是静默回落到默认值——回落会让人以为换模型生效了，
    /// 但库里写的还是旧 `embedding_model`，几周后才在召回质量上暴露。
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bge-m3-int8" | "bge-m3" => Ok(Self::BgeM3Int8),
            "bge-m3-fp32" => Ok(Self::BgeM3Fp32),
            "multilingual-e5-large" | "e5-large" => Ok(Self::MultilingualE5Large),
            other => Err(CortexError::Config(format!(
                "未知的 embedding 模型 {other:?}，可选：\
                 bge-m3-int8 / bge-m3-fp32 / multilingual-e5-large"
            ))),
        }
    }

    /// fastembed 静态登记的维度。用于启动即校验，无需跑一次推理。
    fn declared_dim(self) -> usize {
        match self {
            Self::BgeM3Int8 => Bgem3Embedding::get_model_info(&Bgem3Model::BGEM3Q).dim,
            Self::BgeM3Fp32 => text_model_dim(&EmbeddingModel::BGEM3),
            Self::MultilingualE5Large => text_model_dim(&EmbeddingModel::MultilingualE5Large),
        }
    }
}

fn text_model_dim(model: &EmbeddingModel) -> usize {
    TextEmbedding::list_supported_models()
        .into_iter()
        .find(|m| &m.model == model)
        .map_or(0, |m| m.dim)
}

/// 已加载的 ONNX 会话。
///
/// 两个变体不是为了兼容，是因为 fastembed 把 BGE-M3 拆成了两套 API：
/// int8 版只在 `Bgem3Embedding`（dense+sparse+colbert 联合输出）里，
/// `TextEmbedding` 那条路拿不到量化权重。我们只取 dense。
enum Session {
    Joint(Box<Bgem3Embedding>),
    Dense(Box<TextEmbedding>),
}

impl Session {
    /// 同步阻塞推理。调用方**必须**已经在 `spawn_blocking` 里。
    fn run(&mut self, texts: &[String], batch: usize) -> Result<Vec<Vec<f32>>> {
        let raw = match self {
            Self::Joint(m) => m.embed(texts, Some(batch)).map(|o| o.dense),
            Self::Dense(m) => m.embed(texts, Some(batch)),
        };
        raw.map_err(|e| CortexError::Memory(format!("ONNX 推理失败：{e}")))
    }
}

/// [`FastEmbedder`] 的构造参数。
#[derive(Debug, Clone)]
pub struct FastEmbedderConfig {
    pub model: FastModel,
    /// 模型缓存目录。`None` 时按 [`default_cache_dir`] 推导。
    pub cache_dir: Option<PathBuf>,
    pub backfill_batch: usize,
    pub max_length: usize,
}

impl Default for FastEmbedderConfig {
    fn default() -> Self {
        Self {
            model: FastModel::default(),
            cache_dir: None,
            backfill_batch: DEFAULT_BACKFILL_BATCH,
            max_length: DEFAULT_MAX_LENGTH,
        }
    }
}

impl FastEmbedderConfig {
    /// 从环境变量读取。
    ///
    /// # Errors
    ///
    /// 模型名或批大小无法解析时报错。
    pub fn from_env() -> Result<Self> {
        // 空串按「没设」处理。**这不是防御性编程，是一个已经发生过两次的
        // 生产故障**：compose 里的 `${VAR:-}` 把没写进 .env 的变量设成
        // 空串而不是不设，而 `env::var` 对空串返回 `Ok("")`。
        //
        // 具体到这里：`CORTEX_EMBED_BATCH: ${CORTEX_EMBED_BATCH:-}` 会让
        // `"".parse::<usize>()` 失败，于是 **cortexd 拒绝启动** ——
        // 而运维那侧根本没配过这个变量。
        //
        // `cortex-core` 的 `non_empty`、`embed_api::from_env` 各有一份同样的
        // 处理，这里是第三处。刻意不抽公共函数：`cortex-core` 的那个是
        // 私有的，为一个三行闭包在 crate 之间开一个公共 API，
        // 换来的耦合比省下的重复更贵。
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());

        let mut cfg = Self::default();
        if let Some(v) = var("CORTEX_EMBED_MODEL") {
            cfg.model = FastModel::parse(&v)?;
        }
        if let Some(v) = var("CORTEX_MODEL_CACHE") {
            cfg.cache_dir = Some(PathBuf::from(v));
        }
        if let Some(v) = var("CORTEX_EMBED_BATCH") {
            cfg.backfill_batch = v.trim().parse().map_err(|_| {
                CortexError::Config(format!("CORTEX_EMBED_BATCH 不是正整数：{v:?}"))
            })?;
            if cfg.backfill_batch == 0 {
                return Err(CortexError::Config("CORTEX_EMBED_BATCH 不能为 0".into()));
            }
        }
        Ok(cfg)
    }

    /// 阻塞地加载模型（首次会下载几百 MB）。
    fn load(self) -> Result<Session> {
        let cache = self.cache_dir.unwrap_or_else(default_cache_dir);
        let map =
            |e: fastembed::Error| CortexError::Memory(format!("加载 embedding 模型失败：{e}"));

        match self.model {
            FastModel::BgeM3Int8 => Bgem3Embedding::try_new(
                Bgem3InitOptions::new(Bgem3Model::BGEM3Q)
                    .with_cache_dir(cache)
                    .with_max_length(self.max_length)
                    // 下载进度条是 stdout 的裸写入，会打乱 CLI 的 SSE 输出；
                    // 首次下载的可见性由下面的 tracing 事件负责
                    .with_show_download_progress(false),
            )
            .map(|m| Session::Joint(Box::new(m)))
            .map_err(map),

            FastModel::BgeM3Fp32 => text_session(EmbeddingModel::BGEM3, cache, self.max_length),
            FastModel::MultilingualE5Large => {
                text_session(EmbeddingModel::MultilingualE5Large, cache, self.max_length)
            }
        }
    }
}

fn text_session(model: EmbeddingModel, cache: PathBuf, max_length: usize) -> Result<Session> {
    TextEmbedding::try_new(
        TextInitOptions::new(model)
            .with_cache_dir(cache)
            .with_max_length(max_length)
            .with_show_download_progress(false),
    )
    .map(|m| Session::Dense(Box::new(m)))
    .map_err(|e| CortexError::Memory(format!("加载 embedding 模型失败：{e}")))
}

/// 模型缓存目录。
///
/// fastembed 的默认是 CWD 下的 `./.fastembed_cache`——那意味着从不同目录
/// 启动 cortexd 就各下一份 600 MB，而且换个工作目录就「模型不见了」。
/// 统一落到用户级 cache 目录，全机共享一份。
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    // 尊重 fastembed 自己的变量，方便与其它用了 fastembed 的工具共用缓存
    if let Ok(v) = std::env::var("FASTEMBED_CACHE_DIR") {
        return PathBuf::from(v);
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));

    base.map_or_else(
        || PathBuf::from(".fastembed_cache"),
        |b| b.join("cortex").join("models"),
    )
}

/// 真实语义向量化器。
///
/// # 为什么整个模型被一把 `Mutex` 锁住
///
/// fastembed 的 `embed` 取 `&mut self`（内部 ONNX session 复用输入缓冲），
/// 而 [`Embedder::embed`] 只有 `&self`。可选做法有三：
///
/// 1. 每次调用新建 session —— 每次几百 MB 加载，绝对不可行
/// 2. 每条并发路径一份模型 —— int8 也要 600 MB × N，8 GB 机器上放不下
/// 3. **一份模型 + 互斥** —— 采用
///
/// 互斥让并发 embed 串行化，但这**本来就是想要的**：ONNX Runtime 已经把
/// 单次推理并行到了所有核上，再叠加并发只会让线程互相抢核而总吞吐下降。
///
/// 代价是一次回填会挡住交互 query，且 **fastembed 内部按 `batch_size` 分块时
/// 锁并不释放**——一次 `embed` 调用无论传多少条都独占到底。
/// 所以「让 query 插队」不可能在这一层解决，只能靠上层纪律：
/// 回填任务要自己把几千条切成一批批多次调用，给 query 留出缝隙。
pub struct FastEmbedder {
    session: Arc<Mutex<Session>>,
    model_id: &'static str,
    backfill_batch: usize,
}

impl FastEmbedder {
    /// 按环境变量加载。
    ///
    /// # Errors
    ///
    /// 配置非法、模型下载失败、ONNX session 创建失败。
    pub async fn from_env() -> Result<Self> {
        Self::new(FastEmbedderConfig::from_env()?).await
    }

    /// 加载模型。**构造即加载**，不做惰性初始化。
    ///
    /// 惰性会把几十秒的加载推迟到第一次 query，表现为「第一条消息卡住」；
    /// 更糟的是模型缺失/损坏要到那时才暴露，而不是启动时。
    ///
    /// # Errors
    ///
    /// 模型下载失败或 ONNX session 创建失败。
    pub async fn new(config: FastEmbedderConfig) -> Result<Self> {
        let model_id = config.model.model_id();
        let backfill_batch = config.backfill_batch;
        let declared = config.model.declared_dim();

        // 维度不符时**加载前就失败**：加载要几十秒，没必要为一个注定写不进
        // VECTOR(1024) 的模型白等
        if declared != EMBEDDING_DIM {
            return Err(CortexError::Config(format!(
                "模型 {model_id} 是 {declared} 维，但 schema 写死 VECTOR({EMBEDDING_DIM})；\
                 换维度需要配套 migration（见 memory.md §七）"
            )));
        }

        tracing::info!(model = model_id, "加载 embedding 模型（首次会下载数百 MB）");
        let started = std::time::Instant::now();

        // 加载是纯阻塞的磁盘 IO + ONNX 图构建，几十秒起步。
        // 放在 async 上下文里直裸跑会让整个 tokio runtime 在这期间不调度任何任务。
        let session = tokio::task::spawn_blocking(move || config.load())
            .await
            .map_err(|e| CortexError::Memory(format!("加载模型的阻塞任务 panic：{e}")))??;

        tracing::info!(
            model = model_id,
            elapsed_ms = started.elapsed().as_millis(),
            "embedding 模型就绪"
        );

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            model_id,
            backfill_batch,
        })
    }

    /// 指定批大小的推理。
    ///
    /// 交互路径与回填路径的取舍相反：query 只有一条、要的是最低延迟；
    /// 回填有成千上万条、要的是吞吐。ONNX 会把整批 padding 到批内最长序列，
    /// 所以把一条短 query 塞进大 batch 反而更慢——两条路必须分开。
    ///
    /// # 分开的代价：约 0.013 的余弦漂移
    ///
    /// int8 GEMM 在 batch 维度变化时走不同的 kernel（tiling 与累加顺序不同），
    /// 实测 batch=1 与 batch≥2 的同一条文本相差 0.012–0.015，
    /// 与批内其它文本的长度无关，同一批次重复调用则逐位一致。
    /// 也就是说：**回填写进库的向量与查询向量之间有一个固定的小偏差**。
    ///
    /// 之所以不为此把回填也降到 batch=1：语义信号量级是它的 30 倍
    /// （实测同义 0.81 / 无关 0.38），而 batch=32 的吞吐收益是数倍。
    /// 见 `tests/fastembed.rs` 的 `MAX_BATCH_DRIFT`。
    ///
    /// # Errors
    ///
    /// 推理失败，或产出的向量维度/模长非法。
    pub async fn embed_with_batch(
        &self,
        texts: &[String],
        batch_size: usize,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if batch_size == 0 {
            return Err(CortexError::Invalid("batch_size 不能为 0".into()));
        }

        let session = Arc::clone(&self.session);
        let owned = texts.to_vec();

        let mut vectors = tokio::task::spawn_blocking(move || {
            // 锁中毒说明上一次推理 panic 了。ONNX session 的内部状态在
            // `run` 失败后仍是自洽的，为一次瞬时 panic 把 embedder 永久废掉
            // 会连带废掉整个记忆写入路径，代价远大于收益 —— 恢复它。
            let mut guard = session.lock().unwrap_or_else(PoisonError::into_inner);
            guard.run(&owned, batch_size)
        })
        .await
        .map_err(|e| CortexError::Memory(format!("embedding 任务 panic：{e}")))??;

        if vectors.len() != texts.len() {
            return Err(CortexError::Memory(format!(
                "embedder 返回 {} 条向量，但输入是 {} 条",
                vectors.len(),
                texts.len()
            )));
        }

        for (i, v) in vectors.iter_mut().enumerate() {
            normalize(v).map_err(|e| CortexError::Memory(format!("第 {i} 条文本：{e}")))?;
        }
        Ok(vectors)
    }
}

#[async_trait::async_trait]
impl Embedder for FastEmbedder {
    fn model_id(&self) -> &str {
        self.model_id
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // 单条即交互路径，batch=1 不做任何 padding 浪费
        let batch = if texts.len() == 1 {
            1
        } else {
            self.backfill_batch
        };
        self.embed_with_batch(texts, batch).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fast_model_matches_schema_dimension() {
        // 这条断言是 migration 的护栏：往 FastModel 里加一个非 1024 维的模型，
        // 在这里就炸，而不是等到运行时 INSERT 被 Postgres 拒绝
        for m in [
            FastModel::BgeM3Int8,
            FastModel::BgeM3Fp32,
            FastModel::MultilingualE5Large,
        ] {
            assert_eq!(
                m.declared_dim(),
                EMBEDDING_DIM,
                "{} 是 {} 维，写不进 VECTOR({EMBEDDING_DIM})",
                m.model_id(),
                m.declared_dim()
            );
        }
    }

    #[test]
    fn model_ids_are_distinct() {
        // embedding_model 列是渐进回填唯一的判据，撞名等于新旧向量混在一个
        // 坐标系里算距离——结果无意义且无人察觉。
        //
        // 跨后端也必须不同：远端跑的 bge-m3（`api:bge-m3`）与本地 int8
        // （`fastembed:gpahal/bge-m3-onnx-int8`）虽然是「同一个模型」，
        // 量化与池化实现并不逐位一致，混在一张表里是静默的质量损失
        let hash = crate::embed::HashEmbedder::new();
        let ids = [
            FastModel::BgeM3Int8.model_id(),
            FastModel::BgeM3Fp32.model_id(),
            FastModel::MultilingualE5Large.model_id(),
            hash.model_id(),
        ];
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "model_id 必须两两不同，实际 {ids:?}"
        );
    }

    #[test]
    fn unknown_names_are_rejected_not_defaulted() {
        assert!(
            FastModel::parse("bge-m4").is_err(),
            "无法识别的模型名必须报错；静默回落到默认模型会让人以为换模型生效了"
        );
        assert_eq!(
            FastModel::parse("BGE-M3-Int8").unwrap(),
            FastModel::BgeM3Int8
        );
    }

    #[test]
    fn cache_dir_is_not_cwd_relative() {
        // fastembed 默认 ./.fastembed_cache 会随启动目录漂移，
        // 表现为「换个目录跑就重新下 600 MB」
        let dir = default_cache_dir();
        assert!(
            dir.is_absolute() || std::env::var_os("FASTEMBED_CACHE_DIR").is_some(),
            "默认缓存目录应是绝对路径，实际 {dir:?}"
        );
    }
}

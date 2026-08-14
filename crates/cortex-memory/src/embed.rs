//! 向量化 —— [`Embedder`] 抽象与后端选择。
//!
//! 三个后端，具体实现各在一处：
//!
//! | 后端 | 在哪 | 何时用 |
//! |---|---|---|
//! | `api`（**默认**） | [`crate::embed_api`] | 调远端 `/v1/embeddings`（自建容器或云服务） |
//! | `hash` | 本文件 | 离线开发与 CI。**不是语义空间** |
//! | `fast` | [`crate::embed_local`]，feature `local-embed` | 一个进程搞定，不依赖外部服务 |
//!
//! # 为什么默认从进程内推理改成了 HTTP
//!
//! 在此之前 `fastembed`（内部 ONNX Runtime）是硬依赖。代价不只是「重」：
//! Intel Mac 上 `ort-sys` 没有预编译产物、**构建期**就失败；每台机器都要
//! 能拿到那 590 MB 权重，而生产节点上 Hugging Face 完全不可达时，
//! cortexd 会**照常启动、`/health` 返回 ok**，只在日志里留一行
//! 「回落到 mock」—— 记忆检索是假的而没有任何红灯。
//! 详见 [`crate::embed_api`] 的模块注释。
//!
//! 「记忆内容不应出网」这条理由**只对一半成立**，得说清楚：抽取本来就
//! 已经把对话发给 LLM 供应商了。走 API 之后多出网的是**每条事实、
//! 每个 episode 与每次检索 query**。真在意这一点的部署，把
//! `CORTEX_EMBED_ENDPOINT` 指向内网那个容器即可 —— 协议是同一个。
//!
//! # 维度是 schema 的一部分
//!
//! 迁移里写死 `VECTOR(1024)`。换模型要么维度一致，要么得加新列迁移
//! （见 memory.md §七「换模型不停机」）。所有向量表都记 `embedding_model`，
//! 正是为了支持渐进回填。
//!
//! # 环境变量
//!
//! | 变量 | 取值 | 默认 |
//! |---|---|---|
//! | `CORTEX_EMBED_BACKEND` | `api` \| `hash` \| `fast` | `api` |
//! | `CORTEX_EMBED_ENDPOINT` | 服务地址 | `http://embeddings/v1/embeddings` |
//! | `CORTEX_EMBED_MODEL` | 模型名 | `bge-m3` |
//! | `CORTEX_EMBED_API_KEY` | 鉴权，自建可不填 | 无 |
//! | `CORTEX_EMBED_BATCH` | 单请求条数 / 回填批大小 | 10 |

use std::sync::Arc;

use cortex_core::{CortexError, Result};

/// schema 写死的向量维度。变更需配套 migration。
pub const EMBEDDING_DIM: usize = 1024;

/// 向量化后端。
///
/// 抽象成 trait 有两个实际用途：测试时换成确定性的假实现（不必下载模型），
/// 以及将来换模型时新旧并存做渐进回填。
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// 模型标识，写入各表的 `embedding_model` 列。
    fn model_id(&self) -> &str;

    fn dim(&self) -> usize {
        EMBEDDING_DIM
    }

    /// 批量向量化。交互路径用 batch=1，后台回填用大 batch。
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// 单条便捷方法。
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(&[text.to_string()]).await?;
        v.pop()
            .ok_or_else(|| CortexError::Memory("embedder 返回空结果".into()))
    }
}

/// 确定性的哈希向量化器 —— **仅供测试与离线开发**。
///
/// 不具备任何语义能力（同义词不会相近），但它无需下载模型、无需网络、
/// 输出稳定可断言，使得存储层与检索管线的测试不必依赖真实模型。
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    #[must_use]
    pub fn new() -> Self {
        Self { dim: EMBEDDING_DIM }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        "hash-stub-v1"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|t| hash_vector(t, self.dim)).collect())
    }
}

/// 把文本散列成单位长度向量。相同输入必得相同输出。
fn hash_vector(text: &str, dim: usize) -> Vec<f32> {
    use std::hash::{Hash, Hasher};

    let mut v = vec![0f32; dim];
    // 按字符 n-gram 散列，让相似字符串的向量也有一定相关性
    let chars: Vec<char> = text.chars().collect();
    for window in chars.windows(2.min(chars.len().max(1))) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        window.hash(&mut h);
        let idx = (h.finish() % dim as u64) as usize;
        v[idx] += 1.0;
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

// ───────────────────────────── 真实语义模型 ─────────────────────────────

/// 就地归一化到单位长度。
///
/// 模型是否已归一化取决于导出时的图结构（int8 版与 fp32 版并不一致），
/// 与其逐模型考据，不如统一在这里兜底——对已归一化的向量这是恒等操作。
pub(crate) fn normalize(v: &mut [f32]) -> std::result::Result<(), String> {
    if v.len() != EMBEDDING_DIM {
        return Err(format!(
            "维度 {} 与 schema 的 VECTOR({EMBEDDING_DIM}) 不符",
            v.len()
        ));
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    // 零向量在 pgvector 的 cosine 距离下得到 NaN，会静默污染整个 ORDER BY，
    // 比在这里报错难查得多
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(format!("模长 {norm} 非法，无法归一化"));
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
    Ok(())
}

// ───────────────────────────── 后端选择 ─────────────────────────────

/// 便于在配置里切换后端。
pub type SharedEmbedder = Arc<dyn Embedder>;

/// 选哪个后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// 调远端 `/v1/embeddings`。**默认。**
    #[default]
    Api,
    /// 确定性哈希桩，仅测试与离线开发。
    Hash,
    /// 进程内 ONNX 推理。需要 feature `local-embed`。
    ///
    /// 变体本身**不**跟着 feature 一起消失：那样的话没开 feature 时
    /// `CORTEX_EMBED_BACKEND=fast` 会报「未知取值」，把「这个后端没编进来」
    /// 说成「你拼错了」。留着变体，让 [`build_embedder`] 给出真正的原因。
    Fast,
}

impl Backend {
    /// 解析 `CORTEX_EMBED_BACKEND`。
    ///
    /// **默认是 `api`**。看起来 `hash` 更「安全」（不联网、不依赖任何服务），
    /// 但那会让语义召回在没人配置时静默失效：检索照常返回结果、
    /// 没有任何报错，只是召回质量烂得毫无道理 —— 这种错要几周才发现。
    /// 相比之下连不上 embedding 服务是**响亮**的：第一次写记忆就报错。
    ///
    /// CI 与离线开发显式设 `CORTEX_EMBED_BACKEND=hash`。
    ///
    /// # Errors
    ///
    /// 取值不认识时报错。
    pub fn from_env() -> Result<Self> {
        // 空串按「没设」处理，理由见 cortex_core::config 的 `non_empty`
        std::env::var("CORTEX_EMBED_BACKEND")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or(Ok(Self::Api), |v| Self::parse(&v))
    }

    /// 解析后端名。
    ///
    /// # Errors
    ///
    /// 取值不认识时报错。
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "api" | "http" | "remote" => Ok(Self::Api),
            "hash" | "stub" => Ok(Self::Hash),
            "fast" | "fastembed" | "local" => Ok(Self::Fast),
            other => Err(CortexError::Config(format!(
                "未知的 CORTEX_EMBED_BACKEND={other:?}，可选：api / hash / fast"
            ))),
        }
    }
}

/// 新建一个 embedder（**不**复用全局实例）。
///
/// 换模型的渐进回填需要新旧两个实例同时在世（memory.md §七），
/// 所以这条路必须保留；日常代码应该用 [`shared_embedder`]。
///
/// # Errors
///
/// 配置非法或模型加载失败。
pub async fn build_embedder(backend: Backend) -> Result<SharedEmbedder> {
    match backend {
        Backend::Hash => Ok(Arc::new(HashEmbedder::new())),
        Backend::Api => Ok(Arc::new(crate::embed_api::ApiEmbedder::from_env()?)),

        #[cfg(feature = "local-embed")]
        Backend::Fast => Ok(Arc::new(
            crate::embed_local::FastEmbedder::from_env().await?,
        )),

        // 没编进来时给出**真正的原因**，而不是让它在 parse 那一步
        // 表现成「未知取值」——后者会把「这个二进制没带这个后端」
        // 说成「你把名字拼错了」，而两者的处置完全不同
        #[cfg(not(feature = "local-embed"))]
        Backend::Fast => Err(CortexError::Config(
            "CORTEX_EMBED_BACKEND=fast 需要 feature `local-embed`，而这个二进制没有编进来。\n\
             要么改用默认的 api 后端（把 CORTEX_EMBED_ENDPOINT 指向一个 \
             OpenAI 兼容的 /v1/embeddings 服务），\n\
             要么自己带 feature 重编：cargo build -p cortexd --features cortex-memory/local-embed"
                .into(),
        )),
    }
}

/// 进程内共享的 embedder，按环境变量选择后端。
///
/// 用 `OnceCell` 而不是每处各建一个：模型 int8 也有 600 MB，
/// 「daemon 而非嵌入式」这个架构决策的理由之一就是**模型常驻共享**
/// （architecture.md）。重复加载会直接吃掉那个收益。
///
/// # Errors
///
/// 首次调用时配置非法或模型加载失败；失败不会被缓存，下次调用会重试。
pub async fn shared_embedder() -> Result<SharedEmbedder> {
    static SHARED: tokio::sync::OnceCell<SharedEmbedder> = tokio::sync::OnceCell::const_new();
    SHARED
        .get_or_try_init(|| async { build_embedder(Backend::from_env()?).await })
        .await
        .cloned()
}

/// 让 `Arc<dyn Embedder>` 本身也是 `Embedder`。
///
/// 没有这个的话，凡是持有 embedder 的泛型结构（如 `Retriever<E: Embedder>`）
/// 都无法用共享句柄实例化，调用方只能到处写 `&*arc` 或改成非泛型 ——
/// 前者难看，后者丢掉静态分发。
#[async_trait::async_trait]
impl<T: Embedder + ?Sized> Embedder for Arc<T> {
    fn model_id(&self) -> &str {
        (**self).model_id()
    }

    fn dim(&self) -> usize {
        (**self).dim()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        (**self).embed(texts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_embedder_is_deterministic() {
        let e = HashEmbedder::new();
        let a = e.embed_one("对象存储用 RustFS").await.unwrap();
        let b = e.embed_one("对象存储用 RustFS").await.unwrap();
        assert_eq!(a, b, "相同输入必须得到相同向量，否则测试无法断言");
    }

    #[tokio::test]
    async fn produces_schema_dimension() {
        let e = HashEmbedder::new();
        let v = e.embed_one("x").await.unwrap();
        assert_eq!(
            v.len(),
            EMBEDDING_DIM,
            "维度必须与 migration 的 VECTOR(1024) 一致"
        );
    }

    #[tokio::test]
    async fn vectors_are_unit_length() {
        let e = HashEmbedder::new();
        let v = e.embed_one("一些内容").await.unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "余弦距离要求单位向量，实际模长 {norm}"
        );
    }

    #[tokio::test]
    async fn batch_matches_single() {
        let e = HashEmbedder::new();
        let batch = e.embed(&["a".into(), "b".into()]).await.unwrap();
        assert_eq!(batch.len(), 2);
        assert_eq!(batch[0], e.embed_one("a").await.unwrap());
    }

    #[test]
    fn normalize_rejects_zero_vector() {
        let mut v = vec![0f32; EMBEDDING_DIM];
        assert!(
            normalize(&mut v).is_err(),
            "零向量必须报错：pgvector 的 cosine 距离会给出 NaN 并静默污染排序"
        );

        let mut v = vec![2f32; EMBEDDING_DIM];
        normalize(&mut v).unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "归一化后模长应为 1，实际 {norm}");
    }

    #[test]
    fn normalize_rejects_wrong_dimension() {
        let mut v = vec![1f32; 768];
        assert!(
            normalize(&mut v).is_err(),
            "非 1024 维必须在写库前被拦下，否则错误会以 Postgres 类型错误的形式出现在很远的地方"
        );
    }

    #[tokio::test]
    async fn hash_backend_needs_no_model() {
        // 这条同时是 CI 的可用性保证：CORTEX_EMBED_BACKEND=hash 必须能在
        // 无网络、无模型缓存的机器上走通
        let e = build_embedder(Backend::Hash).await.unwrap();
        assert_eq!(e.model_id(), "hash-stub-v1");
        assert_eq!(e.embed_one("x").await.unwrap().len(), EMBEDDING_DIM);
    }

    #[test]
    fn backend_defaults_to_api_and_rejects_typos() {
        assert_eq!(Backend::parse("").unwrap(), Backend::Api);
        assert_eq!(Backend::parse(" HASH ").unwrap(), Backend::Hash);
        assert!(
            Backend::parse("apii").is_err(),
            "拼错的后端名必须报错；静默回落会让「CI 用 hash」的意图失效"
        );
        assert_eq!(
            Backend::default(),
            Backend::Api,
            "默认必须是真实语义后端：默认成 hash 会让召回在没人配置时静默失效"
        );
    }
}

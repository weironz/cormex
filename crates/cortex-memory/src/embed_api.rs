//! 走 HTTP 的 embedding 后端 —— OpenAI 兼容的 `/v1/embeddings`。
//!
//! # 为什么默认是这一条，而不是进程内推理
//!
//! 在此之前 `fastembed`（内部 ONNX Runtime）是 `cortex-memory` 的**硬依赖**，
//! 没有 feature gate。代价不只是「重」：
//!
//! 1. **Intel Mac 根本编不出来** —— `ort-sys: no prebuilt binaries available
//!    for target x86_64-apple-darwin`。v0.1.0 发版时试了两轮才确认这不是
//!    「没有现成二进制」而是「编不了」，连 `CORTEX_EMBED_BACKEND=hash`
//!    也救不了，因为 ort-sys 在**构建期**就失败
//! 2. **每台跑 cortexd 的机器都得能拿到那 590 MB**。第一次上生产时
//!    那台节点 Hugging Face 完全不可达（DNS 被污染到 Meta 的段、
//!    `hf-mirror.com` 也不通），cortexd 于是**照常启动、`/health` 返回 ok**，
//!    只在日志里留一行「回落到 mock 数据源」—— 记忆检索是假的而没有红灯
//! 3. 启动加载 12 秒、常驻 1.0 GiB
//!
//! 换成 HTTP 之后这三条一起消失，而且**同一个协议同时覆盖两种部署**：
//! `CORTEX_EMBED_ENDPOINT` 指向 compose 里那个自建容器，或者指向
//! 任何一家兼容 OpenAI 的云服务，切换只是改一行环境变量。
//!
//! # 维度不匹配必须当场炸
//!
//! schema 把向量列写死成 `VECTOR(1024)`。接错一家（OpenAI 的
//! `text-embedding-3-small` 是 1536）时，最坏的结果不是报错，是
//! **写入被数据库拒绝、而调用方把它当成一次普通的写失败**，
//! 或者更糟：某些服务会截断。所以这里在**第一次响应**就校验维度并
//! 给出指名道姓的错误，而不是把问题推给 Postgres。

use std::time::Duration;

use cortex_core::{CortexError, Result};
use serde::{Deserialize, Serialize};

use crate::embed::{EMBEDDING_DIM, Embedder, normalize};

/// 单次请求最多送多少条文本。
///
/// 不是性能调优，是**兼容性下限**：各家对 batch 大小的限制差得很远
/// （TEI 默认 32，阿里百炼 `text-embedding-v3` 是 10）。取 10 让默认配置
/// 在所有已知服务上都能跑通；自建服务想要更大的吞吐可以调
/// `CORTEX_EMBED_BATCH`。
const DEFAULT_BATCH: usize = 10;

/// 单次 HTTP 请求的超时。
///
/// 给到 60 秒是因为**冷启动**：自建的 TEI 容器第一次收到请求时可能还在
/// 载入权重。设成常见的 30 秒会让「服务刚起来的头一分钟」表现为
/// 一串超时失败，而那正是有人在盯着看的时候。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// 失败重试次数（不含首次）。
///
/// 只重试**可能是瞬时**的错误：连接失败、超时、5xx、429。
/// 4xx（模型名不对、鉴权失败、维度不符）重试多少次都是同样的结果，
/// 重试只会把一条清楚的错误信息延迟三倍才让人看见。
const MAX_RETRIES: usize = 2;

/// [`ApiEmbedder`] 的构造参数。
#[derive(Debug, Clone)]
pub struct ApiEmbedderConfig {
    /// 完整的请求地址，或者能推导出它的基地址。见 [`resolve_url`]。
    pub endpoint: String,
    /// 模型名。自建 TEI 忽略这个字段，云服务必填。
    pub model: String,
    /// `Authorization: Bearer <key>`。自建服务通常不需要，留空即不发这个头。
    pub api_key: Option<String>,
    /// 单次请求最多几条文本。
    pub batch: usize,
}

impl ApiEmbedderConfig {
    /// 从环境变量读取。
    ///
    /// | 变量 | 含义 | 默认 |
    /// |---|---|---|
    /// | `CORTEX_EMBED_ENDPOINT` | 服务地址 | `http://embeddings/v1/embeddings` |
    /// | `CORTEX_EMBED_MODEL` | 模型名 | `bge-m3` |
    /// | `CORTEX_EMBED_API_KEY` | 鉴权（自建可不填） | 无 |
    /// | `CORTEX_EMBED_BATCH` | 单请求条数 | 10 |
    ///
    /// 默认地址指向 compose 里那个 `embeddings` 服务的容器名 ——
    /// 「什么都不配就能跑」的那条路是自托管，不是某一家云。
    ///
    /// # Errors
    ///
    /// `CORTEX_EMBED_BATCH` 不是正整数时报错。
    pub fn from_env() -> Result<Self> {
        // 空串按「没设」处理：compose 里的 `${VAR:-}` 会把没写在 .env 里的
        // 变量设成空串而不是不设，直接 `env::var` 会拿到 `Ok("")` 并把
        // 默认值顶掉。这个坑在 cortex-core 的 `non_empty` 注释里有完整记录
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());

        let batch = match var("CORTEX_EMBED_BATCH") {
            None => DEFAULT_BATCH,
            Some(v) => {
                let n: usize = v.trim().parse().map_err(|_| {
                    CortexError::Config(format!("CORTEX_EMBED_BATCH 不是正整数：{v:?}"))
                })?;
                if n == 0 {
                    return Err(CortexError::Config("CORTEX_EMBED_BATCH 不能为 0".into()));
                }
                n
            }
        };

        Ok(Self {
            endpoint: var("CORTEX_EMBED_ENDPOINT")
                .unwrap_or_else(|| "http://embeddings/v1/embeddings".to_string()),
            model: var("CORTEX_EMBED_MODEL").unwrap_or_else(|| "bge-m3".to_string()),
            api_key: var("CORTEX_EMBED_API_KEY"),
            batch,
        })
    }
}

/// 把用户填的地址补成真正要 POST 的 URL。
///
/// 存在的理由是各家文档给的「基地址」形状不一样，而让用户去猜要不要
/// 自己补 `/v1/embeddings` 的结果是**一半的人配错**，症状还是 404 ——
/// 一个看起来像「服务没起来」的错误。
///
/// 三种形状都认：
///
/// | 填的 | 实际请求 |
/// |---|---|
/// | `http://embeddings/v1/embeddings` | 原样 |
/// | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `…/v1/embeddings` |
/// | `http://embeddings` | `http://embeddings/v1/embeddings` |
/// # 只能看**路径**，不能看整串
///
/// 第一版写的是 `base.ends_with("/embeddings")`，它对
/// `http://embeddings` 返回真 —— `//embeddings` 里那个斜杠就够了。
/// 而 `embeddings` 恰好是 compose 里那个服务的容器名，
/// 于是**默认配置**会原样 POST 到 `http://embeddings` 并拿到 404，
/// 症状看起来像「服务没起来」。所以这里先把 authority 剥掉再判断。
#[must_use]
pub fn resolve_url(raw: &str) -> String {
    let base = raw.trim().trim_end_matches('/');

    // 找到 authority 之后第一个 '/'，它之后才是路径。
    // 没有 scheme 时（`embeddings:80`）也按同样的方式退化处理
    let after_scheme = base.find("://").map_or(0, |i| i + 3);
    let path = base[after_scheme..]
        .find('/')
        .map(|i| &base[after_scheme + i..]);

    match path {
        Some(p) if p.ends_with("/embeddings") => base.to_string(),
        Some(p) if p.ends_with("/v1") => format!("{base}/embeddings"),
        _ => format!("{base}/v1/embeddings"),
    }
}

#[derive(Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
    /// 显式要 float。不写的话有些服务默认回 base64（OpenAI 的 SDK 会这么请求），
    /// 而我们的反序列化只认数组 —— 那会表现为一条看不懂的解析错误
    encoding_format: &'static str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedItem>,
}

#[derive(Deserialize)]
struct EmbedItem {
    /// 请求里第几条。**必须按它排序**：协议不保证返回顺序与请求一致，
    /// 而错位不会报错 —— 它表现为「检索质量莫名其妙地差」，
    /// 是这类 bug 里最难被发现的一种
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

/// 调远端 `/v1/embeddings` 的 embedder。
pub struct ApiEmbedder {
    http: reqwest::Client,
    url: String,
    cfg: ApiEmbedderConfig,
    model_id: String,
}

impl std::fmt::Debug for ApiEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 手写而不是 derive：ApiEmbedderConfig 里有 api_key，
        // derive 出来的 Debug 会把密钥打进任何一条 `{:?}` 日志
        f.debug_struct("ApiEmbedder")
            .field("url", &self.url)
            .field("model", &self.cfg.model)
            .field("api_key", &self.cfg.api_key.as_ref().map(|_| "<已设置>"))
            .field("batch", &self.cfg.batch)
            .finish()
    }
}

impl ApiEmbedder {
    /// 按配置构造。**不**在这里探活：探活要发一次真实请求，
    /// 而 cortexd 启动时远端可能还没起来（compose 里两个服务同时拉起）。
    /// 第一次 embed 失败时的错误信息已经足够指明问题。
    ///
    /// # Errors
    ///
    /// HTTP 客户端构造失败（基本只可能是 TLS 后端有问题）。
    pub fn new(cfg: ApiEmbedderConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| CortexError::Memory(format!("构造 HTTP 客户端失败：{e}")))?;
        let url = resolve_url(&cfg.endpoint);
        // 写进各表 embedding_model 列的标识。带 `api:` 前缀是为了让
        // 「同一个模型名、本地跑 vs 远端跑」在库里可分辨 —— 两边的
        // 归一化与池化实现未必逐位一致，混在一张表里是静默的质量损失
        let model_id = format!("api:{}", cfg.model);
        Ok(Self {
            http,
            url,
            cfg,
            model_id,
        })
    }

    /// 从环境变量构造。
    ///
    /// # Errors
    ///
    /// 配置非法。
    pub fn from_env() -> Result<Self> {
        Self::new(ApiEmbedderConfig::from_env()?)
    }

    /// 从 `CORTEX_EMBED_LEGACY_*` 构造**上一个模型**的 embedder。没配就是 `None`。
    ///
    /// # 它解决什么
    ///
    /// 回填期间旧事实的向量还只在旧空间里。要让它们仍然被检索到，就得把
    /// **查询**也编到旧空间去 —— 而那需要旧模型本身还在手上。
    /// 这就是 memory.md §七 第 3 步「检索期间按模型分组，各自召回」的前提。
    ///
    /// # ⚠️ 它救不了「从进程内 fastembed 换到远端 API」这一次
    ///
    /// 那种迁移里的旧模型是 fastembed，而它默认已经不编进二进制
    /// （feature `local-embed`）。也就是说 docker 镜像里根本没有那个旧模型，
    /// 配了这几个变量也构造不出它来。
    ///
    /// 这条路覆盖的是**将来的 api → api 换模型**（换云厂商 / 换模型名）。
    /// 那一次迁移只能靠回填器把窗口熬过去，期间 BM25 / 图 / 近因三路兜着。
    ///
    /// # Errors
    ///
    /// 配了 endpoint 但别的取值非法。
    pub fn legacy_from_env() -> Result<Option<Self>> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());

        let Some(endpoint) = var("CORTEX_EMBED_LEGACY_ENDPOINT") else {
            return Ok(None);
        };
        // 模型名必填：旧空间的 `model_id` 要写进查询的过滤条件，
        // 猜错等于查了一个空集合 —— 而那与「没配」的表现一模一样，
        // 分辨不出来。宁可当场报错
        let model = var("CORTEX_EMBED_LEGACY_MODEL").ok_or_else(|| {
            CortexError::Config(
                "设了 CORTEX_EMBED_LEGACY_ENDPOINT 就必须设 CORTEX_EMBED_LEGACY_MODEL：\n\
                 旧模型名会成为向量查询的过滤条件，猜错的表现是「查到 0 条」，\n\
                 与「没开这个功能」一模一样，事后分辨不出来"
                    .into(),
            )
        })?;

        Self::new(ApiEmbedderConfig {
            endpoint,
            model,
            api_key: var("CORTEX_EMBED_LEGACY_API_KEY"),
            batch: DEFAULT_BATCH,
        })
        .map(Some)
    }

    /// 发一批（已经切好，长度 <= batch）。
    async fn embed_chunk(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = EmbedRequest {
            model: &self.cfg.model,
            input: texts,
            encoding_format: "float",
        };

        let mut last: Option<CortexError> = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                // 线性退避就够：这里重试的是「服务刚起来」与「瞬时 429」，
                // 不是雪崩场景
                tokio::time::sleep(Duration::from_millis(500 * attempt as u64)).await;
            }

            let mut req = self.http.post(&self.url).json(&body);
            if let Some(key) = &self.cfg.api_key {
                req = req.bearer_auth(key);
            }

            match req.send().await {
                Err(e) => {
                    last = Some(CortexError::Memory(format!(
                        "请求 embedding 服务失败（{}）：{e}",
                        self.url
                    )));
                }
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        let parsed: EmbedResponse = resp.json().await.map_err(|e| {
                            CortexError::Memory(format!("embedding 响应解析失败：{e}"))
                        })?;
                        return Self::collect(parsed, texts.len(), &self.cfg.model);
                    }

                    let retryable = status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                    let detail = resp.text().await.unwrap_or_default();
                    let err = CortexError::Memory(format!(
                        "embedding 服务返回 {status}（{}）：{}",
                        self.url,
                        detail.trim()
                    ));
                    if !retryable {
                        // 4xx 直接抛：模型名不对 / 鉴权失败 / 请求形状不对，
                        // 重试三次只是把同一条错误延后三倍才让人看见
                        return Err(err);
                    }
                    last = Some(err);
                }
            }
        }
        Err(last.unwrap_or_else(|| CortexError::Memory("embedding 请求失败".into())))
    }

    /// 校验并按 index 排好序。
    fn collect(mut parsed: EmbedResponse, want: usize, model: &str) -> Result<Vec<Vec<f32>>> {
        if parsed.data.len() != want {
            return Err(CortexError::Memory(format!(
                "embedding 服务返回 {} 条，但请求了 {want} 条",
                parsed.data.len()
            )));
        }
        parsed.data.sort_by_key(|d| d.index);

        for item in &parsed.data {
            if item.embedding.len() != EMBEDDING_DIM {
                return Err(CortexError::Memory(format!(
                    "维度不匹配：模型 {model} 返回 {} 维，而 schema 写死 {EMBEDDING_DIM} 维。\n\
                     换一个 {EMBEDDING_DIM} 维的模型（bge-m3 / text-embedding-v3 / jina-embeddings-v3 都可以），\n\
                     或者按 docs/memory.md 的「换模型不停机」加一列迁移。\n\
                     常见的踩法：OpenAI 的 text-embedding-3-small 是 1536 维、\
                     text-embedding-3-large 是 3072 维 —— 这两家支持 dimensions 参数缩到 {EMBEDDING_DIM}，\
                     但要在服务端配置里指定，我们不替你猜。",
                    item.embedding.len()
                )));
            }
        }
        let mut out: Vec<Vec<f32>> = parsed.data.into_iter().map(|d| d.embedding).collect();
        // 统一兜底归一化。**不能假设远端已经归一化过**：TEI 默认归一化，
        // 但这是它的一个可关闭选项，而各家云服务的口径并不一致、
        // 也很少写在文档里。对已归一化的向量这是恒等操作，代价可忽略。
        //
        // 不做的后果不是「差一点」：`retrieval` 的语义地板（0.50）是按
        // 单位向量的余弦距离标定的，喂进非单位向量之后那个阈值失去含义 ——
        // 表现为召回莫名其妙地宽或者莫名其妙地空，而没有任何报错。
        for (i, v) in out.iter_mut().enumerate() {
            normalize(v).map_err(|e| {
                CortexError::Memory(format!("模型 {model} 返回的第 {i} 条向量无法归一化：{e}"))
            })?;
        }
        Ok(out)
    }
}

#[async_trait::async_trait]
impl Embedder for ApiEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dim(&self) -> usize {
        EMBEDDING_DIM
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(self.cfg.batch) {
            out.extend(self.embed_chunk(chunk).await?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_url_accepts_all_three_shapes() {
        // 自建 TEI，文档里给的就是完整路径
        assert_eq!(
            resolve_url("http://embeddings/v1/embeddings"),
            "http://embeddings/v1/embeddings",
            "已经是完整路径的地址不该被再拼一次"
        );
        // 云服务文档给的通常是「基地址到 /v1 为止」
        assert_eq!(
            resolve_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings",
            "以 /v1 结尾的基地址只该补 /embeddings，不能补成 /v1/v1/embeddings"
        );
        // 只给了主机。**这条是回归测试**：第一版用 `ends_with("/embeddings")`
        // 判断整串，而 `http://embeddings` 里 `//embeddings` 那个斜杠就够它
        // 返回真 —— 而 `embeddings` 恰恰是 compose 里的服务名，
        // 于是默认配置直接 POST 到 404
        assert_eq!(
            resolve_url("http://embeddings"),
            "http://embeddings/v1/embeddings",
            "主机名叫 embeddings 时也必须补全路径 —— 不能把 authority 当成路径"
        );
        // 带端口的同一个坑
        assert_eq!(
            resolve_url("http://embeddings:80"),
            "http://embeddings:80/v1/embeddings",
            "带端口时同样要补全"
        );
        // 末尾多一个斜杠是最常见的手滑
        assert_eq!(
            resolve_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/embeddings",
            "末尾斜杠不该产生 //"
        );
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        let e = ApiEmbedder::new(ApiEmbedderConfig {
            endpoint: "http://x".into(),
            model: "bge-m3".into(),
            api_key: Some("sk-super-secret-value".into()),
            batch: 4,
        })
        .expect("构造不该失败");
        let s = format!("{e:?}");
        assert!(
            !s.contains("sk-super-secret-value"),
            "Debug 输出泄漏了 API key —— 它会随任何一条 {{:?}} 日志进日志文件：{s}"
        );
        assert!(
            s.contains("<已设置>"),
            "Debug 应当能看出 key 有没有配，只是不给出取值：{s}"
        );
    }

    #[test]
    fn collect_rejects_wrong_dimension_with_an_actionable_message() {
        let parsed = EmbedResponse {
            data: vec![EmbedItem {
                index: 0,
                embedding: vec![0.0; 1536], // OpenAI text-embedding-3-small
            }],
        };
        let err = ApiEmbedder::collect(parsed, 1, "text-embedding-3-small")
            .expect_err("1536 维必须被拒绝，不能写进 VECTOR(1024) 的列");
        let msg = err.to_string();
        assert!(
            msg.contains("1536") && msg.contains("1024"),
            "错误信息要同时给出实际维度与期望维度，否则排查得去翻 schema：{msg}"
        );
        assert!(
            msg.contains("text-embedding-3-small"),
            "错误信息要点名是哪个模型：{msg}"
        );
    }

    #[test]
    fn collect_sorts_by_index() {
        // 故意乱序返回：协议不保证顺序，而错位是静默的质量损失。
        // 两条向量的方向必须不同，否则归一化之后分辨不出谁是谁 ——
        // 也不能用零向量：那是**合法地**会被归一化拒掉的输入
        let mut a = vec![0.0f32; EMBEDDING_DIM];
        a[0] = 1.0; // index=0 那条
        let mut b = vec![0.0f32; EMBEDDING_DIM];
        b[1] = 1.0; // index=1 那条
        let parsed = EmbedResponse {
            data: vec![
                EmbedItem {
                    index: 1,
                    embedding: b,
                },
                EmbedItem {
                    index: 0,
                    embedding: a,
                },
            ],
        };
        let got = ApiEmbedder::collect(parsed, 2, "bge-m3").expect("两条都是 1024 维，应当通过");
        assert_eq!(
            (got[0][0], got[0][1]),
            (1.0, 0.0),
            "返回顺序必须按 index 归位 —— 错位不会报错，只会让检索莫名其妙地差"
        );
        assert_eq!(
            (got[1][0], got[1][1]),
            (0.0, 1.0),
            "第二条应当是 index=1 那条"
        );
    }

    #[test]
    fn collect_normalizes_and_rejects_zero_vectors() {
        // 远端未必归一化（TEI 默认归一化，但那是可关的；各家云口径不一）。
        // 语义地板 0.50 是按单位向量的余弦距离标定的，喂进非单位向量之后
        // 那个阈值失去含义 —— 而且不会报错
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = 3.0;
        v[1] = 4.0; // 模长 5
        let got = ApiEmbedder::collect(
            EmbedResponse {
                data: vec![EmbedItem {
                    index: 0,
                    embedding: v,
                }],
            },
            1,
            "bge-m3",
        )
        .expect("合法向量应当通过");
        let norm = got[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "返回的向量必须是单位长度，实际模长 {norm}"
        );

        // 零向量在 pgvector 的余弦距离下得到 NaN，会静默污染整个 ORDER BY
        let err = ApiEmbedder::collect(
            EmbedResponse {
                data: vec![EmbedItem {
                    index: 0,
                    embedding: vec![0.0; EMBEDDING_DIM],
                }],
            },
            1,
            "bge-m3",
        )
        .expect_err("零向量必须被拒 —— 它在 pgvector 里会变成 NaN 并污染排序");
        assert!(
            err.to_string().contains("归一化"),
            "错误信息要说清原因：{err}"
        );
    }

    #[test]
    fn collect_rejects_count_mismatch() {
        let parsed = EmbedResponse {
            data: vec![EmbedItem {
                index: 0,
                embedding: vec![0.0; EMBEDDING_DIM],
            }],
        };
        let err = ApiEmbedder::collect(parsed, 3, "bge-m3")
            .expect_err("少返回了两条必须报错，否则调用方会拿到一个错位的数组");
        assert!(
            err.to_string().contains('3'),
            "错误信息要说清请求了几条：{err}"
        );
    }
}

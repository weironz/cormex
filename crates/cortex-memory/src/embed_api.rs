//! The embedding backend that goes over HTTP — an OpenAI-compatible
//! `/v1/embeddings`.
//!
//! # Why this is the default rather than in-process inference
//!
//! Before this, `fastembed` (ONNX Runtime inside) was a **hard dependency** of
//! `cortex-memory`, with no feature gate. The cost was more than just
//! "heavy":
//!
//! 1. **Intel Macs simply cannot build it** — `ort-sys: no prebuilt binaries
//!    available for target x86_64-apple-darwin`. It took two rounds during the
//!    v0.1.0 release to confirm that this is not "no prebuilt binary handy"
//!    but "cannot be built", and not even `CORTEX_EMBED_BACKEND=hash` saves
//!    you, because ort-sys fails at **build time**
//! 2. **Every machine running cortexd has to be able to fetch those 590 MB.**
//!    The first time this went to production, Hugging Face was completely
//!    unreachable from that node (DNS poisoned to Meta's ranges,
//!    `hf-mirror.com` unreachable too), so cortexd **started up as usual and
//!    returned ok from `/health`**, leaving a single line in the log saying it
//!    had "fallen back to the mock data source" — memory retrieval was fake,
//!    with no red light
//! 3. 12 seconds of startup loading, 1.0 GiB resident
//!
//! Switching to HTTP makes all three disappear at once, and **one protocol
//! covers both deployment shapes**: point `CORTEX_EMBED_ENDPOINT` at the
//! self-hosted container in compose, or at any OpenAI-compatible cloud
//! service — switching is a one-line environment variable change.
//!
//! # A dimension mismatch has to blow up on the spot
//!
//! The schema hardcodes the vector column as `VECTOR(1024)`. When you wire up
//! the wrong vendor (OpenAI's `text-embedding-3-small` is 1536), the worst
//! outcome is not an error, it is **the write being rejected by the database
//! while the caller treats it as an ordinary write failure** — or worse: some
//! services truncate. So we validate the dimension on the **very first
//! response** and give an error that names names, instead of handing the
//! problem off to Postgres.

use std::time::Duration;

use cortex_core::{CortexError, Result};
use serde::{Deserialize, Serialize};

use crate::embed::{EMBEDDING_DIM, Embedder, normalize};

/// How many texts at most go into a single request.
///
/// This is not performance tuning, it is a **compatibility floor**: vendors
/// differ wildly in the batch size they allow (TEI defaults to 32, Alibaba
/// Bailian's `text-embedding-v3` is 10). Picking 10 lets the default
/// configuration work against every service we know of; a self-hosted service
/// that wants more throughput can raise `CORTEX_EMBED_BATCH`.
const DEFAULT_BATCH: usize = 10;

/// Timeout for a single HTTP request.
///
/// It is 60 seconds because of **cold start**: a self-hosted TEI container may
/// still be loading weights when it receives its first request. Setting the
/// usual 30 seconds would make "the first minute after the service comes up"
/// present as a string of timeout failures — which is exactly the moment
/// someone is watching.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Number of retries after a failure (not counting the first attempt).
///
/// Only errors that **may be transient** are retried: connection failure,
/// timeout, 5xx, 429. A 4xx (wrong model name, failed auth, dimension
/// mismatch) gives the same result however many times you retry it; retrying
/// only delays one clear error message by a factor of three before anyone sees
/// it.
const MAX_RETRIES: usize = 2;

/// Construction parameters for [`ApiEmbedder`].
#[derive(Debug, Clone)]
pub struct ApiEmbedderConfig {
    /// The full request address, or a base address it can be derived from.
    /// See [`resolve_url`].
    pub endpoint: String,
    /// Model name. Self-hosted TEI ignores this field; cloud services require
    /// it.
    pub model: String,
    /// `Authorization: Bearer <key>`. Self-hosted services usually do not need
    /// it; leaving it empty means the header is not sent.
    pub api_key: Option<String>,
    /// How many texts at most go into a single request.
    pub batch: usize,
}

impl ApiEmbedderConfig {
    /// Reads from environment variables.
    ///
    /// | Variable | Meaning | Default |
    /// |---|---|---|
    /// | `CORTEX_EMBED_ENDPOINT` | service address | `http://embeddings/v1/embeddings` |
    /// | `CORTEX_EMBED_MODEL` | model name | `bge-m3` |
    /// | `CORTEX_EMBED_API_KEY` | auth (optional when self-hosted) | none |
    /// | `CORTEX_EMBED_BATCH` | items per request | 10 |
    ///
    /// The default address points at the container name of the `embeddings`
    /// service in compose — the "works with zero configuration" path is
    /// self-hosting, not any particular cloud.
    ///
    /// # Errors
    ///
    /// Errors when `CORTEX_EMBED_BATCH` is not a positive integer.
    pub fn from_env() -> Result<Self> {
        // An empty string is treated as "not set": `${VAR:-}` in compose sets
        // a variable that is absent from .env to the empty string rather than
        // leaving it unset, and a plain `env::var` then gets `Ok("")` and
        // overrides the default. This trap is documented in full in the
        // `non_empty` comment in cortex-core
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

/// Completes the address the user filled in into the URL we actually POST to.
///
/// It exists because vendor docs hand out "base addresses" in different
/// shapes, and making users guess whether to append `/v1/embeddings`
/// themselves means **half of them get it wrong**, with a 404 as the symptom
/// — an error that looks like "the service is not up".
///
/// All three shapes are accepted:
///
/// | Filled in | Actually requested |
/// |---|---|
/// | `http://embeddings/v1/embeddings` | as-is |
/// | `https://dashscope.aliyuncs.com/compatible-mode/v1` | `…/v1/embeddings` |
/// | `http://embeddings` | `http://embeddings/v1/embeddings` |
/// # Only the **path** may be inspected, never the whole string
///
/// The first version wrote `base.ends_with("/embeddings")`, which returns true
/// for `http://embeddings` — that slash inside `//embeddings` is enough. And
/// `embeddings` happens to be the container name of that service in compose,
/// so the **default configuration** would POST to `http://embeddings`
/// unchanged and get a 404, with a symptom that looks like "the service is not
/// up". Hence we strip the authority off before deciding.
#[must_use]
pub fn resolve_url(raw: &str) -> String {
    let base = raw.trim().trim_end_matches('/');

    // Find the first '/' after the authority; only what follows it is the
    // path. When there is no scheme (`embeddings:80`) it degrades the same way
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
    /// Ask for float explicitly. Without it some services default to base64
    /// (OpenAI's SDK requests it that way), while our deserialization only
    /// accepts an array — that would present as an incomprehensible parse
    /// error
    encoding_format: &'static str,
}

#[derive(Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedItem>,
}

#[derive(Deserialize)]
struct EmbedItem {
    /// Which item of the request this is. **We must sort by it**: the protocol
    /// makes no guarantee that the response order matches the request order,
    /// and a misalignment does not error — it presents as "retrieval quality
    /// is inexplicably bad", the hardest kind of bug of this family to spot
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

/// The embedder that calls a remote `/v1/embeddings`.
pub struct ApiEmbedder {
    http: reqwest::Client,
    url: String,
    cfg: ApiEmbedderConfig,
    model_id: String,
}

impl std::fmt::Debug for ApiEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written rather than derived: ApiEmbedderConfig holds an
        // api_key, and a derived Debug would print the secret into any `{:?}`
        // log line
        f.debug_struct("ApiEmbedder")
            .field("url", &self.url)
            .field("model", &self.cfg.model)
            .field("api_key", &self.cfg.api_key.as_ref().map(|_| "<已设置>"))
            .field("batch", &self.cfg.batch)
            .finish()
    }
}

impl ApiEmbedder {
    /// Constructs from configuration. Deliberately does **not** health-check
    /// here: a health check means sending a real request, and the remote may
    /// not be up yet when cortexd starts (compose brings both services up at
    /// once). The error message from the first failing embed already points at
    /// the problem well enough.
    ///
    /// # Errors
    ///
    /// The HTTP client fails to build (essentially only possible when the TLS
    /// backend is broken).
    pub fn new(cfg: ApiEmbedderConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| CortexError::Memory(format!("构造 HTTP 客户端失败：{e}")))?;
        let url = resolve_url(&cfg.endpoint);
        // The identifier written into the embedding_model column of every
        // table. The `api:` prefix exists so that "same model name, run
        // locally vs. run remotely" stays distinguishable in the database —
        // the two sides' normalization and pooling implementations are not
        // necessarily bit-for-bit identical, and mixing them into one table is
        // a silent quality loss
        let model_id = format!("api:{}", cfg.model);
        Ok(Self {
            http,
            url,
            cfg,
            model_id,
        })
    }

    /// Constructs from environment variables.
    ///
    /// # Errors
    ///
    /// Invalid configuration.
    pub fn from_env() -> Result<Self> {
        Self::new(ApiEmbedderConfig::from_env()?)
    }

    /// Constructs an embedder for the **previous model** from
    /// `CORTEX_EMBED_LEGACY_*`. `None` when it is not configured.
    ///
    /// # What it solves
    ///
    /// During backfill, the vectors of old facts still live only in the old
    /// space. For them to still be retrievable, the **query** has to be
    /// encoded into that old space too — and that requires still having the
    /// old model itself on hand. This is the precondition for step 3 of
    /// memory.md §7, "during retrieval, group by model and recall each
    /// separately".
    ///
    /// # ⚠️ It does not save the "in-process fastembed → remote API" migration
    ///
    /// In that migration the old model is fastembed, which by default is no
    /// longer compiled into the binary (feature `local-embed`). Which means
    /// the docker image does not contain that old model at all, and setting
    /// these variables cannot construct it.
    ///
    /// This path covers the **future api → api model switch** (changing cloud
    /// vendor / changing model name). That earlier migration can only ride the
    /// window out on the backfiller, with BM25 / graph / recency covering the
    /// other three routes in the meantime.
    ///
    /// # Errors
    ///
    /// The endpoint is configured but another value is invalid.
    pub fn legacy_from_env() -> Result<Option<Self>> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());

        let Some(endpoint) = var("CORTEX_EMBED_LEGACY_ENDPOINT") else {
            return Ok(None);
        };
        // The model name is mandatory: the old space's `model_id` goes into
        // the query's filter condition, and guessing it wrong means querying
        // an empty set — which behaves exactly like "not configured" and
        // cannot be told apart. Better to error on the spot
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

    /// Sends one batch (already chunked, length <= batch).
    async fn embed_chunk(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let body = EmbedRequest {
            model: &self.cfg.model,
            input: texts,
            encoding_format: "float",
        };

        let mut last: Option<CortexError> = None;
        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                // Linear backoff is enough: what is being retried here is "the
                // service just came up" and "a transient 429", not an
                // avalanche scenario
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
                        // Throw a 4xx straight through: wrong model name /
                        // failed auth / wrong request shape. Retrying three
                        // times only delays the same error by a factor of
                        // three before anyone sees it
                        return Err(err);
                    }
                    last = Some(err);
                }
            }
        }
        Err(last.unwrap_or_else(|| CortexError::Memory("embedding 请求失败".into())))
    }

    /// Validates and sorts back into `index` order.
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
        // A uniform normalize as a backstop. **We cannot assume the remote has
        // already normalized**: TEI normalizes by default, but that is an
        // option it can turn off, and cloud services do not agree with each
        // other on this and rarely write it down in their docs. For an
        // already-normalized vector this is the identity operation, at
        // negligible cost.
        //
        // Skipping it is not a matter of being "slightly off": `retrieval`'s
        // semantic floor (0.50) is calibrated against the cosine distance of
        // unit vectors, and once you feed it non-unit vectors that threshold
        // loses its meaning — presenting as recall being inexplicably wide or
        // inexplicably empty, with no error at all.
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
        // Self-hosted TEI; the docs hand you the full path already
        assert_eq!(
            resolve_url("http://embeddings/v1/embeddings"),
            "http://embeddings/v1/embeddings",
            "an address that is already a full path must not be appended to a second time"
        );
        // Cloud service docs usually give you "the base address up to /v1"
        assert_eq!(
            resolve_url("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "https://dashscope.aliyuncs.com/compatible-mode/v1/embeddings",
            "a base address ending in /v1 should only get /embeddings appended, never become /v1/v1/embeddings"
        );
        // Only the host was given. **This one is a regression test**: the
        // first version used `ends_with("/embeddings")` against the whole
        // string, and that slash inside `//embeddings` in `http://embeddings`
        // is enough to make it return true — and `embeddings` is exactly the
        // service name in compose, so the default configuration POSTed
        // straight into a 404
        assert_eq!(
            resolve_url("http://embeddings"),
            "http://embeddings/v1/embeddings",
            "the path must still be completed when the host is named embeddings —— the authority must not be mistaken for the path"
        );
        // The same trap, with a port
        assert_eq!(
            resolve_url("http://embeddings:80"),
            "http://embeddings:80/v1/embeddings",
            "completion must happen with a port too"
        );
        // A trailing slash is the most common slip of the finger
        assert_eq!(
            resolve_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/embeddings",
            "a trailing slash must not produce //"
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
        .expect("construction should not fail");
        let s = format!("{e:?}");
        assert!(
            !s.contains("sk-super-secret-value"),
            "the Debug output leaked the API key —— it would ride any {{:?}} log line into the log file: {s}"
        );
        assert!(
            s.contains("<已设置>"),
            "Debug should still reveal whether a key is configured, just not its value: {s}"
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
        let err = ApiEmbedder::collect(parsed, 1, "text-embedding-3-small").expect_err(
            "1536 dimensions must be rejected, they cannot go into a VECTOR(1024) column",
        );
        let msg = err.to_string();
        assert!(
            msg.contains("1536") && msg.contains("1024"),
            "the error message must give both the actual and the expected dimension, otherwise diagnosing it means digging through the schema: {msg}"
        );
        assert!(
            msg.contains("text-embedding-3-small"),
            "the error message must name which model it was: {msg}"
        );
    }

    #[test]
    fn collect_sorts_by_index() {
        // Deliberately return them out of order: the protocol guarantees no
        // ordering, and a misalignment is a silent quality loss. The two
        // vectors must point in different directions, otherwise there is no
        // telling which is which after normalizing — and a zero vector cannot
        // be used either: that is an input normalize **legitimately** rejects
        let mut a = vec![0.0f32; EMBEDDING_DIM];
        a[0] = 1.0; // the index=0 one
        let mut b = vec![0.0f32; EMBEDDING_DIM];
        b[1] = 1.0; // the index=1 one
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
        let got = ApiEmbedder::collect(parsed, 2, "bge-m3")
            .expect("both are 1024-dimensional, this should pass");
        assert_eq!(
            (got[0][0], got[0][1]),
            (1.0, 0.0),
            "the response order must be put back by index —— a misalignment does not error, it just makes retrieval inexplicably bad"
        );
        assert_eq!(
            (got[1][0], got[1][1]),
            (0.0, 1.0),
            "the second one should be the index=1 one"
        );
    }

    #[test]
    fn collect_normalizes_and_rejects_zero_vectors() {
        // The remote does not necessarily normalize (TEI does by default, but
        // that can be turned off; cloud vendors do not agree). The semantic
        // floor of 0.50 is calibrated against the cosine distance of unit
        // vectors, and once you feed it non-unit vectors that threshold loses
        // its meaning — and nothing errors
        let mut v = vec![0.0f32; EMBEDDING_DIM];
        v[0] = 3.0;
        v[1] = 4.0; // norm 5
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
        .expect("a legal vector should pass");
        let norm = got[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "the returned vector must be unit length, actual norm {norm}"
        );

        // A zero vector yields NaN under pgvector's cosine distance and
        // silently poisons the entire ORDER BY
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
        .expect_err(
            "a zero vector must be rejected —— in pgvector it turns into NaN and poisons the ordering",
        );
        assert!(
            err.to_string().contains("归一化"),
            "the error message must state the reason: {err}"
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
        let err = ApiEmbedder::collect(parsed, 3, "bge-m3").expect_err(
            "returning two fewer items must error, otherwise the caller gets a misaligned array",
        );
        assert!(
            err.to_string().contains('3'),
            "the error message must state how many items were requested: {err}"
        );
    }
}

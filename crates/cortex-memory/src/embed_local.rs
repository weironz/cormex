//! The embedding backend that does in-process ONNX inference —
//! **optional feature `local-embed`**.
//!
//! This entire file is **not compiled** in a default build. It was split out
//! of `embed.rs` precisely so that cfg is written exactly once: fastembed used
//! to be a hard dependency, and the consequence was more than just "heavy" —
//!
//!   1. On Intel Macs `ort-sys` has no prebuilt artifacts and fails at
//!      **build time**, and not even `CORTEX_EMBED_BACKEND=hash` saves you
//!   2. Every machine has to be able to fetch those 590 MB of weights; when it
//!      cannot, cortexd **starts up as usual and returns ok from `/health`**,
//!      leaving a single line in the log saying it "fell back to mock"
//!   3. 12 second cold start, 1.0 GiB resident
//!
//! The default backend became [`crate::embed_api`] (HTTP, OpenAI-compatible).
//! Turn this feature on only when you need "one process handles everything,
//! depending on no external service":
//!
//! ```bash
//! cargo build -p cortexd --features cortex-memory/local-embed
//! ```
//!
//! # Environment variables
//!
//! | Variable | Values | Default |
//! |---|---|---|
//! | `CORTEX_EMBED_MODEL` | see [`FastModel::parse`] | `bge-m3-int8` |
//! | `CORTEX_MODEL_CACHE` | model cache directory | `cortex/models` under the user cache directory |
//! | `CORTEX_EMBED_BATCH` | background backfill batch size | [`DEFAULT_BACKFILL_BATCH`] |

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use cortex_core::{CortexError, Result};
use fastembed::{
    Bgem3Embedding, Bgem3InitOptions, Bgem3Model, EmbeddingModel, TextEmbedding, TextInitOptions,
};

use crate::embed::{EMBEDDING_DIM, Embedder, normalize};

/// Batch size for background backfill.
///
/// Far smaller than fastembed's own 256: BGE-M3's ColBERT output is
/// `[batch, seq_len, 1024]` of f32, and the 256×512 tier alone needs more than
/// 500 MB of temporary tensors, which OOMs outright inside the 8 GB memory
/// budget drawn up in architecture.md.
pub const DEFAULT_BACKFILL_BATCH: usize = 32;

/// Sequence truncation length.
///
/// BGE-M3 itself supports 8192, but what memory stores is **a single fact
/// statement** (on the order of tens of characters); enlarging the window only
/// grows padding and attention cost linearly while recall does not move at
/// all.
pub const DEFAULT_MAX_LENGTH: usize = 512;

/// The available local models. **All three are 1024-dimensional** — that is
/// not a coincidence but a hard constraint: the migration hardcodes
/// `VECTOR(1024)`, and a model of a different dimension simply cannot be
/// stored.
///
/// All three support Chinese. The remaining 1024-dimensional models in
/// fastembed (`BGELargeENV15`, `MxbaiEmbedLargeV1`, `GTELargeENV15`…) are all
/// English-only and therefore unusable for this project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FastModel {
    /// The int8 quantized build of BAAI/bge-m3 (about 600 MB).
    ///
    /// The default. The deployment budget in architecture.md states plainly
    /// that "on an 8 GB machine int8 is mandatory"; fp32's 2.2 GB would make
    /// the minimum-spec VPS unable to run at all.
    #[default]
    BgeM3Int8,
    /// The original fp32 BAAI/bge-m3 (about 2.2 GB). Use it only when the
    /// quantization loss is unacceptable.
    BgeM3Fp32,
    /// intfloat/multilingual-e5-large.
    ///
    /// Kept as a control and an escape hatch: should bge-m3's Chinese
    /// performance fall short of expectations, switching to it requires no
    /// schema change (also 1024-dimensional) and can go straight through
    /// progressive backfill.
    MultilingualE5Large,
}

impl FastModel {
    /// The identifier written into the `embedding_model` column of every
    /// table.
    ///
    /// It carries the HF repository name rather than a short alias, because
    /// **the quantized build and the original are different weights** and the
    /// vectors they produce do not live in the same space. Progressive
    /// backfill tells old from new by this field, and short aliases cannot
    /// tell them apart.
    #[must_use]
    pub fn model_id(self) -> &'static str {
        match self {
            Self::BgeM3Int8 => "fastembed:gpahal/bge-m3-onnx-int8",
            Self::BgeM3Fp32 => "fastembed:BAAI/bge-m3",
            Self::MultilingualE5Large => "fastembed:Qdrant/multilingual-e5-large-onnx",
        }
    }

    /// Parses `CORTEX_EMBED_MODEL`.
    ///
    /// # Errors
    ///
    /// An unrecognized name errors instead of silently falling back to the
    /// default — a fallback would make people believe the model switch took
    /// effect while the database still records the old `embedding_model`, and
    /// that only surfaces weeks later in the recall quality.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "bge-m3-int8" | "bge-m3" => Ok(Self::BgeM3Int8),
            "bge-m3-fp32" => Ok(Self::BgeM3Fp32),
            "multilingual-e5-large" | "e5-large" => Ok(Self::MultilingualE5Large),
            other => Err(CortexError::Config(format!(
                "unknown embedding model {other:?}; choices are \
                 bge-m3-int8 / bge-m3-fp32 / multilingual-e5-large"
            ))),
        }
    }

    /// The dimension fastembed registers statically. Used to validate at
    /// startup without having to run inference once.
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

/// A loaded ONNX session.
///
/// The two variants are not there for compatibility; they exist because
/// fastembed split BGE-M3 across two APIs: the int8 build only lives in
/// `Bgem3Embedding` (joint dense+sparse+colbert output), and the
/// `TextEmbedding` route cannot get at the quantized weights. We only take
/// dense.
enum Session {
    Joint(Box<Bgem3Embedding>),
    Dense(Box<TextEmbedding>),
}

impl Session {
    /// Synchronous blocking inference. The caller **must** already be inside
    /// `spawn_blocking`.
    fn run(&mut self, texts: &[String], batch: usize) -> Result<Vec<Vec<f32>>> {
        let raw = match self {
            Self::Joint(m) => m.embed(texts, Some(batch)).map(|o| o.dense),
            Self::Dense(m) => m.embed(texts, Some(batch)),
        };
        raw.map_err(|e| CortexError::Memory(format!("ONNX inference failed: {e}")))
    }
}

/// Construction parameters for [`FastEmbedder`].
#[derive(Debug, Clone)]
pub struct FastEmbedderConfig {
    pub model: FastModel,
    /// Model cache directory. When `None`, derived via [`default_cache_dir`].
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
    /// Reads from environment variables.
    ///
    /// # Errors
    ///
    /// Errors when the model name or the batch size cannot be parsed.
    pub fn from_env() -> Result<Self> {
        // An empty string is treated as "not set". **This is not defensive
        // programming, it is a production incident that has already happened
        // twice**: `${VAR:-}` in compose sets a variable that is absent from
        // .env to the empty string rather than leaving it unset, and
        // `env::var` returns `Ok("")` for an empty string.
        //
        // Concretely here: `CORTEX_EMBED_BATCH: ${CORTEX_EMBED_BATCH:-}` makes
        // `"".parse::<usize>()` fail, and therefore **cortexd refuses to
        // start** — while the ops side never configured that variable at all.
        //
        // `non_empty` in `cortex-core` and `embed_api::from_env` each carry
        // the same handling; this is the third copy. Deliberately not factored
        // into a shared function: the one in `cortex-core` is private, and
        // opening a public API across crates for a three-line closure buys
        // coupling that costs more than the duplication it saves.
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
                CortexError::Config(format!(
                    "CORTEX_EMBED_BATCH is not a positive integer: {v:?}"
                ))
            })?;
            if cfg.backfill_batch == 0 {
                return Err(CortexError::Config(
                    "CORTEX_EMBED_BATCH must not be 0".into(),
                ));
            }
        }
        Ok(cfg)
    }

    /// Loads the model, blocking (the first time this downloads several
    /// hundred MB).
    fn load(self) -> Result<Session> {
        let cache = self.cache_dir.unwrap_or_else(default_cache_dir);
        let map = |e: fastembed::Error| {
            CortexError::Memory(format!("loading the embedding model failed: {e}"))
        };

        match self.model {
            FastModel::BgeM3Int8 => Bgem3Embedding::try_new(
                Bgem3InitOptions::new(Bgem3Model::BGEM3Q)
                    .with_cache_dir(cache)
                    .with_max_length(self.max_length)
                    // The download progress bar is a raw write to stdout and
                    // would scramble the CLI's SSE output; visibility for the
                    // first download is handled by the tracing event below
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
    .map_err(|e| CortexError::Memory(format!("loading the embedding model failed: {e}")))
}

/// The model cache directory.
///
/// fastembed's default is `./.fastembed_cache` under the CWD — which means
/// starting cortexd from a different directory downloads its own 600 MB copy,
/// and changing working directory makes "the model disappear". We put it
/// uniformly under the user-level cache directory, one copy shared
/// machine-wide.
#[must_use]
pub fn default_cache_dir() -> PathBuf {
    // Respect fastembed's own variable, so the cache can be shared with other
    // tools that use fastembed
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

/// The real semantic vectorizer.
///
/// # Why the whole model sits behind a single `Mutex`
///
/// fastembed's `embed` takes `&mut self` (the internal ONNX session reuses its
/// input buffers), while [`Embedder::embed`] only has `&self`. There were
/// three options:
///
/// 1. A new session per call — several hundred MB of loading each time,
///    absolutely not viable
/// 2. One model per concurrent path — even int8 is 600 MB × N, which does not
///    fit on an 8 GB machine
/// 3. **One model plus a mutex** — adopted
///
/// The mutex serializes concurrent embeds, but that is **what we wanted
/// anyway**: ONNX Runtime already parallelizes a single inference across all
/// cores, and layering concurrency on top only makes threads fight each other
/// for cores while total throughput drops.
///
/// The cost is that a backfill blocks interactive queries, and **the lock is
/// not released while fastembed chunks internally by `batch_size`** — a
/// single `embed` call holds it to the end no matter how many items were
/// passed in. So "let queries jump the line" cannot be solved at this layer;
/// it takes discipline one level up: a backfill job has to slice its several
/// thousand items into batches across multiple calls itself, leaving gaps for
/// queries.
pub struct FastEmbedder {
    session: Arc<Mutex<Session>>,
    model_id: &'static str,
    backfill_batch: usize,
}

impl FastEmbedder {
    /// Loads according to environment variables.
    ///
    /// # Errors
    ///
    /// Invalid configuration, model download failure, ONNX session creation
    /// failure.
    pub async fn from_env() -> Result<Self> {
        Self::new(FastEmbedderConfig::from_env()?).await
    }

    /// Loads the model. **Construction is loading**, there is no lazy
    /// initialization.
    ///
    /// Laziness would defer tens of seconds of loading to the first query,
    /// presenting as "the first message hangs"; worse, a missing or corrupted
    /// model would only surface then rather than at startup.
    ///
    /// # Errors
    ///
    /// The model fails to download, or the ONNX session fails to be created.
    pub async fn new(config: FastEmbedderConfig) -> Result<Self> {
        let model_id = config.model.model_id();
        let backfill_batch = config.backfill_batch;
        let declared = config.model.declared_dim();

        // On a dimension mismatch, **fail before loading**: loading takes tens
        // of seconds, and there is no point waiting them out for a model that
        // is destined never to fit into VECTOR(1024)
        if declared != EMBEDDING_DIM {
            return Err(CortexError::Config(format!(
                "model {model_id} is {declared}-dimensional, but the schema hard-codes \
                 VECTOR({EMBEDDING_DIM}); changing the dimension needs a matching migration \
                 (see memory.md §7)"
            )));
        }

        tracing::info!(
            model = model_id,
            "loading the embedding model (the first time this downloads a few hundred MB)"
        );
        let started = std::time::Instant::now();

        // Loading is pure blocking disk IO plus ONNX graph construction, tens
        // of seconds at minimum. Running it bare inside an async context would
        // stop the entire tokio runtime from scheduling any task for that
        // whole time.
        let session = tokio::task::spawn_blocking(move || config.load())
            .await
            .map_err(|e| {
                CortexError::Memory(format!(
                    "the blocking task that loads the model panicked: {e}"
                ))
            })??;

        tracing::info!(
            model = model_id,
            elapsed_ms = started.elapsed().as_millis(),
            "the embedding model is ready"
        );

        Ok(Self {
            session: Arc::new(Mutex::new(session)),
            model_id,
            backfill_batch,
        })
    }

    /// Inference with an explicit batch size.
    ///
    /// The interactive path and the backfill path make opposite trade-offs: a
    /// query is a single item and wants minimum latency; a backfill has tens
    /// of thousands of items and wants throughput. ONNX pads the whole batch
    /// to the longest sequence in it, so stuffing one short query into a large
    /// batch is actually slower — the two paths have to be kept separate.
    ///
    /// # The cost of separating them: about 0.013 of cosine drift
    ///
    /// int8 GEMM takes different kernels as the batch dimension changes
    /// (different tiling and accumulation order); measured, the same text at
    /// batch=1 versus batch≥2 differs by 0.012–0.015, independent of the
    /// length of the other texts in the batch, while repeated calls within the
    /// same batch shape are bit-for-bit identical. Which is to say: **there is
    /// a fixed small offset between the vectors backfill writes into the
    /// database and the query vectors**.
    ///
    /// The reason we do not drop backfill to batch=1 over this: the semantic
    /// signal is 30 times larger (measured, synonymous 0.81 / unrelated 0.38),
    /// while the throughput gain of batch=32 is several-fold. See
    /// `MAX_BATCH_DRIFT` in `tests/fastembed.rs`.
    ///
    /// # Errors
    ///
    /// Inference fails, or a produced vector has an invalid dimension or norm.
    pub async fn embed_with_batch(
        &self,
        texts: &[String],
        batch_size: usize,
    ) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        if batch_size == 0 {
            return Err(CortexError::Invalid("batch_size must not be 0".into()));
        }

        let session = Arc::clone(&self.session);
        let owned = texts.to_vec();

        let mut vectors = tokio::task::spawn_blocking(move || {
            // A poisoned lock means the previous inference panicked. The ONNX
            // session's internal state is still self-consistent after `run`
            // fails, and permanently killing the embedder over one transient
            // panic would take the entire memory write path down with it — a
            // cost far larger than the benefit. So recover it.
            let mut guard = session.lock().unwrap_or_else(PoisonError::into_inner);
            guard.run(&owned, batch_size)
        })
        .await
        .map_err(|e| CortexError::Memory(format!("the embedding task panicked: {e}")))??;

        if vectors.len() != texts.len() {
            return Err(CortexError::Memory(format!(
                "the embedder returned {} vectors, but {} texts went in",
                vectors.len(),
                texts.len()
            )));
        }

        for (i, v) in vectors.iter_mut().enumerate() {
            normalize(v).map_err(|e| CortexError::Memory(format!("text {i}: {e}")))?;
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
        // A single item means the interactive path; batch=1 wastes nothing on
        // padding
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
        // This assertion is the migration's guardrail: add a model that is not
        // 1024-dimensional to FastModel and it blows up right here, rather
        // than waiting for Postgres to reject the INSERT at runtime
        for m in [
            FastModel::BgeM3Int8,
            FastModel::BgeM3Fp32,
            FastModel::MultilingualE5Large,
        ] {
            assert_eq!(
                m.declared_dim(),
                EMBEDDING_DIM,
                "{} is {}-dimensional and cannot be written into VECTOR({EMBEDDING_DIM})",
                m.model_id(),
                m.declared_dim()
            );
        }
    }

    #[test]
    fn model_ids_are_distinct() {
        // The embedding_model column is progressive backfill's only criterion;
        // a name collision means old and new vectors having their distances
        // computed inside one coordinate system — a meaningless result that
        // nobody notices.
        //
        // They must also differ across backends: bge-m3 run remotely
        // (`api:bge-m3`) and the local int8
        // (`fastembed:gpahal/bge-m3-onnx-int8`) may be "the same model", but
        // the quantization and pooling implementations are not bit-for-bit
        // identical, and mixing them into one table is a silent quality loss
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
            "every model_id must be pairwise distinct, actual {ids:?}"
        );
    }

    #[test]
    fn unknown_names_are_rejected_not_defaulted() {
        assert!(
            FastModel::parse("bge-m4").is_err(),
            "an unrecognizable model name must error; silently falling back to the default model makes people believe the model switch took effect"
        );
        assert_eq!(
            FastModel::parse("BGE-M3-Int8").unwrap(),
            FastModel::BgeM3Int8
        );
    }

    #[test]
    fn cache_dir_is_not_cwd_relative() {
        // fastembed's default ./.fastembed_cache drifts with the startup
        // directory, presenting as "run it from another directory and it
        // re-downloads 600 MB"
        let dir = default_cache_dir();
        assert!(
            dir.is_absolute() || std::env::var_os("FASTEMBED_CACHE_DIR").is_some(),
            "the default cache directory should be an absolute path, actual {dir:?}"
        );
    }
}

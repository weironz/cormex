//! Vectorization — the [`Embedder`] abstraction and backend selection.
//!
//! Three backends, each implemented in exactly one place:
//!
//! | Backend | Where | When to use |
//! |---|---|---|
//! | `api` (**default**) | [`crate::embed_api`] | Calls a remote `/v1/embeddings` (self-hosted container or cloud service) |
//! | `hash` | this file | Offline development and CI. **Not a semantic space** |
//! | `fast` | [`crate::embed_local`], feature `local-embed` | One process handles everything, no external service |
//!
//! # Why the default moved from in-process inference to HTTP
//!
//! Before this, `fastembed` (ONNX Runtime inside) was a hard dependency. The
//! cost was more than just "heavy": on Intel Macs `ort-sys` has no prebuilt
//! artifacts and fails at **build time**; every machine has to be able to
//! fetch those 590 MB of weights, and when Hugging Face was completely
//! unreachable from the production node, cortexd would **start up as usual and
//! return ok from `/health`**, leaving a single line in the log saying it had
//! "fallen back to mock" — memory retrieval was fake, with no red light at
//! all. See the module comment of [`crate::embed_api`] for the full story.
//!
//! The argument "memory content must not leave the network" is **only half
//! true**, and that has to be stated clearly: extraction already sends the
//! conversation to an LLM vendor. What additionally leaves the network once we
//! go through the API is **every fact, every episode and every retrieval
//! query**. A deployment that genuinely cares about this just points
//! `CORTEX_EMBED_ENDPOINT` at the container on its own network — the protocol
//! is the same one.
//!
//! # The dimension is part of the schema
//!
//! The migration hardcodes `VECTOR(1024)`. Switching models means either
//! matching that dimension, or adding a migration with a new column
//! (see memory.md §7 "switching models without downtime"). Every vector table
//! records `embedding_model`, precisely so that progressive backfill is
//! possible.
//!
//! # Environment variables
//!
//! | Variable | Values | Default |
//! |---|---|---|
//! | `CORTEX_EMBED_BACKEND` | `api` \| `hash` \| `fast` | `api` |
//! | `CORTEX_EMBED_ENDPOINT` | service address | `http://embeddings/v1/embeddings` |
//! | `CORTEX_EMBED_MODEL` | model name | `bge-m3` |
//! | `CORTEX_EMBED_API_KEY` | auth, optional when self-hosted | none |
//! | `CORTEX_EMBED_BATCH` | items per request / backfill batch size | 10 |

use std::sync::Arc;

use cortex_core::{CortexError, Result};

/// The vector dimension hardcoded in the schema. Changing it requires a
/// matching migration.
pub const EMBEDDING_DIM: usize = 1024;

/// A vectorization backend.
///
/// Making this a trait buys two concrete things: swapping in a deterministic
/// fake for tests (no model download needed), and having the old and new model
/// coexist for progressive backfill when the model is eventually switched.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync {
    /// Model identifier, written into the `embedding_model` column of every
    /// table.
    fn model_id(&self) -> &str;

    fn dim(&self) -> usize {
        EMBEDDING_DIM
    }

    /// Batch vectorization. The interactive path uses batch=1, background
    /// backfill uses a large batch.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// Single-item convenience method.
    async fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let mut v = self.embed(&[text.to_string()]).await?;
        v.pop()
            .ok_or_else(|| CortexError::Memory("the embedder returned nothing".into()))
    }
}

/// A deterministic hash vectorizer — **for tests and offline development
/// only**.
///
/// It has no semantic ability whatsoever (synonyms do not end up close), but
/// it needs no model download, no network, and its output is stable enough to
/// assert on, which lets the storage layer and the retrieval pipeline be
/// tested without depending on a real model.
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

/// Hashes text into a unit-length vector. Identical input must produce
/// identical output.
fn hash_vector(text: &str, dim: usize) -> Vec<f32> {
    use std::hash::{Hash, Hasher};

    let mut v = vec![0f32; dim];
    // Hash by character n-gram, so that similar strings also get somewhat
    // correlated vectors
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

// ───────────────────────── Real semantic models ─────────────────────────

/// Normalizes to unit length in place.
///
/// Whether a model has already normalized depends on the graph structure it
/// was exported with (the int8 and fp32 versions do not agree). Rather than
/// researching this model by model, we backstop it uniformly here — for an
/// already-normalized vector this is the identity operation.
pub(crate) fn normalize(v: &mut [f32]) -> std::result::Result<(), String> {
    if v.len() != EMBEDDING_DIM {
        return Err(format!(
            "dimension {} does not match the schema's VECTOR({EMBEDDING_DIM})",
            v.len()
        ));
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    // A zero vector yields NaN under pgvector's cosine distance and silently
    // poisons the entire ORDER BY, which is far harder to track down than an
    // error raised right here
    if !norm.is_finite() || norm <= f32::EPSILON {
        return Err(format!("norm {norm} is not usable, cannot normalize"));
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
    Ok(())
}

// ────────────────────────── Backend selection ──────────────────────────

/// Makes it easy to switch backends from configuration.
pub type SharedEmbedder = Arc<dyn Embedder>;

/// Which backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    /// Calls a remote `/v1/embeddings`. **Default.**
    #[default]
    Api,
    /// Deterministic hash stub, for tests and offline development only.
    Hash,
    /// In-process ONNX inference. Requires feature `local-embed`.
    ///
    /// The variant itself does **not** disappear along with the feature: if it
    /// did, `CORTEX_EMBED_BACKEND=fast` without the feature would report
    /// "unknown value", turning "this backend was not compiled in" into "you
    /// made a typo". Keeping the variant lets [`build_embedder`] give the real
    /// reason.
    Fast,
}

impl Backend {
    /// Parses `CORTEX_EMBED_BACKEND`.
    ///
    /// **The default is `api`.** `hash` looks "safer" (no network, no
    /// dependency on any service), but it would make semantic recall fail
    /// silently whenever nobody configured anything: retrieval still returns
    /// results, nothing errors, the recall quality is just inexplicably
    /// terrible — a bug of that shape takes weeks to notice. Failing to reach
    /// the embedding service, by contrast, is **loud**: the very first memory
    /// write errors out.
    ///
    /// CI and offline development set `CORTEX_EMBED_BACKEND=hash` explicitly.
    ///
    /// # Errors
    ///
    /// Errors on an unrecognized value.
    pub fn from_env() -> Result<Self> {
        // An empty string is treated as "not set"; see `non_empty` in
        // cortex_core::config for the reason
        std::env::var("CORTEX_EMBED_BACKEND")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or(Ok(Self::Api), |v| Self::parse(&v))
    }

    /// Parses a backend name.
    ///
    /// # Errors
    ///
    /// Errors on an unrecognized value.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "api" | "http" | "remote" => Ok(Self::Api),
            "hash" | "stub" => Ok(Self::Hash),
            "fast" | "fastembed" | "local" => Ok(Self::Fast),
            other => Err(CortexError::Config(format!(
                "unknown CORTEX_EMBED_BACKEND={other:?}; choices are api / hash / fast"
            ))),
        }
    }
}

/// Builds a fresh embedder (does **not** reuse the global instance).
///
/// Progressive backfill across a model switch needs the old and the new
/// instance alive at the same time (memory.md §7), so this path has to stay;
/// everyday code should use [`shared_embedder`].
///
/// # Errors
///
/// Invalid configuration, or the model fails to load.
pub async fn build_embedder(backend: Backend) -> Result<SharedEmbedder> {
    match backend {
        Backend::Hash => Ok(Arc::new(HashEmbedder::new())),
        Backend::Api => Ok(Arc::new(crate::embed_api::ApiEmbedder::from_env()?)),

        #[cfg(feature = "local-embed")]
        Backend::Fast => Ok(Arc::new(
            crate::embed_local::FastEmbedder::from_env().await?,
        )),

        // When it was not compiled in, give the **actual reason** instead of
        // letting it surface as "unknown value" back in parse — the latter
        // turns "this binary does not carry this backend" into "you misspelled
        // the name", and the two call for completely different fixes
        #[cfg(not(feature = "local-embed"))]
        Backend::Fast => Err(CortexError::Config(
            "CORTEX_EMBED_BACKEND=fast needs the `local-embed` feature, and this binary was not \
             built with it.\n\
             Either switch to the default api backend (point CORTEX_EMBED_ENDPOINT at an \
             OpenAI-compatible /v1/embeddings service),\n\
             or rebuild with the feature: cargo build -p cortexd --features cortex-memory/local-embed"
                .into(),
        )),
    }
}

/// The process-wide shared embedder, backend chosen by environment variable.
///
/// A `OnceCell` rather than one instance per call site: even the int8 model is
/// 600 MB, and one of the reasons behind the "daemon, not embedded"
/// architectural decision is precisely that **the model stays resident and
/// shared** (architecture.md). Loading it repeatedly eats that benefit
/// outright.
///
/// # Errors
///
/// Invalid configuration or a failed model load on the first call; failures
/// are not cached, so the next call retries.
pub async fn shared_embedder() -> Result<SharedEmbedder> {
    static SHARED: tokio::sync::OnceCell<SharedEmbedder> = tokio::sync::OnceCell::const_new();
    SHARED
        .get_or_try_init(|| async { build_embedder(Backend::from_env()?).await })
        .await
        .cloned()
}

/// Makes `Arc<dyn Embedder>` itself an `Embedder`.
///
/// Without this, any generic structure holding an embedder (such as
/// `Retriever<E: Embedder>`) cannot be instantiated with a shared handle, and
/// callers are left writing `&*arc` everywhere or dropping the generic —
/// the former is ugly, the latter gives up static dispatch.
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
        assert_eq!(
            a, b,
            "identical input must give an identical vector, otherwise tests have nothing to assert on"
        );
    }

    #[tokio::test]
    async fn produces_schema_dimension() {
        let e = HashEmbedder::new();
        let v = e.embed_one("x").await.unwrap();
        assert_eq!(
            v.len(),
            EMBEDDING_DIM,
            "the dimension must match VECTOR(1024) in the migration"
        );
    }

    #[tokio::test]
    async fn vectors_are_unit_length() {
        let e = HashEmbedder::new();
        let v = e.embed_one("一些内容").await.unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "cosine distance requires unit vectors, actual norm {norm}"
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
            "a zero vector must error: pgvector's cosine distance yields NaN and silently poisons the ordering"
        );

        let mut v = vec![2f32; EMBEDDING_DIM];
        normalize(&mut v).unwrap();
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "the norm after normalizing should be 1, actual {norm}"
        );
    }

    #[test]
    fn normalize_rejects_wrong_dimension() {
        let mut v = vec![1f32; 768];
        assert!(
            normalize(&mut v).is_err(),
            "anything other than 1024 dimensions must be caught before the write, otherwise the error surfaces far away in the form of a Postgres type error"
        );
    }

    #[tokio::test]
    async fn hash_backend_needs_no_model() {
        // This also doubles as CI's availability guarantee:
        // CORTEX_EMBED_BACKEND=hash must work on a machine with no network and
        // no model cache
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
            "a misspelled backend name must error; falling back silently defeats the intent of \"CI uses hash\""
        );
        assert_eq!(
            Backend::default(),
            Backend::Api,
            "the default must be the real semantic backend: defaulting to hash makes recall fail silently whenever nobody configured anything"
        );
    }
}

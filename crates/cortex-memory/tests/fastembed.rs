//! `FastEmbedder` verified against the real model.
//!
//! Everything is marked `#[ignore]`: one run downloads hundreds of MB of
//! weights, and neither CI nor offline development should be blocked on that.
//! Run manually:
//!
//! ```bash
//! cargo test -p cortex-memory --test fastembed -- --ignored --nocapture
//! ```
//!
//! # Why these assertions deserve to exist on their own
//!
//! `HashEmbedder` is a character-bigram hash, so the cosine between
//! 「对象存储」 and 「blob 存储」 is close to 0 — the vector retrieval path might
//! as well not be there. `semantics_beat_hashing` below runs both backends over
//! the same inputs, turning "is swapping the model actually worth anything" into
//! a number you can assert on, rather than a claim that it "should be better".

use cortex_memory::EMBEDDING_DIM;
use cortex_memory::embed::{Embedder, HashEmbedder};
use cortex_memory::embed_local::{FastEmbedder, FastEmbedderConfig};

/// For unit vectors, cosine similarity is just the dot product.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

async fn load() -> FastEmbedder {
    FastEmbedder::new(FastEmbedderConfig::default())
        .await
        .expect("failed to load BGE-M3 int8; the first run needs access to huggingface.co")
}

#[tokio::test]
#[ignore = "needs the model weights downloaded"]
async fn produces_schema_dimension_and_unit_length() {
    let e = load().await;
    let v = e.embed_one("对象存储用 RustFS").await.unwrap();

    assert_eq!(
        v.len(),
        EMBEDDING_DIM,
        "the dimension must match the migration's VECTOR({EMBEDDING_DIM}); got {}",
        v.len()
    );
    assert_eq!(
        e.dim(),
        EMBEDDING_DIM,
        "dim() must agree with what is really produced"
    );

    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-4,
        "pgvector's cosine distance requires unit vectors; the norm is {norm}"
    );
}

#[tokio::test]
#[ignore = "needs the model weights downloaded"]
async fn is_deterministic() {
    let e = load().await;
    // Two calls on the same text must be bit-identical, otherwise "the same
    // sentence stored twice produces two different vectors", and neither
    // deduplication nor idempotent backfill means anything any more
    let a = e.embed_one("对象存储用 RustFS").await.unwrap();
    let b = e.embed_one("对象存储用 RustFS").await.unwrap();
    assert_eq!(a, b, "identical input must yield bit-identical vectors");
}

/// The numeric drift tolerated between batch=1 (the interactive path) and
/// batch=N (the backfill path).
///
/// **It is not 0.** int8 GEMM switches kernel path when the batch dimension
/// changes (different tiling and accumulation order); measured drift sits
/// steadily at 0.012–0.015 and is independent of the length of the other texts
/// in the batch (same-length partner 0.0146, very long partner 0.0145, short
/// partner 0.0121), while repeating the same batch is exactly identical (-1e-7)
/// — which says this is a fixed bias of the quantized operator, not padding
/// leakage.
///
/// Why that is acceptable: the semantic signal is far larger in magnitude.
/// Measured, synonymous sentences score 0.81 and unrelated ones 0.38, a gap of
/// 0.43 — thirty times this drift.
/// If the drift ever climbs above 0.05, the model or the ORT version has changed
/// kernels and this needs re-evaluating.
const MAX_BATCH_DRIFT: f32 = 0.03;

#[tokio::test]
#[ignore = "needs the model weights downloaded"]
async fn batch_stays_comparable_with_single() {
    let e = load().await;
    // Backfill runs large batches, queries run batch=1, and the vectors the two
    // produce must land in comparable positions — otherwise the "query vector"
    // and the "vectors in the store" are not even in the same coordinate system
    let texts: Vec<String> = vec!["对象存储用 RustFS".into(), "今天天气不错".into()];
    let batch = e.embed(&texts).await.unwrap();
    let single = e.embed_one(&texts[0]).await.unwrap();

    assert_eq!(
        batch.len(),
        2,
        "the batch must return as many results as it had inputs"
    );
    let drift = 1.0 - cosine(&batch[0], &single);
    println!("batch/single drift: {drift:.6}");
    assert!(
        drift < MAX_BATCH_DRIFT,
        "the cosine gap between batch and single, {drift:.6}, exceeds the tolerance \
         {MAX_BATCH_DRIFT}; exceeding it means the kernel path changed and backfilled \
         vectors are no longer comparable with query vectors"
    );
}

#[tokio::test]
#[ignore = "needs the model weights downloaded"]
async fn same_batch_is_bit_stable() {
    let e = load().await;
    // The previous test tolerates drift; this one draws its boundary:
    // **the same batch composition must be identical from run to run**. If this
    // one drifts too, it is not a fixed kernel bias but genuine randomness, and
    // deduplication and idempotent backfill both stop working
    let texts: Vec<String> = vec!["对象存储用 RustFS".into(), "今天天气不错".into()];
    let a = e.embed(&texts).await.unwrap();
    let b = e.embed(&texts).await.unwrap();
    assert_eq!(
        a, b,
        "the same batch composition must produce bit-identical vectors"
    );
}

#[tokio::test]
#[ignore = "needs the model weights downloaded"]
async fn model_id_identifies_weights() {
    let e = load().await;
    assert_eq!(
        e.model_id(),
        "fastembed:gpahal/bge-m3-onnx-int8",
        "model_id lands in every embedding_model column — the sole criterion for backfill"
    );
}

#[tokio::test]
#[ignore = "needs the model weights downloaded"]
async fn semantics_beat_hashing() {
    let fast = load().await;
    let hash = HashEmbedder::new();

    // (left, right, should they be similar)
    let pairs = [
        ("对象存储用 RustFS", "blob 存储选了 RustFS", true),
        ("对象存储", "object storage", true),
        ("对象存储用 RustFS", "今天天气不错", false),
    ];

    println!(
        "\n{:<24} {:<24} {:>10} {:>10}",
        "left", "right", "fast", "hash"
    );
    let mut scores = Vec::new();
    for (l, r, want_similar) in pairs {
        let f = cosine(
            &fast.embed_one(l).await.unwrap(),
            &fast.embed_one(r).await.unwrap(),
        );
        let h = cosine(
            &hash.embed_one(l).await.unwrap(),
            &hash.embed_one(r).await.unwrap(),
        );
        println!("{l:<24} {r:<24} {f:>10.4} {h:>10.4}");
        scores.push((l, r, want_similar, f, h));
    }
    println!();

    for (l, r, want_similar, f, _) in &scores {
        if *want_similar {
            assert!(
                *f > 0.6,
                "「{l}」 and 「{r}」 say the same thing, so the cosine should be \
                 markedly high; got {f:.4}"
            );
        } else {
            assert!(
                *f < 0.5,
                "「{l}」 and 「{r}」 have nothing to do with each other, so the cosine \
                 should be low; got {f:.4}"
            );
        }
    }

    // The reason this whole task exists, turned into an assertion: the semantic
    // model must open a wider gap between "similar" and "unrelated" than the
    // hash stub does. If that does not hold, the replacement was not worth it
    let sim_min = scores
        .iter()
        .filter(|s| s.2)
        .map(|s| s.3)
        .fold(f32::INFINITY, f32::min);
    let unrel_max = scores
        .iter()
        .filter(|s| !s.2)
        .map(|s| s.3)
        .fold(f32::NEG_INFINITY, f32::max);
    let fast_margin = sim_min - unrel_max;

    let h_sim_min = scores
        .iter()
        .filter(|s| s.2)
        .map(|s| s.4)
        .fold(f32::INFINITY, f32::min);
    let h_unrel_max = scores
        .iter()
        .filter(|s| !s.2)
        .map(|s| s.4)
        .fold(f32::NEG_INFINITY, f32::max);
    let hash_margin = h_sim_min - h_unrel_max;

    println!("separation: fast={fast_margin:.4}  hash={hash_margin:.4}\n");
    assert!(
        fast_margin > hash_margin,
        "the semantic model's separation ({fast_margin:.4}) must beat the hash stub's \
         ({hash_margin:.4}), otherwise this replacement is pointless"
    );
}

//! Measures embedding load time, single-item latency and semantic
//! discrimination for real.
//!
//! The deployment docs need these numbers (the "minimum recommended
//! configuration" section of architecture.md), and they depend heavily on the
//! machine, so they can only be measured:
//!
//! ```bash
//! cargo run -p cortex-memory --example embed_bench --release
//! ```
//!
//! `--release` is mandatory: in debug the ndarray copies around ONNX Runtime
//! are not inlined, the latency comes out an order of magnitude higher, and
//! writing that into the deployment docs is simply misleading.
//!
//! # One measured run (Windows 11 / x86_64 desktop CPU / bge-m3 int8 / release)
//!
//! ```text
//! first download        ~590 MB, 3–4 minutes (bandwidth-dependent, happens once)
//! load (cache warm)     1.61 s
//! single embed          median 14.7 ms   p95 16.3 ms
//! batch of 64           363 ms → 5.7 ms each
//! ```
//!
//! Batching thins the per-item cost to 1/2.6 — this is the quantitative basis
//! for why "interactive batch=1, backfill large batch" is worth splitting into
//! two paths.
//!
//! architecture.md estimates about 700 ms per item for a low-spec VPS
//! (2 vCPU); the numbers above are from a desktop CPU, the two differ by about
//! 50×, and **the deployment docs must not mix them**.
//!
//! # One trap on Windows
//!
//! `hf-hub` 0.5 runs `assert!(pointer_path.exists())` after downloading, and
//! on Windows it tries a symlink first and only falls back to rename on
//! failure. Without developer mode enabled that assertion panics (presenting
//! as "加载模型的阻塞任务 panic"), but **the file has in fact downloaded
//! completely**, and running it once more passes. Not a defect of this crate;
//! recorded here so nobody diagnoses it twice.

use std::time::Instant;

use cortex_memory::embed::{Embedder, HashEmbedder};
use cortex_memory::embed_local::{FastEmbedder, FastEmbedderConfig};

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = FastEmbedderConfig::default();
    println!("model: {}", cfg.model.model_id());
    println!(
        "cache dir: {:?}\n",
        cortex_memory::embed_local::default_cache_dir()
    );

    let t0 = Instant::now();
    let fast = FastEmbedder::new(cfg).await?;
    println!(
        "load time: {:?} (far more than this if it downloads first)",
        t0.elapsed()
    );

    // Warm-up: the first inference has to allocate ONNX's internal arena, and
    // counting it into the latency statistics inflates them
    let _ = fast.embed_one("warm-up").await?;

    let probe = "对象存储用 RustFS";
    let mut single = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let _ = fast.embed_one(probe).await?;
        single.push(t.elapsed());
    }
    single.sort_unstable();
    println!(
        "single embed (20 runs): median {:?}  p95 {:?}  fastest {:?}",
        single[10], single[19], single[0]
    );

    let batch: Vec<String> = (0..64)
        .map(|i| format!("第 {i} 条记忆：对象存储用 RustFS"))
        .collect();
    let t = Instant::now();
    let _ = fast.embed(&batch).await?;
    let elapsed = t.elapsed();
    println!(
        "batch of 64: {:?} ({:?} each)\n",
        elapsed,
        elapsed / batch.len() as u32
    );

    let hash = HashEmbedder::new();
    let pairs = [
        ("对象存储用 RustFS", "blob 存储选了 RustFS"),
        ("对象存储", "object storage"),
        ("对象存储用 RustFS", "今天天气不错"),
    ];

    println!(
        "{:<22} {:<24} {:>10} {:>10}",
        "left", "right", "fast", "hash"
    );
    println!("{}", "-".repeat(70));
    for (l, r) in pairs {
        let f = cosine(&fast.embed_one(l).await?, &fast.embed_one(r).await?);
        let h = cosine(&hash.embed_one(l).await?, &hash.embed_one(r).await?);
        println!("{l:<22} {r:<24} {f:>10.4} {h:>10.4}");
    }

    Ok(())
}

//! 实测 embedding 的加载耗时、单条延迟与语义区分度。
//!
//! 部署文档需要这几个数字（architecture.md 的「最低推荐配置」那一节），
//! 而它们高度依赖机器，只能实测：
//!
//! ```bash
//! cargo run -p cortex-memory --example embed_bench --release
//! ```
//!
//! 必须用 `--release`：debug 下 ONNX Runtime 周边的 ndarray 拷贝没有内联，
//! 测出来的延迟会高一个数量级，写进部署文档就是误导。
//!
//! # 一次实测（Windows 11 / x86_64 桌面 CPU / bge-m3 int8 / release）
//!
//! ```text
//! 首次下载        约 590 MB，3–4 分钟（带宽相关，只发生一次）
//! 加载（缓存已热） 1.61 s
//! 单条 embed      中位 14.7 ms   p95 16.3 ms
//! 批量 64 条      363 ms → 每条 5.7 ms
//! ```
//!
//! 批量把每条摊薄到 1/2.6 —— 这就是「交互 batch=1、回填大 batch」
//! 值得分成两条路的量化依据。
//!
//! architecture.md 给低配 VPS（2 vCPU）估的是单条约 700 ms；
//! 上面是桌面 CPU 的数字，两者相差约 50 倍，**部署文档不能混用**。
//!
//! # 一个 Windows 上的坑
//!
//! `hf-hub` 0.5 下载完会 `assert!(pointer_path.exists())`，而它在 Windows 上
//! 先试符号链接、失败才回退到 rename。未开开发者模式时这个断言会 panic
//! （表现为「加载模型的阻塞任务 panic」），但**文件其实已经下载完整**，
//! 再跑一次即可通过。不是本 crate 的缺陷，记在这里免得重复排查。

use std::time::Instant;

use cortex_memory::embed::{Embedder, HashEmbedder};
use cortex_memory::embed_local::{FastEmbedder, FastEmbedderConfig};

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = FastEmbedderConfig::default();
    println!("模型：{}", cfg.model.model_id());
    println!(
        "缓存目录：{:?}\n",
        cortex_memory::embed_local::default_cache_dir()
    );

    let t0 = Instant::now();
    let fast = FastEmbedder::new(cfg).await?;
    println!("加载耗时：{:?}（含首次下载则远不止）", t0.elapsed());

    // 预热：第一次推理要分配 ONNX 的内部 arena，算进延迟统计会虚高
    let _ = fast.embed_one("预热").await?;

    let probe = "对象存储用 RustFS";
    let mut single = Vec::new();
    for _ in 0..20 {
        let t = Instant::now();
        let _ = fast.embed_one(probe).await?;
        single.push(t.elapsed());
    }
    single.sort_unstable();
    println!(
        "单条 embed（20 次）：中位 {:?}  p95 {:?}  最快 {:?}",
        single[10], single[19], single[0]
    );

    let batch: Vec<String> = (0..64)
        .map(|i| format!("第 {i} 条记忆：对象存储用 RustFS"))
        .collect();
    let t = Instant::now();
    let _ = fast.embed(&batch).await?;
    let elapsed = t.elapsed();
    println!(
        "批量 64 条：{:?}（每条 {:?}）\n",
        elapsed,
        elapsed / batch.len() as u32
    );

    let hash = HashEmbedder::new();
    let pairs = [
        ("对象存储用 RustFS", "blob 存储选了 RustFS"),
        ("对象存储", "object storage"),
        ("对象存储用 RustFS", "今天天气不错"),
    ];

    println!("{:<22} {:<24} {:>10} {:>10}", "左", "右", "fast", "hash");
    println!("{}", "-".repeat(70));
    for (l, r) in pairs {
        let f = cosine(&fast.embed_one(l).await?, &fast.embed_one(r).await?);
        let h = cosine(&hash.embed_one(l).await?, &hash.embed_one(r).await?);
        println!("{l:<22} {r:<24} {f:>10.4} {h:>10.4}");
    }

    Ok(())
}

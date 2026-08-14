//! `FastEmbedder` 的真实模型验证。
//!
//! 全部标 `#[ignore]`：跑一次要下载数百 MB 权重，CI 与无网开发不该被它卡住。
//! 手动跑：
//!
//! ```bash
//! cargo test -p cortex-memory --test fastembed -- --ignored --nocapture
//! ```
//!
//! # 为什么这些断言值得单独存在
//!
//! `HashEmbedder` 是字符 bigram 散列，「对象存储」与「blob 存储」的余弦
//! 接近 0——向量召回那一路等于没有。下面的 `semantics_beat_hashing`
//! 用同一组输入同时跑两个后端，把「换模型是否真的有用」变成可断言的数字，
//! 而不是一句「应该更好」。

use cortex_memory::EMBEDDING_DIM;
use cortex_memory::embed::{Embedder, HashEmbedder};
use cortex_memory::embed_local::{FastEmbedder, FastEmbedderConfig};

/// 单位向量的余弦相似度就是点积。
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

async fn load() -> FastEmbedder {
    FastEmbedder::new(FastEmbedderConfig::default())
        .await
        .expect("加载 BGE-M3 int8 失败；首次运行需要能访问 huggingface.co")
}

#[tokio::test]
#[ignore = "需要下载模型权重"]
async fn produces_schema_dimension_and_unit_length() {
    let e = load().await;
    let v = e.embed_one("对象存储用 RustFS").await.unwrap();

    assert_eq!(
        v.len(),
        EMBEDDING_DIM,
        "维度必须与 migration 的 VECTOR({EMBEDDING_DIM}) 一致，实际 {}",
        v.len()
    );
    assert_eq!(e.dim(), EMBEDDING_DIM, "dim() 应与实际产出一致");

    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(
        (norm - 1.0).abs() < 1e-4,
        "pgvector 的 cosine 距离要求单位向量，实际模长 {norm}"
    );
}

#[tokio::test]
#[ignore = "需要下载模型权重"]
async fn is_deterministic() {
    let e = load().await;
    // 同一条文本两次调用必须逐位相同，否则「同一句话重复入库会产生两个不同
    // 向量」，去重与回填的幂等性都无从谈起
    let a = e.embed_one("对象存储用 RustFS").await.unwrap();
    let b = e.embed_one("对象存储用 RustFS").await.unwrap();
    assert_eq!(a, b, "相同输入必须得到逐位相同的向量");
}

/// batch=1（交互路径）与 batch=N（回填路径）之间可容忍的数值漂移。
///
/// **不是 0。** int8 GEMM 在 batch 维度变化时会换一条 kernel 路径
/// （不同的 tiling 与累加顺序），实测漂移稳定在 0.012–0.015，
/// 且与批内其它文本的长度无关（同长伙伴 0.0146、超长伙伴 0.0145、
/// 短伙伴 0.0121），重复同一 batch 则完全一致（-1e-7）——
/// 说明这是量化算子的固定偏差，不是 padding 泄漏。
///
/// 为什么可以接受：语义信号的量级远大于它。实测同义句 0.81、
/// 无关句 0.38，间距 0.43，是这个漂移的 30 倍。
/// 若哪天漂移涨到 0.05 以上，说明模型或 ORT 版本换了 kernel，需要重新评估。
const MAX_BATCH_DRIFT: f32 = 0.03;

#[tokio::test]
#[ignore = "需要下载模型权重"]
async fn batch_stays_comparable_with_single() {
    let e = load().await;
    // 回填走大 batch、查询走 batch=1，两者产出的向量必须落在可比的位置上，
    // 否则「查询向量」与「库里的向量」根本不在一个坐标系
    let texts: Vec<String> = vec!["对象存储用 RustFS".into(), "今天天气不错".into()];
    let batch = e.embed(&texts).await.unwrap();
    let single = e.embed_one(&texts[0]).await.unwrap();

    assert_eq!(batch.len(), 2, "批量结果条数必须与输入一致");
    let drift = 1.0 - cosine(&batch[0], &single);
    println!("batch/single 漂移：{drift:.6}");
    assert!(
        drift < MAX_BATCH_DRIFT,
        "batch 与单条的余弦差 {drift:.6} 超过容忍上限 {MAX_BATCH_DRIFT}；\
         超出说明 kernel 路径变了，回填向量与查询向量不再可比"
    );
}

#[tokio::test]
#[ignore = "需要下载模型权重"]
async fn same_batch_is_bit_stable() {
    let e = load().await;
    // 上一个测试容忍了漂移，这个测试划定它的边界：
    // **同样的 batch 组成必须逐次一致**。若这条也漂，那就不是 kernel 固定偏差
    // 而是真随机，去重与幂等回填全部失效
    let texts: Vec<String> = vec!["对象存储用 RustFS".into(), "今天天气不错".into()];
    let a = e.embed(&texts).await.unwrap();
    let b = e.embed(&texts).await.unwrap();
    assert_eq!(a, b, "同样的批次组成必须产出逐位相同的向量");
}

#[tokio::test]
#[ignore = "需要下载模型权重"]
async fn model_id_identifies_weights() {
    let e = load().await;
    assert_eq!(
        e.model_id(),
        "fastembed:gpahal/bge-m3-onnx-int8",
        "model_id 会写进各表的 embedding_model 列，是渐进回填的唯一判据"
    );
}

#[tokio::test]
#[ignore = "需要下载模型权重"]
async fn semantics_beat_hashing() {
    let fast = load().await;
    let hash = HashEmbedder::new();

    // (左, 右, 是否应该相似)
    let pairs = [
        ("对象存储用 RustFS", "blob 存储选了 RustFS", true),
        ("对象存储", "object storage", true),
        ("对象存储用 RustFS", "今天天气不错", false),
    ];

    println!("\n{:<24} {:<24} {:>10} {:>10}", "左", "右", "fast", "hash");
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
                "「{l}」与「{r}」是同义表述，余弦应显著高，实际 {f:.4}"
            );
        } else {
            assert!(*f < 0.5, "「{l}」与「{r}」毫无关系，余弦应低，实际 {f:.4}");
        }
    }

    // 本任务存在的理由，落成一条断言：语义模型必须把「相似」与「无关」
    // 拉开比哈希桩更大的间距。若这条不成立，这次替换就不值得
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

    println!("区分度：fast={fast_margin:.4}  hash={hash_margin:.4}\n");
    assert!(
        fast_margin > hash_margin,
        "语义模型的区分度（{fast_margin:.4}）必须优于哈希桩（{hash_margin:.4}），\
         否则这次替换没有意义"
    );
}

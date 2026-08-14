//! 转录冒烟测试：给一张图跑一遍完整的 [`TranscribePipeline`]，
//! 把最终会写进 `blob_transcripts` 的那一行原样打出来。
//!
//! 这是「不转成文本的媒体是检索黑洞」这条判断的验收现场 —— 看的不是
//! 「模型说了话」，而是**产出的文字里有没有足够多能被检索命中的信号**：
//! 图里的文字、可识别实体、结构类型。
//!
//! ```bash
//! # 本机 Ollama，不需要任何密钥（模型需先 ollama pull）
//! CORTEX_VISION_PROVIDER=ollama CORTEX_VISION_MODEL=gemma4:latest \
//!   cargo run -p cortex-memory --example vision_caption
//!
//! # 换一张图
//! ... --example vision_caption -- 路径/图.png
//! ```
//!
//! 没设 `CORTEX_VISION_PROVIDER` 时会退到 [`StubTranscriber`]，
//! 用来验证流水线本身（分词、向量化、抹除防护）在离线环境下也走得通。
//!
//! embedding 固定用 [`HashEmbedder`]：本例要验的是转录，
//! 不该顺带下载 600 MB 的 bge-m3。

use std::path::PathBuf;
use std::sync::Arc;

use cortex_memory::embed::HashEmbedder;
use cortex_memory::transcribe::{
    NeverRedacted, Outcome, StubTranscriber, TranscribePipeline, Transcriber, VisionTranscriber,
};

const DEFAULT_IMAGE: &str = "crates/cortex-memory/tests/fixtures/console_screenshot.png";

/// 一个假的 blob hash。真实链路里它来自 `blobs.hash`（内容的 SHA-256）。
const DEMO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000001";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("提示：未加载 .env（{e}），改用进程环境变量");
    }

    let path = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_IMAGE), PathBuf::from);
    let bytes =
        std::fs::read(&path).map_err(|e| anyhow::anyhow!("读不到图片 {}：{e}", path.display()))?;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        other => anyhow::bail!("不认识的扩展名 {other:?}"),
    };

    let transcriber: Arc<dyn Transcriber> = match VisionTranscriber::from_env()? {
        Some(v) => {
            println!("转录器：{}（真实 vision 模型）", v.model_id());
            Arc::new(v)
        }
        None => {
            println!("转录器：桩实现（未配置 CORTEX_VISION_PROVIDER）");
            Arc::new(StubTranscriber::new())
        }
    };

    let pipeline =
        TranscribePipeline::new(Arc::new(HashEmbedder::new())).with_transcriber(transcriber);

    println!("输入：{} （{} 字节，{mime}）", path.display(), bytes.len());
    println!("─────────────────────────────────────────");
    let started = std::time::Instant::now();
    let outcome = pipeline
        .run(DEMO_HASH, &bytes, mime, &NeverRedacted)
        .await?;
    let elapsed = started.elapsed();

    match &outcome {
        Outcome::Redacted => anyhow::bail!("NeverRedacted 不该报告已抹除"),
        Outcome::Unsupported { mime } => anyhow::bail!("没有转录器认领 {mime}"),
        Outcome::Ready(rows) => {
            anyhow::ensure!(!rows.is_empty(), "转录产出为空 —— 这份媒体仍然检索不到");
            for (i, row) in rows.iter().enumerate() {
                println!("段落 {}/{}", i + 1, rows.len());
                println!("  kind           {}", row.kind.as_str());
                println!("  transcribed_by {}", row.transcribed_by);
                println!(
                    "  span           {:?}..{:?}",
                    row.span_start_ms, row.span_end_ms
                );
                println!(
                    "  embedding      {} 维（{}）",
                    row.embedding.as_ref().map_or(0, |v| v.as_slice().len()),
                    row.embedding_model
                );
                println!("  文本 {} 字：", row.text.chars().count());
                println!("{}", row.text);
                println!("  tsv 前 20 词：");
                let tsv = row.tsv_source.as_deref().unwrap_or("");
                println!(
                    "    {}",
                    tsv.split_whitespace()
                        .take(20)
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                println!("─────────────────────────────────────────");
            }
        }
    }
    println!("耗时 {:.1}s", elapsed.as_secs_f32());
    Ok(())
}

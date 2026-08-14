//! A multimodal smoke test: actually send an image, and look at two things.
//!
//! 1. **The local gate**: a model without vision support (DeepSeek) gets
//!    blocked by [`LlmClient::ensure_can_see`] before the request goes out,
//!    with the error naming which model it was and how to fix it;
//! 2. **On-the-wire behavior**: bypass the gate and go straight through goose's
//!    `Provider`, proving the message is **constructed correctly** (goose
//!    converted it into each vendor's image format per the engine), and print
//!    the remote end's real reply or real error verbatim.
//!
//! Step 2 deliberately uses `client.provider()` rather than
//! `client.complete()` — the latter would be stopped by our own gate, and then
//! you would never see how the remote end actually responds. That path belongs
//! to this example only; production code should not be written this way.
//!
//! ```bash
//! # Defaults to the provider configured in .env (deepseek, no vision →
//! # demonstrates the error path)
//! cargo run -p cortex-llm --example vision
//!
//! # Name one that can see images (a local Ollama needs no key)
//! CORTEX_LLM_PROVIDER=ollama CORTEX_LLM_MODEL=gemma4:latest \
//!   cargo run -p cortex-llm --example vision -- path/to/image.png
//! ```

use std::path::PathBuf;

use cortex_llm::{LlmClient, Message, MessageImageExt, VisionSupport};

/// The test image that ships with the repository: a synthesized "console
/// screenshot" with a Chinese title, three metric cards, seven bars carrying
/// numeric values, and a footer line — built specifically to check whether a
/// description really read the text and numbers out.
const DEFAULT_IMAGE: &str = "crates/cortex-memory/tests/fixtures/console_screenshot.png";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("提示：未加载 .env（{e}），改用进程环境变量");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cortex_llm=debug,warn".into()),
        )
        .init();

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

    let client = LlmClient::from_env()?;
    let model = client.model().clone();
    println!(
        "供应商 {} / 模型 {} / vision 声明 {:?}",
        client.provider_id(),
        model.model_name,
        client.vision_support(&model)
    );
    println!("图片 {} （{} 字节，{mime}）", path.display(), bytes.len());
    println!("─────────────────────────────────────────");

    let messages = [Message::user()
        .with_text("这张图里有什么？把你看到的文字逐字念出来。")
        .with_image_bytes(&bytes, mime)?];
    println!(
        "消息构造完成：{} 个图像块",
        cortex_llm::image_count(&messages)
    );

    // ① The local gate
    match client.ensure_can_see(&model, &messages) {
        Ok(()) => println!("① 本地闸门：放行"),
        Err(e) => println!("① 本地闸门：拦下 —— {e}"),
    }

    // ② Actually send it. The gate is bypassed only to observe remote
    // behavior; see the note at the top of the file.
    println!("② 直发供应商（绕过闸门）……");
    match client
        .provider()
        .complete(&model, "你是媒体转录器，回答简洁。", &messages, &[])
        .await
    {
        Ok((reply, usage)) => {
            println!("─────────────────────────────────────────");
            println!("{}", reply.as_concat_text());
            println!("─────────────────────────────────────────");
            println!(
                "token：入 {:?} 出 {:?}",
                usage.usage.input_tokens, usage.usage.output_tokens
            );
            anyhow::ensure!(
                client.vision_support(&model) != VisionSupport::Unsupported,
                "声明不支持 vision 的模型却成功返回了 —— 定义 JSON 该更新了"
            );
        }
        // The provider refusing the image is also a valid result: it proves the
        // request was assembled and delivered successfully, and the only
        // difference is whether the far side accepts it. The error must be
        // readable, which is exactly what we are here to look at.
        Err(e) => println!("远端拒绝：{e}"),
    }
    Ok(())
}

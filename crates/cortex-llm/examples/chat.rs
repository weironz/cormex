//! A provider-layer smoke test: ask the configured model one question and print
//! the streamed deltas chunk by chunk.
//!
//! Before running, put the key in the `.env` at the project root (that file is
//! excluded by .gitignore):
//!
//! ```text
//! DEEPSEEK_API_KEY=sk-...
//! CORTEX_LLM_PROVIDER=deepseek
//! CORTEX_LLM_CHEAP_MODEL=deepseek-v4-flash
//! ```
//!
//! Then: `cargo run -p cortex-llm --example chat`
//!
//! A `|` is printed between chunks, so you can confirm by eye that this is real
//! streaming rather than collecting everything and dumping it at the end.

use std::io::Write;

use cortex_llm::{LlmClient, Message};
use futures::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Search upward from the project root for .env; an example's working
    // directory is the workspace root, so it is found directly.
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("提示：未加载 .env（{e}），改用进程环境变量");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cortex_llm=debug,warn".into()),
        )
        .init();

    let client = LlmClient::from_env()?;
    // The smoke test uses the cheap model; do not burn the main model's quota.
    let model = client.cheap_model();
    println!(
        "供应商 {} / 模型 {}（上下文上限 {}）",
        client.provider_id(),
        model.model_name,
        model.context_limit()
    );
    println!("─────────────────────────────────────────");

    let messages = [Message::user().with_text("用一句话介绍你自己")];
    let mut stream = client
        .stream_text(model, "你是 Cortex 的助手，回答简洁。", &messages)
        .await?;

    let mut chunks = 0usize;
    while let Some(chunk) = stream.next().await {
        print!("{}|", chunk?);
        // Without a flush the terminal buffers until a newline, and the
        // streaming effect is invisible.
        std::io::stdout().flush()?;
        chunks += 1;
    }

    println!("\n─────────────────────────────────────────");
    println!("共 {chunks} 个增量块");
    anyhow::ensure!(chunks > 1, "只收到 {chunks} 块，流式没生效");
    Ok(())
}

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
        eprintln!("note: .env was not loaded ({e}), falling back to the process environment");
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
        "provider {} / model {} (context limit {})",
        client.provider_id(),
        model.model_name,
        model.context_limit()
    );
    println!("─────────────────────────────────────────");

    let messages = [Message::user().with_text("Introduce yourself in one sentence")];
    let mut stream = client
        .stream_text(
            model,
            "You are Cortex's assistant. Answer concisely.",
            &messages,
        )
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
    println!("{chunks} delta chunks in total");
    anyhow::ensure!(
        chunks > 1,
        "only {chunks} chunk(s) arrived — streaming did not take effect"
    );
    Ok(())
}

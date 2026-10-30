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
//! cargo run -p cormex-llm --example vision
//!
//! # Name one that can see images (a local Ollama needs no key)
//! CORMEX_LLM_PROVIDER=ollama CORMEX_LLM_MODEL=gemma4:latest \
//!   cargo run -p cormex-llm --example vision -- path/to/image.png
//! ```

use std::path::PathBuf;

use cormex_llm::{LlmClient, Message, MessageImageExt, VisionSupport};

/// The test image that ships with the repository: a synthesized "console
/// screenshot" with a Chinese title, three metric cards, seven bars carrying
/// numeric values, and a footer line — built specifically to check whether a
/// description really read the text and numbers out.
const DEFAULT_IMAGE: &str = "crates/cormex-memory/tests/fixtures/console_screenshot.png";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("note: .env was not loaded ({e}), falling back to the process environment");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cormex_llm=debug,warn".into()),
        )
        .init();

    let path = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_IMAGE), PathBuf::from);
    let bytes = std::fs::read(&path)
        .map_err(|e| anyhow::anyhow!("cannot read the image {}: {e}", path.display()))?;
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
        other => anyhow::bail!("unrecognized extension {other:?}"),
    };

    let client = LlmClient::from_env()?;
    let model = client.model().clone();
    println!(
        "provider {} / model {} / declared vision {:?}",
        client.provider_id(),
        model.model_name,
        client.vision_support(&model)
    );
    println!("image {} ({} bytes, {mime})", path.display(), bytes.len());
    println!("─────────────────────────────────────────");

    let messages = [Message::user()
        .with_text("What is in this image? Read out any text you see, verbatim.")
        .with_image_bytes(&bytes, mime)?];
    println!(
        "message assembled: {} image block(s)",
        cormex_llm::image_count(&messages)
    );

    // ① The local gate
    match client.ensure_can_see(&model, &messages) {
        Ok(()) => println!("(1) local gate: let through"),
        Err(e) => println!("(1) local gate: blocked — {e}"),
    }

    // ② Actually send it. The gate is bypassed only to observe remote
    // behavior; see the note at the top of the file.
    println!("(2) sending straight to the provider (bypassing the gate)...");
    match client
        .provider()
        .complete(
            &model,
            "You transcribe media. Answer concisely.",
            &messages,
            &[],
        )
        .await
    {
        Ok((reply, usage)) => {
            println!("─────────────────────────────────────────");
            println!("{}", reply.as_concat_text());
            println!("─────────────────────────────────────────");
            println!(
                "tokens: in {:?} out {:?}",
                usage.usage.input_tokens, usage.usage.output_tokens
            );
            anyhow::ensure!(
                client.vision_support(&model) != VisionSupport::Unsupported,
                "a vision-incapable model returned successfully — update the definition JSON"
            );
        }
        // The provider refusing the image is also a valid result: it proves the
        // request was assembled and delivered successfully, and the only
        // difference is whether the far side accepts it. The error must be
        // readable, which is exactly what we are here to look at.
        Err(e) => println!("the remote refused: {e}"),
    }
    Ok(())
}

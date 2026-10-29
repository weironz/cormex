//! Transcription smoke test: runs one image through the complete
//! [`TranscribePipeline`] and prints, verbatim, the row that would eventually
//! be written into `blob_transcripts`.
//!
//! This is the acceptance site for the judgement "media that is not turned
//! into text is a retrieval black hole" — what we look at is not "the model
//! said something", but **whether the produced text carries enough signal for
//! retrieval to hit**: the text inside the image, recognizable entities,
//! structural type.
//!
//! ```bash
//! # Local Ollama, no key of any kind needed (the model must be ollama pull'd first)
//! CORMEX_VISION_PROVIDER=ollama CORMEX_VISION_MODEL=gemma4:latest \
//!   cargo run -p cormex-memory --example vision_caption
//!
//! # Use a different image
//! ... --example vision_caption -- path/to/image.png
//! ```
//!
//! With `CORMEX_VISION_PROVIDER` unset it falls back to [`StubTranscriber`],
//! which verifies that the pipeline itself (tokenization, vectorization,
//! redaction protection) also works in an offline environment.
//!
//! The embedding is fixed to [`HashEmbedder`]: what this example verifies is
//! transcription, and it should not download 600 MB of bge-m3 along the way.

use std::path::PathBuf;
use std::sync::Arc;

use cormex_memory::embed::HashEmbedder;
use cormex_memory::transcribe::{
    NeverRedacted, Outcome, StubTranscriber, TranscribePipeline, Transcriber, VisionTranscriber,
};

const DEFAULT_IMAGE: &str = "crates/cormex-memory/tests/fixtures/console_screenshot.png";

/// A fake blob hash. On the real path it comes from `blobs.hash` (the SHA-256
/// of the content).
const DEMO_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000001";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("note: .env was not loaded ({e}); using the process environment instead");
    }

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

    let transcriber: Arc<dyn Transcriber> = match VisionTranscriber::from_env()? {
        Some(v) => {
            println!("transcriber: {} (a real vision model)", v.model_id());
            Arc::new(v)
        }
        None => {
            println!("transcriber: the stub (CORMEX_VISION_PROVIDER is not configured)");
            Arc::new(StubTranscriber::new())
        }
    };

    let pipeline =
        TranscribePipeline::new(Arc::new(HashEmbedder::new())).with_transcriber(transcriber);

    println!("input: {} ({} bytes, {mime})", path.display(), bytes.len());
    println!("─────────────────────────────────────────");
    let started = std::time::Instant::now();
    let outcome = pipeline
        .run(DEMO_HASH, &bytes, mime, &NeverRedacted)
        .await?;
    let elapsed = started.elapsed();

    match &outcome {
        Outcome::Redacted => anyhow::bail!("NeverRedacted should never report a redaction"),
        Outcome::Unsupported { mime } => anyhow::bail!("no transcriber claims {mime}"),
        Outcome::Ready(rows) => {
            anyhow::ensure!(
                !rows.is_empty(),
                "the transcription produced nothing -- this media is still unretrievable"
            );
            for (i, row) in rows.iter().enumerate() {
                println!("segment {}/{}", i + 1, rows.len());
                println!("  kind           {}", row.kind.as_str());
                println!("  transcribed_by {}", row.transcribed_by);
                println!(
                    "  span           {:?}..{:?}",
                    row.span_start_ms, row.span_end_ms
                );
                println!(
                    "  embedding      {} dims ({})",
                    row.embedding.as_ref().map_or(0, |v| v.as_slice().len()),
                    row.embedding_model
                );
                println!("  text {} chars:", row.text.chars().count());
                println!("{}", row.text);
                println!("  first 20 tsv words:");
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
    println!("elapsed {:.1}s", elapsed.as_secs_f32());
    Ok(())
}

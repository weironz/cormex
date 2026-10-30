//! The transcription API seen from **outside the crate**.
//!
//! The unit tests already cover the internal logic; this file only checks the
//! two things they cannot reach:
//!
//! 1. Whether what is exported is enough — can another crate (cormexd) implement
//!    its own [`Transcriber`] and [`RedactionGuard`] using only the exports
//!    under `cormex_memory::`, without having to `use` internal paths;
//! 2. Whether the traits' object safety and `Send + Sync` bounds still hold
//!    across a crate boundary — inside the crate the concrete types are visible,
//!    so those two often cannot be tested at all.
//!
//! No database: this file checks "what the produced rows look like", not
//! "can they be written".

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cormex_core::Result;
use cormex_memory::embed::HashEmbedder;
use cormex_memory::{
    NeverRedacted, Outcome, RedactionGuard, Segment, StubTranscriber, TranscribePipeline,
    Transcriber,
};
use cormex_store::TranscriptKind;

const HASH: &str = "0000000000000000000000000000000000000000000000000000000000000042";

/// The test image shipped with the repository. It is the input for real vision
/// transcription, see `examples/vision_caption.rs`.
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/console_screenshot.png"
);

/// A transcriber implemented entirely outside the crate — proof that the trait
/// is usable downstream.
///
/// Simulates OCR: cuts the media into a fixed three chunks of text with a
/// timeline.
struct ExternalOcr {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Transcriber for ExternalOcr {
    fn kind(&self) -> TranscriptKind {
        TranscriptKind::Ocr
    }
    fn transcribed_by(&self) -> &str {
        "external-ocr-test"
    }
    fn supports(&self, mime: &str) -> bool {
        mime == "image/tiff"
    }
    async fn transcribe(&self, _bytes: &[u8], _mime: &str) -> Result<Vec<Segment>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![
            Segment::whole("第一页：合同编号 CX-2026-0807"),
            Segment::whole("第二页：签署方 张三 与 Cormex 有限公司"),
        ])
    }
}

/// An externally implemented redaction verdict — this is the shape it takes on
/// the cormexd side (which calls `store.is_redacted` internally).
struct DenyList(Vec<String>);

#[async_trait::async_trait]
impl RedactionGuard for DenyList {
    async fn is_redacted(&self, blob_hash: &str) -> Result<bool> {
        Ok(self.0.iter().any(|h| h == blob_hash))
    }
}

#[tokio::test]
async fn downstream_can_supply_its_own_transcriber() {
    let ocr = Arc::new(ExternalOcr {
        calls: AtomicUsize::new(0),
    });
    let pipeline = TranscribePipeline::new(Arc::new(HashEmbedder::new()))
        .with_transcriber(ocr.clone())
        .with_transcriber(Arc::new(StubTranscriber::new()));

    let out = pipeline
        .run(HASH, b"II*\x00 fake tiff", "image/tiff", &NeverRedacted)
        .await
        .expect("an external transcriber should run through cleanly");

    let rows = out.rows();
    assert_eq!(
        rows.len(),
        2,
        "the external transcriber returned two chunks, so there should be two rows"
    );
    assert!(rows.iter().all(|r| r.kind == TranscriptKind::Ocr));
    assert!(rows.iter().all(|r| r.transcribed_by == "external-ocr-test"));
    assert_eq!(ocr.calls.load(Ordering::SeqCst), 1);
    assert!(
        rows[0].text.contains("CX-2026-0807"),
        "transcribed text must be kept verbatim; rewrite it and it can no longer be found"
    );
}

#[tokio::test]
async fn first_matching_transcriber_wins() {
    // When several candidates claim the same MIME, order is priority — an image
    // can be captioned or OCR'd, and this ordering decides which one gets
    // written to blob_transcripts.kind.
    let pipeline = TranscribePipeline::new(Arc::new(HashEmbedder::new()))
        .with_transcriber(Arc::new(StubTranscriber::with_kind(TranscriptKind::Ocr)))
        .with_transcriber(Arc::new(StubTranscriber::new()));

    let out = pipeline
        .run(HASH, b"\x89PNG fake", "image/png", &NeverRedacted)
        .await
        .unwrap();
    assert_eq!(
        out.rows()[0].kind,
        TranscriptKind::Ocr,
        "the one attached first should be the one that matches"
    );
}

#[tokio::test]
async fn downstream_redaction_guard_blocks_the_write() {
    let pipeline = TranscribePipeline::new(Arc::new(HashEmbedder::new()))
        .with_transcriber(Arc::new(StubTranscriber::new()));
    let guard = DenyList(vec![HASH.to_string()]);

    let out = pipeline
        .run(HASH, b"\x89PNG fake", "image/png", &guard)
        .await
        .unwrap();
    assert!(matches!(out, Outcome::Redacted));
    assert!(
        out.rows().is_empty(),
        "a redacted blob must not carry out a single row, otherwise the in-flight \
         task writes the secret back"
    );

    // Switch to a hash that is not on the list; the same pipeline should produce
    // output as usual
    let other = "0000000000000000000000000000000000000000000000000000000000000043";
    let out = pipeline
        .run(other, b"\x89PNG fake", "image/png", &guard)
        .await
        .unwrap();
    assert_eq!(
        out.rows().len(),
        1,
        "a blob that has not been redacted must be unaffected"
    );
}

#[tokio::test]
async fn pipeline_is_shareable_across_tasks() {
    // cormexd stuffs this into axum's state and then calls it concurrently;
    // this assertion nails the Send + Sync + 'static requirement to compile time.
    let pipeline = Arc::new(
        TranscribePipeline::new(Arc::new(HashEmbedder::new()))
            .with_transcriber(Arc::new(StubTranscriber::new())),
    );
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let p = Arc::clone(&pipeline);
            tokio::spawn(async move {
                let bytes = vec![b'a'; i + 1];
                p.run(HASH, &bytes, "image/png", &NeverRedacted).await
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.await.unwrap().unwrap().rows().len(), 1);
    }
}

#[test]
fn vision_fixture_is_present_and_is_a_png() {
    // Real vision verification depends on this file (both
    // examples/vision_caption.rs and cormex-llm's examples/vision.rs read it by
    // default). If it goes missing, it should be caught right here.
    let bytes = std::fs::read(FIXTURE).expect("the test image is missing");
    assert!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "the fixture must be a real PNG — given bad bytes a vision model just \
         answers with a refusal"
    );
    assert!(
        bytes.len() > 1024 && bytes.len() < cormex_llm::MAX_IMAGE_BYTES,
        "the fixture should have real content and stay under the send limit; got {} bytes",
        bytes.len()
    );
}

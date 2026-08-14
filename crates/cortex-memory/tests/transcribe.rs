//! 从 **crate 外部**看转录 API。
//!
//! 单测已经覆盖了内部逻辑，这里只验单测验不到的两件事：
//!
//! 1. 公开出去的东西够不够用 —— 别的 crate（cortexd）能不能只靠
//!    `cortex_memory::` 下的导出实现自己的 [`Transcriber`] 与
//!    [`RedactionGuard`]，而不必去 `use` 内部路径；
//! 2. trait 的 object safety 与 `Send + Sync` 边界在跨 crate 时是否仍成立 ——
//!    这两条在 crate 内部因为具体类型可见，往往测不出来。
//!
//! 不连数据库：本文件验的是「产出的行长什么样」，不是「写得进去吗」。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cortex_core::Result;
use cortex_memory::embed::HashEmbedder;
use cortex_memory::{
    NeverRedacted, Outcome, RedactionGuard, Segment, StubTranscriber, TranscribePipeline,
    Transcriber,
};
use cortex_store::TranscriptKind;

const HASH: &str = "0000000000000000000000000000000000000000000000000000000000000042";

/// 仓库自带的测试图。真实 vision 转录的输入，
/// 见 `examples/vision_caption.rs`。
const FIXTURE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/console_screenshot.png"
);

/// 一个完全由外部实现的转录器 —— 证明 trait 对下游可用。
///
/// 模拟 OCR：把媒体切成固定的三段带时间轴的文本。
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
            Segment::whole("第二页：签署方 张三 与 Cortex 有限公司"),
        ])
    }
}

/// 外部实现的抹除判定 —— cortexd 那侧就是这个形状（内部调 `store.is_redacted`）。
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
        .expect("外部转录器应能正常跑通");

    let rows = out.rows();
    assert_eq!(rows.len(), 2, "外部转录器返回两段，就该有两行");
    assert!(rows.iter().all(|r| r.kind == TranscriptKind::Ocr));
    assert!(rows.iter().all(|r| r.transcribed_by == "external-ocr-test"));
    assert_eq!(ocr.calls.load(Ordering::SeqCst), 1);
    assert!(
        rows[0].text.contains("CX-2026-0807"),
        "转录文本必须原样保留，被改写就检索不到了"
    );
}

#[tokio::test]
async fn first_matching_transcriber_wins() {
    // 同一个 MIME 有多个候选时顺序即优先级 —— 图片既可 caption 也可 OCR，
    // 这个次序决定了 blob_transcripts.kind 写的是哪一个。
    let pipeline = TranscribePipeline::new(Arc::new(HashEmbedder::new()))
        .with_transcriber(Arc::new(StubTranscriber::with_kind(TranscriptKind::Ocr)))
        .with_transcriber(Arc::new(StubTranscriber::new()));

    let out = pipeline
        .run(HASH, b"\x89PNG fake", "image/png", &NeverRedacted)
        .await
        .unwrap();
    assert_eq!(out.rows()[0].kind, TranscriptKind::Ocr, "先挂的应先匹配");
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
        "被抹除的 blob 一行都不许带出去，否则在途任务会把秘密回填"
    );

    // 换一个没在名单里的 hash，同一条流水线应照常产出
    let other = "0000000000000000000000000000000000000000000000000000000000000043";
    let out = pipeline
        .run(other, b"\x89PNG fake", "image/png", &guard)
        .await
        .unwrap();
    assert_eq!(out.rows().len(), 1, "未被抹除的 blob 不该受影响");
}

#[tokio::test]
async fn pipeline_is_shareable_across_tasks() {
    // cortexd 会把它塞进 axum 的 state 再并发调用；
    // 这条断言把 Send + Sync + 'static 的要求钉死在编译期。
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
    // 真实 vision 验证依赖这个文件（examples/vision_caption.rs 与
    // cortex-llm 的 examples/vision.rs 都默认读它）。丢了要在这里就发现。
    let bytes = std::fs::read(FIXTURE).expect("测试图缺失");
    assert!(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "fixture 必须是真 PNG —— vision 模型对坏字节只会回一句拒绝"
    );
    assert!(
        bytes.len() > 1024 && bytes.len() < cortex_llm::MAX_IMAGE_BYTES,
        "fixture 应有实际内容且不超发送上限，实际 {} 字节",
        bytes.len()
    );
}

//! Media transcription: turn images / audio / video into **text that can be retrieved**.
//!
//! ```text
//! blob bytes
//!     │
//!     ├─① pick a transcriber   route by MIME to vision / OCR / ASR
//!     ├─② redaction pre-check  consult redactions; don't burn a model call for nothing
//!     ├─③ transcribe           → Vec<Segment> (audio/video carry ms offsets, images don't)
//!     ├─④ tokenize + embed     the precondition for writing in the same txn as the main row
//!     └─⑤ redaction re-check   ←── the one that matters: a redact may have landed during ③④
//! ```
//!
//! # Why this layer has to exist
//!
//! The `blob_transcripts` table has been in place since the migration (with
//! `span_start_ms` / `span_end_ms`), but until now **nothing ever wrote to it**.
//! Media that never becomes text cannot be hit by any of the four retrieval
//! paths — storing it is the same as not storing it, and three months later the
//! user cannot find the image they themselves uploaded.
//!
//! # Two disciplines that must hold
//!
//! 1. **Consult `redactions` before writing.**
//!    redact is the single exception to append-only across the whole chain
//!    (docs/memory.md §11); its promise to destroy data rests on "no in-flight
//!    task writes the secret back afterwards". Transcription is the slowest
//!    class of in-flight task (one vision call takes tens of seconds), so its
//!    window is the widest.
//!    This module builds that into the **signature**: [`TranscribePipeline::run`]
//!    must be handed a [`RedactionGuard`], so whoever wires it up cannot forget.
//! 2. **Model calls happen outside the transaction.** Same reasoning as
//!    extraction (CLAUDE.md constraint 2): this module only produces "rows
//!    waiting to be written" such as [`cortex_store::NewBlobTranscript`];
//!    the actual `write_txn` is opened by the caller, and committed at once.
//!
//! # State of ASR
//!
//! This module has **no** working ASR implementation — only the trait and the
//! [`TranscriptKind::Asr`] branch. The reasons and the gaps are documented on
//! [`Transcriber`].
//! Better to have [`Outcome::Unsupported`] say plainly "nobody claims this MIME"
//! than to stuff in an implementation that does not run.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use cortex_core::{CortexError, Id, Result};
use cortex_llm::{LlmClient, Message, MessageImageExt, ModelConfig, VisionSupport};
use cortex_store::{NewBlobTranscript, TranscriptKind, Vector};

use crate::embed::SharedEmbedder;
use crate::tokenize;

// ══════════════════════════════════════════════════════════
//  Segment
// ══════════════════════════════════════════════════════════

/// One chunk of text produced by transcription. One chunk = one row in
/// `blob_transcripts`.
///
/// # Why chunks instead of one blob of text
///
/// The retrieval value of audio and video is **being able to jump back**: after
/// finding "he said we should use RustFS", what the user wants is the player to
/// seek to 31:40, not a two-hour wall of text.
/// `span_*_ms` exists for exactly that, and chunking is its only use.
///
/// Images have no timeline, so `span_*` is always `None` — which is also where
/// the schema's `CHECK (span_end_ms IS NULL OR span_start_ms IS NOT NULL)`
/// comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    /// Chunk start (milliseconds). `None` for images / OCR.
    pub span_start_ms: Option<i64>,
    /// Chunk end (milliseconds). An end requires a start, see [`Self::validate`].
    pub span_end_ms: Option<i64>,
}

impl Segment {
    /// A chunk of text with no timeline (image caption, whole-page OCR).
    #[must_use]
    pub fn whole(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            span_start_ms: None,
            span_end_ms: None,
        }
    }

    /// A chunk of text with a timeline (ASR, video frame captures).
    #[must_use]
    pub fn span(text: impl Into<String>, start_ms: i64, end_ms: i64) -> Self {
        Self {
            text: text.into(),
            span_start_ms: Some(start_ms),
            span_end_ms: Some(end_ms),
        }
    }

    /// Duration (milliseconds). `None` if either endpoint is missing.
    #[must_use]
    pub fn duration_ms(&self) -> Option<i64> {
        self.span_start_ms.zip(self.span_end_ms).map(|(s, e)| e - s)
    }

    /// Check whether this chunk can be written into `blob_transcripts`.
    ///
    /// # Why check again on the Rust side
    ///
    /// The database has all three constraints (`text NOT NULL`,
    /// `CHECK (span_end_ms IS NULL OR span_start_ms IS NOT NULL)`), but the cost
    /// of violating them is asymmetric: `write_txn` holds
    /// `pg_advisory_xact_lock(4272)`, so one bad row rolls back **the entire
    /// transaction**, taking the other transcripts of the same batch down with
    /// it — and the only error you get is a Postgres constraint name. Catching
    /// it here lets us name the offending chunk exactly.
    ///
    /// # Errors
    ///
    /// Empty text, an end without a start, a negative time, or an end earlier
    /// than its start.
    pub fn validate(&self) -> Result<()> {
        if self.text.trim().is_empty() {
            return Err(CortexError::Invalid(
                "the transcript segment text is empty -- an empty transcript is hit by no retrieval \
                 at all, which is the same as never transcribing it"
                    .into(),
            ));
        }
        match (self.span_start_ms, self.span_end_ms) {
            (None, Some(end)) => Err(CortexError::Invalid(format!(
                "the transcript segment has an end at {end}ms but no start, which violates the \
                 CHECK constraint on blob_transcripts"
            ))),
            (Some(start), _) if start < 0 => Err(CortexError::Invalid(format!(
                "transcript segment start {start}ms is negative"
            ))),
            (Some(start), Some(end)) if end < start => Err(CortexError::Invalid(format!(
                "the transcript segment end {end}ms is earlier than its start {start}ms"
            ))),
            _ => Ok(()),
        }
    }
}

// ══════════════════════════════════════════════════════════
//  Transcriber
// ══════════════════════════════════════════════════════════

/// One way of turning media into text.
///
/// # The four kinds are four completely different pipelines
///
/// | kind | input | state |
/// |---|---|---|
/// | [`TranscriptKind::VisionCaption`] | images | [`VisionTranscriber`], via a multimodal LLM |
/// | [`TranscriptKind::Ocr`] | images / scans | not implemented. The "文字" line of a vision caption already covers most cases |
/// | [`TranscriptKind::Asr`] | audio / video | **not implemented**, see below |
/// | [`TranscriptKind::FrameCaption`] | video frames | not implemented, needs container demuxing and frame extraction |
///
/// # What ASR is missing
///
/// All three routes are blocked, which is why there is an interface here and no
/// implementation:
///
/// - **Local inference**: the `fastembed` route only has text models. Whisper
///   means either binding `whisper.cpp` (a C dependency plus cross-compiling for
///   six targets) or assembling it yourself on `ort` (mel spectrogram pre/post
///   processing, beam search and timestamp alignment all written by hand);
/// - **Cloud API**: needs a key from a provider that offers ASR. goose's
///   provider layer only covers chat completions; the transcription endpoints
///   (OpenAI `/v1/audio/transcriptions`) are not among them, so we would have to
///   write the HTTP client ourselves — precisely the kind of work that "take the
///   part off the shelf" is meant to avoid;
/// - **Feeding audio straight to a multimodal LLM**: technically viable — the
///   local Ollama's `gemma4` already declares an `audio` capability in
///   `/api/show`, and rmcp has had `RawAudioContent` for a while.
///   **The gap is only in goose**: its `MessageContent` only wires up
///   Text / Image / Tool* / Thinking / SystemNotification, never pulled in
///   rmcp's Audio variant, and there is no audio counterpart to `convert_image`
///   on the image route.
///   This is the route closest to usable; what's missing is a small piece on the
///   goose side (one variant plus a `convert_audio`). Bypassing goose and
///   issuing the request from this layer would work, but that means giving up
///   the take-it-off-the-shelf payoff and shouldering every vendor's audio
///   format differences again.
///
/// Until then, audio and video land on [`Outcome::Unsupported`], which
/// **says plainly that nobody handles them**.
#[async_trait::async_trait]
pub trait Transcriber: Send + Sync {
    /// Written into `blob_transcripts.kind`.
    fn kind(&self) -> TranscriptKind;

    /// Written into `blob_transcripts.transcribed_by`.
    ///
    /// It must uniquely identify "which model / which pipeline produced this",
    /// for exactly the same reason as `embedding_model` (see
    /// [`crate::embed::Embedder::model_id`]): after switching models it is what
    /// tells old rows from new ones and decides which rows to re-run.
    fn transcribed_by(&self) -> &str;

    /// Whether this MIME is its business.
    fn supports(&self, mime: &str) -> bool;

    /// Transcribe.
    ///
    /// # Errors
    ///
    /// The model call failed, the content is invalid, or the output is unusable.
    async fn transcribe(&self, bytes: &[u8], mime: &str) -> Result<Vec<Segment>>;
}

#[async_trait::async_trait]
impl<T: Transcriber + ?Sized> Transcriber for Arc<T> {
    fn kind(&self) -> TranscriptKind {
        (**self).kind()
    }
    fn transcribed_by(&self) -> &str {
        (**self).transcribed_by()
    }
    fn supports(&self, mime: &str) -> bool {
        (**self).supports(mime)
    }
    async fn transcribe(&self, bytes: &[u8], mime: &str) -> Result<Vec<Segment>> {
        (**self).transcribe(bytes, mime).await
    }
}

// ══════════════════════════════════════════════════════════
//  Redaction guard
// ══════════════════════════════════════════════════════════

/// The verdict on "has this blob already been redacted".
///
/// # Why it is a required parameter and not an option
///
/// The in-flight-task protection from docs/memory.md §11: for redact to truly
/// destroy data, it must guarantee that from the moment it runs, **no in-flight
/// task writes the content back afterwards**. Transcription is the in-flight
/// task with the widest window — a single vision call takes tens of seconds, and
/// the user may well hit "redact" during it.
///
/// This crate must not touch store (the layering and parallel-task boundary), so
/// the constraint is encoded in the type: [`TranscribePipeline::run`] takes a
/// `&dyn RedactionGuard`, and whoever wires it up has **no way** to call it
/// without explicitly supplying an implementation.
///
/// The implementation on the cortexd side is one line:
///
/// ```ignore
/// store.is_redacted(RedactionTarget::Blob, blob_hash).await
/// ```
#[async_trait::async_trait]
pub trait RedactionGuard: Send + Sync {
    /// Look for a tombstone against this blob in the `redactions` table.
    ///
    /// # Errors
    ///
    /// The query failed. **This must propagate upward and must never be treated
    /// as `false`** — taking "cannot tell" for "not redacted" is exactly what
    /// this guard exists to prevent.
    async fn is_redacted(&self, blob_hash: &str) -> Result<bool>;
}

/// Always answers "not redacted". **Unit tests and offline development only**,
/// same standing as [`crate::embed::HashEmbedder`].
///
/// It is named `Never` rather than `AllowAll` so that its appearance in
/// production code looks wrong at a glance.
pub struct NeverRedacted;

#[async_trait::async_trait]
impl RedactionGuard for NeverRedacted {
    async fn is_redacted(&self, _blob_hash: &str) -> Result<bool> {
        Ok(false)
    }
}

// ══════════════════════════════════════════════════════════
//  Pipeline
// ══════════════════════════════════════════════════════════

/// How one transcription ended.
///
/// Three states rather than a `Vec`: an empty `Vec` cannot distinguish "redacted,
/// must be discarded" from "no transcriber, catch up later" from "the
/// transcription came back empty", and the caller's next move differs completely
/// across those three (discard / re-queue / raise an alarm).
#[derive(Debug)]
pub enum Outcome {
    /// Transcription finished; these rows can go into `blob_transcripts`.
    ///
    /// They have already passed the redaction re-check, so the caller should
    /// open a transaction and write them **as soon as possible**; the longer it
    /// waits, the wider the window opens again.
    Ready(Vec<NewBlobTranscript>),
    /// This blob has been redacted. **Discard the lot; not a single row may be
    /// written.**
    Redacted,
    /// No transcriber claims this MIME (currently that mostly means audio and
    /// video, see [`Transcriber`]).
    /// Not an error: once ASR lands, re-running once is enough.
    Unsupported { mime: String },
}

impl Outcome {
    /// The writable rows; an empty slice when this is not [`Self::Ready`].
    #[must_use]
    pub fn rows(&self) -> &[NewBlobTranscript] {
        match self {
            Self::Ready(rows) => rows,
            _ => &[],
        }
    }
}

/// Strings transcribers, tokenization and embedding into one pipeline whose
/// output can go straight into the database.
pub struct TranscribePipeline {
    transcribers: Vec<Arc<dyn Transcriber>>,
    embedder: SharedEmbedder,
}

impl TranscribePipeline {
    /// Build an empty pipeline. It only becomes useful once at least one
    /// [`Self::with_transcriber`] has been added.
    #[must_use]
    pub fn new(embedder: SharedEmbedder) -> Self {
        Self {
            transcribers: Vec::new(),
            embedder,
        }
    }

    /// Attach a transcriber. **First attached, first matched** — when several
    /// candidates claim the same MIME (an image can be captioned or OCR'd, for
    /// instance), order is priority.
    #[must_use]
    pub fn with_transcriber(mut self, transcriber: Arc<dyn Transcriber>) -> Self {
        self.transcribers.push(transcriber);
        self
    }

    /// Who claims this MIME.
    #[must_use]
    pub fn transcriber_for(&self, mime: &str) -> Option<&Arc<dyn Transcriber>> {
        self.transcribers.iter().find(|t| t.supports(mime))
    }

    /// Run once: bytes → `blob_transcripts` rows waiting to be written.
    ///
    /// # Why the redaction check happens twice
    ///
    /// - **Before transcribing**: purely to save money and time. There is no
    ///   point spending tens of seconds on a vision call for a blob that is
    ///   already redacted. Missing a redaction here leaks nothing.
    /// - **Before returning**: this one is the guard itself. A redact can
    ///   perfectly well land while transcription is running, and that is the
    ///   exact shape of "an in-flight task writes the secret back".
    ///
    /// Checking twice does not make the window zero (there is still a gap
    /// between receiving `Ready` and the actual INSERT); the real closure is the
    /// idempotent cascading re-run on the redact executor side
    /// (docs/memory.md §11). What happens here is squeezing the window from
    /// "tens of seconds" down to "a few milliseconds".
    ///
    /// # Errors
    ///
    /// Transcription failed, embedding failed, the redaction query failed, or a
    /// produced chunk is invalid.
    pub async fn run(
        &self,
        blob_hash: &str,
        bytes: &[u8],
        mime: &str,
        guard: &dyn RedactionGuard,
    ) -> Result<Outcome> {
        let Some(transcriber) = self.transcriber_for(mime) else {
            tracing::debug!(mime, blob_hash, "no transcriber claims this MIME, skipping");
            return Ok(Outcome::Unsupported {
                mime: mime.to_string(),
            });
        };

        if guard.is_redacted(blob_hash).await? {
            tracing::info!(
                blob_hash,
                "the blob has been redacted; skipped before transcription"
            );
            return Ok(Outcome::Redacted);
        }

        let segments = transcriber.transcribe(bytes, mime).await?;
        if segments.is_empty() {
            tracing::warn!(
                blob_hash,
                mime,
                transcriber = transcriber.transcribed_by(),
                "the transcriber returned nothing; this media is still a retrieval black hole"
            );
            return Ok(Outcome::Ready(Vec::new()));
        }
        for (i, seg) in segments.iter().enumerate() {
            seg.validate()
                .map_err(|e| CortexError::Invalid(format!("segment {i}: {e}")))?;
        }

        // Embed the whole batch in one call: the chunk count is usually a single
        // digit to a few dozen, and calling one at a time would shred the
        // embedder's lock into a sieve (see the lock notes in embed.rs)
        let texts: Vec<String> = segments.iter().map(|s| s.text.clone()).collect();
        let vectors = self.embedder.embed(&texts).await?;
        if vectors.len() != segments.len() {
            return Err(CortexError::Memory(format!(
                "the embedder returned {} vectors, but there are {} segments",
                vectors.len(),
                segments.len()
            )));
        }

        // ← the one that matters: a redact may have landed during ③④
        if guard.is_redacted(blob_hash).await? {
            tracing::warn!(
                blob_hash,
                segments = segments.len(),
                "the blob was redacted while transcription was running; dropping the whole batch"
            );
            return Ok(Outcome::Redacted);
        }

        let kind = transcriber.kind();
        let transcribed_by = transcriber.transcribed_by().to_string();
        let embedding_model = self.embedder.model_id().to_string();

        let rows = segments
            .into_iter()
            .zip(vectors)
            .map(|(seg, vector)| NewBlobTranscript {
                id: Id::new(),
                blob_hash: blob_hash.to_string(),
                // tsv must be written in the same transaction as the main row
                // and never backfilled asynchronously (CLAUDE.md constraint 3),
                // so tokenization is done right here and handed to the caller
                // together with the row
                tsv_source: Some(tokenize::to_tsvector_input(&seg.text)),
                text: seg.text,
                kind,
                embedding: Some(Vector::from(vector)),
                span_start_ms: seg.span_start_ms,
                span_end_ms: seg.span_end_ms,
                transcribed_by: transcribed_by.clone(),
                embedding_model: embedding_model.clone(),
            })
            .collect();

        Ok(Outcome::Ready(rows))
    }
}

// ══════════════════════════════════════════════════════════
//  VisionTranscriber — the real multimodal model
// ══════════════════════════════════════════════════════════

/// Every quality requirement for the caption lives here.
///
/// # Why the prompt is this long
///
/// This text **is** the entire basis on which the image can later be found — the
/// original is never read a second time. A caption like "an image" is worth zero
/// in retrieval: it is identical to the caption of every other image in the
/// store, so neither BM25 nor the vectors can tell them apart. The prompt
/// therefore has to force out four classes of retrievable signal:
///
/// 1. **The text inside the image** (OCR in nature) — what users most often
///    recall by ("what was that error message called again");
/// 2. **Identifiable entities** — people, products, file names, URLs, version
///    numbers; the extraction pipeline can consume these a second time;
/// 3. **Structural type** — "screenshot" vs "photo" vs "chart", the strongest
///    coarse-filter signal there is;
/// 4. **Detail enough to recognize it** — the last metre of retrieval is a human
///    eye confirming "yes, that's the one".
///
/// Output is in Chinese: the tokenizer in the store is jieba
/// ([`crate::tokenize`]) and the embedding is bge-m3 (Chinese-first). Making the
/// model answer in English takes a haircut on both retrieval paths at once.
const VISION_SYSTEM: &str = "\
你是媒体转录器。你的输出会被存进记忆库并用于**全文检索与向量检索**，\
原图之后不会再被读取——找不回来就等于这张图不存在。

因此：
- 绝不写「这是一张图片」「图中展示了一些内容」这类零信息的话。
- 绝不臆造：看不清就写「看不清」，不要猜测、不要补全。
- 用中文输出；专有名词、代码、路径、URL、数字、版本号保持原样。
- 不要 Markdown 标题、不要加粗、不要列表符号，只按下面四行输出。

严格输出四行，每行以给定标签开头，缺项写「无」：

类型：截图 / 照片 / 图表 / 示意图 / 扫描件 / 表格 / 图标或 logo / 手写 / 其它，之一，\
再补一句这是什么界面或场景
文字：图中出现的全部文字，逐字转录，按阅读顺序，不同区块之间用「｜」分隔；最多 600 字
实体：可识别的人名、产品名、品牌、文件名、路径、URL、版本号、日期、金额，逗号分隔
内容：2 到 4 句话描述画面，要写到「看过这张图的人能凭这段话把它认出来」的程度；最多 200 字";

/// Caption images with a multimodal LLM.
///
/// # Why the model has to be configurable on its own
///
/// Cortex's default provider is DeepSeek, and **DeepSeek has no vision model at
/// all** (`/models` only returns `deepseek-v4-flash` / `deepseek-v4-pro`).
/// So the model used for transcription may naturally come from a different
/// vendor than the main conversation model; see the `CORTEX_VISION_*` variables
/// on [`Self::from_env`].
///
/// `Debug` is forwarded to [`LlmClient`]'s own implementation, which already
/// guarantees it never prints the key.
#[derive(Debug)]
pub struct VisionTranscriber {
    client: LlmClient,
    model: ModelConfig,
    transcribed_by: String,
}

impl VisionTranscriber {
    /// Bind an already-constructed client to a model name.
    ///
    /// # Errors
    ///
    /// The model name failed to resolve, or the provider definition **explicitly
    /// states** that the model does not support vision. Models that declare no
    /// capability at all (Ollama, providers in goose's catalogue) are treated as
    /// usable — the reasoning is on [`cortex_llm::VisionSupport`].
    pub fn new(client: LlmClient, model: &str) -> Result<Self> {
        let model_config = client.model_config(model).map_err(CortexError::from)?;
        if client.vision_support(&model_config) == VisionSupport::Unsupported {
            return Err(CortexError::Config(format!(
                "{}/{model} does not accept image input and cannot be used as a vision transcriber",
                client.provider_id()
            )));
        }
        Ok(Self {
            transcribed_by: format!("{}/{model}", client.provider_id()),
            client,
            model: model_config,
        })
    }

    /// Construct from `CORTEX_VISION_PROVIDER` / `CORTEX_VISION_MODEL`.
    ///
    /// **If `CORTEX_VISION_PROVIDER` is unset, return `Ok(None)`** — a
    /// deliberate graceful degradation: multimodal transcription needs an extra
    /// provider that may cost extra money, and no deployment should be forced to
    /// configure one. Images are still archived as usual, they just cannot be
    /// retrieved for now; configure it later and re-run once.
    ///
    /// But **a provider set with the key missing is an error**, with no further
    /// degradation: that is an explicit statement of "I want this feature on",
    /// and turning it off silently means finding out weeks later.
    ///
    /// # Errors
    ///
    /// The provider name is not recognized, the provider's key environment
    /// variable is missing, or the model does not support vision.
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(provider) = std::env::var("CORTEX_VISION_PROVIDER") else {
            tracing::info!(
                "CORTEX_VISION_PROVIDER is not set; image transcription is off (images are still archived)"
            );
            return Ok(None);
        };
        let provider = provider.trim().to_string();
        if provider.is_empty() {
            return Ok(None);
        }

        let model = std::env::var("CORTEX_VISION_MODEL").map_err(|_| {
            CortexError::Config(
                "setting CORTEX_VISION_PROVIDER means CORTEX_VISION_MODEL must be set too".into(),
            )
        })?;

        let key_var = cortex_llm::provider::api_key_env(&provider).map_err(CortexError::from)?;
        let api_key = if key_var.is_empty() {
            String::new() // no authentication required (Ollama)
        } else {
            std::env::var(&key_var).map_err(|_| {
                CortexError::Config(format!(
                    "CORTEX_VISION_PROVIDER={provider} needs the {key_var} environment variable"
                ))
            })?
        };

        let client = LlmClient::from_config(
            &cortex_core::config::LlmConfig {
                provider,
                model: model.clone(),
                cheap_model: model.clone(),
                // The vision model used for transcription may point at a
                // different endpoint — a self-hosted deployment is often a mix
                // of "chat runs locally, looking at images runs in the cloud",
                // and the two should not be welded together
                base_url: std::env::var("CORTEX_VISION_BASE_URL")
                    .ok()
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty()),
            },
            &api_key,
        )
        .map_err(CortexError::from)?;

        Self::new(client, &model).map(Some)
    }

    /// The identifier written into `transcribed_by`, shaped like
    /// `ollama/gemma4:latest`.
    #[must_use]
    pub fn model_id(&self) -> &str {
        &self.transcribed_by
    }
}

#[async_trait::async_trait]
impl Transcriber for VisionTranscriber {
    fn kind(&self) -> TranscriptKind {
        TranscriptKind::VisionCaption
    }

    fn transcribed_by(&self) -> &str {
        &self.transcribed_by
    }

    fn supports(&self, mime: &str) -> bool {
        // Only accept the intersection of the vendors' vision APIs: things like
        // BMP/TIFF are images but cannot be sent, so claiming them would just
        // mean failing once inside transcribe and coming back here
        cortex_llm::normalize_image_mime(mime).is_ok()
    }

    async fn transcribe(&self, bytes: &[u8], mime: &str) -> Result<Vec<Segment>> {
        let message = Message::user()
            .with_text(format!(
                "这是一份 {mime}，共 {} 字节。请按系统提示的四行格式转录。",
                bytes.len()
            ))
            .with_image_bytes(bytes, mime)
            .map_err(CortexError::from)?;

        let started = std::time::Instant::now();
        let (reply, usage) = self
            .client
            .complete(&self.model, VISION_SYSTEM, &[message], &[])
            .await
            .map_err(CortexError::from)?;

        // as_concat_text only joins Text blocks, so thinking / redacted-thinking
        // are excluded automatically — writing the model's chain of thought into
        // the memory store has no retrieval value and pollutes BM25
        let text = reply.as_concat_text().trim().to_string();
        tracing::info!(
            model = %self.transcribed_by,
            elapsed_ms = started.elapsed().as_millis(),
            input_tokens = usage.usage.input_tokens,
            output_tokens = usage.usage.output_tokens,
            chars = text.chars().count(),
            "vision transcription finished"
        );

        if text.is_empty() {
            return Err(CortexError::Memory(format!(
                "{} returned an empty description for the image",
                self.transcribed_by
            )));
        }
        // Images have no timeline; the whole picture is one chunk
        Ok(vec![Segment::whole(text)])
    }
}

// ══════════════════════════════════════════════════════════
//  StubTranscriber — depends on no external service
// ══════════════════════════════════════════════════════════

/// A deterministic transcription stub, **for tests and offline development
/// only**, same standing as [`crate::embed::HashEmbedder`].
///
/// It understands nothing (the text it produces has no relation to the image
/// content), but it needs no model and no network and its output is stable
/// enough to assert on, which lets the tests for the pipeline, the redaction
/// guard and the timeline avoid depending on a real vision call that takes tens
/// of seconds.
///
/// kind is configurable: `Asr` / `FrameCaption` produce **several chunks with
/// millisecond offsets**, precisely so that the `span_*_ms` path has something
/// to test against.
pub struct StubTranscriber {
    kind: TranscriptKind,
}

impl StubTranscriber {
    /// Defaults to [`TranscriptKind::VisionCaption`], which claims images.
    #[must_use]
    pub fn new() -> Self {
        Self {
            kind: TranscriptKind::VisionCaption,
        }
    }

    /// Pick the kind. It decides which MIME family this stub claims, and whether
    /// its output carries a timeline.
    #[must_use]
    pub fn with_kind(kind: TranscriptKind) -> Self {
        Self { kind }
    }

    /// Whether this kind's output carries a timeline.
    fn is_timed(&self) -> bool {
        matches!(
            self.kind,
            TranscriptKind::Asr | TranscriptKind::FrameCaption
        )
    }
}

impl Default for StubTranscriber {
    fn default() -> Self {
        Self::new()
    }
}

/// Content fingerprint. `DefaultHasher` rather than a real SHA-256: all that is
/// needed here is "same input, same output; different inputs most likely
/// different", no cryptographic property whatsoever, and it is not worth adding
/// a dependency for.
fn fingerprint(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

#[async_trait::async_trait]
impl Transcriber for StubTranscriber {
    fn kind(&self) -> TranscriptKind {
        self.kind
    }

    fn transcribed_by(&self) -> &str {
        // Deliberately shaped unlike a real transcriber's id, so a glance at the
        // store shows which rows came from the stub, and a future re-run can
        // filter on it
        "stub-transcriber-v1"
    }

    fn supports(&self, mime: &str) -> bool {
        let mime = mime.trim().to_ascii_lowercase();
        match self.kind {
            TranscriptKind::VisionCaption | TranscriptKind::Ocr => mime.starts_with("image/"),
            TranscriptKind::Asr => mime.starts_with("audio/") || mime.starts_with("video/"),
            TranscriptKind::FrameCaption => mime.starts_with("video/"),
        }
    }

    async fn transcribe(&self, bytes: &[u8], mime: &str) -> Result<Vec<Segment>> {
        if bytes.is_empty() {
            return Err(CortexError::Invalid(
                "the transcription input is empty".into(),
            ));
        }
        let fp = fingerprint(bytes);
        let label = self.kind.as_str();

        if !self.is_timed() {
            return Ok(vec![Segment::whole(format!(
                "[桩转录 {label}] {mime} {} 字节 指纹 {fp:016x}",
                bytes.len()
            ))]);
        }

        // 1..=3 chunks of 1500ms each, laid end to end. The chunk count is
        // derived from the content, so it is reproducible
        const SEGMENT_MS: i64 = 1500;
        let count = (bytes.len() % 3) + 1;
        Ok((0..count)
            .map(|i| {
                let start = i as i64 * SEGMENT_MS;
                Segment::span(
                    format!(
                        "[桩转录 {label}] 第 {}/{count} 段 {mime} 指纹 {fp:016x}",
                        i + 1
                    ),
                    start,
                    start + SEGMENT_MS,
                )
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n fake but non-empty";
    const HASH: &str = "0000000000000000000000000000000000000000000000000000000000000001";

    fn pipeline() -> TranscribePipeline {
        TranscribePipeline::new(Arc::new(HashEmbedder::new()))
            .with_transcriber(Arc::new(StubTranscriber::new()))
    }

    // ── Segment validation ────────────────────────────────

    #[test]
    fn end_without_start_is_rejected_before_postgres_sees_it() {
        let bad = Segment {
            text: "x".into(),
            span_start_ms: None,
            span_end_ms: Some(100),
        };
        assert!(
            bad.validate().is_err(),
            "an end without a start violates the CHECK on blob_transcripts; \
             letting it through rolls back the whole lock-holding transaction \
             and takes the other transcripts of the same batch with it"
        );
    }

    #[test]
    fn reversed_and_negative_spans_are_rejected() {
        assert!(
            Segment::span("x", 900, 100).validate().is_err(),
            "a chunk whose end precedes its start turns \"seek to 31:40\" into seeking backwards"
        );
        assert!(Segment::span("x", -1, 100).validate().is_err());
        assert!(
            Segment::span("x", 0, 0).validate().is_ok(),
            "a zero-length span is legal"
        );
    }

    #[test]
    fn empty_text_is_rejected() {
        assert!(
            Segment::whole("   ").validate().is_err(),
            "an empty transcript is hit by no retrieval path; writing it only occupies a row"
        );
    }

    #[test]
    fn duration_needs_both_ends() {
        assert_eq!(Segment::span("x", 100, 400).duration_ms(), Some(300));
        assert_eq!(Segment::whole("x").duration_ms(), None);
    }

    // ── MIME routing ─────────────────────────────────────

    #[test]
    fn stub_claims_only_its_own_media_family() {
        let vision = StubTranscriber::new();
        assert!(vision.supports("image/png"));
        assert!(
            vision.supports("IMAGE/JPEG"),
            "MIME letter case must not affect routing"
        );
        assert!(!vision.supports("audio/mpeg"));
        assert!(!vision.supports("application/pdf"));

        let asr = StubTranscriber::with_kind(TranscriptKind::Asr);
        assert!(asr.supports("audio/mpeg") && asr.supports("video/mp4"));
        assert!(!asr.supports("image/png"));

        let frames = StubTranscriber::with_kind(TranscriptKind::FrameCaption);
        assert!(frames.supports("video/mp4"));
        assert!(
            !frames.supports("audio/mpeg"),
            "frame extraction is meaningless for pure audio; it must not claim it"
        );
    }

    #[tokio::test]
    async fn unclaimed_mime_says_so_instead_of_returning_nothing() {
        let out = pipeline()
            .run(HASH, b"ID3 fake mp3", "audio/mpeg", &NeverRedacted)
            .await
            .unwrap();
        match out {
            Outcome::Unsupported { mime } => assert_eq!(mime, "audio/mpeg"),
            other => panic!(
                "audio is unclaimed today; this must say Unsupported outright, got {other:?}"
            ),
        }
    }

    // ── Timeline ─────────────────────────────────────────

    #[tokio::test]
    async fn image_rows_carry_no_timeline() {
        let out = pipeline()
            .run(HASH, PNG, "image/png", &NeverRedacted)
            .await
            .unwrap();
        let rows = out.rows();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (rows[0].span_start_ms, rows[0].span_end_ms),
            (None, None),
            "images have no timeline; inventing an offset puts a \"seek to 0:00\" on a screenshot"
        );
        assert_eq!(rows[0].kind, TranscriptKind::VisionCaption);
    }

    #[tokio::test]
    async fn timed_segments_are_contiguous_and_ordered() {
        let p = TranscribePipeline::new(Arc::new(HashEmbedder::new()))
            .with_transcriber(Arc::new(StubTranscriber::with_kind(TranscriptKind::Asr)));
        // 3 bytes → 3 % 3 + 1 = 1 chunk; 4 bytes → 2 chunks. Pick a length that
        // yields several
        let bytes = b"abcde"; // 5 % 3 + 1 = 3 chunks
        let out = p
            .run(HASH, bytes, "audio/mpeg", &NeverRedacted)
            .await
            .unwrap();
        let rows = out.rows();
        assert_eq!(rows.len(), 3, "a 5-byte stub input yields 3 chunks");

        for (i, row) in rows.iter().enumerate() {
            let start = row.span_start_ms.expect("an ASR chunk must have a start");
            let end = row.span_end_ms.expect("an ASR chunk must have an end");
            assert!(end > start, "chunk {i} has a non-positive duration");
            if i > 0 {
                assert_eq!(
                    start,
                    rows[i - 1].span_end_ms.unwrap(),
                    "chunks must lie end to end; a gap makes \"jump back\" land on nothing"
                );
            }
        }
    }

    // ── Redaction guard ──────────────────────────────────

    struct AlwaysRedacted;

    #[async_trait::async_trait]
    impl RedactionGuard for AlwaysRedacted {
        async fn is_redacted(&self, _: &str) -> Result<bool> {
            Ok(true)
        }
    }

    /// Lets the first query through and blocks every one after it — simulating
    /// "the user hit redact while transcription was running".
    struct RedactedMidFlight(std::sync::atomic::AtomicUsize);

    #[async_trait::async_trait]
    impl RedactionGuard for RedactedMidFlight {
        async fn is_redacted(&self, _: &str) -> Result<bool> {
            Ok(self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0)
        }
    }

    struct BrokenGuard;

    #[async_trait::async_trait]
    impl RedactionGuard for BrokenGuard {
        async fn is_redacted(&self, _: &str) -> Result<bool> {
            Err(CortexError::Memory("数据库连不上".into()))
        }
    }

    #[tokio::test]
    async fn already_redacted_blob_is_never_transcribed() {
        let out = pipeline()
            .run(HASH, PNG, "image/png", &AlwaysRedacted)
            .await
            .unwrap();
        assert!(matches!(out, Outcome::Redacted));
        assert!(
            out.rows().is_empty(),
            "Redacted must not carry out a single writable row"
        );
    }

    #[tokio::test]
    async fn redaction_during_transcription_discards_everything() {
        // This is the **only** reason this guard exists: the tens-of-seconds
        // window during transcription.
        let guard = RedactedMidFlight(std::sync::atomic::AtomicUsize::new(0));
        let out = pipeline()
            .run(HASH, PNG, "image/png", &guard)
            .await
            .unwrap();
        assert!(
            matches!(out, Outcome::Redacted),
            "the re-check must happen before returning, otherwise an in-flight task \
             writes the redacted content back"
        );
    }

    #[tokio::test]
    async fn guard_failure_propagates_instead_of_defaulting_to_allow() {
        let err = pipeline()
            .run(HASH, PNG, "image/png", &BrokenGuard)
            .await
            .expect_err("a query that cannot answer must be an error");
        assert!(
            err.to_string().contains("数据库连不上"),
            "taking \"cannot tell\" for \"not redacted\" is exactly what this guard \
             prevents; got: {err}"
        );
    }

    // ── Completeness of the rows to be written ───────────

    #[tokio::test]
    async fn rows_carry_everything_the_insert_needs() {
        let out = pipeline()
            .run(HASH, PNG, "image/png", &NeverRedacted)
            .await
            .unwrap();
        let row = &out.rows()[0];

        assert_eq!(row.blob_hash, HASH);
        assert_eq!(row.transcribed_by, "stub-transcriber-v1");
        assert_eq!(row.embedding_model, "hash-stub-v1");
        assert_eq!(
            row.embedding.as_ref().unwrap().as_slice().len(),
            crate::embed::EMBEDDING_DIM,
            "the dimension must match the migration's VECTOR(1024)"
        );
        let tsv = row
            .tsv_source
            .as_deref()
            .expect("tsv must be written in the same transaction as the main row");
        assert!(
            !tsv.is_empty(),
            "an empty tsv = the BM25 retrieval path is permanently dead for this row"
        );
    }

    #[tokio::test]
    async fn stub_output_is_deterministic() {
        let a = pipeline()
            .run(HASH, PNG, "image/png", &NeverRedacted)
            .await
            .unwrap();
        let b = pipeline()
            .run(HASH, PNG, "image/png", &NeverRedacted)
            .await
            .unwrap();
        assert_eq!(
            a.rows()[0].text,
            b.rows()[0].text,
            "the stub must be reproducible, or the tests have nothing to assert on"
        );
        assert_ne!(
            a.rows()[0].text,
            pipeline()
                .run(
                    HASH,
                    b"\x89PNG different content",
                    "image/png",
                    &NeverRedacted
                )
                .await
                .unwrap()
                .rows()[0]
                .text,
            "different content must produce different text, else retrieval sees one image"
        );
    }

    #[tokio::test]
    async fn empty_bytes_are_rejected() {
        assert!(
            pipeline()
                .run(HASH, &[], "image/png", &NeverRedacted)
                .await
                .is_err()
        );
    }

    // ── prompt ──────────────────────────────────────────

    #[test]
    fn vision_prompt_demands_all_four_retrieval_signals() {
        // Drop any one of these classes and the image loses one of the routes by
        // which it could later be found.
        for signal in ["类型", "文字", "实体", "内容"] {
            assert!(
                VISION_SYSTEM.contains(signal),
                "the prompt must ask for \"{signal}\", otherwise the caption it \
                 produces carries no such signal into retrieval"
            );
        }
        assert!(
            VISION_SYSTEM.contains("这是一张图片"),
            "the prompt must ban zero-information captions **by name** — merely saying \
             \"be detailed\" still gets filler out of the model"
        );
        assert!(
            VISION_SYSTEM.contains("中文"),
            "the store runs jieba + bge-m3; answering in English takes a haircut on \
             both the BM25 and the vector path"
        );
    }

    #[test]
    fn vision_transcriber_refuses_a_blind_model() {
        // DeepSeek has no vision model at all. This must fail at construction
        // time, not when the first image arrives in production.
        let client = cortex_llm::LlmClient::from_config(
            &cortex_core::config::LlmConfig {
                provider: "deepseek".into(),
                model: "deepseek-v4-pro".into(),
                cheap_model: "deepseek-v4-flash".into(),
                base_url: None,
            },
            "test-key",
        )
        .unwrap();
        let err = VisionTranscriber::new(client, "deepseek-v4-pro")
            .expect_err("a model without vision support cannot serve as a transcriber");
        assert!(err.to_string().contains("deepseek-v4-pro"), "got: {err}");
    }

    #[test]
    fn vision_transcriber_accepts_a_seeing_model() {
        let client = cortex_llm::LlmClient::from_config(
            &cortex_core::config::LlmConfig {
                provider: "anthropic".into(),
                model: "claude-opus-5".into(),
                cheap_model: "claude-haiku-4-5-20251001".into(),
                base_url: None,
            },
            "test-key",
        )
        .unwrap();
        let t = VisionTranscriber::new(client, "claude-opus-5").expect("Claude supports images");
        assert_eq!(t.kind(), TranscriptKind::VisionCaption);
        assert_eq!(t.transcribed_by(), "anthropic/claude-opus-5");
        assert!(t.supports("image/png") && !t.supports("image/bmp"));
    }
}

//! 媒体转录：把图片 / 音视频变成**可被检索的文本**。
//!
//! ```text
//! blob 字节
//!     │
//!     ├─① 挑转录器   按 MIME 分流 vision / OCR / ASR
//!     ├─② 抹除前置检查  查 redactions，已抹除就别白跑一趟模型
//!     ├─③ 转录       → Vec<Segment>（音视频带毫秒偏移，图片不带）
//!     ├─④ 分词 + 向量化  与主行同事务写入的前提
//!     └─⑤ 抹除复检   ←── 真正关键的一次：③④ 期间可能刚好被 redact 了
//! ```
//!
//! # 为什么必须有这一层
//!
//! `blob_transcripts` 表在 migration 里就建好了（含 `span_start_ms` /
//! `span_end_ms`），但在此之前**没有任何东西往里写**。
//! 不转成文本的媒体在四路召回里一条也命中不了 —— 存了等于没存，
//! 用户传过的图三个月后自己也找不回来。
//!
//! # 两条必须守住的纪律
//!
//! 1. **写入前必须查 `redactions`。**
//!    redact 是全链路 append-only 里唯一的例外（docs/memory.md §十一），
//!    它的销毁承诺依赖「没有在途任务事后把秘密回填」。转录是最慢的一类
//!    在途任务（一次 vision 调用几十秒），窗口最大。
//!    本模块把它做进了**签名**：[`TranscribePipeline::run`] 必须收一个
//!    [`RedactionGuard`]，接线的人不可能忘记传。
//! 2. **模型调用在事务外。** 与抽取同理（CLAUDE.md 约束 2）：这里只产出
//!    [`cortex_store::NewBlobTranscript`] 这样的「待写行」，
//!    真正的 `write_txn` 由调用方开，且开完立刻提交。
//!
//! # ASR 现状
//!
//! 本模块**没有**可用的 ASR 实现，只留了 trait 与 [`TranscriptKind::Asr`]
//! 这条分支。原因与缺口见 [`Transcriber`] 的文档。
//! 宁可让 [`Outcome::Unsupported`] 明说「这个 MIME 没人管」，
//! 也不塞一个跑不通的实现进来。

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

/// 转录产出的一段文本。一段 = `blob_transcripts` 里的一行。
///
/// # 为什么分段而不是一整块
///
/// 音视频的检索价值在于**能跳回去**：搜到「他说了要用 RustFS」之后，
/// 用户要的是播放器跳到 31:40，而不是一段两小时的文字墙。
/// `span_*_ms` 就是为此存在的，切段是它唯一的用法。
///
/// 图片没有时间轴，`span_*` 恒为 `None` —— 这也是 schema 里
/// `CHECK (span_end_ms IS NULL OR span_start_ms IS NOT NULL)` 的由来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    /// 片段起点（毫秒）。图片 / OCR 为 `None`。
    pub span_start_ms: Option<i64>,
    /// 片段终点（毫秒）。有终点必须有起点，见 [`Self::validate`]。
    pub span_end_ms: Option<i64>,
}

impl Segment {
    /// 一段没有时间轴的文本（图片描述、整页 OCR）。
    #[must_use]
    pub fn whole(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            span_start_ms: None,
            span_end_ms: None,
        }
    }

    /// 一段带时间轴的文本（ASR、视频抽帧）。
    #[must_use]
    pub fn span(text: impl Into<String>, start_ms: i64, end_ms: i64) -> Self {
        Self {
            text: text.into(),
            span_start_ms: Some(start_ms),
            span_end_ms: Some(end_ms),
        }
    }

    /// 时长（毫秒）。缺任一端点即 `None`。
    #[must_use]
    pub fn duration_ms(&self) -> Option<i64> {
        self.span_start_ms.zip(self.span_end_ms).map(|(s, e)| e - s)
    }

    /// 校验这一段能不能写进 `blob_transcripts`。
    ///
    /// # 为什么在 Rust 侧再查一遍
    ///
    /// 三条约束数据库都有（`text NOT NULL`、`CHECK (span_end_ms IS NULL OR
    /// span_start_ms IS NOT NULL)`），但违反它们的代价不对称：
    /// `write_txn` 是持着 `pg_advisory_xact_lock(4272)` 的，一条坏行会让
    /// **整个事务回滚**，连带丢掉同批次的其它转录，而错误信息只有一句
    /// Postgres 的约束名。在这里拦下来能指名道姓说清是哪一段。
    ///
    /// # Errors
    ///
    /// 文本为空、有终点无起点、时间为负、或终点早于起点。
    pub fn validate(&self) -> Result<()> {
        if self.text.trim().is_empty() {
            return Err(CortexError::Invalid(
                "转录段落文本为空 —— 空转录不会被任何检索命中，等同于没转".into(),
            ));
        }
        match (self.span_start_ms, self.span_end_ms) {
            (None, Some(end)) => Err(CortexError::Invalid(format!(
                "转录段落有终点 {end}ms 却没有起点，违反 blob_transcripts 的 CHECK 约束"
            ))),
            (Some(start), _) if start < 0 => {
                Err(CortexError::Invalid(format!("转录段落起点 {start}ms 为负")))
            }
            (Some(start), Some(end)) if end < start => Err(CortexError::Invalid(format!(
                "转录段落终点 {end}ms 早于起点 {start}ms"
            ))),
            _ => Ok(()),
        }
    }
}

// ══════════════════════════════════════════════════════════
//  Transcriber
// ══════════════════════════════════════════════════════════

/// 一种把媒体变成文本的办法。
///
/// # 四种 kind 对应四条完全不同的 pipeline
///
/// | kind | 输入 | 现状 |
/// |---|---|---|
/// | [`TranscriptKind::VisionCaption`] | 图片 | [`VisionTranscriber`]，走多模态 LLM |
/// | [`TranscriptKind::Ocr`] | 图片 / 扫描件 | 未实现。vision caption 的「文字」行已覆盖大部分场景 |
/// | [`TranscriptKind::Asr`] | 音频 / 视频 | **未实现**，见下 |
/// | [`TranscriptKind::FrameCaption`] | 视频抽帧 | 未实现，依赖解容器抽帧 |
///
/// # ASR 缺什么
///
/// 三条路都不通，所以这里只留接口不留实现：
///
/// - **本地推理**：`fastembed` 那条路只有文本模型，Whisper 要么绑
///   `whisper.cpp`（C 依赖 + 六端交叉编译）要么自己用 `ort` 装配
///   （前后处理 mel 谱、beam search、时间戳对齐全得手写）；
/// - **云 API**：需要一个提供 ASR 的供应商密钥。goose 的 provider 层
///   只覆盖 chat completions，转写端点（OpenAI `/v1/audio/transcriptions`）
///   不在其中，得自己写 HTTP 客户端 —— 那正好是「取件」要避免的那类活；
/// - **多模态 LLM 直接吃音频**：技术上可行 —— 本机 Ollama 上的 `gemma4`
///   在 `/api/show` 里就声明了 `audio` 能力，rmcp 也早有
///   `RawAudioContent`。**缺口只在 goose**：它的 `MessageContent`
///   只接了 Text / Image / Tool* / Thinking / SystemNotification，
///   没把 rmcp 的 Audio 变体接进来，图像那条路上的 `convert_image`
///   也没有音频对应物。
///   这一条离可用最近，缺的是 goose 侧的一小段（一个变体 + 一个
///   `convert_audio`）；在本层绕开 goose 自己发请求能解，
///   但那等于放弃取件收益、把各家音频格式差异重新背回来。
///
/// 在此之前，音视频走到 [`Outcome::Unsupported`]，**明说没人管**。
#[async_trait::async_trait]
pub trait Transcriber: Send + Sync {
    /// 写进 `blob_transcripts.kind`。
    fn kind(&self) -> TranscriptKind;

    /// 写进 `blob_transcripts.transcribed_by`。
    ///
    /// 必须能唯一标识「哪个模型/哪套 pipeline 产出的」，理由与
    /// `embedding_model` 完全相同（见 [`crate::embed::FastModel::model_id`]）：
    /// 换模型后要靠它区分新旧、决定重跑哪些行。
    fn transcribed_by(&self) -> &str;

    /// 这个 MIME 归不归它管。
    fn supports(&self, mime: &str) -> bool;

    /// 转录。
    ///
    /// # Errors
    ///
    /// 模型调用失败、内容非法、或产出无法使用。
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
//  抹除防护
// ══════════════════════════════════════════════════════════

/// 「这个 blob 是不是已经被抹除了」的判定。
///
/// # 为什么它是必填参数而不是可选项
///
/// docs/memory.md §十一 的在途任务防护：redact 要真正销毁数据，就必须保证
/// 执行的那一刻起，**没有任何在途任务事后把内容写回来**。转录是窗口最大的
/// 在途任务 —— 一次 vision 调用几十秒，期间用户完全可能点了「抹除」。
///
/// 本 crate 不能碰 store（分层与并行任务的边界），所以把这个约束做进类型：
/// [`TranscribePipeline::run`] 收一个 `&dyn RedactionGuard`，
/// 接线的人**没有办法**在不显式提供实现的情况下调用它。
///
/// cortexd 那侧的实现就一行：
///
/// ```ignore
/// store.is_redacted(RedactionTarget::Blob, blob_hash).await
/// ```
#[async_trait::async_trait]
pub trait RedactionGuard: Send + Sync {
    /// 查 `redactions` 表里有没有针对这个 blob 的墓碑。
    ///
    /// # Errors
    ///
    /// 查询失败。**必须向上传播，绝不能当成 `false`** ——
    /// 把「查不出来」当成「没被抹除」正是这道防护要防的事。
    async fn is_redacted(&self, blob_hash: &str) -> Result<bool>;
}

/// 永远回答「没被抹除」。**仅供单测与离线开发**，定位同
/// [`crate::embed::HashEmbedder`]。
///
/// 名字里带 `Never` 而不是 `AllowAll`，是为了让它出现在生产代码里时
/// 一眼就看出不对劲。
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

/// 一次转录的结局。
///
/// 三态而非 `Vec`：空 `Vec` 无法区分「被抹除了，必须丢弃」与
/// 「没有转录器，等以后补」与「转出来是空的」，而这三种情况调用方的
/// 后续动作完全不同（丢弃 / 重排队 / 报警）。
#[derive(Debug)]
pub enum Outcome {
    /// 转录完成，这些行可以写进 `blob_transcripts`。
    ///
    /// 已经过抹除复检，调用方拿到后**尽快**开事务写掉；
    /// 拖得越久，窗口又开回来了。
    Ready(Vec<NewBlobTranscript>),
    /// 该 blob 已被抹除。**整体丢弃，一行都不许写。**
    Redacted,
    /// 没有转录器认领这个 MIME（当前主要是音视频，见 [`Transcriber`]）。
    /// 不是错误：将来 ASR 落地后重跑一次即可。
    Unsupported { mime: String },
}

impl Outcome {
    /// 可写入的行；非 [`Self::Ready`] 时为空切片。
    #[must_use]
    pub fn rows(&self) -> &[NewBlobTranscript] {
        match self {
            Self::Ready(rows) => rows,
            _ => &[],
        }
    }
}

/// 把转录器、分词与向量化串成一条可直接落库的流水线。
pub struct TranscribePipeline {
    transcribers: Vec<Arc<dyn Transcriber>>,
    embedder: SharedEmbedder,
}

impl TranscribePipeline {
    /// 建一条空流水线。至少要 [`Self::with_transcriber`] 一个才有用。
    #[must_use]
    pub fn new(embedder: SharedEmbedder) -> Self {
        Self {
            transcribers: Vec::new(),
            embedder,
        }
    }

    /// 挂一个转录器。**先挂的先匹配** —— 同一个 MIME 有多个候选时
    /// （例如图片既能 caption 又能 OCR），顺序即优先级。
    #[must_use]
    pub fn with_transcriber(mut self, transcriber: Arc<dyn Transcriber>) -> Self {
        self.transcribers.push(transcriber);
        self
    }

    /// 谁认领这个 MIME。
    #[must_use]
    pub fn transcriber_for(&self, mime: &str) -> Option<&Arc<dyn Transcriber>> {
        self.transcribers.iter().find(|t| t.supports(mime))
    }

    /// 跑一遍：字节 → 待写入的 `blob_transcripts` 行。
    ///
    /// # 抹除检查为什么查两次
    ///
    /// - **转录之前**：纯粹省钱省时间。已经被抹除的 blob 没必要再花几十秒
    ///   调一次 vision 模型。这一次查漏了不会造成数据泄露。
    /// - **返回之前**：这一次才是防护本身。redact 完全可能落在转录进行中，
    ///   而那正是「在途任务把秘密回填」的确切形状。
    ///
    /// 两次都查不能保证零窗口（拿到 `Ready` 到真正 INSERT 之间仍有间隙），
    /// 真正的收口在 redact 执行器那侧的幂等级联重跑（docs/memory.md §十一）；
    /// 这里做的是把窗口从「几十秒」压到「几毫秒」。
    ///
    /// # Errors
    ///
    /// 转录失败、向量化失败、抹除查询失败，或产出的段落非法。
    pub async fn run(
        &self,
        blob_hash: &str,
        bytes: &[u8],
        mime: &str,
        guard: &dyn RedactionGuard,
    ) -> Result<Outcome> {
        let Some(transcriber) = self.transcriber_for(mime) else {
            tracing::debug!(mime, blob_hash, "没有转录器认领这个 MIME，跳过");
            return Ok(Outcome::Unsupported {
                mime: mime.to_string(),
            });
        };

        if guard.is_redacted(blob_hash).await? {
            tracing::info!(blob_hash, "blob 已被抹除，转录前即跳过");
            return Ok(Outcome::Redacted);
        }

        let segments = transcriber.transcribe(bytes, mime).await?;
        if segments.is_empty() {
            tracing::warn!(
                blob_hash,
                mime,
                transcriber = transcriber.transcribed_by(),
                "转录器返回空结果，这份媒体仍然是检索黑洞"
            );
            return Ok(Outcome::Ready(Vec::new()));
        }
        for (i, seg) in segments.iter().enumerate() {
            seg.validate()
                .map_err(|e| CortexError::Invalid(format!("第 {i} 段：{e}")))?;
        }

        // 向量化整批一次调用：段落数通常是个位数到几十，
        // 逐条调用会把 embedder 的锁抢成筛子（见 embed.rs 的锁说明）
        let texts: Vec<String> = segments.iter().map(|s| s.text.clone()).collect();
        let vectors = self.embedder.embed(&texts).await?;
        if vectors.len() != segments.len() {
            return Err(CortexError::Memory(format!(
                "embedder 返回 {} 条向量，但有 {} 个段落",
                vectors.len(),
                segments.len()
            )));
        }

        // ← 真正关键的一次：③④ 期间可能刚好被 redact 了
        if guard.is_redacted(blob_hash).await? {
            tracing::warn!(
                blob_hash,
                segments = segments.len(),
                "转录进行中该 blob 被抹除，整批丢弃"
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
                // tsv 必须与主行同事务写入，永不异步补写（CLAUDE.md 约束 3），
                // 所以分词在这里就做完，随行一起交给调用方
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
//  VisionTranscriber —— 真实的多模态模型
// ══════════════════════════════════════════════════════════

/// 描述的质量要求全部写在这里。
///
/// # 为什么 prompt 要这么长
///
/// 这段文字**是**这份图片将来能不能被找到的全部依据 —— 原图不会再被读第二遍。
/// 「一张图片」这种描述在检索里等于零：它与库里任何一张图的描述都一样，
/// BM25 分不出、向量也分不出。所以 prompt 必须逼出四类可检索信号：
///
/// 1. **图里的文字**（OCR 性质）—— 用户最常靠它回忆（「那个报错叫什么来着」）；
/// 2. **可识别实体** —— 人名、产品、文件名、URL、版本号；抽取管线还能二次消费；
/// 3. **结构类型** —— 「截图」还是「照片」还是「图表」，是最强的粗筛信号；
/// 4. **足以回想起来的细节** —— 检索的最后一米是人眼确认「对，就是这张」。
///
/// 输出用中文：库里的分词器是 jieba（[`crate::tokenize`]），
/// embedding 是 bge-m3（中文优先）。让模型输出英文会在两条召回路上同时打折。
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

/// 用多模态 LLM 给图片生成描述。
///
/// # 模型为什么必须能单独配
///
/// Cortex 的默认供应商是 DeepSeek，而 **DeepSeek 一个 vision 模型都没有**
/// （`/models` 只返回 `deepseek-v4-flash` / `deepseek-v4-pro`）。
/// 所以转录用的模型与对话主模型天然可能不是一家，
/// 见 [`Self::from_env`] 的 `CORTEX_VISION_*` 变量。
///
/// `Debug` 转发给 [`LlmClient`] 自己的实现，后者已经保证不打印密钥。
#[derive(Debug)]
pub struct VisionTranscriber {
    client: LlmClient,
    model: ModelConfig,
    transcribed_by: String,
}

impl VisionTranscriber {
    /// 绑定一个已经构造好的客户端与模型名。
    ///
    /// # Errors
    ///
    /// 模型名解析失败，或该模型在供应商定义里**明写**不支持 vision。
    /// 未声明能力（Ollama、goose 目录里的供应商）按可用处理 ——
    /// 理由见 [`cortex_llm::VisionSupport`]。
    pub fn new(client: LlmClient, model: &str) -> Result<Self> {
        let model_config = client.model_config(model).map_err(CortexError::from)?;
        if client.vision_support(&model_config) == VisionSupport::Unsupported {
            return Err(CortexError::Config(format!(
                "{}/{model} 不支持图像输入，无法用作 vision 转录器",
                client.provider_id()
            )));
        }
        Ok(Self {
            transcribed_by: format!("{}/{model}", client.provider_id()),
            client,
            model: model_config,
        })
    }

    /// 按 `CORTEX_VISION_PROVIDER` / `CORTEX_VISION_MODEL` 构造。
    ///
    /// **没设 `CORTEX_VISION_PROVIDER` 就返回 `Ok(None)`** —— 这是刻意的
    /// 优雅降级：多模态转录需要一个额外的、可能要另付费的供应商，
    /// 不该强迫每个部署都配。此时图片照常归档，只是暂时检索不到，
    /// 将来配上再重跑一遍即可。
    ///
    /// 但**设了 provider 却缺密钥就是错误**，不再降级：
    /// 那是明确表达了「我要开这个功能」，静默关掉会让人几周后才发现。
    ///
    /// # Errors
    ///
    /// 供应商名不认识、缺少该供应商的密钥环境变量、或模型不支持 vision。
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(provider) = std::env::var("CORTEX_VISION_PROVIDER") else {
            tracing::info!("未设置 CORTEX_VISION_PROVIDER，图片转录关闭（图片仍会归档）");
            return Ok(None);
        };
        let provider = provider.trim().to_string();
        if provider.is_empty() {
            return Ok(None);
        }

        let model = std::env::var("CORTEX_VISION_MODEL").map_err(|_| {
            CortexError::Config(
                "设置了 CORTEX_VISION_PROVIDER 就必须一并设置 CORTEX_VISION_MODEL".into(),
            )
        })?;

        let key_var = cortex_llm::provider::api_key_env(&provider).map_err(CortexError::from)?;
        let api_key = if key_var.is_empty() {
            String::new() // 免鉴权（Ollama）
        } else {
            std::env::var(&key_var).map_err(|_| {
                CortexError::Config(format!(
                    "CORTEX_VISION_PROVIDER={provider} 需要环境变量 {key_var}"
                ))
            })?
        };

        let client = LlmClient::from_config(
            &cortex_core::config::LlmConfig {
                provider,
                model: model.clone(),
                cheap_model: model.clone(),
                // 转录用的 vision 模型可以指向另一个端点 —— 一个自建部署
                // 常常是「对话走本地、看图走云上」的混合，两者不该被绑死
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

    /// 写进 `transcribed_by` 的标识，形如 `ollama/gemma4:latest`。
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
        // 只认各家 vision API 的交集：BMP/TIFF 这类即使是图片也发不出去，
        // 认领了只会在 transcribe 里失败一次再回到这里
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

        // as_concat_text 只拼 Text 块，thinking / redacted-thinking 自动被排除 ——
        // 把模型的思考过程写进记忆库既没有检索价值又会污染 BM25
        let text = reply.as_concat_text().trim().to_string();
        tracing::info!(
            model = %self.transcribed_by,
            elapsed_ms = started.elapsed().as_millis(),
            input_tokens = usage.usage.input_tokens,
            output_tokens = usage.usage.output_tokens,
            chars = text.chars().count(),
            "vision 转录完成"
        );

        if text.is_empty() {
            return Err(CortexError::Memory(format!(
                "{} 对图像返回了空描述",
                self.transcribed_by
            )));
        }
        // 图片没有时间轴，整张一段
        Ok(vec![Segment::whole(text)])
    }
}

// ══════════════════════════════════════════════════════════
//  StubTranscriber —— 不依赖任何外部服务
// ══════════════════════════════════════════════════════════

/// 确定性的转录桩，**仅供测试与离线开发**，定位同
/// [`crate::embed::HashEmbedder`]。
///
/// 不具备任何理解能力（产出的文本与图片内容无关），但它无需模型、无需网络、
/// 输出稳定可断言，使得流水线、抹除防护、时间轴这些逻辑的测试
/// 不必依赖一次几十秒的真实 vision 调用。
///
/// kind 可配：`Asr` / `FrameCaption` 会产出**多段带毫秒偏移**的结果，
/// 正是为了让 `span_*_ms` 那条路径也有东西可测。
pub struct StubTranscriber {
    kind: TranscriptKind,
}

impl StubTranscriber {
    /// 默认 [`TranscriptKind::VisionCaption`]，认领图片。
    #[must_use]
    pub fn new() -> Self {
        Self {
            kind: TranscriptKind::VisionCaption,
        }
    }

    /// 指定 kind。决定它认领哪类 MIME、以及产出带不带时间轴。
    #[must_use]
    pub fn with_kind(kind: TranscriptKind) -> Self {
        Self { kind }
    }

    /// 该 kind 的产出带不带时间轴。
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

/// 内容指纹。用 `DefaultHasher` 而不是真 SHA-256：这里只要「同输入同输出、
/// 不同输入大概率不同」，不需要任何密码学性质，也不值得为此加一个依赖。
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
        // 与真实转录器的 id 形态明显不同，库里一眼能看出哪些行是桩产出的，
        // 将来重跑时按它筛选
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
            return Err(CortexError::Invalid("转录输入为空".into()));
        }
        let fp = fingerprint(bytes);
        let label = self.kind.as_str();

        if !self.is_timed() {
            return Ok(vec![Segment::whole(format!(
                "[桩转录 {label}] {mime} {} 字节 指纹 {fp:016x}",
                bytes.len()
            ))]);
        }

        // 1..=3 段，每段 1500ms，首尾相接。段数由内容决定，因此可复现
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

    // ── Segment 校验 ──────────────────────────────────────

    #[test]
    fn end_without_start_is_rejected_before_postgres_sees_it() {
        let bad = Segment {
            text: "x".into(),
            span_start_ms: None,
            span_end_ms: Some(100),
        };
        assert!(
            bad.validate().is_err(),
            "有终点无起点违反 blob_transcripts 的 CHECK；\
             放过去会让整个持锁事务回滚，连带丢掉同批次的其它转录"
        );
    }

    #[test]
    fn reversed_and_negative_spans_are_rejected() {
        assert!(
            Segment::span("x", 900, 100).validate().is_err(),
            "终点早于起点的段落会让「跳到 31:40」变成跳回过去"
        );
        assert!(Segment::span("x", -1, 100).validate().is_err());
        assert!(
            Segment::span("x", 0, 0).validate().is_ok(),
            "零时长是合法的"
        );
    }

    #[test]
    fn empty_text_is_rejected() {
        assert!(
            Segment::whole("   ").validate().is_err(),
            "空转录不会被任何检索命中，写进去只是占一行"
        );
    }

    #[test]
    fn duration_needs_both_ends() {
        assert_eq!(Segment::span("x", 100, 400).duration_ms(), Some(300));
        assert_eq!(Segment::whole("x").duration_ms(), None);
    }

    // ── MIME 分流 ────────────────────────────────────────

    #[test]
    fn stub_claims_only_its_own_media_family() {
        let vision = StubTranscriber::new();
        assert!(vision.supports("image/png"));
        assert!(vision.supports("IMAGE/JPEG"), "MIME 大小写不该影响分流");
        assert!(!vision.supports("audio/mpeg"));
        assert!(!vision.supports("application/pdf"));

        let asr = StubTranscriber::with_kind(TranscriptKind::Asr);
        assert!(asr.supports("audio/mpeg") && asr.supports("video/mp4"));
        assert!(!asr.supports("image/png"));

        let frames = StubTranscriber::with_kind(TranscriptKind::FrameCaption);
        assert!(frames.supports("video/mp4"));
        assert!(
            !frames.supports("audio/mpeg"),
            "抽帧对纯音频无意义，不该认领"
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
            other => panic!("音频当前无人认领，应明说 Unsupported，实际 {other:?}"),
        }
    }

    // ── 时间轴 ──────────────────────────────────────────

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
            "图片没有时间轴，编一个偏移会让「跳到 0:00」出现在一张截图上"
        );
        assert_eq!(rows[0].kind, TranscriptKind::VisionCaption);
    }

    #[tokio::test]
    async fn timed_segments_are_contiguous_and_ordered() {
        let p = TranscribePipeline::new(Arc::new(HashEmbedder::new()))
            .with_transcriber(Arc::new(StubTranscriber::with_kind(TranscriptKind::Asr)));
        // 3 字节 → 3 % 3 + 1 = 1 段；4 字节 → 2 段。挑一个能出多段的长度
        let bytes = b"abcde"; // 5 % 3 + 1 = 3 段
        let out = p
            .run(HASH, bytes, "audio/mpeg", &NeverRedacted)
            .await
            .unwrap();
        let rows = out.rows();
        assert_eq!(rows.len(), 3, "5 字节的桩输入应产出 3 段");

        for (i, row) in rows.iter().enumerate() {
            let start = row.span_start_ms.expect("ASR 段落必须有起点");
            let end = row.span_end_ms.expect("ASR 段落必须有终点");
            assert!(end > start, "第 {i} 段时长非正");
            if i > 0 {
                assert_eq!(
                    start,
                    rows[i - 1].span_end_ms.unwrap(),
                    "段落必须首尾相接，留空隙会让「跳回原处」落在没有内容的地方"
                );
            }
        }
    }

    // ── 抹除防护 ────────────────────────────────────────

    struct AlwaysRedacted;

    #[async_trait::async_trait]
    impl RedactionGuard for AlwaysRedacted {
        async fn is_redacted(&self, _: &str) -> Result<bool> {
            Ok(true)
        }
    }

    /// 第一次查放行、之后全部拦下 —— 模拟「转录进行中用户点了抹除」。
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
        assert!(out.rows().is_empty(), "Redacted 不得带出任何可写行");
    }

    #[tokio::test]
    async fn redaction_during_transcription_discards_everything() {
        // 这是这道防护存在的**唯一**理由：转录期间几十秒的窗口。
        let guard = RedactedMidFlight(std::sync::atomic::AtomicUsize::new(0));
        let out = pipeline()
            .run(HASH, PNG, "image/png", &guard)
            .await
            .unwrap();
        assert!(
            matches!(out, Outcome::Redacted),
            "复检必须发生在返回之前，否则在途任务会把已抹除的内容回填"
        );
    }

    #[tokio::test]
    async fn guard_failure_propagates_instead_of_defaulting_to_allow() {
        let err = pipeline()
            .run(HASH, PNG, "image/png", &BrokenGuard)
            .await
            .expect_err("查不出来必须报错");
        assert!(
            err.to_string().contains("数据库连不上"),
            "把「查不出来」当成「没被抹除」正是这道防护要防的事；实际：{err}"
        );
    }

    // ── 落库行的完整性 ──────────────────────────────────

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
            "维度必须与 migration 的 VECTOR(1024) 一致"
        );
        let tsv = row.tsv_source.as_deref().expect("tsv 必须与主行同事务写入");
        assert!(!tsv.is_empty(), "tsv 为空 = BM25 那一路召回对这行永久失效");
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
            "桩必须可复现，否则测试无法断言"
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
            "不同内容应产出不同文本，否则检索会把所有图片视为同一张"
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
        // 少任何一类，这张图将来就少一条能被找到的路。
        for signal in ["类型", "文字", "实体", "内容"] {
            assert!(
                VISION_SYSTEM.contains(signal),
                "prompt 必须索取「{signal}」，否则产出的描述在检索里没有这条信号"
            );
        }
        assert!(
            VISION_SYSTEM.contains("这是一张图片"),
            "prompt 必须**点名**禁止零信息的描述——只说「要详细」模型仍会写废话"
        );
        assert!(
            VISION_SYSTEM.contains("中文"),
            "库里是 jieba + bge-m3，输出英文会在 BM25 与向量两条路上同时打折"
        );
    }

    #[test]
    fn vision_transcriber_refuses_a_blind_model() {
        // DeepSeek 一个 vision 模型都没有。构造期就该失败，
        // 而不是等到线上第一张图进来才发现。
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
            .expect_err("不支持 vision 的模型不能当转录器");
        assert!(err.to_string().contains("deepseek-v4-pro"), "实际：{err}");
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
        let t = VisionTranscriber::new(client, "claude-opus-5").expect("Claude 支持图像");
        assert_eq!(t.kind(), TranscriptKind::VisionCaption);
        assert_eq!(t.transcribed_by(), "anthropic/claude-opus-5");
        assert!(t.supports("image/png") && !t.supports("image/bmp"));
    }
}

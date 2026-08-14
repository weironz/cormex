//! Provider-layer errors.
//!
//! Things only go wrong in two places, construction and invocation: the former
//! is a configuration problem (unrecognized provider name, missing key, invalid
//! URL), and the latter passes goose's [`ProviderError`] through untouched — it
//! already distinguishes auth failure / rate limiting / context overflow /
//! network error, the layers above need that classification for their retry and
//! degradation policies, and flattening it into a string destroys it.

use goose_providers::errors::ProviderError;
use thiserror::Error;

pub type Result<T, E = LlmError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum LlmError {
    /// The configured provider is neither in the built-in definitions nor in
    /// goose's own catalog.
    #[error("未知的 LLM 供应商 `{name}`；可用：{available}")]
    UnknownProvider { name: String, available: String },

    /// The definition JSON is invalid, or building a goose Provider from it
    /// failed (missing key, invalid URL, …).
    #[error("构造供应商 `{name}` 失败：{source}")]
    Build {
        name: String,
        #[source]
        source: anyhow::Error,
    },

    /// Something went wrong calling the provider. goose's original
    /// classification is preserved.
    #[error(transparent)]
    Provider(#[from] ProviderError),

    /// The message carries an image, but the target model's definition
    /// explicitly says it does not support vision.
    ///
    /// This gets its own variant rather than being folded into `Provider`:
    /// **it is the only multimodal error that can be determined locally**, and
    /// the alternative outcome is the worst kind — the image is silently
    /// dropped and the model replies "I don't see any image attached", leaving
    /// neither the user nor the developer with any idea what happened.
    #[error(
        "模型 `{provider}/{model}` 不支持图像输入；请改用支持 vision 的模型（配置 CORTEX_VISION_PROVIDER / CORTEX_VISION_MODEL）"
    )]
    VisionUnsupported { provider: String, model: String },

    /// The image MIME type is not in the intersection of the vendors' vision
    /// APIs.
    #[error("不支持的图像格式 `{mime}`；可用：{supported}")]
    UnsupportedImageMime { mime: String, supported: String },

    /// A single image exceeds the size ceiling.
    #[error("图像 {bytes} 字节，超过上限 {limit} 字节；请在上传时生成缩略图")]
    ImageTooLarge { bytes: usize, limit: usize },

    /// The image payload itself is invalid (empty bytes, bad base64, malformed
    /// data URL).
    #[error("非法的图像内容：{0}")]
    InvalidImage(String),
}

impl From<LlmError> for cortex_core::CortexError {
    fn from(err: LlmError) -> Self {
        match err {
            // A misspelled provider name and a wrongly chosen model (no vision
            // support) are both configuration problems, not a 502.
            LlmError::UnknownProvider { .. } | LlmError::VisionUnsupported { .. } => {
                Self::Config(err.to_string())
            }
            // An invalid image means **what the caller sent** is wrong, which
            // maps to 4xx rather than 5xx.
            LlmError::UnsupportedImageMime { .. }
            | LlmError::ImageTooLarge { .. }
            | LlmError::InvalidImage(_) => Self::Invalid(err.to_string()),
            other => Self::Provider(other.to_string()),
        }
    }
}

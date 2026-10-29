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
    #[error("unknown LLM provider `{name}`; available: {available}")]
    UnknownProvider { name: String, available: String },

    /// The definition JSON is invalid, or building a goose Provider from it
    /// failed (missing key, invalid URL, …).
    #[error("failed to build provider `{name}`: {source}")]
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
        "model `{provider}/{model}` does not accept image input; switch to a vision-capable model (set CORMEX_VISION_PROVIDER / CORMEX_VISION_MODEL)"
    )]
    VisionUnsupported { provider: String, model: String },

    /// The image MIME type is not in the intersection of the vendors' vision
    /// APIs.
    #[error("unsupported image format `{mime}`; available: {supported}")]
    UnsupportedImageMime { mime: String, supported: String },

    /// A single image exceeds the size ceiling.
    #[error(
        "image is {bytes} bytes, over the {limit} byte ceiling; generate a thumbnail at upload time"
    )]
    ImageTooLarge { bytes: usize, limit: usize },

    /// The image payload itself is invalid (empty bytes, bad base64, malformed
    /// data URL).
    #[error("invalid image content: {0}")]
    InvalidImage(String),
}

impl From<LlmError> for cormex_core::CormexError {
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

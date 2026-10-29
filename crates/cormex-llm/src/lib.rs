//! The Cormex provider layer: one interface for calling any LLM, streaming the
//! reply back.
//!
//! This layer contains almost no adapter code — the provider differences are
//! all vendored from goose (Apache-2.0,
//! <https://github.com/aaif-goose/goose>), specifically
//! `goose-provider-types` and `goose-providers`. Those two crates do not depend
//! on goose-core, and they already handle each vendor's prompt caching
//! semantics, thinking/reasoning opaque blocks, request/response format
//! conversion, rate-limit backoff, and retries.
//!
//! Cormex adds only four things on top:
//! 1. A declarative set of provider definitions (`src/definitions/*.json`) that
//!    pin down the models Cormex actually uses, their context limits, and
//!    **vision capability**, so none of it drifts with a goose release;
//! 2. [`LlmClient`], which binds the Cormex notion of "provider + main model +
//!    cheap model";
//! 3. Key injection — supplied by the caller, so users are not forced to set
//!    `GOOSE_*` environment variables;
//! 4. [`vision`] — convenient construction of image-carrying messages from
//!    bytes, plus a capability check **before anything is sent**. goose handles
//!    the differences between each vendor's image JSON format; we only fill in
//!    what it lacks, namely "can this model actually see images at all".
//!
//! # Example
//!
//! ```no_run
//! # async fn demo() -> cormex_llm::Result<()> {
//! use cormex_llm::{LlmClient, Message};
//! use futures::StreamExt;
//!
//! let client = LlmClient::from_env()?;
//! let messages = [Message::user().with_text("Introduce yourself in one sentence")];
//! let mut stream = client
//!     .stream_text(client.model(), "You are Cormex's assistant.", &messages)
//!     .await?;
//!
//! while let Some(chunk) = stream.next().await {
//!     print!("{}", chunk?);
//! }
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod error;
pub mod provider;
pub mod vision;

pub use client::{LlmClient, TextStream};
pub use error::{LlmError, Result};
pub use vision::{
    MAX_IMAGE_BYTES, MessageImageExt, SUPPORTED_IMAGE_MIMES, VisionSupport, image_count,
    normalize_image_mime, parse_data_url, to_data_url,
};

// Re-export goose's canonical types directly. Layers above (cormex-agent /
// cormexd) use exactly these types; do not set up a parallel set inside Cormex
// — any conversion through another wrapping layer is lossy.
pub use goose_providers::base::{MessageStream, Provider};
pub use goose_providers::conversation::message::{Message, MessageContent};
pub use goose_providers::conversation::token_usage::{ProviderUsage, Usage};
pub use goose_providers::errors::ProviderError;
pub use goose_providers::model::ModelConfig;
pub use rmcp::model::Tool;

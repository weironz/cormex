//! Cortex 供应商层：统一接口调用任意 LLM，流式返回。
//!
//! 本层几乎不写适配代码 —— 供应商差异全部取件自 goose
//! （Apache-2.0，<https://github.com/aaif-goose/goose>）的
//! `goose-provider-types` 与 `goose-providers`。这两个 crate 不依赖
//! goose-core，且已经处理好各家的 prompt caching 语义、thinking/reasoning
//! 不透明块、请求/响应格式转换、限流退避与重试。
//!
//! Cortex 在其上只加四样东西：
//! 1. 一份声明式的供应商定义（`src/definitions/*.json`），把 Cortex 实际用的
//!    模型、上下文上限与 **vision 能力**写死，不随 goose 发版漂移；
//! 2. [`LlmClient`]，绑定「供应商 + 主模型 + 廉价模型」这个 Cortex 概念；
//! 3. 密钥注入 —— 由调用方传入，不强迫用户去设 `GOOSE_*` 环境变量；
//! 4. [`vision`] —— 从字节带图的便捷构造，以及**发出去之前**的能力判定。
//!    各家的图像 JSON 格式差异由 goose 处理，我们只补它没有的
//!    「这个模型到底能不能看图」。
//!
//! # 例
//!
//! ```no_run
//! # async fn demo() -> cortex_llm::Result<()> {
//! use cortex_llm::{LlmClient, Message};
//! use futures::StreamExt;
//!
//! let client = LlmClient::from_env()?;
//! let messages = [Message::user().with_text("用一句话介绍你自己")];
//! let mut stream = client
//!     .stream_text(client.model(), "你是 Cortex 的助手。", &messages)
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

// 直接透出 goose 的规范类型。上层（cortex-agent / cortexd）就用这些类型，
// 不要在 Cortex 里另立一套 —— 任何再包一层的转换都是有损的。
pub use goose_providers::base::{MessageStream, Provider};
pub use goose_providers::conversation::message::{Message, MessageContent};
pub use goose_providers::conversation::token_usage::{ProviderUsage, Usage};
pub use goose_providers::errors::ProviderError;
pub use goose_providers::model::ModelConfig;
pub use rmcp::model::Tool;

//! The single entry point for calling an LLM.
//!
//! [`LlmClient`] does only three things: pick a goose `Provider` per the
//! configuration, remember the main and cheap [`ModelConfig`]s, and hand the
//! streamed result out untouched.
//!
//! It deliberately does **not** define a message type of its own. goose's
//! [`Message`] already carries thinking / redacted-thinking opaque blocks, tool
//! calls and responses, and prompt caching markers losslessly; wrapping another
//! layer here would only lose those in conversion (see the lib.rs comments in
//! `cortex-core`).

use std::sync::Arc;

use cortex_core::config::LlmConfig;
use futures::{StreamExt, TryStreamExt};
use goose_providers::base::{MessageStream, Provider};
use goose_providers::conversation::message::Message;
use goose_providers::conversation::token_usage::ProviderUsage;
use goose_providers::model::ModelConfig;
use rmcp::model::Tool;

use crate::error::{LlmError, Result};
use crate::provider;
use crate::vision::{VisionSupport, image_count};

/// A stream of plain-text deltas. Each item is a new chunk of text the model
/// emitted (no thinking, no tool calls).
pub type TextStream = futures::stream::BoxStream<'static, Result<String>>;

/// An LLM client bound to a provider and its models.
///
/// Cloning is cheap (an `Arc` inside), so it can be dropped into handlers
/// freely.
#[derive(Clone)]
pub struct LlmClient {
    provider: Arc<dyn Provider>,
    provider_id: String,
    model: ModelConfig,
    cheap_model: ModelConfig,
}

impl LlmClient {
    /// Build from configuration. The caller resolves `api_key` however it likes
    /// (environment variable, secret manager, whatever).
    ///
    /// For providers needing no authentication (Ollama), pass an empty string.
    pub fn from_config(cfg: &LlmConfig, api_key: &str) -> Result<Self> {
        let provider = provider::build_with(&cfg.provider, api_key, cfg.base_url.as_deref())?;
        Ok(Self {
            provider: Arc::from(provider),
            model: provider::model_config(&cfg.provider, &cfg.model)?,
            cheap_model: provider::model_config(&cfg.provider, &cfg.cheap_model)?,
            provider_id: cfg.provider.clone(),
        })
    }

    /// Build from the environment, for examples, the CLI, and tests.
    ///
    /// Reads `CORTEX_LLM_PROVIDER` / `CORTEX_LLM_MODEL` /
    /// `CORTEX_LLM_CHEAP_MODEL`, and takes the key from the variable declared
    /// in that provider's definition (deepseek → `DEEPSEEK_API_KEY`). An
    /// unconfigured model falls back to the default in the definition.
    ///
    /// The server should not take this path — `cortexd` has a full
    /// [`cortex_core::Config`], so it uses [`Self::from_config`].
    pub fn from_env() -> Result<Self> {
        // Treat the empty string as unset. `env::var` returns `Ok("")` for an
        // empty string, so a bare `.ok()` lets an empty string knock out the
        // default model name — see the comments on `non_empty` in
        // cortex_core::config (that is the other half of the same bug; both
        // sites blew up together on the first production deploy)
        let non_empty = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());

        let provider_id =
            non_empty("CORTEX_LLM_PROVIDER").unwrap_or_else(|| "deepseek".to_string());
        let (default_model, default_cheap) = provider::default_models(&provider_id)?;

        let pick = |key: &str, fallback: Option<String>| -> Result<String> {
            non_empty(key).or(fallback).ok_or_else(|| LlmError::Build {
                name: provider_id.clone(),
                source: anyhow::anyhow!("未设置 {key}，且该供应商定义里没有默认模型"),
            })
        };

        let cfg = LlmConfig {
            model: pick("CORTEX_LLM_MODEL", default_model)?,
            cheap_model: pick("CORTEX_LLM_CHEAP_MODEL", default_cheap)?,
            provider: provider_id.clone(),
            // A directly-connected local agent must also be able to point at a
            // self-hosted endpoint — it reads the same variable the server
            // does, so both sides are configured identically and nobody has to
            // remember two sets of names
            base_url: std::env::var("CORTEX_LLM_BASE_URL")
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
        };

        // An empty api_key_env = this provider needs no authentication.
        let key_var = provider::api_key_env(&provider_id)?;
        let api_key = if key_var.is_empty() {
            String::new()
        } else {
            std::env::var(&key_var).map_err(|_| LlmError::Build {
                name: provider_id.clone(),
                source: anyhow::anyhow!("缺少环境变量 {key_var}"),
            })?
        };

        Self::from_config(&cfg, &api_key)
    }

    /// Assemble from an existing [`Provider`], bypassing the provider registry.
    ///
    /// # Why this path is needed
    ///
    /// Once the agent loop moved local, the local process by default does
    /// **not** connect directly to any provider; it sends requests to cortexd's
    /// `/llm/stream` (the key lives in one place on the server, so multi-device
    /// setups need not configure each machine). The "provider" on that path is
    /// a `Provider` implementation that speaks HTTP, and the registry does not
    /// have it.
    ///
    /// The `provider_id` and the two `ModelConfig`s that come in this way are
    /// the caller's responsibility — they are used only for display and context
    /// window estimation, and take no part in routing.
    #[must_use]
    pub fn from_provider(
        provider: Arc<dyn Provider>,
        provider_id: impl Into<String>,
        model: ModelConfig,
        cheap_model: ModelConfig,
    ) -> Self {
        Self {
            provider,
            provider_id: provider_id.into(),
            model,
            cheap_model,
        }
    }

    /// The provider identifier, e.g. `deepseek`.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// The main conversation model.
    #[must_use]
    pub fn model(&self) -> &ModelConfig {
        &self.model
    }

    /// The cheap model for background work like extraction and summarization.
    #[must_use]
    pub fn cheap_model(&self) -> &ModelConfig {
        &self.cheap_model
    }

    /// The underlying goose `Provider`, for callers that need goose's native
    /// capabilities (listing models, querying limits, …).
    #[must_use]
    pub fn provider(&self) -> &dyn Provider {
        self.provider.as_ref()
    }

    /// Assemble a [`ModelConfig`] for any model under this provider.
    ///
    /// The transcription pipeline needs it: the vision model is not necessarily
    /// the main conversation model (DeepSeek has no vision model at all, so
    /// transcription has to point at another vendor).
    ///
    /// # Errors
    ///
    /// The provider definition fails to parse. A model name absent from the
    /// definition is **not** an error — the context limit falls back to goose's
    /// canonical table, which is exactly what dynamic model lists like Ollama's
    /// need.
    pub fn model_config(&self, model: &str) -> Result<ModelConfig> {
        provider::model_config(&self.provider_id, model)
    }

    /// Whether a given model can see images. See [`VisionSupport`].
    #[must_use]
    pub fn vision_support(&self, model: &ModelConfig) -> VisionSupport {
        provider::vision_support(&self.provider_id, &model.model_name)
    }

    /// Whether the main model can see images.
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.vision_support(&self.model).allows_images()
    }

    /// The last gate before an image-carrying request goes out.
    ///
    /// # Why block at all
    ///
    /// Not blocking does not produce an error, it produces **silent loss**:
    /// goose serializes the image block out as usual, and an endpoint without
    /// vision support either ignores it or returns a vague 400, while what the
    /// user sees is the assistant saying "I don't see any image attached". For
    /// this class of problem, no log line anywhere points at the real cause.
    ///
    /// It only refuses when support is **explicitly declared** absent; anything
    /// undeclared is let through (see [`VisionSupport`]).
    ///
    /// # Errors
    ///
    /// [`LlmError::VisionUnsupported`].
    pub fn ensure_can_see(&self, model: &ModelConfig, messages: &[Message]) -> Result<()> {
        let images = image_count(messages);
        if images == 0 {
            return Ok(());
        }
        let support = self.vision_support(model);
        if support.allows_images() {
            if support == VisionSupport::Unknown {
                // Let through, but leave a trace: when someone later
                // investigates "the image was sent and nothing happened", this
                // line is what distinguishes "we let it through" from "we
                // blocked it"
                tracing::debug!(
                    provider = %self.provider_id,
                    model = %model.model_name,
                    images,
                    "模型的 vision 能力未声明，按放行处理，由供应商自行报错"
                );
            }
            return Ok(());
        }
        Err(LlmError::VisionUnsupported {
            provider: self.provider_id.clone(),
            model: model.model_name.clone(),
        })
    }

    /// Stream a conversation with the main model.
    ///
    /// The stream carries both text deltas and **complete** tool calls (goose
    /// guarantees a tool call is yielded only once fully assembled, never as
    /// half a JSON document), with token usage attached at the end. The agent
    /// loop uses this one.
    pub async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream> {
        self.stream_with(&self.model, system, messages, tools).await
    }

    /// Stream a conversation with the cheap model.
    pub async fn stream_cheap(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream> {
        self.stream_with(&self.cheap_model, system, messages, tools)
            .await
    }

    /// Stream a conversation with a specified model. Retries and rate-limit
    /// backoff are handled by goose internally.
    ///
    /// Image-carrying messages pass through [`Self::ensure_can_see`] first.
    pub async fn stream_with(
        &self,
        model: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream> {
        self.ensure_can_see(model, messages)?;
        Ok(self.provider.stream(model, system, messages, tools).await?)
    }

    /// A simplified stream of text deltas only: usage, tool calls, and thinking
    /// blocks are discarded.
    ///
    /// Suits "have the model write a passage" style uses. For the agent loop,
    /// use [`Self::stream`].
    pub async fn stream_text(
        &self,
        model: &ModelConfig,
        system: &str,
        messages: &[Message],
    ) -> Result<TextStream> {
        let stream = self.stream_with(model, system, messages, &[]).await?;
        Ok(stream
            .map_err(LlmError::from)
            .try_filter_map(|(message, _usage)| async move {
                Ok(message
                    .map(|m| m.as_concat_text())
                    .filter(|text| !text.is_empty()))
            })
            .boxed())
    }

    /// Non-streaming: collect the whole turn's response before returning, with
    /// token usage attached.
    ///
    /// Image-carrying messages pass through [`Self::ensure_can_see`] first.
    pub async fn complete(
        &self,
        model: &ModelConfig,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<(Message, ProviderUsage)> {
        self.ensure_can_see(model, messages)?;
        Ok(self
            .provider
            .complete(model, system, messages, tools)
            .await?)
    }
}

impl std::fmt::Debug for LlmClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written rather than derived: Provider is a trait object with no
        // Debug; and it holds the API key inside, which must never reach a log.
        f.debug_struct("LlmClient")
            .field("provider", &self.provider_id)
            .field("model", &self.model.model_name)
            .field("cheap_model", &self.cheap_model.model_name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> LlmConfig {
        LlmConfig {
            provider: "deepseek".to_string(),
            model: "deepseek-v4-pro".to_string(),
            cheap_model: "deepseek-v4-flash".to_string(),
            base_url: None,
        }
    }

    #[test]
    fn builds_from_config_with_explicit_key() {
        let client = LlmClient::from_config(&config(), "test-key").expect("should build");
        assert_eq!(client.provider_id(), "deepseek");
        assert_eq!(client.model().model_name, "deepseek-v4-pro");
        assert_eq!(client.cheap_model().context_limit(), 128_000);
    }

    #[test]
    fn debug_does_not_leak_the_key() {
        let client = LlmClient::from_config(&config(), "super-secret").expect("should build");
        assert!(!format!("{client:?}").contains("super-secret"));
    }

    /// The smallest valid PNG: 1×1, fully transparent.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    fn with_image() -> Message {
        use crate::vision::MessageImageExt;
        Message::user()
            .with_text("这张图里有什么")
            .with_image_bytes(PNG_1X1, "image/png")
            .expect("a valid PNG")
    }

    #[test]
    fn deepseek_rejects_images_instead_of_dropping_them() {
        let client = LlmClient::from_config(&config(), "k").unwrap();
        assert!(
            !client.supports_vision(),
            "the deepseek definition explicitly says vision:false — this \
             assertion will fail the day DeepSeek ships a vision model, and \
             that is exactly the moment to update the definition"
        );
        let err = client
            .ensure_can_see(client.model(), &[with_image()])
            .expect_err("an image must be blocked");
        assert!(matches!(err, LlmError::VisionUnsupported { .. }));
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek-v4-pro") && msg.contains("CORTEX_VISION"),
            "the error must state both which model it was and how to fix it; \
             got: {msg}"
        );
    }

    #[test]
    fn text_only_conversations_are_never_blocked() {
        let client = LlmClient::from_config(&config(), "k").unwrap();
        client
            .ensure_can_see(client.model(), &[Message::user().with_text("纯文本")])
            .expect("with no image, vision capability should not matter");
    }

    #[test]
    fn vision_capable_provider_passes_the_guard() {
        let cfg = LlmConfig {
            provider: "anthropic".to_string(),
            model: "claude-opus-5".to_string(),
            cheap_model: "claude-haiku-4-5-20251001".to_string(),
            base_url: None,
        };
        let client = LlmClient::from_config(&cfg, "k").unwrap();
        assert!(client.supports_vision());
        client
            .ensure_can_see(client.model(), &[with_image()])
            .expect("the whole Claude line supports images");
    }

    #[test]
    fn undeclared_capability_is_not_a_hard_stop() {
        // Ollama's model list is decided by the local machine, so `models` is
        // empty in the definition → Unknown → let through. Blocking here would
        // mean "the user installed llava and still cannot use it".
        let cfg = LlmConfig {
            provider: "ollama".to_string(),
            model: "gemma4:latest".to_string(),
            cheap_model: "gemma4:latest".to_string(),
            base_url: None,
        };
        let client = LlmClient::from_config(&cfg, "").unwrap();
        assert_eq!(
            client.vision_support(client.model()),
            VisionSupport::Unknown
        );
        client
            .ensure_can_see(client.model(), &[with_image()])
            .expect("undeclared capability should be let through and left for Ollama to reject");
    }

    #[test]
    fn model_config_can_name_a_sibling_model() {
        let client = LlmClient::from_config(&config(), "k").unwrap();
        let cfg = client.model_config("deepseek-reasoner").unwrap();
        assert_eq!(cfg.model_name, "deepseek-reasoner");
        assert_eq!(cfg.context_limit(), 128_000);
    }

    #[test]
    fn unknown_provider_is_rejected() {
        let cfg = LlmConfig {
            provider: "definitely-not-a-provider".to_string(),
            ..config()
        };
        assert!(matches!(
            LlmClient::from_config(&cfg, "k"),
            Err(LlmError::UnknownProvider { .. })
        ));
    }
}

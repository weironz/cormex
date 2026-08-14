//! 统一的 LLM 调用入口。
//!
//! [`LlmClient`] 只做三件事：按配置挑一个 goose `Provider`、记住主/廉价两个
//! [`ModelConfig`]、把流式结果原样交出去。
//!
//! 刻意**不**定义自己的消息类型。goose 的 [`Message`] 已经无损承载了
//! thinking / redacted-thinking 不透明块、工具调用与响应、prompt caching 标记；
//! 在这里再包一层只会在转换中丢掉这些东西（见 `cortex-core` 的 lib.rs 注释）。

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

/// 纯文本增量流。每一项是模型新吐出的一段文本（不含 thinking、不含工具调用）。
pub type TextStream = futures::stream::BoxStream<'static, Result<String>>;

/// 一个绑定了供应商与模型的 LLM 客户端。
///
/// 克隆代价很低（内部 `Arc`），可以随手塞进各个 handler。
#[derive(Clone)]
pub struct LlmClient {
    provider: Arc<dyn Provider>,
    provider_id: String,
    model: ModelConfig,
    cheap_model: ModelConfig,
}

impl LlmClient {
    /// 按配置构造。`api_key` 由调用方解析（环境变量、密钥托管……随意）。
    ///
    /// 免鉴权的供应商（Ollama）传空串即可。
    pub fn from_config(cfg: &LlmConfig, api_key: &str) -> Result<Self> {
        let provider = provider::build_with(&cfg.provider, api_key, cfg.base_url.as_deref())?;
        Ok(Self {
            provider: Arc::from(provider),
            model: provider::model_config(&cfg.provider, &cfg.model)?,
            cheap_model: provider::model_config(&cfg.provider, &cfg.cheap_model)?,
            provider_id: cfg.provider.clone(),
        })
    }

    /// 从环境变量构造，供 example、CLI 与测试用。
    ///
    /// 读 `CORTEX_LLM_PROVIDER` / `CORTEX_LLM_MODEL` / `CORTEX_LLM_CHEAP_MODEL`，
    /// 密钥读该供应商定义里声明的变量（deepseek → `DEEPSEEK_API_KEY`）。
    /// 模型没配就取定义里的默认值。
    ///
    /// 服务端不该走这条路 —— `cortexd` 有完整的 [`cortex_core::Config`]，
    /// 用 [`Self::from_config`]。
    pub fn from_env() -> Result<Self> {
        // 空串按「没设」处理。`env::var` 对空串返回 `Ok("")`，直接 `.ok()`
        // 会让一个空串顶掉默认模型名 —— 详见 cortex_core::config 里
        // `non_empty` 的注释（那是同一个 bug 的另一半，第一次上生产时
        // 两处一起炸的）
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
            // 本地 agent 直连时也要能指向自建端点 —— 与服务端读的是同一个变量，
            // 两侧配法一致，不必记两套名字
            base_url: std::env::var("CORTEX_LLM_BASE_URL")
                .ok()
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
        };

        // api_key_env 为空 = 该供应商免鉴权。
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

    /// 用一个现成的 [`Provider`] 组装，不经供应商注册表。
    ///
    /// # 为什么需要这条路
    ///
    /// agent 循环搬到本地后，本地进程默认**不直连**任何供应商，而是把请求
    /// 打到 cortexd 的 `/llm/stream`（key 只在服务端一处，多设备不用每台配一遍）。
    /// 那条路上的「供应商」是一个讲 HTTP 的 `Provider` 实现，注册表里没有它。
    ///
    /// 走这条路进来的 `provider_id` 与两个 `ModelConfig` 由调用方负责 ——
    /// 它们只用于展示与上下文窗口估算，不参与路由。
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

    /// 供应商标识，如 `deepseek`。
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// 主对话模型。
    #[must_use]
    pub fn model(&self) -> &ModelConfig {
        &self.model
    }

    /// 抽取、摘要等后台任务用的廉价模型。
    #[must_use]
    pub fn cheap_model(&self) -> &ModelConfig {
        &self.cheap_model
    }

    /// 底层 goose `Provider`，留给需要 goose 原生能力的调用方（列模型、查上限……）。
    #[must_use]
    pub fn provider(&self) -> &dyn Provider {
        self.provider.as_ref()
    }

    /// 为本供应商下的任意模型组装 [`ModelConfig`]。
    ///
    /// 转录 pipeline 需要它：vision 模型未必是对话主模型
    /// （DeepSeek 一个 vision 模型都没有，转录只能指到别家去）。
    ///
    /// # Errors
    ///
    /// 供应商定义解析失败。模型名不在定义里**不算错**——
    /// 上下文上限会回落到 goose 的 canonical 表，这正是 Ollama 那类
    /// 动态模型列表所需要的。
    pub fn model_config(&self, model: &str) -> Result<ModelConfig> {
        provider::model_config(&self.provider_id, model)
    }

    /// 某个模型能不能看图。见 [`VisionSupport`]。
    #[must_use]
    pub fn vision_support(&self, model: &ModelConfig) -> VisionSupport {
        provider::vision_support(&self.provider_id, &model.model_name)
    }

    /// 主模型能不能看图。
    #[must_use]
    pub fn supports_vision(&self) -> bool {
        self.vision_support(&self.model).allows_images()
    }

    /// 带图的请求发出去之前的最后一道闸。
    ///
    /// # 为什么要拦
    ///
    /// 不拦的结局不是报错，是**静默丢失**：goose 会照常把 image 块序列化
    /// 出去，不支持 vision 的端点要么忽略它、要么回一个含糊的 400，
    /// 而用户看到的是助手说「我没有看到你附上任何图片」。
    /// 这类问题没有任何一处日志会指向真正的原因。
    ///
    /// 只在**显式声明**不支持时拒绝；未声明一律放行（见 [`VisionSupport`]）。
    ///
    /// # Errors
    ///
    /// [`LlmError::VisionUnsupported`]。
    pub fn ensure_can_see(&self, model: &ModelConfig, messages: &[Message]) -> Result<()> {
        let images = image_count(messages);
        if images == 0 {
            return Ok(());
        }
        let support = self.vision_support(model);
        if support.allows_images() {
            if support == VisionSupport::Unknown {
                // 放行了，但要留痕：将来排查「图发过去没反应」时，
                // 这一行是判断「我们放行 vs 我们拦下」的分界
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

    /// 用主模型流式对话。
    ///
    /// 流里既有文本增量，也有**完整**的工具调用（goose 保证工具调用只在拼完后
    /// 才 yield，不会吐半截 JSON），末尾附带 token 用量。agent 循环用这个。
    pub async fn stream(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream> {
        self.stream_with(&self.model, system, messages, tools).await
    }

    /// 用廉价模型流式对话。
    pub async fn stream_cheap(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[Tool],
    ) -> Result<MessageStream> {
        self.stream_with(&self.cheap_model, system, messages, tools)
            .await
    }

    /// 指定模型流式对话。重试与限流退避由 goose 在内部处理。
    ///
    /// 带图的消息会先过 [`Self::ensure_can_see`]。
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

    /// 只要文本增量的简化流：丢掉用量、工具调用与 thinking 块。
    ///
    /// 适合「让模型写一段话」这类场景。要走 agent 循环请用 [`Self::stream`]。
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

    /// 非流式：把整轮响应收完再返回，附带 token 用量。
    ///
    /// 带图的消息会先过 [`Self::ensure_can_see`]。
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
        // 手写而非 derive：Provider 是 trait object，没有 Debug；
        // 而且它内部握着 API 密钥，绝不能进日志。
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
        let client = LlmClient::from_config(&config(), "test-key").expect("应构造成功");
        assert_eq!(client.provider_id(), "deepseek");
        assert_eq!(client.model().model_name, "deepseek-v4-pro");
        assert_eq!(client.cheap_model().context_limit(), 128_000);
    }

    #[test]
    fn debug_does_not_leak_the_key() {
        let client = LlmClient::from_config(&config(), "super-secret").expect("应构造成功");
        assert!(!format!("{client:?}").contains("super-secret"));
    }

    /// 最小合法 PNG：1×1 全透明。
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
            .expect("合法 PNG")
    }

    #[test]
    fn deepseek_rejects_images_instead_of_dropping_them() {
        let client = LlmClient::from_config(&config(), "k").unwrap();
        assert!(
            !client.supports_vision(),
            "deepseek 定义里已明写 vision:false —— 这条断言在 DeepSeek 出 vision 模型时会失败，那正是该更新定义的时刻"
        );
        let err = client
            .ensure_can_see(client.model(), &[with_image()])
            .expect_err("带图必须被拦下");
        assert!(matches!(err, LlmError::VisionUnsupported { .. }));
        let msg = err.to_string();
        assert!(
            msg.contains("deepseek-v4-pro") && msg.contains("CORTEX_VISION"),
            "错误必须同时说清是哪个模型、以及怎么改；实际：{msg}"
        );
    }

    #[test]
    fn text_only_conversations_are_never_blocked() {
        let client = LlmClient::from_config(&config(), "k").unwrap();
        client
            .ensure_can_see(client.model(), &[Message::user().with_text("纯文本")])
            .expect("不带图就不该受 vision 能力影响");
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
            .expect("Claude 全系支持图像");
    }

    #[test]
    fn undeclared_capability_is_not_a_hard_stop() {
        // Ollama 的模型列表由本机决定，定义里 models 为空 → Unknown → 放行。
        // 在这里拦下等于「用户装了 llava 也用不了」。
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
            .expect("未声明能力应放行，由 Ollama 自己报错");
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

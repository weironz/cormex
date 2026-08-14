//! `POST /llm/stream` —— LLM 代理的线协议。
//!
//! # 为什么不做 OpenAI 兼容层
//!
//! 「让 cortexd 讲 `/v1/chat/completions`，本地那侧就能用任何现成客户端」
//! 听着很省事，代价是把 goose 的 [`Message`] 翻译成 OpenAI 的形状再翻回来。
//! 那次往返会丢掉 **prompt caching 标记与 thinking 块** —— 这是整套里最贵的
//! 两样东西，而丢了之后没有任何症状，只是账单变贵、推理变差。
//!
//! 所以这条线上直接传 goose 的原生类型。两侧编译自同一个 workspace、
//! 同一个 goose 版本，直传是无损的；版本对不上时是 JSON 反序列化失败，
//! 当场就炸，而不是悄悄降级。
//!
//! # 客户端不能点名模型
//!
//! 请求里只有 [`ModelTier`]（主 / 廉价），没有模型名。这不是懒 ——
//! 让客户端指定模型等于让它**拿服务端的 key 花服务端的钱**跑任意模型，
//! 而账单是服务端付。
//!
//! （goose 的 `ModelConfig` 其实是可反序列化的 —— derive 列表里只有
//! `Serialize`，但下面另有一份手写的 `Deserialize`。所以这是一条**刻意的
//! 限制**，不是「传不过来」。上一版注释把它写成了后者，是错的。）

use cortex_llm::{Message, ProviderUsage, Tool};
use serde::{Deserialize, Serialize};

/// 用哪一档模型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// 主对话模型 —— agent 循环用这个
    #[default]
    Main,
    /// 抽取、摘要等后台任务用的廉价模型
    Cheap,
}

/// `POST /llm/stream` 的请求体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStreamRequest {
    #[serde(default)]
    pub tier: ModelTier,
    pub system: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<Tool>,
}

/// SSE 上的一个数据事件 —— 与 goose `MessageStream` 的 `Ok` 项一一对应。
///
/// 两个字段都可能为空（goose 会单独吐一条只带用量的尾项），所以不用元组：
/// 元组的 `[null, {...}]` 读起来全靠位置，而字段名让「这条是用量」自解释。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStreamChunk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderUsage>,
}

/// SSE 上的错误事件。
///
/// goose 的 `ProviderError` 没有 serde，所以这里是它的线上镜像。
/// [`kind`](Self::kind) 保留变体名而不是只传一句话：本地那侧要据此判断
/// 「该不该重试」「是不是上下文超了」，而一句本地化的错误消息判断不了。
///
/// 变体带的额外数据（限流的重试间隔、额度耗尽的充值链接、拒答的分类）
/// 一并过线 —— 丢了它们，本地就只能当成一句泛泛的失败。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStreamError {
    /// `ProviderError` 的变体名，如 `rate_limit_exceeded`
    pub kind: String,
    pub message: String,
    /// 仅 `rate_limit_exceeded`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_delay_ms: Option<u64>,
    /// 仅 `credits_exhausted`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_up_url: Option<String>,
    /// 仅 `refusal`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

impl LlmStreamError {
    /// 服务端方向：`ProviderError` → 线上形态。
    ///
    /// `match` 刻意穷举。goose 将来加一个变体时，**这里编译不过** ——
    /// 那正是想要的：新变体静默塌成 `execution_error` 的话，
    /// 本地那侧对它的判断从第一天起就是错的，而没有任何人会发现。
    #[must_use]
    pub fn from_provider(err: &cortex_llm::ProviderError) -> Self {
        use cortex_llm::ProviderError as E;
        let base = |kind: &str, message: String| Self {
            kind: kind.to_string(),
            message,
            retry_delay_ms: None,
            top_up_url: None,
            category: None,
        };
        match err {
            E::Authentication(m) => base("authentication", m.clone()),
            E::ContextLengthExceeded(m) => base("context_length_exceeded", m.clone()),
            E::RateLimitExceeded {
                details,
                retry_delay,
            } => Self {
                retry_delay_ms: retry_delay.map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64),
                ..base("rate_limit_exceeded", details.clone())
            },
            E::ServerError(m) => base("server_error", m.clone()),
            E::NetworkError(m) => base("network_error", m.clone()),
            E::RequestFailed(m) => base("request_failed", m.clone()),
            E::ExecutionError(m) => base("execution_error", m.clone()),
            E::UsageError(m) => base("usage_error", m.clone()),
            E::NotImplemented(m) => base("not_implemented", m.clone()),
            E::EndpointNotFound(m) => base("endpoint_not_found", m.clone()),
            E::CreditsExhausted {
                details,
                top_up_url,
            } => Self {
                top_up_url: top_up_url.clone(),
                ..base("credits_exhausted", details.clone())
            },
            E::Refusal { details, category } => Self {
                category: category.clone(),
                ..base("refusal", details.clone())
            },
        }
    }

    /// 客户端方向：线上形态 → `ProviderError`。
    ///
    /// 认不出的 `kind` 落到 `ExecutionError` 并在消息里**带上原始 kind** ——
    /// 服务端比自己新时会走到这里，而「代理回了一个我不认识的错误」
    /// 必须能从日志里读出来，不能被抹成一句泛泛的执行失败。
    #[must_use]
    pub fn into_provider(self) -> cortex_llm::ProviderError {
        use cortex_llm::ProviderError as E;
        match self.kind.as_str() {
            "authentication" => E::Authentication(self.message),
            "context_length_exceeded" => E::ContextLengthExceeded(self.message),
            "rate_limit_exceeded" => E::RateLimitExceeded {
                details: self.message,
                retry_delay: self.retry_delay_ms.map(std::time::Duration::from_millis),
            },
            "server_error" => E::ServerError(self.message),
            "network_error" => E::NetworkError(self.message),
            "request_failed" => E::RequestFailed(self.message),
            "execution_error" => E::ExecutionError(self.message),
            "usage_error" => E::UsageError(self.message),
            "not_implemented" => E::NotImplemented(self.message),
            "endpoint_not_found" => E::EndpointNotFound(self.message),
            "credits_exhausted" => E::CreditsExhausted {
                details: self.message,
                top_up_url: self.top_up_url,
            },
            "refusal" => E::Refusal {
                details: self.message,
                category: self.category,
            },
            unknown => E::ExecutionError(format!(
                "cortexd 回了一个本地不认识的错误类型 `{unknown}`（两侧版本可能不一致）：{}",
                self.message
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cortex_llm::ProviderError as E;
    use std::time::Duration;

    /// 每个变体都要能原样往返。
    ///
    /// 这条是 [`LlmStreamError`] 存在的全部意义：代理那一跳不该改变
    /// 「出了什么错」。少一个变体的症状是本地把限流当成普通失败 ——
    /// 不重试、直接报错，而用户只看到「模型出错了」。
    #[test]
    fn every_provider_error_survives_the_round_trip() {
        let cases = vec![
            E::Authentication("a".into()),
            E::ContextLengthExceeded("b".into()),
            E::RateLimitExceeded {
                details: "c".into(),
                retry_delay: Some(Duration::from_millis(1500)),
            },
            E::RateLimitExceeded {
                details: "c2".into(),
                retry_delay: None,
            },
            E::ServerError("d".into()),
            E::NetworkError("e".into()),
            E::RequestFailed("f".into()),
            E::ExecutionError("g".into()),
            E::UsageError("h".into()),
            E::NotImplemented("i".into()),
            E::EndpointNotFound("j".into()),
            E::CreditsExhausted {
                details: "k".into(),
                top_up_url: Some("https://example.invalid/top-up".into()),
            },
            E::Refusal {
                details: "l".into(),
                category: Some("safety".into()),
            },
        ];

        for original in cases {
            let wire = LlmStreamError::from_provider(&original);
            // 真的过一遍 JSON，而不是只在内存里转一圈 ——
            // `skip_serializing_if` 写错的话只有序列化才看得出来
            let json = serde_json::to_string(&wire).expect("线上形态必须可序列化");
            let back: LlmStreamError = serde_json::from_str(&json).expect("必须可反序列化");
            let restored = back.into_provider();
            assert_eq!(
                restored, original,
                "错误变体在代理那一跳被改写了。原：{original:?}\n线上：{json}\n还原：{restored:?}"
            );
        }
    }

    /// 认不出的 kind 必须把原始 kind 留在消息里。
    ///
    /// 这是「服务端比本地新」时唯一的线索。抹掉它，排查就只剩
    /// 一句「执行错误」，谁也想不到是版本不一致。
    #[test]
    fn an_unknown_kind_keeps_the_original_name_in_the_message() {
        let wire = LlmStreamError {
            kind: "quantum_flux_error".into(),
            message: "原始消息".into(),
            retry_delay_ms: None,
            top_up_url: None,
            category: None,
        };
        let restored = wire.into_provider();
        let text = restored.to_string();
        assert!(
            text.contains("quantum_flux_error") && text.contains("原始消息"),
            "认不出的错误类型必须把原始 kind 与原始消息都带上，实际：{text}"
        );
    }

    /// 只带用量的尾项：`message` 缺席时不能反序列化失败。
    ///
    /// goose 的流末尾就是这个形状。要是 `message` 被当成必填，
    /// 整轮对话会在**最后一条**上炸掉 —— 前面所有字都已经吐给用户了。
    #[test]
    fn a_usage_only_chunk_deserialises() {
        let json = r#"{"usage":{"model":"m","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#;
        let chunk: LlmStreamChunk =
            serde_json::from_str(json).expect("只带用量的尾项必须能反序列化");
        assert!(chunk.message.is_none(), "这一项本来就没有 message");
        assert!(
            chunk.usage.is_some(),
            "用量必须解出来，否则计费与上限估算全瞎"
        );
    }
}

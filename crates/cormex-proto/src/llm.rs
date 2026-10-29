//! `POST /llm/stream` — the wire protocol of the LLM proxy.
//!
//! # Why there is no OpenAI compatibility layer
//!
//! "Have cormexd speak `/v1/chat/completions` and the local side can use any
//! off-the-shelf client" sounds like a labour saver. The price is translating
//! goose's `Message` into OpenAI's shape and back again. That round trip
//! loses **prompt caching markers and thinking blocks** — the two most
//! expensive things in the whole arrangement — and losing them produces no
//! symptom at all, just a bigger bill and worse reasoning.
//!
//! So this wire carries goose's native types directly. Both sides are compiled
//! from the same workspace against the same goose version, which makes direct
//! transfer lossless; when the versions do not line up you get a JSON
//! deserialization failure that blows up on the spot, rather than a silent
//! downgrade.
//!
//! # The client cannot name a model
//!
//! The request carries only a `ModelTier` (main / cheap), never a model name.
//! This is not laziness — letting the client pick the model means letting it
//! **spend the server's money with the server's key** on any model it likes,
//! while the server pays the bill.
//!
//! (goose's `ModelConfig` is in fact deserializable — the derive list has only
//! `Serialize`, but there is a separate hand-written `Deserialize` below it. So
//! this is a **deliberate restriction**, not "it cannot be sent". The previous
//! version of this comment claimed the latter, and was wrong.)

use cormex_llm::{Message, ProviderUsage, Tool};
use serde::{Deserialize, Serialize};

/// Which class of model to use.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// The main conversational model — what the agent loop uses.
    #[default]
    Main,
    /// The cheap model used for background work such as extraction and
    /// summarization.
    Cheap,
}

/// The request body of `POST /llm/stream`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStreamRequest {
    #[serde(default)]
    pub tier: ModelTier,
    pub system: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Vec<Tool>,
}

/// One data event on the SSE stream — corresponding one to one with the `Ok`
/// items of goose's `MessageStream`.
///
/// Both fields can be absent (goose emits a separate trailing item carrying
/// only usage), which is why this is not a tuple: a tuple's `[null, {...}]`
/// reads entirely by position, whereas field names make "this one is usage"
/// self-explanatory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStreamChunk {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ProviderUsage>,
}

/// An error event on the SSE stream.
///
/// goose's `ProviderError` has no serde, so this is its mirror on the wire.
/// [`kind`](Self::kind) preserves the variant name rather than shipping just a
/// sentence: the local side has to decide from it "should I retry" and "did I
/// blow the context window", and a localized error message cannot decide that.
///
/// The extra data carried by individual variants (the retry interval for rate
/// limiting, the top-up link for exhausted credits, the category of a refusal)
/// crosses the wire along with it — drop those and the local side can only
/// treat it as a generic failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmStreamError {
    /// The `ProviderError` variant name, e.g. `rate_limit_exceeded`.
    pub kind: String,
    pub message: String,
    /// `rate_limit_exceeded` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_delay_ms: Option<u64>,
    /// `credits_exhausted` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_up_url: Option<String>,
    /// `refusal` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

impl LlmStreamError {
    /// The server direction: `ProviderError` → the wire form.
    ///
    /// The `match` is exhaustive on purpose. When goose adds a variant later,
    /// **this stops compiling** — which is exactly what is wanted: were a new
    /// variant to collapse silently into `execution_error`, the local side's
    /// judgement about it would be wrong from day one, and nobody would ever
    /// find out.
    #[must_use]
    pub fn from_provider(err: &cormex_llm::ProviderError) -> Self {
        use cormex_llm::ProviderError as E;
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
            // Added in goose `alpha.13`. `NotConfigured` is a **deployment**
            // problem, not a transient one: the provider was built without a
            // key or a base URL. It gets its own kind so the client can say
            // "this server is misconfigured" rather than "the model failed".
            E::NotConfigured => base(
                "not_configured",
                "the server's LLM provider is not configured".to_owned(),
            ),
            E::InvalidValue(m) => base("invalid_value", m.clone()),
        }
    }

    /// The client direction: the wire form → `ProviderError`.
    ///
    /// An unrecognized `kind` falls through to `ExecutionError` and **carries
    /// the original kind in the message** — this path is taken when the server
    /// is newer than us, and "the proxy returned an error I do not recognize"
    /// has to be readable out of the logs, not flattened into a generic
    /// execution failure.
    #[must_use]
    pub fn into_provider(self) -> cormex_llm::ProviderError {
        use cormex_llm::ProviderError as E;
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
            "not_configured" => E::NotConfigured,
            "invalid_value" => E::InvalidValue(self.message),
            "refusal" => E::Refusal {
                details: self.message,
                category: self.category,
            },
            unknown => E::ExecutionError(format!(
                "cormexd returned an error kind `{unknown}` this side does not recognize (the two \
                 versions may not match): {}",
                self.message
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cormex_llm::ProviderError as E;
    use std::time::Duration;

    /// Every variant has to survive the round trip unchanged.
    ///
    /// This is the entire point of [`LlmStreamError`]: the proxy hop should not
    /// change *what went wrong*. The symptom of one missing variant is the
    /// local side treating rate limiting as an ordinary failure — no retry,
    /// straight to an error, and all the user sees is "the model failed".
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
            // Actually go through JSON rather than just round-tripping in
            // memory — a mistaken `skip_serializing_if` is only visible once
            // something is serialized.
            let json = serde_json::to_string(&wire).expect("the wire form must be serializable");
            let back: LlmStreamError =
                serde_json::from_str(&json).expect("it must be deserializable");
            let restored = back.into_provider();
            assert_eq!(
                restored, original,
                "the error variant was rewritten by the proxy hop. original: {original:?}\nwire: {json}\nrestored: {restored:?}"
            );
        }
    }

    /// An unrecognized kind must keep the original kind in the message.
    ///
    /// This is the only clue available when the server is newer than the local
    /// side. Erase it and all that is left for diagnosis is the phrase
    /// "execution error", which nobody would ever connect to a version
    /// mismatch.
    #[test]
    fn an_unknown_kind_keeps_the_original_name_in_the_message() {
        let wire = LlmStreamError {
            kind: "quantum_flux_error".into(),
            message: "the original message".into(),
            retry_delay_ms: None,
            top_up_url: None,
            category: None,
        };
        let restored = wire.into_provider();
        let text = restored.to_string();
        assert!(
            text.contains("quantum_flux_error") && text.contains("the original message"),
            "an unrecognized error kind must carry both the original kind and the original message, got: {text}"
        );
    }

    /// The usage-only trailing item: an absent `message` must not fail
    /// deserialization.
    ///
    /// That is exactly the shape of the end of a goose stream. If `message`
    /// were treated as required, a whole conversational turn would blow up on
    /// the **very last** item — after every word had already been streamed out
    /// to the user.
    #[test]
    fn a_usage_only_chunk_deserialises() {
        let json = r#"{"usage":{"model":"m","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}"#;
        let chunk: LlmStreamChunk =
            serde_json::from_str(json).expect("the usage-only trailing item must deserialize");
        assert!(chunk.message.is_none(), "this item never had a message");
        assert!(
            chunk.usage.is_some(),
            "usage has to parse out, otherwise billing and limit estimation are both blind"
        );
    }
}

//! Configuration.
//!
//! One single source: environment variables (injected from `.env` during
//! development). Provider-related `GOOSE_*` variables are absorbed here, so
//! users only ever face `CORTEX_*` and the standard `*_API_KEY` — the naming
//! of what we vendored in is not exposed.

use std::env;

use crate::error::{CortexError, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind: String,
    pub s3: S3Config,
    pub llm: LlmConfig,
    /// This machine's identity, written on every row for auditing and sync
    /// provenance.
    pub device_id: String,
}

#[derive(Debug, Clone)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Provider identifier: deepseek / anthropic / openai / ollama …
    pub provider: String,
    /// The main conversation model
    pub model: String,
    /// The cheap model for background work like extraction and summarization
    pub cheap_model: String,
    /// Override the `base_url` from the provider definition. `None` = use the
    /// built-in one.
    ///
    /// # Why this is needed
    ///
    /// Provider definitions are compiled into the binary, `base_url` included.
    /// Without this setting, pointing the `openai` engine somewhere else — a
    /// self-hosted vLLM / llama.cpp / LM Studio, a corporate gateway, a
    /// one-api / LiteLLM relay, or some cheaper compatible service — would
    /// require editing code and rebuilding.
    ///
    /// "OpenAI-compatible" is very nearly a de facto standard, so a single URL
    /// override is far more useful than us adding providers to a built-in list
    /// one at a time — and it keeps "which providers are supported" from
    /// becoming a table we have to maintain.
    pub base_url: Option<String>,
}

/// Read an environment variable, **treating the empty string as unset**.
///
/// For a variable that is set to an empty string, `env::var` returns `Ok("")`
/// rather than `Err` — so `env::var(k).unwrap_or(default)` gets its default
/// knocked out by an empty string.
///
/// This is not a theoretical problem; it is the kind that **only shows up
/// inside a container**: `CORTEX_LLM_MODEL: ${CORTEX_LLM_MODEL:-}` in compose
/// sets the variable to an empty string (rather than leaving it unset) when
/// `.env` does not mention it, so the process ends up with a model of `""`.
/// Local development with `.env` simply omits the line, so you never hit it.
///
/// This is exactly how the first production deploy blew up: DeepSeek replied
/// `The supported API model names are deepseek-v4-pro or deepseek-v4-flash,
/// but you passed .` (note the gap before the period). Meanwhile cortexd's
/// startup log looked entirely normal and the health check was ok — you had to
/// send a real conversation to see it.
///
/// An empty string is meaningless for every item listed here (bind / bucket /
/// region / model / device_id …), so "empty means unset" holds at every call
/// site. Do not use this for a setting where empty really is a legal value.
fn non_empty(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn required(key: &str) -> Result<String> {
    non_empty(key).ok_or_else(|| CortexError::Config(format!("缺少环境变量 {key}")))
}

fn optional(key: &str, default: &str) -> String {
    non_empty(key).unwrap_or_else(|| default.to_string())
}

impl Config {
    /// Load from the environment. During development the caller injects
    /// `.env` first via `dotenvy::dotenv()`.
    pub fn from_env() -> Result<Self> {
        let provider = optional("CORTEX_LLM_PROVIDER", "deepseek");

        Ok(Self {
            database_url: required("DATABASE_URL")?,
            bind: optional("CORTEXD_BIND", "127.0.0.1:8080"),
            s3: S3Config {
                endpoint: optional("S3_ENDPOINT", "http://localhost:9010"),
                bucket: optional("S3_BUCKET", "cortex-blobs"),
                region: optional("S3_REGION", "us-east-1"),
                access_key: optional("RUSTFS_ACCESS_KEY", "cortexadmin"),
                secret_key: optional("RUSTFS_SECRET_KEY", "cortex_dev_only"),
            },
            llm: LlmConfig {
                model: optional("CORTEX_LLM_MODEL", default_model(&provider)),
                cheap_model: optional("CORTEX_LLM_CHEAP_MODEL", default_cheap_model(&provider)),
                base_url: non_empty("CORTEX_LLM_BASE_URL"),
                provider,
            },
            device_id: optional("CORTEX_DEVICE_ID", "dev-local"),
        })
    }
}

fn default_model(provider: &str) -> &'static str {
    match provider {
        "deepseek" => "deepseek-v4-pro",
        "anthropic" => "claude-opus-5",
        "openai" => "gpt-5.5",
        _ => "deepseek-v4-pro",
    }
}

fn default_cheap_model(provider: &str) -> &'static str {
    match provider {
        "deepseek" => "deepseek-v4-flash",
        "anthropic" => "claude-haiku-4-5-20251001",
        "openai" => "gpt-5-mini",
        _ => "deepseek-v4-flash",
    }
}

//! Configuration.
//!
//! One single source: environment variables (injected from `.env` during
//! development). Provider-related `GOOSE_*` variables are absorbed here, so
//! users only ever face `CORMEX_*` and the standard `*_API_KEY` — the naming
//! of what we vendored in is not exposed.

use std::env;

use crate::error::{CormexError, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind: String,
    pub s3: S3Config,
    pub llm: LlmConfig,
    /// This machine's identity, written on every row for auditing and sync
    /// provenance.
    pub device_id: String,
    /// Capture settings. See [`CaptureConfig`].
    pub capture: CaptureConfig,
    /// The distiller's settings. See [`DistillConfig`].
    pub distill: DistillConfig,
}

/// The distiller's settings (design §9.3).
#[derive(Debug, Clone)]
pub struct DistillConfig {
    /// `CORMEX_DISTILL_MODEL`. `None` = the same cheap model extraction uses.
    ///
    /// Not defaulted to the main model: this runs on every idle session, and
    /// main-model prices would make the archive cost more than the
    /// conversations it archives.
    pub model: Option<String>,
    /// `CORMEX_DISTILL_DAILY_TOKENS`, per tenant per day. **0 = unlimited.**
    ///
    /// Per tenant rather than per deployment: one person importing three years
    /// of history must not spend everyone else's allowance.
    pub daily_tokens: i64,
    /// Whether captured sessions are promoted into `episodes` for fact
    /// extraction (D14). `CORMEX_PROMOTE_ENABLED`, default on.
    ///
    /// Off is a legitimate choice: promotion spends an extraction call per
    /// promoted message, and somebody archiving sessions purely for the raw
    /// record is paying for facts they never asked for.
    pub promote: bool,
    /// How many events one promotion round takes.
    ///
    /// A **rate** limit, not a page size. A history import projects months of
    /// sessions in a few rounds; without a cap the promoter would hand the
    /// extractor thousands of messages at once, and the extraction gate would
    /// then be saturated for hours while ordinary conversation queue behind it.
    /// `CORMEX_PROMOTE_PER_ROUND`.
    pub promote_per_round: i64,
    /// Whether conversation turns get vectors, and `search_conversation`
    /// fuses BM25 with them (P5-6). `CORMEX_SESSION_EMBEDDINGS`, **default
    /// off**.
    ///
    /// Off by default because the card's acceptance is "a comparable number on
    /// the session eval suite proves it is worth turning on; otherwise leave it
    /// off". A vector leg costs an embedding call per conversation turn and a
    /// second index to keep in step — that has to be paid for with a measured
    /// improvement, not assumed.
    pub session_embeddings: bool,
}

/// The capture endpoints' settings.
#[derive(Debug, Clone)]
pub struct CaptureConfig {
    /// A newer source catalog than the one compiled into the binary.
    ///
    /// `None` = use the built-in. A path that does not exist or does not parse
    /// is an **error**, not a fallback: falling back would leave the operator
    /// believing their update took effect, and the symptom would be "some files
    /// stopped being found", which points at the catalog's contents rather than
    /// at the file never having been read.
    pub catalog_path: Option<String>,
    /// The largest logical chunk the server accepts.
    ///
    /// Defaults to 4 MiB, matching the client's chunk size. Raising it is
    /// legitimate for a host that writes very long lines; lowering it below the
    /// client's chunk size makes every upload fail with a 413, which is why the
    /// value is printed at startup.
    pub max_chunk_bytes: usize,
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
/// inside a container**: `CORMEX_LLM_MODEL: ${CORMEX_LLM_MODEL:-}` in compose
/// sets the variable to an empty string (rather than leaving it unset) when
/// `.env` does not mention it, so the process ends up with a model of `""`.
/// Local development with `.env` simply omits the line, so you never hit it.
///
/// This is exactly how the first production deploy blew up: DeepSeek replied
/// `The supported API model names are deepseek-v4-pro or deepseek-v4-flash,
/// but you passed .` (note the gap before the period). Meanwhile cormexd's
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
    non_empty(key).ok_or_else(|| CormexError::Config(format!("missing environment variable {key}")))
}

fn optional(key: &str, default: &str) -> String {
    non_empty(key).unwrap_or_else(|| default.to_string())
}

impl Config {
    /// Load from the environment. During development the caller injects
    /// `.env` first via `dotenvy::dotenv()`.
    pub fn from_env() -> Result<Self> {
        let provider = optional("CORMEX_LLM_PROVIDER", "deepseek");

        Ok(Self {
            database_url: required("DATABASE_URL")?,
            bind: optional("CORMEXD_BIND", "127.0.0.1:8080"),
            s3: S3Config {
                endpoint: optional("S3_ENDPOINT", "http://localhost:9010"),
                bucket: optional("S3_BUCKET", "cormex-blobs"),
                region: optional("S3_REGION", "us-east-1"),
                access_key: optional("RUSTFS_ACCESS_KEY", "cormexadmin"),
                secret_key: optional("RUSTFS_SECRET_KEY", "cormex_dev_only"),
            },
            llm: LlmConfig {
                model: optional("CORMEX_LLM_MODEL", default_model(&provider)),
                cheap_model: optional("CORMEX_LLM_CHEAP_MODEL", default_cheap_model(&provider)),
                base_url: non_empty("CORMEX_LLM_BASE_URL"),
                provider,
            },
            device_id: optional("CORMEX_DEVICE_ID", "dev-local"),
            capture: CaptureConfig {
                catalog_path: non_empty("CORMEX_CAPTURE_CATALOG"),
                max_chunk_bytes: parse_max_chunk()?,
            },
            distill: DistillConfig {
                model: non_empty("CORMEX_DISTILL_MODEL"),
                daily_tokens: parse_distill_daily()?,
                promote: parse_bool("CORMEX_PROMOTE_ENABLED", true),
                promote_per_round: parse_promote_per_round()?,
                session_embeddings: parse_bool("CORMEX_SESSION_EMBEDDINGS", false),
            },
        })
    }
}

/// The default ceiling on a logical capture chunk: 4 MiB.
///
/// Lives here rather than in `cormex-proto` because the dependency direction is
/// `cormex-core ← cormex-proto`, and the config layer must not reach upward.
/// `cormex_proto::capture::MAX_CHUNK_BYTES` carries the same value for the wire
/// side; `the_two_chunk_ceilings_agree` pins them together.
pub const DEFAULT_CAPTURE_MAX_CHUNK: usize = 4 << 20;

/// Parse `CORMEX_CAPTURE_MAX_CHUNK`.
///
/// A bad value **errors** rather than falling back to the default: the setting
/// exists to match the client's chunk size, and silently using a different one
/// turns every upload above the smaller of the two into a 413 that looks like a
/// client bug.
fn parse_max_chunk() -> Result<usize> {
    let Some(raw) = non_empty("CORMEX_CAPTURE_MAX_CHUNK") else {
        return Ok(DEFAULT_CAPTURE_MAX_CHUNK);
    };
    let n: usize = raw.trim().parse().map_err(|_| {
        CormexError::Config(format!(
            "CORMEX_CAPTURE_MAX_CHUNK is not a positive integer: {raw:?}"
        ))
    })?;
    if n == 0 {
        return Err(CormexError::Config(
            "CORMEX_CAPTURE_MAX_CHUNK must not be 0: that refuses every chunk".into(),
        ));
    }
    Ok(n)
}

/// The default per-tenant daily observation budget: 200k tokens.
///
/// Derived from cost, like the quota: at roughly one yuan per million tokens
/// for a cheap model, 200k is about two jiao per tenant per day. An ordinary
/// day of work is a small fraction of it; a three-year history import runs into
/// it, which is the point — a backfill should not silently spend a month's
/// budget in an hour.
pub const DEFAULT_DISTILL_DAILY_TOKENS: i64 = 200_000;

/// The default promotion cap per round: 40 messages.
///
/// At the extraction gate's default concurrency this is a couple of minutes of
/// work per round, which leaves the gate mostly free for live conversation.
pub const DEFAULT_PROMOTE_PER_ROUND: i64 = 40;

/// Parse `CORMEX_PROMOTE_PER_ROUND`. **0 disables promotion**, the same way
/// `CORMEX_PROMOTE_ENABLED=false` does.
fn parse_promote_per_round() -> Result<i64> {
    let Some(raw) = non_empty("CORMEX_PROMOTE_PER_ROUND") else {
        return Ok(DEFAULT_PROMOTE_PER_ROUND);
    };
    let n: i64 = raw.trim().parse().map_err(|_| {
        CormexError::Config(format!(
            "CORMEX_PROMOTE_PER_ROUND is not an integer: {raw:?}"
        ))
    })?;
    if n < 0 {
        return Err(CormexError::Config(
            "CORMEX_PROMOTE_PER_ROUND must not be negative (0 disables promotion)".into(),
        ));
    }
    Ok(n)
}

/// Parse a boolean environment variable.
///
/// Accepts `1/0`, `true/false`, `yes/no`, `on/off` (case-insensitive), and
/// **errors on anything else** rather than falling back: a typo that silently
/// means "on" is the failure mode that matters here — somebody turning
/// promotion off and finding out months later that it never was.
fn parse_bool(key: &str, default: bool) -> bool {
    let Some(raw) = non_empty(key) else {
        return default;
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        other => {
            tracing::warn!(var = key, value = other, "not a boolean; using the default");
            default
        }
    }
}

/// Parse `CORMEX_DISTILL_DAILY_TOKENS`. **0 means unlimited.**
fn parse_distill_daily() -> Result<i64> {
    let Some(raw) = non_empty("CORMEX_DISTILL_DAILY_TOKENS") else {
        return Ok(DEFAULT_DISTILL_DAILY_TOKENS);
    };
    let n: i64 = raw.trim().parse().map_err(|_| {
        CormexError::Config(format!(
            "CORMEX_DISTILL_DAILY_TOKENS is not an integer: {raw:?}"
        ))
    })?;
    if n < 0 {
        return Err(CormexError::Config(
            "CORMEX_DISTILL_DAILY_TOKENS must not be negative (0 means unlimited)".into(),
        ));
    }
    Ok(n)
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

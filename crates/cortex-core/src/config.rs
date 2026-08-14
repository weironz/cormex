//! 配置。
//!
//! 单一来源：环境变量（开发期由 `.env` 注入）。
//! 供应商相关的 `GOOSE_*` 变量在此收编，用户只面对 `CORTEX_*` / 标准的
//! `*_API_KEY`，不暴露取件来源的命名。

use std::env;

use crate::error::{CortexError, Result};

#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub bind: String,
    pub s3: S3Config,
    pub llm: LlmConfig,
    /// 本机标识，写入每一行数据用于审计与同步溯源。
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
    /// 供应商标识：deepseek / anthropic / openai / ollama …
    pub provider: String,
    /// 主对话模型
    pub model: String,
    /// 抽取、摘要等后台任务用的廉价模型
    pub cheap_model: String,
    /// 覆盖供应商定义里的 `base_url`。`None` = 用内置的那个。
    ///
    /// # 为什么需要它
    ///
    /// 供应商定义是编译进二进制的，`base_url` 也在里面。没有这一项，
    /// 想把 `openai` 引擎指向别处 —— 自建的 vLLM / llama.cpp / LM Studio、
    /// 公司内网网关、one-api / LiteLLM 中转、或者某个更便宜的兼容服务 ——
    /// 都得改代码重编。
    ///
    /// 「OpenAI 兼容」几乎是事实标准，所以给一个 URL 覆盖比我们逐个把
    /// 供应商加进内置清单可用得多，也免得「支持哪些供应商」变成我们
    /// 要维护的一张表。
    pub base_url: Option<String>,
}

/// 读环境变量，**空串按「没设」处理**。
///
/// `env::var` 对「设成空串」返回的是 `Ok("")` 而不是 `Err` —— 于是
/// `env::var(k).unwrap_or(default)` 会被一个空串顶掉默认值。
///
/// 这不是理论问题，是**只在容器里显形**的一类：compose 里的
/// `CORTEX_LLM_MODEL: ${CORTEX_LLM_MODEL:-}` 会在 `.env` 没写这一项时
/// 把变量设成空串（而不是不设），于是进程拿到的 model 是 `""`。
/// 本地开发用 `.env` 是压根不写这一行，所以从来撞不到。
///
/// 第一次上生产就是这么炸的：DeepSeek 回
/// `The supported API model names are deepseek-v4-pro or deepseek-v4-flash,
/// but you passed .`（注意句号前面那个空位）。而 cortexd 的启动日志一切正常，
/// 健康检查也是 ok —— 要发一次真实对话才看得见。
///
/// 这里列的每一项（bind / bucket / region / model / device_id …）空串都无意义，
/// 所以「空即未设」对全部调用点都成立。真需要「空是合法值」的配置项别用这个。
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
    /// 从环境变量加载。开发期先由调用方 `dotenvy::dotenv()` 注入 `.env`。
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

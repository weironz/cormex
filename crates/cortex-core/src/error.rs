//! The global error type.
//!
//! Each crate defines its own errors with `thiserror`, and they all converge
//! here. Mapping happens uniformly at the boundaries (HTTP responses, CLI exit
//! codes).

use thiserror::Error;

pub type Result<T, E = CortexError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum CortexError {
    #[error("配置错误：{0}")]
    Config(String),

    #[error("存储错误：{0}")]
    Store(String),

    #[error("供应商错误：{0}")]
    Provider(String),

    #[error("记忆引擎错误：{0}")]
    Memory(String),

    #[error("找不到 {kind}：{id}")]
    NotFound { kind: &'static str, id: String },

    #[error("非法输入：{0}")]
    Invalid(String),

    /// This cannot be done **right now** — unlike "something went wrong", the
    /// caller should wait or go change configuration rather than treat it as a
    /// failed operation.
    ///
    /// Both of these belong here:
    /// - Server side: this instance lacks the capability under its current
    ///   configuration (no LLM provider on a mock backend)
    /// - Local agent: upstream cortexd is unreachable (so this turn gets queued
    ///   offline)
    ///
    /// The wording is deliberately neutral. The previous version read "this
    /// instance does not provide that capability", which was written for the
    /// first case; when the local agent used it for the second case it became
    /// "this instance does not provide that capability: cannot reach cortexd" —
    /// **the subject is wrong**, and this sentence gets shown to the user
    /// verbatim.
    #[error("服务不可用：{0}")]
    Unavailable(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl CortexError {
    /// Status-code mapping for the HTTP layer.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Self::NotFound { .. } => 404,
            Self::Invalid(_) | Self::Config(_) => 400,
            Self::Provider(_) => 502,
            Self::Unavailable(_) => 503,
            _ => 500,
        }
    }
}

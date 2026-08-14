//! 全局错误类型。
//!
//! 各 crate 用 `thiserror` 定义自己的错误，最终收敛到这里。
//! 边界处（HTTP 响应、CLI 退出码）统一映射。

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

    /// 这件事**现在**做不了 —— 与「出错了」不同，调用方该等一等或去改配置，
    /// 而不是把它当成一次失败的操作。
    ///
    /// 两类都归这里：
    /// - 服务端：这个实例在当前配置下没这个能力（mock 后端上没有 LLM 供应商）
    /// - 本地 agent：上游 cortexd 连不上（于是这一轮排进离线队列）
    ///
    /// 措辞刻意中立。上一版写的是「本实例不提供该能力」，那是照着第一类
    /// 写的；本地 agent 用它表示第二类时就成了「本实例不提供该能力：
    /// 连不上 cortexd」—— **主语是错的**，而这句话会原样显示给用户。
    #[error("服务不可用：{0}")]
    Unavailable(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl CortexError {
    /// 供 HTTP 层映射状态码。
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

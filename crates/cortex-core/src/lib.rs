//! Cortex 核心类型。
//!
//! 有意保持**很薄**：只放全局共享的标识符、错误与配置。
//!
//! 特别地，**规范消息格式不在这里** —— 直接复用 `goose-provider-types` 的
//! `Message` / `Provider`，它已经无损承载了各家的 thinking / reasoning
//! 不透明块与 prompt caching 语义（见 docs/architecture.md「LLM 供应商」）。
//! 自己再定义一层只会造成有损转换。
//!
//! # [`injection`] 为什么在这个「很薄」的 crate 里
//!
//! 它本来在 `cortex-memory`，因为记忆注入是记忆引擎的出口。但 agent 循环
//! 搬到本地之后，**两个独立发版的进程要渲染出逐字节相同的记忆块**：
//! cortexd 给 Web 客户端渲染，本地 agent 给自己渲染。
//!
//! 让端点回渲染好的文本、由一侧渲染，看起来更省事，但那条路上格式漂移
//! **不报错**——模型只是收到一段结构不对的记忆，效果悄悄变差，几周后
//! 才从评测分上看出来。让两侧共享同一份代码、端点只回结构化数据，
//! 漂移就变成 JSON 反序列化失败，当场就炸。
//!
//! 放这里的前提是它真的没有依赖：整个模块只 `use std::fmt::Write`。

pub mod config;
pub mod error;
pub mod history;
pub mod id;
pub mod injection;
pub mod state_dir;

pub use config::Config;
pub use error::{CortexError, Result};
pub use id::Id;
pub use state_dir::{
    ADDR_FILE_EXT, LIVE_FILE_EXT, LivePointer, live_file, state_dir, token_fingerprint,
};

/// `session_events.workspace` 的字符数上限，与 migration 里的 CHECK 一致
/// （`cortex_store` re-export 了它，那条对齐关系仍归存储层维护）。
///
/// 放在这个「很薄」的 crate 里，是因为校验它的
/// `cortex_agent::workspace::validate` 要在两侧跑：cortexd 校验服务端目录，
/// 本地 agent 校验用户自己机器上的目录。让上层各写一个字面量的话，
/// 两处漂移的症状是「客户端以为绑定成功，服务端却回 500」——
/// 数据库 CHECK 的报错到不了用户眼前。
pub const WORKSPACE_PATH_MAX_CHARS: usize = 4096;

/// 编译期版本号，用于 `/health` 与 CLI 的版本输出。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

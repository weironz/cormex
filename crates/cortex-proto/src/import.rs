//! 导入 ChatGPT / Claude 的导出文件 —— 三端共用的线上形状。
//!
//! # 两条路，同一份契约
//!
//! - **桌面端**：文件在这台机器上，走本地 agent 的
//!   `POST /local/import/preview` + `POST /local/import/run`。
//!   请求体里给的是**路径**，字节一次都不过网络。
//! - **Web 端**：浏览器读不到磁盘，先 `POST /import/upload` 把文件传给
//!   cortexd 拿一个句柄，再用句柄走 `/import/preview` + `/import/run`。
//!
//! 两条路的 preview 与 run 回的是**同一批类型**（[`ImportEstimate`] /
//! [`ImportEvent`]），所以客户端那张导入页只有一份，区别只在「拿到的是
//! 路径还是句柄」。
//!
//! # 为什么 preview 是独立的一步而不是 run 的一个开关
//!
//! 它对应 CLI 里 `--dry-run` 是默认这件事，而理由在界面上更硬：
//! 每一对消息触发一次 LLM 调用，三年历史可能几千次，而记忆是 append-only ——
//! 灌错了要走 redact 才撤得掉，那是个显式、需要二次确认的操作。
//!
//! 做成两个端点（而不是 `run(dry: true)`）是为了让**误调不会花钱**：
//! 一个只读的端点没有任何路径能写入。

use serde::{Deserialize, Serialize};

/// 上传一个导出文件之后拿到的句柄。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadAck {
    /// 后续 preview / run 用它指代这份文件。
    pub handle: String,
    /// 落盘的字节数 —— 客户端可以拿它与自己那份大小核对。
    pub size_bytes: u64,
    /// 多少秒后这份临时文件会被清掉。
    ///
    /// **必须报出来**：97MB 的上传不便宜，用户放着不管一小时再回来点
    /// 「开始」，此时句柄已经失效 —— 界面要有能力提前说清楚。
    pub expires_in_secs: u64,
}

/// preview / run 的输入。**二选一**：本地路径或上传句柄。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportRequest {
    /// 本机绝对路径。只有本地 agent 认这个字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// [`UploadAck::handle`]。只有 cortexd 认这个字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// `"chatgpt"` / `"claude"`。缺省按文件内容嗅探。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// 只导入这个日期之后有消息的对话（RFC 3339）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// 最多导入几段（**从最近的开始**）。先拿三五段试水。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_conversations: Option<usize>,
}

/// 摊开的账。字段与 `cortex_import::Estimate` 一一对应 ——
/// 那边算，这边原样下发，界面上的数字与 CLI 打印的出自同一次计算。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEstimate {
    /// `"ChatGPT"` / `"Claude"`，给人看的写法
    pub platform: String,
    pub conversations: usize,
    pub messages: usize,
    /// 会触发多少次抽取 = 多少次 LLM 调用。**这个数字直接就是钱。**
    pub pairs: usize,
    pub tokens: usize,
    /// RFC 3339；一段对话都没有时为 `None`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// 落单的消息数（进原文，但不产生事实）。
    ///
    /// 单独报出来，是因为用户看到「导入了 100 条消息」却只多了 40 条记忆时，
    /// 需要有个地方解释这个差。
    pub unpaired: usize,
    /// 按客户端限速估算的下限分钟数。
    pub minutes: f64,
}

/// `run` 的 SSE 事件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImportEvent {
    /// 开工前先把账重报一遍。
    ///
    /// 客户端手上已经有 preview 的结果了，但那可能是十分钟前点的，
    /// 而 `--max-conversations` 之类的参数可能已经改过。以**这条**为准。
    Started { estimate: ImportEstimate },
    /// 进度。字段与 `cortex_import::Progress` 对应。
    Progress {
        conversations_done: usize,
        conversations_total: usize,
        pairs_done: usize,
        /// 服务端认出「早就有了」的条数 —— 没有重复计费
        skipped: usize,
        failures: usize,
    },
    /// 跑完了。
    Done {
        pairs_done: usize,
        skipped: usize,
        /// 失败数。**不为 0 时界面要说「重跑一次即可」** ——
        /// 写入幂等，重跑只补没成的那些，不会重复计费
        failures: usize,
    },
    /// 出错。仍以 SSE 事件形式返回，避免流中断后客户端无从判断原因。
    Error { message: String },
}

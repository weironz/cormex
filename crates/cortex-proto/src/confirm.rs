//! 工具确认回路的**线上**那一半 —— `GET /confirmations` 的响应项。
//!
//! # 为什么只剩这一个类型
//!
//! 曾经整本簿子（`ConfirmRegistry` 与它的 oneshot、TTL、`Drop` 清理）也在
//! 这里，理由是「`GET /confirmations` 的响应体就是它的 `PendingInfo`，
//! 而 [`crate::dto::ChatEvent::Confirm`] 的字段口径必须与它逐字对齐」。
//!
//! 那个理由只覆盖 [`PendingInfo`]，覆盖不了那本簿子。而簿子把整个
//! `cortex-agent`（agent 循环 + 工具 + 沙箱）拖进了这个**线协议** crate ——
//! 于是任何想只用记忆那一层的人，都要连 agent 一起编。
//!
//! 现在簿子在 `cortex-local`（它是唯一的宿主：cortexd 不跑 agent，容器里
//! 那个不问确认），线上类型留在这里与 `ChatEvent::Confirm` 做邻居。
//! 契约的两半仍然在同一个文件夹里，而依赖断了。

use serde::{Deserialize, Serialize};

/// 待确认项的对外形态（`GET /confirmations` 与 SSE 事件共用字段口径）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInfo {
    pub token: String,
    pub session_id: String,
    pub tool: String,
    /// `"safe"` / `"write"` / `"execute"` —— 取值由 `cortex_agent::Risk::as_wire`
    /// 定死，客户端按它决定确认框长什么样。
    ///
    /// 宿主那侧（`cortex-local` 的 `PendingMeta`）是 `&'static str`，因为它只
    /// 可能来自那三个字面量。这里必须是 `String`：这个响应要被**读回来**，
    /// 而 `&'static str` 反序列化不了。
    pub risk: String,
    pub preview: String,
    pub asked_at: String,
    /// 还剩多少秒会被按拒绝处理。客户端据此画倒计时，也据此知道
    /// 一个「看起来还在等」的提示其实早就作废了
    pub expires_in_secs: u64,
    /// 越界访问的目标绝对路径。`None` = 没有越界。
    ///
    /// 越界批准与普通写入批准是两件不同的事，用户必须能分辨 —— 而只给工具名
    /// 与 preview 是不够的：preview 里那个 path 可能是相对的，「相对于哪儿」
    /// 正是他此刻最需要知道、也最容易搞错的东西。
    ///
    /// `default` 是为了老客户端与老服务端能互相读：这个字段是后加的，
    /// 少了它只是少一行提示，不该让整条确认反序列化失败 ——
    /// 那会让用户完全看不到弹窗，而不是看到一个信息少一点的弹窗。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// 这次写入会把文件改成什么样。`None` = 没有可看的改动。
    ///
    /// **它治的是「盲签」**：只给工具名与 preview，用户看到的是
    /// 「write_file 要写 config.toml」—— 他批准的是一个他没读过的内容。
    ///
    /// `default` 的理由与 `scope` 逐字相同：后加的字段，少了它只是少一块
    /// diff，不该让整条确认反序列化失败 —— 那会让用户**完全看不到弹窗**，
    /// 比看到一个信息少一点的弹窗糟得多。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

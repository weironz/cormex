//! `POST /episodes` —— 本地 agent 把一轮对话写回记忆库。
//!
//! # 为什么写入与检索是**同一个**请求
//!
//! 最初的设计是两个端点：`GET /memory/context` 取记忆，写 episode 另走一条。
//! 那样不行 —— 「本轮注入了哪些事实」这条归因锚在 user episode 上
//! （`episode_memories` 表，也就是界面上「为什么记得这个」那个抽屉的唯一来源）。
//! 拆成两次意味着客户端得把 fact id 传回来做归因：多一次 RTT，而且
//! **归因变成客户端说了算** —— 它可以少报、错报，而没有任何办法核对。
//!
//! 合成一次之后，检索与归因在服务端同一段代码里完成，客户端拿到的是结果。
//!
//! # 为什么返回结构化数据而不是渲染好的记忆块
//!
//! 渲染留在本地，用两侧共享的 [`cortex_core::injection`]。理由见那个模块的
//! 注释：端点回文本的话，格式漂移**不报错**；回结构化数据的话，漂移是
//! JSON 反序列化失败，当场就炸。
//!
//! # id 由客户端生成
//!
//! ULID 本来就是为此设计的（全局唯一、时间有序、多端无需协调）。客户端
//! 必须先有 id 才能在**离线**时把这一轮排进本地队列，联网后再重放 ——
//! 而重放要求服务端幂等，见 [`EpisodeAck::already_existed`]。

use serde::{Deserialize, Serialize};

use crate::dto::{AttachmentRef, FactDto};

/// 一次工具调用的归因 —— 上行方向。
///
/// 与回放用的 `dto::ToolCallDto` 分开：那个是下行的展示形态（没有 `ordinal`，
/// 因为顺序由数组位置表达），这个要落进 `episode_tool_calls` 表。
/// 合成一个类型会逼其中一侧带上它不需要的字段，而那些字段的取值
/// 就成了「填什么都行」——下一个读代码的人无从判断哪个是真的。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallInput {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub summary: String,
    pub ok: bool,
    /// 这次写入改了什么（统一 diff，已截断）。见 `dto::ChatEvent::Tool` 的 `diff`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// `POST /episodes` 的请求体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewEpisodeRequest {
    /// **客户端生成的 ULID。** 幂等键，见模块注释。
    pub id: String,
    pub session_id: String,
    /// `user` / `assistant`。服务端只接受这两个 —— `tool` 与 `system`
    /// 不是「一轮对话」，它们由工具归因与系统提示各自表达
    pub role: String,
    pub text: String,
    /// 这一轮**实际发生**的时间，RFC 3339。
    ///
    /// 由客户端给而不是服务端取 `now()`：离线排队的那几轮可能几小时后才灌回来，
    /// 用落库时刻会让时间线整体错位，而记忆系统的双时间轴正是为了区分
    /// 「事情何时发生」与「何时知道」。系统时间那一半仍由服务端的
    /// `created_at` 记，客户端改不了。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
    /// 附件。哈希必须是**已登记**的 blob，服务端只做关联
    #[serde(default)]
    pub attachments: Vec<AttachmentRef>,
    /// 要不要顺带检索一次记忆。只对 `role = user` 有意义。
    ///
    /// 离线重放时设 false：那一轮的记忆早就该注入而没注入，
    /// 现在补一次检索既晚了也不真实（会把「后来才知道的事」记成
    /// 「当时注入过的」，把归因写成假的）。
    #[serde(default)]
    pub retrieve: bool,
    /// 这一轮的 **user episode id**。`role = assistant` 时该带上。
    ///
    /// 它同时定两件事：
    ///
    /// 1. **工具归因锚在哪** —— 是 user 那条，不是 assistant 那条
    ///    （见 migration 20260807000005）。模型出错时 assistant 那条根本
    ///    不落库，而工具确实执行过了。
    /// 2. **抽取拿哪两句配对** —— 服务端据此把 user 的原文读回来，
    ///    与本次的 `text` 拼成一轮再送进抽取管线。不让客户端把 user 原文
    ///    再传一遍：同一句话有两个来源，就有了「两边不一样」这条要处理的路，
    ///    而库里那一份才是权威。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_episode_id: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallInput>,
}

/// `POST /episodes` 的响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EpisodeAck {
    pub episode_id: String,
    /// 这一轮该注入哪些记忆。`retrieve = false` 时为空。
    ///
    /// **不是渲染好的文本** —— 渲染在本地做，见模块注释。
    #[serde(default)]
    pub memories: Vec<FactDto>,
    /// 这个 id 之前就写过了，本次是空操作。
    ///
    /// 离线队列重放必然会重复投递（「服务端收下了」与「本地记下已刷出」
    /// 之间崩溃就会重来一次）。返回 true 而不是 409：重复投递是**预期内**的
    /// 正常路径，报错会逼客户端把一个正常结果当异常处理，
    /// 而那段处理代码平时跑不到、真出事时才第一次执行。
    #[serde(default)]
    pub already_existed: bool,
}

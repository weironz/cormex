//! 把 ChatGPT / Claude 的导出文件灌进记忆库。
//!
//! # 为什么这是一个独立 crate
//!
//! 同一份解析器要在**三处**跑：`cortex-cli`（命令行）、`cortex-local`
//! （桌面端选本机文件）、`cortexd`（Web 端把文件传上来）。
//!
//! 不共用的话就是两三份实现，而它们漂开的症状很难看：同一个导出文件
//! 在桌面端导进 812 段、在网页端导进 790 段，两边都不报错。
//!
//! 所以这里放的是**与「谁来发请求」无关**的全部东西：解析、配对、
//! 派生 id、算账。真正发请求的那一步走 [`Sink`] —— 三处各自实现，
//! 而限速、写入顺序、错误处理只有这一份。
//!
//! # 为什么它值得存在
//!
//! 这个产品的卖点全在记忆上，而一个空记忆库与普通聊天框没有区别 ——
//! 新用户装完之后要攒好几周才看得出好处。而抽取管线的输入**正好就是对话**，
//! 与两家导出的东西是同一个形状：导入大半是格式适配 + 复用已有管线。
//!
//! # 它不需要任何新的服务端端点
//!
//! `POST /episodes` 的形状与这件事完全吻合：user 一条、assistant 一条并带
//! `anchor_episode_id`，服务端就会把这一对送进抽取。而它**本来就是幂等的**
//! （按 episode id 判重）—— 配上 [`cortex_core::Id::derived`] 算出的稳定 id，
//! 断点续传是免费的，不需要任何进度文件。
//!
//! # 三件必须先说清楚的事
//!
//! 1. **要花钱。** 每一对消息触发一次抽取，也就是一次 LLM 调用。三年历史
//!    可能是几千次。所以 `--dry-run` 是**默认**：先把数字摆出来再说。
//! 2. **会压服务器。** 服务端已经有并发闸门（`CORTEX_EXTRACT_CONCURRENCY`），
//!    但那不是背压 —— 灌得太快只会让队列长在服务端内存里。所以这一侧
//!    自己限速。
//! 3. **价值在抽取跑完之后才兑现。** 四路召回返回的是**事实**，
//!    而 episodes 那一路默认是关的。只把原文灌进去，检索里什么都不会多出来。

pub mod chatgpt;
pub mod claude;
pub mod plan;

pub use plan::{Estimate, PACE, Plan, Progress, Sink, execute, load, plan_from_bytes};

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use cortex_core::Id;

/// 从哪个平台导出来的。
///
/// 它进种子串（见 [`Conversation::session_id`]）：不带来源的话，
/// 两个平台里恰好同号的对话会撞成同一个会话。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    ChatGpt,
    Claude,
}

impl Platform {
    /// 种子前缀，也是会话标题的前缀。
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::ChatGpt => "chatgpt",
            Self::Claude => "claude",
        }
    }

    /// 显示名。会话标题里用它，所以是给人看的写法。
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::ChatGpt => "ChatGPT",
            Self::Claude => "Claude",
        }
    }
}

/// 一条消息，已经归一化成两家共有的那部分。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    /// 导出文件里的原始 id。派生 episode id 的种子之一，**必须稳定**
    pub source_id: String,
    pub role: Role,
    pub text: String,
    /// 原始时刻。进 ULID 的时间段，所以顺序与新鲜度都靠它
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub const fn wire(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// 一段对话。
#[derive(Debug, Clone)]
pub struct Conversation {
    pub source_id: String,
    pub title: String,
    pub at: DateTime<Utc>,
    /// 已按时间排好，且**只含 user / assistant**
    pub messages: Vec<Message>,
}

impl Conversation {
    /// 这段对话在 Cortex 里的会话 id。
    ///
    /// 派生而不是随机：重跑导入要落到**同一个**会话里，否则每跑一次
    /// 就多出一份同名会话。
    #[must_use]
    pub fn session_id(&self, platform: Platform) -> Id {
        Id::derived(
            self.at,
            &format!("{}:conv:{}", platform.tag(), self.source_id),
        )
    }

    /// 会话标题。**来源写在最前面** —— 这是「这条记忆是导进来的」
    /// 在界面上唯一的落点（我们刻意没有为此改 schema）。
    #[must_use]
    pub fn display_title(&self, platform: Platform) -> String {
        let t = self.title.trim();
        let base = if t.is_empty() { "未命名对话" } else { t };
        let full = format!("{} · {base}", platform.label());
        // session_events 的 CHECK 限制标题 1..=200 字符
        full.chars().take(200).collect()
    }

    /// 成对拆开：一条 user 配它后面**紧邻**的第一条 assistant。
    ///
    /// # 为什么要配对，而不是一条条灌
    ///
    /// 服务端的抽取以「一问一答」为单位（`用户：…\n助手：…`）。落单的 user
    /// 消息没有 assistant 来触发抽取，落单的 assistant 也没有 anchor ——
    /// 两者都会写进 episodes（原文不丢），但不产生任何事实。
    ///
    /// 连续多条同角色的情况（用户连发三条）取**最后一条**作为那一问：
    /// 前面几条通常是补充，而助手回答的是它们的合集。
    #[must_use]
    pub fn pairs(&self) -> Vec<(&Message, &Message)> {
        let mut out = Vec::new();
        let mut pending: Option<&Message> = None;
        for m in &self.messages {
            match m.role {
                Role::User => pending = Some(m),
                Role::Assistant => {
                    if let Some(u) = pending.take() {
                        out.push((u, m));
                    }
                }
            }
        }
        out
    }
}

/// 一条消息在 Cortex 里的 episode id。
#[must_use]
pub fn episode_id(platform: Platform, conversation: &str, m: &Message) -> Id {
    Id::derived(
        m.at,
        &format!("{}:msg:{conversation}:{}", platform.tag(), m.source_id),
    )
}

/// 把一对消息变成两个 `POST /episodes` 请求体。
///
/// user 那条 `retrieve: false`：导入不需要给历史对话注入记忆 ——
/// 那既没有意义（回答早就写好了），又会给每一条都加一次检索的开销。
#[must_use]
pub fn requests_for(
    platform: Platform,
    conv: &Conversation,
    user: &Message,
    assistant: &Message,
) -> [cortex_proto::episodes::NewEpisodeRequest; 2] {
    use cortex_proto::episodes::NewEpisodeRequest;

    let session = conv.session_id(platform).to_string();
    let user_id = episode_id(platform, &conv.source_id, user).to_string();
    let assistant_id = episode_id(platform, &conv.source_id, assistant).to_string();

    [
        NewEpisodeRequest {
            id: user_id.clone(),
            session_id: session.clone(),
            role: Role::User.wire().to_string(),
            text: user.text.clone(),
            occurred_at: Some(user.at.to_rfc3339()),
            attachments: Vec::new(),
            retrieve: false,
            anchor_episode_id: None,
            tool_calls: Vec::new(),
        },
        NewEpisodeRequest {
            id: assistant_id,
            session_id: session,
            role: Role::Assistant.wire().to_string(),
            text: assistant.text.clone(),
            occurred_at: Some(assistant.at.to_rfc3339()),
            attachments: Vec::new(),
            retrieve: false,
            // 这一条是抽取的开关：服务端拿它把 (user, assistant) 配成一轮
            anchor_episode_id: Some(user_id),
            tool_calls: Vec::new(),
        },
    ]
}

/// 解析一个导出文件。
///
/// `platform` 为 `None` 时按内容嗅探 —— 两家的顶层结构差别足够大
/// （ChatGPT 有 `mapping`，Claude 有 `chat_messages`）。
///
/// # Errors
/// 文件读不了、不是 JSON、或者**结构认不出来**。最后这一条会把实际见到的
/// 键列出来：格式漂移是这个功能最可能的失败方式，而「解析出 0 条然后报成功」
/// 会让人以为自己的历史是空的。
pub fn parse(bytes: &[u8], platform: Option<Platform>) -> Result<(Platform, Vec<Conversation>)> {
    let root: serde_json::Value =
        serde_json::from_slice(bytes).context("这个文件不是合法的 JSON")?;
    let items = root.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "导出文件的顶层应当是一个数组，实际是 {}。\
             确认给的是 conversations.json，而不是压缩包里别的文件",
            type_name(&root)
        )
    })?;
    if items.is_empty() {
        bail!("导出文件里一段对话都没有");
    }

    let platform = match platform {
        Some(p) => p,
        None => sniff(&items[0])?,
    };
    let conversations = match platform {
        Platform::ChatGpt => chatgpt::parse(items)?,
        Platform::Claude => claude::parse(items)?,
    };
    Ok((platform, conversations))
}

/// 按第一条记录的形状认平台。
fn sniff(first: &serde_json::Value) -> Result<Platform> {
    if first.get("mapping").is_some() {
        return Ok(Platform::ChatGpt);
    }
    if first.get("chat_messages").is_some() {
        return Ok(Platform::Claude);
    }
    bail!(
        "认不出这是哪家的导出文件：第一条记录里既没有 `mapping`（ChatGPT）\
         也没有 `chat_messages`（Claude）。它实际有这些键：{}\n\
         格式可能变了 —— 用 --platform 指定，或者把这个键列表反馈回来",
        keys_of(first)
    );
}

/// 列出一个 JSON 对象的键。**格式漂移时这是唯一有用的线索。**
pub fn keys_of(v: &serde_json::Value) -> String {
    match v.as_object() {
        Some(m) if m.is_empty() => "（空对象）".into(),
        Some(m) => m.keys().cloned().collect::<Vec<_>>().join(", "),
        None => format!("（不是对象，是 {}）", type_name(v)),
    }
}

fn type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "布尔值",
        serde_json::Value::Number(_) => "数字",
        serde_json::Value::String(_) => "字符串",
        serde_json::Value::Array(_) => "数组",
        serde_json::Value::Object(_) => "对象",
    }
}

/// Unix 秒（可能带小数）→ UTC 时刻。
pub fn from_unix_secs(v: &serde_json::Value) -> Option<DateTime<Utc>> {
    let secs = v.as_f64()?;
    if !secs.is_finite() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    DateTime::from_timestamp(secs.trunc() as i64, (secs.fract().abs() * 1e9) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, role: Role, text: &str) -> Message {
        Message {
            source_id: id.into(),
            role,
            text: text.into(),
            at: "2024-03-01T10:00:00Z".parse().unwrap(),
        }
    }

    fn conv(messages: Vec<Message>) -> Conversation {
        Conversation {
            source_id: "c1".into(),
            title: "关于 Rust 异步".into(),
            at: "2024-03-01T10:00:00Z".parse().unwrap(),
            messages,
        }
    }

    /// 配对：一问一答，落单的不产生对。
    #[test]
    fn pairing_takes_one_question_and_its_answer() {
        let c = conv(vec![
            msg("1", Role::User, "问题一"),
            msg("2", Role::Assistant, "回答一"),
            msg("3", Role::User, "问题二"),
            msg("4", Role::Assistant, "回答二"),
        ]);
        let pairs = c.pairs();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0.text, "问题一");
        assert_eq!(pairs[0].1.text, "回答一");
    }

    /// 连发多条时取**最后一条**作为那一问。
    ///
    /// 前面几条通常是补充说明，而助手回答的是它们的合集 ——
    /// 取第一条会让「问题」与「回答」对不上。
    #[test]
    fn a_burst_of_user_messages_pairs_on_the_last_one() {
        let c = conv(vec![
            msg("1", Role::User, "先说一句"),
            msg("2", Role::User, "补充一下，其实我想问的是 X"),
            msg("3", Role::Assistant, "关于 X……"),
        ]);
        let pairs = c.pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0.text, "补充一下，其实我想问的是 X");
    }

    /// 落单的消息不产生对（但它们仍会被写进 episodes —— 原文不丢）。
    #[test]
    fn dangling_messages_produce_no_pair() {
        assert!(
            conv(vec![msg("1", Role::User, "没人回")])
                .pairs()
                .is_empty()
        );
        assert!(
            conv(vec![msg("1", Role::Assistant, "凭空一句")])
                .pairs()
                .is_empty()
        );
    }

    /// 会话 id 稳定，且**跨平台不撞**。
    #[test]
    fn session_ids_are_stable_and_platform_scoped() {
        let c = conv(vec![]);
        assert_eq!(
            c.session_id(Platform::ChatGpt),
            c.session_id(Platform::ChatGpt),
            "重跑导入必须落回同一个会话，否则每跑一次多一份同名会话"
        );
        assert_ne!(
            c.session_id(Platform::ChatGpt),
            c.session_id(Platform::Claude),
            "两家里同号的对话不能撞成一个会话"
        );
    }

    /// 标题带来源前缀，空标题有回落，且不超过 schema 的 200 字符上限。
    #[test]
    fn titles_announce_where_they_came_from() {
        assert_eq!(
            conv(vec![]).display_title(Platform::ChatGpt),
            "ChatGPT · 关于 Rust 异步"
        );

        let mut empty = conv(vec![]);
        empty.title = "   ".into();
        assert_eq!(empty.display_title(Platform::Claude), "Claude · 未命名对话");

        let mut long = conv(vec![]);
        long.title = "很".repeat(500);
        assert!(
            long.display_title(Platform::ChatGpt).chars().count() <= 200,
            "超过 session_events 的 CHECK 上限会让改标题整个失败"
        );
    }

    /// 请求体：user 不检索，assistant 带 anchor（否则不触发抽取）。
    #[test]
    fn the_assistant_request_carries_the_anchor_that_triggers_extraction() {
        let c = conv(vec![
            msg("1", Role::User, "问"),
            msg("2", Role::Assistant, "答"),
        ]);
        let (u, a) = c.pairs()[0];
        let [user_req, asst_req] = requests_for(Platform::ChatGpt, &c, u, a);

        assert!(!user_req.retrieve, "导入不需要给历史对话注入记忆");
        assert_eq!(
            asst_req.anchor_episode_id.as_deref(),
            Some(user_req.id.as_str()),
            "anchor 是抽取的开关 —— 少了它这一对只会落原文，一条事实都不出"
        );
        assert_eq!(user_req.session_id, asst_req.session_id);
    }

    /// 认不出的结构必须**响亮失败**，并带上实际见到的键。
    ///
    /// 「解析出 0 条然后报成功」是这里最坏的失败方式：用户会以为
    /// 自己三年的历史就是空的。
    #[test]
    fn an_unrecognised_shape_fails_loudly_with_the_keys_it_saw() {
        let err = parse(br#"[{"foo": 1, "bar": 2}]"#, None).expect_err("认不出就该报错");
        let msg = err.to_string();
        assert!(msg.contains("foo") && msg.contains("bar"), "实际：{msg}");

        assert!(parse(b"{}", None).is_err(), "顶层不是数组要报错");
        assert!(parse(b"[]", None).is_err(), "空文件要报错，而不是静默成功");
        assert!(parse(b"not json", None).is_err());
    }
}

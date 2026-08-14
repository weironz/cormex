//! Claude 的 `conversations.json`。
//!
//! # 它**也是**树，只是一半的对话没连起来
//!
//! `chat_messages` 看着是平铺数组，但每条带 `parent_message_uuid` ——
//! 编辑消息与重新生成同样会开分支。所以「取哪条路径」的问题这边一样存在。
//!
//! 麻烦在于**一个文件里混着两种形态**（真导出文件上数的，1234 段）：
//!
//! | 形态 | 段数 | 该怎么处理 |
//! |---|---|---|
//! | 恰好一个根 —— 真正的树 | 906 | 从最新的**叶子**上溯，只取那条路径 |
//! | 全部消息都没有 parent | 272 | 老对话没填这个字段。按时间排，**不能当树走** |
//! | 半连接（多个根） | ~10 | 链不可信，同样按时间排 |
//!
//! 一刀切按树走的代价是具体的：那 272 段里每段只会剩**一条**消息
//! （每条都是自己的根，链长为 1）—— 实测最惨的一段 78 条只剩 1 条。
//! 反过来一刀切按时间排，则会把 906 段里被放弃的分支一起导进长期记忆。
//!
//! 所以判据是「**它是不是一棵树**」：根恰好一个才按树走。
//! 混合处理下真实丢弃率是 0.8%（12731 → 12628），丢掉的全是被放弃的分支。
//!
//! # 根有两种标记
//!
//! `parent_message_uuid` 为 `null`（3614 条），或者是零 UUID
//! `00000000-0000-4000-8000-000000000000`（928 条）。只认一种会让
//! 另一种的链走不到头。
//!
//! # 「最后一条消息」不等于叶子
//!
//! 实测有 236 段对话里，按 `created_at` 排最后的那条**还有子节点** ——
//! 时间戳与树结构并不单调（编辑保留原时间）。所以要先筛出叶子
//! （没有任何消息以它为 parent），再在叶子里取最新的。
//!
//! # 导出包里别的东西
//!
//! `memories.json`、`projects/`、`design_chats/`、`login_history.json`
//! v1 都不看。`content` 块里的 `thinking` / `tool_use` / `tool_result`
//! 也一律跳过 —— 只取 `type == "text"`。
//!
//! 与 [`super::chatgpt`] 同样的纪律：结构认不出时带上**实际见到的键**报错。
//! 「解析出 0 条然后报成功」会让用户以为自己的历史是空的。

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{Conversation, Message, Role, keys_of};

pub fn parse(items: &[Value]) -> Result<Vec<Conversation>> {
    let mut out = Vec::with_capacity(items.len());
    for (i, raw) in items.iter().enumerate() {
        match one(raw) {
            Ok(Some(c)) => out.push(c),
            Ok(None) => {}
            Err(e) => bail!("第 {} 段对话解析失败：{e}", i + 1),
        }
    }
    if out.is_empty() {
        bail!(
            "{} 段对话里一段都没解析出可用消息。格式很可能变了 —— \
             这比「你的历史是空的」更可能",
            items.len()
        );
    }
    Ok(out)
}

fn one(raw: &Value) -> Result<Option<Conversation>> {
    let Some(list) = raw.get("chat_messages").and_then(Value::as_array) else {
        bail!(
            "缺少 `chat_messages` 数组。这一段实际有这些键：{}",
            keys_of(raw)
        );
    };

    let source_id = raw
        .get("uuid")
        .or_else(|| raw.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if source_id.is_empty() {
        bail!(
            "既没有 `uuid` 也没有 `id` —— 没有稳定标识就没法做幂等导入。\
             这一段实际有这些键：{}",
            keys_of(raw)
        );
    }

    let title = raw
        .get("name")
        .or_else(|| raw.get("title"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let ordered = current_branch(list);
    let mut messages: Vec<Message> = ordered.into_iter().filter_map(message_of).collect();
    if messages.is_empty() {
        return Ok(None);
    }
    // 平铺那条路上导出顺序通常已经对了，但不保证；树那条路上链本身就是有序的，
    // 再排一次不会变。顺序错了的代价是「问」与「答」配反 ——
    // 那会把助手的话当成用户的原话记进去
    messages.sort_by_key(|m| m.at);

    let at = raw
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)
        .unwrap_or(messages[0].at);

    Ok(Some(Conversation {
        source_id: source_id.to_string(),
        title,
        at,
        messages,
    }))
}

/// 根的第二种标记。`null` 是第一种。
const ZERO_UUID: &str = "00000000-0000-4000-8000-000000000000";

/// 取用户最后留下的那条路径 —— **只在这段对话确实是一棵树时**。
///
/// 判据是「根恰好一个」。理由见模块文档：一个导出文件里混着真树与
/// 从没填过 `parent_message_uuid` 的老对话，而对后者按树走会让每段
/// 只剩一条消息。
///
/// 不是树就原样返回，让调用方按时间排 —— 那时链不可信，
/// **宁可多导几条被放弃的草稿，也不能丢掉一整段对话**。
fn current_branch(list: &[Value]) -> Vec<&Value> {
    // 写成函数而不是闭包：闭包推不出「返回的 &str 借自入参」这层关系
    fn id_of(m: &Value) -> Option<&str> {
        m.get("uuid").and_then(Value::as_str)
    }
    fn parent_of(m: &Value) -> Option<&str> {
        m.get("parent_message_uuid")
            .and_then(Value::as_str)
            .filter(|p| *p != ZERO_UUID)
    }

    let ids: std::collections::HashSet<&str> = list.iter().filter_map(id_of).collect();
    let roots = list
        .iter()
        .filter(|m| parent_of(m).is_none_or(|p| !ids.contains(p)))
        .count();
    if roots != 1 {
        return list.iter().collect();
    }

    let by_id: std::collections::HashMap<&str, &Value> = list
        .iter()
        .filter_map(|m| id_of(m).map(|i| (i, m)))
        .collect();
    let has_child: std::collections::HashSet<&str> = list.iter().filter_map(parent_of).collect();

    // **先筛叶子再取最新**，不能直接取 created_at 最大的那条：实测有
    // 236 段对话里，最后创建的那条还有子节点（编辑保留原时间戳）
    let leaf = list
        .iter()
        .filter(|m| id_of(m).is_some_and(|i| !has_child.contains(i)))
        .max_by_key(|m| m.get("created_at").and_then(Value::as_str).unwrap_or(""));
    let Some(leaf) = leaf else {
        return list.iter().collect(); // 一个叶子都没有 = 有环，别赌
    };

    let mut seen = std::collections::HashSet::new();
    let mut chain = Vec::new();
    let mut cursor = id_of(leaf);
    while let Some(id) = cursor {
        if !seen.insert(id) {
            break; // 环。导出文件不该有，但转不出来是最难查的故障
        }
        let Some(node) = by_id.get(id) else { break };
        chain.push(*node);
        cursor = parent_of(node);
    }
    chain.reverse();

    // 链走空了（结构比预期更怪）就退回全集 —— 少导不如多导
    if chain.is_empty() {
        list.iter().collect()
    } else {
        chain
    }
}

fn message_of(raw: &Value) -> Option<Message> {
    let role = match raw.get("sender")?.as_str()? {
        // Claude 那边用户侧叫 human
        "human" | "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };

    let text = text_of(raw)?;
    if text.trim().is_empty() {
        return None;
    }

    let source_id = raw
        .get("uuid")
        .or_else(|| raw.get("id"))
        .and_then(Value::as_str)?
        .to_string();

    let at = raw
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(parse_rfc3339)?;

    Some(Message {
        source_id,
        role,
        text,
        at,
    })
}

/// 取正文。
///
/// 两种形状：老的 `text` 一个字符串，新的 `content` 是块数组
/// （`[{type: "text", text: "…"}]`）。优先用块数组 —— 有它时 `text`
/// 可能只是第一块的副本。非 text 块（图片、工具）跳过：v1 不处理附件。
fn text_of(raw: &Value) -> Option<String> {
    if let Some(blocks) = raw.get("content").and_then(Value::as_array) {
        let joined = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if !joined.is_empty() {
            return Some(joined);
        }
    }
    raw.get("text").and_then(Value::as_str).map(Into::into)
}

/// Claude 的时间戳带 6 位小数（`2024-03-01T10:00:00.123456Z`），
/// `DateTime::parse_from_rfc3339` 吃得下。
fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!([{
            "uuid": "conv-1",
            "name": "关于所有权",
            "created_at": "2024-03-01T10:00:00.000000Z",
            "chat_messages": [
                {
                    "uuid": "m1", "sender": "human",
                    "created_at": "2024-03-01T10:00:01.000000Z",
                    "text": "borrow checker 怎么想的",
                    "content": [{"type": "text", "text": "borrow checker 怎么想的"}]
                },
                {
                    "uuid": "m2", "sender": "assistant",
                    "created_at": "2024-03-01T10:00:02.000000Z",
                    "content": [{"type": "text", "text": "它在追踪生命周期"}]
                }
            ]
        }])
    }

    #[test]
    fn a_flat_conversation_parses() {
        let convs = parse(sample().as_array().unwrap()).expect("应能解析");
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, "关于所有权");
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, Role::User);
        assert_eq!(convs[0].messages[1].text, "它在追踪生命周期");
    }

    /// 顺序乱了要能排回来。
    ///
    /// 配对是按顺序做的，顺序错了就是把助手的话当成用户的原话记进去 ——
    /// 而那条错误的「用户说过」会被当成他的既定立场反复强化。
    #[test]
    fn messages_are_sorted_by_time_even_if_the_file_is_not() {
        let v = json!([{
            "uuid": "c", "name": "n", "created_at": "2024-03-01T10:00:00Z",
            "chat_messages": [
                {"uuid":"m2","sender":"assistant","created_at":"2024-03-01T10:00:02Z","text":"答"},
                {"uuid":"m1","sender":"human","created_at":"2024-03-01T10:00:01Z","text":"问"}
            ]
        }]);
        let convs = parse(v.as_array().unwrap()).expect("应能解析");
        let roles: Vec<Role> = convs[0].messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
    }

    /// `content` 块优先于 `text`，非 text 块跳过。
    #[test]
    fn content_blocks_win_over_the_legacy_text_field() {
        let raw = json!({
            "text": "只有第一块",
            "content": [
                {"type": "text", "text": "第一块"},
                {"type": "image", "source": {}},
                {"type": "text", "text": "第二块"}
            ]
        });
        assert_eq!(text_of(&raw).as_deref(), Some("第一块\n第二块"));

        // 只有老字段时用它
        assert_eq!(
            text_of(&json!({"text": "老格式"})).as_deref(),
            Some("老格式")
        );
    }

    /// 造一段对话。`msgs` 每项是 `(uuid, parent, sender, text, 时刻后缀)`。
    fn conv_with(msgs: &[(&str, Option<&str>, &str, &str, &str)]) -> Value {
        json!([{
            "uuid": "c1", "name": "n", "created_at": "2024-03-01T10:00:00Z",
            "chat_messages": msgs.iter().map(|(id, parent, sender, text, t)| json!({
                "uuid": id,
                "parent_message_uuid": parent,
                "sender": sender,
                "text": text,
                "created_at": format!("2024-03-01T10:00:{t}Z"),
            })).collect::<Vec<_>>()
        }])
    }

    /// **真的是树时，只取用户最后留下的那条路径。**
    ///
    /// u1 → a1(被放弃) 与 u1 → a2(保留)。真导出文件里 906 段是这个形态。
    #[test]
    fn a_real_tree_keeps_only_the_surviving_branch() {
        let v = conv_with(&[
            ("m1", None, "human", "问题", "01"),
            ("m2", Some("m1"), "assistant", "被放弃的那一版", "02"),
            ("m3", Some("m1"), "assistant", "重新生成的这一版", "03"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("应能解析");
        let texts: Vec<&str> = c[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["问题", "重新生成的这一版"]);
    }

    /// **没有 parent 的老对话必须整段留下，绝不能当树走。**
    ///
    /// 真导出文件里 272 段是这个形态（`parent_message_uuid` 全是 null）。
    /// 一刀切按树走的话，每条都是自己的根、链长为 1 —— 实测最惨的一段
    /// 78 条只剩 1 条。这是这个解析器最容易犯、后果最大的错。
    #[test]
    fn a_flat_conversation_without_parents_keeps_everything() {
        let v = conv_with(&[
            ("m1", None, "human", "第一问", "01"),
            ("m2", None, "assistant", "第一答", "02"),
            ("m3", None, "human", "第二问", "03"),
            ("m4", None, "assistant", "第二答", "04"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("应能解析");
        assert_eq!(
            c[0].messages.len(),
            4,
            "老对话没填 parent，按树走会只剩一条 —— 那是丢掉整段历史"
        );
    }

    /// 零 UUID 也是根。只认 `null` 的话链走不到头。
    ///
    /// 真文件里这种写法有 928 条。
    #[test]
    fn the_zero_uuid_counts_as_a_root() {
        let v = conv_with(&[
            ("m1", Some(ZERO_UUID), "human", "问", "01"),
            ("m2", Some("m1"), "assistant", "答", "02"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("应能解析");
        assert_eq!(c[0].messages.len(), 2, "零 UUID 没被当成根，链断了");
    }

    /// **叶子要先筛再取最新** —— 不能直接拿 `created_at` 最大的那条。
    ///
    /// 真文件里有 236 段对话，按时间排最后的那条还有子节点
    /// （编辑消息保留原时间戳）。直接取最大值会从中间开始上溯，
    /// 把它后面的整段回答全丢掉。
    #[test]
    fn the_newest_message_is_not_necessarily_the_leaf() {
        // m2 时刻最晚，却是 m3 的父节点
        let v = conv_with(&[
            ("m1", None, "human", "问", "01"),
            ("m2", Some("m1"), "assistant", "答", "09"),
            ("m3", Some("m2"), "human", "追问", "05"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("应能解析");
        let texts: Vec<&str> = c[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert!(
            texts.contains(&"追问"),
            "从非叶子上溯，把它的子孙整段丢了。实际：{texts:?}"
        );
    }

    /// 有环也要能出来，而且不丢消息。
    #[test]
    fn a_cycle_falls_back_instead_of_hanging() {
        let v = conv_with(&[
            ("m1", Some("m2"), "human", "问", "01"),
            ("m2", Some("m1"), "assistant", "答", "02"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("有环也要能出来");
        assert_eq!(c[0].messages.len(), 2);
    }

    /// 结构不认识时报错，且带上实际见到的键。
    #[test]
    fn a_missing_message_array_says_what_it_actually_saw() {
        let v = json!([{"uuid": "c", "name": "n", "messages": []}]);
        let err = parse(v.as_array().unwrap()).expect_err("没有 chat_messages 就该报错");
        let msg = err.to_string();
        assert!(msg.contains("chat_messages"), "实际：{msg}");
        assert!(
            msg.contains("uuid") && msg.contains("messages"),
            "报错要列出实际见到的键。实际：{msg}"
        );
    }

    /// 没有稳定 id 就不做幂等导入。
    #[test]
    fn a_conversation_without_an_id_is_refused() {
        let v = json!([{"name": "n", "chat_messages": []}]);
        assert!(parse(v.as_array().unwrap()).is_err());
    }
}

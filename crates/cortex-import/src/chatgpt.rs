//! ChatGPT 的 `conversations.json`。
//!
//! # 它不是一个消息列表，是一棵树
//!
//! 每段对话有一个 `mapping`：节点 id → `{message, parent, children}`。
//! 之所以是树，是因为「重新生成」与「编辑上一条」都会开一条新分支 ——
//! 于是一段对话里同一个位置可能有好几个版本，而 `current_node` 指向
//! **用户最后看到的那一条**。
//!
//! # 只取当前那条路径，不取全部分支
//!
//! 从 `current_node` 沿 `parent` 一路上溯到根，反转，就是用户实际留下的
//! 那次对话。别的分支是**被放弃的草稿**。
//!
//! 全导的代价有两面：抽取次数成倍增长（直接就是钱），以及**把用户丢掉的
//! 东西写进他的长期记忆** —— 他重新生成过一次，正说明第一版不是他要的。
//!
//! # 认不出结构就响亮失败
//!
//! 这个格式没有公开契约，随时可能变。「解析出 0 条然后报成功」是最坏的
//! 失败方式：用户会以为自己三年的历史就是空的。所以每一处结构假设不成立
//! 时都带上**实际见到的键**报错。

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{Conversation, Message, Role, from_unix_secs, keys_of};

pub fn parse(items: &[Value]) -> Result<Vec<Conversation>> {
    let mut out = Vec::with_capacity(items.len());
    let mut skipped = 0usize;
    for (i, raw) in items.iter().enumerate() {
        match one(raw) {
            Ok(Some(c)) => out.push(c),
            // 没有可用消息的对话（只有 system、或者整段是空的）不算错
            Ok(None) => skipped += 1,
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
    if skipped > 0 {
        tracing::debug!(skipped, "跳过了没有可用消息的对话");
    }
    Ok(out)
}

fn one(raw: &Value) -> Result<Option<Conversation>> {
    let Some(mapping) = raw.get("mapping").and_then(Value::as_object) else {
        bail!("缺少 `mapping` 对象。这一段实际有这些键：{}", keys_of(raw));
    };

    let source_id = raw
        .get("conversation_id")
        .or_else(|| raw.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if source_id.is_empty() {
        bail!(
            "既没有 `conversation_id` 也没有 `id` —— 没有稳定标识就没法做\
             幂等导入。这一段实际有这些键：{}",
            keys_of(raw)
        );
    }

    let title = raw
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    // 沿 current_node 上溯。没有 current_node 时回落到「所有节点按时间排」——
    // 那不如树路径准确（会混进被放弃的分支），但比整段丢掉好
    let path = match raw.get("current_node").and_then(Value::as_str) {
        Some(leaf) => walk_up(mapping, leaf),
        None => all_nodes_by_time(mapping),
    };

    let messages: Vec<Message> = path.into_iter().filter_map(message_of).collect();
    if messages.is_empty() {
        return Ok(None);
    }

    let at = raw
        .get("create_time")
        .and_then(from_unix_secs)
        .unwrap_or(messages[0].at);

    Ok(Some(Conversation {
        source_id: source_id.to_string(),
        title,
        at,
        messages,
    }))
}

/// 从叶子沿 `parent` 上溯到根，返回**根到叶**的顺序。
///
/// 带一个访问集合：导出文件理论上不该有环，但一个环会让这里死循环，
/// 而「导入卡住不动」是最难查的一种故障。
fn walk_up<'a>(
    mapping: &'a serde_json::Map<String, Value>,
    leaf: &str,
) -> Vec<&'a serde_json::Map<String, Value>> {
    let mut seen = std::collections::HashSet::new();
    let mut chain = Vec::new();
    let mut cursor = Some(leaf.to_string());

    while let Some(id) = cursor {
        if !seen.insert(id.clone()) {
            tracing::warn!(node = id, "mapping 里有环，在这里停下");
            break;
        }
        let Some(node) = mapping.get(&id).and_then(Value::as_object) else {
            break;
        };
        cursor = node
            .get("parent")
            .and_then(Value::as_str)
            .map(ToString::to_string);
        chain.push(node);
    }

    chain.reverse();
    chain
}

/// 没有 `current_node` 时的回落：所有节点按消息时间排。
fn all_nodes_by_time(
    mapping: &serde_json::Map<String, Value>,
) -> Vec<&serde_json::Map<String, Value>> {
    let mut nodes: Vec<&serde_json::Map<String, Value>> =
        mapping.values().filter_map(Value::as_object).collect();
    nodes.sort_by_key(|n| {
        n.get("message")
            .and_then(|m| m.get("create_time"))
            .and_then(from_unix_secs)
            .unwrap_or_else(Utc::now)
    });
    nodes
}

/// 从一个节点里取出消息。取不出就是 `None`（system、工具调用、空节点）。
fn message_of(node: &serde_json::Map<String, Value>) -> Option<Message> {
    let m = node.get("message")?.as_object()?;

    let role = match m.get("author")?.get("role")?.as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        // system / tool 一概不要：前者是平台注入的，后者是工具往返。
        // 两者都不是「用户与助手之间发生了什么」
        _ => return None,
    };

    let text = text_of(m.get("content")?)?;
    if text.trim().is_empty() {
        return None;
    }

    // id 优先用消息自己的；没有就用节点 id —— 它同样稳定
    let source_id = m
        .get("id")
        .and_then(Value::as_str)
        .or_else(|| node.get("id").and_then(Value::as_str))?
        .to_string();

    let at: DateTime<Utc> = m.get("create_time").and_then(from_unix_secs)?;

    Some(Message {
        source_id,
        role,
        text,
        at,
    })
}

/// 把 `content` 抠成纯文本。
///
/// 见过三种形状：`{parts: [...]}`（最常见）、`{text: "..."}`、以及直接一个
/// 字符串。`parts` 里除了字符串还可能是对象（图片、附件的占位），
/// 那些跳过 —— v1 不处理附件。
fn text_of(content: &Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(parts) = content.get("parts").and_then(Value::as_array) {
        let joined = parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n");
        return Some(joined);
    }
    content.get("text").and_then(Value::as_str).map(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 一棵**有分支**的树：用户重新生成过一次。
    ///
    /// root → u1 → a1(被放弃)
    ///           → a2(current)
    fn branched() -> Value {
        json!([{
            "conversation_id": "c1",
            "title": "测试",
            "create_time": 1_709_287_200.0,
            "current_node": "n-a2",
            "mapping": {
                "n-root": {"id": "n-root", "message": null, "parent": null, "children": ["n-u1"]},
                "n-u1": {
                    "id": "n-u1", "parent": "n-root", "children": ["n-a1", "n-a2"],
                    "message": {
                        "id": "m-u1",
                        "author": {"role": "user"},
                        "create_time": 1_709_287_201.0,
                        "content": {"content_type": "text", "parts": ["怎么做异步"]}
                    }
                },
                "n-a1": {
                    "id": "n-a1", "parent": "n-u1", "children": [],
                    "message": {
                        "id": "m-a1",
                        "author": {"role": "assistant"},
                        "create_time": 1_709_287_202.0,
                        "content": {"content_type": "text", "parts": ["这是被放弃的那一版"]}
                    }
                },
                "n-a2": {
                    "id": "n-a2", "parent": "n-u1", "children": [],
                    "message": {
                        "id": "m-a2",
                        "author": {"role": "assistant"},
                        "create_time": 1_709_287_203.0,
                        "content": {"content_type": "text", "parts": ["用 tokio"]}
                    }
                }
            }
        }])
    }

    /// **只取 current_node 那条路径。**
    ///
    /// 被放弃的分支不进记忆：用户重新生成过一次，正说明第一版不是他要的。
    /// 全导的另一面是抽取次数成倍增长，而那直接是钱。
    #[test]
    fn only_the_current_branch_is_imported() {
        let convs = parse(branched().as_array().unwrap()).expect("应能解析");
        assert_eq!(convs.len(), 1);

        let texts: Vec<&str> = convs[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["怎么做异步", "用 tokio"]);
        assert!(
            !texts.contains(&"这是被放弃的那一版"),
            "被放弃的分支被导进来了 —— 那是用户主动丢掉的东西"
        );
    }

    /// system / tool 消息不进来，空消息也不进来。
    #[test]
    fn system_and_empty_messages_are_dropped() {
        let v = json!([{
            "conversation_id": "c1", "title": "t", "current_node": "n3",
            "mapping": {
                "n1": {"id":"n1","parent":null,"children":["n2"],"message":{
                    "id":"m1","author":{"role":"system"},"create_time":1.0,
                    "content":{"parts":["你是一个助手"]}}},
                "n2": {"id":"n2","parent":"n1","children":["n3"],"message":{
                    "id":"m2","author":{"role":"user"},"create_time":2.0,
                    "content":{"parts":["   "]}}},
                "n3": {"id":"n3","parent":"n2","children":[],"message":{
                    "id":"m3","author":{"role":"user"},"create_time":3.0,
                    "content":{"parts":["真正的问题"]}}}
            }
        }]);
        let convs = parse(v.as_array().unwrap()).expect("应能解析");
        let texts: Vec<&str> = convs[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["真正的问题"]);
    }

    /// 环不能让它转不出来。
    ///
    /// 导出文件不该有环，但真有一个的话，「导入卡住不动」是最难查的故障。
    #[test]
    fn a_cycle_in_the_tree_does_not_hang() {
        let v = json!([{
            "conversation_id": "c1", "title": "t", "current_node": "a",
            "mapping": {
                "a": {"id":"a","parent":"b","children":[],"message":{
                    "id":"ma","author":{"role":"user"},"create_time":1.0,
                    "content":{"parts":["问"]}}},
                "b": {"id":"b","parent":"a","children":[],"message":{
                    "id":"mb","author":{"role":"assistant"},"create_time":2.0,
                    "content":{"parts":["答"]}}}
            }
        }]);
        let convs = parse(v.as_array().unwrap()).expect("有环也要能出来");
        assert_eq!(convs[0].messages.len(), 2);
    }

    /// 结构不认识时报错，且**带上实际见到的键**。
    #[test]
    fn a_missing_mapping_says_what_it_actually_saw() {
        let v = json!([{"title": "t", "messages": []}]);
        let err = parse(v.as_array().unwrap()).expect_err("没有 mapping 就该报错");
        let msg = err.to_string();
        assert!(msg.contains("mapping"), "实际：{msg}");
        assert!(
            msg.contains("title") && msg.contains("messages"),
            "报错要列出实际见到的键，否则格式漂移时无从下手。实际：{msg}"
        );
    }

    /// 没有稳定 id 就不能做幂等导入 —— 宁可报错。
    #[test]
    fn a_conversation_without_an_id_is_refused() {
        let v = json!([{"title": "t", "mapping": {}, "current_node": "x"}]);
        assert!(parse(v.as_array().unwrap()).is_err());
    }

    /// content 的三种形状都要吃得下。
    #[test]
    fn all_three_content_shapes_are_understood() {
        assert_eq!(
            text_of(&json!({"parts": ["一", "二"]})).as_deref(),
            Some("一\n二")
        );
        assert_eq!(
            text_of(&json!({"text": "直接给"})).as_deref(),
            Some("直接给")
        );
        assert_eq!(text_of(&json!("裸字符串")).as_deref(), Some("裸字符串"));
        // parts 里混进非字符串（图片占位）时跳过它，不整条丢掉
        assert_eq!(
            text_of(&json!({"parts": ["文字", {"content_type": "image"}]})).as_deref(),
            Some("文字")
        );
    }
}

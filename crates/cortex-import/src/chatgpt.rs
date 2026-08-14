//! ChatGPT's `conversations.json`.
//!
//! # It is not a message list, it is a tree
//!
//! Each conversation has a `mapping`: node id → `{message, parent, children}`.
//! It is a tree because both "regenerate" and "edit the previous message" open
//! a new branch — so one position in a conversation may hold several versions,
//! and `current_node` points at **the one the user last saw**.
//!
//! # Take only the current path, not every branch
//!
//! Walking up from `current_node` along `parent` to the root and reversing
//! gives the conversation the user actually kept. The other branches are
//! **abandoned drafts**.
//!
//! Importing everything costs on two fronts: the number of extractions
//! multiplies (which is money, directly), and **it writes what the user threw
//! away into their long-term memory** — the fact that they regenerated once is
//! precisely the evidence that the first version was not what they wanted.
//!
//! # Fail loudly on an unrecognized structure
//!
//! This format has no published contract and may change at any time. "Parse 0
//! entries and report success" is the worst failure mode: the user would
//! conclude that three years of their history is simply empty. So every failed
//! structural assumption reports an error carrying **the keys actually seen**.

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
            // A conversation with no usable messages (system only, or entirely
            // empty) is not an error
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

    // Walk up from current_node. With no current_node, fall back to "all nodes
    // sorted by time" — less accurate than the tree path (abandoned branches
    // get mixed in), but better than dropping the whole conversation
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

/// Walk up from the leaf along `parent` to the root, returned in **root-to-leaf**
/// order.
///
/// Carries a visited set: an export file should not contain a cycle in theory,
/// but one cycle would spin here forever, and "the import hangs" is the hardest
/// kind of fault to diagnose.
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

/// The fallback when there is no `current_node`: every node sorted by message
/// time.
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

/// Extract the message from a node. `None` when there is none (system, tool
/// calls, empty nodes).
fn message_of(node: &serde_json::Map<String, Value>) -> Option<Message> {
    let m = node.get("message")?.as_object()?;

    let role = match m.get("author")?.get("role")?.as_str()? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        // system / tool are never taken: the former is injected by the
        // platform, the latter is a tool round-trip. Neither is "what happened
        // between the user and the assistant"
        _ => return None,
    };

    let text = text_of(m.get("content")?)?;
    if text.trim().is_empty() {
        return None;
    }

    // Prefer the message's own id; failing that use the node id — it is just as
    // stable
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

/// Dig plain text out of `content`.
///
/// Three shapes have been observed: `{parts: [...]}` (most common),
/// `{text: "..."}`, and a bare string. Besides strings, `parts` may also hold
/// objects (placeholders for images and attachments); those are skipped — v1
/// does not handle attachments.
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

    /// A tree **with a branch**: the user regenerated once.
    ///
    /// root → u1 → a1 (abandoned)
    ///           → a2 (current)
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

    /// **Only the current_node path is taken.**
    ///
    /// Abandoned branches do not enter memory: the user regenerated once, which
    /// is precisely the evidence that the first version was not what they
    /// wanted. The other side of importing everything is that the number of
    /// extractions multiplies, and that is money, directly.
    #[test]
    fn only_the_current_branch_is_imported() {
        let convs = parse(branched().as_array().unwrap()).expect("should parse");
        assert_eq!(convs.len(), 1);

        let texts: Vec<&str> = convs[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["怎么做异步", "用 tokio"]);
        assert!(
            !texts.contains(&"这是被放弃的那一版"),
            "an abandoned branch got imported — that is something the user \
             actively threw away"
        );
    }

    /// system / tool messages do not come in, and neither do empty ones.
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
        let convs = parse(v.as_array().unwrap()).expect("should parse");
        let texts: Vec<&str> = convs[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["真正的问题"]);
    }

    /// A cycle must not make it spin forever.
    ///
    /// An export file should not contain a cycle, but if one really is there,
    /// "the import hangs" is the hardest fault to diagnose.
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
        let convs = parse(v.as_array().unwrap()).expect("it must get out even with a cycle");
        assert_eq!(convs[0].messages.len(), 2);
    }

    /// An unrecognized structure is an error, and it **carries the keys
    /// actually seen**.
    #[test]
    fn a_missing_mapping_says_what_it_actually_saw() {
        let v = json!([{"title": "t", "messages": []}]);
        let err = parse(v.as_array().unwrap()).expect_err("a missing mapping should be an error");
        let msg = err.to_string();
        assert!(msg.contains("mapping"), "got: {msg}");
        assert!(
            msg.contains("title") && msg.contains("messages"),
            "the error must list the keys actually seen, otherwise there is no \
             way in when the format drifts. Got: {msg}"
        );
    }

    /// Without a stable id there is no idempotent import — better to error.
    #[test]
    fn a_conversation_without_an_id_is_refused() {
        let v = json!([{"title": "t", "mapping": {}, "current_node": "x"}]);
        assert!(parse(v.as_array().unwrap()).is_err());
    }

    /// All three shapes of `content` have to be handled.
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
        // When a non-string (an image placeholder) is mixed into parts, skip it
        // rather than dropping the whole message
        assert_eq!(
            text_of(&json!({"parts": ["文字", {"content_type": "image"}]})).as_deref(),
            Some("文字")
        );
    }
}

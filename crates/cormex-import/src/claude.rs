//! Claude's `conversations.json`.
//!
//! # It is **also** a tree, except half the conversations are not wired up
//!
//! `chat_messages` looks like a flat array, but every entry carries a
//! `parent_message_uuid` — editing a message and regenerating both open a
//! branch here too. So the "which path do we take" question exists on this side
//! as well.
//!
//! The trouble is that **one file mixes two shapes** (counted on a real export
//! file, 1234 conversations):
//!
//! | Shape | Count | How to handle it |
//! |---|---|---|
//! | Exactly one root — a real tree | 906 | Walk up from the newest **leaf**, take only that path |
//! | No message has a parent | 272 | Old conversations never filled this field. Sort by time; **do not walk it as a tree** |
//! | Half-connected (multiple roots) | ~10 | The chain is untrustworthy; sort by time as well |
//!
//! The cost of uniformly walking it as a tree is concrete: each of those 272
//! conversations would be left with **one** message (every entry is its own
//! root, chain length 1) — the worst measured case went from 78 messages to 1.
//! Conversely, uniformly sorting by time would import the abandoned branches
//! from those 906 conversations into long-term memory.
//!
//! So the criterion is "**is it a tree**": walk it as a tree only when there is
//! exactly one root. Under the mixed handling the real discard rate is 0.8%
//! (12731 → 12628), and everything discarded is an abandoned branch.
//!
//! # A root is marked two ways
//!
//! `parent_message_uuid` is either `null` (3614 entries) or the zero UUID
//! `00000000-0000-4000-8000-000000000000` (928 entries). Recognizing only one
//! of them leaves the other's chain unable to reach its end.
//!
//! # "The last message" is not the leaf
//!
//! Measured: in 236 conversations, the entry that sorts last by `created_at`
//! **still has children** — timestamps and tree structure are not monotonic
//! together (an edit keeps the original time). So the leaves have to be
//! filtered out first (no message has it as a parent), and the newest taken
//! among those.
//!
//! # Other things in the export archive
//!
//! `memories.json`, `projects/`, `design_chats/`, and `login_history.json` are
//! all ignored in v1. `thinking` / `tool_use` / `tool_result` blocks inside
//! `content` are likewise skipped — only `type == "text"` is taken.
//!
//! The same discipline as [`super::chatgpt`]: an unrecognized structure reports
//! an error carrying **the keys actually seen**. "Parse 0 entries and report
//! success" would leave the user believing their history is empty.

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
            Err(e) => bail!("conversation {} failed to parse: {e}", i + 1),
        }
    }
    if out.is_empty() {
        bail!(
            "not one of the {} conversations yielded a usable message. The format has most \
             likely changed — that is far more likely than your history being empty",
            items.len()
        );
    }
    Ok(out)
}

fn one(raw: &Value) -> Result<Option<Conversation>> {
    let Some(list) = raw.get("chat_messages").and_then(Value::as_array) else {
        bail!(
            "no `chat_messages` array. The keys this entry does have: {}",
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
            "neither `uuid` nor `id` — without a stable identifier there is no idempotent \
             import. The keys this entry does have: {}",
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
    // On the flat path the export order is usually already right, but it is not
    // guaranteed; on the tree path the chain is ordered by construction, so
    // sorting again changes nothing. The cost of wrong order is question and
    // answer being paired backwards — which records the assistant's words as
    // the user's own
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

/// The second way a root is marked. `null` is the first.
const ZERO_UUID: &str = "00000000-0000-4000-8000-000000000000";

/// Take the path the user last kept — **only when this conversation really is a
/// tree**.
///
/// The criterion is "exactly one root". The reasoning is in the module docs: an
/// export file mixes real trees with old conversations that never filled in
/// `parent_message_uuid`, and walking the latter as a tree leaves each one with
/// a single message.
///
/// If it is not a tree, return it as-is and let the caller sort by time — the
/// chain is untrustworthy in that case, and **importing a few extra abandoned
/// drafts is preferable to losing an entire conversation**.
fn current_branch(list: &[Value]) -> Vec<&Value> {
    // Written as functions rather than closures: a closure cannot infer that
    // the returned &str borrows from the argument
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

    // **Filter for leaves first, then take the newest**; do not simply take the
    // entry with the largest created_at: measured, in 236 conversations the
    // last-created entry still has children (an edit keeps the original
    // timestamp)
    let leaf = list
        .iter()
        .filter(|m| id_of(m).is_some_and(|i| !has_child.contains(i)))
        .max_by_key(|m| m.get("created_at").and_then(Value::as_str).unwrap_or(""));
    let Some(leaf) = leaf else {
        // Not a single leaf = there is a cycle; do not gamble
        return list.iter().collect();
    };

    let mut seen = std::collections::HashSet::new();
    let mut chain = Vec::new();
    let mut cursor = id_of(leaf);
    while let Some(id) = cursor {
        if !seen.insert(id) {
            // A cycle. An export file should not have one, but spinning forever
            // is the hardest fault to diagnose
            break;
        }
        let Some(node) = by_id.get(id) else { break };
        chain.push(*node);
        cursor = parent_of(node);
    }
    chain.reverse();

    // If the chain came out empty (the structure is stranger than expected),
    // fall back to the full set — importing too much beats importing too little
    if chain.is_empty() {
        list.iter().collect()
    } else {
        chain
    }
}

fn message_of(raw: &Value) -> Option<Message> {
    let role = match raw.get("sender")?.as_str()? {
        // On Claude's side the user role is called human
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

/// Take the body text.
///
/// Two shapes: the old `text`, a single string, and the new `content`, an array
/// of blocks (`[{type: "text", text: "…"}]`). The block array wins — when it is
/// present, `text` may be only a copy of the first block. Non-text blocks
/// (images, tools) are skipped: v1 does not handle attachments.
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

/// Claude's timestamps carry 6 fractional digits
/// (`2024-03-01T10:00:00.123456Z`), which `DateTime::parse_from_rfc3339`
/// handles.
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
        let convs = parse(sample().as_array().unwrap()).expect("should parse");
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title, "关于所有权");
        assert_eq!(convs[0].messages.len(), 2);
        assert_eq!(convs[0].messages[0].role, Role::User);
        assert_eq!(convs[0].messages[1].text, "它在追踪生命周期");
    }

    /// Scrambled order has to be sorted back.
    ///
    /// Pairing works in order, so wrong order records the assistant's words as
    /// the user's own — and that false "the user said this" gets reinforced
    /// over and over as their established position.
    #[test]
    fn messages_are_sorted_by_time_even_if_the_file_is_not() {
        let v = json!([{
            "uuid": "c", "name": "n", "created_at": "2024-03-01T10:00:00Z",
            "chat_messages": [
                {"uuid":"m2","sender":"assistant","created_at":"2024-03-01T10:00:02Z","text":"答"},
                {"uuid":"m1","sender":"human","created_at":"2024-03-01T10:00:01Z","text":"问"}
            ]
        }]);
        let convs = parse(v.as_array().unwrap()).expect("should parse");
        let roles: Vec<Role> = convs[0].messages.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant]);
    }

    /// `content` blocks win over `text`, and non-text blocks are skipped.
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

        // With only the legacy field present, use it
        assert_eq!(
            text_of(&json!({"text": "老格式"})).as_deref(),
            Some("老格式")
        );
    }

    /// Build a conversation. Each item of `msgs` is
    /// `(uuid, parent, sender, text, seconds suffix)`.
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

    /// **When it really is a tree, take only the path the user last kept.**
    ///
    /// u1 → a1 (abandoned) alongside u1 → a2 (kept). 906 conversations in a
    /// real export file have this shape.
    #[test]
    fn a_real_tree_keeps_only_the_surviving_branch() {
        let v = conv_with(&[
            ("m1", None, "human", "问题", "01"),
            ("m2", Some("m1"), "assistant", "被放弃的那一版", "02"),
            ("m3", Some("m1"), "assistant", "重新生成的这一版", "03"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("should parse");
        let texts: Vec<&str> = c[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["问题", "重新生成的这一版"]);
    }

    /// **An old conversation with no parents must be kept whole, and must never
    /// be walked as a tree.**
    ///
    /// 272 conversations in a real export file have this shape
    /// (`parent_message_uuid` is null throughout). Uniformly walking it as a
    /// tree makes every entry its own root with chain length 1 — the worst
    /// measured case went from 78 messages to 1. This is the easiest mistake to
    /// make in this parser and the one with the largest consequences.
    #[test]
    fn a_flat_conversation_without_parents_keeps_everything() {
        let v = conv_with(&[
            ("m1", None, "human", "第一问", "01"),
            ("m2", None, "assistant", "第一答", "02"),
            ("m3", None, "human", "第二问", "03"),
            ("m4", None, "assistant", "第二答", "04"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("should parse");
        assert_eq!(
            c[0].messages.len(),
            4,
            "old conversations never filled in parent, so walking them as a \
             tree leaves one message — that discards an entire history"
        );
    }

    /// The zero UUID is also a root. Recognizing only `null` leaves the chain
    /// unable to reach its end.
    ///
    /// A real file contains 928 entries written this way.
    #[test]
    fn the_zero_uuid_counts_as_a_root() {
        let v = conv_with(&[
            ("m1", Some(ZERO_UUID), "human", "问", "01"),
            ("m2", Some("m1"), "assistant", "答", "02"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("should parse");
        assert_eq!(
            c[0].messages.len(),
            2,
            "the zero UUID was not treated as a root and the chain broke"
        );
    }

    /// **Filter for leaves first, then take the newest** — do not simply grab
    /// the entry with the largest `created_at`.
    ///
    /// A real file has 236 conversations where the entry that sorts last by
    /// time still has children (editing a message keeps the original
    /// timestamp). Taking the maximum directly starts the walk from the middle
    /// and discards the entire stretch of answers after it.
    #[test]
    fn the_newest_message_is_not_necessarily_the_leaf() {
        // m2 has the latest timestamp, yet it is m3's parent
        let v = conv_with(&[
            ("m1", None, "human", "问", "01"),
            ("m2", Some("m1"), "assistant", "答", "09"),
            ("m3", Some("m2"), "human", "追问", "05"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("should parse");
        let texts: Vec<&str> = c[0].messages.iter().map(|m| m.text.as_str()).collect();
        assert!(
            texts.contains(&"追问"),
            "the walk started from a non-leaf and discarded its whole \
             descendant stretch. Got: {texts:?}"
        );
    }

    /// It must get out even with a cycle, and without losing messages.
    #[test]
    fn a_cycle_falls_back_instead_of_hanging() {
        let v = conv_with(&[
            ("m1", Some("m2"), "human", "问", "01"),
            ("m2", Some("m1"), "assistant", "答", "02"),
        ]);
        let c = parse(v.as_array().unwrap()).expect("it must get out even with a cycle");
        assert_eq!(c[0].messages.len(), 2);
    }

    /// An unrecognized structure is an error carrying the keys actually seen.
    #[test]
    fn a_missing_message_array_says_what_it_actually_saw() {
        let v = json!([{"uuid": "c", "name": "n", "messages": []}]);
        let err =
            parse(v.as_array().unwrap()).expect_err("a missing chat_messages should be an error");
        let msg = err.to_string();
        assert!(msg.contains("chat_messages"), "got: {msg}");
        assert!(
            msg.contains("uuid") && msg.contains("messages"),
            "the error must list the keys actually seen. Got: {msg}"
        );
    }

    /// Without a stable id there is no idempotent import.
    #[test]
    fn a_conversation_without_an_id_is_refused() {
        let v = json!([{"name": "n", "chat_messages": []}]);
        assert!(parse(v.as_array().unwrap()).is_err());
    }
}

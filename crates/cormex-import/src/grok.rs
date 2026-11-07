//! Grok's account export (`prod-grok-backend.json`).
//!
//! # Where the format came from
//!
//! xAI does not document it. The shape here is what the community tools that
//! read it agree on (`hugoamorales/grok_export_toolkit`,
//! `beejaksharam/grok-export-viewer`), and it has not been checked against an
//! export from a real account — the error messages below name the keys they
//! saw, so a drifted file says what changed instead of importing nothing.
//!
//! ```text
//! {"conversations":[
//!   {"conversation":{"id":"…","title":"…","create_time":"2025-…Z"},
//!    "responses":[{"response":{"_id":"…","message":"…","sender":"human",
//!                              "create_time":{"$date":{"$numberLong":"1712…"}}}}]}],
//!  "projects":[…], …}
//! ```
//!
//! The file is the whole account: conversations sit under `conversations`
//! beside projects, tasks and media. The splitter hands this parser one
//! conversation at a time, as a one-element array of the
//! `{conversation, responses}` wrapper.
//!
//! `create_time` has been seen as an RFC 3339 string, epoch seconds or
//! milliseconds, and MongoDB extended JSON (`{"$date":…}`); all are accepted.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{Conversation, Message, Role, keys_of};

/// Parse `{conversation, responses}` wrappers.
///
/// # Errors
/// A record has no `conversation` object.
pub fn parse(items: &[Value]) -> Result<Vec<Conversation>> {
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let Some(conv) = item.get("conversation") else {
            bail!(
                "record {i} of the Grok export has no `conversation`; the keys it does have: {}",
                keys_of(item)
            );
        };
        let (id, title) = identity(item);
        let id = if id.is_empty() {
            format!("grok-{i}")
        } else {
            id
        };
        let mut messages = Vec::new();
        for (j, r) in item
            .get("responses")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .enumerate()
        {
            let r = r.get("response").unwrap_or(r);
            let text = r
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned();
            if text.is_empty() {
                continue;
            }
            let role = match r
                .get("sender")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("human" | "user") => Role::User,
                _ => Role::Assistant,
            };
            let Some(at) = r.get("create_time").and_then(time_of) else {
                continue;
            };
            messages.push(Message {
                source_id: r
                    .get("_id")
                    .or_else(|| r.get("id"))
                    .and_then(Value::as_str)
                    .map_or_else(|| format!("{id}-{j}"), str::to_owned),
                role,
                text,
                at,
            });
        }
        messages.sort_by_key(|m| m.at);
        let at = conv
            .get("create_time")
            .and_then(time_of)
            .or_else(|| messages.first().map(|m| m.at));
        let Some(at) = at else {
            // No timestamp anywhere: a conversation with no messages at all
            continue;
        };
        out.push(Conversation {
            source_id: id,
            title,
            at,
            messages,
        });
    }
    Ok(out)
}

/// A conversation's id and title, from the `conversation` object of a
/// `{conversation, responses}` wrapper. The splitter uses the same rule.
#[must_use]
pub fn identity(item: &Value) -> (String, String) {
    let conv = item.get("conversation").unwrap_or(item);
    let id = conv
        .get("id")
        .or_else(|| conv.get("_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let title = conv
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    (id, title)
}

/// A timestamp in any of the shapes the export has used.
pub(crate) fn time_of(v: &Value) -> Option<DateTime<Utc>> {
    match v {
        Value::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.with_timezone(&Utc))
            .or_else(|| s.parse::<i64>().ok().and_then(epoch)),
        Value::Number(n) => n
            .as_i64()
            .or_else(|| {
                #[allow(clippy::cast_possible_truncation)]
                n.as_f64().map(|f| f as i64)
            })
            .and_then(epoch),
        Value::Object(o) => o
            .get("$date")
            .or_else(|| o.get("$numberLong"))
            .and_then(time_of),
        _ => None,
    }
}

/// Seconds or milliseconds since the epoch, told apart by size.
fn epoch(n: i64) -> Option<DateTime<Utc>> {
    if n > 100_000_000_000 {
        DateTime::<Utc>::from_timestamp_millis(n)
    } else {
        DateTime::<Utc>::from_timestamp(n, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conversation_with_both_parties_and_mixed_time_shapes() {
        let item = serde_json::json!({
            "conversation": {"id": "c1", "title": "Rust 生命周期", "create_time": "2026-01-02T03:04:05Z"},
            "responses": [
                {"response": {"_id": "r2", "message": "生命周期标注描述的是引用之间的约束。", "sender": "ASSISTANT",
                              "create_time": {"$date": {"$numberLong": "1767323110000"}}}},
                {"response": {"_id": "r1", "message": "什么是生命周期？", "sender": "human",
                              "create_time": 1_767_323_100}}
            ]
        });
        let convs = parse(&[item]).expect("parse");
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(c.source_id, "c1");
        assert_eq!(c.title, "Rust 生命周期");
        let roles: Vec<_> = c.messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant],
            "sorted by time, whatever order the export listed them in"
        );
    }

    #[test]
    fn a_record_without_a_conversation_names_its_keys() {
        let err = parse(&[serde_json::json!({"thread": {}})]).expect_err("must fail");
        assert!(format!("{err}").contains("thread"), "{err}");
    }
}

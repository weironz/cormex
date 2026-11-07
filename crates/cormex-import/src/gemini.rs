//! Gemini's Google Takeout export (`My Activity › Gemini Apps › MyActivity.json`).
//!
//! # Where the format came from
//!
//! Google does not document it. The shape here is the one guides and tools
//! describe for the JSON form of Takeout's "My Activity" (the HTML form is not
//! supported), and it has not been checked against a real account's export:
//!
//! ```text
//! [{"header":"Gemini Apps","title":"Prompted 什么是 keyset 分页",
//!   "time":"2026-01-02T03:04:05.678Z","products":["Gemini Apps"],
//!   "safeHtmlItem":[{"html":"<p>Keyset 分页……</p>"}]}, …]
//! ```
//!
//! # One activity is one session
//!
//! Takeout records each prompt as its own activity, with **no conversation
//! id**: the thread a prompt belonged to is not in the export. Grouping by time
//! gaps would merge two unrelated chats that happened close together, which is
//! worse than splitting one chat into several, so each activity becomes one
//! session of a prompt and its answer. Its id is derived from the activity's
//! time and title, which a later export of the same activity repeats.
//!
//! Activities that are not prompts (feature toggles, feedback, "Used Gemini
//! Apps") carry no answer and are skipped.

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::{Conversation, Message, Role, keys_of};

/// The English title prefix. Other locales write other words; a title without
/// it is kept whole rather than guessed at.
const PROMPTED: &str = "Prompted ";

/// Parse Takeout activity records.
///
/// # Errors
/// A record has no `time`.
pub fn parse(items: &[Value]) -> Result<Vec<Conversation>> {
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Some(time) = item.get("time").and_then(Value::as_str) else {
            bail!(
                "activity {i} of the Gemini export has no `time`; the keys it does have: {}",
                keys_of(item)
            );
        };
        let Some(at) = DateTime::parse_from_rfc3339(time)
            .ok()
            .map(|t| t.with_timezone(&Utc))
        else {
            continue;
        };
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let answer = item
            .get("safeHtmlItem")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|h| h.get("html").and_then(Value::as_str))
                    .map(html_to_text)
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default();
        if answer.trim().is_empty() {
            continue;
        }
        let prompt = title
            .strip_prefix(PROMPTED)
            .unwrap_or(title)
            .trim()
            .to_owned();
        let id = activity_id(time, title);
        let mut messages = Vec::with_capacity(2);
        if !prompt.is_empty() {
            messages.push(Message {
                source_id: format!("{id}-q"),
                role: Role::User,
                text: prompt.clone(),
                at,
            });
        }
        messages.push(Message {
            source_id: format!("{id}-a"),
            role: Role::Assistant,
            text: answer.trim().to_owned(),
            at,
        });
        out.push(Conversation {
            source_id: id,
            title: prompt,
            at,
            messages,
        });
    }
    Ok(out)
}

/// An activity's id and title (the prompt), or `None` for an activity that is
/// not a prompt with an answer. The splitter uses the same rule.
#[must_use]
pub fn identity(item: &Value) -> Option<(String, String)> {
    parse(std::slice::from_ref(item))
        .ok()?
        .into_iter()
        .next()
        .map(|c| (c.source_id, c.title))
}

/// A stable id for an activity: FNV-1a over its time and title.
///
/// Not a security hash; it only has to be the same for the same activity in
/// two exports, and different for two activities.
#[must_use]
pub fn activity_id(time: &str, title: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in time.bytes().chain([0x1f]).chain(title.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("gemini-{h:016x}")
}

/// The text of Takeout's answer HTML: tags dropped, block ends as newlines,
/// the common entities decoded.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            for t in chars.by_ref() {
                if t == '>' {
                    break;
                }
                tag.push(t);
            }
            let name = tag
                .trim_start_matches('/')
                .split(|ch: char| ch.is_whitespace() || ch == '/')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(
                name.as_str(),
                "br" | "p" | "div" | "li" | "h1" | "h2" | "h3" | "tr" | "pre"
            ) && !out.ends_with('\n')
            {
                out.push('\n');
            }
        } else {
            out.push(c);
        }
    }
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_and_its_answer_become_one_session() {
        let item = serde_json::json!({
            "header": "Gemini Apps",
            "title": "Prompted 什么是 keyset 分页",
            "time": "2026-01-02T03:04:05.678Z",
            "products": ["Gemini Apps"],
            "safeHtmlItem": [{"html": "<p>Keyset 分页用上一页的末尾作游标。</p><p>不用 OFFSET &amp; 不跳行。</p>"}]
        });
        let convs = parse(&[item]).expect("parse");
        assert_eq!(convs.len(), 1);
        let c = &convs[0];
        assert_eq!(
            c.title, "什么是 keyset 分页",
            "the English prefix is not part of the prompt"
        );
        assert_eq!(c.messages[0].role, Role::User);
        assert_eq!(
            c.messages[1].text, "Keyset 分页用上一页的末尾作游标。\n不用 OFFSET & 不跳行。",
            "tags dropped, paragraphs kept as lines, entities decoded"
        );
    }

    #[test]
    fn an_activity_without_an_answer_is_skipped() {
        let item = serde_json::json!({
            "header": "Gemini Apps", "title": "Used Gemini Apps",
            "time": "2026-01-02T03:04:05Z", "products": ["Gemini Apps"]
        });
        assert!(parse(&[item]).expect("parse").is_empty());
    }

    #[test]
    fn the_same_activity_gets_the_same_id_in_two_exports() {
        let a = activity_id("2026-01-02T03:04:05.678Z", "Prompted x");
        assert_eq!(a, activity_id("2026-01-02T03:04:05.678Z", "Prompted x"));
        assert_ne!(
            a,
            activity_id("2026-01-02T03:04:05.679Z", "Prompted x"),
            "two prompts with the same words a moment apart are two sessions"
        );
    }
}

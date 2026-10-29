//! `cormex sessions ls` and `cormex sessions show`.
//!
//! Design: docs/plan/session-archive-plan.md P2-7.
//!
//! # Why the CLI has this at all
//!
//! The Web browser is the place to read a conversation. This exists for the
//! case where there is no browser: an agent driving the CLI, a shell on a
//! server, a script that wants `--json`. The rendering is deliberately plain —
//! `[time] role: text` — because anything richer would be a second UI to keep
//! in step with the first.

use anyhow::Result;
use cormex_proto::archive::ArchiveSessionDto;
use cormex_proto::transcript::{ContentBlock, TranscriptEventDto};

use crate::client::ApiClient;

/// How many sessions `ls` asks for by default.
const DEFAULT_LIMIT: i64 = 50;

/// How many events `show` asks for by default.
const DEFAULT_EVENTS: i64 = 100;

/// `cormex sessions ls`.
///
/// # Errors
/// The server is unreachable, or it refused the credential.
pub async fn ls(
    client: &ApiClient,
    agent: Option<&str>,
    project: Option<&str>,
    limit: i64,
    json: bool,
) -> Result<String> {
    let mut query = vec![format!(
        "limit={}",
        if limit <= 0 { DEFAULT_LIMIT } else { limit }
    )];
    if let Some(a) = agent.filter(|s| !s.is_empty()) {
        query.push(format!("agent={}", urlencode(a)));
    }
    if let Some(p) = project.filter(|s| !s.is_empty()) {
        query.push(format!("project={}", urlencode(p)));
    }
    let page: cormex_proto::archive::ArchiveSessionsResponse = client
        .get_json(&format!("/archive/sessions?{}", query.join("&")))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    if json {
        return Ok(serde_json::to_string_pretty(&page)?);
    }
    Ok(render_list(&page.sessions))
}

/// Render the list as a table.
#[must_use]
pub fn render_list(sessions: &[ArchiveSessionDto]) -> String {
    if sessions.is_empty() {
        return "no sessions captured yet\n".to_owned();
    }
    let mut out = String::new();
    out.push_str(&format!(
        "{:<26} {:<14} {:>7} {:<12} {}\n",
        "id", "agent", "events", "last", "title"
    ));
    for s in sessions {
        out.push_str(&format!(
            "{:<26} {:<14} {:>7} {:<12} {}{}\n",
            s.id,
            s.agent,
            s.event_count,
            s.last_at
                .as_deref()
                .map(|t| t.split('T').next().unwrap_or(t))
                .unwrap_or("—"),
            s.title.clone().unwrap_or_else(|| "(untitled)".to_owned()),
            // The marker is a word, not a symbol: a terminal with no colour and
            // a font with no glyph both turn a symbol into nothing
            if s.sole_copy { "  [only copy]" } else { "" }
        ));
    }
    out
}

/// `cormex sessions show`.
///
/// # Errors
/// The server is unreachable, the session does not exist, or it has not been
/// projected yet.
pub async fn show(
    client: &ApiClient,
    id: &str,
    view: &str,
    raw: bool,
    limit: i64,
) -> Result<String> {
    if raw {
        return show_raw(client, id).await;
    }

    let detail: cormex_proto::archive::ArchiveSessionDetail = client
        .get_json(&format!("/archive/sessions/{}", urlencode(id)))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // A failed projection says so rather than printing an empty conversation:
    // an empty one reads as "this session has nothing in it", and the actual
    // situation is "the parser failed on this input"
    if detail.projection_status.as_deref() == Some("failed") {
        anyhow::bail!(
            "this session's projection failed: {}",
            detail
                .projection_error
                .unwrap_or_else(|| "(no error recorded)".into())
        );
    }

    let page: cormex_proto::archive::ArchiveEventsResponse = client
        .get_json(&format!(
            "/archive/sessions/{}/events?view={}&limit={}",
            urlencode(id),
            urlencode(view),
            if limit <= 0 { DEFAULT_EVENTS } else { limit }
        ))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut out = render_header(&detail.session);
    out.push_str(&render_events(&page.events));
    if page.next_cursor.is_some() {
        out.push_str("\n(more events; pass --limit to see more)\n");
    }
    Ok(out)
}

/// Render the session's header lines.
#[must_use]
pub fn render_header(s: &ArchiveSessionDto) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n", s.title.as_deref().unwrap_or("(untitled)")));
    out.push_str(&format!(
        "{} · {} events · {}\n",
        s.agent,
        s.event_count,
        s.project_key.as_deref().unwrap_or("no project")
    ));
    if s.sole_copy {
        out.push_str("⚠ Cormex holds the only copy: the host deleted its own files.\n");
    }
    out.push('\n');
    out
}

/// Render events as `[time] role: text`.
#[must_use]
pub fn render_events(events: &[TranscriptEventDto]) -> String {
    let mut out = String::new();
    for e in events {
        // `meta` and `unknown` are part of the session but not something anyone
        // said; printing them would bury the conversation
        if matches!(
            e.kind,
            cormex_proto::transcript::TranscriptKind::Meta
                | cormex_proto::transcript::TranscriptKind::Unknown
        ) {
            continue;
        }
        let time = e
            .occurred_at
            .as_deref()
            .map(|t| t.replace('T', " ").replace('Z', ""))
            .unwrap_or_else(|| "—".to_owned());
        let role = role_label(e);
        let body = event_text(e);
        out.push_str(&format!("[{time}] {role}: {body}\n"));
    }
    out
}

fn role_label(e: &TranscriptEventDto) -> String {
    use cormex_proto::transcript::TranscriptKind as K;
    match e.kind {
        K::UserMessage => "user".to_owned(),
        K::AgentMessage => "agent".to_owned(),
        K::AgentThought => "thought".to_owned(),
        K::ToolCall => format!("call {}", e.tool_name.as_deref().unwrap_or("")),
        K::ToolResult => "result".to_owned(),
        K::Plan => "plan".to_owned(),
        K::System => "system".to_owned(),
        K::Meta | K::Unknown => "meta".to_owned(),
    }
}

fn event_text(e: &TranscriptEventDto) -> String {
    let text = e.text.as_deref().unwrap_or("");
    if !text.is_empty() {
        return text.replace('\n', " ");
    }
    // No plain text: fall back to the **first** block that has something
    // readable. `first()` rather than a loop, because every arm returns — a
    // loop whose body returns on every path runs exactly once, and clippy is
    // right to call that out
    match e.content.first() {
        Some(b) => match b {
            ContentBlock::Text { text } | ContentBlock::Thought { text } => {
                text.replace('\n', " ")
            }
            ContentBlock::ToolCall { name, .. } => name.clone(),
            ContentBlock::ToolResult { output, .. } => output.replace('\n', " "),
            ContentBlock::Diff { path, .. } => path.clone(),
            ContentBlock::Plan { entries } => entries.join("; "),
            ContentBlock::Image { blob_ref } => format!("[image {blob_ref}]"),
            ContentBlock::ResourceRef { length, .. } => {
                format!("[{length} bytes, see the raw record]")
            }
            ContentBlock::Unknown { raw_type } => format!("[{raw_type}]"),
        },
        None => String::new(),
    }
}

/// `--raw`: the original record for the session's newest generation.
async fn show_raw(client: &ApiClient, id: &str) -> Result<String> {
    let detail: cormex_proto::archive::ArchiveSessionDetail = client
        .get_json(&format!("/archive/sessions/{}", urlencode(id)))
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // The **newest generation of the first file**: a session can have several,
    // and "the raw record" without saying which one is not a question with an
    // answer
    let generation = detail
        .files
        .first()
        .and_then(|f| f.generations.last())
        .map(|g| g.id.clone())
        .ok_or_else(|| anyhow::anyhow!("this session has no files to show"))?;

    // Masked by default: `raw:read` is a scope the CLI's key may not have, and
    // unmasking is a deliberate act even where it does
    let bytes = client
        .get_bytes(&format!("/archive/raw/{}", urlencode(&generation)), &[])
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Percent-encode a query value.
///
/// Hand-rolled rather than pulling in a URL crate for one function: the values
/// here are ids, agent names and project keys, and everything outside that set
/// is escaped byte by byte.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cormex_proto::transcript::{SourceSpan, TranscriptKind};

    fn session(id: &str, title: Option<&str>, sole: bool) -> ArchiveSessionDto {
        ArchiveSessionDto {
            id: id.into(),
            agent: "claude_code".into(),
            title: title.map(str::to_owned),
            project_key: Some("github.com/u/p".into()),
            devices: vec![],
            first_at: Some("2026-10-29T08:00:00Z".into()),
            last_at: Some("2026-10-29T09:30:00Z".into()),
            event_count: 12,
            summary: None,
            sole_copy: sole,
        }
    }

    fn event(kind: TranscriptKind, text: &str) -> TranscriptEventDto {
        TranscriptEventDto {
            id: "01E".into(),
            seq: 0,
            kind,
            occurred_at: Some("2026-10-29T08:00:00Z".into()),
            role: None,
            content: vec![],
            text: Some(text.into()),
            tool_name: None,
            tool_call_id: None,
            source: SourceSpan {
                generation_id: "01G".into(),
                offset: 0,
                length: 1,
            },
        }
    }

    /// The list's columns line up and the only-copy marker is a **word**.
    #[test]
    fn the_list_renders_a_table_and_marks_the_only_copy() {
        let out = render_list(&[session("01S", Some("对象存储用什么"), true)]);
        assert!(out.contains("01S"), "{out}");
        assert!(
            out.contains("2026-10-29"),
            "the date should be date-only: {out}"
        );
        assert!(
            out.contains("[only copy]"),
            "the marker must be a word: a terminal with no colour and a font with no glyph both \
             turn a symbol into nothing: {out}"
        );
    }

    #[test]
    fn an_untitled_session_says_so() {
        let out = render_list(&[session("01S", None, false)]);
        assert!(out.contains("(untitled)"), "{out}");
    }

    #[test]
    fn an_empty_list_says_there_is_nothing_yet() {
        assert_eq!(render_list(&[]), "no sessions captured yet\n");
    }

    /// Events render as `[time] role: text`, and `meta` is skipped.
    #[test]
    fn events_render_in_the_documented_shape() {
        let out = render_events(&[
            event(TranscriptKind::UserMessage, "hello"),
            event(TranscriptKind::Meta, "mode"),
            event(TranscriptKind::AgentMessage, "hi there"),
        ]);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "meta events must be skipped -- they are more than half a real session and would \
             bury the conversation: {out}"
        );
        assert_eq!(lines[0], "[2026-10-29 08:00:00] user: hello");
        assert_eq!(lines[1], "[2026-10-29 08:00:00] agent: hi there");
    }

    /// A multi-line message is flattened to one line.
    ///
    /// The output is one record per line so that `grep` and `awk` work on it;
    /// a message with newlines would break that.
    #[test]
    fn a_multiline_message_is_flattened() {
        let out = render_events(&[event(TranscriptKind::UserMessage, "one\ntwo")]);
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(out.contains("one two"), "{out}");
    }

    #[test]
    fn the_header_marks_the_only_copy() {
        let out = render_header(&session("01S", Some("t"), true));
        assert!(out.contains("only copy"), "{out}");
        assert!(out.contains("12 events"), "{out}");
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(urlencode("github.com/u/p"), "github.com%2Fu%2Fp");
        assert_eq!(urlencode("claude_code"), "claude_code");
    }
}

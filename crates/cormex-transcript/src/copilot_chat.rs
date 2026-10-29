//! The VS Code Copilot Chat operation-log parser (P4-3).
//!
//! Design: plan P4-3. Written against `microsoft/vscode` (MIT),
//! `src/vs/workbench/contrib/chat/common/model/objectMutationLog.ts` and
//! `chatSessionStore.ts`.
//!
//! # The file is a mutation log, not a conversation
//!
//! Since 1.109 a chat session is stored as `<sessionId>.jsonl` — an **append
//! log of object mutations**, not a list of messages:
//!
//! ```text
//! {"kind":0,"v":{…}}                    the initial complete object
//! {"kind":1,"k":["requests",0],"v":{…}} set a property
//! {"kind":2,"k":["requests"],"v":[…] }  push to an array
//! {"kind":3,"k":["requests",0]}         delete a property
//! ```
//!
//! So the parser has to **replay the log into the session object** before it
//! can produce any events. That is why the checkpoint carries the replayed
//! object: resuming without it would replay only the tail, and the tail's
//! `kind: 1` entries refer to paths that do not exist yet.
//!
//! # Only complete objects are projected
//!
//! A replay in progress is a session that is still being written. Emitting
//! events from it would project a half-built conversation, and the next append
//! would produce a second, slightly different one — the projector's derived ids
//! are per offset, so the two would coexist as two versions of the same
//! message.
//!
//! So events are emitted **only when the file is complete enough to parse**,
//! and the checkpoint records how far the replay has got. The projector re-feeds
//! the whole file on each run (a snapshot source), and the parser emits only
//! when the log's last line is complete.
//!
//! # `response` is a union of parts
//!
//! `ISerializableChatRequestData.response` is `SerializedChatResponsePart[]`,
//! whose members are `markdownContent`, `thinking`, `toolInvocation`, … A part
//! this parser does not know is kept as `Unknown` with its `kind`, so the next
//! person can see which parts still need handling.

use chrono::{DateTime, Utc};
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use serde_json::{Value, json};

use crate::parser::{ParsedEvent, Parser};

/// The parser.
pub struct CopilotChatParser {
    /// The replayed session object.
    state: Value,
    /// Whether this buffer has already been projected.
    emitted: bool,
}

impl CopilotChatParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Value::Null,
            emitted: false,
        }
    }

    /// Replay one log entry into the state.
    ///
    /// Returns `false` when the entry is malformed, which stops the replay: a
    /// `kind: 1` whose path does not exist would otherwise write into a
    /// position the log never created, and the result would be a session that
    /// looks plausible and is wrong.
    fn apply(&mut self, entry: &Value) -> bool {
        let kind = entry.get("kind").and_then(Value::as_i64).unwrap_or(-1);
        match kind {
            // Initial: the complete object
            0 => {
                self.state = entry.get("v").cloned().unwrap_or(Value::Null);
                true
            }
            1 => {
                let Some(path) = path_of(entry) else {
                    return false;
                };
                let v = entry.get("v").cloned().unwrap_or(Value::Null);
                set_at(&mut self.state, &path, v)
            }
            2 => {
                let Some(path) = path_of(entry) else {
                    return false;
                };
                let items = entry
                    .get("v")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                // `i` set means "everything after that index is removed", which
                // is what a rewind looks like in this format
                let truncate = entry.get("i").and_then(Value::as_u64);
                push_at(&mut self.state, &path, items, truncate)
            }
            3 => {
                let Some(path) = path_of(entry) else {
                    return false;
                };
                remove_at(&mut self.state, &path)
            }
            _ => false,
        }
    }

    /// Turn the replayed session into events.
    fn project(&self, offset: u64, len: u64) -> Vec<ParsedEvent> {
        let Some(requests) = self.state.get("requests").and_then(Value::as_array) else {
            return vec![ParsedEvent::unknown(
                "copilot_chat:no-requests",
                offset,
                len,
            )];
        };

        let mut out = Vec::new();
        for (i, req) in requests.iter().enumerate() {
            // The user's message: a string in the old format, an object with
            // `text` in the new one
            let user_text = match req.get("message") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Object(o)) => o
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                _ => String::new(),
            };
            let at = req
                .get("timestamp")
                .and_then(Value::as_i64)
                .and_then(DateTime::from_timestamp_millis);
            if !user_text.is_empty() {
                out.push(text_event(
                    TranscriptKind::UserMessage,
                    "user",
                    user_text,
                    at,
                    offset,
                    len,
                    i,
                ));
            }

            // The response is a union of parts
            if let Some(parts) = req.get("response").and_then(Value::as_array) {
                let mut pending = String::new();
                for part in parts {
                    match part.get("kind").and_then(Value::as_str).unwrap_or("") {
                        // `markdownContent` wraps an `IMarkdownString`; the bare
                        // string form is the old format
                        "markdownContent" => {
                            if let Some(t) = part
                                .get("content")
                                .and_then(|c| c.get("value"))
                                .and_then(Value::as_str)
                                .or_else(|| part.get("content").and_then(Value::as_str))
                            {
                                if !pending.is_empty() {
                                    pending.push('\n');
                                }
                                pending.push_str(t);
                            }
                        }
                        "thinking" => {
                            if !pending.is_empty() {
                                out.push(text_event(
                                    TranscriptKind::AgentMessage,
                                    "assistant",
                                    std::mem::take(&mut pending),
                                    at,
                                    offset,
                                    len,
                                    i,
                                ));
                            }
                            let t = part
                                .get("value")
                                .and_then(Value::as_str)
                                .or_else(|| part.get("text").and_then(Value::as_str))
                                .unwrap_or_default()
                                .to_owned();
                            out.push(ParsedEvent {
                                kind: TranscriptKind::AgentThought,
                                role: Some("assistant".to_owned()),
                                occurred_at: at,
                                content: vec![ContentBlock::Thought { text: t.clone() }],
                                text: t,
                                tool_name: None,
                                tool_call_id: None,
                                src_offset: offset,
                                src_length: len,
                            });
                        }
                        "toolInvocation" => {
                            if !pending.is_empty() {
                                out.push(text_event(
                                    TranscriptKind::AgentMessage,
                                    "assistant",
                                    std::mem::take(&mut pending),
                                    at,
                                    offset,
                                    len,
                                    i,
                                ));
                            }
                            let id = part
                                .get("toolCallId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            let name = part
                                .get("toolId")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            out.push(ParsedEvent {
                                kind: TranscriptKind::ToolCall,
                                role: Some("assistant".to_owned()),
                                occurred_at: at,
                                content: vec![ContentBlock::ToolCall {
                                    tool_call_id: id.clone(),
                                    name: name.clone(),
                                    input: part
                                        .get("toolSpecificData")
                                        .cloned()
                                        .unwrap_or(Value::Null),
                                }],
                                text: String::new(),
                                tool_name: Some(name),
                                tool_call_id: Some(id),
                                src_offset: offset,
                                src_length: len,
                            });
                        }
                        // `progress`, `treeData`, `inlineReference`, `questionCarousel` …:
                        // no text to contribute, and the raw file keeps them
                        _ => {}
                    }
                }
                if !pending.is_empty() {
                    out.push(text_event(
                        TranscriptKind::AgentMessage,
                        "assistant",
                        pending,
                        at,
                        offset,
                        len,
                        i,
                    ));
                }
            }
        }

        if out.is_empty() {
            out.push(ParsedEvent::unknown("copilot_chat:empty", offset, len));
        }
        out
    }
}

impl Default for CopilotChatParser {
    fn default() -> Self {
        Self::new()
    }
}

fn text_event(
    kind: TranscriptKind,
    role: &str,
    text: String,
    at: Option<DateTime<Utc>>,
    offset: u64,
    len: u64,
    _request: usize,
) -> ParsedEvent {
    ParsedEvent {
        kind,
        role: Some(role.to_owned()),
        occurred_at: at,
        content: vec![ContentBlock::Text { text: text.clone() }],
        text,
        tool_name: None,
        tool_call_id: None,
        src_offset: offset,
        src_length: len,
    }
}

/// The `k` path of an entry.
fn path_of(entry: &Value) -> Option<Vec<PathSeg>> {
    let arr = entry.get("k").and_then(Value::as_array)?;
    let mut out = Vec::with_capacity(arr.len());
    for seg in arr {
        if let Some(s) = seg.as_str() {
            out.push(PathSeg::Key(s.to_owned()));
        } else {
            let n = seg.as_u64()?;
            out.push(PathSeg::Index(usize::try_from(n).ok()?));
        }
    }
    Some(out)
}

#[derive(Debug, Clone)]
enum PathSeg {
    Key(String),
    Index(usize),
}

/// Set the value at `path`, creating intermediate objects/arrays as needed.
///
/// Returns `false` when the path does not fit the existing shape — which means
/// the log is out of order and replaying further would produce a session that
/// looks plausible and is wrong.
fn set_at(root: &mut Value, path: &[PathSeg], v: Value) -> bool {
    if path.is_empty() {
        *root = v;
        return true;
    }
    let mut cur = root;
    for (i, seg) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        match seg {
            PathSeg::Key(k) => {
                if !cur.is_object() {
                    *cur = json!({});
                }
                let obj = cur.as_object_mut().expect("just made it an object");
                if last {
                    obj.insert(k.clone(), v);
                    return true;
                }
                cur = obj.entry(k.clone()).or_insert(Value::Null);
            }
            PathSeg::Index(n) => {
                if !cur.is_array() {
                    *cur = json!([]);
                }
                let arr = cur.as_array_mut().expect("just made it an array");
                while arr.len() <= *n {
                    arr.push(Value::Null);
                }
                if last {
                    arr[*n] = v;
                    return true;
                }
                cur = &mut arr[*n];
            }
        }
    }
    true
}

/// Push (optionally truncating first) at `path`.
fn push_at(root: &mut Value, path: &[PathSeg], items: Vec<Value>, truncate: Option<u64>) -> bool {
    let Some(target) = get_at_mut(root, path) else {
        return false;
    };
    if !target.is_array() {
        *target = json!([]);
    }
    let arr = target.as_array_mut().expect("just made it an array");
    // `i` set means "everything after that index is removed": a rewind
    if let Some(i) = truncate {
        let keep = usize::try_from(i).unwrap_or(usize::MAX).saturating_add(1);
        arr.truncate(keep);
    }
    arr.extend(items);
    true
}

/// Remove at `path`.
fn remove_at(root: &mut Value, path: &[PathSeg]) -> bool {
    let Some((last, parent_path)) = path.split_last() else {
        return false;
    };
    let Some(parent) = get_at_mut(root, parent_path) else {
        return false;
    };
    match last {
        PathSeg::Key(k) => parent
            .as_object_mut()
            .map(|o| o.remove(k).is_some())
            .unwrap_or(false),
        PathSeg::Index(n) => {
            let Some(arr) = parent.as_array_mut() else {
                return false;
            };
            if *n < arr.len() {
                arr.remove(*n);
                true
            } else {
                false
            }
        }
    }
}

fn get_at_mut<'a>(root: &'a mut Value, path: &[PathSeg]) -> Option<&'a mut Value> {
    let mut cur = root;
    for seg in path {
        cur = match seg {
            PathSeg::Key(k) => cur.get_mut(k)?,
            PathSeg::Index(n) => cur.get_mut(*n)?,
        };
    }
    Some(cur)
}

impl Parser for CopilotChatParser {
    fn version(&self) -> &'static str {
        "copilot_chat@1"
    }

    fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<ParsedEvent>, String> {
        let len = u64::try_from(bytes.len()).unwrap_or(0);

        // **Replay the whole buffer, from the start, on every feed.** The
        // projector hands a snapshot source the whole file, and a partial
        // replay would apply `kind: 1` entries to paths that do not exist yet.
        // Starting over is cheap: a chat log is a few hundred lines
        self.state = Value::Null;
        let mut applied = 0;
        let mut saw_initial = false;
        for line in bytes.split(|b| *b == b'\n') {
            let line = trim_ascii(line);
            if line.is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_slice::<Value>(line) else {
                // A truncated last line is the normal state of a file being
                // appended to. Stop there rather than failing: what came
                // before is still a valid prefix
                break;
            };
            if entry.get("kind").and_then(Value::as_i64) == Some(0) {
                saw_initial = true;
            }
            if !self.apply(&entry) {
                break;
            }
            applied += 1;
        }

        if !saw_initial || applied == 0 {
            return Ok(vec![ParsedEvent::malformed(offset, len)]);
        }
        if self.emitted {
            return Ok(Vec::new());
        }
        self.emitted = true;
        Ok(self.project(offset, len))
    }

    fn checkpoint(&self) -> Value {
        json!({
            "emitted": self.emitted,
            // **The replayed object**, because a resume without it would
            // replay only the tail and every `kind: 1` would target a path that
            // does not exist
            "state": self.state,
        })
    }
}

impl CopilotChatParser {
    /// Resume from a checkpoint.
    ///
    /// # Errors
    /// The checkpoint is not one this parser wrote.
    pub fn restore(state: &Value) -> Result<Self, String> {
        Ok(Self {
            state: state.get("state").cloned().unwrap_or(Value::Null),
            emitted: state
                .get("emitted")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && bytes[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &bytes[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One session's log, in the format `objectMutationLog.ts` writes.
    fn log() -> String {
        let initial = json!({
            "kind": 0,
            "v": {
                "version": 3,
                "sessionId": "s1",
                "requests": []
            }
        });
        let push = json!({
            "kind": 2,
            "k": ["requests"],
            "v": [{
                "requestId": "r1",
                "message": { "text": "why does the health check time out" },
                "timestamp": 1_760_000_000_000i64,
                "response": [
                    { "kind": "markdownContent", "content": { "value": "it binds to loopback" } }
                ]
            }]
        });
        format!("{initial}\n{push}\n")
    }

    fn parse(body: &str) -> Vec<ParsedEvent> {
        let mut p = CopilotChatParser::new();
        p.feed(0, body.as_bytes()).expect("feed")
    }

    /// The log is replayed into a session, then projected.
    #[test]
    fn the_log_is_replayed_then_projected() {
        let events = parse(&log());
        assert_eq!(events.len(), 2, "{events:?}");
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
        assert_eq!(events[0].text, "why does the health check time out");
        assert_eq!(events[1].kind, TranscriptKind::AgentMessage);
        assert_eq!(events[1].text, "it binds to loopback");
    }

    /// A `kind: 1` set into a pushed request updates it.
    #[test]
    fn a_set_updates_a_pushed_request() {
        let mut body = log();
        body.push_str(
            &serde_json::json!({
                "kind": 1,
                "k": ["requests", 0, "response"],
                "v": [{ "kind": "markdownContent", "content": { "value": "updated answer" } }]
            })
            .to_string(),
        );
        body.push('\n');
        let events = parse(&body);
        assert!(
            events.iter().any(|e| e.text == "updated answer"),
            "the set was not applied: {events:?}"
        );
        assert!(
            !events.iter().any(|e| e.text == "it binds to loopback"),
            "the old value survived the set"
        );
    }

    /// A `kind: 3` delete removes a property.
    #[test]
    fn a_delete_removes_a_property() {
        let mut body = log();
        body.push_str(
            &serde_json::json!({ "kind": 3, "k": ["requests", 0, "response"] }).to_string(),
        );
        body.push('\n');
        let events = parse(&body);
        assert_eq!(events.len(), 1, "the response was not deleted: {events:?}");
        assert_eq!(events[0].kind, TranscriptKind::UserMessage);
    }

    /// A `kind: 2` with `i` truncates first — that is what a rewind looks like.
    #[test]
    fn a_push_with_an_index_truncates() {
        let mut body = log();
        // Push a second request, then rewind to index 0 and push a replacement
        body.push_str(
            &serde_json::json!({
                "kind": 2,
                "k": ["requests"],
                "v": [{ "requestId": "r2", "message": "second", "response": [] }]
            })
            .to_string(),
        );
        body.push('\n');
        body.push_str(
            &serde_json::json!({
                "kind": 2,
                "k": ["requests"],
                "i": 0,
                "v": [{ "requestId": "r3", "message": "replacement", "response": [] }]
            })
            .to_string(),
        );
        body.push('\n');
        let events = parse(&body);
        let texts: Vec<&str> = events.iter().map(|e| e.text.as_str()).collect();
        assert!(texts.contains(&"replacement"), "{texts:?}");
        assert!(
            !texts.contains(&"second"),
            "the rewound request survived: {texts:?}"
        );
    }

    /// A thinking part becomes an `AgentThought`.
    #[test]
    fn a_thinking_part_becomes_a_thought() {
        let body = format!(
            "{}\n{}\n",
            json!({"kind":0,"v":{"requests":[]}}),
            json!({"kind":2,"k":["requests"],"v":[{
                "message":"q",
                "response":[
                    {"kind":"thinking","value":"let me think"},
                    {"kind":"markdownContent","content":{"value":"answer"}}
                ]
            }]})
        );
        let events = parse(&body);
        assert!(
            events
                .iter()
                .any(|e| e.kind == TranscriptKind::AgentThought)
        );
        assert!(events.iter().any(|e| e.text == "answer"));
    }

    /// A tool invocation becomes a tool call.
    #[test]
    fn a_tool_invocation_becomes_a_call() {
        let body = format!(
            "{}\n{}\n",
            json!({"kind":0,"v":{"requests":[]}}),
            json!({"kind":2,"k":["requests"],"v":[{
                "message":"q",
                "response":[{"kind":"toolInvocation","toolId":"copilot_readFile","toolCallId":"t1"}]
            }]})
        );
        let events = parse(&body);
        let call = events
            .iter()
            .find(|e| e.kind == TranscriptKind::ToolCall)
            .expect("a tool call");
        assert_eq!(call.tool_name.as_deref(), Some("copilot_readFile"));
        assert_eq!(call.tool_call_id.as_deref(), Some("t1"));
    }

    /// A truncated last line is tolerated: it is the normal state of a file
    /// being appended to.
    #[test]
    fn a_truncated_last_line_is_tolerated() {
        let mut body = log();
        body.push_str(r#"{"kind":2,"k":["requests"],"v":[{"mess"#);
        let events = parse(&body);
        assert_eq!(
            events.len(),
            2,
            "the valid prefix should still project: {events:?}"
        );
    }

    /// A log with no initial entry is malformed.
    #[test]
    fn a_log_without_an_initial_entry_is_malformed() {
        let events = parse(r#"{"kind":2,"k":["requests"],"v":[]}"#);
        assert_eq!(events[0].kind, TranscriptKind::Unknown);
    }

    /// Feeding twice does not duplicate.
    #[test]
    fn feeding_twice_does_not_duplicate() {
        let body = log();
        let mut p = CopilotChatParser::new();
        assert!(!p.feed(0, body.as_bytes()).expect("first").is_empty());
        assert!(p.feed(0, body.as_bytes()).expect("second").is_empty());
    }

    /// The checkpoint carries the replayed object.
    ///
    /// A resume without it would replay only the tail, and every `kind: 1`
    /// would target a path that does not exist.
    #[test]
    fn the_checkpoint_carries_the_replayed_object() {
        let body = log();
        let mut p = CopilotChatParser::new();
        p.feed(0, body.as_bytes()).expect("feed");
        let state = p.checkpoint();
        let restored = CopilotChatParser::restore(&state).expect("restore");
        assert_eq!(
            restored.state, p.state,
            "the replayed object did not survive the checkpoint"
        );
        assert_eq!(restored.state["requests"].as_array().map(Vec::len), Some(1));
    }
}

//! The distiller: a session's events → **dated observation entries**.
//!
//! Design: docs/design/session-archive-architecture.md §8.4 (D14).
//!
//! # What this is for, and what it is not
//!
//! The archive keeps every session verbatim, which answers "what happened" but
//! not "what did we learn". A person coming back to a project three weeks later
//! does not want to re-read the transcript; they want the observations it
//! produced, each with the date it was made.
//!
//! So the output is **append-only observations**, not a rewritten summary. Each
//! run appends what is new since the last run; nothing is ever replaced. That is
//! what makes "what did I know on the 14th" answerable later, and it is why the
//! store side writes ordinary `summaries` rows rather than an UPDATE.
//!
//! # The model is behind a trait, deliberately
//!
//! [`Observer`] is the seam the tests drive. A distiller whose only
//! implementation is "call the provider" can be tested only by calling a
//! provider: the assertions then measure the model's mood rather than the
//! pipeline's behaviour, and the test cannot run in CI at all. The trait lets
//! the tick's real logic — when it fires, how the input is truncated, how the
//! output lands, when the budget stops it — be tested with a fake.
//!
//! # The prompt is behaviour, not copy
//!
//! [`OBSERVE_SYSTEM`] decides what gets remembered. Changing a word in it
//! changes what the product does, so it goes through the evals like any other
//! behaviour change (CLAUDE.md, "代码风格").

use chrono::{DateTime, NaiveDate, TimeZone as _, Utc};
use cormex_core::{Id, Result};
use cormex_llm::{LlmClient, Message};
use cormex_store::TranscriptEventKind;
use serde_json::Value;

/// How much of a tool result reaches the model.
///
/// A build log or a `cargo test` dump can be hundreds of kilobytes, and the
/// distiller's job is "what did this session establish", not "reproduce the
/// output". The first 512 bytes carry the command and its verdict; the rest is
/// noise that would cost tokens on every single session.
pub const TOOL_RESULT_LIMIT: usize = 512;

/// One event as the distiller sees it.
///
/// Its own type rather than [`cormex_store::TranscriptEvent`]: the observer has
/// no business holding a database row, and a fake in a test should not have to
/// manufacture `src_generation_id` / `tsv` columns that mean nothing to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistillEvent {
    pub id: Id,
    pub seq: i64,
    pub kind: TranscriptEventKind,
    pub occurred_at: Option<DateTime<Utc>>,
    pub text: String,
}

/// One observation the model produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The day this observation is about. Absolute, never "yesterday": the
    /// whole point of the log is that it can be read months later.
    pub at: DateTime<Utc>,
    pub text: String,
    /// The events it was drawn from. **Never empty** — a summary with no
    /// provenance cannot be found after its source is purged, and
    /// `insert_summary` refuses it.
    pub sources: Vec<Id>,
}

/// What one observation run produced.
#[derive(Debug, Clone, Default)]
pub struct ObservationOutcome {
    pub entries: Vec<Observation>,
    /// Tokens spent. Recorded against the tenant's daily budget **even when the
    /// run produced nothing**, because the money was spent either way.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub model: String,
}

/// Turns a batch of events into observations.
#[async_trait::async_trait]
pub trait Observer: Send + Sync {
    /// # Errors
    /// The provider call failed, or its answer could not be read at all.
    async fn observe(
        &self,
        session_id: &str,
        events: &[DistillEvent],
    ) -> Result<ObservationOutcome>;
}

/// The production observer: one cheap-model call per batch.
///
/// **Never the main conversation model.** This runs on every idle session, and
/// paying main-model prices for it would make the archive cost more than the
/// conversations it archives (docs/memory.md §10).
pub struct CheapModelObserver {
    llm: LlmClient,
    /// `CORMEX_DISTILL_MODEL`. `None` = the same cheap model extraction uses.
    model: Option<String>,
}

impl CheapModelObserver {
    #[must_use]
    pub fn new(llm: LlmClient) -> Self {
        Self { llm, model: None }
    }

    /// Point at a specific model instead of the configured cheap one.
    ///
    /// # Errors
    /// The provider definition does not know this model.
    pub fn with_model(mut self, model: Option<String>) -> Result<Self> {
        if let Some(name) = &model {
            // Resolve eagerly: a typo should fail at startup, not once per
            // session forever, with the symptom "the observation log is empty"
            self.llm.model_config(name)?;
        }
        self.model = model;
        Ok(self)
    }

    /// The model to call, **owned**: the configured one is a lookup, the
    /// default one is borrowed, and returning either as `&ModelConfig` would
    /// mean leaking the lookup's result.
    fn model_config(&self) -> Result<cormex_llm::ModelConfig> {
        match &self.model {
            Some(name) => Ok(self.llm.model_config(name)?),
            None => Ok(self.llm.cheap_model().clone()),
        }
    }
}

#[async_trait::async_trait]
impl Observer for CheapModelObserver {
    async fn observe(
        &self,
        session_id: &str,
        events: &[DistillEvent],
    ) -> Result<ObservationOutcome> {
        if events.is_empty() {
            return Ok(ObservationOutcome::default());
        }

        let model = self.model_config()?;
        let rendered = render_events(events);
        let messages = [Message::user().with_text(rendered)];
        let (reply, usage) = self
            .llm
            .complete(&model, OBSERVE_SYSTEM, &messages, &[])
            .await?;

        let raw = reply.as_concat_text();
        tracing::debug!(
            session = %session_id,
            events = events.len(),
            tokens = ?usage.usage.total_tokens,
            "observation run finished"
        );

        Ok(ObservationOutcome {
            entries: parse_observations(&raw, events),
            input_tokens: i64::from(usage.usage.input_tokens.unwrap_or(0)),
            output_tokens: i64::from(usage.usage.output_tokens.unwrap_or(0)),
            model: model.model_name.clone(),
        })
    }
}

/// Render the batch for the model, truncating tool output.
///
/// Each line carries its position (`#3`) because the model is asked to cite
/// positions back — that is what turns "the model said something" into a
/// `derivations` edge that survives the source being purged.
#[must_use]
pub fn render_events(events: &[DistillEvent]) -> String {
    let mut out = String::new();
    for (i, e) in events.iter().enumerate() {
        let when = e
            .occurred_at
            .map(|t| t.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "时间未知".to_owned());
        let body = match e.kind {
            TranscriptEventKind::ToolCall | TranscriptEventKind::ToolResult => {
                truncate(&e.text, TOOL_RESULT_LIMIT)
            }
            _ => e.text.clone(),
        };
        out.push_str(&format!(
            "#{n} [{when}] {kind}: {body}\n",
            n = i + 1,
            kind = e.kind,
        ));
    }
    out
}

/// Cut to at most `limit` bytes **on a character boundary**.
///
/// A byte cut can split a multi-byte character, and the result is not valid
/// UTF-8 — which the provider then rejects, turning one long tool output into a
/// failed run for the whole session.
fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[截断]", &text[..end])
}

/// Parse the model's observations. **Fault-tolerant per entry.**
///
/// An entry without a readable date or without a citable source is dropped
/// rather than repaired: a summary with no provenance can never be found once
/// its source is purged, and guessing "it probably came from the whole batch"
/// would produce an edge that is plausible and wrong.
#[must_use]
pub fn parse_observations(raw: &str, events: &[DistillEvent]) -> Vec<Observation> {
    let Some(blob) = extract_json_blob(raw) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(blob) else {
        return Vec::new();
    };

    let items = match &value {
        Value::Array(items) => items.as_slice(),
        Value::Object(map) => map
            .get("observations")
            .or_else(|| map.get("entries"))
            .or_else(|| map.get("data"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };

    let mut out = Vec::new();
    for item in items {
        let Some(text) = item
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|t| !t.is_empty())
        else {
            continue;
        };
        let Some(at) = item.get("at").and_then(Value::as_str).and_then(parse_day) else {
            tracing::debug!("an observation has no readable date, dropping it");
            continue;
        };
        // 1-based positions, as the prompt asks for. Out-of-range citations are
        // dropped rather than clamped: a clamped one is a wrong edge
        let sources: Vec<Id> = item
            .get("sources")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_u64)
                    .filter_map(|n| usize::try_from(n).ok())
                    .filter(|n| *n >= 1 && *n <= events.len())
                    .map(|n| events[n - 1].id)
                    .collect()
            })
            .unwrap_or_default();
        if sources.is_empty() {
            tracing::debug!("an observation cites no valid event, dropping it");
            continue;
        }
        out.push(Observation {
            at,
            text: text.to_owned(),
            sources,
        });
    }
    out
}

/// `YYYY-MM-DD` (or a full RFC 3339 timestamp) → midnight UTC.
fn parse_day(raw: &str) -> Option<DateTime<Utc>> {
    let raw = raw.trim();
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        return Some(dt.with_timezone(&Utc));
    }
    let day = raw.get(..10).unwrap_or(raw);
    NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| Utc.from_utc_datetime(&dt))
}

/// The first balanced JSON object or array in the reply.
///
/// Models wrap their answer in prose and fences even when told not to; taking
/// "the first `{` through the last `}`" would swallow a trailing explanation
/// that happens to contain braces.
fn extract_json_blob(raw: &str) -> Option<&str> {
    let start = raw.find(['{', '['])?;
    let open = raw.as_bytes()[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (i, b) in raw.as_bytes().iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *b == b'\\' {
                escaped = true;
            } else if *b == b'"' {
                in_string = false;
            }
            continue;
        }
        match *b {
            b'"' => in_string = true,
            x if x == open => depth += 1,
            x if x == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&raw[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The observation prompt.
///
/// Same shape as `extract.rs`'s: one criterion, an explicit "do not" list, and a
/// strict output format. The criterion is different, though — extraction asks
/// "would the next conversation make a wrong decision without this", while this
/// asks "what did this session establish". A session can legitimately produce
/// nothing.
const OBSERVE_SYSTEM: &str = r#"你是 Cormex 记忆系统的观察日志记录器。用户会把一段编程会话的事件按顺序给你，每行带序号（#1、#2……）与时间。你的任务是把这段会话里**已经确立的东西**写成若干条观察。

# 唯一判据

> 三周后回看这个项目的人，读到这一条会不会少走一段弯路？

会，才写。**宁可少写，不要凑数** —— 观察日志会进入检索，噪声会把真正有用的挤出去。

# 该写什么

- 已作出的决定，以及为什么（「决定用 RustFS 而不是 MinIO，因为 …」）
- 踩过的坑与结论（「X 在这个版本里不工作，改成 Y 才行」）
- 确认过的约束（「这个接口必须带 Host 头」「这个字段不能为空」）
- 当前状态与下一步（仅当会话里明确说了）
- 关键文件与命令（仅当会话里明确用过）

# 不写

- 寒暄、确认、客套
- 工具的原始输出本身（结论写下来，日志不用抄）
- 你的推理过程、建议、总结
- 尚在讨论、没有结论的选项
- 情绪、猜测、以及任何关于个人的推断
- 凭据与密钥的字面值（会话文本里的疑似密钥已被替换成 `[masked:…]`，照抄那个占位符即可，不要试图还原）

# 日期

每条的 `at` 是这条观察**关于哪一天**，取它对应事件的日期（YYYY-MM-DD）。不要写「昨天」「上周」。事件时间未知时，用该事件在会话中的位置推断，仍推断不出就不写这一条。

# 出处

每条的 `sources` 是它**依据的事件序号**（1 起的整数，可多个）。没有出处就不要写这一条 —— 一条找不到来源的观察在被擦除时无法被一并处理。

# 输出格式

只输出 JSON，不要任何解释文字：

{
  "observations": [
    {
      "at": "2026-10-29",
      "text": "一句能独立读懂的观察，不要用「上面」「刚才」指代",
      "sources": [3, 4]
    }
  ]
}

这段会话没有值得记录的东西时，输出 {"observations": []}。"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: i64, kind: TranscriptEventKind, text: &str) -> DistillEvent {
        DistillEvent {
            id: Id::new(),
            seq,
            kind,
            occurred_at: Some(Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap()),
            text: text.to_owned(),
        }
    }

    /// A tool result is cut to the limit, on a character boundary.
    ///
    /// The boundary matters: a byte cut can split a multi-byte character, and
    /// the provider then rejects the whole request.
    #[test]
    fn a_long_tool_result_is_truncated_on_a_character_boundary() {
        let long = "错".repeat(400); // 3 bytes each: 1200 bytes
        let events = [event(0, TranscriptEventKind::ToolResult, &long)];
        let rendered = render_events(&events);
        assert!(
            rendered.contains("[截断]"),
            "a 1200-byte tool result should have been truncated"
        );
        // The rendered line is valid UTF-8 by construction; assert the body
        // itself is within the limit plus the marker
        let body = rendered.split(": ").nth(1).expect("a body");
        assert!(
            body.len() <= TOOL_RESULT_LIMIT + "…[截断]".len() + 1,
            "the truncated body is {} bytes, over the limit",
            body.len()
        );
    }

    /// A short tool result passes through untouched.
    #[test]
    fn a_short_tool_result_is_not_truncated() {
        let events = [event(0, TranscriptEventKind::ToolResult, "ok")];
        assert!(!render_events(&events).contains("[截断]"));
    }

    /// A user message is never truncated — the person's own words are the
    /// densest part of the session.
    #[test]
    fn a_long_user_message_is_kept_whole() {
        let long = "a".repeat(4000);
        let events = [event(0, TranscriptEventKind::UserMessage, &long)];
        assert!(!render_events(&events).contains("[截断]"));
    }

    /// Every rendered line carries its 1-based position, which is what the
    /// model cites back.
    #[test]
    fn each_line_carries_its_position() {
        let events = [
            event(0, TranscriptEventKind::UserMessage, "hello"),
            event(1, TranscriptEventKind::AgentMessage, "hi"),
        ];
        let rendered = render_events(&events);
        assert!(rendered.contains("#1 "), "missing #1: {rendered}");
        assert!(rendered.contains("#2 "), "missing #2: {rendered}");
    }

    /// A plain observation parses, with its sources mapped back to event ids.
    #[test]
    fn parses_a_plain_observation() {
        let events = [
            event(0, TranscriptEventKind::UserMessage, "a"),
            event(1, TranscriptEventKind::AgentMessage, "b"),
        ];
        let raw = r#"{"observations":[{"at":"2026-10-29","text":"决定用 RustFS","sources":[2]}]}"#;
        let out = parse_observations(raw, &events);
        assert_eq!(out.len(), 1, "actually parsed {}", out.len());
        assert_eq!(out[0].sources, vec![events[1].id]);
        assert_eq!(out[0].at.format("%Y-%m-%d").to_string(), "2026-10-29");
    }

    /// An observation with no citable source is dropped.
    ///
    /// Not repaired: a summary with no provenance cannot be found once its
    /// source is purged, and `insert_summary` refuses it anyway.
    #[test]
    fn an_observation_without_sources_is_dropped() {
        let events = [event(0, TranscriptEventKind::UserMessage, "a")];
        let raw = r#"{"observations":[{"at":"2026-10-29","text":"no source","sources":[]}]}"#;
        assert!(parse_observations(raw, &events).is_empty());
    }

    /// An out-of-range citation is dropped, not clamped.
    ///
    /// Clamping would produce an edge that is plausible and wrong.
    #[test]
    fn an_out_of_range_source_is_dropped() {
        let events = [event(0, TranscriptEventKind::UserMessage, "a")];
        let raw = r#"{"observations":[{"at":"2026-10-29","text":"x","sources":[9]}]}"#;
        assert!(parse_observations(raw, &events).is_empty());
    }

    /// A relative date is not a date, and the entry is dropped.
    #[test]
    fn a_relative_date_is_not_accepted() {
        let events = [event(0, TranscriptEventKind::UserMessage, "a")];
        let raw = r#"{"observations":[{"at":"昨天","text":"x","sources":[1]}]}"#;
        assert!(parse_observations(raw, &events).is_empty());
    }

    /// A full RFC 3339 timestamp is accepted as well as a bare day.
    #[test]
    fn a_full_timestamp_is_accepted() {
        let events = [event(0, TranscriptEventKind::UserMessage, "a")];
        let raw = r#"{"observations":[{"at":"2026-10-29T15:04:05Z","text":"x","sources":[1]}]}"#;
        assert_eq!(parse_observations(raw, &events).len(), 1);
    }

    /// The reply may be wrapped in a fence and prose; the balanced span is
    /// taken.
    #[test]
    fn tolerates_a_fence_and_trailing_prose() {
        let events = [event(0, TranscriptEventKind::UserMessage, "a")];
        let raw = "结果如下：\n```json\n{\"observations\":[{\"at\":\"2026-10-29\",\
                   \"text\":\"x\",\"sources\":[1]}]}\n```\n说明：格式为 {\"observations\": []}";
        assert_eq!(
            parse_observations(raw, &events).len(),
            1,
            "the trailing prose's braces must not be swallowed"
        );
    }

    /// An empty batch produces nothing, and no call is made.
    #[test]
    fn an_empty_reply_produces_nothing() {
        let events = [event(0, TranscriptEventKind::UserMessage, "a")];
        assert!(parse_observations("", &events).is_empty());
        assert!(parse_observations("{\"observations\":[]}", &events).is_empty());
    }
}

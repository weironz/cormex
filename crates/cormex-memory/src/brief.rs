//! The handover brief: one session → the seven things the next person needs.
//!
//! Design: docs/design/session-archive-architecture.md §8.4 (D14).
//!
//! # Why this is not "a summary"
//!
//! A summary says what happened. A handover brief says **what to do next**: the
//! goal, the constraints, the decisions already made (with dates), where things
//! stand, the next step, the open questions, and the files and commands that
//! matter. Those are seven different questions, and a model asked for "a
//! summary" answers none of them reliably — it writes prose.
//!
//! So the prompt asks for the seven sections explicitly, and the renderer
//! keeps them as sections rather than flattening them into a paragraph. A
//! person picking the session up reads the section they need.
//!
//! # Every line carries its source
//!
//! Same obligation as the observation log, for the same reason: a brief whose
//! claims cannot be traced is one that cannot be checked, and one that cannot
//! be found again once the session is purged.
//!
//! # Generated lazily, kept forever
//!
//! The brief is generated the first time somebody asks for it, not by a
//! background job: most archived sessions are never looked at again, and
//! generating a brief for every one of them spends money on nothing. Once
//! generated it is a `summaries` row, and when the session gains events a new
//! brief is generated — the old one stays, because "what did we think the state
//! was at the last handover" is half of what a handover record is for.

use chrono::{DateTime, NaiveDate, TimeZone as _, Utc};
use cormex_core::{Id, Result};
use cormex_llm::{LlmClient, Message};
use serde_json::Value;

use crate::distill::{DistillEvent, extract_json_blob, render_events};

/// What `summaries.embedding_model` holds when there is no vector.
///
/// The column is `NOT NULL` and every vector query filters on it, so this value
/// is never matched — which is right for a row that has no vector.
pub const NO_EMBEDDING: &str = "none";

/// The seven sections, in reading order.
///
/// A fixed list rather than free-form headings: the renderer emits them in this
/// order even when the model omits one, and an omitted section says "none"
/// rather than silently disappearing. A reader who cannot tell "there are no
/// open questions" from "the model forgot to mention them" is worse off than
/// one who is told plainly.
pub const SECTIONS: &[(&str, &str)] = &[
    ("goal", "目标"),
    ("constraints", "约束"),
    ("decisions", "已做决定"),
    ("status", "当前状态"),
    ("next", "下一步"),
    ("open", "未决问题"),
    ("files", "关键文件与命令"),
];

/// One brief, as the model produced it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Brief {
    /// `(section key, lines)`, in [`SECTIONS`] order, empty sections included.
    pub sections: Vec<(&'static str, Vec<BriefLine>)>,
    /// Every event cited anywhere in the brief.
    pub sources: Vec<Id>,
}

/// One line of a brief, with the events behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BriefLine {
    pub text: String,
    /// **Never empty.** A line with no source is dropped when parsing.
    pub sources: Vec<Id>,
}

impl Brief {
    /// The rendered body, as it goes inside `render_session_excerpt`.
    ///
    /// Empty sections are written out as "（无）" rather than skipped: the
    /// difference between "there are no open questions" and "the model did not
    /// say" is exactly what a handover reader needs to know, and only the
    /// former is safe to act on.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (key, label) in SECTIONS {
            let lines = self
                .sections
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, l)| l.as_slice())
                .unwrap_or(&[]);
            out.push_str(&format!("## {label}\n"));
            if lines.is_empty() {
                out.push_str("（无）\n");
            } else {
                for line in lines {
                    out.push_str(&format!("- {}\n", line.text));
                }
            }
            out.push('\n');
        }
        out.trim_end().to_owned()
    }
}

/// What one brief generation produced.
#[derive(Debug, Clone, Default)]
pub struct BriefOutcome {
    pub brief: Brief,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub model: String,
}

/// Turns a session's events into a brief.
#[async_trait::async_trait]
pub trait Briefer: Send + Sync {
    /// # Errors
    /// The provider call failed, or its answer could not be read at all.
    async fn brief(&self, session_id: &str, events: &[DistillEvent]) -> Result<BriefOutcome>;
}

/// The production briefer: one cheap-model call.
pub struct CheapModelBriefer {
    llm: LlmClient,
    model: Option<String>,
}

impl CheapModelBriefer {
    #[must_use]
    pub fn new(llm: LlmClient) -> Self {
        Self { llm, model: None }
    }

    /// Point at `CORMEX_DISTILL_MODEL` instead of the configured cheap model.
    ///
    /// # Errors
    /// The provider definition does not know this model.
    pub fn with_model(mut self, model: Option<String>) -> Result<Self> {
        if let Some(name) = &model {
            self.llm.model_config(name)?;
        }
        self.model = Some(model).flatten();
        Ok(self)
    }

    fn model_config(&self) -> Result<cormex_llm::ModelConfig> {
        match &self.model {
            Some(name) => Ok(self.llm.model_config(name)?),
            None => Ok(self.llm.cheap_model().clone()),
        }
    }
}

#[async_trait::async_trait]
impl Briefer for CheapModelBriefer {
    async fn brief(&self, session_id: &str, events: &[DistillEvent]) -> Result<BriefOutcome> {
        if events.is_empty() {
            return Ok(BriefOutcome::default());
        }
        let model = self.model_config()?;
        let messages = [Message::user().with_text(render_events(events))];
        let (reply, usage) = self
            .llm
            .complete(&model, BRIEF_SYSTEM, &messages, &[])
            .await?;
        let raw = reply.as_concat_text();
        tracing::debug!(
            session = %session_id,
            events = events.len(),
            tokens = ?usage.usage.total_tokens,
            "brief generation finished"
        );
        Ok(BriefOutcome {
            brief: parse_brief(&raw, events),
            input_tokens: i64::from(usage.usage.input_tokens.unwrap_or(0)),
            output_tokens: i64::from(usage.usage.output_tokens.unwrap_or(0)),
            model: model.model_name.clone(),
        })
    }
}

/// Parse the model's brief. **Fault-tolerant per line.**
///
/// A line with no readable date (for `decisions`) or no citable source is
/// dropped rather than repaired — a claim nobody can check is worse than a
/// missing one, because it reads as verified.
#[must_use]
pub fn parse_brief(raw: &str, events: &[DistillEvent]) -> Brief {
    let Some(blob) = extract_json_blob(raw) else {
        return Brief::default();
    };
    let Ok(value) = serde_json::from_str::<Value>(blob) else {
        return Brief::default();
    };

    let mut out = Brief::default();
    let mut all_sources: Vec<Id> = Vec::new();

    for (key, _) in SECTIONS {
        let lines = value
            .get(*key)
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| line_from_json(item, events))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for line in &lines {
            for id in &line.sources {
                if !all_sources.contains(id) {
                    all_sources.push(*id);
                }
            }
        }
        out.sections.push((key, lines));
    }
    out.sources = all_sources;
    out
}

fn line_from_json(item: &Value, events: &[DistillEvent]) -> Option<BriefLine> {
    // A line may be a bare string (no sources — dropped) or an object with
    // `text` and `sources`
    let text = item
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())?;
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
        tracing::debug!("a brief line cites no valid event, dropping it");
        return None;
    }
    // A decision without a date is not a decision record: "we chose RustFS" is
    // useless without "when", because the next person needs to know whether it
    // predates the thing they are about to change
    Some(BriefLine {
        text: text.to_owned(),
        sources,
    })
}

/// `YYYY-MM-DD` → midnight UTC. Kept here so a caller that wants to stamp the
/// brief's own date does not re-parse the same formats differently.
#[must_use]
pub fn parse_day(raw: &str) -> Option<DateTime<Utc>> {
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

/// The brief prompt.
///
/// Seven explicit sections rather than "summarize this": a model asked for a
/// summary writes prose, and prose does not answer "what do I do next".
const BRIEF_SYSTEM: &str = r#"你是 Cormex 记忆系统的交接简报生成器。用户会把一段编程会话的事件按顺序给你，每行带序号（#1、#2……）与时间。你的任务是把这段会话整理成一份**交接简报**：接手这个会话的人看完就知道该做什么。

# 七个部分，一个都不能少

1. `goal` —— 目标：这段会话在做什么，一句话
2. `constraints` —— 约束：必须遵守的规则、不能碰的东西、接口要求
3. `decisions` —— 已做决定：已经拍板的方案，**每条带日期**
4. `status` —— 当前状态：现在做到哪一步，什么已经能用、什么还不能
5. `next` —— 下一步：接下来该做什么，按顺序
6. `open` —— 未决问题：还没定的、有分歧的、需要问人的
7. `files` —— 关键文件与命令：接手的人会用到的路径与命令

某一节在会话里确实没有内容时，给空数组 `[]`，**不要编**。

# 每条都要有出处

每一条的 `sources` 是它依据的事件序号（1 起的整数，可多个）。没有出处就不要写这一条 —— 一份查不了证的简报比没有简报更糟，因为它看起来是核实过的。

# 日期

`decisions` 的每一条开头写 `YYYY-MM-DD `，取该决定对应事件的日期。不要写「昨天」「上周」。

# 不写

- 寒暄、客套、确认
- 工具的原始输出本身
- 情绪、猜测、关于个人的推断
- 凭据与密钥的字面值（会话里的疑似密钥已被替换成 `[masked:…]`，照抄占位符即可）

# 输出格式

只输出 JSON，不要任何解释文字：

{
  "goal":        [{"text": "…", "sources": [1]}],
  "constraints": [{"text": "…", "sources": [2]}],
  "decisions":   [{"text": "2026-10-29 决定用 RustFS", "sources": [3]}],
  "status":      [{"text": "…", "sources": [4]}],
  "next":        [{"text": "…", "sources": [5]}],
  "open":        [{"text": "…", "sources": [6]}],
  "files":       [{"text": "…", "sources": [7]}]
}"#;

#[cfg(test)]
mod tests {
    use super::*;
    use cormex_store::TranscriptEventKind;

    fn event(text: &str) -> DistillEvent {
        DistillEvent {
            id: Id::new(),
            seq: 0,
            kind: TranscriptEventKind::UserMessage,
            occurred_at: Some(Utc.with_ymd_and_hms(2026, 10, 29, 12, 0, 0).unwrap()),
            text: text.to_owned(),
        }
    }

    /// Every section is rendered, including the empty ones.
    ///
    /// "There are no open questions" and "the model forgot to say" must not
    /// look the same to the reader.
    #[test]
    fn empty_sections_are_rendered_as_none() {
        let brief = Brief {
            sections: SECTIONS
                .iter()
                .map(|(k, _)| {
                    let lines = if *k == "goal" {
                        vec![BriefLine {
                            text: "ship it".into(),
                            sources: vec![Id::new()],
                        }]
                    } else {
                        Vec::new()
                    };
                    (*k, lines)
                })
                .collect(),
            sources: Vec::new(),
        };
        let rendered = brief.render();
        assert!(rendered.contains("## 目标\n- ship it"), "{rendered}");
        assert!(
            rendered.contains("## 未决问题\n（无）"),
            "an empty section must say so rather than disappear: {rendered}"
        );
    }

    /// A plain brief parses, with sources mapped back to event ids.
    #[test]
    fn parses_a_plain_brief() {
        let events = [event("a"), event("b")];
        let raw = r#"{"goal":[{"text":"ship it","sources":[1]}],
                      "decisions":[{"text":"2026-10-29 用 RustFS","sources":[2]}]}"#;
        let brief = parse_brief(raw, &events);
        let goal = brief.sections.iter().find(|(k, _)| *k == "goal").unwrap();
        assert_eq!(goal.1.len(), 1);
        assert_eq!(goal.1[0].sources, vec![events[0].id]);
        assert_eq!(
            brief.sources.len(),
            2,
            "both cited events should be collected"
        );
    }

    /// A line with no source is dropped.
    #[test]
    fn a_line_without_sources_is_dropped() {
        let events = [event("a")];
        let raw = r#"{"goal":[{"text":"x","sources":[]}]}"#;
        let brief = parse_brief(raw, &events);
        assert!(
            brief.sections.iter().all(|(_, l)| l.is_empty()),
            "an untraceable line must not survive"
        );
    }

    /// An out-of-range citation is dropped, not clamped.
    #[test]
    fn an_out_of_range_source_is_dropped() {
        let events = [event("a")];
        let raw = r#"{"goal":[{"text":"x","sources":[9]}]}"#;
        assert!(
            parse_brief(raw, &events)
                .sections
                .iter()
                .all(|(_, l)| l.is_empty())
        );
    }

    /// A bare string line has no sources and is dropped.
    #[test]
    fn a_bare_string_line_is_dropped() {
        let events = [event("a")];
        let raw = r#"{"goal":["x"]}"#;
        assert!(
            parse_brief(raw, &events)
                .sections
                .iter()
                .all(|(_, l)| l.is_empty())
        );
    }

    /// Every section key is present in the output even when the model omits it.
    #[test]
    fn omitted_sections_are_present_and_empty() {
        let events = [event("a")];
        let brief = parse_brief("{}", &events);
        assert_eq!(
            brief.sections.len(),
            SECTIONS.len(),
            "the renderer relies on every key being present"
        );
    }

    /// A fenced reply with trailing prose still parses.
    #[test]
    fn tolerates_a_fence_and_trailing_prose() {
        let events = [event("a")];
        let raw = "好的：\n```json\n{\"goal\":[{\"text\":\"x\",\"sources\":[1]}]}\n```\n说明：{\"goal\":[]}";
        let brief = parse_brief(raw, &events);
        let goal = brief.sections.iter().find(|(k, _)| *k == "goal").unwrap();
        assert_eq!(goal.1.len(), 1);
    }
}

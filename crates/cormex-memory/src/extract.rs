//! The fact extraction pipeline: one conversational turn → structured facts →
//! persisted.
//!
//! ```text
//! episode text
//!     │
//!     ├─① extract          cheap model emits JSON candidate facts   ← the only network hop
//!     ├─② entity resolution exact name → vector nearest → create new
//!     ├─③ conflict resolution deterministic logic, the model is not consulted again
//!     └─④ persist          one write_txn writes entity + fact + fact_events
//! ```
//!
//! # Three rules that must hold
//!
//! 1. **①②③ all happen outside the transaction.** A network round trip inside a
//!    lock-holding transaction stretches how long `pg_advisory_xact_lock(4272)`
//!    is held into seconds, and every write in the database queues up behind it
//!    — see docs/memory.md §9. This is also exactly why
//!    [`cormex_store::WriteTxn`] offers no read methods at all.
//! 2. **Conflict resolution is not delegated to the LLM.** Asking a model which
//!    of the old and new facts is right is neither stable nor reproducible, and
//!    invalidation is an irreversible append-only write. Only two objective
//!    quantities are used here: whether the objects are identical, and which
//!    event time comes first.
//! 3. **Prefer extracting too little.** Retrieval is top-K, so noise crowds out
//!    the useful entries. There is exactly one criterion: "if the next
//!    conversation didn't know this, would it make a wrong decision?"
//!
//! # Why there is no incremental / streaming extraction
//!
//! What extraction produces is **whole** (a fact either holds or it doesn't).
//! Streaming out JSON would only make fault-tolerant parsing harder while saving
//! essentially no latency — extraction already runs on an async queue and never
//! blocks the conversation.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use cormex_core::{CormexError, Id, Result};
use cormex_llm::{LlmClient, Message};
use cormex_store::{FactSource, NewEntity, NewFact, NewFactEvent, RedactionTarget, Store, Vector};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::embed::SharedEmbedder;
use crate::tokenize;
use crate::trace::{self, TraceRules};

// ══════════════════════════════════════════════════════════
//  Candidate facts
// ══════════════════════════════════════════════════════════

/// What the extraction result calls an entity. Before it is persisted it still
/// has to go through entity resolution to obtain a real id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub name: String,
    /// person / project / tool / org / file / concept … open-ended, not an enum
    pub kind: String,
}

impl EntityRef {
    pub fn new(name: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            kind: kind.into(),
        }
    }

    /// The key used for resolution and deduplication. Differing case or
    /// surrounding whitespace should not produce two separate entities.
    fn cache_key(&self) -> (String, String) {
        (
            self.kind.trim().to_lowercase(),
            self.name.trim().to_lowercase(),
        )
    }
}

/// A fact's object: either a literal, or a pointer to another entity (the latter
/// being one edge in the knowledge graph).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectRef {
    Literal(String),
    Entity(EntityRef),
}

/// A candidate fact that has not been persisted yet.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateFact {
    pub subject: EntityRef,
    pub predicate: String,
    pub object: ObjectRef,
    /// The natural-language phrasing. Both vectorization and the UI use it, so it
    /// has to be readable standing entirely on its own
    pub statement: String,
    pub domain: Option<String>,
    /// [Event time] when this started being true. If it can't be inferred, it's
    /// `None` — do not pass "now" off as the answer, because that makes time
    /// replay treat a backfilled old fact as a new one
    pub valid_at: Option<DateTime<Utc>>,
    pub confidence: f32,
}

/// The context needed to extract from one conversational turn.
#[derive(Debug, Clone)]
pub struct ExtractContext {
    /// Provenance. `facts.source_episode_id NOT NULL` is the foundation that
    /// makes any of this auditable
    pub episode_id: Id,
    /// When this turn happened. Used in two places: as the reference point that
    /// lets the model resolve relative phrasings like "last week", and as the
    /// fallback for conflict resolution when event time is unknown (see
    /// [`Rival::effective_at`])
    pub occurred_at: DateTime<Utc>,
    pub domain: Option<String>,
    /// Known entity names. Nudges the model into reusing existing names,
    /// suppressing the alias explosion at the source
    pub known_entities: Vec<String>,
    /// **Who wrote this turn back.** Determines which trust tier the extracted
    /// facts land in.
    pub origin: TurnOrigin,
}

/// Where a turn came from. **This is the sole entry point for trust tiers.**
///
/// # What a trust tier actually measures
///
/// Not "did this really happen", but **"how easy is this content for an attacker
/// to manipulate"**. The three tiers are three positions on that one ruler:
///
/// | | Who is speaking | Lands in |
/// |---|---|---|
/// | [`Self::Trusted`] | The user themselves, via a full user credential | tier 2 `conversation` |
/// | [`Self::Delegated`] | A caller holding a delegated credential (typically a sandbox container) | tier 3 `tool_output` |
/// | [`Self::External`] | A third-party agent (MCP) | tier 4 `external` |
///
/// # Why the delegated tier gets demoted one level
///
/// Whatever holds a delegated credential is running untrusted code: model
/// output, dependencies the user installed, tool calls that got prompt-injected.
/// The `role: user` text it writes back **is not what the user said** — it is
/// what that process said, and that process may already have been steered by
/// something it read. This is the same argument that demotes tool traces to
/// tier 3.
///
/// # Why external gets demoted one level further
///
/// The delegated side is at least **something exchanged for the user's own
/// bearer token**, doing work the user asked for, with its scope pinned to a
/// single session. For a third-party agent not even that holds: we have no idea
/// who is on the other end, what was fed to it, or whether anyone is watching.
///
/// # Why this slot is specific to us
///
/// A poisoned fact gets recalled and injected into the system prompt across
/// **every future session on every device**, long after the process or the third
/// party that wrote it is gone. The four cloud agents have no such channel
/// (their sandboxes have no writable long-term memory), so nobody is guarding
/// this on our behalf.
///
/// # Why an enum rather than a couple of bools
///
/// This used to be `from_sandbox: bool`. If a second bool had been added for the
/// second origin, "both are true" would become a state that is legal in the type
/// system and undefined in meaning — and it *would* eventually occur, at which
/// point which tier it lands in depends on nothing more than which `if` happens
/// to come first in the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TurnOrigin {
    /// The user themselves, via a full user credential. **The default.**
    #[default]
    Trusted,
    /// Written back by a caller holding a delegated credential. So far the only
    /// instance is the cloud sandbox container, but what this tier judges is the
    /// trust level of the credential, not where the code happens to run.
    Delegated,
    /// Written in by a third-party agent over MCP.
    External,
}

impl TurnOrigin {
    /// Which channel facts extracted from this tier's conversation text land in.
    ///
    /// The tool-trace path **does not consult this** — it is always
    /// `ToolOutput`; the reasoning is in the comment at the `trace_candidates`
    /// call site.
    #[must_use]
    pub const fn conversation_channel(self) -> FactSource {
        match self {
            Self::Trusted => FactSource::Conversation,
            Self::Delegated => FactSource::ToolOutput,
            Self::External => FactSource::External,
        }
    }
}

impl ExtractContext {
    #[must_use]
    pub fn new(episode_id: Id, occurred_at: DateTime<Utc>) -> Self {
        Self {
            episode_id,
            occurred_at,
            domain: None,
            known_entities: Vec::new(),
            origin: TurnOrigin::default(),
        }
    }

    /// Who wrote this turn back. See [`TurnOrigin`].
    #[must_use]
    pub fn from(mut self, origin: TurnOrigin) -> Self {
        self.origin = origin;
        self
    }

    #[must_use]
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    #[must_use]
    pub fn with_known_entities<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.known_entities = names.into_iter().map(Into::into).collect();
        self
    }
}

// ══════════════════════════════════════════════════════════
//  Prompt
// ══════════════════════════════════════════════════════════

/// Upper bound on the text sent to the model for a single turn.
///
/// Sending oversized turns (someone pasted a whole file) in full just burns money
/// linearly, and the tail end is usually source code — precisely the thing we
/// explicitly do not extract. Truncation beats chunking: chunking makes the same
/// fact get extracted twice.
const MAX_EPISODE_CHARS: usize = 24_000;

/// How many candidates a single turn will accept at most. The model occasionally
/// pads out its answer, and holding a hard ceiling is cheaper than cleaning up
/// afterwards.
const MAX_CANDIDATES: usize = 24;

const EXTRACT_SYSTEM: &str = r#"你是 Cormex 记忆系统的事实抽取器。从一轮对话中提炼**值得长期记住**的结构化事实。

# 唯一判据

> 下次对话如果不知道这条，会不会做错决定？

不满足则不抽。**抽多了比抽少了更糟** —— 检索是 top-K，噪声会把真正有用的挤出去。
宁可一条都不抽，也不要凑数。

# 明确不抽

- 寒暄、客套话
- 一次性的事实查询（「现在几点」「这个函数怎么写」）
- 代码片段、日志、命令输出**本身**（原始对话已完整保存；只在做出选择时记「决定采用 X 方案」）
- 你自己的推理过程、解释、总结（**但见下面的「必须抽」第一条**）
- 从常识就能推出的内容
- 尚在讨论中、没有结论的选项（「要不要用 Redis」不是事实；「决定不用 Redis」是）

# 绝对不抽（不是「信噪比低」，是「不能存」）

- **用户的情绪状态**。「他今天很沮丧」「用户对此感到不满」一概不抽。
  情绪是本轮的上下文，不是跨轮的事实；而且在工作场景下推断情绪是被明令禁止的
- **种族、政治观点、宗教信仰、工会成员、健康状况、性生活或性取向的推断**。
  即使用户自己提到，也只在他**明确要求记住**时才抽，否则一律跳过。
  「推断得对不对」不影响这条 —— 做出这类推断本身就是不该做的事
- **关于第三方的私人属性**。「张三负责支付模块」是角色职责，可以抽；
  「张三最近在闹离婚」是私人属性，不抽 —— 用户没有资格替第三方做这个决定
- **凭据与密钥的字面值**。API key、token、密码、连接串里的口令部分。
  可以记「用了哪个服务」，绝不记那串字符

# 必须抽（容易被上面的规则误伤，特别点名）

- **你对用户断言的纠正与反驳**。用户说「X 是这样」而你指出「不对，实际是 Y」——
  这**不属于**「解释与总结」，必须抽成事实，且要让 statement 同时带上
  「用户曾认为 X」与「实际是 Y」两侧。
  漏掉这一条的后果很具体：下次检索只捞回「用户认为 X」，
  而那条错误认知会被当成用户的既定立场反复强化
- 用户明确说「记住这个」的任何内容（除非落在上面的「绝对不抽」里）

# 该抽什么

长期偏好、身份与关系、已作出的技术/业务决策、约束与规则、目标与截止日期、
实体之间稳定的关联。

# 输出格式

只输出 JSON，不要任何解释文字：

{
  "facts": [
    {
      "subject":   {"name": "实体名", "kind": "person|project|tool|org|file|concept|team|task"},
      "predicate": "snake_case 的谓词，如 prefers / decided / uses / owns / blocked_by / deadline",
      "object":    {"text": "字面量"} 或 {"entity": {"name": "实体名", "kind": "..."}},
      "statement": "一句能独立读懂的自然语言陈述",
      "domain":    "coding | office | personal，判断不出填 null",
      "valid_at":  "YYYY-MM-DD，事件在真实世界何时开始为真；推断不出必须填 null",
      "confidence": 0.0~1.0，你对「这条忠实反映了原文」的把握
    }
  ]
}

# 字段要求

- `subject` 用对话里出现的**具体称呼**，不要造词；能复用已知实体名就复用
- `predicate` 只用小写字母与下划线；同一种关系务必用同一个谓词
- `statement` 必须自带主语，不能是「用它」「这个方案」这类离开上下文就读不懂的话
- `valid_at` 只在文本里有明确时间线索时才填。**不要**用今天的日期充数 ——
  推断不出就是 null，系统会自己处理
- 没有任何值得记的事实时，输出 {"facts": []}"#;

fn build_user_prompt(episode_text: &str, ctx: &ExtractContext) -> String {
    let mut s = String::with_capacity(episode_text.len() + 512);

    s.push_str("# 元信息\n\n");
    s.push_str(&format!(
        "- 本轮对话发生于：{}\n",
        ctx.occurred_at.format("%Y-%m-%d %H:%M UTC")
    ));
    if let Some(domain) = &ctx.domain {
        s.push_str(&format!("- 当前领域：{domain}\n"));
    }
    if !ctx.known_entities.is_empty() {
        s.push_str(&format!(
            "- 已知实体（能对上就复用这些名字）：{}\n",
            ctx.known_entities.join("、")
        ));
    }

    s.push_str("\n# 对话内容\n\n");
    if episode_text.chars().count() > MAX_EPISODE_CHARS {
        let cut: String = episode_text.chars().take(MAX_EPISODE_CHARS).collect();
        s.push_str(&cut);
        s.push_str("\n\n（超长，后半截已截断）");
    } else {
        s.push_str(episode_text);
    }

    s.push_str("\n\n只输出 JSON。");
    s
}

// ══════════════════════════════════════════════════════════
//  Fault-tolerant parsing
// ══════════════════════════════════════════════════════════

/// Peel the JSON body out of whatever text the model returned.
///
/// All three flavours of dirty output we've actually seen have to be digestible:
/// ```json fences, prose wrapped around the JSON, and a top level that is an
/// array rather than an object. The approach is to scan for the **first balanced**
/// `{…}` or `[…]` rather than naively taking the first and last character — the
/// naive version falls apart the moment the surrounding prose contains a closing
/// brace.
///
/// `pub(crate)`: [`crate::crosslink`]'s judgements come back as the same kind of
/// dirty JSON from the same model, and keeping a second copy just means fixing a
/// bug in only one of them.
pub(crate) fn extract_json_blob(raw: &str) -> Option<&str> {
    let bytes = raw.as_bytes();
    let start = bytes.iter().position(|b| *b == b'{' || *b == b'[')?;
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (i, b) in bytes.iter().enumerate().skip(start) {
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
            b if b == open => depth += 1,
            b if b == close => {
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

/// Parse candidate facts. **Fault-tolerant per entry**: a missing field on one
/// entry drops only that entry, it does not void the whole batch.
///
/// The model occasionally omits `statement`, or writes `object` as a bare string.
/// Throwing away the good entries in the same batch over a blemish like that is
/// trading accuracy for tidiness.
pub fn parse_candidates(raw: &str) -> Result<Vec<CandidateFact>> {
    let blob = extract_json_blob(raw)
        .ok_or_else(|| CormexError::Memory("no JSON found in the extraction result".into()))?;

    let value: Value = serde_json::from_str(blob).map_err(|e| {
        CormexError::Memory(format!("the extraction result is not valid JSON: {e}"))
    })?;

    // The top level may be {"facts":[…]}, {"data":[…]}, or just […] directly
    let items = match &value {
        Value::Array(items) => items.as_slice(),
        Value::Object(map) => map
            .get("facts")
            .or_else(|| map.get("data"))
            .or_else(|| map.get("results"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };

    let mut out = Vec::new();
    for item in items.iter().take(MAX_CANDIDATES) {
        match candidate_from_json(item) {
            Some(c) => out.push(c),
            None => tracing::debug!(?item, "candidate fact is missing fields, skipping"),
        }
    }
    Ok(out)
}

fn candidate_from_json(v: &Value) -> Option<CandidateFact> {
    let subject = entity_ref_from_json(v.get("subject")?, "concept")?;

    let predicate = normalize_predicate(v.get("predicate").and_then(Value::as_str)?);
    if predicate.is_empty() {
        return None;
    }

    let object = object_ref_from_json(v.get("object")?)?;

    // A missing statement should not cost us the whole entry — subject,
    // predicate and object are all there, so just assemble a sentence
    let statement = v
        .get("statement")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(
            || {
                let obj = match &object {
                    ObjectRef::Literal(t) => t.clone(),
                    ObjectRef::Entity(e) => e.name.clone(),
                };
                format!("{} {} {}", subject.name, predicate, obj)
            },
            str::to_owned,
        );

    let domain = v
        .get("domain")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null"))
        .map(str::to_owned);

    let valid_at = v
        .get("valid_at")
        .and_then(Value::as_str)
        .and_then(parse_event_time);

    // The schema's CHECK requires 0..=1; when the model hands back a percentage
    // like 95, don't throw the whole entry away over it
    let confidence = v
        .get("confidence")
        .and_then(Value::as_f64)
        .map_or(0.8_f32, |c| {
            let c = if c > 1.0 { c / 100.0 } else { c };
            (c as f32).clamp(0.0, 1.0)
        });

    Some(CandidateFact {
        subject,
        predicate,
        object,
        statement,
        domain,
        valid_at,
        confidence,
    })
}

fn entity_ref_from_json(v: &Value, default_kind: &str) -> Option<EntityRef> {
    match v {
        // The model sometimes writes subject as a bare string
        Value::String(s) => {
            let name = s.trim();
            (!name.is_empty()).then(|| EntityRef::new(name, default_kind))
        }
        Value::Object(map) => {
            let name = map
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())?;
            let kind = map
                .get("kind")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(default_kind);
            Some(EntityRef::new(name, kind.to_lowercase()))
        }
        _ => None,
    }
}

fn object_ref_from_json(v: &Value) -> Option<ObjectRef> {
    match v {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| ObjectRef::Literal(t.to_owned()))
        }
        Value::Number(n) => Some(ObjectRef::Literal(n.to_string())),
        Value::Bool(b) => Some(ObjectRef::Literal(b.to_string())),
        Value::Object(map) => {
            if let Some(e) = map.get("entity") {
                return entity_ref_from_json(e, "concept").map(ObjectRef::Entity);
            }
            if let Some(t) = map
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                return Some(ObjectRef::Literal(t.to_owned()));
            }
            // {"name":…,"kind":…} placed directly under object is accepted too
            entity_ref_from_json(v, "concept").map(ObjectRef::Entity)
        }
        _ => None,
    }
}

/// Predicate normalization: lowercase, with runs of non-alphanumerics folded into
/// a single underscore.
///
/// The same relation showing up as `decided on` / `Decided-On` / `decided_on` is
/// the norm, and without normalization conflict detection keyed on
/// `(subject_id, predicate)` would never collide at all.
#[must_use]
pub fn normalize_predicate(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for c in raw.trim().chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Parse an event time. The granularity the model hands back ranges from years to
/// seconds, so accept as much of that range as possible.
fn parse_event_time(raw: &str) -> Option<DateTime<Utc>> {
    let s = raw.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("null") || s.eq_ignore_ascii_case("unknown") {
        return None;
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    for (fmt, padded) in [
        ("%Y-%m-%d", s.to_owned()),
        ("%Y-%m-%d", format!("{s}-01")),    // YYYY-MM
        ("%Y-%m-%d", format!("{s}-01-01")), // YYYY
    ] {
        if let Ok(d) = NaiveDate::parse_from_str(&padded, fmt) {
            return d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
        }
    }
    None
}

// ══════════════════════════════════════════════════════════
//  Conflict resolution — deterministic logic
// ══════════════════════════════════════════════════════════

/// Cardinality rules for predicates: which predicates can hold only one value per
/// subject.
///
/// This is the only place in conflict resolution that carries "knowledge", which
/// is exactly why it **must be a lookup table rather than a model judgement**:
/// invalidation is an irreversible append-only write, and there is no way back
/// from getting it wrong (all you can do is append a revoke on top).
///
/// Any predicate not in the table is treated as multi-valued — that is, "add it,
/// both entries coexist". That default direction is deliberate: missing a
/// conflict merely leaves one extra stale fact around, whereas calling one
/// wrongly marks a fact that is still true as invalidated.
#[derive(Debug, Clone)]
pub struct PredicateRules {
    functional: BTreeSet<String>,
}

/// Predicates that are inherently single-valued. The names are taken from the
/// examples in the schema comments and from what shows up most often in real
/// extraction.
const DEFAULT_FUNCTIONAL: &[&str] = &[
    "decided",
    "decided_on",
    "decided_to_use",
    "chose",
    "adopted",
    "uses",
    "runs_on",
    "deployed_to",
    "stores_in",
    "status",
    "state",
    "stage",
    "priority",
    "version",
    "deadline",
    "due_at",
    "assigned_to",
    "owned_by",
    "reports_to",
    "works_at",
    "lives_in",
    "located_in",
    "role",
    "title",
    "email",
    "phone",
    "birthday",
    "timezone",
    "primary_language",
];

impl Default for PredicateRules {
    fn default() -> Self {
        Self {
            functional: DEFAULT_FUNCTIONAL.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

impl PredicateRules {
    /// Add more single-valued predicates. Domain-specific predicates are filled
    /// in by the caller rather than hard-coded here.
    #[must_use]
    pub fn with_functional<I, S>(mut self, predicates: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        for p in predicates {
            self.functional.insert(normalize_predicate(p.as_ref()));
        }
        self
    }

    #[must_use]
    pub fn is_functional(&self, predicate: &str) -> bool {
        self.functional.contains(&normalize_predicate(predicate))
    }
}

/// One existing fact (or one already planned within this batch) that takes part
/// in the conflict decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rival {
    pub fact_id: Id,
    /// The normalized object key, used for duplicate detection
    pub object_key: String,
    /// The event time used for the decision.
    ///
    /// `facts.valid_at` is allowed to be NULL (unknown, or true all along), but a
    /// comparison needs *some* value, so it falls back to that row's system
    /// time. **This fallback value is never written back into `valid_at`** —
    /// doing so would pass "when I learned it" off as "when it became true", and
    /// the whole bitemporal design would have been for nothing.
    pub effective_at: DateTime<Utc>,
}

/// The verdict of conflict resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A valid fact with the same subject/predicate/object is already in the
    /// database — discard, write not a single word.
    ///
    /// This is the single largest source of noise: the same thing being brought
    /// up over and over across consecutive turns.
    Duplicate,
    /// Write the new fact, and append events to the old ones as needed.
    /// `supersede` and `flag` both empty means "addition": both entries coexist.
    Write {
        /// Append `invalidate/superseded`, with `superseded_by` pointing at the
        /// new fact
        supersede: Vec<Id>,
        /// Append `flag` (an annotation; it does not change status) — it has to
        /// be attached to both the old and the new side
        flag: Vec<Id>,
        /// The `invalid_at` on the `superseded` event
        invalid_at: DateTime<Utc>,
    },
}

/// Decide how a new fact relates to the existing facts under the same
/// `(subject_id, predicate)`.
///
/// Only two objective quantities are consulted; no model is involved at all:
///
/// | Situation | Verdict |
/// |---|---|
/// | Identical object | Duplicate, discard |
/// | Predicate is multi-valued | Addition, coexist |
/// | Single-valued + new fact's event time is later | Supersede |
/// | Single-valued + new fact's event time is earlier | Coexist (backfilled history; non-overlapping intervals are not a conflict) |
/// | Single-valued + event times are exactly equal | A conflict we cannot adjudicate: both coexist, plus a flag |
#[must_use]
pub fn decide(
    new_object_key: &str,
    new_effective_at: DateTime<Utc>,
    functional: bool,
    rivals: &[Rival],
) -> Verdict {
    if rivals.iter().any(|r| r.object_key == new_object_key) {
        return Verdict::Duplicate;
    }

    let mut supersede = Vec::new();
    let mut flag = Vec::new();

    if functional {
        for r in rivals {
            match new_effective_at.cmp(&r.effective_at) {
                std::cmp::Ordering::Greater => supersede.push(r.fact_id),
                // The new fact describes an earlier state: this is a backfill,
                // and the old fact is still "the present". Non-overlapping time
                // intervals do not constitute a conflict (docs/memory.md §5).
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => flag.push(r.fact_id),
            }
        }
    }

    Verdict::Write {
        supersede,
        flag,
        invalid_at: new_effective_at,
    }
}

/// The normalized object key. Entity objects compare by id; literals compare
/// lowercased with whitespace squeezed out.
fn object_key(literal: Option<&str>, entity_id: Option<&str>) -> String {
    if let Some(id) = entity_id {
        return format!("e:{id}");
    }
    let text = literal.unwrap_or_default();
    let squeezed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("t:{}", squeezed.to_lowercase())
}

// ══════════════════════════════════════════════════════════
//  Entity nearest-neighbour index
// ══════════════════════════════════════════════════════════

/// The minimal profile of a known entity, for approximate matching.
#[derive(Debug, Clone)]
pub struct EntityRecord {
    pub id: Id,
    pub kind: String,
    pub name: String,
    pub embedding: Vec<f32>,
}

/// Nearest-neighbour lookup over entity vectors.
///
/// This is a trait rather than a direct database query because `cormex-store`
/// currently only exposes exact `(kind, name)` matching — it has no
/// nearest-neighbour query over entity vectors. The default implementation,
/// [`InProcessEntityIndex`], does it in-process (cormexd is long-running, which
/// is enough to cover alias merging within one process); once the storage layer
/// grows an `entities_near`, swapping in another implementation is all it takes
/// and no call site changes by a line.
#[async_trait::async_trait]
pub trait EntityIndex: Send + Sync {
    /// Remember an entity (newly created, or just resolved) for later
    /// approximate matching.
    async fn remember(&self, record: EntityRecord);

    /// The entities of the same kind closest to the given vector, in descending
    /// order of cosine similarity.
    async fn nearest(
        &self,
        kind: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Vec<(EntityRecord, f32)>;
}

/// The in-process entity index.
#[derive(Default)]
pub struct InProcessEntityIndex {
    records: RwLock<Vec<EntityRecord>>,
}

impl InProcessEntityIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl EntityIndex for InProcessEntityIndex {
    async fn remember(&self, record: EntityRecord) {
        let mut guard = self.records.write().await;
        if guard.iter().any(|r| r.id == record.id) {
            return;
        }
        guard.push(record);
    }

    async fn nearest(
        &self,
        kind: &str,
        embedding: &[f32],
        limit: usize,
    ) -> Vec<(EntityRecord, f32)> {
        let guard = self.records.read().await;
        let mut hits: Vec<(EntityRecord, f32)> = guard
            .iter()
            .filter(|r| r.kind.eq_ignore_ascii_case(kind))
            .map(|r| (r.clone(), cosine(&r.embedding, embedding)))
            .collect();
        hits.sort_by(|a, b| b.1.total_cmp(&a.1));
        hits.truncate(limit);
        hits
    }
}

/// Cosine similarity. Vectors are already normalized by [`crate::embed`], but the
/// magnitudes are still computed here in case something outside hands us
/// un-normalized vectors.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

// ══════════════════════════════════════════════════════════
//  The extractor
// ══════════════════════════════════════════════════════════

/// The cosine threshold for approximate entity matching.
///
/// The value leans conservative: wrongly merging two distinct entities into one
/// is far harder to track down than leaving one extra alias around
/// (`entity_merges` is first-writer-wins and currently irreversible). It can be
/// raised when switching to a semantically stronger embedding model, via
/// [`Extractor::with_entity_match_threshold`].
pub const DEFAULT_ENTITY_MATCH_THRESHOLD: f32 = 0.82;

/// The outcome of persisting one turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestReport {
    /// How many candidates the model produced
    pub candidates: usize,
    /// The fact ids actually written
    pub written: Vec<Id>,
    /// How many were discarded for sharing subject/predicate/object with a fact
    /// already in the database
    pub duplicates: usize,
    /// The ids of old facts that were superseded (a superseded event was
    /// appended to them)
    pub superseded: Vec<Id>,
    /// The fact ids marked "conflict, awaiting confirmation" (both the old and
    /// the new side are included)
    pub flagged: Vec<Id>,
    /// The ids of newly created entities
    pub new_entities: Vec<Id>,
}

/// The fact extractor. Cheap to clone (everything inside is an `Arc`), so it can
/// be dropped straight into an async task queue.
#[derive(Clone)]
pub struct Extractor {
    llm: LlmClient,
    embedder: SharedEmbedder,
    index: Arc<dyn EntityIndex>,
    rules: PredicateRules,
    trace_rules: TraceRules,
    device_id: String,
    entity_match_threshold: f32,
}

impl Extractor {
    pub fn new(llm: LlmClient, embedder: SharedEmbedder, device_id: impl Into<String>) -> Self {
        Self {
            llm,
            embedder,
            index: Arc::new(InProcessEntityIndex::new()),
            rules: PredicateRules::default(),
            trace_rules: TraceRules::default(),
            device_id: device_id.into(),
            entity_match_threshold: DEFAULT_ENTITY_MATCH_THRESHOLD,
        }
    }

    #[must_use]
    pub fn with_rules(mut self, rules: PredicateRules) -> Self {
        self.rules = rules;
        self
    }

    /// Adjust the criteria used for tool traces (the habit threshold, the
    /// per-turn budget, and so on).
    #[must_use]
    pub fn with_trace_rules(mut self, rules: TraceRules) -> Self {
        self.trace_rules = rules;
        self
    }

    #[must_use]
    pub fn with_entity_index(mut self, index: Arc<dyn EntityIndex>) -> Self {
        self.index = index;
        self
    }

    #[must_use]
    pub fn with_entity_match_threshold(mut self, threshold: f32) -> Self {
        self.entity_match_threshold = threshold;
        self
    }

    /// What gets written to `facts.extracted_by`: provider + model.
    /// Recording it down to the exact model is what will later let us trace
    /// quality problems back to a specific extractor version.
    fn extracted_by(&self) -> String {
        format!(
            "{}/{}",
            self.llm.provider_id(),
            self.llm.cheap_model().model_name
        )
    }

    // ── ① Extract ──────────────────────────────────────────

    /// Extract candidate facts from one episode. **Uses the cheap model** —
    /// anything that runs on every single turn must never go through the main
    /// conversation model (docs/memory.md §10).
    pub async fn extract(
        &self,
        episode_text: &str,
        ctx: &ExtractContext,
    ) -> Result<Vec<CandidateFact>> {
        if episode_text.trim().is_empty() {
            return Ok(Vec::new());
        }

        // ── Write-side negative filter. **It must be on this line, not any
        // later** ──
        //
        // What follows immediately below is an LLM call: move this filter into
        // `write_candidates`, or delegate it to the "don't extract credentials"
        // sentence in the prompt, and the raw text has already crossed the
        // network in full either way. This is precisely the lesson Mem0 teaches
        // by counterexample (memory-content.md §7, principle 2).
        //
        // What gets blocked is only the **derived layer**: the episode was
        // persisted long ago, and what is given up here is this turn's fact
        // extraction, its embedding, and its entry into the retrieval index. L0
        // is not allowed to have holes — and the conversation body is exactly
        // where secrets most often appear.
        //
        // The log records only the rule name. Recording the context would move
        // the secret out of the conversation body and into the container logs
        if let Some(rule) = crate::redaction::scan(episode_text) {
            tracing::warn!(
                episode = %ctx.episode_id,
                rule = rule.name(),
                "this turn contains credentials; skipping extraction (the raw text stays in episodes as usual)"
            );
            return Ok(Vec::new());
        }

        let messages = [Message::user().with_text(build_user_prompt(episode_text, ctx))];
        let (reply, usage) = self
            .llm
            .complete(self.llm.cheap_model(), EXTRACT_SYSTEM, &messages, &[])
            .await?;

        let raw = reply.as_concat_text();
        tracing::debug!(
            episode = %ctx.episode_id,
            tokens = ?usage.usage.total_tokens,
            "extraction finished"
        );

        parse_candidates(&raw)
    }

    // ── ②③④ Resolution and persistence ────────────────────

    /// The full pipeline: extract → entity resolution → conflict resolution →
    /// persist.
    pub async fn ingest(
        &self,
        store: &Store,
        episode_text: &str,
        ctx: &ExtractContext,
    ) -> Result<IngestReport> {
        let candidates = self.extract(episode_text, ctx).await?;
        self.write_candidates(store, candidates, ctx).await
    }

    /// Resolve the candidate facts and persist them, **including the candidates
    /// derived from this turn's tool traces**.
    ///
    /// This is separate from [`Self::extract`] so that the deterministic half can
    /// be unit-tested without an LLM, and so that "re-run the persistence step"
    /// doesn't cost another model call.
    ///
    /// # Why the trace path hangs off this method rather than [`Self::ingest`]
    ///
    /// 1. **It needs no model.** Hanging it off `ingest` would amount to
    ///    declaring "you must first spend an LLM call before tool traces get
    ///    looked at", when trace extraction is pure deterministic computation.
    /// 2. **Hanging it off `ingest` would make it untestable.** The eval suite's
    ///    `seed` mode goes only through `write_candidates` (see the "two modes"
    ///    section of `evals/README.md`), so with trace extraction sitting inside
    ///    `ingest`, that cell of the regression gate would read zero forever.
    ///
    /// The trace path performs only two **read** queries plus a stretch of pure
    /// computation, all of it outside `write_txn`, so it does not violate
    /// constraint 2.
    pub async fn write_candidates(
        &self,
        store: &Store,
        candidates: Vec<CandidateFact>,
        ctx: &ExtractContext,
    ) -> Result<IngestReport> {
        // In-flight task guard: when a redact runs, extraction tasks may still be
        // in the air, and without this check they would write the erased content
        // straight back in (docs/memory.md §11). It goes first because the
        // trace-derived candidates come from the same episode and must be
        // blocked by it too.
        let source = ctx.episode_id.to_string();
        if store.is_redacted(RedactionTarget::Episode, &source).await? {
            tracing::warn!(
                episode = %source,
                "the source has been redacted; dropping this turn's extraction output"
            );
            return Ok(IngestReport {
                candidates: candidates.len(),
                ..IngestReport::default()
            });
        }

        // The source channel travels alongside each candidate rather than being
        // guessed after the fact from the predicate: `CandidateFact` has **no**
        // such field, and adding one would break callers' struct-literal
        // construction (the eval suite's dataset conversion is written exactly
        // that way). Pairing them internally is the only approach that doesn't
        // leak outward. Conversation written back by the sandbox is **demoted one
        // level** to tier 3, on par with tool traces.
        //
        // That turn's `role: user` is not what the user said, it is what the
        // container said — and the container runs untrusted code that may already
        // have been steered by something it read itself. The argument for the
        // demotion is word-for-word the same as the trace passage below: what a
        // trust tier measures is "how easy is this content for an attacker to
        // manipulate".
        //
        // Note this is "demote", not "don't write": the work done inside the
        // sandbox genuinely is worth remembering (it is what the user asked for),
        // it just should not stand on equal footing with what the user said in
        // their own words.
        let conversation_tier = ctx.origin.conversation_channel();
        let mut candidates: Vec<(CandidateFact, FactSource)> = candidates
            .into_iter()
            .map(|c| (c, conversation_tier))
            .collect();
        candidates.extend(
            self.trace_candidates(store, ctx)
                .await?
                .into_iter()
                // tier 3. Being below Conversation (tier 2) is correct: what a
                // trust tier measures is not "did this happen" — an exit code is
                // of course objective — but **how easy this content is for an
                // attacker to manipulate**. What decides which command gets run
                // is the model, and the model may already have been steered by
                // the very content awaiting extraction (MINJA and AgentPoison are
                // both write-side attacks); a tool trace is a closed loop of "the
                // model produced it and then read it back itself", exactly the
                // self-reinforcement entry point §4.4 calls out. The objective
                // half we have already honoured separately: candidates are
                // **located by hard signals alone** (the true/false sequence of
                // `ok`, and occurrence counts), and that part never passes
                // through trust tiers at all.
                .map(|c| (c, FactSource::ToolOutput)),
        );

        let mut report = IngestReport {
            candidates: candidates.len(),
            ..IngestReport::default()
        };
        if candidates.is_empty() {
            return Ok(report);
        }

        // ── ② Entity resolution: every read happens outside write_txn ──
        let mut resolver = EntityResolution::default();
        for (c, _) in &candidates {
            self.resolve_entity(store, &c.subject, &mut resolver)
                .await?;
            if let ObjectRef::Entity(e) = &c.object {
                self.resolve_entity(store, e, &mut resolver).await?;
            }
        }

        // ── Vectorization: likewise, it must be finished outside the
        // transaction ──
        let statements: Vec<String> = candidates
            .iter()
            .map(|(c, _)| c.statement.clone())
            .collect();
        let vectors = self.embedder.embed(&statements).await?;
        if vectors.len() != candidates.len() {
            return Err(CormexError::Memory(format!(
                "the embedder returned {} vectors, which does not match {} facts",
                vectors.len(),
                candidates.len()
            )));
        }

        // ── ③ Conflict resolution ──
        let extracted_by = self.extracted_by();
        let mut plans: Vec<Plan> = Vec::new();
        // Facts already planned within this batch have to take part in the
        // decision too, otherwise two mutually exclusive facts from the same turn
        // would both end up in force
        let mut pending: HashMap<(Id, String), Vec<Rival>> = HashMap::new();

        for ((candidate, source), vector) in candidates.into_iter().zip(vectors) {
            let subject_id = resolver.require(&candidate.subject)?;
            let object_entity_id = match &candidate.object {
                ObjectRef::Entity(e) => Some(resolver.require(e)?),
                ObjectRef::Literal(_) => None,
            };
            let object_text = match &candidate.object {
                ObjectRef::Literal(t) => Some(t.clone()),
                ObjectRef::Entity(_) => None,
            };

            let key = object_key(
                object_text.as_deref(),
                object_entity_id.map(|id| id.to_string()).as_deref(),
            );
            // When event time is unknown, fall back to when this turn happened:
            // that is the latest lower bound for when this fact became true, and
            // the only timestamp available without another database round trip.
            let effective_at = candidate.valid_at.unwrap_or(ctx.occurred_at);

            let slot = (subject_id, candidate.predicate.clone());
            let mut rivals = self.rivals(store, subject_id, &candidate.predicate).await?;
            rivals.extend(pending.get(&slot).cloned().unwrap_or_default());

            let functional = self.rules.is_functional(&candidate.predicate);
            let verdict = decide(&key, effective_at, functional, &rivals);

            let (supersede, flag, invalid_at) = match verdict {
                Verdict::Duplicate => {
                    report.duplicates += 1;
                    continue;
                }
                Verdict::Write {
                    supersede,
                    flag,
                    invalid_at,
                } => (supersede, flag, invalid_at),
            };

            let fact_id = Id::new();
            // Anything superseded by a new fact within this batch should no
            // longer act as a rival for the candidates that follow
            let survivors: Vec<Rival> = pending
                .remove(&slot)
                .unwrap_or_default()
                .into_iter()
                .filter(|r| !supersede.contains(&r.fact_id))
                .chain(std::iter::once(Rival {
                    fact_id,
                    object_key: key,
                    effective_at,
                }))
                .collect();
            pending.insert(slot, survivors);

            plans.push(Plan {
                fact: NewFact {
                    id: fact_id,
                    subject_id,
                    predicate: candidate.predicate,
                    object_text,
                    object_entity_id,
                    tsv_source: Some(tokenize::to_tsvector_input(&candidate.statement)),
                    statement: candidate.statement,
                    embedding: Some(Vector::from(vector)),
                    embedding_model: self.embedder.model_id().to_owned(),
                    domain: candidate.domain.or_else(|| ctx.domain.clone()),
                    confidence: candidate.confidence,
                    valid_at: candidate.valid_at,
                    source_episode_id: ctx.episode_id,
                    // The conversation path is uniformly Conversation (tier 2);
                    // it **does not single out "the user said this themselves"**.
                    //
                    // That isn't conservatism, it's that this pipeline genuinely
                    // cannot tell right now: what gets sent to the model is one
                    // block assembled as "user: … / assistant: …" (see
                    // cormexd::live), and the candidates come back with no
                    // indication of which side any sentence came from. Marking
                    // some subset of them tier 1 would be promoting them out of
                    // thin air — and the whole point of tier 1 is "the most
                    // trustworthy entries must not be overwritten by external
                    // content", so an error in that direction does damage.
                    //
                    // Unlocking tier 1 means changing the extractor itself: add a
                    // "from the user / from the assistant" field to the candidate
                    // schema and have the model annotate each entry. That follows
                    // the §8.3 principle that the extractor must explicitly record
                    // which input fragment each entry corresponds to, rather than
                    // reconstructing it after the fact — which is precisely where
                    // the existing rows came to grief.
                    //
                    // The tool-trace path is ToolOutput (tier 3); the reasoning is
                    // at the matching spot above.
                    source,
                    // Single-source: the provenance is exactly the one episode in
                    // `source_episode_id`
                    derived_from: Vec::new(),
                    extracted_by: extracted_by.clone(),
                    device_id: self.device_id.clone(),
                },
                supersede,
                flag,
                invalid_at,
            });
        }

        if plans.is_empty() && resolver.created.is_empty() {
            return Ok(report);
        }

        // ── ④ Persist: one transaction writes entity + fact + fact_events ──
        let new_entities = resolver.created.clone();
        let device_id = self.device_id.clone();
        store
            .write_txn(async |tx| {
                for e in &new_entities {
                    tx.insert_entity(e).await?;
                }
                for plan in &plans {
                    tx.insert_fact(&plan.fact).await?;
                    for old in &plan.supersede {
                        let event = NewFactEvent::superseded(
                            *old,
                            plan.fact.id,
                            plan.invalid_at,
                            device_id.clone(),
                        );
                        tx.insert_fact_event(&event).await?;
                    }
                    if !plan.flag.is_empty() {
                        // A flag is an annotation and does not change status, so
                        // it has to be attached to both the old and the new side:
                        // the UI's "awaiting confirmation" list needs to show both
                        // of the mutually contradictory entries at once
                        let reason = format!(
                            "conflicts with {} fact(s) on the same subject and predicate, same event time",
                            plan.flag.len()
                        );
                        for old in &plan.flag {
                            let event = NewFactEvent::flag(*old, reason.clone(), device_id.clone());
                            tx.insert_fact_event(&event).await?;
                        }
                        let event = NewFactEvent::flag(plan.fact.id, reason, device_id.clone());
                        tx.insert_fact_event(&event).await?;
                    }
                }
                Ok(())
            })
            .await?;

        for plan in plans {
            report.written.push(plan.fact.id);
            report.superseded.extend(plan.supersede);
            if !plan.flag.is_empty() {
                report.flagged.extend(plan.flag);
                report.flagged.push(plan.fact.id);
            }
        }
        report
            .new_entities
            .extend(resolver.created.iter().map(|e| e.id));
        Ok(report)
    }

    /// Produce candidate facts from this turn's tool traces.
    ///
    /// Two read queries and a stretch of pure computation, with no model
    /// involved — the criteria live in [`crate::trace`] and are derived entirely
    /// from the true/false sequence of `ok` and from occurrence counts.
    ///
    /// When the turn contains no tool calls at all it returns after **a single
    /// query**: the overwhelming majority of conversational turns are pure chat,
    /// and making them carry a full-table scan for nothing is not worth it.
    async fn trace_candidates(
        &self,
        store: &Store,
        ctx: &ExtractContext,
    ) -> Result<Vec<CandidateFact>> {
        let calls = store
            .episode_tool_calls(&ctx.episode_id.to_string())
            .await?;
        if calls.is_empty() {
            return Ok(Vec::new());
        }

        // The "habit" criterion cannot be computed within a single turn; it has
        // to see past this turn's horizon. The scope is **the entire memory
        // store** rather than the current project — `episode_tool_calls` has no
        // workspace/project column, so there is nothing to narrow by. Narrow it
        // once that column exists.
        let history = store
            .recent_tool_calls(self.trace_rules.history_limit)
            .await?;

        let out = trace::candidates_from_trace(
            &calls,
            &history,
            ctx.domain.as_deref(),
            ctx.occurred_at,
            &self.trace_rules,
        );
        if !out.is_empty() {
            tracing::debug!(
                episode = %ctx.episode_id,
                calls = calls.len(),
                candidates = out.len(),
                "tool traces produced candidate facts"
            );
        }
        Ok(out)
    }

    /// Look up the existing valid facts under the same
    /// `(subject_id, predicate)` and turn them into rivals for the decision.
    async fn rivals(&self, store: &Store, subject_id: Id, predicate: &str) -> Result<Vec<Rival>> {
        let existing = store
            .active_facts_by_subject_predicate(&subject_id.to_string(), predicate)
            .await?;

        let mut out = Vec::with_capacity(existing.len());
        for f in existing {
            // Ids in the database live in the ulid domain (text). Failing to
            // parse means someone wrote to the database directly, bypassing Id;
            // in that case it is better to skip the row than to fail the whole
            // turn's extraction.
            let Ok(fact_id) = f.id.parse::<Id>() else {
                tracing::warn!(
                    fact = %f.id,
                    "fact id is not a valid ULID; the contradiction check skips it"
                );
                continue;
            };
            out.push(Rival {
                fact_id,
                object_key: object_key(f.object_text.as_deref(), f.object_entity_id.as_deref()),
                effective_at: f.valid_at.unwrap_or(f.created_at),
            });
        }
        Ok(out)
    }

    /// Entity resolution: exact name → vector nearest → create new.
    ///
    /// It is all reads, and it must finish outside `write_txn` —
    /// [`cormex_store::WriteTxn`] simply offers no read methods at all, precisely
    /// so this rule cannot be worked around.
    async fn resolve_entity(
        &self,
        store: &Store,
        r: &EntityRef,
        resolver: &mut EntityResolution,
    ) -> Result<Id> {
        let cache_key = r.cache_key();
        if let Some(id) = resolver.resolved.get(&cache_key) {
            return Ok(*id);
        }

        let kind = r.kind.trim().to_lowercase();
        let name = r.name.trim();

        // ① Exact match. Differing case should not create two entities, so try
        // the lowercased form as well.
        let mut hits = store.entities_named(&kind, name).await?;
        if hits.is_empty() && name != name.to_lowercase() {
            hits = store.entities_named(&kind, &name.to_lowercase()).await?;
        }
        if let Some(hit) = hits.first() {
            // Walk the alias-merge chain to its end — whatever we hit may
            // already have been merged into something else
            let canonical = store
                .canonical_entity(&hit.id)
                .await?
                .unwrap_or_else(|| hit.id.clone());
            let id = canonical.parse::<Id>().map_err(|e| {
                CormexError::Store(format!("entity id {canonical} is not a valid ULID: {e}"))
            })?;
            resolver.resolved.insert(cache_key, id);
            return Ok(id);
        }

        // ② Vector approximation. The name is vectorized on its own (kind is not
        // concatenated in): what differs between aliases is the name itself, and
        // mixing kind in would lift the similarity of every entity of that kind
        // together, leaving the threshold with no discriminating power.
        let embedding = self.embedder.embed_one(&name.to_lowercase()).await?;
        if let Some((record, score)) = self.index.nearest(&kind, &embedding, 1).await.pop()
            && score >= self.entity_match_threshold
        {
            tracing::debug!(
                name = %name, matched = %record.name, score,
                "entity merged by vector approximation"
            );
            resolver.resolved.insert(cache_key, record.id);
            return Ok(record.id);
        }

        // ③ Create new
        let id = Id::new();
        resolver.created.push(NewEntity {
            id,
            kind: kind.clone(),
            name: name.to_owned(),
            summary: None,
            embedding: Some(Vector::from(embedding.clone())),
            embedding_model: self.embedder.model_id().to_owned(),
            device_id: self.device_id.clone(),
        });
        resolver.resolved.insert(cache_key, id);
        self.index
            .remember(EntityRecord {
                id,
                kind,
                name: name.to_owned(),
                embedding,
            })
            .await;
        Ok(id)
    }
}

impl std::fmt::Debug for Extractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extractor")
            .field("llm", &self.llm)
            .field("embedding_model", &self.embedder.model_id())
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// The entity resolution results for one turn: the resolved (kind, name) → id
/// mapping, plus the entities still to be created.
#[derive(Default)]
struct EntityResolution {
    resolved: HashMap<(String, String), Id>,
    created: Vec<NewEntity>,
}

impl EntityResolution {
    fn require(&self, r: &EntityRef) -> Result<Id> {
        self.resolved.get(&r.cache_key()).copied().ok_or_else(|| {
            CormexError::Memory(format!("entity {}({}) was never resolved", r.name, r.kind))
        })
    }
}

/// The persistence plan for one candidate fact. Every decision has already been
/// computed before the transaction is entered.
struct Plan {
    fact: NewFact,
    supersede: Vec<Id>,
    flag: Vec<Id>,
    invalid_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    // ── Fault-tolerant JSON parsing ────────────────────────

    #[test]
    fn parses_plain_json() {
        let raw = r#"{"facts":[{"subject":{"name":"张三","kind":"person"},
            "predicate":"prefers","object":{"text":"Rust"},
            "statement":"张三偏好用 Rust 写服务","confidence":0.9}]}"#;
        let out = parse_candidates(raw).expect("should parse");
        assert_eq!(out.len(), 1, "actually parsed {} entries", out.len());
        assert_eq!(out[0].subject.name, "张三");
        assert_eq!(out[0].object, ObjectRef::Literal("Rust".into()));
    }

    #[test]
    fn tolerates_markdown_fence() {
        let raw = "好的，这是抽取结果：\n```json\n{\"facts\":[{\"subject\":\"cormex\",\
            \"predicate\":\"uses\",\"object\":\"RustFS\",\"statement\":\"cormex 用 RustFS\"}]}\n```\n希望有帮助。";
        let out =
            parse_candidates(raw).expect("should parse even with a fence and surrounding prose");
        assert_eq!(out.len(), 1, "actually parsed {} entries", out.len());
        assert_eq!(
            out[0].subject.kind, "concept",
            "a subject given as a bare string should fall back to the default kind"
        );
    }

    #[test]
    fn tolerates_trailing_prose_with_braces() {
        // A closing brace in the trailing prose — the naive "take the first {
        // through the last }" would swallow the explanation along with it
        let raw = r#"{"facts":[]}  说明：格式为 {"facts": []}"#;
        let out = parse_candidates(raw).expect("should take only the balanced span");
        assert!(out.is_empty(), "actually got {} entries", out.len());
    }

    #[test]
    fn accepts_bare_array() {
        let raw = r#"[{"subject":{"name":"cormex","kind":"project"},"predicate":"uses",
            "object":{"entity":{"name":"Postgres","kind":"tool"}},"statement":"cormex 用 Postgres"}]"#;
        let out = parse_candidates(raw).expect("a top-level array should parse too");
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0].object, ObjectRef::Entity(_)));
    }

    #[test]
    fn drops_only_the_broken_entry() {
        let raw = r#"{"facts":[
            {"predicate":"uses","object":"x"},
            {"subject":{"name":"cormex","kind":"project"},"predicate":"uses","object":"Postgres"}
        ]}"#;
        let out = parse_candidates(raw).expect("should parse");
        assert_eq!(
            out.len(),
            1,
            "the entry missing a subject should be dropped and the good one kept, actually got {} entries",
            out.len()
        );
        assert_eq!(
            out[0].statement, "cormex uses Postgres",
            "a missing statement should be assembled automatically"
        );
    }

    #[test]
    fn clamps_confidence_and_percentages() {
        let raw = r#"{"facts":[{"subject":"a","predicate":"p","object":"o","confidence":95},
                                {"subject":"b","predicate":"p","object":"o","confidence":-3}]}"#;
        let out = parse_candidates(raw).expect("should parse");
        assert!(
            (out[0].confidence - 0.95).abs() < 1e-6,
            "a percentage should be rescaled, actually {}",
            out[0].confidence
        );
        assert!(
            (out[1].confidence - 0.0).abs() < 1e-6,
            "an out-of-range value should be clamped, actually {}",
            out[1].confidence
        );
    }

    #[test]
    fn non_json_is_an_error_not_a_panic() {
        assert!(parse_candidates("我没有找到值得记录的事实。").is_err());
    }

    #[test]
    fn parses_event_time_at_several_granularities() {
        assert_eq!(parse_event_time("2026-08-07"), Some(ts(2026, 8, 7)));
        assert_eq!(parse_event_time("2026-08"), Some(ts(2026, 8, 1)));
        assert_eq!(parse_event_time("2026"), Some(ts(2026, 1, 1)));
        assert!(parse_event_time("2026-08-07T10:30:00Z").is_some());
        assert_eq!(parse_event_time("null"), None);
        assert_eq!(
            parse_event_time("上周"),
            None,
            "a vague phrasing must not be guessed into some specific date"
        );
    }

    #[test]
    fn normalizes_predicate_spelling_variants() {
        for raw in ["decided on", "Decided-On", "decided_on", "  DECIDED  ON  "] {
            assert_eq!(normalize_predicate(raw), "decided_on", "input: {raw}");
        }
    }

    // ── Conflict resolution ────────────────────────────────

    fn rival(key: &str, at: DateTime<Utc>) -> Rival {
        Rival {
            fact_id: Id::new(),
            object_key: key.to_owned(),
            effective_at: at,
        }
    }

    #[test]
    fn identical_object_is_a_duplicate() {
        let rivals = [rival("t:postgres", ts(2026, 1, 1))];
        assert_eq!(
            decide("t:postgres", ts(2026, 6, 1), true, &rivals),
            Verdict::Duplicate,
            "re-persisting the same subject/predicate/object would pack the retrieval results with one and the same fact"
        );
    }

    #[test]
    fn multi_valued_predicate_coexists() {
        let rivals = [rival("t:rust", ts(2026, 1, 1))];
        let v = decide("t:go", ts(2026, 6, 1), false, &rivals);
        assert!(
            matches!(&v, Verdict::Write { supersede, flag, .. } if supersede.is_empty() && flag.is_empty()),
            "a multi-valued predicate is an addition and must not invalidate anything: {v:?}"
        );
    }

    #[test]
    fn functional_predicate_supersedes_the_older_one() {
        let old = rival("t:mysql", ts(2026, 1, 1));
        let v = decide(
            "t:postgres",
            ts(2026, 6, 1),
            true,
            std::slice::from_ref(&old),
        );
        match v {
            Verdict::Write {
                supersede,
                invalid_at,
                ..
            } => {
                assert_eq!(
                    supersede,
                    vec![old.fact_id],
                    "the later value should supersede the earlier one"
                );
                assert_eq!(
                    invalid_at,
                    ts(2026, 6, 1),
                    "invalid_at must be the new fact's event time"
                );
            }
            other => panic!("should have been judged a supersede, actually {other:?}"),
        }
    }

    #[test]
    fn backfilled_history_does_not_invalidate_the_present() {
        let current = rival("t:postgres", ts(2026, 6, 1));
        let v = decide("t:mysql", ts(2025, 1, 1), true, &[current]);
        assert!(
            matches!(&v, Verdict::Write { supersede, flag, .. } if supersede.is_empty() && flag.is_empty()),
            "backfilling last year's history must not mark a fact that is still true today as invalidated: {v:?}"
        );
    }

    #[test]
    fn same_instant_conflict_is_flagged_not_decided() {
        let other = rival("t:mysql", ts(2026, 6, 1));
        let v = decide(
            "t:postgres",
            ts(2026, 6, 1),
            true,
            std::slice::from_ref(&other),
        );
        match v {
            Verdict::Write {
                supersede, flag, ..
            } => {
                assert!(
                    supersede.is_empty(),
                    "if the ordering cannot be determined, neither entry should be invalidated"
                );
                assert_eq!(
                    flag,
                    vec![other.fact_id],
                    "it should land on the list awaiting manual confirmation"
                );
            }
            other => panic!("should have been judged a flag, actually {other:?}"),
        }
    }

    #[test]
    fn default_rules_treat_unknown_predicates_as_multi_valued() {
        let rules = PredicateRules::default();
        assert!(rules.is_functional("uses"));
        assert!(
            rules.is_functional("Decided-On"),
            "predicate spelling should be normalized before the lookup"
        );
        assert!(
            !rules.is_functional("mentions"),
            "predicates outside the table are always treated as multi-valued — a wrong invalidation costs far more than a missed one"
        );
    }

    #[test]
    fn rules_accept_domain_specific_predicates() {
        let rules = PredicateRules::default().with_functional(["current sprint"]);
        assert!(rules.is_functional("current_sprint"));
    }

    #[test]
    fn object_key_ignores_case_and_spacing() {
        assert_eq!(
            object_key(Some("  Postgres  "), None),
            object_key(Some("postgres"), None)
        );
        assert_ne!(object_key(Some("x"), None), object_key(None, Some("x")));
    }

    // ── Entity index ───────────────────────────────────────

    #[tokio::test]
    async fn index_returns_nearest_within_kind() {
        let index = InProcessEntityIndex::new();
        let a = EntityRecord {
            id: Id::new(),
            kind: "project".into(),
            name: "cormex".into(),
            embedding: vec![1.0, 0.0, 0.0],
        };
        let b = EntityRecord {
            id: Id::new(),
            kind: "person".into(),
            name: "张三".into(),
            embedding: vec![1.0, 0.0, 0.0],
        };
        index.remember(a.clone()).await;
        index.remember(b).await;

        let hits = index.nearest("project", &[1.0, 0.0, 0.0], 5).await;
        assert_eq!(
            hits.len(),
            1,
            "entities of a different kind must not slip into the candidates"
        );
        assert_eq!(hits[0].0.id, a.id);
        assert!(
            (hits[0].1 - 1.0).abs() < 1e-6,
            "actual similarity {}",
            hits[0].1
        );
    }

    #[tokio::test]
    async fn index_is_idempotent_on_the_same_entity() {
        let index = InProcessEntityIndex::new();
        let r = EntityRecord {
            id: Id::new(),
            kind: "project".into(),
            name: "cormex".into(),
            embedding: vec![1.0, 0.0],
        };
        index.remember(r.clone()).await;
        index.remember(r).await;
        assert_eq!(index.nearest("project", &[1.0, 0.0], 5).await.len(), 1);
    }

    #[test]
    fn cosine_handles_degenerate_input() {
        assert_eq!(cosine(&[], &[]), 0.0);
        assert_eq!(
            cosine(&[1.0], &[1.0, 2.0]),
            0.0,
            "mismatched dimensions should return 0 rather than panic"
        );
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    // ── prompt ─────────────────────────────────────────────

    #[test]
    fn system_prompt_states_the_only_criterion() {
        assert!(
            EXTRACT_SYSTEM.contains("会不会做错决定"),
            "the extraction criterion must be written into the prompt, or the model will start extracting small talk too"
        );
    }

    /// The "never extract" section blocks not noise, but **things that cannot be
    /// stored in an append-only database at all**.
    ///
    /// Splitting it from "explicitly do not extract" into two separate sections
    /// is deliberate: the latter is a signal-to-noise problem (extracting it only
    /// wastes budget), whereas anything in this section can never be taken back
    /// out once extracted — `redact` is the only exit, and it demands an explicit
    /// trigger, a second confirmation, and a tombstone (memory.md §11). One
    /// wrongly extracted health inference syncs all the way out to every device
    /// and then stays there permanently.
    ///
    /// The emotion rule especially must not be removed: inferring emotion in a
    /// workplace setting is a prohibited practice under EU AI Act Art.5(1)(f),
    /// in force since 2025-02-02 with no grace period — and Cormex is explicitly
    /// positioned to cover office work.
    #[test]
    fn system_prompt_forbids_what_cannot_be_unstored() {
        for (needle, why) in [
            (
                "情绪状态",
                "inferring emotion in a workplace setting is a prohibited practice already in force",
            ),
            (
                "性取向",
                "a GDPR Art.9 special category, whose lawfulness bar is far above ordinary personal data",
            ),
            (
                "第三方",
                "the user has no standing to consent on a third party's behalf",
            ),
            (
                "凭据与密钥",
                "under append-only, extracting one by mistake means it is retained forever and synced to every device",
            ),
        ] {
            assert!(
                EXTRACT_SYSTEM.contains(needle),
                "the prompt is missing the 「{needle}」 do-not-extract rule: {why}"
            );
        }
    }

    /// "Do not extract your own explanations and summaries" also sweeps away the
    /// assistant's **corrections** along the way — and those are exactly the
    /// class most worth keeping.
    ///
    /// The failure mode is silent: the model dutifully follows the rule and skips
    /// "no, it's actually Y", so all that remains in the database is "the user
    /// believes X". The next retrieval pulls that mistaken belief back as the
    /// user's settled position and reinforces it turn after turn — outside
    /// research records this as a primary cause of memory-induced sycophancy.
    /// The carve-out therefore has to appear in the prompt **alongside** the rule
    /// itself; a vague sentence of encouragement is not enough.
    #[test]
    fn system_prompt_carves_out_corrections_from_the_no_summary_rule() {
        assert!(
            EXTRACT_SYSTEM.contains("纠正与反驳"),
            "the no-explanations-or-summaries rule must have a carve-out for rebuttals, or the assistant's corrections get silently dropped"
        );
        assert!(
            EXTRACT_SYSTEM.contains("用户曾认为"),
            "a rebuttal must record both sides: keeping only the correct answer loses the context of what the user was wrong about"
        );
        let deny = EXTRACT_SYSTEM
            .find("你自己的推理过程")
            .expect("the no-explanations-or-summaries rule itself is still present");
        let carve = EXTRACT_SYSTEM
            .find("纠正与反驳")
            .expect("the carve-out is still present");
        assert!(
            deny < carve,
            "the carve-out must come after the rule, otherwise the rule is read without any knowledge that an exception exists"
        );
    }

    #[test]
    fn user_prompt_truncates_oversized_episodes() {
        let ctx = ExtractContext::new(Id::new(), ts(2026, 8, 7));
        let huge = "代码".repeat(MAX_EPISODE_CHARS);
        let prompt = build_user_prompt(&huge, &ctx);
        assert!(
            prompt.contains("已截断"),
            "an oversized turn must be truncated, otherwise cost grows linearly with it"
        );
        assert!(prompt.chars().count() < huge.chars().count());
    }

    #[test]
    fn user_prompt_carries_time_and_known_entities() {
        let ctx = ExtractContext::new(Id::new(), ts(2026, 8, 7))
            .with_domain("coding")
            .with_known_entities(["cormex"]);
        let prompt = build_user_prompt("聊了点东西", &ctx);
        assert!(
            prompt.contains("2026-08-07"),
            "the model needs a reference date before it can resolve relative times"
        );
        assert!(prompt.contains("cormex"));
        assert!(prompt.contains("coding"));
    }
}

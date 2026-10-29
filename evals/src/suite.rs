//! The suite's data model, loading, and static validation.
//!
//! # Why a home-grown suite
//!
//! Public benchmarks like LoCoMo / LongMemEval are entirely in English and only
//! test "ask one question, get one answer". Cormex's differentiating
//! capabilities — Chinese BM25 via jieba + tsvector, bitemporal replay,
//! provenance follow-ups, cross-domain retrieval — are **covered by no public
//! benchmark at all**. Without building our own, the selling points can never
//! be demonstrated, and RRF's k and the four paths' retrieval widths can only
//! ever be tuned by feel.
//!
//! # Why JSON rather than YAML
//!
//! The workspace currently has no YAML crate, and pulling in a new dependency
//! for one suite file (with `serde_yaml` no longer maintained) is not worth it.
//! JSON is a bit more verbose, but `serde_json` is already a dependency.
//!
//! # Format
//!
//! ```text
//! scenarios[]        Setup conversations. Poured into the real ingest pipeline in array order
//!   turns[]          One conversational turn
//!     text           The raw text, landing as an episode; in llm mode it is also the extractor's input
//!     facts[]        Handwritten candidate facts for seed mode, going through real resolution + write
//!     checkpoint     Optional. Records the **system time** after this turn commits, for time-replay questions to reference
//!     tool_calls[]   This turn's tool trace, landing as episode_tool_calls (in both modes)
//!     tool_expect[]  Statements a correct extractor should derive from the trace (**not written to the store**)
//!     tool_avoid[]   Things that must never be extracted from the trace (**not written to the store**)
//! questions[]
//!   gold[]           Each element is a group of substrings; a hit requires all of them in one fact's statement
//!   forbidden[]      The same matching rule, but **appearing costs points**
//!   never_extracted[] The same matching rule, but compared against the **corpus snapshot** rather than the retrieval results
//!   as_of            References a checkpoint, using system-time replay instead of the four retrieval paths
//! ```
//!
//! # Why tool_expect / tool_avoid are not written to the store
//!
//! Writing them would be answering our own question: the suite declares "this
//! sentence should be extracted", the eval writes that sentence into the store,
//! and then announces that retrieval can find it. Their only use is as
//! **anchors** for gold and never_extracted — a tool-experience question's gold
//! matches no `SeedFact` by definition (if it did, the question could be
//! answered without the extractor), but that cannot become license to invent
//! one out of thin air. The full admission procedure is in `evals/README.md`.
//!
//! # Why gold matches by substring rather than by fact id
//!
//! A fact's id is a ULID generated at write time, which a suite cannot write
//! down. And in llm mode even the statement's wording is decided by the model.
//! Substring matching ("these keywords all have to be in the same fact") is the
//! only identification scheme that holds in both modes.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// Question kinds. The basis for producing grouped numbers — a total score
/// hides problems, while "Chinese-semantic Recall@5 is only 0.3" is actionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// Chinese semantics: ask about the same thing in different words, testing
    /// tokenization and semantic proximity
    ChineseSemantic,
    /// Exact proper-noun matching: model numbers, ports, identifiers, personal
    /// names — BM25's home turf
    ExactTerm,
    /// Time replay: "what did I believe three months ago", going through system
    /// time
    TemporalReplay,
    /// Relational reasoning: hop from an entity to the facts linked to it —
    /// graph traversal's home turf
    Relational,
    /// Cross-domain: ask about office matters while in coding (or vice versa)
    CrossDomain,
    /// **Tool experience**: the answer exists only in the tool trace, with the
    /// conversation text saying not one word of it.
    ///
    /// The fundamental difference from the five kinds above: those five test
    /// **retrieval** (the fact is already in the store; the question is whether
    /// it ranks up front), while this one tests **extraction** (the fact does
    /// not exist yet at all; the question is whether the extractor can
    /// construct it from "command + path + success/failure").
    ///
    /// It is therefore the only kind in this eval that **necessarily fails
    /// entirely** under `mode=seed` — seed feeds handwritten `SeedFact`s, and
    /// the tool-trace questions deliberately declare no `facts`. The failure
    /// takes the form of `Status::GoldMissing` (the gold never landed), not
    /// "ranked too low". See `docs/memory-content.md` §2②.
    ToolExperience,
    /// **Should retrieve nothing**: the store simply has no answer.
    ///
    /// This kind is mandatory. Measuring recall alone rewards the degenerate
    /// strategy of "retrieve everything": crank the retrieval width to infinity
    /// and Recall@k is forever 1.
    Unanswerable,
}

impl QuestionKind {
    /// The label used in the report. It also becomes part of the gate-metric
    /// key (`kind.<label>.recall@5`), so it has to stay in step with the row
    /// names in `evals/README.md`.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ChineseSemantic => "chinese_semantic",
            Self::ExactTerm => "exact_term",
            Self::TemporalReplay => "temporal_replay",
            Self::Relational => "relational",
            Self::CrossDomain => "cross_domain",
            Self::ToolExperience => "tool_experience",
            Self::Unanswerable => "unanswerable",
        }
    }

    #[must_use]
    pub fn all() -> [Self; 7] {
        [
            Self::ChineseSemantic,
            Self::ExactTerm,
            Self::TemporalReplay,
            Self::Relational,
            Self::CrossDomain,
            Self::ToolExperience,
            Self::Unanswerable,
        ]
    }
}

/// The query's language. Chinese-English bilingualism is a hard requirement:
/// Cormex's corpus is natively mixed (Chinese prose with identifiers like
/// `RustFS` / `pg_advisory_xact_lock` embedded in it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    Zh,
    En,
    /// Mixed Chinese and English — a Chinese question carrying English
    /// identifiers
    Mixed,
}

impl Lang {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Zh => "chinese",
            Self::En => "english",
            Self::Mixed => "mixed_zh_en",
        }
    }

    #[must_use]
    pub fn all() -> [Self; 3] {
        [Self::Zh, Self::En, Self::Mixed]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suite {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub scenarios: Vec<Scenario>,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub id: String,
    /// coding / office / personal — lands in the `domain` column of episodes
    /// and facts
    pub domain: String,
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// The conversation's raw text. Lands as an episode; in llm mode it is also
    /// the extractor's input
    pub text: String,
    /// When the event occurred, `YYYY-MM-DD`
    pub occurred_at: String,
    /// Records the system time after this turn commits, for `as_of` to
    /// reference
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// Handwritten candidate facts for seed mode
    #[serde(default)]
    pub facts: Vec<SeedFact>,
    /// This turn's tool trace, landing as `episode_tool_calls` in array order
    /// (`ordinal` is just the index).
    ///
    /// It is **corpus**, not **candidate facts**: it is written to the store in
    /// seed and llm mode alike. The extractor does not read this table yet (the
    /// gap noted in `docs/memory-content.md` §2②), so for now this fixture is
    /// simply "sitting there waiting to be read" — which is exactly why
    /// tool-experience questions necessarily fail today, and also why the gain
    /// will be measurable once it is wired up, with no suite change needed.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// The statements a correct extractor **should** derive from the trace
    /// above.
    ///
    /// They are never written to the store (writing them would be answering our
    /// own question). Their only use is as anchors for `tool_experience`
    /// questions' gold: the integrity check requires every gold to match at
    /// least one `tool_expect`, otherwise that gold was invented out of thin
    /// air.
    #[serde(default)]
    pub tool_expect: Vec<String>,
    /// Things that **must never** be extracted from this trace — usually
    /// fragments of a command's raw output.
    ///
    /// The criterion is in `docs/memory-content.md` §2③, "raw bytes of tool
    /// output are explicitly not stored". Likewise anchors only: a question's
    /// `never_extracted` must match one of these.
    #[serde(default)]
    pub tool_avoid: Vec<String>,
}

/// A fixture for one tool call, corresponding one-to-one with
/// `cormex_store::NewEpisodeToolCall`.
///
/// Only four things — command + path + success/failure + a one-line conclusion
/// — and **no raw stdout**. The shape is copied from production's
/// `episode_tool_calls`; the eval must not be able to express more than
/// production can, or the gains it measures are fake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    /// The path this call touched; left empty for tools that touch no file
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Whether this call succeeded. **The delta from "failed" to "succeeded" is
    /// precisely the knowledge this kind of question tests**
    pub ok: bool,
    /// A one-line conclusion. On failure, it carries the first line of the
    /// tool's output, as production does
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedFact {
    pub subject: String,
    #[serde(default = "default_kind")]
    pub subject_kind: String,
    pub predicate: String,
    /// A literal object. Mutually exclusive with `object_entity`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// An entity object — this is what forms an edge in the knowledge graph,
    /// and graph-traversal retrieval depends on it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_entity: Option<String>,
    #[serde(default = "default_kind")]
    pub object_entity_kind: String,
    /// The natural-language phrasing. This is what gold's substrings match
    /// against
    pub statement: String,
    /// [Event time] `YYYY-MM-DD`. Contradiction resolution orders by it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// Override the scenario's domain (for cross-domain questions that need two
    /// domains mixed within one scenario)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

fn default_kind() -> String {
    "concept".to_owned()
}

fn default_confidence() -> f32 {
    0.9
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: QuestionKind,
    pub lang: Lang,
    pub query: String,
    /// The current conversation's domain, passed to the retriever for domain
    /// weighting
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// References a checkpoint — uses `retrieve_as_of` (system-time replay)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// The facts that should be retrieved. Each element is a group of
    /// substrings (ANDed together)
    #[serde(default)]
    pub gold: Vec<Vec<String>>,
    /// Facts that should **not** appear in the results. Superseded old values,
    /// topically adjacent distractors
    #[serde(default)]
    pub forbidden: Vec<Vec<String>>,
    /// Things that **should never have been extracted** — a different matter
    /// from `forbidden`; do not conflate them.
    ///
    /// `forbidden` judges "did retrieval rank it into the top k", which
    /// presupposes it is **already in the store**; `never_extracted` judges "it
    /// should not be in the store at all", and compares against the **corpus
    /// snapshot** rather than the retrieval results.
    ///
    /// Why they must be separate: after the "never extract this" section of the
    /// extraction prompt was rewritten, all eighteen retrieval-side metrics did
    /// not move — because whether something should be extracted is simply not
    /// something retrieval can see. This column is that section's only ruler.
    #[serde(default)]
    pub never_extracted: Vec<Vec<String>>,
    #[serde(default)]
    pub note: String,
}

impl Suite {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the suite file {}", path.display()))?;
        let suite: Self = serde_json::from_str(&raw).with_context(|| {
            format!(
                "suite {} is not valid JSON, or its fields do not line up",
                path.display()
            )
        })?;
        suite.validate()?;
        Ok(suite)
    }

    /// Static validation. **No database needed** — CI can run this step first
    /// to stop bad questions quickly.
    pub fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        let mut checkpoints = HashSet::new();

        for sc in &self.scenarios {
            if !seen.insert(format!("scenario:{}", sc.id)) {
                bail!("duplicate scenario id: {}", sc.id);
            }
            if sc.turns.is_empty() {
                bail!("scenario {} has no conversation turns at all", sc.id);
            }
            for (i, turn) in sc.turns.iter().enumerate() {
                parse_date(&turn.occurred_at).with_context(|| {
                    format!(
                        "scenario {} turn {}: occurred_at is not YYYY-MM-DD",
                        sc.id,
                        i + 1
                    )
                })?;
                if let Some(cp) = &turn.checkpoint
                    && !checkpoints.insert(cp.clone())
                {
                    bail!("duplicate checkpoint name: {cp}");
                }
                for f in &turn.facts {
                    validate_fact(&sc.id, i + 1, f)?;
                }
                validate_tool_trace(&sc.id, i + 1, turn)?;
            }
        }

        if self.questions.is_empty() {
            bail!("the suite has no questions at all");
        }

        for q in &self.questions {
            if !seen.insert(format!("question:{}", q.id)) {
                bail!("duplicate question id: {}", q.id);
            }
            if q.query.trim().is_empty() {
                bail!("question {} has an empty query", q.id);
            }
            if let Some(cp) = &q.as_of
                && !checkpoints.contains(cp)
            {
                bail!(
                    "question {} references checkpoint \"{cp}\", which does not exist",
                    q.id
                );
            }
            for group in q
                .gold
                .iter()
                .chain(q.forbidden.iter())
                .chain(q.never_extracted.iter())
            {
                if group.is_empty() || group.iter().any(|s| s.trim().is_empty()) {
                    bail!(
                        "question {} has an empty string in a matcher group -- an empty string \
                         matches everything, which is the same as writing nothing",
                        q.id
                    );
                }
            }
            match q.kind {
                QuestionKind::Unanswerable => {
                    if !q.gold.is_empty() {
                        bail!("question {} is marked unanswerable yet carries gold", q.id);
                    }
                    if q.forbidden.is_empty() {
                        bail!(
                            "question {} is unanswerable and must carry forbidden -- otherwise there is \
                             nothing in it that can be judged at all",
                            q.id
                        );
                    }
                }
                _ => {
                    if q.gold.is_empty() {
                        bail!("question {} has no gold", q.id);
                    }
                }
            }
            if q.kind == QuestionKind::TemporalReplay && q.as_of.is_none() {
                bail!(
                    "question {} is a time-replay question but has no as_of -- the four legs query \
                     the currently valid set and would replay nothing",
                    q.id
                );
            }
            if q.kind == QuestionKind::ToolExperience && q.as_of.is_some() {
                bail!(
                    "question {} is a tool-experience question but carries as_of -- replay uses the \
                     three legs inside the snapshot, which measures \"what was in the store then\" \
                     rather than \"can the extractor extract it\"",
                    q.id
                );
            }
        }

        // Question-kind coverage: one kind missing means the eval has a blind
        // spot
        let counts = self.count_by_kind();
        for kind in QuestionKind::all() {
            if counts.get(&kind).copied().unwrap_or(0) == 0 {
                bail!(
                    "question kind \"{}\" has no questions at all; the eval has a blind spot",
                    kind.label()
                );
            }
        }

        // Tool-experience questions with not a single trace = that kind of
        // question is **doomed** to be unanswerable, and the way it fails (the
        // gold never landed) looks exactly like "the extractor is not wired to
        // tool traces yet" — from the report there is no way to tell a missing
        // fixture from a missing implementation
        if counts
            .get(&QuestionKind::ToolExperience)
            .copied()
            .unwrap_or(0)
            > 0
            && self.tool_call_count() == 0
        {
            bail!(
                "the suite has tool-experience questions but no tool_calls corpus at all -- \
                 that kind of question is then permanently unanswerable"
            );
        }

        Ok(())
    }

    #[must_use]
    pub fn count_by_kind(&self) -> BTreeMap<QuestionKind, usize> {
        let mut out = BTreeMap::new();
        for q in &self.questions {
            *out.entry(q.kind).or_insert(0) += 1;
        }
        out
    }

    #[must_use]
    pub fn count_by_lang(&self) -> BTreeMap<Lang, usize> {
        let mut out = BTreeMap::new();
        for q in &self.questions {
            *out.entry(q.lang).or_insert(0) += 1;
        }
        out
    }

    /// How many setup facts the suite declares in total (the corpus size under
    /// seed mode).
    #[must_use]
    pub fn seed_fact_count(&self) -> usize {
        self.scenarios
            .iter()
            .flat_map(|s| &s.turns)
            .map(|t| t.facts.len())
            .sum()
    }

    #[must_use]
    pub fn turn_count(&self) -> usize {
        self.scenarios.iter().map(|s| s.turns.len()).sum()
    }

    /// How many tool calls the suite declares in total.
    #[must_use]
    pub fn tool_call_count(&self) -> usize {
        self.turns().map(|t| t.tool_calls.len()).sum()
    }

    /// Every turn, in scenario order.
    pub fn turns(&self) -> impl Iterator<Item = &Turn> {
        self.scenarios.iter().flat_map(|s| &s.turns)
    }
}

/// Structural validation of a tool trace. The length limits align with
/// `cormex_store`'s CHECKs — the symptom of a mismatch is the whole write
/// transaction rolling back halfway through loading the corpus, with the error
/// message stuck over on the Postgres side.
fn validate_tool_trace(scenario: &str, turn_idx: usize, turn: &Turn) -> Result<()> {
    for (i, c) in turn.tool_calls.iter().enumerate() {
        let at = format!("scenario {scenario} turn {turn_idx} call {}", i + 1);
        if c.name.trim().is_empty() {
            bail!("{at} has no tool name");
        }
        if c.name.chars().count() > cormex_store::TOOL_NAME_MAX_CHARS {
            bail!(
                "{at}: the tool name exceeds {} characters and the CHECK would reject the write",
                cormex_store::TOOL_NAME_MAX_CHARS
            );
        }
        if c.summary.trim().is_empty() {
            bail!(
                "{at} has no summary -- all that is left is \"a command ran\", which expresses \
                 no knowledge whatsoever"
            );
        }
        if c.summary.chars().count() > cormex_store::TOOL_SUMMARY_MAX_CHARS {
            bail!(
                "{at}: the summary exceeds {} characters and the CHECK would reject the write",
                cormex_store::TOOL_SUMMARY_MAX_CHARS
            );
        }
        if let Some(p) = &c.path
            && p.chars().count() > cormex_store::TOOL_PATH_MAX_CHARS
        {
            bail!(
                "{at}: the path exceeds {} characters and the CHECK would reject the write",
                cormex_store::TOOL_PATH_MAX_CHARS
            );
        }
    }

    if turn.tool_calls.is_empty() && !(turn.tool_expect.is_empty() && turn.tool_avoid.is_empty()) {
        bail!(
            "scenario {scenario} turn {turn_idx} declares tool_expect/tool_avoid but has no \
             tool_calls -- that \"ought to be extracted\" is then made up out of thin air \
             rather than coming from a trace"
        );
    }
    for s in turn.tool_expect.iter().chain(turn.tool_avoid.iter()) {
        if s.trim().is_empty() {
            bail!(
                "scenario {scenario} turn {turn_idx}: tool_expect/tool_avoid contains an empty string"
            );
        }
    }
    Ok(())
}

fn validate_fact(scenario: &str, turn: usize, f: &SeedFact) -> Result<()> {
    match (&f.object, &f.object_entity) {
        (Some(_), None) | (None, Some(_)) => {}
        _ => bail!(
            "scenario {scenario} turn {turn}: fact \"{}\" must give exactly one of object and \
         object_entity",
            f.statement
        ),
    }
    if f.statement.trim().is_empty() {
        bail!("scenario {scenario} turn {turn}: a fact is missing its statement");
    }
    if !(0.0..=1.0).contains(&f.confidence) {
        bail!(
            "scenario {scenario} turn {turn}: confidence {} is outside the schema's CHECK range",
            f.confidence
        );
    }
    if let Some(v) = &f.valid_at {
        parse_date(v).with_context(|| {
            format!("scenario {scenario} turn {turn}: valid_at is not YYYY-MM-DD")
        })?;
    }
    Ok(())
}

/// `YYYY-MM-DD` → midnight UTC.
pub fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .with_context(|| format!("\"{s}\" is not YYYY-MM-DD"))?;
    d.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc())
        .context("the date cannot be turned into a timestamp")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Suite {
        let mut questions: Vec<Question> = QuestionKind::all()
            .iter()
            .enumerate()
            .map(|(i, kind)| Question {
                id: format!("q{i}"),
                kind: *kind,
                lang: Lang::Zh,
                query: "问题".into(),
                domain: None,
                as_of: (*kind == QuestionKind::TemporalReplay).then(|| "cp".to_owned()),
                gold: if *kind == QuestionKind::Unanswerable {
                    vec![]
                } else {
                    vec![vec!["甲".into()]]
                },
                forbidden: if *kind == QuestionKind::Unanswerable {
                    vec![vec!["乙".into()]]
                } else {
                    vec![]
                },
                never_extracted: if *kind == QuestionKind::ToolExperience {
                    vec![vec!["原始输出".into()]]
                } else {
                    vec![]
                },
                note: String::new(),
            })
            .collect();
        questions[0].note = "首题".into();

        Suite {
            name: "t".into(),
            version: "1".into(),
            description: String::new(),
            scenarios: vec![Scenario {
                id: "sc".into(),
                domain: "coding".into(),
                turns: vec![Turn {
                    text: "一轮对话".into(),
                    occurred_at: "2026-01-01".into(),
                    checkpoint: Some("cp".into()),
                    facts: vec![SeedFact {
                        subject: "甲".into(),
                        subject_kind: "concept".into(),
                        predicate: "p".into(),
                        object: Some("乙".into()),
                        object_entity: None,
                        object_entity_kind: "concept".into(),
                        statement: "甲的 p 是乙".into(),
                        valid_at: None,
                        confidence: 0.9,
                        domain: None,
                    }],
                    tool_calls: vec![ToolCall {
                        name: "bash".into(),
                        path: Some("Cargo.lock".into()),
                        ok: true,
                        summary: "跑了个命令".into(),
                    }],
                    tool_expect: vec!["应当抽出来的一条".into()],
                    tool_avoid: vec!["原始输出".into()],
                }],
            }],
            questions,
        }
    }

    #[test]
    fn minimal_suite_passes() {
        minimal()
            .validate()
            .expect("the minimal suite should be valid");
    }

    #[test]
    fn duplicate_question_ids_are_rejected() {
        let mut s = minimal();
        s.questions[1].id = s.questions[0].id.clone();
        let err = s.validate().expect_err("a duplicate id should be an error");
        assert!(err.to_string().contains("duplicate"), "actual error: {err}");
    }

    #[test]
    fn unanswerable_must_not_carry_gold() {
        let mut s = minimal();
        let i = s
            .questions
            .iter()
            .position(|q| q.kind == QuestionKind::Unanswerable)
            .expect("there should be a should-retrieve-nothing question");
        s.questions[i].gold = vec![vec!["甲".into()]];
        assert!(
            s.validate().is_err(),
            "a should-retrieve-nothing question carrying gold is \
             self-contradictory and must be blocked"
        );
    }

    #[test]
    fn missing_kind_coverage_is_rejected() {
        let mut s = minimal();
        s.questions.retain(|q| q.kind != QuestionKind::Unanswerable);
        let err = s
            .validate()
            .expect_err("a missing question kind should be an error");
        assert!(
            err.to_string().contains("unanswerable"),
            "the error must name which kind is missing: {err}"
        );
    }

    #[test]
    fn temporal_question_without_checkpoint_is_rejected() {
        let mut s = minimal();
        let i = s
            .questions
            .iter()
            .position(|q| q.kind == QuestionKind::TemporalReplay)
            .expect("there should be a time-replay question");
        s.questions[i].as_of = None;
        assert!(
            s.validate().is_err(),
            "a time-replay question without as_of degenerates into an ordinary \
             retrieval question"
        );
    }

    #[test]
    fn empty_matcher_group_is_rejected() {
        let mut s = minimal();
        s.questions[0].gold = vec![vec!["  ".into()]];
        assert!(
            s.validate().is_err(),
            "an empty string matches everything, which makes this question \
             score full marks unconditionally"
        );
    }

    #[test]
    fn fact_needs_exactly_one_object_form() {
        let mut s = minimal();
        s.scenarios[0].turns[0].facts[0].object_entity = Some("丙".into());
        assert!(
            s.validate().is_err(),
            "giving both a literal object and an entity object leaves the write \
             path unable to decide"
        );
    }

    #[test]
    fn tool_experience_question_without_any_trace_is_rejected() {
        let mut s = minimal();
        s.scenarios[0].turns[0].tool_calls.clear();
        s.scenarios[0].turns[0].tool_expect.clear();
        s.scenarios[0].turns[0].tool_avoid.clear();
        let err = s.validate().expect_err(
            "a tool-experience question should be blocked when there is no trace corpus",
        );
        assert!(
            err.to_string().contains("tool_calls"),
            "the error must name the missing trace corpus, otherwise it gets \
             misread as \"the extractor is not wired up yet\": {err}"
        );
    }

    #[test]
    fn tool_expectation_without_a_trace_is_rejected() {
        let mut s = minimal();
        s.scenarios[0].turns[0].tool_calls.clear();
        let err = s
            .validate()
            .expect_err("declaring tool_expect with no trace should be an error");
        assert!(
            err.to_string().contains("thin air"),
            "a \"should be extracted\" claim must rest on a trace: {err}"
        );
    }

    #[test]
    fn tool_call_without_summary_is_rejected() {
        let mut s = minimal();
        s.scenarios[0].turns[0].tool_calls[0].summary = "   ".into();
        assert!(
            s.validate().is_err(),
            "a trace with a command name but no conclusion gives the extractor \
             nothing to work with, which is the same as not writing the corpus \
             at all"
        );
    }

    #[test]
    fn tool_experience_question_must_not_be_a_replay() {
        let mut s = minimal();
        let i = s
            .questions
            .iter()
            .position(|q| q.kind == QuestionKind::ToolExperience)
            .expect("there should be a tool-experience question");
        s.questions[i].as_of = Some("cp".into());
        assert!(
            s.validate().is_err(),
            "a tool-experience question carrying as_of measures snapshot \
             contents, not extraction capability"
        );
    }

    #[test]
    fn never_extracted_rejects_empty_parts() {
        let mut s = minimal();
        let i = s
            .questions
            .iter()
            .position(|q| q.kind == QuestionKind::ToolExperience)
            .expect("there should be a tool-experience question");
        s.questions[i].never_extracted = vec![vec!["  ".into()]];
        assert!(
            s.validate().is_err(),
            "an empty string matches everything — this \"should never be \
             extracted\" entry would forever be judged a leak"
        );
    }

    #[test]
    fn parses_dates() {
        assert!(parse_date("2026-08-07").is_ok());
        assert!(parse_date("2026/08/07").is_err());
    }
}

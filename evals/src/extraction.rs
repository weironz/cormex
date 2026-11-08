//! Extraction quality on the path production runs: one complete QA turn
//! through [`DialogueAnalyzer`] (`qa@4`), analyse then verify, both passed
//! through [`validate_analysis`], then the credential scan that blocks a whole
//! analysis.
//!
//! # Why a separate suite and not `run --mode llm`
//!
//! `run --mode llm` feeds turns to `Extractor::ingest`, the per-episode
//! extractor. Production stopped using that for archived chats: automatic
//! memory goes through the dialogue analyser, which sees a whole question and
//! its answer, quotes its evidence, and is filtered by rules no other path
//! has (unadopted suggestions, one-off assignments, weaker reports). So the
//! retrieval suite, in either mode, said nothing about the extractor that
//! actually writes memory. This one measures that extractor and nothing else:
//! no database, no retrieval, no embedding.
//!
//! # What it scores
//!
//! Each case is a short conversation with two lists (checked by
//! [`ExtractionSuite::validate`]):
//!
//! - `expect`: groups of alternatives. A group is met when some retained
//!   memory contains one of them. **Recall** is met groups / all groups. The
//!   first alternative appears verbatim in the source; the others are
//!   renderings a correct memory may use instead. The extractor writes its
//!   memories in Chinese, so an English conversation about "absolute dates"
//!   comes back as 绝对日期, and an English-only group would score a correct
//!   memory as a miss.
//! - `avoid`: fragments no retained memory may contain. A case with any of
//!   them retained has **leaked**. Credentials, declined suggestions, one-off
//!   progress, small talk and injected instructions live here. Each appears
//!   verbatim in the source. Substring matching cannot tell "use X" from
//!   "do not use X", so an avoid fragment is a detail a correct memory has no
//!   reason to repeat (the declined value, the secret), never the name of a
//!   thing a correct constraint may mention.
//!
//! Matching is by substring, case-insensitive, over a memory's statement and
//! its object text. The model words its own statements, so the fragments are
//! the terms a correct memory cannot avoid (`RustFS`, `write_txn`), not
//! sentences.
//!
//! # Not a regression gate
//!
//! It calls a model, so its numbers drift with the model: the same reason the
//! retrieval suite keeps `--mode llm` out of CI. What goes into CI is
//! `validate --extraction`, which keeps the cases themselves honest. The run
//! is for comparing a prompt or rule change against a baseline by hand, with
//! `--min-recall` / `--max-leak` to make that comparison exit non-zero.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Context, Result, bail};
use cormex_memory::dialogue::{
    DialogueAnalyzer, DialogueMessage, ToolEvidence, TurnAnalysis, TurnInput, validate_analysis,
};
use serde::{Deserialize, Serialize};

/// The categories a case can be in. Closed, so a typo is a validation error
/// rather than a category of one.
pub const CATEGORIES: &[&str] = &[
    "project_fact",
    "user_preference",
    "decision_adopted",
    "proposal_not_adopted",
    "credential",
    "general_knowledge",
    "transient_progress",
    "delegated_task",
    "tool_verified",
    "prompt_injection",
    "correction",
    "chitchat",
    "assistant_report",
];

#[derive(Debug, Clone, Deserialize)]
pub struct ExtractionSuite {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub cases: Vec<Case>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Case {
    pub id: String,
    pub category: String,
    pub lang: String,
    #[serde(default)]
    pub project: Option<String>,
    /// `conversation` (default) or `delegated_task`, as production decides it.
    #[serde(default)]
    pub source_mode: Option<String>,
    pub messages: Vec<CaseMessage>,
    #[serde(default)]
    pub tools: Vec<CaseTool>,
    #[serde(default)]
    pub expect: Vec<Vec<String>>,
    #[serde(default)]
    pub avoid: Vec<String>,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaseMessage {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CaseTool {
    pub tool_name: String,
    pub command: String,
    pub text: String,
    pub successful: bool,
}

impl ExtractionSuite {
    /// # Errors
    /// The file cannot be read or does not parse.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the extraction suite {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| {
            format!(
                "extraction suite {} is not valid JSON, or its fields do not line up",
                path.display()
            )
        })
    }

    /// Keep the cases honest. No model, no database.
    ///
    /// The rule that matters most: **the fragments are in the source.** An
    /// `expect` group whose first alternative the conversation never mentions
    /// is a question no extractor can pass; an `avoid` it never mentions is a
    /// check that passes whatever the extractor does. Both would move the
    /// numbers without measuring anything.
    ///
    /// # Errors
    /// The first broken case, by id.
    pub fn validate(&self) -> Result<()> {
        if self.cases.is_empty() {
            bail!("the extraction suite has no cases");
        }
        let mut ids = HashSet::new();
        for c in &self.cases {
            if !ids.insert(c.id.as_str()) {
                bail!("duplicate case id {}", c.id);
            }
            if !CATEGORIES.contains(&c.category.as_str()) {
                bail!("{}: unknown category {:?}", c.id, c.category);
            }
            if !matches!(c.lang.as_str(), "zh" | "en") {
                bail!("{}: lang must be zh or en, not {:?}", c.id, c.lang);
            }
            if let Some(mode) = &c.source_mode
                && !matches!(mode.as_str(), "conversation" | "delegated_task")
            {
                bail!("{}: unknown source_mode {mode:?}", c.id);
            }
            if !c.messages.iter().any(|m| m.role == "user")
                || !c.messages.iter().any(|m| m.role == "assistant")
            {
                bail!(
                    "{}: needs a user message and an assistant answer -- production only \
                     analyses complete turns",
                    c.id
                );
            }
            if let Some(m) = c
                .messages
                .iter()
                .find(|m| !matches!(m.role.as_str(), "user" | "assistant"))
            {
                bail!(
                    "{}: message role {:?} is not user or assistant",
                    c.id,
                    m.role
                );
            }
            if c.expect.is_empty() && c.avoid.is_empty() {
                bail!("{}: neither expect nor avoid, so it scores nothing", c.id);
            }
            let source = c.source_text().to_lowercase();
            for group in &c.expect {
                let Some(anchor) = group.first() else {
                    bail!("{}: an empty expect group is always met", c.id);
                };
                if !source.contains(&anchor.to_lowercase()) {
                    bail!(
                        "{}: expect {anchor:?} (a group's first alternative) does not appear \
                         in the source",
                        c.id
                    );
                }
            }
            for a in &c.avoid {
                if !source.contains(&a.to_lowercase()) {
                    bail!(
                        "{}: avoid {a:?} does not appear in the source, so it cannot leak",
                        c.id
                    );
                }
                for alt in c.expect.iter().flatten() {
                    let (x, y) = (alt.to_lowercase(), a.to_lowercase());
                    if x.contains(&y) || y.contains(&x) {
                        bail!("{}: expect {alt:?} and avoid {a:?} contradict", c.id);
                    }
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn count_by_category(&self) -> BTreeMap<&str, usize> {
        let mut out = BTreeMap::new();
        for c in &self.cases {
            *out.entry(c.category.as_str()).or_default() += 1;
        }
        out
    }
}

impl Case {
    /// Everything the extractor is shown, as one text: the messages and the
    /// tool output (not the command, which the prompt forbids quoting).
    fn source_text(&self) -> String {
        self.messages
            .iter()
            .map(|m| m.text.as_str())
            .chain(self.tools.iter().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The input production would build for this turn: message ids `m1…`,
    /// tool ids `t1…`, one minute apart, no earlier context, no existing
    /// memories (each case is scored on its own).
    #[must_use]
    pub fn input(&self) -> TurnInput {
        let at = |i: usize| format!("2026-10-01T10:{i:02}:00Z");
        TurnInput {
            session_id: format!("eval-{}", self.id),
            chat_title: self.id.clone(),
            project_name: self.project.clone(),
            source_mode: self
                .source_mode
                .clone()
                .unwrap_or_else(|| "conversation".into()),
            messages: self
                .messages
                .iter()
                .enumerate()
                .map(|(i, m)| DialogueMessage {
                    id: format!("m{}", i + 1),
                    seq: i64::try_from(i).unwrap_or(0),
                    role: m.role.clone(),
                    text: m.text.clone(),
                    occurred_at: at(i),
                })
                .collect(),
            recent_context: Vec::new(),
            previous_summary: None,
            evidence_records: self
                .tools
                .iter()
                .enumerate()
                .map(|(i, t)| ToolEvidence {
                    id: format!("t{}", i + 1),
                    seq: i64::try_from(100 + i).unwrap_or(0),
                    tool_name: t.tool_name.clone(),
                    command: t.command.clone(),
                    text: t.text.clone(),
                    successful: t.successful,
                })
                .collect(),
            existing_memories: Vec::new(),
        }
    }
}

/// One case's outcome.
#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub id: String,
    pub category: String,
    pub lang: String,
    /// What was retained: statement and object text, as stored.
    pub retained: Vec<String>,
    pub expect_total: usize,
    pub expect_met: usize,
    /// The `expect` groups no retained memory met, by first alternative.
    pub missed: Vec<String>,
    /// The `avoid` fragments some retained memory contains.
    pub leaked: Vec<String>,
    pub has_avoid: bool,
    /// The credential scan blocked the whole analysis, as production does.
    pub blocked: bool,
    /// The model call or its parsing failed; nothing was scored.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Totals {
    pub cases: usize,
    pub errors: usize,
    pub expect_total: usize,
    pub expect_met: usize,
    pub avoid_cases: usize,
    pub leaked_cases: usize,
    pub retained: usize,
}

impl Totals {
    fn add(&mut self, r: &CaseResult) {
        self.cases += 1;
        if r.error.is_some() {
            self.errors += 1;
            return;
        }
        self.expect_total += r.expect_total;
        self.expect_met += r.expect_met;
        self.avoid_cases += usize::from(r.has_avoid);
        self.leaked_cases += usize::from(!r.leaked.is_empty());
        self.retained += r.retained.len();
    }

    /// Met groups / all groups. `None` with no groups to meet.
    #[must_use]
    pub fn recall(&self) -> Option<f64> {
        ratio(self.expect_met, self.expect_total)
    }

    /// Cases that leaked / cases with something to avoid.
    #[must_use]
    pub fn leak_rate(&self) -> Option<f64> {
        ratio(self.leaked_cases, self.avoid_cases)
    }
}

#[allow(clippy::cast_precision_loss)]
fn ratio(n: usize, d: usize) -> Option<f64> {
    (d > 0).then(|| n as f64 / d as f64)
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractionReport {
    pub suite: String,
    pub model: String,
    pub totals: Totals,
    pub by_category: BTreeMap<String, Totals>,
    pub cases: Vec<CaseResult>,
}

/// Score one analysis against one case. Pure, so it is tested without a model.
#[must_use]
pub fn score(case: &Case, analysis: &TurnAnalysis) -> CaseResult {
    // Production blocks the whole analysis when anything in it is
    // credential-shaped: nothing from that turn is written
    let blocked =
        cormex_memory::redaction::scan(&serde_json::to_string(analysis).unwrap_or_default())
            .is_some();
    let retained: Vec<String> = if blocked {
        Vec::new()
    } else {
        analysis
            .memories
            .iter()
            .filter(|m| m.status == "retain")
            .map(|m| {
                let statement = m.fact["statement"].as_str().unwrap_or_default();
                let object = m.fact["object"]["text"].as_str().unwrap_or_default();
                format!("{statement}\n{object}")
            })
            .collect()
    };
    let lower: Vec<String> = retained.iter().map(|t| t.to_lowercase()).collect();
    let contains = |fragment: &str| {
        let f = fragment.to_lowercase();
        lower.iter().any(|t| t.contains(&f))
    };
    let missed: Vec<String> = case
        .expect
        .iter()
        .filter(|group| !group.iter().any(|alt| contains(alt)))
        .map(|group| group[0].clone())
        .collect();
    let leaked: Vec<String> = case.avoid.iter().filter(|a| contains(a)).cloned().collect();
    CaseResult {
        id: case.id.clone(),
        category: case.category.clone(),
        lang: case.lang.clone(),
        expect_total: case.expect.len(),
        expect_met: case.expect.len() - missed.len(),
        missed,
        leaked,
        has_avoid: !case.avoid.is_empty(),
        blocked,
        retained,
        error: None,
    }
}

/// Run every case (or those whose id starts with `only`) through `analyzer`,
/// the way production does: analyse, validate, verify, validate again.
///
/// A failed model call fails that case, not the run: one timeout must not
/// throw away twenty-three scored cases.
pub async fn run(
    analyzer: &dyn DialogueAnalyzer,
    suite: &ExtractionSuite,
    model: &str,
    only: Option<&str>,
) -> ExtractionReport {
    let mut cases = Vec::new();
    for case in suite
        .cases
        .iter()
        .filter(|c| only.is_none_or(|p| c.id.starts_with(p)))
    {
        let input = case.input();
        let outcome = async {
            let proposed = validate_analysis(analyzer.analyze(&input).await?, &input);
            let verified = validate_analysis(analyzer.verify(&input, &proposed).await?, &input);
            anyhow::Ok(verified)
        }
        .await;
        let result = match outcome {
            Ok(analysis) => score(case, &analysis),
            Err(e) => CaseResult {
                id: case.id.clone(),
                category: case.category.clone(),
                lang: case.lang.clone(),
                retained: Vec::new(),
                expect_total: case.expect.len(),
                expect_met: 0,
                missed: Vec::new(),
                leaked: Vec::new(),
                has_avoid: !case.avoid.is_empty(),
                blocked: false,
                error: Some(format!("{e:#}")),
            },
        };
        tracing::info!(
            case = %result.id,
            met = result.expect_met,
            of = result.expect_total,
            leaked = result.leaked.len(),
            "scored"
        );
        cases.push(result);
    }
    let mut totals = Totals::default();
    let mut by_category: BTreeMap<String, Totals> = BTreeMap::new();
    for r in &cases {
        totals.add(r);
        by_category.entry(r.category.clone()).or_default().add(r);
    }
    ExtractionReport {
        suite: suite.name.clone(),
        model: model.to_owned(),
        totals,
        by_category,
        cases,
    }
}

/// The report as text: totals, a row per category, then every case that
/// missed, leaked or failed (the ones worth reading).
#[must_use]
pub fn render(report: &ExtractionReport) -> String {
    use std::fmt::Write as _;
    let pct = |v: Option<f64>| v.map_or_else(|| "—".into(), |v| format!("{:.0}%", v * 100.0));
    let mut out = String::new();
    let t = &report.totals;
    let _ = writeln!(
        out,
        "extraction suite {} · model {} · {} cases ({} failed to run)",
        report.suite, report.model, t.cases, t.errors
    );
    let _ = writeln!(
        out,
        "recall {} ({}/{} expected memories) · leak rate {} ({}/{} cases with something to avoid) · {} memories retained\n",
        pct(t.recall()),
        t.expect_met,
        t.expect_total,
        pct(t.leak_rate()),
        t.leaked_cases,
        t.avoid_cases,
        t.retained
    );
    let _ = writeln!(
        out,
        "| category | cases | recall | leak rate | retained |\n|---|---:|---:|---:|---:|"
    );
    for (category, c) in &report.by_category {
        let _ = writeln!(
            out,
            "| {category} | {} | {} | {} | {} |",
            c.cases,
            pct(c.recall()),
            pct(c.leak_rate()),
            c.retained
        );
    }
    for r in &report.cases {
        if let Some(e) = &r.error {
            let _ = writeln!(out, "\n{} failed to run: {e}", r.id);
        } else if !r.missed.is_empty() || !r.leaked.is_empty() {
            let _ = writeln!(
                out,
                "\n{} ({}): missed {:?} leaked {:?}{}",
                r.id,
                r.category,
                r.missed,
                r.leaked,
                if r.blocked {
                    " (blocked by the credential scan)"
                } else {
                    ""
                }
            );
            for m in &r.retained {
                let _ = writeln!(out, "  retained: {}", m.replace('\n', " ⏎ "));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use cormex_memory::dialogue::{AssessedMemory, Evidence};

    fn case(expect: &[&[&str]], avoid: &[&str]) -> Case {
        Case {
            id: "t1".into(),
            category: "project_fact".into(),
            lang: "zh".into(),
            project: None,
            source_mode: None,
            messages: vec![
                CaseMessage {
                    role: "user".into(),
                    text: "对象存储用 RustFS，不用 MinIO".into(),
                },
                CaseMessage {
                    role: "assistant".into(),
                    text: "好的".into(),
                },
            ],
            tools: vec![],
            expect: expect
                .iter()
                .map(|g| g.iter().map(|s| (*s).to_owned()).collect())
                .collect(),
            avoid: avoid.iter().map(|s| (*s).to_owned()).collect(),
            note: String::new(),
        }
    }

    fn analysis(statements: &[(&str, &str)]) -> TurnAnalysis {
        TurnAnalysis {
            summary_title: "t".into(),
            summary_markdown: "s".into(),
            memories: statements
                .iter()
                .map(|(status, statement)| AssessedMemory {
                    status: (*status).into(),
                    reason: "r".into(),
                    evidence: vec![Evidence {
                        message_id: "m1".into(),
                        role: "user".into(),
                        quote: "RustFS".into(),
                    }],
                    fact: serde_json::json!({"statement": statement, "object": {"text": ""}}),
                    memory_key: "storage/object".into(),
                    memory_kind: "fact".into(),
                    verification: String::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn a_retained_memory_meets_expect_and_a_discarded_one_does_not() {
        let c = case(&[&["rustfs"]], &[]);
        let met = score(&c, &analysis(&[("retain", "对象存储使用 RustFS")]));
        assert_eq!(
            (met.expect_met, met.expect_total),
            (1, 1),
            "case-insensitive substring"
        );
        let discarded = score(&c, &analysis(&[("discard", "对象存储使用 RustFS")]));
        assert_eq!(
            discarded.expect_met, 0,
            "a discarded memory is not written, so it meets nothing"
        );
        assert_eq!(discarded.missed, ["rustfs"]);
    }

    #[test]
    fn an_avoided_fragment_in_a_retained_memory_is_a_leak() {
        let c = case(&[], &["MinIO"]);
        let leaked = score(&c, &analysis(&[("retain", "对象存储从 MinIO 换成 RustFS")]));
        assert_eq!(leaked.leaked, ["MinIO"]);
        let clean = score(&c, &analysis(&[("retain", "对象存储使用 RustFS")]));
        assert!(clean.leaked.is_empty());
    }

    #[test]
    fn a_credential_blocks_the_whole_analysis_as_in_production() {
        let c = case(&[&["RustFS"]], &[]);
        let r = score(
            &c,
            &analysis(&[(
                "retain",
                "RustFS 的 key 是 sk-3f9c2a7b1d8e4f60a5b2c9d7e1f3a8b4",
            )]),
        );
        assert!(r.blocked, "credential-shaped output is blocked");
        assert!(
            r.retained.is_empty(),
            "and nothing from the turn is written"
        );
        assert_eq!(r.expect_met, 0);
    }

    #[test]
    fn totals_count_recall_over_groups_and_leaks_over_cases() {
        let mut t = Totals::default();
        let c = case(&[&["RustFS"], &["MinIO"]], &["好的"]);
        t.add(&score(&c, &analysis(&[("retain", "RustFS")])));
        assert_eq!(t.recall(), Some(0.5));
        assert_eq!(t.leak_rate(), Some(0.0));
        let mut errored = score(&c, &analysis(&[]));
        errored.error = Some("timeout".into());
        t.add(&errored);
        assert_eq!(
            (t.cases, t.errors, t.expect_total),
            (2, 1, 2),
            "a failed case counts as run but scores nothing"
        );
    }

    #[test]
    fn validation_refuses_a_fragment_the_source_never_mentions() {
        let suite = |c: Case| ExtractionSuite {
            name: "t".into(),
            description: String::new(),
            cases: vec![c],
        };
        assert!(suite(case(&[&["RustFS"]], &["MinIO"])).validate().is_ok());
        let err = suite(case(&[&["Postgres"]], &[])).validate().unwrap_err();
        assert!(format!("{err}").contains("does not appear"), "{err}");
        let err = suite(case(&[], &["Redis"])).validate().unwrap_err();
        assert!(format!("{err}").contains("cannot leak"), "{err}");
        let err = suite(case(&[&["RustFS"]], &["Rust"]))
            .validate()
            .unwrap_err();
        assert!(format!("{err}").contains("contradict"), "{err}");
    }
}

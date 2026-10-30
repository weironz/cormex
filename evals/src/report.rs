//! Reports: machine-readable JSON (for CI comparison) and human-readable text.
//!
//! Both are needed. The JSON serves the regression gate — CI compares this
//! run's `by_kind[*].recall["5"]` against the baseline's, and a drop goes red.
//! The text serves people — nobody reads an eval tool that only emits JSON.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::harness::{CrossLinkStats, IngestStats};
use crate::metrics::{Aggregate, K_VALUES, QuestionResult, Status};
use crate::suite::{Lang, QuestionKind, Suite};

#[derive(Debug, Clone, Serialize)]
pub struct SuiteInfo {
    pub name: String,
    pub version: String,
    pub scenarios: usize,
    pub turns: usize,
    pub seed_facts: usize,
    pub questions: usize,
    pub by_kind: BTreeMap<String, usize>,
    pub by_lang: BTreeMap<String, usize>,
}

impl SuiteInfo {
    #[must_use]
    pub fn of(suite: &Suite) -> Self {
        Self {
            name: suite.name.clone(),
            version: suite.version.clone(),
            scenarios: suite.scenarios.len(),
            turns: suite.turn_count(),
            seed_facts: suite.seed_fact_count(),
            questions: suite.questions.len(),
            by_kind: suite
                .count_by_kind()
                .into_iter()
                .map(|(k, v)| (k.label().to_owned(), v))
                .collect(),
            by_lang: suite
                .count_by_lang()
                .into_iter()
                .map(|(k, v)| (k.label().to_owned(), v))
                .collect(),
        }
    }
}

/// Every knob used for this run. Baseline numbers are meaningless divorced from
/// these parameters.
#[derive(Debug, Clone, Serialize)]
pub struct RunInfo {
    pub mode: String,
    pub embedding_model: String,
    pub rrf_k: f64,
    pub width_bm25: i64,
    pub width_vector: i64,
    pub width_graph: i64,
    pub width_recency: i64,
    pub width_episode: i64,
    pub graph_hops: i32,
    pub recent_days: i64,
    pub context_window: usize,
    pub budget_tokens: usize,
    /// Whether domain weighting was turned off (`--ignore-domain`)
    pub domain_boost_off: bool,
    /// The abstention threshold. Sweeping the curve relies entirely on this to
    /// tell runs apart
    pub abstain_below: f64,
    /// The form of temporal recency: `channel` / `decay(strength,half-life)` /
    /// `off`
    pub recency_mode: String,
    /// Whether the episodes path is on
    pub episode_channel: bool,
    /// Compensation weight for a strong hit on a single path
    pub peak_bonus: f64,
    /// The semantic floor (an upper bound on cosine distance); `None` = unset
    pub semantic_floor: Option<f64>,
    /// Whether replay uses the ranked version or the full-picture version
    pub replay_ranked: bool,
    pub schema: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CorpusInfo {
    pub ingest: IngestStats,
    /// How many facts ended up in the store
    pub facts_total: usize,
    /// How many of those are still active
    pub facts_active: usize,
    pub entities: i64,
    /// How many rows landed in `episode_tool_calls`
    pub tool_calls: i64,
    /// Results of the cross-domain link scan. `None` = no scan was run this
    /// time.
    ///
    /// The edges themselves go into the report, not just a count: a noisy edge
    /// does its damage by spreading through graph traversal, and the number
    /// "wrote 12" is no help at all in judging noise — those 12 have to be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cross_links: Option<CrossLinkStats>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub suite: SuiteInfo,
    pub run: RunInfo,
    pub corpus: CorpusInfo,
    pub overall: Aggregate,
    pub by_kind: Vec<Aggregate>,
    pub by_lang: Vec<Aggregate>,
    pub questions: Vec<QuestionResult>,
}

impl Report {
    /// The handful of numbers the CI regression gate looks at.
    #[must_use]
    pub fn gate_metrics(&self) -> BTreeMap<String, f64> {
        let mut out = BTreeMap::new();
        for k in K_VALUES {
            out.insert(
                format!("overall.recall@{k}"),
                self.overall.recall.get(&k).copied().unwrap_or(0.0),
            );
        }
        out.insert("overall.mrr".into(), self.overall.mrr);
        for agg in &self.by_kind {
            out.insert(
                format!("kind.{}.recall@5", agg.label),
                agg.recall.get(&5).copied().unwrap_or(0.0),
            );
        }
        out
    }

    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }
}

// ══════════════════════════════════════════════════════════
//  Text rendering
// ══════════════════════════════════════════════════════════

/// How many of the worst questions the report lists.
const WORST_SAMPLE: usize = 12;

#[must_use]
pub fn render_text(report: &Report) -> String {
    let mut s = String::with_capacity(8192);

    let _ = writeln!(s, "# Cormex retrieval evaluation report");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "suite: {} {}    {} scenarios / {} turns / {} seeded facts / {} questions",
        report.suite.name,
        report.suite.version,
        report.suite.scenarios,
        report.suite.turns,
        report.suite.seed_facts,
        report.suite.questions
    );
    let r = &report.run;
    let _ = writeln!(
        s,
        "config: mode={}  embedder={}  RRF k={}  widths bm25/vector/graph/recency = {}/{}/{}/{}",
        r.mode,
        r.embedding_model,
        r.rrf_k,
        r.width_bm25,
        r.width_vector,
        r.width_graph,
        r.width_recency
    );
    let _ = writeln!(
        s,
        "        graph {} hops  recency window {} days  episodes width {}  context {} tokens  \
         injection budget {} tokens  domain weighting {}",
        r.graph_hops,
        r.recent_days,
        r.width_episode,
        r.context_window,
        r.budget_tokens,
        if r.domain_boost_off { "off" } else { "on" }
    );
    // This line is the A/B run's identity card: without printing it, two
    // reports side by side cannot be told apart
    let _ = writeln!(
        s,
        "knobs: abstain below {:.4}  semantic floor {}  recency shape {}  episodes leg {}  \
         peak bonus {}  replay {}",
        r.abstain_below,
        r.semantic_floor
            .map_or_else(|| "none".to_owned(), |v| format!("{v:.2}")),
        r.recency_mode,
        if r.episode_channel { "on" } else { "off" },
        r.peak_bonus,
        if r.replay_ranked {
            "ranked"
        } else {
            "full-set"
        }
    );
    let c = &report.corpus;
    let _ = writeln!(
        s,
        "corpus: {} candidates -> {} written (dropped as duplicates {}  superseded {}  \
         flagged as contradictory {})  entities {}  final facts {} ({} valid)",
        c.ingest.candidates,
        c.ingest.written,
        c.ingest.duplicates,
        c.ingest.superseded,
        c.ingest.flagged,
        c.entities,
        c.facts_total,
        c.facts_active
    );
    if c.tool_calls > 0 {
        // How many trace rows landed has to be printed on its own. When the
        // tool-experience questions all fail there are two possibilities — the
        // traces never reached the store, or they did and nobody read them —
        // and only this number tells them apart
        let _ = writeln!(
            s,
            "tool traces: {} rows landed in `episode_tool_calls` (whether the extractor read them \
             shows up as the gold-missing count on the `tool_experience` row below)",
            c.tool_calls
        );
    }
    if let Some(cl) = &c.cross_links {
        let _ = writeln!(
            s,
            "cross-domain links: judge {}  scanned {} facts -> {} candidate pairs -> {} accepted \
             ({} cut by degree/batch caps) -> {} rows written",
            cl.judge, cl.scanned, cl.candidates, cl.accepted, cl.degree_capped, cl.written
        );
    }
    let _ = writeln!(s);

    if let Some(cl) = &c.cross_links
        && !cl.links.is_empty()
    {
        // The edges themselves have to be printed. "Wrote N" cannot decide
        // whether they are noise; reading them can
        let _ = writeln!(s, "## Cross-domain links added by this run");
        let _ = writeln!(s);
        for l in &cl.links {
            let _ = writeln!(s, "- {l}");
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "## Overall");
    let _ = writeln!(s);
    let _ = write!(
        s,
        "{}",
        render_table(std::slice::from_ref(&report.overall), "scope")
    );
    let _ = writeln!(s);

    let _ = writeln!(s, "## By question kind");
    let _ = writeln!(s);
    let _ = write!(s, "{}", render_table(&report.by_kind, "type"));
    let _ = writeln!(s);

    let _ = writeln!(s, "## By language");
    let _ = writeln!(s);
    let _ = write!(s, "{}", render_table(&report.by_lang, "language"));
    let _ = writeln!(s);
    let _ = writeln!(s, "{TABLE_LEGEND}");

    let _ = writeln!(s, "## Per-leg contribution (raw retrieval, before fusion)");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "\"Exclusive\" = the number of questions only this leg caught. **That, and not \
         coverage, is the basis for whether a leg deserves to stay** --"
    );
    let _ = writeln!(
        s,
        "high coverage with zero exclusives means the leg can be entirely replaced by the others."
    );
    let _ = writeln!(s);
    let _ = write!(s, "{}", render_channels(&report.overall));
    let _ = writeln!(s);

    let _ = writeln!(s, "## Per-leg contribution - exclusives by question kind");
    let _ = writeln!(s);
    let _ = write!(s, "{}", render_channel_matrix(&report.by_kind));
    let _ = writeln!(s);

    let _ = write!(s, "{}", render_extraction(&report.questions));

    let broken: Vec<&QuestionResult> = report
        .questions
        .iter()
        .filter(|q| q.status != Status::Scored)
        .collect();
    if !broken.is_empty() {
        let _ = writeln!(
            s,
            "## Broken questions (excluded from the retrieval metrics)"
        );
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "gold never reaching the store at all is a problem of **extraction or of the question**, \
         not of retrieval. Mixing it into the metrics charges retrieval for it."
        );
        let _ = writeln!(s);
        for q in &broken {
            let _ = writeln!(
                s,
                "- `{}` [{}] {:?}  gold: {}",
                q.id,
                q.kind.label(),
                q.status,
                q.gold_labels.join(" / ")
            );
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "## Worst questions");
    let _ = writeln!(s);
    let mut worst: Vec<&QuestionResult> = report
        .questions
        .iter()
        .filter(|q| q.status == Status::Scored && q.gold_total > 0)
        .collect();
    worst.sort_by(|a, b| {
        a.reciprocal_rank
            .partial_cmp(&b.reciprocal_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.id.cmp(&b.id))
    });
    for q in worst.iter().take(WORST_SAMPLE) {
        let hit = q
            .gold_ranks
            .iter()
            .map(|r| r.map_or("×".to_owned(), |v| format!("#{}", v + 1)))
            .collect::<Vec<_>>()
            .join(",");
        let channels: Vec<String> = q
            .channels
            .iter()
            .filter(|(_, c)| c.best_rank.is_some())
            .map(|(n, c)| format!("{n}#{}", c.best_rank.unwrap_or(0) + 1))
            .collect();
        let _ = writeln!(
            s,
            "- `{}` [{}/{}] RR={:.2}  gold rank {hit}  hit by {}",
            q.id,
            q.kind.label(),
            q.lang.label(),
            q.reciprocal_rank,
            if channels.is_empty() {
                "none".to_owned()
            } else {
                channels.join(" ")
            }
        );
        let _ = writeln!(s, "  query: {}", q.query);
        if let Some((i, label)) = q
            .gold_ranks
            .iter()
            .position(Option::is_none)
            .and_then(|i| q.gold_labels.get(i).map(|l| (i, l)))
        {
            let _ = writeln!(s, "  missed gold[{i}]: {label}");
        }
    }

    s
}

/// Assertions on the extraction side.
///
/// Not one of the dozen-plus retrieval-side metrics can answer "should this
/// have been extracted at all" — their denominators are all "facts already in
/// the store". After the "never extract this" and "always extract corrections"
/// sections of the extraction prompt were rewritten, all eighteen regression
/// gate numbers stayed flat, and the root cause was that this eval had only a
/// retrieval-side ruler at the time. This section is the other one.
fn render_extraction(questions: &[QuestionResult]) -> String {
    let declared: Vec<&QuestionResult> = questions
        .iter()
        .filter(|q| q.never_extracted_total > 0)
        .collect();
    if declared.is_empty() {
        return String::new();
    }

    let leaked: Vec<&&QuestionResult> = declared.iter().filter(|q| !q.leaked.is_empty()).collect();
    let total: usize = declared.iter().map(|q| q.never_extracted_total).sum();
    let hit: usize = declared.iter().map(|q| q.leaked.len()).sum();

    let mut s = String::new();
    let _ = writeln!(
        s,
        "## Extraction side: things that should never have been extracted"
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "this compares against the **corpus snapshot**, not the retrieval result -- the rank is \
         irrelevant, **existing at all** is the failure."
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{} questions declare {total} assertions; {hit} leaked.",
        declared.len()
    );
    let _ = writeln!(s);
    for q in &leaked {
        for l in &q.leaked {
            let _ = writeln!(s, "- ✗ `{}` leaked: {l}", q.id);
        }
    }
    if !leaked.is_empty() {
        let _ = writeln!(s);
    }
    s
}

fn render_table(rows: &[Aggregate], head: &str) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "| {head} | count | scored | R@1 | R@5 | R@10 | R@∞ | MRR | mean hit rank | mis-retr.@5 \
         | mean injected | gold missing |"
    );
    let _ = writeln!(
        s,
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    );
    for a in rows {
        let _ = writeln!(
            s,
            "| {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {:.1} | {:.3} | {:.1} | {} |",
            a.label,
            a.total,
            a.scored,
            a.recall.get(&1).copied().unwrap_or(0.0),
            a.recall.get(&5).copied().unwrap_or(0.0),
            a.recall.get(&10).copied().unwrap_or(0.0),
            a.recall_any,
            a.mrr,
            a.avg_gold_rank,
            a.forbidden_rate.get(&5).copied().unwrap_or(0.0),
            a.avg_returned,
            a.gold_missing + a.gold_inactive,
        );
    }
    s
}

/// How to read the tables. Written once, shared by all three — an unexplained
/// metric leaves nobody able to tell what to fix.
const TABLE_LEGEND: &str = "> `R@∞` = recall with no rank limit; the gap from `R@5` is the debt \
owed by **ranking** -- a large gap means fix the fusion, a small one means fix retrieval.\n\
> `mean injected` not being 0 on the `unanswerable` row means the retriever **cannot abstain** -- \
with no answer to give, it still stuffs a whole block of memory into the prompt.\n";

fn render_channels(overall: &Aggregate) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "| leg | covered | exclusive | leg R@5 | mean retrieved | distractors sent into top 5 |"
    );
    let _ = writeln!(s, "|---|---:|---:|---:|---:|---:|");
    for (name, c) in &overall.channels {
        let _ = writeln!(
            s,
            "| {name} | {} | {} | {:.3} | {:.1} | {} |",
            c.covered, c.exclusive, c.recall_at_k, c.avg_size, c.forbidden_contributions
        );
    }
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "(denominator = {} questions that go through the four legs and can be scored; \
         time-replay questions do not issue a four-leg query and are excluded)",
        overall.channel_scored
    );
    s
}

fn render_channel_matrix(by_kind: &[Aggregate]) -> String {
    let mut s = String::new();
    let names: Vec<&String> = by_kind
        .first()
        .map(|a| a.channels.keys().collect())
        .unwrap_or_default();

    let _ = write!(s, "| type |");
    for n in &names {
        let _ = write!(s, " {n} covered/exclusive |");
    }
    let _ = writeln!(s);
    let _ = write!(s, "|---|");
    for _ in &names {
        let _ = write!(s, "---:|");
    }
    let _ = writeln!(s);

    for a in by_kind {
        let _ = write!(s, "| {} |", a.label);
        for n in &names {
            let c = a.channels.get(*n);
            let _ = write!(
                s,
                " {}/{} |",
                c.map_or(0, |c| c.covered),
                c.map_or(0, |c| c.exclusive)
            );
        }
        let _ = writeln!(s);
    }
    s
}

/// Stable ordering of question kinds / languages, so two runs' reports can be
/// diffed directly.
#[must_use]
pub fn kind_order() -> [QuestionKind; 7] {
    QuestionKind::all()
}

#[must_use]
pub fn lang_order() -> [Lang; 3] {
    Lang::all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::aggregate;

    #[test]
    fn empty_aggregate_renders_without_nan() {
        let table = render_table(&[aggregate("empty", &[])], "scope");
        assert!(
            !table.contains("NaN"),
            "a NaN would seep all the way into the CI comparison and must be \
             stopped at the source: {table}"
        );
    }

    #[test]
    fn gate_metrics_cover_every_kind() {
        // Comparing only the overall score in CI is not enough: one question
        // kind can collapse while the overall score dips slightly, and the
        // regression gate lets it through
        let report = Report {
            suite: SuiteInfo {
                name: "t".into(),
                version: "1".into(),
                scenarios: 0,
                turns: 0,
                seed_facts: 0,
                questions: 0,
                by_kind: BTreeMap::new(),
                by_lang: BTreeMap::new(),
            },
            run: RunInfo {
                mode: "seed".into(),
                embedding_model: "hash-stub-v1".into(),
                rrf_k: 60.0,
                width_bm25: 40,
                width_vector: 40,
                width_graph: 30,
                width_recency: 20,
                width_episode: 20,
                graph_hops: 2,
                recent_days: 7,
                context_window: 128_000,
                budget_tokens: 6000,
                domain_boost_off: false,
                abstain_below: 0.017,
                recency_mode: "off".into(),
                episode_channel: true,
                peak_bonus: 0.04,
                semantic_floor: Some(0.5),
                replay_ranked: true,
                schema: "x".into(),
            },
            corpus: CorpusInfo {
                ingest: IngestStats::default(),
                facts_total: 0,
                facts_active: 0,
                entities: 0,
                tool_calls: 0,
                cross_links: None,
            },
            overall: aggregate("all", &[]),
            by_kind: vec![aggregate("chinese_semantic", &[])],
            by_lang: vec![],
            questions: vec![],
        };
        let gate = report.gate_metrics();
        assert!(gate.contains_key("overall.recall@5"));
        assert!(gate.contains_key("kind.chinese_semantic.recall@5"));
    }
}

//! Metrics: Recall@k, MRR, per-path contribution, false-recall rate, and
//! grouped aggregation by question kind / language.
//!
//! # Why grouping is mandatory
//!
//! A total score hides problems. "Overall Recall@5 = 0.72" looks acceptable,
//! but if it is the average of "proper-noun exact 0.95 + Chinese semantic 0.30",
//! what actually needs fixing is tokenization and the vector path. So every
//! metric is emitted once per question kind and once per language.
//!
//! # Why per-path contribution is mandatory
//!
//! Which of the four retrieval paths to keep, and which to replace with a decay
//! applied after fusion, can only be settled by "how many questions had
//! **only** this path find the gold". Without that number, the roadmap item
//! "settle the temporal recency retrieval path" stays a matter of argument
//! forever.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::matcher::{Matcher, QuestionMatchers};
use crate::suite::{Lang, Question, QuestionKind};

/// The k values reported. 1 checks "is the first one right", 5 matches the
/// actual injection window, 10 shows the recall ceiling.
pub const K_VALUES: [usize; 3] = [1, 5, 10];

/// The k used for per-path diagnostics — aligned with the main metrics on the
/// fusion side.
pub const CHANNEL_K: usize = 5;

/// The stable ordering of the paths; both the report and the JSON follow it.
pub const CHANNELS: [&str; 5] = ["bm25", "vector", "graph", "recency", "episode"];

/// Whether a question can be scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Scored normally
    Scored,
    /// The gold never landed in the store at all — that is **the question's or
    /// the extraction pipeline's** problem, not retrieval's. Mixing it into the
    /// retrieval metrics would charge extraction's fault to retrieval, so it is
    /// counted separately.
    GoldMissing,
    /// The gold is in `facts` but is invalidated, and this is not a time-replay
    /// question — the question is mislabelled
    GoldInactive,
}

/// One result returned by the retriever (post-fusion, already truncated to the
/// budget).
#[derive(Debug, Clone, Serialize)]
pub struct ScoredItem {
    pub fact_id: String,
    pub statement: String,
    /// The retrieval paths that found it
    pub channels: Vec<String>,
}

/// One path's raw (unfused) results, for per-path diagnostics.
#[derive(Debug, Clone)]
pub struct ChannelRun {
    pub name: &'static str,
    /// Statements sorted by that path's own relevance
    pub statements: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelResult {
    /// How many entries this path retrieved
    pub size: usize,
    /// The gold's best rank within this path (zero-based); `None` = this path
    /// found nothing at all
    pub best_rank: Option<usize>,
    /// This path's own Recall@5
    pub recall_at_k: f64,
}

/// One question's complete result.
#[derive(Debug, Clone, Serialize)]
pub struct QuestionResult {
    pub id: String,
    pub kind: QuestionKind,
    pub lang: Lang,
    pub query: String,
    pub status: Status,
    /// Whether this went through `retrieve_as_of` (system-time replay) or the
    /// four retrieval paths
    pub replay: bool,
    pub gold_total: usize,
    /// How many forbidden entries this question declares. Questions with 0 do
    /// not count toward the false-recall rate, otherwise the denominator is
    /// diluted
    pub forbidden_total: usize,
    /// How many "should never have been extracted" assertions this question
    /// declares
    pub never_extracted_total: usize,
    /// How many of those were actually found in the corpus snapshot — **a
    /// mis-extraction on the extraction side**.
    ///
    /// Not the same thing as `forbidden_hit`: that one asks "did it rank into
    /// the top k", this one asks "is it in the store at all". The extraction
    /// prompt's "never extract this" leaves its trace in this column alone
    pub leaked: Vec<String>,
    /// Each gold's rank in the fused results
    pub gold_ranks: Vec<Option<usize>>,
    /// Descriptions of the golds, the same length as `gold_ranks` — the report
    /// has to point out "which one was not recalled"
    pub gold_labels: Vec<String>,
    /// k → Recall@k
    pub recall: BTreeMap<usize, f64>,
    /// Recall with no rank limit — separating "never found at all" from "found
    /// but ranked too low". The two are fixed completely differently: the
    /// former means changing retrieval, the latter means changing ranking
    pub recall_any: f64,
    /// The reciprocal of the first gold's rank; 0 when nothing was recalled
    pub reciprocal_rank: f64,
    /// k → whether a forbidden entry slipped into the top k
    pub forbidden_hit: BTreeMap<usize, bool>,
    /// Which paths pulled up each forbidden entry that slipped into the top 5.
    /// Only this number can answer "who caused the false recall" — without it,
    /// noise can only be blamed on "fusion" as a whole
    pub forbidden_channels: BTreeMap<String, usize>,
    /// How many entries the fused result holds
    pub returned: usize,
    pub channels: BTreeMap<String, ChannelResult>,
    /// If only one path found the gold, which one — the direct basis for
    /// deciding whether a path should be kept
    pub exclusive_channel: Option<String>,
    /// The first few fused results, for inspecting bad cases by hand
    pub top: Vec<ScoredItem>,
}

/// How many top results the report displays.
const TOP_SAMPLE: usize = 5;

/// Score one question.
///
/// `fused` is the retriever's final output (already truncated to the budget);
/// `channels` holds the four paths' raw retrievals, used only for diagnostics
/// and taking no part in the main metrics. `status` and `leaked` are determined
/// by the caller from the corpus snapshot after ingest — both are readings from
/// the **extraction** side, already fixed before retrieval has run a single
/// step.
#[must_use]
pub fn score(
    q: &Question,
    matchers: &QuestionMatchers,
    fused: &[ScoredItem],
    channels: &[ChannelRun],
    status: Status,
    replay: bool,
    leaked: Vec<String>,
) -> QuestionResult {
    let statements: Vec<&str> = fused.iter().map(|i| i.statement.as_str()).collect();

    let gold_ranks: Vec<Option<usize>> = matchers
        .gold
        .iter()
        .map(|m| m.first_hit(statements.iter().copied()))
        .collect();

    let gold_total = gold_ranks.len();
    let mut recall = BTreeMap::new();
    for k in K_VALUES {
        recall.insert(k, recall_at(&gold_ranks, k));
    }

    let recall_any = recall_at(&gold_ranks, usize::MAX);

    // MRR takes the reciprocal of the **first** matching gold's rank, matching
    // the standard information-retrieval definition
    let reciprocal_rank = gold_ranks
        .iter()
        .flatten()
        .min()
        .map_or(0.0, |r| 1.0 / (*r as f64 + 1.0));

    let mut forbidden_hit = BTreeMap::new();
    for k in K_VALUES {
        let hit = matchers
            .forbidden
            .iter()
            .any(|m| m.first_hit(statements.iter().take(k).copied()).is_some());
        forbidden_hit.insert(k, hit);
    }

    let mut forbidden_channels: BTreeMap<String, usize> = BTreeMap::new();
    for item in fused.iter().take(CHANNEL_K) {
        if matchers
            .forbidden
            .iter()
            .any(|m| m.matches(&item.statement))
        {
            for c in &item.channels {
                *forbidden_channels.entry(c.clone()).or_insert(0) += 1;
            }
        }
    }

    let mut channel_results = BTreeMap::new();
    let mut hit_channels = Vec::new();
    for run in channels {
        let refs: Vec<&str> = run.statements.iter().map(String::as_str).collect();
        let ranks: Vec<Option<usize>> = matchers
            .gold
            .iter()
            .map(|m| m.first_hit(refs.iter().copied()))
            .collect();
        let best = ranks.iter().flatten().min().copied();
        if best.is_some() {
            hit_channels.push(run.name.to_owned());
        }
        channel_results.insert(
            run.name.to_owned(),
            ChannelResult {
                size: run.statements.len(),
                best_rank: best,
                recall_at_k: recall_at(&ranks, CHANNEL_K),
            },
        );
    }

    let exclusive_channel = (hit_channels.len() == 1).then(|| hit_channels[0].clone());

    QuestionResult {
        id: q.id.clone(),
        kind: q.kind,
        lang: q.lang,
        query: q.query.clone(),
        status,
        replay,
        gold_total,
        forbidden_total: matchers.forbidden.len(),
        never_extracted_total: matchers.never_extracted.len(),
        leaked,
        gold_ranks,
        gold_labels: matchers.gold.iter().map(Matcher::describe).collect(),
        recall,
        recall_any,
        reciprocal_rank,
        forbidden_hit,
        forbidden_channels,
        returned: fused.len(),
        channels: channel_results,
        exclusive_channel,
        top: fused.iter().take(TOP_SAMPLE).cloned().collect(),
    }
}

/// Recall@k: how many golds landed within the top k.
///
/// Returning `NaN` when gold is empty ("should retrieve nothing" questions) is
/// far too likely to blow up downstream, so this returns 1.0 and the
/// aggregation layer excludes such questions from the Recall statistics.
fn recall_at(ranks: &[Option<usize>], k: usize) -> f64 {
    if ranks.is_empty() {
        return 1.0;
    }
    let hits = ranks.iter().flatten().filter(|r| **r < k).count();
    hits as f64 / ranks.len() as f64
}

// ══════════════════════════════════════════════════════════
//  Aggregation
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct ChannelAggregate {
    /// How many questions this path found the gold for
    pub covered: usize,
    /// How many questions **only** this path found the gold for — the direct
    /// measure of the path's irreplaceability
    pub exclusive: usize,
    /// The macro average of this path's own Recall@5
    pub recall_at_k: f64,
    /// Average number of entries retrieved
    pub avg_size: f64,
    /// How many forbidden entries this path sent into the top 5 — attribution
    /// for the noise
    pub forbidden_contributions: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Aggregate {
    pub label: String,
    /// Total questions in this group
    pub total: usize,
    /// Questions counted toward Recall / MRR (excluding golds that never landed
    /// and "should retrieve nothing" questions)
    pub scored: usize,
    /// Questions counted toward the per-path statistics. Time-replay questions
    /// do not use the four retrieval paths and must be removed from the
    /// denominator, otherwise every path's coverage is diluted for no reason
    pub channel_scored: usize,
    /// Average number of entries returned. If this is non-zero on "should
    /// retrieve nothing" questions, the retriever **cannot abstain**
    pub avg_returned: f64,
    pub gold_missing: usize,
    pub gold_inactive: usize,
    /// The macro average of k → Recall@k
    pub recall: BTreeMap<usize, f64>,
    /// The macro average of recall with no rank limit
    pub recall_any: f64,
    pub mrr: f64,
    /// The average rank (one-based) when a gold is hit. Only counts the ones
    /// actually recalled
    pub avg_gold_rank: f64,
    /// Among questions that have forbidden entries, the proportion where a
    /// distractor slipped into the top k
    pub forbidden_rate: BTreeMap<usize, f64>,
    pub forbidden_questions: usize,
    /// How many questions declare "should never have been extracted"
    /// assertions
    pub leak_questions: usize,
    /// How many of those actually leaked — **the only lower-is-better metric on
    /// the extraction side**.
    ///
    /// It is orthogonal to Recall: Recall only ever rewards "extract more", and
    /// extracting more is precisely the direction `docs/memory-content.md` §2③
    /// judges harmful. Without this column, the "never extract this" criterion
    /// has nowhere to land anywhere in the eval
    pub extraction_leaks: usize,
    pub channels: BTreeMap<String, ChannelAggregate>,
}

/// Macro-average aggregation.
///
/// A macro average (each question weighted equally) rather than a micro
/// average: the questions in a suite have different numbers of golds, and a
/// micro average would implicitly give a question with 3 golds triple weight.
#[must_use]
pub fn aggregate(label: impl Into<String>, results: &[&QuestionResult]) -> Aggregate {
    let total = results.len();

    // Recall / MRR only look at questions that have a gold and whose gold
    // really landed in the store
    let scorable: Vec<&&QuestionResult> = results
        .iter()
        .filter(|r| r.status == Status::Scored && r.gold_total > 0)
        .collect();

    let mut recall = BTreeMap::new();
    for k in K_VALUES {
        let v = mean(
            scorable
                .iter()
                .map(|r| r.recall.get(&k).copied().unwrap_or(0.0)),
        );
        recall.insert(k, v);
    }
    let recall_any = mean(scorable.iter().map(|r| r.recall_any));
    let mrr = mean(scorable.iter().map(|r| r.reciprocal_rank));
    let avg_gold_rank = mean(
        scorable
            .iter()
            .flat_map(|r| r.gold_ranks.iter().flatten())
            .map(|rank| *rank as f64 + 1.0),
    );

    // Only questions that actually declare forbidden entries count toward the
    // false-recall rate, otherwise the denominator is diluted by a pile of
    // questions that can never hit, and the number looks good while meaning
    // nothing
    let forbidden_pool: Vec<&&QuestionResult> =
        results.iter().filter(|r| r.forbidden_total > 0).collect();

    let mut forbidden_rate = BTreeMap::new();
    for k in K_VALUES {
        let v = mean(
            forbidden_pool
                .iter()
                .map(|r| f64::from(u8::from(r.forbidden_hit.get(&k).copied().unwrap_or(false)))),
        );
        forbidden_rate.insert(k, v);
    }

    // Time replay goes through `facts_as_of` and never issues the four-path
    // queries at all — counting it in the per-path denominator would saddle
    // every path with a pile of undeserved "did not find it"
    let channel_pool: Vec<&&QuestionResult> =
        scorable.iter().filter(|r| !r.replay).copied().collect();

    let mut channels = BTreeMap::new();
    for name in CHANNELS {
        let covered = channel_pool
            .iter()
            .filter(|r| r.channels.get(name).is_some_and(|c| c.best_rank.is_some()))
            .count();
        let exclusive = channel_pool
            .iter()
            .filter(|r| r.exclusive_channel.as_deref() == Some(name))
            .count();
        let recall_at_k = mean(
            channel_pool
                .iter()
                .map(|r| r.channels.get(name).map_or(0.0, |c| c.recall_at_k)),
        );
        let avg_size = mean(
            results
                .iter()
                .filter(|r| !r.channels.is_empty())
                .map(|r| r.channels.get(name).map_or(0.0, |c| c.size as f64)),
        );
        let forbidden_contributions: usize = results
            .iter()
            .map(|r| r.forbidden_channels.get(name).copied().unwrap_or(0))
            .sum();
        channels.insert(
            name.to_owned(),
            ChannelAggregate {
                covered,
                exclusive,
                recall_at_k,
                avg_size,
                forbidden_contributions,
            },
        );
    }

    Aggregate {
        label: label.into(),
        total,
        scored: scorable.len(),
        channel_scored: channel_pool.len(),
        avg_returned: mean(results.iter().map(|r| r.returned as f64)),
        gold_missing: results
            .iter()
            .filter(|r| r.status == Status::GoldMissing)
            .count(),
        gold_inactive: results
            .iter()
            .filter(|r| r.status == Status::GoldInactive)
            .count(),
        recall,
        recall_any,
        mrr,
        avg_gold_rank,
        forbidden_rate,
        forbidden_questions: forbidden_pool.len(),
        leak_questions: results
            .iter()
            .filter(|r| r.never_extracted_total > 0)
            .count(),
        extraction_leaks: results.iter().map(|r| r.leaked.len()).sum(),
        channels,
    }
}

fn mean<I: Iterator<Item = f64>>(it: I) -> f64 {
    let mut n = 0usize;
    let mut sum = 0.0;
    for v in it {
        sum += v;
        n += 1;
    }
    if n == 0 { 0.0 } else { sum / n as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{Lang, Question, QuestionKind};

    fn item(id: &str, statement: &str, channels: &[&str]) -> ScoredItem {
        ScoredItem {
            fact_id: id.into(),
            statement: statement.into(),
            channels: channels.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn question(gold: &[&[&str]], forbidden: &[&[&str]]) -> Question {
        Question {
            id: "q".into(),
            kind: QuestionKind::ChineseSemantic,
            lang: Lang::Zh,
            query: "问".into(),
            domain: None,
            as_of: None,
            gold: gold
                .iter()
                .map(|g| g.iter().map(|s| (*s).to_owned()).collect())
                .collect(),
            forbidden: forbidden
                .iter()
                .map(|g| g.iter().map(|s| (*s).to_owned()).collect())
                .collect(),
            never_extracted: vec![],
            note: String::new(),
        }
    }

    fn run(name: &'static str, statements: &[&str]) -> ChannelRun {
        ChannelRun {
            name,
            statements: statements.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn recall_and_mrr_track_rank() {
        let q = question(&[&["RustFS"]], &[]);
        let m = QuestionMatchers::of(&q);
        let fused = vec![
            item("a", "别的事实", &["bm25"]),
            item("b", "对象存储用 RustFS", &["bm25", "vector"]),
        ];
        let r = score(&q, &m, &fused, &[], Status::Scored, false, vec![]);
        assert_eq!(
            r.recall[&1], 0.0,
            "the gold ranks second, so Recall@1 should be 0"
        );
        assert_eq!(r.recall[&5], 1.0);
        assert!(
            (r.reciprocal_rank - 0.5).abs() < 1e-9,
            "the reciprocal of rank 2 should be 0.5, got {}",
            r.reciprocal_rank
        );
    }

    #[test]
    fn recall_is_a_fraction_over_multiple_golds() {
        let q = question(&[&["甲"], &["乙"]], &[]);
        let m = QuestionMatchers::of(&q);
        let fused = vec![item("a", "只有甲", &["bm25"])];
        let r = score(&q, &m, &fused, &[], Status::Scored, false, vec![]);
        assert!(
            (r.recall[&5] - 0.5).abs() < 1e-9,
            "hitting one of two golds should be 0.5, got {}",
            r.recall[&5]
        );
    }

    #[test]
    fn forbidden_hit_respects_k() {
        let q = question(&[&["甲"]], &[&["MinIO"]]);
        let m = QuestionMatchers::of(&q);
        let fused = vec![
            item("a", "甲", &[]),
            item("b", "c", &[]),
            item("c", "d", &[]),
            item("d", "e", &[]),
            item("e", "f", &[]),
            item("f", "旧方案是 MinIO", &[]),
        ];
        let r = score(&q, &m, &fused, &[], Status::Scored, false, vec![]);
        assert!(
            !r.forbidden_hit[&5],
            "the distractor ranks sixth, so @5 must not count as a hit"
        );
        assert!(
            r.forbidden_hit[&10],
            "the distractor is within the top 10, so @10 should count as a hit"
        );
    }

    #[test]
    fn exclusive_channel_is_detected() {
        let q = question(&[&["RRF"]], &[]);
        let m = QuestionMatchers::of(&q);
        let channels = vec![
            run("bm25", &["融合用 RRF"]),
            run("vector", &["毫不相干"]),
            run("graph", &[]),
            run("recency", &["最近的一条"]),
        ];
        let r = score(&q, &m, &[], &channels, Status::Scored, false, vec![]);
        assert_eq!(
            r.exclusive_channel.as_deref(),
            Some("bm25"),
            "when only BM25 found it that must be detectable — this is the \
             basis for deciding whether a path can be cut"
        );
    }

    #[test]
    fn no_exclusive_channel_when_several_hit() {
        let q = question(&[&["RRF"]], &[]);
        let m = QuestionMatchers::of(&q);
        let channels = vec![run("bm25", &["RRF"]), run("vector", &["RRF"])];
        let r = score(&q, &m, &[], &channels, Status::Scored, false, vec![]);
        assert_eq!(r.exclusive_channel, None);
    }

    #[test]
    fn aggregate_excludes_gold_missing_from_recall() {
        let q = question(&[&["甲"]], &[]);
        let m = QuestionMatchers::of(&q);
        let good = score(
            &q,
            &m,
            &[item("a", "甲", &[])],
            &[],
            Status::Scored,
            false,
            vec![],
        );
        let broken = score(&q, &m, &[], &[], Status::GoldMissing, false, vec![]);
        let agg = aggregate("all", &[&good, &broken]);
        assert_eq!(agg.total, 2);
        assert_eq!(
            agg.scored, 1,
            "a question whose gold never landed must not drag the retrieval \
             metrics down"
        );
        assert_eq!(agg.gold_missing, 1);
        assert!(
            (agg.recall[&5] - 1.0).abs() < 1e-9,
            "got {}",
            agg.recall[&5]
        );
    }

    #[test]
    fn aggregate_of_nothing_is_zero_not_nan() {
        let agg = aggregate("empty", &[]);
        assert_eq!(agg.mrr, 0.0);
        assert!(
            agg.recall.values().all(|v| v.is_finite()),
            "a NaN would poison the entire report"
        );
    }
}

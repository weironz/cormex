//! 指标：Recall@k、MRR、逐路贡献、误召率，以及按题型 / 语种的分组聚合。
//!
//! # 为什么必须分组
//!
//! 总分掩盖问题。「整体 Recall@5 = 0.72」看起来还行，但如果它是
//! 「专名精确 0.95 + 中文语义 0.30」平均出来的，真正该修的是分词与向量那一路。
//! 所以所有指标都按题型与语种各出一份。
//!
//! # 为什么必须有逐路贡献
//!
//! 四路召回里哪一路该保留、哪一路该换成融合后的 decay，只能靠
//! 「有多少题**只有**这一路捞到 gold」来裁决。没有这个数字，
//! roadmap 里「时间近因召回路定案」那条就永远只能靠嘴仗。

use std::collections::BTreeMap;

use serde::Serialize;

use crate::matcher::{Matcher, QuestionMatchers};
use crate::suite::{Lang, Question, QuestionKind};

/// 报告的 k 取值。1 看「第一条对不对」，5 看实际注入窗口，10 看召回上限。
pub const K_VALUES: [usize; 3] = [1, 5, 10];

/// 逐路诊断时用的 k —— 与融合侧的主指标对齐。
pub const CHANNEL_K: usize = 5;

/// 各路的稳定顺序，报告与 JSON 都按它排。
pub const CHANNELS: [&str; 5] = ["bm25", "vector", "graph", "recency", "episode"];

/// 一道题是否可计分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// 正常计分
    Scored,
    /// gold 压根没落进库 —— 这是**题目或抽取管线**的问题，不是检索的问题。
    /// 混进检索指标里会把抽取的锅算到检索头上，所以单独统计。
    GoldMissing,
    /// gold 在 `facts` 里但已失效，而这道题又不是时间回放题 —— 题目标错了
    GoldInactive,
}

/// 检索器返回的一条结果（融合后、已按预算截断）。
#[derive(Debug, Clone, Serialize)]
pub struct ScoredItem {
    pub fact_id: String,
    pub statement: String,
    /// 命中它的召回路
    pub channels: Vec<String>,
}

/// 一路召回的原始结果（未融合），用于逐路诊断。
#[derive(Debug, Clone)]
pub struct ChannelRun {
    pub name: &'static str,
    /// 按该路自身相关性排好序的 statement
    pub statements: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelResult {
    /// 该路召回了多少条
    pub size: usize,
    /// gold 在该路内的最佳名次（从 0 起）；`None` = 这一路完全没捞到
    pub best_rank: Option<usize>,
    /// 该路自己的 Recall@5
    pub recall_at_k: f64,
}

/// 一道题的完整结果。
#[derive(Debug, Clone, Serialize)]
pub struct QuestionResult {
    pub id: String,
    pub kind: QuestionKind,
    pub lang: Lang,
    pub query: String,
    pub status: Status,
    /// 走的是 `retrieve_as_of`（系统时间回放）还是四路召回
    pub replay: bool,
    pub gold_total: usize,
    /// 这道题写了几条 forbidden。为 0 的题不计入误召率，否则分母被稀释
    pub forbidden_total: usize,
    /// 这道题写了几条「不该被抽出来」的断言
    pub never_extracted_total: usize,
    /// 其中实际在语料快照里找到了的 —— **抽取侧的误抽**。
    ///
    /// 与 `forbidden_hit` 不是一回事：那个问「排没排进前 k」，这个问
    /// 「它压根在不在库里」。抽取 prompt 的「绝对不抽」只在这一列上留痕
    pub leaked: Vec<String>,
    /// 每条 gold 在融合结果里的名次
    pub gold_ranks: Vec<Option<usize>>,
    /// gold 的描述，与 `gold_ranks` 等长 —— 报告里要指出「哪一条没召回」
    pub gold_labels: Vec<String>,
    /// k → Recall@k
    pub recall: BTreeMap<usize, f64>,
    /// 不限名次的召回率 —— 把「根本没捞到」与「捞到了但排太后」分开。
    /// 两者的修法完全不同：前者要动召回，后者要动排序
    pub recall_any: f64,
    /// 首个 gold 名次的倒数；一条都没召回则为 0
    pub reciprocal_rank: f64,
    /// k → top-k 里是否混入了 forbidden
    pub forbidden_hit: BTreeMap<usize, bool>,
    /// 混进 top-5 的 forbidden 条目分别是被哪几路捞上来的。
    /// 「误召是谁造成的」只有这个数字能回答 —— 没有它，噪声只能整体归咎于「融合」
    pub forbidden_channels: BTreeMap<String, usize>,
    /// 融合结果的条数
    pub returned: usize,
    pub channels: BTreeMap<String, ChannelResult>,
    /// 若只有一路捞到了 gold，是哪一路 —— 判断某路该不该保留的直接依据
    pub exclusive_channel: Option<String>,
    /// 融合结果的前几条，供人工看 bad case
    pub top: Vec<ScoredItem>,
}

/// 报告里展示多少条 top 结果。
const TOP_SAMPLE: usize = 5;

/// 给一道题打分。
///
/// `fused` 是检索器的最终输出（已按预算截断）；`channels` 是四路的原始召回，
/// 只用于诊断，不参与主指标。`status` 与 `leaked` 由调用方在 ingest 后
/// 根据语料快照判定 —— 它们都是**抽取侧**的读数，检索一步都还没跑就已经确定。
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

    // MRR 取**首个**命中 gold 的名次倒数，与信息检索的标准定义一致
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

/// Recall@k：有多少条 gold 落在了前 k 名里。
///
/// gold 为空（「应当召回不到」题）时返回 `NaN` 的做法太容易在下游炸开，
/// 这里返回 1.0 并由聚合层把这类题排除在 Recall 统计之外。
fn recall_at(ranks: &[Option<usize>], k: usize) -> f64 {
    if ranks.is_empty() {
        return 1.0;
    }
    let hits = ranks.iter().flatten().filter(|r| **r < k).count();
    hits as f64 / ranks.len() as f64
}

// ══════════════════════════════════════════════════════════
//  聚合
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, Serialize)]
pub struct ChannelAggregate {
    /// 这一路捞到 gold 的题数
    pub covered: usize,
    /// **只有**这一路捞到 gold 的题数 —— 该路不可替代性的直接度量
    pub exclusive: usize,
    /// 该路单独的 Recall@5 宏平均
    pub recall_at_k: f64,
    /// 平均召回条数
    pub avg_size: f64,
    /// 这一路把多少个 forbidden 条目送进了 top-5 —— 噪声的归因
    pub forbidden_contributions: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Aggregate {
    pub label: String,
    /// 该组的总题数
    pub total: usize,
    /// 计入 Recall / MRR 的题数（排除 gold 未落库与「应当召回不到」）
    pub scored: usize,
    /// 计入逐路统计的题数。时间回放题不走四路召回，必须从分母里剔除，
    /// 否则每一路的覆盖率都被凭空稀释
    pub channel_scored: usize,
    /// 平均返回条数。「应当召回不到」的题上它不为 0，就说明检索器**无法弃权**
    pub avg_returned: f64,
    pub gold_missing: usize,
    pub gold_inactive: usize,
    /// k → Recall@k 的宏平均
    pub recall: BTreeMap<usize, f64>,
    /// 不限名次的召回率宏平均
    pub recall_any: f64,
    pub mrr: f64,
    /// gold 命中时的平均名次（从 1 起）。只统计确实召回到的
    pub avg_gold_rank: f64,
    /// 有 forbidden 的题里，top-k 混入干扰项的比例
    pub forbidden_rate: BTreeMap<usize, f64>,
    pub forbidden_questions: usize,
    /// 写了「不该被抽出来」断言的题数
    pub leak_questions: usize,
    /// 其中实际泄漏的条数 —— **抽取侧唯一的越少越好指标**。
    ///
    /// 它与 Recall 正交：Recall 只会奖励「抽得多」，而抽得多恰恰是
    /// `docs/memory-content.md` §二③ 判定为有害的方向。没有这一列，
    /// 「绝对不抽」这条判据在整套评测里没有任何落点
    pub extraction_leaks: usize,
    pub channels: BTreeMap<String, ChannelAggregate>,
}

/// 宏平均聚合。
///
/// 用宏平均（每题等权）而非微平均：题集里各题的 gold 条数不一样，
/// 微平均会让「gold 有 3 条」的题隐性地占 3 倍权重。
#[must_use]
pub fn aggregate(label: impl Into<String>, results: &[&QuestionResult]) -> Aggregate {
    let total = results.len();

    // Recall / MRR 只看「有 gold 且 gold 确实落库了」的题
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

    // 只有真写了 forbidden 的题才计入误召率，否则分母被一堆恒不命中的题稀释，
    // 数字会好看得毫无意义
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

    // 时间回放走 `facts_as_of`，根本不发四路查询 —— 把它算进逐路分母，
    // 每一路都会平白背上一堆「没捞到」
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
        assert_eq!(r.recall[&1], 0.0, "gold 排第二，Recall@1 应为 0");
        assert_eq!(r.recall[&5], 1.0);
        assert!(
            (r.reciprocal_rank - 0.5).abs() < 1e-9,
            "排名 2 的倒数应为 0.5，实际 {}",
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
            "两条 gold 命中一条应为 0.5，实际 {}",
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
        assert!(!r.forbidden_hit[&5], "干扰项排第 6，@5 不该算命中");
        assert!(r.forbidden_hit[&10], "干扰项在前 10 名内，@10 应当命中");
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
            "只有 BM25 捞到时必须能识别出来 —— 这是判断某路能否砍掉的依据"
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
        let agg = aggregate("全部", &[&good, &broken]);
        assert_eq!(agg.total, 2);
        assert_eq!(agg.scored, 1, "gold 未落库的题不该拉低检索指标");
        assert_eq!(agg.gold_missing, 1);
        assert!(
            (agg.recall[&5] - 1.0).abs() < 1e-9,
            "实际 {}",
            agg.recall[&5]
        );
    }

    #[test]
    fn aggregate_of_nothing_is_zero_not_nan() {
        let agg = aggregate("空", &[]);
        assert_eq!(agg.mrr, 0.0);
        assert!(
            agg.recall.values().all(|v| v.is_finite()),
            "NaN 会毒化整张报告"
        );
    }
}

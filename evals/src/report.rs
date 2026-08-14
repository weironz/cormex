//! 报告：机器可读的 JSON（供 CI 比对）与人类可读的文本。
//!
//! 两份都要。JSON 是给回归门用的 —— CI 里把本次与基线的
//! `by_kind[*].recall["5"]` 比一比，跌了就红。文本是给人看的 ——
//! 一个只会吐 JSON 的评测工具，没人会去读它。

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

/// 跑这一次用的全部旋钮。基线数字离开这些参数就没有意义。
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
    /// 是否关掉了域加权（`--ignore-domain`）
    pub domain_boost_off: bool,
    /// 弃权阈值。扫描曲线全靠它区分各次运行
    pub abstain_below: f64,
    /// 时间近因的形态：`channel` / `decay(强度,半衰期)` / `off`
    pub recency_mode: String,
    /// episodes 那一路开没开
    pub episode_channel: bool,
    /// 单路强命中补偿权重
    pub peak_bonus: f64,
    /// 语义地板（余弦距离上限）；`None` = 没设
    pub semantic_floor: Option<f64>,
    /// 回放走排序版还是全貌版
    pub replay_ranked: bool,
    pub schema: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CorpusInfo {
    pub ingest: IngestStats,
    /// 库里最终有多少条事实
    pub facts_total: usize,
    /// 其中仍然有效的
    pub facts_active: usize,
    pub entities: i64,
    /// 落进 `episode_tool_calls` 的行数
    pub tool_calls: i64,
    /// 跨域连接扫描的结果。`None` = 这次没跑扫描。
    ///
    /// 边本身要落进报告，不能只留个条数：噪声边的危害是在图遍历里传染，
    /// 「写了 12 条」这个数字对判断噪声毫无帮助，得把 12 条读一遍。
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
    /// CI 回归门要看的那几个数字。
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
//  文本渲染
// ══════════════════════════════════════════════════════════

/// 报告里列出多少道最差的题。
const WORST_SAMPLE: usize = 12;

#[must_use]
pub fn render_text(report: &Report) -> String {
    let mut s = String::with_capacity(8192);

    let _ = writeln!(s, "# Cortex 检索评测报告");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "题集：{} {}　　{} 场景 / {} 轮对话 / {} 条铺垫事实 / {} 道题",
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
        "配置：mode={}　embedder={}　RRF k={}　召回宽度 bm25/vector/graph/recency = {}/{}/{}/{}",
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
        "　　　图遍历 {} 跳　近因回看 {} 天　episodes 宽度 {}　context {} token　注入预算 {} token　域加权 {}",
        r.graph_hops,
        r.recent_days,
        r.width_episode,
        r.context_window,
        r.budget_tokens,
        if r.domain_boost_off { "关" } else { "开" }
    );
    // 这一行是 A/B 的身份证：不打出来，两份报告放在一起就分不清谁是谁
    let _ = writeln!(
        s,
        "旋钮：弃权阈值 {:.4}　语义地板 {}　近因形态 {}　episodes 路 {}　强命中补偿 {}　回放 {}",
        r.abstain_below,
        r.semantic_floor
            .map_or_else(|| "无".to_owned(), |v| format!("{v:.2}")),
        r.recency_mode,
        if r.episode_channel { "开" } else { "关" },
        r.peak_bonus,
        if r.replay_ranked {
            "排序版"
        } else {
            "全貌版"
        }
    );
    let c = &report.corpus;
    let _ = writeln!(
        s,
        "语料：候选 {} → 写入 {}（重复丢弃 {}　被取代 {}　矛盾标记 {}）　实体 {}　最终事实 {}（有效 {}）",
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
        // 轨迹落了多少行必须单独打出来。工具经验题全挂时有两种可能 ——
        // 轨迹没进库，或者进了库但没人读 —— 只有这个数字能把它们分开
        let _ = writeln!(
            s,
            "工具轨迹：`episode_tool_calls` 落库 {} 行（抽取器读没读它，看下面「工具经验」那一行的 gold 缺失数）",
            c.tool_calls
        );
    }
    if let Some(cl) = &c.cross_links {
        let _ = writeln!(
            s,
            "跨域连接：判定 {}　扫描 {} 条事实 → 候选 {} 对 → 接受 {} 对（度数/批次截掉 {}）→ 写入 {} 行",
            cl.judge, cl.scanned, cl.candidates, cl.accepted, cl.degree_capped, cl.written
        );
    }
    let _ = writeln!(s);

    if let Some(cl) = &c.cross_links
        && !cl.links.is_empty()
    {
        // 边本身必须打出来。「写了 N 条」判断不了噪声，读一遍才行
        let _ = writeln!(s, "## 本次新增的跨域连接");
        let _ = writeln!(s);
        for l in &cl.links {
            let _ = writeln!(s, "- {l}");
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "## 总体");
    let _ = writeln!(s);
    let _ = write!(
        s,
        "{}",
        render_table(std::slice::from_ref(&report.overall), "口径")
    );
    let _ = writeln!(s);

    let _ = writeln!(s, "## 分题型");
    let _ = writeln!(s);
    let _ = write!(s, "{}", render_table(&report.by_kind, "题型"));
    let _ = writeln!(s);

    let _ = writeln!(s, "## 分语种");
    let _ = writeln!(s);
    let _ = write!(s, "{}", render_table(&report.by_lang, "语种"));
    let _ = writeln!(s);
    let _ = writeln!(s, "{TABLE_LEGEND}");

    let _ = writeln!(s, "## 逐路贡献（融合前的原始召回）");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "「独占」= 只有这一路捞到了 gold 的题数。**它才是某一路该不该保留的依据** ——"
    );
    let _ = writeln!(s, "覆盖数高但独占数为 0，说明这一路完全可以被别的路替代。");
    let _ = writeln!(s);
    let _ = write!(s, "{}", render_channels(&report.overall));
    let _ = writeln!(s);

    let _ = writeln!(s, "## 逐路贡献 · 分题型独占数");
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
        let _ = writeln!(s, "## 题目问题（不计入检索指标）");
        let _ = writeln!(s);
        let _ = writeln!(
            s,
            "gold 压根没落进库 = **抽取或题目**的问题，不是检索的问题。混进指标会让检索背锅。"
        );
        let _ = writeln!(s);
        for q in &broken {
            let _ = writeln!(
                s,
                "- `{}` [{}] {:?}　gold：{}",
                q.id,
                q.kind.label(),
                q.status,
                q.gold_labels.join(" ／ ")
            );
        }
        let _ = writeln!(s);
    }

    let _ = writeln!(s, "## 最差的题");
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
            "- `{}` [{}/{}] RR={:.2}　gold 名次 {hit}　命中路 {}",
            q.id,
            q.kind.label(),
            q.lang.label(),
            q.reciprocal_rank,
            if channels.is_empty() {
                "无".to_owned()
            } else {
                channels.join(" ")
            }
        );
        let _ = writeln!(s, "  查询：{}", q.query);
        if let Some((i, label)) = q
            .gold_ranks
            .iter()
            .position(Option::is_none)
            .and_then(|i| q.gold_labels.get(i).map(|l| (i, l)))
        {
            let _ = writeln!(s, "  漏召 gold[{i}]：{label}");
        }
    }

    s
}

/// 抽取侧的断言。
///
/// 检索侧的十几项指标里没有一项能回答「这条该不该被抽出来」——
/// 它们的分母全都是「已经在库里的事实」。抽取 prompt 里
/// 「绝对不抽」与「纠正必抽」两节改完之后回归门十八项全部持平，
/// 根本原因就是这套评测当时只有检索侧的尺子。这一段是另一把。
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
    let _ = writeln!(s, "## 抽取侧：不该被抽出来的东西");
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "比对的是**语料快照**而不是检索结果 —— 排第几名无关紧要，**存在**本身就是失败。"
    );
    let _ = writeln!(s);
    let _ = writeln!(
        s,
        "{} 道题声明了 {total} 条断言，泄漏 {hit} 条。",
        declared.len()
    );
    let _ = writeln!(s);
    for q in &leaked {
        for l in &q.leaked {
            let _ = writeln!(s, "- ✗ `{}` 泄漏：{l}", q.id);
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
        "| {head} | 题数 | 计分 | R@1 | R@5 | R@10 | R@∞ | MRR | 命中均名次 | 误召@5 | 平均注入条数 | gold 缺失 |"
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

/// 表格的读法。写一次，三张表共用 —— 指标不解释就没人看得懂该修哪里。
const TABLE_LEGEND: &str = "> `R@∞` = 不限名次的召回率，与 `R@5` 的差额就是**排序**欠的债；\
差额大该修融合，差额小该修召回。\n\
> `平均注入条数` 在「应召不到」那一行不为 0，就说明**检索器无法弃权** —— \
没有答案时它照样会塞一整块记忆进 prompt。\n";

fn render_channels(overall: &Aggregate) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "| 召回路 | 覆盖题数 | 独占题数 | 该路 R@5 | 平均召回条数 | 送进 top5 的干扰项 |"
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
        "（分母 = 走四路召回且可计分的题数 {}；时间回放题不发四路查询，已剔除）",
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

    let _ = write!(s, "| 题型 |");
    for n in &names {
        let _ = write!(s, " {n} 覆盖/独占 |");
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

/// 题型 / 语种的稳定排序，保证两次跑出来的报告能直接 diff。
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
        let table = render_table(&[aggregate("空", &[])], "口径");
        assert!(
            !table.contains("NaN"),
            "NaN 会一路渗进 CI 比对，必须在源头挡住：{table}"
        );
    }

    #[test]
    fn gate_metrics_cover_every_kind() {
        // CI 只比总分是不够的：某一题型崩了而总分微跌，回归门就放过去了
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
            overall: aggregate("全部", &[]),
            by_kind: vec![aggregate("中文语义", &[])],
            by_lang: vec![],
            questions: vec![],
        };
        let gate = report.gate_metrics();
        assert!(gate.contains_key("overall.recall@5"));
        assert!(gate.contains_key("kind.中文语义.recall@5"));
    }
}

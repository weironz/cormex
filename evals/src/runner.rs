//! 把题集、语料、检索器、指标串成一次完整的评测。

use anyhow::{Context, Result};

use crate::harness::{
    Checkpoints, CrossLinkOptions, EvalDb, EvalRetriever, ExtractMode, FactRow, RetrievalConfig,
    build_cross_links, build_extractor, default_embedder, ingest_suite,
};
use crate::matcher::{Matcher, QuestionMatchers};
use crate::metrics::{Aggregate, QuestionResult, Status, aggregate, score};
use crate::report::{CorpusInfo, Report, RunInfo, SuiteInfo, kind_order, lang_order};
use crate::suite::{Question, QuestionKind, Suite};
use cortex_memory::retrieval::{DEFAULT_ABSTAIN_BELOW, RecencyMode};

/// 把近因形态写成报告里那一行字。
fn describe_recency(mode: RecencyMode) -> String {
    match mode {
        RecencyMode::Channel => "channel".to_owned(),
        RecencyMode::Off => "off".to_owned(),
        RecencyMode::Decay {
            strength,
            half_life_days,
        } => format!("decay(强度 {strength}，半衰期 {half_life_days} 天)"),
    }
}

pub struct RunOptions {
    pub database_url: String,
    pub mode: ExtractMode,
    pub keep_schema: bool,
    /// 实体向量近似归并的阈值。`None` 用生产默认值
    pub entity_threshold: Option<f32>,
    pub retrieval: RetrievalConfig,
    /// 只跑 id 里含此子串的题（调 bad case 用）
    pub only: Option<String>,
    /// 不把题目的 domain 传给检索器，等价于关掉域加权。
    ///
    /// `Retriever` 没有暴露 `domain_boost` 的 setter，但传 `None` 时
    /// `apply_domain_boost` 直接返回 —— 这就是现成的 A/B 开关，
    /// 用来回答「域加权到底帮了还是害了跨域题」。
    pub ignore_domain: bool,
    /// 灌完语料后跑一遍跨域连接扫描。默认关 —— 它会往库里写额外的边，
    /// 不显式要求就不该改变基线
    pub cross_link: CrossLinkOptions,
}

pub async fn run(suite: &Suite, opts: &RunOptions) -> Result<Report> {
    let db = EvalDb::create(&opts.database_url).await?;
    let schema = db.schema().to_owned();

    // 出错也要把临时 schema 清掉，否则调试期间会攒出几十个死 schema
    let outcome = run_inner(&db, suite, opts).await;
    let keep = opts.keep_schema || outcome.is_err();
    db.cleanup(keep).await;

    outcome.map(|mut r| {
        r.run.schema = schema;
        r
    })
}

async fn run_inner(db: &EvalDb, suite: &Suite, opts: &RunOptions) -> Result<Report> {
    let embedder = default_embedder().await?;
    let extractor = build_extractor(embedder.clone(), opts.mode, opts.entity_threshold)?;

    let (ingest, checkpoints) = ingest_suite(db, suite, &extractor, opts.mode).await?;

    // 扫描必须在 ingest 之后、快照之前：写进去的 relates_to 也是 facts，
    // gold 存在性核对要看得见它们（否则拿它们当 gold 的题会被判成 GoldMissing，
    // 而那正好会把「桥没建起来」伪装成「题目坏了」）
    let cross_links = if opts.cross_link.enabled {
        Some(build_cross_links(db, &embedder, opts.cross_link).await?)
    } else {
        None
    };

    let facts = db.snapshot_facts().await?;
    let entities = db.count_entities().await?;
    let tool_calls_in_db = db.count_tool_calls().await?;

    // 题集声明了多少条轨迹，库里就该有多少行。对不上说明轨迹在写入路径上丢了，
    // 而丢了的表现（工具经验题全挂）与「抽取器还没接轨迹」一模一样 ——
    // 不在这里断言，这两件事在报告里永远分不开
    let declared = i64::try_from(suite.tool_call_count()).unwrap_or(i64::MAX);
    anyhow::ensure!(
        tool_calls_in_db == declared,
        "题集声明 {declared} 次工具调用，库里只有 {tool_calls_in_db} 行 —— \
         轨迹没落全，工具经验题的结果不可信"
    );

    let retriever = EvalRetriever::new(embedder, opts.retrieval);

    let mut questions = Vec::with_capacity(suite.questions.len());
    for q in &suite.questions {
        if let Some(filter) = &opts.only
            && !q.id.contains(filter.as_str())
        {
            continue;
        }
        questions
            .push(evaluate_one(db, &retriever, q, &facts, &checkpoints, opts.ignore_domain).await?);
    }

    let refs: Vec<&QuestionResult> = questions.iter().collect();
    let overall = aggregate("全部", &refs);

    let by_kind: Vec<Aggregate> = kind_order()
        .into_iter()
        .map(|kind| {
            let group: Vec<&QuestionResult> = questions.iter().filter(|r| r.kind == kind).collect();
            aggregate(kind.label(), &group)
        })
        .filter(|a| a.total > 0)
        .collect();

    let by_lang: Vec<Aggregate> = lang_order()
        .into_iter()
        .map(|lang| {
            let group: Vec<&QuestionResult> = questions.iter().filter(|r| r.lang == lang).collect();
            aggregate(lang.label(), &group)
        })
        .filter(|a| a.total > 0)
        .collect();

    let cfg = &opts.retrieval;
    Ok(Report {
        suite: SuiteInfo::of(suite),
        run: RunInfo {
            mode: opts.mode.to_string(),
            embedding_model: retriever.embedding_model(),
            rrf_k: cortex_memory::fusion::RRF_K,
            width_bm25: cfg.width.bm25,
            width_vector: cfg.width.vector,
            width_graph: cfg.width.graph,
            width_recency: cfg.width.recent,
            width_episode: cfg.width.episode,
            graph_hops: cfg.width.graph_hops,
            recent_days: cfg.width.recent_days,
            context_window: cfg.context_window,
            budget_tokens: cfg.budget.tokens_for(cfg.context_window),
            domain_boost_off: opts.ignore_domain,
            abstain_below: cfg.abstain_below.unwrap_or(DEFAULT_ABSTAIN_BELOW),
            recency_mode: describe_recency(cfg.recency),
            episode_channel: cfg.episode_channel,
            peak_bonus: cfg.peak_bonus,
            semantic_floor: retriever.semantic_floor(),
            replay_ranked: cfg.replay_ranked,
            schema: String::new(),
        },
        corpus: CorpusInfo {
            ingest,
            facts_total: facts.len(),
            facts_active: facts.iter().filter(|f| f.active).count(),
            entities,
            tool_calls: tool_calls_in_db,
            cross_links,
        },
        overall,
        by_kind,
        by_lang,
        questions,
    })
}

async fn evaluate_one(
    db: &EvalDb,
    retriever: &EvalRetriever,
    q: &Question,
    facts: &[FactRow],
    checkpoints: &Checkpoints,
    ignore_domain: bool,
) -> Result<QuestionResult> {
    let domain = if ignore_domain {
        None
    } else {
        q.domain.as_deref()
    };
    let matchers = QuestionMatchers::of(q);
    let status = gold_status(q, &matchers, facts);
    let leaked = leaks(&matchers, facts);

    // gold 根本没落库时，检索必然是 0 分，跑它只是浪费一次数据库往返。
    // 但仍然要产出一条结果 —— 报告里必须看得见「有几道题是坏的」。
    if status == Status::GoldMissing {
        return Ok(score(
            q,
            &matchers,
            &[],
            &[],
            status,
            q.as_of.is_some(),
            leaked,
        ));
    }

    let (fused, channels, replay) = match &q.as_of {
        Some(name) => {
            let at = checkpoints
                .get(name)
                .copied()
                .with_context(|| format!("题目 {} 引用的 checkpoint「{name}」不存在", q.id))?;
            // 回放走的是**快照内**的三路（同一份 as-of 集合上的 bm25/向量/时间倒序），
            // 与日常四路查的不是同一批数据。把它塞进逐路诊断会让两套口径混在
            // 一张表里，所以这里仍然留空 —— 回放的排序质量由主指标衡量
            (
                retriever.replay(&db.store, &q.query, at).await?,
                Vec::new(),
                true,
            )
        }
        None => {
            let fused = retriever.retrieve(&db.store, &q.query, domain).await?;
            let channels = retriever.per_channel(&db.store, &q.query).await?;
            (fused, channels, false)
        }
    };

    Ok(score(
        q, &matchers, &fused, &channels, status, replay, leaked,
    ))
}

/// 「不该被抽出来」的断言里，有哪几条实际在语料快照里找到了。
///
/// 比对的是**全部**事实而不是有效事实：失效只是打了个墓碑，行还在、
/// 向量还在（`docs/memory-content.md` §八.5 的 Ghost Vectors）。
/// 「抽出来又失效掉」不等于「没抽」。
fn leaks(matchers: &QuestionMatchers, facts: &[FactRow]) -> Vec<String> {
    matchers
        .never_extracted
        .iter()
        .filter(|m| facts.iter().any(|f| m.matches(&f.statement)))
        .map(Matcher::describe)
        .collect()
}

/// gold 是否真的落进了库。
///
/// 这一步把两类失败分开：**检索没找到**（检索的锅）与
/// **压根没这条事实**（抽取或题目的锅）。不分开的话，抽取管线的问题
/// 会全部记在检索头上，而后者是这套评测唯一要衡量的东西。
fn gold_status(q: &Question, matchers: &QuestionMatchers, facts: &[FactRow]) -> Status {
    if matchers.gold.is_empty() {
        return Status::Scored;
    }

    let mut any_inactive = false;
    for m in &matchers.gold {
        let hits: Vec<&FactRow> = facts.iter().filter(|f| m.matches(&f.statement)).collect();
        if hits.is_empty() {
            return Status::GoldMissing;
        }
        if !hits.iter().any(|f| f.active) {
            any_inactive = true;
        }
    }

    // 时间回放题的 gold 本来就该是已被取代的旧事实，不算标注错误
    if any_inactive && q.kind != QuestionKind::TemporalReplay {
        return Status::GoldInactive;
    }
    Status::Scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::Lang;

    fn fact(statement: &str, active: bool) -> FactRow {
        FactRow {
            id: "01".into(),
            statement: statement.into(),
            domain: None,
            active,
        }
    }

    fn question(kind: QuestionKind, gold: &[&str]) -> Question {
        Question {
            id: "q".into(),
            kind,
            lang: Lang::Zh,
            query: "问".into(),
            domain: None,
            as_of: None,
            gold: vec![gold.iter().map(|s| (*s).to_owned()).collect()],
            forbidden: vec![],
            never_extracted: vec![],
            note: String::new(),
        }
    }

    #[test]
    fn missing_gold_is_not_a_retrieval_failure() {
        let q = question(QuestionKind::ChineseSemantic, &["从未存在"]);
        let m = QuestionMatchers::of(&q);
        assert_eq!(
            gold_status(&q, &m, &[fact("别的事实", true)]),
            Status::GoldMissing,
            "gold 没落库时把责任算给检索，会让抽取的问题永远查不出来"
        );
    }

    #[test]
    fn superseded_gold_flags_a_mislabelled_question() {
        let q = question(QuestionKind::ChineseSemantic, &["MinIO"]);
        let m = QuestionMatchers::of(&q);
        assert_eq!(
            gold_status(&q, &m, &[fact("对象存储先用 MinIO", false)]),
            Status::GoldInactive,
            "非回放题却把已失效的事实当 gold，是题目标错了"
        );
    }

    #[test]
    fn temporal_question_may_target_a_superseded_fact() {
        let q = question(QuestionKind::TemporalReplay, &["MinIO"]);
        let m = QuestionMatchers::of(&q);
        assert_eq!(
            gold_status(&q, &m, &[fact("对象存储先用 MinIO", false)]),
            Status::Scored,
            "回放题的 gold 本来就该是被取代掉的旧事实"
        );
    }

    #[test]
    fn leak_is_measured_against_the_corpus_not_the_ranking() {
        let mut q = question(QuestionKind::ToolExperience, &["--locked"]);
        q.never_extracted = vec![vec!["failed to select a version".into()]];
        let m = QuestionMatchers::of(&q);
        // 这条事实排在第几名无关紧要 —— 它**存在**本身就是抽取侧的失败
        let corpus = [fact(
            "error: failed to select a version for `sqlx-core`",
            true,
        )];
        assert_eq!(
            leaks(&m, &corpus).len(),
            1,
            "命令原始输出被抽成了事实，必须能量到（docs/memory-content.md §二③）"
        );
    }

    #[test]
    fn invalidated_leak_still_counts_as_a_leak() {
        let mut q = question(QuestionKind::ToolExperience, &["--locked"]);
        q.never_extracted = vec![vec!["failed to select a version".into()]];
        let m = QuestionMatchers::of(&q);
        let corpus = [fact("error: failed to select a version for `x`", false)];
        assert_eq!(
            leaks(&m, &corpus).len(),
            1,
            "失效只是加了个墓碑，行和向量都还在 —— 抽出来又失效不等于没抽"
        );
    }

    #[test]
    fn unanswerable_question_is_always_scorable() {
        let mut q = question(QuestionKind::Unanswerable, &["x"]);
        q.gold.clear();
        let m = QuestionMatchers::of(&q);
        assert_eq!(gold_status(&q, &m, &[]), Status::Scored);
    }
}

//! Wire the suite, the corpus, the retriever, and the metrics into one complete
//! eval run.

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

/// Render the recency mode as the line of text that goes into the report.
fn describe_recency(mode: RecencyMode) -> String {
    match mode {
        RecencyMode::Channel => "channel".to_owned(),
        RecencyMode::Off => "off".to_owned(),
        RecencyMode::Decay {
            strength,
            half_life_days,
        } => format!("decay(strength {strength}, half-life {half_life_days}d)"),
    }
}

pub struct RunOptions {
    pub database_url: String,
    pub mode: ExtractMode,
    pub keep_schema: bool,
    /// Threshold for approximate entity-vector merging. `None` uses the
    /// production default
    pub entity_threshold: Option<f32>,
    pub retrieval: RetrievalConfig,
    /// Run only the questions whose id contains this substring (for chasing bad
    /// cases)
    pub only: Option<String>,
    /// Do not pass the question's domain to the retriever, which is equivalent
    /// to turning domain weighting off.
    ///
    /// `Retriever` exposes no setter for `domain_boost`, but with `None`
    /// passed, `apply_domain_boost` returns immediately — that is a ready-made
    /// A/B switch for answering "did domain weighting help or hurt the
    /// cross-domain questions".
    pub ignore_domain: bool,
    /// Run a cross-domain link scan after the corpus is loaded. Off by default —
    /// it writes extra edges into the store, and without an explicit request it
    /// should not move the baseline
    pub cross_link: CrossLinkOptions,
}

pub async fn run(suite: &Suite, opts: &RunOptions) -> Result<Report> {
    let db = EvalDb::create(&opts.database_url).await?;
    let schema = db.schema().to_owned();

    // Clean the temp schema up even on error, otherwise debugging accumulates
    // dozens of dead schemas
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

    // The scan has to run after ingest and before the snapshot: the relates_to
    // edges it writes are facts too, and the gold existence check has to see
    // them (otherwise a question using them as gold would be judged
    // GoldMissing, which would disguise "the bridge was never built" as "the
    // question is broken")
    let cross_links = if opts.cross_link.enabled {
        Some(build_cross_links(db, &embedder, opts.cross_link).await?)
    } else {
        None
    };

    let facts = db.snapshot_facts().await?;
    let entities = db.count_entities().await?;
    let tool_calls_in_db = db.count_tool_calls().await?;

    // However many tool calls the suite declares, the store should hold that
    // many rows. A mismatch means the trace was lost somewhere on the write
    // path, and losing it looks exactly like "the extractor is not wired to
    // traces yet" (all the tool-experience questions fail) — without asserting
    // here, those two would be permanently indistinguishable in the report
    let declared = i64::try_from(suite.tool_call_count()).unwrap_or(i64::MAX);
    anyhow::ensure!(
        tool_calls_in_db == declared,
        "the suite declares {declared} tool calls but the store holds only {tool_calls_in_db} \
         rows -- traces were lost, so the tool-experience results cannot be trusted"
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
    let overall = aggregate("all", &refs);

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

    // When the gold never landed in the store, retrieval is guaranteed to score
    // 0, and running it merely wastes a database round-trip. A result still has
    // to be produced, though — the report must show "how many questions are
    // broken".
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
            let at = checkpoints.get(name).copied().with_context(|| {
                format!(
                    "question {} references checkpoint \"{name}\", which does not exist",
                    q.id
                )
            })?;
            // Replay uses three paths **within the snapshot** (bm25 / vector /
            // reverse-chronological over the same as-of set), which is not the
            // same body of data the everyday four paths query. Feeding it into
            // the per-path diagnostics would mix two different accountings into
            // one table, so it is left empty here — replay's ranking quality is
            // measured by the main metrics
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

/// Which of the "should never have been extracted" assertions were actually
/// found in the corpus snapshot.
///
/// The comparison is against **all** facts, not just active ones: invalidation
/// merely places a tombstone, and the row and the vector both remain (the Ghost
/// Vectors of `docs/memory-content.md` §8.5). "Extracted and then invalidated"
/// is not the same as "never extracted".
fn leaks(matchers: &QuestionMatchers, facts: &[FactRow]) -> Vec<String> {
    matchers
        .never_extracted
        .iter()
        .filter(|m| facts.iter().any(|f| m.matches(&f.statement)))
        .map(Matcher::describe)
        .collect()
}

/// Whether the gold really landed in the store.
///
/// This step separates two kinds of failure: **retrieval did not find it**
/// (retrieval's fault) versus **the fact is not there at all** (extraction's or
/// the question's fault). Without the separation, extraction-pipeline problems
/// would all be charged to retrieval, and retrieval is the only thing this eval
/// exists to measure.
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

    // A time-replay question's gold is supposed to be a superseded old fact, so
    // that is not a labeling error
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
            "charging retrieval when the gold never landed in the store would \
             keep extraction problems permanently undiagnosable"
        );
    }

    #[test]
    fn superseded_gold_flags_a_mislabelled_question() {
        let q = question(QuestionKind::ChineseSemantic, &["MinIO"]);
        let m = QuestionMatchers::of(&q);
        assert_eq!(
            gold_status(&q, &m, &[fact("对象存储先用 MinIO", false)]),
            Status::GoldInactive,
            "a non-replay question using an invalidated fact as gold means the \
             question is mislabelled"
        );
    }

    #[test]
    fn temporal_question_may_target_a_superseded_fact() {
        let q = question(QuestionKind::TemporalReplay, &["MinIO"]);
        let m = QuestionMatchers::of(&q);
        assert_eq!(
            gold_status(&q, &m, &[fact("对象存储先用 MinIO", false)]),
            Status::Scored,
            "a replay question's gold is supposed to be a superseded old fact"
        );
    }

    #[test]
    fn leak_is_measured_against_the_corpus_not_the_ranking() {
        let mut q = question(QuestionKind::ToolExperience, &["--locked"]);
        q.never_extracted = vec![vec!["failed to select a version".into()]];
        let m = QuestionMatchers::of(&q);
        // What rank this fact lands at is irrelevant — its **existence** is
        // itself the failure on the extraction side
        let corpus = [fact(
            "error: failed to select a version for `sqlx-core`",
            true,
        )];
        assert_eq!(
            leaks(&m, &corpus).len(),
            1,
            "raw command output was extracted into a fact, which must be \
             measurable (docs/memory-content.md §2③)"
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
            "invalidation only adds a tombstone; the row and the vector both \
             remain — extracted then invalidated is not the same as never \
             extracted"
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

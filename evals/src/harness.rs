//! The eval execution environment: an isolated database, the real ingest
//! pipeline, the real retriever.
//!
//! # Isolation: a temporary schema
//!
//! An eval pours in hundreds of facts. **It must never contaminate the
//! development database** — one eval run and `memory search` during development
//! returns nothing but eval corpus.
//!
//! The approach is the same as `crates/cortex-store/tests/common/mod.rs`: the
//! connection string is unchanged, only `search_path` is pointed at a freshly
//! created schema, migrations are run inside it, and it ends with
//! `DROP SCHEMA ... CASCADE`.
//!
//! # Why go through the real ingest rather than INSERT facts directly
//!
//! Stuffing facts in directly bypasses jieba tokenization (the `tsv` column),
//! bypasses embedding, bypasses entity resolution, and bypasses contradiction
//! resolution. Those four are exactly what decide whether retrieval can find
//! anything: if `tsv` is wrong the BM25 path is dead; if entities are
//! unresolved graph traversal cannot hop. The eval evaluates **the whole
//! chain**, not four SQL queries.
//!
//! # Two extraction modes
//!
//! | Mode | Extraction | Purpose |
//! |---|---|---|
//! | `seed` | Uses the handwritten candidate facts from the suite, via `write_candidates` | The default. The corpus is fixed, a hundred runs agree, and it can go into CI |
//! | `llm` | Really calls the cheap model, via `ingest` | Measures extraction quality too, but the results drift with the model and are unsuited to a regression gate |
//!
//! Both modes **share** resolution, vectorization, tokenization, and the write
//! path — the only difference is where the candidate facts come from.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use cortex_core::Id;
use cortex_core::config::LlmConfig;
use cortex_llm::LlmClient;
use cortex_memory::embed::{Backend, Embedder, SharedEmbedder, build_embedder};
use cortex_memory::extract::{CandidateFact, EntityRef, ExtractContext, Extractor, ObjectRef};
use cortex_memory::injection::Budget;
use cortex_memory::retrieval::{GRAPH_SEED_LIMIT, RecallWidth, RecencyMode, Retriever};
use cortex_memory::tokenize;
use cortex_store::{NewEpisode, NewEpisodeToolCall, Role, Store, Vector};
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use crate::metrics::{ChannelRun, ScoredItem};
use crate::suite::{Scenario, SeedFact, Suite, parse_date};

const SCHEMA_PREFIX: &str = "cortex_eval_";

/// Schemas left behind by a crash get swept once they are older than this.
/// No indiscriminate sweep — that would delete a schema a sibling process is
/// still using.
const STALE_SCHEMA_AGE_MS: u128 = 30 * 60 * 1000;

pub const DEVICE: &str = "eval-harness";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    /// Use the handwritten candidate facts from the suite
    Seed,
    /// Really call the LLM to extract
    Llm,
}

impl std::fmt::Display for ExtractMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Seed => "seed",
            Self::Llm => "llm",
        })
    }
}

// ══════════════════════════════════════════════════════════
//  The isolated database
// ══════════════════════════════════════════════════════════

pub struct EvalDb {
    pub store: Store,
    /// Keeps its own pool for diagnostic queries — `Store`'s pool is
    /// crate-private, while the eval needs the out-of-band ability to "pull
    /// every fact in the store out and check the golds against them"
    pool: PgPool,
    admin: PgPool,
    schema: String,
}

impl EvalDb {
    /// Create an isolated eval database.
    pub async fn create(database_url: &str) -> Result<Self> {
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(database_url)
            .await
            .with_context(|| "连不上数据库；先跑 `just up`，并确认 .env 里的 DATABASE_URL")?;

        sweep_stale_schemas(&admin).await;

        let schema = unique_schema_name();
        let create = format!("CREATE SCHEMA \"{schema}\"");
        sqlx::query(AssertSqlSafe(create))
            .execute(&admin)
            .await
            .context("建不出临时 schema —— 当前用户可能没有 CREATE 权限")?;

        // `public` stays in search_path so the `vector` extension's types
        // remain visible; tables and views are all created in the first schema,
        // i.e. the one dedicated to this eval run
        let options = PgConnectOptions::from_str(database_url)
            .context("DATABASE_URL 不是合法的连接串")?
            .options([("search_path", format!("{schema},public"))]);

        let pool = PgPoolOptions::new()
            .max_connections(16)
            .connect_with(options)
            .await
            .context("临时 schema 建好之后应当能连上")?;

        let store = Store::from_pool(pool.clone());
        store
            .migrate()
            .await
            .context("migration 在临时 schema 里跑不通")?;

        Ok(Self {
            store,
            pool,
            admin,
            schema,
        })
    }

    #[must_use]
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// The database's current time.
    ///
    /// A checkpoint must take **the database's** clock, not the local machine's:
    /// the default for `facts.created_at` is `clock_timestamp()`, and even a few
    /// dozen milliseconds of skew between the two clocks makes time-replay
    /// questions randomly retrieve one fact too many or too few.
    pub async fn db_now(&self) -> Result<DateTime<Utc>> {
        let (now,): (DateTime<Utc>,) = sqlx::query_as("SELECT clock_timestamp()")
            .fetch_one(&self.pool)
            .await
            .context("取不到数据库时钟")?;
        Ok(now)
    }

    /// Every fact in the store (invalidated ones included), for checking gold
    /// existence.
    pub async fn snapshot_facts(&self) -> Result<Vec<FactRow>> {
        let rows: Vec<(String, String, Option<String>, bool)> = sqlx::query_as(
            "SELECT f.id, f.statement, f.domain, (a.id IS NOT NULL) AS active
               FROM facts f
               LEFT JOIN active_facts a ON a.id = f.id
              ORDER BY f.created_at ASC, f.id ASC",
        )
        .fetch_all(&self.pool)
        .await
        .context("拉不到事实快照")?;

        Ok(rows
            .into_iter()
            .map(|(id, statement, domain, active)| FactRow {
                id,
                statement,
                domain,
                active,
            })
            .collect())
    }

    pub async fn count_entities(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM entities")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// How many rows actually landed in `episode_tool_calls`.
    ///
    /// A mismatch with the count declared in the suite means the trace was
    /// swallowed somewhere on the write path — which makes tool-experience
    /// questions fail "because the corpus never got in", looking exactly like
    /// "the extractor is not wired up". This number is the only basis for
    /// telling the two apart.
    pub async fn count_tool_calls(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM episode_tool_calls")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// Drop the temporary schema. With `keep` true it is retained (so bad cases
    /// can be inspected by hand afterwards).
    pub async fn cleanup(self, keep: bool) {
        self.store.close().await;
        if keep {
            eprintln!("已保留临时 schema：{}（记得手工 DROP）", self.schema);
        } else {
            let sql = format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema);
            if let Err(err) = sqlx::query(AssertSqlSafe(sql)).execute(&self.admin).await {
                eprintln!("清理临时 schema {} 失败：{err}", self.schema);
            }
        }
        self.admin.close().await;
    }
}

#[derive(Debug, Clone)]
pub struct FactRow {
    pub id: String,
    pub statement: String,
    pub domain: Option<String>,
    pub active: bool,
}

fn unique_schema_name() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock should not predate 1970")
        .as_millis();
    format!("{SCHEMA_PREFIX}{millis}_{}", std::process::id())
}

async fn sweep_stale_schemas(admin: &PgPool) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock should not predate 1970")
        .as_millis();

    let names: Vec<(String,)> = sqlx::query_as(
        "SELECT nspname::text FROM pg_namespace WHERE nspname LIKE 'cortex\\_eval\\_%'",
    )
    .fetch_all(admin)
    .await
    .unwrap_or_default();

    for (name,) in names {
        let born = name
            .strip_prefix(SCHEMA_PREFIX)
            .and_then(|rest| rest.split('_').next())
            .and_then(|millis| millis.parse::<u128>().ok());
        if born.is_some_and(|born| now.saturating_sub(born) > STALE_SCHEMA_AGE_MS) {
            let sql = format!("DROP SCHEMA IF EXISTS \"{name}\" CASCADE");
            let _ = sqlx::query(AssertSqlSafe(sql)).execute(admin).await;
        }
    }
}

// ══════════════════════════════════════════════════════════
//  Loading the corpus
// ══════════════════════════════════════════════════════════

/// Statistics from one load. These numbers are themselves a health report on
/// the extraction pipeline.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IngestStats {
    pub episodes: usize,
    /// How many tool calls landed in `episode_tool_calls`.
    ///
    /// Its gap against `candidates` is **the width of the current gap**: every
    /// trace landed in the store, yet not one candidate fact came from a trace
    /// (the extractor does not read that table yet)
    pub tool_calls: usize,
    pub candidates: usize,
    pub written: usize,
    pub duplicates: usize,
    pub superseded: usize,
    pub flagged: usize,
    pub entities: usize,
}

/// Checkpoint name → the system time after that turn committed.
pub type Checkpoints = HashMap<String, DateTime<Utc>>;

/// Build an extractor.
///
/// In seed mode the `LlmClient` is constructed purely as a placeholder
/// (`write_candidates` never touches it), so a fake key will do — which is
/// exactly why extraction and writing were split into two methods.
pub fn build_extractor(
    embedder: SharedEmbedder,
    mode: ExtractMode,
    entity_threshold: Option<f32>,
) -> Result<Extractor> {
    let llm = match mode {
        ExtractMode::Seed => {
            let cfg = LlmConfig {
                provider: "deepseek".to_owned(),
                model: "deepseek-v4-pro".to_owned(),
                cheap_model: "deepseek-v4-flash".to_owned(),
                // The eval must be able to hit a self-hosted endpoint too:
                // evaluating a different model often just means pointing at a
                // different compatible service
                base_url: std::env::var("CORTEX_LLM_BASE_URL")
                    .ok()
                    .filter(|v| !v.trim().is_empty()),
            };
            LlmClient::from_config(&cfg, "eval-offline-placeholder")
                .map_err(|e| anyhow::anyhow!("离线构造 LlmClient 失败：{e}"))?
        }
        ExtractMode::Llm => LlmClient::from_env().map_err(|e| {
            anyhow::anyhow!("llm 模式需要 .env 里的供应商 API key（如 DEEPSEEK_API_KEY）：{e}")
        })?,
    };

    let mut extractor = Extractor::new(llm, embedder, DEVICE);
    if let Some(t) = entity_threshold {
        extractor = extractor.with_entity_match_threshold(t);
    }
    Ok(extractor)
}

/// Pour the suite into the real ingest pipeline, in suite order.
pub async fn ingest_suite(
    db: &EvalDb,
    suite: &Suite,
    extractor: &Extractor,
    mode: ExtractMode,
) -> Result<(IngestStats, Checkpoints)> {
    let mut stats = IngestStats::default();
    let mut checkpoints = Checkpoints::new();

    for scenario in &suite.scenarios {
        // Entity names already seen in this scenario, hinting to the model that
        // it should reuse existing names (suppressing an alias explosion in llm
        // mode)
        let known = known_entity_names(scenario);

        for turn in &scenario.turns {
            let occurred_at = parse_date(&turn.occurred_at)?;
            let episode_id = insert_episode(db, scenario, turn, occurred_at).await?;
            stats.episodes += 1;
            stats.tool_calls += turn.tool_calls.len();

            let ctx = ExtractContext::new(episode_id, occurred_at)
                .with_domain(scenario.domain.clone())
                .with_known_entities(known.clone());

            let report = match mode {
                ExtractMode::Seed => {
                    let candidates = turn
                        .facts
                        .iter()
                        .map(|f| candidate_of(f, &scenario.domain))
                        .collect::<Result<Vec<_>>>()?;
                    extractor
                        .write_candidates(&db.store, candidates, &ctx)
                        .await
                        .map_err(|e| anyhow::anyhow!("落库失败（场景 {}）：{e}", scenario.id))?
                }
                ExtractMode::Llm => extractor
                    .ingest(&db.store, &turn.text, &ctx)
                    .await
                    .map_err(|e| anyhow::anyhow!("抽取失败（场景 {}）：{e}", scenario.id))?,
            };

            stats.candidates += report.candidates;
            stats.written += report.written.len();
            stats.duplicates += report.duplicates;
            stats.superseded += report.superseded.len();
            stats.flagged += report.flagged.len();
            stats.entities += report.new_entities.len();

            if let Some(name) = &turn.checkpoint {
                // Take the database's clock, and it must be after this turn
                // committed — see the comments on `db_now`
                checkpoints.insert(name.clone(), db.db_now().await?);
            }
        }
    }

    Ok((stats, checkpoints))
}

fn known_entity_names(scenario: &Scenario) -> Vec<String> {
    let mut out: Vec<String> = scenario
        .turns
        .iter()
        .flat_map(|t| &t.facts)
        .flat_map(|f| std::iter::once(f.subject.clone()).chain(f.object_entity.iter().cloned()))
        .collect();
    out.sort();
    out.dedup();
    out
}

async fn insert_episode(
    db: &EvalDb,
    scenario: &Scenario,
    turn: &crate::suite::Turn,
    occurred_at: DateTime<Utc>,
) -> Result<Id> {
    let id = Id::new();
    let episode = NewEpisode {
        id,
        session_id: scenario.id.clone(),
        role: Role::User,
        content: serde_json::json!({ "role": "user", "text": turn.text }),
        text: Some(turn.text.clone()),
        // Same as production: the space-separated string after jieba
        // tokenization, written in the same transaction as the main row
        tsv_source: Some(tokenize::to_tsvector_input(&turn.text)),
        domain: Some(scenario.domain.clone()),
        device_id: DEVICE.to_owned(),
        occurred_at,
    };

    // The tool trace and the episode land in **the same transaction**, matching
    // production's `live.rs`. Splitting them into two transactions would make
    // "episode committed, trace not yet committed" an observable intermediate
    // state, and the extractor is precisely what gets triggered after the
    // episode commits — at which point the trace must already be there
    let calls: Vec<NewEpisodeToolCall> = turn
        .tool_calls
        .iter()
        .enumerate()
        .map(|(i, c)| NewEpisodeToolCall {
            id: Id::new(),
            episode_id: id,
            ordinal: i32::try_from(i).unwrap_or(i32::MAX),
            name: c.name.clone(),
            path: c.path.clone(),
            summary: c.summary.clone(),
            ok: c.ok,
            device_id: DEVICE.to_owned(),
            diff: None,
        })
        .collect();

    db.store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            for call in &calls {
                tx.insert_episode_tool_call(call).await?;
            }
            Ok(())
        })
        .await
        .context("episode / 工具轨迹落库失败")?;
    Ok(id)
}

fn candidate_of(f: &SeedFact, scenario_domain: &str) -> Result<CandidateFact> {
    let object = match (&f.object, &f.object_entity) {
        (Some(text), None) => ObjectRef::Literal(text.clone()),
        (None, Some(name)) => ObjectRef::Entity(EntityRef::new(name, &f.object_entity_kind)),
        _ => bail!("事实「{}」的宾语形式非法（校验应当已挡住）", f.statement),
    };

    Ok(CandidateFact {
        subject: EntityRef::new(&f.subject, &f.subject_kind),
        predicate: f.predicate.clone(),
        object,
        statement: f.statement.clone(),
        domain: Some(
            f.domain
                .clone()
                .unwrap_or_else(|| scenario_domain.to_owned()),
        ),
        valid_at: f.valid_at.as_deref().map(parse_date).transpose()?,
        confidence: f.confidence,
    })
}

// ══════════════════════════════════════════════════════════
//  Cross-domain links (relates_to)
// ══════════════════════════════════════════════════════════

/// Who decides "is this pair worth linking".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeKind {
    /// No judging; accept everything the prefilter yields.
    /// **Deterministic**, CI-safe.
    ///
    /// This is not "judging turned off" but a **control group**: the prefilter
    /// has already applied three filters (cross-domain, not already connected,
    /// distance band), and this setting measures exactly those three filters'
    /// precision on their own. Without it there is no way to answer "what did
    /// the model's judgement actually buy".
    Prefilter,
    /// Really call the cheap model. Higher quality, but it drifts with the
    /// model and is unsuited to a regression gate.
    Cheap,
}

impl std::fmt::Display for JudgeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Prefilter => "prefilter",
            Self::Cheap => "cheap",
        })
    }
}

/// Configuration for the cross-domain link scan. With `enabled` false the whole
/// stage is skipped and it does not appear in the report either.
#[derive(Debug, Clone, Copy)]
pub struct CrossLinkOptions {
    pub enabled: bool,
    pub judge: JudgeKind,
    pub min_distance: f64,
    pub max_distance: f64,
    pub degree_cap: usize,
    /// Whether an edge's statement enters the BM25/vector index. The default
    /// matches production (it does not)
    pub indexed: bool,
}

impl Default for CrossLinkOptions {
    fn default() -> Self {
        Self {
            enabled: false,
            judge: JudgeKind::Prefilter,
            min_distance: cortex_memory::crosslink::DEFAULT_MIN_DISTANCE,
            max_distance: cortex_memory::crosslink::DEFAULT_MAX_DISTANCE,
            degree_cap: cortex_memory::crosslink::DEFAULT_DEGREE_CAP,
            indexed: false,
        }
    }
}

/// The scan's results, together with each edge's statement — a count alone
/// cannot decide whether they are noise; the edges themselves have to be
/// printed for a person to read.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CrossLinkStats {
    pub judge: String,
    pub scanned: usize,
    pub candidates: usize,
    pub accepted: usize,
    pub degree_capped: usize,
    /// Rows written (two rows per linked pair; see `crosslink::Linker::plan`)
    pub written: usize,
    /// The forward direction's statement, in write order
    pub links: Vec<String>,
}

/// Run a cross-domain link scan once the corpus is loaded.
///
/// The timing matters: it must be **after** ingest and **before** retrieval. The
/// nightly scan sits in the same position in production — it is offline batch
/// work, on neither the write path nor the retrieval path.
pub async fn build_cross_links(
    db: &EvalDb,
    embedder: &SharedEmbedder,
    opts: CrossLinkOptions,
) -> Result<CrossLinkStats> {
    use cortex_memory::crosslink::{AcceptAll, CheapModelJudge, LinkJudge, Linker, StoreCorpus};

    // The lookback window is opened to ten years: the eval corpus was just
    // loaded, but its `occurred_at` values span several months, while
    // `recall_recent` filters on `created_at` — widening the window pulls in
    // nothing extra, it merely prevents this from silently scanning zero facts
    // after somebody changes how the corpus is loaded
    let corpus = StoreCorpus::new(&db.store).with_window(3650, 200);
    let linker = Linker::new(DEVICE)
        .with_distance_band(opts.min_distance, opts.max_distance)
        .with_degree_cap(opts.degree_cap)
        .with_indexed(opts.indexed);

    let judge: Box<dyn LinkJudge> = match opts.judge {
        JudgeKind::Prefilter => Box::new(AcceptAll),
        JudgeKind::Cheap => Box::new(CheapModelJudge::new(LlmClient::from_env().map_err(
            |e| anyhow::anyhow!("`--cross-link-judge cheap` 需要 .env 里的供应商 API key：{e}"),
        )?)),
    };

    let report = linker
        .run(&db.store, &corpus, judge.as_ref(), embedder)
        .await
        .map_err(|e| anyhow::anyhow!("跨域连接扫描失败：{e}"))?;

    let mut links = Vec::new();
    for id in &report.written {
        if let Some(f) = db
            .store
            .fact(&id.to_string())
            .await
            .map_err(|e| anyhow::anyhow!("读不回刚写的连接：{e}"))?
        {
            links.push(f.statement);
        }
    }

    Ok(CrossLinkStats {
        judge: opts.judge.to_string(),
        scanned: report.scanned,
        candidates: report.candidates,
        accepted: report.accepted,
        degree_capped: report.degree_capped,
        written: report.written.len(),
        links,
    })
}

// ══════════════════════════════════════════════════════════
//  Running retrieval
// ══════════════════════════════════════════════════════════

/// The retrieval-side configuration. When it matches the production defaults,
/// what comes out is "the current implementation's baseline".
#[derive(Debug, Clone, Copy)]
pub struct RetrievalConfig {
    pub width: RecallWidth,
    pub budget: Budget,
    pub context_window: usize,
    /// How many entries time replay takes.
    ///
    /// Set to the storage layer's ceiling of 200: replay's three paths all rank
    /// within one and the same snapshot, and limiting it too tightly measures
    /// "is the snapshot big enough" rather than "is the ordering right".
    pub replay_limit: i64,
    /// Whether replay uses the query-ranked version or the pure
    /// reverse-chronological full-picture version. For A/B.
    pub replay_ranked: bool,
    /// The abstention threshold. `None` uses the production default (for
    /// threshold sweeps)
    pub abstain_below: Option<f64>,
    pub recency: RecencyMode,
    pub episode_channel: bool,
    pub peak_bonus: f64,
    /// The semantic floor (an upper bound on cosine distance). `None` = use the
    /// production default
    pub semantic_floor: Option<f64>,
    /// The cross-domain bridge's exemption from the domain penalty. The default
    /// matches production (on)
    pub bridge_exemption: bool,
    /// Force the semantic floor off.
    ///
    /// Not the same thing as `semantic_floor: None`: `None` means "follow the
    /// production default", this means "explicitly do not want it". An A/B has
    /// to be able to express both intentions
    pub no_semantic_floor: bool,
}

impl Default for RetrievalConfig {
    fn default() -> Self {
        Self {
            width: RecallWidth::default(),
            budget: Budget::default(),
            context_window: 128_000,
            replay_limit: 200,
            replay_ranked: true,
            abstain_below: None,
            recency: RecencyMode::Off,
            episode_channel: false,
            peak_bonus: 0.04,
            semantic_floor: None,
            no_semantic_floor: false,
            bridge_exemption: true,
        }
    }
}

pub struct EvalRetriever {
    retriever: Retriever<SharedEmbedder>,
    embedder: SharedEmbedder,
    config: RetrievalConfig,
}

impl EvalRetriever {
    #[must_use]
    pub fn new(embedder: SharedEmbedder, config: RetrievalConfig) -> Self {
        let mut retriever = Retriever::new(embedder.clone())
            .with_width(config.width)
            .with_budget(config.budget)
            .with_recency(config.recency)
            .with_episode_channel(config.episode_channel)
            .with_peak_bonus(config.peak_bonus)
            .with_bridge_exemption(config.bridge_exemption);
        if let Some(t) = config.abstain_below {
            retriever = retriever.with_abstain_below(t);
        }
        if config.no_semantic_floor {
            retriever = retriever.with_semantic_floor(None);
        } else if config.semantic_floor.is_some() {
            retriever = retriever.with_semantic_floor(config.semantic_floor);
        }
        Self {
            retriever,
            embedder,
            config,
        }
    }

    #[must_use]
    pub fn embedding_model(&self) -> String {
        self.embedder.model_id().to_owned()
    }

    /// The semantic floor actually in effect (the default varies with the
    /// embedding backend, so it has to be asked for).
    #[must_use]
    pub fn semantic_floor(&self) -> Option<f64> {
        self.retriever.semantic_floor()
    }

    /// The production path: four-way retrieval + RRF + domain weighting +
    /// budget truncation.
    pub async fn retrieve(
        &self,
        store: &Store,
        query: &str,
        domain: Option<&str>,
    ) -> Result<Vec<ScoredItem>> {
        let out = self
            .retriever
            .retrieve(store, query, domain, self.config.context_window)
            .await
            .map_err(|e| anyhow::anyhow!("检索失败：{e}"))?;
        Ok(zip_items(out))
    }

    /// Replay by system time: which facts Cortex held to be true at the moment
    /// `as_of`.
    ///
    /// `replay_ranked` decides between the query-ranked version and the pure
    /// reverse-chronological full-picture version — the existence of this
    /// switch is itself the evidence from that A/B.
    pub async fn replay(
        &self,
        store: &Store,
        query: &str,
        as_of: DateTime<Utc>,
    ) -> Result<Vec<ScoredItem>> {
        let out = if self.config.replay_ranked {
            self.retriever
                .retrieve_as_of_ranked(
                    store,
                    query,
                    as_of,
                    self.config.replay_limit,
                    self.config.context_window,
                )
                .await
        } else {
            self.retriever
                .retrieve_as_of(
                    store,
                    as_of,
                    self.config.replay_limit,
                    self.config.context_window,
                )
                .await
        }
        .map_err(|e| anyhow::anyhow!("时间回放失败：{e}"))?;
        Ok(zip_items(out))
    }

    /// The four paths' **raw** retrievals, each sorted by its own relevance,
    /// unfused.
    ///
    /// This replicates `Retriever::recall_all`'s orchestration — that method is
    /// private, and per-path attribution needs each path's complete ranking (the
    /// fused result's `Attribution` keeps only the channel names and discards
    /// the within-path rank). The main metrics on the fusion side still go
    /// through the real `retrieve`; this is for diagnostics only.
    pub async fn per_channel(&self, store: &Store, query: &str) -> Result<Vec<ChannelRun>> {
        let tsquery = tokenize::to_tsquery_input(query);
        let qvec = self
            .embedder
            .embed_one(query)
            .await
            .map_err(|e| anyhow::anyhow!("查询向量化失败：{e}"))?;
        let embedding = Vector::from(qvec);

        let terms: Vec<String> = tokenize::to_tsvector_input(query)
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let seeds = store.entities_matching(&terms, GRAPH_SEED_LIMIT).await?;

        let w = self.config.width;
        let (bm25, vector, graph, recency, episode) = tokio::join!(
            store.recall_bm25(&tsquery, w.bm25),
            store.recall_vector(&embedding, self.embedder.model_id(), w.vector),
            store.recall_graph(&seeds, w.graph_hops, w.graph),
            store.recall_recent(w.recent_days, w.recent),
            store.recall_via_episodes(&tsquery, w.episode_docs, w.episode),
        );

        // Per-path diagnostics **always issue all five paths**, regardless of
        // whether the fusion side has them enabled: "what this path would have
        // found once it is switched off" is precisely the number you most want
        // during an A/B, and switching it off along with the flag leaves
        // nothing but a column of zeros, from which nothing can be judged
        Ok(vec![
            ChannelRun {
                name: "bm25",
                statements: statements(bm25?),
            },
            ChannelRun {
                name: "vector",
                statements: statements(vector?),
            },
            ChannelRun {
                name: "graph",
                statements: statements(graph?),
            },
            ChannelRun {
                name: "recency",
                statements: statements(recency?),
            },
            ChannelRun {
                name: "episode",
                statements: statements(episode?),
            },
        ])
    }
}

fn statements(facts: Vec<cortex_store::Fact>) -> Vec<String> {
    facts.into_iter().map(|f| f.statement).collect()
}

fn zip_items(out: cortex_memory::retrieval::Retrieved) -> Vec<ScoredItem> {
    out.items
        .into_iter()
        .zip(out.attribution)
        .map(|(item, attr)| ScoredItem {
            fact_id: item.id,
            statement: item.statement,
            channels: attr.channels.iter().map(|c| (*c).to_owned()).collect(),
        })
        .collect()
}

/// The default vectorization backend — **the same selection logic as
/// production**.
///
/// It goes through `Backend::from_env()` (`CORTEX_EMBED_BACKEND`, defaulting to
/// `api`) rather than hardcoding `HashEmbedder`. There was a version that
/// hardcoded it, and the price was that every baseline number had been measured
/// on the premise that "the vector path is just a second lexical channel" —
/// switching to a real semantic model overturned the per-path conclusions
/// wholesale, wasting a round of parameter tuning.
///
/// Set `CORTEX_EMBED_BACKEND=hash` to switch back to the hash stub as a control
/// (also the way to run CI without network access).
///
/// ⚠️ **Baselines are calibrated against a specific model.**
/// `scripts/evals-baseline.*.json` records `embedding_model` and the regression
/// gate checks it — after changing backends (even "the same bge-m3, just moved
/// from in-process to remote"), throwing an old baseline at new scores is
/// meaningless, and that check exists for exactly this reason.
///
/// # Errors
///
/// The environment variable is invalid, or the model fails to load/download.
pub async fn default_embedder() -> Result<SharedEmbedder> {
    let backend = Backend::from_env().map_err(|e| anyhow::anyhow!("{e}"))?;
    build_embedder(backend)
        .await
        .map_err(|e| anyhow::anyhow!("加载 embedding 后端失败：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::SeedFact;

    fn seed(object: Option<&str>, entity: Option<&str>) -> SeedFact {
        SeedFact {
            subject: "cortex".into(),
            subject_kind: "project".into(),
            predicate: "stores_in".into(),
            object: object.map(str::to_owned),
            object_entity: entity.map(str::to_owned),
            object_entity_kind: "tool".into(),
            statement: "Cortex 的对象存储用 RustFS".into(),
            valid_at: Some("2026-05-18".into()),
            confidence: 0.9,
            domain: None,
        }
    }

    #[test]
    fn literal_object_becomes_literal_ref() {
        let c = candidate_of(&seed(Some("RustFS"), None), "coding").expect("should convert");
        assert_eq!(c.object, ObjectRef::Literal("RustFS".into()));
        assert_eq!(
            c.domain.as_deref(),
            Some("coding"),
            "when unspecified it should inherit the scenario's domain"
        );
    }

    #[test]
    fn entity_object_becomes_graph_edge() {
        let c = candidate_of(&seed(None, Some("RustFS")), "coding").expect("should convert");
        match c.object {
            ObjectRef::Entity(e) => assert_eq!(e.kind, "tool"),
            other => panic!("only an entity object forms an edge in the graph, got {other:?}"),
        }
    }

    #[test]
    fn ambiguous_object_is_rejected() {
        assert!(
            candidate_of(&seed(Some("a"), Some("b")), "coding").is_err(),
            "giving both object forms leaves the write path unable to decide"
        );
    }

    #[test]
    fn schema_names_are_unique_and_prefixed() {
        let a = unique_schema_name();
        assert!(
            a.starts_with(SCHEMA_PREFIX),
            "the sweep logic identifies schemas by this prefix: {a}"
        );
    }

    #[test]
    fn default_retrieval_config_matches_production() {
        let c = RetrievalConfig::default();
        // Baseline numbers must come from a run at the production defaults,
        // otherwise the control group is meaningless
        assert_eq!(c.width.bm25, RecallWidth::default().bm25);
        assert_eq!(
            c.budget.absolute_max_tokens,
            Budget::default().absolute_max_tokens
        );
    }
}

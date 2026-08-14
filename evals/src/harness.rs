//! 评测执行环境：隔离的数据库、真实 ingest 管线、真实检索器。
//!
//! # 隔离：临时 schema
//!
//! 评测会灌进上百条事实。**绝不能污染开发库** —— 一次评测跑完，
//! 开发时的 `memory search` 就全是评测语料了。
//!
//! 做法与 `crates/cortex-store/tests/common/mod.rs` 同款：连接串不变，
//! 只把 `search_path` 指到一个新建的 schema，在其中跑一遍 migration，
//! 结束 `DROP SCHEMA ... CASCADE`。
//!
//! # 为什么要走真实 ingest 而不是直接 INSERT facts
//!
//! 直接塞 facts 会绕过 jieba 分词（`tsv` 列）、绕过 embedding、绕过实体消解、
//! 绕过矛盾消解。而这四样恰恰决定了检索能不能召回：
//! `tsv` 不对，BM25 那一路就是死的；实体没消解，图遍历就跳不动。
//! 评测评的是**整条链路**，不是四条 SQL。
//!
//! # 两种抽取模式
//!
//! | 模式 | 抽取 | 用途 |
//! |---|---|---|
//! | `seed` | 用题集里手写的候选事实，走 `write_candidates` | 默认。语料确定，跑一百遍结果一致，能进 CI |
//! | `llm` | 真调廉价模型，走 `ingest` | 连抽取质量一起测，但结果随模型漂移，不适合做回归门 |
//!
//! 两种模式**共用**消解、向量化、分词与落库 —— 差别只在候选事实从哪来。

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

/// 崩溃残留的 schema 超过这个岁数就顺手清掉。
/// 不做无差别清扫 —— 那会删掉兄弟进程正在用的 schema。
const STALE_SCHEMA_AGE_MS: u128 = 30 * 60 * 1000;

pub const DEVICE: &str = "eval-harness";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMode {
    /// 用题集里手写的候选事实
    Seed,
    /// 真调 LLM 抽取
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
//  隔离的数据库
// ══════════════════════════════════════════════════════════

pub struct EvalDb {
    pub store: Store,
    /// 自己留一份池子做诊断查询 —— `Store` 的 pool 是 crate 私有的，
    /// 而评测需要「把库里全部事实拉出来核对 gold」这种越界能力
    pool: PgPool,
    admin: PgPool,
    schema: String,
}

impl EvalDb {
    /// 建一个隔离的评测库。
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

        // `public` 留在 search_path 里是为了让 `vector` 扩展的类型可见；
        // 建表建视图一律落到第一个 schema，也就是本次评测专属的那个
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

    /// 数据库当前时间。
    ///
    /// checkpoint 必须取**库的**时钟而不是本机时钟：`facts.created_at` 的默认值是
    /// `clock_timestamp()`，两边时钟哪怕差几十毫秒，时间回放题就会随机地
    /// 多召回或少召回一条。
    pub async fn db_now(&self) -> Result<DateTime<Utc>> {
        let (now,): (DateTime<Utc>,) = sqlx::query_as("SELECT clock_timestamp()")
            .fetch_one(&self.pool)
            .await
            .context("取不到数据库时钟")?;
        Ok(now)
    }

    /// 库里全部事实（含已失效的），供 gold 存在性核对。
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

    /// 真正落进 `episode_tool_calls` 的行数。
    ///
    /// 与题集里声明的条数对不上，就说明轨迹在写入路径上被吞了 ——
    /// 而那会让工具经验题「因为语料没进去」而挂，与「抽取器没接」
    /// 的表现一模一样。这个数字是把两者分开的唯一依据。
    pub async fn count_tool_calls(&self) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM episode_tool_calls")
            .fetch_one(&self.pool)
            .await?;
        Ok(n)
    }

    /// 删掉临时 schema。`keep` 为真时保留（便于事后手工查 bad case）。
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
        .expect("系统时钟不应早于 1970")
        .as_millis();
    format!("{SCHEMA_PREFIX}{millis}_{}", std::process::id())
}

async fn sweep_stale_schemas(admin: &PgPool) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟不应早于 1970")
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
//  灌语料
// ══════════════════════════════════════════════════════════

/// 一次灌库的统计。这些数字本身就是抽取管线的体检报告。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct IngestStats {
    pub episodes: usize,
    /// 落进 `episode_tool_calls` 的工具调用数。
    ///
    /// 它与 `candidates` 的落差就是**当前这条缺口的宽度**：轨迹全落库了，
    /// 但一条候选事实都不是从轨迹来的（抽取器还不读那张表）
    pub tool_calls: usize,
    pub candidates: usize,
    pub written: usize,
    pub duplicates: usize,
    pub superseded: usize,
    pub flagged: usize,
    pub entities: usize,
}

/// checkpoint 名 → 该轮提交后的系统时间。
pub type Checkpoints = HashMap<String, DateTime<Utc>>;

/// 造一个抽取器。
///
/// seed 模式下 `LlmClient` 只是被构造出来占位（`write_candidates` 不碰它），
/// 拿一个假密钥即可 —— 这正是抽取与落库被拆成两个方法的原因。
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
                // 评测也要能打到自建端点：换模型评测常常就是换一个兼容服务
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

/// 按题集顺序灌进真实 ingest 管线。
pub async fn ingest_suite(
    db: &EvalDb,
    suite: &Suite,
    extractor: &Extractor,
    mode: ExtractMode,
) -> Result<(IngestStats, Checkpoints)> {
    let mut stats = IngestStats::default();
    let mut checkpoints = Checkpoints::new();

    for scenario in &suite.scenarios {
        // 场景内已出现过的实体名，提示模型复用既有称呼（llm 模式下压制别名爆炸）
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
                // 取库的时钟，且必须在本轮提交之后 —— 见 `db_now` 的注释
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
        // 与生产同款：jieba 分词后的空格串，与主行同事务落库
        tsv_source: Some(tokenize::to_tsvector_input(&turn.text)),
        domain: Some(scenario.domain.clone()),
        device_id: DEVICE.to_owned(),
        occurred_at,
    };

    // 工具轨迹与 episode **同事务**落库，与生产的 `live.rs` 一致。
    // 拆成两个事务会让「episode 已提交、轨迹还没提交」成为一个可观测的中间态，
    // 而抽取器将来正是在 episode 提交之后被触发的 —— 那时轨迹必须已经在
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
//  跨域连接（relates_to）
// ══════════════════════════════════════════════════════════

/// 谁来判「这一对值不值得连」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JudgeKind {
    /// 不判，预筛过的全接受。**确定性**，可进 CI。
    ///
    /// 它不是「关掉判定」而是**对照组**：预筛已经做了跨域、未连通、距离带
    /// 三道过滤，这一档量的正是那三道过滤单独的精度。没有它就没法回答
    /// 「模型判定到底买到了什么」。
    Prefilter,
    /// 真调廉价模型。质量高但随模型漂移，不适合做回归门。
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

/// 跨域连接扫描的配置。`enabled` 为假时整个环节跳过，报告里也不出现。
#[derive(Debug, Clone, Copy)]
pub struct CrossLinkOptions {
    pub enabled: bool,
    pub judge: JudgeKind,
    pub min_distance: f64,
    pub max_distance: f64,
    pub degree_cap: usize,
    /// 边的 statement 进不进 BM25/向量索引。默认与生产一致（不进）
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

/// 扫描的结果，连同每条边的 statement —— 光看条数判断不了噪声，
/// 得把边本身打出来给人读。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CrossLinkStats {
    pub judge: String,
    pub scanned: usize,
    pub candidates: usize,
    pub accepted: usize,
    pub degree_capped: usize,
    /// 写入的行数（每对关联两行，见 `crosslink::Linker::plan`）
    pub written: usize,
    /// 正向那一条的 statement，按写入顺序
    pub links: Vec<String>,
}

/// 在语料灌完之后跑一遍跨域连接扫描。
///
/// 时机很重要：必须在 ingest **之后**、检索**之前**。夜间扫描在生产里也是
/// 这个位置 —— 它是离线批处理，不在写入路径上，也不在检索路径上。
pub async fn build_cross_links(
    db: &EvalDb,
    embedder: &SharedEmbedder,
    opts: CrossLinkOptions,
) -> Result<CrossLinkStats> {
    use cortex_memory::crosslink::{AcceptAll, CheapModelJudge, LinkJudge, Linker, StoreCorpus};

    // 回看窗口开到十年：评测语料是刚灌进去的，但 `occurred_at` 跨了好几个月，
    // 而 `recall_recent` 按 `created_at` 过滤 —— 窗口开大不会多捞到东西，
    // 只是免得将来有人改了灌库方式之后这里静默扫不到任何事实
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
//  跑检索
// ══════════════════════════════════════════════════════════

/// 检索侧的配置。与生产默认值一致时，跑出来的就是「当前实现的基线」。
#[derive(Debug, Clone, Copy)]
pub struct RetrievalConfig {
    pub width: RecallWidth,
    pub budget: Budget,
    pub context_window: usize,
    /// 时间回放取多少条。
    ///
    /// 取存储层的上限 200：回放的三路都在同一份快照上排序，限得小了，
    /// 测出来的是「快照够不够大」而不是「排序对不对」。
    pub replay_limit: i64,
    /// 回放走带查询的排序版还是纯时间倒序的全貌版。A/B 用。
    pub replay_ranked: bool,
    /// 弃权阈值。`None` 用生产默认值（阈值扫描用）
    pub abstain_below: Option<f64>,
    pub recency: RecencyMode,
    pub episode_channel: bool,
    pub peak_bonus: f64,
    /// 语义地板（余弦距离上限）。`None` = 用生产默认值
    pub semantic_floor: Option<f64>,
    /// 跨域桥的域惩罚豁免。默认与生产一致（开）
    pub bridge_exemption: bool,
    /// 强制关掉语义地板。
    ///
    /// 与 `semantic_floor: None` 不是一回事：`None` 是「按生产默认走」，
    /// 这个是「明确不要」。A/B 里两种意图都得表达得出来
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

    /// 实际生效的语义地板（默认值随 embedding 后端而变，得问出来）。
    #[must_use]
    pub fn semantic_floor(&self) -> Option<f64> {
        self.retriever.semantic_floor()
    }

    /// 走生产路径的四路召回 + RRF + 域加权 + 预算截断。
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

    /// 按系统时间回放：在 `as_of` 那一刻，Cortex 认为哪些事实为真。
    ///
    /// `replay_ranked` 决定走带查询的排序版还是纯时间倒序的全貌版 ——
    /// 这个开关的存在本身就是那次 A/B 的证据。
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

    /// 四路的**原始**召回，各自按自身相关性排序，不融合。
    ///
    /// 复刻了 `Retriever::recall_all` 的编排 —— 那个方法是私有的，而逐路
    /// 归因需要每一路的完整排名（融合结果的 `Attribution` 只留了通道名，
    /// 丢掉了路内名次）。融合侧的主指标仍然走真实的 `retrieve`，
    /// 这里只用于诊断。
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

        // 逐路诊断**永远发全部五路**，与融合侧是否启用它们无关：
        // 「关掉这一路之后它本来能捞到什么」正是 A/B 时最想看的那个数字，
        // 跟着开关一起关掉就只剩一列零，什么都判断不了
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

/// 默认的向量化后端 —— **跟生产同一条选择逻辑**。
///
/// 走 `Backend::from_env()`（`CORTEX_EMBED_BACKEND`，默认 `api`），
/// 而不是写死 `HashEmbedder`。写死过一版，代价是基线数字全都是在
/// 「向量那一路只是第二条词法通道」的前提下测出来的 —— 换成真实语义模型后
/// 逐路结论要整体推翻，白调了一轮参数。
///
/// 设 `CORTEX_EMBED_BACKEND=hash` 可切回散列桩做对照（也是无网 CI 的走法）。
///
/// ⚠️ **基线是按具体模型标定的。** `scripts/evals-baseline.*.json` 里记着
/// `embedding_model`，回归门会核对它 —— 换后端（哪怕是「同一个 bge-m3、
/// 只是从进程内换成远端」）之后拿旧基线撞新分数是没有意义的，
/// 那道核对存在的理由正是这个。
///
/// # Errors
///
/// 环境变量非法，或模型加载/下载失败。
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
        let c = candidate_of(&seed(Some("RustFS"), None), "coding").expect("应能转换");
        assert_eq!(c.object, ObjectRef::Literal("RustFS".into()));
        assert_eq!(
            c.domain.as_deref(),
            Some("coding"),
            "未指定时应继承场景 domain"
        );
    }

    #[test]
    fn entity_object_becomes_graph_edge() {
        let c = candidate_of(&seed(None, Some("RustFS")), "coding").expect("应能转换");
        match c.object {
            ObjectRef::Entity(e) => assert_eq!(e.kind, "tool"),
            other => panic!("实体宾语才是图里的一条边，实际 {other:?}"),
        }
    }

    #[test]
    fn ambiguous_object_is_rejected() {
        assert!(
            candidate_of(&seed(Some("a"), Some("b")), "coding").is_err(),
            "两种宾语同时给，落库时无从判定"
        );
    }

    #[test]
    fn schema_names_are_unique_and_prefixed() {
        let a = unique_schema_name();
        assert!(a.starts_with(SCHEMA_PREFIX), "清扫逻辑靠前缀识别：{a}");
    }

    #[test]
    fn default_retrieval_config_matches_production() {
        let c = RetrievalConfig::default();
        // 基线数字必须是「生产默认值跑出来的」，否则对照组毫无意义
        assert_eq!(c.width.bm25, RecallWidth::default().bm25);
        assert_eq!(
            c.budget.absolute_max_tokens,
            Budget::default().absolute_max_tokens
        );
    }
}

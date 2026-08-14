//! 抽取管线集成测试的脚手架。
//!
//! 与 `cortex-store/tests/common` 同款：每个测试跑在自己的临时 schema 里，
//! 拿不到数据库时返回 `None`（测试 skip 而非 fail —— CI 有 Postgres，
//! 开发机上可能没起 `docker compose`）。

#![allow(dead_code)]

use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, TimeZone, Utc};
use cortex_core::Id;
use cortex_core::config::LlmConfig;
use cortex_llm::LlmClient;
use cortex_memory::embed::HashEmbedder;
use cortex_memory::extract::{CandidateFact, EntityRef, ExtractContext, Extractor, ObjectRef};
use cortex_store::{NewEpisode, NewEpisodeToolCall, Role, Store};
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

const STALE_SCHEMA_AGE_MS: u128 = 10 * 60 * 1000;
const SCHEMA_PREFIX: &str = "cortex_mem_test_";

pub const DEVICE: &str = "test-device";

pub struct TestDb {
    pub store: Store,
    admin: PgPool,
    schema: String,
}

impl TestDb {
    pub async fn cleanup(self) {
        self.store.close().await;
        let sql = format!("DROP SCHEMA IF EXISTS \"{}\" CASCADE", self.schema);
        if let Err(err) = sqlx::query(AssertSqlSafe(sql)).execute(&self.admin).await {
            eprintln!("清理临时 schema {} 失败：{err}", self.schema);
        }
        self.admin.close().await;
    }
}

/// 建一个隔离的测试库。拿不到数据库返回 `None`。
pub async fn setup() -> Option<TestDb> {
    let url = database_url()?;

    let admin = match PgPoolOptions::new().max_connections(2).connect(&url).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("跳过：连不上数据库（{err}）");
            return None;
        }
    };

    sweep_stale_schemas(&admin).await;

    let schema = unique_schema_name();
    let create = format!("CREATE SCHEMA \"{schema}\"");
    if let Err(err) = sqlx::query(AssertSqlSafe(create)).execute(&admin).await {
        eprintln!("跳过：建不出临时 schema（{err}）");
        admin.close().await;
        return None;
    }

    // public 留在 search_path 里是为了让 vector 扩展的类型可见
    let options = PgConnectOptions::from_str(&url)
        .expect("DATABASE_URL 应当是合法的连接串")
        .options([("search_path", format!("{schema},public"))]);

    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .expect("临时 schema 建好之后应当能连上");

    let store = Store::from_pool(pool);
    store
        .migrate()
        .await
        .expect("migration 应当能在临时 schema 里跑通");

    Some(TestDb {
        store,
        admin,
        schema,
    })
}

fn database_url() -> Option<String> {
    let _ = dotenvy::dotenv();
    match std::env::var("DATABASE_URL") {
        Ok(url) if !url.is_empty() => Some(url),
        _ => {
            eprintln!("跳过：未设置 DATABASE_URL");
            None
        }
    }
}

fn unique_schema_name() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟不应早于 1970")
        .as_millis();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{SCHEMA_PREFIX}{millis}_{}_{n}", std::process::id())
}

async fn sweep_stale_schemas(admin: &PgPool) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟不应早于 1970")
        .as_millis();

    let names: Vec<(String,)> = sqlx::query_as(
        "SELECT nspname::text FROM pg_namespace WHERE nspname LIKE 'cortex\\_mem\\_test\\_%'",
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
//  造数据
// ══════════════════════════════════════════════════════════

/// 造一个**不联网**的抽取器。
///
/// `LlmClient::from_config` 只是装配 goose Provider，不会发请求，所以拿一个
/// 假密钥就能构造。只要不调 `extract()`（走 `write_candidates` 直接喂候选），
/// 整条确定性链路就能脱离模型测试 —— 这正是把抽取与落库拆成两个方法的原因。
pub fn offline_extractor() -> Extractor {
    let cfg = LlmConfig {
        provider: "deepseek".to_string(),
        model: "deepseek-v4-pro".to_string(),
        cheap_model: "deepseek-v4-flash".to_string(),
        base_url: None,
    };
    let llm = LlmClient::from_config(&cfg, "not-a-real-key").expect("应能离线构造");
    Extractor::new(llm, Arc::new(HashEmbedder::new()), DEVICE)
        // HashEmbedder 是字符 bigram 散列，不是语义模型，别名之间的余弦
        // 天然低于真实的 bge-m3。显式调低阈值，让测试测的是消解逻辑本身。
        .with_entity_match_threshold(0.75)
}

/// 落一条 episode 并返回它的 id。事实的 `source_episode_id` 是 NOT NULL 的。
pub async fn seed_episode(store: &Store, text: &str) -> Id {
    let id = Id::new();
    let episode = NewEpisode {
        id,
        session_id: "s1".to_owned(),
        role: Role::User,
        content: serde_json::json!({ "role": "user", "text": text }),
        text: Some(text.to_owned()),
        tsv_source: Some(text.to_owned()),
        domain: Some("coding".to_owned()),
        device_id: DEVICE.to_owned(),
        occurred_at: Utc::now(),
    };
    store
        .write_txn(async |tx| tx.insert_episode(&episode).await)
        .await
        .expect("episode 应能落库");
    id
}

/// 落一条 episode 并把 `(name, path, ok, summary)` 挂成工具轨迹。
///
/// 与生产的 `live.rs`、评测的 harness 一致：轨迹与 episode **同一个
/// `write_txn`**。拆成两个事务会造出「episode 已提交、轨迹还没提交」这个
/// 中间态，而抽取器正是在 episode 提交之后被触发的。
pub async fn seed_episode_with_tools(
    store: &Store,
    text: &str,
    calls: &[(&str, Option<&str>, bool, &str)],
) -> Id {
    let id = Id::new();
    let episode = NewEpisode {
        id,
        session_id: "s1".to_owned(),
        role: Role::User,
        content: serde_json::json!({ "role": "user", "text": text }),
        text: Some(text.to_owned()),
        tsv_source: Some(text.to_owned()),
        domain: Some("coding".to_owned()),
        device_id: DEVICE.to_owned(),
        occurred_at: Utc::now(),
    };
    let rows: Vec<NewEpisodeToolCall> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, path, ok, summary))| NewEpisodeToolCall {
            id: Id::new(),
            episode_id: id,
            ordinal: i32::try_from(i).expect("测试里的调用数不会溢出"),
            name: (*name).to_owned(),
            path: path.map(str::to_owned),
            summary: (*summary).to_owned(),
            ok: *ok,
            device_id: DEVICE.to_owned(),
            diff: None,
        })
        .collect();

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            for row in &rows {
                tx.insert_episode_tool_call(row).await?;
            }
            Ok(())
        })
        .await
        .expect("episode 与工具轨迹应能同事务落库");
    id
}

pub fn ctx(episode_id: Id, occurred_at: DateTime<Utc>) -> ExtractContext {
    ExtractContext::new(episode_id, occurred_at).with_domain("coding")
}

pub fn ts(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0)
        .single()
        .expect("测试用的日期应当合法")
}

/// 字面量宾语的候选事实。
pub fn candidate(
    subject: &str,
    kind: &str,
    predicate: &str,
    object: &str,
    valid_at: Option<DateTime<Utc>>,
) -> CandidateFact {
    CandidateFact {
        subject: EntityRef::new(subject, kind),
        predicate: predicate.to_owned(),
        object: ObjectRef::Literal(object.to_owned()),
        statement: format!("{subject} 的 {predicate} 是 {object}"),
        domain: Some("coding".to_owned()),
        valid_at,
        confidence: 0.9,
    }
}

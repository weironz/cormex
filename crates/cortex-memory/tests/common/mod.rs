//! Scaffolding for the extraction pipeline's integration tests.
//!
//! Same shape as `cortex-store/tests/common`: every test runs in its own
//! temporary schema, and returns `None` when the database is unreachable (the
//! test skips rather than fails — CI has Postgres, a dev machine may not have
//! `docker compose` up).

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
            eprintln!(
                "cleaning up the temporary schema {} failed: {err}",
                self.schema
            );
        }
        self.admin.close().await;
    }
}

/// Builds an isolated test database. Returns `None` when the database is
/// unreachable.
pub async fn setup() -> Option<TestDb> {
    let url = database_url()?;

    let admin = match PgPoolOptions::new().max_connections(2).connect(&url).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("skipped: cannot reach the database ({err})");
            return None;
        }
    };

    sweep_stale_schemas(&admin).await;

    let schema = unique_schema_name();
    let create = format!("CREATE SCHEMA \"{schema}\"");
    if let Err(err) = sqlx::query(AssertSqlSafe(create)).execute(&admin).await {
        eprintln!("skipped: cannot create a temporary schema ({err})");
        admin.close().await;
        return None;
    }

    // public stays on the search_path so the vector extension's types remain visible
    let options = PgConnectOptions::from_str(&url)
        .expect("DATABASE_URL should be a valid connection string")
        .options([("search_path", format!("{schema},public"))]);

    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect_with(options)
        .await
        .expect("should be able to connect once the temporary schema exists");

    let store = Store::from_pool(pool);
    store
        .migrate()
        .await
        .expect("migrations should run through inside the temporary schema");

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
            eprintln!("skipped: DATABASE_URL is not set");
            None
        }
    }
}

fn unique_schema_name() -> String {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock should not predate 1970")
        .as_millis();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{SCHEMA_PREFIX}{millis}_{}_{n}", std::process::id())
}

async fn sweep_stale_schemas(admin: &PgPool) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock should not predate 1970")
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
//  Fixtures
// ══════════════════════════════════════════════════════════

/// Builds an extractor that **never touches the network**.
///
/// `LlmClient::from_config` only assembles the goose Provider; it sends no
/// request, so a fake key is enough to construct one. As long as `extract()`
/// is never called (candidates are fed in directly through `write_candidates`),
/// the entire deterministic chain can be tested without a model — which is
/// exactly why extraction and persistence were split into two methods.
pub fn offline_extractor() -> Extractor {
    let cfg = LlmConfig {
        provider: "deepseek".to_string(),
        model: "deepseek-v4-pro".to_string(),
        cheap_model: "deepseek-v4-flash".to_string(),
        base_url: None,
    };
    let llm = LlmClient::from_config(&cfg, "not-a-real-key").expect("should construct offline");
    Extractor::new(llm, Arc::new(HashEmbedder::new()), DEVICE)
        // HashEmbedder is a character-bigram hash, not a semantic model, so the
        // cosine between two aliases is naturally lower than a real bge-m3 would
        // give. Lower the threshold explicitly, so what the test measures is the
        // resolution logic itself.
        .with_entity_match_threshold(0.75)
}

/// Persists one episode and returns its id. A fact's `source_episode_id` is
/// NOT NULL.
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
        .expect("the episode should be persisted");
    id
}

/// Persists one episode and hangs `(name, path, ok, summary)` off it as a tool
/// trace.
///
/// Same as production's `live.rs` and the eval harness: the trace and the
/// episode share **one `write_txn`**. Splitting them into two transactions
/// would create the intermediate state "episode committed, trace not yet" —
/// and the extractor is triggered precisely after the episode commits.
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
            ordinal: i32::try_from(i).expect("a test's call count will not overflow"),
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
        .expect("episode and tool trace should be persisted in one transaction");
    id
}

pub fn ctx(episode_id: Id, occurred_at: DateTime<Utc>) -> ExtractContext {
    ExtractContext::new(episode_id, occurred_at).with_domain("coding")
}

pub fn ts(y: i32, m: u32, d: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, 0, 0, 0)
        .single()
        .expect("the dates used in tests should be valid")
}

/// A candidate fact whose object is a literal.
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

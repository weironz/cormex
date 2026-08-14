//! Integration test scaffolding.
//!
//! ## Isolation
//!
//! Every test runs in **its own temporary schema**: the connection string is
//! unchanged, only `search_path` is pointed at a freshly created schema, and the
//! migrations are then run inside it. Tests cannot see each other, and they
//! never pollute the data in the development database. At the end of a test,
//! `DROP SCHEMA ... CASCADE`.
//!
//! The "run inside a transaction and roll back afterwards" approach is not used:
//! the test for not dropping rows under concurrency has to genuinely commit, and
//! a single transaction cannot simulate the commit ordering of several
//! concurrent transactions at all.
//!
//! ## Skip rather than fail when the database is unavailable
//!
//! CI has a real Postgres; a development machine may not have `docker compose`
//! running. When no database can be reached, [`setup`] returns `None` and the
//! test simply returns — that is not a failure.

#![allow(dead_code)]

use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use cortex_core::Id;
use cortex_store::{FactSource, NewEntity, NewEpisode, NewFact, Role, Store};
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

/// Leaked temporary schemas older than this get cleaned up in passing.
///
/// When a test panics, [`TestDb::cleanup`] never runs and the schema is left
/// behind — this sweep is the backstop for that case. It goes by age rather
/// than "wipe them all" because `cargo test` runs several test binaries, and an
/// indiscriminate sweep would delete a schema a sibling process is currently
/// using. Ten minutes is far longer than the lifetime of any one test binary, so
/// there is no collateral damage.
const STALE_SCHEMA_AGE_MS: u128 = 10 * 60 * 1000;

const SCHEMA_PREFIX: &str = "cortex_test_";

pub struct TestDb {
    pub store: Store,
    admin: PgPool,
    schema: String,
}

impl TestDb {
    /// Drop the temporary schema and disconnect. Called explicitly at the end
    /// of a test.
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

    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// Execute one SQL statement inside the test schema.
    ///
    /// It exists for exactly one reason: **to construct bad data that the
    /// normal code paths cannot produce**. `Store` deliberately offers no raw
    /// write entry point (writes can only go through `write_txn`), but some
    /// failure modes require a row that should not exist in the first place —
    /// for instance a table name in `sync_log` that the payload loader does not
    /// recognize. Nowhere outside tests should use this.
    pub async fn exec_raw(&self, sql: impl Into<String>) {
        self.try_exec_raw(sql)
            .await
            .expect("the test SQL should execute successfully");
    }

    /// Run one query inside the test schema and take the **first column** back
    /// as text.
    ///
    /// It exists for `EXPLAIN`: a query plan is a multi-row text table, while
    /// `Store` exposes only domain methods and `pool()` is `pub(crate)`. Having
    /// the test open its own connection would bypass `search_path` and query the
    /// development database instead of this temporary schema.
    ///
    /// Bind parameters are not supported — the caller splices its literals in
    /// itself. That is acceptable in a test and not in production code.
    ///
    /// It has to go through `raw_sql` (the simple query protocol) rather than
    /// `query_as`: the latter uses a prepared statement, which permits only one
    /// statement at a time, whereas here we must `SET search_path` before
    /// querying — and the error it reports is `cannot insert multiple commands
    /// into a prepared statement`, which points nowhere near the actual cause.
    pub async fn query_raw(&self, sql: impl Into<String>) -> Vec<String> {
        use sqlx::Row as _;
        // `public` has to stay in search_path, or `::vector` cannot be
        // resolved -- and the error is `type "vector" does not exist`, which
        // looks like "the extension is not installed".
        // Keep this consistent with the connection options in setup()
        let stmt = format!(
            "SET search_path TO \"{}\", public; {}",
            self.schema,
            sql.into()
        );
        sqlx::raw_sql(AssertSqlSafe(stmt))
            .fetch_all(&self.admin)
            .await
            .expect("the test query should execute successfully")
            .into_iter()
            .filter_map(|r| r.try_get::<String, _>(0).ok())
            .collect()
    }

    /// Like [`Self::exec_raw`], but treats the database's refusal as a
    /// **result** rather than a panic.
    ///
    /// Used for assertions of the form "this bad data should not be writable":
    /// a CHECK constraint error is exactly what is expected, and `exec_raw`
    /// would turn that expectation into a test crash.
    pub async fn try_exec_raw(&self, sql: impl Into<String>) -> Result<(), String> {
        let stmt = format!("SET search_path TO \"{}\"; {}", self.schema, sql.into());
        sqlx::raw_sql(AssertSqlSafe(stmt))
            .execute(&self.admin)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Build an isolated test database. Returns `None` when no database can be
/// reached (the test should skip).
pub async fn setup() -> Option<TestDb> {
    setup_upto(None).await
}

/// Run migrations only up to a given version, in order to **manufacture
/// pre-existing rows**.
///
/// It exists for the same reason as [`TestDb::exec_raw`]: some failure modes
/// require the database to start in an older shape. The value that
/// `ALTER TABLE ... ADD COLUMN ... DEFAULT` leaves on pre-existing rows can
/// never be changed afterwards (changing it would take an UPDATE, and UPDATE is
/// only permitted in redact/purge), so that default has to be genuinely
/// verified — and a database with all migrations applied has **no pre-existing
/// rows at all**, so there is nothing to verify.
///
/// Afterwards call `db.store.migrate()` to apply the remaining migrations.
pub async fn setup_upto(max_version: Option<i64>) -> Option<TestDb> {
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

    // `public` stays in search_path so that the `vector` extension's types are
    // visible; tables and views all land in the first schema, i.e. the one
    // dedicated to this test.
    let options = PgConnectOptions::from_str(&url)
        .expect("DATABASE_URL should be a valid connection string")
        .options([("search_path", format!("{schema},public"))]);

    // The test for not dropping rows under concurrency runs dozens of write
    // transactions plus a reader at once, so leave enough connections that the
    // reader does not queue behind the writers and fail to get one (that would
    // mask the race).
    let pool = PgPoolOptions::new()
        .max_connections(40)
        .connect_with(options)
        .await
        .expect("it should be possible to connect once the temporary schema exists");

    // Run the migrations on the pool before handing it to Store:
    // `Store::pool()` is pub(crate) and integration tests cannot reach it (that
    // is deliberate -- writes can only go through write_txn)
    if let Some(v) = max_version {
        run_migrations_upto(&pool, v).await;
    }
    let store = Store::from_pool(pool);
    if max_version.is_none() {
        store
            .migrate()
            .await
            .expect("the migrations should run cleanly in the temporary schema");
    }

    Some(TestDb {
        store,
        admin,
        schema,
    })
}

/// Apply only the migrations with `version <= max_version`.
///
/// `Migrator`'s fields are `#[doc(hidden)]` semver-exempt fields, but this is
/// the only entry point that can "run halfway" — the alternative is copying the
/// migration SQL into the test, and then what is being tested is no longer the
/// copy in `migrations/`.
async fn run_migrations_upto(pool: &PgPool, max_version: i64) {
    static ALL: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

    let subset: Vec<_> = ALL
        .migrations
        .iter()
        .filter(|m| m.version <= max_version)
        .cloned()
        .collect();
    assert!(
        !subset.is_empty(),
        "there is no migration with version <= {max_version} -- is the version number wrong?"
    );

    let partial = sqlx::migrate::Migrator {
        migrations: subset.into(),
        ..sqlx::migrate::Migrator::DEFAULT
    };
    partial
        .run(pool)
        .await
        .expect("the first stretch of migrations should run cleanly");
}

fn database_url() -> Option<String> {
    // The test's working directory is the crate directory, and dotenvy walks
    // upward until it finds the repository root's .env
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

/// Clean up temporary schemas left behind by a previous crash (only ones old
/// enough; see [`STALE_SCHEMA_AGE_MS`]).
async fn sweep_stale_schemas(admin: &PgPool) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock should not predate 1970")
        .as_millis();

    let names: Vec<(String,)> = sqlx::query_as(
        "SELECT nspname::text FROM pg_namespace WHERE nspname LIKE 'cortex\\_test\\_%'",
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
//  Small helpers for manufacturing data
// ══════════════════════════════════════════════════════════

pub const DEVICE: &str = "test-device";
pub const MODEL: &str = "bge-m3";

pub fn new_episode(session_id: &str, text: &str) -> NewEpisode {
    NewEpisode {
        id: Id::new(),
        session_id: session_id.to_owned(),
        role: Role::User,
        content: serde_json::json!({ "role": "user", "text": text }),
        text: Some(text.to_owned()),
        // On the real path this is a space-separated string after jieba
        // segmentation; in tests the original text is good enough
        tsv_source: Some(text.to_owned()),
        domain: Some("test".to_owned()),
        device_id: DEVICE.to_owned(),
        occurred_at: Utc::now(),
    }
}

pub fn new_entity(kind: &str, name: &str) -> NewEntity {
    NewEntity {
        id: Id::new(),
        kind: kind.to_owned(),
        name: name.to_owned(),
        summary: None,
        embedding: None,
        embedding_model: MODEL.to_owned(),
        device_id: DEVICE.to_owned(),
    }
}

pub fn new_fact(subject: Id, predicate: &str, object: &str, source_episode: Id) -> NewFact {
    NewFact {
        id: Id::new(),
        subject_id: subject,
        predicate: predicate.to_owned(),
        object_text: Some(object.to_owned()),
        object_entity_id: None,
        statement: format!("{predicate} {object}"),
        tsv_source: Some(object.to_owned()),
        embedding: None,
        embedding_model: MODEL.to_owned(),
        domain: Some("test".to_owned()),
        confidence: 0.9,
        valid_at: Some(Utc::now()),
        source_episode_id: source_episode,
        // Same tier as the real extraction pipeline: it currently cannot tell
        // "the user said it themselves" apart, so everything is Conversation
        source: FactSource::Conversation,
        derived_from: Vec::new(),
        extracted_by: "test-extractor".to_owned(),
        device_id: DEVICE.to_owned(),
    }
}

pub fn now() -> DateTime<Utc> {
    Utc::now()
}

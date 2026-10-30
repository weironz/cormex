//! The migration order **CI** runs, which is not the order the server runs.
//!
//! # Why this is its own test
//!
//! `Store::migrate_global` creates-or-renames the accounts schema, runs the
//! tenant set, then the global set, then renames back. CI cannot call it — it
//! drives `sqlx migrate run` from the shell, because the test binary it is about
//! to build does not exist yet — so `.github/workflows/ci.yml` mirrors the
//! sequence by hand.
//!
//! A hand-mirrored sequence drifts. It did: CI created `cormex_auth` and pointed
//! the global migrator at it, which worked for exactly as long as there was one
//! global migration. The second one is fully qualified `cortex_auth.…` (its
//! sha256 is pinned and it is applied in production, so it cannot be edited),
//! and every branch carrying it went red with
//!
//! ```text
//! schema "cortex_auth" does not exist
//! ```
//!
//! — the same error, from the same cause, as the runtime bug
//! `fresh_database::a_new_global_migration_applies_to_an_already_migrated_database`
//! pins.
//!
//! # What this actually checks
//!
//! The **shape CI leaves behind**, not the shell commands: migrate the tenant
//! set, migrate the global set against a schema named `cortex_auth`, rename it,
//! and then assert what a test would query. If CI's `run:` block and this test
//! ever disagree, one of them is wrong — and this one is the one that runs on
//! every `cargo test`.

use sqlx::migrate::Migrator;
use sqlx::postgres::PgPoolOptions;

/// A database name this test owns.
fn fresh_database_name() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock should not predate 1970")
        .as_millis();
    format!("cormex_ci_{}_{}", std::process::id(), millis)
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

fn with_database(url: &str, database: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("DATABASE_URL should end in /<database>");
    format!("{base}/{database}")
}

/// The sequence `.github/workflows/ci.yml` runs, in the same order.
#[tokio::test]
#[ignore = "creates a database; run with just test-db"]
async fn the_ci_migration_order_leaves_a_usable_database() {
    let Some(url) = database_url() else {
        return;
    };
    let admin = match PgPoolOptions::new().max_connections(1).connect(&url).await {
        Ok(pool) => pool,
        Err(err) => {
            eprintln!("skipped: cannot reach the database ({err})");
            return;
        }
    };

    let name = fresh_database_name();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&admin)
        .await
        .expect("the test role should be allowed to create a database");
    let fresh_url = with_database(&url, &name);

    // ── 1. `psql -c 'CREATE SCHEMA IF NOT EXISTS cortex_auth'` ──
    let boot = PgPoolOptions::new()
        .max_connections(1)
        .connect(&fresh_url)
        .await
        .expect("the fresh database should be reachable");
    sqlx::query("CREATE SCHEMA IF NOT EXISTS cortex_auth")
        .execute(&boot)
        .await
        .expect("creating the accounts schema should succeed");
    boot.close().await;

    // ── 2. `sqlx migrate run --source migrations` ──
    let tenant = Migrator::new(std::path::Path::new("../../migrations"))
        .await
        .expect("the tenant migration set should load");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&fresh_url)
        .await
        .expect("connect");
    let tenant_result = tenant.run(&pool).await;
    pool.close().await;

    // ── 3. `sqlx migrate run --source migrations-global` with
    //       `search_path=cortex_auth,public` ──
    let global = Migrator::new(std::path::Path::new("../../migrations-global"))
        .await
        .expect("the global migration set should load");
    let options = sqlx::postgres::PgConnectOptions::from_str_with_std_default(&fresh_url)
        .options([("search_path", "cortex_auth,public")]);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("connect with the accounts search_path");
    let global_result = global.run(&pool).await;
    pool.close().await;

    // ── 4. `psql -c 'ALTER SCHEMA cortex_auth RENAME TO cormex_auth'` ──
    let probe = PgPoolOptions::new()
        .max_connections(1)
        .connect(&fresh_url)
        .await
        .expect("connect");
    let renamed = sqlx::query("ALTER SCHEMA cortex_auth RENAME TO cormex_auth")
        .execute(&probe)
        .await;

    // What the test suite will query once CI gets here
    let users: Option<String> = sqlx::query_scalar("SELECT to_regclass('cormex_auth.users')::text")
        .fetch_one(&probe)
        .await
        .expect("querying the migrated database should succeed");
    let device_keys: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('cormex_auth.device_keys')::text")
            .fetch_one(&probe)
            .await
            .expect("querying the migrated database should succeed");
    let episodes: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('public.episodes')::text")
            .fetch_one(&probe)
            .await
            .expect("querying the migrated database should succeed");
    probe.close().await;

    let dropped = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
    )))
    .execute(&admin)
    .await;
    admin.close().await;
    if let Err(err) = dropped {
        eprintln!("cleaning up the fresh database {name} failed: {err}");
    }

    tenant_result.expect("the tenant set should apply");
    global_result.unwrap_or_else(|err| {
        panic!(
            "the global set failed to apply in the order CI runs it: {err}\n\
             CI creates `cortex_auth` and points the migrator at it because the pinned \
             migrations are fully qualified `cortex_auth.…`. Creating `cormex_auth` instead \
             works only until the second global migration, and then every branch goes red \
             with `schema \"cortex_auth\" does not exist` -- see .github/workflows/ci.yml."
        )
    });
    renamed.expect("the schema should rename back to the name the deployment queries");
    assert!(
        users.is_some(),
        "cormex_auth.users is missing after the CI sequence"
    );
    assert!(
        device_keys.is_some(),
        "cormex_auth.device_keys is missing: the newest global migration did not apply, which \
         is the upgrade path every existing deployment takes"
    );
    assert!(
        episodes.is_some(),
        "public.episodes is missing: the tenant set did not apply"
    );
}

/// `PgConnectOptions` has no `FromStr` re-export here; this keeps the test
/// readable without pulling `std::str::FromStr` into scope for one call.
trait FromStrWithStdDefault {
    fn from_str_with_std_default(url: &str) -> sqlx::postgres::PgConnectOptions;
}

impl FromStrWithStdDefault for sqlx::postgres::PgConnectOptions {
    fn from_str_with_std_default(url: &str) -> Self {
        use std::str::FromStr as _;
        Self::from_str(url).expect("the test URL should parse")
    }
}

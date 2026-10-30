//! `Store::migrate_global` has to work on a database that has never been
//! migrated.
//!
//! # Why a brand-new database rather than the usual temporary schema
//!
//! Every other test in this crate runs inside a temporary schema that sits in
//! an **already-migrated** database, so `public` is populated and the two
//! DOMAINs (`ulid`, `sha256`) are there. That is exactly the shape our own
//! deployment had, and it is why the ordering bug went unnoticed for three
//! months: the global set uses those DOMAINs but runs first, so a genuinely
//! empty database stopped at
//!
//! ```text
//! type "ulid" does not exist
//! ```
//!
//! and the server restart-looped. Only a database created from nothing can
//! reproduce that, which is why this test creates one.
//!
//! # Why it is `#[ignore]`
//!
//! `CREATE DATABASE` cannot run inside a transaction and needs a role that may
//! create databases. CI's Postgres role has it, but a development machine
//! pointed at a shared server may not — and a test that fails for a reason
//! unrelated to the code under test is worse than one that is skipped on
//! demand. `just test-db` runs it with `--include-ignored`.

use cormex_store::Store;
use sqlx::postgres::PgPoolOptions;

/// A database name that cannot collide with anything: the process id and the
/// wall clock, both in the `cormex_fresh_` namespace this test owns.
fn fresh_database_name() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the system clock should not predate 1970")
        .as_millis();
    format!("cormex_fresh_{}_{}", std::process::id(), millis)
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

/// Point a connection string at a different database on the same server.
///
/// `DATABASE_URL` is `postgres://user:pass@host:port/db`; only the last path
/// segment changes. Parsing it with `PgConnectOptions` and writing it back out
/// would be the tidy way, but that type has no `Display` — and the URL form is
/// what `migrate_global` takes.
fn with_database(url: &str, database: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("DATABASE_URL should end in /<database>");
    format!("{base}/{database}")
}

#[tokio::test]
#[ignore = "creates a database; run with just test-db"]
async fn migrate_global_works_on_an_empty_database() {
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
    // The name is generated here, never taken from input, so splicing it in is
    // not an injection surface; identifiers cannot be bound as parameters
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
        .execute(&admin)
        .await
        .expect("the test role should be allowed to create a database");

    let fresh_url = with_database(&url, &name);

    // Every step records its outcome instead of asserting on the spot: the
    // database has to be dropped whether the migration passed or not, and a
    // panic would skip the drop. Only the last step below turns a failure into
    // a panic, after the cleanup has run.
    let migrated = Store::migrate_global(&fresh_url).await;

    let (tenant, global) = if migrated.is_ok() {
        let probe = PgPoolOptions::new()
            .max_connections(1)
            .connect(&fresh_url)
            .await
            .expect("the fresh database should be reachable after migrating it");
        let tenant: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('public.episodes')::text")
                .fetch_one(&probe)
                .await
                .expect("querying the fresh database should succeed");
        let global: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('cormex_auth.users')::text")
                .fetch_one(&probe)
                .await
                .expect("querying the fresh database should succeed");
        probe.close().await;
        (tenant, global)
    } else {
        (None, None)
    };

    let dropped = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
    )))
    .execute(&admin)
    .await;
    admin.close().await;
    if let Err(err) = dropped {
        eprintln!("cleaning up the fresh database {name} failed: {err}");
    }

    if let Err(err) = migrated {
        panic!(
            "Store::migrate_global failed on an empty database: {err}\n\
             A fresh deployment would restart-loop here. The global set uses the `ulid` \
             DOMAIN, which the tenant set creates in `public`, so the tenant set has to run \
             first -- see Store::migrate_global's docs."
        );
    }
    assert!(
        tenant.is_some(),
        "public.episodes is missing after migrate_global: the tenant set did not run, \
         so the DOMAINs the global set needs were never created"
    );
    assert!(
        global.is_some(),
        "cormex_auth.users is missing after migrate_global: the global set did not run"
    );
}

/// The **upgrade** path: an already-migrated database that gains a new global
/// migration.
///
/// This is the shape every existing deployment is in, and it is the one that
/// broke: the accounts schema is `cormex_auth` on disk, the new migration's SQL
/// is fully qualified `cortex_auth.…`, and the migrator was pointed at
/// `cormex_auth`. The result was
///
/// ```text
/// schema "cortex_auth" does not exist
/// ```
///
/// on a database that had been running for months — while the fresh-database
/// test above passed, because a genuinely empty database creates `cortex_auth`
/// first and only renames it afterwards.
///
/// The state is produced the way a real upgrade produces it: migrate once, then
/// un-apply the newest global migration by hand (drop its table, remove its
/// version row). Nothing else about the database changes.
#[tokio::test]
#[ignore = "creates a database; run with just test-db"]
async fn a_new_global_migration_applies_to_an_already_migrated_database() {
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

    // Run everything, then roll the newest global migration back by hand.
    let first = Store::migrate_global(&fresh_url).await;
    let rolled_back = async {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(&fresh_url)
            .await?;
        sqlx::query("DROP TABLE IF EXISTS cormex_auth.device_keys")
            .execute(&pool)
            .await?;
        sqlx::query("DELETE FROM cormex_auth._sqlx_migrations WHERE version = 20261101000001")
            .execute(&pool)
            .await?;
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM cormex_auth._sqlx_migrations")
            .fetch_one(&pool)
            .await?;
        pool.close().await;
        Ok::<i64, sqlx::Error>(left)
    }
    .await;

    // The second run is the upgrade.
    let second = Store::migrate_global(&fresh_url).await;

    let (schema, device_keys) = if second.is_ok() {
        let probe = PgPoolOptions::new()
            .max_connections(1)
            .connect(&fresh_url)
            .await
            .expect("the database should be reachable after the upgrade");
        let schema: Option<String> = sqlx::query_scalar(
            "SELECT nspname FROM pg_namespace WHERE nspname IN ('cormex_auth', 'cortex_auth')",
        )
        .fetch_one(&probe)
        .await
        .expect("querying the database should succeed");
        let device_keys: Option<String> =
            sqlx::query_scalar("SELECT to_regclass('cormex_auth.device_keys')::text")
                .fetch_one(&probe)
                .await
                .expect("querying the database should succeed");
        probe.close().await;
        (schema, device_keys)
    } else {
        (None, None)
    };

    let dropped = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
    )))
    .execute(&admin)
    .await;
    admin.close().await;
    if let Err(err) = dropped {
        eprintln!("cleaning up the fresh database {name} failed: {err}");
    }

    first.expect("the first migrate_global should succeed on an empty database");
    let left = rolled_back.expect("rolling the newest global migration back should succeed");
    assert_eq!(
        left, 1,
        "the rollback should leave exactly the first global migration applied"
    );
    second.unwrap_or_else(|err| {
        panic!(
            "migrate_global failed on an already-migrated database gaining a new global \
             migration: {err}\n\
             This is the upgrade path every existing deployment takes. The pinned global SQL \
             hard-codes `cortex_auth`, so the migrator has to run against a schema by that \
             name -- see Store::migrate_global's docs."
        )
    });
    assert_eq!(
        schema.as_deref(),
        Some("cormex_auth"),
        "after the upgrade the accounts schema must be back under the name the deployment \
         queries, not the one the migrations hard-code"
    );
    assert!(
        device_keys.is_some(),
        "cormex_auth.device_keys is missing after the upgrade: the new global migration did \
         not run"
    );
}

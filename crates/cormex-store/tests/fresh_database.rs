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

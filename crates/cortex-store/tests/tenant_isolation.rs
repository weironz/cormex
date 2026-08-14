//! **The one place in this change where being wrong is catastrophic.**
//!
//! Under multi-user, isolation rests on "each connection's `search_path`
//! contains only its own schema". Once that breaks the symptom is not an error —
//! it is a few unfamiliar memories showing up in somebody's retrieval, and those
//! may be another person's medical records, password hints, or home address.
//!
//! So this file does not test "is the function's return value right"; it tests
//! that **two real tenants in the same database cannot see each other**. It runs
//! against a real database rather than a mock: inside a mock the concept of
//! `search_path` does not exist at all, and `search_path` is exactly what is
//! being verified here.

use std::sync::Arc;

use cortex_store::{NewEntity, NewFact, SchemaName, Store, TenantPools};
use sqlx::AssertSqlSafe;
use sqlx::postgres::PgPoolOptions;

fn database_url() -> Option<String> {
    let url = std::env::var("DATABASE_URL").ok()?;
    (!url.trim().is_empty()).then_some(url)
}

/// Create a real tenant schema and pour the migrations into it — the same thing
/// production does when provisioning an account.
async fn make_tenant(url: &str, pools: &TenantPools, schema: &SchemaName) -> Arc<Store> {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .expect("the database should be reachable");
    // AssertSqlSafe: sqlx 0.9 requires an explicit vouch for concatenated SQL.
    // It is affordable here -- `SchemaName`'s constructor has already pinned it
    // to `public` or `u_` + a 26-character ULID, which is precisely what this
    // test file guards
    sqlx::query(AssertSqlSafe(format!(
        r#"CREATE SCHEMA IF NOT EXISTS "{}""#,
        schema.as_str()
    )))
    .execute(&admin)
    .await
    .expect("the schema should be creatable");
    admin.close().await;

    let store = pools
        .get(schema)
        .await
        .expect("the tenant pool should be obtainable");
    store
        .migrate()
        .await
        .expect("the migrations should run cleanly in the tenant schema");
    store
}

async fn drop_tenant(url: &str, schema: &SchemaName) {
    let Ok(admin) = PgPoolOptions::new().max_connections(1).connect(url).await else {
        return;
    };
    let _ = sqlx::query(AssertSqlSafe(format!(
        r#"DROP SCHEMA IF EXISTS "{}" CASCADE"#,
        schema.as_str()
    )))
    .execute(&admin)
    .await;
    admin.close().await;
}

/// Write one retrievable fact into a tenant and return its statement.
async fn seed(store: &Store, who: &str) -> String {
    let entity = cortex_core::Id::new();
    let episode = cortex_core::Id::new();
    let statement = format!("{who} 的私密事实");

    store
        .write_txn(async |tx| {
            tx.insert_entity(&NewEntity {
                id: entity,
                name: format!("{who}-实体"),
                kind: "person".into(),
                summary: None,
                embedding: None,
                embedding_model: "hash-stub-v1".into(),
                device_id: "test".into(),
            })
            .await?;
            tx.insert_episode(&cortex_store::NewEpisode {
                id: episode,
                session_id: cortex_core::Id::new().to_string(),
                role: cortex_store::Role::User,
                content: serde_json::json!([{"type": "text", "text": "x"}]),
                text: Some("x".into()),
                tsv_source: None,
                domain: None,
                device_id: "test".into(),
                occurred_at: chrono::Utc::now(),
            })
            .await?;
            tx.insert_fact(&NewFact {
                id: cortex_core::Id::new(),
                subject_id: entity,
                predicate: "knows".into(),
                object_text: Some(who.into()),
                object_entity_id: None,
                statement: statement.clone(),
                tsv_source: Some(format!("{who} 私密")),
                embedding: None,
                embedding_model: "hash-stub-v1".into(),
                domain: None,
                confidence: 0.9,
                valid_at: None,
                source_episode_id: episode,
                source: cortex_store::FactSource::Conversation,
                derived_from: Vec::new(),
                extracted_by: "test".into(),
                device_id: "test".into(),
            })
            .await?;
            Ok(())
        })
        .await
        .expect("the write should go through");

    statement
}

/// **Neither tenant can see a single row belonging to the other.**
///
/// How to fault-inject: remove the `.options(search_path)` line from
/// `TenantPools::connect` and both sides land on `public`, turning this test red
/// immediately.
#[tokio::test]
async fn two_tenants_cannot_see_each_others_rows() {
    let Some(url) = database_url() else {
        eprintln!("跳过：没有 DATABASE_URL");
        return;
    };

    let pools = TenantPools::new(&url);
    let a = SchemaName::derive(&cortex_core::Id::new().to_string())
        .expect("the schema name should derive");
    let b = SchemaName::derive(&cortex_core::Id::new().to_string())
        .expect("the schema name should derive");

    let store_a = make_tenant(&url, &pools, &a).await;
    let store_b = make_tenant(&url, &pools, &b).await;

    let secret_a = seed(&store_a, "甲").await;
    let secret_b = seed(&store_b, "乙").await;

    // Every retrieval channel must be checked: missing one is missing one leak
    // channel
    for (store, mine, theirs, who) in [
        (&store_a, &secret_a, &secret_b, "甲"),
        (&store_b, &secret_b, &secret_a, "乙"),
    ] {
        let hits = store
            .recall_bm25("私密", 50)
            .await
            .expect("BM25 should run");
        let statements: Vec<&str> = hits.iter().map(|f| f.statement.as_str()).collect();

        assert!(
            statements.contains(&mine.as_str()),
            "{who} should be able to retrieve the fact they wrote themselves, otherwise this \
             test is not testing isolation at all: {statements:?}"
        );
        assert!(
            !statements.contains(&theirs.as_str()),
            "**{who} retrieved another tenant's fact** -- search_path is not taking effect, and \
             this is a cross-user data leak: {statements:?}"
        );
    }

    // The counts must line up too: exactly one's own row
    for (store, who) in [(&store_a, "甲"), (&store_b, "乙")] {
        let all = store
            .recall_bm25("私密", 50)
            .await
            .expect("BM25 should run");
        assert_eq!(
            all.len(),
            1,
            "{who}'s database should hold only their own single fact; it holds {}",
            all.len()
        );
    }

    drop(store_a);
    drop(store_b);
    drop_tenant(&url, &a).await;
    drop_tenant(&url, &b).await;
}

/// Asking for the same schema repeatedly builds only one pool.
///
/// If that does not hold, every request opens 4 new connections and a few dozen
/// requests exhaust `max_connections` — with the symptom "nobody can connect to
/// the database".
#[tokio::test]
async fn asking_twice_for_the_same_tenant_reuses_one_pool() {
    let Some(url) = database_url() else {
        eprintln!("跳过：没有 DATABASE_URL");
        return;
    };

    let pools = TenantPools::new(&url);
    let schema = SchemaName::new("public").expect("public is a valid schema name");

    let first = pools
        .get(&schema)
        .await
        .expect("the first get should succeed");
    let second = pools
        .get(&schema)
        .await
        .expect("the second get should succeed");

    assert!(
        Arc::ptr_eq(&first, &second),
        "getting the same schema twice returned two pools -- that means every request opens new \
         connections"
    );
    assert_eq!(pools.resident_count().await, 1);
}

/// **Two tenants' write transactions must not queue behind each other.**
///
/// `pg_advisory_xact_lock` used to take a single global key, so while one person
/// imported three years of history nobody else could write a single sentence.
/// Switched to the two-argument form (classid + tenant key), the two tenants
/// each take their own lock.
///
/// How it is tested: tenant A opens a write transaction and **holds it open**
/// while tenant B writes. B must be able to finish — under a global lock it
/// would wait until A commits.
#[tokio::test]
async fn one_tenants_long_write_does_not_block_another() {
    let Some(url) = database_url() else {
        eprintln!("跳过：没有 DATABASE_URL");
        return;
    };

    let pools = TenantPools::new(&url);
    let a = SchemaName::derive(&cortex_core::Id::new().to_string())
        .expect("the schema name should derive");
    let b = SchemaName::derive(&cortex_core::Id::new().to_string())
        .expect("the schema name should derive");
    let store_a = make_tenant(&url, &pools, &a).await;
    let store_b = make_tenant(&url, &pools, &b).await;

    assert_ne!(
        a.lock_key(),
        b.lock_key(),
        "the two tenants computed the same lock key -- their writes would still queue behind each \
         other"
    );

    // Tenant A opens a slow transaction: take the lock, then sleep without
    // committing
    let slow = {
        let store_a = Arc::clone(&store_a);
        tokio::spawn(async move {
            store_a
                .write_txn(async |_tx| {
                    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
                    Ok(())
                })
                .await
        })
    };
    // Let A take the lock first
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // B writes at the same time. Under a global lock this would wait out A's
    // full 1.5 seconds
    let started = std::time::Instant::now();
    seed(&store_b, "乙").await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_millis(1000),
        "B took {elapsed:?} to finish writing -- it was waiting on A's lock, so the two tenants \
         still queue behind each other"
    );

    slow.await
        .expect("A's transaction task should finish")
        .expect("A's write should succeed");
    drop(store_a);
    drop(store_b);
    drop_tenant(&url, &a).await;
    drop_tenant(&url, &b).await;
}

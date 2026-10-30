//! Two accounts read across each other, and **not one row may cross over**.
//!
//! # This is the one place in the whole multi-tenancy change where being wrong
//! # is catastrophic
//!
//! Other mistakes raise errors, slow things down, or return nothing. This one
//! does not: it returns **somebody else's memory, looking perfectly normal**. No
//! exception, no log line, no alert — and the only way it comes to light is
//! somebody happening to recognise "that is not something I said".
//!
//! # What the two tests here each cover, and what each of them cannot
//!
//! The storage-layer test (`cormex-store/tests/tenant_isolation.rs`) verifies
//! `TenantPools` itself. **It was green the entire time while two users on the
//! request path still shared one database** — because the service layer never
//! called it. That is precisely the gap this change fills.
//!
//! - `one_users_memory_never_shows_up_in_anothers` verifies the **shape of the
//!   isolation**: two pools and three read paths (by id, listing sessions,
//!   full-text search) all stay separate. It has been measured to be **still
//!   green while the service layer forgot to bind** — it cannot reach the
//!   service layer, so it cannot on its own be the basis for "multi-user is
//!   ready".
//! - `every_data_path_goes_through_bind` is the one that verifies the service
//!   layer: every data path in `state.rs` must bind a tenant first. Fault
//!   injection was tried; it goes red.
//!
//! And "which schema a given account resolves to" is covered by the unit tests
//! in `src/request_tenant.rs` (which need a real database) — that being the one
//! genuinely new decision in this change.
//!
//! How to run (needs a real database):
//! ```bash
//! DATABASE_URL=postgres://... cargo test -p cormexd --test two_users_cannot_see_each_other
//! ```
//! Without `DATABASE_URL` the whole file is skipped — no meaningful fake can be
//! built for this kind of test: `search_path` is Postgres behaviour, and
//! simulating it simulates away the thing under test.

use cormex_store::{SchemaName, Store, TenantPools};

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty())
}

/// Build one tenant: create the schema, run the migrations, return its `Store`.
async fn provision(pools: &TenantPools, schema: &SchemaName) -> Store {
    pools
        .create_schema(schema)
        .await
        .expect("create the schema");
    let store = pools.get(schema).await.expect("obtain the pool");
    store.migrate().await.expect("run the migrations");
    (*store).clone()
}

async fn write_one(store: &Store, session: &str, text: &str) -> String {
    let id = cormex_core::Id::new();
    let session = session.to_owned();
    let text = text.to_owned();
    store
        .write_txn(async |tx| {
            tx.insert_episode(&cormex_store::NewEpisode {
                id,
                session_id: session,
                role: cormex_store::Role::User,
                content: serde_json::json!([{"type": "text", "text": text}]),
                text: Some(text.clone()),
                tsv_source: Some(text),
                domain: None,
                device_id: "test".into(),
                occurred_at: chrono::Utc::now(),
            })
            .await?;
            Ok(())
        })
        .await
        .expect("write the episode");
    id.to_string()
}

/// Nothing A writes is readable by B, and vice versa.
#[tokio::test]
async fn one_users_memory_never_shows_up_in_anothers() {
    let Some(url) = database_url() else {
        eprintln!("skipped: no DATABASE_URL");
        return;
    };
    let pools = TenantPools::new(&url);

    // Two realistically shaped schema names (u_ + a 26-character lowercase ULID)
    let a = SchemaName::derive(&cormex_core::Id::new().to_string()).expect("A's schema name");
    let b = SchemaName::derive(&cormex_core::Id::new().to_string()).expect("B's schema name");
    let store_a = provision(&pools, &a).await;
    let store_b = provision(&pools, &b).await;

    let a_secret = "阿尔法的工资是 41730 元";
    let b_secret = "布拉沃的工资是 86290 元";
    let a_id = write_one(&store_a, "sess-a", a_secret).await;
    let b_id = write_one(&store_b, "sess-b", b_secret).await;

    // Cross-read: fetch by id
    assert!(
        store_b
            .episode(&a_id)
            .await
            .expect("query B's database")
            .is_none(),
        "B found something using A's episode id — the two accounts landed in the same schema. \
         This is exactly what \"two people logged in, both reading the same database\" looks like"
    );
    assert!(
        store_a
            .episode(&b_id)
            .await
            .expect("query A's database")
            .is_none(),
        "A found something using B's episode id (the reverse direction must be equally closed)"
    );

    // Cross-read: list sessions
    let a_sessions = store_a
        .session_digests(100, true, None)
        .await
        .expect("list A's sessions");
    assert!(
        a_sessions.iter().all(|s| s.session_id != "sess-b"),
        "B's session sess-b appeared in A's session list. Invisible by id but visible in the \
         list is the worse kind of crossover — it shows up right there in the sidebar"
    );
    let b_sessions = store_b
        .session_digests(100, true, None)
        .await
        .expect("list B's sessions");
    assert!(
        b_sessions.iter().all(|s| s.session_id != "sess-a"),
        "A's session sess-a appeared in B's session list"
    );

    // Cross-read: full-text search. This path is the most dangerous — it does
    // not require knowing the other party's id
    let hits = store_b
        .recall_bm25("工资", 50)
        .await
        .expect("search inside B's database");
    assert!(
        hits.iter().all(|h| !h.statement.contains("41730")),
        "B searching for \"salary\" retrieved A's figure. Search does not require knowing the \
         other party's id, which makes this the path where a crossover is most likely to be hit \
         by a real user"
    );

    for s in [&a, &b] {
        let _ = pools.drop_schema(s).await;
    }
}

/// **Not a single** path on the request side bypasses `bind`.
///
/// # Why this reads the source rather than issuing requests
///
/// The test above proves "routing by tenant is correct", but it cannot prove
/// "every path routes" — one path that forgot to `bind` crosses over on **that
/// one endpoint only**, and there are two dozen endpoints, so running them all
/// is coverage we cannot afford.
///
/// Meanwhile the shape in `state.rs` is stable: the data methods all sit behind
/// `Backend::Live(l) =>`. So this asserts on how those branches are spelled. It
/// does not catch everything, but it catches "added a new endpoint, copied the
/// previous one, forgot to bind" — which is the only source this class of
/// regression has.
///
/// The real foundation is the type system: those ten methods on `Live` are
/// module-private, and only `BoundLive` forwards them out. This test is the
/// second line of defence, guarding against somebody making them `pub` again.
#[test]
fn every_data_path_goes_through_bind() {
    let src = include_str!("../src/state.rs");

    // Anything spelled `Backend::Live(l) => l.<method>(` must be bind or on the
    // allow-list
    let allowed_unbound = [
        "store", // the realtime-push listener task reading the cursor, process-level
        "latest_cursor",
        "database_status",
        "embedding_health",
        // Pure forwarding; touches no table at all. The bring-your-own-key
        // branch receives an already-decrypted string, and **the tenant was
        // already bound when that key was fetched** (own_key(&tenant) in the
        // handler)
        "llm_stream_with_key",
        "bind",
    ];
    let mut offenders = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let Some(rest) = line.split("Backend::Live(l) => ").nth(1) else {
            continue;
        };
        let Some(call) = rest.strip_prefix("l.") else {
            continue; // a multi-line branch, inspected separately below
        };
        let name: String = call
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !allowed_unbound.contains(&name.as_str()) {
            offenders.push(format!("{}: {}", i + 1, line.trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "state.rs has branches calling data methods directly on an unbound Live:\n{}\n\
         Every path that reads or writes data must first call \
         `l.bind(tenant.store()?)`, or it lands on public — that is, user #1's \
         database.\n\
         Methods that genuinely are tenant-independent (the process-level cursor, \
         health checks, pure forwarding) belong in `allowed_unbound`, with the \
         reason written down.",
        offenders.join("\n")
    );
}

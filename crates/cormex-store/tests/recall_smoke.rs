//! Smoke test for the four retrieval channels.
//!
//! It verifies only that the SQL is syntactically correct and the return types
//! line up — ranking quality belongs to retrieval evaluation (see the P1
//! "retrieval evaluation trio" in docs/roadmap.md) and is not asserted here.

mod common;

use chrono::Utc;

#[tokio::test]
async fn four_channels_execute_against_real_schema() {
    let Some(ctx) = common::setup().await else {
        eprintln!("skipped: the database is unavailable");
        return;
    };
    let store = &ctx.store;

    // Empty input must return safely: it can neither panic nor assemble an
    // illegal tsquery
    assert!(store.recall_bm25("", 10).await.unwrap().is_empty());
    assert!(store.recall_graph(&[], 2, 10).await.unwrap().is_empty());
    assert!(store.entities_matching(&[], 10).await.unwrap().is_empty());

    // Each of the four channels runs
    store
        .recall_bm25("对象 | 存储", 10)
        .await
        .expect("BM25 retrieval");
    let v = cormex_store::Vector::from(vec![0.1f32; cormex_store::EMBEDDING_DIM]);
    store
        .recall_vector(&v, "bge-m3", 10)
        .await
        .expect("vector retrieval");
    store.recall_recent(7, 10).await.expect("recency retrieval");
    store
        .recall_graph(&["01JZZZZZZZZZZZZZZZZZZZZZZ1".into()], 2, 10)
        .await
        .expect("graph traversal retrieval");
    store
        .facts_as_of(Utc::now(), 10)
        .await
        .expect("time replay");

    // An out-of-range limit must be clamped rather than making the database
    // raise an error
    store
        .recall_recent(99999, 99999)
        .await
        .expect("limit should be clamped");
}

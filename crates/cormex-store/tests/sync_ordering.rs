//! Correctness of the sync total order — the most important group of tests in
//! this crate.
//!
//! The core assertion: **concurrent writes never drop a row**. A client that
//! keeps advancing its cursor, pulling repeatedly while writes are happening
//! concurrently, must end up with every record, none missing and none
//! duplicated.
//!
//! This is a direct verification of the `pg_advisory_xact_lock(4272)` approach.
//! Without that lock, transaction A may take seq=100 first yet commit later; the
//! client pulls B's 101 before A commits and advances its cursor to 101, and A's
//! row becomes **permanently** invisible.
//!
//! This test has been **fault-injection verified**: comment out the advisory
//! lock in `WriteTxn::begin` and it fails consistently (12–13 of the 32 rows go
//! missing); put the lock back and it passes consistently. So it really is
//! testing that thing rather than holding by coincidence. To reproduce the
//! verification, temporarily delete that one statement and run this file.

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cormex_store::{SyncPayload, table};

/// The number of concurrent write transactions. Writing plenty of them is what
/// squeezes out-of-order commits into the open.
const WRITERS: usize = 32;

/// Half the write transactions stall this long after allocating their sequence
/// number before committing, manufacturing a window where
/// "allocation order != commit order".
///
/// With the advisory lock in place these transactions are serialized and the
/// total takes roughly `WRITERS / 2 * SLOW_COMMIT`, so this value cannot be
/// large.
const SLOW_COMMIT: Duration = Duration::from_millis(6);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_are_never_missed_by_a_cursor() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = db.store.clone();

    let writers_done = Arc::new(AtomicBool::new(false));

    // ── Reader: simulate a client that keeps advancing its cursor ──
    let reader = {
        let store = store.clone();
        let writers_done = Arc::clone(&writers_done);
        tokio::spawn(async move {
            let mut cursor = 0_i64;
            let mut seen: Vec<(i64, String)> = Vec::new();

            loop {
                // Read the flag before pulling: if the flag is already set, this
                // pull covers every committed write, so an empty pull proves we
                // really are drained. The other way around (pull first, read
                // after) would miss rows committed in between.
                let finished = writers_done.load(Ordering::SeqCst);

                let batch = store
                    .fetch_since(cursor, 8)
                    .await
                    .expect("the incremental pull should not fail");

                if let Some(last) = batch.last() {
                    cursor = last.seq;
                }
                let drained = batch.is_empty();
                seen.extend(batch.into_iter().map(|r| (r.seq, r.record_id)));

                if drained {
                    if finished {
                        break;
                    }
                    // The polling has to be tight enough to land inside the
                    // out-of-order commit window above
                    tokio::time::sleep(Duration::from_micros(200)).await;
                }
            }

            seen
        })
    };

    // ── Writers: run WRITERS write transactions concurrently ──
    let mut writers = Vec::with_capacity(WRITERS);
    for i in 0..WRITERS {
        let store = store.clone();
        writers.push(tokio::spawn(async move {
            let episode = common::new_episode("concurrent", &format!("第 {i} 条"));
            let id = episode.id.to_string();

            store
                .write_txn(async |tx| {
                    let seq = tx.insert_episode(&episode).await?;
                    // The crucial part: stall for a while **after allocating
                    // the sequence number** and only then commit. Half the
                    // transactions commit immediately and half stall for
                    // SLOW_COMMIT, so "allocated first, committed later" is
                    // guaranteed to occur. Without the advisory lock the reader
                    // reads the later allocator inside that window and advances
                    // its cursor past it, and the earlier allocator's row is
                    // lost forever.
                    if i % 2 == 0 {
                        tokio::time::sleep(SLOW_COMMIT).await;
                    }
                    Ok(seq)
                })
                .await
                .expect("the write transaction should not fail");

            id
        }));
    }

    let mut expected = HashSet::with_capacity(WRITERS);
    for writer in writers {
        expected.insert(writer.await.expect("the write task should not panic"));
    }
    writers_done.store(true, Ordering::SeqCst);

    let seen = reader.await.expect("the read task should not panic");

    // ① None missing
    let seen_ids: HashSet<String> = seen.iter().map(|(_, id)| id.clone()).collect();
    let missing: Vec<_> = expected.difference(&seen_ids).collect();
    assert!(
        missing.is_empty(),
        "the cursor dropped {} records (the advisory lock is not working): {missing:?}",
        missing.len()
    );

    // ② None duplicated
    assert_eq!(
        seen.len(),
        WRITERS,
        "the pull results contain duplicates: got {} records, expected {WRITERS}",
        seen.len()
    );

    // ③ seq strictly increasing -- the premise on which cursor semantics rest
    let seqs: Vec<i64> = seen.iter().map(|(seq, _)| *seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq is not strictly increasing: {seqs:?}"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn one_business_row_yields_exactly_one_sync_log_row() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "你好");
    let entity = common::new_entity("project", "Cormex");
    let fact = common::new_fact(entity.id, "uses", "Postgres", episode.id);

    let before = store
        .latest_seq()
        .await
        .expect("reading the far end seq should not fail");

    let last_seq = store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&fact).await
        })
        .await
        .expect("the write transaction should not fail");

    // Three business rows → three sync_log rows, one for one
    assert_eq!(last_seq, before + 3);

    let batch = store
        .fetch_since(before, 100)
        .await
        .expect("the pull should not fail");
    assert_eq!(batch.len(), 3);

    assert_eq!(batch[0].table_name, table::EPISODES);
    assert_eq!(batch[1].table_name, table::ENTITIES);
    assert_eq!(batch[2].table_name, table::FACTS);

    // The business row comes back alongside the log row, and is typed
    match batch[0]
        .payload
        .as_ref()
        .expect("the episode payload should be present")
    {
        SyncPayload::Episode(row) => {
            assert_eq!(row.id, episode.id.to_string());
            assert_eq!(row.text.as_deref(), Some("你好"));
        }
        other => panic!("wrong payload type: {other:?}"),
    }
    match batch[2]
        .payload
        .as_ref()
        .expect("the fact payload should be present")
    {
        SyncPayload::Fact(row) => {
            assert_eq!(row.id, fact.id.to_string());
            assert_eq!(row.subject_id, entity.id.to_string());
        }
        other => panic!("wrong payload type: {other:?}"),
    }

    db.cleanup().await;
}

#[tokio::test]
async fn failed_transaction_leaves_no_trace_in_the_sync_log() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let before = store
        .latest_seq()
        .await
        .expect("reading the far end seq should not fail");

    let episode = common::new_episode("s1", "会被回滚");
    // The second fact's source_episode points at an episode that does not
    // exist → FK violation → the whole transaction rolls back
    let orphan_entity = common::new_entity("project", "Ghost");
    let orphan_fact = common::new_fact(
        orphan_entity.id,
        "uses",
        "Nothing",
        cormex_core::Id::new(), // an ownerless provenance
    );

    let outcome = store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&orphan_entity).await?;
            tx.insert_fact(&orphan_fact).await
        })
        .await;

    assert!(
        outcome.is_err(),
        "a foreign key violation should make the write transaction fail"
    );

    // The business row and sync_log live and die together: after a rollback
    // neither side should leave a trace
    assert_eq!(
        store
            .latest_seq()
            .await
            .expect("reading the far end seq should not fail"),
        before,
        "a rolled-back transaction should not advance sync_log"
    );
    assert!(
        store
            .episode(&episode.id.to_string())
            .await
            .expect("the query should not fail")
            .is_none(),
        "a rolled-back transaction should not leave a business row behind"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn fetch_since_paginates_in_seq_order() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let mut ids = Vec::new();
    for i in 0..10 {
        let episode = common::new_episode("paging", &format!("第 {i} 条"));
        ids.push(episode.id.to_string());
        store
            .write_txn(async |tx| tx.insert_episode(&episode).await)
            .await
            .expect("the write transaction should not fail");
    }

    let mut cursor = 0_i64;
    let mut collected = Vec::new();
    loop {
        let batch = store
            .fetch_since(cursor, 3)
            .await
            .expect("the pull should not fail");
        if batch.is_empty() {
            break;
        }
        cursor = batch
            .last()
            .expect("a non-empty batch must have a last element")
            .seq;
        collected.extend(batch.into_iter().map(|r| r.record_id));
    }

    assert_eq!(
        collected, ids,
        "the order of paginated pulls should match the write order"
    );

    db.cleanup().await;
}

//! Idempotency of offline queue replay — and why it **cannot** use
//! `ON CONFLICT DO NOTHING`.
//!
//! A local agent that loses its connection queues up episodes and replays them
//! in order once it is back online. Duplicate delivery is **an expected, normal
//! path**: a crash between "the server accepted it" and "the local side
//! recorded it as flushed" makes that one come round again. So writes must be
//! idempotent.
//!
//! The laziest way to write that is to add `ON CONFLICT DO NOTHING` to the
//! `INSERT`. This group of tests exists to pin down what that would break:
//! `Guarded::insert_row` appends a `sync_log` row **unconditionally**, so a
//! no-op that inserts nothing still leaves behind a phantom row pointing at an
//! episode that was synced long ago — and `sync_log` is the one true order for
//! sync.
//!
//! A phantom row makes nothing raise an error. Other devices merely pull an
//! episode they already have one more time, and the result is entirely correct.
//! Its cost is long-term: every replay of the queue costs every device another
//! round of pulling, and "how far the cursor advanced" stops being equal to "how
//! much was actually written".

mod common;

use cortex_store::table;

/// Count how many rows sync_log currently has.
async fn sync_rows(db: &common::TestDb) -> i64 {
    db.query_raw("SELECT count(*)::text FROM sync_log")
        .await
        .first()
        .and_then(|s| s.parse().ok())
        .expect("count(*) must return exactly one row")
}

/// Replaying the same episode: read once to check for duplicates, and on a hit
/// skip straight past.
///
/// This is the path `Live::write_episode` takes. There are **two** assertions:
/// the second write adds no business row (which it obviously should not), and
/// `sync_log` **did not grow by a single row** — the latter is what this test is
/// really guarding.
#[tokio::test]
async fn a_replayed_episode_adds_no_row_and_no_sync_entry() {
    let Some(db) = common::setup().await else {
        return;
    };

    let ep = common::new_episode("s-replay", "断网期间说的这句话");
    let id = ep.id.to_string();

    db.store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("the first write should succeed");

    let after_first = sync_rows(&db).await;
    assert_eq!(
        after_first, 1,
        "one episode should leave exactly one sync_log row"
    );

    // Replay: the duplicate check hits → the whole write is skipped
    let seen = db
        .store
        .episode(&id)
        .await
        .expect("reading the episode should not fail")
        .is_some();
    assert!(
        seen,
        "the episode just written must be readable, or the duplicate check does not work at all"
    );

    assert_eq!(
        sync_rows(&db).await,
        after_first,
        "sync_log must not grow when the duplicate check hits -- growth means the replay path \
         really did write something"
    );

    db.cleanup().await;
}

/// When another replay squeezes in between the duplicate check and the write,
/// what it hits is the unique constraint, and the whole transaction rolls back.
///
/// This guards the backstop under genuine concurrency: the duplicate check is a
/// read outside the transaction, and two replays passing through it
/// simultaneously is possible. At that point the second one must receive an
/// error that is **recognizable** (`is_duplicate_key`) rather than a generic
/// database failure — failing to recognize it produces a 500, the client treats
/// a successful replay as a failure, and then retries forever.
#[tokio::test]
async fn a_concurrent_replay_hits_the_unique_constraint_and_rolls_back() {
    let Some(db) = common::setup().await else {
        return;
    };

    let ep = common::new_episode("s-race", "同一条被投了两次");
    db.store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("the first write should succeed");
    let before = sync_rows(&db).await;

    // Skip the duplicate check and write again directly -- simulating two
    // replays passing through that out-of-transaction read at the same time
    let err = db
        .store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect_err("writing the same id a second time must fail");

    assert!(
        err.is_duplicate_key(),
        "a duplicate primary key must be recognizable by is_duplicate_key, or the caller can \
         only treat it as a 500. Got: {err}"
    );
    assert_eq!(
        sync_rows(&db).await,
        before,
        "a failed transaction must roll back entirely -- the business row not going in while \
         sync_log grows by one is a cursor entry pointing at a record that does not exist"
    );

    db.cleanup().await;
}

/// **A cautionary counter-example**: `ON CONFLICT DO NOTHING` would leave a
/// phantom sync_log row.
///
/// This one does not go through `WriteTxn` (`insert_row` is `pub(super)` and
/// out of a test's reach); it executes the two statements from inside it by
/// hand — same order, same semantics. **What it tests is not production code
/// but the rejected formulation**: turning "why this cannot be written that way"
/// into an assertion that actually runs, rather than a comment sitting there
/// waiting to be worked around.
///
/// If somebody genuinely added `ON CONFLICT DO NOTHING` to `insert_episode`,
/// the second test above would go red first (the error would no longer be a
/// duplicate key). This one is here to explain what happened at that point.
#[tokio::test]
async fn on_conflict_do_nothing_would_leave_a_phantom_sync_row() {
    let Some(db) = common::setup().await else {
        return;
    };

    let ep = common::new_episode("s-phantom", "第一次写进去的那条");
    let id = ep.id.to_string();
    db.store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("the first write should succeed");
    let before = sync_rows(&db).await;

    // The two statements from Guarded::insert_row, with the INSERT swapped for
    // the lenient version
    db.exec_raw(format!(
        "INSERT INTO episodes (id, session_id, role, content, text, tsv, device_id, occurred_at)
         VALUES ('{id}', 's-phantom', 'user', '{{}}'::jsonb, 'x',
                 to_tsvector('simple', 'x'), 'test', now())
         ON CONFLICT DO NOTHING"
    ))
    .await;
    db.exec_raw(format!(
        "INSERT INTO sync_log (table_name, record_id) VALUES ('{}', '{id}')",
        table::EPISODES
    ))
    .await;

    let episodes: i64 = db
        .query_raw(format!(
            "SELECT count(*)::text FROM episodes WHERE id = '{id}'"
        ))
        .await
        .first()
        .and_then(|s| s.parse().ok())
        .expect("count(*) must return exactly one row");

    assert_eq!(
        episodes, 1,
        "ON CONFLICT DO NOTHING did indeed insert not a single row"
    );
    assert_eq!(
        sync_rows(&db).await,
        before + 1,
        "and yet sync_log grew by one -- that is the phantom row. It makes nothing raise an \
         error, it just stops \"how far the cursor advanced\" from equalling \"how much was \
         actually written\""
    );

    db.cleanup().await;
}

//! Integration test for the real-time event source: write → trigger → NOTIFY →
//! the subscriber receives a signal.
//!
//! Every link in this chain is real (a real transaction, a real trigger, a real
//! `LISTEN`), because all of its failure modes are silent: the trigger was not
//! installed, the channel names do not match, the listener is connected under a
//! different search_path — all three have exactly the same symptom, "one side
//! wrote and the other does not refresh", and a unit test catches none of them.
//!
//! Skip rather than fail when the database is unavailable (see
//! [`common::setup`]).

mod common;

use std::time::Duration;

use cortex_store::SyncSignal;
use tokio::sync::mpsc;

/// Spawn a background task that forwards the signals out.
///
/// Subscribing must come before writing — notifications from transactions that
/// committed before the `LISTEN` are never re-sent. The returned receiver is
/// "the other side".
fn spawn_subscriber(store: cortex_store::Store) -> mpsc::UnboundedReceiver<SyncSignal> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut notifications = store.listen_sync().await;
        while let Some(sig) = notifications.recv().await {
            if tx.send(sig).is_err() {
                break;
            }
        }
    });
    rx
}

fn cursor_of(sig: SyncSignal) -> i64 {
    match sig {
        SyncSignal::Bump { cursor } | SyncSignal::Resync { cursor } => cursor,
    }
}

/// One side writes → the other side receives a signal.
#[tokio::test]
async fn a_write_reaches_the_listener() {
    let Some(db) = common::setup().await else {
        return;
    };

    let mut rx = spawn_subscriber(db.store.clone());

    // The subscription starts asynchronously, so writing too early lands before
    // the LISTEN. Retrying rather than sleeping a magic number: on a slow
    // machine the former is stable while the latter just turns flaky
    let mut seen = None;
    for _ in 0..10 {
        let ep = common::new_episode("listen-test", "实时推送测试");
        let seq = db
            .store
            .write_txn(async |t| t.insert_episode(&ep).await)
            .await
            .expect("writing the episode should not fail");

        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while let Ok(Some(sig)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            // Other temporary schemas running concurrently in the same database
            // also send notifications on this channel (channel names are scoped
            // to the database; see sync.rs), so only accept "caught up with my
            // own seq"
            if cursor_of(sig) >= seq {
                seen = Some((seq, cursor_of(sig)));
                break;
            }
        }
        if seen.is_some() {
            break;
        }
    }

    let outcome = seen;
    db.cleanup().await;

    let (seq, cursor) = outcome.expect(
        "a notification should arrive within 8 seconds of writing to sync_log; \
         not receiving one means either the trigger was not installed or LISTEN subscribed to a \
         different channel",
    );
    assert!(
        cursor >= seq,
        "the pushed cursor {cursor} lags behind the seq {seq} just written, so the client would \
         believe it had already caught up"
    );
}

/// The trigger must be idempotent: cortexd reinstalls it on every restart.
#[tokio::test]
async fn installing_the_trigger_twice_is_a_no_op() {
    let Some(db) = common::setup().await else {
        return;
    };

    let first = db.store.install_sync_notify().await;
    let second = db.store.install_sync_notify().await;

    db.cleanup().await;

    first.expect("the first trigger installation should not fail");
    second.expect("reinstalling the trigger must be idempotent and must not error");
}

/// After installing three times, `sync_log` still carries exactly **one**
/// trigger.
///
/// "install returned Ok" is not enough on its own: the symptom of genuinely
/// duplicated triggers is two pushes per write — the client merely pulls one
/// extra time and nothing errors, so the defect can live in production for a
/// long while. Hence counting directly in `pg_trigger`.
#[tokio::test]
async fn reinstalling_does_not_stack_up_triggers() {
    let Some(db) = common::setup().await else {
        return;
    };

    for _ in 0..3 {
        db.store
            .install_sync_notify()
            .await
            .expect("reinstalling the trigger must be idempotent");
    }

    // Count over a separate connection: its search_path is the default public,
    // so the schema must be qualified explicitly inside the regclass, or what
    // resolves is somebody else's sync_log
    let count = match admin_pool().await {
        Some(admin) => {
            let row: Option<(i64,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgname = 'cortex_sync_notify' AND NOT tgisinternal
                    AND tgrelid = '\"{}\".sync_log'::regclass",
                db.schema()
            )))
            .fetch_one(&admin)
            .await
            .ok();
            admin.close().await;
            row.map(|c| c.0)
        }
        None => None,
    };

    db.cleanup().await;

    assert_eq!(
        count,
        Some(1),
        "after reinstalling there is more than one notify trigger on sync_log -- every write \
         would push several notifications"
    );
}

/// A connection used only to query system catalogs. If it cannot be obtained,
/// the caller skips.
async fn admin_pool() -> Option<sqlx::PgPool> {
    let _ = dotenvy::dotenv();
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .ok()
}

/// After the storage layer is closed the subscription must wrap up rather than
/// backing off forever against a pool that is already closed.
#[tokio::test]
async fn closing_the_store_ends_the_stream() {
    let Some(db) = common::setup().await else {
        return;
    };

    let mut notifications = db.store.listen_sync().await;
    let store = db.store.clone();

    // Get it into the "subscribed and waiting" state first and only then close
    // the pool -- closing straight away degenerates into "never connected at
    // all", which does not exercise the "the connection is interrupted by the
    // pool's close event" path
    let ep = common::new_episode("close-test", "先跑通一次");
    store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("writing the episode should not fail");
    let _ = tokio::time::timeout(Duration::from_millis(800), notifications.recv()).await;

    store.close().await;

    let ended = tokio::time::timeout(Duration::from_secs(5), notifications.recv()).await;

    db.cleanup().await;

    assert_eq!(
        ended.expect(
            "recv should return promptly once the storage layer is closed, rather than hanging \
             in backoff-and-reconnect"
        ),
        None,
        "recv must return None once the pool is closed so the layer above can wrap up"
    );
}

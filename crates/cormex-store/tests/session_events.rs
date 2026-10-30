//! Session lifecycle: the state machines, the archived filter, the schema
//! constraints, and delivery over sync.
//!
//! All of the logic lives in SQL (the three DISTINCT ONs in the `session_state`
//! view, the archived filter in `session_digests`, the CHECKs on the table), and
//! a unit test can reach none of it — only a real database connection can. Skip
//! rather than fail when the database is unavailable.

mod common;

use chrono::{Duration, Utc};
use cormex_core::Id;
use cormex_store::{
    Actor, NewEpisode, NewSessionEvent, Role, SessionOp, Store, SyncPayload, table,
};

const DEV: &str = "test-device";

fn episode(session: &str, text: &str, minutes_ago: i64) -> NewEpisode {
    NewEpisode {
        id: Id::new(),
        session_id: session.to_owned(),
        role: Role::User,
        content: serde_json::json!({ "text": text }),
        text: Some(text.to_owned()),
        tsv_source: Some(text.to_owned()),
        domain: None,
        device_id: DEV.to_owned(),
        occurred_at: Utc::now() - Duration::minutes(minutes_ago),
    }
}

async fn write_episode(store: &Store, ep: NewEpisode) {
    store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("writing the episode should not fail");
}

/// Append one lifecycle event and return its seq in the sync total order.
async fn emit(store: &Store, e: NewSessionEvent) -> i64 {
    store
        .write_txn(async |t| t.insert_session_event(&e).await)
        .await
        .expect("writing the session_event should not fail")
}

/// The final state is decided by the **last event of the same kind**, not by an
/// undo stack.
#[tokio::test]
async fn the_last_event_of_each_kind_wins() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    emit(
        s,
        NewSessionEvent::rename("sess", "标题 A", Actor::User, DEV),
    )
    .await;
    emit(
        s,
        NewSessionEvent::rename("sess", "标题 B", Actor::User, DEV),
    )
    .await;

    emit(s, NewSessionEvent::archive("sess", Actor::User, DEV)).await;
    emit(s, NewSessionEvent::unarchive("sess", Actor::User, DEV)).await;

    let state = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail");

    db.cleanup().await;

    let state = state.expect("a session that has had events should have a final state");
    assert_eq!(
        state.title.as_deref(),
        Some("标题 B"),
        "after [rename A, rename B] the answer should be B -- the final state is decided by the \
         last event, with no undo stack: {state:?}"
    );
    assert!(
        !state.archived,
        "after [archive, unarchive] it should be unarchived: {state:?}"
    );
}

/// The three dimensions are independent — renaming after archiving must not
/// unarchive the session.
#[tokio::test]
async fn the_three_dimensions_do_not_interfere() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let dir = tempdir_path();

    emit(s, NewSessionEvent::archive("sess", Actor::User, DEV)).await;
    // Rename and bind a workspace after archiving. If the view decided by "the
    // last event in the whole table", these two would wash the archived state
    // away -- which is exactly the implementation error this test stops
    emit(s, NewSessionEvent::rename("sess", "档案", Actor::User, DEV)).await;
    emit(
        s,
        NewSessionEvent::bind_workspace("sess", &dir, Actor::User, DEV),
    )
    .await;

    let state = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail");

    db.cleanup().await;

    let state = state.expect("a session that has had events should have a final state");
    assert!(
        state.archived,
        "renaming or binding a workspace after archiving must not unarchive the session \
         automatically: {state:?}"
    );
    assert_eq!(state.title.as_deref(), Some("档案"));
    assert_eq!(state.workspace.as_deref(), Some(dir.as_str()));
}

/// After unbinding, `workspace` must go back to `None` rather than keeping the
/// last path it was bound to.
#[tokio::test]
async fn unbinding_clears_the_workspace() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let dir = tempdir_path();

    emit(
        s,
        NewSessionEvent::bind_workspace("sess", &dir, Actor::User, DEV),
    )
    .await;
    let bound = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail");

    emit(
        s,
        NewSessionEvent::unbind_workspace("sess", Actor::User, DEV),
    )
    .await;
    let unbound = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail");

    db.cleanup().await;

    assert_eq!(
        bound.and_then(|s| s.workspace).as_deref(),
        Some(dir.as_str())
    );
    assert_eq!(
        unbound.expect("the session still has events").workspace,
        None,
        "after unbinding it must be None -- keeping the old path would leave the file tools \
         showing up in a pure chat session"
    );
}

/// A session with no events has no final state, and that is not an error.
#[tokio::test]
async fn a_session_without_events_has_no_state() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    write_episode(s, episode("plain", "只发过消息，没改过名", 5)).await;

    let state = s
        .session_state("plain")
        .await
        .expect("querying the final state should not fail");

    db.cleanup().await;
    assert!(
        state.is_none(),
        "a session that has never had a lifecycle event should have no final state"
    );
}

/// Each op may carry only its own payload field — the CHECKs on both sides must
/// genuinely take effect.
#[tokio::test]
async fn schema_rejects_mismatched_payloads() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // rename with no title
    let mut no_title = NewSessionEvent::rename("sess", "x", Actor::User, DEV);
    no_title.title = None;

    // archive carrying a title -- letting this through would make the view
    // behave as though archiving also changed the title
    let mut archive_with_title = NewSessionEvent::archive("sess", Actor::User, DEV);
    archive_with_title.title = Some("不该有的标题".into());

    // unbind carrying a workspace
    let mut unbind_with_path = NewSessionEvent::unbind_workspace("sess", Actor::User, DEV);
    unbind_with_path.workspace = Some("/tmp".into());

    // an empty title
    let empty_title = NewSessionEvent::rename("sess", "", Actor::User, DEV);

    let mut outcome = Vec::new();
    for (label, e) in [
        ("rename missing its title", no_title),
        ("archive carrying a title", archive_with_title),
        ("unbind carrying a workspace", unbind_with_path),
        ("an empty title", empty_title),
    ] {
        let r = s
            .write_txn(async |t| t.insert_session_event(&e).await)
            .await;
        outcome.push((label, r.is_err()));
    }

    db.cleanup().await;

    for (label, rejected) in outcome {
        assert!(
            rejected,
            "\"{label}\" should be rejected by the schema's CHECK"
        );
    }
}

/// Archived sessions do not appear in the list by default, and
/// `include_archived` brings them back.
#[tokio::test]
async fn archived_sessions_are_hidden_by_default() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    write_episode(s, episode("keep", "留在列表里的会话", 10)).await;
    write_episode(s, episode("gone", "要归档的会话", 5)).await;
    emit(s, NewSessionEvent::archive("gone", Actor::User, DEV)).await;

    let default = s
        .session_digests(10, false, None)
        .await
        .expect("querying the list should not fail");
    let all = s
        .session_digests(10, true, None)
        .await
        .expect("querying the list should not fail");

    // It must reappear after unarchiving -- archiving is not deleting
    emit(s, NewSessionEvent::unarchive("gone", Actor::User, DEV)).await;
    let restored = s
        .session_digests(10, false, None)
        .await
        .expect("querying the list should not fail");

    db.cleanup().await;

    let ids = |v: &[cormex_store::SessionDigest]| {
        v.iter().map(|d| d.session_id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&default),
        vec!["keep"],
        "archived sessions must not appear in the default list"
    );
    assert_eq!(
        ids(&all).len(),
        2,
        "include_archived should bring the archived ones back too: {:?}",
        ids(&all)
    );
    assert!(
        all.iter().any(|d| d.session_id == "gone" && d.archived),
        "the archived flag should be reported faithfully: {:?}",
        ids(&all)
    );
    assert_eq!(
        ids(&restored).len(),
        2,
        "a session must reappear after unarchiving -- archiving is not deleting, and not one row \
         was lost"
    );
}

/// Archived sessions must not **consume** the limit's quota.
///
/// "Take N rows first and drop the archived ones in memory" is the easiest
/// implementation to write, and its defect raises no error: archive twenty
/// sessions and the list shows twenty fewer. The filter has to come before the
/// LIMIT.
#[tokio::test]
async fn archived_sessions_do_not_consume_the_limit() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // Three archived sessions, all newer than the unarchived one, so they sort
    // ahead of it
    for i in 0..3 {
        let id = format!("arch-{i}");
        write_episode(s, episode(&id, "归档会话", 1 + i)).await;
        emit(s, NewSessionEvent::archive(&id, Actor::User, DEV)).await;
    }
    write_episode(s, episode("visible", "唯一该被看见的", 100)).await;

    // limit=1: if the filter happened after the LIMIT, an archived session
    // would take that one slot and the result would be empty
    let got = s
        .session_digests(1, false, None)
        .await
        .expect("querying the list should not fail");

    db.cleanup().await;

    assert_eq!(
        got.iter().map(|d| d.session_id.clone()).collect::<Vec<_>>(),
        vec!["visible"],
        "the archived filter must happen before the LIMIT, or archived sessions eat the visible \
         quota"
    );
}

/// A user-set title takes precedence over the derived one, and rides along with
/// the digest.
#[tokio::test]
async fn a_custom_title_rides_along_with_the_digest() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    write_episode(s, episode("sess", "首条用户消息", 5)).await;
    let before = s
        .session_digest("sess")
        .await
        .expect("querying the digest should not fail");
    emit(
        s,
        NewSessionEvent::rename("sess", "我起的名字", Actor::User, DEV),
    )
    .await;
    let after = s
        .session_digest("sess")
        .await
        .expect("querying the digest should not fail");
    let missing = s
        .session_digest("nope")
        .await
        .expect("querying the digest should not fail");

    db.cleanup().await;

    assert_eq!(
        before.expect("the session has messages").title,
        None,
        "with no rename ever, title should be None and the layer above falls back to the derived \
         title"
    );
    let after = after.expect("the session has messages");
    assert_eq!(after.title.as_deref(), Some("我起的名字"));
    assert_eq!(
        after.first_user_text.as_deref(),
        Some("首条用户消息"),
        "the raw material for the derived title must still come back -- the client has to be \
         able to show what the automatic title would be"
    );
    assert!(
        missing.is_none(),
        "a session with no messages should have no digest"
    );
}

/// The new table must enter `sync_log`, and `/sync` must be able to pull its
/// business row back.
///
/// Missing the arm in `load_payloads` does not mean "session renames fail to
/// sync", it means **the whole of /sync breaks**: one session_events log row
/// makes the entire batch report UnknownTable, and the client's cursor is stuck
/// in front of it from then on.
#[tokio::test]
async fn session_events_flow_through_the_sync_log() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let before = s
        .latest_seq()
        .await
        .expect("querying the cursor should not fail");
    let e = NewSessionEvent::rename("sess", "同步得到吗", Actor::User, DEV);
    let id = e.id.to_string();
    let seq = emit(s, e).await;
    // Follow it with a write to another table, confirming that an interleaved
    // pull does not fall over
    write_episode(s, episode("sess", "一条普通消息", 1)).await;

    let records = s
        .fetch_since(before, 100)
        .await
        .expect("the incremental pull should not fail");
    let events = s
        .session_events("sess")
        .await
        .expect("querying the timeline should not fail");

    db.cleanup().await;

    assert!(seq > before, "a session_event must advance the sync cursor");
    let hit = records
        .iter()
        .find(|r| r.table_name == table::SESSION_EVENTS)
        .unwrap_or_else(|| panic!("session_events did not enter sync_log: {records:?}"));
    assert_eq!(hit.record_id, id);
    match hit.payload.as_ref() {
        Some(SyncPayload::SessionEvent(ev)) => {
            assert_eq!(ev.op, SessionOp::Rename);
            assert_eq!(ev.title.as_deref(), Some("同步得到吗"));
            assert_eq!(ev.actor, Actor::User);
        }
        other => panic!("wrong payload type, so /sync cannot deliver it: {other:?}"),
    }
    assert_eq!(
        events.len(),
        1,
        "the timeline should hold exactly one entry: {events:?}"
    );
}

/// The several changes in one PATCH are written in one transaction — other
/// devices must never see an intermediate state.
#[tokio::test]
async fn multiple_changes_land_in_one_transaction() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let dir = tempdir_path();

    let before = s
        .latest_seq()
        .await
        .expect("querying the cursor should not fail");
    let batch = vec![
        NewSessionEvent::rename("sess", "一次改完", Actor::User, DEV),
        NewSessionEvent::archive("sess", Actor::User, DEV),
        NewSessionEvent::bind_workspace("sess", &dir, Actor::User, DEV),
    ];
    s.write_txn(async |t| {
        let mut last = 0;
        for e in &batch {
            last = t.insert_session_event(e).await?;
        }
        Ok(last)
    })
    .await
    .expect("the batch write should not fail");

    let logged = s
        .sync_log_since(before, 100)
        .await
        .expect("querying the log should not fail");
    let state = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail");

    db.cleanup().await;

    let seqs: Vec<i64> = logged.iter().map(|l| l.seq).collect();
    assert_eq!(
        seqs.len(),
        3,
        "all three events should have their own sync_log row: {seqs:?}"
    );
    assert_eq!(
        seqs.last().unwrap() - seqs.first().unwrap(),
        2,
        "three rows written in one transaction should have consecutive numbers -- another write \
         transaction slipping in between means the lock is not working: {seqs:?}"
    );

    let state = state.expect("there should be a final state");
    assert_eq!(state.title.as_deref(), Some("一次改完"));
    assert!(state.archived);
    assert_eq!(state.workspace.as_deref(), Some(dir.as_str()));
}

/// The storage layer does not validate paths — "what counts as a system
/// directory" is knowledge about the deployment shape and belongs to cormexd.
/// This just borrows the temp directory to get an absolute path string that
/// genuinely exists.
fn tempdir_path() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

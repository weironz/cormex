//! Projects: the two state machines, the blast radius of deleting a project,
//! filtering by project, the schema constraints, and delivery over sync.
//!
//! Same lineage as the session_events file: the logic lives entirely in SQL
//! (the two DISTINCT ONs in `project_state`, the EXISTS in `session_state` that
//! says "treat a dangling membership as ungrouped", the project filter in
//! `session_digests`, the CHECKs on the tables), and a unit test can reach none
//! of it — only a real database connection can. Skip rather than fail when the
//! database is unavailable.

mod common;

use chrono::{Duration, Utc};
use cormex_core::Id;
use cormex_store::{
    Actor, NewEpisode, NewProjectEvent, NewSessionEvent, ProjectOp, Role, Store, SyncPayload, table,
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

async fn write_episode(store: &Store, ep: NewEpisode) -> Id {
    let id = ep.id;
    store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("writing the episode should not fail");
    id
}

async fn emit_project(store: &Store, e: NewProjectEvent) -> i64 {
    store
        .write_txn(async |t| t.insert_project_event(&e).await)
        .await
        .expect("writing the project_event should not fail")
}

async fn emit_session(store: &Store, e: NewSessionEvent) -> i64 {
    store
        .write_txn(async |t| t.insert_session_event(&e).await)
        .await
        .expect("writing the session_event should not fail")
}

/// The name and the existence are each a state machine, and each takes its own
/// last event.
#[tokio::test]
async fn the_last_event_of_each_machine_wins() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // Rename twice: the final state is the last one, with no undo stack
    emit_project(
        s,
        NewProjectEvent::create("renamed", "甲", Actor::User, DEV),
    )
    .await;
    emit_project(
        s,
        NewProjectEvent::rename("renamed", "乙", Actor::User, DEV),
    )
    .await;
    emit_project(
        s,
        NewProjectEvent::rename("renamed", "丙", Actor::User, DEV),
    )
    .await;

    // Rename after deleting: the rename must not resurrect it -- the two state
    // machines do not interfere, and a "last row in the whole table" criterion
    // would conclude here that "renaming it automatically brought it back"
    emit_project(
        s,
        NewProjectEvent::create("gone", "要删的", Actor::User, DEV),
    )
    .await;
    emit_project(s, NewProjectEvent::delete("gone", Actor::User, DEV)).await;
    emit_project(
        s,
        NewProjectEvent::rename("gone", "改过的", Actor::User, DEV),
    )
    .await;

    // Creating the same id again = undoing an accidental deletion
    emit_project(s, NewProjectEvent::create("back", "初版", Actor::User, DEV)).await;
    emit_project(s, NewProjectEvent::delete("back", Actor::User, DEV)).await;
    emit_project(s, NewProjectEvent::create("back", "重建", Actor::User, DEV)).await;

    let projects = s
        .projects()
        .await
        .expect("querying the project list should not fail");
    let events = s
        .project_events("renamed")
        .await
        .expect("querying the timeline should not fail");

    db.cleanup().await;

    let names: Vec<(String, String)> = projects
        .iter()
        .map(|p| (p.project_id.clone(), p.name.clone()))
        .collect();

    assert!(
        names.contains(&("renamed".into(), "丙".into())),
        "after [create 甲, rename 乙, rename 丙] it should be called 丙 -- the final state is \
         decided by the last event: {names:?}"
    );
    assert!(
        !names.iter().any(|(id, _)| id == "gone"),
        "renaming after deleting must not resurrect the project -- existence is an independent \
         state machine: {names:?}"
    );
    assert!(
        names.contains(&("back".into(), "重建".into())),
        "creating the same id again should restore the project (undoing an accidental deletion), \
         under its newest name: {names:?}"
    );

    assert_eq!(
        events.len(),
        3,
        "the timeline should hold three entries: {events:?}"
    );
    assert_eq!(
        events[0].op,
        ProjectOp::Create,
        "the timeline must be ascending by time, so the first entry is create: {events:?}"
    );
}

/// **Deleting a project only dissolves the grouping**: the sessions become
/// ungrouped, and not one message or extracted memory is lost.
///
/// This is the easiest part of the feature to implement wrong — written as a
/// cascading delete of sessions, one click on "delete project" loses months of
/// conversation, irreversibly.
#[tokio::test]
async fn deleting_a_project_ungroups_its_sessions_and_destroys_nothing() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    emit_project(
        s,
        NewProjectEvent::create("proj", "记忆引擎", Actor::User, DEV),
    )
    .await;
    let ep = write_episode(s, episode("sess", "项目里的一句话", 5)).await;
    emit_session(
        s,
        NewSessionEvent::move_to_project("sess", "proj", Actor::User, DEV),
    )
    .await;

    // Hang a derived memory off it so we can assert "deleting a project does
    // not touch memory"
    let entity = common::new_entity("person", "老王");
    let fact = common::new_fact(entity.id, "喜欢", "深色主题", ep);
    let subject = entity.id;
    let fact_id = fact.id.to_string();
    s.write_txn(async |t| {
        t.insert_entity(&entity).await?;
        t.insert_fact(&fact).await
    })
    .await
    .expect("writing the entity and the fact should not fail");

    let grouped = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail")
        .expect("a session that has had events should have a final state");
    let in_project = s
        .session_digests(10, false, Some("proj"))
        .await
        .expect("querying the list by project should not fail");

    emit_project(s, NewProjectEvent::delete("proj", Actor::User, DEV)).await;

    let after_state = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail")
        .expect("the session's events are still there, so its final state is too");
    let after_projects = s
        .projects()
        .await
        .expect("querying the project list should not fail");
    let after_sessions = s
        .session_digests(10, false, None)
        .await
        .expect("querying the list should not fail");
    let after_facts = s
        .active_facts_by_subject(&subject.to_string(), 10)
        .await
        .expect("querying the facts should not fail");
    let after_episodes = s
        .episodes_by_session("sess", 10)
        .await
        .expect("querying the messages should not fail");
    // The events themselves are append-only, so deleting a project must not
    // erase the history
    let session_events = s
        .session_events("sess")
        .await
        .expect("querying the timeline should not fail");

    db.cleanup().await;

    assert_eq!(
        grouped.project_id.as_deref(),
        Some("proj"),
        "after being moved into the project the final state should point at it: {grouped:?}"
    );
    assert_eq!(
        in_project.len(),
        1,
        "the project should contain one session: {in_project:?}"
    );

    assert!(
        after_projects.is_empty(),
        "once deleted the project must disappear from the list: {after_projects:?}"
    );
    assert_eq!(
        after_state.project_id, None,
        "once the project is gone the session must become ungrouped -- a dangling membership \
         would make the UI draw a group label that leads to an empty view: {after_state:?}"
    );
    assert_eq!(
        after_sessions.len(),
        1,
        "deleting a project must not make the session disappear from the list: {after_sessions:?}"
    );
    assert_eq!(
        after_sessions[0].message_count, 1,
        "deleting a project must not touch a single message: {after_sessions:?}"
    );
    assert_eq!(after_episodes.len(), 1, "not one raw message may be lost");
    assert_eq!(
        after_facts.len(),
        1,
        "deleting a project must not touch a single extracted memory -- destroying content can \
         only go through redact / purge"
    );
    assert_eq!(after_facts[0].id, fact_id);
    assert_eq!(
        session_events.len(),
        1,
        "the session's move_to_project event must stay exactly as it was (append-only); not \
         cascading a rewrite of it on project deletion is precisely the premise on which \
         \"undo an accidental deletion\" rests: {session_events:?}"
    );
}

/// The fourth dimension does not interfere with the first three.
#[tokio::test]
async fn the_project_dimension_does_not_interfere_with_the_others() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    emit_project(s, NewProjectEvent::create("proj", "分组", Actor::User, DEV)).await;
    emit_session(
        s,
        NewSessionEvent::move_to_project("sess", "proj", Actor::User, DEV),
    )
    .await;
    // Archive and rename after moving into the project. If the view decided by
    // "the last row in the whole table", these two would wash the membership
    // away
    emit_session(s, NewSessionEvent::archive("sess", Actor::User, DEV)).await;
    emit_session(s, NewSessionEvent::rename("sess", "标题", Actor::User, DEV)).await;

    let grouped = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail");

    emit_session(
        s,
        NewSessionEvent::remove_from_project("sess", Actor::User, DEV),
    )
    .await;
    let ungrouped = s
        .session_state("sess")
        .await
        .expect("querying the final state should not fail");

    db.cleanup().await;

    let grouped = grouped.expect("a session that has had events should have a final state");
    assert_eq!(
        grouped.project_id.as_deref(),
        Some("proj"),
        "membership should survive archiving / renaming untouched: {grouped:?}"
    );
    assert!(
        grouped.archived,
        "membership must not wash the archived state away: {grouped:?}"
    );
    assert_eq!(grouped.title.as_deref(), Some("标题"));

    assert_eq!(
        ungrouped.expect("the session still has events").project_id,
        None,
        "after being moved out it must be None, not the last project it happened to be in"
    );
}

/// Filtering by project must happen before the `LIMIT`.
///
/// "Take N rows first and filter in memory" is the easiest implementation to
/// write, and its defect raises no error: other projects' sessions eat the whole
/// quota and the user opens their project to an empty list — right after having
/// dragged sessions into it.
#[tokio::test]
async fn filtering_by_project_happens_before_the_limit() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    emit_project(s, NewProjectEvent::create("proj", "分组", Actor::User, DEV)).await;

    // Three sessions outside the project, all newer than it, so they sort ahead
    for i in 0..3 {
        let id = format!("other-{i}");
        write_episode(s, episode(&id, "别的会话", 1 + i)).await;
    }
    write_episode(s, episode("mine", "项目里唯一的会话", 100)).await;
    emit_session(
        s,
        NewSessionEvent::move_to_project("mine", "proj", Actor::User, DEV),
    )
    .await;

    let filtered = s
        .session_digests(1, false, Some("proj"))
        .await
        .expect("querying the list by project should not fail");
    let unfiltered = s
        .session_digests(10, false, None)
        .await
        .expect("querying the list should not fail");
    let missing_project = s
        .session_digests(10, false, Some("nope"))
        .await
        .expect("querying a nonexistent project should not raise an error");

    db.cleanup().await;

    assert_eq!(
        filtered
            .iter()
            .map(|d| d.session_id.clone())
            .collect::<Vec<_>>(),
        vec!["mine"],
        "the project filter must happen before the LIMIT, or other projects' sessions eat the \
         quota"
    );
    assert_eq!(
        unfiltered.len(),
        4,
        "with no project_id passed, every session must be returned (including ungrouped ones): \
         {unfiltered:?}"
    );
    assert!(
        unfiltered
            .iter()
            .any(|d| d.session_id == "mine" && d.project_id.as_deref() == Some("proj")),
        "membership must ride along with the digest, or the client cannot draw the group label: \
         {unfiltered:?}"
    );
    assert!(
        missing_project.is_empty(),
        "querying a nonexistent project should give an empty list, not everything: \
         {missing_project:?}"
    );
}

/// Each op may carry only its own payload field, and the CHECKs on both sides
/// must genuinely take effect.
#[tokio::test]
async fn schema_rejects_mismatched_payloads() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let mut create_without_name = NewProjectEvent::create("p", "x", Actor::User, DEV);
    create_without_name.name = None;

    // delete carrying a name -- letting it through would make the view behave
    // as though deleting it also changed its name
    let mut delete_with_name = NewProjectEvent::delete("p", Actor::User, DEV);
    delete_with_name.name = Some("不该有的名字".into());

    let empty_name = NewProjectEvent::create("p", "", Actor::User, DEV);
    let too_long = NewProjectEvent::create("p", &"x".repeat(101), Actor::User, DEV);

    let mut project_outcome = Vec::new();
    for (label, e) in [
        ("create missing its name", create_without_name),
        ("delete carrying a name", delete_with_name),
        ("an empty name", empty_name),
        ("an over-long name", too_long),
    ] {
        let r = s
            .write_txn(async |t| t.insert_project_event(&e).await)
            .await;
        project_outcome.push((label, r.is_err()));
    }

    let mut move_without_target = NewSessionEvent::move_to_project("s", "p", Actor::User, DEV);
    move_without_target.project_id = None;

    // archive carrying a project_id -- the view would behave as though
    // archiving also moved it to a different project
    let mut archive_with_project = NewSessionEvent::archive("s", Actor::User, DEV);
    archive_with_project.project_id = Some("p".into());

    let mut session_outcome = Vec::new();
    for (label, e) in [
        (
            "move_to_project missing its project_id",
            move_without_target,
        ),
        ("archive carrying a project_id", archive_with_project),
    ] {
        let r = s
            .write_txn(async |t| t.insert_session_event(&e).await)
            .await;
        session_outcome.push((label, r.is_err()));
    }

    db.cleanup().await;

    for (label, rejected) in project_outcome.into_iter().chain(session_outcome) {
        assert!(
            rejected,
            "\"{label}\" should be rejected by the schema's CHECK"
        );
    }
}

/// What `session_count` counts must match what the session list shows.
#[tokio::test]
async fn session_count_matches_what_the_session_list_shows() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    emit_project(s, NewProjectEvent::create("proj", "分组", Actor::User, DEV)).await;

    // The one that counts
    write_episode(s, episode("visible", "看得见的会话", 10)).await;
    // Archived: it vanishes from the default list, so it must not appear in the
    // count either
    write_episode(s, episode("archived", "归档的会话", 9)).await;
    emit_session(s, NewSessionEvent::archive("archived", Actor::User, DEV)).await;
    // With no messages at all: the session list starts from the episodes
    // aggregate, so it is not in the list to begin with
    for id in ["visible", "archived", "silent"] {
        emit_session(
            s,
            NewSessionEvent::move_to_project(id, "proj", Actor::User, DEV),
        )
        .await;
    }

    let projects = s
        .projects()
        .await
        .expect("querying the project list should not fail");
    let listed = s
        .session_digests(10, false, Some("proj"))
        .await
        .expect("querying the list by project should not fail");
    let single = s
        .project("proj")
        .await
        .expect("querying a single project should not fail");
    let missing = s
        .project("nope")
        .await
        .expect("querying a nonexistent project should not raise an error");

    db.cleanup().await;

    assert_eq!(
        projects.len(),
        1,
        "there should be exactly one project: {projects:?}"
    );
    assert_eq!(
        projects[0].session_count,
        listed.len() as i64,
        "the count must agree with what GET /sessions?project_id= shows, or the user sees \
         \"3 sessions\" and finds only 1 after clicking in: count {}, list {:?}",
        projects[0].session_count,
        listed.iter().map(|d| &d.session_id).collect::<Vec<_>>()
    );
    assert_eq!(
        projects[0].session_count, 1,
        "neither the archived one nor the one with no messages may enter the count: {projects:?}"
    );
    assert_eq!(
        single.map(|p| p.name),
        Some("分组".into()),
        "looking a single project up by id should find it"
    );
    assert!(
        missing.is_none(),
        "a nonexistent project should be None rather than an error"
    );
}

/// The new table must enter `sync_log`, and `/sync` must be able to pull its
/// business row back.
///
/// Missing the arm in `load_payloads` does not mean "projects fail to sync", it
/// means **the whole of /sync breaks**: one project_events log row makes the
/// entire batch report UnknownTable, and the client's cursor is stuck in front
/// of it from then on. The same goes for `project_id` on the session side —
/// without shipping it, every session on other devices is ungrouped, and that
/// merely looks like "the grouping feature didn't sync over".
#[tokio::test]
async fn project_events_and_the_session_link_flow_through_the_sync_log() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let before = s
        .latest_seq()
        .await
        .expect("querying the cursor should not fail");

    let created = NewProjectEvent::create("proj", "同步得到吗", Actor::User, DEV);
    let project_event_id = created.id.to_string();
    let seq = emit_project(s, created).await;

    let moved = NewSessionEvent::move_to_project("sess", "proj", Actor::User, DEV);
    let session_event_id = moved.id.to_string();
    emit_session(s, moved).await;

    let records = s
        .fetch_since(before, 100)
        .await
        .expect("the incremental pull should not fail");

    db.cleanup().await;

    assert!(seq > before, "a project_event must advance the sync cursor");

    let project_hit = records
        .iter()
        .find(|r| r.table_name == table::PROJECT_EVENTS)
        .unwrap_or_else(|| panic!("project_events did not enter sync_log: {records:?}"));
    assert_eq!(project_hit.record_id, project_event_id);
    match project_hit.payload.as_ref() {
        Some(SyncPayload::ProjectEvent(e)) => {
            assert_eq!(e.op, ProjectOp::Create);
            assert_eq!(e.name.as_deref(), Some("同步得到吗"));
            assert_eq!(e.project_id, "proj");
        }
        other => panic!("wrong payload type, so /sync cannot deliver it: {other:?}"),
    }

    let session_hit = records
        .iter()
        .find(|r| r.record_id == session_event_id)
        .unwrap_or_else(|| panic!("session_events did not enter sync_log: {records:?}"));
    match session_hit.payload.as_ref() {
        Some(SyncPayload::SessionEvent(e)) => assert_eq!(
            e.project_id.as_deref(),
            Some("proj"),
            "a session event's project_id must ship inside the payload, or every session on \
             other devices is ungrouped"
        ),
        other => panic!("wrong payload type: {other:?}"),
    }
}

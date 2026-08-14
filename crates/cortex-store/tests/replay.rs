//! Replay fidelity: whether what went in is still there when it is replayed.
//!
//! Three groups of assertions, corresponding to three silent corruptions that
//! raise no error:
//!
//! 1. Long-session pagination — the old `ORDER BY occurred_at ASC LIMIT 500`
//!    dropped the **newest** messages, leaving the user with a conversation that
//!    has no ending;
//! 2. Injection attribution — it used to live only in the SSE stream, and one
//!    refresh emptied the drawer;
//! 3. Transcription rescan candidates — large files uploaded via presigned
//!    direct upload used to never be transcribed at all.
//!
//! All of these can only be tested against a real database: the correctness is
//! in the SQL.

mod common;

use chrono::{Duration, Utc};
use cortex_core::Id;
use cortex_store::{
    Actor, EpisodeCursor, NewBlob, NewEpisode, NewEpisodeMemory, NewEpisodeToolCall, NewFactEvent,
    NewRedaction, RedactionMode, RedactionTarget, Role, Store,
};

fn episode_at(session: &str, text: &str, seconds_ago: i64) -> NewEpisode {
    NewEpisode {
        id: Id::new(),
        session_id: session.to_owned(),
        role: Role::User,
        content: serde_json::json!({ "text": text }),
        text: Some(text.to_owned()),
        tsv_source: Some(text.to_owned()),
        domain: None,
        device_id: common::DEVICE.to_owned(),
        occurred_at: Utc::now() - Duration::seconds(seconds_ago),
    }
}

// ══════════════════════════════════════════════════════════
//  R1 -- what a long session drops is the newest messages
// ══════════════════════════════════════════════════════════

/// For a session longer than one page, the default page must contain the **last
/// message**.
///
/// This is the most important assertion in this change: the old implementation
/// was ascending + LIMIT, so an over-long session was truncated at the end, and
/// no field in the response indicated that truncation had occurred — the user
/// simply feels "my history looks odd".
#[tokio::test]
async fn the_default_page_contains_the_newest_message_not_the_oldest() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // 600 rows > the old implementation's cap of 500
    const TOTAL: i64 = 600;
    for i in 0..TOTAL {
        // A decreasing seconds_ago = increasing time, so the last one written
        // is the newest
        let ep = episode_at("long", &format!("第 {i} 句"), TOTAL - i);
        s.write_txn(async |t| t.insert_episode(&ep).await)
            .await
            .expect("writing the episode should not fail");
    }

    let page = s
        .episodes_by_session_page("long", 200, None)
        .await
        .expect("分页查询不该失败");
    // The shape of the old implementation: ascending + LIMIT
    let old = s
        .episodes_by_session("long", 200)
        .await
        .expect("the ascending query should not fail");

    db.cleanup().await;

    assert_eq!(page.len(), 200, "exactly one page should come back");
    assert_eq!(
        page[0].text.as_deref(),
        Some(format!("第 {} 句", TOTAL - 1)).as_deref(),
        "pagination is descending, so the first row must be the **newest** message; getting \
         anything else means it is still fetching the opening stretch"
    );
    assert_eq!(
        old.last().and_then(|e| e.text.clone()),
        Some("第 199 句".to_owned()),
        "the old ascending implementation stops at message 199 here -- that is exactly the shape \
         of \"dropping the newest messages\", and it is kept so the benefit of this change is \
         visible at a glance"
    );
}

/// The cursor must span both `(occurred_at, id)` columns.
///
/// Every message is crammed onto the **same** `occurred_at`: an implementation
/// using time alone as its cursor either cannot advance here (the second page is
/// identical to the first) or skips an entire batch outright.
#[tokio::test]
async fn paging_never_skips_or_repeats_rows_sharing_a_timestamp() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let same_instant = Utc::now();
    const TOTAL: usize = 25;
    for i in 0..TOTAL {
        let mut ep = episode_at("tie", &format!("同刻第 {i} 句"), 0);
        ep.occurred_at = same_instant;
        s.write_txn(async |t| t.insert_episode(&ep).await)
            .await
            .expect("writing the episode should not fail");
    }

    // 7 per page, paged to the end, collecting every id
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<EpisodeCursor> = None;
    for _ in 0..10 {
        let page = s
            .episodes_by_session_page("tie", 7, cursor.as_ref())
            .await
            .expect("the paginated query should not fail");
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|e| EpisodeCursor {
            occurred_at: e.occurred_at,
            id: e.id.clone(),
        });
        seen.extend(page.into_iter().map(|e| e.id));
    }

    db.cleanup().await;

    let unique: std::collections::HashSet<&String> = seen.iter().collect();
    assert_eq!(
        seen.len(),
        TOTAL,
        "paging through everything should show exactly {TOTAL} rows; it showed {} -- fewer means \
         dropped rows, more means duplicates, and either way the cursor is not spanning \
         (occurred_at, id)",
        seen.len()
    );
    assert_eq!(
        unique.len(),
        TOTAL,
        "the same message was paged out twice -- the cursor used occurred_at alone"
    );
}

/// The order stitched back together from the pages must match a single
/// ascending fetch exactly.
#[tokio::test]
async fn pages_stitch_back_into_the_original_order() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    for i in 0..30 {
        let ep = episode_at("stitch", &format!("第 {i} 句"), 30 - i);
        s.write_txn(async |t| t.insert_episode(&ep).await)
            .await
            .expect("writing the episode should not fail");
    }

    let mut stitched: Vec<String> = Vec::new();
    let mut cursor: Option<EpisodeCursor> = None;
    loop {
        let page = s
            .episodes_by_session_page("stitch", 8, cursor.as_ref())
            .await
            .expect("the paginated query should not fail");
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|e| EpisodeCursor {
            occurred_at: e.occurred_at,
            id: e.id.clone(),
        });
        stitched.extend(page.into_iter().map(|e| e.id));
    }
    stitched.reverse(); // descending pagination → forward-order rendering

    let straight: Vec<String> = s
        .episodes_by_session("stitch", 1000)
        .await
        .expect("the ascending query should not fail")
        .into_iter()
        .map(|e| e.id)
        .collect();

    db.cleanup().await;

    assert_eq!(
        stitched, straight,
        "reversing the pages and stitching them together must match a single ascending fetch row \
         for row, or the conversation order the client renders will differ from the real one"
    );
}

// ══════════════════════════════════════════════════════════
//  R2 -- replayed historical turns need their memory drawer
// ══════════════════════════════════════════════════════════

/// Injection attribution goes in, comes back out, and **still comes back after
/// invalidation**, carrying a marker.
///
/// This is where storing fact ids rather than snapshots costs something: replay
/// sees the fact's present state. And that is exactly what we want — "the thing
/// this was based on no longer holds" is the information an audit most wants to
/// see, and hiding it would be falsifying the replay.
#[tokio::test]
async fn the_memory_drawer_survives_a_reload_and_marks_invalidated_facts() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let turn = common::new_episode("drawer", "我们用什么做对象存储来着");
    let source = common::new_episode("drawer", "对象存储就用 RustFS");
    let subject = common::new_entity("project", "Cortex");
    let still_true = common::new_fact(subject.id, "uses", "RustFS", source.id);
    let gone_stale = common::new_fact(subject.id, "uses", "MinIO", source.id);

    let (turn_id, kept, stale) = (turn.id, still_true.id, gone_stale.id);

    s.write_txn(async |t| {
        t.insert_episode(&turn).await?;
        t.insert_episode(&source).await?;
        t.insert_entity(&subject).await?;
        t.insert_fact(&still_true).await?;
        t.insert_fact(&gone_stale).await
    })
    .await
    .expect("manufacturing the data should not fail");

    // Two memories injected in fused-score order
    for (ordinal, fact_id, channels, score) in [
        (0, kept, vec!["bm25".to_owned(), "vector".to_owned()], 0.032),
        (1, stale, vec!["graph".to_owned()], 0.016),
    ] {
        let row = NewEpisodeMemory {
            id: Id::new(),
            episode_id: turn_id,
            fact_id,
            ordinal,
            channels,
            score: Some(score),
            device_id: common::DEVICE.to_owned(),
        };
        s.write_txn(async |t| t.insert_episode_memory(&row).await)
            .await
            .expect("writing the injection attribution should not fail");
    }

    // After injection, the second one is superseded by a new fact
    let ev = NewFactEvent::superseded(stale, kept, Utc::now(), common::DEVICE);
    s.write_txn(async |t| t.insert_fact_event(&ev).await)
        .await
        .expect("writing the invalidation event should not fail");

    let drawer = s
        .injected_memories(&turn_id.to_string())
        .await
        .expect("querying the drawer should not fail");

    db.cleanup().await;

    assert_eq!(
        drawer.len(),
        2,
        "both injected memories should be replayed, and the invalidated one **must not** be \
         filtered out: {drawer:?}"
    );
    assert_eq!(
        drawer.iter().map(|m| m.ordinal).collect::<Vec<_>>(),
        vec![0, 1],
        "injection order must be restored -- re-sorting by score will not do, since it is \
         nullable: {drawer:?}"
    );
    assert_eq!(
        drawer[0].channels.as_deref(),
        Some(["bm25".to_owned(), "vector".to_owned()].as_slice()),
        "the retrieval channels that hit must come back verbatim; in a bad case they are the \
         only thing that can localize the problem: {drawer:?}"
    );
    assert!(
        drawer[0].statement.is_some(),
        "statement should be JOINed out of facts -- the table stores only ids so that redact \
         clearing it in one place takes effect everywhere: {drawer:?}"
    );
    assert!(
        !drawer[0].invalidated && drawer[1].invalidated,
        "only the superseded one should carry the invalidation marker: {drawer:?}"
    );
}

/// A tool call's `path` must be **its own column**.
///
/// Clients used to regex the filename out of the rendered summary, so a change
/// in the wording silently displayed the wrong file.
#[tokio::test]
async fn tool_calls_replay_with_a_separate_path_column() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let turn = common::new_episode("tools", "把 README 里的版本号改一下");
    let turn_id = turn.id;
    s.write_txn(async |t| t.insert_episode(&turn).await)
        .await
        .expect("writing the episode should not fail");

    let calls = [
        (0, "read_file", Some("README.md"), true),
        (1, "write_file", Some("README.md"), true),
        // A tool that touches no file -- path must be nullable
        (2, "memory_search", None, true),
        (3, "read_file", Some("../etc/passwd"), false),
    ];
    for (ordinal, name, path, ok) in calls {
        let row = NewEpisodeToolCall {
            id: Id::new(),
            episode_id: turn_id,
            ordinal,
            name: name.to_owned(),
            path: path.map(str::to_owned),
            summary: format!("{name} 完成"),
            ok,
            device_id: common::DEVICE.to_owned(),
            diff: None,
        };
        s.write_txn(async |t| t.insert_episode_tool_call(&row).await)
            .await
            .expect("writing the tool attribution should not fail");
    }

    let replay = s
        .episode_tool_calls(&turn_id.to_string())
        .await
        .expect("querying the tool attribution should not fail");

    db.cleanup().await;

    assert_eq!(
        replay.iter().map(|c| c.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2, 3],
        "tool calls have causal relationships between them (list_dir before read_file), so they \
         must be replayed in call order: {replay:?}"
    );
    assert_eq!(
        replay.iter().map(|c| c.path.as_deref()).collect::<Vec<_>>(),
        vec![
            Some("README.md"),
            Some("README.md"),
            None,
            Some("../etc/passwd")
        ],
        "the path must be replayed verbatim, and NULL for tools that touch no file: {replay:?}"
    );
    assert!(
        !replay[3].ok,
        "a failed call must be marked faithfully -- \"which ones failed\" is what you need to see \
         at a glance during replay: {replay:?}"
    );
}

/// A diff that went in is still there when replayed.
///
/// # Why this one column gets its own test
///
/// "It was written but cannot be read back" has bitten this repository once
/// before (`FactDto` was missing two columns; persistence worked fine, the UI
/// stayed empty forever, and it went unnoticed for a long time). diff has
/// exactly the same shape: three read paths each hand-write their SELECT column
/// list, and missing one raises no error whatsoever — the change simply
/// "never happened" once you reopen the session.
///
/// It also actually trips the 8192 CHECK along the way. That CHECK is the last
/// fence (the agent side has already truncated once), and a fence nobody has
/// ever tripped is indistinguishable from a comment.
#[tokio::test]
async fn tool_call_diff_survives_replay_and_the_length_guard_bites() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let turn = common::new_episode("diff", "把第二行改一下");
    let turn_id = turn.id;
    s.write_txn(async |t| t.insert_episode(&turn).await)
        .await
        .expect("writing the episode should not fail");

    let diff = " line one\n-line two\n+line TWO\n line three\n";
    let row = |ordinal: i32, name: &str, diff: Option<&str>| NewEpisodeToolCall {
        id: Id::new(),
        episode_id: turn_id,
        ordinal,
        name: name.to_owned(),
        path: Some("note.txt".to_owned()),
        summary: format!("{name} 完成"),
        ok: true,
        device_id: common::DEVICE.to_owned(),
        diff: diff.map(str::to_owned),
    };

    for r in [row(0, "read_file", None), row(1, "write_file", Some(diff))] {
        s.write_txn(async |t| t.insert_episode_tool_call(&r).await)
            .await
            .expect("writing the tool attribution should not fail");
    }

    // One character over and the database should stop it. The agent side has
    // already truncated once, so reaching here means that truncation failed --
    // at which point we would rather the write fail than let an unbounded piece
    // of text broadcast to every device along sync_log
    let oversized = row(
        2,
        "write_file",
        Some(&"x".repeat(cortex_store::TOOL_DIFF_MAX_CHARS + 1)),
    );
    let rejected = s
        .write_txn(async |t| t.insert_episode_tool_call(&oversized).await)
        .await;

    let replay = s
        .episode_tool_calls(&turn_id.to_string())
        .await
        .expect("querying the tool attribution should not fail");

    // The fourth read path: the copy `/sync` delivers to **other devices**. It
    // has its own hand-written SELECT in `sync.rs`, independent of the three
    // above -- missing it when adding a column produces no compile error at
    // all, and the symptom is a batch-wide /sync 500 with the client's cursor
    // jammed from then on (far worse than one diff going undisplayed).
    //
    // The `sync_coverage` test does not stop this: what it inserts are ids of
    // rows that **do not exist**, so the SELECT always returns an empty set,
    // and sqlx only notices a missing column when it actually has to decode a
    // row
    let synced = s
        .fetch_since(0, 1000)
        .await
        .expect("the sync pull should not fail");

    db.cleanup().await;

    let synced_diff = synced.iter().find_map(|r| match &r.payload {
        Some(cortex_store::SyncPayload::EpisodeToolCall(c)) if c.name == "write_file" => {
            Some(c.diff.clone())
        }
        _ => None,
    });
    assert_eq!(
        synced_diff,
        Some(Some(diff.to_owned())),
        "the copy delivered over sync must carry the diff too -- otherwise this change \
         \"never happened\" on other devices: {synced:?}"
    );

    assert!(
        rejected.is_err(),
        "a diff over {} characters must be stopped by the CHECK -- this column ships to every \
         device via sync_log, and no cap means traffic nobody ever budgeted for",
        cortex_store::TOOL_DIFF_MAX_CHARS
    );
    assert_eq!(
        replay.iter().map(|c| c.diff.as_deref()).collect::<Vec<_>>(),
        vec![None, Some(diff)],
        "the diff must be replayed character for character, and NULL for tools that change no \
         file (not an empty string -- an empty string makes the UI draw an empty diff box that \
         looks like \"it was changed but nothing changed\"): {replay:?}"
    );
}

// ══════════════════════════════════════════════════════════
//  R5 -- media from presigned direct upload must be
//  catchable by the rescanner
// ══════════════════════════════════════════════════════════

/// The candidate list: everything not yet transcribed, and **never** anything
/// already redacted.
///
/// If a redacted blob were selected, the rescanner would periodically pull it
/// from object storage into memory and send it over the network — and even
/// though the pipeline's own recheck eventually discards the result, the secret
/// has by then left the process once.
#[tokio::test]
async fn the_backfill_queue_skips_transcribed_and_redacted_blobs() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let blob = |c: char, mime: &str| NewBlob {
        hash: c.to_string().repeat(64),
        mime: mime.to_owned(),
        size_bytes: 4096,
        storage_key: format!("blobs/{c}{c}"),
    };
    let pending = blob('a', "image/png"); // should be selected
    let done = blob('b', "image/jpeg"); // already transcribed
    let erased = blob('c', "image/png"); // already redacted
    let text = blob('d', "text/plain"); // type not in the coarse filter

    let transcript = cortex_store::NewBlobTranscript {
        id: Id::new(),
        blob_hash: done.hash.clone(),
        kind: cortex_store::TranscriptKind::VisionCaption,
        text: "一张截图".to_owned(),
        tsv_source: Some("一张 截图".to_owned()),
        embedding: None,
        span_start_ms: None,
        span_end_ms: None,
        transcribed_by: "test-vision".to_owned(),
        embedding_model: common::MODEL.to_owned(),
    };
    let tombstone = NewRedaction {
        id: Id::new(),
        target_kind: RedactionTarget::Blob,
        target_id: erased.hash.clone(),
        mode: RedactionMode::Redact,
        reason: "误传了身份证照片".to_owned(),
        actor: Actor::User.as_str().to_owned(),
        device_id: common::DEVICE.to_owned(),
    };

    s.write_txn(async |t| {
        for b in [&pending, &done, &erased, &text] {
            t.insert_blob(b).await?;
        }
        t.insert_blob_transcript(&transcript).await?;
        t.insert_redaction(&tombstone).await
    })
    .await
    .expect("manufacturing the data should not fail");

    let queue = s
        .blobs_missing_transcript(&["image/%".to_owned()], 100)
        .await
        .expect("querying the rescan candidates should not fail");
    let hashes: Vec<String> = queue.into_iter().map(|b| b.hash).collect();

    db.cleanup().await;

    assert_eq!(
        hashes,
        vec![pending.hash.clone()],
        "the candidate list should hold only the one that is untranscribed, unredacted, and of a \
         matching type: {hashes:?}"
    );
    assert!(
        !hashes.contains(&erased.hash),
        "a redacted blob must never enter the rescan queue -- that would have the secret pulled \
         periodically back into memory and sent over the network"
    );
}

/// The rescan is idempotent: once the transcript is backfilled, the same blob
/// must not appear in the queue again.
#[tokio::test]
async fn a_backfilled_blob_leaves_the_queue() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let blob = NewBlob {
        hash: "e".repeat(64),
        mime: "image/png".to_owned(),
        size_bytes: 40 * 1024 * 1024, // the direct-upload size tier
        storage_key: "blobs/ee".to_owned(),
    };
    s.write_txn(async |t| t.insert_blob(&blob).await)
        .await
        .expect("writing the blob should not fail");

    let before = queue_len(s, &blob.hash).await;

    let transcript = cortex_store::NewBlobTranscript {
        id: Id::new(),
        blob_hash: blob.hash.clone(),
        kind: cortex_store::TranscriptKind::VisionCaption,
        text: "回扫补上的描述".to_owned(),
        tsv_source: Some("回扫 补上 的 描述".to_owned()),
        embedding: None,
        span_start_ms: None,
        span_end_ms: None,
        transcribed_by: "test-vision".to_owned(),
        embedding_model: common::MODEL.to_owned(),
    };
    s.write_txn(async |t| t.insert_blob_transcript(&transcript).await)
        .await
        .expect("writing the transcript should not fail");

    let after = queue_len(s, &blob.hash).await;

    db.cleanup().await;

    assert_eq!(
        before, 1,
        "a large file that arrived by direct upload and has never been transcribed must enter \
         the queue"
    );
    assert_eq!(
        after, 0,
        "it must leave the queue once the transcript is backfilled, or the rescanner \
         re-transcribes the same object every round"
    );
}

async fn queue_len(s: &Store, hash: &str) -> usize {
    s.blobs_missing_transcript(&["image/%".to_owned()], 100)
        .await
        .expect("querying the rescan candidates should not fail")
        .into_iter()
        .filter(|b| b.hash == hash)
        .count()
}

//! Session digests and bulk attachment reads.
//!
//! Both queries summarize from `episodes` with no corresponding entity table,
//! so their correctness lives entirely in the SQL — a unit test cannot reach it,
//! only a real database connection can.

mod common;

use chrono::{Duration, Utc};
use cortex_core::Id;
use cortex_store::{NewBlob, NewEpisode, NewEpisodeBlob, Role, Store};

/// Manufacture a message with a given role and timestamp.
fn episode(session: &str, role: Role, text: &str, minutes_ago: i64) -> NewEpisode {
    NewEpisode {
        id: Id::new(),
        session_id: session.to_owned(),
        role,
        content: serde_json::json!({ "text": text }),
        text: Some(text.to_owned()),
        tsv_source: Some(text.to_owned()),
        domain: None,
        device_id: common::DEVICE.to_owned(),
        occurred_at: Utc::now() - Duration::minutes(minutes_ago),
    }
}

async fn write(store: &Store, ep: NewEpisode) -> Id {
    let id = ep.id;
    store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("writing the episode should not fail");
    id
}

#[tokio::test]
async fn digests_summarise_each_session() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // The old session: several messages, the first from the assistant (the
    // title must not be taken from it)
    write(s, episode("sess-old", Role::Assistant, "开场白", 300)).await;
    write(
        s,
        episode("sess-old", Role::User, "老会话的第一句用户消息", 299),
    )
    .await;
    write(s, episode("sess-old", Role::User, "老会话的最后一句", 290)).await;

    // The new session: only one message
    write(s, episode("sess-new", Role::User, "新会话唯一的一句", 5)).await;

    let digests = s
        .session_digests(10, false, None)
        .await
        .expect("querying session digests should not fail");
    let outcome: Vec<_> = digests
        .iter()
        .map(|d| {
            (
                d.session_id.clone(),
                d.message_count,
                d.first_user_text.clone(),
                d.last_text.clone(),
            )
        })
        .collect();

    db.cleanup().await;

    assert_eq!(
        outcome.len(),
        2,
        "exactly two sessions should be summarized: {outcome:?}"
    );
    assert_eq!(
        outcome[0].0, "sess-new",
        "the list must be ordered by last activity descending, newest session first: {outcome:?}"
    );
    assert_eq!(
        outcome[1].1, 3,
        "the old session should have 3 messages: {outcome:?}"
    );
    assert_eq!(
        outcome[1].2.as_deref(),
        Some("老会话的第一句用户消息"),
        "the title must come from the first **user** message, not the assistant's opening line: \
         {outcome:?}"
    );
    assert_eq!(
        outcome[1].3.as_deref(),
        Some("老会话的最后一句"),
        "the preview must come from the last message that had a body: {outcome:?}"
    );
}

/// The limit must apply to **sessions**, not to "take the most recent N
/// messages and then summarize".
///
/// The latter is what this used to do, and the defect raises no error: one
/// talkative session can fill the whole message window, so yesterday's sessions
/// disappear from the list — and what the user sees is "my history is gone".
#[tokio::test]
async fn a_chatty_session_cannot_crowd_out_a_quiet_one() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // The quiet old session: one message, and the earliest of all
    write(
        s,
        episode("sess-quiet", Role::User, "很久以前说过的一句话", 600),
    )
    .await;

    // The talkative session: 40 messages, all newer than the one above
    for i in 0..40 {
        write(
            s,
            episode("sess-chatty", Role::User, &format!("第 {i} 句"), 100 - i),
        )
        .await;
    }

    let digests = s
        .session_digests(10, false, None)
        .await
        .expect("querying session digests should not fail");
    let ids: Vec<String> = digests.iter().map(|d| d.session_id.clone()).collect();
    let chatty_count = digests
        .iter()
        .find(|d| d.session_id == "sess-chatty")
        .map(|d| d.message_count);

    db.cleanup().await;

    assert!(
        ids.contains(&"sess-quiet".to_string()),
        "the quiet old session was crowded out of the list by the talkative one -- the \
         summarizing was not pushed down into SQL: {ids:?}"
    );
    assert_eq!(
        chatty_count,
        Some(40),
        "the talkative session's message count is wrong: {ids:?}"
    );
}

/// Session detail has to hang attachments off every message, and querying one at
/// a time is N+1.
#[tokio::test]
async fn attachments_come_back_in_one_round_trip() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let with_image = write(s, episode("sess-media", Role::User, "看这张图", 10)).await;
    let plain = write(s, episode("sess-media", Role::Assistant, "收到", 9)).await;

    // The blob rows must precede the link rows -- episode_blobs has foreign
    // keys onto both sides
    let hash_a = "a".repeat(64);
    let hash_b = "b".repeat(64);
    for hash in [&hash_a, &hash_b] {
        let blob = NewBlob {
            hash: hash.clone(),
            mime: "image/png".into(),
            size_bytes: 1,
            storage_key: format!("blobs/{}/{}/{hash}", &hash[0..2], &hash[2..4]),
        };
        s.write_txn(async |t| t.insert_blob(&blob).await)
            .await
            .expect("writing the blob should not fail");
    }
    for (hash, kind, filename) in [
        (&hash_a, "image", Some("设计稿.png")),
        // The filename may be null: old data has none, and it cannot be
        // backfilled
        (&hash_b, "document", None),
    ] {
        let link = NewEpisodeBlob {
            episode_id: with_image,
            blob_hash: hash.clone(),
            kind: Some(kind.to_owned()),
            filename: filename.map(str::to_owned),
        };
        s.write_txn(async |t| t.link_episode_blob(&link).await)
            .await
            .expect("writing episode_blobs should not fail");
    }

    let ids = vec![with_image.to_string(), plain.to_string()];
    let links = s
        .episode_attachments_bulk(&ids)
        .await
        .expect("the bulk attachment query should not fail");
    let empty = s
        .episode_attachments_bulk(&[])
        .await
        .expect("empty input should issue no query, let alone raise an error");

    db.cleanup().await;

    assert_eq!(
        links.len(),
        2,
        "both links should come back, and in a single round trip: {links:?}"
    );
    assert!(
        links.iter().all(|l| l.episode_id == with_image.to_string()),
        "the other message has no attachments and must not get mixed in: {links:?}"
    );
    assert_eq!(
        links.iter().filter_map(|l| l.kind.as_deref()).count(),
        2,
        "kind must be returned verbatim; it is the client's only basis for telling images from \
         documents: {links:?}"
    );
    // This one is the crux of R3: without mime / size, reopening a session
    // leaves a PDF displayable only as "document · a1b2c3d4"
    assert!(
        links
            .iter()
            .all(|l| l.mime == "image/png" && l.size_bytes == 1),
        "mime and size must be JOINed out of blobs and ride along: {links:?}"
    );
    let named: Vec<_> = links.iter().filter_map(|l| l.filename.as_deref()).collect();
    assert_eq!(
        named,
        vec!["设计稿.png"],
        "the filename belongs to this particular reference: carry it back verbatim when present, \
         and None when absent: {links:?}"
    );
    assert!(
        empty.is_empty(),
        "empty input should return empty straight away"
    );
}

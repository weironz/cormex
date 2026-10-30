//! Fact lifecycle and entity ownership: acceptance tests for the views'
//! semantics.
//!
//! These assertions correspond directly to the promises in docs/memory.md §11
//! and §4 — deletion is reversible, the final state is decided by the last
//! status event, and a chained merge can be walked to its end.

mod common;

use cormex_store::{
    Actor, FactOp, InvalidationKind, NewBlob, NewBlobTranscript, NewEntityMerge, NewEpisodeBlob,
    NewFactEvent, NewRedaction, NewSummary, ProvenanceRef, RedactionMode, RedactionTarget,
    StoreError, SummaryScope, TranscriptKind, Vector,
};

#[tokio::test]
async fn delete_then_restore_round_trips_through_active_facts() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "我们改用 Postgres");
    let entity = common::new_entity("project", "Cormex");
    let fact = common::new_fact(entity.id, "uses", "Postgres", episode.id);

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&fact).await
        })
        .await
        .expect("the write transaction should not fail");

    let subject = entity.id.to_string();
    let fact_id = fact.id.to_string();

    // ① Just written: valid
    assert_eq!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("the query should not fail")
            .len(),
        1
    );
    assert!(
        store
            .fact_status(&fact_id)
            .await
            .expect("the query should not fail")
            .is_none(),
        "a fact that has never been invalidated must not appear in fact_status"
    );

    // ② The user deletes it → it leaves active_facts, but the main row remains
    // (append-only)
    let retract = NewFactEvent::retracted(fact.id, common::now(), "记错了", common::DEVICE);
    store
        .write_txn(async |tx| tx.insert_fact_event(&retract).await)
        .await
        .expect("the write transaction should not fail");

    assert!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("the query should not fail")
            .is_empty(),
        "a deleted fact must not appear in active_facts"
    );
    assert!(
        store
            .fact(&fact_id)
            .await
            .expect("the query should not fail")
            .is_some(),
        "deletion only appends an event, so the main row must still be there"
    );

    let status = store
        .fact_status(&fact_id)
        .await
        .expect("the query should not fail")
        .expect("there should be a status event");
    assert_eq!(status.op, FactOp::Invalidate);
    assert_eq!(status.kind, Some(InvalidationKind::Retracted));
    assert_eq!(status.actor, Actor::User);

    // ③ Restore → it re-enters active_facts
    let revoke = NewFactEvent::revoke(fact.id, Actor::User, common::DEVICE);
    store
        .write_txn(async |tx| tx.insert_fact_event(&revoke).await)
        .await
        .expect("the write transaction should not fail");

    assert_eq!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("the query should not fail")
            .len(),
        1,
        "the fact should be valid again after the revoke"
    );
    assert_eq!(
        store
            .fact_status(&fact_id)
            .await
            .expect("the query should not fail")
            .expect("there should be a status event")
            .op,
        FactOp::Revoke
    );

    // ④ The timeline is fully visible: not one of the events is missing
    let timeline = store
        .fact_events(&fact_id)
        .await
        .expect("the query should not fail");
    assert_eq!(timeline.len(), 2, "two lifecycle events");
    assert_eq!(timeline[0].op, FactOp::Invalidate);
    assert_eq!(timeline[1].op, FactOp::Revoke);

    db.cleanup().await;
}

#[tokio::test]
async fn last_status_event_wins_and_flag_is_only_an_annotation() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "取舍");
    let entity = common::new_entity("project", "Cormex");
    let fact = common::new_fact(entity.id, "prefers", "RRF", episode.id);

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&fact).await
        })
        .await
        .expect("the write transaction should not fail");

    let subject = entity.id.to_string();
    let fact_id = fact.id.to_string();

    // flag is an annotation and does not change the status
    let flag = NewFactEvent::flag(fact.id, "与另一条矛盾", common::DEVICE);
    store
        .write_txn(async |tx| tx.insert_fact_event(&flag).await)
        .await
        .expect("the write transaction should not fail");

    assert_eq!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("the query should not fail")
            .len(),
        1,
        "a flag must not push the fact out of active_facts"
    );
    assert!(
        store
            .fact_status(&fact_id)
            .await
            .expect("the query should not fail")
            .is_none(),
        "a flag is not a status event and must not enter fact_status"
    );

    // After the sequence [invalidate, invalidate, revoke] the fact is **valid**
    // -- state machine semantics, not an undo stack
    for event in [
        NewFactEvent::retracted(fact.id, common::now(), "删一次", common::DEVICE),
        NewFactEvent::retracted(fact.id, common::now(), "再删一次", common::DEVICE),
        NewFactEvent::revoke(fact.id, Actor::User, common::DEVICE),
    ] {
        store
            .write_txn(async |tx| tx.insert_fact_event(&event).await)
            .await
            .expect("the write transaction should not fail");
    }

    assert_eq!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("the query should not fail")
            .len(),
        1,
        "the final state is revoke, so the fact should be valid (undo-stack semantics would call \
         it invalidated, which is wrong)"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn superseding_a_fact_links_the_evolution_chain() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "先用 SQLite，后来换成 Postgres");
    let entity = common::new_entity("project", "Cormex");
    let old = common::new_fact(entity.id, "uses", "SQLite", episode.id);
    let new = common::new_fact(entity.id, "uses", "Postgres", episode.id);

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&old).await
        })
        .await
        .expect("the write transaction should not fail");

    // Supersede: the new fact and the invalidation event are written in the
    // same transaction
    let supersede = NewFactEvent::superseded(old.id, new.id, common::now(), common::DEVICE);
    store
        .write_txn(async |tx| {
            tx.insert_fact(&new).await?;
            tx.insert_fact_event(&supersede).await
        })
        .await
        .expect("the write transaction should not fail");

    let active = store
        .active_facts_by_subject_predicate(&entity.id.to_string(), "uses")
        .await
        .expect("the query should not fail");
    assert_eq!(
        active.len(),
        1,
        "only the new one should remain under the same (subject, predicate)"
    );
    assert_eq!(active[0].id, new.id.to_string());

    let status = store
        .fact_status(&old.id.to_string())
        .await
        .expect("the query should not fail")
        .expect("the old fact should have a status event");
    assert_eq!(status.kind, Some(InvalidationKind::Superseded));
    assert_eq!(
        status.superseded_by,
        Some(new.id.to_string()),
        "the evolution chain must state what superseded it"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn chained_entity_merges_resolve_to_the_final_canonical() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let a = common::new_entity("person", "老王");
    let b = common::new_entity("person", "王先生");
    let c = common::new_entity("person", "Wang");

    store
        .write_txn(async |tx| {
            tx.insert_entity(&a).await?;
            tx.insert_entity(&b).await?;
            tx.insert_entity(&c).await
        })
        .await
        .expect("the write transaction should not fail");

    // A → B → C, written as two separate steps, simulating two resolutions
    // that happened one after the other
    for merge in [
        NewEntityMerge {
            id: cormex_core::Id::new(),
            from_entity: a.id,
            into_entity: b.id,
            reason: Some("同一人".to_owned()),
            source_episode_id: None,
            device_id: common::DEVICE.to_owned(),
        },
        NewEntityMerge {
            id: cormex_core::Id::new(),
            from_entity: b.id,
            into_entity: c.id,
            reason: Some("还是同一人".to_owned()),
            source_episode_id: None,
            device_id: common::DEVICE.to_owned(),
        },
    ] {
        store
            .write_txn(async |tx| tx.insert_entity_merge(&merge).await)
            .await
            .expect("the write transaction should not fail");
    }

    let canonical = c.id.to_string();
    for id in [a.id, b.id, c.id] {
        assert_eq!(
            store
                .canonical_entity(&id.to_string())
                .await
                .expect("the query should not fail"),
            Some(canonical.clone()),
            "after the chained merge A→B→C, {id}'s owner should be C"
        );
    }

    // Bulk resolution goes through the same view
    let ids: Vec<String> = vec![a.id.to_string(), b.id.to_string(), c.id.to_string()];
    let resolved = store
        .canonical_entities(&ids)
        .await
        .expect("the query should not fail");
    assert_eq!(resolved.len(), 3);
    assert!(resolved.iter().all(|row| row.canonical == canonical));

    db.cleanup().await;
}

#[tokio::test]
async fn concurrent_merge_fork_is_first_writer_wins() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let a = common::new_entity("person", "老王");
    let b = common::new_entity("person", "王先生");
    let c = common::new_entity("person", "Wang");

    store
        .write_txn(async |tx| {
            tx.insert_entity(&a).await?;
            tx.insert_entity(&b).await?;
            tx.insert_entity(&c).await
        })
        .await
        .expect("the write transaction should not fail");

    let first = NewEntityMerge {
        id: cormex_core::Id::new(),
        from_entity: a.id,
        into_entity: b.id,
        reason: None,
        source_episode_id: None,
        device_id: "device-1".to_owned(),
    };
    store
        .write_txn(async |tx| tx.insert_entity_merge(&first).await)
        .await
        .expect("the first merge should succeed");

    // Another device merges A into C -- UNIQUE(from_entity) rejects whoever
    // arrives second at the physical level
    let fork = NewEntityMerge {
        id: cormex_core::Id::new(),
        from_entity: a.id,
        into_entity: c.id,
        reason: None,
        source_episode_id: None,
        device_id: "device-2".to_owned(),
    };
    let err = store
        .write_txn(async |tx| tx.insert_entity_merge(&fork).await)
        .await
        .expect_err(
            "the later arrival of a concurrent fork should be rejected by the unique constraint",
        );

    match err {
        StoreError::Sql(sqlx::Error::Database(db_err)) => {
            assert_eq!(
                db_err.code().as_deref(),
                Some("23505"),
                "it should be a unique constraint violation; got: {db_err}"
            );
        }
        other => panic!("wrong error type: {other}"),
    }

    assert_eq!(
        store
            .canonical_entity(&a.id.to_string())
            .await
            .expect("the query should not fail"),
        Some(b.id.to_string()),
        "first-writer-wins: the owner should still be B, which was written first"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn blobs_transcripts_and_vectors_round_trip() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("media", "看这段录音");
    let blob = NewBlob {
        hash: "b".repeat(64),
        mime: "audio/mpeg".to_owned(),
        size_bytes: 1024,
        storage_key: "blobs/bb".to_owned(),
    };
    let link = NewEpisodeBlob {
        episode_id: episode.id,
        blob_hash: blob.hash.clone(),
        kind: Some("attachment".to_owned()),
        filename: Some("会议录音.mp3".to_owned()),
    };
    let embedding = Vector::from(vec![0.25_f32; cormex_store::EMBEDDING_DIM]);
    let transcript = NewBlobTranscript {
        id: cormex_core::Id::new(),
        blob_hash: blob.hash.clone(),
        kind: TranscriptKind::Asr,
        text: "这是转录出来的文本".to_owned(),
        tsv_source: Some("这是 转录 出来 的 文本".to_owned()),
        embedding: Some(embedding.clone()),
        span_start_ms: Some(1_900_000),
        span_end_ms: Some(1_920_000),
        transcribed_by: "whisper-test".to_owned(),
        embedding_model: common::MODEL.to_owned(),
    };

    store
        .write_txn(async |tx| {
            // The fixed three-step order: object storage → the blobs row →
            // the episode plus its link rows
            tx.insert_blob(&blob).await?;
            tx.insert_episode(&episode).await?;
            tx.link_episode_blob(&link).await?;
            tx.insert_blob_transcript(&transcript).await
        })
        .await
        .expect("the write transaction should not fail");

    assert_eq!(
        store
            .blob_reference_count(&blob.hash)
            .await
            .expect("the query should not fail"),
        1
    );
    let attachments = store
        .episode_attachments(&episode.id.to_string())
        .await
        .expect("the query should not fail");
    assert_eq!(attachments.len(), 1);
    // mime and size must ride along: given only the hash, a client reopening
    // the session has nothing in hand but a string of hex and can only draw it
    // as "document · b1b2b3b4"
    assert_eq!(
        (
            attachments[0].mime.as_str(),
            attachments[0].size_bytes,
            attachments[0].filename.as_deref()
        ),
        ("audio/mpeg", 1024, Some("会议录音.mp3")),
        "an attachment must carry mime / size / filename, or replay is left with nothing but a \
         string of hex: {attachments:?}"
    );

    let transcripts = store
        .blob_transcripts(&blob.hash)
        .await
        .expect("the query should not fail");
    assert_eq!(transcripts.len(), 1);
    assert_eq!(transcripts[0].kind, TranscriptKind::Asr);
    assert_eq!(transcripts[0].span_start_ms, Some(1_900_000));
    assert_eq!(
        transcripts[0].embedding.as_ref(),
        Some(&embedding),
        "the 1024-dimensional vector should read back verbatim"
    );

    // episode_blobs' composite record_id also resolves on the sync side
    let batch = store
        .fetch_since(0, 100)
        .await
        .expect("the pull should not fail");
    let composite = batch
        .iter()
        .find(|r| r.table_name == cormex_store::table::EPISODE_BLOBS)
        .expect("there should be a sync record for episode_blobs");
    assert_eq!(composite.record_id, link.record_id());
    assert!(
        composite.payload.is_some(),
        "the business row behind a composite key should be fetched back too"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn redaction_tombstone_is_visible_to_in_flight_guard() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("secret", "这里粘错了一个 API key");
    store
        .write_txn(async |tx| tx.insert_episode(&episode).await)
        .await
        .expect("the write transaction should not fail");

    let target_id = episode.id.to_string();
    assert!(
        !store
            .is_redacted(RedactionTarget::Episode, &target_id)
            .await
            .expect("the query should not fail")
    );

    let tombstone = NewRedaction {
        id: cormex_core::Id::new(),
        target_kind: RedactionTarget::Episode,
        target_id: target_id.clone(),
        mode: RedactionMode::Redact,
        reason: "误粘贴密钥".to_owned(),
        actor: "user".to_owned(),
        device_id: common::DEVICE.to_owned(),
    };
    store
        .write_txn(async |tx| tx.insert_redaction(&tombstone).await)
        .await
        .expect("the write transaction should not fail");

    assert!(
        store
            .is_redacted(RedactionTarget::Episode, &target_id)
            .await
            .expect("the query should not fail"),
        "the in-flight task guard must be able to see the tombstone"
    );
    assert_eq!(
        store
            .redactions_for_target(RedactionTarget::Episode, &target_id)
            .await
            .expect("the query should not fail")
            .len(),
        1
    );

    db.cleanup().await;
}

#[tokio::test]
async fn summaries_and_episode_queries_work() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let mut covered = Vec::new();
    for i in 0..3 {
        let episode = common::new_episode("sess-a", &format!("第 {i} 轮"));
        covered.push(episode.id);
        store
            .write_txn(async |tx| tx.insert_episode(&episode).await)
            .await
            .expect("the write transaction should not fail");
    }
    let other = common::new_episode("sess-b", "另一场会话");
    store
        .write_txn(async |tx| tx.insert_episode(&other).await)
        .await
        .expect("the write transaction should not fail");

    assert_eq!(
        store
            .episodes_by_session("sess-a", 10)
            .await
            .expect("the query should not fail")
            .len(),
        3
    );
    assert_eq!(
        store
            .recent_episodes(2)
            .await
            .expect("the query should not fail")
            .len(),
        2
    );

    let summary = NewSummary {
        id: cormex_core::Id::new(),
        scope: SummaryScope::Session,
        scope_key: "sess-a".to_owned(),
        text: "这场会话讨论了存储层".to_owned(),
        tsv_source: Some("这场 会话 讨论 了 存储层".to_owned()),
        embedding: None,
        embedding_model: common::MODEL.to_owned(),
        covers_from: None,
        covers_to: None,
        // A summary must state which episodes it was summarized from, or once
        // the source is redacted there is no way whatsoever to find it
        // (docs/memory-content.md §5.3)
        sources: covered
            .iter()
            .copied()
            .map(ProvenanceRef::episode)
            .collect(),
        device_id: common::DEVICE.to_owned(),
    };
    store
        .write_txn(async |tx| tx.insert_summary(&summary).await)
        .await
        .expect("the write transaction should not fail");

    let found = store
        .summaries_for_scope(SummaryScope::Session, "sess-a", 10)
        .await
        .expect("the query should not fail");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].scope, SummaryScope::Session);

    db.cleanup().await;
}

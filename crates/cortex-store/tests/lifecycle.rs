//! 事实生命周期与实体归属：视图语义的验收。
//!
//! 这几条断言直接对应 docs/memory.md §十一 与 §四 的承诺 ——
//! 删除是可逆的、末态由最后一条状态事件决定、链式合并能走到底。

mod common;

use cortex_store::{
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
    let entity = common::new_entity("project", "Cortex");
    let fact = common::new_fact(entity.id, "uses", "Postgres", episode.id);

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&fact).await
        })
        .await
        .expect("写事务不应失败");

    let subject = entity.id.to_string();
    let fact_id = fact.id.to_string();

    // ① 刚写入：有效
    assert_eq!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("查询不应失败")
            .len(),
        1
    );
    assert!(
        store
            .fact_status(&fact_id)
            .await
            .expect("查询不应失败")
            .is_none(),
        "从未失效过的事实不该出现在 fact_status 里"
    );

    // ② 用户删除 → 退出 active_facts，但主行仍在（append-only）
    let retract = NewFactEvent::retracted(fact.id, common::now(), "记错了", common::DEVICE);
    store
        .write_txn(async |tx| tx.insert_fact_event(&retract).await)
        .await
        .expect("写事务不应失败");

    assert!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("查询不应失败")
            .is_empty(),
        "已删除的事实不该出现在 active_facts"
    );
    assert!(
        store.fact(&fact_id).await.expect("查询不应失败").is_some(),
        "删除只是追加事件，主行必须还在"
    );

    let status = store
        .fact_status(&fact_id)
        .await
        .expect("查询不应失败")
        .expect("应当有状态事件");
    assert_eq!(status.op, FactOp::Invalidate);
    assert_eq!(status.kind, Some(InvalidationKind::Retracted));
    assert_eq!(status.actor, Actor::User);

    // ③ 恢复 → 重新进入 active_facts
    let revoke = NewFactEvent::revoke(fact.id, Actor::User, common::DEVICE);
    store
        .write_txn(async |tx| tx.insert_fact_event(&revoke).await)
        .await
        .expect("写事务不应失败");

    assert_eq!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("查询不应失败")
            .len(),
        1,
        "revoke 之后事实应当重新有效"
    );
    assert_eq!(
        store
            .fact_status(&fact_id)
            .await
            .expect("查询不应失败")
            .expect("应当有状态事件")
            .op,
        FactOp::Revoke
    );

    // ④ 时间线完整可见：三条事件一条不少
    let timeline = store.fact_events(&fact_id).await.expect("查询不应失败");
    assert_eq!(timeline.len(), 2, "两条生命周期事件");
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
    let entity = common::new_entity("project", "Cortex");
    let fact = common::new_fact(entity.id, "prefers", "RRF", episode.id);

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&fact).await
        })
        .await
        .expect("写事务不应失败");

    let subject = entity.id.to_string();
    let fact_id = fact.id.to_string();

    // flag 是批注，不改变状态
    let flag = NewFactEvent::flag(fact.id, "与另一条矛盾", common::DEVICE);
    store
        .write_txn(async |tx| tx.insert_fact_event(&flag).await)
        .await
        .expect("写事务不应失败");

    assert_eq!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("查询不应失败")
            .len(),
        1,
        "flag 不该让事实退出 active_facts"
    );
    assert!(
        store
            .fact_status(&fact_id)
            .await
            .expect("查询不应失败")
            .is_none(),
        "flag 不是状态事件，不该进 fact_status"
    );

    // 序列 [invalidate, invalidate, revoke] 之后事实**有效** ——
    // 状态机语义，不是撤销栈
    for event in [
        NewFactEvent::retracted(fact.id, common::now(), "删一次", common::DEVICE),
        NewFactEvent::retracted(fact.id, common::now(), "再删一次", common::DEVICE),
        NewFactEvent::revoke(fact.id, Actor::User, common::DEVICE),
    ] {
        store
            .write_txn(async |tx| tx.insert_fact_event(&event).await)
            .await
            .expect("写事务不应失败");
    }

    assert_eq!(
        store
            .active_facts_by_subject(&subject, 10)
            .await
            .expect("查询不应失败")
            .len(),
        1,
        "末态是 revoke，事实应当有效（撤销栈语义会算成失效，那是错的）"
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
    let entity = common::new_entity("project", "Cortex");
    let old = common::new_fact(entity.id, "uses", "SQLite", episode.id);
    let new = common::new_fact(entity.id, "uses", "Postgres", episode.id);

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&old).await
        })
        .await
        .expect("写事务不应失败");

    // 取代：新事实与失效事件同事务写入
    let supersede = NewFactEvent::superseded(old.id, new.id, common::now(), common::DEVICE);
    store
        .write_txn(async |tx| {
            tx.insert_fact(&new).await?;
            tx.insert_fact_event(&supersede).await
        })
        .await
        .expect("写事务不应失败");

    let active = store
        .active_facts_by_subject_predicate(&entity.id.to_string(), "uses")
        .await
        .expect("查询不应失败");
    assert_eq!(active.len(), 1, "同一 (主语, 谓词) 下只应剩新的那条");
    assert_eq!(active[0].id, new.id.to_string());

    let status = store
        .fact_status(&old.id.to_string())
        .await
        .expect("查询不应失败")
        .expect("旧事实应当有状态事件");
    assert_eq!(status.kind, Some(InvalidationKind::Superseded));
    assert_eq!(
        status.superseded_by,
        Some(new.id.to_string()),
        "演化链必须指明被谁取代"
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
        .expect("写事务不应失败");

    // A → B → C，两步分开写，模拟先后发生的两次消解
    for merge in [
        NewEntityMerge {
            id: cortex_core::Id::new(),
            from_entity: a.id,
            into_entity: b.id,
            reason: Some("同一人".to_owned()),
            source_episode_id: None,
            device_id: common::DEVICE.to_owned(),
        },
        NewEntityMerge {
            id: cortex_core::Id::new(),
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
            .expect("写事务不应失败");
    }

    let canonical = c.id.to_string();
    for id in [a.id, b.id, c.id] {
        assert_eq!(
            store
                .canonical_entity(&id.to_string())
                .await
                .expect("查询不应失败"),
            Some(canonical.clone()),
            "链式合并 A→B→C 之后 {id} 的归属应当是 C"
        );
    }

    // 批量解析走同一视图
    let ids: Vec<String> = vec![a.id.to_string(), b.id.to_string(), c.id.to_string()];
    let resolved = store.canonical_entities(&ids).await.expect("查询不应失败");
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
        .expect("写事务不应失败");

    let first = NewEntityMerge {
        id: cortex_core::Id::new(),
        from_entity: a.id,
        into_entity: b.id,
        reason: None,
        source_episode_id: None,
        device_id: "device-1".to_owned(),
    };
    store
        .write_txn(async |tx| tx.insert_entity_merge(&first).await)
        .await
        .expect("第一条合并应当成功");

    // 另一台设备把 A 并进 C —— UNIQUE(from_entity) 在物理层拒绝后到者
    let fork = NewEntityMerge {
        id: cortex_core::Id::new(),
        from_entity: a.id,
        into_entity: c.id,
        reason: None,
        source_episode_id: None,
        device_id: "device-2".to_owned(),
    };
    let err = store
        .write_txn(async |tx| tx.insert_entity_merge(&fork).await)
        .await
        .expect_err("并发分叉的后到者应当被唯一约束拒绝");

    match err {
        StoreError::Sql(sqlx::Error::Database(db_err)) => {
            assert_eq!(
                db_err.code().as_deref(),
                Some("23505"),
                "应当是唯一约束冲突，实际：{db_err}"
            );
        }
        other => panic!("错误类型不对：{other}"),
    }

    assert_eq!(
        store
            .canonical_entity(&a.id.to_string())
            .await
            .expect("查询不应失败"),
        Some(b.id.to_string()),
        "first-writer-wins：归属仍应是先写入的 B"
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
    let embedding = Vector::from(vec![0.25_f32; cortex_store::EMBEDDING_DIM]);
    let transcript = NewBlobTranscript {
        id: cortex_core::Id::new(),
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
            // 三步固定顺序：对象存储 → blobs 行 → episode + 关联行
            tx.insert_blob(&blob).await?;
            tx.insert_episode(&episode).await?;
            tx.link_episode_blob(&link).await?;
            tx.insert_blob_transcript(&transcript).await
        })
        .await
        .expect("写事务不应失败");

    assert_eq!(
        store
            .blob_reference_count(&blob.hash)
            .await
            .expect("查询不应失败"),
        1
    );
    let attachments = store
        .episode_attachments(&episode.id.to_string())
        .await
        .expect("查询不应失败");
    assert_eq!(attachments.len(), 1);
    // mime 与 size 必须随行返回：只给 hash 的话，重开会话时客户端手上
    // 除了一串哈希什么都没有，只能画成「文档 · b1b2b3b4」
    assert_eq!(
        (
            attachments[0].mime.as_str(),
            attachments[0].size_bytes,
            attachments[0].filename.as_deref()
        ),
        ("audio/mpeg", 1024, Some("会议录音.mp3")),
        "附件必须带上 mime / 大小 / 文件名，否则回放时只剩一串哈希：{attachments:?}"
    );

    let transcripts = store
        .blob_transcripts(&blob.hash)
        .await
        .expect("查询不应失败");
    assert_eq!(transcripts.len(), 1);
    assert_eq!(transcripts[0].kind, TranscriptKind::Asr);
    assert_eq!(transcripts[0].span_start_ms, Some(1_900_000));
    assert_eq!(
        transcripts[0].embedding.as_ref(),
        Some(&embedding),
        "1024 维向量应当原样读回"
    );

    // episode_blobs 的复合 record_id 在同步侧也解析得回来
    let batch = store.fetch_since(0, 100).await.expect("拉取不应失败");
    let composite = batch
        .iter()
        .find(|r| r.table_name == cortex_store::table::EPISODE_BLOBS)
        .expect("应当有 episode_blobs 的同步记录");
    assert_eq!(composite.record_id, link.record_id());
    assert!(composite.payload.is_some(), "复合键的业务行也应当被取回来");

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
        .expect("写事务不应失败");

    let target_id = episode.id.to_string();
    assert!(
        !store
            .is_redacted(RedactionTarget::Episode, &target_id)
            .await
            .expect("查询不应失败")
    );

    let tombstone = NewRedaction {
        id: cortex_core::Id::new(),
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
        .expect("写事务不应失败");

    assert!(
        store
            .is_redacted(RedactionTarget::Episode, &target_id)
            .await
            .expect("查询不应失败"),
        "在途任务防护必须能看到墓碑"
    );
    assert_eq!(
        store
            .redactions_for_target(RedactionTarget::Episode, &target_id)
            .await
            .expect("查询不应失败")
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
            .expect("写事务不应失败");
    }
    let other = common::new_episode("sess-b", "另一场会话");
    store
        .write_txn(async |tx| tx.insert_episode(&other).await)
        .await
        .expect("写事务不应失败");

    assert_eq!(
        store
            .episodes_by_session("sess-a", 10)
            .await
            .expect("查询不应失败")
            .len(),
        3
    );
    assert_eq!(
        store.recent_episodes(2).await.expect("查询不应失败").len(),
        2
    );

    let summary = NewSummary {
        id: cortex_core::Id::new(),
        scope: SummaryScope::Session,
        scope_key: "sess-a".to_owned(),
        text: "这场会话讨论了存储层".to_owned(),
        tsv_source: Some("这场 会话 讨论 了 存储层".to_owned()),
        embedding: None,
        embedding_model: common::MODEL.to_owned(),
        covers_from: None,
        covers_to: None,
        // 摘要必须说清楚自己是从哪几条 episode 摘出来的，否则源被 redact 后
        // 没有任何办法找到它（docs/memory-content.md §5.3）
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
        .expect("写事务不应失败");

    let found = store
        .summaries_for_scope(SummaryScope::Session, "sess-a", 10)
        .await
        .expect("查询不应失败");
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].scope, SummaryScope::Session);

    db.cleanup().await;
}

//! 来源信任级与派生血缘：migration 20260807000006 的验收。
//!
//! 这一组测试守的两条承诺，共同点是**错了就不可逆**：
//!
//! 1. 存量行拿到的是诚实的「不知道」，不是一个查询时会被当真的档位。
//!    改它需要 UPDATE，而 UPDATE 只在 redact/purge 里被允许 —— 也就是说
//!    这个值一旦写错，就永远错下去。
//! 2. 派生物写进去时必须带血缘。血缘事后补不出来（谁摘的、摘了什么，
//!    只有生成它的那一步知道），而没有血缘的摘要在源被 redact 之后
//!    会继续泄露内容（docs/memory-content.md §5.3）。

mod common;

use cortex_core::Id;
use cortex_store::{
    FactSource, NewSummary, ProvenanceRef, SourceChannel, StoreError, SummaryScope, SyncPayload,
    table,
};

/// 加列之前的最后一个 migration 版本。
const BEFORE_PROVENANCE: i64 = 20_260_807_000_005;

// ══════════════════════════════════════════════════════════
//  一、存量行
// ══════════════════════════════════════════════════════════

/// 存量行必须落在 `unknown_legacy` / NULL 上。
///
/// 为什么不能给它一个真实档位：现存每条 fact 的 `source_episode_id` 按构造
/// 都指向 `role='user'` 的那条 episode，而送进抽取器的是「用户：… / 助手：…」
/// 拼起来的一整块 —— 库里没有任何痕迹能区分「用户亲述」与「模型推断」。
/// 标 `user_stated` 就是凭空拔高，而 tier 1 的用途恰恰是「这批不该被外部
/// 内容盖掉」，标错方向是有害的。
///
/// 顺带钉住一个 Postgres 行为：`ADD COLUMN NOT NULL DEFAULT` 之后紧接着
/// `DROP DEFAULT`，**存量行的取值仍在**（PG 把它存在 attmissingval 里，
/// DROP DEFAULT 不回填也不重写表）。migration 靠这一点做到「存量行有值、
/// 今后忘写会报错」，这个假设值得被真库验一次。
#[tokio::test]
async fn legacy_facts_get_an_honest_unknown_not_a_fabricated_tier() {
    let Some(db) = common::setup_upto(Some(BEFORE_PROVENANCE)).await else {
        return;
    };

    // 造一条「加列之前就存在」的事实。必须走裸 SQL：此刻表上还没有这两列，
    // 而 WriteTxn::insert_fact 已经在写它们了
    let episode = Id::new();
    let entity = Id::new();
    let fact = Id::new();
    db.exec_raw(format!(
        "INSERT INTO episodes (id, session_id, role, content, device_id, occurred_at)
         VALUES ('{episode}', 's-legacy', 'user', '{{}}'::jsonb, 'dev', now());
         INSERT INTO entities (id, kind, name, embedding_model, device_id)
         VALUES ('{entity}', 'person', '老张', 'bge-m3', 'dev');
         INSERT INTO facts
             (id, subject_id, predicate, object_text, statement, embedding_model,
              source_episode_id, extracted_by, device_id)
         VALUES ('{fact}', '{entity}', 'uses', 'Postgres', '老张用 Postgres',
                 'bge-m3', '{episode}', 'legacy-extractor', 'dev')"
    ))
    .await;

    // 补上剩下的 migration —— 也就是本次这一条
    db.store.migrate().await.expect("补跑 migration 应当成功");

    let row = db
        .store
        .fact(&fact.to_string())
        .await
        .expect("查询不应失败")
        .expect("存量事实应当还在");

    assert_eq!(
        row.source_channel,
        SourceChannel::UnknownLegacy,
        "存量行的来源无法事后重建，只能标「不知道」。\
         若这里变成了某个真实档位，说明 migration 在伪造来源 —— \
         而这个值此后改不了（改它要 UPDATE，只有 redact/purge 允许）"
    );
    assert_eq!(
        row.trust_tier, None,
        "「不知道」不是信任标尺上的一个点。给它一个哨兵档位（0 或 5）会让\
         任何 `WHERE trust_tier <= n` 悄悄把它算进去或排除掉，取决于哨兵\
         取在哪一头；NULL 在 SQL 里的传染性恰好让忘记处理的查询得到空结果"
    );

    db.cleanup().await;
}

/// 加列之后，忘记表态的 INSERT 必须**当场报错**。
///
/// migration 在 `ADD COLUMN ... DEFAULT` 之后立刻 `DROP DEFAULT`，为的就是
/// 这个：默认值留着的话，今后每一条忘写来源的事实都会静默变成一条新的
/// 「不知道」—— 存量行那笔一次性的历史债会变成持续扩大的债。
#[tokio::test]
async fn new_facts_cannot_silently_become_unknown() {
    let Some(db) = common::setup().await else {
        return;
    };

    let episode = Id::new();
    let entity = Id::new();
    db.exec_raw(format!(
        "INSERT INTO episodes (id, session_id, role, content, device_id, occurred_at)
         VALUES ('{episode}', 's', 'user', '{{}}'::jsonb, 'dev', now());
         INSERT INTO entities (id, kind, name, embedding_model, device_id)
         VALUES ('{entity}', 'person', '老张', 'bge-m3', 'dev')"
    ))
    .await;

    let omitted = raw_insert_fact(&db, &entity.to_string(), &episode.to_string(), None).await;
    assert!(
        omitted.is_err(),
        "省掉 source_channel 的 INSERT 必须失败（NOT NULL 且已 DROP DEFAULT）。\
         它若能成功，就说明默认值还在，今后每条忘写来源的事实都会静默\
         变成「不知道」"
    );

    // 通道与档位对不上也必须被挡住：两处描述同一件事，允许它们不一致
    // 等于允许出现「通道说是网页、档位说是用户亲述」的行，而这种行只在
    // 真出了安全事件、要按来源批量失效时才会暴露
    let mismatched = raw_insert_fact(
        &db,
        &entity.to_string(),
        &episode.to_string(),
        Some(("external", "1")),
    )
    .await;
    assert!(
        mismatched.is_err(),
        "external 通道配 tier 1 必须被 facts_trust_tier_matches_channel 拒绝"
    );

    // 反过来：存量档位不许带 tier，否则「不知道」就混进标尺里了
    let legacy_with_tier = raw_insert_fact(
        &db,
        &entity.to_string(),
        &episode.to_string(),
        Some(("unknown_legacy", "2")),
    )
    .await;
    assert!(
        legacy_with_tier.is_err(),
        "unknown_legacy 必须配 NULL —— 给它一个档位就是把「不知道」当成了「知道」"
    );

    db.cleanup().await;
}

/// 正常写入路径落下来的通道与档位要对得上，而且要能同步出去。
#[tokio::test]
async fn written_facts_carry_channel_and_tier_all_the_way_to_sync() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "我们改用 Postgres");
    let entity = common::new_entity("person", "老张");
    let mut fact = common::new_fact(entity.id, "uses", "Postgres", episode.id);
    fact.source = FactSource::External;

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&fact).await
        })
        .await
        .expect("写事务不应失败");

    let row = store
        .fact(&fact.id.to_string())
        .await
        .expect("查询不应失败")
        .expect("刚写的事实应当查得到");
    assert_eq!(row.source_channel, SourceChannel::External);
    assert_eq!(
        row.trust_tier,
        Some(FactSource::External.trust_tier()),
        "档位由通道算出，调用方不该也不能单独指定"
    );

    // 下发路径也要带上。客户端拿不到来源就没法在本地按来源批量处置，
    // 而「按来源批量失效」正是这两列存在的第三个理由
    let synced = store.fetch_since(0, 100).await.expect("同步拉取不应失败");
    let payload = synced
        .iter()
        .find(|r| r.table_name == table::FACTS)
        .and_then(|r| r.payload.clone())
        .expect("facts 行应当出现在同步流里");
    match payload {
        SyncPayload::Fact(f) => {
            assert_eq!(f.source_channel, SourceChannel::External);
            assert_eq!(f.trust_tier, Some(4));
        }
        other => panic!("facts 的日志行应当带回 Fact 载荷，实际是 {other:?}"),
    }

    db.cleanup().await;
}

// ══════════════════════════════════════════════════════════
//  二、派生血缘
// ══════════════════════════════════════════════════════════

/// 没有源的摘要**写不进去**，且不留半条记录。
///
/// 这不是普通的参数校验：摘要没有 `source_episode_id NOT NULL` 那样的兜底列，
/// 血缘全在 `derivations` 里。允许它先写进去、回头再补边，等于允许出现一条
/// 永远找不到源头的摘要 —— 源被 redact 之后它会继续泄露内容，而 GDPR
/// Art.17 的擦除义务及于派生数据。
#[tokio::test]
async fn a_summary_without_provenance_is_refused() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let orphan = new_summary(Vec::new());
    let err = store
        .write_txn(async |tx| tx.insert_summary(&orphan).await)
        .await
        .expect_err("没有源的摘要必须被拒绝");
    assert!(
        matches!(err, StoreError::MissingProvenance { .. }),
        "应当是 MissingProvenance 而不是别的错误，实际：{err}"
    );

    assert_eq!(
        store
            .summaries_for_scope(SummaryScope::Session, "sess-a", 10)
            .await
            .expect("查询不应失败")
            .len(),
        0,
        "被拒绝的摘要不该留下半条记录"
    );

    db.cleanup().await;
}

/// 摘要与它的血缘边同事务落库，两者都进同步流。
#[tokio::test]
async fn a_summary_writes_its_lineage_in_the_same_transaction() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let a = common::new_episode("sess-a", "第一轮");
    let b = common::new_episode("sess-a", "第二轮");
    store
        .write_txn(async |tx| {
            tx.insert_episode(&a).await?;
            tx.insert_episode(&b).await
        })
        .await
        .expect("写事务不应失败");

    let summary = new_summary(vec![
        ProvenanceRef::episode(a.id),
        ProvenanceRef::episode(b.id),
    ]);
    store
        .write_txn(async |tx| tx.insert_summary(&summary).await)
        .await
        .expect("写事务不应失败");

    let records = store.fetch_since(0, 100).await.expect("同步拉取不应失败");
    let edges: Vec<_> = records
        .iter()
        .filter_map(|r| match r.payload.as_ref() {
            Some(SyncPayload::Derivation(d)) => Some(d.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        edges.len(),
        2,
        "两个源就该有两条血缘边 —— 少一条就少一条 redact 追得到的路"
    );
    for edge in &edges {
        assert_eq!(edge.derived_id, summary.id.to_string());
    }
    let mut sources: Vec<&str> = edges.iter().map(|e| e.source_id.as_str()).collect();
    sources.sort_unstable();
    let mut want = [a.id.to_string(), b.id.to_string()];
    want.sort_unstable();
    assert_eq!(sources, want.iter().map(String::as_str).collect::<Vec<_>>());

    // 血缘边必须排在摘要行**之后**：客户端按 sync_log 序回放，
    // 边先到就会指向一条还不存在的摘要
    let summary_seq = seq_of(&records, table::SUMMARIES);
    let first_edge_seq = seq_of(&records, table::DERIVATIONS);
    assert!(
        summary_seq < first_edge_seq,
        "摘要行（seq {summary_seq}）应当先于它的血缘边（seq {first_edge_seq}）"
    );

    db.cleanup().await;
}

/// 多源派生的事实（crosslink 的跨域边）两端都要记下来。
///
/// 在此之前这条边只记左侧那条事实的 `source_episode_id`：只 redact 右侧时，
/// 这行边会带着右侧实体名活下来，且没有任何查询能找到它。
#[tokio::test]
async fn a_multi_source_fact_records_every_source() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "两个领域撞上了");
    let entity = common::new_entity("person", "老张");
    let left = common::new_fact(entity.id, "uses", "Postgres", episode.id);
    let right = common::new_fact(entity.id, "owns", "cortex", episode.id);

    let mut edge = common::new_fact(entity.id, "relates_to", "跨域", episode.id);
    edge.source = FactSource::Derived;
    edge.derived_from = vec![ProvenanceRef::fact(left.id), ProvenanceRef::fact(right.id)];

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&left).await?;
            tx.insert_fact(&right).await?;
            tx.insert_fact(&edge).await
        })
        .await
        .expect("写事务不应失败");

    let records = store.fetch_since(0, 200).await.expect("同步拉取不应失败");
    let edges: Vec<_> = records
        .iter()
        .filter_map(|r| match r.payload.as_ref() {
            Some(SyncPayload::Derivation(d)) => Some(d.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        edges.len(),
        2,
        "单源事实（left / right）不该写血缘边 —— 它们的出处就是 source_episode_id；\
         多源的那条边该写两条。实际写了 {} 条",
        edges.len()
    );
    for e in &edges {
        assert_eq!(e.derived_id, edge.id.to_string(), "只有跨域边是多源派生物");
    }

    db.cleanup().await;
}

// ══════════════════════════════════════════════════════════
//  小工具
// ══════════════════════════════════════════════════════════

fn new_summary(sources: Vec<ProvenanceRef>) -> NewSummary {
    NewSummary {
        id: Id::new(),
        scope: SummaryScope::Session,
        scope_key: "sess-a".to_owned(),
        text: "这场会话讨论了存储层".to_owned(),
        tsv_source: Some("这场 会话 讨论 存储层".to_owned()),
        embedding: None,
        embedding_model: common::MODEL.to_owned(),
        covers_from: None,
        covers_to: None,
        sources,
        device_id: common::DEVICE.to_owned(),
    }
}

fn seq_of(records: &[cortex_store::SyncRecord], table_name: &str) -> i64 {
    records
        .iter()
        .find(|r| r.table_name == table_name)
        .unwrap_or_else(|| panic!("同步流里应当有 {table_name} 的日志行"))
        .seq
}

/// 绕过 [`cortex_store::WriteTxn`] 直插一条 fact，用于构造正常路径写不出的坏行。
///
/// 返回 `Err` 表示数据库拒绝了它 —— 这正是这几条断言想看到的。
async fn raw_insert_fact(
    db: &common::TestDb,
    entity: &str,
    episode: &str,
    channel_and_tier: Option<(&str, &str)>,
) -> Result<(), String> {
    let id = Id::new();
    let (cols, vals) = match channel_and_tier {
        None => (String::new(), String::new()),
        Some((channel, tier)) => (
            ", source_channel, trust_tier".to_owned(),
            format!(", '{channel}', {tier}"),
        ),
    };
    let sql = format!(
        "INSERT INTO facts
             (id, subject_id, predicate, object_text, statement, embedding_model,
              source_episode_id, extracted_by, device_id{cols})
         VALUES ('{id}', '{entity}', 'uses', 'x', 'x', 'bge-m3',
                 '{episode}', 'raw', 'dev'{vals})"
    );
    db.try_exec_raw(sql).await
}

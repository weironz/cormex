//! 回放保真：存进去的东西，回放时还在不在。
//!
//! 三组断言，对应三个「不会报错的静默损坏」：
//!
//! 1. 长会话分页 —— 之前 `ORDER BY occurred_at ASC LIMIT 500` 丢的是**最新**
//!    那些消息，用户看到一段没有结尾的对话；
//! 2. 注入归因 —— 之前只活在 SSE 流里，刷新一次抽屉就空了；
//! 3. 转录回扫候选 —— 之前预签名直传上来的大文件永远不会被转录。
//!
//! 全部只能真连库测：正确性都在 SQL 里。

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
//  R1 —— 长会话丢的是最新的话
// ══════════════════════════════════════════════════════════

/// 超过一页的会话，默认那一页里必须有**最后一句**。
///
/// 这是本次改动最要紧的一条：旧实现是升序 + LIMIT，超长会话截掉的是结尾，
/// 而响应里没有任何字段说明发生过截断 —— 用户只会觉得「我的历史怪怪的」。
#[tokio::test]
async fn the_default_page_contains_the_newest_message_not_the_oldest() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // 600 条 > 旧实现那个 500 的上限
    const TOTAL: i64 = 600;
    for i in 0..TOTAL {
        // seconds_ago 递减 = 时间递增，最后写的那条最新
        let ep = episode_at("long", &format!("第 {i} 句"), TOTAL - i);
        s.write_txn(async |t| t.insert_episode(&ep).await)
            .await
            .expect("写 episode 不该失败");
    }

    let page = s
        .episodes_by_session_page("long", 200, None)
        .await
        .expect("分页查询不该失败");
    // 旧实现的形态：升序 + LIMIT
    let old = s
        .episodes_by_session("long", 200)
        .await
        .expect("升序查询不该失败");

    db.cleanup().await;

    assert_eq!(page.len(), 200, "应当恰好取回一页");
    assert_eq!(
        page[0].text.as_deref(),
        Some(format!("第 {} 句", TOTAL - 1)).as_deref(),
        "分页是降序，第一条必须是**最新**那句；拿到别的说明取的仍是开头那一段"
    );
    assert_eq!(
        old.last().and_then(|e| e.text.clone()),
        Some("第 199 句".to_owned()),
        "旧的升序实现在这里停在第 199 句 —— 这正是「丢掉最新的话」的形状，\
         留着它是为了让这条改动的收益一眼可见"
    );
}

/// 游标必须落在 `(occurred_at, id)` 两列上。
///
/// 全部消息压在**同一个** `occurred_at` 上：只用时间做游标的实现在这里
/// 要么翻不动（第二页与第一页完全相同），要么直接跳过一整批。
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
            .expect("写 episode 不该失败");
    }

    // 每页 7 条，翻到底，把 id 全收起来
    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<EpisodeCursor> = None;
    for _ in 0..10 {
        let page = s
            .episodes_by_session_page("tie", 7, cursor.as_ref())
            .await
            .expect("分页查询不该失败");
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
        "翻完全部页应当恰好看到 {TOTAL} 条，实际 {} 条 —— \
         少了是漏行，多了是重复，两者都说明游标没落在 (occurred_at, id) 上",
        seen.len()
    );
    assert_eq!(
        unique.len(),
        TOTAL,
        "同一条消息被翻出来两次 —— 游标只用了 occurred_at"
    );
}

/// 翻页拼起来的顺序必须与一次性升序取回完全一致。
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
            .expect("写 episode 不该失败");
    }

    let mut stitched: Vec<String> = Vec::new();
    let mut cursor: Option<EpisodeCursor> = None;
    loop {
        let page = s
            .episodes_by_session_page("stitch", 8, cursor.as_ref())
            .await
            .expect("分页查询不该失败");
        if page.is_empty() {
            break;
        }
        cursor = page.last().map(|e| EpisodeCursor {
            occurred_at: e.occurred_at,
            id: e.id.clone(),
        });
        stitched.extend(page.into_iter().map(|e| e.id));
    }
    stitched.reverse(); // 降序翻页 → 正序渲染

    let straight: Vec<String> = s
        .episodes_by_session("stitch", 1000)
        .await
        .expect("升序查询不该失败")
        .into_iter()
        .map(|e| e.id)
        .collect();

    db.cleanup().await;

    assert_eq!(
        stitched, straight,
        "把各页反转拼起来必须与一次性升序取回逐条相同，否则客户端渲染出的\
         对话顺序会与真实顺序不一致"
    );
}

// ══════════════════════════════════════════════════════════
//  R2 —— 回放的历史轮次要有记忆抽屉
// ══════════════════════════════════════════════════════════

/// 注入归因写进去、查回来，且**失效之后照样能查回来**并带标记。
///
/// 存 fact id 而不是快照的代价就在这里：回放看到的是事实的现状。
/// 那正是要的 —— 「当时依据的这条现在已经不成立了」是审计最想看到的信息，
/// 把它藏起来等于篡改回放。
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
    .expect("造数据不该失败");

    // 两条记忆按融合分顺序注入
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
            .expect("写注入归因不该失败");
    }

    // 注入之后，第二条被新事实取代
    let ev = NewFactEvent::superseded(stale, kept, Utc::now(), common::DEVICE);
    s.write_txn(async |t| t.insert_fact_event(&ev).await)
        .await
        .expect("写失效事件不该失败");

    let drawer = s
        .injected_memories(&turn_id.to_string())
        .await
        .expect("查抽屉不该失败");

    db.cleanup().await;

    assert_eq!(
        drawer.len(),
        2,
        "两条注入记忆都该回放出来，失效的那条**不能**被过滤掉：{drawer:?}"
    );
    assert_eq!(
        drawer.iter().map(|m| m.ordinal).collect::<Vec<_>>(),
        vec![0, 1],
        "必须按注入顺序还原 —— 靠 score 重排是不行的，它可空：{drawer:?}"
    );
    assert_eq!(
        drawer[0].channels.as_deref(),
        Some(["bm25".to_owned(), "vector".to_owned()].as_slice()),
        "命中的召回路要原样带回，bad case 时它是唯一能定位问题的东西：{drawer:?}"
    );
    assert!(
        drawer[0].statement.is_some(),
        "statement 应当从 facts JOIN 出来 —— 表里只存 id 是为了让 redact \
         一处清除处处生效：{drawer:?}"
    );
    assert!(
        !drawer[0].invalidated && drawer[1].invalidated,
        "只有被 superseded 的那条该带失效标记：{drawer:?}"
    );
}

/// 工具调用的 `path` 必须是**独立一列**。
///
/// 之前客户端从渲染后的 summary 里正则抠文件名，参数格式一变就静默显示错文件。
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
        .expect("写 episode 不该失败");

    let calls = [
        (0, "read_file", Some("README.md"), true),
        (1, "write_file", Some("README.md"), true),
        // 不碰文件的工具 —— path 必须能为空
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
            .expect("写工具归因不该失败");
    }

    let replay = s
        .episode_tool_calls(&turn_id.to_string())
        .await
        .expect("查工具归因不该失败");

    db.cleanup().await;

    assert_eq!(
        replay.iter().map(|c| c.ordinal).collect::<Vec<_>>(),
        vec![0, 1, 2, 3],
        "工具调用之间有因果（先 list_dir 再 read_file），必须按调用顺序回放：{replay:?}"
    );
    assert_eq!(
        replay.iter().map(|c| c.path.as_deref()).collect::<Vec<_>>(),
        vec![
            Some("README.md"),
            Some("README.md"),
            None,
            Some("../etc/passwd")
        ],
        "路径必须原样回放，且不碰文件的工具为 NULL：{replay:?}"
    );
    assert!(
        !replay[3].ok,
        "失败的调用要如实标出来 —— 「哪几次失败了」是回放时一眼要看到的：{replay:?}"
    );
}

/// diff 存进去之后回放时还在。
///
/// # 为什么单独测这一列
///
/// 「写进去了但读不出来」在这个仓库栽过一次（`FactDto` 少了两列，
/// 落库正常、界面永远空着，隔了很久才发现）。diff 的形状一模一样：
/// 三条读路各自手写 SELECT 列名，漏掉一列不会有任何报错 ——
/// 只是重开会话后那次改动"就没改过"。
///
/// 顺带把 8192 那条 CHECK 也真踩一次。它是最后一道栅栏（agent 侧已经截断
/// 过一次），而一道从没被踩过的栅栏和一条注释没有区别。
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
        .expect("写 episode 不该失败");

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
            .expect("写工具归因不该失败");
    }

    // 超一个字符就该被数据库挡下来。agent 侧已经截断过一次，走到这里说明
    // 那次截断失效了 —— 这时宁可写入失败，也不要让一份没有上限的文本
    // 顺着 sync_log 广播到每一台设备
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
        .expect("查工具归因不该失败");

    // 第四条读路：`/sync` 下发给**别的设备**的那份。它在 `sync.rs` 里另有一句
    // 手写的 SELECT，与上面三句各自独立 —— 补列时漏掉它不会有任何编译错误，
    // 症状是整批 /sync 500，客户端游标从此卡死（比少显示一段 diff 严重得多）。
    //
    // `sync_coverage` 那条测试挡不住这个：它塞的是**不存在的行 id**，
    // SELECT 永远返回空集，而 sqlx 只在真要解码一行时才发现少了列
    let synced = s.fetch_since(0, 1000).await.expect("拉同步不该失败");

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
        "同步下发的那份也要带上 diff —— 否则这次改动在别的设备上「就没改过」：{synced:?}"
    );

    assert!(
        rejected.is_err(),
        "超过 {} 字符的 diff 必须被 CHECK 挡下 —— 这一列会随 sync_log 下发到所有设备，\
         没有上限就是一条谁都没预算过的流量",
        cortex_store::TOOL_DIFF_MAX_CHARS
    );
    assert_eq!(
        replay.iter().map(|c| c.diff.as_deref()).collect::<Vec<_>>(),
        vec![None, Some(diff)],
        "diff 必须一字不差地回放，且不改文件的工具为 NULL（不是空串 —— \
         空串会让界面画出一个空的 diff 框，看着像「改了但什么都没改」）：{replay:?}"
    );
}

// ══════════════════════════════════════════════════════════
//  R5 —— 预签名直传的媒体要能被回扫补上
// ══════════════════════════════════════════════════════════

/// 候选清单：只要没转过的，且**绝不**包含已抹除的。
///
/// 已抹除的若被选中，回扫器会周期性地把它从对象存储拉回内存、送上网 ——
/// 即使管线那侧的复检最终丢弃结果，秘密也已经出过一次进程。
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
    let pending = blob('a', "image/png"); // 该被选中
    let done = blob('b', "image/jpeg"); // 已经转过
    let erased = blob('c', "image/png"); // 已抹除
    let text = blob('d', "text/plain"); // 类型不在粗筛里

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
    .expect("造数据不该失败");

    let queue = s
        .blobs_missing_transcript(&["image/%".to_owned()], 100)
        .await
        .expect("查回扫候选不该失败");
    let hashes: Vec<String> = queue.into_iter().map(|b| b.hash).collect();

    db.cleanup().await;

    assert_eq!(
        hashes,
        vec![pending.hash.clone()],
        "候选清单应当只剩没转过、没被抹除、类型对得上的那一个：{hashes:?}"
    );
    assert!(
        !hashes.contains(&erased.hash),
        "已抹除的 blob 绝不能进回扫队列 —— 那会让秘密被周期性地拉回内存并送上网"
    );
}

/// 回扫是幂等的：补完转录之后，同一个 blob 不该再出现在队列里。
#[tokio::test]
async fn a_backfilled_blob_leaves_the_queue() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let blob = NewBlob {
        hash: "e".repeat(64),
        mime: "image/png".to_owned(),
        size_bytes: 40 * 1024 * 1024, // 直传那一档
        storage_key: "blobs/ee".to_owned(),
    };
    s.write_txn(async |t| t.insert_blob(&blob).await)
        .await
        .expect("写 blob 不该失败");

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
        .expect("写转录不该失败");

    let after = queue_len(s, &blob.hash).await;

    db.cleanup().await;

    assert_eq!(before, 1, "直传上来、从未转录过的大文件必须进队列");
    assert_eq!(
        after, 0,
        "补上转录之后必须离开队列，否则回扫器会每轮重转同一个对象"
    );
}

async fn queue_len(s: &Store, hash: &str) -> usize {
    s.blobs_missing_transcript(&["image/%".to_owned()], 100)
        .await
        .expect("查回扫候选不该失败")
        .into_iter()
        .filter(|b| b.hash == hash)
        .count()
}

//! 会话生命周期：状态机、归档过滤、schema 约束、同步下发。
//!
//! 全部逻辑都在 SQL 里（`session_state` 视图的三次 DISTINCT ON、
//! `session_digests` 的归档过滤、表上的 CHECK），单测一条都测不到 ——
//! 只能真连库。数据库不可用时 skip 而非 fail。

mod common;

use chrono::{Duration, Utc};
use cortex_core::Id;
use cortex_store::{
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
        .expect("写 episode 不该失败");
}

/// 追加一条生命周期事件，返回它在同步全序里的 seq。
async fn emit(store: &Store, e: NewSessionEvent) -> i64 {
    store
        .write_txn(async |t| t.insert_session_event(&e).await)
        .await
        .expect("写 session_event 不该失败")
}

/// 末态由**最后一条同类事件**决定，不是撤销栈。
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

    let state = s.session_state("sess").await.expect("查末态不该失败");

    db.cleanup().await;

    let state = state.expect("写过事件的会话应当有末态");
    assert_eq!(
        state.title.as_deref(),
        Some("标题 B"),
        "[rename A, rename B] 之后应当是 B —— 末态由最后一条决定，不做撤销栈：{state:?}"
    );
    assert!(
        !state.archived,
        "[archive, unarchive] 之后应当是未归档：{state:?}"
    );
}

/// 三个维度互相独立 —— 归档之后改名，不该把会话改出档。
#[tokio::test]
async fn the_three_dimensions_do_not_interfere() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let dir = tempdir_path();

    emit(s, NewSessionEvent::archive("sess", Actor::User, DEV)).await;
    // 归档之后再改名、再绑工作区。若视图用「整表最后一条事件」判定，
    // 这两条会把归档态冲掉 —— 那正是这个测试要挡的实现错误
    emit(s, NewSessionEvent::rename("sess", "档案", Actor::User, DEV)).await;
    emit(
        s,
        NewSessionEvent::bind_workspace("sess", &dir, Actor::User, DEV),
    )
    .await;

    let state = s.session_state("sess").await.expect("查末态不该失败");

    db.cleanup().await;

    let state = state.expect("写过事件的会话应当有末态");
    assert!(
        state.archived,
        "归档之后改名 / 绑工作区不该让会话自动出档：{state:?}"
    );
    assert_eq!(state.title.as_deref(), Some("档案"));
    assert_eq!(state.workspace.as_deref(), Some(dir.as_str()));
}

/// 解绑之后 `workspace` 必须回到 `None`，而不是留着最后绑过的那个路径。
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
    let bound = s.session_state("sess").await.expect("查末态不该失败");

    emit(
        s,
        NewSessionEvent::unbind_workspace("sess", Actor::User, DEV),
    )
    .await;
    let unbound = s.session_state("sess").await.expect("查末态不该失败");

    db.cleanup().await;

    assert_eq!(
        bound.and_then(|s| s.workspace).as_deref(),
        Some(dir.as_str())
    );
    assert_eq!(
        unbound.expect("会话仍有事件").workspace,
        None,
        "解绑后必须是 None —— 留着旧路径会让文件工具继续出现在纯聊天会话里"
    );
}

/// 没有任何事件的会话查不到末态，这不是错误。
#[tokio::test]
async fn a_session_without_events_has_no_state() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    write_episode(s, episode("plain", "只发过消息，没改过名", 5)).await;

    let state = s.session_state("plain").await.expect("查末态不该失败");

    db.cleanup().await;
    assert!(state.is_none(), "从未有过生命周期事件的会话应当查不到末态");
}

/// 每个 op 只能带自己那一个载荷字段 —— 两侧的 CHECK 都要真的生效。
#[tokio::test]
async fn schema_rejects_mismatched_payloads() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // rename 没给 title
    let mut no_title = NewSessionEvent::rename("sess", "x", Actor::User, DEV);
    no_title.title = None;

    // archive 却带着 title —— 放过它的话，视图里会表现为「归档一下标题就变了」
    let mut archive_with_title = NewSessionEvent::archive("sess", Actor::User, DEV);
    archive_with_title.title = Some("不该有的标题".into());

    // unbind 却带着 workspace
    let mut unbind_with_path = NewSessionEvent::unbind_workspace("sess", Actor::User, DEV);
    unbind_with_path.workspace = Some("/tmp".into());

    // 空标题
    let empty_title = NewSessionEvent::rename("sess", "", Actor::User, DEV);

    let mut outcome = Vec::new();
    for (label, e) in [
        ("rename 缺 title", no_title),
        ("archive 带 title", archive_with_title),
        ("unbind 带 workspace", unbind_with_path),
        ("空标题", empty_title),
    ] {
        let r = s
            .write_txn(async |t| t.insert_session_event(&e).await)
            .await;
        outcome.push((label, r.is_err()));
    }

    db.cleanup().await;

    for (label, rejected) in outcome {
        assert!(rejected, "「{label}」应当被 schema 的 CHECK 拒绝");
    }
}

/// 归档的会话默认不出现在列表里，`include_archived` 能取回来。
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
        .expect("查列表不该失败");
    let all = s
        .session_digests(10, true, None)
        .await
        .expect("查列表不该失败");

    // 取消归档后必须重新出现 —— 归档不是删除
    emit(s, NewSessionEvent::unarchive("gone", Actor::User, DEV)).await;
    let restored = s
        .session_digests(10, false, None)
        .await
        .expect("查列表不该失败");

    db.cleanup().await;

    let ids = |v: &[cortex_store::SessionDigest]| {
        v.iter().map(|d| d.session_id.clone()).collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&default),
        vec!["keep"],
        "归档的会话不该出现在默认列表里"
    );
    assert_eq!(
        ids(&all).len(),
        2,
        "include_archived 应当把归档的也带回来：{:?}",
        ids(&all)
    );
    assert!(
        all.iter().any(|d| d.session_id == "gone" && d.archived),
        "归档标记应当如实返回：{:?}",
        ids(&all)
    );
    assert_eq!(
        ids(&restored).len(),
        2,
        "取消归档后会话必须重新出现 —— 归档不是删除，数据一条没少"
    );
}

/// 归档的会话不能**占用** limit 的配额。
///
/// 「先取 N 条再在内存里丢掉归档的」是最容易写出的实现，缺陷不报错：
/// 归档二十个会话，列表里能看见的就少二十个。过滤必须在 LIMIT 之前。
#[tokio::test]
async fn archived_sessions_do_not_consume_the_limit() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // 三个归档的会话比未归档的都新，会排在前面
    for i in 0..3 {
        let id = format!("arch-{i}");
        write_episode(s, episode(&id, "归档会话", 1 + i)).await;
        emit(s, NewSessionEvent::archive(&id, Actor::User, DEV)).await;
    }
    write_episode(s, episode("visible", "唯一该被看见的", 100)).await;

    // limit=1：若过滤发生在 LIMIT 之后，这一条会被归档会话占掉，结果为空
    let got = s
        .session_digests(1, false, None)
        .await
        .expect("查列表不该失败");

    db.cleanup().await;

    assert_eq!(
        got.iter().map(|d| d.session_id.clone()).collect::<Vec<_>>(),
        vec!["visible"],
        "归档过滤必须发生在 LIMIT 之前，否则归档的会话会把可见配额吃掉"
    );
}

/// 用户改的标题优先于派生标题，且随 digest 一起回来。
#[tokio::test]
async fn a_custom_title_rides_along_with_the_digest() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    write_episode(s, episode("sess", "首条用户消息", 5)).await;
    let before = s.session_digest("sess").await.expect("查概览不该失败");
    emit(
        s,
        NewSessionEvent::rename("sess", "我起的名字", Actor::User, DEV),
    )
    .await;
    let after = s.session_digest("sess").await.expect("查概览不该失败");
    let missing = s.session_digest("nope").await.expect("查概览不该失败");

    db.cleanup().await;

    assert_eq!(
        before.expect("会话有消息").title,
        None,
        "从未改名时 title 应当是 None，由上层回落到派生标题"
    );
    let after = after.expect("会话有消息");
    assert_eq!(after.title.as_deref(), Some("我起的名字"));
    assert_eq!(
        after.first_user_text.as_deref(),
        Some("首条用户消息"),
        "派生标题的原料仍要带回来 —— 客户端要能显示「自动标题是什么」"
    );
    assert!(missing.is_none(), "没有消息的会话应当查不到概览");
}

/// 新表必须进 `sync_log`，且 `/sync` 拉得回它的业务行。
///
/// 漏掉 `load_payloads` 的那一支不是「会话改名同步不了」，而是**整条 /sync
/// 断掉**：一条 session_events 日志会让整批拉取报 UnknownTable，
/// 客户端的游标从此卡死在它前面。
#[tokio::test]
async fn session_events_flow_through_the_sync_log() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let before = s.latest_seq().await.expect("查游标不该失败");
    let e = NewSessionEvent::rename("sess", "同步得到吗", Actor::User, DEV);
    let id = e.id.to_string();
    let seq = emit(s, e).await;
    // 后面再跟一条别的表的写入，确认混排拉取不会翻车
    write_episode(s, episode("sess", "一条普通消息", 1)).await;

    let records = s.fetch_since(before, 100).await.expect("增量拉取不该失败");
    let events = s.session_events("sess").await.expect("查时间线不该失败");

    db.cleanup().await;

    assert!(seq > before, "session_event 必须推进同步游标");
    let hit = records
        .iter()
        .find(|r| r.table_name == table::SESSION_EVENTS)
        .unwrap_or_else(|| panic!("session_events 没有进 sync_log：{records:?}"));
    assert_eq!(hit.record_id, id);
    match hit.payload.as_ref() {
        Some(SyncPayload::SessionEvent(ev)) => {
            assert_eq!(ev.op, SessionOp::Rename);
            assert_eq!(ev.title.as_deref(), Some("同步得到吗"));
            assert_eq!(ev.actor, Actor::User);
        }
        other => panic!("载荷类型不对，/sync 会下发不出去：{other:?}"),
    }
    assert_eq!(events.len(), 1, "时间线应当恰好一条：{events:?}");
}

/// 一次 PATCH 里的多个改动写在同一个事务里 —— 别的设备不该看到中间态。
#[tokio::test]
async fn multiple_changes_land_in_one_transaction() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let dir = tempdir_path();

    let before = s.latest_seq().await.expect("查游标不该失败");
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
    .expect("批量写不该失败");

    let logged = s.sync_log_since(before, 100).await.expect("查日志不该失败");
    let state = s.session_state("sess").await.expect("查末态不该失败");

    db.cleanup().await;

    let seqs: Vec<i64> = logged.iter().map(|l| l.seq).collect();
    assert_eq!(seqs.len(), 3, "三条事件都该有各自的 sync_log 行：{seqs:?}");
    assert_eq!(
        seqs.last().unwrap() - seqs.first().unwrap(),
        2,
        "同事务写入的三行应当连号 —— 中间插进别的写事务说明锁没起作用：{seqs:?}"
    );

    let state = state.expect("应当有末态");
    assert_eq!(state.title.as_deref(), Some("一次改完"));
    assert!(state.archived);
    assert_eq!(state.workspace.as_deref(), Some(dir.as_str()));
}

/// 存储层不校验路径 —— 「什么算系统目录」是部署形态的知识，属于 cortexd。
/// 这里只借临时目录取一个真实存在的绝对路径字符串。
fn tempdir_path() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

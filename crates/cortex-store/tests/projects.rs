//! 项目：两台状态机、删项目的爆炸半径、按项目过滤、schema 约束、同步下发。
//!
//! 与 session_events 那份同源：逻辑全在 SQL 里（`project_state` 的两次
//! DISTINCT ON、`session_state` 里那句「悬挂归属按未分组处理」的 EXISTS、
//! `session_digests` 的项目过滤、表上的 CHECK），单测一条都测不到 ——
//! 只能真连库。数据库不可用时 skip 而非 fail。

mod common;

use chrono::{Duration, Utc};
use cortex_core::Id;
use cortex_store::{
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
        .expect("写 episode 不该失败");
    id
}

async fn emit_project(store: &Store, e: NewProjectEvent) -> i64 {
    store
        .write_txn(async |t| t.insert_project_event(&e).await)
        .await
        .expect("写 project_event 不该失败")
}

async fn emit_session(store: &Store, e: NewSessionEvent) -> i64 {
    store
        .write_txn(async |t| t.insert_session_event(&e).await)
        .await
        .expect("写 session_event 不该失败")
}

/// 名字与存在与否各是一台状态机，各取各的末条。
#[tokio::test]
async fn the_last_event_of_each_machine_wins() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // 改名两次：末态是最后那个，不做撤销栈
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

    // 删掉之后再改名：改名不该把它救回来 —— 两台状态机互不干扰，
    // 「整表最后一条」那种判据会在这里得出「改个名就自动复活了」
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

    // 同 id 再 create 一次 = 撤销误删
    emit_project(s, NewProjectEvent::create("back", "初版", Actor::User, DEV)).await;
    emit_project(s, NewProjectEvent::delete("back", Actor::User, DEV)).await;
    emit_project(s, NewProjectEvent::create("back", "重建", Actor::User, DEV)).await;

    let projects = s.projects().await.expect("查项目列表不该失败");
    let events = s.project_events("renamed").await.expect("查时间线不该失败");

    db.cleanup().await;

    let names: Vec<(String, String)> = projects
        .iter()
        .map(|p| (p.project_id.clone(), p.name.clone()))
        .collect();

    assert!(
        names.contains(&("renamed".into(), "丙".into())),
        "[create 甲, rename 乙, rename 丙] 之后应当叫「丙」—— 末态由最后一条决定：{names:?}"
    );
    assert!(
        !names.iter().any(|(id, _)| id == "gone"),
        "删掉之后再改名不该让项目复活 —— 存在与否是独立的一台状态机：{names:?}"
    );
    assert!(
        names.contains(&("back".into(), "重建".into())),
        "同 id 再 create 一次应当把项目恢复（撤销误删），且用最新的名字：{names:?}"
    );

    assert_eq!(events.len(), 3, "时间线应当有三条：{events:?}");
    assert_eq!(
        events[0].op,
        ProjectOp::Create,
        "时间线必须按时间升序，第一条是 create：{events:?}"
    );
}

/// **删项目只解散分组**：会话变未分组，消息与已抽取的记忆一条不少。
///
/// 这是这个功能最容易被实现错的一条 —— 写成级联删会话的话，
/// 用户点一下「删除项目」就丢掉了几个月的对话，而那不可逆。
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

    // 挂一条派生记忆上去，好断言「删项目不动记忆」
    let entity = common::new_entity("person", "老王");
    let fact = common::new_fact(entity.id, "喜欢", "深色主题", ep);
    let subject = entity.id;
    let fact_id = fact.id.to_string();
    s.write_txn(async |t| {
        t.insert_entity(&entity).await?;
        t.insert_fact(&fact).await
    })
    .await
    .expect("写实体与事实不该失败");

    let grouped = s
        .session_state("sess")
        .await
        .expect("查末态不该失败")
        .expect("写过事件的会话应当有末态");
    let in_project = s
        .session_digests(10, false, Some("proj"))
        .await
        .expect("按项目查列表不该失败");

    emit_project(s, NewProjectEvent::delete("proj", Actor::User, DEV)).await;

    let after_state = s
        .session_state("sess")
        .await
        .expect("查末态不该失败")
        .expect("会话的事件还在，末态就还在");
    let after_projects = s.projects().await.expect("查项目列表不该失败");
    let after_sessions = s
        .session_digests(10, false, None)
        .await
        .expect("查列表不该失败");
    let after_facts = s
        .active_facts_by_subject(&subject.to_string(), 10)
        .await
        .expect("查事实不该失败");
    let after_episodes = s
        .episodes_by_session("sess", 10)
        .await
        .expect("查消息不该失败");
    // 事件本身是 append-only 的，删项目不该把历史抹掉
    let session_events = s.session_events("sess").await.expect("查时间线不该失败");

    db.cleanup().await;

    assert_eq!(
        grouped.project_id.as_deref(),
        Some("proj"),
        "移进项目之后末态应当指向它：{grouped:?}"
    );
    assert_eq!(in_project.len(), 1, "项目里应当有一个会话：{in_project:?}");

    assert!(
        after_projects.is_empty(),
        "删掉之后项目必须从列表消失：{after_projects:?}"
    );
    assert_eq!(
        after_state.project_id, None,
        "项目没了之后会话必须变成未分组 —— 悬挂的归属会让界面画出一个\
         点进去空空如也的分组标签：{after_state:?}"
    );
    assert_eq!(
        after_sessions.len(),
        1,
        "删项目不该让会话从列表里消失：{after_sessions:?}"
    );
    assert_eq!(
        after_sessions[0].message_count, 1,
        "删项目不该动任何一条消息：{after_sessions:?}"
    );
    assert_eq!(after_episodes.len(), 1, "原始消息必须一条不少");
    assert_eq!(
        after_facts.len(),
        1,
        "删项目不该动任何一条已抽取的记忆 —— 销毁内容只能走 redact / purge"
    );
    assert_eq!(after_facts[0].id, fact_id);
    assert_eq!(
        session_events.len(),
        1,
        "会话的 move_to_project 事件必须原样留着（append-only），\
         删项目不级联改写它正是「撤销误删」成立的前提：{session_events:?}"
    );
}

/// 第四个维度与前三个互不干扰。
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
    // 移进项目之后归档、改名。若视图用「整表最后一条」判定，这两条会把归属冲掉
    emit_session(s, NewSessionEvent::archive("sess", Actor::User, DEV)).await;
    emit_session(s, NewSessionEvent::rename("sess", "标题", Actor::User, DEV)).await;

    let grouped = s.session_state("sess").await.expect("查末态不该失败");

    emit_session(
        s,
        NewSessionEvent::remove_from_project("sess", Actor::User, DEV),
    )
    .await;
    let ungrouped = s.session_state("sess").await.expect("查末态不该失败");

    db.cleanup().await;

    let grouped = grouped.expect("写过事件的会话应当有末态");
    assert_eq!(
        grouped.project_id.as_deref(),
        Some("proj"),
        "归档 / 改名之后归属应当原样保留：{grouped:?}"
    );
    assert!(grouped.archived, "归属不该把归档态冲掉：{grouped:?}");
    assert_eq!(grouped.title.as_deref(), Some("标题"));

    assert_eq!(
        ungrouped.expect("会话仍有事件").project_id,
        None,
        "移出项目之后必须是 None，而不是留着最后待过的那个项目"
    );
}

/// 按项目过滤必须发生在 `LIMIT` 之前。
///
/// 「先取 N 条再在内存里筛」是最容易写出的实现，缺陷不报错：别的项目的会话
/// 把配额吃光，用户点开项目看到的是一个空列表 —— 而他明明刚往里拖过会话。
#[tokio::test]
async fn filtering_by_project_happens_before_the_limit() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    emit_project(s, NewProjectEvent::create("proj", "分组", Actor::User, DEV)).await;

    // 三个不属于该项目的会话都比它新，会排在前面
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
        .expect("按项目查列表不该失败");
    let unfiltered = s
        .session_digests(10, false, None)
        .await
        .expect("查列表不该失败");
    let missing_project = s
        .session_digests(10, false, Some("nope"))
        .await
        .expect("查一个不存在的项目不该报错");

    db.cleanup().await;

    assert_eq!(
        filtered
            .iter()
            .map(|d| d.session_id.clone())
            .collect::<Vec<_>>(),
        vec!["mine"],
        "项目过滤必须发生在 LIMIT 之前，否则别的项目的会话会把配额吃掉"
    );
    assert_eq!(
        unfiltered.len(),
        4,
        "不传 project_id 时必须给全部会话（含未分组的）：{unfiltered:?}"
    );
    assert!(
        unfiltered
            .iter()
            .any(|d| d.session_id == "mine" && d.project_id.as_deref() == Some("proj")),
        "归属要随 digest 一起回来，客户端才画得出分组标签：{unfiltered:?}"
    );
    assert!(
        missing_project.is_empty(),
        "查一个不存在的项目应当是空列表而不是全量：{missing_project:?}"
    );
}

/// 每个 op 只能带自己那一个载荷字段，两侧的 CHECK 都要真的生效。
#[tokio::test]
async fn schema_rejects_mismatched_payloads() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let mut create_without_name = NewProjectEvent::create("p", "x", Actor::User, DEV);
    create_without_name.name = None;

    // delete 却带着 name —— 放过它的话，视图里会表现为「删一下名字就变了」
    let mut delete_with_name = NewProjectEvent::delete("p", Actor::User, DEV);
    delete_with_name.name = Some("不该有的名字".into());

    let empty_name = NewProjectEvent::create("p", "", Actor::User, DEV);
    let too_long = NewProjectEvent::create("p", &"x".repeat(101), Actor::User, DEV);

    let mut project_outcome = Vec::new();
    for (label, e) in [
        ("create 缺 name", create_without_name),
        ("delete 带 name", delete_with_name),
        ("空名字", empty_name),
        ("超长名字", too_long),
    ] {
        let r = s
            .write_txn(async |t| t.insert_project_event(&e).await)
            .await;
        project_outcome.push((label, r.is_err()));
    }

    let mut move_without_target = NewSessionEvent::move_to_project("s", "p", Actor::User, DEV);
    move_without_target.project_id = None;

    // 归档却带着 project_id —— 视图里会表现为「归档一下就换了个项目」
    let mut archive_with_project = NewSessionEvent::archive("s", Actor::User, DEV);
    archive_with_project.project_id = Some("p".into());

    let mut session_outcome = Vec::new();
    for (label, e) in [
        ("move_to_project 缺 project_id", move_without_target),
        ("archive 带 project_id", archive_with_project),
    ] {
        let r = s
            .write_txn(async |t| t.insert_session_event(&e).await)
            .await;
        session_outcome.push((label, r.is_err()));
    }

    db.cleanup().await;

    for (label, rejected) in project_outcome.into_iter().chain(session_outcome) {
        assert!(rejected, "「{label}」应当被 schema 的 CHECK 拒绝");
    }
}

/// `session_count` 的口径必须与会话列表看到的一致。
#[tokio::test]
async fn session_count_matches_what_the_session_list_shows() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    emit_project(s, NewProjectEvent::create("proj", "分组", Actor::User, DEV)).await;

    // 会算进去的那一个
    write_episode(s, episode("visible", "看得见的会话", 10)).await;
    // 归档的：从默认列表消失，也就不该出现在计数里
    write_episode(s, episode("archived", "归档的会话", 9)).await;
    emit_session(s, NewSessionEvent::archive("archived", Actor::User, DEV)).await;
    // 一条消息都没有的：会话列表的起点是 episodes 聚合，它本来就不在列表里
    for id in ["visible", "archived", "silent"] {
        emit_session(
            s,
            NewSessionEvent::move_to_project(id, "proj", Actor::User, DEV),
        )
        .await;
    }

    let projects = s.projects().await.expect("查项目列表不该失败");
    let listed = s
        .session_digests(10, false, Some("proj"))
        .await
        .expect("按项目查列表不该失败");
    let single = s.project("proj").await.expect("查单个项目不该失败");
    let missing = s.project("nope").await.expect("查不存在的项目不该报错");

    db.cleanup().await;

    assert_eq!(projects.len(), 1, "应当恰好一个项目：{projects:?}");
    assert_eq!(
        projects[0].session_count,
        listed.len() as i64,
        "计数与 GET /sessions?project_id= 看到的条数必须一致，否则用户会看到\
         「3 个会话」点进去只有 1 个：计数 {}，列表 {:?}",
        projects[0].session_count,
        listed.iter().map(|d| &d.session_id).collect::<Vec<_>>()
    );
    assert_eq!(
        projects[0].session_count, 1,
        "归档的与一条消息都没有的都不该算进计数：{projects:?}"
    );
    assert_eq!(
        single.map(|p| p.name),
        Some("分组".into()),
        "按 id 查单个项目应当拿得到"
    );
    assert!(missing.is_none(), "不存在的项目应当是 None 而不是报错");
}

/// 新表必须进 `sync_log`，且 `/sync` 拉得回它的业务行。
///
/// 漏掉 `load_payloads` 的那一支不是「项目同步不了」，而是**整条 /sync 断掉**：
/// 一条 project_events 日志会让整批拉取报 UnknownTable，客户端的游标从此
/// 卡死在它前面。会话那一侧的 `project_id` 同理 —— 不下发的话，别的设备上
/// 所有会话都是未分组的，而它看起来只像「分组功能没同步过来」。
#[tokio::test]
async fn project_events_and_the_session_link_flow_through_the_sync_log() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let before = s.latest_seq().await.expect("查游标不该失败");

    let created = NewProjectEvent::create("proj", "同步得到吗", Actor::User, DEV);
    let project_event_id = created.id.to_string();
    let seq = emit_project(s, created).await;

    let moved = NewSessionEvent::move_to_project("sess", "proj", Actor::User, DEV);
    let session_event_id = moved.id.to_string();
    emit_session(s, moved).await;

    let records = s.fetch_since(before, 100).await.expect("增量拉取不该失败");

    db.cleanup().await;

    assert!(seq > before, "project_event 必须推进同步游标");

    let project_hit = records
        .iter()
        .find(|r| r.table_name == table::PROJECT_EVENTS)
        .unwrap_or_else(|| panic!("project_events 没有进 sync_log：{records:?}"));
    assert_eq!(project_hit.record_id, project_event_id);
    match project_hit.payload.as_ref() {
        Some(SyncPayload::ProjectEvent(e)) => {
            assert_eq!(e.op, ProjectOp::Create);
            assert_eq!(e.name.as_deref(), Some("同步得到吗"));
            assert_eq!(e.project_id, "proj");
        }
        other => panic!("载荷类型不对，/sync 会下发不出去：{other:?}"),
    }

    let session_hit = records
        .iter()
        .find(|r| r.record_id == session_event_id)
        .unwrap_or_else(|| panic!("session_events 没有进 sync_log：{records:?}"));
    match session_hit.payload.as_ref() {
        Some(SyncPayload::SessionEvent(e)) => assert_eq!(
            e.project_id.as_deref(),
            Some("proj"),
            "会话事件的 project_id 必须随载荷下发，否则别的设备上所有会话都是未分组的"
        ),
        other => panic!("载荷类型不对：{other:?}"),
    }
}

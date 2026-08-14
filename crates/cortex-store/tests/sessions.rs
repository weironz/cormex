//! 会话概览与附件批量读取。
//!
//! 这两条查询都是「从 episodes 归纳」，没有对应的实体表，所以它们的正确性
//! 全在 SQL 里 —— 单测测不到，只能真连库。

mod common;

use chrono::{Duration, Utc};
use cortex_core::Id;
use cortex_store::{NewBlob, NewEpisode, NewEpisodeBlob, Role, Store};

/// 造一条指定角色与时间的消息。
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
        .expect("写 episode 不该失败");
    id
}

#[tokio::test]
async fn digests_summarise_each_session() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // 老会话：两条消息，第一条是助手（标题不该取它）
    write(s, episode("sess-old", Role::Assistant, "开场白", 300)).await;
    write(
        s,
        episode("sess-old", Role::User, "老会话的第一句用户消息", 299),
    )
    .await;
    write(s, episode("sess-old", Role::User, "老会话的最后一句", 290)).await;

    // 新会话：只有一条
    write(s, episode("sess-new", Role::User, "新会话唯一的一句", 5)).await;

    let digests = s
        .session_digests(10, false, None)
        .await
        .expect("查会话概览不该失败");
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

    assert_eq!(outcome.len(), 2, "应当恰好归纳出两个会话：{outcome:?}");
    assert_eq!(
        outcome[0].0, "sess-new",
        "列表必须按最后活动时间倒序，最新的会话排第一：{outcome:?}"
    );
    assert_eq!(outcome[1].1, 3, "老会话应当有 3 条消息：{outcome:?}");
    assert_eq!(
        outcome[1].2.as_deref(),
        Some("老会话的第一句用户消息"),
        "标题应取首条**用户**消息，不能取助手的开场白：{outcome:?}"
    );
    assert_eq!(
        outcome[1].3.as_deref(),
        Some("老会话的最后一句"),
        "预览应取最后一条有正文的消息：{outcome:?}"
    );
}

/// limit 必须作用在**会话**上，而不是「先取最近 N 条消息再归纳」。
///
/// 后者是曾经的实现，缺陷不会报错：一个话痨会话就能把消息窗口占满，
/// 于是昨天的会话从列表里消失 —— 用户看到的是「我的历史丢了」。
#[tokio::test]
async fn a_chatty_session_cannot_crowd_out_a_quiet_one() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    // 安静的老会话：一条消息，且是最早的
    write(
        s,
        episode("sess-quiet", Role::User, "很久以前说过的一句话", 600),
    )
    .await;

    // 话痨会话：40 条，全都比上面那条新
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
        .expect("查会话概览不该失败");
    let ids: Vec<String> = digests.iter().map(|d| d.session_id.clone()).collect();
    let chatty_count = digests
        .iter()
        .find(|d| d.session_id == "sess-chatty")
        .map(|d| d.message_count);

    db.cleanup().await;

    assert!(
        ids.contains(&"sess-quiet".to_string()),
        "安静的老会话被话痨会话挤出了列表 —— 归纳没有下沉到 SQL：{ids:?}"
    );
    assert_eq!(chatty_count, Some(40), "话痨会话的消息数不对：{ids:?}");
}

/// 会话详情要给每条消息挂附件，逐条查就是 N+1。
#[tokio::test]
async fn attachments_come_back_in_one_round_trip() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let with_image = write(s, episode("sess-media", Role::User, "看这张图", 10)).await;
    let plain = write(s, episode("sess-media", Role::Assistant, "收到", 9)).await;

    // blob 行必须先于关联行 —— episode_blobs 对两侧都有外键
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
            .expect("写 blob 不该失败");
    }
    for (hash, kind, filename) in [
        (&hash_a, "image", Some("设计稿.png")),
        // 文件名可空：老数据没有，回填不出来
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
            .expect("写 episode_blobs 不该失败");
    }

    let ids = vec![with_image.to_string(), plain.to_string()];
    let links = s
        .episode_attachments_bulk(&ids)
        .await
        .expect("批量查附件不该失败");
    let empty = s
        .episode_attachments_bulk(&[])
        .await
        .expect("空入参不该发查询，更不该报错");

    db.cleanup().await;

    assert_eq!(
        links.len(),
        2,
        "两条关联都该回来，且只用一次往返：{links:?}"
    );
    assert!(
        links.iter().all(|l| l.episode_id == with_image.to_string()),
        "另一条消息没有附件，不该混进来：{links:?}"
    );
    assert_eq!(
        links.iter().filter_map(|l| l.kind.as_deref()).count(),
        2,
        "kind 应当原样返回，它是客户端区分图片 / 文档的唯一依据：{links:?}"
    );
    // 这条是 R3 的要害：没有 mime / size 的话，重开会话时一个 PDF
    // 只能显示成「文档 · a1b2c3d4」
    assert!(
        links
            .iter()
            .all(|l| l.mime == "image/png" && l.size_bytes == 1),
        "mime 与 size 必须从 blobs JOIN 出来随行返回：{links:?}"
    );
    let named: Vec<_> = links.iter().filter_map(|l| l.filename.as_deref()).collect();
    assert_eq!(
        named,
        vec!["设计稿.png"],
        "文件名属于这一次引用，有就该原样带回、没有就是 None：{links:?}"
    );
    assert!(empty.is_empty(), "空入参应当直接返回空");
}

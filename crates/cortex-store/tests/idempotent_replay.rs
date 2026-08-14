//! 离线队列重放的幂等性 —— 以及为什么它**不能**用 `ON CONFLICT DO NOTHING`。
//!
//! 本地 agent 断网时把 episode 排进队列，联网后按序重放。重复投递是**预期内的
//! 正常路径**：「服务端收下了」与「本地记下已刷出」之间崩一次，那一条就会
//! 再来一遍。所以写入必须幂等。
//!
//! 最省事的写法是给 `INSERT` 加 `ON CONFLICT DO NOTHING`。这一组测试的存在
//! 就是为了钉住「那样做会坏什么」：`Guarded::insert_row` **无条件**往
//! `sync_log` 追一行，于是一次什么都没插的空操作会留下一条指向早已同步的
//! episode 的幽灵行 —— 而 `sync_log` 是同步的唯一事实序。
//!
//! 幽灵行不会让任何东西报错。别的设备只是又拉了一遍它已经有的那条 episode，
//! 结果完全正确。它的代价是长期的：队列每重放一次，全部设备就多拉一轮，
//! 而「游标推进了多少」从此不再等于「真的写了多少」。

mod common;

use cortex_store::table;

/// 数一数 sync_log 现在有几行。
async fn sync_rows(db: &common::TestDb) -> i64 {
    db.query_raw("SELECT count(*)::text FROM sync_log")
        .await
        .first()
        .and_then(|s| s.parse().ok())
        .expect("count(*) 必须有一行")
}

/// 重放同一条 episode：先读一次判重，命中就直接跳过。
///
/// 这是 `Live::write_episode` 走的那条路。断言是**两条**：
/// 写第二次没有新增业务行（本来就该如此），且 `sync_log` **一行都没长** ——
/// 后者才是这条测试真正守着的东西。
#[tokio::test]
async fn a_replayed_episode_adds_no_row_and_no_sync_entry() {
    let Some(db) = common::setup().await else {
        return;
    };

    let ep = common::new_episode("s-replay", "断网期间说的这句话");
    let id = ep.id.to_string();

    db.store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("第一次写入应当成功");

    let after_first = sync_rows(&db).await;
    assert_eq!(after_first, 1, "一条 episode 应当只留一行 sync_log");

    // 重放：判重命中 → 整段写入被跳过
    let seen = db
        .store
        .episode(&id)
        .await
        .expect("读 episode 不该失败")
        .is_some();
    assert!(seen, "刚写进去的 episode 必须读得到，否则判重根本不成立");

    assert_eq!(
        sync_rows(&db).await,
        after_first,
        "判重命中时 sync_log 不该增长 —— 长了就说明重放路径真的写了东西"
    );

    db.cleanup().await;
}

/// 判重与写入之间挤进另一个重放时，撞的是唯一约束，而且事务整段回滚。
///
/// 这条守的是「真并发下的兜底」：判重是事务外的读，两个重放同时穿过它是
/// 可能的。此时后一个必须拿到一个**能被认出来**的错误
///（`is_duplicate_key`），而不是一句泛泛的数据库失败 ——
/// 认不出来就会报 500，客户端把一次成功的重放当成失败，然后永远重试下去。
#[tokio::test]
async fn a_concurrent_replay_hits_the_unique_constraint_and_rolls_back() {
    let Some(db) = common::setup().await else {
        return;
    };

    let ep = common::new_episode("s-race", "同一条被投了两次");
    db.store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("第一次写入应当成功");
    let before = sync_rows(&db).await;

    // 跳过判重，直接再写一次 —— 模拟两个重放同时穿过事务外那道读
    let err = db
        .store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect_err("同一个 id 写第二次必须失败");

    assert!(
        err.is_duplicate_key(),
        "重复主键必须能被 is_duplicate_key 认出来，否则调用方只能当成 500。实际：{err}"
    );
    assert_eq!(
        sync_rows(&db).await,
        before,
        "失败的事务必须整段回滚 —— 业务行没进去而 sync_log 长了一行，\
         就是一条指向不存在记录的游标条目"
    );

    db.cleanup().await;
}

/// **反面教材**：`ON CONFLICT DO NOTHING` 会留下幽灵 sync_log 行。
///
/// 这一条不走 `WriteTxn`（`insert_row` 是 `pub(super)`，测试够不着），
/// 而是手工执行它内部那两条语句 —— 顺序、语义都一样。**它测的不是生产
/// 代码，而是那条被否决的写法**：把「为什么不能这么写」钉成一条会跑的断言，
/// 而不是留一句注释等着被人绕过。
///
/// 真有人把 `ON CONFLICT DO NOTHING` 加进 `insert_episode`，上面第二条测试
/// 会先变红（错误不再是重复键）。这一条负责解释那时候发生了什么。
#[tokio::test]
async fn on_conflict_do_nothing_would_leave_a_phantom_sync_row() {
    let Some(db) = common::setup().await else {
        return;
    };

    let ep = common::new_episode("s-phantom", "第一次写进去的那条");
    let id = ep.id.to_string();
    db.store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("第一次写入应当成功");
    let before = sync_rows(&db).await;

    // Guarded::insert_row 的两条语句，只是把 INSERT 换成宽容版本
    db.exec_raw(format!(
        "INSERT INTO episodes (id, session_id, role, content, text, tsv, device_id, occurred_at)
         VALUES ('{id}', 's-phantom', 'user', '{{}}'::jsonb, 'x',
                 to_tsvector('simple', 'x'), 'test', now())
         ON CONFLICT DO NOTHING"
    ))
    .await;
    db.exec_raw(format!(
        "INSERT INTO sync_log (table_name, record_id) VALUES ('{}', '{id}')",
        table::EPISODES
    ))
    .await;

    let episodes: i64 = db
        .query_raw(format!(
            "SELECT count(*)::text FROM episodes WHERE id = '{id}'"
        ))
        .await
        .first()
        .and_then(|s| s.parse().ok())
        .expect("count(*) 必须有一行");

    assert_eq!(episodes, 1, "ON CONFLICT DO NOTHING 确实一行都没插");
    assert_eq!(
        sync_rows(&db).await,
        before + 1,
        "而 sync_log 却长了一行 —— 这就是幽灵行。它不会让任何东西报错，\
         只是让「游标推进了多少」不再等于「真的写了多少」"
    );

    db.cleanup().await;
}

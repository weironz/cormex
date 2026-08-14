//! 每一张会进 `sync_log` 的表，`fetch_since` 都必须认得。
//!
//! # 这道门挡的是什么
//!
//! `load_payloads` 用 `match table_name` 分派，末尾一支 `other =>` 返回
//! `UnknownTable`。加一张新表时，人很容易只写完「写入侧」就收工 —— 因为
//! 那样测起来是全绿的：新表能写、能查、单测都过。
//!
//! 出事要等到**第一台设备来拉同步**：`fetch_since` 撞上不认识的表名后
//! 整批失败，不是「这张表同步不了」，是那个游标之后的**所有**变更都拉不动，
//! 客户端从此卡死。而这可能是上线几周之后的事，那时新表已经写进去很多行了。
//!
//! 所以这条测试拿 [`table::ALL`]（由宏与常量一起生成，加常量必然进列表）
//! 逐个撞一遍。忘记加 match 分支的人会**当场红**。
//!
//! # 它挡不住的：**给已有的表加一列**
//!
//! 下面塞的是不存在的行 id —— SELECT 每次都返回空集，而 sqlx 只在真要解码
//! 一行时才发现少了列。于是「给 `episode_tool_calls` 加了 `diff`，
//! 补了 `query.rs` 的三句 SELECT，漏了 `sync.rs` 的第四句」这类改动，
//! 在这里是全绿的，出事仍然是整批 /sync 500、游标卡死（真发生过一次）。
//!
//! 加列时要钉的是**带真行**的往返：见 `replay.rs` 里
//! `tool_call_diff_survives_replay_and_the_length_guard_bites`。

mod common;

use cortex_store::table;

#[tokio::test]
async fn every_synced_table_has_a_payload_loader() {
    let Some(db) = common::setup().await else {
        return;
    };

    // 每张表塞一条指向「不存在的行」的日志。payload 加载器会照常执行 SELECT、
    // 查不到、返回 None —— 这完全正常（记录被 redact 后就是这样）。
    // 唯一会失败的情况就是这张表根本没有分派分支。
    for name in table::ALL {
        db.exec_raw(format!(
            "INSERT INTO sync_log (table_name, record_id) \
             VALUES ('{name}', 'ZZZZZZZZZZZZZZZZZZZZZZZZZZ')"
        ))
        .await;
    }

    let got = db.store.fetch_since(0, 1000).await;

    let records = match got {
        Ok(records) => records,
        Err(err) => {
            db.cleanup().await;
            panic!(
                "有表名没有 payload 加载分支：{err}\n\
                 —— 加表时改了 table 常量却漏了 sync.rs 的 match。\
                 不修的话，这张表第一次被写入后，所有客户端的同步游标会永久卡死。"
            );
        }
    };

    assert_eq!(
        records.len(),
        table::ALL.len(),
        "每条 sync_log 都该回一条记录，实际 {} 条对 {} 张表",
        records.len(),
        table::ALL.len()
    );

    // 顺带钉住「查不到行时给 None 而不是报错」—— 这是 redact 之后的正常形态，
    // 若哪天改成报错，被抹除过内容的库会整条同步不动
    for r in &records {
        assert!(
            r.payload.is_none(),
            "指向不存在行的日志应当 payload 为空，而不是凭空造一条：{}",
            r.table_name
        );
    }

    db.cleanup().await;
}

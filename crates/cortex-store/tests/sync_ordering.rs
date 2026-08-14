//! 同步全序的正确性 —— 本 crate 最要紧的一组测试。
//!
//! 核心断言：**并发写不漏行**。一个不断推进游标的客户端，在写入并发进行的
//! 同时反复拉取，最终必须一条不少、一条不重地拿到全部记录。
//!
//! 这是对 `pg_advisory_xact_lock(4272)` 方案的直接验证。若去掉那把锁，
//! 事务 A 可能先取到 seq=100 却后提交，客户端在 A 提交前拉到了 B 的 101、
//! 把游标推到 101，A 那一行就**永久**不可见。
//!
//! 这条测试是**做过反向验证**的：把 `WriteTxn::begin` 里的 advisory lock 注释掉，
//! 它稳定失败（32 条里漏 12～13 条）；锁加回来则稳定通过。所以它确实在测那件事，
//! 而不是碰巧成立。想复现这个反向验证，只需临时删掉那一句再跑本文件。

mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cortex_store::{SyncPayload, table};

/// 并发写事务数。写得多一点，才压得出提交乱序。
const WRITERS: usize = 32;

/// 一半的写事务在取号之后拖这么久才提交，制造「取号序 != 提交序」的窗口。
///
/// 加了 advisory lock 之后这些事务被串行化，总耗时约
/// `WRITERS / 2 * SLOW_COMMIT`，所以这个值不能太大。
const SLOW_COMMIT: Duration = Duration::from_millis(6);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_writes_are_never_missed_by_a_cursor() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = db.store.clone();

    let writers_done = Arc::new(AtomicBool::new(false));

    // ── 读端：模拟一个不断推进游标的客户端 ──
    let reader = {
        let store = store.clone();
        let writers_done = Arc::clone(&writers_done);
        tokio::spawn(async move {
            let mut cursor = 0_i64;
            let mut seen: Vec<(i64, String)> = Vec::new();

            loop {
                // 先读标志再拉取：若标志已置位，则本次拉取覆盖了全部已提交的写入，
                // 拉空即可断定确实拉干净了。反过来（先拉后读）会漏掉两者之间提交的行。
                let finished = writers_done.load(Ordering::SeqCst);

                let batch = store
                    .fetch_since(cursor, 8)
                    .await
                    .expect("增量拉取不应失败");

                if let Some(last) = batch.last() {
                    cursor = last.seq;
                }
                let drained = batch.is_empty();
                seen.extend(batch.into_iter().map(|r| (r.seq, r.record_id)));

                if drained {
                    if finished {
                        break;
                    }
                    // 轮询要足够密，才落得进上面那个提交乱序窗口
                    tokio::time::sleep(Duration::from_micros(200)).await;
                }
            }

            seen
        })
    };

    // ── 写端：并发跑 WRITERS 个写事务 ──
    let mut writers = Vec::with_capacity(WRITERS);
    for i in 0..WRITERS {
        let store = store.clone();
        writers.push(tokio::spawn(async move {
            let episode = common::new_episode("concurrent", &format!("第 {i} 条"));
            let id = episode.id.to_string();

            store
                .write_txn(async |tx| {
                    let seq = tx.insert_episode(&episode).await?;
                    // 关键：**取号之后**再拖一会儿才提交。
                    // 一半事务立刻提交、一半拖 SLOW_COMMIT_MS，于是必然出现
                    // 「先取号的后提交」。没有 advisory lock 时，读端会在这个
                    // 窗口里读到后取号者、把游标推过去，先取号那一行就永久丢失。
                    if i % 2 == 0 {
                        tokio::time::sleep(SLOW_COMMIT).await;
                    }
                    Ok(seq)
                })
                .await
                .expect("写事务不应失败");

            id
        }));
    }

    let mut expected = HashSet::with_capacity(WRITERS);
    for writer in writers {
        expected.insert(writer.await.expect("写任务不应 panic"));
    }
    writers_done.store(true, Ordering::SeqCst);

    let seen = reader.await.expect("读任务不应 panic");

    // ① 一条不少
    let seen_ids: HashSet<String> = seen.iter().map(|(_, id)| id.clone()).collect();
    let missing: Vec<_> = expected.difference(&seen_ids).collect();
    assert!(
        missing.is_empty(),
        "游标漏掉了 {} 条记录（advisory lock 失效）：{missing:?}",
        missing.len()
    );

    // ② 一条不重
    assert_eq!(
        seen.len(),
        WRITERS,
        "拉取结果有重复：拿到 {} 条，期望 {WRITERS} 条",
        seen.len()
    );

    // ③ seq 严格递增 —— 游标语义成立的前提
    let seqs: Vec<i64> = seen.iter().map(|(seq, _)| *seq).collect();
    assert!(
        seqs.windows(2).all(|w| w[0] < w[1]),
        "seq 未严格递增：{seqs:?}"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn one_business_row_yields_exactly_one_sync_log_row() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "你好");
    let entity = common::new_entity("project", "Cortex");
    let fact = common::new_fact(entity.id, "uses", "Postgres", episode.id);

    let before = store.latest_seq().await.expect("取末端 seq 不应失败");

    let last_seq = store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&fact).await
        })
        .await
        .expect("写事务不应失败");

    // 三行业务数据 → 三行 sync_log，一一对应
    assert_eq!(last_seq, before + 3);

    let batch = store.fetch_since(before, 100).await.expect("拉取不应失败");
    assert_eq!(batch.len(), 3);

    assert_eq!(batch[0].table_name, table::EPISODES);
    assert_eq!(batch[1].table_name, table::ENTITIES);
    assert_eq!(batch[2].table_name, table::FACTS);

    // 业务行随日志行一并返回，且是类型化的
    match batch[0].payload.as_ref().expect("episode 载荷应当存在") {
        SyncPayload::Episode(row) => {
            assert_eq!(row.id, episode.id.to_string());
            assert_eq!(row.text.as_deref(), Some("你好"));
        }
        other => panic!("载荷类型不对：{other:?}"),
    }
    match batch[2].payload.as_ref().expect("fact 载荷应当存在") {
        SyncPayload::Fact(row) => {
            assert_eq!(row.id, fact.id.to_string());
            assert_eq!(row.subject_id, entity.id.to_string());
        }
        other => panic!("载荷类型不对：{other:?}"),
    }

    db.cleanup().await;
}

#[tokio::test]
async fn failed_transaction_leaves_no_trace_in_the_sync_log() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let before = store.latest_seq().await.expect("取末端 seq 不应失败");

    let episode = common::new_episode("s1", "会被回滚");
    // 第二条 fact 的 source_episode 指向不存在的 episode → FK 违约 → 整事务回滚
    let orphan_entity = common::new_entity("project", "Ghost");
    let orphan_fact = common::new_fact(
        orphan_entity.id,
        "uses",
        "Nothing",
        cortex_core::Id::new(), // 无主的出处
    );

    let outcome = store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&orphan_entity).await?;
            tx.insert_fact(&orphan_fact).await
        })
        .await;

    assert!(outcome.is_err(), "外键违约应当让写事务失败");

    // 业务行与 sync_log 同生共死：回滚之后两边都不该留下任何痕迹
    assert_eq!(
        store.latest_seq().await.expect("取末端 seq 不应失败"),
        before,
        "回滚的事务不应推进 sync_log"
    );
    assert!(
        store
            .episode(&episode.id.to_string())
            .await
            .expect("查询不应失败")
            .is_none(),
        "回滚的事务不应留下业务行"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn fetch_since_paginates_in_seq_order() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let mut ids = Vec::new();
    for i in 0..10 {
        let episode = common::new_episode("paging", &format!("第 {i} 条"));
        ids.push(episode.id.to_string());
        store
            .write_txn(async |tx| tx.insert_episode(&episode).await)
            .await
            .expect("写事务不应失败");
    }

    let mut cursor = 0_i64;
    let mut collected = Vec::new();
    loop {
        let batch = store.fetch_since(cursor, 3).await.expect("拉取不应失败");
        if batch.is_empty() {
            break;
        }
        cursor = batch.last().expect("非空批次必有末条").seq;
        collected.extend(batch.into_iter().map(|r| r.record_id));
    }

    assert_eq!(collected, ids, "分页拉取的顺序应当与写入顺序一致");

    db.cleanup().await;
}

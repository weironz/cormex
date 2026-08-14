//! 实时事件源的集成测试：写入 → 触发器 → NOTIFY → 订阅者收到信号。
//!
//! 这条链路上的每一环都是真的（真事务、真触发器、真 `LISTEN`），因为它的
//! 失效方式全都是「静默」：触发器没装上、通道名对不上、监听连的是另一个
//! search_path —— 三者的症状完全相同，都是「一端写了、另一端不刷新」，
//! 而单测一条也测不出来。
//!
//! 数据库不可用时 skip 而非 fail（见 [`common::setup`]）。

mod common;

use std::time::Duration;

use cortex_store::SyncSignal;
use tokio::sync::mpsc;

/// 起一个后台任务把信号转出来。
///
/// 必须先订阅再写入 —— `LISTEN` 之前提交的事务，其通知不会补发。
/// 返回的接收端就是「另一端」。
fn spawn_subscriber(store: cortex_store::Store) -> mpsc::UnboundedReceiver<SyncSignal> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut notifications = store.listen_sync().await;
        while let Some(sig) = notifications.recv().await {
            if tx.send(sig).is_err() {
                break;
            }
        }
    });
    rx
}

fn cursor_of(sig: SyncSignal) -> i64 {
    match sig {
        SyncSignal::Bump { cursor } | SyncSignal::Resync { cursor } => cursor,
    }
}

/// 一端写入 → 另一端收到信号。
#[tokio::test]
async fn a_write_reaches_the_listener() {
    let Some(db) = common::setup().await else {
        return;
    };

    let mut rx = spawn_subscriber(db.store.clone());

    // 订阅是异步起的，写太早会落在 LISTEN 之前。重试而不是 sleep 一个魔法
    // 数字：慢机器上前者稳，后者只会变成 flaky
    let mut seen = None;
    for _ in 0..10 {
        let ep = common::new_episode("listen-test", "实时推送测试");
        let seq = db
            .store
            .write_txn(async |t| t.insert_episode(&ep).await)
            .await
            .expect("写 episode 不该失败");

        let deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while let Ok(Some(sig)) = tokio::time::timeout_at(deadline, rx.recv()).await {
            // 同一个 database 里并发跑的其他临时 schema 也会往这个通道发通知
            // （通道名的作用域是 database，见 sync.rs），因此只认「追上了我的 seq」
            if cursor_of(sig) >= seq {
                seen = Some((seq, cursor_of(sig)));
                break;
            }
        }
        if seen.is_some() {
            break;
        }
    }

    let outcome = seen;
    db.cleanup().await;

    let (seq, cursor) = outcome.expect(
        "写入 sync_log 后应当在 8 秒内收到通知；\
         收不到说明触发器没装上、或 LISTEN 订阅的不是同一个通道",
    );
    assert!(
        cursor >= seq,
        "推送的游标 {cursor} 落后于刚写入的 seq {seq}，客户端会以为自己已经追平"
    );
}

/// 触发器必须幂等：cortexd 每次重启都会重新装一遍。
#[tokio::test]
async fn installing_the_trigger_twice_is_a_no_op() {
    let Some(db) = common::setup().await else {
        return;
    };

    let first = db.store.install_sync_notify().await;
    let second = db.store.install_sync_notify().await;

    db.cleanup().await;

    first.expect("首次安装触发器不该失败");
    second.expect("重复安装触发器必须幂等，不该报错");
}

/// 装三遍之后，`sync_log` 上仍然只挂着**一个**触发器。
///
/// 单看「install 返回 Ok」是不够的：真装重了的症状是每次写入推两遍 ——
/// 客户端只是多拉一次，不报错，于是这个缺陷可以在生产上活很久。
/// 所以直接去 `pg_trigger` 数。
#[tokio::test]
async fn reinstalling_does_not_stack_up_triggers() {
    let Some(db) = common::setup().await else {
        return;
    };

    for _ in 0..3 {
        db.store
            .install_sync_notify()
            .await
            .expect("重复安装触发器必须幂等");
    }

    // 走一条独立连接来数：它的 search_path 是默认的 public，
    // 因此必须显式把 schema 限定进 regclass，否则解析到的是别人的 sync_log
    let count = match admin_pool().await {
        Some(admin) => {
            let row: Option<(i64,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM pg_trigger
                  WHERE tgname = 'cortex_sync_notify' AND NOT tgisinternal
                    AND tgrelid = '\"{}\".sync_log'::regclass",
                db.schema()
            )))
            .fetch_one(&admin)
            .await
            .ok();
            admin.close().await;
            row.map(|c| c.0)
        }
        None => None,
    };

    db.cleanup().await;

    assert_eq!(
        count,
        Some(1),
        "重复安装后 sync_log 上的通知触发器不止一个 —— 每次写入会推多条通知"
    );
}

/// 一条只用来查系统表的连接。拿不到就让调用方跳过。
async fn admin_pool() -> Option<sqlx::PgPool> {
    let _ = dotenvy::dotenv();
    let url = std::env::var("DATABASE_URL")
        .ok()
        .filter(|u| !u.is_empty())?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .ok()
}

/// 关闭存储层后订阅必须收工，而不是对着一个已关闭的池子无限退避。
#[tokio::test]
async fn closing_the_store_ends_the_stream() {
    let Some(db) = common::setup().await else {
        return;
    };

    let mut notifications = db.store.listen_sync().await;
    let store = db.store.clone();

    // 先让它进入「已订阅、正在等」的状态，再关池子 —— 直接关会退化成
    // 「一次都没连上」，测不到「连接被池子关闭事件打断」这条路径
    let ep = common::new_episode("close-test", "先跑通一次");
    store
        .write_txn(async |t| t.insert_episode(&ep).await)
        .await
        .expect("写 episode 不该失败");
    let _ = tokio::time::timeout(Duration::from_millis(800), notifications.recv()).await;

    store.close().await;

    let ended = tokio::time::timeout(Duration::from_secs(5), notifications.recv()).await;

    db.cleanup().await;

    assert_eq!(
        ended.expect("存储层关闭后 recv 应当很快返回，而不是挂住退避重连"),
        None,
        "池子关闭后 recv 必须返回 None 让上层收工"
    );
}

//! 两个账号交叉读一遍，**一条都不能串过去**。
//!
//! # 这是整个多租户改动里唯一「错了会很惨」的地方
//!
//! 别的错误会报错、会变慢、会返回空。这一个不会：它返回**看起来完全正常
//! 的、属于别人的记忆**。没有异常、没有日志、没有告警，而暴露它的
//! 唯一途径是有人恰好认出「这不是我说过的话」。
//!
//! # 这里的两条各自覆盖什么，以及各自覆盖不到什么
//!
//! 存储层那条（`cortex-store/tests/tenant_isolation.rs`）验 `TenantPools`
//! 本身。**它一直是绿的，而请求路径上两个用户照样共用一片库** ——
//! 因为服务层从来没调用过它。这正是本次改动要补的缺口。
//!
//! - `one_users_memory_never_shows_up_in_anothers` 验的是**隔离的形状**：
//!   两个池、三条读法（按 id、列会话、全文检索）都不串。实测过它在
//!   「服务层忘了 bind」时**仍然是绿的** —— 它够不到服务层，
//!   所以它不能单独作为「多用户可以用了」的依据。
//! - `every_data_path_goes_through_bind` 才验服务层：`state.rs` 里
//!   每一条数据路都必须先绑定租户。故障注入过，会红。
//!
//! 而「某个账号解析到哪个 schema」由 `src/request_tenant.rs` 里的单元测试
//! 覆盖（要真库），那是这次唯一的新判断。
//!
//! 跑法（要真库）：
//! ```bash
//! DATABASE_URL=postgres://... cargo test -p cortexd --test two_users_cannot_see_each_other
//! ```
//! 没有 `DATABASE_URL` 时整个文件跳过 —— 这类测试造不出有意义的假替身：
//! `search_path` 是 Postgres 的行为，模拟它等于模拟掉被测对象本身。

use cortex_store::{SchemaName, Store, TenantPools};

fn database_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty())
}

/// 造一个租户：建 schema、灌 migration、返回它的 `Store`。
async fn provision(pools: &TenantPools, schema: &SchemaName) -> Store {
    pools.create_schema(schema).await.expect("建 schema");
    let store = pools.get(schema).await.expect("取池");
    store.migrate().await.expect("灌 migration");
    (*store).clone()
}

async fn write_one(store: &Store, session: &str, text: &str) -> String {
    let id = cortex_core::Id::new();
    let session = session.to_owned();
    let text = text.to_owned();
    store
        .write_txn(async |tx| {
            tx.insert_episode(&cortex_store::NewEpisode {
                id,
                session_id: session,
                role: cortex_store::Role::User,
                content: serde_json::json!([{"type": "text", "text": text}]),
                text: Some(text.clone()),
                tsv_source: Some(text),
                domain: None,
                device_id: "test".into(),
                occurred_at: chrono::Utc::now(),
            })
            .await?;
            Ok(())
        })
        .await
        .expect("写 episode");
    id.to_string()
}

/// A 写的东西，B 一条都读不到；反之亦然。
#[tokio::test]
async fn one_users_memory_never_shows_up_in_anothers() {
    let Some(url) = database_url() else {
        eprintln!("跳过：没有 DATABASE_URL");
        return;
    };
    let pools = TenantPools::new(&url);

    // 两个真实形状的 schema 名（u_ + 26 位小写 ULID）
    let a = SchemaName::derive(&cortex_core::Id::new().to_string()).expect("A 的 schema 名");
    let b = SchemaName::derive(&cortex_core::Id::new().to_string()).expect("B 的 schema 名");
    let store_a = provision(&pools, &a).await;
    let store_b = provision(&pools, &b).await;

    let a_secret = "阿尔法的工资是 41730 元";
    let b_secret = "布拉沃的工资是 86290 元";
    let a_id = write_one(&store_a, "sess-a", a_secret).await;
    let b_id = write_one(&store_b, "sess-b", b_secret).await;

    // 交叉读：按 id 取
    assert!(
        store_b.episode(&a_id).await.expect("查 B 的库").is_none(),
        "B 用 A 的 episode id 查到了东西 —— 两个账号落在同一个 schema 上了。\
         这正是「登录了两个人，读的是同一片库」的样子"
    );
    assert!(
        store_a.episode(&b_id).await.expect("查 A 的库").is_none(),
        "A 用 B 的 episode id 查到了东西（反方向同样必须不通）"
    );

    // 交叉读：列会话
    let a_sessions = store_a
        .session_digests(100, true, None)
        .await
        .expect("列 A 的会话");
    assert!(
        a_sessions.iter().all(|s| s.session_id != "sess-b"),
        "A 的会话列表里出现了 B 的会话 sess-b。按 id 查不到但列表里能看见，\
         是更坏的一种串档 —— 侧边栏上就直接显示出来了"
    );
    let b_sessions = store_b
        .session_digests(100, true, None)
        .await
        .expect("列 B 的会话");
    assert!(
        b_sessions.iter().all(|s| s.session_id != "sess-a"),
        "B 的会话列表里出现了 A 的会话 sess-a"
    );

    // 交叉读：全文检索。这一路最危险 —— 它不需要知道对方的 id
    let hits = store_b
        .recall_bm25("工资", 50)
        .await
        .expect("在 B 的库里检索");
    assert!(
        hits.iter().all(|h| !h.statement.contains("41730")),
        "B 检索「工资」时召回了 A 的数字。检索不需要知道对方的 id，\
         所以这一路是串档最容易被真实用户撞见的地方"
    );

    for s in [&a, &b] {
        let _ = pools.drop_schema(s).await;
    }
}

/// 请求路径上**没有任何一条**绕过 `bind` 的通路。
///
/// # 为什么读源码，而不是跑一遍请求
///
/// 上面那条测试证明「按租户分流是对的」，但它证明不了「每一条路都分流了」——
/// 一条忘了 `bind` 的路径只会在**那一个端点**上串档，而端点有二十几个，
/// 逐个跑一遍是买不起的覆盖率。
///
/// 而 `state.rs` 里的形状是稳定的：数据方法都在 `Backend::Live(l) =>` 后面。
/// 所以直接断言那些分支的写法。它拦不住所有事，但拦得住「新加了一个端点、
/// 照着上一个抄、忘了 bind」—— 而那是这类回归唯一的来源。
///
/// 真正的地基是类型：`Live` 上那十个方法是模块私有的，
/// 只有 `BoundLive` 转发出去。这条测试是第二道，防的是有人把它们改回 `pub`。
#[test]
fn every_data_path_goes_through_bind() {
    let src = include_str!("../src/state.rs");

    // `Backend::Live(l) =>` 之后直接跟 `l.<方法>(` 的，必须是 bind 或白名单里的
    let allowed_unbound = [
        "store", // 实时推送的监听任务查游标，进程级
        "latest_cursor",
        "database_status",
        "embedding_health",
        // 纯转发，不碰任何一张表。自带 key 那一支拿的是解密后的字符串，
        // 而**取那把 key 的时候已经绑定过租户了**（handler 里 own_key(&tenant)）
        "llm_stream_with_key",
        "bind",
    ];
    let mut offenders = Vec::new();
    for (i, line) in src.lines().enumerate() {
        let Some(rest) = line.split("Backend::Live(l) => ").nth(1) else {
            continue;
        };
        let Some(call) = rest.strip_prefix("l.") else {
            continue; // 多行分支，下面单独看
        };
        let name: String = call
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !allowed_unbound.contains(&name.as_str()) {
            offenders.push(format!("{}: {}", i + 1, line.trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "state.rs 里有分支直接在未绑定的 Live 上调数据方法：\n{}\n\
         每一条读写数据的路都必须先 `l.bind(tenant.store()?)`，\
         否则它落在 public 上 —— 也就是 1 号用户的库。\n\
         确实与租户无关的方法（进程级游标、健康检查、纯转发）请加进 \
         `allowed_unbound` 并写清楚为什么。",
        offenders.join("\n")
    );
}

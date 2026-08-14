//! 工具轨迹抽取的集成测试：真连数据库，验证轨迹派生的事实最终**长在库里**。
//!
//! 判据的纯逻辑在 `cortex_memory::trace` 的单测里已经钉死了。这一层只回答
//! 单测回答不了的三个问题：
//!
//! 1. 轨迹到底有没有被**读**（`write_candidates` 是否真的多了一个输入源）；
//! 2. 落库的行上 `source_channel` / `trust_tier` 对不对；
//! 3. 惯例这条判据能不能**跨 episode** 累计 —— 单测里的 `history` 是喂进去的，
//!    真实的 history 来自 `store.recent_tool_calls`。

mod common;

use cortex_core::Id;
use cortex_store::{NewRedaction, RedactionMode, RedactionTarget};

use common::{DEVICE, ctx, offline_extractor, seed_episode, seed_episode_with_tools, setup, ts};

/// 题集 `sc-tool-lock` 那段轨迹：两次失败一次成功，形状照抄生产。
const LOCK_TRACE: &[(&str, Option<&str>, bool, &str)] = &[
    (
        "bash",
        Some("Cargo.toml"),
        false,
        "cargo test --workspace 失败：error: failed to select a version for sqlx-core",
    ),
    (
        "bash",
        Some("Cargo.lock"),
        false,
        "cargo update -p sqlx-core --precise 0.9.1 之后重跑 cargo test --workspace，仍然失败：那一步把版本又按回去了",
    ),
    (
        "bash",
        Some("Cargo.lock"),
        true,
        "cargo test --workspace --locked 通过：锁住 Cargo.lock 之后本地与 CI 解析一致",
    ),
];

async fn statements(store: &cortex_store::Store, ids: &[Id]) -> Vec<String> {
    let mut out = Vec::new();
    for id in ids {
        let f = store
            .fact(&id.to_string())
            .await
            .expect("查事实不该失败")
            .expect("刚写的事实应当查得到");
        out.push(f.statement);
    }
    out
}

/// 这条测试就是「抽取器读没读那张表」的最小证据。
///
/// 注意候选列表是**空的** —— 对话本身一个字都没提结论，全部产出都来自轨迹。
#[tokio::test]
async fn a_turn_with_only_a_tool_trace_still_produces_facts() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode_with_tools(&store, "工作区的测试先跑通再说。", LOCK_TRACE).await;
    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("轨迹抽取不该失败");

    assert!(
        !report.written.is_empty(),
        "对话候选为空但轨迹里有「失败后成功」，抽取器必须产出事实；\
         一条都没有就说明它压根没读 episode_tool_calls"
    );

    let all = statements(&store, &report.written).await.join("\n");
    assert!(
        all.contains("--locked"),
        "最终生效的那个参数必须逐字留下来，实际落库：\n{all}"
    );
    assert!(
        all.contains("cargo update"),
        "被证伪的那条路同样是知识（Manus：保留错误记录不清洗），实际落库：\n{all}"
    );

    db.cleanup().await;
}

/// 「失败：」之后那一截是工具输出原文，`docs/memory-content.md` §二③ 已定案不存。
///
/// 这一条在评测里的对应物是 qt01 的 `never_extracted`。放一份在这里，
/// 是因为评测要连数据库跑整套题集，而这条约束值得有一个几秒钟就能失败的守卫。
#[tokio::test]
async fn raw_tool_output_never_reaches_the_database() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode_with_tools(&store, "跑一下测试。", LOCK_TRACE).await;
    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("轨迹抽取不该失败");

    let all = statements(&store, &report.written).await.join("\n");
    assert!(
        !all.contains("failed to select a version"),
        "命令的原始输出进了 append-only 的库就再也拿不出来（只剩 redact 一条出路），\
         实际落库：\n{all}"
    );

    db.cleanup().await;
}

/// 轨迹派生的事实必须标 `tool_output`（tier 3）。
///
/// 标错方向是有害的：tier 的用途是「最可信的那批不该被外部内容盖掉」，
/// 把「模型自己造出来又自己读回来」的那批抬到 tier 2 就等于取消了这道栅栏。
#[tokio::test]
async fn trace_facts_are_written_as_tool_output_tier_three() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode_with_tools(&store, "跑一下测试。", LOCK_TRACE).await;
    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("轨迹抽取不该失败");

    for id in &report.written {
        let f = store
            .fact(&id.to_string())
            .await
            .expect("查事实不该失败")
            .expect("刚写的事实应当查得到");
        assert_eq!(
            f.source_channel.as_str(),
            "tool_output",
            "轨迹派生的事实来源通道错了：{}",
            f.statement
        );
        assert_eq!(
            f.trust_tier,
            Some(3),
            "tier 必须与通道一致：{}",
            f.statement
        );
        assert!(
            f.embedding.is_some(),
            "embedding 必须与主行同事务写入，否则向量召回永远漏这条：{}",
            f.statement
        );
    }

    db.cleanup().await;
}

/// 惯例这条判据要跨 episode 才算得出来 —— 这是 `store.recent_tool_calls`
/// 存在的唯一理由，也是单测（history 是喂进去的）覆盖不到的那一半。
#[tokio::test]
async fn a_habit_only_emerges_after_enough_turns() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ci: &[(&str, Option<&str>, bool, &str)] = &[("bash", Some("."), true, "just ci 通过")];

    let mut written_so_far = Vec::new();
    for turn in 1..=3 {
        let ep = seed_episode_with_tools(&store, "提交前先过一遍。", ci).await;
        let report = extractor
            .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 6, turn)))
            .await
            .expect("轨迹抽取不该失败");
        written_so_far.extend(report.written);

        let all = statements(&store, &written_so_far).await.join("\n");
        let claimed = all.contains("惯例");
        assert_eq!(
            claimed,
            turn >= 3,
            "第 {turn} 轮时是否已经宣布惯例 = {claimed}，与阈值 3 不符。\
             库里现有：\n{all}"
        );
    }

    db.cleanup().await;
}

/// 出处被抹除之后，轨迹派生的那一路也必须一个字都不写。
///
/// 单独测一遍是因为它走的是**新加的**输入源：原有那条守卫只覆盖了
/// 调用方传进来的候选，而轨迹是抽取器自己去库里读的 —— 漏掉这一句，
/// redact 之后一次在途的抽取任务就能把已抹除轮次的命令重新写回来。
#[tokio::test]
async fn redacted_source_blocks_trace_derived_writes() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode_with_tools(&store, "跑一下测试。", LOCK_TRACE).await;
    let tombstone = NewRedaction {
        id: Id::new(),
        target_kind: RedactionTarget::Episode,
        target_id: ep.to_string(),
        mode: RedactionMode::Redact,
        reason: "命令行里带了口令".to_owned(),
        actor: "user".to_owned(),
        device_id: DEVICE.to_owned(),
    };
    store
        .write_txn(async |tx| tx.insert_redaction(&tombstone).await)
        .await
        .expect("墓碑应能落库");

    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("被抹除的出处不该导致报错，只是什么都不写");

    assert!(
        report.written.is_empty() && report.new_entities.is_empty(),
        "在途抽取任务绝不能把已抹除轮次的轨迹回填：{report:?}"
    );

    db.cleanup().await;
}

/// 纯聊天的轮次不该因为新增了这一路而多出任何事实。
#[tokio::test]
async fn a_turn_without_tool_calls_is_unaffected() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode(&store, "随便聊两句").await;
    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("空候选 + 无轨迹不该失败");

    assert_eq!(
        report.candidates, 0,
        "没有轨迹的轮次不该凭空多出候选：{report:?}"
    );
    assert!(report.written.is_empty());

    db.cleanup().await;
}

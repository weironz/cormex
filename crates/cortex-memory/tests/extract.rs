//! 抽取管线的集成测试：真连数据库，验证消解与落库的最终状态。
//!
//! 判断标准一律是**库里的最终状态**（`active_facts` / `fact_events`），
//! 不是函数返回值 —— 返回值对了但没写进去，或写进去了但视图算错，
//! 都是这一层才能发现的问题。

mod common;

use std::sync::Arc;

use cortex_core::Id;
use cortex_memory::extract::{EntityRef, ObjectRef, PredicateRules};
use cortex_store::{FactOp, InvalidationKind, NewRedaction, RedactionMode, RedactionTarget};

use common::{DEVICE, candidate, ctx, offline_extractor, seed_episode, setup, ts};

/// 取某 (主语, 谓词) 下当前有效的事实的宾语集合。
async fn active_objects(store: &cortex_store::Store, subject: Id, predicate: &str) -> Vec<String> {
    store
        .active_facts_by_subject_predicate(&subject.to_string(), predicate)
        .await
        .expect("查有效事实不该失败")
        .into_iter()
        .filter_map(|f| f.object_text)
        .collect()
}

#[tokio::test]
async fn superseding_fact_invalidates_the_old_one() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep1 = seed_episode(&store, "我们决定用 MySQL").await;
    let first = extractor
        .write_candidates(
            &store,
            vec![candidate(
                "cortex",
                "project",
                "uses",
                "MySQL",
                Some(ts(2026, 1, 1)),
            )],
            &ctx(ep1, ts(2026, 1, 1)),
        )
        .await
        .expect("首条事实应能落库");
    assert_eq!(first.written.len(), 1, "首条事实应被写入");
    let old_fact = first.written[0];
    let subject = first.new_entities[0];

    let ep2 = seed_episode(&store, "改用 Postgres 了").await;
    let second = extractor
        .write_candidates(
            &store,
            vec![candidate(
                "cortex",
                "project",
                "uses",
                "Postgres",
                Some(ts(2026, 6, 1)),
            )],
            &ctx(ep2, ts(2026, 6, 1)),
        )
        .await
        .expect("取代性事实应能落库");

    assert_eq!(
        second.superseded,
        vec![old_fact],
        "uses 是单值谓词，更晚的取值必须取代更早的"
    );

    // ── 旧事实退出检索，新事实留下 ──
    let active = active_objects(&store, subject, "uses").await;
    assert_eq!(
        active,
        vec!["Postgres".to_string()],
        "active_facts 应当只剩新事实，实际 {active:?}"
    );

    // ── 失效以追加事件表达，不是就地改行 ──
    let events = store
        .fact_events(&old_fact.to_string())
        .await
        .expect("查生命周期事件不该失败");
    assert_eq!(
        events.len(),
        1,
        "旧事实应恰好有一条失效事件，实际 {events:?}"
    );
    let ev = &events[0];
    assert_eq!(ev.op, FactOp::Invalidate);
    assert_eq!(
        ev.kind,
        Some(InvalidationKind::Superseded),
        "世界变了属于 superseded，不是 corrected"
    );
    assert_eq!(
        ev.superseded_by.as_deref(),
        Some(second.written[0].to_string().as_str()),
        "superseded_by 必须指向新事实，否则事实演化链断裂"
    );
    assert_eq!(
        ev.invalid_at,
        Some(ts(2026, 6, 1)),
        "invalid_at 应取新事实的事件时间，而不是入库时刻"
    );

    // 旧事实本身没被改动，审计视图仍看得见
    let row = store
        .fact(&old_fact.to_string())
        .await
        .expect("查事实不该失败")
        .expect("旧事实必须仍在表里 —— append-only 不做删除");
    assert_eq!(row.object_text.as_deref(), Some("MySQL"));

    db.cleanup().await;
}

#[tokio::test]
async fn supplementary_fact_coexists() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode(&store, "张三喜欢 Rust，也喜欢 Go").await;
    let report = extractor
        .write_candidates(
            &store,
            vec![
                candidate("张三", "person", "likes", "Rust", None),
                candidate("张三", "person", "likes", "Go", None),
            ],
            &ctx(ep, ts(2026, 6, 1)),
        )
        .await
        .expect("补充性事实应能落库");

    assert_eq!(report.written.len(), 2, "两条都该写入");
    assert!(
        report.superseded.is_empty(),
        "likes 不在单值谓词表里，两条是补充关系，不该失效任何一条：{:?}",
        report.superseded
    );

    let subject = report.new_entities[0];
    let mut active = active_objects(&store, subject, "likes").await;
    active.sort();
    assert_eq!(
        active,
        vec!["Go".to_string(), "Rust".to_string()],
        "两条应当并存"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn entity_aliases_merge_via_vector_similarity() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep1 = seed_episode(&store, "cortex 用 Postgres").await;
    let first = extractor
        .write_candidates(
            &store,
            vec![candidate("cortex", "project", "uses", "Postgres", None)],
            &ctx(ep1, ts(2026, 6, 1)),
        )
        .await
        .expect("首条应能落库");
    assert_eq!(first.new_entities.len(), 1, "首次出现应新建实体");
    let entity = first.new_entities[0];

    // 名字不同（精确匹配必然落空），只能靠向量近似归并
    let ep2 = seed_episode(&store, "Cortex 项目的对象存储用 RustFS").await;
    let second = extractor
        .write_candidates(
            &store,
            vec![candidate(
                "Cortex 项目",
                "project",
                "stores_in",
                "RustFS",
                None,
            )],
            &ctx(ep2, ts(2026, 6, 2)),
        )
        .await
        .expect("第二条应能落库");

    assert!(
        second.new_entities.is_empty(),
        "「Cortex 项目」应归并到已有实体，不该再造一个：{:?}",
        second.new_entities
    );

    let fact = store
        .fact(&second.written[0].to_string())
        .await
        .expect("查事实不该失败")
        .expect("第二条事实应已落库");
    assert_eq!(
        fact.subject_id,
        entity.to_string(),
        "两条事实必须挂在同一个实体上，否则图谱会被别名撕成两半"
    );

    // 精确同名的那条路也要走通
    let ep3 = seed_episode(&store, "cortex 的部署目标是 fly.io").await;
    let third = extractor
        .write_candidates(
            &store,
            vec![candidate(
                "cortex",
                "project",
                "deployed_to",
                "fly.io",
                None,
            )],
            &ctx(ep3, ts(2026, 6, 3)),
        )
        .await
        .expect("第三条应能落库");
    assert!(third.new_entities.is_empty(), "同名实体应走精确匹配复用");

    db.cleanup().await;
}

#[tokio::test]
async fn duplicate_candidate_is_dropped() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep1 = seed_episode(&store, "cortex 用 Postgres").await;
    extractor
        .write_candidates(
            &store,
            vec![candidate("cortex", "project", "uses", "Postgres", None)],
            &ctx(ep1, ts(2026, 6, 1)),
        )
        .await
        .expect("首条应能落库");

    // 同一件事在下一轮被再次提及，只是大小写和空白不同
    let ep2 = seed_episode(&store, "对，我们用的是 postgres").await;
    let again = extractor
        .write_candidates(
            &store,
            vec![candidate("cortex", "project", "uses", "  postgres ", None)],
            &ctx(ep2, ts(2026, 6, 2)),
        )
        .await
        .expect("重复候选也应正常返回");

    assert!(
        again.written.is_empty(),
        "同主谓宾的重复事实必须丢弃 —— 检索是 top-K，重复会把别的挤出去"
    );
    assert_eq!(again.duplicates, 1);

    db.cleanup().await;
}

#[tokio::test]
async fn same_instant_conflict_flags_both_sides() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let at = ts(2026, 6, 1);
    let ep1 = seed_episode(&store, "任务 A 归张三").await;
    let first = extractor
        .write_candidates(
            &store,
            vec![candidate("任务A", "task", "assigned_to", "张三", Some(at))],
            &ctx(ep1, at),
        )
        .await
        .expect("首条应能落库");
    let old_fact = first.written[0];
    let subject = first.new_entities[0];

    // 同一时刻、同一单值谓词、不同取值 —— 确定性逻辑判不出谁对
    let ep2 = seed_episode(&store, "任务 A 归李四").await;
    let second = extractor
        .write_candidates(
            &store,
            vec![candidate("任务A", "task", "assigned_to", "李四", Some(at))],
            &ctx(ep2, at),
        )
        .await
        .expect("冲突候选应能落库");

    assert!(
        second.superseded.is_empty(),
        "判不出先后就不该失效任何一条：{:?}",
        second.superseded
    );
    assert_eq!(
        second.flagged.len(),
        2,
        "新旧两侧都要挂 flag，待确认列表才看得到矛盾的两条：{:?}",
        second.flagged
    );

    // flag 是批注，不改变状态：两条仍然有效
    let mut active = active_objects(&store, subject, "assigned_to").await;
    active.sort();
    assert_eq!(
        active,
        vec!["张三".to_string(), "李四".to_string()],
        "flag 不该改变 active_facts"
    );

    let events = store
        .fact_events(&old_fact.to_string())
        .await
        .expect("查事件不该失败");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].op, FactOp::Flag);
    assert!(
        events[0].kind.is_none() && events[0].invalid_at.is_none(),
        "flag 不得携带 invalidate 专属字段（schema CHECK 也会拦）"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn redacted_source_blocks_derived_writes() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode(&store, "误粘贴了一串密钥").await;
    let tombstone = NewRedaction {
        id: Id::new(),
        target_kind: RedactionTarget::Episode,
        target_id: ep.to_string(),
        mode: RedactionMode::Redact,
        reason: "误粘贴密钥".to_owned(),
        actor: "user".to_owned(),
        device_id: DEVICE.to_owned(),
    };
    store
        .write_txn(async |tx| tx.insert_redaction(&tombstone).await)
        .await
        .expect("墓碑应能落库");

    // 抽取任务是异步的，很可能在 redact 执行时还在飞
    let report = extractor
        .write_candidates(
            &store,
            vec![candidate("cortex", "project", "uses", "sk-xxx", None)],
            &ctx(ep, ts(2026, 6, 1)),
        )
        .await
        .expect("被抹除的出处不该导致报错，只是什么都不写");

    assert!(
        report.written.is_empty() && report.new_entities.is_empty(),
        "在途抽取任务绝不能把已抹除的内容回填：{report:?}"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn intra_batch_conflict_is_resolved_too() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor().with_rules(PredicateRules::default());

    let ep = seed_episode(&store, "先说用 MySQL，后来又改口说用 Postgres").await;
    let report = extractor
        .write_candidates(
            &store,
            vec![
                candidate("cortex", "project", "uses", "MySQL", Some(ts(2026, 1, 1))),
                candidate(
                    "cortex",
                    "project",
                    "uses",
                    "Postgres",
                    Some(ts(2026, 6, 1)),
                ),
            ],
            &ctx(ep, ts(2026, 6, 1)),
        )
        .await
        .expect("同批次冲突应能落库");

    assert_eq!(report.written.len(), 2, "两条都写入，失效靠追加事件表达");
    assert_eq!(
        report.superseded.len(),
        1,
        "同一批次内的互斥事实也要判定，否则两条会双双生效"
    );

    let subject = report.new_entities[0];
    let active = active_objects(&store, subject, "uses").await;
    assert_eq!(active, vec!["Postgres".to_string()], "实际 {active:?}");

    db.cleanup().await;
}

#[tokio::test]
async fn entity_object_becomes_a_graph_edge() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode(&store, "张三负责 cortex").await;
    let mut c = candidate("张三", "person", "owns", "", None);
    c.object = ObjectRef::Entity(EntityRef::new("cortex", "project"));
    c.statement = "张三负责 cortex 项目".to_owned();

    let report = extractor
        .write_candidates(&store, vec![c], &ctx(ep, ts(2026, 6, 1)))
        .await
        .expect("实体宾语应能落库");

    assert_eq!(report.new_entities.len(), 2, "主语与宾语都是新实体");
    let fact = store
        .fact(&report.written[0].to_string())
        .await
        .expect("查事实不该失败")
        .expect("事实应已落库");
    assert!(
        fact.object_entity_id.is_some() && fact.object_text.is_none(),
        "指向实体的事实应写 object_entity_id 而非 object_text，它是图谱里的一条边"
    );

    let inbound = store
        .active_facts_by_object(fact.object_entity_id.as_deref().unwrap(), 10)
        .await
        .expect("反向一跳不该失败");
    assert_eq!(inbound.len(), 1, "图遍历的反向一跳应能找到这条边");

    db.cleanup().await;
}

/// 真实模型的抽取质量。CI 没有 key，默认跳过；本地
/// `cargo test -p cortex-memory --test extract -- --ignored --nocapture` 手跑。
#[tokio::test]
#[ignore = "需要 DEEPSEEK_API_KEY，会产生真实 API 调用"]
async fn real_llm_extraction_quality() {
    let _ = dotenvy::dotenv();
    let llm = cortex_llm::LlmClient::from_env().expect("应能从环境构造 LlmClient");
    let extractor = cortex_memory::extract::Extractor::new(
        llm,
        Arc::new(cortex_memory::embed::HashEmbedder::new()),
        DEVICE,
    );

    let episode = "\
用户：早上好！我叫周伟，是 cortex 这个项目的负责人。今天想跟你确认几件事。
助手：早上好！请说。
用户：第一，对象存储我们最后定了用 RustFS，不用 MinIO 了，因为 license 变更那事儿。这个是上周三定的。
第二，帮我看下这段代码：fn main() { println!(\"hi\") } —— 有什么问题吗？
第三，我个人比较讨厌在代码里写英文注释，以后所有注释都写中文。
第四，现在几点了？
助手：现在无法获知当前时间。你这段代码没有问题。关于注释语言，我记下了。";

    let ctx = cortex_memory::extract::ExtractContext::new(Id::new(), ts(2026, 8, 7))
        .with_domain("coding");

    let facts = extractor
        .extract(episode, &ctx)
        .await
        .expect("真实抽取不该失败");

    println!("\n=== 抽出 {} 条事实 ===", facts.len());
    for f in &facts {
        println!(
            "- [{}] {} --{}--> {:?}\n    statement: {}\n    valid_at: {:?}  conf: {}",
            f.subject.kind,
            f.subject.name,
            f.predicate,
            f.object,
            f.statement,
            f.valid_at,
            f.confidence
        );
    }

    assert!(!facts.is_empty(), "这段对话里明显有该记的事实");
    assert!(
        facts.len() <= 8,
        "抽出 {} 条，太多了 —— 寒暄、代码片段、一次性查询都不该抽",
        facts.len()
    );
    let joined: String = facts.iter().map(|f| f.statement.as_str()).collect();
    assert!(
        joined.contains("RustFS"),
        "「决定用 RustFS」是典型的该抽项：{joined}"
    );
    assert!(
        !joined.contains("println"),
        "代码片段本身不该进 L1：{joined}"
    );
}

/// 真实模型 + 真实数据库跑完整条管线，验证 `extract → 消解 → 落库` 的接线。
/// 需要 `DEEPSEEK_API_KEY` 与 `DATABASE_URL`。
#[tokio::test]
#[ignore = "需要 DEEPSEEK_API_KEY 与数据库，会产生真实 API 调用"]
async fn real_llm_end_to_end_ingest() {
    let _ = dotenvy::dotenv();
    let Some(db) = setup().await else { return };
    let store = db.store.clone();

    let llm = cortex_llm::LlmClient::from_env().expect("应能从环境构造 LlmClient");
    let extractor = cortex_memory::extract::Extractor::new(
        llm,
        Arc::new(cortex_memory::embed::HashEmbedder::new()),
        DEVICE,
    );

    let text = "用户：记一下，cortex 的对象存储决定用 RustFS。另外我叫周伟，是这个项目的负责人。";
    let ep = seed_episode(&store, text).await;
    let report = extractor
        .ingest(&store, text, &ctx(ep, ts(2026, 8, 7)))
        .await
        .expect("完整管线不该失败");

    println!("\n=== ingest 报告 ===\n{report:#?}");
    assert!(!report.written.is_empty(), "应至少落库一条事实");

    for id in &report.written {
        let fact = store
            .fact(&id.to_string())
            .await
            .expect("查事实不该失败")
            .expect("刚写的事实应当查得到");
        println!("落库：{} | {}", fact.predicate, fact.statement);
        assert!(
            fact.embedding.is_some(),
            "embedding 必须与主行同事务写入，否则向量召回永远漏这条"
        );
        assert_eq!(fact.source_episode_id, ep.to_string(), "出处必须可追溯");
    }

    db.cleanup().await;
}

/// 沙箱写回来的那一轮，抽出的事实**降一档**到 tier 3。
///
/// # 这一条守的是什么
///
/// 沙箱里跑的是不可信代码。它写回来的 `role: user` 不是用户说的，是那个
/// 容器说的 —— 而容器可能已经被它自己读到的东西诱导（MINJA / AgentPoison
/// 都是写侧攻击）。不降级的话，一条被注入的「事实」会以**用户亲口说的**
/// 同等信任级，在未来所有会话、所有设备上被召回并注入 system prompt，
/// 而那个容器早就销毁了。
///
/// 四家云 agent 没有这条通道（它们的沙箱没有可写的长期记忆），所以这一格
/// 是 Cortex 特有的 —— 没有先例可抄，只能自己守。
///
/// **不是「不写」而是「降级」**：沙箱里干的活确实值得记住（那本来就是用户
/// 让它干的），只是不该与用户亲口说的话平起平坐。
#[tokio::test]
async fn facts_written_from_a_sandbox_are_not_trusted_like_the_user_speaking() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep_user = seed_episode(&store, "我们用 Postgres").await;
    let ep_sbx = seed_episode(&store, "沙箱说我们用 MySQL").await;

    let normal = extractor
        .write_candidates(
            &store,
            vec![candidate("alpha", "project", "uses", "Postgres", None)],
            &ctx(ep_user, ts(2026, 1, 1)),
        )
        .await
        .expect("普通那轮应能落库");
    let sandboxed = extractor
        .write_candidates(
            &store,
            vec![candidate("beta", "project", "uses", "MySQL", None)],
            &ctx(ep_sbx, ts(2026, 1, 2)).from(cortex_memory::extract::TurnOrigin::Sandbox),
        )
        .await
        .expect("沙箱那轮同样应能落库");

    let tier_of = |id: cortex_core::Id| {
        let store = store.clone();
        async move {
            store
                .fact(&id.to_string())
                .await
                .expect("查事实不该失败")
                .expect("刚写进去的事实应当查得到")
        }
    };
    let normal_fact = tier_of(normal.written[0]).await;
    let sandbox_fact = tier_of(sandboxed.written[0]).await;

    assert_eq!(
        normal_fact.source_channel.as_str(),
        "conversation",
        "普通对话抽出来的仍是 conversation（tier 2），这一条不该被改动"
    );
    assert_eq!(
        sandbox_fact.source_channel.as_str(),
        "tool_output",
        "沙箱写回来的必须降到 tool_output（tier 3）—— 与工具轨迹同级，         因为它们同属「模型自己造出来又自己读回来」的闭环"
    );
    assert!(
        sandbox_fact.trust_tier > normal_fact.trust_tier,
        "数字越大越不可信：沙箱那条的 tier（{:?}）必须严格高于普通那条（{:?}）",
        sandbox_fact.trust_tier,
        normal_fact.trust_tier
    );
}

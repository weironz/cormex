//! Integration tests for the extraction pipeline: a real database, checking
//! the final state left by resolution and persistence.
//!
//! The yardstick is always **the final state in the database** (`active_facts`
//! / `fact_events`), never a function's return value — a return value that is
//! correct but never written, or written but miscomputed by the view, are both
//! problems only this layer can catch.

mod common;

use std::sync::Arc;

use cormex_core::Id;
use cormex_memory::extract::{EntityRef, ObjectRef, PredicateRules};
use cormex_store::{FactOp, InvalidationKind, NewRedaction, RedactionMode, RedactionTarget};

use common::{DEVICE, candidate, ctx, offline_extractor, seed_episode, setup, ts};

/// Collects the objects of every fact currently active under a given
/// (subject, predicate).
async fn active_objects(store: &cormex_store::Store, subject: Id, predicate: &str) -> Vec<String> {
    store
        .active_facts_by_subject_predicate(&subject.to_string(), predicate)
        .await
        .expect("querying active facts should not fail")
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
                "cormex",
                "project",
                "uses",
                "MySQL",
                Some(ts(2026, 1, 1)),
            )],
            &ctx(ep1, ts(2026, 1, 1)),
        )
        .await
        .expect("the first fact should be persisted");
    assert_eq!(first.written.len(), 1, "the first fact should be written");
    let old_fact = first.written[0];
    let subject = first.new_entities[0];

    let ep2 = seed_episode(&store, "改用 Postgres 了").await;
    let second = extractor
        .write_candidates(
            &store,
            vec![candidate(
                "cormex",
                "project",
                "uses",
                "Postgres",
                Some(ts(2026, 6, 1)),
            )],
            &ctx(ep2, ts(2026, 6, 1)),
        )
        .await
        .expect("the superseding fact should be persisted");

    assert_eq!(
        second.superseded,
        vec![old_fact],
        "uses is a functional predicate: the later value must supersede the earlier one"
    );

    // ── the old fact drops out of retrieval, the new one stays ──
    let active = active_objects(&store, subject, "uses").await;
    assert_eq!(
        active,
        vec!["Postgres".to_string()],
        "active_facts should hold nothing but the new fact, actually {active:?}"
    );

    // ── invalidation is expressed by an appended event, not an in-place edit ──
    let events = store
        .fact_events(&old_fact.to_string())
        .await
        .expect("querying lifecycle events should not fail");
    assert_eq!(
        events.len(),
        1,
        "the old fact should have exactly one invalidation event, actually {events:?}"
    );
    let ev = &events[0];
    assert_eq!(ev.op, FactOp::Invalidate);
    assert_eq!(
        ev.kind,
        Some(InvalidationKind::Superseded),
        "the world changed, which is superseded, not corrected"
    );
    assert_eq!(
        ev.superseded_by.as_deref(),
        Some(second.written[0].to_string().as_str()),
        "superseded_by must point at the new fact, or the chain of fact evolution snaps"
    );
    assert_eq!(
        ev.invalid_at,
        Some(ts(2026, 6, 1)),
        "invalid_at should take the new fact's event time, not the moment it was persisted"
    );

    // the old fact itself is untouched; the audit view can still see it
    let row = store
        .fact(&old_fact.to_string())
        .await
        .expect("querying a fact should not fail")
        .expect("the old fact must still be in the table — append-only does not delete");
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
        .expect("supplementary facts should be persisted");

    assert_eq!(report.written.len(), 2, "both should be written");
    assert!(
        report.superseded.is_empty(),
        "likes is not in the functional-predicate table; the two supplement each other, \
         so neither should be invalidated: {:?}",
        report.superseded
    );

    let subject = report.new_entities[0];
    let mut active = active_objects(&store, subject, "likes").await;
    active.sort();
    assert_eq!(
        active,
        vec!["Go".to_string(), "Rust".to_string()],
        "the two should coexist"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn entity_aliases_merge_via_vector_similarity() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep1 = seed_episode(&store, "cormex 用 Postgres").await;
    let first = extractor
        .write_candidates(
            &store,
            vec![candidate("cormex", "project", "uses", "Postgres", None)],
            &ctx(ep1, ts(2026, 6, 1)),
        )
        .await
        .expect("the first should be persisted");
    assert_eq!(
        first.new_entities.len(),
        1,
        "a first appearance should create a new entity"
    );
    let entity = first.new_entities[0];

    // the names differ (exact match is bound to miss), so only vector similarity can merge them
    let ep2 = seed_episode(&store, "Cormex 项目的对象存储用 RustFS").await;
    let second = extractor
        .write_candidates(
            &store,
            vec![candidate(
                "Cormex 项目",
                "project",
                "stores_in",
                "RustFS",
                None,
            )],
            &ctx(ep2, ts(2026, 6, 2)),
        )
        .await
        .expect("the second should be persisted");

    assert!(
        second.new_entities.is_empty(),
        "\"Cormex 项目\" should merge into the existing entity, not spawn another one: {:?}",
        second.new_entities
    );

    let fact = store
        .fact(&second.written[0].to_string())
        .await
        .expect("querying a fact should not fail")
        .expect("the second fact should already be persisted");
    assert_eq!(
        fact.subject_id,
        entity.to_string(),
        "both facts must hang off the same entity, or aliases tear the graph in half"
    );

    // the exact-same-name path has to work as well
    let ep3 = seed_episode(&store, "cormex 的部署目标是 fly.io").await;
    let third = extractor
        .write_candidates(
            &store,
            vec![candidate(
                "cormex",
                "project",
                "deployed_to",
                "fly.io",
                None,
            )],
            &ctx(ep3, ts(2026, 6, 3)),
        )
        .await
        .expect("the third should be persisted");
    assert!(
        third.new_entities.is_empty(),
        "an identically named entity should be reused through the exact-match path"
    );

    db.cleanup().await;
}

#[tokio::test]
async fn duplicate_candidate_is_dropped() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep1 = seed_episode(&store, "cormex 用 Postgres").await;
    extractor
        .write_candidates(
            &store,
            vec![candidate("cormex", "project", "uses", "Postgres", None)],
            &ctx(ep1, ts(2026, 6, 1)),
        )
        .await
        .expect("the first should be persisted");

    // the same thing is mentioned again next turn, differing only in case and whitespace
    let ep2 = seed_episode(&store, "对，我们用的是 postgres").await;
    let again = extractor
        .write_candidates(
            &store,
            vec![candidate("cormex", "project", "uses", "  postgres ", None)],
            &ctx(ep2, ts(2026, 6, 2)),
        )
        .await
        .expect("a duplicate candidate should still return normally");

    assert!(
        again.written.is_empty(),
        "a repeat of the same subject/predicate/object must be dropped — \
         retrieval is top-K, and duplicates squeeze other facts out"
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
        .expect("the first should be persisted");
    let old_fact = first.written[0];
    let subject = first.new_entities[0];

    // same instant, same functional predicate, different values — deterministic
    // logic cannot tell which one is right
    let ep2 = seed_episode(&store, "任务 A 归李四").await;
    let second = extractor
        .write_candidates(
            &store,
            vec![candidate("任务A", "task", "assigned_to", "李四", Some(at))],
            &ctx(ep2, at),
        )
        .await
        .expect("the conflicting candidate should be persisted");

    assert!(
        second.superseded.is_empty(),
        "if the order cannot be told apart, neither should be invalidated: {:?}",
        second.superseded
    );
    assert_eq!(
        second.flagged.len(),
        2,
        "both the old and the new side must be flagged, or the pending-confirmation \
         list only shows one half of the contradiction: {:?}",
        second.flagged
    );

    // a flag is an annotation, it changes no state: both remain active
    let mut active = active_objects(&store, subject, "assigned_to").await;
    active.sort();
    assert_eq!(
        active,
        vec!["张三".to_string(), "李四".to_string()],
        "a flag should not change active_facts"
    );

    let events = store
        .fact_events(&old_fact.to_string())
        .await
        .expect("querying events should not fail");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].op, FactOp::Flag);
    assert!(
        events[0].kind.is_none() && events[0].invalid_at.is_none(),
        "a flag must not carry invalidate-only fields (the schema CHECK stops it too)"
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
        .expect("the tombstone should be persisted");

    // extraction is asynchronous and may well still be in flight when the redact runs
    let report = extractor
        .write_candidates(
            &store,
            vec![candidate("cormex", "project", "uses", "sk-xxx", None)],
            &ctx(ep, ts(2026, 6, 1)),
        )
        .await
        .expect("a redacted source should not raise an error, it just writes nothing");

    assert!(
        report.written.is_empty() && report.new_entities.is_empty(),
        "an in-flight extraction must never write already-redacted content back: {report:?}"
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
                candidate("cormex", "project", "uses", "MySQL", Some(ts(2026, 1, 1))),
                candidate(
                    "cormex",
                    "project",
                    "uses",
                    "Postgres",
                    Some(ts(2026, 6, 1)),
                ),
            ],
            &ctx(ep, ts(2026, 6, 1)),
        )
        .await
        .expect("an intra-batch conflict should be persisted");

    assert_eq!(
        report.written.len(),
        2,
        "both are written; invalidation is expressed by an appended event"
    );
    assert_eq!(
        report.superseded.len(),
        1,
        "mutually exclusive facts inside one batch must be resolved too, or both stay active"
    );

    let subject = report.new_entities[0];
    let active = active_objects(&store, subject, "uses").await;
    assert_eq!(active, vec!["Postgres".to_string()], "actually {active:?}");

    db.cleanup().await;
}

#[tokio::test]
async fn entity_object_becomes_a_graph_edge() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode(&store, "张三负责 cormex").await;
    let mut c = candidate("张三", "person", "owns", "", None);
    c.object = ObjectRef::Entity(EntityRef::new("cormex", "project"));
    c.statement = "张三负责 cormex 项目".to_owned();

    let report = extractor
        .write_candidates(&store, vec![c], &ctx(ep, ts(2026, 6, 1)))
        .await
        .expect("an entity object should be persisted");

    assert_eq!(
        report.new_entities.len(),
        2,
        "subject and object are both new entities"
    );
    let fact = store
        .fact(&report.written[0].to_string())
        .await
        .expect("querying a fact should not fail")
        .expect("the fact should already be persisted");
    assert!(
        fact.object_entity_id.is_some() && fact.object_text.is_none(),
        "a fact pointing at an entity should write object_entity_id rather than object_text — \
         it is an edge in the graph"
    );

    let inbound = store
        .active_facts_by_object(fact.object_entity_id.as_deref().unwrap(), 10)
        .await
        .expect("a reverse one-hop should not fail");
    assert_eq!(
        inbound.len(),
        1,
        "the reverse one-hop of a graph traversal should find this edge"
    );

    db.cleanup().await;
}

/// Extraction quality with a real model. CI has no key, so this is skipped by
/// default; run it by hand locally with
/// `cargo test -p cormex-memory --test extract -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs DEEPSEEK_API_KEY; makes real API calls"]
async fn real_llm_extraction_quality() {
    let _ = dotenvy::dotenv();
    // Skip rather than fail when no provider is configured, mirroring how
    // `setup()` treats a missing DATABASE_URL. `#[ignore]` alone stopped being
    // enough the moment CI started passing `--include-ignored` to reach the
    // database tests -- that flag cannot tell "needs a database CI has" from
    // "needs an API key CI must not have", and the latter costs money.
    let Ok(llm) = cormex_llm::LlmClient::from_env() else {
        eprintln!("skipped: no LLM provider configured (DEEPSEEK_API_KEY and friends)");
        return;
    };
    let extractor = cormex_memory::extract::Extractor::new(
        llm,
        Arc::new(cormex_memory::embed::HashEmbedder::new()),
        DEVICE,
    );

    let episode = "\
用户：早上好！我叫周伟，是 cormex 这个项目的负责人。今天想跟你确认几件事。
助手：早上好！请说。
用户：第一，对象存储我们最后定了用 RustFS，不用 MinIO 了，因为 license 变更那事儿。这个是上周三定的。
第二，帮我看下这段代码：fn main() { println!(\"hi\") } —— 有什么问题吗？
第三，我个人比较讨厌在代码里写英文注释，以后所有注释都写中文。
第四，现在几点了？
助手：现在无法获知当前时间。你这段代码没有问题。关于注释语言，我记下了。";

    let ctx = cormex_memory::extract::ExtractContext::new(Id::new(), ts(2026, 8, 7))
        .with_domain("coding");

    let facts = extractor
        .extract(episode, &ctx)
        .await
        .expect("a real extraction should not fail");

    println!("\n=== extracted {} facts ===", facts.len());
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

    assert!(
        !facts.is_empty(),
        "this conversation plainly contains facts worth recording"
    );
    assert!(
        facts.len() <= 8,
        "extracted {}, far too many — small talk, code snippets and one-off questions \
         should none of them be extracted",
        facts.len()
    );
    let joined: String = facts.iter().map(|f| f.statement.as_str()).collect();
    assert!(
        joined.contains("RustFS"),
        "\"decided on RustFS\" is a textbook thing to extract: {joined}"
    );
    assert!(
        !joined.contains("println"),
        "the code snippet itself has no business in L1: {joined}"
    );
}

/// Runs the whole pipeline against a real model plus a real database, checking
/// the wiring of `extract → resolution → persistence`.
/// Requires `DEEPSEEK_API_KEY` and `DATABASE_URL`.
#[tokio::test]
#[ignore = "needs DEEPSEEK_API_KEY and a database; makes real API calls"]
async fn real_llm_end_to_end_ingest() {
    let _ = dotenvy::dotenv();
    let Some(db) = setup().await else { return };
    let store = db.store.clone();

    // Skip rather than fail when no provider is configured, mirroring how
    // `setup()` treats a missing DATABASE_URL. `#[ignore]` alone stopped being
    // enough the moment CI started passing `--include-ignored` to reach the
    // database tests -- that flag cannot tell "needs a database CI has" from
    // "needs an API key CI must not have", and the latter costs money.
    let Ok(llm) = cormex_llm::LlmClient::from_env() else {
        eprintln!("skipped: no LLM provider configured (DEEPSEEK_API_KEY and friends)");
        return;
    };
    let extractor = cormex_memory::extract::Extractor::new(
        llm,
        Arc::new(cormex_memory::embed::HashEmbedder::new()),
        DEVICE,
    );

    let text = "用户：记一下，cormex 的对象存储决定用 RustFS。另外我叫周伟，是这个项目的负责人。";
    let ep = seed_episode(&store, text).await;
    let report = extractor
        .ingest(&store, text, &ctx(ep, ts(2026, 8, 7)))
        .await
        .expect("the full pipeline should not fail");

    println!("\n=== ingest report ===\n{report:#?}");
    assert!(
        !report.written.is_empty(),
        "at least one fact should have been persisted"
    );

    for id in &report.written {
        let fact = store
            .fact(&id.to_string())
            .await
            .expect("querying a fact should not fail")
            .expect("a fact just written should be findable");
        println!("written: {} | {}", fact.predicate, fact.statement);
        assert!(
            fact.embedding.is_some(),
            "the embedding must be written in the same transaction as the main row, \
             or vector recall misses this one forever"
        );
        assert_eq!(
            fact.source_episode_id,
            ep.to_string(),
            "provenance must stay traceable"
        );
    }

    db.cleanup().await;
}

/// Facts extracted from a turn written back by a sandbox get **demoted one
/// notch**, to tier 3.
///
/// # What this one guards
///
/// What runs inside a sandbox is untrusted code. The `role: user` it writes
/// back was not said by the user, it was said by that container — and the
/// container may already have been steered by something it read itself (MINJA
/// and AgentPoison are both write-side attacks). Without the demotion, a single
/// injected "fact" would be recalled and injected into the system prompt at the
/// **same trust tier as words the user said out loud**, in every future session
/// on every device, long after that container was destroyed.
///
/// None of the four cloud agents has this channel (their sandboxes have no
/// writable long-term memory), so this cell is Cormex-specific — there is no
/// precedent to copy, we have to guard it ourselves.
///
/// **Not "don't write" but "demote"**: the work done inside the sandbox really
/// is worth remembering (the user asked for it in the first place), it just
/// should not stand on equal footing with what the user said out loud.
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
        .expect("the ordinary turn should be persisted");
    let sandboxed = extractor
        .write_candidates(
            &store,
            vec![candidate("beta", "project", "uses", "MySQL", None)],
            &ctx(ep_sbx, ts(2026, 1, 2)).from(cormex_memory::extract::TurnOrigin::Delegated),
        )
        .await
        .expect("the sandboxed turn should be persisted just the same");

    let tier_of = |id: cormex_core::Id| {
        let store = store.clone();
        async move {
            store
                .fact(&id.to_string())
                .await
                .expect("querying a fact should not fail")
                .expect("a fact just written should be findable")
        }
    };
    let normal_fact = tier_of(normal.written[0]).await;
    let sandbox_fact = tier_of(sandboxed.written[0]).await;

    assert_eq!(
        normal_fact.source_channel.as_str(),
        "conversation",
        "what ordinary conversation extracts is still conversation (tier 2); \
         that must not be disturbed"
    );
    assert_eq!(
        sandbox_fact.source_channel.as_str(),
        "tool_output",
        "what a sandbox writes back must drop to tool_output (tier 3) — the same level as a \
         tool trace, because both belong to the loop where the model produces something \
         and then reads it back itself"
    );
    assert!(
        sandbox_fact.trust_tier > normal_fact.trust_tier,
        "a bigger number means less trusted: the sandboxed fact's tier ({:?}) must be \
         strictly higher than the ordinary one's ({:?})",
        sandbox_fact.trust_tier,
        normal_fact.trust_tier
    );
}

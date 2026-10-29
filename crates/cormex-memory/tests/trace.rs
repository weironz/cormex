//! Integration tests for tool-trace extraction: a real database, checking that
//! trace-derived facts really **end up living in the database**.
//!
//! The pure logic of the criteria is already nailed down by the unit tests in
//! `cormex_memory::trace`. This layer answers only the three questions the unit
//! tests cannot:
//!
//! 1. whether the trace is actually **read** (whether `write_candidates` really
//!    gained one more input source);
//! 2. whether `source_channel` / `trust_tier` are right on the persisted rows;
//! 3. whether the habit criterion can accumulate **across episodes** — in the
//!    unit tests `history` is fed in, whereas the real history comes from
//!    `store.recent_tool_calls`.

mod common;

use cormex_core::Id;
use cormex_store::{NewRedaction, RedactionMode, RedactionTarget};

use common::{DEVICE, ctx, offline_extractor, seed_episode, seed_episode_with_tools, setup, ts};

/// The trace from eval item `sc-tool-lock`: two failures then one success,
/// shape copied verbatim from production.
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

async fn statements(store: &cormex_store::Store, ids: &[Id]) -> Vec<String> {
    let mut out = Vec::new();
    for id in ids {
        let f = store
            .fact(&id.to_string())
            .await
            .expect("querying a fact should not fail")
            .expect("a fact just written should be findable");
        out.push(f.statement);
    }
    out
}

/// This test is the minimal evidence for "did the extractor read that table".
///
/// Note the candidate list is **empty** — the conversation itself says not one
/// word about the conclusion; everything produced comes from the trace.
#[tokio::test]
async fn a_turn_with_only_a_tool_trace_still_produces_facts() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode_with_tools(&store, "工作区的测试先跑通再说。", LOCK_TRACE).await;
    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("trace extraction should not fail");

    assert!(
        !report.written.is_empty(),
        "the conversation candidates are empty but the trace holds a success after failure, \
         so the extractor must produce facts; not a single one means it never read \
         episode_tool_calls at all"
    );

    let all = statements(&store, &report.written).await.join("\n");
    assert!(
        all.contains("--locked"),
        "the flag that finally worked must survive verbatim, actually persisted:\n{all}"
    );
    assert!(
        all.contains("cargo update"),
        "the path that got falsified is knowledge too (Manus: keep the failure record, \
         do not scrub it), actually persisted:\n{all}"
    );

    db.cleanup().await;
}

/// Everything after "失败：" is the tool's raw output, and
/// `docs/memory-content.md` §2③ already settled that it is not stored.
///
/// Its counterpart in the eval suite is qt01's `never_extracted`. A copy lives
/// here because the eval has to run the whole suite against a database, whereas
/// this constraint deserves a guard that can go red in a couple of seconds.
#[tokio::test]
async fn raw_tool_output_never_reaches_the_database() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode_with_tools(&store, "跑一下测试。", LOCK_TRACE).await;
    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("trace extraction should not fail");

    let all = statements(&store, &report.written).await.join("\n");
    assert!(
        !all.contains("failed to select a version"),
        "once a command's raw output enters an append-only store there is no getting it \
         back out (redact is the only way out), actually persisted:\n{all}"
    );

    db.cleanup().await;
}

/// Trace-derived facts must be marked `tool_output` (tier 3).
///
/// Getting the direction wrong is harmful: what a tier is for is "the most
/// trusted batch must not be papered over by external content", so lifting the
/// batch the model produced itself and then read back itself up to tier 2
/// amounts to abolishing that barrier.
#[tokio::test]
async fn trace_facts_are_written_as_tool_output_tier_three() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode_with_tools(&store, "跑一下测试。", LOCK_TRACE).await;
    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("trace extraction should not fail");

    for id in &report.written {
        let f = store
            .fact(&id.to_string())
            .await
            .expect("querying a fact should not fail")
            .expect("a fact just written should be findable");
        assert_eq!(
            f.source_channel.as_str(),
            "tool_output",
            "wrong source channel on a trace-derived fact: {}",
            f.statement
        );
        assert_eq!(
            f.trust_tier,
            Some(3),
            "the tier must agree with the channel: {}",
            f.statement
        );
        assert!(
            f.embedding.is_some(),
            "the embedding must be written in the same transaction as the main row, \
             or vector recall misses this one forever: {}",
            f.statement
        );
    }

    db.cleanup().await;
}

/// The habit criterion can only be computed across episodes — that is the sole
/// reason `store.recent_tool_calls` exists, and it is the half the unit tests
/// (where `history` is fed in) cannot cover.
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
            .expect("trace extraction should not fail");
        written_so_far.extend(report.written);

        let all = statements(&store, &written_so_far).await.join("\n");
        let claimed = all.contains("惯例");
        assert_eq!(
            claimed,
            turn >= 3,
            "whether a habit had been declared by turn {turn} = {claimed}, which disagrees \
             with the threshold of 3. Currently in the database:\n{all}"
        );
    }

    db.cleanup().await;
}

/// Once the source is redacted, the trace-derived path must not write a single
/// word either.
///
/// It gets its own test because it goes through a **newly added** input source:
/// the existing guard only covered the candidates handed in by the caller,
/// whereas the trace is something the extractor reads out of the database
/// itself — miss this one line and a single in-flight extraction after a redact
/// can write the commands of the redacted turn straight back in.
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
        .expect("the tombstone should be persisted");

    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("a redacted source should not raise an error, it just writes nothing");

    assert!(
        report.written.is_empty() && report.new_entities.is_empty(),
        "an in-flight extraction must never write back the trace of a redacted turn: {report:?}"
    );

    db.cleanup().await;
}

/// A pure-chat turn should not gain any facts just because this path was added.
#[tokio::test]
async fn a_turn_without_tool_calls_is_unaffected() {
    let Some(db) = setup().await else { return };
    let store = db.store.clone();
    let extractor = offline_extractor();

    let ep = seed_episode(&store, "随便聊两句").await;
    let report = extractor
        .write_candidates(&store, Vec::new(), &ctx(ep, ts(2026, 2, 11)))
        .await
        .expect("empty candidates plus no trace should not fail");

    assert_eq!(
        report.candidates, 0,
        "a turn with no trace should not conjure candidates out of thin air: {report:?}"
    );
    assert!(report.written.is_empty());

    db.cleanup().await;
}

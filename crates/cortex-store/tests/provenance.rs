//! Source trust tiers and derivation provenance: the acceptance tests for
//! migration 20260807000006.
//!
//! What the two promises this group guards have in common is that **getting
//! them wrong is irreversible**:
//!
//! 1. Pre-existing rows get an honest "unknown" rather than a tier that would be
//!    taken at face value at query time. Changing it would take an UPDATE, and
//!    UPDATE is only permitted in redact/purge — meaning that once this value is
//!    written wrong, it stays wrong forever.
//! 2. A derived record must carry its provenance when it is written. Provenance
//!    cannot be reconstructed after the fact (who summarized it, and from what,
//!    is known only to the step that produced it), and a summary without
//!    provenance keeps leaking content after its source is redacted
//!    (docs/memory-content.md §5.3).

mod common;

use cortex_core::Id;
use cortex_store::{
    FactSource, NewSummary, ProvenanceRef, SourceChannel, StoreError, SummaryScope, SyncPayload,
    table,
};

/// The last migration version before the columns were added.
const BEFORE_PROVENANCE: i64 = 20_260_807_000_005;

// ══════════════════════════════════════════════════════════
//  I. Pre-existing rows
// ══════════════════════════════════════════════════════════

/// Pre-existing rows must land on `unknown_legacy` / NULL.
///
/// Why they cannot be given a real tier: by construction every existing fact's
/// `source_episode_id` points at the episode with `role='user'`, while what was
/// fed to the extractor was one whole block of "user: … / assistant: …" glued
/// together — the database holds no trace that could distinguish "the user said
/// it themselves" from "the model inferred it". Labelling them `user_stated`
/// would be inflating out of thin air, and the purpose of tier 1 is precisely
/// "this batch must not be overridden by external content", so labelling in that
/// direction is actively harmful.
///
/// It also pins down a Postgres behavior along the way: after
/// `ADD COLUMN NOT NULL DEFAULT` followed immediately by `DROP DEFAULT`, **the
/// value on pre-existing rows is still there** (PG stores it in attmissingval,
/// and DROP DEFAULT neither backfills nor rewrites the table). The migration
/// relies on this to achieve "pre-existing rows have a value while future writes
/// that forget error out", and that assumption is worth verifying once against a
/// real database.
#[tokio::test]
async fn legacy_facts_get_an_honest_unknown_not_a_fabricated_tier() {
    let Some(db) = common::setup_upto(Some(BEFORE_PROVENANCE)).await else {
        return;
    };

    // Manufacture a fact that "existed before the columns were added". It has
    // to go through raw SQL: the table does not have those two columns yet,
    // while WriteTxn::insert_fact already writes them
    let episode = Id::new();
    let entity = Id::new();
    let fact = Id::new();
    db.exec_raw(format!(
        "INSERT INTO episodes (id, session_id, role, content, device_id, occurred_at)
         VALUES ('{episode}', 's-legacy', 'user', '{{}}'::jsonb, 'dev', now());
         INSERT INTO entities (id, kind, name, embedding_model, device_id)
         VALUES ('{entity}', 'person', '老张', 'bge-m3', 'dev');
         INSERT INTO facts
             (id, subject_id, predicate, object_text, statement, embedding_model,
              source_episode_id, extracted_by, device_id)
         VALUES ('{fact}', '{entity}', 'uses', 'Postgres', '老张用 Postgres',
                 'bge-m3', '{episode}', 'legacy-extractor', 'dev')"
    ))
    .await;

    // Apply the remaining migrations -- namely the one under test
    db.store
        .migrate()
        .await
        .expect("applying the remaining migrations should succeed");

    let row = db
        .store
        .fact(&fact.to_string())
        .await
        .expect("the query should not fail")
        .expect("the pre-existing fact should still be there");

    assert_eq!(
        row.source_channel,
        SourceChannel::UnknownLegacy,
        "the source of a pre-existing row cannot be reconstructed after the fact and can only be \
         labelled \"unknown\". If this turned into some real tier, the migration is fabricating \
         provenance -- and this value cannot be changed afterwards (changing it takes an UPDATE, \
         which only redact/purge permits)"
    );
    assert_eq!(
        row.trust_tier, None,
        "\"unknown\" is not a point on the trust scale. Giving it a sentinel tier (0 or 5) would \
         make any `WHERE trust_tier <= n` quietly include or exclude it depending on which end \
         the sentinel sits at; NULL's contagiousness in SQL instead makes any query that forgot \
         to handle it return empty"
    );

    db.cleanup().await;
}

/// After the columns exist, an INSERT that forgets to declare a source must
/// **error on the spot**.
///
/// The migration does `DROP DEFAULT` immediately after
/// `ADD COLUMN ... DEFAULT` for exactly this reason: if the default stayed,
/// every future fact that forgot to write its source would silently become a new
/// "unknown" — turning the one-off historical debt of the pre-existing rows into
/// a debt that keeps growing.
#[tokio::test]
async fn new_facts_cannot_silently_become_unknown() {
    let Some(db) = common::setup().await else {
        return;
    };

    let episode = Id::new();
    let entity = Id::new();
    db.exec_raw(format!(
        "INSERT INTO episodes (id, session_id, role, content, device_id, occurred_at)
         VALUES ('{episode}', 's', 'user', '{{}}'::jsonb, 'dev', now());
         INSERT INTO entities (id, kind, name, embedding_model, device_id)
         VALUES ('{entity}', 'person', '老张', 'bge-m3', 'dev')"
    ))
    .await;

    let omitted = raw_insert_fact(&db, &entity.to_string(), &episode.to_string(), None).await;
    assert!(
        omitted.is_err(),
        "an INSERT that omits source_channel must fail (NOT NULL, and the DEFAULT has been \
         dropped). If it can succeed, the default is still there and every future fact that \
         forgets its source will silently become \"unknown\""
    );

    // A channel that disagrees with the tier must also be stopped: the two
    // describe the same thing, and allowing them to disagree allows rows where
    // "the channel says web page while the tier says the user said it in
    // person" -- rows that only surface during an actual security incident,
    // when you need to invalidate in bulk by source
    let mismatched = raw_insert_fact(
        &db,
        &entity.to_string(),
        &episode.to_string(),
        Some(("external", "1")),
    )
    .await;
    assert!(
        mismatched.is_err(),
        "the external channel paired with tier 1 must be rejected by \
         facts_trust_tier_matches_channel"
    );

    // And the other way round: the legacy channel must not carry a tier, or
    // "unknown" gets mixed into the scale
    let legacy_with_tier = raw_insert_fact(
        &db,
        &entity.to_string(),
        &episode.to_string(),
        Some(("unknown_legacy", "2")),
    )
    .await;
    assert!(
        legacy_with_tier.is_err(),
        "unknown_legacy must be paired with NULL -- giving it a tier is treating \"unknown\" as \
         \"known\""
    );

    db.cleanup().await;
}

/// The channel and tier persisted by the normal write path must line up, and
/// must make it out over sync.
#[tokio::test]
async fn written_facts_carry_channel_and_tier_all_the_way_to_sync() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "我们改用 Postgres");
    let entity = common::new_entity("person", "老张");
    let mut fact = common::new_fact(entity.id, "uses", "Postgres", episode.id);
    fact.source = FactSource::External;

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&fact).await
        })
        .await
        .expect("the write transaction should not fail");

    let row = store
        .fact(&fact.id.to_string())
        .await
        .expect("the query should not fail")
        .expect("the fact just written should be findable");
    assert_eq!(row.source_channel, SourceChannel::External);
    assert_eq!(
        row.trust_tier,
        Some(FactSource::External.trust_tier()),
        "the tier is computed from the channel; a caller neither should nor can specify it \
         separately"
    );

    // The delivery path must carry them too. Without the source, a client
    // cannot act on things in bulk by source locally -- and "invalidate in bulk
    // by source" is the third reason these two columns exist
    let synced = store
        .fetch_since(0, 100)
        .await
        .expect("the sync pull should not fail");
    let payload = synced
        .iter()
        .find(|r| r.table_name == table::FACTS)
        .and_then(|r| r.payload.clone())
        .expect("the facts row should appear in the sync stream");
    match payload {
        SyncPayload::Fact(f) => {
            assert_eq!(f.source_channel, SourceChannel::External);
            assert_eq!(f.trust_tier, Some(4));
        }
        other => panic!("the facts log row should carry a Fact payload; it carried {other:?}"),
    }

    db.cleanup().await;
}

// ══════════════════════════════════════════════════════════
//  II. Derivation provenance
// ══════════════════════════════════════════════════════════

/// A summary with no sources **cannot be written**, and leaves not half a
/// record behind.
///
/// This is not ordinary argument validation: a summary has no
/// `source_episode_id NOT NULL` column to fall back on, and its provenance lives
/// entirely in `derivations`. Allowing it to be written first and have its edges
/// added later is allowing a summary whose source can never be found — one that
/// keeps leaking content after its source is redacted, while GDPR Art. 17's
/// erasure obligation extends to derived data.
#[tokio::test]
async fn a_summary_without_provenance_is_refused() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let orphan = new_summary(Vec::new());
    let err = store
        .write_txn(async |tx| tx.insert_summary(&orphan).await)
        .await
        .expect_err("a summary with no sources must be rejected");
    assert!(
        matches!(err, StoreError::MissingProvenance { .. }),
        "it should be MissingProvenance rather than some other error; got: {err}"
    );

    assert_eq!(
        store
            .summaries_for_scope(SummaryScope::Session, "sess-a", 10)
            .await
            .expect("the query should not fail")
            .len(),
        0,
        "a rejected summary must not leave half a record behind"
    );

    db.cleanup().await;
}

/// A summary and its provenance edges are persisted in the same transaction,
/// and both enter the sync stream.
#[tokio::test]
async fn a_summary_writes_its_lineage_in_the_same_transaction() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let a = common::new_episode("sess-a", "第一轮");
    let b = common::new_episode("sess-a", "第二轮");
    store
        .write_txn(async |tx| {
            tx.insert_episode(&a).await?;
            tx.insert_episode(&b).await
        })
        .await
        .expect("the write transaction should not fail");

    let summary = new_summary(vec![
        ProvenanceRef::episode(a.id),
        ProvenanceRef::episode(b.id),
    ]);
    store
        .write_txn(async |tx| tx.insert_summary(&summary).await)
        .await
        .expect("the write transaction should not fail");

    let records = store
        .fetch_since(0, 100)
        .await
        .expect("the sync pull should not fail");
    let edges: Vec<_> = records
        .iter()
        .filter_map(|r| match r.payload.as_ref() {
            Some(SyncPayload::Derivation(d)) => Some(d.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        edges.len(),
        2,
        "two sources should mean two provenance edges -- one edge short is one path redact can \
         no longer follow"
    );
    for edge in &edges {
        assert_eq!(edge.derived_id, summary.id.to_string());
    }
    let mut sources: Vec<&str> = edges.iter().map(|e| e.source_id.as_str()).collect();
    sources.sort_unstable();
    let mut want = [a.id.to_string(), b.id.to_string()];
    want.sort_unstable();
    assert_eq!(sources, want.iter().map(String::as_str).collect::<Vec<_>>());

    // The provenance edges must come **after** the summary row: a client
    // replays in sync_log order, and an edge arriving first would point at a
    // summary that does not exist yet
    let summary_seq = seq_of(&records, table::SUMMARIES);
    let first_edge_seq = seq_of(&records, table::DERIVATIONS);
    assert!(
        summary_seq < first_edge_seq,
        "the summary row (seq {summary_seq}) should precede its provenance edges \
         (seq {first_edge_seq})"
    );

    db.cleanup().await;
}

/// A multi-source derived fact (crosslink's cross-domain edge) must record both
/// ends.
///
/// Before this, such an edge recorded only the left-hand fact's
/// `source_episode_id`: redacting only the right-hand one left this edge alive
/// carrying the right-hand entity's name, with no query able to find it.
#[tokio::test]
async fn a_multi_source_fact_records_every_source() {
    let Some(db) = common::setup().await else {
        return;
    };
    let store = &db.store;

    let episode = common::new_episode("s1", "两个领域撞上了");
    let entity = common::new_entity("person", "老张");
    let left = common::new_fact(entity.id, "uses", "Postgres", episode.id);
    let right = common::new_fact(entity.id, "owns", "cortex", episode.id);

    let mut edge = common::new_fact(entity.id, "relates_to", "跨域", episode.id);
    edge.source = FactSource::Derived;
    edge.derived_from = vec![ProvenanceRef::fact(left.id), ProvenanceRef::fact(right.id)];

    store
        .write_txn(async |tx| {
            tx.insert_episode(&episode).await?;
            tx.insert_entity(&entity).await?;
            tx.insert_fact(&left).await?;
            tx.insert_fact(&right).await?;
            tx.insert_fact(&edge).await
        })
        .await
        .expect("the write transaction should not fail");

    let records = store
        .fetch_since(0, 200)
        .await
        .expect("the sync pull should not fail");
    let edges: Vec<_> = records
        .iter()
        .filter_map(|r| match r.payload.as_ref() {
            Some(SyncPayload::Derivation(d)) => Some(d.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        edges.len(),
        2,
        "single-source facts (left / right) should write no provenance edges -- their provenance \
         is source_episode_id; the multi-source edge should write two. It wrote {}",
        edges.len()
    );
    for e in &edges {
        assert_eq!(
            e.derived_id,
            edge.id.to_string(),
            "only the cross-domain edge is a multi-source derived record"
        );
    }

    db.cleanup().await;
}

// ══════════════════════════════════════════════════════════
//  Small helpers
// ══════════════════════════════════════════════════════════

fn new_summary(sources: Vec<ProvenanceRef>) -> NewSummary {
    NewSummary {
        id: Id::new(),
        scope: SummaryScope::Session,
        scope_key: "sess-a".to_owned(),
        text: "这场会话讨论了存储层".to_owned(),
        tsv_source: Some("这场 会话 讨论 存储层".to_owned()),
        embedding: None,
        embedding_model: common::MODEL.to_owned(),
        covers_from: None,
        covers_to: None,
        sources,
        device_id: common::DEVICE.to_owned(),
    }
}

fn seq_of(records: &[cortex_store::SyncRecord], table_name: &str) -> i64 {
    records
        .iter()
        .find(|r| r.table_name == table_name)
        .unwrap_or_else(|| panic!("the sync stream should contain a {table_name} log row"))
        .seq
}

/// Insert a fact directly, bypassing [`cortex_store::WriteTxn`], to construct
/// bad rows the normal path cannot produce.
///
/// Returning `Err` means the database refused it — which is exactly what these
/// assertions want to see.
async fn raw_insert_fact(
    db: &common::TestDb,
    entity: &str,
    episode: &str,
    channel_and_tier: Option<(&str, &str)>,
) -> Result<(), String> {
    let id = Id::new();
    let (cols, vals) = match channel_and_tier {
        None => (String::new(), String::new()),
        Some((channel, tier)) => (
            ", source_channel, trust_tier".to_owned(),
            format!(", '{channel}', {tier}"),
        ),
    };
    let sql = format!(
        "INSERT INTO facts
             (id, subject_id, predicate, object_text, statement, embedding_model,
              source_episode_id, extracted_by, device_id{cols})
         VALUES ('{id}', '{entity}', 'uses', 'x', 'x', 'bge-m3',
                 '{episode}', 'raw', 'dev'{vals})"
    );
    db.try_exec_raw(sql).await
}

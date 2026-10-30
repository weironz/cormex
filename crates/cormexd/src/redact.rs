//! The redact / purge executor: the one place in this system where `UPDATE` and
//! `DELETE` are permitted.
//!
//! Design: `docs/memory.md` §十一.
//!
//! # Why this exists, and why it did not until now
//!
//! The tombstone writer (`WriteTxn::insert_redaction`) has been there since the
//! first migration, and its own doc comment says "the actual cascading purge is
//! the redact executor's job". **That job did not exist.** So a `redactions` row
//! marked content as erased while the content was still there, and the promise
//! in `docs/memory.md` §十一 was false.
//!
//! This module is that job.
//!
//! # The cascade, and why each landing point is in it
//!
//! A secret pasted into a conversation ends up in more places than the
//! conversation:
//!
//! | Landing point | Why it has to go |
//! |---|---|
//! | `episodes.content` / `text` / `tsv` | the original |
//! | `facts.statement` / `tsv` / `embedding` | a paraphrase is still the secret; **the vector can be inverted approximately**, so it must go too |
//! | `blob_transcripts` | a caption or a transcript of the same content |
//! | `summaries` derived from it | a summary of the secret is the secret |
//! | `archive_chunks` / `transcript_events` | the archive's own copies |
//!
//! Missing any one of them makes the promise a lie, and **the failure is
//! silent**: the content is gone from the place you looked and still present in
//! the place you did not.
//!
//! # Ordering: content first, tombstone last
//!
//! Same argument as the archive purge (P6-2). A crash after the tombstone but
//! before the cascade leaves content that looks erased and is not — the worst
//! state, because nobody looks again. A crash before the tombstone leaves
//! content partly gone and a row that still says "present", which the next
//! attempt (or an operator) can see.
//!
//! # Idempotent
//!
//! Every step is a `WHERE … = …` that a second run matches nothing in. Running
//! the cascade twice is safe, which is what makes "resume after a crash" a
//! matter of running it again.
//!
//! # What it does not touch
//!
//! The `redactions` row itself. "Something was erased here, when, by whom" is
//! permanent — the content gives way, the audit chain does not.

use anyhow::{Context as _, Result};
use chrono::Utc;
use cormex_core::Id;
use cormex_store::{Actor, NewFactEvent, RedactionMode, RedactionTarget, Store, StoreError};

use crate::request_tenant::Tenant;
use crate::state::AppState;

/// What one redact did, per landing point.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactReport {
    pub episodes_cleared: usize,
    pub facts_cleared: usize,
    pub facts_invalidated: usize,
    pub blob_transcripts_cleared: usize,
    pub summaries_deleted: usize,
    /// Sessions whose distiller progress was rewound, so the next round
    /// rebuilds their summaries from what is left.
    pub sessions_rewound: usize,
    pub transcript_events_cleared: usize,
    pub archive_chunks_deleted: usize,
    /// Blobs whose objects were deleted from storage. Purge only.
    pub blobs_deleted: usize,
}

/// Redact one episode: clear every landing point, keep the tombstone.
///
/// # Errors
/// A database write failed. The tombstone is written last, so a failure here
/// leaves the episode listed as present and the cascade resumable.
pub async fn redact_episode(
    st: &AppState,
    tenant: &Tenant,
    episode_id: &str,
    mode: RedactionMode,
    reason: &str,
) -> Result<RedactReport> {
    let store = tenant.store()?;
    // Validated before anything is written: a malformed id would otherwise
    // fail halfway through the cascade, leaving the episode partly cleared
    episode_id
        .parse::<Id>()
        .map_err(|_| anyhow::anyhow!("{episode_id} is not a ULID"))?;

    // ── The episode itself ──
    //
    // `content` gets a placeholder rather than being deleted: the row is the
    // identity every `fact.source_episode_id` points at, and removing it would
    // turn a cascade into a dangling reference
    let mut report = RedactReport {
        episodes_cleared: store
            .redact_episode_content(episode_id)
            .await
            .context("clearing the episode failed")?,
        ..RedactReport::default()
    };

    // ── Facts derived from it ──
    //
    // Two things per fact: the content, and an `invalidate` event. The second
    // is not decoration — `active_facts` reads the latest `fact_events` row, so
    // a fact whose statement was blanked but which is still "valid" keeps
    // occupying a slot in retrieval, and its id keeps appearing in results
    let facts = store.facts_from_episode(episode_id).await?;
    for fact_id in &facts {
        report.facts_cleared += store.redact_fact_content(fact_id).await?;
        let event = NewFactEvent::retracted(
            fact_id
                .parse()
                .map_err(|_| anyhow::anyhow!("a fact id from the database is not a ULID"))?,
            Utc::now(),
            format!("its source episode was redacted: {reason}"),
            "server",
        );
        store
            .write_txn(async |t| t.insert_fact_event(&event).await)
            .await
            .context("invalidating a derived fact failed")?;
        report.facts_invalidated += 1;
    }

    // ── Blob transcripts of blobs attached to it ──
    for blob_hash in store.blobs_of_episode(episode_id).await? {
        report.blob_transcripts_cleared += store.redact_blob_transcript(&blob_hash).await?;
        if mode == RedactionMode::Purge {
            // **Only when nothing else references it.** Content addressing means
            // one blob may be attached to several episodes, and deleting the
            // object would break the others
            if !store
                .blob_referenced_elsewhere(&blob_hash, episode_id)
                .await?
                && let Ok(key) = store.blob_storage_key(&blob_hash).await
                && st.delete_capture_object(&key).await.is_ok()
            {
                report.blobs_deleted += 1;
            }
        }
    }

    // ── Summaries derived from it ──
    //
    // **Deleted, not blanked.** A summary is a paraphrase, and a paraphrase of a
    // secret is the secret.
    //
    // The design says "delete and regenerate excluding the redacted episode".
    // Deleting is here; **regenerating is the next step below**, and it has to
    // be: a summary that covers only the innocent half of a conversation is
    // still worth having, and leaving it deleted forever loses it.
    let summaries = store
        .summaries_derived_from(RedactionTarget::Episode, episode_id)
        .await?;
    let mut affected_sessions: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for summary_id in &summaries {
        // The session a summary belongs to is its `scope_key`; that is what the
        // distiller will rebuild from
        if let Some(summary) = store.summary(summary_id).await?
            && summary.scope == cormex_store::SummaryScope::Session
        {
            affected_sessions.insert(summary.scope_key);
        }
        report.summaries_deleted += store.delete_summary(summary_id).await?;
    }

    // ── The archive's copies ──
    //
    // A promoted episode is an `episodes` row whose `content` carries
    // `transcript_event_id`; its text is the conversation. The events
    // themselves are cleared so the archive index and the conversation index
    // both stop returning it
    let cleared = store
        .redact_transcript_events_of_episode(episode_id)
        .await
        .context("clearing the archive's copies failed")?;
    report.transcript_events_cleared = cleared.events;
    report.archive_chunks_deleted = cleared.chunks;

    // ── Regenerate what the deletion took ──
    //
    // **Rewind the distiller's progress**, so its next round re-reads this
    // session from the start. That is the regeneration: the summaries are
    // rebuilt from what is left of the events (the redacted one is now empty),
    // and the innocent content in the same session comes back.
    //
    // Why rewinding rather than regenerating here: the distiller is the only
    // thing that knows how to turn events into observations, and a second
    // implementation inside the redact cascade would be a second thing to keep
    // in step — with the failure being "the regenerated summary reads
    // differently from the original", which nobody would notice.
    //
    // Rewinding is safe because a summary's id is derived from its sources: the
    // ones that were **not** deleted come back with the same id and collide on
    // the primary key, which `insert_summary`'s caller treats as a no-op.
    for session_id in &affected_sessions {
        // **Only a real capture session can be rewound.** The progress table
        // has a foreign key onto `capture_sessions(id)`, and a summary's
        // `scope_key` is usually — but not always — one of those
        if !store.is_capture_session(session_id).await? {
            tracing::debug!(
                scope_key = %session_id,
                "a summary's scope is not a capture session; nothing to rewind"
            );
            continue;
        }
        store
            .archive_txn(async |t| t.insert_distill_progress(session_id, -1).await)
            .await
            .context("rewinding the distiller failed")?;
        report.sessions_rewound += 1;
    }

    // ── The tombstone, last ──
    let tombstone = cormex_store::NewRedaction {
        id: Id::new(),
        target_kind: RedactionTarget::Episode,
        target_id: episode_id.to_owned(),
        mode,
        reason: reason.to_owned(),
        actor: Actor::User.to_string(),
        device_id: "server".to_owned(),
    };
    store
        .write_txn(async |t| t.insert_redaction(&tombstone).await)
        .await
        .context("the cascade completed but its tombstone could not be written")?;

    Ok(report)
}

/// Redact one blob: clear its transcripts, and purge the object when asked.
///
/// # Errors
/// A database write failed.
pub async fn redact_blob(
    st: &AppState,
    tenant: &Tenant,
    blob_hash: &str,
    mode: RedactionMode,
    reason: &str,
) -> Result<RedactReport> {
    let store = tenant.store()?;

    let mut report = RedactReport {
        blob_transcripts_cleared: store
            .redact_blob_transcript(blob_hash)
            .await
            .context("clearing the blob's transcripts failed")?,
        ..RedactReport::default()
    };

    if mode == RedactionMode::Purge
        && !store.blob_referenced_anywhere(blob_hash).await?
        && let Ok(key) = store.blob_storage_key(blob_hash).await
        && st.delete_capture_object(&key).await.is_ok()
    {
        report.blobs_deleted += 1;
    }

    let tombstone = cormex_store::NewRedaction {
        id: Id::new(),
        target_kind: RedactionTarget::Blob,
        target_id: blob_hash.to_owned(),
        mode,
        reason: reason.to_owned(),
        actor: Actor::User.to_string(),
        device_id: "server".to_owned(),
    };
    store
        .write_txn(async |t| t.insert_redaction(&tombstone).await)
        .await
        .context("the cascade completed but its tombstone could not be written")?;

    Ok(report)
}

/// Whether a target already carries a tombstone of this mode or stronger.
///
/// A `purge` supersedes a `redact`; a second `redact` is a no-op. The caller
/// uses this to answer "already done" rather than re-running the cascade on
/// every request.
///
/// # Errors
/// The query failed.
pub async fn already_redacted(
    store: &Store,
    target: RedactionTarget,
    target_id: &str,
    mode: RedactionMode,
) -> Result<bool> {
    let rows = store.redactions_for_target(target, target_id).await?;
    Ok(rows.iter().any(|r| match mode {
        RedactionMode::Redact => true,
        RedactionMode::Purge => r.mode == RedactionMode::Purge,
    }))
}

/// Convert a store error into one that says what went wrong.
///
/// Kept for the call sites that want the message rather than the variant.
#[must_use]
pub fn describe(err: &StoreError) -> String {
    err.to_string()
}

#[cfg(test)]
mod tests {
    //! The cascade, against a real database.
    //!
    //! What these measure is the part that is silent when it is wrong: a
    //! landing point the cascade forgot still holds the content, and nothing
    //! fails — the content is gone from the place you looked and present in the
    //! place you did not.

    use super::*;
    use cormex_store::{
        FactSource, NewEntity, NewEpisode, NewFact, NewSummary, ProvenanceRef, Role, SourceKind,
        SummaryScope, Vector,
    };

    /// One episode, one fact from it, one summary derived from it, one
    /// transcript event.
    ///
    /// The session id is a real `capture_sessions` row: the distiller's
    /// progress table has a foreign key onto it, and a made-up string would
    /// make the rewind untestable.
    async fn seed(store: &Store) -> (String, String, String, String) {
        let session_id = Id::new().to_string();
        store
            .archive_txn(async |t| {
                t.upsert_capture_session(&cormex_store::NewCaptureSession {
                    id: session_id.parse().expect("a ULID"),
                    agent: "claude_code".into(),
                    native_session_id: format!("native-{session_id}"),
                    parent_session_id: None,
                })
                .await
            })
            .await
            .expect("a capture session");

        let episode = NewEpisode {
            id: Id::new(),
            session_id: session_id.clone(),
            role: Role::User,
            content: serde_json::json!({"role":"user","text":"my key is AKIAIOSFODNN7EXAMPLE"}),
            text: Some("my key is AKIAIOSFODNN7EXAMPLE".into()),
            tsv_source: Some("my key is AKIAIOSFODNN7EXAMPLE".into()),
            domain: None,
            device_id: "test".into(),
            occurred_at: Utc::now(),
        };
        let episode_id = episode.id;
        store
            .write_txn(async |t| t.insert_episode(&episode).await)
            .await
            .expect("episode");

        let entity = NewEntity {
            id: Id::new(),
            kind: "concept".into(),
            name: "RustFS".into(),
            summary: None,
            embedding: Some(Vector::from(vec![0.1f32; 1024])),
            embedding_model: "test".into(),
            device_id: "test".into(),
        };
        let entity_id = entity.id;
        let fact = NewFact {
            id: Id::new(),
            subject_id: entity_id,
            predicate: "uses".into(),
            object_text: Some("RustFS".into()),
            object_entity_id: None,
            statement: "cormex 使用 RustFS".into(),
            tsv_source: Some("cormex 使用 RustFS".into()),
            embedding: Some(Vector::from(vec![0.1f32; 1024])),
            embedding_model: "test".into(),
            domain: None,
            confidence: 1.0,
            valid_at: None,
            source_episode_id: episode_id,
            source: FactSource::Conversation,
            derived_from: Vec::new(),
            extracted_by: "test".into(),
            device_id: "test".into(),
        };
        let fact_id = fact.id;
        store
            .write_txn(async |t| {
                t.insert_entity(&entity).await?;
                t.insert_fact(&fact).await
            })
            .await
            .expect("fact");

        let summary = NewSummary {
            id: Id::new(),
            scope: SummaryScope::Session,
            scope_key: session_id.clone(),
            text: "the user pasted a key".into(),
            tsv_source: Some("the user pasted a key".into()),
            embedding: None,
            embedding_model: "none".into(),
            covers_from: None,
            covers_to: None,
            sources: vec![ProvenanceRef {
                kind: SourceKind::Episode,
                id: episode_id,
            }],
            device_id: "test".into(),
        };
        let summary_id = summary.id;
        store
            .write_txn(async |t| t.insert_summary(&summary).await)
            .await
            .expect("summary");

        (
            episode_id.to_string(),
            fact_id.to_string(),
            summary_id.to_string(),
            session_id,
        )
    }

    /// Every landing point is cleared, and the tombstone is written.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn every_landing_point_is_cleared() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt) = crate::test_support::live_state(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (episode_id, fact_id, summary_id, _) = seed(&store).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let report = redact_episode(
            &st,
            &tenant,
            &episode_id,
            RedactionMode::Redact,
            "a key was pasted",
        )
        .await
        .expect("redact");

        assert_eq!(report.episodes_cleared, 1);
        assert_eq!(report.facts_cleared, 1);
        assert_eq!(report.facts_invalidated, 1);
        assert_eq!(report.summaries_deleted, 1);

        // The episode's content is the placeholder
        let episode = store.episode(&episode_id).await.expect("q").expect("row");
        assert_eq!(episode.content, serde_json::json!({"redacted": true}));
        assert!(episode.text.is_none());

        // The fact's content and vector are gone
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        let (statement, has_vec, object): (String, bool, Option<String>) = sqlx::query_as(
            "SELECT statement, embedding IS NOT NULL, object_text FROM facts WHERE id = $1",
        )
        .bind(&fact_id)
        .fetch_one(&pool)
        .await
        .expect("fact row");
        assert_eq!(statement, "[redacted]");
        assert!(!has_vec, "the embedding survived");
        assert_eq!(
            object.as_deref(),
            Some("[redacted]"),
            "the object text survived"
        );

        // The fact is out of retrieval
        let active: (bool,) =
            sqlx::query_as("SELECT EXISTS(SELECT 1 FROM active_facts WHERE id = $1)")
                .bind(&fact_id)
                .fetch_one(&pool)
                .await
                .expect("active_facts");
        assert!(!active.0, "the fact is still in active_facts");

        // The summary is gone
        let gone: (bool,) =
            sqlx::query_as("SELECT NOT EXISTS(SELECT 1 FROM summaries WHERE id = $1)")
                .bind(&summary_id)
                .fetch_one(&pool)
                .await
                .expect("summary");
        assert!(gone.0, "the derived summary survived");
        pool.close().await;

        // The tombstone is there
        assert!(
            already_redacted(
                &store,
                RedactionTarget::Episode,
                &episode_id,
                RedactionMode::Redact
            )
            .await
            .expect("check"),
            "no tombstone"
        );

        db.drop().await;
    }

    /// Running the cascade twice is safe.
    ///
    /// Every step matches nothing the second time, which is what makes "resume
    /// after a crash" mean "run it again".
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_cascade_is_idempotent() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt) = crate::test_support::live_state(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (episode_id, _, _, _) = seed(&store).await;
        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };

        let first = redact_episode(&st, &tenant, &episode_id, RedactionMode::Redact, "first")
            .await
            .expect("first");
        assert_eq!(first.episodes_cleared, 1);

        let second = redact_episode(&st, &tenant, &episode_id, RedactionMode::Redact, "second")
            .await
            .expect("second");
        assert_eq!(
            second.episodes_cleared, 0,
            "the second run cleared the episode again"
        );
        assert_eq!(
            second.facts_cleared, 0,
            "the second run cleared a fact again"
        );

        db.drop().await;
    }

    /// A fact whose object is an entity keeps its `object_entity_id`.
    ///
    /// The table's CHECK is `object_text IS NOT NULL OR object_entity_id IS NOT
    /// NULL`; blanking `object_text` on a literal-object fact would violate it
    /// and abort the whole cascade. The placeholder keeps both cases legal.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_literal_object_fact_stays_legal_after_redaction() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt) = crate::test_support::live_state(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (episode_id, _, _, _) = seed(&store).await;
        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };

        // The seed's fact has a literal object, so this is exactly the case
        let result = redact_episode(
            &st,
            &tenant,
            &episode_id,
            RedactionMode::Redact,
            "literal object",
        )
        .await;
        assert!(
            result.is_ok(),
            "the cascade aborted on the facts CHECK: {result:?}"
        );

        db.drop().await;
    }

    /// The summaries come back on the distiller's next round.
    ///
    /// The design says "delete and regenerate excluding the redacted episode".
    /// Deleting alone loses the innocent half of a conversation forever; the
    /// regeneration is a **rewind of the distiller's progress**, so its next
    /// round rebuilds the session's summaries from what is left.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_summaries_are_regenerated_by_the_next_distill_round() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt) = crate::test_support::live_state(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (episode_id, _, summary_id, scope_key) = seed(&store).await;

        // The distiller's progress says "already done", so its next round would
        // skip this session entirely without the rewind
        store
            .archive_txn(async |t| t.insert_distill_progress(&scope_key, 0).await)
            .await
            .expect("progress");

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let report = redact_episode(
            &st,
            &tenant,
            &episode_id,
            RedactionMode::Redact,
            "regenerate",
        )
        .await
        .expect("redact");

        assert_eq!(report.summaries_deleted, 1);
        assert_eq!(
            report.sessions_rewound, 1,
            "the distiller's progress was not rewound, so the summary is gone for good"
        );

        // The progress now says "nothing done yet", which is what makes the
        // next round rebuild
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        let through: i64 = sqlx::query_scalar(
            "SELECT through_seq FROM distill_progress
              WHERE session_id::text = $1
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(&scope_key)
        .fetch_one(&pool)
        .await
        .expect("progress row");
        pool.close().await;
        assert_eq!(
            through, -1,
            "the distiller would skip this session: its progress still says it is done"
        );

        // And the summary really is gone until then
        let gone: bool = store.summary(&summary_id).await.expect("q").is_none();
        assert!(gone, "the redacted summary survived");

        db.drop().await;
    }

    /// A session with no derived summary is not rewound.
    ///
    /// Rewinding every session a redact touches would make the distiller
    /// re-read conversations that lost nothing, which costs a model call per
    /// session for no reason.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_session_with_no_summary_is_not_rewound() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt) = crate::test_support::live_state(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (episode_id, _, _, _) = seed(&store).await;

        // Remove the summary so nothing is derived from this episode
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        sqlx::raw_sql("DELETE FROM derivations; DELETE FROM summaries;")
            .execute(&pool)
            .await
            .expect("clear");
        pool.close().await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let report = redact_episode(
            &st,
            &tenant,
            &episode_id,
            RedactionMode::Redact,
            "no summary",
        )
        .await
        .expect("redact");
        assert_eq!(
            report.sessions_rewound, 0,
            "a session that lost no summary was rewound"
        );

        db.drop().await;
    }
}

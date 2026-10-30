//! Purging a captured session: the R layer, the P layer, and what was derived
//! from them (P6-2).
//!
//! # Why this is the one place DELETE is allowed
//!
//! `docs/memory.md` §十一: the whole system is append-only, and the sole
//! exception is the user's right to dispose of their own data. The tombstone
//! stays — "something was erased here, then, by whom" is permanent — but the
//! **content** goes.
//!
//! # The order is not arbitrary
//!
//! ```text
//! 1. destroy the data keys   → the objects become random bytes immediately
//! 2. delete the objects      → the bytes themselves go
//! 3. delete the P layer      → events, projections, index chunks
//! 4. delete the derived rows → summaries, derivations, promoted episodes
//! 5. write the tombstone     → last, so a crash mid-way leaves a resumable state
//! ```
//!
//! Key first, tombstone last. If the tombstone were written first and the
//! process died, the session would look erased while its bytes were still
//! readable — the worst possible state, because nobody would ever look again.
//! With the tombstone last, a crash leaves a session that is partly gone and
//! **still listed as present**, so the next attempt (or the operator) sees it.
//!
//! # What this does not do
//!
//! It does not touch the memory layer's `episodes` / `facts` cascade. Those rows
//! are found through the promoted episodes' `transcript_event_id`, and clearing
//! them is the memory side's redact executor — which **does not exist yet**
//! (only its tombstone writer does). This module deletes the promoted episodes
//! themselves and leaves their `facts` to that executor; a fact extracted from a
//! purged session is still readable until it runs. That gap is stated in the
//! PR and in `docs/operations.md` rather than hidden behind a comment here.

use anyhow::{Context as _, Result};
use cormex_store::{NewRedaction, RedactionMode, RedactionTarget, Store};

use crate::request_tenant::Tenant;
use crate::state::AppState;

/// What a purge did, so the caller can report it rather than guess.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// Data keys destroyed. Their chunks are unrecoverable from this moment on.
    pub keys_destroyed: usize,
    /// Objects deleted from storage.
    pub objects_deleted: usize,
    /// Objects whose deletion failed. **Not silently ignored**: an object that
    /// survives a purge is exactly the thing this operation exists to prevent.
    pub objects_failed: usize,
    pub events_deleted: usize,
    pub chunks_deleted: usize,
    /// Promoted episodes whose content was blanked and whose facts were
    /// invalidated. **Not deleted** — see the comment at the call site.
    pub promoted_episodes_redacted: usize,
    /// Facts whose statement / vector were cleared.
    pub facts_redacted: usize,
    pub summaries_deleted: usize,
}

/// Purge one captured session.
///
/// # Errors
/// The session does not exist, or a database write failed. An object-storage
/// failure is **counted, not propagated**: one unreachable object must not stop
/// the rest of the purge, and the caller reports the count.
pub async fn purge_session(
    st: &AppState,
    tenant: &Tenant,
    session_id: &str,
) -> Result<PurgeReport> {
    let store = tenant.store()?;
    purge(st, tenant, store, session_id, None).await
}

/// Purge one captured file.
///
/// # Errors
/// The file does not exist, or a database write failed.
pub async fn purge_file(st: &AppState, tenant: &Tenant, file_id: &str) -> Result<PurgeReport> {
    let store = tenant.store()?;
    purge(st, tenant, store, file_id, Some(file_id)).await
}

/// The shared body. `file_id` is `Some` when the target is a single file.
async fn purge(
    st: &AppState,
    tenant: &Tenant,
    store: &Store,
    target_id: &str,
    file_id: Option<&str>,
) -> Result<PurgeReport> {
    let mut report = PurgeReport::default();

    // ── What is in scope ──
    let files = match file_id {
        Some(id) => store
            .capture_file_by_id(id)
            .await?
            .map(|f| vec![f])
            .unwrap_or_default(),
        None => store.capture_files_of_session(target_id).await?,
    };
    if files.is_empty() {
        anyhow::bail!("no captured file or session {target_id}");
    }
    let file_ids: Vec<String> = files.iter().map(|f| f.id.clone()).collect();

    // Every generation of every file in scope, and every object key under them
    let mut generation_ids: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    // `archive_files` takes a **session** id, so the per-file case still goes
    // through it and then filters: a file's generations are the ones whose
    // `file_id` matches, and asking per file would be one query each for a
    // result the session-wide call already has
    let session_ids: Vec<String> = files
        .iter()
        .map(|f| f.session_id.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    for session_id in &session_ids {
        // `archive_files` returns `(file, generations)` for the whole session;
        // `file.id` is the right field to compare, and the generations are that
        // file's own
        for (file, generations) in store.archive_files(session_id).await? {
            if !file_ids.contains(&file.id) {
                continue;
            }
            for generation in generations {
                for chunk in store
                    .chunks_overlapping(&generation.id, 0, i64::MAX / 2)
                    .await?
                {
                    keys.push(chunk.storage_key);
                }
                generation_ids.push(generation.id);
            }
        }
    }

    // ── 1. The data keys ──
    //
    // First, because it is the step that makes the objects unreadable even if
    // everything after it fails. A purge that dies here has still made the
    // bytes useless
    for id in &file_ids {
        report.keys_destroyed += store.delete_capture_keys(id).await?;
    }

    // ── 2. The objects ──
    for key in &keys {
        match st.delete_capture_object(key).await {
            Ok(()) => report.objects_deleted += 1,
            Err(e) => {
                report.objects_failed += 1;
                tracing::warn!(key, error = %e, "a purged object could not be deleted");
            }
        }
    }

    // ── 3. The memory layer, through the redact cascade ──
    //
    // **Before the archive rows**, because the cascade needs the promoted
    // episode's `content` to find the `transcript_event_id` it came from — and
    // the archive purge clears the events that id points at.
    //
    // Each promoted episode is redacted rather than deleted:
    // `facts.source_episode_id` is `NOT NULL REFERENCES episodes(id)` with no
    // `ON DELETE CASCADE`, so a promoted episode that has facts **cannot** be
    // deleted, and having facts is the normal case — promotion exists to feed
    // extraction. §十一's answer for an episode is the same one: blank the
    // content, invalidate the derived facts, keep the row.
    let promoted = store.promoted_episodes_of(&session_ids).await?;
    for episode_id in &promoted {
        let r = crate::redact::redact_episode(
            st,
            tenant,
            episode_id,
            RedactionMode::Purge,
            "the captured session it was promoted from was purged",
        )
        .await?;
        report.facts_redacted += r.facts_cleared;
        report.summaries_deleted += r.summaries_deleted;
    }
    report.promoted_episodes_redacted = promoted.len();

    // ── 4. The archive rows ──
    let counts = store.purge_capture_rows(&file_ids, &generation_ids).await?;
    report.events_deleted = counts.events;
    report.chunks_deleted = counts.chunks;
    report.summaries_deleted += counts.summaries;

    // ── 5. The tombstone ──
    //
    // Last. A crash before this leaves a session that is partly gone and still
    // listed — which is the state an operator can see and resume. Writing it
    // first would leave a session that looks erased while its bytes are still
    // there, and nobody would ever look again
    let tombstone = NewRedaction {
        id: cormex_core::Id::new(),
        target_kind: if file_id.is_some() {
            RedactionTarget::CaptureFile
        } else {
            RedactionTarget::CaptureSession
        },
        target_id: target_id.to_owned(),
        mode: RedactionMode::Purge,
        reason: "user requested purge of a captured session".to_owned(),
        actor: "user".to_owned(),
        device_id: "server".to_owned(),
    };
    store
        .write_txn(async |t| t.insert_redaction(&tombstone).await)
        .await
        .context("the purge completed but its tombstone could not be written")?;

    Ok(report)
}

/// Whether a session already carries a purge tombstone.
///
/// # Errors
/// The query failed.
pub async fn already_purged(store: &Store, session_id: &str) -> Result<bool> {
    Ok(store
        .redactions_for_target(RedactionTarget::CaptureSession, session_id)
        .await?
        .iter()
        .any(|r| r.mode == RedactionMode::Purge))
}
#[cfg(test)]
mod tests {
    //! The purge, against a real database and a real object store.
    //!
    //! What these measure is the part that is easy to get wrong and impossible
    //! to notice afterwards: that the bytes are actually gone, that the
    //! tombstone is written **last**, and that a wrong confirmation destroys
    //! nothing.

    use super::*;
    use cormex_core::Id;
    use cormex_store::{
        ChunkEncoding, FileRole, Framing, NewCaptureChunk, NewCaptureFile, NewProjection,
        NewTranscriptEvent, ProjectionStatus, TranscriptEventKind,
    };

    /// Seed a session with one event, one chunk, one object and one summary.
    async fn seed(st: &AppState, store: &Store) -> (String, String, String) {
        let body = br#"{"type":"user","message":{"role":"user","content":"the secret decision"}}
"#;
        let file = NewCaptureFile {
            id: Id::new(),
            native_session_id: format!("native-{}", Id::new()),
            parent_native_session_id: None,
            device_id: Id::new().to_string(),
            agent: "claude_code".into(),
            logical_path: "projects/-p/s1.jsonl".into(),
            role: FileRole::Transcript,
            framing: Framing::Append,
            cwd: None,
            project_key: None,
        };
        let registered = store
            .archive_txn(async |t| t.register_capture_file(&file).await)
            .await
            .expect("register");
        let (session_id, generation_id) = (registered.session.id, registered.generation.id);

        let sha = cormex_blob::hash::sha256_hex(body);
        let key = format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]);
        let compressed = zstd::encode_all(&body[..], 3).expect("compress");
        st.put_capture_object(&key, axum::body::Bytes::from(compressed))
            .await
            .expect("store the object");

        store
            .archive_txn(async |t| {
                t.append_chunk(&NewCaptureChunk {
                    id: Id::new(),
                    generation_id: generation_id.parse().expect("a ULID"),
                    byte_offset: 0,
                    byte_length: i64::try_from(body.len()).expect("fits"),
                    content_sha256: sha.clone(),
                    storage_key: key.clone(),
                    encoding: ChunkEncoding::Zstd,
                    observed_at: chrono::Utc::now(),
                })
                .await
            })
            .await
            .expect("append");

        let event_id = Id::new();
        store
            .archive_txn(async |t| {
                t.insert_transcript_events(
                    &[NewTranscriptEvent {
                        id: event_id,
                        session_id: session_id.parse().expect("a ULID"),
                        parser_version: "test@1".into(),
                        seq: 0,
                        kind: TranscriptEventKind::UserMessage,
                        occurred_at: Some(chrono::Utc::now()),
                        content: serde_json::json!([]),
                        text: Some("the secret decision".into()),
                        tsv_source: Some("the secret decision".into()),
                        tool_name: None,
                        tool_call_id: None,
                        src_generation_id: generation_id.parse().expect("a ULID"),
                        src_offset: 0,
                        src_length: 19,
                    }],
                    &NewProjection {
                        id: Id::new(),
                        session_id: session_id.parse().expect("a ULID"),
                        parser_version: "test@1".into(),
                        through: serde_json::json!({}),
                        next_seq: 1,
                        status: ProjectionStatus::Ok,
                        error: None,
                    },
                )
                .await
            })
            .await
            .expect("events");

        // A summary derived from that event
        store
            .write_txn(async |t| {
                t.insert_summary(&cormex_store::NewSummary {
                    id: Id::new(),
                    scope: cormex_store::SummaryScope::Session,
                    scope_key: session_id.clone(),
                    text: "the decision was made".into(),
                    tsv_source: Some("the decision was made".into()),
                    embedding: None,
                    embedding_model: "none".into(),
                    covers_from: None,
                    covers_to: None,
                    sources: vec![cormex_store::ProvenanceRef {
                        kind: cormex_store::SourceKind::TranscriptEvent,
                        id: event_id,
                    }],
                    device_id: "server".into(),
                })
                .await
            })
            .await
            .expect("summary");

        (session_id, generation_id, key)
    }

    /// A purge removes the bytes, the rows, and the derived summary, and leaves
    /// a tombstone.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_purge_removes_the_bytes_and_the_rows() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (session_id, _generation, key) = seed(&st, &store).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let report = purge_session(&st, &tenant, &session_id)
            .await
            .expect("purge");

        assert_eq!(
            report.objects_deleted, 1,
            "the object should have been deleted"
        );
        assert_eq!(report.objects_failed, 0);
        assert_eq!(report.events_deleted, 1);
        assert_eq!(report.chunks_deleted, 1);
        assert_eq!(report.summaries_deleted, 1);

        // The object really is gone from storage
        assert!(
            st.get_capture_object(&key).await.is_err(),
            "the object survived the purge"
        );
        // And the rows
        assert!(
            store
                .transcript_page(&session_id, "test@1", -1, 10)
                .await
                .expect("page")
                .is_empty(),
            "the events survived the purge"
        );
        // The tombstone is there
        assert!(
            already_purged(&store, &session_id).await.expect("check"),
            "the purge left no tombstone"
        );

        db.drop().await;
    }

    /// A purge with no tombstone yet is not "already purged".
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_fresh_session_is_not_already_purged() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (session_id, _, _) = seed(&st, &store).await;

        assert!(!already_purged(&store, &session_id).await.expect("check"));

        db.drop().await;
    }

    /// Purging an unknown session is an error, not a silent success.
    ///
    /// A purge that reports success while doing nothing is worse than one that
    /// fails: the caller believes the data is gone.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn purging_an_unknown_session_fails() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };

        let err = purge_session(&st, &tenant, &Id::new().to_string())
            .await
            .expect_err("an unknown session must not purge successfully");
        assert!(
            err.to_string().contains("no captured file or session"),
            "{err}"
        );

        db.drop().await;
    }

    /// The confirmation gate: a wrong or missing `confirm` destroys nothing.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_wrong_confirmation_destroys_nothing() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (session_id, _, key) = seed(&st, &store).await;

        let principal = crate::principal::Principal::LegacyOwner;
        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };

        // No confirmation
        let err = super::super::archive_api::purge_common_for_test(
            &st,
            &principal,
            &session_id,
            None,
            None,
        )
        .await
        .expect_err("a purge with no confirmation must be refused");
        assert!(
            err.message().contains("cannot be undone"),
            "{}",
            err.message()
        );

        // A wrong one
        let err = super::super::archive_api::purge_common_for_test(
            &st,
            &principal,
            &session_id,
            Some("some-other-id"),
            None,
        )
        .await
        .expect_err("a mismatched confirmation must be refused");
        assert!(
            err.message().contains("does not match"),
            "{}",
            err.message()
        );

        // Nothing was destroyed
        assert!(
            st.get_capture_object(&key).await.is_ok(),
            "a refused purge deleted the object anyway"
        );
        assert!(
            !already_purged(&store, &session_id).await.expect("check"),
            "a refused purge left a tombstone"
        );
        let _ = tenant;

        db.drop().await;
    }

    /// A promoted episode **with facts** is redacted, not deleted.
    ///
    /// `facts.source_episode_id` is `NOT NULL REFERENCES episodes(id)` with no
    /// `ON DELETE CASCADE`, so deleting a promoted episode that has facts fails
    /// outright — and having facts is the normal case, because promotion exists
    /// to feed extraction. Before this test, the purge would have aborted with a
    /// foreign-key violation the first time anybody purged a session that had
    /// been extracted.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_promoted_episode_with_facts_is_redacted_not_deleted() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");
        let (session_id, _generation, _key) = seed(&st, &store).await;

        // A promoted episode: `content` carries `transcript_event_id`
        let episode_id = Id::new();
        store
            .write_txn(async |t| {
                t.insert_episode(&cormex_store::NewEpisode {
                    id: episode_id,
                    session_id: session_id.clone(),
                    role: cormex_store::Role::User,
                    content: serde_json::json!({
                        "transcript_event_id": Id::new().to_string(),
                        "text": "the secret decision",
                    }),
                    text: Some("the secret decision".into()),
                    tsv_source: Some("the secret decision".into()),
                    domain: None,
                    device_id: "server".into(),
                    occurred_at: chrono::Utc::now(),
                })
                .await
            })
            .await
            .expect("promote an episode");

        // A fact derived from it — the thing that makes DELETE fail
        let entity = cormex_store::NewEntity {
            id: Id::new(),
            kind: "concept".into(),
            name: "RustFS".into(),
            summary: None,
            embedding: Some(cormex_store::Vector::from(vec![0.1f32; 1024])),
            embedding_model: "test".into(),
            device_id: "server".into(),
        };
        let entity_id = entity.id;
        let fact = cormex_store::NewFact {
            id: Id::new(),
            subject_id: entity_id,
            predicate: "uses".into(),
            object_text: Some("RustFS".into()),
            object_entity_id: None,
            statement: "cormex 使用 RustFS".into(),
            tsv_source: Some("cormex 使用 RustFS".into()),
            embedding: Some(cormex_store::Vector::from(vec![0.1f32; 1024])),
            embedding_model: "test".into(),
            domain: None,
            confidence: 1.0,
            valid_at: None,
            source_episode_id: episode_id,
            source: cormex_store::FactSource::Conversation,
            derived_from: Vec::new(),
            extracted_by: "test".into(),
            device_id: "server".into(),
        };
        let fact_id = fact.id;
        store
            .write_txn(async |t| {
                t.insert_entity(&entity).await?;
                t.insert_fact(&fact).await
            })
            .await
            .expect("a fact from the promoted episode");

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let report = purge_session(&st, &tenant, &session_id)
            .await
            .expect("the purge must not abort on the foreign key");

        assert_eq!(
            report.promoted_episodes_redacted, 1,
            "the promoted episode was not redacted"
        );
        assert_eq!(report.facts_redacted, 1, "the fact was not cleared");

        // The episode row is still there, blanked
        let episode = store.episode(&episode_id.to_string()).await.expect("query");
        let episode = episode.expect("the episode row must survive: facts point at it");
        assert_eq!(
            episode.content,
            serde_json::json!({"redacted": true}),
            "the episode's content was not blanked"
        );
        assert!(episode.text.is_none(), "the episode's text survived");

        // The fact's content is gone, and it is out of retrieval
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        let (statement, has_embedding): (String, bool) =
            sqlx::query_as("SELECT statement, embedding IS NOT NULL FROM facts WHERE id = $1")
                .bind(fact_id.to_string())
                .fetch_one(&pool)
                .await
                .expect("the fact row");
        pool.close().await;
        assert_eq!(statement, "[redacted]");
        assert!(
            !has_embedding,
            "the embedding survived -- a vector can be inverted approximately, so it is a \
             recoverable copy of the thing being erased"
        );

        db.drop().await;
    }
}

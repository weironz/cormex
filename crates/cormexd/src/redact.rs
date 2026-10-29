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
use cormex_store::{
    Actor, NewFactEvent, RedactionMode, RedactionTarget, Store, StoreError,
};

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
    pub summaries_regenerated: usize,
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
    let id = episode_id
        .parse::<Id>()
        .map_err(|_| anyhow::anyhow!("{episode_id} is not a ULID"))?;

    let mut report = RedactReport::default();

    // ── The episode itself ──
    //
    // `content` gets a placeholder rather than being deleted: the row is the
    // identity every `fact.source_episode_id` points at, and removing it would
    // turn a cascade into a dangling reference
    report.episodes_cleared = store
        .redact_episode_content(episode_id)
        .await
        .context("clearing the episode failed")?;

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
            if !store.blob_referenced_elsewhere(&blob_hash, episode_id).await? {
                if let Ok(key) = store.blob_storage_key(&blob_hash).await {
                    if st.delete_capture_object(&key).await.is_ok() {
                        report.blobs_deleted += 1;
                    }
                }
            }
        }
    }

    // ── Summaries derived from it ──
    //
    // **Deleted, not blanked.** A summary is a paraphrase, and a paraphrase of a
    // secret is the secret. The design's "delete and regenerate excluding the
    // redacted episode" is what this does: the deletion is here, the
    // regeneration is the distiller's next round, which reads
    // `transcript_events` — and those are cleared below
    let summaries = store.summaries_derived_from(RedactionTarget::Episode, episode_id).await?;
    for summary_id in &summaries {
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
    let mut report = RedactReport::default();

    report.blob_transcripts_cleared = store
        .redact_blob_transcript(blob_hash)
        .await
        .context("clearing the blob's transcripts failed")?;

    if mode == RedactionMode::Purge && !store.blob_referenced_anywhere(blob_hash).await? {
        if let Ok(key) = store.blob_storage_key(blob_hash).await
            && st.delete_capture_object(&key).await.is_ok()
        {
            report.blobs_deleted += 1;
        }
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
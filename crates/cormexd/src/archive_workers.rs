//! The archive background jobs: the masker and the projector.
//!
//! Design: docs/design/session-archive-architecture.md §8.1, §8.2.
//!
//! # No job queue, and that is the design
//!
//! Both jobs' work is **derivable from the data**: a generation needs masking
//! when its end is past the scanner's progress, a session needs projecting when
//! any of its generations is past the projection's `through`. So there is no
//! table of pending jobs to fall out of step with the rows, and a process that
//! dies mid-round loses nothing — the next round recomputes the same todo.
//!
//! # Both jobs are idempotent, and that is what makes the above safe
//!
//! - The masker inserts with `ON CONFLICT DO NOTHING` on
//!   `(generation, offset, rule, scanner)`, and only advances its progress
//!   after the masks are in.
//! - The projector derives each event's id from
//!   `(session, parser_version, generation, offset)` and inserts with
//!   `ON CONFLICT (id) DO NOTHING`.
//!
//! So a round that is interrupted halfway is simply re-run, and a round that
//! overlaps with itself changes nothing.
//!
//! # One failure does not stop the round
//!
//! A session whose parser errors is recorded as `status='failed'` with the
//! error, and the loop continues. It is **not retried for an hour**: a parser
//! that fails on a given input fails on it every time, and retrying every round
//! turns one bad session into a log flood.

use std::sync::Arc;

use anyhow::Result;
use cormex_core::Id;
use cormex_memory::distill::{DistillEvent, Observation, Observer};
use cormex_proto::transcript::ContentBlock;
use cormex_store::{
    CaptureChunk, ChunkEncoding, NewCaptureMask, NewEpisode, NewMaskProgress, NewProjection,
    NewSummary, NewTranscriptEvent, Projection, ProjectionStatus, ProvenanceRef, SourceKind, Store,
    SummaryScope, TranscriptEvent, TranscriptEventKind,
};

use crate::request_tenant::Tenant;
use crate::state::AppState;

/// How often a round runs.
const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How long to wait after startup before the first round.
///
/// Not zero: a restart during a deploy would otherwise start scanning while the
/// database is still warming, and the first round's errors would be noise.
const FIRST_SCAN_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// How many generations / sessions one round handles per tenant.
const BATCH: i64 = 20;

/// How many events go in one `archive_txn`.
///
/// The design says "at most 500 rows per transaction": a transaction that holds
/// a lock stays short, and 500 is small enough that a rollback costs little.
const EVENTS_PER_TXN: usize = 500;

/// Blocks larger than this become a [`ContentBlock::ResourceRef`] instead of
/// being inlined.
///
/// The raw bytes are always kept, so a large tool output is a pointer rather
/// than a copy — which is what makes "the original is always readable" true
/// without storing everything twice.
const MAX_INLINE_BLOCK: usize = 16 * 1024;

/// How much context before the progress point the masker re-scans.
///
/// A secret can straddle the boundary: a key that starts 100 bytes before the
/// progress point was not matched last round, because the regex needs its whole
/// body. Re-scanning a window catches it. 1 KiB is far more than the longest
/// pattern.
const MASK_OVERLAP: i64 = 1024;

/// The scanner's version string, recorded with every mask.
///
/// It is a constant rather than a value derived from the rules: a rule change
/// that does not bump this would leave old masks attributed to the new scanner,
/// and "which rules produced this mask" would have no answer.
pub const SCANNER: &str = "redaction@1";

/// How long a failed session is left alone.
const FAILED_BACKOFF: chrono::Duration = chrono::Duration::hours(1);

/// How long a session has to be quiet before it is distilled.
///
/// Thirty minutes is the design's number (§8.4). Shorter and one stretch of
/// work produces several entries that repeat each other's context; longer and
/// "I just finished this, what did we learn" goes unanswered.
const DISTILL_IDLE_MINUTES: i64 = 30;

/// How many events one distillation run takes.
///
/// A cost ceiling, not a page size: a session with ten thousand new events
/// would otherwise go to the model in one request and be rejected for length
/// after the tokens had already been counted. The next round picks up where
/// this one stopped.
const DISTILL_BATCH: i64 = 200;

/// The `device_id` on rows the server itself produced.
///
/// Not a device: the server produced this. Recording a device id here would
/// attribute the observation to whichever machine happened to run the worker.
pub(crate) const SERVER_DEVICE: &str = "server";

/// Spawn both workers.
///
/// Does nothing on the mock backend: there is no database, so there is nothing
/// to mask or project.
pub fn spawn(st: AppState) {
    let Some(_) = st.live().cloned() else {
        tracing::info!("mock backend: the archive workers will not start");
        return;
    };

    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SCAN_DELAY).await;
        let mut tick = tokio::time::interval(SCAN_INTERVAL);
        loop {
            tick.tick().await;

            // Re-enumerated each round: a tenant that registered after startup
            // has to be covered too. Holding the startup list would leave new
            // users' archives unmasked and unprojected, with no symptom
            let schemas = match st.tenant_schemas().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "could not list the tenants; skipping this round");
                    continue;
                }
            };

            for schema in &schemas {
                let Ok(store) = st.tenant_store(schema).await else {
                    tracing::warn!(
                        schema = schema.as_str(),
                        "could not connect to this tenant; skipping its archive work"
                    );
                    continue;
                };
                let tenant = Tenant::Live {
                    schema: schema.clone(),
                    store,
                };

                match mask_tick(&st, &tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(count = n, schema = schema.as_str(), "masked"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the masker round failed; the next one retries"
                    ),
                }
                // **Projector first, then indexer.** The indexer consumes
                // events the projector wrote; the other order means the first
                // round indexes nothing and every round is one behind
                match project_tick(&st, &tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(projected = n, schema = schema.as_str(), "projected"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the projector round failed; the next one retries"
                    ),
                }
                match index_tick(&tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(chunks = n, schema = schema.as_str(), "indexed"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the indexer round failed; the next one retries"
                    ),
                }
                // **Last, and only over what the projector has already
                // written.** The distiller's input is P-layer events; running
                // it before the projector would distill a session that is one
                // round behind, and the observation would be about work that
                // the next round then re-distills
                match distill_tick(&st, &tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(entries = n, schema = schema.as_str(), "distilled"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the distiller round failed; the next one retries"
                    ),
                }
                // Promotion is last and independent: it writes `episodes`,
                // which is a synchronous table, so it takes the sync lock —
                // running it before the archive work would let a history
                // import's promotion delay masking and projection, and those
                // two are what make the archive readable at all
                // Vectors last and optional: they are the only step here
                // that is off by default, and running them before the archive
                // work would make a deployment that turned them on pay for
                // them before anything is readable
                match embed_tick(&st, &tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(vectors = n, schema = schema.as_str(), "embedded"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the vector round failed; the next one retries"
                    ),
                }
                match promote_tick(&st, &tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(promoted = n, schema = schema.as_str(), "promoted"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the promoter round failed; the next one retries"
                    ),
                }
            }
        }
    });
}

/// One masker round. Returns how many generations were processed.
///
/// # Errors
/// The todo query failed, or the store could not be reached.
pub(crate) async fn mask_tick(st: &AppState, tenant: &Tenant) -> Result<usize> {
    let store = tenant.store()?;
    let todo = store.generations_needing_masks(SCANNER, BATCH).await?;
    let mut done = 0;

    for (generation_id, end) in todo {
        let through = store.mask_progress(&generation_id, SCANNER).await?;
        // The window starts a little before the progress point, so a secret
        // straddling the boundary is caught. It must not go below zero
        let from = (through - MASK_OVERLAP).max(0);
        let len = end - from;
        if len <= 0 {
            continue;
        }

        let bytes = read_logical_range(st, store, &generation_id, from, len).await?;
        let spans = cormex_memory::redaction::scan_spans_bytes(&bytes);

        let masks: Vec<NewCaptureMask> = spans
            .iter()
            .map(|s| NewCaptureMask {
                id: Id::new(),
                generation_id: generation_id.parse().expect("a ULID from the database"),
                // The span is relative to the window, so it is shifted back to
                // the generation's own coordinates
                byte_offset: from + i64::try_from(s.range.start).unwrap_or(0),
                byte_length: i32::try_from(s.range.end - s.range.start).unwrap_or(i32::MAX),
                rule: s.rule.name().to_owned(),
                scanner: SCANNER.to_owned(),
            })
            .collect();

        let progress = NewMaskProgress {
            id: Id::new(),
            generation_id: generation_id.parse().expect("a ULID from the database"),
            scanner: SCANNER.to_owned(),
            through_offset: end,
        };

        store
            .archive_txn(async |t| t.insert_masks(&masks, &progress).await)
            .await?;
        done += 1;
    }
    Ok(done)
}

/// One indexer round. Returns how many chunks were written.
///
/// # What it indexes, and what it deliberately does not
///
/// Only `tool_call` / `tool_result` events — the ones the conversation index
/// skips. Indexing the messages here as well would put every sentence in two
/// indexes, and `scope=archive` would then return the conversation with tool
/// output mixed in, which is exactly the noise D4 keeps out of the default
/// search.
///
/// # Errors
/// The todo query failed, or the store could not be reached.
pub(crate) async fn index_tick(tenant: &Tenant) -> Result<usize> {
    let store = tenant.store()?;
    let todo = store.events_needing_index(BATCH * 20).await?;
    let mut written = 0;

    for event in todo {
        // A project excluded from the archive index is skipped at **write**
        // time as well as at query time: writing it would put a project the
        // user asked to leave alone into an index, and the query-side filter
        // would only hide it
        if let Some(project) = store.session_project_key(&event.session_id).await?
            && store.archive_index_excluded(&project).await?
        {
            continue;
        }

        let text = event.text.unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }

        let chunks: Vec<cormex_store::NewArchiveChunk> = split_chunks(&text)
            .into_iter()
            .enumerate()
            .map(|(part, piece)| cormex_store::NewArchiveChunk {
                id: Id::new(),
                session_id: event.session_id.parse().expect("a ULID from the database"),
                event_id: event.id.parse().expect("a ULID from the database"),
                part: i32::try_from(part).unwrap_or(0),
                tsv_source: cormex_memory::tokenize::to_tsvector_input(&piece),
                text: piece,
            })
            .collect();

        store
            .archive_txn(async |t| t.insert_archive_chunks(&chunks).await)
            .await?;
        written += chunks.len();
    }
    Ok(written)
}

/// One distiller round. Returns how many observation entries were written.
///
/// # The order of the four gates
///
/// 1. **Idle.** A session being written to right now would be distilled in the
///    middle of a thought; see `sessions_needing_distill`.
/// 2. **Budget.** Checked *before* the model call, for the same reason
///    `enforce_quota` is: deducting afterwards means the last call always
///    overshoots, and "the last call" can be a three-year history import.
/// 3. **Redaction scan.** Before the text leaves the process. The P layer is
///    already masked, so this is a second fence rather than the first — but a
///    scanner rule added after a session was projected would otherwise let the
///    unmasked text through here.
/// 4. **The model call, outside any transaction** (constraint 1).
///
/// # Errors
/// The todo query failed, or the store could not be reached. A model failure
/// for one session is logged and the round continues: a provider outage must
/// not stop masking and projecting, which do not need a model at all.
pub(crate) async fn distill_tick(st: &AppState, tenant: &Tenant) -> Result<usize> {
    // The observer is built once per round rather than per session: it holds
    // the provider client, and constructing one per session would re-read the
    // provider definition every time
    let observer = match observer_for(st) {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(error = %e, "no model is configured; the distiller will not run");
            return Ok(0);
        }
    };
    distill_round(st, tenant, observer.as_ref()).await
}

/// The round itself, with the observer supplied.
///
/// Split from [`distill_tick`] so the tests can drive it with a fake: a
/// distiller whose only implementation is "call the provider" can be tested
/// only by calling a provider, and then the assertions measure the model's mood
/// rather than the tick's behaviour.
pub(crate) async fn distill_round(
    st: &AppState,
    tenant: &Tenant,
    observer: &dyn Observer,
) -> Result<usize> {
    let store = tenant.store()?;
    let todo = store
        .sessions_needing_distill(DISTILL_IDLE_MINUTES, BATCH)
        .await?;
    if todo.is_empty() {
        return Ok(0);
    }

    // The owner of this schema, for the per-tenant budget. `None` = no accounts
    // system (single-user self-hosting), where there is no one to bill and the
    // budget is the operator's own business
    let owner = st.user_for_schema(&tenant_schema(tenant)).await?;
    let daily_limit = st.distill_config().daily_tokens;

    let mut written = 0;
    for (session_id, through_seq) in todo {
        if let (Some(user), true) = (&owner, daily_limit > 0) {
            match st.distill_tokens_today(user).await {
                Ok(used) if used >= daily_limit => {
                    tracing::info!(
                        used,
                        limit = daily_limit,
                        "this tenant's distillation budget is spent for today; stopping"
                    );
                    break;
                }
                Ok(_) => {}
                // **Fail open on an accounting error.** The alternative is that
                // a broken usage query silently stops the observation log for
                // every tenant, and nothing says why
                Err(e) => {
                    tracing::warn!(error = %e.message(), "could not read the distill budget; continuing")
                }
            }
        }

        let events = store
            .events_after_seq(&session_id, through_seq, DISTILL_BATCH)
            .await?;
        if events.is_empty() {
            continue;
        }
        let last_seq = events.last().map_or(through_seq, |e| e.seq);

        let batch: Vec<DistillEvent> = events.iter().map(distill_event_of).collect();
        // The scan runs on the **joined** text: a secret split across two
        // events (a command and its output) would pass a per-event scan
        let joined: String = batch
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(rule) = cormex_memory::redaction::scan(&joined) {
            tracing::warn!(
                session = %session_id,
                rule = rule.name(),
                "this batch still contains credentials after masking; skipping distillation \
                 (the raw archive keeps the original as usual)"
            );
            // Progress still advances: re-reading the same batch every round
            // would log this warning forever and never get past it
            store
                .archive_txn(async |t| t.insert_distill_progress(&session_id, last_seq).await)
                .await?;
            continue;
        }

        let outcome = match observer.observe(&session_id, &batch).await {
            Ok(o) => o,
            Err(e) => {
                // No progress advance: the next round retries. Unlike the
                // projector's parse failures, a provider error is usually
                // transient, and giving up on the session forever would lose
                // the observations for good
                tracing::warn!(session = %session_id, error = %e, "the observation run failed; the next round retries");
                continue;
            }
        };

        if let Some(user) = &owner {
            st.record_usage(
                user,
                "distill",
                crate::quota::Usage {
                    input_tokens: outcome.input_tokens,
                    output_tokens: outcome.output_tokens,
                    own_key: false,
                },
                &outcome.model,
            )
            .await;
        }

        written += write_observations(st, store, &session_id, &outcome.entries).await?;

        store
            .archive_txn(async |t| t.insert_distill_progress(&session_id, last_seq).await)
            .await?;
    }
    Ok(written)
}

/// Write one run's observations as `summaries` + `derivations`.
///
/// # Why these are synchronous tables
///
/// `summaries` and `derivations` are in `table::ALL`, so they go through
/// `write_txn` and into `sync_log` — a device that receives a purged session's
/// tombstone has to be able to find the summaries derived from it, and that
/// lookup is the `derivations` edge.
///
/// # Idempotency
///
/// The summary's id is `Id::derived(at, "distill|<session>|<sources>")`, so a
/// re-run over the same events produces the same rows and the PK rejects the
/// duplicate. That is what makes "the process died after the model call but
/// before the progress row" safe: the next round redoes the call, and the write
/// is a no-op.
async fn write_observations(
    _st: &AppState,
    store: &Store,
    session_id: &str,
    entries: &[Observation],
) -> Result<usize> {
    let mut written = 0;
    for entry in entries {
        let seed = format!(
            "distill|{session_id}|{}",
            entry
                .sources
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        let summary = NewSummary {
            id: Id::derived(entry.at, &seed),
            scope: SummaryScope::Session,
            scope_key: session_id.to_owned(),
            text: entry.text.clone(),
            tsv_source: Some(cormex_memory::tokenize::to_tsvector_input(&entry.text)),
            // **No vector.** Summaries are not in the vector recall channels
            // today; computing one here would spend an embedding call per
            // observation for a column nothing reads. `embedding_model` is NOT
            // NULL, so it carries an explicit "there is no vector" marker
            // rather than an empty string — an empty string reads like a model
            // whose name failed to be filled in
            embedding: None,
            embedding_model: cormex_memory::brief::NO_EMBEDDING.to_owned(),
            covers_from: Some(entry.at),
            covers_to: Some(entry.at),
            sources: entry
                .sources
                .iter()
                .map(|id| ProvenanceRef {
                    kind: SourceKind::TranscriptEvent,
                    id: *id,
                })
                .collect(),
            // Not a device: the server produced this. Recording a device id
            // here would attribute the observation to whichever machine
            // happened to run the worker
            device_id: SERVER_DEVICE.to_owned(),
        };
        match store
            .write_txn(async |t| t.insert_summary(&summary).await)
            .await
        {
            Ok(_) => written += 1,
            // A duplicate means this exact observation was already written —
            // the run is idempotent, not failed
            Err(cormex_store::StoreError::Sql(sqlx::Error::Database(e)))
                if e.code().as_deref() == Some("23505") =>
            {
                tracing::debug!(session = %session_id, "this observation was already written");
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(written)
}

/// The observer the deployment is configured for.
///
/// `Err` when no provider is configured at all — which is the mock backend's
/// shape, and the reason this is not an `Option<Observer>`: the caller has to
/// decide what "no model" means, and silently skipping is the wrong default for
/// a misconfigured deployment.
fn observer_for(st: &AppState) -> Result<Arc<dyn Observer>> {
    let live = st
        .live()
        .ok_or_else(|| anyhow::anyhow!("the mock backend has no model"))?;
    let observer = cormex_memory::distill::CheapModelObserver::new(live.llm().clone())
        .with_model(st.distill_config().model.clone())?;
    Ok(Arc::new(observer))
}

fn tenant_schema(tenant: &Tenant) -> cormex_store::SchemaName {
    match tenant {
        Tenant::Live { schema, .. } => schema.clone(),
        Tenant::Mock => cormex_store::SchemaName::public(),
    }
}

pub(crate) fn distill_event_of(e: &TranscriptEvent) -> DistillEvent {
    DistillEvent {
        id: e.id.parse().unwrap_or_else(|_| Id::new()),
        seq: e.seq,
        kind: e.kind,
        occurred_at: e.occurred_at,
        text: e.text.clone().unwrap_or_default(),
    }
}

// ══════════════════════════════════════════════════════════
//  Promotion: P-layer events → episodes → facts (D14)
// ══════════════════════════════════════════════════════════

/// Which events become `episodes`, and therefore facts.
///
/// # The policy, and why it is this one
///
/// Every user message, plus **the last agent message of each turn**. Not every
/// agent message: an agent writes several paragraphs per turn, and promoting
/// each one is several extraction calls to say one thing. The last one carries
/// the conclusion, which is what a fact is made of.
///
/// Tool output is **never** promoted. It is the largest, least factual part of
/// a transcript, and the trace pipeline (`cormex_memory::trace`) already mines
/// it for the hard signals worth keeping — running it through the extractor as
/// well would pay a model to read build logs.
///
/// # Why the rule is computed over the batch rather than stored
///
/// "Is this the last agent message of its turn" is a property of the events
/// around it. Storing it would need a column that the projector would have to
/// keep in step with every append; computing it over the batch costs one pass
/// and cannot go stale.
#[must_use]
pub fn should_promote(events: &[TranscriptEvent], index: usize) -> bool {
    match events[index].kind {
        TranscriptEventKind::UserMessage => true,
        TranscriptEventKind::AgentMessage => {
            // The last agent message before the next user message (or the end)
            !events[index + 1..]
                .iter()
                .take_while(|e| e.kind != TranscriptEventKind::UserMessage)
                .any(|e| e.kind == TranscriptEventKind::AgentMessage)
        }
        _ => false,
    }
}

/// The step that turns one promoted episode into facts.
///
/// A trait so the tests can drive promotion without a provider: the assertions
/// that matter here are "the same event is never promoted twice" and "the facts
/// land where `memory_search` can find them", and neither should depend on a
/// model's mood.
#[async_trait::async_trait]
pub trait FactSink: Send + Sync {
    /// # Errors
    /// The extraction failed. The episode is already written and stays; only
    /// the facts are missing.
    async fn extract(
        &self,
        store: &Store,
        text: &str,
        ctx: &cormex_memory::extract::ExtractContext,
    ) -> anyhow::Result<()>;
}

/// The production sink: the same extractor and gate `POST /episodes` uses.
pub struct LiveFactSink<'a> {
    extractor: &'a cormex_memory::extract::Extractor,
    gate: &'a Arc<tokio::sync::Semaphore>,
}

impl<'a> LiveFactSink<'a> {
    #[must_use]
    pub fn new(
        extractor: &'a cormex_memory::extract::Extractor,
        gate: &'a Arc<tokio::sync::Semaphore>,
    ) -> Self {
        Self { extractor, gate }
    }
}

#[async_trait::async_trait]
impl FactSink for LiveFactSink<'_> {
    async fn extract(
        &self,
        store: &Store,
        text: &str,
        ctx: &cormex_memory::extract::ExtractContext,
    ) -> anyhow::Result<()> {
        // The **same** gate as the request path: a history import promoting
        // thousands of messages must not run at twice the rate the gate was
        // sized for
        let _permit = self.gate.acquire().await.ok();
        let report = self.extractor.ingest(store, text, ctx).await?;
        tracing::debug!(
            written = report.written.len(),
            superseded = report.superseded.len(),
            "promoted episode extracted"
        );
        Ok(())
    }
}

/// One promotion round. Returns how many episodes were promoted.
///
/// # Errors
/// The todo query failed, or the store could not be reached. An extraction
/// failure for one episode is logged and the round continues — a provider
/// outage must not stop the archive from being promoted once it recovers, and
/// the progress row only advances past what actually succeeded.
pub(crate) async fn promote_tick(st: &AppState, tenant: &Tenant) -> Result<usize> {
    let cfg = st.distill_config();
    if !cfg.promote || cfg.promote_per_round <= 0 {
        return Ok(0);
    }
    let Some(live) = st.live() else {
        return Ok(0);
    };
    let (extractor, gate) = live.extractor();
    promote_round(st, tenant, &LiveFactSink::new(extractor, gate)).await
}

/// The round itself, with the fact sink supplied.
pub(crate) async fn promote_round(
    st: &AppState,
    tenant: &Tenant,
    sink: &dyn FactSink,
) -> Result<usize> {
    let cfg = st.distill_config();
    if !cfg.promote || cfg.promote_per_round <= 0 {
        return Ok(0);
    }
    let store = tenant.store()?;
    let todo = store.sessions_needing_promotion(BATCH).await?;
    if todo.is_empty() {
        return Ok(0);
    }

    let mut promoted = 0usize;
    for (session_id, through_seq) in todo {
        let events = store
            .events_after_seq(&session_id, through_seq, cfg.promote_per_round)
            .await?;
        if events.is_empty() {
            continue;
        }

        let mut last_ok = through_seq;
        for (i, event) in events.iter().enumerate() {
            if !should_promote(&events, i) {
                last_ok = event.seq;
                continue;
            }
            let Some(text) = event
                .text
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            else {
                last_ok = event.seq;
                continue;
            };

            // Write-side negative filter, before the text reaches the model.
            // The P layer is already masked; this catches a scanner rule that
            // arrived after the session was projected
            if let Some(rule) = cormex_memory::redaction::scan(text) {
                tracing::warn!(
                    session = %session_id,
                    rule = rule.name(),
                    "this event still contains credentials; not promoting it"
                );
                last_ok = event.seq;
                continue;
            }

            let episode = NewEpisode {
                // **Derived from the event id.** A re-run over the same events
                // produces the same id, so the PK rejects the duplicate and
                // "promote this event twice" is impossible by construction —
                // which is what makes the progress row safe to lose
                id: Id::derived(
                    event.occurred_at.unwrap_or(event.created_at),
                    &format!("promote|{}", event.id),
                ),
                session_id: session_id.clone(),
                role: match event.kind {
                    TranscriptEventKind::UserMessage => cormex_store::Role::User,
                    _ => cormex_store::Role::Assistant,
                },
                // The event id travels inside `content`, which is how a fact
                // extracted from this episode can be traced back to the
                // transcript line it came from
                content: serde_json::json!({
                    "transcript_event_id": event.id,
                    "text": text,
                }),
                text: Some(text.to_owned()),
                tsv_source: Some(cormex_memory::tokenize::to_tsvector_input(text)),
                domain: None,
                device_id: SERVER_DEVICE.to_owned(),
                occurred_at: event.occurred_at.unwrap_or(event.created_at),
            };
            let episode_id = episode.id;

            let written = store
                .write_txn(async |t| t.insert_episode(&episode).await)
                .await;
            match written {
                Ok(_) => promoted += 1,
                Err(cormex_store::StoreError::Sql(sqlx::Error::Database(e)))
                    if e.code().as_deref() == Some("23505") =>
                {
                    // Already promoted. Not an error: the progress row was
                    // lost or the round overlapped
                    tracing::debug!(session = %session_id, event = %event.id, "already promoted");
                }
                Err(e) => {
                    // **Stop this session here** rather than skipping ahead:
                    // advancing past an episode that failed to write would
                    // leave it unextracted forever
                    tracing::warn!(session = %session_id, error = %e, "could not promote this event");
                    break;
                }
            }

            // ── The extraction, through the same gate as `POST /episodes` ──
            let ctx = cormex_memory::extract::ExtractContext::new(
                episode_id,
                event.occurred_at.unwrap_or(event.created_at),
            );
            // The episode is written; only the facts are missing if this
            // fails. Advancing is right: re-running extraction would need the
            // episode to be re-promoted, and its id makes that a no-op
            if let Err(e) = sink.extract(store, text, &ctx).await {
                tracing::warn!(
                    session = %session_id,
                    error = %e,
                    "the promoted episode could not be extracted; the episode itself is kept"
                );
            }

            last_ok = event.seq;
        }

        if last_ok > through_seq {
            store
                .archive_txn(async |t| t.insert_promote_progress(&session_id, last_ok).await)
                .await?;
        }
    }
    Ok(promoted)
}

/// One vector round (P5-6). Returns how many vectors were written.
///
/// **Off unless `CORMEX_SESSION_EMBEDDINGS` is set.** The card's acceptance is
/// "a comparable number on the session eval suite proves it is worth turning
/// on"; a vector leg costs an embedding call per conversation turn and a second
/// index to keep in step, and that has to be paid for with a measured
/// improvement rather than assumed.
///
/// # Errors
/// The todo query failed, or the embedding call failed.
pub(crate) async fn embed_tick(st: &AppState, tenant: &Tenant) -> Result<usize> {
    if !st.distill_config().session_embeddings {
        return Ok(0);
    }
    let Some(live) = st.live() else {
        return Ok(0);
    };
    let store = tenant.store()?;
    let model = live.embedder().model_id().to_owned();
    let todo = store.events_needing_embedding(&model, BATCH * 20).await?;
    if todo.is_empty() {
        return Ok(0);
    }

    let texts: Vec<String> = todo
        .iter()
        .map(|e| e.text.clone().unwrap_or_default())
        .collect();
    let vectors = live.embedder().embed(&texts).await?;
    if vectors.len() != todo.len() {
        anyhow::bail!(
            "the embedder returned {} vectors for {} texts",
            vectors.len(),
            todo.len()
        );
    }

    let rows: Vec<cormex_store::NewTranscriptEmbedding> = todo
        .iter()
        .zip(vectors)
        .map(|(e, v)| cormex_store::NewTranscriptEmbedding {
            event_id: e.id.parse().unwrap_or_else(|_| Id::new()),
            embedding: cormex_store::Vector::from(v),
            embedding_model: model.clone(),
        })
        .collect();

    store
        .archive_txn(async |t| t.insert_transcript_embeddings(&rows).await)
        .await?;
    Ok(rows.len())
}

/// Split a long text into 2 KiB pieces, on a **character** boundary.
///
/// A byte boundary can land in the middle of a multi-byte character, and the
/// result is not valid UTF-8 — which `to_tsvector` then rejects, turning one
/// long tool output into a failed round.
fn split_chunks(text: &str) -> Vec<String> {
    const PIECE: usize = 2 * 1024;
    if text.len() <= PIECE {
        return vec![text.to_owned()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + PIECE).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // A single character larger than the piece size: take it whole
            end = text[start..]
                .chars()
                .next()
                .map_or(text.len(), |c| start + c.len_utf8());
        }
        out.push(text[start..end].to_owned());
        start = end;
    }
    out
}

/// One projector round. Returns how many events were written.
///
/// # Errors
/// The todo query failed, or the store could not be reached.
pub(crate) async fn project_tick(st: &AppState, tenant: &Tenant) -> Result<usize> {
    let store = tenant.store()?;
    let todo = store
        .sessions_needing_projection("claude_code@1", BATCH)
        .await?;
    let mut written = 0;

    for (session_id, agent) in todo {
        match project_session(st, store, &session_id, &agent).await {
            Ok(n) => written += n,
            Err(e) => {
                // **Recorded, then skipped.** A parser that fails on this input
                // fails on it every time; retrying every round turns one bad
                // session into a log flood
                tracing::warn!(session = %session_id, error = %e, "could not project this session");
                let _ = store
                    .archive_txn(async |t| {
                        t.insert_transcript_events(
                            &[],
                            &NewProjection {
                                id: Id::new(),
                                session_id: session_id.parse().map_err(|_| {
                                    cormex_store::StoreError::Invalid("bad session id".into())
                                })?,
                                parser_version: "claude_code@1".into(),
                                through: serde_json::json!({}),
                                next_seq: 0,
                                status: ProjectionStatus::Failed,
                                error: Some(e.to_string()),
                            },
                        )
                        .await
                    })
                    .await;
            }
        }
    }
    Ok(written)
}

/// Project one session's files.
async fn project_session(
    st: &AppState,
    store: &Store,
    session_id: &str,
    agent: &str,
) -> Result<usize> {
    // Every file of the session, with its newest generation
    let files = store.capture_files_of_session(session_id).await?;
    let mut written = 0;

    for file in files {
        // The store's `FileRole` and the wire's are two types (the store must
        // not depend on the wire crate's enum). The conversion is one `match`,
        // where a new variant is a compile error rather than a silently
        // unhandled role
        let Some(parser) = cormex_transcript::parser_for(agent, wire_role(file.role)) else {
            continue;
        };
        let Some((generation, end)) = store.latest_generation(&file.id).await? else {
            continue;
        };

        // ── Resume from the parser's own checkpoint ──
        //
        // **Not just a byte offset.** The parser holds a line buffer: a record
        // straddling the resume point has to be completed by the next chunk, and
        // feeding the parser a chunk that starts mid-file fails its contiguity
        // check. So `through` carries the parser's checkpoint (which includes
        // the buffer and the tool-name map) alongside the offset, and a resume
        // restores it rather than starting from zero.
        let latest = store
            .latest_projection(session_id, parser.version())
            .await?;
        let slot = latest.as_ref().and_then(|p| p.through.get(&generation.id));

        let (through, mut parser) = match slot {
            Some(v) if v.get("parser").is_some() => {
                let offset = v
                    .get("offset")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let state = v.get("parser").cloned().unwrap_or(serde_json::Value::Null);
                // The parser type is known from the agent, so the restore is
                // dispatched the same way `parser_for` dispatches
                let restored = restore_parser(agent, file.role, &state)?;
                (offset, restored)
            }
            _ => (0, parser),
        };
        let mut next_seq = latest.as_ref().map_or(0, |p| p.next_seq);

        if through >= end {
            continue;
        }

        let chunks = store
            .chunks_overlapping(&generation.id, through, end - through)
            .await?;
        let mut pending: Vec<NewTranscriptEvent> = Vec::new();
        // Assigned in the loop; the final value is what the last checkpoint
        // records. The initial value is only a placeholder for the compiler.
        #[allow(unused_assignments)]
        let mut projected_to = through;

        for chunk in chunks {
            let bytes = read_chunk_logical(st, &chunk).await?;
            // The chunk may start before `through` when it straddles the resume
            // point; slice to the part not yet consumed
            let skip = usize::try_from((through - chunk.byte_offset).max(0)).unwrap_or(0);
            let slice = if skip < bytes.len() {
                &bytes[skip..]
            } else {
                &[][..]
            };
            let feed_from = u64::try_from(chunk.byte_offset.max(through)).unwrap_or(0);

            let events = parser
                .feed(feed_from, slice)
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            for e in events {
                let masked_text = cormex_memory::redaction::mask_text(&e.text).into_owned();
                let content = mask_blocks(&e.content);
                let occurred_at = e.occurred_at.unwrap_or(chunk.created_at);
                pending.push(NewTranscriptEvent {
                    id: Id::derived(
                        occurred_at,
                        &format!(
                            "{session_id}|{}|{}|{}",
                            parser.version(),
                            generation.id,
                            e.src_offset
                        ),
                    ),
                    session_id: session_id.parse()?,
                    parser_version: parser.version().to_owned(),
                    seq: next_seq,
                    kind: kind_of(e.kind),
                    occurred_at: Some(occurred_at),
                    content: serde_json::to_value(&content)?,
                    text: Some(masked_text.clone()),
                    tsv_source: Some(cormex_memory::tokenize::to_tsvector_input(&masked_text)),
                    tool_name: e.tool_name,
                    tool_call_id: e.tool_call_id,
                    src_generation_id: generation.id.parse()?,
                    src_offset: i64::try_from(e.src_offset).unwrap_or(0),
                    src_length: i64::try_from(e.src_length).unwrap_or(0),
                });
                next_seq += 1;
            }
            projected_to = chunk.byte_offset + chunk.byte_length;

            if pending.len() >= EVENTS_PER_TXN {
                let batch = std::mem::take(&mut pending);
                let checkpoint = checkpoint_of(&generation.id, projected_to, parser.checkpoint());
                store
                    .archive_txn(async |t| {
                        t.insert_transcript_events(
                            &batch,
                            &NewProjection {
                                id: Id::new(),
                                session_id: session_id.parse().map_err(|_| {
                                    cormex_store::StoreError::Invalid("bad session id".into())
                                })?,
                                parser_version: parser.version().to_owned(),
                                through: checkpoint.clone(),
                                next_seq,
                                status: ProjectionStatus::Partial,
                                error: None,
                            },
                        )
                        .await
                    })
                    .await?;
                written += batch.len();
            }
        }

        // The final batch, with `ok` status and the generation marked complete
        store
            .archive_txn(async |t| {
                t.insert_transcript_events(
                    &pending,
                    &NewProjection {
                        id: Id::new(),
                        session_id: session_id.parse().map_err(|_| {
                            cormex_store::StoreError::Invalid("bad session id".into())
                        })?,
                        parser_version: parser.version().to_owned(),
                        through: checkpoint_of(&generation.id, end, parser.checkpoint()),
                        next_seq,
                        status: ProjectionStatus::Ok,
                        error: None,
                    },
                )
                .await
            })
            .await?;
        written += pending.len();
    }
    Ok(written)
}

/// Read one chunk's logical bytes from object storage.
async fn read_chunk_logical(st: &AppState, chunk: &CaptureChunk) -> Result<Vec<u8>> {
    let stored = st.get_capture_object(&chunk.storage_key).await?;
    match chunk.encoding {
        ChunkEncoding::Zstd => Ok(zstd::decode_all(&stored[..])?),
        ChunkEncoding::Identity => Ok(stored.to_vec()),
    }
}

/// Read `[offset, offset + len)` of a generation's logical bytes.
async fn read_logical_range(
    st: &AppState,
    store: &Store,
    generation_id: &str,
    offset: i64,
    len: i64,
) -> Result<Vec<u8>> {
    let chunks = store.chunks_overlapping(generation_id, offset, len).await?;
    let mut out = Vec::new();
    for chunk in chunks {
        let bytes = read_chunk_logical(st, &chunk).await?;
        // The chunk may start before `offset`; slice to the window
        let start = usize::try_from((offset - chunk.byte_offset).max(0)).unwrap_or(0);
        let end = usize::try_from((offset + len - chunk.byte_offset).min(chunk.byte_length))
            .unwrap_or(bytes.len())
            .min(bytes.len());
        if start < end {
            out.extend_from_slice(&bytes[start..end]);
        }
    }
    Ok(out)
}

/// Mask every text field inside a block, and replace a large one with a
/// reference.
fn mask_blocks(blocks: &[ContentBlock]) -> Vec<ContentBlock> {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => ContentBlock::Text {
                text: cormex_memory::redaction::mask_text(text).into_owned(),
            },
            ContentBlock::Thought { text } => ContentBlock::Thought {
                text: cormex_memory::redaction::mask_text(text).into_owned(),
            },
            ContentBlock::ToolResult {
                tool_call_id,
                output,
                is_error,
            } => {
                if output.len() > MAX_INLINE_BLOCK {
                    // A large tool output is a pointer rather than a copy: the
                    // raw bytes are always kept, so "the original is always
                    // readable" stays true without storing everything twice
                    ContentBlock::ResourceRef {
                        generation_id: String::new(),
                        offset: 0,
                        length: i64::try_from(output.len()).unwrap_or(0),
                        mime: "text/plain".into(),
                    }
                } else {
                    ContentBlock::ToolResult {
                        tool_call_id: tool_call_id.clone(),
                        output: cormex_memory::redaction::mask_text(output).into_owned(),
                        is_error: *is_error,
                    }
                }
            }
            other => other.clone(),
        })
        .collect()
}

/// The `through` value: the offset **and** the parser's checkpoint.
///
/// The offset alone is not enough to resume — the parser holds a line buffer,
/// and feeding it a chunk that starts mid-file fails its contiguity check. The
/// failure is an `Err`, which the caller records as a `failed` projection, so
/// the symptom would be "the session stopped projecting after the first
/// append".
fn checkpoint_of(
    generation_id: &str,
    offset: i64,
    parser_state: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        generation_id: {
            "offset": offset,
            "parser": parser_state,
        }
    })
}

/// Restore a parser from a stored checkpoint.
fn restore_parser(
    agent: &str,
    role: cormex_store::FileRole,
    state: &serde_json::Value,
) -> Result<Box<dyn cormex_transcript::Parser>> {
    use cormex_proto::capture::FileRole as W;
    match (agent, wire_role(role)) {
        ("claude_code", W::Transcript | W::Subagent) => Ok(Box::new(
            cormex_transcript::claude_code::ClaudeCodeParser::restore(state)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        )),
        ("codex", W::Transcript) => Ok(Box::new(
            cormex_transcript::codex::CodexParser::restore(state)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        )),
        _ => anyhow::bail!("no parser for {agent}"),
    }
}

fn wire_role(role: cormex_store::FileRole) -> cormex_proto::capture::FileRole {
    use cormex_proto::capture::FileRole as W;
    use cormex_store::FileRole as S;
    match role {
        S::Transcript => W::Transcript,
        S::Subagent => W::Subagent,
        S::Sidecar => W::Sidecar,
        S::Attachment => W::Attachment,
        S::Memory => W::Memory,
        S::Row => W::Row,
    }
}

fn kind_of(k: cormex_proto::transcript::TranscriptKind) -> TranscriptEventKind {
    use cormex_proto::transcript::TranscriptKind as K;
    match k {
        K::UserMessage => TranscriptEventKind::UserMessage,
        K::AgentMessage => TranscriptEventKind::AgentMessage,
        K::AgentThought => TranscriptEventKind::AgentThought,
        K::ToolCall => TranscriptEventKind::ToolCall,
        K::ToolResult => TranscriptEventKind::ToolResult,
        K::Plan => TranscriptEventKind::Plan,
        K::System => TranscriptEventKind::System,
        K::Meta => TranscriptEventKind::Meta,
        K::Unknown => TranscriptEventKind::Unknown,
    }
}

/// The failed-session backoff, exposed so the projector can consult it.
#[must_use]
pub fn failed_recently(p: &Projection) -> bool {
    p.status == ProjectionStatus::Failed && chrono::Utc::now() - p.created_at < FAILED_BACKOFF
}

#[cfg(test)]
mod tests {
    //! The masker and the projector, driven directly against a real database.
    //!
    //! The tick functions are `pub(crate)` so a test can call one round without
    //! waiting for the interval — a test that slept 60 seconds per round would
    //! not be run.

    use super::*;
    use cormex_store::{FileRole, Framing, NewCaptureChunk, NewCaptureFile};

    /// Register a file with one chunk of `body`, returning
    /// `(session_id, generation_id)`.
    async fn seed(store: &Store, agent: &str, body: &[u8]) -> (String, String) {
        let new = NewCaptureFile {
            id: Id::new(),
            native_session_id: format!("native-{}", Id::new()),
            parent_native_session_id: None,
            device_id: Id::new().to_string(),
            agent: agent.into(),
            logical_path: "projects/-p/s1.jsonl".into(),
            role: FileRole::Transcript,
            framing: Framing::Append,
            cwd: None,
            project_key: None,
        };
        let registered = store
            .archive_txn(async |t| t.register_capture_file(&new).await)
            .await
            .expect("register");
        let (session_id, generation_id) = (registered.session.id, registered.generation.id);

        let sha = {
            use sha2::Digest as _;
            format!("{:x}", sha2::Sha256::digest(body))
        };
        store
            .archive_txn(async |t| {
                t.append_chunk(&NewCaptureChunk {
                    id: Id::new(),
                    generation_id: generation_id.parse().expect("a ULID"),
                    byte_offset: 0,
                    byte_length: i64::try_from(body.len()).expect("fits"),
                    content_sha256: sha.clone(),
                    storage_key: format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]),
                    encoding: ChunkEncoding::Zstd,
                    observed_at: chrono::Utc::now(),
                })
                .await
            })
            .await
            .expect("append");
        (session_id, generation_id)
    }

    /// Put the chunk's bytes into the object store, zstd-compressed.
    async fn store_object(st: &AppState, body: &[u8]) {
        use sha2::Digest as _;
        let sha = format!("{:x}", sha2::Sha256::digest(body));
        // The bucket levels must agree with the hash: `validate_capture_key`
        // checks that, because a key whose levels do not match is legal to
        // write and then unreachable by `capture_key`
        let key = format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]);
        let compressed = zstd::encode_all(body, 3).expect("compress");
        st.put_capture_object(&key, axum::body::Bytes::from(compressed))
            .await
            .expect("store");
    }

    /// The masker finds a key in the logical bytes and records its range.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_masker_records_the_span_of_a_key() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"my key is AKIAIOSFODNN7EXAMPLE"}}
"#;
        let store = st.public_store().expect("a live backend");
        let (_session, generation) = seed(&store, "claude_code", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let n = mask_tick(&st, &tenant).await.expect("mask tick");
        assert_eq!(n, 1, "one generation should have been masked");

        let masks = store
            .masks_in_range(&generation, 0, i64::try_from(body.len()).expect("fits"))
            .await
            .expect("query");
        assert_eq!(masks.len(), 1, "the key should have produced one mask");
        assert_eq!(masks[0].rule, "vendor_key");
        assert_eq!(masks[0].scanner, SCANNER);
        assert!(
            masks[0].byte_offset > 0,
            "the mask must be at the key's offset, not at 0"
        );

        // A second tick has nothing to do
        let n = mask_tick(&st, &tenant).await.expect("second tick");
        assert_eq!(
            n, 0,
            "the second tick re-masked a generation that was already done"
        );

        db.drop().await;
    }

    /// The projector turns a session's chunks into events, with the text masked.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_projector_writes_masked_events() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        // Three records: a user message with a fake key, an assistant message,
        // and a record type the parser knows as Meta
        let body = br#"{"type":"user","message":{"role":"user","content":"my key is AKIAIOSFODNN7EXAMPLE"},"timestamp":"2026-10-29T08:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"noted"}]},"timestamp":"2026-10-29T08:00:01Z"}
{"type":"mode"}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, _generation) = seed(&store, "claude_code", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let n = project_tick(&st, &tenant).await.expect("project tick");
        assert_eq!(n, 3, "all three records should have become events");

        let events = store
            .transcript_page(&session, "claude_code@1", -1, 100)
            .await
            .expect("page");
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "the events must be in file order"
        );
        assert_eq!(events[0].kind, TranscriptEventKind::UserMessage);
        assert_eq!(events[1].kind, TranscriptEventKind::AgentMessage);
        assert_eq!(events[2].kind, TranscriptEventKind::Meta);

        // **The key is masked in the P layer.**
        let text = events[0].text.as_deref().expect("text");
        assert!(
            !text.contains("AKIA"),
            "the P layer still contains the key: {text}"
        );
        assert!(
            text.contains("[masked:"),
            "the P layer should carry the mask marker: {text}"
        );

        db.drop().await;
    }

    /// A second projector tick writes nothing new.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_second_projector_tick_is_idempotent() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, _generation) = seed(&store, "claude_code", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        project_tick(&st, &tenant).await.expect("first tick");
        let second = project_tick(&st, &tenant).await.expect("second tick");
        assert_eq!(
            second, 0,
            "the second tick projected the same session again; the projection would then have \
             duplicate events, or the unique constraint would turn a normal round into an error"
        );

        let events = store
            .transcript_page(&session, "claude_code@1", -1, 100)
            .await
            .expect("page");
        assert_eq!(events.len(), 1);

        db.drop().await;
    }

    /// Appending a chunk makes exactly the new records appear.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn an_appended_chunk_adds_only_the_new_events() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let first = br#"{"type":"user","message":{"role":"user","content":"one"}}
"#;
        let second = br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"two"}]}}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, generation) = seed(&store, "claude_code", first).await;
        store_object(&st, first).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        project_tick(&st, &tenant).await.expect("first tick");

        // Append the second record as a second chunk
        let end = i64::try_from(first.len()).expect("fits");
        let sha = {
            use sha2::Digest as _;
            format!("{:x}", sha2::Sha256::digest(second))
        };
        store
            .archive_txn(async |t| {
                t.append_chunk(&NewCaptureChunk {
                    id: Id::new(),
                    generation_id: generation.parse().expect("a ULID"),
                    byte_offset: end,
                    byte_length: i64::try_from(second.len()).expect("fits"),
                    content_sha256: sha.clone(),
                    storage_key: format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]),
                    encoding: ChunkEncoding::Zstd,
                    observed_at: chrono::Utc::now(),
                })
                .await
            })
            .await
            .expect("append");
        store_object(&st, second).await;

        project_tick(&st, &tenant).await.expect("second tick");

        let events = store
            .transcript_page(&session, "claude_code@1", -1, 100)
            .await
            .expect("page");
        assert_eq!(
            events.len(),
            2,
            "the appended record should be the only new event, got {} events",
            events.len()
        );
        assert_eq!(events[1].kind, TranscriptEventKind::AgentMessage);

        db.drop().await;
    }

    /// A session with no parser is skipped, not failed.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_session_with_no_parser_is_skipped() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = b"whatever\n";
        let store = st.public_store().expect("a live backend");
        let (_session, _generation) = seed(&store, "unknown_agent", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let n = project_tick(&st, &tenant).await.expect("tick");
        assert_eq!(
            n, 0,
            "a session with no parser should be skipped rather than failing the round"
        );

        db.drop().await;
    }

    /// The worker does not start on the mock backend.
    #[test]
    fn the_worker_does_not_start_on_the_mock_backend() {
        let src = include_str!("archive_workers.rs");
        let body = src
            .split("pub fn spawn(st: AppState) {")
            .nth(1)
            .expect("spawn is still here");
        let head = &body[..body.len().min(400)];
        assert!(
            head.contains("st.live()"),
            "spawn no longer checks for the mock backend; on a mock instance the workers would \
             spin against a database that does not exist"
        );
    }

    /// The indexer writes chunks for tool events only.
    ///
    /// Indexing the messages here as well would put every sentence in two
    /// indexes, and `scope=archive` would return the conversation with tool
    /// output mixed in — the noise D4 keeps out of the default search.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_indexer_indexes_tool_events_only() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"run the tests"},"timestamp":"2026-10-29T08:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]},"timestamp":"2026-10-29T08:00:01Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"test result: ok. 42 passed"}]},"timestamp":"2026-10-29T08:00:02Z"}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, _generation) = seed(&store, "claude_code", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        project_tick(&st, &tenant).await.expect("project");
        let chunks = index_tick(&tenant).await.expect("index");
        assert!(chunks > 0, "the tool events should have been indexed");

        // The tool result is findable in the archive
        let archive = store
            .search_archive(
                &cormex_memory::tokenize::to_tsquery_input("passed"),
                None,
                None,
                None,
                None,
                20,
            )
            .await
            .expect("archive search");
        assert_eq!(
            archive.len(),
            1,
            "the tool result should be in the archive index"
        );

        // The message is not
        let conversation = store
            .search_conversation(
                &cormex_memory::tokenize::to_tsquery_input("tests"),
                None,
                None,
                None,
                None,
                20,
            )
            .await
            .expect("conversation search");
        assert_eq!(
            conversation.len(),
            1,
            "the user message should be in the conversation index"
        );

        // A second tick has nothing to do
        let again = index_tick(&tenant).await.expect("second index");
        assert_eq!(
            again, 0,
            "the second tick re-indexed what was already there"
        );

        // The session id is the one we seeded
        assert_eq!(archive[0].session_id, session);

        db.drop().await;
    }

    /// A long tool output is split on a **character** boundary.
    ///
    /// A byte boundary can land inside a multi-byte character, and the result
    /// is not valid UTF-8 — which `to_tsvector` then rejects, turning one long
    /// tool output into a failed round.
    #[test]
    fn a_long_output_is_split_on_a_char_boundary() {
        let text = "对".repeat(3000); // 3 bytes each, 9000 bytes
        let pieces = split_chunks(&text);
        assert!(pieces.len() > 1, "a 9 KB text should have been split");
        for p in &pieces {
            assert!(
                std::str::from_utf8(p.as_bytes()).is_ok(),
                "a piece is not valid UTF-8: a byte boundary landed inside a character"
            );
        }
        assert_eq!(pieces.concat(), text, "the pieces must reassemble");
    }

    #[test]
    fn a_short_output_is_one_piece() {
        assert_eq!(split_chunks("short"), vec!["short".to_owned()]);
    }

    // ── promotion ──────────────────────────────────────────

    /// The policy: every user message, and only the **last** agent message of
    /// each turn.
    ///
    /// Promoting every agent message would be several extraction calls to say
    /// one thing; promoting none of them would lose the conclusions.
    #[test]
    fn the_promotion_policy_takes_users_and_the_last_agent_message() {
        use TranscriptEventKind as K;
        let kinds = [
            K::UserMessage,
            K::AgentMessage,
            K::ToolCall,
            K::AgentMessage,
            K::ToolResult,
            K::UserMessage,
            K::AgentMessage,
        ];
        let events: Vec<TranscriptEvent> = kinds
            .iter()
            .enumerate()
            .map(|(i, k)| fake_event(i as i64, *k))
            .collect();
        let picked: Vec<bool> = (0..events.len())
            .map(|i| should_promote(&events, i))
            .collect();
        assert_eq!(
            picked,
            vec![true, false, false, true, false, true, true],
            "user messages always promote; of two agent messages in a turn only the last one \
             does; tool calls and results never do"
        );
    }

    /// A trailing agent message with no user after it is still the turn's last.
    #[test]
    fn a_lone_agent_message_is_promoted() {
        use TranscriptEventKind as K;
        let events = vec![fake_event(0, K::AgentMessage)];
        assert!(should_promote(&events, 0));
    }

    fn fake_event(seq: i64, kind: TranscriptEventKind) -> TranscriptEvent {
        TranscriptEvent {
            id: Id::new().to_string(),
            session_id: Id::new().to_string(),
            parser_version: "claude_code@1".into(),
            seq,
            kind,
            occurred_at: Some(chrono::Utc::now()),
            content: serde_json::json!([]),
            text: Some(format!("event {seq}")),
            tool_name: None,
            tool_call_id: None,
            src_generation_id: Id::new().to_string(),
            src_offset: 0,
            src_length: 0,
            created_at: chrono::Utc::now(),
        }
    }

    /// A sink that writes one fact per episode, so `memory_search` can find it.
    ///
    /// Writing a real fact (rather than counting calls) is the point: the
    /// acceptance is "the extracted facts can be found by `memory_search`", and
    /// a counter would pass even if the write went to the wrong schema.
    struct FakeFactSink {
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl FakeFactSink {
        fn new() -> Self {
            Self {
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> usize {
            self.seen.lock().expect("not poisoned").len()
        }
    }

    #[async_trait::async_trait]
    impl FactSink for FakeFactSink {
        async fn extract(
            &self,
            store: &Store,
            text: &str,
            ctx: &cormex_memory::extract::ExtractContext,
        ) -> anyhow::Result<()> {
            self.seen
                .lock()
                .expect("not poisoned")
                .push(text.to_owned());
            // The same shape the real extractor writes: an entity, a fact with
            // its embedding, and a fact_event
            let entity = cormex_store::NewEntity {
                id: Id::new(),
                kind: "concept".into(),
                name: "RustFS".into(),
                summary: None,
                embedding: Some(cormex_store::Vector::from(vec![0.1f32; 1024])),
                embedding_model: "test-model".into(),
                device_id: SERVER_DEVICE.into(),
            };
            let entity_id = entity.id;
            let statement = format!("cormex 使用 RustFS（{text}）");
            let fact = cormex_store::NewFact {
                id: Id::new(),
                subject_id: entity_id,
                predicate: "uses".into(),
                object_text: Some("RustFS".into()),
                object_entity_id: None,
                statement: statement.clone(),
                embedding: Some(cormex_store::Vector::from(vec![0.1f32; 1024])),
                embedding_model: "test-model".into(),
                domain: None,
                confidence: 1.0,
                valid_at: None,
                source_episode_id: ctx.episode_id,
                tsv_source: Some(cormex_memory::tokenize::to_tsvector_input(&statement)),
                source: cormex_store::FactSource::Conversation,
                derived_from: Vec::new(),
                extracted_by: "fake/test".into(),
                device_id: SERVER_DEVICE.into(),
            };
            store
                .write_txn(async |t| {
                    t.insert_entity(&entity).await?;
                    t.insert_fact(&fact).await
                })
                .await?;
            Ok(())
        }
    }

    /// The same event is never promoted twice.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn promotion_is_idempotent() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        let body = br#"{"type":"user","message":{"role":"user","content":"we use RustFS"}}
"#;
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;

        let sink = FakeFactSink::new();
        let first = promote_round(&st, &tenant, &sink)
            .await
            .expect("first round");
        assert_eq!(first, 1, "one user message should have been promoted");
        assert_eq!(sink.calls(), 1);

        // Lose the progress row, as a crash between the write and the progress
        // insert would
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        sqlx::query("DELETE FROM promote_progress")
            .execute(&pool)
            .await
            .expect("drop the progress");
        pool.close().await;

        let second = promote_round(&st, &tenant, &sink)
            .await
            .expect("second round");
        assert_eq!(
            second, 0,
            "the event was promoted again after the progress row was lost -- its derived id is \
             what makes a re-run safe"
        );

        // And there is exactly one episode for it
        let episodes: Vec<(String,)> = {
            let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
            let rows = sqlx::query_as("SELECT id::text FROM episodes")
                .fetch_all(&pool)
                .await
                .expect("episodes");
            pool.close().await;
            rows
        };
        assert_eq!(episodes.len(), 1, "duplicate episodes: {episodes:?}");

        db.drop().await;
    }

    /// A second round with the progress row intact does nothing.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_second_promotion_round_does_nothing() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        let body = br#"{"type":"user","message":{"role":"user","content":"we use RustFS"}}
"#;
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;

        let sink = FakeFactSink::new();
        promote_round(&st, &tenant, &sink).await.expect("first");
        let again = promote_round(&st, &tenant, &sink).await.expect("second");
        assert_eq!(again, 0);
        assert_eq!(sink.calls(), 1, "the extractor ran twice for one event");

        db.drop().await;
    }

    /// The facts the promotion produced are findable by `memory_search`.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn promoted_facts_are_findable_by_memory_search() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        let body =
            br#"{"type":"user","message":{"role":"user","content":"we use RustFS for objects"}}
"#;
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;

        let sink = FakeFactSink::new();
        promote_round(&st, &tenant, &sink).await.expect("promote");

        let text = st
            .memory_search_text(&tenant, "RustFS", None, 10)
            .await
            .expect("memory search");
        assert!(
            text.contains("RustFS"),
            "the promoted fact is not reachable through memory_search: {text}"
        );

        db.drop().await;
    }

    /// An event whose text still carries a credential is not promoted.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_credential_is_not_promoted() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;

        // A scanner rule that arrived after this session was projected
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        sqlx::query(
            "UPDATE transcript_events SET text = 'my key is sk-abcdef0123456789abcdef0123456789'",
        )
        .execute(&pool)
        .await
        .expect("plant");
        pool.close().await;

        let sink = FakeFactSink::new();
        let promoted = promote_round(&st, &tenant, &sink).await.expect("promote");
        assert_eq!(
            promoted, 0,
            "an event carrying a credential was promoted; the raw text would then cross the \
             network on its way to the extractor"
        );
        assert_eq!(sink.calls(), 0);

        db.drop().await;
    }

    // ── the two time axes (D13) ────────────────────────────

    /// A **backfilled** session is found by `at` and not by `as_of`.
    ///
    /// This is the card's acceptance, and it is the whole reason the two axes
    /// exist. The session's events happened in June (`occurred_at`), but Cormex
    /// only learned about them today (`created_at`, set by the projector). So:
    ///
    /// - `at = July` finds it: the conversation had happened by then
    /// - `as_of = July` does **not**: Cormex had not been told about it yet
    ///
    /// Getting this backwards would make a history import look like it was
    /// always there, and "what did I know in June" would return sessions from
    /// June that were imported last week.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_backfilled_session_is_found_by_at_and_not_by_as_of() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        // The transcript says June; the projector writes it today
        let body = br#"{"type":"user","message":{"role":"user","content":"the storage decision"},"timestamp":"2026-06-01T09:00:00Z"}
"#;
        let (session, _tenant) = seed_projected_session(&st, &store, body).await;

        let july = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 7, 1, 0, 0, 0).unwrap();
        let found_at = store
            .archive_sessions_at(None, None, None, None, Some(july), None, None, 10)
            .await
            .expect("at query");
        assert_eq!(
            found_at.len(),
            1,
            "at=July should find a session whose conversation happened in June"
        );
        assert_eq!(found_at[0].id, session);

        let found_as_of = store
            .archive_sessions_at(None, None, None, None, None, Some(july), None, 10)
            .await
            .expect("as_of query");
        assert!(
            found_as_of.is_empty(),
            "as_of=July found a session Cormex only learned about today -- the two axes have \
             been collapsed into one"
        );

        // The events page agrees
        let events_at = store
            .transcript_page_at(&session, "claude_code@1", -1, Some(july), None, 10)
            .await
            .expect("at page");
        assert_eq!(events_at.len(), 1);
        let events_as_of = store
            .transcript_page_at(&session, "claude_code@1", -1, None, Some(july), 10)
            .await
            .expect("as_of page");
        assert!(
            events_as_of.is_empty(),
            "the event page collapsed the two axes"
        );

        db.drop().await;
    }

    /// Both axes at once means both apply.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn both_axes_apply_together() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        let body = br#"{"type":"user","message":{"role":"user","content":"the storage decision"},"timestamp":"2026-06-01T09:00:00Z"}
"#;
        let (session, _tenant) = seed_projected_session(&st, &store, body).await;

        let july = chrono::TimeZone::with_ymd_and_hms(&chrono::Utc, 2026, 7, 1, 0, 0, 0).unwrap();
        // `at` matches, `as_of` does not: the intersection is empty
        let both = store
            .archive_sessions_at(None, None, None, None, Some(july), Some(july), None, 10)
            .await
            .expect("both");
        assert!(
            both.is_empty(),
            "both axes given must mean both apply; this returned the session, so one of them \
             was ignored"
        );

        // And with `as_of` in the future both match
        let future = chrono::Utc::now() + chrono::Duration::days(1);
        let both = store
            .archive_sessions_at(None, None, None, None, Some(july), Some(future), None, 10)
            .await
            .expect("both");
        assert_eq!(both.len(), 1, "session {session}");

        db.drop().await;
    }

    // ── conversation vectors (P5-6) ────────────────────────

    /// The vector round does nothing while the feature is off.
    ///
    /// It is off by default because the card's acceptance is a measured
    /// improvement, and a feature that spends an embedding call per turn before
    /// that number exists is a cost with no evidence behind it.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_vector_round_is_off_by_default() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;

        let written = embed_tick(&st, &tenant).await.expect("embed tick");
        assert_eq!(
            written, 0,
            "the vector round wrote vectors while the feature was off"
        );

        db.drop().await;
    }

    /// With the feature on, conversation turns get vectors and tool output does
    /// not.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_vector_round_covers_conversation_turns_only() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        // One user message and one tool result
        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_result","tool_use_id":"t1","content":"build output"}]}}
"#;
        let (session, tenant) = seed_projected_session(&st, &store, body).await;

        let on = st.with_session_embeddings(true);
        let written = embed_tick(&on, &tenant).await.expect("embed tick");
        assert_eq!(
            written, 1,
            "only the user message should have been embedded; a tool result has no semantic \
             content worth a vector"
        );

        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        let kinds: Vec<(String,)> = sqlx::query_as(
            "SELECT e.kind FROM transcript_embeddings v
               JOIN transcript_events e ON e.id = v.event_id
              WHERE e.session_id = $1",
        )
        .bind(&session)
        .fetch_all(&pool)
        .await
        .expect("vectors");
        pool.close().await;
        assert_eq!(
            kinds.into_iter().map(|(k,)| k).collect::<Vec<_>>(),
            vec!["user_message".to_owned()]
        );

        // A second round has nothing to do
        let again = embed_tick(&on, &tenant).await.expect("second round");
        assert_eq!(
            again, 0,
            "the second round re-embedded what was already there"
        );

        db.drop().await;
    }

    /// Promotion can be turned off.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn promotion_can_be_disabled() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().expect("a live backend");

        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;

        let off = st.with_promotion(false);
        let sink = FakeFactSink::new();
        let promoted = promote_round(&off, &tenant, &sink).await.expect("promote");
        assert_eq!(promoted, 0, "promotion ran while it was turned off");
        assert_eq!(sink.calls(), 0);

        db.drop().await;
    }

    // ── the distiller ──────────────────────────────────────

    /// An observer that answers with a fixed observation citing event #1.
    ///
    /// The tick's real logic — when it fires, what it passes in, where the
    /// output lands, when the budget stops it — is what these tests measure. A
    /// real provider would make them measure the model's mood instead, and they
    /// could not run in CI at all.
    struct FakeObserver {
        entries: Vec<cormex_memory::distill::Observation>,
        seen: std::sync::Mutex<Vec<usize>>,
    }

    impl FakeObserver {
        fn new(entries: Vec<cormex_memory::distill::Observation>) -> Self {
            Self {
                entries,
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Observer for FakeObserver {
        async fn observe(
            &self,
            _session_id: &str,
            events: &[DistillEvent],
        ) -> cormex_core::Result<cormex_memory::distill::ObservationOutcome> {
            self.seen.lock().expect("not poisoned").push(events.len());
            Ok(cormex_memory::distill::ObservationOutcome {
                entries: self.entries.clone(),
                input_tokens: 100,
                output_tokens: 20,
                model: "fake".into(),
            })
        }
    }

    /// Make every event look an hour old, so the idle gate opens.
    ///
    /// `transcript_events.created_at` is `clock_timestamp()`, and a test cannot
    /// wait thirty minutes. This is the one place a test writes an `UPDATE` —
    /// it is simulating the passage of time, not changing a fact.
    async fn age_events(db: &crate::test_support::FreshDb) {
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        sqlx::query(
            "UPDATE transcript_events SET created_at = clock_timestamp() - interval '1 hour'",
        )
        .execute(&pool)
        .await
        .expect("backdate");
        pool.close().await;
    }

    async fn seed_projected_session(st: &AppState, store: &Store, body: &[u8]) -> (String, Tenant) {
        let (session, _generation) = seed(store, "claude_code", body).await;
        store_object(st, body).await;
        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: std::sync::Arc::new(store.clone()),
        };
        project_tick(st, &tenant).await.expect("project tick");
        (session, tenant)
    }

    /// An idle session produces a summary with a `transcript_event` edge.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_distiller_writes_observations_with_provenance() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body =
            br#"{"type":"user","message":{"role":"user","content":"we decided to use RustFS"}}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, tenant) = seed_projected_session(&st, &store, body).await;
        age_events(&db).await;

        let events = store
            .events_after_seq(&session, -1, 10)
            .await
            .expect("events");
        assert_eq!(events.len(), 1, "the fixture should have one event");

        let observer = FakeObserver::new(vec![cormex_memory::distill::Observation {
            at: chrono::Utc::now(),
            text: "决定用 RustFS".into(),
            sources: vec![events[0].id.parse().expect("a ULID")],
        }]);
        let written = distill_round(&st, &tenant, &observer)
            .await
            .expect("distill round");
        assert_eq!(written, 1, "one observation should have been written");

        let summaries = store
            .summaries_for_scope(cormex_store::SummaryScope::Session, &session, 10)
            .await
            .expect("summaries");
        assert_eq!(summaries.len(), 1, "the observation should be a summary");
        assert_eq!(summaries[0].text, "决定用 RustFS");

        // **The edge is what makes the summary findable after a purge.**
        let edges: Vec<String> = {
            let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
            let rows: Vec<(String,)> =
                sqlx::query_as("SELECT source_kind FROM derivations WHERE derived_id = $1")
                    .bind(&summaries[0].id)
                    .fetch_all(&pool)
                    .await
                    .expect("derivations");
            pool.close().await;
            rows.into_iter().map(|(k,)| k).collect()
        };
        assert_eq!(
            edges,
            vec!["transcript_event".to_owned()],
            "the summary must cite the event it was drawn from"
        );

        // A second round has nothing to do: progress advanced past the event
        let again = distill_round(&st, &tenant, &observer)
            .await
            .expect("second round");
        assert_eq!(
            again, 0,
            "the second round re-distilled events that were already processed"
        );

        db.drop().await;
    }

    /// A session that is still being written to is left alone.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_session_that_is_not_idle_is_not_distilled() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let store = st.public_store().expect("a live backend");
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;

        let observer = FakeObserver::new(Vec::new());
        let written = distill_round(&st, &tenant, &observer)
            .await
            .expect("distill round");
        assert_eq!(
            written, 0,
            "a session projected seconds ago was distilled; the observation would be about a \
             thought that is still being finished"
        );
        assert!(
            observer.seen.lock().expect("not poisoned").is_empty(),
            "the model was called for a session that is not idle"
        );

        db.drop().await;
    }

    /// A spent budget stops the round **before** the model call.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_daily_budget_stops_the_round_before_the_model_call() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let store = st.public_store().expect("a live backend");
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;
        age_events(&db).await;

        // An account owns `public`, and it has already spent more than the
        // default daily budget on distillation
        let user = Id::new().to_string();
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        sqlx::query(
            "INSERT INTO cormex_auth.users (id, username, password_hash, schema_name)
             VALUES ($1, 'budget-test', 'x', 'public')",
        )
        .bind(&user)
        .execute(&pool)
        .await
        .expect("insert the account");
        sqlx::query(
            "INSERT INTO cormex_auth.usage (id, user_id, kind, input_tokens, output_tokens)
             VALUES ($1, $2, 'distill', $3, 0)",
        )
        .bind(Id::new().to_string())
        .bind(&user)
        .bind(cormex_core::config::DEFAULT_DISTILL_DAILY_TOKENS)
        .execute(&pool)
        .await
        .expect("insert the usage");
        pool.close().await;

        let observer = FakeObserver::new(Vec::new());
        let written = distill_round(&st, &tenant, &observer)
            .await
            .expect("distill round");
        assert_eq!(written, 0, "a spent budget must write nothing");
        assert!(
            observer.seen.lock().expect("not poisoned").is_empty(),
            "the model was called after the budget was spent -- the check has to come before \
             the call, or the last call always overshoots"
        );

        db.drop().await;
    }

    /// A batch that still contains a credential after masking is skipped, and
    /// progress still advances.
    ///
    /// Advancing is the point: re-reading the same batch every round would log
    /// the warning forever and never get past it.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_credential_in_the_batch_is_skipped_and_progress_advances() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let store = st.public_store().expect("a live backend");
        let (_session, tenant) = seed_projected_session(&st, &store, body).await;
        age_events(&db).await;

        // Simulate a scanner rule that arrived after this session was
        // projected: the P layer's text is what the distiller reads, and it is
        // not masked for a rule that did not exist when it was written
        let pool = sqlx::PgPool::connect(&db.url).await.expect("connect");
        sqlx::query(
            "UPDATE transcript_events SET text = 'token sk-abcdef0123456789abcdef0123456789'",
        )
        .execute(&pool)
        .await
        .expect("plant a credential");
        pool.close().await;

        let observer = FakeObserver::new(Vec::new());
        let written = distill_round(&st, &tenant, &observer)
            .await
            .expect("distill round");
        assert_eq!(written, 0, "nothing should have been written");
        assert!(
            observer.seen.lock().expect("not poisoned").is_empty(),
            "the text crossed the network despite the scan"
        );

        // Progress advanced, so the next round does not re-read the same batch
        let again = distill_round(&st, &tenant, &observer)
            .await
            .expect("second round");
        assert_eq!(again, 0);
        assert!(
            observer.seen.lock().expect("not poisoned").is_empty(),
            "the second round re-read the batch it had already skipped"
        );

        db.drop().await;
    }
}

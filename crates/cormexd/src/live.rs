//! The live backend — wiring the storage layer, the provider layer and the
//! memory engine onto the HTTP contract.
//!
//! Its contract is identical to the mock backend's
//! ([`crate::state::BackendChoice::Mock`]): routes and DTOs do not change when
//! the backend is switched.
//!
//! # There is **no** agent loop here
//!
//! This is a memory service, and it **does not run conversations**. The agent is
//! always another process — it comes in over HTTP holding a delegated
//! credential (see [`crate::delegated_token`]): `/llm/stream` to borrow the
//! model, `/episodes` to write memory, `/mcp` to query memory. Who that agent
//! is, which machine it runs on, whether there is a container — this process
//! knows none of it, and does not need to.
//!
//! There used to be an in-process loop here as well, reached only when a
//! deployment could not attach to docker. The reason for deleting it was not
//! that it had bugs, but that **it was a second implementation**: the same
//! `Turn::run` assembled in two places, so every added tool and every change to
//! injection discipline had to be remembered twice, and the one that was missed
//! made no test go red — the two paths served different populations of users.
//! More importantly: with the agent evicted, our own agent takes the same path a
//! third party takes (`/llm/stream` to borrow the model, `/episodes` to write
//! memory, `/mcp` to query memory), and only then does that API stay healthy.
//!
//! # What one conversational turn amounts to here
//!
//! ```text
//! agent finishes a turn → POST /episodes
//!   ├─ 1. land L0: episodes + sync_log in one txn (serialized by advisory lock)
//!   ├─ 2. four retrieval paths + RRF + budget truncation
//!   ├─ 3. render the injection block back to the agent (memory is data, not instructions)
//!   └─ 4. extract facts asynchronously (never blocking the response)
//! ```
//!
//! Step 4 has to be asynchronous: extraction makes one LLM call, doing it
//! synchronously would add seconds to every turn, and the extraction result is
//! of no use whatsoever to **this** turn — it is preparation for the next one.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use cormex_core::{Config, CormexError, Id, Result};
use cormex_llm::{LlmClient, MessageStream};
use cormex_memory::{
    Retrieved, Retriever,
    embed::SharedEmbedder,
    extract::{ExtractContext, Extractor, TurnOrigin},
    injection,
};
use cormex_store::{NewEpisode, Role, Store};

use crate::dto::*;
use cormex_proto::episodes::{EpisodeAck, NewEpisodeRequest};
use cormex_proto::llm::{LlmStreamRequest, ModelTier};

/// What to do with the `workspace` field in `PATCH /sessions`.
///
/// Returns `true` = unbind; `false` = the field was not mentioned this time.
/// Binding always errors.
///
/// # Why binding is **refused** rather than silently ignored
///
/// This path used to be the web client's fallback for binding a workspace (when
/// the local agent was absent), and what it bound was a directory **on the
/// server** — so a remote user's `read_file` touched the production machine's
/// filesystem, with a blast radius of the whole machine plus every tenant's
/// data.
///
/// Silently ignoring it would leave the client displaying "bound to
/// D:\myproject" and then failing every file operation inexplicably — while the
/// user plainly saw the binding succeed. The error message names the two paths
/// that do work.
///
/// # Why unbinding is still permitted
///
/// An older session may still carry a server-side binding (stored before this
/// change). Refusing to unbind would weld that record in place forever, leaving
/// the user nothing to do but delete the entire session.
///
/// # Why this is factored into a function
///
/// `patch_session` needs a real database to be reached at all, so this decision
/// cannot be tested there. And it is precisely the decision in this change most
/// in need of a test — inverted, it means **the server can bind again**, with
/// no symptom whatsoever.
pub(crate) fn workspace_patch(field: Option<Option<&str>>) -> Result<bool> {
    match field {
        Some(Some(_)) => Err(CormexError::Invalid(
            "the cormexd process provides no file execution environment of its own, so a \
             host path cannot be bound here. Files and commands have two routes: run on \
             **your own machine** (the desktop client, or run cormex-local locally — the \
             `cormex` command line starts it for you), or turn on the **cloud sandbox** \
             — that turn's workspace is /workspace inside the container, managed by the \
             server, which neither needs nor accepts a path from outside."
                .into(),
        )),
        Some(None) => Ok(true),
        None => Ok(false),
    }
}

/// Validate a project name, returning the trimmed version.
///
/// # Why this is a standalone pure function
///
/// Same reasoning as [`workspace_patch`]: the real project path needs a database
/// to be reached at all, so this decision cannot be tested there. It is also
/// reused by the mock backend — the client CI's assertion "an empty name is
/// rejected" has to hold on mock too, or it tests a contract looser than the
/// live backend's.
///
/// # Why emptiness is checked after trimming rather than directly
///
/// Typing one space into the input box would otherwise create a project whose
/// name is blank, which in the sidebar is a clickable row containing nothing —
/// it looks like the UI is broken.
///
/// The ceiling is in **characters**, not bytes: the CHECK in the database uses
/// `length()`, which also counts characters. The symptom of the two disagreeing
/// is "it clearly is not over the limit, yet the server returns 500".
pub(crate) fn validate_project_name(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(CormexError::Invalid(
            "a project name must not be blank".into(),
        ));
    }
    if name.chars().count() > cormex_store::PROJECT_NAME_MAX_CHARS {
        return Err(CormexError::Invalid(format!(
            "the project name is too long (the ceiling is {} characters)",
            cormex_store::PROJECT_NAME_MAX_CHARS
        )));
    }
    Ok(name.to_owned())
}

/// How many extractions may run at once. The gate it sizes is
/// [`Live::extract_gate`]; [`under_gate`] is what holds a permit against it.
const EXTRACT_CONCURRENCY_ENV: &str = "CORMEX_EXTRACT_CONCURRENCY";

/// Defaults to 3.
///
/// Not derived from the CPU core count: extraction's bottleneck is **the
/// provider's end**, and locally it costs almost no CPU (it waits on one HTTP
/// round trip). Deriving from core count would release dozens of concurrent
/// extractions on a large machine, running straight into the provider's rate
/// limit — which manifests as a batch of 429s, and every 429 is one turn's
/// memory lost.
///
/// 3 is conservative: during normal chat nothing ever queues (a human types far
/// slower than that), while during bulk writes it flattens the peak. Raise it if
/// it is not enough; that is an explicit operational act.
const DEFAULT_EXTRACT_CONCURRENCY: usize = 3;

/// Read and parse [`EXTRACT_CONCURRENCY_ENV`].
fn extract_concurrency() -> Result<usize> {
    parse_extract_concurrency(
        std::env::var(EXTRACT_CONCURRENCY_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .as_deref(),
    )
}

/// The same discipline as `MAX_ROUNDS_ENV`: an invalid value errors rather than
/// quietly using the default — running on with a misconfiguration means the
/// ceiling is not in effect while ops has no idea.
///
/// 0 counts as invalid too: that does not mean "unlimited", it means "no
/// extraction may run at all", and turning extraction off deserves a switch of
/// its own.
///
/// A standalone pure function (rather than reading the environment variable
/// directly) so it can be tested: `env::set_var` mutates process-global state,
/// and other tests in the same binary get trampled along with it.
fn parse_extract_concurrency(raw: Option<&str>) -> Result<usize> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_EXTRACT_CONCURRENCY);
    };
    match raw.trim().parse::<usize>() {
        Ok(0) | Err(_) => Err(CormexError::Config(format!(
            "{EXTRACT_CONCURRENCY_ENV} must be a positive integer; it is {raw:?}"
        ))),
        Ok(n) => Ok(n),
    }
}

/// Run one extraction under the gate. **The permit is held until it finishes.**
///
/// This is its own function because it contains something very easy to get
/// wrong, and **everything stays green** once it is wrong:
///
/// ```ignore
/// let _ = gate.acquire_owned().await;   // ← the permit is dropped immediately; the gate does nothing
/// let _permit = gate.acquire_owned().await;  // ← this is the correct one
/// ```
///
/// `_` means "discard immediately"; `_permit` means "bind it and do not use it".
/// The two differ by a few characters, and the former turns the whole semaphore
/// into an expensive no-op — no test goes red over it unless somebody
/// deliberately tests the concurrency level. Gathered into this one function,
/// that test only has to be written once.
async fn under_gate<F: Future>(gate: &Arc<tokio::sync::Semaphore>, fut: F) -> F::Output {
    // acquire only fails when the semaphore is closed, and we never close it.
    // Even if it did fail, carry on: extraction should not stop entirely just
    // because the gate broke
    let _permit = Arc::clone(gate).acquire_owned().await.ok();
    fut.await
}

/// How many sessions one listing returns.
///
/// The ceiling applies to **sessions**, not messages — the storage layer
/// aggregates in a single SQL statement, rather than "pull the most recent N
/// messages and then group them" (under which one talkative session could fill
/// the entire window and every other session would simply vanish from the list).
const SESSION_LIST_LIMIT: i64 = 200;

/// How many characters of the first user message a session title takes.
const TITLE_CHARS: usize = 24;

/// Cloning is cheap — every field is either an `Arc`, a handle with `Arc`
/// semantics (`Store` is a connection pool internally), or a scalar.
/// [`Live::bind`] relies on that to swap databases per request.
#[derive(Clone)]
pub struct Live {
    store: Store,
    llm: LlmClient,
    retriever: Retriever<SharedEmbedder>,
    extractor: Arc<Extractor>,
    /// Media transcription. `None` when no vision model is configured — images
    /// are still archived, they simply cannot be retrieved
    transcribe: Option<Arc<cormex_memory::TranscribePipeline>>,
    device_id: String,
    context_window: usize,
    /// The current vectorization backend.
    ///
    /// Kept as its own field rather than dug out of `retriever`: the
    /// model-change backfiller needs it to compute vectors, and `/health`'s
    /// census needs its `model_id()` for comparison. `SharedEmbedder` is an
    /// `Arc`, so one more handle is not one more model.
    embedder: SharedEmbedder,
    /// How many extractions may run at once. Usage in [`under_gate`].
    ///
    /// # Why this gate is mandatory
    ///
    /// Extraction is a **bare `tokio::spawn`**: one task per `/episodes`, none
    /// of them aware of the others. During normal chat that is fine (human
    /// typing speed is a natural rate limit), but the moment one client writes
    /// rapidly and continuously — a history import being the archetype — that
    /// is thousands of concurrent tasks in an instant, each making one LLM call.
    ///
    /// The production machine is 2 cores and 3.5 GB with a dozen containers
    /// already running. Thousands of concurrent extractions are not "a bit
    /// slower", they drag the whole machine down along with every other service
    /// on it.
    ///
    /// # Why the permit is awaited **after** the spawn
    ///
    /// Awaiting it before the spawn would block `/episodes`'s response behind
    /// the extraction queue — and the entire design premise of extraction is
    /// "never block the caller; it is preparation for the next turn". As it
    /// stands, what queues is those background tasks themselves, and the caller
    /// still receives its response immediately.
    ///
    /// State the cost plainly: **this is not backpressure**. When writes come in
    /// too fast the queue keeps growing, it merely grows as parked tasks in
    /// memory. Real throttling has to be done by the client (the importer does
    /// exactly that); what this gate protects is CPU and the provider's quota.
    extract_gate: Arc<tokio::sync::Semaphore>,
}

/// A [`Live`] that has already been bound to a tenant.
///
/// # Its entire reason to exist: make "forgot to bind" fail to compile
///
/// The ten methods that read and write data are **module-private** on `Live`,
/// and only this type forwards them out. So `state.rs`, holding a
/// `Backend::Live(l)`, still cannot call `l.get_episode(…)` — it has to call
/// `l.bind(tenant.store()?)` first.
///
/// This is not fastidiousness. In the previous version `TenantPools` was built,
/// its isolation was tested, and fault injection was performed — and yet
/// **nothing called it**: the request path kept holding the startup `Store`
/// connected to `public`, so two logged-in users read and wrote the same
/// database. And in that state every test was green: no test naturally covers
/// "which connection pool does this method use".
///
/// The type system covers it.
pub struct BoundLive(Arc<Live>);

impl BoundLive {
    /// Lend out the inner value.
    ///
    /// For **background jobs**: they call tenant-independent public methods such
    /// as `store()` / `embedder()` / `transcribe_and_store()`, and those live on
    /// `Live`. In this bound copy the `store` field already points at that
    /// tenant.
    ///
    /// The request path must **not** use it to bypass the forwarding layer —
    /// that hands back the guarantee "you cannot call it without binding". The
    /// guard test in `state.rs` watches precisely for this.
    #[must_use]
    pub fn as_live(&self) -> &Arc<Live> {
        &self.0
    }

    // Everything below is forwarding. The signatures are character for character
    // the same as `Live`'s, with one layer added.
    //
    // `Deref<Target = Live>` was considered to save this pile — it does not
    // work: it would make `Live`'s methods directly callable again, and "you
    // cannot call it without binding" is this type's only output. The forwarding
    // is the price of that guarantee.

    pub async fn write_episode(
        &self,
        req: NewEpisodeRequest,
        origin: TurnOrigin,
    ) -> Result<EpisodeAck> {
        self.0.write_episode(req, origin).await
    }

    pub async fn register_blob(&self, new: cormex_store::NewBlob) -> Result<Option<i64>> {
        self.0.register_blob(new).await
    }

    pub async fn blob_meta(&self, hash: &str) -> Result<cormex_store::Blob> {
        self.0.blob_meta(hash).await
    }

    pub async fn memory_search(&self, q: MemorySearchQuery) -> Result<MemorySearchResponse> {
        self.0.memory_search(q).await
    }

    pub async fn memory_search_text(
        &self,
        query: &str,
        as_of: Option<&str>,
        limit: i64,
    ) -> Result<String> {
        self.0.memory_search_text(query, as_of, limit).await
    }

    pub async fn profile_text(&self, limit: i64) -> Result<String> {
        self.0.profile_text(limit).await
    }

    pub async fn get_episode(&self, id: &str) -> Result<EpisodeDto> {
        self.0.get_episode(id).await
    }

    pub async fn list_sessions(&self, q: &ListSessionsQuery) -> Result<Vec<SessionDto>> {
        self.0.list_sessions(q).await
    }

    pub async fn list_projects(&self) -> Result<Vec<ProjectDto>> {
        self.0.list_projects().await
    }

    pub async fn create_project(&self, name: &str) -> Result<ProjectDto> {
        self.0.create_project(name).await
    }

    pub async fn patch_project(&self, id: &str, patch: ProjectPatch) -> Result<ProjectDto> {
        self.0.patch_project(id, patch).await
    }

    pub async fn delete_project(&self, id: &str) -> Result<()> {
        self.0.delete_project(id).await
    }

    pub async fn patch_session(&self, session_id: &str, patch: SessionPatch) -> Result<SessionDto> {
        self.0.patch_session(session_id, patch).await
    }

    pub async fn session_detail(
        &self,
        session_id: &str,
        q: &SessionDetailQuery,
    ) -> Result<SessionDetail> {
        self.0.session_detail(session_id, q).await
    }

    pub async fn sync_since(&self, q: SyncQuery) -> Result<SyncResponse> {
        self.0.sync_since(q).await
    }
}

/// How one transcription ended.
///
/// One variant more than [`cormex_memory::transcribe::Outcome`] (`Disabled`) and
/// without that large pile of pending rows — all the caller wants is "did it
/// work, and should it be retried". The rescanner uses it to distinguish "it
/// failed this time" from "this can never be transcribed".
#[derive(Debug)]
pub enum TranscribeOutcome {
    /// Transcription completed and was persisted
    Stored { segments: usize },
    /// This blob has been redacted, so the whole result is discarded. **Not a
    /// failure, and must never be retried**
    Redacted,
    /// No transcriber claims this MIME (currently mostly audio and video).
    /// Rerunning once ASR lands is fine, but until then any number of retries
    /// produces the same result
    Unsupported { mime: String },
    /// No vision model configured; the whole path is off
    Disabled,
}

impl TranscribeOutcome {
    fn log(&self, hash: &str) {
        match self {
            Self::Stored { segments } => {
                tracing::info!(hash, segments, "the media transcript was persisted")
            }
            Self::Redacted => tracing::info!(
                hash,
                "the blob was redacted while it was being transcribed; the result was discarded"
            ),
            Self::Unsupported { mime } => {
                tracing::debug!(hash, mime, "no transcriber claims this type")
            }
            Self::Disabled => {
                tracing::debug!(hash, "no vision model configured; skipping transcription")
            }
        }
    }
}

/// Wire `store.is_redacted` into the transcription pipeline's
/// [`cormex_memory::transcribe::RedactionGuard`].
///
/// The pipeline builds this constraint into its signature (taking a trait rather
/// than a closure), so the caller has no way to omit it — the consequence of
/// omitting it is that a transcription task already in flight when a redaction
/// executes writes the erased content back into `blob_transcripts` afterwards
/// (the in-flight task protection in memory.md §11).
struct StoreRedactionGuard {
    store: Store,
}

#[async_trait::async_trait]
impl cormex_memory::transcribe::RedactionGuard for StoreRedactionGuard {
    async fn is_redacted(&self, blob_hash: &str) -> Result<bool> {
        self.store
            .is_redacted(cormex_store::RedactionTarget::Blob, blob_hash)
            .await
            .map_err(store_err)
    }
}

impl Live {
    pub async fn new(config: &Config, embedder: SharedEmbedder) -> Result<Self> {
        // The global set (cormex_auth) runs first: the account tables must be in
        // place before anybody can log in.
        //
        // It and the tenant set are **two independent migration sets**, with
        // their version tables in their own schemas — merging them into one
        // makes a new tenant's account creation run into
        // `relation "users" already exists`, with the failure landing halfway
        // through creating the account. See the header of migrations-global/
        Store::migrate_global(&config.database_url)
            .await
            .map_err(|e| CormexError::Store(format!("the global schema migration failed: {e}")))?;

        let store = Store::connect(&config.database_url)
            .await
            .map_err(|e| CormexError::Store(format!("could not connect to the database: {e}")))?;

        // User #1's schema is literally public, so this statement doubles as
        // "migrate theirs".
        //
        // Once multi-tenancy is wired through, this becomes
        // `cormex_store::migrate_all`, migrating one by one and calling out the
        // broken ones individually — one user's database being broken must not
        // take everybody offline
        store
            .migrate()
            .await
            .map_err(|e| CormexError::Store(format!("the database migration failed: {e}")))?;

        // An empty `api_key_env` = this provider requires no credentials (a local
        // Ollama, and any self-hosted endpoint that does not check keys).
        //
        // **This branch used to be missing**, so `std::env::var("")` failed
        // necessarily and reported "missing the API key environment variable for
        // ollama" — an error that reads like a configuration omission while in
        // fact no configuration could ever get past it. cormexd therefore could
        // not start at all, while `cormex-llm::LlmClient::from_config`'s path was
        // correct the whole time: the same decision in two places, written
        // correctly in only one.
        let key_var = cormex_llm::provider::api_key_env(&config.llm.provider)?;
        let api_key = if key_var.is_empty() {
            String::new()
        } else {
            std::env::var(&key_var).map_err(|_| {
                CormexError::Config(format!(
                    "the API key environment variable {key_var} for {} is missing",
                    config.llm.provider
                ))
            })?
        };
        let llm = LlmClient::from_config(&config.llm, &api_key)?;

        let context_window = llm.model().context_limit();

        // With no CORMEX_VISION_PROVIDER configured the whole path is off,
        // rather than wiring up a fake transcriber: the latter would write
        // garbage captions into the database, and garbage is harder to notice
        // than nothing at all
        let transcribe = match cormex_memory::transcribe::VisionTranscriber::from_env()? {
            Some(v) => {
                tracing::info!("media transcription is enabled");
                Some(Arc::new(
                    cormex_memory::TranscribePipeline::new(embedder.clone())
                        .with_transcriber(Arc::new(v)),
                ))
            }
            None => None,
        };

        // The previous model. Useful **only during the backfill**: it also
        // encodes the query into the old space, so old facts that have not been
        // filled in yet can still enter vector retrieval.
        //
        // ⚠️ It cannot rescue the "fastembed → api" migration — the old model
        // there was the in-process fastembed, which is not compiled into the
        // binary by default. See ApiEmbedder::legacy_from_env
        let legacy: Option<SharedEmbedder> =
            cormex_memory::embed_api::ApiEmbedder::legacy_from_env()?
                .map(|e| Arc::new(e) as SharedEmbedder);
        if let Some(l) = &legacy {
            tracing::info!(
                legacy = l.model_id(),
                current = embedder.model_id(),
                "the previous embedding model is attached: until the backfill finishes, queries fish in both spaces"
            );
        }

        let extract_permits = extract_concurrency()?;
        tracing::info!(
            concurrent = extract_permits,
            "the extraction concurrency ceiling ({EXTRACT_CONCURRENCY_ENV} adjusts it)"
        );

        Ok(Self {
            retriever: Retriever::new(embedder.clone()).with_legacy(legacy),
            extractor: Arc::new(Extractor::new(
                llm.clone(),
                embedder.clone(),
                config.device_id.clone(),
            )),
            embedder,
            transcribe,
            store,
            llm,
            device_id: config.device_id.clone(),
            context_window,
            extract_gate: Arc::new(tokio::sync::Semaphore::new(extract_permits)),
        })
    }

    /// The local agent writing one conversational turn back into the memory
    /// database.
    ///
    /// This is the **memory half of a turn**, and since the agent loop left this
    /// process it is the only half still here: the loop runs wherever the agent
    /// runs, and `POST /episodes` hands the finished turn over. There used to be
    /// a second, in-process copy of this same sequence (write the episode →
    /// retrieve → attribute → tool attribution → extract) sitting behind
    /// cormexd's own turn loop. Deleting that copy is what leaves our own agent
    /// and a third-party agent exercising one function rather than two — which
    /// is the only thing that keeps this function honest, because a divergence
    /// between two copies goes unnoticed until the two populations of users
    /// compare notes.
    ///
    /// # Idempotent, with the duplicate check outside the transaction
    ///
    /// Offline queue replay necessarily delivers duplicates. The duplicate check
    /// reads `episodes` first and returns immediately on a hit — it **must not**
    /// become `INSERT ... ON CONFLICT DO NOTHING`: `Guarded::insert_row`
    /// unconditionally appends a `sync_log` row, so a no-op would leave a ghost
    /// row pointing at an episode that synced long ago — and `sync_log` is the
    /// single source of truth for sync ordering.
    ///
    /// Keeping the read outside the transaction has a second reason: `write_txn`
    /// deliberately provides no read methods (see discipline #3 in
    /// `cormex-store::txn`: a ticket-taking transaction must be short and
    /// write-only).
    ///
    /// Under real concurrency, two replays can pass the duplicate check
    /// simultaneously and the second hits the PK unique constraint — that path
    /// maps to the same "already existed" for [`Self::write_episode`]'s caller.
    async fn write_episode(
        &self,
        req: NewEpisodeRequest,
        origin: TurnOrigin,
    ) -> Result<EpisodeAck> {
        let episode_id: Id = req.id.parse().map_err(|_| {
            CormexError::Invalid(format!("the episode id is not a valid ULID: {}", req.id))
        })?;
        let role = match req.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            other => {
                return Err(CormexError::Invalid(format!(
                    "role accepts only user / assistant, and {other} arrived; tool and \
                     system are not \"one turn of conversation\" — they are expressed by \
                     tool attribution and the system prompt respectively"
                )));
            }
        };

        // ── Idempotent duplicate check ──
        if self
            .store
            .episode(&req.id)
            .await
            .map_err(store_err)?
            .is_some()
        {
            tracing::debug!(episode = %req.id, "this episode has been written before; this call is a no-op");
            return Ok(EpisodeAck {
                episode_id: req.id,
                memories: Vec::new(),
                already_existed: true,
            });
        }

        let occurred_at = match req.occurred_at.as_deref() {
            None => Utc::now(),
            Some(raw) => chrono::DateTime::parse_from_rfc3339(raw)
                .map(|t| t.with_timezone(&Utc))
                .map_err(|e| {
                    CormexError::Invalid(format!(
                        "occurred_at is not a valid RFC 3339 time: {raw} ({e})"
                    ))
                })?,
        };

        // ── Attachment pre-check (deliberately outside the transaction: this
        //    is a read, and `write_txn` offers none — a ticket-taking
        //    transaction stays short and write-only) ──
        let attachments = dedup_attachments(&req.attachments);
        for a in &attachments {
            if self.store.blob(&a.hash).await.map_err(store_err)?.is_none() {
                return Err(CormexError::Invalid(format!(
                    "attachment {} is not registered yet; POST /blobs to upload it, \
                     or POST /blobs/commit after a direct upload",
                    a.hash
                )));
            }
        }

        // ── Write L0. The tsv is **computed by the server** ──
        //
        // A client-computed tsv is not accepted: it decides whether the BM25
        // path can retrieve anything at all, getting it wrong has no symptom
        // (things simply cannot be found), and there are many clients on many
        // different versions. The constraint "the tsv is written in the same
        // transaction as its main row" can also only be guaranteed on this side.
        let tsv = cormex_memory::tokenize::to_tsvector_input(&req.text);
        let ep = NewEpisode {
            id: episode_id,
            session_id: req.session_id.clone(),
            role,
            content: serde_json::json!({ "text": req.text }),
            text: Some(req.text.clone()),
            tsv_source: Some(tsv),
            domain: None,
            device_id: self.device_id.clone(),
            occurred_at,
        };
        let links: Vec<cormex_store::NewEpisodeBlob> = attachments
            .iter()
            .map(|a| cormex_store::NewEpisodeBlob {
                episode_id,
                blob_hash: a.hash.clone(),
                kind: a.kind.clone(),
                filename: a
                    .filename
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| clamp_chars(s, cormex_store::ATTACHMENT_FILENAME_MAX_CHARS)),
            })
            .collect();

        match self
            .store
            .write_txn(async |t| {
                let mut last = t.insert_episode(&ep).await?;
                for link in &links {
                    last = t.link_episode_blob(link).await?;
                }
                Ok(last)
            })
            .await
        {
            Ok(_) => {}
            // Another replay squeezed in between the duplicate check and the
            // write. This is a **normal path**, not an error — a 500 would make
            // the client treat a successful replay as a failure and retry
            // forever
            Err(e) if e.is_duplicate_key() => {
                tracing::debug!(episode = %req.id, "a concurrent replay hit the unique constraint; \
                         treating it as already present");
                return Ok(EpisodeAck {
                    episode_id: req.id,
                    memories: Vec::new(),
                    already_existed: true,
                });
            }
            Err(e) => return Err(store_err(e)),
        }

        // ── Retrieval + attribution ──
        let mut memories = Vec::new();
        if req.retrieve && role == Role::User {
            let retrieved = self
                .retriever
                .retrieve(&self.store, &req.text, None, self.context_window)
                .await?;
            if !retrieved.items.is_empty() {
                memories = retrieved.items.iter().map(fact_dto_of).collect();
                record_injected_memories(&self.store, episode_id, &retrieved, &self.device_id)
                    .await;
            }
        }

        // ── Tool attribution (its own transaction, so that it can fail on its
        //    own without taking the episode down with it — see the trade-off
        //    stated where the failure is logged, below) ──
        if !req.tool_calls.is_empty() {
            let anchor = req.anchor_episode_id.as_deref().unwrap_or(&req.id);
            let anchor: Id = anchor.parse().map_err(|_| {
                CormexError::Invalid(format!("anchor_episode_id is not a valid ULID: {anchor}"))
            })?;
            let rows: Vec<cormex_store::NewEpisodeToolCall> = req
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, c)| cormex_store::NewEpisodeToolCall {
                    id: Id::new(),
                    episode_id: anchor,
                    ordinal: i as i32,
                    name: clamp_chars(&c.name, cormex_store::TOOL_NAME_MAX_CHARS),
                    path: c
                        .path
                        .as_deref()
                        .map(|p| clamp_chars(p, cormex_store::TOOL_PATH_MAX_CHARS)),
                    summary: clamp_chars(&c.summary, cormex_store::TOOL_SUMMARY_MAX_CHARS),
                    ok: c.ok,
                    device_id: self.device_id.clone(),
                    // The path where the local agent pushes this in (POST
                    // /episodes). Like path, diff comes from the client and
                    // must likewise be clamped — enforcing the storage ceiling
                    // is this layer's responsibility, and it cannot assume
                    // upstream already clamped everything
                    diff: c
                        .diff
                        .as_deref()
                        .map(|d| clamp_chars(d, cormex_store::TOOL_DIFF_MAX_CHARS)),
                })
                .collect();
            let n = rows.len();
            let res = self
                .store
                .write_txn(async |t| {
                    let mut last = 0;
                    for c in &rows {
                        last = t.insert_episode_tool_call(c).await?;
                    }
                    Ok(last)
                })
                .await;
            // The trade-off: the conversation has completed, and failing to
            // write the attribution is a loss of observability that should not
            // surface as a failed write
            if let Err(e) = res {
                tracing::warn!(error = %e, calls = n, "could not persist the tool attribution");
            }
        }

        // ── Asynchronous extraction. Never blocks the caller ──
        if role == Role::Assistant {
            self.spawn_extraction(
                req.anchor_episode_id.clone(),
                req.text.clone(),
                occurred_at,
                origin,
            );
        }

        Ok(EpisodeAck {
            episode_id: req.id,
            memories,
            already_existed: false,
        })
    }

    /// Feed this turn into the extraction pipeline.
    ///
    /// The user half is read back from the database rather than having the
    /// client send it again: the same sentence arriving from two sources creates
    /// a "the two disagree" path that has to be handled, and the copy in the
    /// database is the authoritative one.
    fn spawn_extraction(
        &self,
        anchor: Option<String>,
        reply: String,
        occurred_at: DateTime<Utc>,
        origin: TurnOrigin,
    ) {
        let Some(anchor) = anchor else {
            tracing::debug!(
                "the assistant episode carried no anchor_episode_id; skipping extraction"
            );
            return;
        };
        let Ok(anchor_id) = anchor.parse::<Id>() else {
            tracing::warn!(
                anchor,
                "anchor_episode_id is not a valid ULID; skipping extraction"
            );
            return;
        };
        let store = self.store.clone();
        let extractor = Arc::clone(&self.extractor);
        let gate = Arc::clone(&self.extract_gate);
        tokio::spawn(async move {
            // Queue here. **The wait happens after the spawn** — the caller
            // received its response long ago, and what queues is this background
            // task itself. See the documentation on the `Live::extract_gate`
            // field
            under_gate(&gate, async move {
                let user_text = match store.episode(&anchor).await {
                    Ok(Some(ep)) => ep.text.unwrap_or_default(),
                    Ok(None) => {
                        tracing::warn!(anchor, "extraction could not find the matching user episode; skipping");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, anchor, "extraction could not read the user episode; skipping");
                        return;
                    }
                };
                let text = format!("用户：{user_text}\n助手：{reply}");
                // The trust tier is **decided by the origin alone**, and the
                // origin is decided by the credential alone (see the passage in
                // `routes::write_episode`). Letting the request body decide is
                // letting a prompt-injected agent pick its own tier
                let ctx = ExtractContext::new(anchor_id, occurred_at).from(origin);
                match extractor.ingest(&store, &text, &ctx).await {
                    Ok(report) => tracing::info!(
                        candidates = report.candidates,
                        written = report.written.len(),
                        superseded = report.superseded.len(),
                        duplicates = report.duplicates,
                        "this turn's extraction is done (local agent)"
                    ),
                    Err(e) => tracing::warn!(error = %e, "this turn's extraction failed (local agent)"),
                }
            })
            .await;
        });
    }

    /// The LLM proxy: forward the local agent's streaming call to the provider
    /// verbatim.
    ///
    /// **Pure forwarding; it touches no memory and writes to no database.** What
    /// the local side gets back from this trip is exactly the same
    /// `MessageStream` as a direct connection (see `cormex_proto::llm`'s module
    /// comment: what travels on the wire is goose's native type, with no
    /// intermediate format).
    ///
    /// # The model is chosen by the server, never named by the client
    ///
    /// The request carries only a tier. Letting the client name a model would
    /// let anyone holding a token **run any model on the server's key** — with
    /// the server paying the bill.
    ///
    /// This variant additionally lets the caller specify **which key** to call
    /// with.
    ///
    /// `own` is the key the user supplied themselves. When present, a provider
    /// is constructed on the spot to use it, so this call does not count against
    /// the quota (and does not spend the server's money).
    ///
    /// # Why one is constructed each time rather than cached
    ///
    /// A cache would need a key of (user, provider, api key) plus invalidation
    /// handling — and the key can be rotated or withdrawn at any moment, so a
    /// still-live cache entry means "still billing against the old key after it
    /// was withdrawn". Constructing a provider merely assembles a reqwest
    /// client, far cheaper than the HTTP call that follows it.
    ///
    /// # Errors
    /// The provider could not be constructed, or upstream refused.
    pub async fn llm_stream_with_key(
        &self,
        req: LlmStreamRequest,
        own: Option<(&str, &str, Option<&str>)>,
    ) -> Result<MessageStream> {
        let Some((provider_name, api_key, base_url)) = own else {
            let model = match req.tier {
                ModelTier::Main => self.llm.model(),
                ModelTier::Cheap => self.llm.cheap_model(),
            };
            return Ok(self
                .llm
                .stream_with(model, &req.system, &req.messages, &req.tools)
                .await?);
        };

        let provider: std::sync::Arc<dyn cormex_llm::Provider> =
            cormex_llm::provider::build_with(provider_name, api_key, base_url)
                .map_err(|e| {
                    CormexError::Config(format!(
                        "could not build a provider from the bring-your-own key: {e}"
                    ))
                })?
                .into();
        // The model configuration stays the server's — what the user supplied is
        // a key, not a model. Letting them swap the model too would require a
        // whole apparatus for validating "which models does this provider have",
        // and the symptom of a mistyped model name is that every conversation
        // fails
        let client = cormex_llm::LlmClient::from_provider(
            provider,
            provider_name,
            self.llm.model().clone(),
            self.llm.cheap_model().clone(),
        );
        let model = match req.tier {
            ModelTier::Main => client.model(),
            ModelTier::Cheap => client.cheap_model(),
        };
        Ok(client
            .stream_with(model, &req.system, &req.messages, &req.tools)
            .await?)
    }

    pub async fn database_status(&self) -> String {
        match self.store.ping().await {
            Ok(()) => "ok".into(),
            Err(e) => format!("error: {e}"),
        }
    }

    /// The storage-layer handle. For the realtime-push listener task reading the
    /// cursor.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Switch to one tenant's database, sharing every other component.
    ///
    /// # Why "clone a Live" rather than "add a store parameter to every method"
    ///
    /// The latter would mean editing 38 occurrences of `self.store` and a dozen
    /// method signatures, with every one of them a **manual** judgement of
    /// "should this use the tenant's database or public's" — and a wrong
    /// judgement raises no error, it merely writes one person's memory into
    /// another person's schema.
    ///
    /// Cloning replaces it wholesale: once bound, **every** method on this
    /// `Live` lands in that schema, with no possible exception. The price is
    /// about ten extra `Arc` clones and one `String` per request, three orders
    /// of magnitude cheaper than the database round trip that follows.
    ///
    /// Returns a [`BoundLive`] rather than a `Live`: the data methods live only
    /// on that type, so **they cannot be called without binding** (see the
    /// source-reading guard test in `state.rs`).
    #[must_use]
    pub fn bind(&self, store: &Store) -> BoundLive {
        BoundLive(Arc::new(Self {
            store: store.clone(),
            ..self.clone()
        }))
    }

    /// The current vectorization backend. The backfiller uses it to compute
    /// vectors.
    #[must_use]
    pub fn embedder(&self) -> &SharedEmbedder {
        &self.embedder
    }

    /// Vectorization coverage: how many active facts the current model covers,
    /// and how many still belong to an older model.
    ///
    /// # Why this number has to be visible
    ///
    /// After a model change the old vectors are still there, but all four vector
    /// queries filter on `embedding_model` (see
    /// `cormex_store::Store::recall_vector`), so old facts **drop out of vector
    /// retrieval the same day**. Meanwhile BM25, graph traversal and recency all
    /// keep working, so it does not look broken — it just looks like "semantic
    /// retrieval seems a bit worse". Without this number, nobody is ever told.
    pub async fn embedding_health(&self) -> crate::dto::EmbeddingHealth {
        let model = self.embedder.model_id().to_owned();
        let (covered, stale) = self
            .store
            .embedding_coverage(&model)
            .await
            .unwrap_or_else(|e| {
                // On a failed query report (0, 0) rather than taking down all
                // of /health: this endpoint's consumers are HEALTHCHECK and
                // probes, and turning a container unhealthy over one diagnostic
                // field has the priorities backwards. The error goes to the log
                tracing::warn!(error = %e, "could not compute the embedding coverage");
                (0, 0)
            });
        crate::dto::EmbeddingHealth {
            model,
            covered,
            stale,
        }
    }

    /// Report vectorization coverage once at startup.
    ///
    /// One INFO line under full coverage; a **WARN** when rows from an older
    /// model remain, listing them per model. This is the **only** place that
    /// speaks up about "a model change causing silent amnesia" — the retrieval
    /// side raises no error, and nothing shows in the UI.
    pub async fn report_embedding_state(&self) {
        let model = self.embedder.model_id();
        let census = match self.store.embedding_census().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "the embedding census failed; skipping the startup comparison");
                return;
            }
        };

        let others: Vec<String> = census
            .iter()
            .filter(|(m, _)| m != model)
            .map(|(m, n)| format!("{m}={n}"))
            .collect();

        if others.is_empty() {
            let covered = census
                .iter()
                .find(|(m, _)| m == model)
                .map_or(0, |(_, n)| *n);
            tracing::info!(
                model,
                covered,
                "embeddings: every fact is under the current model"
            );
            return;
        }

        let (covered, stale) = self.store.embedding_coverage(model).await.unwrap_or((0, 0));
        tracing::warn!(
            current = model,
            covered,
            stale,
            others = others.join(" "),
            "the embedding model changed: {stale} facts have **no vector** under the \
             current model, so they cannot enter vector retrieval (BM25 / graph traversal \
             / recency carry on as usual, so all the UI shows is \"semantic recall got a \
             bit worse\"). The backfill will fill them in; CORMEX_REEMBED=off turns it off"
        );
    }

    pub async fn latest_cursor(&self) -> i64 {
        self.store.latest_seq().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not read latest_seq; serving 0 instead");
            0
        })
    }

    // ────────────────────────── retrieval ──────────────────────

    /// The one implementation of retrieval. Both the HTTP endpoint and the
    /// agent's `memory_search` tool go through here — if the two paths each had
    /// their own copy, "what the tool found" and "what the UI shows" would
    /// eventually disagree, and that is the hardest class of inconsistency to
    /// explain to a user.
    async fn retrieve(&self, query: &str, as_of: Option<&str>, limit: i64) -> Result<Retrieved> {
        let at = match as_of {
            Some(t) => Some(
                chrono::DateTime::parse_from_rfc3339(t)
                    .map_err(|e| CormexError::Invalid(format!("as_of is not valid RFC3339: {e}")))?
                    .with_timezone(&Utc),
            ),
            None => None,
        };

        match retrieval_plan(query, at) {
            // With no query terms, what is wanted is "the whole picture at that
            // instant", newest first. When `at` is None, that instant is now
            RetrievalPlan::Snapshot(at) => {
                self.retriever
                    .retrieve_as_of(&self.store, at, limit, self.context_window)
                    .await
            }
            // With query terms, relevance ranking should happen **on that same
            // as-of snapshot**. The snapshot is what replay actually is;
            // "newest first" is merely an ordering unrelated to the question —
            // measured on the evaluation set: switching to three-way RRF over
            // the snapshot moved time-replay R@5 from 0.417 to 0.833, and the
            // gold answer's mean rank from 11.5 to 2.8.
            RetrievalPlan::SnapshotRanked(at) => {
                self.retriever
                    .retrieve_as_of_ranked(&self.store, query, at, limit, self.context_window)
                    .await
            }
            RetrievalPlan::Ranked => {
                self.retriever
                    .retrieve(&self.store, query, None, self.context_window)
                    .await
            }
        }
    }

    /// Annotate a batch of retrieval results with "does this still hold **right
    /// now**". See [`FactDto::invalidated`].
    ///
    /// # Why only the replay path queries the database
    ///
    /// Without `as_of`, all four retrieval paths go through the `active_facts`
    /// view and by definition cannot find an invalidated fact — it is
    /// permanently `false` on that path, and not a single query need be issued.
    /// Only an `as_of` snapshot returns rows that were valid then and have since
    /// been overturned, and **that is the one situation where this field carries
    /// any information at all**.
    ///
    /// # Why N small queries rather than one batched query
    ///
    /// `cormex-store` currently offers only the single-row `fact_status`, and
    /// cormexd does not write SQL (see the passage in `Cargo.toml`: "there is no
    /// sqlx here and there should not be again"). Adding a batch interface means
    /// touching the storage layer, which is another crate's business. N is
    /// bounded by the retrieval `limit` (20 by default), and replay is a
    /// low-frequency operation the user explicitly initiates — the trade here is
    /// "a few more index-hitting point queries" in exchange for "no SQL hole
    /// opened inside cormexd".
    ///
    /// Issued sequentially rather than concurrently: concurrency would only
    /// contend for the same connection pool as the conversations currently being
    /// served, and the wall-clock time saved is meaningless for a replay the
    /// user clicked on themselves.
    async fn invalidation_flags(
        &self,
        items: &[cormex_memory::MemoryItem],
        replaying: bool,
    ) -> Vec<bool> {
        if !replaying {
            return vec![false; items.len()];
        }
        let mut out = Vec::with_capacity(items.len());
        for m in items {
            out.push(match self.store.fact_status(&m.id).await {
                Ok(s) => s.is_some_and(|s| s.op == cormex_store::FactOp::Invalidate),
                // Report false on a failed query: marking a still-valid fact as
                // "invalidated" would leave the user believing their current
                // conclusion had been overturned, whereas the reverse merely
                // omits a badge. The costs of the two directions are
                // asymmetric, so lean towards saying less
                Err(e) => {
                    tracing::warn!(fact = %m.id, error = %e, "could not read the fact's status; \
                         serving it as still valid");
                    false
                }
            });
        }
        out
    }

    /// Retrieve and **render the block of text the model reads**.
    ///
    /// # Why this layer exists on its own
    ///
    /// Every consumer reaches it through one door — the `memory_search` tool on
    /// [`crate::mcp`] — and that includes our own agent, which holds a delegated
    /// credential and speaks the same MCP as Claude Code or goose. Let any
    /// caller render its own and "what a third party found" and "what our own
    /// agent found" drift apart — which is the rule
    /// [`Self::retrieve`]'s comment already established once, lifted one level
    /// higher here: **there is only one rendering, too**.
    ///
    /// The rendering must go through `injection::render_turn_block`; no separate
    /// "MCP result format" may be written. This text enters the model's context
    /// just the same, so not one part of the framing "memory is background data,
    /// not instructions" may be dropped — memory may contain malicious
    /// instructions that were extracted into it (a MemoryGraft), and arriving
    /// through the MCP channel is exactly as dangerous as arriving through the
    /// injection channel.
    pub async fn memory_search_text(
        &self,
        query: &str,
        as_of: Option<&str>,
        limit: i64,
    ) -> Result<String> {
        let r = self.retrieve(query, as_of, limit).await?;
        if r.items.is_empty() {
            return Ok("没有检索到相关记忆。".into());
        }
        Ok(injection::render_turn_block(&r.items))
    }

    /// The core profile block — the things about this person that **change
    /// little**, rendered as text for the model.
    ///
    /// An empty query takes `RetrievalPlan::Snapshot(now)`, i.e. "the whole
    /// picture as of now", newest first. That is exactly how a profile should be
    /// taken: it does not answer a specific question.
    ///
    /// The rendering goes through `render_profile_block` rather than
    /// `render_turn_block` — their framings differ, and the profile one is
    /// written for "the prefix that is resent every turn".
    ///
    /// **This is its first caller**: after that function was built, nobody used
    /// it.
    pub async fn profile_text(&self, limit: i64) -> Result<String> {
        let r = self.retrieve("", None, limit).await?;
        if r.items.is_empty() {
            return Ok(String::new());
        }
        Ok(injection::render_profile_block(&r.items))
    }

    async fn memory_search(&self, q: MemorySearchQuery) -> Result<MemorySearchResponse> {
        let r = self.retrieve(&q.q, q.as_of.as_deref(), q.limit).await?;
        let invalidated = self.invalidation_flags(&r.items, q.as_of.is_some()).await;

        Ok(MemorySearchResponse {
            // Goes through `fact_dto_of` rather than assembling it again here.
            // This used to be a hand-copied version of it, so "add one field"
            // meant editing two places — and the symptom of missing one is the
            // same memory looking different in the search results and in the
            // conversation drawer, with no error at all
            facts: r
                .items
                .iter()
                .zip(invalidated)
                .map(|(m, invalidated)| FactDto {
                    invalidated,
                    ..fact_dto_of(m)
                })
                .collect(),
            channels: r
                .attribution
                .into_iter()
                .map(|a| ChannelHit {
                    fact_id: a.fact_id,
                    channels: a.channels.into_iter().map(str::to_string).collect(),
                    score: a.score,
                })
                .collect(),
        })
    }

    async fn get_episode(&self, id: &str) -> Result<EpisodeDto> {
        let e = self
            .store
            .episode(id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CormexError::NotFound {
                kind: "episode",
                id: id.into(),
            })?;
        let replay = Replay {
            attachments: self
                .store
                .episode_attachments(id)
                .await
                .map_err(store_err)?
                .into_iter()
                .map(attachment_of)
                .collect(),
            memories: self
                .store
                .injected_memories(id)
                .await
                .map_err(store_err)?
                .into_iter()
                .map(memory_dto)
                .collect(),
            tool_calls: self
                .store
                .episode_tool_calls(id)
                .await
                .map_err(store_err)?
                .into_iter()
                .map(tool_call_dto)
                .collect(),
        };
        Ok(episode_dto(e, replay))
    }

    async fn list_sessions(&self, q: &ListSessionsQuery) -> Result<Vec<SessionDto>> {
        let digests = self
            .store
            .session_digests(
                SESSION_LIST_LIMIT,
                q.include_archived,
                q.project_id.as_deref(),
            )
            .await
            .map_err(store_err)?;
        Ok(digests.into_iter().map(session_dto).collect())
    }

    // ── Projects ───────────────────────────────────────────

    async fn list_projects(&self) -> Result<Vec<ProjectDto>> {
        let rows = self.store.projects().await.map_err(store_err)?;
        Ok(rows.into_iter().map(project_dto).collect())
    }

    /// Create a project. The id is generated by **the server**.
    ///
    /// The opposite of sessions (whose ids are generated by the client, because
    /// a session has to be creatable offline): a project can only be created
    /// while online — its entire meaning is "the server knows this grouping
    /// exists", and creating a local project offline and waiting to merge would
    /// produce states like "two devices each created a project with the same
    /// name" that require manual disambiguation. Grouping is not worth that.
    async fn create_project(&self, name: &str) -> Result<ProjectDto> {
        let name = validate_project_name(name)?;
        let id = Id::new().to_string();
        let event = cormex_store::NewProjectEvent::create(
            &id,
            &name,
            cormex_store::Actor::User,
            &self.device_id,
        );

        self.store
            .write_txn(async |t| t.insert_project_event(&event).await)
            .await
            .map_err(store_err)?;

        // Rather than assembling a DTO from the name in hand, read the final
        // state back: under concurrency another device may have just renamed it,
        // in which case echoing this request's name tells the client something
        // that is no longer true. One extra read trades away an entire class of
        // "what the UI shows differs from what the database holds"
        self.project_overview(&id).await
    }

    async fn patch_project(&self, id: &str, patch: ProjectPatch) -> Result<ProjectDto> {
        let Some(raw) = patch.name.as_deref() else {
            return Err(CormexError::Invalid(
                "the request body contains no field to change (name)".into(),
            ));
        };
        let name = validate_project_name(raw)?;

        // Confirm it still exists before renaming. Without this line, renaming a
        // deleted project **silently succeeds** (the event lands as usual) while
        // nothing appears in the list — what the user sees is "I renamed it and
        // it vanished"
        self.require_project(id).await?;

        let event = cormex_store::NewProjectEvent::rename(
            id,
            &name,
            cormex_store::Actor::User,
            &self.device_id,
        );
        self.store
            .write_txn(async |t| t.insert_project_event(&event).await)
            .await
            .map_err(store_err)?;

        self.project_overview(id).await
    }

    /// Delete a project — **dissolve the grouping, leave the content alone**.
    ///
    /// The sessions inside become ungrouped (`session_state` collapses a
    /// membership pointing at a deleted project to NULL), with every message,
    /// attachment and extracted fact intact. A cascade of
    /// `remove_from_project` writes is deliberately avoided: that would be N
    /// writes, and it would make "undo an accidental delete" impossible — as it
    /// stands, creating again with the same id restores every session's
    /// membership exactly as it was.
    ///
    /// **A repeated delete is 404, not 200**: a client deleting once on each of
    /// two devices is common, but reporting "it is already gone" as success
    /// makes an error like "which one did I delete" permanently undiagnosable.
    async fn delete_project(&self, id: &str) -> Result<()> {
        self.require_project(id).await?;

        let event =
            cormex_store::NewProjectEvent::delete(id, cormex_store::Actor::User, &self.device_id);
        self.store
            .write_txn(async |t| t.insert_project_event(&event).await)
            .await
            .map_err(store_err)?;
        Ok(())
    }

    /// The project's final state right now. 404 when it does not exist (or was
    /// deleted).
    async fn project_overview(&self, id: &str) -> Result<ProjectDto> {
        Ok(project_dto(self.require_project(id).await?))
    }

    /// "Does this project still exist" — read the final state once, and 404 if
    /// it does not.
    ///
    /// Factored out because both rename and delete need it, and both must ask
    /// **before entering the write transaction**: that transaction holds the
    /// advisory lock, and discipline requires it to be short and write-only
    /// (rule #3 in `cormex-store::txn`).
    ///
    /// Moving a session into a project does **not** go through here: on that
    /// path a nonexistent project is a 400, not a 404; reasoning in the comment
    /// inside [`Self::patch_session`].
    async fn require_project(&self, id: &str) -> Result<cormex_store::Project> {
        self.store
            .project(id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CormexError::NotFound {
                kind: "project",
                id: id.into(),
            })
    }

    /// Rename / archive / bind a workspace.
    ///
    /// # Why the session is not required to have messages already
    ///
    /// New session → choose a workspace → send the first message is the most
    /// natural order. Returning 404 here because "no such session was found"
    /// would break that order, forcing the client to invert it and make the user
    /// say something first. The event tables do not depend on episodes existing
    /// anyway, so permitting it is enough — the final state is independent of
    /// write order, and as soon as a message arrives the session appears in the
    /// list carrying its title and workspace.
    ///
    /// # Why every change goes into one write transaction
    ///
    /// Split "rename + archive" across two transactions and another device pulls
    /// the intermediate state "renamed but not yet archived" first, making the
    /// list flicker. In one transaction they arrive with consecutive numbers in
    /// `sync_log`.
    async fn patch_session(&self, session_id: &str, patch: SessionPatch) -> Result<SessionDto> {
        let mut events: Vec<cormex_store::NewSessionEvent> = Vec::new();

        if let Some(raw) = patch.title.as_deref() {
            let title = raw.trim();
            if title.is_empty() {
                return Err(CormexError::Invalid(
                    "the title must not be blank; this version has no \
                     \"restore the automatic title\" — give a non-empty one"
                        .into(),
                ));
            }
            if title.chars().count() > cormex_store::SESSION_TITLE_MAX_CHARS {
                return Err(CormexError::Invalid(format!(
                    "the title is too long (the ceiling is {} characters)",
                    cormex_store::SESSION_TITLE_MAX_CHARS
                )));
            }
            events.push(cormex_store::NewSessionEvent::rename(
                session_id,
                title,
                cormex_store::Actor::User,
                &self.device_id,
            ));
        }

        if let Some(archived) = patch.archived {
            events.push(if archived {
                cormex_store::NewSessionEvent::archive(
                    session_id,
                    cormex_store::Actor::User,
                    &self.device_id,
                )
            } else {
                cormex_store::NewSessionEvent::unarchive(
                    session_id,
                    cormex_store::Actor::User,
                    &self.device_id,
                )
            });
        }

        // Validation must happen **before** entering the transaction: the write
        // transaction holds the advisory lock, while canonicalize costs a
        // filesystem round trip (tens of milliseconds on a network drive). A
        // ticket-taking transaction is short and write-only — discipline #3 in
        // cormex-store::txn.
        //
        // ── Binding a workspace: **the server refuses explicitly** ──
        //
        // This path used to be the web client's fallback for binding a workspace
        // (when the local agent was absent), and what it bound was a directory
        // **on the server** — so a remote user's `read_file` touched the
        // production machine's filesystem. The blast radius is the whole machine
        // plus every tenant's data.
        //
        // Refused rather than silently ignored: silently, the client would
        // display "bound to D:\myproject" and then fail every file operation
        // inexplicably — while the user plainly saw the binding succeed. The
        // error message names the two paths that do work.
        //
        // Unbinding (`Some(None)`) **is still permitted**: an older session may
        // still carry a server-side binding and the user must be able to clear
        // it. Refusing to unbind would weld that record in place forever.
        if workspace_patch(patch.workspace.as_ref().map(Option::as_deref))? {
            events.push(cormex_store::NewSessionEvent::unbind_workspace(
                session_id,
                cormex_store::Actor::User,
                &self.device_id,
            ));
        }

        // ── Move into / out of a project ──
        //
        // The target project's existence is checked **before entering the
        // transaction** (as above: a ticket-taking transaction is short and
        // write-only). Without the check, moving into a deleted project silently
        // succeeds — the event lands as usual, while the final-state view treats
        // the dangling membership as ungrouped, so what the user sees is "I
        // dragged it in and it bounced back out", with nothing reporting an error
        // anywhere.
        match patch.project_id.as_ref().map(Option::as_deref) {
            Some(Some(project_id)) => {
                // A nonexistent target project is a **400, not a 404**: the
                // resource of this request is the session, and it is perfectly
                // fine. A 404 would leave the client unable to distinguish "the
                // session is gone" from "the project is gone", and the two call
                // for entirely different actions (refresh the list vs. rebuild
                // the grouping)
                if self
                    .store
                    .project(project_id)
                    .await
                    .map_err(store_err)?
                    .is_none()
                {
                    return Err(CormexError::Invalid(format!(
                        "the target project does not exist or has been deleted: {project_id}"
                    )));
                }
                events.push(cormex_store::NewSessionEvent::move_to_project(
                    session_id,
                    project_id,
                    cormex_store::Actor::User,
                    &self.device_id,
                ));
            }
            Some(None) => events.push(cormex_store::NewSessionEvent::remove_from_project(
                session_id,
                cormex_store::Actor::User,
                &self.device_id,
            )),
            None => {}
        }

        // ── Runtime assignment ──
        //
        // "Does that binding really exist on this machine" is not validated: the
        // server **cannot see** the client machine's `workspaces.json`, which is
        // a premise of this design rather than an oversight (see
        // `cormex-local::workspaces`'s module header). The party declaring it is
        // the only one that knows.
        if let Some(runtime) = patch.runtime {
            events.push(cormex_store::NewSessionEvent::set_runtime(
                session_id,
                match runtime {
                    SessionRuntimeDto::Local => cormex_store::SessionRuntime::Local,
                    SessionRuntimeDto::Cloud => cormex_store::SessionRuntime::Cloud,
                },
                cormex_store::Actor::User,
                &self.device_id,
            ));
        }

        if events.is_empty() {
            return Err(CormexError::Invalid(
                "the request body contains no field to change (title / archived / workspace / project_id)".into(),
            ));
        }

        self.store
            .write_txn(async |t| {
                let mut last = 0;
                for e in &events {
                    last = t.insert_session_event(e).await?;
                }
                Ok(last)
            })
            .await
            .map_err(store_err)?;

        self.session_overview(session_id).await
    }

    /// The session overview. Still returns a row when there are no messages —
    /// after a PATCH the client needs the final state back, and at that moment
    /// the session may well have no messages at all.
    async fn session_overview(&self, session_id: &str) -> Result<SessionDto> {
        // When messages exist, the aggregate query has already brought back the
        // title / archived flag / workspace along with them
        if let Some(d) = self
            .store
            .session_digest(session_id)
            .await
            .map_err(store_err)?
        {
            return Ok(session_dto(d));
        }

        // No messages — assemble a shell from the events' final state. This path
        // is exactly what the order "choose a workspace first, then send the
        // first message" requires
        let state = self
            .store
            .session_state(session_id)
            .await
            .map_err(store_err)?;
        let now = Utc::now().to_rfc3339();
        let title = state.as_ref().and_then(|s| s.title.clone());
        Ok(SessionDto {
            id: session_id.to_string(),
            title_is_custom: title.is_some(),
            title: title.unwrap_or_else(|| session_title(None)),
            created_at: now.clone(),
            updated_at: now,
            message_count: 0,
            preview: None,
            archived: state.as_ref().is_some_and(|s| s.archived),
            workspace: state.as_ref().and_then(|s| s.workspace.clone()),
            runtime: state
                .as_ref()
                .map_or(SessionRuntimeDto::Cloud, |s| runtime_dto(s.runtime)),
            project_id: state.and_then(|s| s.project_id),
        })
    }

    /// One page of a session's messages, with attachments and the replay drawer
    /// attached.
    ///
    /// # Why the overview is no longer computed from the messages fetched
    ///
    /// It used to be "the messages are all in hand anyway, compute it while we
    /// are here", and pagination made that path outright wrong: only **one
    /// page** is in hand, so the computed `message_count` is the page size,
    /// `created_at` is the oldest entry on this page, and the derived title
    /// takes the first user message on this page. Those numbers happen to be
    /// correct on the first page and are all wrong one page back — and they
    /// report no error.
    ///
    /// The overview now always comes from the `session_digest` aggregate SQL: it
    /// counts **every** message, independent of which page was requested. One
    /// extra round trip trades away an entire class of "the numbers start
    /// drifting once you paginate" bugs. The client-side patch that compared
    /// `message_count` against the page length to detect truncation now has
    /// `has_more` available, but the old reading still holds (`message_count` is
    /// still the total).
    async fn session_detail(
        &self,
        session_id: &str,
        q: &SessionDetailQuery,
    ) -> Result<SessionDetail> {
        let digest = self
            .store
            .session_digest(session_id)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CormexError::NotFound {
                kind: "session",
                id: session_id.into(),
            })?;

        let limit = q
            .limit
            .unwrap_or(DEFAULT_EPISODE_PAGE)
            .clamp(1, MAX_EPISODE_PAGE);
        let before = q.before.as_deref().map(crate::cursor::decode).transpose()?;

        // Ask for one extra row to decide "is there anything older". Inferring
        // from len == limit would trick the client into requesting one empty
        // page whenever the count divides exactly, and most clients treat an
        // empty page as "that is the end" — mixing the two criteria produces
        // "sometimes you cannot page back to the earliest few"
        let mut page = self
            .store
            .episodes_by_session_page(session_id, limit + 1, before.as_ref())
            .await
            .map_err(store_err)?;
        let has_more = page.len() as i64 > limit;
        page.truncate(limit as usize);

        // What arrives here is descending (newest → oldest). Reverse it to
        // ascending before sending; reasoning in `SessionDetail::episodes`'s
        // documentation: sending descending raises no error, it merely makes an
        // older client draw the whole conversation backwards
        let next_cursor = page
            .last()
            .filter(|_| has_more)
            .map(|e| crate::cursor::encode(e.occurred_at, &e.id));
        page.reverse();

        let ids: Vec<String> = page.iter().map(|e| e.id.clone()).collect();
        let mut by_episode = self.replay_bundle(&ids).await?;

        Ok(SessionDetail {
            session: session_dto(digest),
            episodes: page
                .into_iter()
                .map(|e| {
                    let replay = by_episode.remove(&e.id).unwrap_or_default();
                    episode_dto(e, replay)
                })
                .collect(),
            has_more,
            next_cursor,
        })
    }

    /// A page of messages' attachments plus replay drawer, in three bulk queries
    /// rather than N+1 per row.
    async fn replay_bundle(
        &self,
        episode_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Replay>> {
        let mut out: std::collections::HashMap<String, Replay> =
            std::collections::HashMap::with_capacity(episode_ids.len());

        for a in self
            .store
            .episode_attachments_bulk(episode_ids)
            .await
            .map_err(store_err)?
        {
            out.entry(a.episode_id.clone())
                .or_default()
                .attachments
                .push(attachment_of(a));
        }
        for m in self
            .store
            .injected_memories_bulk(episode_ids)
            .await
            .map_err(store_err)?
        {
            out.entry(m.episode_id.clone())
                .or_default()
                .memories
                .push(memory_dto(m));
        }
        for t in self
            .store
            .episode_tool_calls_bulk(episode_ids)
            .await
            .map_err(store_err)?
        {
            out.entry(t.episode_id.clone())
                .or_default()
                .tool_calls
                .push(tool_call_dto(t));
        }

        Ok(out)
    }

    // ──────────────────────────── media ────────────────────────

    /// The metadata on a `blobs` row. Fetching the bytes relies on it for the
    /// MIME type and total length (that length is the denominator of HTTP
    /// Range's `Content-Range`).
    async fn blob_meta(&self, hash: &str) -> Result<cormex_store::Blob> {
        self.store
            .blob(hash)
            .await
            .map_err(store_err)?
            .ok_or_else(|| CormexError::NotFound {
                kind: "blob",
                id: hash.into(),
            })
    }

    /// Register a blob that has already landed in object storage — step two of
    /// the three-step order.
    ///
    /// Goes through `write_txn` rather than a bare INSERT: the `blobs` row must
    /// be in the same transaction as `sync_log`, or other devices never learn
    /// this object exists and the `episode_blobs` they sync down point at a blob
    /// row they do not have.
    ///
    /// Returning `None` means this content **was already registered** and no row
    /// was added this time. That is not an error: under content addressing the
    /// two rows would be byte-for-byte identical, so registering again is
    /// neither meaningful nor harmful.
    async fn register_blob(&self, new: cormex_store::NewBlob) -> Result<Option<i64>> {
        if self
            .store
            .blob(&new.hash)
            .await
            .map_err(store_err)?
            .is_some()
        {
            return Ok(None);
        }

        match self
            .store
            .write_txn(async |t| t.insert_blob(&new).await)
            .await
        {
            Ok(seq) => Ok(Some(seq)),
            Err(e) => {
                // The existence check above is a TOCTOU: two devices uploading
                // the same image simultaneously collide on the primary key.
                // Query again: if it is there now, treat it as success. The
                // SQLSTATE is deliberately not matched — that would require
                // cormexd to know sqlx's error structure, and this layer should
                // not have sqlx again
                if self
                    .store
                    .blob(&new.hash)
                    .await
                    .map_err(store_err)?
                    .is_some()
                {
                    tracing::debug!(hash = %new.hash, "the same content was registered concurrently; \
                         treating it idempotently");
                    Ok(None)
                } else {
                    Err(store_err(e))
                }
            }
        }
    }

    /// Queue one media transcription — **without blocking the upload response**.
    ///
    /// Transcription makes one vision model call (measured at over ten seconds),
    /// and doing it synchronously would stall "drag an image in" for that long,
    /// while the transcription result is of no use whatsoever to **this upload**:
    /// it is preparation for future retrieval. Same reasoning as fact
    /// extraction, and the same shape.
    ///
    /// Queued only on the blob's **first** registration: under content
    /// addressing a repeated upload is the same bytes, the transcription result
    /// would be identical, and redoing it burns model money for nothing.
    pub fn spawn_transcription(self: &Arc<Self>, hash: String, bytes: bytes::Bytes, mime: String) {
        if self.transcribe.is_none() {
            // No vision model configured; images are still archived, they simply
            // cannot be retrieved
            return;
        }
        let live = Arc::clone(self);

        tokio::spawn(async move {
            match live.transcribe_and_store(&hash, &bytes, &mime).await {
                Ok(outcome) => outcome.log(&hash),
                // A failed transcription must not affect an upload that already
                // succeeded; a log line is enough
                Err(e) => tracing::warn!(hash, error = %e, "the media transcription failed"),
            }
        });
    }

    /// Whether media transcription is enabled. The background rescanner uses it
    /// to decide whether to start at all — with no `CORMEX_VISION_PROVIDER`
    /// configured the whole path is off, matching the upload path's behaviour.
    #[must_use]
    pub fn transcription_enabled(&self) -> bool {
        self.transcribe.is_some()
    }

    /// Run one transcription and persist it. **Awaits the result
    /// synchronously** — the upload path throws it into
    /// [`Self::spawn_transcription`] fire-and-forget, while the background
    /// rescanner needs the outcome to decide "should this blob be retried".
    ///
    /// Redaction protection goes through [`StoreRedactionGuard`], and neither
    /// path can bypass it: the pipeline builds
    /// [`cormex_memory::transcribe::RedactionGuard`] into `run`'s signature.
    pub async fn transcribe_and_store(
        &self,
        hash: &str,
        bytes: &[u8],
        mime: &str,
    ) -> Result<TranscribeOutcome> {
        let Some(pipeline) = self.transcribe.as_ref() else {
            return Ok(TranscribeOutcome::Disabled);
        };
        let guard = StoreRedactionGuard {
            store: self.store.clone(),
        };
        match pipeline.run(hash, bytes, mime, &guard).await? {
            cormex_memory::transcribe::Outcome::Ready(rows) => {
                let n = rows.len();
                // All segments are written in one transaction: ASR segments have
                // no independent meaning between them, and half a timeline is
                // worse than none
                self.store
                    .write_txn(async |t| {
                        for r in &rows {
                            t.insert_blob_transcript(r).await?;
                        }
                        Ok(())
                    })
                    .await
                    .map_err(store_err)?;
                Ok(TranscribeOutcome::Stored { segments: n })
            }
            cormex_memory::transcribe::Outcome::Redacted => Ok(TranscribeOutcome::Redacted),
            cormex_memory::transcribe::Outcome::Unsupported { mime } => {
                Ok(TranscribeOutcome::Unsupported { mime })
            }
        }
    }

    // ──────────────────────────── sync ─────────────────────────

    async fn sync_since(&self, q: SyncQuery) -> Result<SyncResponse> {
        let limit = q.limit.clamp(1, 1000);
        let records = self
            .store
            .fetch_since(q.since, limit)
            .await
            .map_err(|e| CormexError::Store(e.to_string()))?;

        let cursor = records.last().map_or(q.since, |r| r.seq);
        let has_more = records.len() as i64 == limit;

        Ok(SyncResponse {
            cursor,
            has_more,
            records: records
                .into_iter()
                .map(|r| SyncRecord {
                    seq: r.seq,
                    table: r.table_name,
                    id: r.record_id,
                    // A None payload means the business row no longer exists (it
                    // was purged). The log entry is still sent down: the client
                    // uses it to advance its cursor and to learn that a deletion
                    // happened
                    payload: r
                        .payload
                        .as_ref()
                        .map_or(serde_json::Value::Null, crate::sync_payload::to_json),
                })
                .collect(),
        })
    }
}

/// Write this turn's retrieval attribution into `episode_memories`.
///
/// Failure only logs: the conversation is already under way, attribution is
/// observability, and it must not drag a successful conversation into a 500. But
/// it **must be logged**, or "why is the drawer empty" has nothing to
/// investigate in production.
async fn record_injected_memories(
    store: &Store,
    episode_id: Id,
    retrieved: &Retrieved,
    device_id: &str,
) {
    // attribution and items are the same length and in the same order (see
    // cormex_memory::Retrieved's documentation), but that is not assumed here —
    // indexing by fact_id means a missing entry costs one attribution rather
    // than shifting everything into "A's score recorded against B"
    let scores: std::collections::HashMap<&str, &cormex_memory::Attribution> = retrieved
        .attribution
        .iter()
        .map(|a| (a.fact_id.as_str(), a))
        .collect();

    let mut rows: Vec<cormex_store::NewEpisodeMemory> = Vec::with_capacity(retrieved.items.len());
    for (i, item) in retrieved.items.iter().enumerate() {
        // A malformed fact id skips that one entry rather than abandoning the
        // batch: episode_memories.fact_id is a ulid domain, so one bad id would
        // roll back the entire transaction and take every other attribution with
        // it
        let Ok(fact_id) = item.id.parse::<Id>() else {
            tracing::warn!(id = %item.id, "a retrieval result's id is not a valid ULID; skipping that attribution");
            continue;
        };
        let attribution = scores.get(item.id.as_str());
        rows.push(cormex_store::NewEpisodeMemory {
            id: Id::new(),
            episode_id,
            fact_id,
            ordinal: i as i32,
            channels: attribution.map_or_else(Vec::new, |a| {
                a.channels.iter().map(|c| (*c).to_string()).collect()
            }),
            score: attribution.map(|a| a.score),
            device_id: device_id.to_owned(),
        });
    }

    if rows.is_empty() {
        return;
    }

    let n = rows.len();
    let res = store
        .write_txn(async |t| {
            let mut last = 0;
            for r in &rows {
                last = t.insert_episode_memory(r).await?;
            }
            Ok(last)
        })
        .await;
    match res {
        Ok(_) => tracing::debug!(injected = n, "this turn's memory attribution was persisted"),
        Err(e) => {
            tracing::warn!(error = %e, injected = n, "could not persist the memory attribution; \
             this turn's drawer will be empty")
        }
    }
}

/// Truncate to the ceiling, measured in **characters**.
///
/// These ceilings correspond to CHECKs in the schema. Exceeding one is not "this
/// row cannot be written", it is **the entire write transaction rolling back**
/// — one file with an unusually long name can take a whole turn's attribution
/// with it. Characters rather than bytes: Postgres's `length()` counts
/// characters, and cutting by bytes would also split a CJK character in half.
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    s.chars().take(max).collect()
}

// ─────────────────────── domain → DTO ───────────────────────

fn store_err(e: cormex_store::StoreError) -> CormexError {
    CormexError::Store(e.to_string())
}

/// Retrieval result → wire fact.
///
/// [`Live::write_episode`] (the memory handed back with a turn's ack, which the
/// client shows in the conversation drawer) and [`Live::memory_search`] (the
/// search results) share this one mapping. With a copy in each, the symptom of
/// drift is **the same memory looking different in the drawer and in search**,
/// with no error at all.
fn fact_dto_of(m: &cormex_memory::MemoryItem) -> FactDto {
    FactDto {
        id: m.id.clone(),
        statement: m.statement.clone(),
        predicate: m.predicate.clone(),
        domain: m.domain.clone(),
        // `confidence` was once hard-coded to 1.0 here, so **every** memory in
        // the drawer displayed "100%" — a number that looked like data and was
        // in fact a constant. When it is unavailable the value is 0.0 rather
        // than 1.0: the client uses that to omit the badge entirely
        confidence: m.confidence.unwrap_or(0.0),
        valid_at: m.valid_at.clone(),
        created_at: m.known_since.clone(),
        // This turn's injection goes through retrieval without as_of, querying
        // active_facts only — by definition it cannot obtain an invalidated
        // fact. See `Live::invalidation_flags`
        invalidated: false,
        source_episode_id: m.source_episode_id.clone(),
        source_channel: m.source_channel.clone(),
        trust_tier: m.trust_tier,
    }
}

/// A message's "extras": attachments plus the replay drawer.
///
/// Three `Vec`s packed into one struct rather than three parameters on
/// [`episode_dto`]: with three same-typed parameters in a row, passing them in
/// the wrong order draws no complaint from the compiler.
#[derive(Debug, Default)]
struct Replay {
    attachments: Vec<AttachmentDto>,
    memories: Vec<InjectedMemoryDto>,
    tool_calls: Vec<ToolCallDto>,
}

fn episode_dto(e: cormex_store::Episode, replay: Replay) -> EpisodeDto {
    EpisodeDto {
        id: e.id,
        session_id: e.session_id,
        role: e.role.as_str().to_string(),
        text: e.text,
        occurred_at: e.occurred_at.to_rfc3339(),
        attachments: replay.attachments,
        memories: replay.memories,
        tool_calls: replay.tool_calls,
    }
}

fn attachment_of(a: cormex_store::EpisodeAttachment) -> AttachmentDto {
    AttachmentDto {
        hash: a.blob_hash,
        kind: a.kind,
        filename: a.filename,
        mime: a.mime,
        size_bytes: a.size_bytes,
    }
}

fn memory_dto(m: cormex_store::InjectedMemory) -> InjectedMemoryDto {
    InjectedMemoryDto {
        fact_id: m.fact_id,
        statement: m.statement,
        domain: m.domain,
        channels: m.channels.unwrap_or_default(),
        score: m.score,
        invalidated: m.invalidated,
        source_episode_id: m.source_episode_id,
    }
}

fn tool_call_dto(t: cormex_store::EpisodeToolCall) -> ToolCallDto {
    ToolCallDto {
        name: t.name,
        path: t.path,
        summary: t.summary,
        ok: t.ok,
        diff: t.diff,
    }
}

fn session_dto(d: cormex_store::SessionDigest) -> SessionDto {
    SessionDto {
        id: d.session_id,
        title_is_custom: d.title.is_some(),
        // Use the name the user gave if they gave one, otherwise derive it from
        // the first user message. The order cannot be inverted — inverted, it
        // means "the user renamed it and a refresh changed it back"
        title: d
            .title
            .unwrap_or_else(|| session_title(d.first_user_text.as_deref())),
        created_at: d.started_at.to_rfc3339(),
        updated_at: d.updated_at.to_rfc3339(),
        message_count: d.message_count,
        preview: d.last_text,
        archived: d.archived,
        workspace: d.workspace,
        project_id: d.project_id,
        runtime: runtime_dto(d.runtime),
    }
}

/// Storage-layer enum → wire-protocol enum.
///
/// Two enums rather than one: `cormex-store` does not depend on `cormex-proto`
/// (the dependency direction is `core ← store ← … ← cormexd`), and once the wire
/// protocol's values are fixed they must not follow a storage-layer refactor.
/// The price of one extra match buys "changing the database representation
/// cannot silently change the wire protocol".
fn runtime_dto(r: cormex_store::SessionRuntime) -> SessionRuntimeDto {
    match r {
        cormex_store::SessionRuntime::Local => SessionRuntimeDto::Local,
        cormex_store::SessionRuntime::Cloud => SessionRuntimeDto::Cloud,
    }
}

fn project_dto(p: cormex_store::Project) -> ProjectDto {
    ProjectDto {
        id: p.project_id,
        name: p.name,
        created_at: p.created_at.to_rfc3339(),
        session_count: p.session_count,
    }
}

/// The title is derived from the first user message.
///
/// The truncation lives at this layer rather than in SQL: it is a
/// **presentation decision**, and the storage layer should not decide on the
/// UI's behalf how long a title should be. Truncated by characters rather than
/// bytes — cutting CJK text at a byte boundary produces mojibake.
fn session_title(first_user_text: Option<&str>) -> String {
    let trimmed = first_user_text.map(str::trim).unwrap_or_default();
    if trimmed.is_empty() {
        return "New session".into();
    }
    trimmed.chars().take(TITLE_CHARS).collect()
}

/// Drop duplicate attachment hashes, preserving the order the client gave.
///
/// `episode_blobs`'s primary key is `(episode_id, blob_hash)`, so attaching the
/// same image twice to one message hits the primary key directly and rolls back
/// the whole write transaction — while "the same image got dragged in twice" is
/// about as ordinary as UI operations get.
fn dedup_attachments(input: &[AttachmentRef]) -> Vec<AttachmentRef> {
    let mut seen = std::collections::HashSet::new();
    input
        .iter()
        .filter(|a| seen.insert(a.hash.clone()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All four cells laid out. **Missing one means the memory panel opens
    /// empty, with no error reported at all.**
    #[test]
    fn no_cell_of_the_retrieval_matrix_comes_back_empty_handed() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .expect("a constant timestamp parses")
            .with_timezone(&Utc);

        assert_eq!(
            retrieval_plan("", Some(at)),
            RetrievalPlan::Snapshot(at),
            "replay + no query terms = the whole picture as it stood then"
        );
        assert_eq!(
            retrieval_plan("object storage", Some(at)),
            RetrievalPlan::SnapshotRanked(at),
            "replay + query terms = ranked over that snapshot, not degraded to newest-first"
        );
        assert_eq!(
            retrieval_plan("object storage", None),
            RetrievalPlan::Ranked,
            "now + query terms = all four retrieval paths"
        );

        // **This cell used to be Ranked.** And all four retrieval paths require
        // query terms, so it returned 0 rows every time. What showed in the UI
        // was "no memories yet" — against a database holding 159 facts
        match retrieval_plan("   ", None) {
            RetrievalPlan::Snapshot(_) => {}
            other => panic!(
                "now + no query terms must yield \"the most recent whole picture\"; actually got \
                 {other:?}. Going through the four retrieval paths, an empty query returns 0 rows \
                 every time: the memory panel opens empty and says \"no memories yet\" — from \
                 which the user concludes the feature is useless"
            ),
        }
    }

    #[test]
    fn a_project_name_must_survive_trimming() {
        assert_eq!(
            validate_project_name("  memory engine  ")
                .expect("whitespace on both sides should be trimmed, not rejected"),
            "memory engine"
        );

        for bad in ["", "   ", "\t\n"] {
            assert!(
                validate_project_name(bad).is_err(),
                "{bad:?} should be rejected — a project whose name is all whitespace is a \
                 clickable row in the sidebar containing nothing, which looks like the UI is \
                 broken"
            );
        }
    }

    #[test]
    fn the_project_name_limit_counts_characters_not_bytes() {
        let max = cormex_store::PROJECT_NAME_MAX_CHARS;

        // A CJK name exactly at the limit: measured in bytes it is three times
        // as long and would be wrongly rejected
        let full = "记".repeat(max);
        assert_eq!(
            validate_project_name(&full).expect(
                "exactly at the limit must pass — the ceiling counts characters, matching the \
                 database's length()"
            ),
            full
        );

        assert!(
            validate_project_name(&"记".repeat(max + 1)).is_err(),
            "one character over must be rejected here, rather than letting the database's CHECK \
             turn into a 500"
        );
    }

    #[test]
    fn session_title_falls_back_and_truncates_by_char() {
        assert_eq!(session_title(None), "New session");
        assert_eq!(
            session_title(Some("   ")),
            "New session",
            "all whitespace must fall back too"
        );

        // Truncated by characters. Cutting by bytes would split a CJK character
        // in half and JSON serialization would emit mojibake outright
        let long = "一二三四五六七八九十".repeat(4); // 40 CJK characters
        let title = session_title(Some(&long));
        assert_eq!(
            title.chars().count(),
            TITLE_CHARS,
            "the title should be cut to {TITLE_CHARS} characters: {title}"
        );
        assert!(
            title.starts_with("一二三"),
            "the beginning must be unchanged after truncation: {title}"
        );
    }

    #[test]
    fn duplicate_attachments_are_collapsed_in_order() {
        // episode_blobs's primary key is (episode_id, blob_hash), so attaching
        // the same image twice hits the primary key and rolls back the whole
        // write transaction — while "the same image got dragged in twice" is a
        // routine operation
        let a = AttachmentRef {
            hash: "a".repeat(64),
            kind: Some("image".into()),
            filename: Some("design-draft.png".into()),
        };
        let b = AttachmentRef {
            hash: "b".repeat(64),
            kind: None,
            filename: None,
        };
        let got = dedup_attachments(&[a.clone(), b.clone(), a.clone()]);
        assert_eq!(
            got.iter().map(|x| x.hash.clone()).collect::<Vec<_>>(),
            vec![a.hash.clone(), b.hash.clone()],
            "duplicate hashes should be dropped while preserving the order the client gave"
        );
        assert_eq!(
            got[0].kind.as_deref(),
            Some("image"),
            "the entry kept should be the first occurrence, along with its kind"
        );
    }

    /// The server **cannot** bind a workspace, but it **can** unbind one.
    ///
    /// Inverting the binding half means "the server can bind again", with no
    /// symptom whatsoever — until the day a remote user's `read_file` reads the
    /// `.env` on the production machine. Inverting the unbinding half means a
    /// binding on an older session can never be cleared.
    #[test]
    fn the_server_refuses_to_bind_but_still_lets_you_unbind() {
        let msg = workspace_patch(Some(Some("D:/anything")))
            .expect_err("a server-side workspace binding must be refused")
            .to_string();
        assert!(
            msg.contains("your own machine") && msg.contains("cormex-local"),
            "the refusal must name paths that do work (the desktop client / running cormex-local \
             locally), not merely say no. Actual: {msg}"
        );
        // Once the sandbox landed there was **one more path that works**, and it
        // is precisely the only one a web user can take — the first two both
        // require installing a desktop client first. Omitting it makes this error
        // amount to telling a web user "the thing you want cannot be done here",
        // when in fact flipping one switch provides it
        assert!(
            msg.contains("cloud sandbox"),
            "a web user cannot take either of the first two paths (both need a desktop client). \
             Without mentioning the sandbox, this error is a dead end for them. Actual: {msg}"
        );

        assert!(
            workspace_patch(Some(None)).expect("unbinding must be permitted"),
            "unbinding has to keep working — an older session may still carry a server-side \
             binding, and refusing to unbind welds it in place forever"
        );
        assert!(
            !workspace_patch(None).expect("not mentioning the field must not error"),
            "a request that only changes the title must not be blocked by the workspace rule"
        );
    }
}

/// Which path one retrieval should take. **All four retrieval paths require
/// query terms**, so "are there terms" and "is this a replay" are two
/// independent dimensions, giving four cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetrievalPlan {
    /// The whole picture at that instant, newest first. Taken when there are no
    /// query terms.
    Snapshot(chrono::DateTime<Utc>),
    /// Relevance ranking **on top of** the snapshot at that instant.
    SnapshotRanked(chrono::DateTime<Utc>),
    /// The four retrieval paths as of now, plus RRF.
    Ranked,
}

/// The decision part of [`Live::retrieve`], factored into a pure function purely
/// so that **the four cells can be laid out and tested**.
///
/// # One cell was missing here
///
/// "No terms + no replay" used to land on `Ranked`. And all four retrieval paths
/// require terms: BM25 has no terms, the vector has no direction to embed, graph
/// traversal has no seed — so it returned 0 rows every time, with even
/// `channels` empty (not one path ran). The symptom was that the memory panel
/// **opened empty**, captioned "no memories yet", against a real database
/// holding 159 rows. The conclusion the user drew was "this feature is not much
/// use".
///
/// The correct answer was already in the code (`as_of` + no terms → the whole
/// picture); what was missing was only "**now** is also a valid as_of". Laid out
/// as a table of four cells, a gap like that has nowhere to hide.
fn retrieval_plan(query: &str, as_of: Option<chrono::DateTime<Utc>>) -> RetrievalPlan {
    match (query.trim().is_empty(), as_of) {
        (true, Some(at)) => RetrievalPlan::Snapshot(at),
        (true, None) => RetrievalPlan::Snapshot(Utc::now()),
        (false, Some(at)) => RetrievalPlan::SnapshotRanked(at),
        (false, None) => RetrievalPlan::Ranked,
    }
}

#[cfg(test)]
mod extract_gate_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An invalid value errors rather than quietly using the default.
    #[test]
    fn a_bad_concurrency_setting_is_an_error_not_a_fallback() {
        assert_eq!(
            parse_extract_concurrency(None).unwrap(),
            DEFAULT_EXTRACT_CONCURRENCY
        );
        assert_eq!(parse_extract_concurrency(Some("8")).unwrap(), 8);
        assert_eq!(parse_extract_concurrency(Some(" 8 ")).unwrap(), 8);

        for bad in ["0", "abc", "-1", "3.5"] {
            let err = parse_extract_concurrency(Some(bad)).expect_err(
                "an invalid value must error; falling back to the default means the ceiling is \
                 not in effect while ops has no idea",
            );
            assert!(
                err.to_string().contains(EXTRACT_CONCURRENCY_ENV),
                "the error must name which variable is at fault; actual: {err}"
            );
        }
    }

    /// **The gate genuinely holds concurrency down.**
    ///
    /// What this tests is not the semaphore itself (that is tokio's business)
    /// but whether [`under_gate`] **holds the permit until the future ends**.
    /// Written as `let _ = acquire().await`, the permit is dropped on the spot
    /// and the gate becomes an expensive no-op — and that change makes no other
    /// test go red, because functionally everything works, there is simply no
    /// ceiling any more.
    ///
    /// The failure looks very concrete: during a bulk import, thousands of
    /// extractions fire at once and drag the 2-core, 3.5 GB production node down
    /// along with every other service on it.
    #[tokio::test]
    async fn the_gate_actually_caps_concurrency() {
        const PERMITS: usize = 3;
        const TASKS: usize = 40;

        let gate = Arc::new(tokio::sync::Semaphore::new(PERMITS));
        let now = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(TASKS);
        for _ in 0..TASKS {
            let (gate, now, peak) = (Arc::clone(&gate), Arc::clone(&now), Arc::clone(&peak));
            handles.push(tokio::spawn(async move {
                under_gate(&gate, async {
                    let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(n, Ordering::SeqCst);
                    // Yield so other tasks get a chance to squeeze in —
                    // without an await each task would run straight through,
                    // the peak would always be 1, and the test would be
                    // meaningless
                    tokio::task::yield_now().await;
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    now.fetch_sub(1, Ordering::SeqCst);
                })
                .await;
            }));
        }
        for h in handles {
            h.await.expect("the task must not panic");
        }

        let peak = peak.load(Ordering::SeqCst);
        assert!(
            peak <= PERMITS,
            "{peak} were running at once against a ceiling of {PERMITS} — the permit was not \
             held until the end"
        );
        assert!(
            peak > 1,
            "the peak was only {peak}, meaning these tasks never overlapped at all and this test \
             measured nothing"
        );
    }
}

//! Import a chat export into the **archive** from the browser.
//!
//! `POST /archive/import/preview` and `POST /archive/import/run` take a file
//! already spooled by `POST /import/upload` (a ChatGPT, Claude, Grok or Gemini
//! export, or the zip it came in) and do what `cormex import` does from a
//! terminal: one archived session per conversation, browsable and searchable
//! like any captured agent session.
//!
//! # Why not the existing `/import/run`
//!
//! That route feeds conversations into **memory** (episodes, then extraction,
//! and its preview estimates the LLM calls that costs). Archiving is a
//! different operation: it stores the conversation as it was, costs nothing,
//! and is what the archive's import button means. The two coexist.
//!
//! # Idempotent, and nothing is deleted
//!
//! Each conversation is compared with what the archive holds for the same
//! vendor conversation id (any file of that session, whoever wrote it):
//!
//! | archive holds | action |
//! |---|---|
//! | nothing | a new session, one snapshot generation |
//! | exactly these bytes | skipped |
//! | different bytes | a new snapshot generation on the existing file; the old one stays |
//!
//! So importing a fresh full export every month writes only what changed, and
//! a conversation deleted at the vendor stays in the archive.
//!
//! # The device
//!
//! Capture files belong to a device. A browser import has none, so the files it
//! creates belong to one fixed id per tenant, derived from the tenant's schema;
//! a later import finds them again. A conversation the CLI imported first keeps
//! its file, and a changed version is written there: the import is about the
//! conversation, not about which machine sent it.

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use cormex_core::Id;
use cormex_proto::archive::{ArchiveImportRequest, ArchiveImportSummary};
use cormex_store::{FileRole, Framing, GenerationReason, NewCaptureFile};
use cormex_transcript::export::{ExportPlatform, SplitConversation};

use crate::principal::Principal;
use crate::routes::ApiError;
use crate::state::AppState;

/// What the archive already has for one conversation.
enum Found {
    New,
    Unchanged,
    /// The file to write the new version to.
    Changed(cormex_store::CaptureFile),
}

/// `POST /archive/import/preview` — what a run would write, writing nothing.
///
/// # Errors
/// The caller may not write the archive, the handle is unknown, or the file is
/// not an export.
pub async fn preview(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Json(req): Json<ArchiveImportRequest>,
) -> Result<Json<ArchiveImportSummary>, ApiError> {
    let (_, store) = writer(&st, &principal).await?;
    let (platform, conversations) = load(&req).await?;
    let mut summary = summary(platform, conversations.len());
    for c in &conversations {
        match find(&store, platform, c).await? {
            Found::New => summary.new += 1,
            Found::Unchanged => summary.unchanged += 1,
            Found::Changed(_) => summary.changed += 1,
        }
    }
    Ok(Json(summary))
}

/// `POST /archive/import/run` — archive every new or changed conversation.
///
/// # Errors
/// As [`preview`], or a write failed. Conversations written before the failure
/// stay written; running again skips them.
pub async fn run(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Json(req): Json<ArchiveImportRequest>,
) -> Result<Json<ArchiveImportSummary>, ApiError> {
    let (tenant, store) = writer(&st, &principal).await?;
    let (platform, conversations) = load(&req).await?;
    let device = web_device(&store);
    let now = Utc::now();
    let mut summary = summary(platform, conversations.len());
    for c in &conversations {
        match find(&store, platform, c).await? {
            Found::Unchanged => summary.unchanged += 1,
            Found::New => {
                let new = NewCaptureFile {
                    id: Id::new(),
                    native_session_id: c.session_id.clone(),
                    parent_native_session_id: None,
                    device_id: device.clone(),
                    agent: platform.agent().to_owned(),
                    logical_path: format!("conversations/{}.json", c.session_id),
                    role: FileRole::Transcript,
                    framing: Framing::Snapshot,
                    cwd: None,
                    project_key: None,
                };
                let registered = store
                    .archive_txn(async |t| t.register_capture_file(&new).await)
                    .await
                    .map_err(store_err)?;
                // A registration that finds bytes already there lost a race
                // with another import of the same conversation; a fresh
                // generation keeps both versions instead of splicing them
                let generation = if registered.end_offset == 0 {
                    registered.generation.id.clone()
                } else {
                    open_snapshot(&store, &registered.file.id).await?
                };
                write(&st, &tenant, &store, &registered.file, &generation, c, now).await?;
                summary.new += 1;
            }
            Found::Changed(file) => {
                let generation = open_snapshot(&store, &file.id).await?;
                write(&st, &tenant, &store, &file, &generation, c, now).await?;
                summary.changed += 1;
            }
        }
    }
    summary.written = true;
    Ok(Json(summary))
}

async fn writer(
    st: &AppState,
    principal: &Principal,
) -> Result<(crate::request_tenant::Tenant, cormex_store::Store), ApiError> {
    // Writing the archive is `capture:write`'s job for a device; a person's own
    // credential carries every scope
    if !principal.has_scope(crate::device_keys::Scope::CaptureWrite) {
        return Err(ApiError::forbidden(
            "this credential does not carry capture:write",
        ));
    }
    let tenant = principal.tenant(st).await?;
    let store = tenant
        .store()
        .cloned()
        .map_err(|e| ApiError::unsupported(e.to_string()))?;
    Ok((tenant, store))
}

/// The uploaded file, unzipped and split into conversations.
async fn load(
    req: &ArchiveImportRequest,
) -> Result<(ExportPlatform, Vec<SplitConversation>), ApiError> {
    let platform = match req.platform.as_deref() {
        None | Some("") => None,
        Some("chatgpt") => Some(ExportPlatform::ChatGpt),
        Some("claude") => Some(ExportPlatform::Claude),
        Some("grok") => Some(ExportPlatform::Grok),
        Some("gemini") => Some(ExportPlatform::Gemini),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "platform is chatgpt, claude, grok or gemini, not {other:?}"
            )));
        }
    };
    let path = crate::import::path_of(&req.handle)?;
    let bytes = tokio::fs::read(&path).await.map_err(|e| {
        ApiError::not_found(format!(
            "the uploaded file is gone ({e}); upload it again — uploads are kept for a limited time"
        ))
    })?;
    // Parsing a large export is CPU work; it runs off the async executor
    tokio::task::spawn_blocking(move || {
        let json = cormex_import::export_json(&bytes)?;
        cormex_transcript::export::split_export_detected(&json, platform)
    })
    .await
    .map_err(|e| ApiError::internal(format!("the import task failed: {e}")))?
    .map_err(|e| ApiError::unprocessable(format!("{e:#}")))
}

fn summary(platform: ExportPlatform, conversations: usize) -> ArchiveImportSummary {
    let source = cormex_proto::catalog::Catalog::builtin()
        .agents
        .iter()
        .find(|a| a.id == platform.agent())
        .map_or_else(|| platform.agent().to_owned(), |a| a.name.clone());
    ArchiveImportSummary {
        agent: platform.agent().to_owned(),
        source,
        conversations,
        new: 0,
        changed: 0,
        unchanged: 0,
        written: false,
    }
}

async fn find(
    store: &cormex_store::Store,
    platform: ExportPlatform,
    c: &SplitConversation,
) -> Result<Found, ApiError> {
    let Some(session) = store
        .capture_session_by_native(platform.agent(), &c.session_id)
        .await
        .map_err(store_err)?
    else {
        return Ok(Found::New);
    };
    let sha = cormex_blob::hash::sha256_hex(&c.bytes);
    let len = i64::try_from(c.bytes.len()).unwrap_or(i64::MAX);
    let files: Vec<_> = store
        .capture_files_of_session(&session)
        .await
        .map_err(store_err)?
        .into_iter()
        .filter(|f| f.role == FileRole::Transcript)
        .collect();
    let Some(first) = files.first().cloned() else {
        return Ok(Found::New);
    };
    for f in &files {
        let Some((generation, end)) = store.latest_generation(&f.id).await.map_err(store_err)?
        else {
            continue;
        };
        if end != len {
            continue;
        }
        // An import uploads a conversation as one chunk, so "the same bytes"
        // is one chunk at offset 0 with this hash
        let chunks = store
            .chunks_overlapping(&generation.id, 0, end.max(1))
            .await
            .map_err(store_err)?;
        if let [only] = chunks.as_slice()
            && only.byte_offset == 0
            && only.content_sha256 == sha
        {
            return Ok(Found::Unchanged);
        }
    }
    Ok(Found::Changed(first))
}

async fn open_snapshot(store: &cormex_store::Store, file_id: &str) -> Result<String, ApiError> {
    let generation = store
        .archive_txn(async |t| t.open_generation(file_id, GenerationReason::Snapshot).await)
        .await
        .map_err(store_err)?;
    Ok(generation.id)
}

async fn write(
    st: &AppState,
    tenant: &crate::request_tenant::Tenant,
    store: &cormex_store::Store,
    file: &cormex_store::CaptureFile,
    generation: &str,
    c: &SplitConversation,
    now: DateTime<Utc>,
) -> Result<(), ApiError> {
    let sha = cormex_blob::hash::sha256_hex(&c.bytes);
    let outcome =
        crate::capture::store_chunk(st, tenant, store, file, generation, 0, &c.bytes, &sha, now)
            .await?;
    match outcome {
        cormex_store::AppendOutcome::Appended { .. }
        | cormex_store::AppendOutcome::AlreadyPresent { .. } => Ok(()),
        cormex_store::AppendOutcome::OffsetMismatch { expected } => {
            Err(ApiError::conflict(format!(
                "conversation {} changed while it was being imported (the generation ends at \
                 {expected}); run the import again",
                c.session_id
            )))
        }
    }
}

/// The device that owns the files a browser import creates: one fixed id per
/// tenant, so a later import finds the same files.
fn web_device(store: &cormex_store::Store) -> String {
    Id::derived(
        DateTime::<Utc>::UNIX_EPOCH,
        &format!("web-import|{}", store.schema().as_str()),
    )
    .to_string()
}

fn store_err(e: cormex_store::StoreError) -> ApiError {
    ApiError::internal(format!("the archive import failed: {e}"))
}

#[cfg(test)]
mod tests {

    fn claude_export(extra_turn: bool, third: bool) -> Vec<u8> {
        let mut deploy = serde_json::json!({
            "uuid": "c-deploy", "name": "Deploy notes", "created_at": "2026-10-30T00:00:00Z",
            "chat_messages": [
                {"uuid": "m1", "sender": "human", "created_at": "2026-10-30T00:00:00Z",
                 "text": "the health check times out"},
                {"uuid": "m2", "sender": "assistant", "created_at": "2026-10-30T00:00:10Z",
                 "text": "it binds to loopback"}
            ]
        });
        if extra_turn {
            deploy["chat_messages"]
                .as_array_mut()
                .expect("array")
                .push(serde_json::json!({"uuid": "m3", "sender": "human",
                    "created_at": "2026-11-02T00:00:00Z", "text": "fixed, thanks"}));
        }
        let mut all = vec![
            serde_json::json!({
                "uuid": "c-storage", "name": "Storage", "created_at": "2026-10-01T00:00:00Z",
                "chat_messages": [{"uuid": "s1", "sender": "human",
                    "created_at": "2026-10-01T00:00:00Z", "text": "RustFS or MinIO?"}]
            }),
            deploy,
        ];
        if third {
            // Newest first, as the vendor lists them: every position shifts
            all.insert(
                0,
                serde_json::json!({
                    "uuid": "c-new", "name": "New", "created_at": "2026-11-05T00:00:00Z",
                    "chat_messages": [{"uuid": "n1", "sender": "human",
                        "created_at": "2026-11-05T00:00:00Z", "text": "hello"}]
                }),
            );
        }
        serde_json::to_vec(&all).expect("bytes")
    }

    async fn import(
        app: &axum::Router,
        token: &str,
        body: Vec<u8>,
        step: &str,
    ) -> serde_json::Value {
        let (status, ack) = crate::test_support::call_bytes(
            app,
            axum::http::Method::POST,
            "/import/upload",
            token,
            &[],
            body,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "upload");
        let handle = serde_json::from_slice::<serde_json::Value>(&ack).expect("ack")["handle"]
            .as_str()
            .expect("a handle")
            .to_owned();
        let (status, summary) = crate::test_support::call(
            app,
            axum::http::Method::POST,
            &format!("/archive/import/{step}"),
            Some(token),
            Some(serde_json::json!({ "handle": handle })),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{step}: {summary}");
        summary
    }

    /// The monthly re-export: unchanged conversations are skipped, a changed one
    /// gets a new generation with the old one kept, a new one is added — and
    /// none of them moves onto another conversation's session when positions
    /// shift.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn reimporting_an_export_writes_only_what_changed() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let user = st
            .create_account("importer", "correct horse battery")
            .await
            .expect("account");
        let token = st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));
        let app = crate::routes::router(st.clone());
        let store = st.public_store().expect("store");
        let generations = async |native: &str| -> i64 {
            sqlx::query_scalar(
                "SELECT count(*) FROM capture_generations g JOIN capture_files f ON f.id = g.file_id
                   JOIN capture_sessions s ON s.id = f.session_id WHERE s.native_session_id = $1",
            )
            .bind(native)
            .fetch_one(store.pool())
            .await
            .expect("count")
        };

        let preview = import(&app, &token, claude_export(false, false), "preview").await;
        assert_eq!(
            (preview["new"].as_u64(), preview["written"].as_bool()),
            (Some(2), Some(false)),
            "{preview}"
        );
        assert_eq!(generations("c-deploy").await, 0, "a preview writes nothing");

        let first = import(&app, &token, claude_export(false, false), "run").await;
        assert_eq!(first["new"].as_u64(), Some(2), "{first}");
        assert_eq!(first["agent"], "claude_export");

        let again = import(&app, &token, claude_export(false, false), "run").await;
        assert_eq!(
            (
                again["new"].as_u64(),
                again["changed"].as_u64(),
                again["unchanged"].as_u64()
            ),
            (Some(0), Some(0), Some(2)),
            "the same file twice writes nothing the second time: {again}"
        );
        assert_eq!(generations("c-deploy").await, 1);

        let later = import(&app, &token, claude_export(true, true), "run").await;
        assert_eq!(
            (
                later["new"].as_u64(),
                later["changed"].as_u64(),
                later["unchanged"].as_u64()
            ),
            (Some(1), Some(1), Some(1)),
            "{later}"
        );
        assert_eq!(
            generations("c-deploy").await,
            2,
            "the changed conversation has a new generation and keeps the old one"
        );
        assert_eq!(
            generations("c-storage").await,
            1,
            "the unchanged one is untouched although its position in the file moved"
        );
        db.drop().await;
    }
}

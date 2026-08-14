//! Import from the web client: **upload the file first**, then parse it.
//!
//! The desktop client does not take this path — its file is on the local
//! machine and the local agent reads it directly (see
//! `cortex_local::local_import`). A browser cannot read the disk, so uploading is
//! all that is left.
//!
//! Both paths parse with **the same** [`cortex_import`] and return the same wire
//! types ([`cortex_proto::import`]), so there is only one import page on the
//! client.
//!
//! # Why it is spooled to disk rather than held in memory
//!
//! A real Claude export is 97 MB. Parsing it into `serde_json::Value` balloons
//! to several hundred MB, and the production machine is **2 cores, 3.5 GB, with
//! 19 containers already running**. The upload streams straight into a temporary
//! file and is only read at parse time — so memory holds at most one parsed
//! result, rather than both an upload buffer and a parsed result.
//!
//! # The temporary files have to clean themselves up
//!
//! A user may well close the page right after uploading 97 MB. Without cleanup,
//! that machine's disk slowly fills with unclaimed export files — and a full
//! disk manifests as **the database being unable to write**, a failure with no
//! apparent connection to imports whatsoever.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use axum::{
    Json,
    body::Body,
    extract::State,
    response::{IntoResponse, Response, Sse, sse::Event},
};
use cortex_proto::import::{ImportEvent, ImportRequest, UploadAck};
use futures::StreamExt as _;
use futures::stream::Stream;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::mpsc;

use crate::routes::ApiError;
use crate::state::AppState;

/// Ceiling for a single upload.
///
/// Twice the headroom over a real export (97 MB). The reason for having a
/// ceiling is not saving disk, it is that **without one there is no
/// backpressure** — a connection can keep writing until the disk is full.
const MAX_UPLOAD: u64 = 256 * 1024 * 1024;

/// How long an unclaimed uploaded file survives before it is swept away.
///
/// One hour is enough for "upload → look at the estimate → think about it →
/// click start", without leaving an abandoned 97 MB on disk for an entire day.
/// This number is reported to the client verbatim
/// ([`UploadAck::expires_in_secs`]), so the UI can say so up front.
const TTL: Duration = Duration::from_secs(3600);

/// Where the temporary files go.
///
/// Into a dedicated subdirectory of the system temp directory rather than
/// strewn across `/tmp` directly: cleanup has to be able to scan only our own
/// files, and `remove_dir_all` on a shared directory is a catastrophe.
fn spool_dir() -> PathBuf {
    std::env::temp_dir().join("cortex-import-spool")
}

/// Handle → file path.
///
/// **The handle must be treated as untrusted input**: it comes from the client
/// and it gets joined into a path. Only the ULID alphabet is accepted, so `..`,
/// `/` and `\` cannot get through — the consequence of path traversal here is
/// "parse any file at all on the server".
fn path_of(handle: &str) -> Result<PathBuf, ApiError> {
    let ok = handle.len() == 26
        && handle
            .bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)));
    if !ok {
        return Err(ApiError::bad_request("the upload handle is malformed"));
    }
    Ok(spool_dir().join(handle))
}

/// `POST /import/upload` — stream straight to disk.
///
/// # Errors
/// The directory could not be created, the disk could not be written, or the
/// upload exceeded [`MAX_UPLOAD`].
pub async fn upload(State(_): State<AppState>, body: Body) -> Result<Json<UploadAck>, ApiError> {
    let dir = spool_dir();
    tokio::fs::create_dir_all(&dir).await.map_err(|e| {
        ApiError::internal(format!(
            "could not create the upload staging directory: {e}"
        ))
    })?;
    // Sweep while we are here. A timer would mean one more background task plus
    // its failure path, whereas this code path only runs when somebody is
    // actually uploading a file — when nobody is, there is nothing to sweep
    sweep(&dir).await;

    let handle = cortex_core::Id::new().to_string();
    let path = dir.join(&handle);
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|e| ApiError::internal(format!("could not create the staging file: {e}")))?;

    let mut written: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                // A half-written file left behind would just be parsed as
                // legitimate input and produce a baffling JSON error. Delete
                // it, so the failure stops here
                let _ = tokio::fs::remove_file(&path).await;
                return Err(ApiError::bad_request(format!(
                    "the upload was interrupted: {e}"
                )));
            }
        };
        written += chunk.len() as u64;
        if written > MAX_UPLOAD {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(ApiError::bad_request(format!(
                "the file is over the ceiling ({} MiB). The conversations.json inside an \
                 export archive is normally far smaller — check that what you uploaded \
                 is that file rather than the whole archive",
                MAX_UPLOAD / 1024 / 1024
            )));
        }
        if let Err(e) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(ApiError::internal(format!(
                "could not write the staging file: {e}"
            )));
        }
    }
    file.flush()
        .await
        .map_err(|e| ApiError::internal(format!("could not flush it to disk: {e}")))?;

    Ok(Json(UploadAck {
        handle,
        size_bytes: written,
        expires_in_secs: TTL.as_secs(),
    }))
}

/// Sweep away spooled files that have outlived the TTL.
async fn sweep(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return;
    };
    let now = SystemTime::now();
    while let Ok(Some(e)) = entries.next_entry().await {
        let stale = e
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > TTL);
        if stale {
            let _ = tokio::fs::remove_file(e.path()).await;
        }
    }
}

/// Turn the request into a `Plan`. **Writes nothing.**
async fn plan_of(req: &ImportRequest) -> Result<cortex_import::Plan, ApiError> {
    let handle = req
        .handle
        .as_deref()
        .filter(|h| !h.trim().is_empty())
        .ok_or_else(|| {
            ApiError::bad_request("a handle is required — POST /import/upload the file first")
        })?;
    let path = path_of(handle)?;
    let bytes = tokio::fs::read(&path).await.map_err(|_| {
        // Do not echo the path back: that is the server's filesystem layout.
        // And there is really only one likely cause
        ApiError::bad_request(
            "this upload handle has expired (they are cleaned up after an hour); upload again",
        )
    })?;

    let platform = match req.platform.as_deref() {
        None => None,
        Some("chatgpt") => Some(cortex_import::Platform::ChatGpt),
        Some("claude") => Some(cortex_import::Platform::Claude),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unrecognised platform: {other} (only chatgpt / claude are supported)"
            )));
        }
    };
    let since = match req.since.as_deref() {
        None => None,
        Some(s) => Some(
            chrono::DateTime::parse_from_rfc3339(s)
                .map_err(|e| ApiError::bad_request(format!("since is not valid RFC 3339: {e}")))?
                .with_timezone(&chrono::Utc),
        ),
    };

    // Carry **the whole** anyhow chain through: the most common failure is
    // "the structure was not recognised", and that error carries the keys
    // actually encountered — reporting only the outermost layer throws the clue
    // away
    cortex_import::plan_from_bytes(&bytes, platform, since, req.max_conversations)
        .map_err(|e| ApiError::bad_request(format!("{e:#}")))
}

/// `POST /import/preview` — read-only.
///
/// # Errors
/// The handle has expired, a filter is invalid, or the file structure was not
/// recognised.
pub async fn preview(
    State(_): State<AppState>,
    Json(req): Json<ImportRequest>,
) -> Result<Response, ApiError> {
    let plan = plan_of(&req).await?;
    Ok(Json(plan.estimate().to_wire(plan.platform)).into_response())
}

/// `POST /import/run` — SSE.
pub async fn run(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<ImportRequest>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let (tx, rx) = mpsc::channel::<ImportEvent>(64);

    tokio::spawn(async move {
        // The tenant is resolved **before any work starts**. If it cannot be
        // resolved, not a single row is written — discovering halfway through
        // that we do not know whose data this is cannot be rolled back
        let tenant = match st.tenant(&headers).await {
            Ok(t) => t,
            Err(e) => {
                let _ = tx
                    .send(ImportEvent::Error {
                        message: format!(
                            "could not work out which account this import belongs to: {}",
                            e.message()
                        ),
                    })
                    .await;
                return;
            }
        };
        let plan = match plan_of(&req).await {
            Ok(p) => p,
            Err(e) => {
                let _ = tx
                    .send(ImportEvent::Error {
                        message: e.message(),
                    })
                    .await;
                return;
            }
        };
        let _ = tx
            .send(ImportEvent::Started {
                estimate: plan.estimate().to_wire(plan.platform),
            })
            .await;

        let sink = LocalSink(st, tenant);
        let progress_tx = tx.clone();
        // Progress events are droppable: when the channel is full, skip this
        // one. Dropping one only makes the progress bar jump, whereas blocking
        // here would stop the import itself to wait for the client
        let outcome = cortex_import::execute(&sink, &plan, move |p| {
            let _ = progress_tx.try_send(ImportEvent::Progress {
                conversations_done: p.conversations_done,
                conversations_total: p.conversations_total,
                pairs_done: p.pairs_done,
                skipped: p.skipped,
                failures: p.failures,
            });
        })
        .await;

        let ev = match outcome {
            Ok(p) => ImportEvent::Done {
                pairs_done: p.pairs_done,
                skipped: p.skipped,
                failures: p.failures,
            },
            Err(e) => ImportEvent::Error {
                message: format!("{e:#}"),
            },
        };
        let _ = tx.send(ev).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|ev| {
        Ok(
            Event::default().data(serde_json::to_string(&ev).unwrap_or_else(|e| {
                format!(r#"{{"type":"error","message":"could not serialize the event: {e}"}}"#)
            })),
        )
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

/// Uses cortexd's **own** write path — it does not loop back around through its
/// own HTTP surface.
///
/// This differs from the other two callers: both the CLI and the local agent
/// reach `POST /episodes` over the network, whereas this code is already inside
/// the server process. Calling our own HTTP again would buy nothing but an extra
/// serialization + authentication + TCP round trip, and the failure modes on
/// that path (connection pool exhaustion) have nothing to do with importing.
///
/// It carries the tenant — an import belongs to **a particular person**.
///
/// Without it, one person's uploaded three-year history lands in someone else's
/// database, and nothing reports an error at any point.
struct LocalSink(AppState, crate::request_tenant::Tenant);

impl cortex_import::Sink for LocalSink {
    async fn write_episode(
        &self,
        req: &cortex_proto::episodes::NewEpisodeRequest,
    ) -> anyhow::Result<cortex_proto::episodes::EpisodeAck> {
        self.0
            // An import is not written by a sandbox: the bytes on this path come
            // from an export file the user uploaded, parsed by cortexd itself,
            // with no container in between
            .write_episode(
                &self.1,
                req.clone(),
                // What is being imported is the user's **own** conversation
                // history (a ChatGPT / Claude export), which belongs in the same
                // tier as the things they say to us directly
                cortex_memory::extract::TurnOrigin::Trusted,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn rename_session(&self, session_id: &str, title: &str) -> anyhow::Result<()> {
        self.0
            .patch_session(
                &self.1,
                session_id,
                // `..Default::default()` rather than spelling out each None:
                // all we want here is to change the title, and every new
                // dimension added to SessionPatch (workspace, project, ...)
                // would break a "list them all" spelling, at which point
                // somebody fills in a None by reflex — which happens to mean
                // "leave it alone", but that is a coincidence, not a reason to
                // write it down
                cortex_proto::dto::SessionPatch {
                    title: Some(title.to_string()),
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The handle is untrusted input, and it gets joined into a path.**
    ///
    /// Letting `..` through means "parse any file at all on the server", and the
    /// error message carries the keys seen while parsing failed — which is a
    /// hole usable for probing the contents of those files.
    #[test]
    fn a_handle_can_never_escape_the_spool_directory() {
        for bad in [
            "../../../../etc/passwd",
            "..",
            "a/b",
            "a\\b",
            "",
            "0123456789012345678901234",    // 25 chars, one short
            "0123456789012345678901234567", // 28 chars
            "0123456789ABCDEFGHIJKLMNOI",   // contains I, not in the ULID alphabet
            "0123456789abcdefghjkmnpqrs",   // lowercase
        ] {
            assert!(
                path_of(bad).is_err(),
                "this handle must be rejected: {bad:?} — it gets joined into a path"
            );
        }

        let good = cortex_core::Id::new().to_string();
        let p = path_of(&good).expect("a genuine ULID must be accepted");
        assert_eq!(
            p.parent(),
            Some(spool_dir().as_path()),
            "the resolved path must still be inside the spool directory"
        );
    }
}

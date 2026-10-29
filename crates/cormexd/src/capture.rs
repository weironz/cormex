//! The capture endpoints: what the `cormex` CLI sends.
//!
//! Design: docs/design/session-archive-architecture.md §7.1 (D2, D3, D16, D23).
//!
//! # The write order is the whole point
//!
//! `PUT …/chunks/{offset}` does three things, and the order is not negotiable:
//!
//! ```text
//! ① read the body → decompress → check the sha256 and the length
//! ② zstd-compress → put_keyed(capture_key(tenant, sha256))
//! ③ archive_txn { append_chunk(...) }        // contiguity + idempotence
//! ```
//!
//! Object first, row second — the same reasoning as `blobs.rs`'s "object, then
//! database row". The reverse order leaves a row pointing at an object that
//! does not exist, and nothing can heal that: the client has already been told
//! the chunk is stored, so it will never send those bytes again.
//!
//! # Why the sha256 is checked here rather than trusted
//!
//! The hash is the idempotency key. If a client sends the wrong one, the
//! "already present" branch would compare against a hash that does not describe
//! the bytes, and a replay would silently keep the wrong content. Checking it
//! costs one pass over bytes that are already in memory.
//!
//! # Decompression is bounded
//!
//! A `Content-Encoding: zstd` body is decompressed with a **cap**. A few
//! kilobytes of zstd can expand to gigabytes, and without the cap one request
//! takes the process down. The cap is the same ceiling the DDL puts on a chunk
//! (`byte_length <= 32 MiB`), so a chunk that decompresses past it could never
//! have been stored anyway.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use cormex_core::Id;
use cormex_proto::capture::{
    ChunkAck, FileEventOp, FileRole, Framing, GenerationReason, HEADER_CONTENT_SHA256,
    HEADER_OBSERVED_AT, NewGenerationRequest, OffsetMismatchBody, RegisterFileRequest,
    RegisterFileResponse,
};
use cormex_store::{
    AppendOutcome, ChunkEncoding, GenerationReason as StoreReason, NewCaptureChunk, NewCaptureFile,
    NewCaptureFileEvent, RegisteredFile,
};

use crate::principal::Principal;
use crate::request_tenant::Tenant;
use crate::routes::ApiError;
use crate::state::AppState;

/// The hard ceiling on a decompressed chunk.
///
/// Matches `capture_chunks.byte_length`'s CHECK. A chunk larger than this could
/// never be stored, so decompressing further is pure waste — and it is the
/// bound that makes a decompression bomb a 413 rather than an OOM.
const MAX_DECOMPRESSED: usize = 32 << 20;

/// `GET /capture/catalog` — the source catalog, as JSON.
///
/// # Errors
/// The caller is not a device key with `capture:write` (403), or the configured
/// catalog file could not be read or parsed.
pub async fn get_catalog(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Result<axum::Json<cormex_proto::catalog::Catalog>, ApiError> {
    // Every `/capture/*` route takes a device key and nothing else. The catalog
    // is not secret, but leaving it open would mean the capture surface has two
    // kinds of caller, and the next endpoint added would have to remember which
    // kind it belongs to
    capture_caller(&st, &principal).await?;
    // `CORMEX_CAPTURE_CATALOG` points at a newer copy than the one compiled in.
    // A malformed override is an error rather than a silent fallback to the
    // built-in: falling back would leave the operator believing their update
    // took effect, and the symptom would be "some files stopped being found".
    match st.capture_config().catalog_path.clone() {
        Some(path) => {
            let text = std::fs::read_to_string(&path).map_err(|e| {
                ApiError::internal(format!("could not read CORMEX_CAPTURE_CATALOG={path}: {e}"))
            })?;
            let catalog = cormex_proto::catalog::Catalog::parse(&text).map_err(|e| {
                ApiError::internal(format!("CORMEX_CAPTURE_CATALOG={path} does not parse: {e}"))
            })?;
            Ok(axum::Json(catalog))
        }
        None => Ok(axum::Json(
            cormex_proto::catalog::Catalog::builtin().clone(),
        )),
    }
}

/// `POST /capture/files` — register a logical file, or find the one already
/// there.
///
/// # Errors
/// The caller is not a device key with `capture:write` (403), the request is
/// malformed (400), or the write failed.
pub async fn register_file(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    axum::Json(req): axum::Json<RegisterFileRequest>,
) -> Result<axum::Json<RegisterFileResponse>, ApiError> {
    let (tenant, device_id) = capture_caller(&st, &principal).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::unsupported(e.to_string()))?;

    let new = NewCaptureFile {
        id: Id::new(),
        native_session_id: req.native_session_id.clone(),
        parent_native_session_id: req.parent_native_session_id.clone(),
        device_id,
        agent: req.agent.clone(),
        logical_path: req.logical_path.clone(),
        role: store_role(req.role),
        framing: store_framing(req.framing),
        cwd: req.cwd.clone(),
        project_key: req.project_key.clone(),
    };

    let registered: RegisteredFile = store
        .archive_txn(async |t| t.register_capture_file(&new).await)
        .await
        .map_err(store_err)?;

    Ok(axum::Json(RegisterFileResponse {
        file_id: registered.file.id,
        session_id: registered.session.id,
        generation: cormex_proto::capture::GenerationDto {
            id: registered.generation.id,
            ordinal: registered.generation.ordinal,
            end_offset: registered.end_offset,
        },
    }))
}

/// `POST /capture/files/{id}/generations` — open the next generation.
///
/// # Errors
/// The file does not belong to this device (403), or the write failed.
pub async fn open_generation(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(file_id): Path<String>,
    axum::Json(req): axum::Json<NewGenerationRequest>,
) -> Result<axum::Json<cormex_proto::capture::GenerationDto>, ApiError> {
    let (tenant, device_id) = capture_caller(&st, &principal).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::unsupported(e.to_string()))?;

    // **The file has to be this device's.** Without this check a device could
    // open a generation on somebody else's file — and then append to it, which
    // is a write into another machine's history
    let file = store
        .capture_file_by_id(&file_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found("no such capture file"))?;
    if file.device_id != device_id {
        return Err(ApiError::forbidden("this file belongs to another device"));
    }

    let generation = store
        .archive_txn(async |t| t.open_generation(&file_id, store_reason(req.reason)).await)
        .await
        .map_err(store_err)?;

    Ok(axum::Json(cormex_proto::capture::GenerationDto {
        id: generation.id,
        ordinal: generation.ordinal,
        end_offset: 0,
    }))
}

/// `PUT /capture/generations/{id}/chunks/{offset}` — append one chunk.
///
/// # Errors
/// The caller may not write captures (403), the generation is not this device's
/// (403), the sha256 or the length is wrong (422), the body is too large (413),
/// or the offset is not where the generation ends (409).
pub async fn put_chunk(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path((generation_id, offset)): Path<(String, i64)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let (tenant, device_id) = capture_caller(&st, &principal).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::unsupported(e.to_string()))?;

    // ── ① read the body, decompress, validate ──
    // Whatever the client sent, the R layer stores zstd -- so what the encoding
    // decides here is only whether there is a frame to unwrap first
    let logical = match headers
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
    {
        Some("zstd") => decompress(&body)?,
        None | Some("") | Some("identity") => body.to_vec(),
        Some(other) => {
            return Err(ApiError::bad_request(format!(
                "unsupported Content-Encoding {other:?}; the client may send zstd or nothing"
            )));
        }
    };

    let ceiling = st.capture_config().max_chunk_bytes;
    if logical.len() > ceiling {
        return Err(ApiError::payload_too_large(format!(
            "the chunk is {} bytes, past this server's {ceiling}-byte ceiling \
             (CORMEX_CAPTURE_MAX_CHUNK)",
            logical.len()
        )));
    }

    let declared = headers
        .get(HEADER_CONTENT_SHA256)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| ApiError::bad_request(format!("{HEADER_CONTENT_SHA256} is required")))?;
    let actual = cormex_blob::hash::sha256_hex(&logical);
    if declared != actual {
        return Err(ApiError::unprocessable(format!(
            "the body's sha256 is {actual}, but {HEADER_CONTENT_SHA256} says {declared}"
        )));
    }

    let observed_at = headers
        .get(HEADER_OBSERVED_AT)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| ApiError::bad_request(format!("{HEADER_OBSERVED_AT} is required")))?;
    let observed_at = chrono::DateTime::parse_from_rfc3339(observed_at)
        .map_err(|e| ApiError::bad_request(format!("{HEADER_OBSERVED_AT} is not RFC 3339: {e}")))?
        .with_timezone(&chrono::Utc);

    // **The generation has to belong to this device.** Without this a device
    // could append into another machine's file, which is a write into somebody
    // else's history and would corrupt it silently
    let generation = store
        .generation_by_id(&generation_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found("no such generation"))?;
    let file = store
        .capture_file_by_id(&generation.file_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found("no such capture file"))?;
    if file.device_id != device_id {
        return Err(ApiError::forbidden(
            "this generation belongs to another device",
        ));
    }

    // ── ② the object, before the row ──
    //
    // Stored zstd-compressed whatever the client sent: the R layer is written
    // once and read many times (projector, masker, restore), and JSONL
    // compresses about 3.5×. The key names the *logical* hash, so the same
    // content from a plaintext client and a zstd client lands on one object.
    let prefix = tenant_prefix(&tenant)?;
    let compressed = zstd::encode_all(logical.as_slice(), 3)
        .map_err(|e| ApiError::internal(format!("could not compress the chunk: {e}")))?;

    // ── encryption, when this deployment has a master key (P6-1) ──
    //
    // The data key belongs to the **file**, not the chunk: one key per file
    // means destroying a session is deleting a handful of rows, while
    // everything else in the tenant keeps working. A file that already has one
    // reuses it, so an append does not mint a key per chunk.
    let (key, stored) = if crate::capture_keys::enabled() {
        let data_key = match store.capture_key(&file.id).await.map_err(store_err)? {
            Some((id, wrapped)) => crate::capture_keys::unwrap(&id, &wrapped)
                .map_err(|e| ApiError::internal(format!("could not unwrap the data key: {e}")))?,
            None => {
                let fresh = crate::capture_keys::DataKey::generate()
                    .map_err(|e| ApiError::internal(format!("could not generate a key: {e}")))?;
                let wrapped = crate::capture_keys::wrap(&fresh)
                    .map_err(|e| ApiError::internal(format!("could not wrap the key: {e}")))?;
                store
                    .archive_txn(async |t| {
                        t.insert_capture_key(&cormex_store::NewCaptureKey {
                            id: fresh.id.parse().map_err(|_| {
                                cormex_store::StoreError::Invalid(
                                    "the generated key id is not a ULID".into(),
                                )
                            })?,
                            file_id: file.id.parse().map_err(|_| {
                                cormex_store::StoreError::Invalid(
                                    "the file id is not a ULID".into(),
                                )
                            })?,
                            wrapped_key: wrapped,
                        })
                        .await
                    })
                    .await
                    .map_err(store_err)?;
                fresh
            }
        };
        let key = crate::capture_keys::encrypted_key(&prefix, &data_key.id, &actual)
            .map_err(|e| ApiError::internal(format!("could not build the capture key: {e}")))?;
        let sealed = data_key
            .seal(&compressed)
            .map_err(|e| ApiError::internal(format!("could not encrypt the chunk: {e}")))?;
        (key, sealed)
    } else {
        // **Plaintext, and the startup log says so.** Generating an ephemeral
        // key here would turn every existing chunk into undecryptable bytes
        // after a restart, and the symptom would be "it worked yesterday"
        let key = cormex_blob::hash::capture_key(&prefix, &actual)
            .map_err(|e| ApiError::internal(format!("could not build the capture key: {e}")))?;
        (key, compressed)
    };

    st.put_capture_object(&key, Bytes::from(stored))
        .await
        .map_err(|e| ApiError::internal(format!("could not store the chunk: {e}")))?;

    // ── ③ the row ──
    let outcome = store
        .archive_txn(async |t| {
            t.append_chunk(&NewCaptureChunk {
                id: Id::new(),
                generation_id: generation_id.parse().map_err(|_| {
                    cormex_store::StoreError::Invalid("the generation id is not a ULID".into())
                })?,
                byte_offset: offset,
                byte_length: i64::try_from(logical.len()).map_err(|_| {
                    cormex_store::StoreError::Invalid("the chunk length overflows i64".into())
                })?,
                content_sha256: actual.clone(),
                storage_key: key.clone(),
                encoding: ChunkEncoding::Zstd,
                observed_at,
            })
            .await
        })
        .await
        .map_err(store_err)?;

    match outcome {
        AppendOutcome::Appended { end_offset } => Ok(axum::Json(ChunkAck {
            end_offset,
            already_present: false,
        })
        .into_response()),
        AppendOutcome::AlreadyPresent { end_offset } => Ok(axum::Json(ChunkAck {
            end_offset,
            already_present: true,
        })
        .into_response()),
        AppendOutcome::OffsetMismatch { expected } => Ok((
            StatusCode::CONFLICT,
            axum::Json(OffsetMismatchBody::new(expected)),
        )
            .into_response()),
    }
}

/// `POST /capture/files/{id}/events` — record one lifecycle observation.
///
/// # Errors
/// The file does not belong to this device (403), or the write failed.
pub async fn file_event(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(file_id): Path<String>,
    axum::Json(req): axum::Json<cormex_proto::capture::FileEventRequest>,
) -> Result<StatusCode, ApiError> {
    let (tenant, device_id) = capture_caller(&st, &principal).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::unsupported(e.to_string()))?;

    let file = store
        .capture_file_by_id(&file_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found("no such capture file"))?;
    if file.device_id != device_id {
        return Err(ApiError::forbidden("this file belongs to another device"));
    }

    store
        .archive_txn(async |t| {
            t.insert_file_event(&NewCaptureFileEvent {
                id: Id::new(),
                file_id: file_id.parse().map_err(|_| {
                    cormex_store::StoreError::Invalid("the file id is not a ULID".into())
                })?,
                op: store_op(req.op),
                detail: req.detail.clone(),
            })
            .await
        })
        .await
        .map_err(store_err)?;
    Ok(StatusCode::NO_CONTENT)
}

// ══════════════════════════════════════════════════════════
//  helpers
// ══════════════════════════════════════════════════════════

/// The tenant and device id of a capture caller, or 403.
///
/// The message names the fix (`cormex login`) because the two ways to get here
/// are "the CLI has no device key" and "the user pasted a user token into it",
/// and both are resolved by the same command.
async fn capture_caller(
    st: &AppState,
    principal: &Principal,
) -> Result<(Tenant, String), ApiError> {
    let device = principal.device().ok_or_else(|| {
        ApiError::forbidden("capture requires a device key: run `cormex login` on this machine")
    })?;
    // `has_scope` rather than `device.has`: it is the one place that answers
    // "may this caller do X", and the archive/MCP handlers ask the same question
    // per call. Going through it here keeps a single answer for a user token
    // (which has every scope) and a device key (which has only what it was
    // issued with)
    if !principal.has_scope(crate::device_keys::Scope::CaptureWrite) {
        return Err(ApiError::forbidden(
            "this device key does not carry capture:write; re-issue it with that scope",
        ));
    }
    let tenant = principal.tenant(st).await?;
    Ok((tenant, device.device_id.clone()))
}

fn tenant_prefix(tenant: &Tenant) -> Result<cormex_blob::TenantPrefix, ApiError> {
    cormex_blob::TenantPrefix::new(tenant.schema())
        .map_err(|e| ApiError::internal(format!("the tenant name is not a legal key prefix: {e}")))
}

/// Decompress a `Content-Encoding: zstd` body, **with a cap**.
///
/// `zstd::decode_all` on an attacker-supplied frame is an OOM: a few kilobytes
/// of zstd can expand to gigabytes. `Decoder::take` stops at the ceiling, and
/// the caller then rejects anything that reached it — a chunk that decompresses
/// past the DDL's `byte_length` bound could never have been stored anyway.
fn decompress(body: &[u8]) -> Result<Vec<u8>, ApiError> {
    use std::io::Read as _;
    let mut out = Vec::new();
    zstd::Decoder::new(body)
        .map_err(|e| ApiError::unprocessable(format!("the body is not a zstd frame: {e}")))?
        // One byte past the ceiling, so "exactly at the limit" and "over it" are
        // distinguishable
        .take(u64::try_from(MAX_DECOMPRESSED).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut out)
        .map_err(|e| ApiError::unprocessable(format!("the zstd frame does not decode: {e}")))?;
    Ok(out)
}

fn store_role(role: FileRole) -> cormex_store::FileRole {
    match role {
        FileRole::Transcript => cormex_store::FileRole::Transcript,
        FileRole::Subagent => cormex_store::FileRole::Subagent,
        FileRole::Sidecar => cormex_store::FileRole::Sidecar,
        FileRole::Attachment => cormex_store::FileRole::Attachment,
        FileRole::Memory => cormex_store::FileRole::Memory,
        FileRole::Row => cormex_store::FileRole::Row,
    }
}

fn store_framing(framing: Framing) -> cormex_store::Framing {
    match framing {
        Framing::Append => cormex_store::Framing::Append,
        Framing::Snapshot => cormex_store::Framing::Snapshot,
    }
}

fn store_reason(reason: GenerationReason) -> StoreReason {
    match reason {
        GenerationReason::Initial => StoreReason::Initial,
        GenerationReason::Rewrite => StoreReason::Rewrite,
        GenerationReason::Truncate => StoreReason::Truncate,
        GenerationReason::Snapshot => StoreReason::Snapshot,
    }
}

fn store_op(op: FileEventOp) -> cormex_store::FileEventOp {
    match op {
        FileEventOp::Missing => cormex_store::FileEventOp::Missing,
        FileEventOp::Reappeared => cormex_store::FileEventOp::Reappeared,
        FileEventOp::Excluded => cormex_store::FileEventOp::Excluded,
        FileEventOp::Included => cormex_store::FileEventOp::Included,
    }
}

fn store_err(e: cormex_store::StoreError) -> ApiError {
    ApiError::internal(format!("the capture write failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A decompression bomb is refused rather than allowed to exhaust memory.
    ///
    /// This is the one test in this file that does not need a database, and it
    /// covers the failure that would be least visible: without the cap the
    /// process dies, and the log shows nothing but the OOM killer.
    #[test]
    fn a_decompression_bomb_is_capped() {
        // 64 MiB of zeroes compresses to a few kilobytes
        use std::io::Read as _;
        let bomb = zstd::encode_all(std::io::repeat(0).take(64 << 20), 3)
            .expect("compressing the bomb should succeed");
        assert!(
            bomb.len() < (1 << 20),
            "the test's own premise: the compressed form should be tiny, got {} bytes",
            bomb.len()
        );
        let out = decompress(&bomb).expect("decompression itself should not error");
        assert!(
            out.len() <= MAX_DECOMPRESSED + 1,
            "the decompressor produced {} bytes; without a cap one request takes the process \
             down and the log shows only the OOM killer",
            out.len()
        );
    }

    /// A body that is not a zstd frame is a 422, not a panic or a 500.
    #[test]
    fn a_non_zstd_body_is_rejected() {
        assert!(
            decompress(b"not zstd at all").is_err(),
            "a plaintext body sent with Content-Encoding: zstd must be refused"
        );
    }

    // ══════════════════════════════════════════════════════════
    //  End to end, against a real database and a local object store
    // ══════════════════════════════════════════════════════════

    use axum::http::Method;

    /// The whole capture flow, including every way it can go wrong.
    ///
    /// One test rather than eight because they share an expensive setup (a
    /// fresh database, a live backend, a device key) and because the interesting
    /// properties are about **order**: the object is written before the row, a
    /// replay is a 200 rather than a 409, and a device cannot write into another
    /// device's generation.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_capture_flow_stores_bytes_contiguously_and_refuses_the_wrong_caller() {
        let Some(base) = crate::test_support::database_url() else {
            eprintln!("skipped: DATABASE_URL is not set");
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let user = st
            .create_account("capture-user", "correct horse battery")
            .await
            .expect("create the account");
        let app = crate::routes::router(st.clone());
        let token = st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));

        // ── A device key with capture:write ──
        let (status, issued) = crate::test_support::call(
            &app,
            Method::POST,
            "/devices",
            Some(&token),
            Some(serde_json::json!({
                "label": "capture-laptop",
                "scopes": ["capture:write"]
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "minting a device key: {issued}");
        let device_key = issued["key"].as_str().expect("a key").to_owned();

        // ── The catalog is reachable with capture:write ──
        let (status, catalog) = crate::test_support::call(
            &app,
            Method::GET,
            "/capture/catalog",
            Some(&device_key),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the catalog should be readable: {catalog}"
        );
        assert!(
            catalog["agents"].as_array().is_some_and(|a| !a.is_empty()),
            "the catalog came back with no agents: {catalog}"
        );

        // ── Register a file ──
        let (status, registered) = crate::test_support::call(
            &app,
            Method::POST,
            "/capture/files",
            Some(&device_key),
            Some(serde_json::json!({
                "agent": "claude_code",
                "native_session_id": "native-1",
                "logical_path": "projects/-home-u-proj/s1.jsonl",
                "role": "transcript",
                "framing": "append",
                "cwd": "/home/u/proj"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "registering a file: {registered}");
        let file_id = registered["file_id"]
            .as_str()
            .expect("a file id")
            .to_owned();
        let generation = registered["generation"]["id"]
            .as_str()
            .expect("a generation id")
            .to_owned();
        assert_eq!(
            registered["generation"]["end_offset"], 0,
            "a freshly registered file ends at 0"
        );

        // Registering again is idempotent: same ids, still one generation
        let (status, again) = crate::test_support::call(
            &app,
            Method::POST,
            "/capture/files",
            Some(&device_key),
            Some(serde_json::json!({
                "agent": "claude_code",
                "native_session_id": "native-1",
                "logical_path": "projects/-home-u-proj/s1.jsonl",
                "role": "transcript",
                "framing": "append"
            })),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            again["file_id"], file_id,
            "registering the same file twice produced a different id"
        );
        assert_eq!(again["generation"]["id"], generation);

        // ── Append two chunks ──
        let first = br#"{"type":"user","text":"hello"}"#.to_vec();
        let second = br#"{"type":"assistant","text":"hi"}"#.to_vec();
        let sha = |b: &[u8]| cormex_blob::hash::sha256_hex(b);

        let put = |offset: i64, body: Vec<u8>, sha: String| {
            let path = format!("/capture/generations/{generation}/chunks/{offset}");
            let headers = vec![
                ("x-content-sha256", sha),
                ("x-observed-at", "2026-10-29T00:00:00Z".to_owned()),
            ];
            (path, headers, body)
        };

        let (path, headers, body) = put(0, first.clone(), sha(&first));
        let (status, ack) =
            crate::test_support::call_bytes(&app, Method::PUT, &path, &device_key, &headers, body)
                .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the first chunk: {}",
            String::from_utf8_lossy(&ack)
        );
        let ack: ChunkAck = serde_json::from_slice(&ack).expect("a ChunkAck");
        assert!(!ack.already_present);
        let end = ack.end_offset;

        let (path, headers, body) = put(end, second.clone(), sha(&second));
        let (status, ack) =
            crate::test_support::call_bytes(&app, Method::PUT, &path, &device_key, &headers, body)
                .await;
        assert_eq!(status, StatusCode::OK);
        let ack: ChunkAck = serde_json::from_slice(&ack).expect("a ChunkAck");
        assert_eq!(
            ack.end_offset,
            end + i64::try_from(second.len()).expect("fits")
        );

        // ── Replaying the same chunk is a 200 with already_present ──
        let (path, headers, body) = put(0, first.clone(), sha(&first));
        let (status, ack) =
            crate::test_support::call_bytes(&app, Method::PUT, &path, &device_key, &headers, body)
                .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a replay must be a 200: a client that died between 'the server accepted it' and              'I recorded it' retries, and a 409 would make it treat a success as a failure"
        );
        let ack: ChunkAck = serde_json::from_slice(&ack).expect("a ChunkAck");
        assert!(
            ack.already_present,
            "the replay should be marked already_present"
        );

        // ── A gap is a 409 carrying where to resume ──
        let far = 10_000;
        let (path, headers, body) = put(far, b"gap".to_vec(), sha(b"gap"));
        let (status, body) =
            crate::test_support::call_bytes(&app, Method::PUT, &path, &device_key, &headers, body)
                .await;
        assert_eq!(status, StatusCode::CONFLICT, "a gap must be refused");
        let mismatch: OffsetMismatchBody =
            serde_json::from_slice(&body).expect("an OffsetMismatchBody");
        assert_eq!(mismatch.error, OffsetMismatchBody::ERROR);
        assert_eq!(
            mismatch.expected_offset,
            end + i64::try_from(second.len()).expect("fits"),
            "the 409 must say where the generation actually ends"
        );

        // ── A wrong sha256 is a 422 ──
        let (path, headers, body) = put(
            end + i64::try_from(second.len()).expect("fits"),
            b"third".to_vec(),
            sha(b"something else"),
        );
        let (status, _) =
            crate::test_support::call_bytes(&app, Method::PUT, &path, &device_key, &headers, body)
                .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "a body that does not match its declared sha256 must be refused: the hash is the              idempotency key, so a wrong one makes a replay compare against the wrong bytes"
        );

        // ── A zstd-encoded body is accepted ──
        let third = br#"{"type":"user","text":"compressed"}"#.to_vec();
        let compressed = zstd::encode_all(third.as_slice(), 3).expect("compress");
        let path = format!(
            "/capture/generations/{generation}/chunks/{}",
            end + i64::try_from(second.len()).expect("fits")
        );
        let headers = vec![
            ("x-content-sha256", sha(&third)),
            ("x-observed-at", "2026-10-29T00:00:01Z".to_owned()),
            ("content-encoding", "zstd".to_owned()),
        ];
        let (status, ack) = crate::test_support::call_bytes(
            &app,
            Method::PUT,
            &path,
            &device_key,
            &headers,
            compressed,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a zstd body: {}",
            String::from_utf8_lossy(&ack)
        );

        // ── The objects are under capture/, and the logical bytes round-trip ──
        //
        // **Either layout is correct.** With `CORMEX_SECRET_KEY` set the chunk
        // is sealed under a per-file data key and its path carries the key id;
        // without it the path is the plaintext one. What this test pins is that
        // the object was written *before* the row, so a row never points at
        // nothing -- which layout it is, is a deployment property
        let plain = cormex_blob::hash::capture_key(
            &cormex_blob::TenantPrefix::new("public").expect("public is a legal prefix"),
            &sha(&first),
        )
        .expect("a capture key");
        let found = if objects.path().join(&plain).exists() {
            Some(objects.path().join(&plain))
        } else {
            // `public/capture/<key_id>/aa/bb/<sha>.zst.enc` — the key id sits
            // between `capture/` and the buckets, so the search is over that
            // one level
            let segments: Vec<&str> = plain.split('/').collect();
            let base = objects.path().join(segments[0]).join(segments[1]);
            // `public/capture/aa/bb/<sha>.zst` → the tail is `aa/bb/<sha>.zst.enc`
            let tail = segments[2..].join("/") + ".enc";
            std::fs::read_dir(&base).ok().and_then(|entries| {
                entries
                    .filter_map(Result::ok)
                    .map(|e| e.path().join(&tail))
                    .find(|p| p.exists())
            })
        };
        let found = found.unwrap_or_else(|| {
            panic!(
                "the first chunk's object is at neither {plain} nor an encrypted sibling; the R \
                 layer stores bytes before the row, and a row pointing at a missing object can \
                 never be healed"
            )
        });
        let stored = std::fs::read(&found).expect("read the object");
        if found.extension().and_then(|e| e.to_str()) == Some("enc") {
            // Decrypting needs the key row, which the API does not expose; the
            // round trip through the API is what `an_encrypted_chunk_reads_back`
            // covers. Here it is enough that the object exists and is not the
            // plaintext frame
            assert_ne!(
                stored.first(),
                Some(&0x28u8),
                "an encrypted object starts with the zstd magic number, so it was not encrypted"
            );
        } else {
            let logical = zstd::decode_all(stored.as_slice()).expect("the object should be zstd");
            assert_eq!(
                logical, first,
                "the stored object does not decode back to the logical bytes"
            );
        }

        // ── A user token cannot capture ──
        let (status, _) =
            crate::test_support::call(&app, Method::GET, "/capture/catalog", Some(&token), None)
                .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a user token reached the capture endpoints; capture is a device-key operation"
        );

        // ── Another device cannot write into this generation ──
        let (_, other) = crate::test_support::call(
            &app,
            Method::POST,
            "/devices",
            Some(&token),
            Some(serde_json::json!({ "label": "other", "scopes": ["capture:write"] })),
        )
        .await;
        let other_key = other["key"].as_str().expect("a key").to_owned();
        let (path, headers, body) = put(0, b"intruder".to_vec(), sha(b"intruder"));
        let (status, _) =
            crate::test_support::call_bytes(&app, Method::PUT, &path, &other_key, &headers, body)
                .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a second device appended into the first device's generation -- that is a write into              another machine's history, and it would corrupt it silently"
        );

        // ── Opening a generation on somebody else's file is refused too ──
        let (status, _) = crate::test_support::call(
            &app,
            Method::POST,
            &format!("/capture/files/{file_id}/generations"),
            Some(&other_key),
            Some(serde_json::json!({ "reason": "rewrite" })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a device opened a generation on another device's file"
        );

        // ── A rewrite opens a new generation, and the old one keeps its bytes ──
        let (status, new_gen) = crate::test_support::call(
            &app,
            Method::POST,
            &format!("/capture/files/{file_id}/generations"),
            Some(&device_key),
            Some(serde_json::json!({ "reason": "rewrite" })),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "opening a generation: {new_gen}");
        assert_eq!(
            new_gen["ordinal"], 1,
            "the new generation should be ordinal 1"
        );
        assert_eq!(new_gen["end_offset"], 0);

        let old_end = st
            .public_store()
            .expect("a live backend")
            .generation_end(&generation)
            .await
            .expect("read the old generation's end");
        assert!(
            old_end > 0,
            "the old generation lost its chunks when a new one was opened; generations are              branches, not versions"
        );

        // ── A file event is recorded ──
        let (status, _) = crate::test_support::call(
            &app,
            Method::POST,
            &format!("/capture/files/{file_id}/events"),
            Some(&device_key),
            Some(serde_json::json!({ "op": "missing", "detail": "the host deleted it" })),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        db.drop().await;
    }
}

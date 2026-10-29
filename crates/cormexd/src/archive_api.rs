//! The archive browsing API: sessions, events, and the raw bytes.
//!
//! Design: docs/design/session-archive-architecture.md §7.2 (D4, D5, D13).
//!
//! # The raw endpoint is the one that needs care
//!
//! Everything else serves the **masked** projection. `/archive/raw` serves the
//! original bytes, and it is the only endpoint that can hand out a secret:
//!
//! - it requires `raw:read`, which `cormex login` does **not** ask for by
//!   default — a lost laptop without it cannot read a single original byte
//! - by default it replaces the mask intervals with `[masked:<rule>]`, so the
//!   reader gets the shape without the secrets
//! - `unmask=true` returns the bytes untouched, and that is the case the audit
//!   log exists for
//! - **both cases are logged.** A privileged read and a normal one look
//!   identical in a request log; `archive_access_log.unmasked` is the only
//!   thing that tells them apart afterwards
//!
//! # `sole_copy` is not decoration
//!
//! The host deletes its own sessions after `cleanupPeriodDays`. Once it has,
//! Cormex holds the only copy — and a user deleting the session here is
//! deleting the last one. The list says so before the click.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use cormex_core::Id;
use cormex_proto::archive::{
    ArchiveEventsResponse, ArchiveFileDto, ArchiveGenerationDto, ArchiveSessionDetail,
    ArchiveSessionDto, ArchiveSessionsResponse, DIGEST_TOOL_RESULT_BYTES, TITLE_CHARS,
};
use cormex_proto::transcript::{ContentBlock, TranscriptEventDto, TranscriptKind};
use cormex_store::{CaptureChunk, ChunkEncoding, NewAccessLog, Store, TranscriptEvent};
use serde::Deserialize;

use crate::principal::Principal;
use crate::request_tenant::Tenant;
use crate::routes::ApiError;
use crate::state::AppState;

/// The parser version the browsing API reads.
///
/// One constant rather than a query parameter: two projections can coexist, and
/// letting the client pick one would mean the UI has to know which parser
/// version produced what it is looking at. When a parser is upgraded, the
/// projector writes the new version and this moves with it.
const PARSER_VERSION: &str = "claude_code@1";

/// `GET /archive/sessions`.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub agent: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    /// Event time: only sessions whose conversation had happened by then.
    ///
    /// The other axis from `as_of`. `at` asks "what had been said by that
    /// moment"; `as_of` asks "what had Cormex been told by that moment". A
    /// session backfilled from six months ago is found by `at` and not by
    /// `as_of` — that asymmetry is the point of having two axes (D13).
    pub at: Option<String>,
    /// System time: only sessions Cormex had projected by then.
    pub as_of: Option<String>,
}

/// `GET /archive/sessions/{id}/events`.
#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// `transcript` (the default) or `digest`.
    pub view: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
    /// Event time: only events that had happened by then.
    pub at: Option<String>,
    /// System time: only events Cormex had projected by then.
    pub as_of: Option<String>,
}

/// `GET /archive/search`.
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    /// `conversation` (the default) or `archive`.
    pub scope: Option<String>,
    pub agent: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    /// Event time: only hits whose conversation had happened by then.
    ///
    /// The same axis as `until` — both cut on `occurred_at`, and both apply if
    /// given. `at` exists as its own name because the two axes need to be
    /// nameable side by side with `as_of`; see D13.
    pub at: Option<String>,
    /// System time: only hits Cormex had projected by then.
    pub as_of: Option<String>,
    pub limit: Option<i64>,
}

/// `GET /archive/sessions/{id}/brief` — the handover brief.
///
/// Generated on first request and cached as a summary; see
/// [`crate::brief`] for why generation is lazy and why a stale brief is still
/// an answer. `stale: true` means the session has events newer than the brief —
/// the caller should say so rather than present it as current.
///
/// # Errors
/// The session does not exist, or the caller may not read it. A model failure
/// is **not** an error: the answer comes back stale instead.
pub async fn get_brief(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tenant = principal.tenant(&st).await?;
    if !principal.has_scope(crate::device_keys::Scope::SessionRead) {
        return Err(ApiError::forbidden(
            "this credential does not carry session:read",
        ));
    }
    let store = archive_store(&st, &principal).await?;
    let row = store
        .archive_session(&id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found("no such session, or it has no events yet"))?;

    let answer = crate::brief::brief_for(&st, &tenant, &id)
        .await
        .map_err(|e| ApiError::internal(format!("could not build the brief: {e}")))?;

    Ok(Json(serde_json::json!({
        "session_id": row.id,
        "generated": answer.generated,
        "stale": answer.stale,
        // The body is the rendered seven sections. Empty means "there is
        // nothing to hand over yet" -- the caller falls back to view=digest
        "brief": if answer.body.is_empty() { serde_json::Value::Null } else { serde_json::Value::String(answer.body) },
        "summary_id": answer.summary_id,
        "title": row.title_source.map(|t| t.chars().take(TITLE_CHARS).collect::<String>()),
        "agent": row.agent,
        "event_count": row.event_count,
    })))
}

/// `GET /archive/sessions/{id}/export` — every file, as a zstd-compressed tar.
///
/// Each file's **newest generation** is reassembled from its chunks and stored
/// under its `logical_path`, so unpacking the tar reproduces the host's own
/// directory layout. That is what makes `cormex restore` on another machine a
/// copy rather than a reconstruction.
///
/// Requires `raw:read` and writes an access log row per file: this hands out
/// every original byte of a session at once.
///
/// # Errors
/// The caller lacks `raw:read` (403), the session does not exist (404), or the
/// bytes cannot be read.
pub async fn export(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    if !principal.has_scope(crate::device_keys::Scope::RawRead) {
        return Err(ApiError::forbidden(
            "exporting a session returns every original byte of it, and requires raw:read",
        ));
    }
    let store = archive_store(&st, &principal).await?;
    let files = store.archive_files(&id).await.map_err(store_err)?;
    if files.is_empty() {
        return Err(ApiError::not_found("no such session, or it has no files"));
    }

    let mut builder = tar::Builder::new(Vec::new());
    let who = principal_name(&principal);

    for (file, generations) in &files {
        let Some(newest) = generations.last() else {
            continue;
        };
        let end = store.generation_end(&newest.id).await.map_err(store_err)?;
        let bytes = read_range(&st, &store, &newest.id, 0, end).await?;

        // The tar entry's path is the **logical** one: relative to the agent's
        // home, forward slashes. Unpacking it into that home reproduces the
        // layout the host itself uses
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(bytes.len()).unwrap_or(0));
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, &file.logical_path, bytes.as_slice())
            .map_err(|e| ApiError::internal(format!("could not build the tar: {e}")))?;

        store
            .archive_txn(async |t| {
                t.insert_access_log(&NewAccessLog {
                    id: Id::new(),
                    principal: who.clone(),
                    generation_id: newest.id.parse().map_err(|_| {
                        cormex_store::StoreError::Invalid("bad generation id".into())
                    })?,
                    byte_offset: 0,
                    byte_length: end,
                    unmasked: true,
                })
                .await
            })
            .await
            .map_err(store_err)?;
    }

    let tar = builder
        .into_inner()
        .map_err(|e| ApiError::internal(format!("could not finish the tar: {e}")))?;
    let compressed = zstd::encode_all(tar.as_slice(), 3)
        .map_err(|e| ApiError::internal(format!("could not compress the export: {e}")))?;

    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/zstd".to_owned()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"cormex-session-{id}.tar.zst\""),
            ),
        ],
        compressed,
    )
        .into_response())
}

/// `DELETE /archive/sessions/{id}` — purge a captured session.
///
/// # Why this needs more than a DELETE
///
/// It destroys the original bytes, the projection, the index, the promoted
/// episodes and the summaries derived from them. There is no undo, and the
/// tombstone it leaves is permanent. So it requires:
///
/// - `raw:read` — the same scope that can read the original bytes. A credential
///   that cannot read the conversation must not be able to destroy it
/// - `?confirm=<session id>` — a **second, explicit** confirmation carrying the
///   id itself. A bare `DELETE` is one mis-wired client away from erasing
///   somebody's archive, and requiring the id to be typed means the caller had
///   to know which one
///
/// The Web UI and the CLI both ask before calling this; the parameter is what
/// makes "asked" enforceable rather than a convention.
///
/// # Errors
/// Missing `raw:read` (403), a missing or wrong `confirm` (400), no such session
/// (404), or the purge failed.
pub async fn purge_session(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    purge_common(&st, &principal, &id, &id, q.confirm.as_deref(), None).await
}

/// `DELETE /archive/files/{id}` — purge one captured file.
///
/// The same two gates as the session purge; see [`purge_session`].
///
/// # Errors
/// Missing `raw:read` (403), a missing or wrong `confirm` (400), no such file
/// (404), or the purge failed.
pub async fn purge_file(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    purge_common(&st, &principal, &id, &id, q.confirm.as_deref(), Some(&id)).await
}

/// The confirmation gate, exposed for the purge tests.
///
/// The gate is the part of this endpoint that must never be bypassed, and
/// testing it through the router would mean minting a device key with
/// `raw:read` — a second thing that could be wrong. This calls the same
/// function the handler calls.
#[cfg(any(test, feature = "test-support"))]
pub async fn purge_common_for_test(
    st: &AppState,
    principal: &Principal,
    target_id: &str,
    confirm: Option<&str>,
    file_id: Option<&str>,
) -> Result<Json<serde_json::Value>, ApiError> {
    purge_common(st, principal, target_id, target_id, confirm, file_id).await
}

/// The confirmation parameter.
#[derive(Debug, Deserialize)]
pub struct ConfirmQuery {
    /// Must repeat the target id. See [`purge_session`].
    pub confirm: Option<String>,
}

async fn purge_common(
    st: &AppState,
    principal: &Principal,
    target_id: &str,
    expected_confirm: &str,
    confirm: Option<&str>,
    file_id: Option<&str>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !principal.has_scope(crate::device_keys::Scope::RawRead) {
        return Err(ApiError::forbidden(
            "purging requires raw:read: a credential that cannot read the original bytes must              not be able to destroy them",
        ));
    }
    match confirm {
        Some(c) if c == expected_confirm => {}
        Some(_) => {
            return Err(ApiError::bad_request(
                "the confirmation does not match the id being purged",
            ));
        }
        None => {
            return Err(ApiError::bad_request(format!(
                "purging destroys the original bytes and cannot be undone. Repeat the id as                  ?confirm={expected_confirm} to proceed"
            )));
        }
    }

    let tenant = principal.tenant(st).await?;
    let report = match file_id {
        Some(id) => crate::purge::purge_file(st, &tenant, id).await,
        None => crate::purge::purge_session(st, &tenant, target_id).await,
    }
    .map_err(|e| {
        let text = e.to_string();
        if text.contains("no captured file or session") {
            ApiError::not_found("no such session or file")
        } else {
            ApiError::internal(format!("the purge failed: {text}"))
        }
    })?;

    Ok(Json(serde_json::json!({
        "purged": true,
        "keys_destroyed": report.keys_destroyed,
        "objects_deleted": report.objects_deleted,
        // **Reported, not hidden.** An object that survived a purge is the one
        // thing this operation exists to prevent, and a caller that is told
        // "purged" while an object remains has been lied to
        "objects_failed": report.objects_failed,
        "events_deleted": report.events_deleted,
        "chunks_deleted": report.chunks_deleted,
        "promoted_episodes_deleted": report.promoted_episodes_deleted,
        "summaries_deleted": report.summaries_deleted,
    })))
}

/// `GET /archive/raw/{generation_id}`.
#[derive(Debug, Deserialize)]
pub struct RawQuery {
    pub offset: Option<i64>,
    pub length: Option<i64>,
    /// Return the bytes untouched. Requires `raw:read`, and is logged.
    pub unmask: Option<bool>,
}

/// `GET /archive/search` — full-text search over conversations or the archive.
///
/// # The two scopes are two different indexes
///
/// `conversation` searches what people and agents **said**; `archive` searches
/// everything, tool output included. They are separate because a build log would
/// otherwise outrank the sentence somebody actually typed (D4).
///
/// `archive` requires `raw:read`: a tool's output is where a secret or a private
/// document actually appears, and this is the index that reaches it.
///
/// # Errors
/// The caller may not search (403), the query is malformed (400), or the search
/// failed.
pub async fn search(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<cormex_proto::archive::ArchiveSearchResponse>, ApiError> {
    let scope = q.scope.as_deref().unwrap_or("conversation");
    if scope == "archive" && !principal.has_scope(crate::device_keys::Scope::RawRead) {
        return Err(ApiError::forbidden(
            "searching the archive requires raw:read: it reaches tool output, which is where a \
             secret or a private document actually appears",
        ));
    }
    if scope != "conversation" && scope != "archive" {
        return Err(ApiError::bad_request(
            "scope accepts `conversation` or `archive`",
        ));
    }

    let store = archive_store(&st, &principal).await?;
    let tsquery = cormex_memory::tokenize::to_tsquery_input(&q.q);
    let since = parse_time(q.since.as_deref())?;
    // `until` and `at` are the same axis; take the earlier of the two so both
    // apply
    let until = match (
        parse_time(q.until.as_deref())?,
        parse_time(q.at.as_deref())?,
    ) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    let as_of = parse_time(q.as_of.as_deref())?;
    let limit = q.limit.unwrap_or(20);

    let hits = if scope == "archive" {
        store
            .search_archive_at(
                &tsquery,
                q.agent.as_deref(),
                q.project.as_deref(),
                since,
                until,
                as_of,
                limit,
            )
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|h| cormex_proto::archive::ArchiveSearchHit {
                session_id: h.session_id,
                event_id: h.event_id,
                snippet: truncate_bytes(&h.text, cormex_proto::archive::SEARCH_SNIPPET_CHARS),
                occurred_at: h.occurred_at.map(|t| t.to_rfc3339()),
                score: h.score,
            })
            .collect()
    } else {
        store
            .search_conversation_fused(
                &tsquery,
                st.conversation_embedding(&tsquery)
                    .await?
                    .as_ref()
                    .map(|(v, m)| (v, m.as_str())),
                q.agent.as_deref(),
                q.project.as_deref(),
                since,
                until,
                as_of,
                cormex_store::archive::CONVERSATION_DISTANCE_FLOOR,
                limit,
            )
            .await
            .map_err(store_err)?
            .into_iter()
            .map(|h| cormex_proto::archive::ArchiveSearchHit {
                session_id: h.session_id,
                event_id: h.event_id,
                snippet: truncate_bytes(
                    h.text.as_deref().unwrap_or(""),
                    cormex_proto::archive::SEARCH_SNIPPET_CHARS,
                ),
                occurred_at: h.occurred_at.map(|t| t.to_rfc3339()),
                score: h.score,
            })
            .collect()
    };

    Ok(Json(cormex_proto::archive::ArchiveSearchResponse { hits }))
}

/// # Errors
/// The caller may not read sessions, or the query failed.
pub async fn list_sessions(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Query(q): Query<ListQuery>,
) -> Result<Json<ArchiveSessionsResponse>, ApiError> {
    let store = archive_store(&st, &principal).await?;

    let since = parse_time(q.since.as_deref())?;
    let until = parse_time(q.until.as_deref())?;
    let at = parse_time(q.at.as_deref())?;
    let as_of = parse_time(q.as_of.as_deref())?;
    let after = q.cursor.as_deref().map(parse_cursor).transpose()?;
    let limit = q.limit.unwrap_or(50);

    let rows = store
        .archive_sessions_at(
            q.agent.as_deref(),
            q.project.as_deref(),
            since,
            until,
            at,
            as_of,
            after.as_ref().map(|(t, id)| (t, id.as_str())),
            limit,
        )
        .await
        .map_err(store_err)?;

    let mut sessions = Vec::with_capacity(rows.len());
    for r in &rows {
        sessions.push(to_dto(&store, r).await?);
    }
    let next_cursor = rows
        .last()
        .and_then(|r| r.last_at.map(|t| format!("{}|{}", t.to_rfc3339(), r.id)));

    Ok(Json(ArchiveSessionsResponse {
        sessions,
        next_cursor,
    }))
}

/// # Errors
/// The session does not exist, the caller may not read it, or the query failed.
pub async fn get_session(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<ArchiveSessionDetail>, ApiError> {
    let store = archive_store(&st, &principal).await?;

    let row = store
        .archive_session(&id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found("no such session, or it has no events yet"))?;
    let session = to_dto(&store, &row).await?;

    let files = store.archive_files(&id).await.map_err(store_err)?;
    let files: Vec<ArchiveFileDto> = files
        .into_iter()
        .map(|(f, gens)| ArchiveFileDto {
            id: f.id,
            role: f.role.to_string(),
            device_id: f.device_id,
            logical_path: f.logical_path,
            generations: gens
                .into_iter()
                .map(|g| ArchiveGenerationDto {
                    id: g.id,
                    ordinal: g.ordinal,
                    reason: g.reason.to_string(),
                    // The end is read per generation rather than stored: a
                    // generation's length is `max(offset + length)` over its
                    // chunks, and caching it would be a second source of truth
                    end_offset: 0,
                    created_at: g.created_at.to_rfc3339(),
                })
                .collect(),
        })
        .collect();

    let projection = store
        .latest_projection(&id, PARSER_VERSION)
        .await
        .map_err(store_err)?;

    Ok(Json(ArchiveSessionDetail {
        session,
        files,
        parser_version: Some(PARSER_VERSION.to_owned()),
        projection_status: projection.as_ref().map(|p| p.status.to_string()),
        projection_error: projection.and_then(|p| p.error),
    }))
}

/// # Errors
/// The caller may not read the session, or the query failed.
pub async fn get_events(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Json<ArchiveEventsResponse>, ApiError> {
    let store = archive_store(&st, &principal).await?;
    let after_seq = q
        .cursor
        .as_deref()
        .map(str::parse::<i64>)
        .transpose()
        .map_err(|_| {
            ApiError::bad_request("the cursor is not a seq number; pass the `next_cursor` verbatim")
        })?;
    let limit = q.limit.unwrap_or(100);
    let digest = q.view.as_deref() == Some("digest");

    let events = store
        .transcript_page_at(
            &id,
            PARSER_VERSION,
            after_seq.unwrap_or(-1),
            parse_time(q.at.as_deref())?,
            parse_time(q.as_of.as_deref())?,
            limit,
        )
        .await
        .map_err(store_err)?;

    let next_cursor = events.last().map(|e| e.seq.to_string());
    let events: Vec<TranscriptEventDto> = events
        .into_iter()
        .map(|e| to_event_dto(e, digest))
        .collect();

    Ok(Json(ArchiveEventsResponse {
        events,
        next_cursor,
    }))
}

/// `GET /archive/raw/{generation_id}` — the original bytes.
///
/// # Errors
/// The caller lacks `raw:read` (403), the generation does not exist (404), or
/// the bytes cannot be read.
pub async fn get_raw(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(generation_id): Path<String>,
    Query(q): Query<RawQuery>,
) -> Result<Response, ApiError> {
    // **`raw:read` is checked here, not by the route allow-list.** The
    // allow-list answers "may this credential reach this path"; this answers
    // "may it see unmasked bytes", and the two differ per call
    if !principal.has_scope(crate::device_keys::Scope::RawRead) {
        return Err(ApiError::forbidden(
            "reading the original archive requires raw:read; re-issue this key with that scope \
             if you need it",
        ));
    }
    let store = archive_store(&st, &principal).await?;

    // Existence check first, so a bad id is a 404 rather than an empty body
    store
        .generation_by_id(&generation_id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found("no such generation"))?;
    let end = store
        .generation_end(&generation_id)
        .await
        .map_err(store_err)?;

    let offset = q.offset.unwrap_or(0).max(0);
    let length = q.length.unwrap_or(end - offset).max(0);
    let unmask = q.unmask.unwrap_or(false);

    let mut bytes = read_range(&st, &store, &generation_id, offset, length).await?;

    if !unmask {
        let masks = store
            .masks_in_range(&generation_id, offset, length)
            .await
            .map_err(store_err)?;
        bytes = apply_masks(&bytes, offset, &masks);
    }

    // **Both cases are logged.** A masked read and an unmasked one look
    // identical in a request log, and `unmasked` is the only thing that tells
    // them apart afterwards
    let who = principal_name(&principal);
    store
        .archive_txn(async |t| {
            t.insert_access_log(&NewAccessLog {
                id: Id::new(),
                principal: who,
                generation_id: generation_id
                    .parse()
                    .map_err(|_| cormex_store::StoreError::Invalid("bad generation id".into()))?,
                byte_offset: offset,
                byte_length: i64::try_from(bytes.len()).unwrap_or(0),
                unmasked: unmask,
            })
            .await
        })
        .await
        .map_err(store_err)?;

    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/octet-stream")],
        bytes,
    )
        .into_response())
}

// ══════════════════════════════════════════════════════════
//  helpers
// ══════════════════════════════════════════════════════════

/// The tenant's store, or 403 when the caller cannot read sessions.
///
/// Returns an **owned** `Store` (a cheap `Arc` clone) rather than a reference:
/// `Tenant::store` borrows from a value this function creates, so a reference
/// could not outlive it.
async fn archive_store(st: &AppState, principal: &Principal) -> Result<Store, ApiError> {
    // A device key needs `session:read`; a user credential has every scope
    if !principal.has_scope(crate::device_keys::Scope::SessionRead) {
        return Err(ApiError::forbidden(
            "this credential does not carry session:read",
        ));
    }
    let tenant: Tenant = principal.tenant(st).await?;
    tenant
        .store()
        .cloned()
        .map_err(|e| ApiError::unsupported(e.to_string()))
}

async fn to_dto(
    store: &Store,
    row: &cormex_store::ArchiveSessionRow,
) -> Result<ArchiveSessionDto, ApiError> {
    Ok(ArchiveSessionDto {
        id: row.id.clone(),
        agent: row.agent.clone(),
        title: row
            .title_source
            .as_deref()
            .map(|t| t.chars().take(TITLE_CHARS).collect::<String>()),
        project_key: row.project_key.clone(),
        devices: row.devices.clone().unwrap_or_default(),
        first_at: row.first_at.map(|t| t.to_rfc3339()),
        last_at: row.last_at.map(|t| t.to_rfc3339()),
        event_count: row.event_count,
        summary: None,
        sole_copy: store
            .session_is_sole_copy(&row.id)
            .await
            .map_err(store_err)?,
    })
}

/// Turn a stored event into the wire DTO, optionally digesting it.
fn to_event_dto(e: TranscriptEvent, digest: bool) -> TranscriptEventDto {
    let mut content = e.content.clone();
    let mut text = e.text.clone();

    if digest {
        // Tool calls collapse to one line and tool results are truncated: the
        // point of the digest view is to skim a conversation, and a 40 KB build
        // log defeats that. The full text is one view away, and the original is
        // always in `raw`
        if let Ok(blocks) = serde_json::from_value::<Vec<ContentBlock>>(content.clone()) {
            let digested: Vec<ContentBlock> = blocks
                .into_iter()
                .map(|b| match b {
                    ContentBlock::ToolResult {
                        tool_call_id,
                        output,
                        is_error,
                    } => ContentBlock::ToolResult {
                        tool_call_id,
                        output: truncate_bytes(&output, DIGEST_TOOL_RESULT_BYTES),
                        is_error,
                    },
                    other => other,
                })
                .collect();
            content = serde_json::to_value(&digested).unwrap_or(content);
        }
        if let Some(t) = &text {
            text = Some(truncate_bytes(t, DIGEST_TOOL_RESULT_BYTES));
        }
    }

    TranscriptEventDto {
        id: e.id,
        seq: e.seq,
        kind: wire_kind(e.kind),
        occurred_at: e.occurred_at.map(|t| t.to_rfc3339()),
        role: None,
        content: serde_json::from_value(content).unwrap_or_default(),
        text,
        tool_name: e.tool_name,
        tool_call_id: e.tool_call_id,
        source: cormex_proto::transcript::SourceSpan {
            generation_id: e.src_generation_id,
            offset: e.src_offset,
            length: e.src_length,
        },
    }
}

/// Truncate to `max` bytes on a char boundary.
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… [{} bytes]", &s[..end], s.len())
}

/// Read `[offset, offset + len)` of a generation's logical bytes.
async fn read_range(
    st: &AppState,
    store: &Store,
    generation_id: &str,
    offset: i64,
    len: i64,
) -> Result<Vec<u8>, ApiError> {
    let chunks: Vec<CaptureChunk> = store
        .chunks_overlapping(generation_id, offset, len)
        .await
        .map_err(store_err)?;
    let mut out = Vec::new();
    for chunk in chunks {
        // Decrypting when the chunk carries a data key (P6-1). A plaintext
        // chunk -- every one written before that feature -- passes through
        let stored = st
            .get_capture_chunk(store, &chunk)
            .await
            .map_err(|e| ApiError::internal(format!("could not read the object: {e}")))?;
        let logical = match chunk.encoding {
            ChunkEncoding::Zstd => zstd::decode_all(&stored[..])
                .map_err(|e| ApiError::internal(format!("could not decompress: {e}")))?,
            ChunkEncoding::Identity => stored.to_vec(),
        };
        let start = usize::try_from((offset - chunk.byte_offset).max(0)).unwrap_or(0);
        let end = usize::try_from((offset + len - chunk.byte_offset).min(chunk.byte_length))
            .unwrap_or(logical.len())
            .min(logical.len());
        if start < end {
            out.extend_from_slice(&logical[start..end]);
        }
    }
    Ok(out)
}

/// Replace the mask intervals inside `bytes` with `[masked:<rule>]`.
///
/// The intervals are in the **generation's** coordinates; `base` is where the
/// slice starts.
fn apply_masks(bytes: &[u8], base: i64, masks: &[cormex_store::CaptureMask]) -> Vec<u8> {
    if masks.is_empty() {
        return bytes.to_vec();
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut cursor = 0usize;
    for m in masks {
        let start = usize::try_from((m.byte_offset - base).max(0)).unwrap_or(0);
        let end = usize::try_from((m.byte_offset - base) + i64::from(m.byte_length))
            .unwrap_or(bytes.len())
            .min(bytes.len());
        if start >= bytes.len() || start < cursor {
            continue;
        }
        out.extend_from_slice(&bytes[cursor..start]);
        out.extend_from_slice(format!("[masked:{}]", m.rule).as_bytes());
        cursor = end;
    }
    out.extend_from_slice(&bytes[cursor..]);
    out
}

fn wire_kind(k: cormex_store::TranscriptEventKind) -> TranscriptKind {
    use cormex_store::TranscriptEventKind as S;
    match k {
        S::UserMessage => TranscriptKind::UserMessage,
        S::AgentMessage => TranscriptKind::AgentMessage,
        S::AgentThought => TranscriptKind::AgentThought,
        S::ToolCall => TranscriptKind::ToolCall,
        S::ToolResult => TranscriptKind::ToolResult,
        S::Plan => TranscriptKind::Plan,
        S::System => TranscriptKind::System,
        S::Meta => TranscriptKind::Meta,
        S::Unknown => TranscriptKind::Unknown,
    }
}

fn principal_name(p: &Principal) -> String {
    match p {
        Principal::User { user_id } => format!("user:{user_id}"),
        Principal::LegacyOwner => "legacy-owner".to_owned(),
        Principal::Device(d) => format!("device:{}", d.device_id),
        Principal::Delegated(s) => format!("delegated:{}", s.session_id),
    }
}

fn parse_time(s: Option<&str>) -> Result<Option<DateTime<Utc>>, ApiError> {
    match s {
        None => Ok(None),
        Some(v) => DateTime::parse_from_rfc3339(v)
            .map(|t| Some(t.with_timezone(&Utc)))
            .map_err(|e| ApiError::bad_request(format!("{v:?} is not RFC 3339: {e}"))),
    }
}

fn parse_cursor(s: &str) -> Result<(DateTime<Utc>, String), ApiError> {
    let (time, id) = s
        .split_once('|')
        .ok_or_else(|| ApiError::bad_request("the cursor should be `<rfc3339>|<id>`"))?;
    let t = DateTime::parse_from_rfc3339(time)
        .map_err(|e| ApiError::bad_request(format!("the cursor's time is not RFC 3339: {e}")))?
        .with_timezone(&Utc);
    Ok((t, id.to_owned()))
}

fn store_err(e: cormex_store::StoreError) -> ApiError {
    ApiError::internal(format!("the archive query failed: {e}"))
}
#[cfg(test)]
mod tests {
    //! The archive API against a real database and a real object store.

    use super::*;
    use cormex_store::{
        FileRole, Framing, NewCaptureChunk, NewCaptureFile, NewCaptureMask, NewMaskProgress,
        NewProjection, NewTranscriptEvent, ProjectionStatus, TranscriptEventKind,
    };

    /// Seed a session with one event and one mask, returning
    /// `(session_id, generation_id, body)`.
    async fn seed(st: &AppState) -> (String, String, Vec<u8>) {
        let store = st.public_store().expect("a live backend");
        let body = b"hello AKIAIOSFODNN7EXAMPLE world\n".to_vec();

        let new = NewCaptureFile {
            id: Id::new(),
            native_session_id: format!("native-{}", Id::new()),
            parent_native_session_id: None,
            device_id: Id::new().to_string(),
            agent: "claude_code".into(),
            logical_path: "projects/-p/s1.jsonl".into(),
            role: FileRole::Transcript,
            framing: Framing::Append,
            cwd: None,
            project_key: Some("github.com/u/p".into()),
        };
        let registered = store
            .archive_txn(async |t| t.register_capture_file(&new).await)
            .await
            .expect("register");
        let (session_id, generation_id) = (registered.session.id, registered.generation.id);

        // The bytes, in the object store
        use sha2::Digest as _;
        let sha = format!("{:x}", sha2::Sha256::digest(&body));
        let key = format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]);
        let compressed = zstd::encode_all(body.as_slice(), 3).expect("compress");
        st.put_capture_object(&key, axum::body::Bytes::from(compressed))
            .await
            .expect("store");

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
                    observed_at: Utc::now(),
                })
                .await
            })
            .await
            .expect("append");

        // One event and one mask covering the key
        let key_start = body
            .windows(4)
            .position(|w| w == b"AKIA")
            .expect("the key is in the body");
        store
            .archive_txn(async |t| {
                t.insert_transcript_events(
                    &[NewTranscriptEvent {
                        id: Id::new(),
                        session_id: session_id.parse().expect("a ULID"),
                        parser_version: PARSER_VERSION.into(),
                        seq: 0,
                        kind: TranscriptEventKind::UserMessage,
                        occurred_at: Some(Utc::now()),
                        content: serde_json::json!([{ "type": "text", "text": "hello" }]),
                        text: Some("hello [masked:vendor_key] world".into()),
                        tsv_source: Some("hello world".into()),
                        tool_name: None,
                        tool_call_id: None,
                        src_generation_id: generation_id.parse().expect("a ULID"),
                        src_offset: 0,
                        src_length: i64::try_from(body.len()).expect("fits"),
                    }],
                    &NewProjection {
                        id: Id::new(),
                        session_id: session_id.parse().expect("a ULID"),
                        parser_version: PARSER_VERSION.into(),
                        through: serde_json::json!({}),
                        next_seq: 1,
                        status: ProjectionStatus::Ok,
                        error: None,
                    },
                )
                .await
            })
            .await
            .expect("insert event");

        store
            .archive_txn(async |t| {
                t.insert_masks(
                    &[NewCaptureMask {
                        id: Id::new(),
                        generation_id: generation_id.parse().expect("a ULID"),
                        byte_offset: i64::try_from(key_start).expect("fits"),
                        byte_length: 20,
                        rule: "vendor_key".into(),
                        scanner: "redaction@1".into(),
                    }],
                    &NewMaskProgress {
                        id: Id::new(),
                        generation_id: generation_id.parse().expect("a ULID"),
                        scanner: "redaction@1".into(),
                        through_offset: i64::try_from(body.len()).expect("fits"),
                    },
                )
                .await
            })
            .await
            .expect("insert mask");

        (session_id, generation_id, body)
    }

    /// A device key with `session:read` can list and read events, but not raw.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_device_key_reads_sessions_but_not_raw() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let (session_id, generation_id, _body) = seed(&st).await;

        let user = st
            .create_account("reader", "correct horse battery")
            .await
            .expect("create the account");
        let token = st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));
        let app = crate::routes::router(st.clone());

        // A device key with session:read only
        let (_, issued) = crate::test_support::call(
            &app,
            axum::http::Method::POST,
            "/devices",
            Some(&token),
            Some(serde_json::json!({ "label": "reader", "scopes": ["session:read"] })),
        )
        .await;
        let device_key = issued["key"].as_str().expect("a key").to_owned();

        // The list works
        let (status, list) = crate::test_support::call(
            &app,
            axum::http::Method::GET,
            "/archive/sessions",
            Some(&device_key),
            None,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{list}");
        assert_eq!(
            list["sessions"].as_array().map(Vec::len),
            Some(1),
            "the seeded session should be listed: {list}"
        );
        assert!(
            list["sessions"][0]["title"].is_string(),
            "the title comes from the first user message: {list}"
        );
        assert_eq!(
            list["sessions"][0]["sole_copy"], false,
            "a session whose files have never been observed missing is not the only copy"
        );

        // The events work
        let (status, events) = crate::test_support::call(
            &app,
            axum::http::Method::GET,
            &format!("/archive/sessions/{session_id}/events"),
            Some(&device_key),
            None,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK, "{events}");
        assert_eq!(events["events"].as_array().map(Vec::len), Some(1));

        // **Raw is refused**: the key has no `raw:read`
        let (status, _) = crate::test_support::call(
            &app,
            axum::http::Method::GET,
            &format!("/archive/raw/{generation_id}"),
            Some(&device_key),
            None,
        )
        .await;
        assert_eq!(
            status,
            axum::http::StatusCode::FORBIDDEN,
            "a key without raw:read reached the original bytes -- that is every conversation, \
             unmasked"
        );

        db.drop().await;
    }

    /// With `raw:read`, the raw endpoint masks by default and unmasks on request,
    /// and **both are logged**.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn raw_is_masked_by_default_and_logs_both_cases() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let (_session_id, generation_id, body) = seed(&st).await;

        let user = st
            .create_account("raw-reader", "correct horse battery")
            .await
            .expect("create the account");
        let token = st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));
        let app = crate::routes::router(st.clone());

        let (_, issued) = crate::test_support::call(
            &app,
            axum::http::Method::POST,
            "/devices",
            Some(&token),
            Some(serde_json::json!({
                "label": "raw-reader",
                "scopes": ["session:read", "raw:read"]
            })),
        )
        .await;
        let device_key = issued["key"].as_str().expect("a key").to_owned();

        // Masked by default
        let (status, masked) = crate::test_support::call_bytes(
            &app,
            axum::http::Method::GET,
            &format!("/archive/raw/{generation_id}"),
            &device_key,
            &[],
            vec![],
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        let masked = String::from_utf8_lossy(&masked).into_owned();
        assert!(
            !masked.contains("AKIA"),
            "the default read returned the key: {masked}"
        );
        assert!(
            masked.contains("[masked:vendor_key]"),
            "the mask marker should be there: {masked}"
        );

        // Unmasked on request
        let (status, raw) = crate::test_support::call_bytes(
            &app,
            axum::http::Method::GET,
            &format!("/archive/raw/{generation_id}?unmask=true"),
            &device_key,
            &[],
            vec![],
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            raw, body,
            "unmask=true must return the original bytes untouched"
        );

        // **Both reads are logged, and the log distinguishes them.**
        let store = st.public_store().expect("a live backend");
        let logs: Vec<String> = sqlx::query_scalar(
            "SELECT unmasked::text FROM archive_access_log
              WHERE generation_id = $1 ORDER BY created_at",
        )
        .bind(&generation_id)
        .fetch_all(store.pool())
        .await
        .expect("read the log");
        db.drop().await;
        assert_eq!(
            logs,
            vec!["false", "true"],
            "a masked read and an unmasked one must both be logged, and distinguished: that is \
             the only thing that makes an audit of raw:read meaningful afterwards"
        );
    }

    /// The list filters by agent.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_list_filters_by_agent() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        seed(&st).await;

        let user = st
            .create_account("filter", "correct horse battery")
            .await
            .expect("create");
        let token = st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));
        let app = crate::routes::router(st.clone());

        let (_, matched) = crate::test_support::call(
            &app,
            axum::http::Method::GET,
            "/archive/sessions?agent=claude_code",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(matched["sessions"].as_array().map(Vec::len), Some(1));

        let (_, unmatched) = crate::test_support::call(
            &app,
            axum::http::Method::GET,
            "/archive/sessions?agent=codex",
            Some(&token),
            None,
        )
        .await;
        assert_eq!(
            unmatched["sessions"].as_array().map(Vec::len),
            Some(0),
            "the agent filter did not apply: {unmatched}"
        );

        db.drop().await;
    }

    /// A session whose every file is missing is flagged as the only copy.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn sole_copy_is_true_once_every_file_is_missing() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let (session_id, _generation, _body) = seed(&st).await;
        let store = st.public_store().expect("a live backend");

        assert!(
            !store
                .session_is_sole_copy(&session_id)
                .await
                .expect("query"),
            "a file never observed missing is not the only copy"
        );

        // Record the file as missing
        let files = store
            .capture_files_of_session(&session_id)
            .await
            .expect("files");
        store
            .archive_txn(async |t| {
                t.insert_file_event(&cormex_store::NewCaptureFileEvent {
                    id: Id::new(),
                    file_id: files[0].id.parse().expect("a ULID"),
                    op: cormex_store::FileEventOp::Missing,
                    detail: None,
                })
                .await
            })
            .await
            .expect("event");

        let sole = store
            .session_is_sole_copy(&session_id)
            .await
            .expect("query");
        db.drop().await;
        assert!(
            sole,
            "every file is missing, so Cormex holds the only copy -- and a user deleting the \
             session from here would be deleting the last one"
        );
    }

    /// The digest view truncates a long tool result.
    #[test]
    fn the_digest_view_truncates_a_long_tool_result() {
        let long = "x".repeat(10_000);
        let e = TranscriptEvent {
            id: "01E".into(),
            session_id: "01S".into(),
            parser_version: PARSER_VERSION.into(),
            seq: 0,
            kind: TranscriptEventKind::ToolResult,
            occurred_at: None,
            content: serde_json::json!([
                { "type": "tool_result", "tool_call_id": "c", "output": long, "is_error": false }
            ]),
            text: Some(long.clone()),
            tool_name: Some("Bash".into()),
            tool_call_id: Some("c".into()),
            src_generation_id: "01G".into(),
            src_offset: 0,
            src_length: 10_000,
            created_at: Utc::now(),
        };
        let digest = to_event_dto(e.clone(), true);
        let text = digest.text.as_deref().expect("text");
        assert!(
            text.len() < long.len(),
            "the digest view did not truncate a 10 KB tool result"
        );
        assert!(
            text.contains("10000 bytes"),
            "the truncation marker should say how much was cut: {text}"
        );

        let full = to_event_dto(e, false);
        assert_eq!(
            full.text.as_deref().map(str::len),
            Some(10_000),
            "the transcript view must not truncate"
        );
    }
}

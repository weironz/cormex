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
/// Fixture version for tests that seed Claude projections.
#[cfg(test)]
const PARSER_VERSION: &str = "claude_code@1";

pub(crate) fn parser_version_for_agent(agent: &str) -> Option<&'static str> {
    use cormex_proto::capture::FileRole;
    [FileRole::Transcript, FileRole::Row, FileRole::Subagent]
        .into_iter()
        .find_map(|role| cormex_transcript::parser_for(agent, role).map(|p| p.version()))
}

#[cfg(test)]
#[test]
fn archive_details_use_each_agents_parser_version() {
    assert_eq!(
        parser_version_for_agent("claude_code"),
        Some("claude_code@1")
    );
    assert_eq!(parser_version_for_agent("codex"), Some("codex@1"));
    assert_eq!(parser_version_for_agent("opencode"), Some("opencode@2"));
    assert_eq!(parser_version_for_agent("unrecognized"), None);
}

/// `GET /archive/sessions`.
#[derive(Debug, Deserialize)]
pub struct ListQuery {
    pub agent: Option<String>,
    pub project: Option<String>,
    #[serde(default)]
    pub unassigned: bool,
    pub q: Option<String>,
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
        "promoted_episodes_redacted": report.promoted_episodes_redacted,
        "facts_redacted": report.facts_redacted,
        "summaries_deleted": report.summaries_deleted,
    })))
}

/// `POST /episodes/{id}/redact` — erase one episode and everything derived
/// from it.
///
/// # The cascade this runs
///
/// `docs/memory.md` §十一. The episode's content, every fact extracted from it,
/// the blob transcripts of its attachments, the summaries derived from it, and
/// the archive's own copies. The tombstone is written last.
///
/// # Two gates, same shape as the archive purge
///
/// `raw:read` — a credential that cannot read the original bytes must not be
/// able to destroy them. And `?confirm=<id>`, because a bare POST that erases a
/// conversation is one mis-wired client away from being called by a retry.
///
/// # Errors
/// Missing `raw:read` (403), a missing or wrong `confirm` (400), no such episode
/// (404), or the cascade failed.
pub async fn redact_episode(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    redact_common(
        &st,
        &principal,
        &id,
        q.confirm.as_deref(),
        RedactionKind::Episode,
    )
    .await
}

/// `POST /blobs/{hash}/redact` — erase one blob's transcripts, and its object
/// when the mode is purge.
///
/// # Errors
/// Missing `raw:read` (403), a missing or wrong `confirm` (400), or the cascade
/// failed.
pub async fn redact_blob(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(hash): Path<String>,
    Query(q): Query<ConfirmQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    redact_common(
        &st,
        &principal,
        &hash,
        q.confirm.as_deref(),
        RedactionKind::Blob,
    )
    .await
}

/// Which kind of target a redact is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedactionKind {
    Episode,
    Blob,
}

async fn redact_common(
    st: &AppState,
    principal: &Principal,
    target_id: &str,
    confirm: Option<&str>,
    kind: RedactionKind,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !principal.has_scope(crate::device_keys::Scope::RawRead) {
        return Err(ApiError::forbidden(
            "redacting requires raw:read: a credential that cannot read the original bytes must \
             not be able to destroy them",
        ));
    }
    match confirm {
        Some(c) if c == target_id => {}
        Some(_) => {
            return Err(ApiError::bad_request(
                "the confirmation does not match the id being redacted",
            ));
        }
        None => {
            return Err(ApiError::bad_request(format!(
                "redacting erases the content and everything derived from it, and cannot be \
                 undone. Repeat the id as ?confirm={target_id} to proceed"
            )));
        }
    }

    let tenant = principal.tenant(st).await?;
    let report = match kind {
        RedactionKind::Episode => {
            crate::redact::redact_episode(
                st,
                &tenant,
                target_id,
                cormex_store::RedactionMode::Redact,
                "requested through the API",
            )
            .await
        }
        RedactionKind::Blob => {
            crate::redact::redact_blob(
                st,
                &tenant,
                target_id,
                cormex_store::RedactionMode::Redact,
                "requested through the API",
            )
            .await
        }
    }
    .map_err(|e| ApiError::internal(format!("the redaction failed: {e}")))?;

    Ok(Json(serde_json::json!({
        "redacted": true,
        "episodes_cleared": report.episodes_cleared,
        "facts_cleared": report.facts_cleared,
        "facts_invalidated": report.facts_invalidated,
        "blob_transcripts_cleared": report.blob_transcripts_cleared,
        "summaries_deleted": report.summaries_deleted,
        "sessions_rewound": report.sessions_rewound,
        "transcript_events_cleared": report.transcript_events_cleared,
        "archive_chunks_deleted": report.archive_chunks_deleted,
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
        .archive_sessions_filtered_at(
            q.agent.as_deref(),
            q.project.as_deref(),
            since,
            until,
            at,
            as_of,
            after.as_ref().map(|(t, id)| (t, id.as_str())),
            limit,
            q.q.as_deref().map(str::trim).filter(|q| !q.is_empty()),
            q.unassigned,
        )
        .await
        .map_err(store_err)?;

    let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
    let sole = store
        .sessions_that_are_sole_copies(&ids)
        .await
        .map_err(store_err)?;
    let sessions = rows
        .iter()
        .map(|r| dto_from_row(r, sole.contains(&r.id)))
        .collect();
    let next_cursor = if rows.len() < limit.clamp(1, 200) as usize {
        None
    } else {
        rows.last()
            .and_then(|r| r.last_at.map(|t| format!("{}|{}", t.to_rfc3339(), r.id)))
    };

    Ok(Json(ArchiveSessionsResponse {
        sessions,
        next_cursor,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ProjectsQuery {
    pub agent: Option<String>,
    pub q: Option<String>,
}

pub async fn list_projects(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Query(q): Query<ProjectsQuery>,
) -> Result<Json<cormex_proto::archive::ArchiveProjectsResponse>, ApiError> {
    let store = archive_store(&st, &principal).await?;
    let rows = store
        .archive_projects(
            q.agent.as_deref(),
            q.q.as_deref().map(str::trim).filter(|q| !q.is_empty()),
        )
        .await
        .map_err(store_err)?;
    Ok(Json(cormex_proto::archive::ArchiveProjectsResponse {
        projects: rows
            .into_iter()
            .map(|r| cormex_proto::archive::ArchiveProjectDto {
                key: r.key,
                name: r.name,
                chat_count: r.chat_count,
            })
            .collect(),
    }))
}

/// `GET /archive/sources` — the sources that have sessions, with their maker
/// and kind, for the archive's source filter.
///
/// # Errors
/// The caller may not read sessions, or the query failed.
pub async fn list_sources(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Result<Json<cormex_proto::archive::ArchiveSourcesResponse>, ApiError> {
    let store = archive_store(&st, &principal).await?;
    let counts = store.archive_source_counts().await.map_err(store_err)?;
    let catalog = crate::capture::effective_catalog(&st)?;
    let mut sources: Vec<cormex_proto::archive::ArchiveSourceDto> = counts
        .into_iter()
        .map(|(agent, chat_count)| {
            let entry = catalog.agents.iter().find(|a| a.id == agent);
            cormex_proto::archive::ArchiveSourceDto {
                name: entry.map_or_else(|| agent.clone(), |a| a.name.clone()),
                provider: entry.and_then(|a| a.provider.clone()),
                kind: entry.and_then(|a| a.kind),
                agent,
                chat_count,
            }
        })
        .collect();
    // Grouped by maker in the filter, so sorted that way; an agent the catalog
    // no longer knows sorts last rather than first
    sources.sort_by(|a, b| {
        (a.provider.is_none(), &a.provider, &a.name).cmp(&(
            b.provider.is_none(),
            &b.provider,
            &b.name,
        ))
    });
    Ok(Json(cormex_proto::archive::ArchiveSourcesResponse {
        sources,
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
    let parser_version = parser_version_for_agent(&row.agent);

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

    let projection = if let Some(version) = parser_version {
        store
            .latest_projection(&id, version)
            .await
            .map_err(store_err)?
    } else {
        None
    };

    Ok(Json(ArchiveSessionDetail {
        session,
        files,
        parser_version: parser_version.map(str::to_owned),
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
    let row = store
        .archive_session(&id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::not_found("no such session, or it has no events yet"))?;
    let parser_version = parser_version_for_agent(&row.agent)
        .ok_or_else(|| ApiError::not_found("no parser available for this agent"))?;
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
            parser_version,
            after_seq.unwrap_or(-1),
            parse_time(q.at.as_deref())?,
            parse_time(q.as_of.as_deref())?,
            limit,
        )
        .await
        .map_err(store_err)?;

    let next_cursor = events.last().map(|e| e.seq.to_string());
    let mut events: Vec<TranscriptEventDto> = events
        .into_iter()
        .map(|e| to_event_dto(e, digest))
        .collect();
    if row.agent == "codex" {
        restore_codex_message_metadata(&st, &store, &mut events).await;
    }

    Ok(Json(ArchiveEventsResponse {
        events,
        next_cursor,
    }))
}

/// Recover only non-sensitive classification fields from archived source spans.
/// Group adjacent records into bounded reads so a page does not fetch each
/// object separately. A missing source leaves the previous unknown behavior.
async fn restore_codex_message_metadata(
    st: &AppState,
    store: &Store,
    events: &mut [TranscriptEventDto],
) -> bool {
    const MAX_RECORD: i64 = 1024 * 1024;
    const MAX_WINDOW: i64 = 8 * 1024 * 1024;
    let mut indices: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, e)| {
            matches!(
                e.kind,
                TranscriptKind::UserMessage | TranscriptKind::AgentMessage
            ) && e.source.offset >= 0
                && e.source.length > 0
                && e.source.length <= MAX_RECORD
                && e.source.offset.checked_add(e.source.length).is_some()
        })
        .map(|(i, _)| i)
        .collect();
    indices.sort_by(|&a, &b| {
        events[a]
            .source
            .generation_id
            .cmp(&events[b].source.generation_id)
            .then(events[a].source.offset.cmp(&events[b].source.offset))
    });
    let mut pos = 0;
    let mut readable = true;
    while pos < indices.len() {
        let first = indices[pos];
        let generation = events[first].source.generation_id.clone();
        let start = events[first].source.offset;
        let mut end = start + events[first].source.length;
        let mut stop = pos + 1;
        while stop < indices.len() {
            let source = &events[indices[stop]].source;
            let next_end = source.offset + source.length;
            if source.generation_id != generation || next_end - start > MAX_WINDOW {
                break;
            }
            end = end.max(next_end);
            stop += 1;
        }
        if let Ok(bytes) = read_range(st, store, &generation, start, end - start).await {
            for &index in &indices[pos..stop] {
                let source = &events[index].source;
                let from = (source.offset - start) as usize;
                let to = from + source.length as usize;
                if let Some(record) = bytes.get(from..to) {
                    let (role, phase) = codex_message_metadata(record);
                    events[index].role = role;
                    events[index].phase = phase;
                } else {
                    readable = false;
                }
            }
        } else {
            readable = false;
        }
        pos = stop;
    }
    readable
}

/// Keep this predicate aligned with the client's default conversation view.
fn is_conversation_message(event: &TranscriptEventDto, agent: &str) -> bool {
    if !matches!(
        event.kind,
        TranscriptKind::UserMessage | TranscriptKind::AgentMessage
    ) {
        return false;
    }
    if matches!(event.role.as_deref(), Some("developer" | "system")) {
        return false;
    }
    if event.role.as_deref() == Some("assistant")
        && matches!(event.phase.as_deref(), Some("commentary" | "analysis"))
    {
        return false;
    }
    let text = event
        .text
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| {
            event
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
    let text = text.trim();
    if text.is_empty() {
        return false;
    }
    const PREFIXES: &[&str] = &[
        "# AGENTS.md instructions",
        "<environment_context>",
        "<recommended_plugins>",
        "<permissions instructions>",
        "<external_codex_apps_open_page>",
        "<user_instructions>",
        "<subagent_notification>",
        "<app-context>",
        "<codex_apps_open_page_instructions>",
        "<codex_apps_client_time_context>",
        "<skills_instructions>",
        "<tools>",
        "You are Codex, an agent based on ",
    ];
    if PREFIXES.iter().any(|p| text.starts_with(p)) {
        return false;
    }
    !(agent == "codex" && event.role.is_none() && text == "Not a git repo, skipping")
}

/// Prepare a whole short conversation using the same classification as the UI.
/// No partial/truncated conversations are silently submitted to a model.
pub(crate) async fn memory_trial_events(
    st: &AppState,
    store: &Store,
    id: &str,
) -> Result<(cormex_store::ArchiveSessionRow, Vec<TranscriptEvent>, i64), ApiError> {
    let row = store
        .archive_session(id)
        .await
        .map_err(store_err)?
        .ok_or_else(|| ApiError::bad_request("聊天不存在或尚未解析"))?;
    let version = parser_version_for_agent(&row.agent)
        .ok_or_else(|| ApiError::bad_request("暂不支持该来源的小样本提取"))?;
    let events = store
        .transcript_page_at(id, version, -1, None, None, 201)
        .await
        .map_err(store_err)?;
    if events.len() > 200 {
        return Err(ApiError::bad_request(
            "请选择更短的聊天：小样本每个聊天最多 200 个事件",
        ));
    }
    let through = events.last().map_or(-1, |e| e.seq);
    let selected = memory_conversation_events(st, store, &row.agent, events).await?;
    Ok((row, selected, through))
}

pub(crate) async fn memory_conversation_events(
    st: &AppState,
    store: &Store,
    agent: &str,
    events: Vec<TranscriptEvent>,
) -> Result<Vec<TranscriptEvent>, ApiError> {
    let mut display = events
        .iter()
        .cloned()
        .map(|e| to_event_dto(e, false))
        .collect::<Vec<_>>();
    if agent == "codex" && !restore_codex_message_metadata(st, store, &mut display).await {
        return Err(ApiError::bad_request("聊天来源暂时不可读，未提交给模型"));
    }
    let selected = events
        .into_iter()
        .zip(display)
        .filter_map(|(mut e, d)| {
            if !is_conversation_message(&d, agent) {
                return None;
            }
            e.kind = match d.role.as_deref() {
                Some("user") => cormex_store::TranscriptEventKind::UserMessage,
                Some("assistant") => cormex_store::TranscriptEventKind::AgentMessage,
                _ if d.kind == TranscriptKind::UserMessage => {
                    cormex_store::TranscriptEventKind::UserMessage
                }
                _ => cormex_store::TranscriptEventKind::AgentMessage,
            };
            e.text = d.text.or(e.text);
            Some(e)
        })
        .collect();
    Ok(selected)
}

pub(crate) async fn update_message_counts(st: &AppState, store: &Store) -> Result<(), ApiError> {
    let mut versions = serde_json::Map::new();
    for agent in &cormex_proto::catalog::Catalog::builtin().agents {
        if let Some(version) = parser_version_for_agent(&agent.id) {
            versions.insert(agent.id.clone(), version.into());
        }
    }
    for (session, agent, version, projection, event_count) in store
        .message_counts_pending(&versions.into(), 20)
        .await
        .map_err(store_err)?
    {
        let mut after = -1;
        let mut count = 0;
        let mut readable = true;
        loop {
            let candidates = store
                .conversation_candidates(&session, &version, after)
                .await
                .map_err(store_err)?;
            if candidates.is_empty() {
                break;
            }
            after = candidates.last().expect("nonempty").seq;
            let mut events = candidates
                .into_iter()
                .map(|e| to_event_dto(e, false))
                .collect::<Vec<_>>();
            if agent == "codex" && !restore_codex_message_metadata(st, store, &mut events).await {
                readable = false;
                break;
            }
            count += events
                .iter()
                .filter(|e| is_conversation_message(e, &agent))
                .count() as i64;
        }
        store
            .archive_txn(async |t| {
                t.cache_message_count(
                    &session,
                    &projection,
                    &version,
                    event_count,
                    readable.then_some(count),
                )
                .await
            })
            .await
            .map_err(store_err)?;
    }
    Ok(())
}

fn codex_message_metadata(record: &[u8]) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(record) else {
        return (None, None);
    };
    if value.get("type").and_then(|v| v.as_str()) != Some("response_item")
        || value.pointer("/payload/type").and_then(|v| v.as_str()) != Some("message")
    {
        return (None, None);
    }
    let role = value
        .pointer("/payload/role")
        .and_then(|v| v.as_str())
        .filter(|v| matches!(*v, "user" | "assistant" | "developer" | "system"))
        .map(str::to_owned);
    let phase = value
        .pointer("/payload/phase")
        .and_then(|v| v.as_str())
        .filter(|v| matches!(*v, "commentary" | "final_answer" | "analysis"))
        .map(str::to_owned);
    (role, phase)
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
    let sole_copy = store
        .session_is_sole_copy(&row.id)
        .await
        .map_err(store_err)?;
    Ok(dto_from_row(row, sole_copy))
}

fn dto_from_row(row: &cormex_store::ArchiveSessionRow, sole_copy: bool) -> ArchiveSessionDto {
    ArchiveSessionDto {
        id: row.id.clone(),
        agent: row.agent.clone(),
        title: row.original_title.clone().or_else(|| {
            row.title_source
                .as_deref()
                .map(|t| t.chars().take(TITLE_CHARS).collect::<String>())
        }),
        project_key: row.project_key.clone(),
        project_name: row.project_name.clone(),
        pinned_position: row.pinned_position,
        source_archived: row.source_archived,
        devices: row.devices.clone().unwrap_or_default(),
        first_at: row.first_at.map(|t| t.to_rfc3339()),
        last_at: row.last_at.map(|t| t.to_rfc3339()),
        event_count: row.event_count,
        message_count: row.message_count,
        summary: None,
        sole_copy,
    }
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
        phase: None,
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

    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn project_facets_and_scoped_pages_are_complete_and_disjoint() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap();
        for i in 0..65 {
            let (id, _, _) = seed(&st).await;
            let project = if i < 35 {
                Some("a")
            } else if i < 63 {
                Some("b")
            } else {
                None
            };
            let title = if i == 34 {
                "needle".to_owned()
            } else {
                format!("chat-{i}")
            };
            sqlx::query("INSERT INTO capture_session_ui_metadata(session_id,device_id,title,project_id,project_name) VALUES($1,$2,$3,$4,$4)")
                .bind(&id).bind(Id::new().to_string()).bind(title).bind(project).execute(store.pool()).await.unwrap();
        }
        // Equal timestamps exercise the ID half of the keyset cursor.
        sqlx::query("UPDATE transcript_events SET occurred_at='2026-10-01T00:00:00Z'")
            .execute(store.pool())
            .await
            .unwrap();
        let facets = store
            .archive_projects(Some("claude_code"), None)
            .await
            .unwrap();
        assert_eq!(facets.len(), 3);
        assert_eq!(facets.iter().map(|p| p.chat_count).sum::<i64>(), 65);
        assert_eq!(
            facets.iter().find(|p| p.key.is_none()).unwrap().chat_count,
            2
        );
        assert!(
            store
                .archive_projects(Some("codex"), None)
                .await
                .unwrap()
                .is_empty()
        );
        let first = store
            .archive_sessions_filtered_at(
                Some("claude_code"),
                Some("a"),
                None,
                None,
                None,
                None,
                None,
                30,
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!(first.len(), 30);
        let last = first.last().unwrap();
        let stamp = last.last_at.unwrap();
        let second = store
            .archive_sessions_filtered_at(
                Some("claude_code"),
                Some("a"),
                None,
                None,
                None,
                None,
                Some((&stamp, &last.id)),
                30,
                None,
                false,
            )
            .await
            .unwrap();
        assert_eq!(second.len(), 5);
        let unique = first
            .iter()
            .chain(&second)
            .map(|r| r.id.as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), 35);
        assert!(
            first
                .iter()
                .chain(&second)
                .all(|r| r.project_key.as_deref() == Some("a"))
        );
        let unassigned = store
            .archive_sessions_filtered_at(None, None, None, None, None, None, None, 30, None, true)
            .await
            .unwrap();
        assert_eq!(unassigned.len(), 2);
        assert!(unassigned.iter().all(|r| r.project_key.is_none()));
        let searched = store.archive_projects(None, Some("needle")).await.unwrap();
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].chat_count, 1);
        let page = store
            .archive_sessions_filtered_at(
                None,
                Some("a"),
                None,
                None,
                None,
                None,
                None,
                1,
                Some("needle"),
                false,
            )
            .await
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].original_title.as_deref(), Some("needle"));
        assert!(
            store
                .archive_projects(None, Some("%_"))
                .await
                .unwrap()
                .is_empty()
        );
        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn file_project_key_is_the_fallback_and_explicit_ui_null_overrides_it() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap();
        let (session, _, _) = seed(&st).await;

        let facets = store
            .archive_projects(Some("claude_code"), None)
            .await
            .unwrap();
        assert_eq!(facets.len(), 1);
        assert_eq!(facets[0].key.as_deref(), Some("github.com/u/p"));
        assert_eq!(facets[0].chat_count, 1);
        let sessions = store
            .archive_sessions(
                Some("claude_code"),
                Some("github.com/u/p"),
                None,
                None,
                None,
                20,
            )
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, session);

        sqlx::query(
            "INSERT INTO capture_session_ui_metadata(session_id,device_id,project_id)
             VALUES($1,$2,NULL)",
        )
        .bind(&session)
        .bind(Id::new().to_string())
        .execute(store.pool())
        .await
        .unwrap();

        let after_ui_null = store
            .archive_projects(Some("claude_code"), None)
            .await
            .unwrap();
        assert_eq!(after_ui_null.len(), 1);
        assert!(
            after_ui_null[0].key.is_none(),
            "an explicit UI project_id = NULL is a user decision to ungroup; falling back to the \
             captured project would make the session impossible to remove from a group"
        );
        assert!(
            store
                .archive_sessions(
                    Some("claude_code"),
                    Some("github.com/u/p"),
                    None,
                    None,
                    None,
                    20,
                )
                .await
                .unwrap()
                .is_empty(),
            "the project filter must use the same explicit-NULL rule as the facet list"
        );
        db.drop().await;
    }

    /// A renamed project directory leaves the old file behind; the session is
    /// listed where it lives now, and only there.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_session_follows_its_newest_file_into_a_renamed_project() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap();
        let (session, _, _) = seed(&st).await;
        let native: String =
            sqlx::query_scalar("SELECT native_session_id FROM capture_sessions WHERE id = $1")
                .bind(&session)
                .fetch_one(store.pool())
                .await
                .unwrap();

        // The host now keeps the same conversation under the renamed directory.
        // "github.com/u/p" sorts after "github.com/u/a-renamed", so a rule of
        // "smallest key wins" would keep the session in the old project
        let moved = NewCaptureFile {
            id: Id::new(),
            native_session_id: native,
            parent_native_session_id: None,
            device_id: Id::new().to_string(),
            agent: "claude_code".into(),
            logical_path: "projects/-a-renamed/s1.jsonl".into(),
            role: FileRole::Transcript,
            framing: Framing::Append,
            cwd: None,
            project_key: Some("github.com/u/a-renamed".into()),
        };
        let registered = store
            .archive_txn(async |t| t.register_capture_file(&moved).await)
            .await
            .unwrap();
        assert_eq!(
            registered.session.id, session,
            "same native id, same session"
        );

        let facets = store
            .archive_projects(Some("claude_code"), None)
            .await
            .unwrap();
        let keys: Vec<_> = facets.iter().map(|p| p.key.as_deref()).collect();
        assert_eq!(
            keys,
            vec![Some("github.com/u/a-renamed")],
            "the facet list must count the session once, under the project its newest file is in"
        );
        let listed = |key: &'static str| {
            let store = store.clone();
            async move {
                store
                    .archive_sessions(Some("claude_code"), Some(key), None, None, None, 20)
                    .await
                    .unwrap()
                    .len()
            }
        };
        assert_eq!(listed("github.com/u/a-renamed").await, 1);
        assert_eq!(
            listed("github.com/u/p").await,
            0,
            "expanding the old project must not show a chat the facet list counts elsewhere"
        );
        assert_eq!(
            store
                .archive_session(&session)
                .await
                .unwrap()
                .unwrap()
                .project_key
                .as_deref(),
            Some("github.com/u/a-renamed"),
            "the detail view agrees with the list"
        );
        assert_eq!(
            store
                .session_project_key(&session)
                .await
                .unwrap()
                .as_deref(),
            Some("github.com/u/a-renamed"),
            "the indexer's exclusion check follows the same rule"
        );
        db.drop().await;
    }

    /// The source filter lists only sources with sessions, named and placed by
    /// the catalog.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_source_list_has_only_sources_with_sessions() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let _ = seed(&st).await;
        let user = st
            .create_account("reader", "correct horse battery")
            .await
            .expect("create the account");
        let token = st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));
        let app = crate::routes::router(st.clone());

        let (status, body) = crate::test_support::call(
            &app,
            axum::http::Method::GET,
            "/archive/sources",
            Some(&token),
            None,
        )
        .await;
        db.drop().await;
        assert_eq!(status, axum::http::StatusCode::OK, "{body}");
        assert_eq!(
            body,
            serde_json::json!({"sources": [{
                "agent": "claude_code",
                "name": "Claude Code",
                "provider": "Anthropic",
                "kind": "code",
                "chat_count": 1,
            }]}),
            "one seeded Claude Code session, and none of the twenty other catalogued agents: an \
             empty source is noise in the filter"
        );
    }

    #[test]
    fn message_statistics_follow_the_conversation_view() {
        let mut event = TranscriptEventDto {
            id: "one".into(),
            seq: 0,
            kind: TranscriptKind::AgentMessage,
            occurred_at: None,
            role: Some("assistant".into()),
            phase: Some("final_answer".into()),
            content: vec![],
            text: Some("done".into()),
            tool_name: None,
            tool_call_id: None,
            source: cormex_proto::transcript::SourceSpan {
                generation_id: "g".into(),
                offset: 0,
                length: 0,
            },
        };
        assert!(is_conversation_message(&event, "codex"));
        event.phase = Some("commentary".into());
        assert!(!is_conversation_message(&event, "codex"));
        event.phase = None;
        assert!(is_conversation_message(&event, "codex"));
        event.role = Some("developer".into());
        assert!(!is_conversation_message(&event, "codex"));
        event.role = None;
        event.kind = TranscriptKind::UserMessage;
        event.text = Some("<environment_context>internal</environment_context>".into());
        assert!(!is_conversation_message(&event, "codex"));
        event.text = Some("question".into());
        assert!(is_conversation_message(&event, "codex"));
        event.kind = TranscriptKind::ToolResult;
        assert!(!is_conversation_message(&event, "codex"));
    }

    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn message_count_cache_invalidates_on_projection_change() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let (id, _, _) = seed(&st).await;
        let store = st.public_store().unwrap();
        assert!(
            store
                .archive_session(&id)
                .await
                .unwrap()
                .unwrap()
                .message_count
                .is_none()
        );
        update_message_counts(&st, &store).await.unwrap();
        assert_eq!(
            store
                .archive_session(&id)
                .await
                .unwrap()
                .unwrap()
                .message_count,
            Some(1)
        );
        assert_eq!(
            store
                .archive_sessions(None, None, None, None, None, 50)
                .await
                .unwrap()[0]
                .message_count,
            Some(1)
        );
        let previous = store
            .latest_projection(&id, PARSER_VERSION)
            .await
            .unwrap()
            .unwrap();
        store
            .archive_txn(async |t| {
                t.insert_transcript_events(
                    &[],
                    &NewProjection {
                        id: Id::new(),
                        session_id: id.parse().unwrap(),
                        parser_version: PARSER_VERSION.into(),
                        through: previous.through.clone(),
                        next_seq: previous.next_seq + 10,
                        status: ProjectionStatus::Ok,
                        error: None,
                    },
                )
                .await
            })
            .await
            .unwrap();
        assert!(
            store
                .archive_session(&id)
                .await
                .unwrap()
                .unwrap()
                .message_count
                .is_none()
        );
        update_message_counts(&st, &store).await.unwrap();
        assert_eq!(
            store
                .archive_session(&id)
                .await
                .unwrap()
                .unwrap()
                .message_count,
            Some(1)
        );
        db.drop().await;
    }

    #[test]
    fn codex_metadata_only_extracts_recognized_message_roles_and_phases() {
        let developer = br#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"secret"}]}}"#;
        assert_eq!(
            codex_message_metadata(developer),
            (Some("developer".into()), None)
        );
        let answer = br#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer"}}"#;
        assert_eq!(
            codex_message_metadata(answer),
            (Some("assistant".into()), Some("final_answer".into()))
        );
        let commentary = br#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"commentary"}}"#;
        assert_eq!(
            codex_message_metadata(commentary).1.as_deref(),
            Some("commentary")
        );
        assert_eq!(
            codex_message_metadata(
                br#"{"type":"event_msg","payload":{"type":"message","role":"user"}}"#
            ),
            (None, None)
        );
        assert_eq!(codex_message_metadata(b"broken"), (None, None));
        assert_eq!(codex_message_metadata(br#"{"type":"response_item","payload":{"type":"message","role":"unknown","phase":"unknown"}}"#), (None, None));
    }

    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn archived_source_is_provenance_on_both_list_and_detail() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let (id, _, _) = seed(&st).await;
        let store = st.public_store().unwrap();
        assert!(
            !store
                .archive_session(&id)
                .await
                .unwrap()
                .unwrap()
                .source_archived
        );
        sqlx::query("UPDATE capture_sessions SET agent='codex' WHERE id=$1")
            .bind(&id)
            .execute(store.pool())
            .await
            .unwrap();
        sqlx::query("UPDATE capture_files SET logical_path='archived_sessions/rollout-test.jsonl' WHERE session_id=$1").bind(&id).execute(store.pool()).await.unwrap();
        let detail = store.archive_session(&id).await.unwrap().unwrap();
        assert!(detail.source_archived);
        assert!(to_dto(&store, &detail).await.unwrap().source_archived);
        let list = store
            .archive_sessions(Some("codex"), None, None, None, None, 50)
            .await
            .unwrap();
        assert_eq!(list.len(), 1);
        assert!(list[0].source_archived);
        sqlx::query("UPDATE capture_files SET logical_path='archivedXsessions/rollout-test.jsonl' WHERE session_id=$1").bind(&id).execute(store.pool()).await.unwrap();
        assert!(
            !store
                .archive_session(&id)
                .await
                .unwrap()
                .unwrap()
                .source_archived
        );
        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn codex_metadata_recovers_from_archived_source_spans() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap();
        let first = br#"{"type":"response_item","payload":{"type":"message","role":"developer"}}"#;
        let second = br#"{"type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer"}}"#;
        let mut body = first.to_vec();
        body.push(b'\n');
        body.extend_from_slice(second);
        body.push(b'\n');
        let registered = store
            .archive_txn(async |t| {
                t.register_capture_file(&NewCaptureFile {
                    id: Id::new(),
                    native_session_id: format!("native-{}", Id::new()),
                    parent_native_session_id: None,
                    device_id: Id::new().to_string(),
                    agent: "codex".into(),
                    logical_path: "session.jsonl".into(),
                    role: FileRole::Transcript,
                    framing: Framing::Append,
                    cwd: None,
                    project_key: None,
                })
                .await
            })
            .await
            .unwrap();
        use sha2::Digest as _;
        let sha = format!("{:x}", sha2::Sha256::digest(&body));
        let key = format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]);
        st.put_capture_object(
            &key,
            axum::body::Bytes::from(zstd::encode_all(body.as_slice(), 3).unwrap()),
        )
        .await
        .unwrap();
        store
            .archive_txn(async |t| {
                t.append_chunk(&NewCaptureChunk {
                    id: Id::new(),
                    generation_id: registered.generation.id.parse().unwrap(),
                    byte_offset: 0,
                    byte_length: body.len() as i64,
                    content_sha256: sha.clone(),
                    storage_key: key.clone(),
                    encoding: ChunkEncoding::Zstd,
                    observed_at: Utc::now(),
                })
                .await
            })
            .await
            .unwrap();
        let event = |offset, length| TranscriptEventDto {
            id: Id::new().to_string(),
            seq: 0,
            kind: TranscriptKind::AgentMessage,
            occurred_at: None,
            role: None,
            phase: None,
            content: vec![],
            text: None,
            tool_name: None,
            tool_call_id: None,
            source: cormex_proto::transcript::SourceSpan {
                generation_id: registered.generation.id.clone(),
                offset,
                length,
            },
        };
        let mut events = vec![
            event(0, first.len() as i64),
            event((first.len() + 1) as i64, second.len() as i64),
        ];
        restore_codex_message_metadata(&st, &store, &mut events).await;
        assert_eq!(events[0].role.as_deref(), Some("developer"));
        assert_eq!(events[1].role.as_deref(), Some("assistant"));
        assert_eq!(events[1].phase.as_deref(), Some("final_answer"));
        db.drop().await;
    }

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
        assert!(
            store
                .sessions_that_are_sole_copies(std::slice::from_ref(&session_id))
                .await
                .expect("batch query")
                .is_empty(),
            "the list page's batched answer must agree with the per-session one"
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
        let batched = store
            .sessions_that_are_sole_copies(&[session_id.clone(), "0".repeat(26)])
            .await
            .expect("batch query");
        db.drop().await;
        assert_eq!(
            batched.into_iter().collect::<Vec<_>>(),
            vec![session_id.clone()],
            "the list page marks a session as the last copy through the batched query; it must \
             flag exactly the sessions the detail page flags, and nothing for an unknown id"
        );
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

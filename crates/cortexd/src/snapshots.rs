//! The **storage half** of sandbox workspace snapshots.
//!
//! # Why one feature is split down the middle
//!
//! Taking a snapshot needs two things: exporting a tar out of a docker volume
//! (**only `cortex-agentd` has docker**), and persisting the bytes plus an index
//! row (**only this side has object storage and the database**). Putting the
//! whole thing on either side would mean giving that side something it is
//! designed not to have.
//!
//! So the cut runs down the middle: agentd produces the tar, hands the bytes
//! over through the existing `POST /blobs`, then calls `POST /sandbox-snapshots`
//! here to record a row. Restore runs the other way — ask
//! `GET /sandbox-snapshots` for a `blob_hash`, fetch the bytes with
//! `GET /blobs/{hash}`, and unpack them into the volume itself.
//!
//! The bytes go through `/blobs` rather than a new upload path, because that
//! path already has chunking, deduplication, tenant prefixes and direct upload
//! — and a snapshot is just a pile of bytes with nothing special about it.
//!
//! # Where authorization for the restore path lives
//!
//! In [`list`]: its query runs inside **the caller's own tenant schema** and
//! filters by scope. agentd can only pick an id out of that list — it cannot
//! pick someone else's, nor one from another of its own projects. Letting the
//! caller hand over a `blob_hash` directly would have no such layer: object
//! storage is content-addressed, and a hash carries no ownership of its own.

use axum::extract::{Query, State};
use axum::{Json, http::HeaderMap};
use cortex_core::{CortexError, Id, Result};

use crate::routes::ApiError;
use crate::state::AppState;

/// How many rows a snapshot listing returns at most. Enough for one screenful
/// in the UI, and enough for "restore from the most recent one".
const LIST_LIMIT: i64 = 50;

/// One snapshot record.
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct SnapshotRow {
    pub id: String,
    pub blob_hash: String,
    pub size_bytes: i64,
    pub taken_at: chrono::DateTime<chrono::Utc>,
}

/// `?scope=` — snapshots are partitioned by **scope**, not by owner.
///
/// Mixed together, a user inside project A would see snapshots from three
/// projects, and clicking "restore" would unpack another project's tar into this
/// workspace.
#[derive(serde::Deserialize)]
pub struct ScopeQuery {
    scope: String,
}

/// Record a new snapshot row. The caller uploads the bytes through
/// `POST /blobs` **first**.
#[derive(serde::Deserialize)]
pub struct RecordRequest {
    pub scope: String,
    pub blob_hash: String,
    pub size_bytes: i64,
}

/// `GET /sandbox-snapshots?scope=` — snapshots for this scope, newest first.
///
/// # Errors
/// Tenant resolution or the query failed.
pub async fn list(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ScopeQuery>,
) -> std::result::Result<Json<Vec<SnapshotRow>>, ApiError> {
    let owner = crate::accounts::current_user(&st, &headers).await;
    Ok(Json(rows_for(&st, &owner, &q.scope).await?))
}

/// `POST /sandbox-snapshots` — record one row.
///
/// # The owner comes from the credential, never from the request body
///
/// If the caller could name the owner, any logged-in user could plant a record
/// under somebody else's name — and on restore that record would be treated as
/// that person's own.
///
/// # Errors
/// Tenant resolution or the write failed.
pub async fn record(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RecordRequest>,
) -> std::result::Result<Json<SnapshotRow>, ApiError> {
    let owner = crate::accounts::current_user(&st, &headers).await;
    let row = SnapshotRow {
        id: Id::new().to_string(),
        blob_hash: req.blob_hash,
        size_bytes: req.size_bytes,
        taken_at: chrono::Utc::now(),
    };
    let store = st.tenant_store_for_user(&owner).await?;
    sqlx::query(
        "INSERT INTO sandbox_snapshots (id, owner, scope, blob_hash, size_bytes, taken_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&row.id)
    .bind(&owner)
    .bind(&req.scope)
    .bind(&row.blob_hash)
    .bind(row.size_bytes)
    .bind(row.taken_at)
    .execute(store.pool())
    .await
    .map_err(|e| CortexError::Store(format!("could not persist the snapshot index: {e}")))?;
    Ok(Json(row))
}

/// The snapshot rows belonging to this scope.
async fn rows_for(st: &AppState, owner: &str, scope: &str) -> Result<Vec<SnapshotRow>> {
    let store = st.tenant_store_for_user(owner).await?;
    sqlx::query_as::<_, SnapshotRow>(
        "SELECT id, blob_hash, size_bytes, taken_at FROM sandbox_snapshots \
         WHERE scope = $1 ORDER BY taken_at DESC LIMIT $2",
    )
    .bind(scope)
    .bind(LIST_LIMIT)
    .fetch_all(store.pool())
    .await
    .map_err(|e| CortexError::Store(format!("could not list the snapshots: {e}")))
}

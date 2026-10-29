//! `POST /devices`, `GET /devices`, `DELETE /devices/{id}`.
//!
//! Design: docs/design/session-archive-architecture.md §7.3.
//!
//! # Who may mint a device key
//!
//! Only a **user credential** — an access token, or the legacy pre-shared
//! token. Not a device key and not a delegated token: a credential that can
//! mint another credential is a credential that can extend its own life and
//! widen its own scope, and the whole point of a scoped device key is that it
//! cannot.
//!
//! # Who may revoke one
//!
//! The user who owns it, or the device key itself (for its own id). The second
//! is not a convenience: `cormex logout` runs on the machine being retired, and
//! it holds only its own device key — requiring a user token there would mean
//! "you cannot remove this machine without logging in again on another one".

use axum::{Json, extract::Path, extract::State};

use crate::principal::Principal;
use crate::routes::ApiError;
use crate::state::AppState;
use cormex_proto::devices::{DeviceKeyIssued, DevicesResponse, NewDeviceRequest};

/// `POST /devices` — mint a key for one machine.
///
/// # Errors
/// The caller is a device key or a delegated token (403), the label or scopes
/// are invalid (400), or the write failed.
pub async fn create(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Json(req): Json<NewDeviceRequest>,
) -> Result<Json<DeviceKeyIssued>, ApiError> {
    // **A device key cannot mint another device key.** Same reasoning as the
    // delegated token's: a credential that can mint credentials can widen its
    // own scope, which is exactly what the scope list exists to prevent
    match &principal {
        Principal::Device(_) => {
            return Err(ApiError::forbidden(
                "a device key cannot mint another device key",
            ));
        }
        Principal::Delegated(_) => {
            return Err(ApiError::forbidden(
                "a delegated token cannot mint a device key; ask the user to run `cormex login`",
            ));
        }
        Principal::User { .. } | Principal::LegacyOwner => {}
    }

    let user_id = principal
        .user_id(&st)
        .await
        .ok_or_else(|| ApiError::internal("this deployment has no account for the legacy token"))?;

    let issued = st
        .device_keys()
        .issue(&user_id, &req.label, &req.scopes, req.platform.as_deref())
        .await?;
    Ok(Json(issued))
}

/// `GET /devices` — every key this user has, revoked ones included.
///
/// # Errors
/// The caller is a device key (403), or the query failed.
pub async fn list(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
) -> Result<Json<DevicesResponse>, ApiError> {
    // A device key listing every device would tell a compromised machine which
    // others exist. It can ask who *it* is via `/auth/me`; that is enough
    if principal.device().is_some() {
        return Err(ApiError::forbidden(
            "a device key cannot list the other devices",
        ));
    }
    let user_id = principal
        .user_id(&st)
        .await
        .ok_or_else(|| ApiError::internal("this deployment has no account for the legacy token"))?;
    Ok(Json(DevicesResponse {
        devices: st.device_keys().list(&user_id).await?,
    }))
}

/// `DELETE /devices/{id}` — revoke one key.
///
/// Idempotent, and it does not report "not found": revoking a key that does not
/// exist, belongs to somebody else, or was already revoked all succeed. The
/// same reasoning as `DELETE /delegated-tokens` — reporting them separately
/// turns "guess an id and see" into a probe.
///
/// # Errors
/// The caller is a delegated token (403), or the write failed.
pub async fn revoke(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, ApiError> {
    let user_id = match &principal {
        // A machine may revoke **itself**; the middleware's allow-list already
        // restricts a device key to its own id, so reaching here with any other
        // id means the allow-list let it through, which it must not
        Principal::Device(scope) => {
            if scope.device_id != id {
                return Err(ApiError::forbidden("a device key may only revoke itself"));
            }
            scope.user_id.clone()
        }
        Principal::User { user_id } => user_id.clone(),
        Principal::LegacyOwner => principal.user_id(&st).await.ok_or_else(|| {
            ApiError::internal("this deployment has no account for the legacy token")
        })?,
        Principal::Delegated(_) => {
            return Err(ApiError::forbidden(
                "a delegated token cannot revoke a device key",
            ));
        }
    };

    st.device_keys().revoke(&user_id, &id).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

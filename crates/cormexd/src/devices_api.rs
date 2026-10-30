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

// ══════════════════════════════════════════════════════════
//  Device-code login (P6-4)
// ══════════════════════════════════════════════════════════

/// `POST /devices/authorize` — start a device-code login. **Anonymous.**
///
/// # Why this one cannot require a credential
///
/// It exists for the machine that has none: a headless box where `cormex login`
/// has no way to show a password prompt anybody would trust. Requiring a
/// credential in order to obtain a credential is the same deadlock as
/// `/auth/login`.
///
/// What it hands back is inert: a short code and a long code, both worthless
/// until a **logged-in person** confirms the short one in a browser. The
/// confirmation is the protected step, and it is where the account is decided.
///
/// # Errors
/// The label or scopes are invalid (400), or the deployment has no account
/// system (501).
pub async fn authorize(
    State(st): State<AppState>,
    Json(req): Json<AuthorizeRequest>,
) -> Result<Json<crate::device_codes::AuthorizationStarted>, ApiError> {
    // The same validation `POST /devices` does, done here so a machine learns
    // about a bad scope **before** a person is asked to walk to a browser
    let label = req.label.trim();
    if label.is_empty() || label.chars().count() > 100 {
        return Err(ApiError::bad_request(
            "the label must be between 1 and 100 characters",
        ));
    }
    for s in &req.scopes {
        if crate::device_keys::Scope::from_wire(s).is_none() {
            return Err(ApiError::bad_request(format!(
                "unknown scope {s:?}; the valid ones are {}",
                crate::device_keys::Scope::all()
                    .iter()
                    .map(|s| s.as_wire())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(Json(st.device_codes().start(
        label.to_owned(),
        req.scopes.clone(),
        req.platform.clone(),
    )))
}

/// `POST /devices/token` — the machine polls. **Anonymous.**
///
/// # Why this one cannot require a credential
///
/// The credential it carries **is** the device code, in the body. That is the
/// whole point of the flow: the machine has no key yet — obtaining one is what
/// this call does.
///
/// A wrong device code gets `expired`, the same answer an expired one gets.
/// Distinguishing them would let a caller enumerate live codes.
///
/// # Errors
/// The body is malformed (400).
pub async fn token(
    State(st): State<AppState>,
    Json(req): Json<TokenRequest>,
) -> Result<Json<crate::device_codes::TokenPoll>, ApiError> {
    Ok(Json(st.device_codes().poll(&req.device_code)))
}

/// `POST /devices/confirm` — a logged-in person approves a code.
///
/// This is the **protected** half, and the one that decides whose account the
/// machine ends up on.
///
/// # Errors
/// The caller is a device key or a delegated token (403), the code is unknown
/// or expired (404), or minting the key failed.
pub async fn confirm(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<Principal>,
    Json(req): Json<ConfirmRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // **A user credential, not a device key.** The same argument as
    // `POST /devices`: a credential that can mint another credential can widen
    // its own reach, and here it would also mean a compromised machine could
    // approve its own next key
    match &principal {
        Principal::Device(_) => {
            return Err(ApiError::forbidden(
                "a device key cannot approve another device's login",
            ));
        }
        Principal::Delegated(_) => {
            return Err(ApiError::forbidden(
                "a delegated token cannot approve a device login; ask the user to do it in the                  browser",
            ));
        }
        Principal::User { .. } | Principal::LegacyOwner => {}
    }

    let user_id = principal
        .user_id(&st)
        .await
        .ok_or_else(|| ApiError::internal("this deployment has no account for the legacy token"))?;

    // Look the code up **before** approving, so the response can name the
    // machine the person is approving. Approving a code without being shown
    // which machine it belongs to is how a phishing flow gets its foot in
    let Some(pending) = st.device_codes().by_user_code(&req.user_code) else {
        return Err(ApiError::not_found(
            "no pending login with that code; it may have expired",
        ));
    };

    if !st.device_codes().approve(&req.user_code, &user_id) {
        return Err(ApiError::not_found(
            "no pending login with that code; it may have expired",
        ));
    }

    // Mint now rather than on the poll: the person has just confirmed, and a
    // key that is created here is a key whose creation time is the moment of
    // consent
    let issued = st
        .device_keys()
        .issue(
            &user_id,
            &pending.label,
            &pending.scopes,
            pending.platform.as_deref(),
        )
        .await?;
    st.device_codes()
        .set_issued(&pending.user_code, issued.clone());

    Ok(Json(serde_json::json!({
        "approved": true,
        "label": pending.label,
        "scopes": pending.scopes,
    })))
}

// The wire types live in `cormex-proto`, beside every other DTO: the CLI and
// the server must agree on the shape, and two copies of it is one copy that
// drifts.
use cormex_proto::devices::{AuthorizeRequest, ConfirmRequest, TokenRequest};

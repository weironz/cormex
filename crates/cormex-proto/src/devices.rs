//! The device-key wire protocol: `POST /devices`, `GET /devices`,
//! `DELETE /devices/{id}`.
//!
//! Design: docs/design/session-archive-architecture.md §7.3 (D9).
//!
//! # Why the plaintext key is a field on the response and nowhere else
//!
//! `DeviceKeyIssued.key` is the only place the plaintext ever appears: the
//! server stores the SHA-256, and there is no endpoint that can hand it back.
//! That is why the response type is separate from [`DeviceDto`] — a client that
//! deserialises a list into the wrong type gets a compile error rather than a
//! struct with a `key` field that is always `None` and a habit of being
//! unwrapped.

use serde::{Deserialize, Serialize};

/// `POST /devices`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewDeviceRequest {
    /// A human name for the machine ("laptop"), shown in the list and used when
    /// deciding what to revoke.
    pub label: String,
    /// The scopes to grant. Validated against the database's CHECK before the
    /// write, so an unknown one produces a message naming the scope rather than
    /// a constraint violation.
    pub scopes: Vec<String>,
    /// `linux-x86_64` / `macos-aarch64` / …, for diagnosis only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// `POST /devices` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceKeyIssued {
    pub id: String,
    /// **The plaintext. It appears exactly once, here.**
    pub key: String,
    pub scopes: Vec<String>,
}

/// One device in `GET /devices`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceDto {
    pub id: String,
    pub label: String,
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    pub created_at: String,
    /// `None` = never used since it was issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    /// `None` = still usable. A revoked key stays in the list: "was this
    /// machine ever set up" is a question people ask, and a deleted row cannot
    /// answer it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
}

/// `GET /devices` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicesResponse {
    pub devices: Vec<DeviceDto>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The issued key and the listed device are **different types**, and the
    /// list type has no `key` field.
    ///
    /// This is the whole reason the two exist separately. A client that
    /// deserialises `GET /devices` into `DeviceKeyIssued` would get a `key` of
    /// `None` (or a parse error), and the natural next step — "unwrap it" —
    /// would be a runtime failure on the one path that matters.
    #[test]
    fn the_list_type_cannot_carry_a_plaintext_key() {
        let dto = DeviceDto {
            id: "01ABC".into(),
            label: "laptop".into(),
            scopes: vec!["capture:write".into()],
            platform: None,
            created_at: "2026-10-29T00:00:00Z".into(),
            last_seen_at: None,
            revoked_at: None,
        };
        let json = serde_json::to_string(&dto).expect("serialize");
        assert!(
            !json.contains("\"key\""),
            "the listed device carries a `key` field: {json}"
        );
        assert!(
            !json.contains("last_seen_at") && !json.contains("revoked_at"),
            "absent optional fields must be omitted rather than sent as null: {json}"
        );

        let issued = DeviceKeyIssued {
            id: "01ABC".into(),
            key: "deadbeef".into(),
            scopes: vec!["capture:write".into()],
        };
        let json = serde_json::to_string(&issued).expect("serialize");
        assert!(
            json.contains("\"key\":\"deadbeef\""),
            "the issue response must carry the plaintext -- it is the only time it exists: {json}"
        );
    }

    /// The request round-trips, and the optional platform is omitted when
    /// absent.
    #[test]
    fn the_request_round_trips() {
        let req = NewDeviceRequest {
            label: "laptop".into(),
            scopes: vec!["capture:write".into(), "session:read".into()],
            platform: None,
        };
        let json = serde_json::to_string(&req).expect("serialize");
        assert!(
            !json.contains("platform"),
            "an absent platform should be omitted: {json}"
        );
        let back: NewDeviceRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.label, "laptop");
        assert_eq!(back.scopes.len(), 2);
    }
}

// ══════════════════════════════════════════════════════════
//  Device-code login (P6-4)
// ══════════════════════════════════════════════════════════

/// `POST /devices/authorize` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizeRequest {
    /// What this machine calls itself, shown on the confirmation screen.
    pub label: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
}

/// `POST /devices/authorize` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthorizationStarted {
    /// The short code the person types. **Not** the polling credential.
    pub user_code: String,
    /// The long secret the machine polls with.
    pub device_code: String,
    /// Where to send the person. Relative, so the CLI can print it against
    /// whatever server it is talking to.
    pub verification_uri: String,
    pub expires_in: u64,
    /// Minimum seconds between polls (RFC 8628).
    pub interval: u64,
}

/// `POST /devices/token` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRequest {
    pub device_code: String,
}

/// `POST /devices/token` response.
///
/// Tagged, so a client cannot mistake "still waiting" for a key: an untagged
/// struct with an optional field would make a missing key look like a parse
/// problem rather than a state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TokenPoll {
    /// Nobody has confirmed yet. Poll again after `interval`.
    Pending,
    /// Confirmed and issued.
    Issued { key: DeviceKeyIssued },
    /// The code expired or was never valid. Start over.
    Expired,
}

/// `POST /devices/confirm` body — a logged-in person approving a code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmRequest {
    pub user_code: String,
}

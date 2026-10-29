//! The on-the-wire shapes for accounts.
//!
//! # Why two kinds of token rather than one
//!
//! A single token only lets you pick between two bad outcomes: either it is
//! short-lived (the user gets kicked out every fifteen minutes), or it is
//! long-lived (an attacker who leaks it can use it for a long time).
//!
//! Splitting it in two gets you both: `access_token` lives 15 minutes, rides
//! on every request, and has a very short leak window; `refresh_token` lives
//! 30 days, is used once per access exchange, and otherwise sits in the
//! system credential store.
//!
//! **"Close the app, reopen it, no need to log in again" rests entirely on the
//! latter.** If the server issues it and the client does not store it, this
//! whole arrangement was for nothing — see `app/lib/auth/`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// What this device is called. Purely diagnostic.
    ///
    /// When one account is logged in on three machines, this is the only thing
    /// that can answer "whose credential leaked".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    /// Invite code. While open registration is closed, this is the only way in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshRequest {
    /// **In the body, not the URL.** It is a long-lived credential, and URLs
    /// end up in access logs, in reverse-proxy logs, in browser history — the
    /// same reason as the token in `ConfirmReceipt`.
    pub refresh_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthTokens {
    pub access_token: String,
    pub access_expires_in_secs: u64,
    /// **Appears here exactly once.** The server stores only its digest; lose
    /// it and the only way back is logging in again.
    pub refresh_token: String,
    /// How long the account can go unused before the password is needed again.
    /// A client can use this to decide whether to warn the user ahead of time.
    pub refresh_expires_in_secs: u64,
    pub user_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmI {
    pub user_id: String,
    pub username: String,
    /// Which schema this person's memory lives in.
    ///
    /// It is handed down so that troubleshooting can line things up at a
    /// glance: "is what I am looking at actually my own database" is a question
    /// people really do ask out loud in a multi-tenant system.
    pub schema_name: String,
}

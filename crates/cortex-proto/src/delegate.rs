//! Delegated credentials — the memory service signing a short-lived, scoped
//! key for an **untrusted caller**.
//!
//! # This is a feature of the memory service, not of the sandbox
//!
//! The absence of "sandbox" from the name is deliberate. The question cortexd
//! answers is "**what may a low-trust caller do to me**", which is its own
//! inbound authorization policy; whether the party receiving the key is a
//! docker container, a WASM sandbox, or a process in somebody else's data
//! centre, cortexd neither knows nor should know.
//!
//! The memory service, open-sourced on its own, therefore carries this
//! capability with it: **you can hand out a per-session slice of memory read
//! and write access without handing over the user's primary credential.**
//!
//! # Who may sign
//!
//! The caller presents **the user's own bearer**, not some service credential.
//! So it can only issue keys for "the user who handed it that token" — no new
//! privilege is created anywhere.
//!
//! Doing it the other way round (giving the orchestrator a service key that can
//! issue on behalf of any user) would gratuitously add an escalation surface:
//! whoever gets that key can sign themselves a key that reads anyone's memory.

use serde::{Deserialize, Serialize};

/// The request body of `POST /delegated-tokens`.
///
/// # Why only `session_id`, with no project
///
/// The project is **looked up out of the session row** by the server, not
/// supplied by the caller. Let the caller supply it and it can claim some
/// session belongs to another project — and the project is what determines
/// container and volume, so that would be stepping over workspace isolation
/// with a single field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateRequest {
    /// Which session this key is bound to. The scope recognizes nothing else.
    pub session_id: String,
}

/// The receipt from `POST /delegated-tokens`.
///
/// # Why all three come back at once
///
/// All three derive from **the same session row**. Split into three requests,
/// concurrent modification lets the three decisions see different snapshots,
/// and the symptom is "occasionally enters the wrong container" — nothing is
/// harder to reproduce than that. The `sandbox_scope` on the cortexd side is a
/// single query for the same reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Delegation {
    /// The key itself. The caller hands it to the delegated party (stuffed into
    /// a container's env, say).
    pub token: String,
    /// Who this key stands for.
    ///
    /// The caller uses it for resource attribution (logs, per-person billing).
    /// **The server parses it out of the credential**, and it is the same id
    /// `GET /auth/me` returns — but that would cost another network round trip,
    /// and these two things should come from the same authentication anyway.
    pub owner: String,
    /// The **canonical name** of this scope: derived from owner and project,
    /// identical for identical scopes.
    ///
    /// The caller uses it as a resource name (container name, volume name,
    /// cache key). It therefore guarantees: only `[a-z0-9-]`, no more than 53
    /// bytes, and no collisions between distinct scopes.
    ///
    /// **Only the server can resolve the owner** (it comes out of the
    /// credential), so this name has to be issued by the server — the caller
    /// cannot construct it, and should not be able to.
    pub scope_key: String,
    /// Where this session is supposed to run.
    ///
    /// `Local` = pinned to directories on some client machine; the cloud side
    /// cannot run it. The caller uses this to refuse, rather than starting a
    /// container that is guaranteed not to be able to read those files.
    pub runtime: crate::dto::SessionRuntimeDto,
    /// Whether this account's model quota is already used up.
    ///
    /// # Why this reports rather than blocks
    ///
    /// Quota governs **model spend**, and this key is not used only for
    /// conversation: listing files or downloading a workspace with it costs not
    /// one token. Blocking issuance on quota means that once a user's quota
    /// runs out they cannot even retrieve their own files — which is exactly
    /// the "can't get my own data out" class of problem.
    ///
    /// So this only reports, and the caller decides according to purpose: a
    /// **conversation** turn should be refused before the container is even
    /// started (saving a cold start), while listing files proceeds as normal.
    ///
    /// The real gate is always `POST /llm/stream` — **that is where the money
    /// gets spent**, and that one does not depend on any caller's good
    /// intentions.
    pub quota_exhausted: bool,
}

/// The request body of `DELETE /delegated-tokens`.
///
/// A key that was signed and then never used (the container failed to start)
/// has to be taken back — otherwise it is an ownerless key that stays valid for
/// the whole TTL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeRequest {
    pub token: String,
}

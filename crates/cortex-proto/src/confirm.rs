//! The **wire** half of the tool confirmation loop — the response items of
//! `GET /confirmations`.
//!
//! # Why only this one type is left
//!
//! The whole registry (`ConfirmRegistry` with its oneshots, TTL and `Drop`
//! cleanup) used to live here too, on the grounds that "the response body of
//! `GET /confirmations` *is* its `PendingInfo`, and the fields of
//! [`crate::dto::ChatEvent::Confirm`] have to line up with it word for word".
//!
//! That reasoning covers [`PendingInfo`]; it does not cover the registry. And
//! the registry dragged the whole of `cortex-agent` (agent loop + tools +
//! sandbox) into this **wire protocol** crate — so anyone who wanted only the
//! memory layer had to compile the agent along with it.
//!
//! The registry now lives in `cortex-local` (its only possible host: cortexd
//! runs no agent, and the one inside a container never asks for confirmation),
//! while the wire type stays here as a neighbour of `ChatEvent::Confirm`. The
//! two halves of the contract are still in the same folder, and the dependency
//! is severed.

use serde::{Deserialize, Serialize};

/// The outward form of a pending confirmation (`GET /confirmations` and the SSE
/// event share the same field vocabulary).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInfo {
    pub token: String,
    pub session_id: String,
    pub tool: String,
    /// `"safe"` / `"write"` / `"execute"` — the set of values is pinned by
    /// `cortex_agent::Risk::as_wire`, and clients use it to decide what the
    /// confirmation dialog looks like.
    ///
    /// On the host side (`PendingMeta` in `cortex-local`) this is a
    /// `&'static str`, because it can only ever come from those three
    /// literals. Here it has to be a `String`: this response gets **read back**,
    /// and a `&'static str` cannot be deserialized into.
    pub risk: String,
    pub preview: String,
    pub asked_at: String,
    /// How many seconds remain before this is treated as a rejection. Clients
    /// draw the countdown from it, and also learn from it that a prompt which
    /// "looks like it is still waiting" went stale long ago.
    pub expires_in_secs: u64,
    /// The absolute target path of an out-of-scope access. `None` = nothing out
    /// of scope.
    ///
    /// Approving something out of scope and approving an ordinary write are two
    /// different things, and the user has to be able to tell them apart — the
    /// tool name plus the preview is not enough: the path inside the preview
    /// may well be relative, and "relative to what" is precisely the thing the
    /// user most needs to know right then, and most easily gets wrong.
    ///
    /// `default` is so old clients and old servers can still read each other:
    /// this field was added later, and its absence should cost one line of
    /// detail, not fail the deserialization of the whole confirmation — that
    /// would leave the user with no dialog at all, rather than a dialog with a
    /// little less information in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// What this write will turn the file into. `None` = no change worth
    /// showing.
    ///
    /// **This is the cure for blind signing**: with only a tool name and a
    /// preview, what the user sees is "write_file wants to write config.toml" —
    /// and what they approve is content they have never read.
    ///
    /// The reason for `default` is word for word the one on `scope`: a field
    /// added later, whose absence should cost one diff, not fail the
    /// deserialization of the whole confirmation — that would leave the user
    /// with **no dialog at all**, which is far worse than a dialog with a
    /// little less information in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

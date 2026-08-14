//! Where local state lives on this machine.
//!
//! # Why this is in cortex-core rather than in the local agent
//!
//! It grew up in `cortex-local::config`, because only the agent needed it. Now
//! the CLI needs it too: it has to look in this directory for the address
//! handshake file of **the agent the desktop app already started**, or two
//! agents end up running on one machine, contending over the same
//! `workspaces.json` and `outbox.mark` — and those two files are protected only
//! by an in-process `Mutex`, with no file lock.
//!
//! One copy each is a shape already caught three times this round (`risk_str`
//! flattened, three copies of `Health`, self-contradictory prompts): both sides
//! compute the same path today, tomorrow somebody changes one of them, and the
//! symptom is "the CLI cannot find the desktop app's agent" while nothing
//! anywhere raises an error.

use std::path::PathBuf;

use crate::{CortexError, Result};

/// The local state directory: the outbox, workspace bindings, and address
/// handshake files all live here.
///
/// Windows uses `%LOCALAPPDATA%` rather than `%APPDATA%`: the latter gets
/// synced to other machines in the domain by roaming profiles, and everything
/// in here is **true only for this machine** — syncing `D:\codes\myproject` to
/// a colleague's laptop is meaningless, and an outbox synced across machines
/// would have the same episode replayed once by each machine.
///
/// # Errors
/// Returns an error when the directory cannot be created (permissions, disk
/// full). **Does not fall back to a temp directory** — that would make the
/// outbox vanish across a restart, i.e. "conversations held while offline
/// quietly disappeared", which is the very reason this queue exists.
pub fn state_dir() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    let base = base.ok_or_else(|| {
        CortexError::Config(
            "找不到本地状态目录：Windows 上需要 %LOCALAPPDATA%，\
             其余平台需要 $XDG_DATA_HOME 或 $HOME"
                .into(),
        )
    })?;
    let dir = base.join("cortex");
    std::fs::create_dir_all(&dir)
        .map_err(|e| CortexError::Config(format!("建不出本地状态目录 {}：{e}", dir.display())))?;
    Ok(dir)
}

/// Extension of the address handshake file.
///
/// After binding `127.0.0.1:0` (letting the kernel pick the port), the local
/// agent writes its real address into a file like this, and **whoever started
/// it** reads it back and immediately deletes it. **Guessing a fixed port fails
/// in the worst possible way**: connecting to something else that happens to be
/// listening — hit for real this round, where 8090 held another application's
/// web client that answered `/health` with a 200 and a chunk of HTML.
pub const ADDR_FILE_EXT: &str = "addr";

/// Extension of the liveness pointer file. See [`live_file`].
pub const LIVE_FILE_EXT: &str = "live";

/// The pointer file saying "there is an agent on port X on this machine".
///
/// # This is a different thing from [`ADDR_FILE_EXT`]
///
/// The address handshake file is **single-use**: whoever started the agent
/// reads it and deletes it, because it only answers "where did the one I just
/// started bind". Keeping it around would manufacture staleness — which is
/// exactly why deleting it was the original decision.
///
/// The liveness pointer answers a different question: **"is there a usable
/// agent right now"**, and whoever is asking may not be the process that
/// started it at all. The desktop app starts an agent, and later the user types
/// `cortex chat` in a terminal — the CLI needs to find it, or the machine ends
/// up with two agent instances contending over the same `workspaces.json` and
/// `outbox.mark` (those two files are protected only by an in-process `Mutex`,
/// with no file lock).
///
/// # What about staleness — probe, do not delete
///
/// This file survives when the process is killed, the power cuts, or it
/// crashes. The reader **must** probe the address it gets for real (hit
/// `/health` and confirm `role == "local-agent"`), and skip it if the probe
/// fails. The addr file chose "read then delete" because **there was no probe
/// back then**; now there is, and staleness is no longer a problem that has to
/// be dodged via file lifetime.
///
/// # The filename carries the pid
///
/// Multiple agents may coexist on one machine (different accounts, ones started
/// by hand during development), and a fixed filename would have them overwrite
/// each other — with the consequence of "pointing at an instance that is
/// already dead". With the pid, each writes its own, and the reader scans the
/// directory, probes them one by one, and uses the first live one.
#[must_use]
pub fn live_file(dir: &std::path::Path, pid: u32) -> PathBuf {
    dir.join(format!("agent-{pid}.{LIVE_FILE_EXT}"))
}

/// Contents of the liveness pointer.
///
/// # Why not just write an address
///
/// Reusing an agent means **handing your requests to its identity**: it uses
/// its own token to reach its own remote. Same address does not mean same
/// identity — an agent pointed at a different cortexd, or logged into another
/// account, would have the CLI silently read and write **someone else's**
/// memory, with no symptom whatsoever. We already got bitten by the same shape
/// this round via "the remote was not passed down".
///
/// So two things must match before reuse: the remote address, and the
/// **credential fingerprint**.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LivePointer {
    /// The actual listening address, e.g. `127.0.0.1:54321`
    pub addr: String,
    /// The cortexd it is connected to
    pub remote: String,
    /// Credential fingerprint. See [`token_fingerprint`]. `None` when there is
    /// no credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_fp: Option<String>,
}

/// A short fingerprint of a credential — used only to decide "is this the same
/// key".
///
/// The first 16 hex characters (64 bits) of a SHA-256. This is **not** for
/// authentication, so the truncation carries no security implication; what it
/// guards against is writing the token itself into a plaintext file — one that
/// sits in the state directory, readable by any process on the machine.
///
/// 64 bits is more than enough for "the same token computes the same value",
/// and the original cannot be recovered from it.
#[must_use]
pub fn token_fingerprint(token: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let d = Sha256::digest(token.as_bytes());
    format!("{d:x}").chars().take(16).collect()
}

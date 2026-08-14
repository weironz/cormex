//! Importing ChatGPT / Claude export files — the wire shapes shared by all
//! three front ends.
//!
//! # Two routes, one contract
//!
//! - **Desktop**: the file is on this machine, so it goes through the local
//!   agent's `POST /local/import/preview` + `POST /local/import/run`. The
//!   request body carries a **path**; not one byte crosses the network.
//! - **Web**: the browser cannot reach the disk, so it first `POST`s the file
//!   to cortexd via `/import/upload` to get a handle, then uses that handle
//!   with `/import/preview` + `/import/run`.
//!
//! Both routes' preview and run return **the same set of types**
//! ([`ImportEstimate`] / [`ImportEvent`]), so there is only one import page on
//! the client side; the only difference is whether it got a path or a handle.
//!
//! # Why preview is a step of its own rather than a flag on run
//!
//! It corresponds to `--dry-run` being the default in the CLI, and the
//! reasoning is even harder in a GUI: every message pair triggers one LLM call,
//! three years of history may be several thousand of them, and memory is
//! append-only — importing the wrong thing can only be undone via redact, which
//! is an explicit operation requiring a second confirmation.
//!
//! Making it two endpoints (rather than `run(dry: true)`) is so that **calling
//! the wrong one cannot cost money**: a read-only endpoint has no path to a
//! write at all.

use serde::{Deserialize, Serialize};

/// The handle you get back after uploading an export file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadAck {
    /// Subsequent preview / run calls use this to refer to the file.
    pub handle: String,
    /// Bytes written to disk — the client can reconcile it against its own
    /// notion of the size.
    pub size_bytes: u64,
    /// How many seconds until this temporary file is cleaned up.
    ///
    /// **It has to be reported**: a 97MB upload is not cheap, and a user who
    /// wanders off and comes back an hour later to click "Start" finds the
    /// handle already expired — the UI needs to be able to say so in advance.
    pub expires_in_secs: u64,
}

/// The input to preview / run. **One or the other**: a local path or an upload
/// handle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ImportRequest {
    /// An absolute path on this machine. Only the local agent honours this
    /// field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A [`UploadAck::handle`]. Only cortexd honours this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
    /// `"chatgpt"` / `"claude"`. Sniffed from the file content when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<String>,
    /// Import only conversations with messages after this date (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// Import at most this many conversations (**starting from the most
    /// recent**). Try three or five first to test the water.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_conversations: Option<usize>,
}

/// The bill, itemized. The fields correspond one to one with
/// `cortex_import::Estimate` — that side computes, this side passes it through
/// verbatim, so the numbers in the UI and the ones the CLI prints come out of
/// the same calculation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportEstimate {
    /// `"ChatGPT"` / `"Claude"`, the human-readable spelling.
    pub platform: String,
    pub conversations: usize,
    pub messages: usize,
    /// How many extractions this will trigger = how many LLM calls.
    /// **This number is money, directly.**
    pub pairs: usize,
    pub tokens: usize,
    /// RFC 3339; `None` when there is not a single conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub earliest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// The count of unpaired messages (they go into the raw text, but produce
    /// no facts).
    ///
    /// Reported separately because when a user sees "imported 100 messages" and
    /// yet only gains 40 memories, there has to be somewhere that explains the
    /// difference.
    pub unpaired: usize,
    /// A lower-bound estimate in minutes, based on the client's rate limit.
    pub minutes: f64,
}

/// The SSE events of `run`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImportEvent {
    /// Restate the bill before starting work.
    ///
    /// The client already holds the preview result, but that may have been
    /// clicked ten minutes ago, and parameters such as `--max-conversations`
    /// may have changed since. **This** one is authoritative.
    Started { estimate: ImportEstimate },
    /// Progress. Fields correspond to `cortex_import::Progress`.
    Progress {
        conversations_done: usize,
        conversations_total: usize,
        pairs_done: usize,
        /// The number the server recognized as "already there" — nothing was
        /// billed twice.
        skipped: usize,
        failures: usize,
    },
    /// Finished.
    Done {
        pairs_done: usize,
        skipped: usize,
        /// The failure count. **When it is non-zero the UI should say "just run
        /// it again"** — writes are idempotent, so a rerun only fills in the
        /// ones that did not make it, and bills nothing twice.
        failures: usize,
    },
    /// Something went wrong. Still returned as an SSE event, so that a client
    /// is not left with a truncated stream and no way to tell why.
    Error { message: String },
}

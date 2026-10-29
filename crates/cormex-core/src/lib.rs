//! Cormex core types.
//!
//! Deliberately kept **very thin**: only the globally shared identifiers,
//! errors, and configuration live here.
//!
//! In particular, **the canonical message format is not here** — we reuse
//! `goose-provider-types`' `Message` / `Provider` directly, which already
//! carries each vendor's thinking / reasoning opaque blocks and prompt-caching
//! semantics losslessly (see docs/architecture.md, "LLM providers"). Defining
//! another layer of our own would only introduce a lossy conversion.
//!
//! # Why [`injection`] lives in this "very thin" crate
//!
//! It used to live in `cormex-memory`, since memory injection is the memory
//! engine's exit. But once the agent loop moved local, **two independently
//! released processes have to render byte-identical memory blocks**: cormexd
//! renders for the Web client, and the local agent renders for itself.
//!
//! Having the endpoint return already-rendered text — one side rendering for
//! both — looks like less work, but on that path format drift **raises no
//! error**: the model simply receives a memory block with the wrong structure,
//! quality quietly degrades, and it only surfaces on eval scores weeks later.
//! Sharing the same code on both sides, with the endpoint returning only
//! structured data, turns drift into a JSON deserialization failure that blows
//! up on the spot.
//!
//! Putting it here is only viable because it genuinely has no dependencies:
//! the entire module's only import is `use std::fmt::Write`.

pub mod config;
pub mod error;
pub mod history;
pub mod id;
pub mod injection;

pub use config::Config;
pub use error::{CormexError, Result};
pub use id::Id;

/// Character limit for `session_events.workspace`, matching the CHECK in the
/// migration (`cormex_store` re-exports it; keeping those two aligned remains
/// the storage layer's job).
///
/// It sits in this "very thin" crate because **both ends need the number and
/// neither should pull in the other's dependencies for it**: the storage layer
/// re-exports it next to the CHECK it mirrors, and an agent validating a path
/// on the user's own machine gets it without depending on sqlx.
///
/// If each layer wrote its own literal instead, the symptom of drift would be
/// "the client thinks the binding succeeded but the server returns 500" — the
/// database CHECK's error never reaches the user.
pub const WORKSPACE_PATH_MAX_CHARS: usize = 4096;

/// Compile-time version number, used by `/health` and the CLI's version
/// output.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

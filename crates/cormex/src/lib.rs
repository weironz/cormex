//! The Cormex capture CLI.
//!
//! Design: docs/design/session-archive-architecture.md §5.
//!
//! # What this is, and what it is not
//!
//! It reads the session files AI agents write to disk and sends them, byte for
//! byte, to a Cormex server. It is **not** an agent, and it does not parse what
//! it reads: parsing happens on the server (D2), so that twenty hosts' worth of
//! format churn is maintained in one place rather than in every installed
//! binary.
//!
//! # The shape of the crate
//!
//! A lib plus a bin. `main.rs` is argument parsing and exit-code mapping and
//! nothing else; everything testable lives here, so the end-to-end test can
//! drive the engine directly instead of shelling out to a binary.
//!
//! # The dependency rule
//!
//! This crate must not depend on `cormex-store`, `cormex-llm`, `sqlx` or
//! `goose-providers`. It is a static single binary that ships to other people's
//! machines, and every one of those would multiply its size for something it
//! never uses. `cargo tree -p cormex` is the check.

pub mod catalog;
pub mod client;
pub mod cmd;
pub mod config;
pub mod credentials;
pub mod git_meta;
pub mod hook;
pub mod install;
pub mod state;
pub mod sync;

pub use client::{ApiClient, ExitCode};
pub use config::{Config, Dirs};
pub use state::StateDb;

/// A process-wide lock for tests that mutate environment variables.
///
/// # Why one lock rather than one per module
///
/// `set_var` is process-global, and `cargo test` runs every test in one binary
/// on a thread pool. Three modules (`cmd::init`, `cmd::uninstall`, `catalog`)
/// each point `CLAUDE_CONFIG_DIR` at their own temporary home; a per-module
/// lock protects each from itself and **none of them from the other two**.
///
/// The failure that produces is the worst shape a test can have: each passes
/// alone, and a full run fails intermittently — `init` looks for transcript
/// files in another test's temporary directory and finds none.
#[cfg(test)]
pub(crate) mod test_env {
    /// Held for the whole of a test that sets or removes an environment
    /// variable. `tokio::sync::Mutex` rather than `std::sync::Mutex`: the
    /// guard is held across `await`, and a std guard there blocks the worker
    /// thread (clippy's `await_holding_lock` is right to refuse it).
    pub static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
}

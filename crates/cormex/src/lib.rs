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
pub mod state;

pub use client::{ApiClient, ExitCode};
pub use config::{Config, Dirs};
pub use state::StateDb;

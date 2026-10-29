//! cormexd as a **library**, so that the end-to-end test can start the real
//! router in-process.
//!
//! # Why this exists
//!
//! P1-12's test has to drive the whole capture path: the CLI's sync engine
//! against the real server, over a real database and a real object store. That
//! needs [`routes::router`] and [`state::AppState`] to be reachable from a test
//! crate, and a `[[bin]]`-only crate exposes nothing.
//!
//! # What is public
//!
//! Only what a test needs to stand a server up: the router, the state, and the
//! account/device helpers that mint a key. Everything else stays private — a
//! library surface that grows because it was convenient is a second API to keep
//! stable, and this one is not an API at all.

pub mod accounts;
pub mod archive_api;
pub mod archive_workers;
pub mod auth;
pub mod backfill;
pub mod blobs;
pub mod brief;
pub mod byo_key;
pub mod capture;
pub mod capture_keys;
pub mod cors;
pub mod credentials;
pub mod cursor;
pub mod delegated_token;
pub mod device_codes;
pub mod device_keys;
pub mod devices_api;
pub mod import;
pub mod live;
/// Memory's outward-facing MCP frontage. See that module's header comment.
pub mod mcp;
pub mod principal;
pub mod purge;
pub mod quota;
pub mod reembed;
pub mod request_tenant;
pub mod routes;
pub mod snapshots;
pub mod state;
pub mod sync_notify;
pub mod sync_payload;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
pub mod ws;

// The wire protocol lives in `cormex-proto`, because the agent side has to
// speak the **same** protocol (see that crate's module comment). Re-exporting
// it here as `crate::dto` keeps every existing call site untouched while
// confining "the protocol is not cormexd's private property" to this one line.
//
// `confirm` is no longer re-exported: that ledger moved to `cormex-local`.
// cormexd does not run an agent, so it has no confirmation loop. A re-export
// that resolves to no call site at all would only make the next person believe
// a copy still lives here.
pub use cormex_proto::dto;

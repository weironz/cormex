//! Server-side transcript parsing: an agent's session file to normalized events.
//!
//! Design: docs/design/session-archive-architecture.md §7.2 (D2, D11).
//!
//! # Parsing lives here, and only here
//!
//! The client never parses what it captures (D2). Twenty hosts' worth of format
//! churn is maintained in one place, on the server, where a parser fix reaches
//! every already-captured conversation — the raw bytes are kept, so a
//! re-projection is always possible.
//!
//! # What is in this crate today
//!
//! [`anonymize`], the structure-preserving anonymizer that produces the test
//! fixtures (P2-0). The parsers themselves land in P2-1, on top of this
//! skeleton.

pub mod anonymize;

pub use anonymize::{Anonymizer, RULES_VERSION, synthetic_text};

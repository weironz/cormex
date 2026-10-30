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
//! # What is in this crate
//!
//! - [`anonymize`]: the structure-preserving anonymizer that produces the test
//!   fixtures
//! - [`jsonl`]: line splitting across chunk boundaries, shared by every parser
//! - [`parser`]: the [`parser::Parser`] trait and the shared event shape
//! - [`claude_code`] and [`codex`]: the two parsers

pub mod aider;
pub mod anonymize;
pub mod claude_code;
pub mod cline;
pub mod codex;
pub mod copilot_chat;
pub mod export;
pub mod gemini;
pub mod jsonl;
pub mod parser;
pub mod qwen;
pub mod rows;

pub use anonymize::{Anonymizer, RULES_VERSION, synthetic_text};
pub use parser::{ParsedEvent, Parser, parser_for};

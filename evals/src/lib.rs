//! Cormex retrieval quality evals.
//!
//! The project docs say themselves that "the bottleneck is retrieval quality,
//! not data volume", yet before this crate existed, RRF's k value, the four
//! retrieval paths' widths, the domain weighting coefficients, and budget
//! truncation could all only be tuned by feel. This does exactly one thing:
//! **give every "optimization" a number to compare against**.
//!
//! ```text
//! suite (JSON) ──► the real ingest pipeline ──► an isolated temp schema
//!                                          │
//!                     ┌────────────────────┴──────────────────┐
//!                     ▼                                       ▼
//!            Retriever::retrieve                  the four raw paths (diagnostic)
//!            (four paths + RRF +                  bm25/vector/graph/recency
//!             domain weighting + budget)
//!                     │                                       │
//!                     └──────────────► metrics ◄──────────────┘
//!                     Recall@k · MRR · per-path exclusives · false-recall rate
//!                                     │
//!                       human-readable report + machine-readable JSON
//! ```
//!
//! Each module's tradeoffs are written in its own file header:
//! - [`suite`] the suite format and static validation (why gold cannot be
//!   written as fact ids)
//! - [`matcher`] gold matching (why only the statement is examined)
//! - [`metrics`] the metrics (why grouping is mandatory, why per-path
//!   contribution is mandatory)
//! - [`harness`] isolation and the real pipeline (why facts cannot simply be
//!   INSERTed)
//! - [`report`] the report (why both JSON and text are needed)

pub mod harness;
pub mod matcher;
pub mod metrics;
pub mod report;
pub mod runner;
pub mod suite;

/// Default path to the in-repository suite (relative to the repo root).
pub const DEFAULT_SUITE: &str = "evals/suites/cormex-zh-en-v1.json";

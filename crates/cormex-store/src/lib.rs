//! Cormex storage layer: sqlx repositories and the sync_log writer.
//!
//! # Boundary
//!
//! **All SQL is confined to this crate.** Layers above get type-safe methods,
//! not query strings. This is not fastidiousness — it is the only escape hatch
//! we will have when the driver, the database, or the caching layer changes.
//!
//! sqlx's compile-time checked macros (`query!` / `query_as!`) are deliberately
//! not used: they require either a live database connection at compile time or
//! a maintained `.sqlx` offline cache, which is a nuisance for CI and for
//! newcomers getting started. Everything here uses the runtime API
//! (`sqlx::query_as` + [`sqlx::FromRow`]); the price is that a misspelled
//! column name only surfaces when the tests run — integration tests that talk
//! to a real database cover that gap.
//!
//! # Write discipline
//!
//! The foundation of sync correctness. Both the schema and docs/memory.md §4
//! and §9 nail down three rules:
//!
//! 1. The business row and the `sync_log` append must be in the **same
//!    transaction**;
//! 2. The transaction opens with `pg_advisory_xact_lock(4272)`, serializing
//!    commit order so that the order of `sync_log.seq` equals the order in
//!    which rows become visible to readers;
//! 3. Sequence-allocating transactions must be short and write-only.
//!
//! This crate turns those three rules into **structural constraints** rather
//! than conventions: the only way in is [`Store::write_txn`], the [`WriteTxn`]
//! it hands out has write methods only, and every one of those write methods
//! reaches the database through the same private channel — a channel that binds
//! "write the business row" and "append to sync_log" into one indivisible step.
//! See the [`txn`] module docs.
//!
//! # Usage
//!
//! ```no_run
//! use cormex_store::{NewEntity, NewFact, Store};
//!
//! # async fn demo(entity: NewEntity, fact: NewFact) -> cormex_store::Result<()> {
//! let store = Store::connect("postgres://cormex@localhost:5442/cormex").await?;
//!
//! // Write several rows in one transaction; sync_log follows automatically
//! store
//!     .write_txn(async |tx| {
//!         tx.insert_entity(&entity).await?;
//!         tx.insert_fact(&fact).await
//!     })
//!     .await?;
//!
//! // Incremental pull from a client
//! let batch = store.fetch_since(0, 100).await?;
//! # let _ = batch;
//! # Ok(())
//! # }
//! ```

pub mod archive;
mod bootstrap;
mod error;
mod model;
mod query;
mod recall;
mod store;
mod sync;
mod tenant;
mod txn;

pub use archive::{
    AppendOutcome, ArchiveSessionRow, ArchiveTxn, CaptureChunk, CaptureFile, CaptureMask,
    CaptureSession, ChunkEncoding, FileEventOp, FileRole, Framing, Generation, GenerationReason,
    NewAccessLog, NewArchiveChunk, NewCaptureChunk, NewCaptureFile, NewCaptureFileEvent,
    NewCaptureMask, NewCaptureSession, NewMaskProgress, NewProjection, NewTranscriptEvent,
    Projection, ProjectionStatus, RegisteredFile, TranscriptEvent, TranscriptEventKind,
};
pub use bootstrap::{TenantMigration, migrate_all, provision};
pub use error::{Result, StoreError};
pub use model::{
    ATTACHMENT_FILENAME_MAX_CHARS, Actor, Blob, BlobTranscript, CanonicalEntity, Derivation,
    DerivedKind, Entity, EntityMerge, Episode, EpisodeAttachment, EpisodeBlob, EpisodeMemory,
    EpisodeToolCall, Fact, FactEvent, FactOp, FactSource, FactStatus, InjectedMemory,
    InvalidationKind, NewBlob, NewBlobTranscript, NewEntity, NewEntityEmbedding, NewEntityMerge,
    NewEpisode, NewEpisodeBlob, NewEpisodeMemory, NewEpisodeToolCall, NewFact, NewFactEmbedding,
    NewFactEvent, NewProjectEvent, NewRedaction, NewSessionEvent, NewSummary,
    PROJECT_NAME_MAX_CHARS, Project, ProjectEvent, ProjectOp, ProvenanceRef, Redaction,
    RedactionMode, RedactionTarget, Role, SESSION_TITLE_MAX_CHARS, SessionEvent, SessionOp,
    SessionRuntime, SessionState, SourceChannel, SourceKind, Summary, SummaryScope,
    TOOL_DIFF_MAX_CHARS, TOOL_NAME_MAX_CHARS, TOOL_PATH_MAX_CHARS, TOOL_SUMMARY_MAX_CHARS,
    TranscriptKind, WORKSPACE_PATH_MAX_CHARS, table,
};
pub use query::EpisodeCursor;
#[doc(hidden)]
pub use recall::RECALL_VECTOR_SQL;
pub use store::{SessionDigest, Store};
pub use sync::{SyncLogEntry, SyncNotifications, SyncPayload, SyncRecord, SyncSignal};
pub use tenant::{MAX_RESIDENT_TENANTS, PER_TENANT_CONNECTIONS, SchemaName, TenantPools};
pub use txn::{SYNC_ADVISORY_LOCK_KEY, WriteTxn};

/// Vector dimensionality, matching `bge-m3`. To switch models, see
/// docs/memory.md §7.
pub const EMBEDDING_DIM: usize = 1024;

pub use pgvector::Vector;

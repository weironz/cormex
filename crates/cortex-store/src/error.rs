//! Storage-layer errors.
//!
//! This layer swallows nothing: the raw sqlx error is carried through as-is,
//! and only at the boundary (HTTP / CLI) is it narrowed down to
//! [`cortex_core::CortexError`].

use thiserror::Error;

pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),

    #[error("migration failed: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),

    #[error("{ty} has no such value: {value}")]
    UnknownVariant { ty: &'static str, value: String },

    /// `sync_log.table_name` points at a table this layer does not recognize.
    /// Seeing this means somebody bypassed [`crate::WriteTxn`] and wrote to the
    /// database directly.
    #[error("sync_log references an unknown table: {0}")]
    UnknownTable(String),

    /// A derived record failed to declare its sources.
    ///
    /// This is not merely "validation failed": once the source has been
    /// redacted, a derived record with no provenance keeps leaking the very
    /// content that was supposed to be erased — and the provenance **cannot be
    /// reconstructed after the fact** (who summarized it, and from what, is
    /// known only to the step that produced it). So we would rather this
    /// summary never be written at all.
    #[error(
        "{kind} {id} declared no source -- a derived record must carry provenance (docs/memory-content.md §5.3)"
    )]
    MissingProvenance { kind: &'static str, id: String },

    /// The caller handed us something wrong (illegal schema name, tenant pool
    /// full).
    ///
    /// Keeping this separate from [`Self::Sql`] earns its keep: those mean
    /// "something went wrong over on the database side" and warrant a retry or
    /// an alert; this class means "you asked wrong", and retrying it any number
    /// of times changes nothing.
    #[error("{0}")]
    Invalid(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl StoreError {
    /// Whether this write failed because "the primary key / unique constraint
    /// already exists".
    ///
    /// We test the SQLSTATE rather than matching on the error message: the
    /// message carries the table name, the constraint name, and Postgres's
    /// localized wording, so a different locale or a renamed constraint stops
    /// matching. 23505 is `unique_violation` from the SQL standard and will not
    /// change.
    ///
    /// # Who needs this
    ///
    /// Idempotent writes. A local agent that loses its connection queues up
    /// episodes and replays them once it is back online — and a crash between
    /// "the server accepted it" and "the local side recorded it as flushed"
    /// makes it replay one more time. That is the **normal path**: failing to
    /// recognize it produces a 500, the client treats a successful replay as a
    /// failure, and then retries forever.
    ///
    /// The check lives at this layer rather than in the caller: the shape of
    /// `sqlx::Error` is a storage-layer implementation detail, and making
    /// cortexd depend on sqlx just to poke at one SQLSTATE would drag the whole
    /// database driver into its dependency graph — and the dependency direction
    /// is strictly one-way.
    #[must_use]
    pub fn is_duplicate_key(&self) -> bool {
        matches!(self, Self::Sql(sqlx::Error::Database(db)) if db.code().as_deref() == Some("23505"))
    }
}

impl From<StoreError> for cortex_core::CortexError {
    fn from(err: StoreError) -> Self {
        Self::Store(err.to_string())
    }
}

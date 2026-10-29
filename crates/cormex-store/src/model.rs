//! Row structs for each table, the inputs used to write them, and enums for the
//! constrained columns.
//!
//! Three conventions:
//!
//! 1. **`tsv` appears in no struct.** It is a purely derived retrieval artifact
//!    with no corresponding Rust type, and reading it back serves no purpose.
//!    The write side instead passes `tsv_source` (a tokenized, space-separated
//!    term string), which the `to_tsvector('simple', $n)` in the SQL persists in
//!    the same transaction as the main row — docs/memory.md §8 and §9 require
//!    that tsv is never backfilled asynchronously.
//! 2. **`embedding` is kept.** It is part of the row's content: multi-device
//!    sync has to ship it, and switching embedding models has to backfill it in
//!    batches keyed by `embedding_model`.
//! 3. **Columns with a CHECK constraint get an enum; open columns get
//!    `String`.** `entities.kind` and `facts.predicate` are deliberately not
//!    enumerated in the schema (enumerating them would shut new domains out), so
//!    they cannot be enumerated here either.
//!
//! Primary keys use [`Id`] on the **write side** (which guarantees ULID shape at
//! compile time) and `String` on the **read side** (the database column is the
//! `ulid` domain, which is text underneath; parsing it back would only add a
//! failure path). A caller who needs an [`Id`] can `parse()` it themselves.

use chrono::{DateTime, Utc};
use cormex_core::Id;
use pgvector::Vector;
use sqlx::FromRow;

/// Table name constants. `sync_log.table_name` and [`crate::SyncPayload`] share
/// this one set of literals.
///
/// # Why a macro generates them instead of writing thirteen `const`s
///
/// Missing one does not mean "this table fails to sync", it means **the whole
/// of `/sync` breaks**: `load_payloads` hitting a table name it does not know
/// returns `UnknownTable`, that entire batch fails, and the client's cursor is
/// stuck in front of it from then on — and it only goes off the **first time
/// that table is written**, which may be weeks after release.
///
/// The macro also generates [`table::ALL`], which `tests/sync_coverage.rs` uses
/// to hit `load_payloads` with every name in turn. So whoever adds a table only
/// has to add the constant, and forgetting the match arm **fails immediately**
/// rather than waiting for a user's cursor to jam. (That test has itself been
/// fault-injection verified: sticking a fake table name into the list does turn
/// it red.)
///
/// Having a human maintain `ALL` by hand would not do — that merely moves the
/// easy-to-forget spot from one place to two.
pub mod table {
    macro_rules! tables {
        ($($name:ident = $lit:literal),+ $(,)?) => {
            $(pub const $name: &str = $lit;)+
            /// Every table name that can appear in `sync_log.table_name`.
            pub const ALL: &[&str] = &[$($lit),+];
        };
    }

    tables! {
        EPISODES          = "episodes",
        BLOBS             = "blobs",
        EPISODE_BLOBS     = "episode_blobs",
        EPISODE_MEMORIES  = "episode_memories",
        EPISODE_TOOL_CALLS = "episode_tool_calls",
        BLOB_TRANSCRIPTS  = "blob_transcripts",
        ENTITIES          = "entities",
        ENTITY_MERGES     = "entity_merges",
        FACTS             = "facts",
        FACT_EVENTS       = "fact_events",
        SUMMARIES         = "summaries",
        DERIVATIONS       = "derivations",
        REDACTIONS        = "redactions",
        SESSION_EVENTS    = "session_events",
        PROJECT_EVENTS    = "project_events",
    }

    // ── The two tables deliberately **not** in the macro above ────
    //
    // `fact_embeddings` / `entity_embeddings` hold the vectors backfilled when
    // the model changes (see migrations/20260808000001). They **never enter
    // sync_log**, so they must not appear in `ALL` either — the definition of
    // `ALL` is precisely "every table name that can appear in
    // sync_log.table_name".
    //
    // Why they are not synced: the module comment of `sync_payload.rs` nailed
    // the reasoning down long ago — "tsv and embedding are server-derived
    // columns and do not enter the sync payload... **they change wholesale when
    // the embedding model changes, blowing up incremental sync for nothing**".
    // That sentence is about exactly this. One backfill over a hundred thousand
    // facts would push out a hundred thousand sync_log rows, and not one field
    // in the payload each client receives is of any use to it.
    //
    // The price is that they cannot go through `Guarded::insert_row` (that
    // channel forces a sync_log write) and must use `insert_derived_row`. That
    // method's comments spell out the boundary of where it applies.
    pub const FACT_EMBEDDINGS: &str = "fact_embeddings";
    pub const ENTITY_EMBEDDINGS: &str = "entity_embeddings";

    // ── The archive tables: also **not** in `ALL` ─────────────────
    //
    // Same rule as the two above, for a different reason. `ALL` means "every
    // table name that can appear in `sync_log.table_name`", and the capture
    // tables never appear there: the raw archive is server-only and is not
    // pushed to any device (D3).
    //
    // They are listed here rather than left unmentioned so that the
    // disjointness is something a test can assert. The failure this guards
    // against is somebody adding a capture table to `ALL` "for completeness",
    // at which point `sync_payload` would need a branch for it and the R layer
    // would start flowing to devices — with no error anywhere.
    //
    // Phase 2 appends `transcript_events`, `transcript_projections` and
    // `archive_chunks` here.
    pub const ARCHIVE: &[&str] = &[
        "capture_sessions",
        "capture_files",
        "capture_generations",
        "capture_chunks",
        "capture_file_events",
        "capture_masks",
        "capture_mask_progress",
    ];
}

/// Every column of the `facts` table. The four retrieval channels, the three
/// replay retrievals, fetching facts by primary key / subject / object /
/// provenance, and the sync payload all need them back verbatim.
///
/// Copying the list a dozen times means that adding a column misses one of the
/// copies — and the error from a missed copy is the runtime "`FromRow` cannot
/// find column x", which only goes off when that particular query happens to be
/// exercised. Adding the `source_channel` / `trust_tier` columns this time
/// around, the five hand-copied duplicates in query.rs demonstrated exactly
/// that risk, so they were consolidated here.
macro_rules! fact_columns {
    () => {
        fact_columns!("")
    };
    // Queries with a JOIN must carry a table alias prefix, or `id` collides
    // with the joined side
    ($p:literal) => {
        concat!(
            $p,
            "id, ",
            $p,
            "subject_id, ",
            $p,
            "predicate, ",
            $p,
            "object_text, ",
            $p,
            "object_entity_id, ",
            $p,
            "statement, ",
            $p,
            "embedding, ",
            $p,
            "embedding_model, ",
            $p,
            "domain, ",
            $p,
            "confidence, ",
            $p,
            "valid_at, ",
            $p,
            "source_episode_id, ",
            $p,
            "source_channel, ",
            $p,
            "trust_tier, ",
            $p,
            "extracted_by, ",
            $p,
            "device_id, ",
            $p,
            "created_at"
        )
    };
}

pub(crate) use fact_columns;

// ══════════════════════════════════════════════════════════
//  Columns with a CHECK constraint — enums stored as TEXT
// ══════════════════════════════════════════════════════════

/// Generate an enum stored as TEXT, together with sqlx's `Type` / `Encode` /
/// `Decode`.
///
/// `#[derive(sqlx::Type)]` is not used: those macros target Postgres's native
/// ENUM type, whereas these columns in the schema are TEXT with a CHECK.
/// The `as_str` / `FromStr` / sqlx `Type`+`Encode`+`Decode` boilerplate for a
/// TEXT column with a CHECK constraint.
///
/// Exported inside the crate so that `archive::model` can declare its own
/// enums the same way rather than hand-writing a second copy of the impls —
/// and so that an unknown value from the database keeps producing
/// [`crate::StoreError::UnknownVariant`] instead of a panic.
macro_rules! text_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(rename_all = "snake_case")]
        $vis enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $name {
            /// The literal used when persisting, corresponding one-to-one with
            /// the CHECK in the migration.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $text ),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::StoreError;

            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                match s {
                    $( $text => Ok(Self::$variant), )+
                    other => Err($crate::StoreError::UnknownVariant {
                        ty: stringify!($name),
                        value: other.to_owned(),
                    }),
                }
            }
        }

        impl sqlx::Type<sqlx::Postgres> for $name {
            fn type_info() -> sqlx::postgres::PgTypeInfo {
                <str as sqlx::Type<sqlx::Postgres>>::type_info()
            }

            fn compatible(ty: &sqlx::postgres::PgTypeInfo) -> bool {
                <str as sqlx::Type<sqlx::Postgres>>::compatible(ty)
            }
        }

        impl sqlx::Encode<'_, sqlx::Postgres> for $name {
            fn encode_by_ref(
                &self,
                buf: &mut sqlx::postgres::PgArgumentBuffer,
            ) -> std::result::Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
                <&str as sqlx::Encode<'_, sqlx::Postgres>>::encode(self.as_str(), buf)
            }
        }

        impl<'r> sqlx::Decode<'r, sqlx::Postgres> for $name {
            fn decode(
                value: sqlx::postgres::PgValueRef<'r>,
            ) -> std::result::Result<Self, sqlx::error::BoxDynError> {
                let raw = <&'r str as sqlx::Decode<'r, sqlx::Postgres>>::decode(value)?;
                Ok(raw.parse()?)
            }
        }
    };
}

// Exported inside the crate so `archive::model` can declare its own enums with
// the same impls rather than hand-writing a second copy -- and so that an
// unknown value from the database keeps producing `StoreError::UnknownVariant`
// instead of a panic.
pub(crate) use text_enum;

text_enum! {
    /// `episodes.role`
    pub enum Role {
        User => "user",
        Assistant => "assistant",
        Tool => "tool",
        System => "system",
    }
}

text_enum! {
    /// `blob_transcripts.kind` — which pipeline produced the media transcript
    pub enum TranscriptKind {
        Asr => "asr",
        VisionCaption => "vision_caption",
        Ocr => "ocr",
        FrameCaption => "frame_caption",
    }
}

text_enum! {
    /// `fact_events.op`
    ///
    /// `Invalidate` / `Revoke` are **status** events, and the final state is
    /// determined by the last one; `Flag` is merely an annotation
    /// ("contradiction awaiting human confirmation") and does not change the
    /// status.
    pub enum FactOp {
        Invalidate => "invalidate",
        Revoke => "revoke",
        Flag => "flag",
    }
}

text_enum! {
    /// `fact_events.kind` — the reason for invalidation. The four have
    /// different temporal semantics, and conflating them makes cognitive replay
    /// wrong.
    pub enum InvalidationKind {
        /// Was once true; the world changed. `superseded_by` is required.
        Superseded => "superseded",
        /// Was never true; an extraction error.
        Corrected => "corrected",
        /// The user deleted it deliberately, taking no position on its truth.
        Retracted => "retracted",
        /// Was once true; expired.
        Expired => "expired",
    }
}

text_enum! {
    /// `fact_events.actor`
    pub enum Actor {
        System => "system",
        User => "user",
    }
}

text_enum! {
    /// `session_events.op`
    ///
    /// Four mutually independent dimensions, each a state machine whose final
    /// state is determined by its last event: `Rename` governs the title,
    /// `Archive` / `Unarchive` the archived flag, `BindWorkspace` /
    /// `UnbindWorkspace` the workspace, and `MoveToProject` /
    /// `RemoveFromProject` the owning project.
    ///
    /// **Archiving is not deleting**: archiving only hides the session from the
    /// default list, touching neither the messages nor the derived memory, and
    /// `Unarchive` restores it at any time. What actually destroys content is
    /// [`RedactionMode`], which must be explicitly triggered, confirmed twice,
    /// and leave a tombstone (docs/memory.md §11).
    pub enum SessionOp {
        Rename => "rename",
        Archive => "archive",
        Unarchive => "unarchive",
        BindWorkspace => "bind_workspace",
        UnbindWorkspace => "unbind_workspace",
        MoveToProject => "move_to_project",
        RemoveFromProject => "remove_from_project",
        SetRuntime => "set_runtime",
    }
}

text_enum! {
    /// **Where** this session runs.
    ///
    /// # Why this has to be recorded on the session
    ///
    /// The same session running on two clients points at completely different
    /// filesystems — on the web that is a cloud container's `/workspace`, on
    /// the desktop some local directory. Without recording it, a file the user
    /// had the agent write on client A simply "does not exist" on client B, and
    /// the agent is telling the truth.
    ///
    /// Of the four systems surveyed (Claude Code / Codex / Cursor / OpenHands),
    /// not one allows the same session to run independently on two clients:
    /// **the execution environment is part of the session's identity**.
    ///
    /// # Why Local does not record "which machine"
    ///
    /// There is no client device identity on the wire (`device_id` carries
    /// cormexd's own stamp). But that identity need not be introduced: the
    /// local binding lives in `workspaces.json`, which by nature exists only on
    /// that one machine, so "does this machine have a binding for this session"
    /// is itself the device check — and a firmer one than an id, since ids lie
    /// after a reinstall or a clone.
    pub enum SessionRuntime {
        /// A cloud container workspace. **The default**, and the only one that
        /// can be resumed anywhere.
        Cloud => "cloud",
        /// Pinned to a local directory on one machine. Other devices can open
        /// it to look, but not to continue.
        Local => "local",
    }
}

text_enum! {
    /// `project_events.op`
    ///
    /// Two state machines share one table: `Create` / `Delete` decide whether
    /// the project still exists, and `Create` / `Rename` decide what it is
    /// currently called. `Create` is the starting point of both machines.
    ///
    /// **There is deliberately no archive**: a project is a lightweight
    /// grouping, and adding a layer of "archived projects" would only give the
    /// UI one more filter nobody uses. For the reasoning see the table header of
    /// `migrations/20260812000003_projects.sql`.
    ///
    /// **`Delete` deletes the grouping, not the content**: the project
    /// disappears from the list, the sessions inside it become ungrouped, and
    /// neither the messages nor the derived memory are touched.
    pub enum ProjectOp {
        Create => "create",
        Rename => "rename",
        Delete => "delete",
    }
}

text_enum! {
    /// `facts.source_channel` — the complete value set on the read side.
    ///
    /// The write side uses [`FactSource`], which **does not include**
    /// [`Self::UnknownLegacy`]: that value belongs only to rows that predate the
    /// column, and a new write must say where it came from.
    pub enum SourceChannel {
        /// The user said it themselves
        UserStated => "user_stated",
        /// The model inferred it from the conversation
        Conversation => "conversation",
        /// The model derived it from existing facts (e.g. crosslink's
        /// cross-domain edges)
        Derived => "derived",
        /// Output from a locally executed tool
        ToolOutput => "tool_output",
        /// External content: web pages / other people's documents / MCP. Not
        /// extracted by default
        External => "external",
        /// Rows that predate the column. The real source cannot be
        /// reconstructed after the fact — so it is labelled "unknown" rather
        /// than fabricating a tier that would be taken at face value at query
        /// time. See migration 20260807000006
        UnknownLegacy => "unknown_legacy",
    }
}

/// The source channel on the write side.
///
/// It is a separate type from [`SourceChannel`] for exactly one reason: **to
/// make "a new write labelled unknown_legacy" impossible at compile time**. The
/// "unknown" of pre-existing rows is a one-off historical debt and should not
/// become a lazy escape hatch for new code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    UserStated,
    Conversation,
    Derived,
    ToolOutput,
    External,
}

impl FactSource {
    /// The channel name as persisted.
    #[must_use]
    pub const fn channel(self) -> SourceChannel {
        match self {
            Self::UserStated => SourceChannel::UserStated,
            Self::Conversation => SourceChannel::Conversation,
            Self::Derived => SourceChannel::Derived,
            Self::ToolOutput => SourceChannel::ToolOutput,
            Self::External => SourceChannel::External,
        }
    }

    /// The trust tier; 1 is highest.
    ///
    /// The mapping is written out once in Rust and once in SQL, with migration
    /// 20260807000006's `facts_trust_tier_matches_channel` as the final
    /// arbiter: drift between the two produces a 23514 outright at insert time
    /// rather than quietly persisting a self-contradictory combination.
    #[must_use]
    pub const fn trust_tier(self) -> i16 {
        match self {
            Self::UserStated => 1,
            // Derived shares a tier with Conversation: both are inferences by
            // the model, differing only in their input.
            // ⚠️ Derived currently does not inherit the tier of its input facts
            // (trust laundering; see the comments in migration
            // 20260807000006) -- that has to wait until derivations are
            // actually used by the read side
            Self::Conversation | Self::Derived => 2,
            Self::ToolOutput => 3,
            Self::External => 4,
        }
    }
}

text_enum! {
    /// `derivations.derived_kind` — the kind on the derived-record side.
    pub enum DerivedKind {
        Fact => "fact",
        Summary => "summary",
    }
}

text_enum! {
    /// `derivations.source_kind` — the kind on the source side.
    ///
    /// One more variant than [`DerivedKind`]: `Episode`. L0 raw text is only
    /// ever a source, never something derived from anything else.
    pub enum SourceKind {
        Episode => "episode",
        Fact => "fact",
        Summary => "summary",
    }
}

text_enum! {
    /// `summaries.scope`
    pub enum SummaryScope {
        Session => "session",
        Topic => "topic",
        Period => "period",
    }
}

text_enum! {
    /// `redactions.target_kind`
    pub enum RedactionTarget {
        Episode => "episode",
        Blob => "blob",
    }
}

text_enum! {
    /// `redactions.mode`
    pub enum RedactionMode {
        /// Clear every derived landing spot inside the database, keeping the
        /// tombstone and the audit chain.
        Redact => "redact",
        /// Everything redact does, plus deleting the blob in object storage and
        /// its backup mirrors.
        Purge => "purge",
    }
}

// ══════════════════════════════════════════════════════════
//  L0 raw layer
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Episode {
    pub id: String,
    pub session_id: String,
    pub role: Role,
    pub content: serde_json::Value,
    pub text: Option<String>,
    pub domain: Option<String>,
    pub device_id: String,
    /// When the event occurred (client clock, after monotonization)
    pub occurred_at: DateTime<Utc>,
    /// When the server persisted it
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEpisode {
    pub id: Id,
    pub session_id: String,
    pub role: Role,
    /// The raw message, including provider-specific opaque thinking / reasoning
    /// blocks, preserved losslessly.
    pub content: serde_json::Value,
    /// The plain text extracted from `content`
    pub text: Option<String>,
    /// A space-separated term string after jieba segmentation, converted to
    /// `tsv` when persisted. `None` leaves `tsv` NULL.
    pub tsv_source: Option<String>,
    pub domain: Option<String>,
    pub device_id: String,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Blob {
    pub hash: String,
    pub mime: String,
    pub size_bytes: i64,
    pub storage_key: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewBlob {
    /// Lowercase hex SHA-256. Content-addressed — identical content yields an
    /// identical hash, so it is idempotent by nature.
    pub hash: String,
    pub mime: String,
    pub size_bytes: i64,
    pub storage_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EpisodeBlob {
    pub episode_id: String,
    pub blob_hash: String,
    pub kind: Option<String>,
    /// The original filename at the time of this reference.
    ///
    /// It belongs to the **reference**, not to the content: under content
    /// addressing the same bytes can carry several filenames (the same image is
    /// "design-draft.png" today and arrives forwarded as "IMG_2043.png" next
    /// week). `None` = unknown (old data, or the client did not supply one).
    pub filename: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewEpisodeBlob {
    pub episode_id: Id,
    pub blob_hash: String,
    pub kind: Option<String>,
    /// See [`EpisodeBlob::filename`]
    pub filename: Option<String>,
}

impl NewEpisodeBlob {
    /// The composite key convention for `sync_log.record_id`:
    /// `episode_id:blob_hash`.
    #[must_use]
    pub fn record_id(&self) -> String {
        format!("{}:{}", self.episode_id, self.blob_hash)
    }
}

/// A vector backfilled for an existing fact when the embedding model changes.
///
/// Not an in-place modification of `facts.embedding` — CLAUDE.md's append-only
/// rule does not allow that path, and "dual-model retrieval during backfill"
/// requires one fact to hold vectors from two spaces **at the same time**, which
/// one column cannot do. See
/// `migrations/20260808000001_embedding_backfill.sql`.
#[derive(Debug, Clone)]
pub struct NewFactEmbedding {
    pub fact_id: Id,
    pub embedding_model: String,
    pub embedding: Vector,
}

impl NewFactEmbedding {
    /// The composite key convention for `sync_log.record_id`:
    /// `fact_id:embedding_model`.
    ///
    /// Putting the model name in record_id is mandatory: the same fact under
    /// different models is a **different row**, and using fact_id alone would
    /// make a second backfill look like a duplicate of the first.
    #[must_use]
    pub fn record_id(&self) -> String {
        format!("{}:{}", self.fact_id, self.embedding_model)
    }
}

/// The same, on the entity side.
#[derive(Debug, Clone)]
pub struct NewEntityEmbedding {
    pub entity_id: Id,
    pub embedding_model: String,
    pub embedding: Vector,
}

impl NewEntityEmbedding {
    /// The composite key convention for `sync_log.record_id`:
    /// `entity_id:embedding_model`.
    #[must_use]
    pub fn record_id(&self) -> String {
        format!("{}:{}", self.entity_id, self.embedding_model)
    }
}

/// One attachment reference **together with its content metadata**.
///
/// [`EpisodeBlob`] is the raw table row and carries only the hash; what a client
/// renders from it is "document · a1b2c3d4". The `mime` and `size_bytes` it
/// actually needs to display live in `blobs`, so the read path uniformly uses
/// this joined struct — rather than making every caller query `blobs` again
/// itself (which is N+1, and would render differently in different places).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EpisodeAttachment {
    pub episode_id: String,
    pub blob_hash: String,
    pub kind: Option<String>,
    pub filename: Option<String>,
    /// Sniffed from the byte header; comes from `blobs.mime`
    pub mime: String,
    pub size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct BlobTranscript {
    pub id: String,
    pub blob_hash: String,
    pub kind: TranscriptKind,
    pub text: String,
    pub embedding: Option<Vector>,
    /// The segment offset in milliseconds for audio/video transcripts; NULL for
    /// images / OCR
    pub span_start_ms: Option<i64>,
    pub span_end_ms: Option<i64>,
    pub transcribed_by: String,
    pub embedding_model: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewBlobTranscript {
    pub id: Id,
    pub blob_hash: String,
    pub kind: TranscriptKind,
    pub text: String,
    pub tsv_source: Option<String>,
    pub embedding: Option<Vector>,
    pub span_start_ms: Option<i64>,
    pub span_end_ms: Option<i64>,
    pub transcribed_by: String,
    pub embedding_model: String,
}

// ══════════════════════════════════════════════════════════
//  L1 fact layer
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Entity {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub summary: Option<String>,
    /// Used only for entity resolution during extraction; takes no part in
    /// four-way retrieval
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEntity {
    pub id: Id,
    /// person / project / file / org / concept / tool … open, not enumerated
    pub kind: String,
    pub name: String,
    pub summary: Option<String>,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EntityMerge {
    pub id: String,
    pub from_entity: String,
    pub into_entity: String,
    pub reason: Option<String>,
    pub source_episode_id: Option<String>,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEntityMerge {
    pub id: Id,
    pub from_entity: Id,
    pub into_entity: Id,
    pub reason: Option<String>,
    pub source_episode_id: Option<Id>,
    pub device_id: String,
}

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Fact {
    pub id: String,
    pub subject_id: String,
    pub predicate: String,
    pub object_text: Option<String>,
    pub object_entity_id: Option<String>,
    pub statement: String,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub domain: Option<String>,
    pub confidence: f32,
    /// [event time] When it started being true. NULL = unknown, or always so
    pub valid_at: Option<DateTime<Utc>>,
    pub source_episode_id: String,
    /// Which channel this fact came in through
    pub source_channel: SourceChannel,
    /// The trust tier of the source; 1 is highest.
    ///
    /// `None` can **only** occur on rows that predate the column
    /// (`unknown_legacy`) — "unknown" is not a point on the trust scale, so it
    /// is NULL rather than some sentinel tier.
    pub trust_tier: Option<i16>,
    pub extracted_by: String,
    pub device_id: String,
    /// [system time] When Cormex learned of this
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewFact {
    pub id: Id,
    pub subject_id: Id,
    /// prefers / decided / owns / blocked_by … open, not enumerated
    pub predicate: String,
    pub object_text: Option<String>,
    pub object_entity_id: Option<Id>,
    pub statement: String,
    pub tsv_source: Option<String>,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub domain: Option<String>,
    /// The extractor's one-time score for "this fact faithfully reflects the
    /// original text"; immutable thereafter
    pub confidence: f32,
    pub valid_at: Option<DateTime<Utc>>,
    pub source_episode_id: Id,
    /// The source channel. Persisting it also emits `trust_tier` (see
    /// [`FactSource::trust_tier`]).
    ///
    /// The absence of a default is deliberate: giving it one would let
    /// "forgetting to say" silently become "the user said it" or "unknown", and
    /// the entire reason this column exists is to be able to pick things out by
    /// source after an incident.
    pub source: FactSource,
    /// Sources **other than** `source_episode_id`.
    ///
    /// Single-source derivation (the extractor: one episode → several facts)
    /// can leave this empty — `source_episode_id NOT NULL` already is its
    /// provenance. What needs this field is **multi-source** derivation:
    /// crosslink's cross-domain edges are inferred from two facts, and
    /// recording only the left one's provenance would let this edge quietly
    /// survive when the right one is redacted (see
    /// `cormex_memory::crosslink`).
    pub derived_from: Vec<ProvenanceRef>,
    pub extracted_by: String,
    pub device_id: String,
}

/// One source reference: the (kind, id) on the source side of the
/// `derivations` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProvenanceRef {
    pub kind: SourceKind,
    pub id: Id,
}

impl ProvenanceRef {
    /// The source is an L0 raw message.
    #[must_use]
    pub const fn episode(id: Id) -> Self {
        Self {
            kind: SourceKind::Episode,
            id,
        }
    }

    /// The source is a fact.
    #[must_use]
    pub const fn fact(id: Id) -> Self {
        Self {
            kind: SourceKind::Fact,
            id,
        }
    }

    /// The source is a summary (provenance can be multi-level: profile block →
    /// summary → episode).
    #[must_use]
    pub const fn summary(id: Id) -> Self {
        Self {
            kind: SourceKind::Summary,
            id,
        }
    }
}

/// The `derivations` table row. The write side gets no separate `New*`: this
/// table has no write-only columns like tsv / embedding, and `id` and
/// `created_at` are filled in by the write method itself.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Derivation {
    pub id: String,
    pub derived_kind: DerivedKind,
    pub derived_id: String,
    pub source_kind: SourceKind,
    pub source_id: String,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct FactEvent {
    pub id: String,
    pub fact_id: String,
    pub op: FactOp,
    pub kind: Option<InvalidationKind>,
    pub invalid_at: Option<DateTime<Utc>>,
    pub superseded_by: Option<String>,
    pub actor: Actor,
    pub reason: Option<String>,
    pub source_episode_id: Option<String>,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewFactEvent {
    pub id: Id,
    pub fact_id: Id,
    pub op: FactOp,
    /// Required when `op = Invalidate`, and must be `None` otherwise (enforced
    /// by a schema CHECK)
    pub kind: Option<InvalidationKind>,
    pub invalid_at: Option<DateTime<Utc>>,
    /// Required when `kind = Superseded`, or the fact's evolution chain breaks
    pub superseded_by: Option<Id>,
    pub actor: Actor,
    pub reason: Option<String>,
    pub source_episode_id: Option<Id>,
    pub device_id: String,
}

impl NewFactEvent {
    /// Automatic invalidation by the system: an old fact superseded by a new
    /// one.
    #[must_use]
    pub fn superseded(
        fact_id: Id,
        superseded_by: Id,
        invalid_at: DateTime<Utc>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            id: Id::new(),
            fact_id,
            op: FactOp::Invalidate,
            kind: Some(InvalidationKind::Superseded),
            invalid_at: Some(invalid_at),
            superseded_by: Some(superseded_by),
            actor: Actor::System,
            reason: None,
            source_episode_id: None,
            device_id: device_id.into(),
        }
    }

    /// The user deletes a memory: taking no position on its truth, merely no
    /// longer using it.
    #[must_use]
    pub fn retracted(
        fact_id: Id,
        invalid_at: DateTime<Utc>,
        reason: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            id: Id::new(),
            fact_id,
            op: FactOp::Invalidate,
            kind: Some(InvalidationKind::Retracted),
            invalid_at: Some(invalid_at),
            superseded_by: None,
            actor: Actor::User,
            reason: Some(reason.into()),
            source_episode_id: None,
            device_id: device_id.into(),
        }
    }

    /// Revoke the previous invalidation (restore). State machine semantics: the
    /// final state is determined by the last status event.
    #[must_use]
    pub fn revoke(fact_id: Id, actor: Actor, device_id: impl Into<String>) -> Self {
        Self {
            id: Id::new(),
            fact_id,
            op: FactOp::Revoke,
            kind: None,
            invalid_at: None,
            superseded_by: None,
            actor,
            reason: None,
            source_episode_id: None,
            device_id: device_id.into(),
        }
    }

    /// Mark it "contradiction awaiting human confirmation". An annotation; it
    /// does not change the status.
    #[must_use]
    pub fn flag(fact_id: Id, reason: impl Into<String>, device_id: impl Into<String>) -> Self {
        Self {
            id: Id::new(),
            fact_id,
            op: FactOp::Flag,
            kind: None,
            invalid_at: None,
            superseded_by: None,
            actor: Actor::System,
            reason: Some(reason.into()),
            source_episode_id: None,
            device_id: device_id.into(),
        }
    }
}

// ══════════════════════════════════════════════════════════
//  L2 summary layer
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct Summary {
    pub id: String,
    pub scope: SummaryScope,
    pub scope_key: String,
    pub text: String,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub covers_from: Option<DateTime<Utc>>,
    pub covers_to: Option<DateTime<Utc>>,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSummary {
    pub id: Id,
    pub scope: SummaryScope,
    pub scope_key: String,
    pub text: String,
    pub tsv_source: Option<String>,
    pub embedding: Option<Vector>,
    pub embedding_model: String,
    pub covers_from: Option<DateTime<Utc>>,
    pub covers_to: Option<DateTime<Utc>>,
    /// What this summary was summarized from. **Must be non-empty**
    /// ([`crate::WriteTxn::insert_summary`] rejects an empty list).
    ///
    /// A summary has no `source_episode_id NOT NULL` column to fall back on the
    /// way a fact does, and without this provenance there is no way whatsoever
    /// to find it once its source has been redacted — while GDPR Art. 17's
    /// erasure obligation extends to derived data. A summary whose source
    /// cannot be traced is a zombie memory nobody can be held accountable for,
    /// so we would rather it never be written at all (see
    /// docs/memory-content.md §5.3).
    pub sources: Vec<ProvenanceRef>,
    pub device_id: String,
}

// ══════════════════════════════════════════════════════════
//  The replay drawer -- "what was injected, what was called"
//  on one conversational turn
//
//  Attribution that lives only in memory is no attribution at all: one refresh
//  and the drawer is empty.
//  For the design trade-offs (storing ids rather than snapshots, anchoring on
//  the user episode) see the table header comments of
//  migrations/20260807000005_episode_replay.sql.
// ══════════════════════════════════════════════════════════

/// The raw `episode_memories` table row (for shipping over sync).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct EpisodeMemory {
    pub id: String,
    pub episode_id: String,
    pub fact_id: String,
    pub ordinal: i32,
    pub channels: Option<Vec<String>>,
    pub score: Option<f64>,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEpisodeMemory {
    pub id: Id,
    /// This turn's anchor — **the user's episode** (the assistant's one is not
    /// persisted when something goes wrong)
    pub episode_id: Id,
    pub fact_id: Id,
    /// Injection order, starting at 0
    pub ordinal: i32,
    /// The retrieval channels that hit (bm25 / vector / graph / episode …)
    pub channels: Vec<String>,
    pub score: Option<f64>,
    pub device_id: String,
}

/// What an injected memory looks like **right now**, when replayed.
///
/// Not a raw table row: `statement` and the invalidation status come from a
/// JOIN against `facts` / `fact_status`. Ids are stored and the present state
/// is rendered — so once a fact is redacted the drawer follows suit and shows a
/// placeholder, with no need to sweep this table too.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct InjectedMemory {
    pub episode_id: String,
    pub fact_id: String,
    pub ordinal: i32,
    pub channels: Option<Vec<String>>,
    pub score: Option<f64>,
    /// `None` = the row for this fact is no longer there. In theory impossible
    /// (append-only, plus a foreign key); seeing it means somebody bypassed
    /// this layer and deleted data — the UI should display "references a
    /// memory that is no longer visible" rather than hiding the whole row
    pub statement: Option<String>,
    pub domain: Option<String>,
    pub source_episode_id: Option<String>,
    /// This fact **has been invalidated** since it was injected. Replay must
    /// say so honestly: "the thing this was based on no longer holds" is
    /// exactly the information an audit most wants to see
    pub invalidated: bool,
}

/// The `episode_tool_calls` table row. The write side and the read side share
/// one shape, so unlike other tables there is no separate `New*` — it has no
/// write-only columns like tsv / embedding.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct EpisodeToolCall {
    pub id: String,
    pub episode_id: String,
    pub ordinal: i32,
    pub name: String,
    /// The file path this call touched; `None` for tools that touch no file
    pub path: Option<String>,
    pub summary: String,
    pub ok: bool,
    pub device_id: String,
    /// What this write changed (unified diff, truncated). `None` = there is no
    /// change worth looking at.
    pub diff: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewEpisodeToolCall {
    pub id: Id,
    pub episode_id: Id,
    pub ordinal: i32,
    pub name: String,
    pub path: Option<String>,
    pub summary: String,
    pub ok: bool,
    pub device_id: String,
    /// What this write changed (unified diff, **truncated**).
    ///
    /// The truncation is already done on the agent side
    /// (`cormex_agent::diff`), where it can also append a "rest not shown"
    /// note. This column ships to every device via `sync_log`, so the migration
    /// carries an 8 KiB CHECK as a backstop as well.
    pub diff: Option<String>,
}

/// The character cap on `episode_tool_calls.summary`, matching the CHECK in the
/// migration.
///
/// The constant lives here rather than leaving layers above to write a literal:
/// the symptom of drift between the two is "as soon as a tool summary gets long,
/// the whole write transaction rolls back" — and that transaction also carries
/// this turn's memory attribution.
pub const TOOL_SUMMARY_MAX_CHARS: usize = 2048;

/// The character cap on `episode_tool_calls.diff`, matching the CHECK in the
/// migration.
///
/// An order of magnitude larger than summary: a diff starts at dozens of lines
/// to begin with. But there still has to be a cap — this column **ships to
/// every device** via `sync_log`, a few-hundred-line refactor easily runs to
/// tens of KB, and multiplied by the device count and by several `write_file`
/// calls per turn, that is traffic on the sync channel nobody ever budgeted
/// for.
///
/// This cap and the one over in `cormex_agent::diff` are **each their own
/// business**: that one governs "a human can finish reading it", this one
/// governs "persistence and sync can bear it". The two numbers may well move
/// independently in future, so they do not share a constant.
pub const TOOL_DIFF_MAX_CHARS: usize = 8192;

/// The character cap on `episode_tool_calls.name`, matching the CHECK in the
/// migration.
pub const TOOL_NAME_MAX_CHARS: usize = 128;

/// The path cap shared by `episode_tool_calls.path` and
/// `session_events.workspace`.
pub const TOOL_PATH_MAX_CHARS: usize = 4096;

/// The character cap on `episode_blobs.filename`, matching the CHECK in the
/// migration.
pub const ATTACHMENT_FILENAME_MAX_CHARS: usize = 255;

// ══════════════════════════════════════════════════════════
//  Session lifecycle
// ══════════════════════════════════════════════════════════

/// The character cap on `session_events.title`, matching the CHECK in the
/// migration.
///
/// The constant lives here rather than leaving each layer above to write its
/// own literal: once the two drift, the symptom is "the client thinks the
/// rename succeeded but the server returns 500" — and the database CHECK's
/// error never reaches the user.
pub const SESSION_TITLE_MAX_CHARS: usize = 200;

/// The character cap on `session_events.workspace`, matching the CHECK in the
/// migration.
///
/// **Defined in [`cormex_core`]**, because the validator that enforces it lives
/// on the agent side (in the repository that drives this service), and an agent
/// should not have to depend on the database layer just to learn a number.
/// The re-export here keeps
/// `cormex_store::WORKSPACE_PATH_MAX_CHARS` usable — keeping it aligned with the
/// migration's CHECK remains the storage layer's responsibility.
pub use cormex_core::WORKSPACE_PATH_MAX_CHARS;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct SessionEvent {
    pub id: String,
    pub session_id: String,
    pub op: SessionOp,
    /// Non-null when `op = Rename`
    pub title: Option<String>,
    /// Non-null when `op = BindWorkspace`. **An absolute path on the local
    /// machine** — once synced to another device it may not exist at all, and a
    /// client should treat "does not exist" as unbound.
    pub workspace: Option<String>,
    /// Non-null when `op = MoveToProject`. The project it points at **may
    /// already have been deleted** — deleting a project deliberately does not
    /// cascade a rewrite of session events, and the final-state view treats a
    /// dangling binding as ungrouped.
    pub project_id: Option<String>,
    /// Non-null when `op = SetRuntime`.
    pub runtime: Option<SessionRuntime>,
    pub actor: Actor,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewSessionEvent {
    pub id: Id,
    pub session_id: String,
    pub op: SessionOp,
    /// Required when `op = Rename`, and must be `None` otherwise (enforced by a
    /// schema CHECK)
    pub title: Option<String>,
    /// Required when `op = BindWorkspace`, and must be `None` otherwise
    /// (enforced by a schema CHECK)
    pub workspace: Option<String>,
    /// Required when `op = MoveToProject`, and must be `None` otherwise
    /// (enforced by a schema CHECK)
    pub project_id: Option<String>,
    /// Required when `op = SetRuntime`, and must be `None` otherwise (enforced
    /// by a schema CHECK)
    pub runtime: Option<SessionRuntime>,
    pub actor: Actor,
    pub device_id: String,
}

impl NewSessionEvent {
    fn bare(session_id: &str, op: SessionOp, actor: Actor, device_id: &str) -> Self {
        Self {
            id: Id::new(),
            session_id: session_id.to_owned(),
            op,
            title: None,
            workspace: None,
            project_id: None,
            runtime: None,
            actor,
            device_id: device_id.to_owned(),
        }
    }

    /// The user gave the session a name. The final state is determined by the
    /// last `rename` — there is no "undo stack", so after
    /// `[rename A, rename B]` the answer is B.
    #[must_use]
    pub fn rename(session_id: &str, title: &str, actor: Actor, device_id: &str) -> Self {
        Self {
            title: Some(title.to_owned()),
            ..Self::bare(session_id, SessionOp::Rename, actor, device_id)
        }
    }

    /// Archive — hide it from the default list, **deleting nothing**.
    ///
    /// What a user calls "deleting a session" is this operation in an
    /// append-only system: the messages, the attachments, and the extracted
    /// facts all stay exactly as they were, they simply stop appearing in the
    /// list. Actually destroying content requires `redactions`' redact / purge,
    /// which has different semantics and requires a second confirmation
    /// (docs/memory.md §11).
    #[must_use]
    pub fn archive(session_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(session_id, SessionOp::Archive, actor, device_id)
    }

    /// Undo the archive.
    #[must_use]
    pub fn unarchive(session_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(session_id, SessionOp::Unarchive, actor, device_id)
    }

    /// Bind the session to a local directory.
    ///
    /// `workspace` must be an absolute path that the **server has already
    /// validated** (it exists, it is a directory, it is not a system
    /// directory). The storage layer does not validate: it has no concept of
    /// "what counts as a system directory", which is knowledge about the
    /// deployment shape and belongs to cormexd.
    #[must_use]
    pub fn bind_workspace(
        session_id: &str,
        workspace: &str,
        actor: Actor,
        device_id: &str,
    ) -> Self {
        Self {
            workspace: Some(workspace.to_owned()),
            ..Self::bare(session_id, SessionOp::BindWorkspace, actor, device_id)
        }
    }

    /// Unbind the workspace — the session falls back to pure chat, and the file
    /// tools disappear from the model's tool catalog.
    #[must_use]
    pub fn unbind_workspace(session_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(session_id, SessionOp::UnbindWorkspace, actor, device_id)
    }

    /// Declare where this session runs. See [`SessionRuntime`].
    ///
    /// **A different thing from `bind_workspace`**, even though the two often
    /// happen together: that one says "which directory", this one says "which
    /// side's execution environment". A session bound to a local directory is
    /// necessarily `Local`, but the converse does not hold — a `Local` session
    /// may not have a directory bound yet (the user just created it on this
    /// machine and has not picked one).
    #[must_use]
    pub fn set_runtime(
        session_id: &str,
        runtime: SessionRuntime,
        actor: Actor,
        device_id: &str,
    ) -> Self {
        Self {
            runtime: Some(runtime),
            ..Self::bare(session_id, SessionOp::SetRuntime, actor, device_id)
        }
    }

    /// Move the session into a project. Moving it again switches projects — a
    /// session belongs to at most one project at a time.
    ///
    /// The storage layer **does not check whether the project exists**: that
    /// would require a read, while the write transaction holds the advisory
    /// lock and the discipline requires it to be short and write-only (see
    /// [`crate::txn`]). The check belongs to cormexd and happens before
    /// entering the transaction. Even a skipped check does not corrupt the
    /// view: `session_state` treats a binding pointing at a deleted project as
    /// ungrouped.
    #[must_use]
    pub fn move_to_project(
        session_id: &str,
        project_id: &str,
        actor: Actor,
        device_id: &str,
    ) -> Self {
        Self {
            project_id: Some(project_id.to_owned()),
            ..Self::bare(session_id, SessionOp::MoveToProject, actor, device_id)
        }
    }

    /// Move the session out of its project, back to ungrouped.
    ///
    /// Not the same thing as "deleting a project": this touches only this one
    /// session, whereas deleting a project dissolves the whole grouping.
    #[must_use]
    pub fn remove_from_project(session_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(session_id, SessionOp::RemoveFromProject, actor, device_id)
    }
}

// ══════════════════════════════════════════════════════════
//  Project lifecycle
// ══════════════════════════════════════════════════════════

/// The character cap on `project_events.name`, matching the CHECK in the
/// migration.
///
/// The constant lives here rather than leaving layers above to write a literal:
/// the symptom of drift between the two is "the client thinks the rename
/// succeeded but the server returns 500" — and the database CHECK's error never
/// reaches the user.
pub const PROJECT_NAME_MAX_CHARS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct ProjectEvent {
    pub id: String,
    pub project_id: String,
    pub op: ProjectOp,
    /// Non-null when `op = Create | Rename`
    pub name: Option<String>,
    pub actor: Actor,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewProjectEvent {
    pub id: Id,
    pub project_id: String,
    pub op: ProjectOp,
    /// Required when `op = Create | Rename`; must be `None` for `Delete`
    /// (enforced by a schema CHECK)
    pub name: Option<String>,
    pub actor: Actor,
    pub device_id: String,
}

impl NewProjectEvent {
    fn bare(project_id: &str, op: ProjectOp, actor: Actor, device_id: &str) -> Self {
        Self {
            id: Id::new(),
            project_id: project_id.to_owned(),
            op,
            name: None,
            actor,
            device_id: device_id.to_owned(),
        }
    }

    /// Create a project. `project_id` is generated by the caller (usually a
    /// fresh ULID).
    ///
    /// **Creating the same id again is "undo the deletion"**, not an error: the
    /// final state is determined by the last lifecycle event, so after
    /// recreating it the original sessions' membership comes back exactly as it
    /// was — which is precisely what deliberately not cascading a rewrite of
    /// session events on project deletion buys.
    #[must_use]
    pub fn create(project_id: &str, name: &str, actor: Actor, device_id: &str) -> Self {
        Self {
            name: Some(name.to_owned()),
            ..Self::bare(project_id, ProjectOp::Create, actor, device_id)
        }
    }

    /// Rename. The final state is determined by the last `rename` (or
    /// `create`); there is no undo stack.
    #[must_use]
    pub fn rename(project_id: &str, name: &str, actor: Actor, device_id: &str) -> Self {
        Self {
            name: Some(name.to_owned()),
            ..Self::bare(project_id, ProjectOp::Rename, actor, device_id)
        }
    }

    /// Delete a project — **dissolve the grouping, leave the content alone**.
    ///
    /// The sessions inside become ungrouped, and not one message, attachment,
    /// or extracted fact is lost. Actually destroying content requires
    /// `redactions`' redact / purge, which is a different path and requires a
    /// second confirmation (docs/memory.md §11).
    #[must_use]
    pub fn delete(project_id: &str, actor: Actor, device_id: &str) -> Self {
        Self::bare(project_id, ProjectOp::Delete, actor, device_id)
    }
}

// ══════════════════════════════════════════════════════════
//  Redaction records
// ══════════════════════════════════════════════════════════

#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Redaction {
    pub id: String,
    pub target_kind: RedactionTarget,
    /// A ULID for an episode, a SHA-256 for a blob
    pub target_id: String,
    pub mode: RedactionMode,
    pub reason: String,
    pub actor: String,
    pub device_id: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewRedaction {
    pub id: Id,
    pub target_kind: RedactionTarget,
    pub target_id: String,
    pub mode: RedactionMode,
    /// Required by the schema — a redaction must leave a reason behind
    pub reason: String,
    pub actor: String,
    pub device_id: String,
}

// ══════════════════════════════════════════════════════════
//  Views
// ══════════════════════════════════════════════════════════

/// The `fact_status` view: a fact's most recent **status** event (`flag` takes
/// no part).
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct FactStatus {
    pub fact_id: String,
    pub op: FactOp,
    pub kind: Option<InvalidationKind>,
    pub invalid_at: Option<DateTime<Utc>>,
    pub superseded_by: Option<String>,
    pub actor: Actor,
    pub decided_at: DateTime<Utc>,
}

/// The `session_state` view: a session's final state along each of its four
/// dimensions.
///
/// The four dimensions take their own last event **independently of each
/// other** — after `[archive, rename]` the session both has a new title and is
/// still archived. Deciding by "the last row in the whole table" would produce
/// behavior nobody asked for, like "renaming it automatically unarchives it".
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct SessionState {
    pub session_id: String,
    /// The title the user set. `None` = never renamed, and the layer above
    /// should fall back to "derived from the first user message".
    pub title: Option<String>,
    pub archived: bool,
    /// The bound local directory. `None` = unbound, and the session is pure
    /// chat (no file tools).
    pub workspace: Option<String>,
    /// The project it belongs to. `None` = ungrouped.
    ///
    /// **A binding pointing at a deleted project has already become `None` in
    /// the view** — deleting a project does not cascade a rewrite of session
    /// events, dangling bindings are routine, and the decision is consolidated
    /// into a single place in SQL.
    pub project_id: Option<String>,
    /// Where this session runs. **The view has already folded NULL into
    /// `Cloud`** — the default is pinned down in one place in SQL rather than
    /// leaving every caller to decide separately what "no event" means.
    pub runtime: SessionRuntime,
    /// The time of the most recent event across the five dimensions
    pub decided_at: DateTime<Utc>,
}

/// The `project_state` view + a session count: one row of the project list.
///
/// `session_count` is not part of the view (the view only answers "what is this
/// project currently called"); it is aggregated in passing by the list query —
/// see [`crate::Store::projects`].
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct Project {
    pub project_id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    /// How many sessions under this project **would appear in the list**. For
    /// what exactly that counts, see the docs on [`crate::Store::projects`].
    pub session_count: i64,
}

/// The `canonical_entities` view: an entity's final owner after alias merges.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct CanonicalEntity {
    pub id: String,
    pub canonical: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_enum_roundtrips_through_str() {
        for op in [FactOp::Invalidate, FactOp::Revoke, FactOp::Flag] {
            assert_eq!(
                op.as_str().parse::<FactOp>().expect("should parse back"),
                op
            );
        }
    }

    #[test]
    fn unknown_variant_is_rejected() {
        let err = "nope"
            .parse::<Role>()
            .expect_err("an unknown value should error");
        assert!(matches!(err, crate::StoreError::UnknownVariant { .. }));
    }

    #[test]
    fn session_event_constructors_respect_the_schema_checks() {
        // The schema's CHECK cuts both ways: rename must have a title, and
        // **only** rename may have one. A mistake in a constructor would show
        // up in the integration tests as a bare SQLSTATE 23514, so this unit
        // test pins the intent down first
        let r = NewSessionEvent::rename("s", "新标题", Actor::User, "dev");
        assert_eq!(r.title.as_deref(), Some("新标题"));
        assert!(r.workspace.is_none(), "rename must not carry a workspace");

        let b = NewSessionEvent::bind_workspace("s", "/tmp/x", Actor::User, "dev");
        assert_eq!(b.workspace.as_deref(), Some("/tmp/x"));
        assert!(b.title.is_none(), "bind_workspace must not carry a title");

        let m = NewSessionEvent::move_to_project("s", "proj", Actor::User, "dev");
        assert_eq!(m.project_id.as_deref(), Some("proj"));
        assert!(
            m.title.is_none() && m.workspace.is_none(),
            "move_to_project must not carry a title / workspace"
        );

        for e in [
            NewSessionEvent::archive("s", Actor::User, "dev"),
            NewSessionEvent::unarchive("s", Actor::User, "dev"),
            NewSessionEvent::unbind_workspace("s", Actor::User, "dev"),
            NewSessionEvent::remove_from_project("s", Actor::User, "dev"),
        ] {
            assert!(
                e.title.is_none() && e.workspace.is_none() && e.project_id.is_none(),
                "{} is a pure status event and must not carry any payload",
                e.op
            );
        }
    }

    #[test]
    fn project_event_constructors_respect_the_schema_checks() {
        // The same two-way CHECK as session_events: create / rename must have a
        // name, and **only** they may have one. A mistake in a constructor
        // would show up in the integration tests as a bare SQLSTATE 23514, so
        // this unit test pins the intent down first
        for e in [
            NewProjectEvent::create("p", "新项目", Actor::User, "dev"),
            NewProjectEvent::rename("p", "改过的名", Actor::User, "dev"),
        ] {
            assert!(e.name.is_some(), "{} must carry a name", e.op);
        }

        let d = NewProjectEvent::delete("p", Actor::User, "dev");
        assert!(
            d.name.is_none(),
            "delete is a pure status event; carrying a name would make the view behave as though \
             deleting it also changed its name"
        );
    }

    #[test]
    fn episode_blob_record_id_uses_colon() {
        let link = NewEpisodeBlob {
            episode_id: "01ARZ3NDEKTSV4RRFFQ69G5FAV".parse().expect("a valid ULID"),
            blob_hash: "a".repeat(64),
            kind: None,
            filename: None,
        };
        assert_eq!(
            link.record_id(),
            format!("01ARZ3NDEKTSV4RRFFQ69G5FAV:{}", "a".repeat(64))
        );
    }
}

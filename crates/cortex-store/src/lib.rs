//! Cortex 存储层：sqlx repository 与 sync_log 写入器。
//!
//! # 边界
//!
//! **全部 SQL 收敛在本 crate。** 上层拿到的是类型安全的方法，不是查询串。
//! 这不是洁癖 —— 它是将来换驱动、换库、加缓存层时唯一的逃生门。
//!
//! 刻意不用 sqlx 的编译期校验宏（`query!` / `query_as!`）：那需要编译时能连
//! 数据库或维护 `.sqlx` 离线缓存，给 CI 与新人上手都添麻烦。这里一律用运行时
//! API（[`sqlx::query_as`] + [`sqlx::FromRow`]），代价是列名写错要到跑测试时
//! 才发现 —— 集成测试真连数据库覆盖了这一点。
//!
//! # 写入纪律
//!
//! 同步正确性的根基。schema 与 docs/memory.md §四、§九 都写死了三条：
//!
//! 1. 业务行与 `sync_log` 追加必须**同事务**；
//! 2. 事务开头执行 `pg_advisory_xact_lock(4272)`，把提交顺序串行化，
//!    使 `sync_log.seq` 的顺序等于对读端的可见顺序；
//! 3. 取号事务必须短小、纯写。
//!
//! 本 crate 把这三条做成了**结构性约束**而非约定：唯一的写入口是
//! [`Store::write_txn`]，它交出的 [`WriteTxn`] 只有写方法，且每个写方法内部
//! 都经由同一个私有通道落库 —— 那个通道把「写业务行」与「追加 sync_log」
//! 绑成不可分割的一步。详见 [`txn`] 模块文档。
//!
//! # 用法
//!
//! ```no_run
//! use cortex_store::{NewEntity, NewFact, Store};
//!
//! # async fn demo(entity: NewEntity, fact: NewFact) -> cortex_store::Result<()> {
//! let store = Store::connect("postgres://cortex@localhost:5442/cortex").await?;
//!
//! // 一个事务里写多行，sync_log 自动跟上
//! store
//!     .write_txn(async |tx| {
//!         tx.insert_entity(&entity).await?;
//!         tx.insert_fact(&fact).await
//!     })
//!     .await?;
//!
//! // 客户端增量拉取
//! let batch = store.fetch_since(0, 100).await?;
//! # let _ = batch;
//! # Ok(())
//! # }
//! ```

mod bootstrap;
mod error;
mod model;
mod query;
mod recall;
mod store;
mod sync;
mod tenant;
mod txn;

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

/// 向量维度，对应 `bge-m3`。换模型见 docs/memory.md §七。
pub const EMBEDDING_DIM: usize = 1024;

pub use pgvector::Vector;

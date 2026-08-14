//! The read side of sync: what sits under `GET /sync?since=`, plus its
//! real-time event source.
//!
//! A client holds **one** cursor (`sync_log.seq`) spanning every table. The
//! total order comes from `sync_log`, and replaying in log order naturally
//! satisfies FK ordering (facts always arrive after their source episode).
//!
//! The advisory lock on the write side (see [`crate::txn`]) guarantees seq
//! order == visibility order, which is what makes "having pulled up to seq=N,
//! advance the cursor to N" safe — no row with seq < N can become visible after
//! that point.
//!
//! Besides pulling there is also a push path ([`Store::listen_sync`]): an
//! insert into `sync_log` fires a Postgres `NOTIFY`, from which the server fans
//! out the **signal** "the cursor moved" to every client. It shares the same
//! cursor semantics as pulling; it is not a second protocol.

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgListener, PgPool, PgPoolOptions};

use crate::error::{Result, StoreError};
use crate::model::{
    Blob, BlobTranscript, Derivation, Entity, EntityMerge, Episode, EpisodeBlob, EpisodeMemory,
    EpisodeToolCall, Fact, FactEvent, ProjectEvent, Redaction, SessionEvent, Summary, fact_columns,
    table,
};
use crate::store::Store;

/// One sync record: a `sync_log` row plus the business row it points at.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncRecord {
    pub seq: i64,
    pub table_name: String,
    pub record_id: String,
    pub logged_at: DateTime<Utc>,
    /// The business row. In theory never `None` — the log row and the business
    /// row commit in the same transaction, so the log being visible means the
    /// business row is visible. `None` means somebody bypassed this layer and
    /// deleted data.
    pub payload: Option<SyncPayload>,
}

/// The typed payload of a business row. The variants map one-to-one onto
/// `sync_log.table_name`.
#[derive(Debug, Clone, PartialEq)]
pub enum SyncPayload {
    Episode(Episode),
    Blob(Blob),
    EpisodeBlob(EpisodeBlob),
    EpisodeMemory(EpisodeMemory),
    EpisodeToolCall(EpisodeToolCall),
    BlobTranscript(BlobTranscript),
    Entity(Entity),
    EntityMerge(EntityMerge),
    Fact(Fact),
    FactEvent(FactEvent),
    Summary(Summary),
    Derivation(Derivation),
    Redaction(Redaction),
    SessionEvent(SessionEvent),
    ProjectEvent(ProjectEvent),
}

impl SyncPayload {
    /// The table name this payload belongs to.
    #[must_use]
    pub const fn table_name(&self) -> &'static str {
        match self {
            Self::Episode(_) => table::EPISODES,
            Self::Blob(_) => table::BLOBS,
            Self::EpisodeBlob(_) => table::EPISODE_BLOBS,
            Self::EpisodeMemory(_) => table::EPISODE_MEMORIES,
            Self::EpisodeToolCall(_) => table::EPISODE_TOOL_CALLS,
            Self::BlobTranscript(_) => table::BLOB_TRANSCRIPTS,
            Self::Entity(_) => table::ENTITIES,
            Self::EntityMerge(_) => table::ENTITY_MERGES,
            Self::Fact(_) => table::FACTS,
            Self::FactEvent(_) => table::FACT_EVENTS,
            Self::Summary(_) => table::SUMMARIES,
            Self::Derivation(_) => table::DERIVATIONS,
            Self::Redaction(_) => table::REDACTIONS,
            Self::SessionEvent(_) => table::SESSION_EVENTS,
            Self::ProjectEvent(_) => table::PROJECT_EVENTS,
        }
    }
}

/// A raw `sync_log` row.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SyncLogEntry {
    pub seq: i64,
    pub table_name: String,
    pub record_id: String,
    pub created_at: DateTime<Utc>,
}

impl Store {
    /// The far end of the current sync total order. A new device pulls from 0;
    /// an existing one uses its own cursor.
    pub async fn latest_seq(&self) -> Result<i64> {
        let (seq,): (i64,) = sqlx::query_as("SELECT coalesce(max(seq), 0) FROM sync_log")
            .fetch_one(self.pool())
            .await?;
        Ok(seq)
    }

    /// Fetch only the log rows, not the business rows. For lightweight probes
    /// along the lines of "let me check whether there is anything new".
    pub async fn sync_log_since(&self, cursor: i64, limit: i64) -> Result<Vec<SyncLogEntry>> {
        let rows = sqlx::query_as::<_, SyncLogEntry>(
            "SELECT seq, table_name, record_id, created_at
               FROM sync_log WHERE seq > $1 ORDER BY seq ASC LIMIT $2",
        )
        .bind(cursor)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Incremental pull: records with `seq > cursor` and their business rows,
    /// ascending by seq.
    ///
    /// Implemented as "one fetch of the log + one fetch of business rows per
    /// table involved" rather than N+1 row by row.
    pub async fn fetch_since(&self, cursor: i64, limit: i64) -> Result<Vec<SyncRecord>> {
        let entries = self.sync_log_since(cursor, limit).await?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        // Group the primary keys we need by table
        let mut wanted: HashMap<String, Vec<String>> = HashMap::new();
        for entry in &entries {
            wanted
                .entry(entry.table_name.clone())
                .or_default()
                .push(entry.record_id.clone());
        }

        // (table_name, record_id) → the business row
        let mut payloads: HashMap<(String, String), SyncPayload> = HashMap::new();
        for (table_name, ids) in &wanted {
            self.load_payloads(table_name, ids, &mut payloads).await?;
        }

        Ok(entries
            .into_iter()
            .map(|entry| {
                let payload = payloads
                    .get(&(entry.table_name.clone(), entry.record_id.clone()))
                    .cloned();
                SyncRecord {
                    seq: entry.seq,
                    table_name: entry.table_name,
                    record_id: entry.record_id,
                    logged_at: entry.created_at,
                    payload,
                }
            })
            .collect())
    }

    /// Bulk-fetch the business rows of one table into `out`.
    async fn load_payloads(
        &self,
        table_name: &str,
        ids: &[String],
        out: &mut HashMap<(String, String), SyncPayload>,
    ) -> Result<()> {
        // `$sql` takes an `expr` rather than a `literal`: the facts arm builds
        // its column list with `concat!`, and `concat!(...)` is still an
        // expression before expansion, so it does not match `literal`
        macro_rules! collect {
            ($sql:expr, $ty:ty, $key:expr, $wrap:expr) => {{
                let rows = sqlx::query_as::<_, $ty>($sql)
                    .bind(ids)
                    .fetch_all(self.pool())
                    .await?;
                for row in rows {
                    #[allow(clippy::redundant_closure_call)]
                    let key = ($key)(&row);
                    #[allow(clippy::redundant_closure_call)]
                    out.insert((table_name.to_owned(), key), ($wrap)(row));
                }
            }};
        }

        match table_name {
            table::EPISODES => collect!(
                "SELECT id, session_id, role, content, text, domain, device_id,
                        occurred_at, created_at
                   FROM episodes WHERE id = ANY($1)",
                Episode,
                |row: &Episode| row.id.clone(),
                SyncPayload::Episode
            ),
            table::BLOBS => collect!(
                "SELECT hash, mime, size_bytes, storage_key, created_at
                   FROM blobs WHERE hash = ANY($1)",
                Blob,
                |row: &Blob| row.hash.clone(),
                SyncPayload::Blob
            ),
            table::EPISODE_BLOBS => collect!(
                "SELECT episode_id, blob_hash, kind, filename FROM episode_blobs
                   WHERE episode_id || ':' || blob_hash = ANY($1)",
                EpisodeBlob,
                |row: &EpisodeBlob| format!("{}:{}", row.episode_id, row.blob_hash),
                SyncPayload::EpisodeBlob
            ),
            // The two tables of the replay drawer. Missing either arm has the
            // same consequence as the session_events comment describes: a
            // single log row makes the whole /sync batch fail, and the client's
            // cursor is stuck in front of it from then on
            table::EPISODE_MEMORIES => collect!(
                "SELECT id, episode_id, fact_id, ordinal, channels, score, device_id, created_at
                   FROM episode_memories WHERE id = ANY($1)",
                EpisodeMemory,
                |row: &EpisodeMemory| row.id.clone(),
                SyncPayload::EpisodeMemory
            ),
            table::EPISODE_TOOL_CALLS => collect!(
                "SELECT id, episode_id, ordinal, name, path, summary, ok, device_id, diff,
                        created_at
                   FROM episode_tool_calls WHERE id = ANY($1)",
                EpisodeToolCall,
                |row: &EpisodeToolCall| row.id.clone(),
                SyncPayload::EpisodeToolCall
            ),
            table::BLOB_TRANSCRIPTS => collect!(
                "SELECT id, blob_hash, kind, text, embedding, span_start_ms, span_end_ms,
                        transcribed_by, embedding_model, created_at
                   FROM blob_transcripts WHERE id = ANY($1)",
                BlobTranscript,
                |row: &BlobTranscript| row.id.clone(),
                SyncPayload::BlobTranscript
            ),
            table::ENTITIES => collect!(
                "SELECT id, kind, name, summary, embedding, embedding_model, device_id, created_at
                   FROM entities WHERE id = ANY($1)",
                Entity,
                |row: &Entity| row.id.clone(),
                SyncPayload::Entity
            ),
            table::ENTITY_MERGES => collect!(
                "SELECT id, from_entity, into_entity, reason, source_episode_id,
                        device_id, created_at
                   FROM entity_merges WHERE id = ANY($1)",
                EntityMerge,
                |row: &EntityMerge| row.id.clone(),
                SyncPayload::EntityMerge
            ),
            table::FACTS => collect!(
                concat!(
                    "SELECT ",
                    fact_columns!(),
                    "  FROM facts WHERE id = ANY($1)"
                ),
                Fact,
                |row: &Fact| row.id.clone(),
                SyncPayload::Fact
            ),
            table::FACT_EVENTS => collect!(
                "SELECT id, fact_id, op, kind, invalid_at, superseded_by, actor, reason,
                        source_episode_id, device_id, created_at
                   FROM fact_events WHERE id = ANY($1)",
                FactEvent,
                |row: &FactEvent| row.id.clone(),
                SyncPayload::FactEvent
            ),
            table::SUMMARIES => collect!(
                "SELECT id, scope, scope_key, text, embedding, embedding_model,
                        covers_from, covers_to, device_id, created_at
                   FROM summaries WHERE id = ANY($1)",
                Summary,
                |row: &Summary| row.id.clone(),
                SyncPayload::Summary
            ),
            // Derivation provenance. The client does the same thing with it as
            // the server: on receiving a redaction tombstone, follow the edges
            // to find the local summaries / graph edges that must be
            // invalidated along with it (the propagation obligation of
            // memory.md §9). Without shipping it, a client can clear the source
            // but not the records derived from it
            table::DERIVATIONS => collect!(
                "SELECT id, derived_kind, derived_id, source_kind, source_id,
                        device_id, created_at
                   FROM derivations WHERE id = ANY($1)",
                Derivation,
                |row: &Derivation| row.id.clone(),
                SyncPayload::Derivation
            ),
            table::REDACTIONS => collect!(
                "SELECT id, target_kind, target_id, mode, reason, actor, device_id, created_at
                   FROM redactions WHERE id = ANY($1)",
                Redaction,
                |row: &Redaction| row.id.clone(),
                SyncPayload::Redaction
            ),
            // Missing this arm does not mean "session renames fail to sync", it
            // means **the whole of /sync breaks**: a single session_events log
            // row makes load_payloads return UnknownTable, that entire batch
            // fails, and the client's cursor is stuck in front of it from then
            // on
            table::SESSION_EVENTS => collect!(
                "SELECT id, session_id, op, title, workspace, project_id, runtime,
                        actor, device_id, created_at
                   FROM session_events WHERE id = ANY($1)",
                SessionEvent,
                |row: &SessionEvent| row.id.clone(),
                SyncPayload::SessionEvent
            ),
            // Project events likewise. The client runs the same state machine
            // over them to compute the final state itself — the server never
            // ships "the current project list", because such a snapshot has no
            // total order to speak of
            table::PROJECT_EVENTS => collect!(
                "SELECT id, project_id, op, name, actor, device_id, created_at
                   FROM project_events WHERE id = ANY($1)",
                ProjectEvent,
                |row: &ProjectEvent| row.id.clone(),
                SyncPayload::ProjectEvent
            ),
            other => return Err(StoreError::UnknownTable(other.to_owned())),
        }

        Ok(())
    }
}

// ══════════════════════════════════════════════════════════
//  Real-time event source: LISTEN / NOTIFY hung off sync_log
// ══════════════════════════════════════════════════════════
//
// This is a free side benefit of the sync_log outbox design (docs/memory.md
// §9): the write side already guarantees that "every business write
// transaction appends a row to sync_log" and that "seq order == visibility
// order", so listening to inserts on that one table is equivalent to listening
// to **every** data change, with no table missed.
//
// # Why a database trigger rather than calling pg_notify from the application
// after writing
//
// Both work; the trade-off is "who guarantees nothing is missed":
//
// - **Application-level notify** (a manual `SELECT pg_notify` after
//   `write_txn`) requires **every write path** to remember to call it. The
//   conversation path is easy enough, but fact extraction runs inside
//   `cortex-memory::extract`, and the future summarizer and redact executor
//   each write to the database of their own accord — every new writer adds
//   another chance to miss it, and missing it shows up as "one device
//   occasionally doesn't refresh", which is nearly impossible to reproduce in a
//   test.
// - **A trigger** hangs off `sync_log`, the choke point every write must pass
//   through. The write discipline already uses the type system to enforce "a
//   business row must be in the same transaction as its sync_log row" (see
//   [`crate::txn`]), so the trigger naturally covers every writer, including
//   the ones not written yet.
//
// There is one more benefit, less obvious but decisive: Postgres delivers
// NOTIFY **only at transaction commit**. The trigger fires inside the write
// transaction and commits along with it, so "the notification arrived" is
// strictly later than "the data became visible". An application-level notify
// placed outside the transaction produces the race where "the notification
// arrived but the pull cannot see that row".
//
// # Why this DDL does not go into a migration
//
// Migrations are the authoritative version of the schema and are append-only;
// this DDL is part of a **runtime component**: idempotent, safe to re-run, and
// requiring no reverse migration when the implementation changes.

/// The **prefix** of the channel name. The actual channel name is
/// `cortex_sync_<schema>`, assembled by
/// [`crate::SchemaName::notify_channel`].
///
/// Changing this changes a server-internal convention; clients do not need to
/// know it (a client only connects to `/ws`).
///
/// # Why the schema has to be part of it
///
/// A channel name is scoped to the **database**, not to a schema. In the
/// single-user case this only meant "two Cortex instances in the same database
/// receive each other's notifications", which is harmless — the signal only
/// says "go pull once", and the cursors are independent.
///
/// Under multi-tenancy the same fact becomes two problems: first, **anybody's
/// write wakes everybody**, so a hundred users means a hundred times the empty
/// pulls; second, every user can observe that "somebody else is writing", which
/// is a side channel that should not exist.
pub(crate) const NOTIFY_CHANNEL_PREFIX: &str = "cortex_sync";

/// The advisory lock key used when installing the trigger.
///
/// It deliberately does not reuse the write transaction's
/// [`crate::SYNC_ADVISORY_LOCK_KEY`]: the semantics of that lock are "serialize
/// sync sequence allocation", and using it to serialize DDL would make two
/// entirely unrelated things block each other, which is misleading when
/// somebody later investigates lock waits.
const DDL_LOCK_KEY: i64 = 4273;

const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// The DDL for the notify function.
///
/// The channel name now has to be spliced in (one per tenant), which is why
/// this went from a constant to a function.
///
/// **Splicing strings into DDL is normally a bad habit**; the only thing that
/// makes it defensible here is that `channel` comes from
/// [`crate::SchemaName::notify_channel`], and that type's constructor is
/// allowlist-validated (it can only be `public` or `u_` + a 26-character
/// lowercase ULID). The safety comes from the **type**, not from this line.
///
/// The function itself is also created inside the tenant schema (by way of the
/// connection's `search_path`), so different tenants' `cortex_notify_sync()`
/// do not overwrite one another.
fn create_function_sql(channel: &str) -> String {
    format!(
        "CREATE OR REPLACE FUNCTION cortex_notify_sync() RETURNS trigger
LANGUAGE plpgsql AS $fn$
BEGIN
    -- The payload carries only seq. A NOTIFY payload is capped at 8000 bytes,
    -- so stuffing business rows in will blow up sooner or later; more to the
    -- point, the client should be pulling from /sync by its own cursor anyway
    PERFORM pg_notify('{channel}', NEW.seq::text);
    RETURN NULL;
END;
$fn$"
    )
}

/// The DDL for the trigger.
///
/// It checks for existence before creating rather than using
/// `DROP TRIGGER IF EXISTS` + `CREATE`: the DROP takes an ACCESS EXCLUSIVE lock
/// on `sync_log`, blocking every in-flight write transaction. Startup happens
/// often (during development every code change restarts the process), and there
/// is no reason to jolt the write path each time.
const CREATE_TRIGGER_SQL: &str = "\
DO $do$
BEGIN
    -- The existence check must be bound to the specific table
    -- ('sync_log'::regclass resolves via search_path); comparing tgname alone
    -- would misread 'another schema already has a trigger by that name' as
    -- 'already installed' -- and integration tests running in a temporary
    -- schema are exactly that scenario
    IF NOT EXISTS (
        SELECT 1 FROM pg_trigger
         WHERE tgname = 'cortex_sync_notify'
           AND NOT tgisinternal
           AND tgrelid = 'sync_log'::regclass
    ) THEN
        CREATE TRIGGER cortex_sync_notify
            AFTER INSERT ON sync_log
            FOR EACH ROW EXECUTE FUNCTION cortex_notify_sync();
    END IF;
END
$do$";

/// One "the cursor moved" signal.
///
/// **Only a signal; it carries no business data.** On receiving it, pull from
/// [`Store::fetch_since`] using your own cursor — the `cursor` in the event is
/// only for gauging how far behind you are and for display, and must never be
/// used as the starting point of the next pull (that would skip the range you
/// have not pulled yet).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncSignal {
    /// There is new data; the server cursor has advanced to `cursor`.
    Bump { cursor: i64 },
    /// Some number of advances may have been missed in between — the
    /// notification connection to Postgres was broken at some point.
    ///
    /// Semantically identical to [`Self::Bump`] (both mean "go pull"); it gets
    /// its own variant so that layers above can measure "normal increments"
    /// separately from "there may be a gap": the latter appearing frequently is
    /// an operational signal.
    Resync { cursor: i64 },
}

/// The subscription handle for `sync_log` advance notifications, handed out by
/// [`Store::listen_sync`].
///
/// One handle exclusively occupies one connection from the storage layer's pool
/// (`LISTEN` is connection-level state and cannot be done on a pooled
/// connection). This is the only long-lived resident connection in the daemon,
/// and the pool leaves headroom accordingly.
pub struct SyncNotifications {
    store: Store,
    /// `None` means not yet connected or just disconnected; the next
    /// [`Self::recv`] will (re)connect.
    listener: Option<PgListener>,
    backoff: Duration,
}

impl std::fmt::Debug for SyncNotifications {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncNotifications")
            .field("connected", &self.listener.is_some())
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl SyncNotifications {
    /// Wait for the next signal.
    ///
    /// **Never returns an error**: if the connection drops it reconnects
    /// internally with exponential backoff, and on success first emits a
    /// [`SyncSignal::Resync`] (notifications during the outage are lost forever,
    /// so the client must be made to catch up on its own — which is exactly the
    /// value of "push the signal, not the data": the remedy is one sentence).
    ///
    /// Returning `None` has **exactly one cause**: the storage layer's pool has
    /// been closed, i.e. the process is shutting down. Layers above use that to
    /// end the forwarding task rather than backing off until the end of time
    /// against a pool that will never connect again.
    pub async fn recv(&mut self) -> Option<SyncSignal> {
        loop {
            if self.store.pool().is_closed() {
                return None;
            }

            if self.listener.is_none() {
                let channel = self.store.schema().notify_channel();
                match subscribe(self.store.pool(), &channel).await {
                    Ok(listener) => {
                        self.backoff = RECONNECT_MIN;
                        self.listener = Some(listener);
                        tracing::info!(channel = %channel, "已订阅 Postgres 通知通道");
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            backoff_secs = self.backoff.as_secs(),
                            "订阅通知通道失败"
                        );
                        self.sleep_backoff().await;
                        continue;
                    }
                }
            }

            // The listening connection is not in the main pool (see
            // [`subscribe`]), so closing the main pool does not interrupt
            // `try_recv`. Hence we explicitly race the pool's close event —
            // otherwise this task would hang forever on a connection nobody
            // writes to once the process exits.
            let closed = self.store.pool().close_event();

            // Pull the result out first and match afterwards: otherwise the
            // mutable borrow of listener would stay alive inside the match
            // body, while the arms need &self.store to query the cursor
            let received = {
                let listener = self
                    .listener
                    .as_mut()
                    .expect("the step above guarantees it is in place");
                let closed = std::pin::pin!(closed);
                tokio::select! {
                    received = listener.try_recv() => received,
                    () = closed => return None,
                }
            };

            match received {
                Ok(Some(note)) => {
                    let cursor = match note.payload().parse::<i64>() {
                        Ok(v) => v,
                        // A non-numeric payload means somebody else is sending
                        // on the same channel. Do not guess — ask the database
                        // once for the real cursor
                        Err(_) => self.cursor_or_zero().await,
                    };
                    return Some(SyncSignal::Bump { cursor });
                }
                // try_recv returning None = the connection dropped and has
                // already been reconnected
                Ok(None) => {
                    let cursor = self.cursor_or_zero().await;
                    tracing::warn!(cursor, "通知连接曾中断，降级为 resync");
                    return Some(SyncSignal::Resync { cursor });
                }
                Err(e) => {
                    // A closed pool also lands here (PoolClosed). The is_closed
                    // check at the top of the next iteration recognizes it and
                    // wraps up, so no backoff is wasted
                    self.listener = None;
                    tracing::warn!(error = %e, "通知连接出错，将重连");
                    self.sleep_backoff().await;
                }
            }
        }
    }

    /// Query the current far end of the cursor. Returns 0 when the query fails
    /// — a cursor that is too small only makes the layer above pull one extra
    /// time, whereas one that is too large makes it skip rows, so failure must
    /// err on the small side.
    async fn cursor_or_zero(&self) -> i64 {
        self.store.latest_seq().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "读取 latest_seq 失败，按 0 下发");
            0
        })
    }

    async fn sleep_backoff(&mut self) {
        tokio::time::sleep(self.backoff).await;
        self.backoff = (self.backoff * 2).min(RECONNECT_MAX);
    }
}

/// Build a second pool holding exactly one connection from the main pool's
/// **connection options**, and subscribe to the channel on it.
///
/// # Why not `PgListener::connect_with(the main pool)` directly
///
/// `LISTEN` is connection-level state, so listening occupies that connection
/// permanently. Losing one connection out of a pool of 16 is a small matter;
/// the real problem is that [`Store::close`] waits for every connection to be
/// returned, and the listening one is only returned when `try_recv` is polled —
/// so anybody who does `await close()` and then `await recv()` in the same task
/// has the two waiting on each other, a flat deadlock. Observed in practice.
///
/// # Why not re-parse the connection string
///
/// Reusing `connect_options` means TLS, `search_path`, and every other option
/// follow along automatically. Reassembling from the connection string is the
/// source of problems of the form "the listener is connected to a different
/// database/schema than the writer", and the symptom of that is "everything is
/// fine, it just doesn't push".
async fn subscribe(pool: &PgPool, channel: &str) -> Result<PgListener> {
    let options = pool.connect_options().as_ref().clone();
    let dedicated = PgPoolOptions::new()
        .max_connections(1)
        // The listening connection has to stay alive: a pool lifetime cap or
        // idle reaping would only produce a "silent connection swap" here, and
        // notifications are lost at the moment of the swap, with no error
        .max_lifetime(None)
        .idle_timeout(None)
        .connect_with(options)
        .await?;

    let mut listener = PgListener::connect_with(&dedicated).await?;
    // This pool is dedicated to listening; nobody but the listener holds it,
    // so nobody is going to close it
    listener.ignore_pool_close_event(true);
    listener.listen(channel).await?;
    Ok(listener)
}

impl Store {
    /// Idempotently install the notify trigger on `sync_log`.
    ///
    /// The function uses `CREATE OR REPLACE`, and the trigger checks for
    /// existence before creating; both happen inside a transaction holding
    /// [`DDL_LOCK_KEY`], so several instances starting concurrently queue up
    /// rather than colliding on the name.
    ///
    /// # Errors
    /// Returns [`StoreError`] on insufficient permissions or when the database
    /// cannot be reached.
    pub async fn install_sync_notify(&self) -> Result<()> {
        let mut tx = self.pool().begin().await?;

        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(DDL_LOCK_KEY)
            .execute(&mut *tx)
            .await?;

        let channel = self.schema().notify_channel();
        // AssertSqlSafe is affordable here: `channel` is assembled by
        // `SchemaName::notify_channel`, and that type's constructor is
        // allowlist-validated. The safety comes from the type
        sqlx::query(sqlx::AssertSqlSafe(create_function_sql(&channel)))
            .execute(&mut *tx)
            .await?;
        sqlx::query(CREATE_TRIGGER_SQL).execute(&mut *tx).await?;

        tx.commit().await?;
        tracing::info!(channel = %channel, "sync_log 通知触发器就绪");
        Ok(())
    }

    /// Install the trigger and subscribe to "the cursor moved" notifications.
    ///
    /// A failure to install the trigger does **not** abort the subscription: it
    /// may simply have been created already by another instance in the same
    /// cluster, and even if it genuinely was not created, returning a
    /// subscription that never speaks is better than making the caller fail to
    /// start — the `GET /sync` polling path, our floor, does not depend on
    /// push. Failures are logged at `error` level, because their consequence
    /// (push silently stops working) shows up in the UI merely as "it sometimes
    /// doesn't refresh".
    pub async fn listen_sync(&self) -> SyncNotifications {
        if let Err(e) = self.install_sync_notify().await {
            tracing::error!(error = %e, "安装 sync_log 通知触发器失败，实时推送可能不可用");
        }
        SyncNotifications {
            store: self.clone(),
            listener: None,
            backoff: RECONNECT_MIN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_and_channel_name_agree() {
        // The channel name is a literal inside the DDL and a constant in Rust,
        // and the two must agree, or the trigger sends to a channel nobody is
        // listening on — the symptom being "silently no push"
        assert!(
            create_function_sql("cortex_sync_public").contains("'cortex_sync_public'"),
            "the channel name was not spliced into the DDL -- the trigger would send on a channel \
             nobody listens to"
        );
    }

    #[test]
    fn ddl_lock_key_differs_from_the_sync_write_lock() {
        assert_ne!(
            DDL_LOCK_KEY,
            i64::from(crate::SYNC_ADVISORY_LOCK_KEY),
            "reusing the write transaction's lock would make DDL creation and sync sequence \
             allocation block each other"
        );
    }

    #[test]
    fn backoff_is_bounded() {
        // With an unbounded backoff, one long database maintenance window would
        // grow the reconnect interval to hours, and once the database is back
        // the UI would wait a very long time before push resumed
        let mut d = RECONNECT_MIN;
        for _ in 0..20 {
            d = (d * 2).min(RECONNECT_MAX);
        }
        assert_eq!(
            d, RECONNECT_MAX,
            "the backoff should converge to the ceiling rather than growing without bound"
        );
    }
}

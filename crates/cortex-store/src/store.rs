//! The connection pool and the entry point for write transactions.

use chrono::{DateTime, Utc};
use sqlx::FromRow;
use sqlx::migrate::Migrator;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::error::Result;
use crate::model::{EpisodeAttachment, Project, ProjectEvent, SessionEvent, SessionState};
use crate::txn::WriteTxn;

/// The migration set embedded at compile time.
///
/// `migrate!` rather than reading the directory at runtime: the binary carries
/// its own schema, so deployments do not have to ship `migrations/` alongside
/// it. Note that this only **reads** files — it needs no compile-time database
/// connection (only the `query!` macros do, and this crate deliberately avoids
/// them; see the crate-level docs).
static MIGRATOR: Migrator = sqlx::migrate!("../../migrations");

/// The set that runs **exactly once per deployment**: accounts, tokens,
/// invitations, usage.
///
/// Multi-tenancy forced the split from [`MIGRATOR`]: that set runs once per
/// tenant, while this one spans tenants. Mixed together, provisioning a new
/// tenant would run into `relation already exists` on the `cortex_auth.users`
/// migration — and the failure point is halfway through account creation.
static GLOBAL_MIGRATOR: Migrator = sqlx::migrate!("../../migrations-global");

/// The default connection pool ceiling. Write transactions are serialized by
/// the advisory lock, so the pool does not need to be large; the surplus
/// connections are left for the read path (four-way retrieval fires several
/// queries concurrently).
const DEFAULT_MAX_CONNECTIONS: u32 = 16;

/// The storage-layer handle. Internally an `Arc`-semantics connection pool, so
/// cloning is nearly free.
#[derive(Clone, Debug)]
pub struct Store {
    pool: PgPool,
    /// Which tenant's schema this pool is connected to.
    ///
    /// It is carried along for two things that **used to be global
    /// singletons**: the advisory lock key for write transactions, and the
    /// NOTIFY channel name for sync. It lives on `Store` rather than being
    /// passed at each call — passing it means every call site could pass the
    /// wrong one, and the consequence of a wrong one is "two tenants sharing a
    /// single lock" or "subscribed to somebody else's channel".
    schema: crate::tenant::SchemaName,
}

impl Store {
    /// Connect with the default parameters.
    pub async fn connect(database_url: &str) -> Result<Self> {
        Self::connect_with(
            PgPoolOptions::new().max_connections(DEFAULT_MAX_CONNECTIONS),
            database_url,
        )
        .await
    }

    /// Connect with custom pool parameters.
    pub async fn connect_with(options: PgPoolOptions, database_url: &str) -> Result<Self> {
        Ok(Self::from_pool(options.connect(database_url).await?))
    }

    /// Take over an already-built connection pool. Tests that use a dedicated
    /// schema come through here.
    ///
    /// The schema is taken to be `public` — that is where both single-user
    /// deployments and the test harness land. For multi-tenancy use
    /// [`Self::from_pool_in`].
    #[must_use]
    pub fn from_pool(pool: PgPool) -> Self {
        Self {
            pool,
            schema: crate::tenant::SchemaName::public(),
        }
    }

    /// Take over a pool whose **`search_path` is already welded in place**, and
    /// remember whose it is.
    #[must_use]
    pub fn from_pool_in(pool: PgPool, schema: crate::tenant::SchemaName) -> Self {
        Self { pool, schema }
    }

    /// The tenant this pool serves.
    #[must_use]
    pub fn schema(&self) -> &crate::tenant::SchemaName {
        &self.schema
    }

    /// Migrate this tenant's schema up to date.
    ///
    /// Runs [`MIGRATOR`] only (`migrations/`, the memory tables of which each
    /// tenant gets a copy). For the global set see [`Self::migrate_global`].
    ///
    /// # Errors
    /// Whichever migration failed to run.
    pub async fn migrate(&self) -> Result<()> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// Run the global set (`cortex_auth`). Once per deployment, not once per
    /// tenant.
    ///
    /// # Why this opens its own connection instead of using the existing pool
    ///
    /// Its `_sqlx_migrations` must land inside `cortex_auth`, or it collides
    /// with the tenant set's version table in `public` — the two sets have
    /// their own version numbers, and mixing them into one table shows up as
    /// "one of the sets is believed to have already been applied", i.e. **the
    /// tables were never created but are assumed to be**.
    ///
    /// And `search_path` is a connection-level property, so it cannot be
    /// changed on an existing pool.
    ///
    /// # Errors
    /// Cannot connect, cannot create `cortex_auth`, or some migration fails to
    /// run.
    pub async fn migrate_global(database_url: &str) -> Result<()> {
        use std::str::FromStr as _;

        // Create the schema first: `_sqlx_migrations` has to land inside it,
        // and that happens before the first migration — letting a migration
        // create it would be one step too late
        let boot = PgPoolOptions::new()
            .max_connections(1)
            .connect(database_url)
            .await?;
        sqlx::query("CREATE SCHEMA IF NOT EXISTS cortex_auth")
            .execute(&boot)
            .await?;
        boot.close().await;

        let options = sqlx::postgres::PgConnectOptions::from_str(database_url)
            .map_err(|e| {
                crate::error::StoreError::Invalid(format!("DATABASE_URL is not valid: {e}"))
            })?
            // public stays at the tail: the `ulid` and `sha256` DOMAINs live
            // there
            .options([("search_path", "cortex_auth,public")]);
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let result = GLOBAL_MIGRATOR.run(&pool).await;
        pool.close().await;
        result?;
        Ok(())
    }

    /// Liveness probe.
    pub async fn ping(&self) -> Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Close the connection pool, waiting for in-flight queries to finish.
    pub async fn close(&self) {
        self.pool.close().await;
    }

    #[doc(hidden)]
    pub fn debug_pool(&self) -> &PgPool {
        &self.pool
    }

    /// The underlying connection pool.
    ///
    /// For tables that live **outside this crate** but still belong to the same
    /// tenant (currently cortexd's `llm_keys`). That table has nothing to do
    /// with memory and does not warrant a set of repository-layer methods of
    /// its own, but it must land in the **same schema** — and this pool's
    /// `search_path` is already welded into its connection options, so this is
    /// the right thing to reach for.
    ///
    /// Do not use it to write memory tables: that path must go through
    /// [`Self::write_txn`] (advisory lock + sync_log in the same transaction).
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Execute `f` inside a **discipline-bound** write transaction.
    ///
    /// The transaction takes `pg_advisory_xact_lock(4272)` right at its start,
    /// serializing concurrent write transactions — this guarantees that the
    /// order of `sync_log.seq` equals the order in which rows become visible to
    /// readers, so advancing the cursor never skips a row. See the module docs
    /// of [`crate::txn`].
    ///
    /// If `f` returns `Ok` the transaction commits; on `Err` it rolls back. The
    /// [`WriteTxn`] that `f` receives has **write methods only**: flows that
    /// need to read before writing (contradiction resolution, entity
    /// resolution, merge-cycle detection) should finish reading and computing
    /// before entering the transaction — sequence-allocating transactions must
    /// be short and write-only.
    ///
    /// ```no_run
    /// # use cortex_store::{Store, NewEpisode};
    /// # async fn demo(store: &Store, ep: NewEpisode) -> cortex_store::Result<()> {
    /// let seq = store
    ///     .write_txn(async |tx| tx.insert_episode(&ep).await)
    ///     .await?;
    /// # let _ = seq;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn write_txn<T, F>(&self, f: F) -> Result<T>
    where
        F: AsyncFnOnce(&mut WriteTxn) -> Result<T>,
    {
        let mut txn = WriteTxn::begin(&self.pool, self.schema.lock_key()).await?;

        match f(&mut txn).await {
            Ok(value) => {
                txn.commit().await?;
                Ok(value)
            }
            Err(err) => {
                if let Err(rollback_err) = txn.rollback().await {
                    tracing::warn!(
                        error = %rollback_err,
                        "rolling back the write transaction failed"
                    );
                }
                Err(err)
            }
        }
    }
}

/// An overview of one session.
///
/// **Stitched from two sources**: everything derivable from the messages
/// themselves (start and end times, message count, first user message) is
/// aggregated from `episodes`; everything that needs storing separately
/// (user-defined title, archived flag, workspace) comes from `session_state`,
/// the final-state view over `session_events`.
///
/// The second half did not always exist — back then sessions were not
/// first-class, and neither renaming nor deleting was possible. What filled the
/// gap was **not** stuffing control records into `episodes` (that would open a
/// control channel inside the L0 raw layer, at the cost of teaching extraction,
/// retrieval, sync, and all three UI clients to skip them), but a separate
/// event table. See `migrations/20260807000002_session_events.sql`.
#[derive(Debug, Clone, PartialEq, Eq, FromRow)]
pub struct SessionDigest {
    pub session_id: String,
    pub message_count: i64,
    /// When the session's first message occurred
    pub started_at: DateTime<Utc>,
    /// When the last message occurred — the list sorts on this
    pub updated_at: DateTime<Utc>,
    /// The body of the first **user** message. The derived title comes from it;
    /// how far to truncate is a presentation decision, left to layers above.
    pub first_user_text: Option<String>,
    /// The last message that had a body, for the list preview.
    pub last_text: Option<String>,
    /// The title the user set. `None` = never renamed, and the layer above
    /// falls back to deriving one from `first_user_text`.
    pub title: Option<String>,
    /// Archived — hidden from the default list, but neither the messages nor
    /// the derived memory are touched.
    pub archived: bool,
    /// The bound local directory. `None` = a pure chat session, and the file
    /// tools do not enter the model's tool catalog.
    pub workspace: Option<String>,
    /// The project it belongs to. `None` = ungrouped, **which also covers "the
    /// project it used to be in has been deleted"** — the view has already
    /// collapsed dangling bindings to `None`, so what is read here is the final
    /// word.
    pub project_id: Option<String>,
    /// Where this session runs. **It must have a value even when there is no
    /// session_state row (a brand-new session)** — the view coalesces it to
    /// 'cloud', and the NULL produced by the LEFT JOIN is caught once more
    /// here.
    pub runtime: crate::model::SessionRuntime,
}

impl Store {
    /// The session list: most recently active first.
    ///
    /// # Why one SQL aggregate instead of pulling the last N episodes and
    /// summarizing them in memory
    ///
    /// The latter (which is what this used to do) has a defect that raises no
    /// error: sessions outside the window **simply vanish**. Pull the 200 most
    /// recent messages and one talkative session can fill the whole window, so
    /// yesterday's sessions disappear from the list — the user sees "my history
    /// is gone" while the logs look perfectly fine. Handing the summarizing to
    /// the database is what makes `limit` actually apply to **sessions** rather
    /// than to messages.
    ///
    /// # Why the archived filter is in SQL instead of filtering after fetching
    ///
    /// The other face of the same trap: take `limit` rows first and drop the
    /// archived ones in memory, and archived sessions **consume the quota** —
    /// archive twenty sessions and the list shows twenty fewer. The filter has
    /// to happen before the `LIMIT`.
    ///
    /// # Only sessions that have messages are listed
    ///
    /// The aggregate starts from `episodes`, so a session that has only written
    /// session_events and not yet sent a message does not appear here. That is
    /// deliberate: it has no timestamps, no count, and no preview, so listing
    /// it produces one blank row. After a user creates a session they pick a
    /// workspace before sending their first sentence, and during that window
    /// the session is held locally by the client — it appears automatically the
    /// moment the first message lands, and the rename / binding events written
    /// beforehand take effect exactly as written (the final state does not
    /// depend on write order).
    ///
    /// # Filtering by project
    ///
    /// When `project_id` is `None` **no filter is applied** and all sessions
    /// are returned (including ungrouped ones). As with archiving, the filter
    /// happens before the `LIMIT` — otherwise other projects' sessions eat the
    /// quota, and the symptom is "this project clearly has sessions, but the
    /// list is empty".
    ///
    /// Passing the id of a **deleted project** yields an empty list: the view
    /// has already collapsed bindings pointing at it to NULL. That is exactly
    /// what we want — a deleted project should not still be diggable.
    pub async fn session_digests(
        &self,
        limit: i64,
        include_archived: bool,
        project_id: Option<&str>,
    ) -> Result<Vec<SessionDigest>> {
        let rows = sqlx::query_as::<_, SessionDigest>(
            // Aggregate first, then JOIN the view: the other way around,
            // session_state gets recomputed once per episode dragging it
            // along, and it already contains four DISTINCT ONs internally
            "SELECT d.session_id, d.message_count, d.started_at, d.updated_at,
                    d.first_user_text, d.last_text,
                    s.title,
                    coalesce(s.archived, false) AS archived,
                    s.workspace,
                    s.project_id,
                    -- The LEFT JOIN may produce an all-NULL row (the session
                    -- has no lifecycle events yet), and then runtime is NULL
                    -- too. The view already coalesces once; what is caught
                    -- here is the 'never joined at all' case
                    coalesce(s.runtime, 'cloud') AS runtime
               FROM (
                    SELECT e.session_id,
                           count(*)            AS message_count,
                           min(e.occurred_at)  AS started_at,
                           max(e.occurred_at)  AS updated_at,
                           (SELECT u.text FROM episodes u
                             WHERE u.session_id = e.session_id
                               AND u.role = 'user' AND u.text IS NOT NULL
                             ORDER BY u.occurred_at ASC, u.id ASC LIMIT 1)  AS first_user_text,
                           (SELECT l.text FROM episodes l
                             WHERE l.session_id = e.session_id AND l.text IS NOT NULL
                             ORDER BY l.occurred_at DESC, l.id DESC LIMIT 1) AS last_text
                      FROM episodes e
                     GROUP BY e.session_id
               ) d
               LEFT JOIN session_state s ON s.session_id = d.session_id
              WHERE ($2 OR NOT coalesce(s.archived, false))
                AND ($3::text IS NULL OR s.project_id = $3::text)
              ORDER BY d.updated_at DESC, d.session_id DESC
              LIMIT $1",
        )
        .bind(limit)
        .bind(include_archived)
        .bind(project_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// An overview of a single session. **No archived filter** — the caller
    /// asked for this specific one by id, and whether it is archived is one of
    /// the facts they want to see, not a reason to hide it.
    ///
    /// Returning `None` means this session has no messages at all (it may have
    /// written only lifecycle events).
    pub async fn session_digest(&self, session_id: &str) -> Result<Option<SessionDigest>> {
        let row = sqlx::query_as::<_, SessionDigest>(
            "SELECT d.session_id, d.message_count, d.started_at, d.updated_at,
                    d.first_user_text, d.last_text,
                    s.title,
                    coalesce(s.archived, false) AS archived,
                    s.workspace,
                    s.project_id,
                    -- The LEFT JOIN may produce an all-NULL row (the session
                    -- has no lifecycle events yet), and then runtime is NULL
                    -- too. The view already coalesces once; what is caught
                    -- here is the 'never joined at all' case
                    coalesce(s.runtime, 'cloud') AS runtime
               FROM (
                    SELECT e.session_id,
                           count(*)            AS message_count,
                           min(e.occurred_at)  AS started_at,
                           max(e.occurred_at)  AS updated_at,
                           (SELECT u.text FROM episodes u
                             WHERE u.session_id = e.session_id
                               AND u.role = 'user' AND u.text IS NOT NULL
                             ORDER BY u.occurred_at ASC, u.id ASC LIMIT 1)  AS first_user_text,
                           (SELECT l.text FROM episodes l
                             WHERE l.session_id = e.session_id AND l.text IS NOT NULL
                             ORDER BY l.occurred_at DESC, l.id DESC LIMIT 1) AS last_text
                      FROM episodes e
                     WHERE e.session_id = $1
                     GROUP BY e.session_id
               ) d
               LEFT JOIN session_state s ON s.session_id = d.session_id",
        )
        .bind(session_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// The final state of a session along each of its four dimensions.
    ///
    /// Returning `None` means this session has never had a lifecycle event at
    /// all — never renamed, never archived, never bound to a workspace, never
    /// put in a project. The caller should treat it as "all defaults"; this is
    /// not an error.
    pub async fn session_state(&self, session_id: &str) -> Result<Option<SessionState>> {
        let row = sqlx::query_as::<_, SessionState>(
            "SELECT session_id, title, archived, workspace, project_id, runtime, decided_at
               FROM session_state WHERE session_id = $1",
        )
        .bind(session_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// Every lifecycle event of one session, ascending. The UI's "how did this
    /// change over time".
    pub async fn session_events(&self, session_id: &str) -> Result<Vec<SessionEvent>> {
        let rows = sqlx::query_as::<_, SessionEvent>(
            "SELECT id, session_id, op, title, workspace, project_id, runtime,
                    actor, device_id, created_at
               FROM session_events WHERE session_id = $1
              ORDER BY created_at ASC, id ASC",
        )
        .bind(session_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── projects ───────────────────────────────────────────

    /// The projects that exist, newest first.
    ///
    /// # What `session_count` counts
    ///
    /// It counts "sessions under this project that **would appear in the
    /// session list**", so both filters are required:
    ///
    /// - Exclude archived ones: the product meaning of archiving is
    ///   disappearing from the default list, so if the count still includes
    ///   them the user sees "3 sessions" and finds only 2 after clicking in
    /// - Require at least one message: the session list starts from the
    ///   `episodes` aggregate, so a session that was created but has never said
    ///   anything is not in the list to begin with (see
    ///   [`Self::session_digests`])
    ///
    /// Both align with the default semantics of `GET /sessions?project_id=` —
    /// a count that disagrees with the list length is the kind of problem that
    /// looks obviously wrong yet offers no lead when you go looking for it.
    pub async fn projects(&self) -> Result<Vec<Project>> {
        let rows = sqlx::query_as::<_, Project>(
            "SELECT p.project_id, p.name, p.created_at,
                    coalesce(c.n, 0) AS session_count
               FROM project_state p
               LEFT JOIN (
                    SELECT s.project_id, count(*) AS n
                      FROM session_state s
                     WHERE s.project_id IS NOT NULL
                       AND NOT s.archived
                       AND EXISTS (SELECT 1 FROM episodes e
                                    WHERE e.session_id = s.session_id)
                     GROUP BY s.project_id
               ) c ON c.project_id = p.project_id
              ORDER BY p.created_at DESC, p.project_id DESC",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// A single project. Returning `None` means it does not exist or has been
    /// deleted — to the caller those are the same thing.
    pub async fn project(&self, project_id: &str) -> Result<Option<Project>> {
        let row = sqlx::query_as::<_, Project>(
            "SELECT p.project_id, p.name, p.created_at,
                    coalesce(c.n, 0) AS session_count
               FROM project_state p
               LEFT JOIN (
                    SELECT s.project_id, count(*) AS n
                      FROM session_state s
                     WHERE s.project_id IS NOT NULL
                       AND NOT s.archived
                       AND EXISTS (SELECT 1 FROM episodes e
                                    WHERE e.session_id = s.session_id)
                     GROUP BY s.project_id
               ) c ON c.project_id = p.project_id
              WHERE p.project_id = $1",
        )
        .bind(project_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// Every lifecycle event of one project, ascending. The UI's "how did this
    /// change over time".
    pub async fn project_events(&self, project_id: &str) -> Result<Vec<ProjectEvent>> {
        let rows = sqlx::query_as::<_, ProjectEvent>(
            "SELECT id, project_id, op, name, actor, device_id, created_at
               FROM project_events WHERE project_id = $1
              ORDER BY created_at ASC, id ASC",
        )
        .bind(project_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Fetch the attachments of a batch of episodes, **along with their mime
    /// type and size**.
    ///
    /// Session detail has to hang attachments off every message, and querying
    /// one at a time is N+1 — a two-hundred-message session becomes two hundred
    /// round trips, and this path is exactly the hot path of "open a session
    /// containing images".
    ///
    /// The metadata rides along instead of making the client take the hash and
    /// ask again via `GET /blobs/{hash}`: that would be one round trip per
    /// attachment, and until it returns the client has **nothing at all to
    /// display** — a PDF could only be drawn as "document · a1b2c3d4".
    pub async fn episode_attachments_bulk(
        &self,
        episode_ids: &[String],
    ) -> Result<Vec<EpisodeAttachment>> {
        if episode_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, EpisodeAttachment>(
            "SELECT eb.episode_id, eb.blob_hash, eb.kind, eb.filename, b.mime, b.size_bytes
               FROM episode_blobs eb
               JOIN blobs b ON b.hash = eb.blob_hash
              WHERE eb.episode_id = ANY($1)
              ORDER BY eb.episode_id ASC, eb.blob_hash ASC",
        )
        .bind(episode_ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }
}

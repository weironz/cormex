//! Application state and business dispatch.
//!
//! Both backends share one contract: [`Backend::Live`] wires up the real storage
//! layer, provider layer and memory engine; [`Backend::Mock`] is an
//! **explicitly chosen** source of fake data.
//!
//! Keeping Mock is not an expedient — the client's CI, offline development, and
//! "can the UI still be looked at when the backend is down" all need it, and it
//! forces us to keep the contract's two implementations aligned, preventing the
//! real one from quietly drifting away from the documentation.
//!
//! **But it is no longer a fallback.** Mock used to be the degradation path that
//! took over automatically whenever the database or the API key was unavailable,
//! and that let cormexd serve traffic backed by a database of fabricated memory
//! while looking completely normal. It now appears only under
//! `CORMEX_BACKEND=mock` — full reasoning in [`BackendChoice`].

use std::ops::Range;
use std::sync::Arc;

use axum::body::Bytes;
use chrono::Utc;
use cormex_core::{Config, CormexError, Id, Result};
use cormex_memory::embed::SharedEmbedder;

use crate::auth::{AuthMode, TicketBook};
use crate::blobs::{MediaStore, PRESIGN_TTL};
use crate::live::Live;
use crate::request_tenant::Tenant;
use crate::sync_notify::SyncBus;
use tokio::sync::broadcast;

use crate::dto::*;
use cormex_llm::MessageStream;
use cormex_proto::episodes::{EpisodeAck, NewEpisodeRequest};
use cormex_proto::llm::LlmStreamRequest;

/// The mock backend's fake cursor. See [`AppState::new_mock`].
static MOCK_CURSOR: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

#[derive(Clone)]
pub struct AppState {
    inner: Arc<Inner>,
}

struct Inner {
    #[allow(dead_code)]
    config: Config,
    backend: Backend,
    bus: SyncBus,
    /// Object storage is **shared by both backends**.
    ///
    /// It is an independent failure domain from Postgres: an image can still be
    /// stored in RustFS while the database is down, and vice versa. Putting it
    /// inside `Backend::Live` would turn "images cannot be uploaded in mock
    /// mode" into a restriction with no justification — and the client's CI
    /// runs the upload flow precisely on mock.
    blobs: MediaStore,
    /// Process-level things that are independent of the backend. See
    /// [`Runtime`].
    rt: Runtime,
}

/// Process-level facilities that are independent of the backend choice.
///
/// A struct of its own rather than three fields scattered through [`Inner`]:
/// what they have in common is that **mock and live must share one copy**.
/// Authentication is a whole-process property, the confirmation ledger has to be
/// findable from any HTTP response, and the ticket book likewise — any one of
/// them following `Backend` would become "this path cannot be tested in mock
/// mode", and the client's CI runs on mock.
#[derive(Clone)]
pub struct Runtime {
    pub auth: AuthMode,
    pub tickets: Arc<TicketBook>,
    /// The connection pool for the account tables (`cormex_auth`), plus the
    /// tenant pool registry.
    ///
    /// **Separate from the memory database's pool**: every query in
    /// `cormex-store` assumes it is inside some tenant's schema, while accounts
    /// are cross-tenant. Sharing one pool would mean changing `search_path` back
    /// and forth, and that is exactly the kind of change
    /// [`cormex_store::TenantPools`]'s documentation calls "miss it once and you
    /// have a cross-user leak".
    ///
    /// `None` = this deployment has no database attached (the mock backend). The
    /// account endpoints all answer 501 in that state — pretending logins work
    /// is far worse than saying plainly that they are unsupported.
    pub accounts: Option<Accounts>,
    /// Issued access tokens. See [`crate::accounts::AccessBook`].
    pub access: Arc<crate::accounts::AccessBook>,
    /// Issued **sandbox** tokens. Alongside the two books above rather than
    /// merged into them: it is the only credential carrying a **semantic scope**
    /// (see [`crate::delegated_token::DelegatedScope`]), and folding it into
    /// `AccessBook` would turn "what may this token do" into a question that has
    /// to be answered all over the call sites.
    pub delegations: Arc<crate::delegated_token::DelegatedTokens>,
    /// Device keys. `None` when there is no accounts database — the key table
    /// lives in `cormex_auth`, so a deployment without one cannot have device
    /// keys at all, and the branch in `auth::require` is simply not taken.
    pub device_keys: Option<Arc<crate::device_keys::DeviceKeys>>,
}

/// The two account-related handles.
#[derive(Clone)]
pub struct Accounts {
    /// A pool connected directly to `cormex_auth`
    pub pool: sqlx::PgPool,
    /// Memory databases, looked up per user
    pub tenants: Arc<cormex_store::TenantPools>,
}

impl Accounts {
    /// Connect to `cormex_auth` and apply the global migrations.
    ///
    /// The pool needs only two connections: account-related queries are "once at
    /// login, once at refresh", while concurrent pressure on the memory database
    /// goes through the tenant pools.
    ///
    /// # Errors
    /// Cannot connect, or the global migration failed.
    pub async fn connect(database_url: &str) -> Result<Self> {
        cormex_store::Store::migrate_global(database_url)
            .await
            .map_err(|e| {
                cormex_core::CormexError::Store(format!("the global schema migration failed: {e}"))
            })?;

        let options = std::str::FromStr::from_str(database_url)
            .map(|o: sqlx::postgres::PgConnectOptions| {
                // The account tables live in cormex_auth, and this pool touches
                // **only** them. Without public in the path, the two DOMAINs
                // `ulid` and `sha256` cannot be resolved
                o.options([("search_path", "cormex_auth,public")])
            })
            .map_err(|e: sqlx::Error| {
                cormex_core::CormexError::Config(format!("DATABASE_URL is malformed: {e}"))
            })?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(options)
            .await
            .map_err(|e| {
                cormex_core::CormexError::Store(format!("could not connect to cormex_auth: {e}"))
            })?;

        Ok(Self {
            pool,
            tenants: Arc::new(cormex_store::TenantPools::new(database_url)),
        })
    }
}

impl Runtime {
    /// Assemble from the environment. Any misconfiguration fails here rather
    /// than waiting for the first request.
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            auth: AuthMode::from_env()?,
            tickets: Arc::new(TicketBook::default()),
            // Building the pool needs an await while from_env is synchronous.
            // main fills it in once it has the configuration — splitting it in
            // two keeps "the configuration is wrong" and "the database is
            // unreachable" as two distinct failures in the log
            accounts: None,
            access: Arc::new(crate::accounts::AccessBook::default()),
            // Purely in memory, living and dying with the process. **Deliberately
            // not persisted**: every delegated credential is void after a
            // restart, and that is correct — the callers holding them need to be
            // taken over again anyway
            delegations: Arc::new(crate::delegated_token::DelegatedTokens::default()),
            device_keys: None,
        })
    }
}

impl AppState {
    /// The two account-related handles.
    ///
    /// # Errors
    /// This deployment has no database attached (the mock backend). It answers
    /// 501 rather than panicking — this was originally written as an `expect`,
    /// justified by "the routing layer blocks it first", except that gate did
    /// not exist at the time, so one anonymous `GET /auth/me` was enough to
    /// panic this handler. **"The caller will check first" is not a
    /// protection.**
    pub fn accounts(&self) -> Result<&Accounts, crate::routes::ApiError> {
        self.inner.rt.accounts.as_ref().ok_or_else(|| {
            crate::routes::ApiError::unsupported(
                "this deployment has no database attached, so accounts are \
                 unavailable (it is running on the mock backend)",
            )
        })
    }

    /// The book of issued access tokens.
    pub fn access_book(&self) -> &crate::accounts::AccessBook {
        &self.inner.rt.access
    }
}

#[derive(Clone)]
enum Backend {
    /// A source of fake data. Its contract is identical to Live's.
    Mock,
    /// The live backend: Postgres + LLM + the memory engine
    Live(Arc<Live>),
}

/// `CORMEX_BACKEND` — which backend to run. **Defaults to live, and never falls
/// back.**
///
/// # Why it cannot be "fall back to mock when the connection fails"
///
/// That is exactly what it used to be, and here is how it went off in
/// production: when cormexd **restarts automatically** after a crash or OOM
/// (`restart: unless-stopped`), compose's
/// `depends_on: condition: service_healthy` **does not apply** — that condition
/// only holds at the moment of `docker compose up`. So one automatic restart
/// during Postgres recovery brings cormexd up with a database of fabricated
/// memory and serves traffic from it.
///
/// And it looks completely normal: HTTP 200, `status: "ok"`, conversation works,
/// retrieval returns results. The only field distinguishing it is
/// `database: "not_wired"` in `/health` — and the production compose file gives
/// cormexd **no healthcheck at all**.
///
/// This is the same failure shape as the 0.1.2 incident where embeddings could
/// not find their weights and silently degraded to fake vectors: **degrading too
/// quietly**. That one went into the CHANGELOG; this one is removed at the level
/// of shape — a container that cannot come up gets restarted by docker
/// repeatedly until the database genuinely returns, which is exactly what we
/// want.
///
/// Mock is still available, but it has to be **said out loud**. To run without a
/// database during development, set `CORMEX_BACKEND=mock`, and `/health` will
/// then honestly report `status: "mock"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendChoice {
    Live,
    Mock,
}

impl BackendChoice {
    /// # Errors
    /// An unrecognised value is an error.
    pub fn from_env() -> cormex_core::Result<Self> {
        // An empty string is treated as "not set", for the same reason as
        // `CORMEX_EMBED_BACKEND`: compose's `${VAR:-}` **sets** an unconfigured
        // variable to the empty string, and `env::var` returns `Ok("")` for it.
        // This trap has already sprung three times in this repository
        std::env::var("CORMEX_BACKEND")
            .ok()
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or(Ok(Self::Live), Self::parse)
    }

    /// # Errors
    /// An unrecognised value is an error — **it never falls back to the
    /// default**.
    ///
    /// A mistyped `CORMEX_BACKEND=moc` quietly running as live would be the
    /// lucky direction; the reverse (`CORMEX_BACKEND=liv` running as mock) is
    /// the very thing this fix exists to prevent, walking back in through
    /// another door.
    pub fn parse(s: &str) -> cormex_core::Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "live" | "real" => Ok(Self::Live),
            "mock" | "fake" => Ok(Self::Mock),
            other => Err(cormex_core::CormexError::Config(format!(
                "unknown CORMEX_BACKEND={other:?}; the choices are live / mock"
            ))),
        }
    }
}

impl AppState {
    pub async fn new_mock(config: Config, rt: Runtime) -> Self {
        let blobs = MediaStore::connect(&config).await;
        let bus = SyncBus::inert();

        // Fake a cursor advance periodically. It looks superfluous, but without
        // it the Flutter/CLI main path — "receive a bump → backfill from /sync
        // → advance the local cursor" — never runs at all during offline
        // development or in the client's CI, and executes for the first time
        // only once a database is attached. Same reasoning as mock_chat_stream
        // fabricating memory and tool events.
        {
            let bus = bus.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(10));
                loop {
                    tick.tick().await;
                    let cursor = MOCK_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                    bus.publish(SyncEvent::Bump { cursor });
                }
            });
        }

        Self {
            inner: Arc::new(Inner {
                config,
                backend: Backend::Mock,
                bus,
                blobs,
                rt,
            }),
        }
    }

    /// Wire up the live backend. An unreachable database or a missing API key
    /// both fail, and **the caller no longer degrades** — it carries the error
    /// all the way to `main` and exits. Reasoning in [`BackendChoice`].
    pub async fn new_live(config: &Config, rt: Runtime) -> Result<Self> {
        // Use the process-wide shared instance (`OnceCell`) rather than
        // constructing one here: the model is close to 600 MB even in int8, and
        // one of the payoffs of the "daemon, not embedded" architectural
        // decision is a resident shared model — building one per site gives that
        // payoff straight back.
        //
        // The backend is chosen by CORMEX_EMBED_BACKEND, defaulting to the real
        // semantic model; CI and offline development set hash explicitly.
        let embedder: SharedEmbedder = cormex_memory::embed::shared_embedder().await?;
        let live = Live::new(config, embedder).await?;
        let bus = SyncBus::spawn(live.store().clone());
        let blobs = MediaStore::connect(config).await;

        let live = Arc::new(live);

        let st = Self {
            inner: Arc::new(Inner {
                config: config.clone(),
                backend: Backend::Live(live),
                bus,
                blobs: blobs.clone(),
                rt,
            }),
        };

        // ── Migrating the tenants that already exist ─────────
        //
        // Newly registered users are migrated by `provision`, while tenants that
        // **already exist** had never once been migrated: `migrate_all` was
        // written and exported, with not a single call site. The consequence is
        // that after adding a new migration, older users' databases stay on the
        // old structure — manifesting as scattered SQL errors rather than one
        // readable startup failure.
        //
        // Broken ones are called out individually and do not drag the service
        // down: one user's database being broken leaves everyone else working.
        if let Ok(schemas) = st.tenant_schemas().await
            && let Ok(acc) = st.accounts()
        {
            let others: Vec<_> = schemas
                .into_iter()
                // public was migrated a moment ago (that line in `Live::new`)
                .filter(|s| s.as_str() != cormex_store::SchemaName::public().as_str())
                .collect();
            if !others.is_empty() {
                let results = cormex_store::migrate_all(acc.tenants.as_ref(), &others).await;
                let bad = results.iter().filter(|r| r.failure.is_some()).count();
                tracing::info!(
                    tenants = results.len(),
                    failed = bad,
                    "finished migrating the existing tenants"
                );
            }
        }

        // On the presigned direct-upload path the server never handles the bytes
        // and transcription has nothing to trigger it — so large files become
        // pure archive. The rescanner treats the blobs table as the
        // authoritative inventory and picks them back up. It does not start
        // itself when no vision model is configured.
        crate::backfill::spawn(st.clone(), blobs);
        // The archive workers: the masker (R-layer byte ranges) and the
        // projector (R → P). Both derive their todo from the data, so a restart
        // loses nothing — see `archive_workers`'s module docs
        crate::archive_workers::spawn(st.clone());
        // Fill in the vectors for old facts after an embedding model change. On
        // by default — the cost of turning it off (old memory permanently
        // exiting vector retrieval) has no symptom at all, while the cost of
        // leaving it on (one embedding call per row in the database) is visible,
        // and it states the numbers before acting. See reembed's module comment
        crate::reembed::spawn(st.clone());

        Ok(st)
    }

    /// A minimal state that touches neither the network nor the filesystem,
    /// purely for the crate's own HTTP tests.
    ///
    /// # Why `new_mock` is not reused
    ///
    /// It calls `MediaStore::connect` (a real S3 round trip, plus creating a
    /// fallback directory in the working directory on failure) and spawns a
    /// background task pushing a fake cursor every 10 seconds. A test asserting
    /// "no credential means 401" should not depend on any of that — this was
    /// precisely the obstacle behind the earlier belief that "route tests cannot
    /// be done", and it turned out to be a constructor problem, not a bin-crate
    /// problem.
    ///
    /// The contract is identical to the mock backend's (`Backend::Mock`), so
    /// what the tests hit is still the **real** [`crate::routes::router`] and the
    /// real handlers.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(rt: Runtime) -> Self {
        Self::for_tests_with(Backend::Mock, rt, Self::test_config())
    }

    /// The same, but backed by a **real** database.
    ///
    /// Takes an already-built [`crate::live::Live`] rather than calling
    /// [`Self::new_live`]: that one also spawns the transcription backfill and
    /// the vector re-embedder, and both of them would then be running against
    /// the test database for the rest of the binary's lifetime. The tests that
    /// need this (tenant isolation by principal) want the request path, not the
    /// background jobs.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests_live(live: crate::live::Live, rt: Runtime, config: Config) -> Self {
        Self::for_tests_with(Backend::Live(Arc::new(live)), rt, config)
    }

    /// storage unavailable" — which would make those tests assert nothing about
    /// the ordering they exist to check.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests_live_with_blobs(
        live: crate::live::Live,
        rt: Runtime,
        config: Config,
        blobs: crate::blobs::MediaStore,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                backend: Backend::Live(Arc::new(live)),
                bus: SyncBus::inert(),
                blobs,
                rt,
            }),
        }
    }

    /// The same state with promotion turned on or off.
    ///
    /// Test-only: the setting normally comes from the environment at startup,
    /// and the tests need to exercise both branches in one process. Rebuilding
    /// the whole `Inner` is what keeps every other handle (the same `Live`, the
    /// same pools) pointing at the same database.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_promotion(&self, on: bool) -> Self {
        let mut config = self.inner.config.clone();
        config.distill.promote = on;
        Self {
            inner: Arc::new(Inner {
                config,
                backend: self.inner.backend.clone(),
                bus: self.inner.bus.clone(),
                blobs: self.inner.blobs.clone(),
                rt: self.inner.rt.clone(),
            }),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    fn for_tests_with(backend: Backend, rt: Runtime, config: Config) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                backend,
                bus: SyncBus::inert(),
                blobs: crate::blobs::MediaStore::unavailable_for_tests(),
                rt,
            }),
        }
    }

    /// A `Config` that reaches nothing.
    #[cfg(any(test, feature = "test-support"))]
    pub fn test_config() -> Config {
        Config {
            database_url: "postgres://tests-never-connect".into(),
            bind: "127.0.0.1:0".into(),
            s3: cormex_core::config::S3Config {
                endpoint: String::new(),
                bucket: String::new(),
                region: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
            llm: cormex_core::config::LlmConfig {
                provider: "none".into(),
                model: String::new(),
                cheap_model: String::new(),
                base_url: None,
            },
            device_id: "test".into(),
            capture: cormex_core::config::CaptureConfig {
                catalog_path: None,
                max_chunk_bytes: cormex_core::config::DEFAULT_CAPTURE_MAX_CHUNK,
            },
            distill: cormex_core::config::DistillConfig {
                model: None,
                daily_tokens: cormex_core::config::DEFAULT_DISTILL_DAILY_TOKENS,
                promote: true,
                promote_per_round: cormex_core::config::DEFAULT_PROMOTE_PER_ROUND,
            },
        }
    }

    // ─────────────────────── authentication ─────────────────

    #[must_use]
    pub fn auth_mode(&self) -> &AuthMode {
        &self.inner.rt.auth
    }

    #[must_use]
    pub fn ticket_book(&self) -> &TicketBook {
        &self.inner.rt.tickets
    }

    #[must_use]
    pub fn delegations(&self) -> &crate::delegated_token::DelegatedTokens {
        &self.inner.rt.delegations
    }

    /// The device-key store.
    ///
    /// Returns a shared no-op when there is none: `auth::require` calls this on
    /// every request, and an `Option` there would mean every caller has to
    /// decide what "no accounts database" means for a device key — the answer
    /// is always "no key resolves", and that belongs here rather than repeated.
    #[must_use]
    pub fn device_keys(&self) -> &crate::device_keys::DeviceKeys {
        static NONE: std::sync::OnceLock<Arc<crate::device_keys::DeviceKeys>> =
            std::sync::OnceLock::new();
        match &self.inner.rt.device_keys {
            Some(d) => d,
            None => NONE.get_or_init(|| {
                // A pool that never connects: `resolve` fails on the query and
                // returns `None`, which is exactly "no device key resolves
                // here"
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://device-keys-unavailable")
                    .expect("a lazy pool does not connect, so this cannot fail");
                Arc::new(crate::device_keys::DeviceKeys::new(pool))
            }),
        }
    }

    // ─────────────────────── multi-tenancy ──────────────────

    /// The live backend's handle. Background jobs use it to bind per tenant.
    #[must_use]
    pub fn live(&self) -> Option<&Arc<Live>> {
        match &self.inner.backend {
            Backend::Mock => None,
            Backend::Live(l) => Some(l),
        }
    }

    /// Which tenants exist right now. **Re-queried every time**, never cached.
    ///
    /// Background jobs iterate over it. Cached, a newly registered user would
    /// not be covered by the backfill until the next restart — silently: all
    /// they notice is that "semantic retrieval seems a bit worse".
    ///
    /// With no accounts system attached this returns `[public]` — the
    /// single-user self-hosted shape.
    ///
    /// # Errors
    /// `cormex_auth.users` could not be queried.
    pub async fn tenant_schemas(&self) -> Result<Vec<cormex_store::SchemaName>> {
        let Ok(acc) = self.accounts() else {
            return Ok(vec![cormex_store::SchemaName::public()]);
        };
        let names: Vec<String> = sqlx::query_scalar(
            // A disabled account is neither migrated nor backfilled — they
            // cannot log in, so embedding money spent on them is pure waste
            "SELECT schema_name FROM cormex_auth.users WHERE disabled_at IS NULL",
        )
        .fetch_all(&acc.pool)
        .await
        .map_err(|e| CormexError::Store(format!("could not list the tenants: {e}")))?;

        Ok(names
            .iter()
            .filter_map(|n| match cormex_store::SchemaName::new(n) {
                Ok(s) => Some(s),
                Err(e) => {
                    // Skip rather than failing the whole batch: one bad row
                    // must not halt everybody's backfill
                    tracing::error!(schema = n, error = %e, "skipping a tenant whose schema name is invalid");
                    None
                }
            })
            .collect())
    }

    /// One tenant's `Store`. Used by background jobs.
    ///
    /// # Errors
    /// The mock backend, or the pool for that schema cannot be created.
    pub async fn tenant_store(
        &self,
        schema: &cormex_store::SchemaName,
    ) -> Result<std::sync::Arc<cormex_store::Store>> {
        if schema.as_str() == cormex_store::SchemaName::public().as_str() {
            return self.public_store().ok_or_else(|| {
                CormexError::Unavailable("the mock backend has no database".into())
            });
        }
        self.accounts()
            .map_err(|_| CormexError::Unavailable("no accounts system is attached".into()))?
            .tenants
            .get(schema)
            .await
            .map_err(|e| {
                CormexError::Store(format!("could not connect to {}: {e}", schema.as_str()))
            })
    }

    /// The `public` pool — the one built at startup. Absent under the mock
    /// backend.
    ///
    /// **This is not a "default tenant".** It is used as a tenant in exactly two
    /// situations: no accounts system attached (single-user self-hosting), and
    /// user #1 (whose schema is literally named `public`). Every other user goes
    /// through the table lookup in [`crate::request_tenant::Tenant`].
    #[must_use]
    pub fn public_store(&self) -> Option<std::sync::Arc<cormex_store::Store>> {
        match &self.inner.backend {
            Backend::Mock => None,
            Backend::Live(l) => Some(std::sync::Arc::new(l.store().clone())),
        }
    }

    // ─────────────────── realtime sync (/ws) ────────────────

    #[must_use]
    pub fn subscribe_sync(&self) -> broadcast::Receiver<SyncEvent> {
        self.inner.bus.subscribe()
    }

    /// The server's current end of the sync cursor.
    ///
    /// A failed read always returns 0: a client receiving too small a cursor
    /// merely fetches once more (harmless), whereas too large a one loses rows
    /// permanently.
    pub async fn latest_cursor(&self) -> i64 {
        match &self.inner.backend {
            Backend::Mock => MOCK_CURSOR.load(std::sync::atomic::Ordering::Relaxed),
            Backend::Live(l) => l.latest_cursor().await,
        }
    }

    /// The top line of `/health`.
    ///
    /// The mock backend **does not report `ok`**: to a probe, that word means
    /// "this service can take traffic", and a process returning fabricated data
    /// cannot. It used to report `ok`, with the `database` line below as the
    /// only thing distinguishing it from "the data is real".
    ///
    /// Mock can now only be chosen explicitly (see [`BackendChoice`]), so this
    /// line mostly serves to confirm at a glance during development whether a
    /// database is actually attached — but it deserves to say so honestly.
    #[must_use]
    pub fn status(&self) -> &'static str {
        match &self.inner.backend {
            Backend::Mock => "mock",
            Backend::Live(_) => "ok",
        }
    }

    pub async fn database_status(&self) -> String {
        match &self.inner.backend {
            Backend::Mock => "not_wired".into(),
            Backend::Live(l) => l.database_status().await,
        }
    }

    /// The capture settings.
    #[must_use]
    pub fn capture_config(&self) -> &cormex_core::config::CaptureConfig {
        &self.inner.config.capture
    }

    /// The distiller's settings.
    #[must_use]
    pub fn distill_config(&self) -> &cormex_core::config::DistillConfig {
        &self.inner.config.distill
    }

    /// Which account owns a schema, for per-tenant accounting.
    ///
    /// `None` means "no accounts system attached" (single-user self-hosting),
    /// where a per-tenant budget is meaningless because there is one tenant and
    /// the money is the operator's own.
    ///
    /// # Errors
    /// `cormex_auth.users` could not be queried.
    pub async fn user_for_schema(
        &self,
        schema: &cormex_store::SchemaName,
    ) -> Result<Option<String>, cormex_core::CormexError> {
        let Ok(acc) = self.accounts() else {
            return Ok(None);
        };
        sqlx::query_scalar("SELECT id::text FROM cormex_auth.users WHERE schema_name = $1")
            .bind(schema.as_str())
            .fetch_optional(&acc.pool)
            .await
            .map_err(|e| CormexError::Store(format!("could not resolve the schema's owner: {e}")))
    }

    pub fn blob_backend(&self) -> &'static str {
        self.inner.blobs.backend()
    }

    /// Vectorization coverage, reported in `/health`. The mock backend has no
    /// database to query, so it returns `None`.
    pub async fn embedding_health(&self) -> Option<crate::dto::EmbeddingHealth> {
        match &self.inner.backend {
            Backend::Mock => None,
            Backend::Live(l) => Some(l.embedding_health().await),
        }
    }

    /// Whether this deployment can sign presigned URLs. See
    /// [`MediaStore::supports_presign`].
    #[must_use]
    pub fn supports_presign(&self) -> bool {
        self.inner.blobs.supports_presign()
    }

    // ────────────────────────── media ───────────────────────

    /// Server-relayed upload: object storage → the `blobs` row. The first two of
    /// the three fixed steps in §9.
    pub async fn upload_blob(
        &self,
        tenant: &Tenant,
        bytes: Bytes,
        declared_mime: Option<&str>,
    ) -> Result<BlobDto> {
        // Keep a copy for transcription. Under content addressing these bytes
        // are the final content, so there is no need to fetch them back from
        // object storage after the row lands
        let for_transcribe = bytes.clone();
        let stored = self.inner.blobs.put(bytes, declared_mime).await?;
        self.register(
            tenant,
            &stored.hash,
            &stored.mime,
            stored.size_bytes,
            stored.deduplicated,
            Some(for_transcribe),
        )
        .await
    }

    /// A user's `Store`, **entered from a user id rather than request headers**.
    ///
    /// Background jobs (snapshots) hold only the owner recorded in a sandbox
    /// token and no access token, so `AppState::tenant`'s path is unavailable to
    /// them. Details in [`Self::tenant_for_user`].
    ///
    /// # Errors
    /// Tenant resolution failed, or the mock backend has no database.
    pub async fn tenant_store_for_user(
        &self,
        user_id: &str,
    ) -> Result<std::sync::Arc<cormex_store::Store>> {
        match self.tenant_for_user(user_id).await.map_err(|e| {
            CormexError::Store(format!("could not resolve the tenant for {user_id}: {e:?}"))
        })? {
            crate::request_tenant::Tenant::Live { store, .. } => Ok(store),
            crate::request_tenant::Tenant::Mock => Err(CormexError::Unavailable(
                "this instance runs on the mock backend, so there is no database to write to"
                    .into(),
            )),
        }
    }

    /// Sign a direct-upload URL. The client **computes the hash first** and then
    /// asks — content addressing has no "upload first and decide the key
    /// afterwards".
    pub async fn presign_upload(&self, hash: &str) -> Result<BlobPresignResponse> {
        // Probe first: this content may well already be there (forwarding the
        // same image, re-uploading across devices). Telling the client "no need
        // to upload" is content addressing's most direct payoff on mobile
        let already_uploaded = self.inner.blobs.exists(hash).await?;
        let url = self.inner.blobs.presign_put(hash).await?;
        Ok(BlobPresignResponse {
            url,
            method: "PUT",
            expires_in_secs: PRESIGN_TTL.as_secs(),
            already_uploaded,
        })
    }

    /// Registration after a direct upload completes.
    ///
    /// # How far the client is trusted here
    ///
    /// - **The hash**: not re-verified. Re-verifying would mean pulling the
    ///   whole object down and recomputing, which exactly cancels the bandwidth
    ///   direct upload saved — the presign path would have been laid for
    ///   nothing. In a star topology the uploader is the owner of the data, so
    ///   lying about the hash only pollutes their own memory database.
    /// - **The byte count**: not re-verified. `BlobStore` has no HEAD
    ///   capability, and the only alternative is again a full fetch. It affects
    ///   only `Content-Length` and the denominator of a range, so getting it
    ///   wrong shows up as a player asking once too many or too few times; it
    ///   cannot corrupt content.
    /// - **The MIME**: **not trusted**. Only the first few KB are pulled and
    ///   sniffed, matching the rule on the direct-upload path — write
    ///   `application/octet-stream` into `blobs.mime` and this content is a
    ///   black hole for the future transcription pipeline, by which point it has
    ///   already sunk to the bottom of the database.
    ///
    /// The one hard check is that **the object really is there**: if it is not,
    /// the `blobs` row is never written, or it would leave a dangling reference
    /// whose content can never be fetched — and that cannot heal itself.
    pub async fn commit_blob(&self, tenant: &Tenant, req: BlobCommitRequest) -> Result<BlobDto> {
        if req.size_bytes < 0 {
            return Err(CormexError::Invalid(format!(
                "size_bytes must not be negative: {}",
                req.size_bytes
            )));
        }
        if !self.inner.blobs.exists(&req.hash).await? {
            return Err(CormexError::Invalid(format!(
                "object {} is not in object storage; PUT it to the presigned URL before registering it",
                req.hash
            )));
        }

        let mime = self
            .inner
            .blobs
            .sniff_mime(&req.hash, req.mime.as_deref())
            .await?;
        // On this path the server never handled the bytes, so only the client
        // knows whether an upload actually happened — deduplication is
        // therefore left entirely to `register`, deciding by the existence of a
        // blobs row
        self.register(tenant, &req.hash, &mime, req.size_bytes, false, None)
            .await
    }

    /// Register the blob row. `bytes` is present only on the relayed path —
    /// with presigned direct upload the server never handled the bytes at all,
    /// and transcription has to wait for a future background rescan (not yet
    /// implemented, recorded in the roadmap).
    async fn register(
        &self,
        tenant: &Tenant,
        hash: &str,
        mime: &str,
        size_bytes: i64,
        deduplicated: bool,
        bytes: Option<bytes::Bytes>,
    ) -> Result<BlobDto> {
        let seq = match &self.inner.backend {
            // mock has no blobs table. Fake a cursor advance so the client's
            // "receive a bump → backfill" path is exercised during offline
            // development too
            Backend::Mock => {
                let cursor = MOCK_CURSOR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                self.inner.bus.publish(SyncEvent::Bump { cursor });
                Some(cursor)
            }
            Backend::Live(l) => {
                let seq = l
                    .bind(tenant.store()?)
                    .register_blob(cormex_store::NewBlob {
                        hash: hash.to_owned(),
                        mime: mime.to_owned(),
                        size_bytes,
                        // The same prefix blobs::MediaStore uses — that side
                        // writes the object, this side writes the row pointing
                        // at it, and the two must agree.
                        //
                        // The prefix follows the tenant. Hard-coding "public"
                        // would crowd everybody's objects under one prefix —
                        // which is exactly what "add a prefix to trade away a
                        // class of GC bug" avoids: A deleting its last reference
                        // while the bytes are still referenced by B
                        storage_key: cormex_blob::hash::storage_key(
                            &cormex_blob::TenantPrefix::new(tenant.schema()).map_err(|e| {
                                CormexError::Invalid(format!("the tenant prefix is invalid: {e}"))
                            })?,
                            hash,
                        )?,
                    })
                    .await?;

                // Transcribe only on **first** registration (seq.is_some()):
                // under content addressing a repeated upload is the same bytes,
                // the transcription result would be identical, and redoing it
                // burns model money for nothing
                if let (Some(_), Some(b)) = (seq, bytes) {
                    l.spawn_transcription(hash.to_owned(), b, mime.to_owned());
                }
                seq
            }
        };

        Ok(BlobDto {
            hash: hash.to_owned(),
            mime: mime.to_owned(),
            size_bytes,
            // register_blob returning None means "already registered", which is
            // more authoritative than object storage's own deduplication
            // verdict — it looks at the blobs table, not at "were bytes
            // uploaded this time"
            deduplicated: deduplicated || seq.is_none(),
            seq,
        })
    }

    /// The metadata needed when fetching: MIME and total length.
    ///
    /// The total length is the denominator of `Content-Range` — without it a
    /// 206 is impossible, and without a 206 a player can only pull the whole
    /// file from the start.
    /// Store one captured chunk's bytes at an exact key (D16).
    ///
    /// Deliberately **not** tenant-parameterized the way the media methods are:
    /// the key is complete, and it was built from the request's tenant. The
    /// media path derives its key from `MediaStore::current_tenant()`, which is
    /// hard-coded to `public` until multi-tenancy reaches object storage —
    /// deriving a capture key from that would file every tenant's conversations
    /// under user #1.
    pub async fn put_capture_object(&self, key: &str, bytes: Bytes) -> Result<()> {
        self.inner
            .blobs
            .put_keyed(key, bytes, "application/zstd")
            .await
    }

    /// Read a captured chunk's stored bytes by its key.
    ///
    /// The masker and the projector both need this: the object is zstd, and
    /// what they work on is the logical bytes.
    pub async fn get_capture_object(&self, key: &str) -> Result<Bytes> {
        self.inner.blobs.get_keyed(key).await
    }

    pub async fn blob_meta(&self, tenant: &Tenant, hash: &str) -> Result<(String, i64)> {
        // Validate the shape before querying the database. Skipping it would not
        // be unsafe (the storage layer stops it anyway), but a malformed hash
        // would first run into "there is no such row in the blobs table" and
        // become a 404 — what the client is told is "not found", while the truth
        // is "what you sent is not a hash at all".
        cormex_blob::hash::validate_hash(hash)?;

        match &self.inner.backend {
            // mock has no blobs table, so it can only compute from the object
            // itself. One full fetch is acceptable: this path runs only during
            // offline development and in the client's CI
            Backend::Mock => {
                let bytes = self.inner.blobs.get(hash).await?;
                let mime = cormex_blob::probe_media(&bytes).resolve_mime(None);
                Ok((mime, bytes.len() as i64))
            }
            Backend::Live(l) => {
                let b = l.bind(tenant.store()?).blob_meta(hash).await?;
                Ok((b.mime, b.size_bytes))
            }
        }
    }

    pub async fn blob_bytes(&self, hash: &str, range: Option<Range<u64>>) -> Result<Bytes> {
        match range {
            Some(r) => self.inner.blobs.get_range(hash, r).await,
            None => self.inner.blobs.get(hash).await,
        }
    }

    /// A direct download URL. Phones playing video use it, saving the half of
    /// the bandwidth that relaying through cormexd would cost.
    pub async fn blob_download_url(&self, tenant: &Tenant, hash: &str) -> Result<BlobUrlResponse> {
        // Confirm this blob **was registered** before signing a URL. Signing
        // directly would turn "object storage happens to hold this key" into
        // probeable information, and would also hand the client content the
        // database knows nothing about
        let _ = self.blob_meta(tenant, hash).await?;
        Ok(BlobUrlResponse {
            url: self.inner.blobs.presign_get(hash).await?,
            expires_in_secs: PRESIGN_TTL.as_secs(),
        })
    }

    /// The local agent writing one conversational turn back into the memory
    /// database. See [`Live::write_episode`].
    ///
    /// Under the mock backend it **does not pretend the write succeeded**: the
    /// local agent would take that to mean the memory landed and cross that
    /// entry off its queue — and that really would lose data.
    ///
    /// `origin` decides which trust tier the extracted facts land in — see
    /// [`cormex_memory::extract::TurnOrigin`].
    pub async fn write_episode(
        &self,
        tenant: &Tenant,
        req: NewEpisodeRequest,
        origin: cormex_memory::extract::TurnOrigin,
    ) -> Result<EpisodeAck> {
        match &self.inner.backend {
            Backend::Mock => Err(CormexError::Unavailable(
                "this instance runs on the mock backend, so there is no memory database to write to".into(),
            )),
            Backend::Live(l) => l.bind(tenant.store()?).write_episode(req, origin).await,
        }
    }

    /// The LLM proxy — forward one streaming call to the provider verbatim.
    ///
    /// The local agent takes this path by default: the API key lives in exactly
    /// one place on the server, so it does not have to be configured on every
    /// device. It **touches no memory and writes to no database**; it is pure
    /// forwarding.
    ///
    /// Under the mock backend there is nothing to forward to, so it returns
    /// `Unavailable` rather than fabricating a stream: a fake token stream would
    /// leave the local agent looking functional right up until somebody asks why
    /// its answers have nothing to do with the question.
    pub async fn llm_stream(
        &self,
        req: LlmStreamRequest,
        own: Option<(&str, &str, Option<&str>)>,
    ) -> Result<MessageStream> {
        match &self.inner.backend {
            Backend::Mock => Err(CormexError::Unavailable(
                "this instance runs on the mock backend, so there is no LLM provider available"
                    .into(),
            )),
            Backend::Live(l) => l.llm_stream_with_key(req, own).await,
        }
    }

    // ────────────────────────── memory ──────────────────────

    pub async fn memory_search(
        &self,
        tenant: &Tenant,
        q: MemorySearchQuery,
    ) -> Result<MemorySearchResponse> {
        match &self.inner.backend {
            Backend::Mock => {
                let facts = mock_facts()
                    .into_iter()
                    .filter(|f| q.q.is_empty() || f.statement.contains(&q.q))
                    // Ordinary retrieval **cannot see invalidated facts** (the
                    // live backend's four retrieval paths query active_facts
                    // only); replay still returns them, merely carrying
                    // invalidated=true. This distinction must hold on mock as
                    // well, or the client CI's assertion "no invalidated fact
                    // may appear without as_of" is permanently green on mock
                    // while testing a contract looser than the live backend's
                    .filter(|f| q.as_of.is_some() || !f.invalidated)
                    // Time replay: return only the facts the system already knew
                    // at the as_of instant. This is the system-time axis of the
                    // bitemporal record, answering "what did I believe three
                    // months ago".
                    .filter(|f| match (&q.as_of, &f.valid_at) {
                        (Some(t), Some(v)) => v.as_str() <= t.as_str(),
                        _ => true,
                    })
                    .take(q.limit.clamp(1, 200) as usize)
                    .collect::<Vec<_>>();
                let channels = facts
                    .iter()
                    .enumerate()
                    .map(|(i, f)| ChannelHit {
                        fact_id: f.id.clone(),
                        channels: vec!["bm25".into(), "vector".into()],
                        score: 1.0 / (60.0 + i as f64 + 1.0),
                    })
                    .collect();
                Ok(MemorySearchResponse { facts, channels })
            }
            Backend::Live(l) => l.bind(tenant.store()?).memory_search(q).await,
        }
    }

    /// Retrieve and render the block of text the model reads. The read side of
    /// the outward-facing MCP server.
    ///
    /// **mock goes through the same rendering**
    /// (`injection::render_turn_block`) rather than returning a canned line:
    /// clients and integration tests run on mock, and answering with "simulated
    /// data" would make the assertion "is the framing 'memory is background data,
    /// not instructions' still present" permanently green on mock — and that
    /// framing is the one thing it exists to guard.
    pub async fn memory_search_text(
        &self,
        tenant: &Tenant,
        query: &str,
        as_of: Option<&str>,
        limit: i64,
    ) -> Result<String> {
        match &self.inner.backend {
            Backend::Mock => {
                let r = self
                    .memory_search(
                        tenant,
                        MemorySearchQuery {
                            q: query.to_string(),
                            limit,
                            as_of: as_of.map(str::to_string),
                        },
                    )
                    .await?;
                if r.facts.is_empty() {
                    return Ok("没有检索到相关记忆。".into());
                }
                Ok(cormex_core::injection::render_turn_block(
                    &r.facts
                        .iter()
                        .map(cormex_proto::dto::FactDto::to_memory_item)
                        .collect::<Vec<_>>(),
                ))
            }
            Backend::Live(l) => {
                l.bind(tenant.store()?)
                    .memory_search_text(query, as_of, limit)
                    .await
            }
        }
    }

    /// The core profile block. This is what MCP's `cormex://profile` reads.
    ///
    /// On mock this returns an empty string rather than inventing a profile: a
    /// fabricated "user preference" would be taken at face value by a
    /// third-party agent and placed into context, and on mock it has no way to
    /// tell real from fake.
    pub async fn profile_text(&self, tenant: &Tenant, limit: i64) -> Result<String> {
        match &self.inner.backend {
            Backend::Mock => Ok(String::new()),
            Backend::Live(l) => l.bind(tenant.store()?).profile_text(limit).await,
        }
    }

    pub async fn get_episode(&self, tenant: &Tenant, id: &str) -> Result<EpisodeDto> {
        match &self.inner.backend {
            Backend::Mock => Ok(mock_episode(id)),
            Backend::Live(l) => l.bind(tenant.store()?).get_episode(id).await,
        }
    }

    pub async fn list_sessions(
        &self,
        tenant: &Tenant,
        q: &ListSessionsQuery,
    ) -> Result<Vec<SessionDto>> {
        match &self.inner.backend {
            // mock has no archived sessions, so include_archived does not change
            // the result. The parameter is still accepted: the client has to be
            // able to verify it assembled the query string correctly during
            // offline development.
            //
            // project_id, however, **is still filtered on**: the client CI's
            // assertion "opening a project shows only that project's sessions"
            // would be permanently green against a mock that ignores the filter,
            // while what it tests is precisely whether that query string was
            // assembled correctly
            Backend::Mock => Ok(mock_sessions()
                .into_iter()
                .filter(|s| q.project_id.is_none() || s.project_id == q.project_id)
                .collect()),
            Backend::Live(l) => l.bind(tenant.store()?).list_sessions(q).await,
        }
    }

    // ────────────────────────── projects ────────────────────

    pub async fn list_projects(&self, tenant: &Tenant) -> Result<Vec<ProjectDto>> {
        match &self.inner.backend {
            Backend::Mock => Ok(mock_projects()),
            Backend::Live(l) => l.bind(tenant.store()?).list_projects().await,
        }
    }

    pub async fn create_project(&self, tenant: &Tenant, name: &str) -> Result<ProjectDto> {
        match &self.inner.backend {
            // mock persists nothing, but it **still runs the validation**: the
            // client CI's assertion "an empty name is rejected" has to hold on
            // mock too, or it tests a contract looser than the live backend's
            Backend::Mock => Ok(ProjectDto {
                id: Id::new().to_string(),
                name: crate::live::validate_project_name(name)?,
                created_at: Utc::now().to_rfc3339(),
                session_count: 0,
            }),
            Backend::Live(l) => l.bind(tenant.store()?).create_project(name).await,
        }
    }

    pub async fn patch_project(
        &self,
        tenant: &Tenant,
        id: &str,
        patch: ProjectPatch,
    ) -> Result<ProjectDto> {
        match &self.inner.backend {
            Backend::Mock => {
                let name = patch.name.as_deref().ok_or_else(|| {
                    CormexError::Invalid(
                        "the request body contains no field to change (name)".into(),
                    )
                })?;
                let mut p = mock_projects()
                    .into_iter()
                    .find(|p| p.id == id)
                    .unwrap_or_else(|| mock_projects().swap_remove(0));
                p.id = id.to_string();
                p.name = crate::live::validate_project_name(name)?;
                Ok(p)
            }
            Backend::Live(l) => l.bind(tenant.store()?).patch_project(id, patch).await,
        }
    }

    pub async fn delete_project(&self, tenant: &Tenant, id: &str) -> Result<()> {
        match &self.inner.backend {
            Backend::Mock => Ok(()),
            Backend::Live(l) => l.bind(tenant.store()?).delete_project(id).await,
        }
    }

    pub async fn patch_session(
        &self,
        tenant: &Tenant,
        id: &str,
        patch: SessionPatch,
    ) -> Result<SessionDto> {
        match &self.inner.backend {
            // mock persists nothing, but it **still runs the validation**: the
            // client CI's assertion "binding to C:\Windows is rejected" has to
            // hold on mock too, or it tests a contract looser than the live
            // backend's
            Backend::Mock => {
                let mut s = mock_sessions()
                    .into_iter()
                    .find(|s| s.id == id)
                    .unwrap_or_else(|| mock_sessions().swap_remove(0));
                s.id = id.to_string();
                if let Some(t) = patch.title.as_deref() {
                    if t.trim().is_empty() {
                        return Err(CormexError::Invalid("the title must not be blank".into()));
                    }
                    s.title = t.trim().to_string();
                    s.title_is_custom = true;
                }
                if let Some(a) = patch.archived {
                    s.archived = a;
                }
                // **Use live's decision, do not write a second copy of it.**
                //
                // This used to be `cormex_agent::workspace::validate` — i.e.
                // mock **accepted** the binding while live refused every one
                // (see the reasoning in `live::workspace_patch`). That forked
                // the contract's two implementations: the client CI runs on
                // mock, so "the server does not accept the binding" was never
                // verified in CI, and its opposite was verified instead.
                if crate::live::workspace_patch(patch.workspace.as_ref().map(Option::as_deref))? {
                    s.workspace = None;
                }
                // The three states are reproduced faithfully: the client CI has
                // to be able to verify the contract "an explicit null removes,
                // an absent field leaves it alone", and that cannot be tested
                // against a mock that ignores project_id
                if let Some(p) = &patch.project_id {
                    s.project_id = p.clone();
                }
                Ok(s)
            }
            Backend::Live(l) => l.bind(tenant.store()?).patch_session(id, patch).await,
        }
    }

    pub async fn session_detail(
        &self,
        tenant: &Tenant,
        id: &str,
        q: &SessionDetailQuery,
    ) -> Result<SessionDetail> {
        match &self.inner.backend {
            Backend::Mock => {
                let session = mock_sessions()
                    .into_iter()
                    .find(|s| s.id == id)
                    .ok_or_else(|| CormexError::NotFound {
                        kind: "session",
                        id: id.into(),
                    })?;
                // mock has only one message, so there is nothing to page
                // through. But the cursor **is still validated** — the client
                // CI's assertion "a bad cursor gets a 400" has to hold on mock
                // too, or it tests a contract looser than the live backend's
                if let Some(raw) = q.before.as_deref() {
                    let _ = crate::cursor::decode(raw)?;
                }
                Ok(SessionDetail {
                    episodes: vec![mock_episode("01JEPISODE000000000000001")],
                    session,
                    has_more: false,
                    next_cursor: None,
                })
            }
            Backend::Live(l) => l.bind(tenant.store()?).session_detail(id, q).await,
        }
    }

    // ─────────────────────────── sync ───────────────────────

    pub async fn sync_since(&self, tenant: &Tenant, q: SyncQuery) -> Result<SyncResponse> {
        match &self.inner.backend {
            Backend::Mock => {
                // There is no real sync_log yet, but the cursor semantics are
                // exercised first: the client can verify the convergence logic
                // "an empty page means we have caught up", rather than
                // discovering a protocol misunderstanding after wiring it up.
                let _ = q.limit.clamp(1, 1000);
                Ok(SyncResponse {
                    cursor: q.since,
                    records: vec![],
                    has_more: false,
                })
            }
            Backend::Live(l) => l.bind(tenant.store()?).sync_since(q).await,
        }
    }
}

// ─────────────────────── the mock data source ──────────────

fn mock_episode(id: &str) -> EpisodeDto {
    EpisodeDto {
        id: id.to_string(),
        session_id: "01JSESSION0000000000000001".into(),
        role: "user".into(),
        text: Some(
            "Let's start with RustFS for object storage; one volume is enough for v1.".into(),
        ),
        occurred_at: Utc::now().to_rfc3339(),
        attachments: vec![],
        // Fabricate one injection attribution so the client can actually draw
        // the "why do you remember this" drawer during offline development —
        // same reasoning as mock_chat_stream fabricating memory events
        memories: vec![InjectedMemoryDto {
            fact_id: "01JFACT00000000000000001".into(),
            statement: Some("Cormex uses RustFS for object storage".into()),
            domain: Some("coding".into()),
            channels: vec!["bm25".into(), "vector".into()],
            score: Some(0.032),
            invalidated: false,
            source_episode_id: Some("01JEPISODE000000000000000".into()),
        }],
        tool_calls: vec![],
    }
}

/// The fake project. **Deliberately only one**, with exactly one of the mock
/// sessions belonging to it and one not — the client has to be able to draw
/// both "a session inside a project" and "ungrouped" during offline
/// development, and if both samples were in the same project, the ungrouped
/// branch of the UI would never be executed once.
fn mock_projects() -> Vec<ProjectDto> {
    vec![ProjectDto {
        id: MOCK_PROJECT.into(),
        name: "Cormex In-House".into(),
        created_at: Utc::now().to_rfc3339(),
        session_count: 1,
    }]
}

/// The id of that project in mock. Both the sessions and the projects reference
/// it, and written as a literal in two places they would eventually disagree —
/// the symptom of which is "projects are always empty on mock".
const MOCK_PROJECT: &str = "01JPROJECT0000000000000001";

fn mock_sessions() -> Vec<SessionDto> {
    let now = Utc::now().to_rfc3339();
    vec![
        SessionDto {
            id: "01JSESSION0000000000000001".into(),
            title: "Picking the tech stack".into(),
            created_at: now.clone(),
            updated_at: now.clone(),
            message_count: 12,
            preview: Some("A single RustFS volume it is, then.".into()),
            title_is_custom: false,
            archived: false,
            workspace: None,
            runtime: cormex_proto::dto::SessionRuntimeDto::Cloud,
            project_id: Some(MOCK_PROJECT.into()),
        },
        SessionDto {
            id: "01JSESSION0000000000000002".into(),
            title: "Memory schema design".into(),
            created_at: now.clone(),
            updated_at: now,
            message_count: 34,
            preview: Some("sync_log uses one cursor across every table.".into()),
            title_is_custom: false,
            archived: false,
            // Deliberately no invented path: mock's consumer is the client's
            // CI, and no invented path exists on that machine, so if the UI
            // renders "nonexistent means unbound" there would be no visible
            // difference anyway. To exercise the bound state, PATCH a real
            // directory in
            workspace: None,
            runtime: cormex_proto::dto::SessionRuntimeDto::Cloud,
            // The ungrouped one; reasoning in [`mock_projects`]
            project_id: None,
        },
    ]
}

/// The **source channels in the fake data are deliberately all different**.
///
/// Same reasoning as the "already overturned" sample below: the source badge is
/// something the client has to draw, and if all four were `user_stated`, the
/// branch of the UI that makes "stated by the user / inferred / tool output"
/// look different would never be executed once before the live backend is wired
/// up. The last entry is given `None`, covering rows that predate the column
/// (`unknown_legacy`).
fn mock_facts() -> Vec<FactDto> {
    vec![
        FactDto {
            id: Id::new().to_string(),
            statement: "Cormex uses RustFS for object storage".into(),
            predicate: Some("uses_object_storage".into()),
            domain: Some("coding".into()),
            confidence: 0.95,
            valid_at: Some("2026-08-06T00:00:00Z".into()),
            created_at: Utc::now().to_rfc3339(),
            invalidated: false,
            source_episode_id: Some(Id::new().to_string()),
            source_channel: Some("user_stated".into()),
            trust_tier: Some(1),
        },
        FactDto {
            id: Id::new().to_string(),
            statement: "Every GUI is Flutter — one codebase across six platforms".into(),
            predicate: Some("decided".into()),
            domain: Some("coding".into()),
            confidence: 0.9,
            valid_at: Some("2026-08-07T00:00:00Z".into()),
            created_at: Utc::now().to_rfc3339(),
            invalidated: false,
            source_episode_id: Some(Id::new().to_string()),
            source_channel: Some("conversation".into()),
            trust_tier: Some(2),
        },
        // One **already overturned** fact. Same reasoning as fabricating memory
        // events and fabricating cursor advances: the badge saying "this no
        // longer holds" is something the client has to draw, and without a
        // sample like this, that branch of the UI executes for the first time
        // only after the live backend is wired up and a fact has actually been
        // redacted or superseded
        FactDto {
            id: Id::new().to_string(),
            statement: "The sync protocol uses a bare BIGSERIAL cursor".into(),
            predicate: Some("decided".into()),
            domain: Some("coding".into()),
            confidence: 0.72,
            valid_at: Some("2026-08-05T00:00:00Z".into()),
            created_at: Utc::now().to_rfc3339(),
            invalidated: true,
            source_episode_id: Some(Id::new().to_string()),
            source_channel: Some("tool_output".into()),
            trust_tier: Some(3),
        },
        FactDto {
            id: Id::new().to_string(),
            statement: "The sync protocol uses a sync_log outbox serialized by an advisory lock"
                .into(),
            predicate: Some("decided".into()),
            domain: Some("coding".into()),
            confidence: 1.0,
            valid_at: Some("2026-08-07T00:00:00Z".into()),
            created_at: Utc::now().to_rfc3339(),
            invalidated: false,
            source_episode_id: Some(Id::new().to_string()),
            source_channel: None,
            trust_tier: None,
        },
    ]
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod backend_choice_tests {
    use super::BackendChoice;

    /// The default must be live, and an unrecognised value must **error rather
    /// than fall back**.
    ///
    /// This guards the completeness of one fix. The original behaviour was
    /// "silently run mock when the database is unreachable", whose consequence
    /// was that cormexd could serve traffic backed by a database of fabricated
    /// memory while looking completely normal. The fix was to make mock an
    /// explicit choice — but if a mistyped value fell back to some default, that
    /// door would still be open: a `CORMEX_BACKEND=moc` would run as something
    /// else while its author believed they had configured mock, and vice versa.
    #[test]
    fn mock_must_be_asked_for_by_name() {
        assert_eq!(BackendChoice::parse("live").unwrap(), BackendChoice::Live);
        assert_eq!(BackendChoice::parse("mock").unwrap(), BackendChoice::Mock);
        assert_eq!(BackendChoice::parse("MOCK").unwrap(), BackendChoice::Mock);
        assert_eq!(
            BackendChoice::parse("  mock  ").unwrap(),
            BackendChoice::Mock
        );

        let err = BackendChoice::parse("moc")
            .expect_err("a mistyped value must error and must not fall back");
        assert!(
            err.to_string().contains("CORMEX_BACKEND"),
            "the error must name which variable is at fault; actual: {err}"
        );

        // An empty string is treated as "not set" on the from_env side
        // (compose's `${VAR:-}`), but passing an empty string directly to parse
        // is the caller's mistake and should error
        assert!(BackendChoice::parse("").is_err());
    }
}

//! Multi-user: one schema per user, one pool per schema.
//!
//! # Why not just add `user_id` to every table
//!
//! There are currently 17 tables and roughly 77 queries. Adding a column means
//! 77 places that have to **remember** to filter, and the consequence of
//! missing any one of them is **one user reading another user's memory** — with
//! no compile-time help, nothing in the existing tests that would naturally
//! cover it, and a symptom that is not an error but "a few unfamiliar memories
//! showed up".
//!
//! Switching to schema isolation means **not one** of those 77 queries has to
//! change: the connection's `search_path` contains only this user's schema, so
//! other people's tables are not even in scope for name resolution. Isolation
//! goes from "remember to write the WHERE in every place" to "pinned down once,
//! when the connection is established".
//!
//! This approach has been proven inside this repository for a long time — the
//! integration tests' `common::setup` builds a dedicated schema per test
//! exactly this way (`tests/common/mod.rs`). What happens here is moving the
//! same technique from tests into production.
//!
//! # Why "one pool per user" rather than "a shared pool + SET search_path each
//! time"
//!
//! A shared pool saves connections, but it demotes isolation from a
//! **structural** property to a **disciplinary** one: every checked-out
//! connection has to remember to `SET search_path` first, and missing it once
//! means querying with the previous user's search_path. Connections are reused,
//! so that one miss **very likely does land in somebody else's schema**.
//!
//! Welding it into [`PgConnectOptions`] makes the mistake physically
//! impossible: from creation to close, that connection can see exactly one
//! schema. The price is connection count, and that is a number you can work out
//! exactly (see the comments on [`TenantPools::new`]) rather than a data leak
//! that happens silently.
//!
//! # The ceiling is a real ceiling
//!
//! Postgres's `max_connections` defaults to 100. At
//! [`PER_TENANT_CONNECTIONS`] per user, the number of simultaneously resident
//! users has a hard ceiling. When it is reached we **fail with an explicit
//! error** rather than queueing — queueing shows up as "everybody got slower",
//! whereas an error at least points at the cause.

use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::Arc;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Mutex;

use crate::error::{Result, StoreError};
use crate::store::Store;

/// The connection pool ceiling for each user.
///
/// Far smaller than the 16 used in the single-user case, and that is
/// **deliberate**: in a single-user deployment the pool serves one person's
/// four-way retrieval, whereas under multi-user the same total budget has to be
/// split across everybody online.
///
/// 4 is enough for one conversation (concurrent four-way retrieval + one write
/// transaction), and beyond that the returns diminish — write transactions are
/// serialized by the advisory lock anyway.
pub const PER_TENANT_CONNECTIONS: u32 = 4;

/// How many users' pools stay resident at once.
///
/// `4 × 20 = 80`, leaving headroom within Postgres's default 100 for
/// operational connections and `pg_basebackup`. **This number is derived
/// backwards from `max_connections`**, not picked out of the air — change the
/// database side first, then this.
pub const MAX_RESIDENT_TENANTS: usize = 20;

/// One user's schema name.
///
/// Newly minted ones look like `u_<26-character ULID>`; **user #1 is `public`**
/// — the production database already holds existing data, and moving 17 tables
/// into a new schema is a one-way DDL whose only reward is tidiness. See the
/// file header of `migrations/20260810000002`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchemaName(String);

impl SchemaName {
    /// # Errors
    /// The shape is wrong. **This is an injection surface**: this string gets
    /// spliced into `search_path`, and that statement cannot use bind
    /// parameters (identifiers are not parameterizable).
    ///
    /// So there is no escaping and no quoting here, only an **allowlist**:
    /// escaping requires every splice site to remember to do it, whereas an
    /// allowlist is written once, and getting it wrong means rejection rather
    /// than execution.
    /// # Why it must be **lowercase**
    ///
    /// Postgres **folds** unquoted identifiers in `search_path` to lowercase.
    /// But this same name is also used for `CREATE SCHEMA "u_..."` (quoted,
    /// preserving case) — and the moment the two disagree, `search_path` points
    /// at a **schema that does not exist**.
    ///
    /// And Postgres **silently skips** a nonexistent schema in `search_path`:
    /// no error, no warning, it just falls through to the next one (which is
    /// `public`). The observed consequence was that a new tenant's migration
    /// created no tables at all and every write went into user #1's database,
    /// while `migrate()` returned `Ok`.
    ///
    /// Lowercase makes the quoted and unquoted spellings equivalent, which
    /// closes that path off. The Crockford base32 that ULIDs use is
    /// case-insensitive to begin with, so lowercasing loses no information.
    pub fn new(raw: &str) -> Result<Self> {
        let ok = raw == "public"
            || (raw.len() == 28
                && raw.starts_with("u_")
                && raw[2..].bytes().all(|b| {
                    b.is_ascii_digit() || (b.is_ascii_lowercase() && !b"ilou".contains(&b))
                }));
        if !ok {
            return Err(StoreError::Invalid(format!(
                "invalid schema name: {raw} (only `public`, or `u_` + a 26-character **lowercase** ULID)"
            )));
        }
        Ok(Self(raw.to_owned()))
    }

    /// Derive a new schema name from a user id.
    ///
    /// **Lowercased**; for why see [`Self::new`] — an uppercase name makes
    /// `search_path` point at a schema that does not exist, and that failure is
    /// silent.
    ///
    /// # Errors
    /// `user_id` is not a valid ULID.
    ///
    /// **Validation is mandatory**: this function once concatenated the string
    /// and returned it directly, so a malformed user_id could produce a
    /// `SchemaName` that [`Self::new`] itself would reject — while the entire
    /// value of this type is "holding one == it has already been validated".
    /// If the constructor does not validate, every downstream site
    /// (`search_path`, `pg_notify`, the object-storage prefix) has to validate
    /// again itself, which is exactly what this newtype exists to eliminate.
    pub fn derive(user_id: &str) -> Result<Self> {
        Self::new(&format!("u_{}", user_id.to_ascii_lowercase()))
    }

    /// The schema for user #1 / a single-user deployment.
    ///
    /// A constant rather than `new("public").unwrap()`: the latter leaves the
    /// impression of "this could panic" at every call site, when in fact it
    /// never can.
    #[must_use]
    pub fn public() -> Self {
        Self("public".to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// This user's write-transaction lock key.
    ///
    /// `pg_advisory_xact_lock` used to take a single global key
    /// (`SYNC_ADVISORY_LOCK_KEY`), which serialized the write transactions of
    /// every user — one person importing three years of history and nobody else
    /// can write a single sentence. What that lock guarantees is "the order of
    /// `sync_log.seq` == the order rows become visible to readers", and **that
    /// is a per-schema matter to begin with**.
    #[must_use]
    pub fn lock_key(&self) -> i32 {
        // A stable hash of the schema name. A collision only means two users
        // share one lock — the result is still correct, those two people's
        // writes merely queue behind each other. So no cryptographic hash is
        // needed here; what is needed is **stability across processes and
        // across versions**
        let mut h: u32 = 2_166_136_261;
        for b in self.0.as_bytes() {
            h ^= u32::from(*b);
            h = h.wrapping_mul(16_777_619);
        }
        #[allow(clippy::cast_possible_wrap)]
        (h as i32)
    }

    /// This user's NOTIFY channel name.
    ///
    /// There used to be one global `cortex_sync`, so anybody's write woke
    /// **everybody's** listener. Not a correctness problem (each catches up
    /// according to its own cursor), but it let every user observe that
    /// "somebody else is writing", and at scale it becomes an all-hands storm.
    #[must_use]
    pub fn notify_channel(&self) -> String {
        // The channel name likewise cannot go through a bind parameter; what
        // makes it safe is that this type has already passed allowlist
        // validation. The prefix comes from the sync module, which is the
        // listening side — if each place wrote its own literal, changing one
        // would mean "the trigger sends to A while the listener listens on B",
        // and that raises no error, it just stops refreshing
        format!("{}_{}", crate::sync::NOTIFY_CHANNEL_PREFIX, self.0)
    }
}

/// Fetch a [`Store`] by user; pools are built on demand and evicted by LRU.
pub struct TenantPools {
    database_url: String,
    resident: Mutex<Resident>,
}

/// The resident pools plus their usage order.
///
/// A `Mutex<..>` rather than a `DashMap`: the hot path is "hit an existing
/// pool", and that is one hash lookup plus one `Arc::clone`, holding the lock
/// for nanoseconds. What is genuinely slow is **building a pool** (a TCP
/// handshake plus authentication), and that has to happen outside the lock —
/// see [`TenantPools::get`].
struct Resident {
    pools: HashMap<SchemaName, Arc<Store>>,
    /// Most recently used at the back. With few entries (≤20), a linear scan
    /// is cheaper than maintaining another index.
    order: Vec<SchemaName>,
}

impl TenantPools {
    #[must_use]
    pub fn new(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
            resident: Mutex::new(Resident {
                pools: HashMap::new(),
                order: Vec::new(),
            }),
        }
    }

    /// Get this user's `Store`, building one if it does not exist.
    ///
    /// # Errors
    /// Cannot reach the database, or the resident user count has hit the
    /// ceiling (see [`MAX_RESIDENT_TENANTS`]).
    pub async fn get(&self, schema: &SchemaName) -> Result<Arc<Store>> {
        // Take one look under the lock first. On a hit we are done — this is
        // the path the vast majority of requests take
        {
            let mut r = self.resident.lock().await;
            if let Some(store) = r.pools.get(schema).cloned() {
                r.touch(schema);
                return Ok(store);
            }
        }

        // **Build the pool outside the lock**: it runs a TCP handshake plus
        // authentication, tens of milliseconds. Doing that while holding the
        // lock would leave every other user's request queued behind it — the
        // classic shape of "one person logging in stalls the whole service"
        let store = Arc::new(self.connect(schema).await?);

        let mut r = self.resident.lock().await;
        // Another request may have built one too while we were building ours.
        // Use whichever arrived first and drop our own (the pool closes its
        // connections when the `Arc` is dropped) — two pools pointing at the
        // same schema is not incorrect, it just wastes 4 connections
        if let Some(existing) = r.pools.get(schema).cloned() {
            r.touch(schema);
            return Ok(existing);
        }
        if r.pools.len() >= MAX_RESIDENT_TENANTS {
            r.evict_oldest();
        }
        // Still full after eviction means the ceiling itself is configured too
        // low
        if r.pools.len() >= MAX_RESIDENT_TENANTS {
            return Err(StoreError::Invalid(format!(
                "the cap on simultaneously resident users is reached: {MAX_RESIDENT_TENANTS} \
                 ({PER_TENANT_CONNECTIONS} connections each). That number is derived backwards \
                 from Postgres's max_connections -- to raise it, raise that one first"
            )));
        }
        r.pools.insert(schema.clone(), Arc::clone(&store));
        r.order.push(schema.clone());
        Ok(store)
    }

    /// Build a pool with `search_path` welded in place.
    ///
    /// Keeping `public` at the tail of `search_path` is **required**: the
    /// `vector` extension's types live there, and without it `::vector` reports
    /// "type does not exist" — an error that looks like "the extension is not
    /// installed". The test harness stepped into this same trap.
    async fn connect(&self, schema: &SchemaName) -> Result<Store> {
        let options = PgConnectOptions::from_str(&self.database_url)
            .map_err(|e| StoreError::Invalid(format!("DATABASE_URL is not valid: {e}")))?
            .options([("search_path", format!("{},public", schema.as_str()))]);

        let pool = PgPoolOptions::new()
            .max_connections(PER_TENANT_CONNECTIONS)
            .connect_with(options)
            .await?;
        Ok(Store::from_pool_in(pool, schema.clone()))
    }

    /// Create this schema (idempotent).
    ///
    /// Uses an admin connection rather than the tenant pool: the tenant pool's
    /// `search_path` points at a schema that does not exist yet, and Postgres
    /// **silently skips** such an entry — that connection falls through to
    /// `public`, so the act of "creating the schema" appears to succeed while
    /// nothing at all happened in the right place.
    ///
    /// # Errors
    /// Cannot connect, or lacks permission to create schemas.
    pub async fn create_schema(&self, schema: &SchemaName) -> Result<()> {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.database_url)
            .await?;
        // AssertSqlSafe is affordable here: `SchemaName`'s constructor is
        // allowlist-validated. The double quotes are required — an unquoted
        // identifier gets folded to lowercase, and the names we mint are
        // lowercase anyway, so quoting merely makes "what was created" and
        // "what search_path looks for" character-for-character identical
        let sql = format!(r#"CREATE SCHEMA IF NOT EXISTS "{}""#, schema.as_str());
        let result = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&admin).await;
        admin.close().await;
        result?;
        Ok(())
    }

    /// Drop this schema and everything in it.
    ///
    /// **Only for use when account provisioning failed partway through** (the
    /// schema was created but the account row was not, leaving an orphan).
    /// Dropping a real user's schema is irreversible data destruction; that
    /// path goes through purge, and purge must be explicitly triggered,
    /// confirmed twice, and leave a tombstone.
    ///
    /// # Errors
    /// Cannot connect, or lacks DROP permission.
    pub async fn drop_schema(&self, schema: &SchemaName) -> Result<()> {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.database_url)
            .await?;
        let sql = format!(r#"DROP SCHEMA IF EXISTS "{}" CASCADE"#, schema.as_str());
        let result = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&admin).await;
        admin.close().await;
        result?;
        Ok(())
    }

    /// How many pools are currently resident. For `/health` and for tests.
    pub async fn resident_count(&self) -> usize {
        self.resident.lock().await.pools.len()
    }
}

impl Resident {
    fn touch(&mut self, schema: &SchemaName) {
        if let Some(i) = self.order.iter().position(|s| s == schema) {
            let s = self.order.remove(i);
            self.order.push(s);
        }
    }

    /// Evict the least recently used one.
    ///
    /// Eviction merely **drops a reference**: requests currently using it still
    /// hold an `Arc`, and the pool does not actually close until the last one
    /// is done. So eviction never interrupts an in-flight request; it only
    /// makes the next request rebuild.
    fn evict_oldest(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let victim = self.order.remove(0);
        self.pools.remove(&victim);
        tracing::info!(
            schema = victim.as_str(),
            "the resident pool is full; evicting the least recently used tenant pool"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A schema name is an injection surface, so the allowlist must be
    /// strict.**
    ///
    /// It gets spliced into `search_path` and into the `pg_notify` channel
    /// name, and identifiers cannot use bind parameters. Letting through a name
    /// containing a quote or a comma amounts to letting the caller choose which
    /// schema to query — that is, to read somebody else's memory.
    #[test]
    fn a_schema_name_can_never_be_anything_but_the_two_shapes_we_mint() {
        for bad in [
            "public, evil",               // comma = smuggle a second entry into search_path
            "u_01ABC\"; DROP SCHEMA x--", // quote
            "u_01ABC'",
            "U_01arz3ndektsv4rrffq69g5fav",  // uppercase prefix
            "u_01ARZ3NDEKTSV4RRFFQ69G5FAV", // uppercase ULID -- search_path folds it to lowercase, pointing at a schema that does not exist
            "u_01arz3ndektsv4rrffq69g5fa",  // 25 characters, one short
            "u_01arz3ndektsv4rrffq69g5favx", // 27 characters
            "u_01arz3ndektsv4rrffq69g5fai", // contains i, which is not in the ULID alphabet
            "u_",
            "",
            "pg_catalog",
            "information_schema",
        ] {
            assert!(
                SchemaName::new(bad).is_err(),
                "this schema name must be rejected: {bad:?} -- it gets spliced into search_path"
            );
        }

        assert!(SchemaName::new("public").is_ok(), "user #1 lives in public");
        assert!(SchemaName::new("u_01arz3ndektsv4rrffq69g5fav").is_ok());
    }

    /// A derived name must pass its own validation — otherwise a new user is
    /// unusable the moment they are created.
    #[test]
    fn a_derived_name_passes_its_own_validation() {
        // ULIDs are uppercase by nature (`Id::new().to_string()` is), so
        // derivation must lowercase them
        let s = SchemaName::derive("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("a valid ULID");
        assert_eq!(
            s.as_str(),
            "u_01arz3ndektsv4rrffq69g5fav",
            "deriving an uppercase name makes search_path point at a schema that does not exist -- \
             and Postgres silently skips such an entry, with the observed consequence of writing \
             into somebody else's database"
        );
        assert!(
            SchemaName::new(s.as_str()).is_ok(),
            "a name we derived ourselves failing our own validation means the two rules have \
             drifted apart"
        );

        // Malformed input must be stopped right here rather than producing the
        // illusion of something "validated"
        for bad in ["", "not-a-ulid", "01ARZ3NDEKTSV4RRFFQ69G5FA", "../etc"] {
            assert!(
                SchemaName::derive(bad).is_err(),
                "derive let a malformed user_id through: {bad:?} -- the entire value of this type \
                 is that holding one means it has already been validated"
            );
        }
    }

    /// Both the lock key and the channel name must be determined **by the
    /// schema alone**, and must differ between users.
    ///
    /// The first half is a cross-process requirement: if two cortexd processes
    /// compute different keys, the lock buys nothing, and the symptom is
    /// `sync_log` occasionally dropping rows — extremely hard to track down.
    #[test]
    fn the_lock_key_and_channel_are_stable_and_per_tenant() {
        let a = SchemaName::new("u_01arz3ndektsv4rrffq69g5fav").unwrap();
        let b = SchemaName::new("u_01bx5zzkbkactav9wevgemmvrz").unwrap();

        assert_eq!(
            a.lock_key(),
            a.lock_key(),
            "the same schema must compute the same key every time"
        );
        assert_ne!(
            a.lock_key(),
            b.lock_key(),
            "two users sharing one lock only makes them queue behind each other (still correct), \
             but they should be separate here"
        );
        // Pin the literal: swapping in a different hash implementation would
        // make **two processes already running** compute different keys, and at
        // that point the lock is a formality. Changing it must be deliberate
        assert_eq!(
            a.lock_key(),
            304_439_192,
            "the lock key algorithm changed, which breaks cross-process consistency"
        );

        assert_eq!(
            a.notify_channel(),
            "cortex_sync_u_01arz3ndektsv4rrffq69g5fav"
        );
        assert_ne!(a.notify_channel(), b.notify_channel());
    }
}

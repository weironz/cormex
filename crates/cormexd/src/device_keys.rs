//! Device keys: the long-lived credential `cormex login` gets for one machine.
//!
//! Design: docs/design/session-archive-architecture.md §6.1 (D9).
//!
//! # Why not reuse the refresh token
//!
//! Refresh tokens **rotate**, and they carry replay detection: the same token
//! used twice is treated as a leak and the whole chain is revoked. The capture
//! client's shape is exactly the opposite — several hook processes run
//! concurrently, each reading the same locally stored credential. Rotation plus
//! replay detection reads that as a leak and revokes the chain, so the user's
//! capture stops silently. A device key does not rotate and does not detect
//! replay; the unit of revocation is "this machine".
//!
//! # Why the digest is SHA-256
//!
//! The same argument as [`crate::auth`]'s module docs: a slow KDF defends a
//! **low-entropy** secret against offline brute force, and this key is 256 bits
//! of kernel randomness. The plaintext appears exactly once, at issue time.
//!
//! # Why the scope check is a route allow-list rather than a semantic check
//!
//! The same reasoning as [`crate::delegated_token::DelegatedScope::allows`]:
//! missing an entry is a loud 403, while admitting one entry too many is a
//! **silent** privilege escalation. A table that can only be wrong in the
//! direction of "denied" is the right shape.
//!
//! # The 60-second cache, and what it costs
//!
//! Every authenticated request would otherwise be a database round trip, and a
//! sync is dozens of requests. The cache makes revocation take up to 60 seconds
//! to bite. That is stated here rather than discovered later: a revoked key
//! keeps working for at most a minute, and for a credential whose job is
//! "upload files from this machine" that is an acceptable window.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::Method;
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;

use crate::routes::ApiError;

/// How long a resolved key is trusted without asking the database again.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// How often `last_seen_at` is written at most.
///
/// Every request would turn the authentication path into a write path, and a
/// sync is dozens of requests serialized on one row lock. Ten minutes is enough
/// for "when was this machine last seen" to be useful and cheap enough not to
/// matter.
const TOUCH_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// What a device key may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Write captured session files.
    CaptureWrite,
    /// Read the normalized conversation view.
    SessionRead,
    /// Read memory.
    MemoryRead,
    /// Write memory.
    MemoryWrite,
    /// Read the raw archive bytes.
    RawRead,
}

impl Scope {
    /// The spelling stored in `device_keys.scopes` and used on the wire.
    #[must_use]
    pub const fn as_wire(self) -> &'static str {
        match self {
            Self::CaptureWrite => "capture:write",
            Self::SessionRead => "session:read",
            Self::MemoryRead => "memory:read",
            Self::MemoryWrite => "memory:write",
            Self::RawRead => "raw:read",
        }
    }

    /// Parse a wire spelling. `None` for anything else — the database's CHECK
    /// already keeps unknown values out, so this only ever sees what we wrote,
    /// but a `panic` here would turn a bad row into a crash on the auth path.
    #[must_use]
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "capture:write" => Some(Self::CaptureWrite),
            "session:read" => Some(Self::SessionRead),
            "memory:read" => Some(Self::MemoryRead),
            "memory:write" => Some(Self::MemoryWrite),
            "raw:read" => Some(Self::RawRead),
            _ => None,
        }
    }

    /// Every scope, for validating a request.
    #[must_use]
    pub const fn all() -> &'static [Scope] {
        &[
            Self::CaptureWrite,
            Self::SessionRead,
            Self::MemoryRead,
            Self::MemoryWrite,
            Self::RawRead,
        ]
    }
}

/// What a resolved device key stands for.
#[derive(Debug, Clone)]
pub struct DeviceScope {
    pub user_id: String,
    pub device_id: String,
    pub scopes: Vec<Scope>,
}

impl DeviceScope {
    /// Whether this key carries a scope.
    #[must_use]
    pub fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    /// Whether this key may reach this route.
    ///
    /// # The shape of the table
    ///
    /// Same philosophy as `DelegatedScope::allows`: an allow-list, so that
    /// omitting an entry denies rather than admits. The two routes every key
    /// always has are `GET /auth/me` (the client asks "who am I" at startup; a
    /// 403 there leaves it stuck in a pending state forever) and
    /// `DELETE /devices/{own id}` (a machine has to be able to revoke itself
    /// without a human fetching a user token).
    #[must_use]
    pub fn allows(&self, method: &Method, path: &str) -> bool {
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        let is = |m: Method| *method == m;

        // Always available, regardless of scopes
        if is(Method::GET) && path == "/auth/me" {
            return true;
        }
        if is(Method::DELETE) && path == format!("/devices/{}", self.device_id) {
            return true;
        }

        if self.has(Scope::CaptureWrite) {
            let capture = (is(Method::GET) && path == "/capture/catalog")
                || (is(Method::POST) && path == "/capture/files")
                || (is(Method::POST) && path == "/capture/snapshots")
                // The parameterised paths, matched by shape: the ids vary per
                // request, so an exact-string table cannot express them
                || (is(Method::POST) && matches!(segs.as_slice(), ["capture", "files", _, "generations"]))
                || (is(Method::POST) && matches!(segs.as_slice(), ["capture", "files", _, "events"]))
                || (is(Method::PUT)
                    && matches!(segs.as_slice(), ["capture", "generations", _, "chunks", _]));
            if capture {
                return true;
            }
        }

        if self.has(Scope::SessionRead) {
            let session = (is(Method::GET) && path == "/archive/sessions")
                || (is(Method::GET) && path == "/archive/search")
                // `/mcp` carries the session tools. The tool handler narrows
                // further per call; this only decides "may this key reach the
                // MCP frontage at all"
                || ((is(Method::POST) || is(Method::GET) || is(Method::DELETE)) && path == "/mcp")
                || (is(Method::GET) && matches!(segs.as_slice(), ["archive", "sessions", _]))
                || (is(Method::GET) && matches!(segs.as_slice(), ["archive", "sessions", _, "events"]))
                || (is(Method::GET) && matches!(segs.as_slice(), ["archive", "sessions", _, "brief"]))
                // The archive browser's own routes (P2-5). `/archive/raw` is
                // **not** here: it needs `raw:read`, and the handler checks
                // that per call
                || (is(Method::GET) && matches!(segs.as_slice(), ["archive", "sessions"]))
                || (is(Method::GET) && matches!(segs.as_slice(), ["archive", "sessions", _, "events"]));
            if session {
                return true;
            }
        }

        if self.has(Scope::MemoryRead)
            && ((is(Method::GET) && path == "/memory/search")
                || ((is(Method::POST) || is(Method::GET) || is(Method::DELETE)) && path == "/mcp"))
        {
            return true;
        }

        if self.has(Scope::RawRead) {
            let raw = (is(Method::GET) && path == "/archive/search")
                || (is(Method::GET) && matches!(segs.as_slice(), ["archive", "raw", _]))
                || (is(Method::GET)
                    && matches!(segs.as_slice(), ["archive", "sessions", _, "export"]));
            if raw {
                return true;
            }
        }

        false
    }
}

pub use cormex_proto::devices::{DeviceDto, DeviceKeyIssued};

/// The device-key store: a pool over `cormex_auth`, plus a short-lived cache.
pub struct DeviceKeys {
    pool: PgPool,
    /// sha256(plaintext) → scope, and when it was cached.
    ///
    /// Keyed by the digest rather than the plaintext: a heap dump or a debug
    /// print then leaks hashes, not credentials.
    cache: Mutex<HashMap<String, (DeviceScope, Instant)>>,
    /// When each device's `last_seen_at` was last written, so the write path
    /// does not run on every request.
    touched: Mutex<HashMap<String, Instant>>,
}

impl DeviceKeys {
    #[must_use]
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            cache: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashMap::new()),
        }
    }

    /// Resolve a presented key, or `None` if it is unknown or revoked.
    ///
    /// A revoked key can keep working for up to [`CACHE_TTL`]. That window is
    /// the price of not doing a database round trip per request; see the module
    /// docs.
    pub async fn resolve(&self, token: &str) -> Option<DeviceScope> {
        let digest = hex::encode(Sha256::digest(token.as_bytes()));

        if let Ok(guard) = self.cache.lock()
            && let Some((scope, at)) = guard.get(&digest)
            && at.elapsed() < CACHE_TTL
        {
            return Some(scope.clone());
        }

        type Row = (
            String,
            String,
            Vec<String>,
            Option<chrono::DateTime<chrono::Utc>>,
        );
        let row: Option<Row> = sqlx::query_as(
            "SELECT id, user_id, scopes, revoked_at
                   FROM cormex_auth.device_keys WHERE key_sha256 = $1",
        )
        .bind(&digest)
        .fetch_optional(&self.pool)
        .await
        .ok()?;

        let (device_id, user_id, raw_scopes, revoked_at) = row?;
        if revoked_at.is_some() {
            // Do not cache a revoked key: caching it would extend the window
            // past the 60 seconds the docs promise
            if let Ok(mut guard) = self.cache.lock() {
                guard.remove(&digest);
            }
            return None;
        }

        let scopes: Vec<Scope> = raw_scopes
            .iter()
            .filter_map(|s| Scope::from_wire(s))
            .collect();
        let scope = DeviceScope {
            user_id,
            device_id: device_id.clone(),
            scopes,
        };
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(digest, (scope.clone(), Instant::now()));
        }
        self.touch(&device_id).await;
        Some(scope)
    }

    /// Record that this device was used, **at most once per
    /// [`TOUCH_INTERVAL`]**.
    async fn touch(&self, device_id: &str) {
        {
            let Ok(mut guard) = self.touched.lock() else {
                return;
            };
            if let Some(at) = guard.get(device_id)
                && at.elapsed() < TOUCH_INTERVAL
            {
                return;
            }
            guard.insert(device_id.to_owned(), Instant::now());
        }
        let res = sqlx::query(
            "UPDATE cormex_auth.device_keys SET last_seen_at = clock_timestamp() WHERE id = $1",
        )
        .bind(device_id)
        .execute(&self.pool)
        .await;
        if let Err(e) = res {
            // Losing a `last_seen_at` update costs nothing; failing the request
            // over it would be a strange trade
            tracing::debug!(error = %e, device = %device_id, "could not update last_seen_at");
        }
    }

    /// Issue a key for a user. The caller must already have authenticated them.
    ///
    /// # Errors
    /// The label is empty or too long, a scope is unknown, or the write failed.
    pub async fn issue(
        &self,
        user_id: &str,
        label: &str,
        scopes: &[String],
        platform: Option<&str>,
    ) -> Result<DeviceKeyIssued, ApiError> {
        let label = label.trim();
        if label.is_empty() || label.chars().count() > 100 {
            return Err(ApiError::bad_request(
                "the label must be between 1 and 100 characters",
            ));
        }
        // Validate **before** writing: the database's CHECK would reject an
        // unknown scope too, but its error names a constraint rather than the
        // scope the caller actually mistyped
        for s in scopes {
            if Scope::from_wire(s).is_none() {
                return Err(ApiError::bad_request(format!(
                    "unknown scope {s:?}; the valid ones are {}",
                    Scope::all()
                        .iter()
                        .map(|s| s.as_wire())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        if scopes.is_empty() {
            return Err(ApiError::bad_request(
                "a device key with no scopes can do nothing; pass at least one",
            ));
        }

        // The same generator as the pre-shared token: 256 bits from the kernel,
        // and the digest computed here rather than stored alongside
        let (plaintext, digest) = crate::auth::generate();
        let id = cormex_core::Id::new().to_string();

        sqlx::query(
            "INSERT INTO cormex_auth.device_keys (id, user_id, key_sha256, label, scopes, platform)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(&digest)
        .bind(label)
        .bind(scopes)
        .bind(platform)
        .execute(&self.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not store the device key: {e}")))?;

        Ok(DeviceKeyIssued {
            id,
            key: plaintext,
            scopes: scopes.to_vec(),
        })
    }

    /// Every key this user has, newest first. Revoked ones are included: "was
    /// this machine ever set up" is a question people ask.
    ///
    /// # Errors
    /// The query failed.
    pub async fn list(&self, user_id: &str) -> Result<Vec<DeviceDto>, ApiError> {
        type Row = (
            String,
            String,
            Vec<String>,
            Option<String>,
            chrono::DateTime<chrono::Utc>,
            Option<chrono::DateTime<chrono::Utc>>,
            Option<chrono::DateTime<chrono::Utc>>,
        );
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, label, scopes, platform, created_at, last_seen_at, revoked_at
               FROM cormex_auth.device_keys WHERE user_id = $1
              ORDER BY created_at DESC, id DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not list the device keys: {e}")))?;

        Ok(rows
            .into_iter()
            .map(
                |(id, label, scopes, platform, created_at, last_seen_at, revoked_at)| DeviceDto {
                    id,
                    label,
                    scopes,
                    platform,
                    created_at: created_at.to_rfc3339(),
                    last_seen_at: last_seen_at.map(|t| t.to_rfc3339()),
                    revoked_at: revoked_at.map(|t| t.to_rfc3339()),
                },
            )
            .collect())
    }

    /// Revoke one key.
    ///
    /// # Why this is idempotent and does not report "not found"
    ///
    /// Revoking a key that does not exist, belongs to somebody else, or was
    /// already revoked all return success — the same reasoning as
    /// `DELETE /delegated-tokens`. Reporting them separately turns "guess an id
    /// and see" into a probe.
    ///
    /// The `user_id` filter is what keeps one user from revoking another's key;
    /// it is not an ownership *check* (which would have to answer "does it
    /// exist"), it is a scoping of the UPDATE.
    ///
    /// # Errors
    /// The write failed.
    pub async fn revoke(&self, user_id: &str, device_id: &str) -> Result<(), ApiError> {
        sqlx::query(
            "UPDATE cormex_auth.device_keys
                SET revoked_at = coalesce(revoked_at, clock_timestamp())
              WHERE id = $1 AND user_id = $2",
        )
        .bind(device_id)
        .bind(user_id)
        .execute(&self.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not revoke the device key: {e}")))?;
        // Drop the cache entry so the revocation takes effect on this process
        // immediately. Other processes still wait out the TTL
        if let Ok(mut guard) = self.cache.lock() {
            guard.retain(|_, (scope, _)| scope.device_id != device_id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(scopes: &[Scope]) -> DeviceScope {
        DeviceScope {
            user_id: "user-a".into(),
            device_id: "device-1".into(),
            scopes: scopes.to_vec(),
        }
    }

    /// Every scope's route allow-list, entry by entry.
    ///
    /// Written out rather than derived from the implementation: a test that
    /// reads the table it is testing proves nothing. The failure this guards
    /// against is a **missing** entry (a loud 403) or an **extra** one (a silent
    /// privilege escalation), and only a list written independently can catch
    /// the second.
    #[test]
    fn the_allowlist_is_exactly_what_the_client_actually_calls() {
        let capture = scope(&[Scope::CaptureWrite]);
        for (method, path) in [
            (Method::GET, "/capture/catalog"),
            (Method::POST, "/capture/files"),
            (Method::POST, "/capture/files/01ABC/generations"),
            (Method::PUT, "/capture/generations/01ABC/chunks/4096"),
            (Method::POST, "/capture/files/01ABC/events"),
            (Method::POST, "/capture/snapshots"),
        ] {
            assert!(
                capture.allows(&method, path),
                "capture:write must reach {method} {path} -- without it the client cannot upload"
            );
        }
        for (method, path) in [
            (Method::GET, "/memory/search"),
            (Method::GET, "/archive/sessions"),
            (Method::GET, "/archive/raw/01ABC"),
            (Method::GET, "/sync"),
            (Method::POST, "/episodes"),
        ] {
            assert!(
                !capture.allows(&method, path),
                "capture:write must not reach {method} {path}: a capture key's job is uploading \
                 files, and every extra route is a silent privilege escalation"
            );
        }

        let session = scope(&[Scope::SessionRead]);
        for (method, path) in [
            (Method::GET, "/archive/sessions"),
            (Method::GET, "/archive/sessions/01ABC"),
            (Method::GET, "/archive/sessions/01ABC/events"),
            (Method::GET, "/archive/sessions/01ABC/brief"),
            (Method::GET, "/archive/search"),
        ] {
            assert!(
                session.allows(&method, path),
                "session:read must reach {method} {path}"
            );
        }
        assert!(
            !session.allows(&Method::GET, "/archive/raw/01ABC"),
            "session:read must not reach the raw archive -- that needs raw:read, which the \
             client only gets when the user asks for it explicitly"
        );
        assert!(
            !session.allows(&Method::GET, "/archive/sessions/01ABC/export"),
            "session:read must not reach export; it hands out the original bytes"
        );

        let memory = scope(&[Scope::MemoryRead]);
        assert!(memory.allows(&Method::GET, "/memory/search"));
        assert!(
            !memory.allows(&Method::GET, "/archive/sessions"),
            "memory:read is about memory, not about the session archive"
        );

        // **raw:read not granted → /archive/raw/* is 403.** This is the entry
        // the card calls out by name, and it is the one whose absence is a
        // silent data exposure
        let no_raw = scope(&[Scope::CaptureWrite, Scope::SessionRead, Scope::MemoryRead]);
        for path in ["/archive/raw/01ABC", "/archive/sessions/01ABC/export"] {
            assert!(
                !no_raw.allows(&Method::GET, path),
                "{path} was reachable without raw:read. The raw archive is the original bytes, \
                 unmasked on request; a key that never asked for it must not reach it"
            );
        }
        let raw = scope(&[Scope::RawRead]);
        assert!(raw.allows(&Method::GET, "/archive/raw/01ABC"));
        assert!(raw.allows(&Method::GET, "/archive/sessions/01ABC/export"));
    }

    /// Every key can ask who it is and revoke itself, whatever its scopes.
    ///
    /// Without `GET /auth/me` the client stays in a pending state forever at
    /// startup; without `DELETE /devices/{self}` a machine cannot be retired
    /// without a human going and fetching a user token.
    #[test]
    fn every_key_can_identify_itself_and_revoke_itself() {
        let none = scope(&[]);
        assert!(
            none.allows(&Method::GET, "/auth/me"),
            "a key that cannot ask who it is leaves the client stuck at startup"
        );
        assert!(
            none.allows(&Method::DELETE, "/devices/device-1"),
            "a machine must be able to revoke itself"
        );
        assert!(
            !none.allows(&Method::DELETE, "/devices/device-2"),
            "a device key must not revoke **another** device: that is a user-token operation"
        );
    }

    // ══════════════════════════════════════════════════════════
    //  End to end, against a real database
    // ══════════════════════════════════════════════════════════

    use crate::principal::Principal;
    use axum::http::{Method, StatusCode};

    /// A device key acts as its own user, cannot mint another key, and stops
    /// working once revoked.
    ///
    /// The "acts as its own user" half is the same defect P0-2 fixed for
    /// delegated tokens: a credential the header-based resolution does not
    /// recognise falls back to user #1, so a key issued to B would read and
    /// write A's memory with no error anywhere.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn device_keys_are_scoped_revocable_and_belong_to_their_own_user() {
        let Some(base) = crate::test_support::database_url() else {
            eprintln!("skipped: DATABASE_URL is not set");
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt) = crate::test_support::live_state(&db.url).await;

        let a = st
            .create_account("alpha", "correct horse battery")
            .await
            .expect("create A");
        let b = st
            .create_account("bravo", "correct horse battery")
            .await
            .expect("create B");

        let app = crate::routes::router(st.clone());
        let b_token = st
            .access_book()
            .issue_for_test(&b, std::time::Duration::from_secs(600));

        // ── Mint a key for B ──
        let (status, issued) = crate::test_support::call(
            &app,
            Method::POST,
            "/devices",
            Some(&b_token),
            Some(serde_json::json!({
                "label": "bravo-laptop",
                "scopes": ["capture:write", "session:read"],
                "platform": "linux-x86_64"
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "minting a device key should succeed: {issued}"
        );
        let device_key = issued["key"]
            .as_str()
            .expect("the response should carry the plaintext key")
            .to_owned();
        let device_id = issued["id"].as_str().expect("an id").to_owned();

        // ── The key identifies itself as B ──
        let (status, me) =
            crate::test_support::call(&app, Method::GET, "/auth/me", Some(&device_key), None).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "a device key must be able to ask who it is"
        );
        assert_eq!(
            me["user_id"], b,
            "the device key resolved to somebody other than its owner -- this is the same \
             fallback-to-user-#1 defect that P0-2 fixed for delegated tokens"
        );
        assert_ne!(me["user_id"], a);

        // ── A route outside its scopes is 403, not 401 ──
        //
        // `/sync` exists (it is the device-sync endpoint for the memory tables)
        // and is deliberately **not** in any device scope: a capture key's job
        // is uploading files, and pulling the whole memory database down is a
        // different thing. `/archive/raw` would be the more pointed example but
        // that route does not exist until P2-5, and a 404 would make this
        // assertion vacuous
        let (status, body) =
            crate::test_support::call(&app, Method::GET, "/sync", Some(&device_key), None).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a key reached /sync, which no device scope covers (got {status}: {body}). 403 rather \
             than 401 is the point: the credential is valid, it simply may not do this"
        );

        // ── A device key cannot mint another device key ──
        let (status, _) = crate::test_support::call(
            &app,
            Method::POST,
            "/devices",
            Some(&device_key),
            Some(serde_json::json!({ "label": "x", "scopes": ["capture:write"] })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a device key minted another device key -- a credential that can mint credentials \
             can widen its own scope"
        );

        // ── The key lands in B's tenant, not A's ──
        let tenant_b = st
            .tenant_of(&Principal::Device(DeviceScope {
                user_id: b.clone(),
                device_id: device_id.clone(),
                scopes: vec![Scope::CaptureWrite],
            }))
            .await
            .expect("resolve B's tenant");
        let tenant_a = st
            .tenant_of(&Principal::User { user_id: a.clone() })
            .await
            .expect("resolve A's tenant");
        assert_ne!(
            tenant_b.schema(),
            tenant_a.schema(),
            "A and B resolved to the same schema"
        );

        // ── Revoke, then wait out the cache ──
        let (status, _) = crate::test_support::call(
            &app,
            Method::DELETE,
            &format!("/devices/{device_id}"),
            Some(&b_token),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "revoking should succeed");

        // The cache holds the key for up to 60 seconds, so the immediate check
        // is only meaningful after it expires. Rather than sleeping a minute in
        // a test, resolve through a **fresh** store: a new process would have an
        // empty cache, and that is what the 60 seconds buys
        let fresh = DeviceKeys::new(st.accounts().expect("accounts").pool.clone());
        assert!(
            fresh.resolve(&device_key).await.is_none(),
            "a revoked key still resolves. The revocation is recorded but the lookup does not \
             check it, so a lost laptop keeps uploading"
        );

        // ── B's key list shows it revoked, A's does not see it ──
        let (status, list) =
            crate::test_support::call(&app, Method::GET, "/devices", Some(&b_token), None).await;
        assert_eq!(status, StatusCode::OK);
        let devices = list["devices"].as_array().expect("an array");
        assert_eq!(devices.len(), 1, "B should see exactly the one key: {list}");
        assert!(
            devices[0]["revoked_at"].is_string(),
            "the revoked key should stay in the list carrying revoked_at: {list}"
        );

        let a_token = st
            .access_book()
            .issue_for_test(&a, std::time::Duration::from_secs(600));
        let (_, list_a) =
            crate::test_support::call(&app, Method::GET, "/devices", Some(&a_token), None).await;
        assert_eq!(
            list_a["devices"].as_array().map(Vec::len),
            Some(0),
            "A sees B's device key -- the list is not scoped to the caller"
        );

        db.drop().await;
    }

    /// The wire spellings round-trip, and an unknown one is `None` rather than
    /// a panic.
    #[test]
    fn scopes_round_trip_and_unknown_values_are_rejected() {
        for s in Scope::all() {
            assert_eq!(Scope::from_wire(s.as_wire()), Some(*s));
        }
        assert_eq!(Scope::from_wire("capture:read"), None);
        assert_eq!(Scope::from_wire(""), None);
        assert_eq!(Scope::from_wire("CAPTURE:WRITE"), None);
    }
}

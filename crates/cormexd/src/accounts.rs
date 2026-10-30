//! Accounts: register / log in / refresh / log out, plus the tables behind them.
//!
//! The pure logic (password hashing, token generation, replay adjudication)
//! lives in [`crate::credentials`]; this module only does "talk to the database"
//! and "assemble it into HTTP".
//!
//! # Why a session keeps working indefinitely
//!
//! Two kinds of token, two orders of magnitude apart in lifetime:
//!
//! - **access token**: 15 minutes. Sent on every request, so the window a leak
//!   opens is short.
//! - **refresh token**: 30 days. Used exactly once, to exchange for a new access
//!   token, and rotated on use.
//!
//! The client stores the refresh token in the system credential store, so
//! "close the app and open it again" does not require retyping the password —
//! it exchanges the old one for a new pair and carries on. **This is the entire
//! mechanism behind "no repeated logins"**, and it only holds if the client
//! genuinely persists that string (see `app/lib/auth/`). A server issuing
//! long-lived tokens that the client does not store has done nothing at all.
//!
//! # Rotation and replay
//!
//! Every refresh issues a new token and stamps the old one with `rotated_at`.
//! Presenting an already-rotated token for exchange means it leaked (a
//! legitimate client only holds the newest one), and at that point **the entire
//! family is revoked together** — revoking just that one is not enough, because
//! the attacker also holds whatever they just exchanged it for.
//!
//! The adjudication itself is in [`crate::credentials::judge_refresh`], where
//! the three outcomes and their reasoning live.

use axum::{Json, extract::State};
use chrono::{Duration, Utc};
use cormex_proto::auth::{AuthTokens, LoginRequest, RefreshRequest, RegisterRequest, WhoAmI};
use sqlx::Row as _;

use crate::credentials::{
    RefreshRecord, RefreshVerdict, RejectReason, generate_refresh, hash_password, judge_refresh,
    refresh_digest, verify_password,
};
use crate::routes::ApiError;
use crate::state::AppState;

/// Lifetime of an access token.
///
/// Short enough that "a leak is not worth much", long enough that "we are not
/// making a refresh round trip every few minutes". 15 minutes is the common
/// compromise between those, with no deeper reason — the real backstop is that
/// the refresh chain can be revoked as a whole.
const ACCESS_TTL: Duration = Duration::minutes(15);

/// Lifetime of a refresh token.
///
/// **This number is "how long you can go unused before having to log in
/// again".** 30 days means someone using the product daily never sees the login
/// box again (every refresh renews it), while a machine left unopened for a
/// month asks for the password — which is exactly the line we want.
const REFRESH_TTL: Duration = Duration::days(30);

/// Registration is **closed** by default.
///
/// Open registration means anyone can burn money with the server's DeepSeek key,
/// and production is a 2-core, 3.5 GB machine already running 19 containers.
///
/// The accepted value is deliberately this literal rather than `1` / `true`,
/// matching `CORMEX_AUTH=disabled`: an environment variable fat-fingered to `0`
/// must not open the door.
const OPEN_REGISTRATION_ENV: &str = "CORMEX_OPEN_REGISTRATION";
const OPEN_REGISTRATION_ON: &str = "enabled";

fn open_registration() -> bool {
    std::env::var(OPEN_REGISTRATION_ENV).is_ok_and(|v| v.trim() == OPEN_REGISTRATION_ON)
}

impl AppState {
    /// Create an account: provision the schema, migrate it, write the users row.
    ///
    /// # The order cannot be reversed
    ///
    /// The schema is created and migrated first, and the users row is written
    /// **last**. The other way round leaves the intermediate state "the account
    /// exists but its database is empty", and that user logs in straight into a
    /// schema with no tables — what they get is an unintelligible SQL error, not
    /// "your account is not finished being set up".
    ///
    /// # Errors
    /// The username already exists, the password is too short, provisioning the
    /// schema failed, or the migration failed.
    pub async fn create_account(&self, username: &str, password: &str) -> Result<String, ApiError> {
        let hash = hash_password(password).map_err(|e| ApiError::bad_request(e.to_string()))?;
        let user_id = cormex_core::Id::new().to_string();

        // **The first account lands on `public`; no separate schema is created.**
        //
        // That database already has things in it — a self-hosting user has been
        // using the system for months and only creates an account later. Deriving
        // a fresh schema for them too means that the moment they finish
        // registering and log in, months of memory vanish (the data is still
        // there, there is simply no path leading to it any more). And this
        // **raises no error**: the session list is just empty, looking like a
        // fresh install.
        //
        // Found only on an end-to-end run: after creating an account with the
        // old token, it pointed at `u_01kz…` while the existing data sat in
        // `public`. The design already said "user #1's schema is named public,
        // existing data does not move"; the implementation had simply missed
        // this branch.
        let schema = schema_for_new_account(&user_id, self.user_count().await? == 0)?;

        // Schema first. On failure there is no account record yet and a retry is
        // fine — whereas the reverse order leaves behind an account nobody can
        // log into
        cormex_store::provision(self.accounts()?.tenants.as_ref(), &schema)
            .await
            .map_err(|e| {
                ApiError::internal(format!(
                    "could not provision the database for the new account: {e}"
                ))
            })?;

        let inserted = sqlx::query(
            "INSERT INTO cormex_auth.users (id, username, password_hash, schema_name)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(&user_id)
        .bind(username)
        .bind(&hash)
        .bind(schema.as_str())
        .execute(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not write the user record: {e}")))?;

        if inserted.rows_affected() == 0 {
            // The schema was created but the account row was not written.
            // Drop it rather than leaving an orphan — an unclaimed schema
            // occupies the catalog forever, and the next registration with the
            // same username would create yet another one
            let _ = self.accounts()?.tenants.as_ref().drop_schema(&schema).await;
            return Err(ApiError::bad_request("that username is already taken"));
        }
        Ok(user_id)
    }

    /// How many accounts this deployment has.
    ///
    /// Only used to decide "is this the first one", so it is not cached — it
    /// runs once per registration request, and registration is an extremely
    /// low-frequency action in this product.
    ///
    /// # Errors
    /// No database is attached, or the query failed.
    pub async fn user_count(&self) -> Result<i64, ApiError> {
        let row = sqlx::query("SELECT count(*) AS n FROM cormex_auth.users")
            .fetch_one(&self.accounts()?.pool)
            .await
            .map_err(|e| ApiError::internal(format!("could not count the users: {e}")))?;
        Ok(row.get("n"))
    }

    /// Issue a new token pair. A `family` of `None` starts a new chain (login).
    async fn issue(
        &self,
        user_id: &str,
        family: Option<String>,
        device: Option<&str>,
    ) -> Result<AuthTokens, ApiError> {
        let (plain, digest) = generate_refresh();
        let family = family.unwrap_or_else(|| cormex_core::Id::new().to_string());
        let expires = Utc::now() + REFRESH_TTL;

        sqlx::query(
            "INSERT INTO cormex_auth.auth_tokens
                 (id, user_id, token_sha256, family_id, expires_at, device_label)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(cormex_core::Id::new().to_string())
        .bind(user_id)
        .bind(&digest)
        .bind(&family)
        .bind(expires)
        .bind(device)
        .execute(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not issue the token: {e}")))?;

        Ok(AuthTokens {
            access_token: self.access_book().issue(
                user_id,
                std::time::Duration::from_secs(
                    u64::try_from(ACCESS_TTL.num_seconds()).unwrap_or(900),
                ),
            ),
            access_expires_in_secs: u64::try_from(ACCESS_TTL.num_seconds()).unwrap_or(900),
            refresh_token: plain,
            refresh_expires_in_secs: u64::try_from(REFRESH_TTL.num_seconds()).unwrap_or(2_592_000),
            user_id: user_id.to_owned(),
        })
    }

    /// Exchange a refresh token for a new pair, marking the old one rotated.
    ///
    /// # Errors
    /// The token is unrecognised, expired, revoked, or **replayed** (in which
    /// case the entire family is revoked).
    pub async fn rotate(&self, plain: &str) -> Result<AuthTokens, ApiError> {
        let digest = refresh_digest(plain);
        let row = sqlx::query(
            "SELECT id, user_id, family_id, rotated_at IS NOT NULL AS rotated,
                    revoked_at IS NOT NULL AS revoked, expires_at < now() AS expired
               FROM cormex_auth.auth_tokens WHERE token_sha256 = $1",
        )
        .bind(&digest)
        .fetch_optional(&self.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not look up the token: {e}")))?;

        // Not found is not found. **Do not** tell the client the difference
        // between "this token does not exist" and "this token expired" — that
        // hands a brute-forcer a progress bar
        let Some(row) = row else {
            return Err(ApiError::unauthorized(
                "this login is no longer valid; sign in again",
            ));
        };

        let family_raw: String = row.get("family_id");
        let record = RefreshRecord {
            family: family_raw.parse().map_err(|_| {
                ApiError::internal("the family_id stored in the database is not a valid ULID")
            })?,
            rotated: row.get("rotated"),
            revoked: row.get("revoked"),
            expired: row.get("expired"),
        };
        let user_id: String = row.get("user_id");
        let token_id: String = row.get("id");

        match judge_refresh(&record) {
            RefreshVerdict::RevokeFamily { family } => {
                // This one has already been exchanged, and a legitimate client
                // only holds the newest one — so this request came from
                // somebody else. Revoke the whole chain; both sides have to log
                // in again
                let family = family.to_string();
                let n = self.revoke_family(&family).await;
                tracing::warn!(
                    user = %user_id,
                    family = %family,
                    revoked = n,
                    "refresh token replay detected; the whole chain has been revoked"
                );
                Err(ApiError::unauthorized(
                    "this login credential has already been used. For safety, every session \
                     on this device has been signed out; sign in again",
                ))
            }
            RefreshVerdict::Reject(why) => Err(ApiError::unauthorized(match why {
                RejectReason::Expired => "this login has expired; sign in again",
                RejectReason::FamilyRevoked => "this login was signed out; sign in again",
            })),
            RefreshVerdict::Rotate => {
                sqlx::query("UPDATE cormex_auth.auth_tokens SET rotated_at = now() WHERE id = $1")
                    .bind(&token_id)
                    .execute(&self.accounts()?.pool)
                    .await
                    .map_err(|e| ApiError::internal(format!("could not rotate the token: {e}")))?;
                self.issue(&user_id, Some(record.family.to_string()), None)
                    .await
            }
        }
    }

    /// Revoke the whole chain, returning how many rows were revoked.
    async fn revoke_family(&self, family: &str) -> u64 {
        let Ok(acc) = self.accounts() else { return 0 };
        sqlx::query(
            "UPDATE cormex_auth.auth_tokens SET revoked_at = now()
              WHERE family_id = $1 AND revoked_at IS NULL",
        )
        .bind(family)
        .execute(&acc.pool)
        .await
        .map(|r| r.rows_affected())
        .unwrap_or(0)
    }
}

/// Issued access tokens → (user id, expiry instant).
///
/// # Why in memory, neither persisted nor signed as a JWT
///
/// Persisting means **one database round trip per request**, and the access
/// token is sent on every request. A JWT would pull in a signing dependency and
/// require key rotation management — and the "stateless verification" it buys
/// is worth nothing to a single-process deployment.
///
/// The price is that **every access token is void after a restart**. That does
/// not hurt: the refresh token is alive and well in the database, the client
/// hits one 401 and exchanges it for a new pair, and the user notices nothing.
/// This is precisely the thing the "two kinds of token" design buys us
/// incidentally.
#[derive(Default)]
pub struct AccessBook {
    inner: std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
}

impl AccessBook {
    fn issue(&self, user_id: &str, ttl: std::time::Duration) -> String {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect(
            "the kernel entropy source is unavailable; refusing to issue a predictable token",
        );
        let token = hex::encode(buf);
        let mut g = self
            .inner
            .lock()
            .expect("the access book's lock should never be poisoned");
        // Prune the expired ones while we are here. A timer would mean one more
        // background task plus its failure path, whereas this code path only
        // runs when somebody logs in or refreshes
        let now = std::time::Instant::now();
        g.retain(|_, (_, exp)| *exp > now);
        g.insert(token.clone(), (user_id.to_owned(), now + ttl));
        token
    }

    /// Who this token belongs to. Returns `None` for both expired and
    /// unrecognised tokens.
    ///
    /// An issuing entry point for tests only.
    ///
    /// `issue` is private (issuing goes through the full login flow), but the
    /// authentication middleware's invariant needs a real token that the book
    /// recognises — hand-rolling a hex string would test nothing at all.
    #[cfg(any(test, feature = "test-support"))]
    pub fn issue_for_test(&self, user_id: &str, ttl: std::time::Duration) -> String {
        self.issue(user_id, ttl)
    }

    pub fn resolve(&self, token: &str) -> Option<String> {
        let g = self
            .inner
            .lock()
            .expect("the access book's lock should never be poisoned");
        let (user, exp) = g.get(token)?;
        (*exp > std::time::Instant::now()).then(|| user.clone())
    }
}

/// Which schema a new account lands in.
///
/// # The first account must be `public`
///
/// That database usually already has things in it — a self-hosting user has
/// been using the system for months and only creates an account later. Deriving
/// a fresh schema for them means that the moment they finish registering and log
/// in, months of memory vanish: the data is still there, there is simply no path
/// leading to it any more. And this **raises no error**; the session list is
/// just empty, looking like a fresh install.
///
/// Found only on an end-to-end run (after creating an account with the old
/// token, it pointed at `u_01kz…` while the existing data sat in `public`). The
/// design already said "user #1's schema is named public, existing data does not
/// move"; the implementation had simply missed this branch.
///
/// # Errors
/// `user_id` is not a valid ULID.
fn schema_for_new_account(
    user_id: &str,
    is_first: bool,
) -> Result<cormex_store::SchemaName, ApiError> {
    if is_first {
        return Ok(cormex_store::SchemaName::public());
    }
    cormex_store::SchemaName::derive(user_id).map_err(|e| ApiError::internal(e.to_string()))
}

/// `POST /auth/login`
pub async fn login(
    State(st): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<AuthTokens>, ApiError> {
    let row = sqlx::query(
        "SELECT id, password_hash, disabled_at IS NOT NULL AS disabled
           FROM cormex_auth.users WHERE lower(username) = lower($1)",
    )
    .bind(&req.username)
    .fetch_optional(&st.accounts()?.pool)
    .await
    .map_err(|e| ApiError::internal(format!("could not look up the user: {e}")))?;

    // A nonexistent username and a wrong password return **the same sentence**:
    // distinguishing them hands out a username dictionary.
    //
    // The timing side channel is deliberately not equalised here: defending it
    // properly would mean running argon2 even for nonexistent users, which turns
    // "guess a random username" into 16ms of CPU — a far better DoS amplifier.
    // The trade-off is written down here; do not mistake it for an oversight
    let deny = || ApiError::unauthorized("wrong username or password");
    let Some(row) = row else { return Err(deny()) };
    if row.get::<bool, _>("disabled") {
        return Err(ApiError::unauthorized("this account has been disabled"));
    }
    if !verify_password(&req.password, &row.get::<String, _>("password_hash")) {
        return Err(deny());
    }

    let user_id: String = row.get("id");
    Ok(Json(
        st.issue(&user_id, None, req.device_label.as_deref())
            .await?,
    ))
}

/// `POST /auth/refresh` — this endpoint is the entire reason the client
/// **never has to log in again**.
pub async fn refresh(
    State(st): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<AuthTokens>, ApiError> {
    Ok(Json(st.rotate(&req.refresh_token).await?))
}

/// `POST /auth/logout` — revoke this chain.
///
/// Revokes **the whole family** rather than only the token presented: the intent
/// of logging out is "the login on this device is no longer valid", and the
/// chain still holds the successor it just exchanged for.
pub async fn logout(
    State(st): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let digest = refresh_digest(&req.refresh_token);
    let n = sqlx::query(
        "UPDATE cormex_auth.auth_tokens SET revoked_at = now()
          WHERE family_id = (SELECT family_id FROM cormex_auth.auth_tokens WHERE token_sha256 = $1)
            AND revoked_at IS NULL",
    )
    .bind(&digest)
    .execute(&st.accounts()?.pool)
    .await
    .map(|r| r.rows_affected())
    .unwrap_or(0);

    // An unrecognised token also gets a 200: logging out is idempotent, and a
    // 404 would only turn "am I already logged out?" into a branch the client
    // has to handle
    Ok(Json(serde_json::json!({ "revoked": n })))
}

/// `POST /auth/register` — **403 by default**.
pub async fn register(
    State(st): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<AuthTokens>, ApiError> {
    // **There is no longer a "the first account is admitted unconditionally"
    // exception.**
    //
    // Its former justification was: the first step of a self-hosted deployment
    // is getting it running, and at that moment there is no email channel and
    // possibly no shell either. The price was that on a freshly deployed machine
    // with no account yet and exposed to the internet, whoever reached it first
    // became the owner — and across repeated deployments that window cannot be
    // outrun.
    //
    // The first account is now created **at startup** from
    // CORMEX_ADMIN_USERNAME / _PASSWORD in `.env` (see `main::ensure_admin`), or
    // with `--create-user`. Neither path crosses the public internet, so no
    // exception is needed here — one fewer branch, and one fewer window.
    if !open_registration() {
        // Say plainly that "this deployment has it closed" rather than "you
        // typed something wrong", or the caller will keep retrying
        return Err(ApiError::forbidden(format!(
            "registration is closed on this deployment. An administrator can open it by \
             setting {OPEN_REGISTRATION_ENV}={OPEN_REGISTRATION_ON}, or create the account \
             directly with `cormexd --create-user`"
        )));
    }
    let user_id = st.create_account(&req.username, &req.password).await?;
    Ok(Json(st.issue(&user_id, None, None).await?))
}

/// `GET /auth/usage` — how much I used this window and how much is left.
///
/// # Errors
/// No accounts system is attached, or the usage could not be computed.
pub async fn usage(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<crate::principal::Principal>,
) -> Result<Json<crate::quota::QuotaView>, ApiError> {
    let user_id = current_user(&st, &principal).await;
    Ok(Json(st.quota_status(&user_id).await?.into()))
}

/// Who sent this request, **from the principal the middleware resolved**.
///
/// This used to take the headers and re-derive the identity, falling back to
/// user #1 for anything it did not recognise — including delegated tokens. That
/// is how a sandbox token issued to user B ended up writing into user A's
/// tenant. The headers are no longer consulted here at all.
pub async fn current_user(st: &AppState, principal: &crate::principal::Principal) -> String {
    principal.user_id_or_owner(st).await
}

/// `GET /auth/me`
///
/// Look up who the request's access token belongs to. **The old pre-shared token
/// is recognised too**, mapping to user #1 — during the transition the CLI and
/// every existing desktop client still use it, and a hard cutover would lose
/// contact with all of them the same day.
pub async fn whoami(
    State(st): State<AppState>,
    axum::Extension(principal): axum::Extension<crate::principal::Principal>,
) -> Result<Json<WhoAmI>, ApiError> {
    // **The principal, not the bearer.** This used to re-read the header, and a
    // delegated token therefore answered "you are user #1".
    let user_id = principal.user_id_or_owner(&st).await;
    let row = sqlx::query("SELECT username, schema_name FROM cormex_auth.users WHERE id = $1")
        .bind(&user_id)
        .fetch_optional(&st.accounts()?.pool)
        .await
        .map_err(|e| ApiError::internal(format!("could not look up the user: {e}")))?;
    match row {
        Some(r) => Ok(Json(WhoAmI {
            user_id,
            username: r.get("username"),
            schema_name: r.get("schema_name"),
            device_id: principal.device().map(|d| d.device_id.clone()),
        })),
        // The legacy token path maps to user #1, and user #1 may not be in the
        // users table yet (the deployment predates the accounts system). This is
        // not an error, it is the normal transitional state
        None => Ok(Json(WhoAmI {
            user_id,
            username: "(no account yet)".into(),
            schema_name: "public".into(),
            device_id: principal.device().map(|d| d.device_id.clone()),
        })),
    }
}

/// User #1's id. The legacy token path maps to them.
///
/// Takes the first row in ascending id order — ULIDs are ordered, so "the
/// first" is "the earliest created".
pub(crate) async fn owner_user_id(st: &AppState) -> Option<String> {
    let acc = st.accounts().ok()?;
    sqlx::query("SELECT id FROM cormex_auth.users ORDER BY id LIMIT 1")
        .fetch_optional(&acc.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("id"))
}

#[cfg(test)]
mod tests {
    /// **Registration no longer has a "first account" exception.**
    ///
    /// It used to admit that request unconditionally, and the price was that on
    /// a freshly deployed machine with no account yet and exposed to the
    /// internet, whoever reached /auth/register first became the owner. You can
    /// outrun that once by being quick; across repeated deployments you cannot.
    ///
    /// The first account is now created from `.env` **before the listener
    /// opens** (see `main::ensure_admin`), or with `--create-user`. Neither path
    /// crosses the public internet, so no exception is needed here — and this
    /// test watches to make sure the exception does not grow back.
    #[test]
    fn registration_has_no_first_account_exception() {
        let src = include_str!("accounts.rs");
        let gate = src
            .split("pub async fn register")
            .nth(1)
            .expect("register is still here, right?");
        // Only look at the first part of the body (up to the password hash is
        // enough)
        let head = &gate[..gate.len().min(2000)];
        assert!(
            !head.contains("user_count"),
            "register is counting users again — that usually means the \"treat the first \
             account specially\" exception is back, and it brings a race-for-owner window on the \
             public internet with it. The first account should be created from .env \
             (main::ensure_admin) or with --create-user"
        );
    }

    use super::*;

    /// **The default has to be closed.**
    ///
    /// Forgetting to set one environment variable must not amount to "strangers
    /// can open accounts and burn my API key".
    #[test]
    fn registration_is_closed_unless_explicitly_opened() {
        // This test does not modify the process environment (that would affect
        // other cases running in parallel); it verifies the criterion itself:
        // nothing counts as open except that one literal
        for v in [
            "", "0", "1", "true", "TRUE", "yes", "on", "disabled", "Enabled",
        ] {
            assert_ne!(
                v.trim(),
                OPEN_REGISTRATION_ON,
                "{v:?} must not count as \"registration open\" — fat-fingering 1 or true must \
                 not open the door"
            );
        }
        assert_eq!("enabled", OPEN_REGISTRATION_ON);
    }

    /// The refresh lifetime is "how long you can go unused before logging in
    /// again"; do not let it quietly shrink.
    #[test]
    fn a_session_survives_a_month_of_restarts() {
        assert_eq!(
            REFRESH_TTL.num_days(),
            30,
            "this number is what the user experiences as \"how often do I have to log in \
             again\"; changing it must be a conscious act"
        );
        assert!(
            ACCESS_TTL < REFRESH_TTL,
            "access must be far shorter than refresh, or the short-lived-token layer means \
             nothing"
        );
    }
}

#[cfg(test)]
mod account_schema_tests {
    use super::*;

    /// The first account takes over `public`; every other one gets its own.
    #[test]
    fn the_first_account_takes_over_public() {
        let id = cormex_core::Id::new().to_string();
        assert_eq!(
            schema_for_new_account(&id, true).unwrap().as_str(),
            "public",
            "the first account must land on public. Anywhere else means that someone who used \
             the system for months before registering logs in afterwards to find all their \
             memory gone — with no error, the session list is simply empty"
        );
        let second = schema_for_new_account(&id, false).unwrap();
        assert!(
            second.as_str().starts_with("u_"),
            "from the second account onwards each must get its own schema; actually got {}",
            second.as_str()
        );
        assert_ne!(second.as_str(), "public");
    }
}

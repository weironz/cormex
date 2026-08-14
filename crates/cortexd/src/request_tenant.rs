//! Who a request belongs to — resolved into a [`Store`] genuinely connected to
//! that schema.
//!
//! # Why this deserves its own type instead of letting each handler look it up
//!
//! The isolation itself has existed for a long time:
//! [`cortex_store::TenantPools`] welds `search_path` into the connection
//! options, so a connection **physically cannot see another schema**. What was
//! missing is that **nobody called it** — the request path kept holding the one
//! `Store` built at startup (which connects to `public`), so two logged-in users
//! were reading and writing the same database.
//!
//! This module fills that gap. It is a type rather than a `let store = …`,
//! because **forgetting to pass it is the only shape this class of bug takes**:
//!
//! - make **every** data method on `AppState` require a `&Tenant`, so omitting
//!   it fails to compile
//! - provide no `Default`, no global "fetch the current tenant" function, and no
//!   task-local
//!
//! That last point is deliberate. A task-local would have saved a great deal of
//! editing, but it has the same shape as "shared pool + `SET search_path` each
//! time": **forgetting to set it raises no error, it just quietly lands on
//! `public`** — which is precisely the accident being defended against. Better
//! to edit 16 signatures.
//!
//! # Single-user self-hosting is unaffected
//!
//! With no accounts system configured, the tenant is always `public` —
//! byte-for-byte identical to today's behaviour. The same holds for the old
//! pre-shared token path: it does not recognise an access token, falls back to
//! user #1, and user #1's schema is named `public`.

use cortex_store::{SchemaName, Store};
use std::sync::Arc;

use crate::routes::ApiError;
use crate::state::AppState;

/// The tenant a request belongs to.
///
/// **Holding one means resolution already happened.** There is no
/// "not yet resolved" state, and therefore no "forgot to resolve" bug.
pub enum Tenant {
    /// Live backend: a pool already connected to some schema.
    Live {
        schema: SchemaName,
        store: Arc<Store>,
    },
    /// Mock backend: no database.
    ///
    /// Its own variant rather than stuffing in an `Option<Store>`: an `Option`
    /// invites `unwrap_or_default()`, which here means falling back to somebody
    /// else's database.
    Mock,
}

impl Tenant {
    /// This tenant's `Store`.
    ///
    /// # Errors
    /// Under the mock backend there is no database to hand out. The caller (the
    /// `Backend::Live` branch) never hits this, but the signature must not lie.
    /// The error type follows `AppState`'s data methods (`CortexError`) rather
    /// than `ApiError` — the latter would add a conversion at every call site.
    pub fn store(&self) -> cortex_core::Result<&Store> {
        match self {
            Self::Live { store, .. } => Ok(store),
            Self::Mock => Err(cortex_core::CortexError::Unavailable(
                "this instance runs on the mock backend, so there is no memory database to reach"
                    .into(),
            )),
        }
    }

    /// Which schema this lands on. For logs and diagnostics.
    #[must_use]
    pub fn schema(&self) -> &str {
        match self {
            Self::Live { schema, .. } => schema.as_str(),
            Self::Mock => "mock",
        }
    }
}

impl AppState {
    /// Resolve which tenant this request belongs to.
    ///
    /// The resolution order is deliberately "check the backend, then check
    /// whether there is an accounts system, and only then query the database":
    /// the first two steps cover the overwhelming majority of deployments
    /// (a mock development machine, single-user self-hosting) without a single
    /// database round trip.
    ///
    /// # Errors
    /// The user exists but their schema name is invalid, or the pool for that
    /// schema cannot be created.
    pub async fn tenant(&self, headers: &axum::http::HeaderMap) -> Result<Tenant, ApiError> {
        // Mock backend: no database, so there is no tenant to assign
        let Some(public_store) = self.public_store() else {
            return Ok(Tenant::Mock);
        };

        // No accounts system attached = single-user self-hosting. Always public,
        // byte-for-byte identical to today
        let Ok(acc) = self.accounts() else {
            return Ok(Tenant::Live {
                schema: SchemaName::public(),
                store: public_store,
            });
        };

        let user_id = crate::accounts::current_user(self, headers).await;
        let schema = resolve_schema(&acc.pool, &user_id).await?;
        self.tenant_on(schema).await
    }

    /// The same resolution, but entered from **a user id** rather than request
    /// headers.
    ///
    /// # Why this is its own method instead of having background jobs forge
    /// # headers
    ///
    /// Forging headers would first require minting an access token, and a
    /// background job has none — all it has is the owner recorded in its
    /// sandbox token. Actually minting one would grant background jobs the
    /// ability to conjure credentials out of nothing, which costs far more than
    /// one extra method.
    ///
    /// # Why `resolve_schema` is not simply exposed
    ///
    /// A schema name on its own is useless: the caller would still have to build
    /// the pool itself, and remember that the `public` one already exists from
    /// startup and must not be built a second time. Leaving those three steps to
    /// the caller puts back exactly the "missed a step" failure that the
    /// [`Tenant`] type was introduced to eliminate.
    ///
    /// # Errors
    /// `cortex_auth.users` could not be queried, or the pool for that schema
    /// cannot be created.
    pub async fn tenant_for_user(&self, user_id: &str) -> Result<Tenant, ApiError> {
        let Some(public_store) = self.public_store() else {
            return Ok(Tenant::Mock);
        };
        let Ok(acc) = self.accounts() else {
            return Ok(Tenant::Live {
                schema: SchemaName::public(),
                store: public_store,
            });
        };
        let schema = resolve_schema(&acc.pool, user_id).await?;
        self.tenant_on(schema).await
    }

    /// Schema name → a `Tenant` that is actually connected. The shared tail of
    /// both resolution paths.
    ///
    /// Factored out because the rule "`public` reuses the pool built at startup"
    /// **has to be remembered on both paths**, and forgetting it raises no error
    /// — it merely opens an extra pool onto the same schema for user #1,
    /// quietly eating into the budget on a machine whose `max_connections` is
    /// only 100.
    async fn tenant_on(&self, schema: SchemaName) -> Result<Tenant, ApiError> {
        let Some(public_store) = self.public_store() else {
            return Ok(Tenant::Mock);
        };
        if schema.as_str() == SchemaName::public().as_str() {
            return Ok(Tenant::Live {
                schema,
                store: public_store,
            });
        }
        let acc = self.accounts().map_err(|_| {
            ApiError::internal("a non-public tenant was resolved with no accounts system attached")
        })?;
        let store = acc.tenants.get(&schema).await.map_err(|e| {
            ApiError::internal(format!(
                "could not connect to {}'s database: {e}",
                schema.as_str()
            ))
        })?;
        Ok(Tenant::Live { schema, store })
    }
}

/// Which schema this user lives in.
///
/// Pulled out on its own so it **can be tested**: it is the only new decision in
/// this change, while `AppState::tenant` as a whole needs a live backend (LLM
/// client, embedder, transcriber) to construct and cannot be stood up in a test.
///
/// # Errors
/// The database could not be queried, or the schema name stored there is
/// invalid.
async fn resolve_schema(pool: &sqlx::PgPool, user_id: &str) -> Result<SchemaName, ApiError> {
    let name: Option<String> =
        sqlx::query_scalar("SELECT schema_name FROM cortex_auth.users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| {
                ApiError::internal(format!(
                    "could not find which schema this account lives in: {e}"
                ))
            })?;

    // No row means this is the legacy token path — it already passed the
    // inbound gate, and user #1 lives in public
    let Some(name) = name else {
        return Ok(SchemaName::public());
    };

    // The `users_schema_shape` CHECK already keeps malformed shapes out of the
    // table, so under normal conditions this never fails. **Validate anyway**:
    // a constraint can be DROPped, and the consequence of failing here (falling
    // back to public = reading user #1's memory) costs far more than one extra
    // line.
    //
    // There is no accompanying unit test, because no meaningful one can be
    // written: the CHECK is in the way, so an invalid name cannot be inserted.
    // `SchemaName::new`'s own boundaries are tested in cortex-store.
    SchemaName::new(&name).map_err(|e| {
        ApiError::internal(format!(
            "account {user_id} has an invalid schema name ({name}): {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Skip when there is no real database. Both `search_path` and this table
    /// are Postgres behaviour, so faking them fakes away the thing under test.
    async fn pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())?;
        crate::state::Accounts::connect(&url)
            .await
            .ok()
            .map(|a| a.pool)
    }

    async fn make_user(pool: &sqlx::PgPool, schema: &str) -> String {
        let id = cortex_core::Id::new().to_string();
        sqlx::query(
            "INSERT INTO cortex_auth.users (id, username, password_hash, schema_name)
             VALUES ($1, $2, 'x', $3)",
        )
        .bind(&id)
        .bind(format!("probe-{id}"))
        .bind(schema)
        .execute(pool)
        .await
        .expect("insert the test user");
        id
    }

    /// Two accounts resolve to **their own** schemas.
    ///
    /// If this is wrong, two people read and write the same database — and that
    /// raises no error at all.
    #[tokio::test]
    async fn each_account_resolves_to_its_own_schema() {
        let Some(pool) = pool().await else {
            eprintln!("skipped: no DATABASE_URL");
            return;
        };
        let a_schema = SchemaName::derive(&cortex_core::Id::new().to_string()).unwrap();
        let b_schema = SchemaName::derive(&cortex_core::Id::new().to_string()).unwrap();
        let a = make_user(&pool, a_schema.as_str()).await;
        let b = make_user(&pool, b_schema.as_str()).await;

        let ra = resolve_schema(&pool, &a).await.expect("resolve A");
        let rb = resolve_schema(&pool, &b).await.expect("resolve B");
        assert_eq!(
            ra.as_str(),
            a_schema.as_str(),
            "A did not land on its own schema"
        );
        assert_eq!(
            rb.as_str(),
            b_schema.as_str(),
            "B did not land on its own schema"
        );
        assert_ne!(
            ra.as_str(),
            rb.as_str(),
            "two accounts resolved to the same schema — this is exactly \"two people logged in, \
             both reading the same database\""
        );
    }

    /// An unrecognised user falls back to `public` — the old pre-shared token
    /// path has to keep working.
    #[tokio::test]
    async fn an_unknown_caller_is_the_legacy_token_path() {
        let Some(pool) = pool().await else { return };
        let r = resolve_schema(&pool, "no-such-user-at-all")
            .await
            .expect("resolve");
        assert_eq!(
            r.as_str(),
            "public",
            "the legacy token path must still land on public, or the CLI and every existing \
             desktop client lose contact the same day"
        );
    }
}

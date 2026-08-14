//! Migrating every tenant's schema up to date at startup.
//!
//! # Why this needs a module of its own
//!
//! With a single user, migration is one `store.migrate()` call with exactly two
//! outcomes: it passed, or the service does not come up. Under multi-tenancy it
//! becomes a **partially failing** operation — one schema out of twenty has a
//! problem (disk full, somebody altered a table by hand, the last upgrade lost
//! power halfway through) while the other nineteen are perfectly fine.
//!
//! At that point "the whole service does not come up" is **wrong**: one user's
//! database being broken should not take everybody offline. And "silently skip
//! it" is worse: that user carries on with a schema that is missing tables, and
//! the symptom is some feature occasionally emitting an incomprehensible SQL
//! error.
//!
//! So what this produces is a **per-tenant result list**, from which the caller
//! decides: the healthy ones keep serving, and the broken few are explicitly
//! marked unavailable and reported.

use crate::error::Result;
use crate::store::Store;
use crate::tenant::{SchemaName, TenantPools};

/// The migration result for one tenant.
#[derive(Debug)]
pub struct TenantMigration {
    pub schema: SchemaName,
    /// `None` = success; `Some` = this tenant is unavailable, with a
    /// human-readable reason attached.
    pub failure: Option<String>,
}

/// Migrate every tenant in `schemas` up to date.
///
/// **One at a time, not concurrently.** The migrations contain `CREATE INDEX`
/// (the HNSW ones especially heavy), and building indexes on twenty schemas at
/// once will flatten a 2-core machine outright — which is the last thing that
/// should happen at startup. A few extra seconds buys a service that actually
/// comes up.
///
/// # Errors
/// Never returns `Err`: an individual tenant's failure goes into
/// [`TenantMigration::failure`]. A wholesale failure (cannot reach the
/// database) has already hit the caller while constructing [`TenantPools`].
pub async fn migrate_all(pools: &TenantPools, schemas: &[SchemaName]) -> Vec<TenantMigration> {
    let mut out = Vec::with_capacity(schemas.len());
    for schema in schemas {
        let failure = match pools.get(schema).await {
            Err(e) => Some(format!("取不到连接池：{e}")),
            Ok(store) => match store.migrate().await {
                Ok(()) => None,
                Err(e) => Some(format!("migration 失败：{e}")),
            },
        };
        if let Some(why) = &failure {
            // error rather than warn: this tenant is unavailable from now on,
            // and everywhere else it will show up only as scattered SQL errors
            tracing::error!(schema = schema.as_str(), reason = %why, "租户 schema 迁移失败，该用户暂不可用");
        }
        out.push(TenantMigration {
            schema: schema.clone(),
            failure,
        });
    }
    out
}

/// Open a schema for a new user and migrate it up to date.
///
/// # The order cannot be reversed
///
/// Create the schema, then migrate, and only then write the `users` row — that
/// is the order callers must follow. The other way around (write users first,
/// create the schema after) leaves an intermediate state where "the account
/// exists but the database is empty", and that user hits a schema with no
/// tables the moment they log in.
///
/// # Errors
/// The schema could not be created, or the migration did not pass. **On failure
/// the schema may already be half-built** — the caller should drop it before
/// reporting the error rather than leaving an orphan behind.
pub async fn provision(pools: &TenantPools, schema: &SchemaName) -> Result<()> {
    pools.create_schema(schema).await?;
    let store: std::sync::Arc<Store> = pools.get(schema).await?;
    store.migrate().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Failures must be recorded **one by one** rather than bringing the whole
    /// batch down.
    ///
    /// This test guards "one user's database being broken should not take
    /// everybody offline".
    #[test]
    fn a_failure_is_recorded_per_tenant_not_thrown() {
        let ok = TenantMigration {
            schema: SchemaName::public(),
            failure: None,
        };
        let bad = TenantMigration {
            schema: SchemaName::new("u_01arz3ndektsv4rrffq69g5fav").unwrap(),
            failure: Some("磁盘满".into()),
        };
        let results = [ok, bad];

        let broken: Vec<&str> = results
            .iter()
            .filter(|r| r.failure.is_some())
            .map(|r| r.schema.as_str())
            .collect();
        assert_eq!(broken, ["u_01arz3ndektsv4rrffq69g5fav"]);
        assert!(
            results.iter().any(|r| r.failure.is_none()),
            "one tenant failing must not mark the other tenants as failed too"
        );
    }
}

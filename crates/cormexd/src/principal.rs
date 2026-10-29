//! "Who is this request" — resolved **once**, in the authentication middleware.
//!
//! # The bug this type exists to kill
//!
//! Every handler used to re-read the `Authorization` header and ask its own
//! question. `accounts::current_user` recognised access tokens and fell back to
//! **user #1** for everything else — including a delegated (sandbox) token. So a
//! sandbox token issued to user B resolved to user A's tenant: B's agent read
//! and wrote A's memory, and nothing anywhere reported an error. The same fallback
//! swallowed `/auth/ticket`, whose ticket did not carry an identity at all.
//!
//! The shape of the failure is "the answer depends on which handler asked", and
//! the only structural fix is to ask **once**, before any handler runs, and hand
//! the answer over. That is what this module does; [`crate::auth::require`]
//! inserts it into the request extensions and every handler takes it as
//! `Extension<Principal>`.
//!
//! # Why an enum rather than a `user_id: Option<String>`
//!
//! The three kinds of caller are **not** interchangeable, and several decisions
//! downstream depend on which one is speaking:
//!
//! - a delegated token may only touch its own session (`write_episode`), and its
//!   writes are recorded at the low trust tier
//! - the legacy pre-shared token has no user row at all, and maps to user #1
//! - an ordinary access token is a real account
//!
//! Collapsing them into an id loses exactly the distinctions those decisions
//! need, and the loss is silent: a delegated caller whose scope was flattened to
//! its owner's id looks like a full user credential and gets the trusted tier.
//!
//! [`Principal::Device`] arrives with device keys (P1-1); it is deliberately not
//! added here, because a variant with no producer is this repository's
//! most-repeated ailment — built and never called.

use crate::delegated_token::DelegatedScope;
use crate::request_tenant::Tenant;
use crate::routes::ApiError;
use crate::state::AppState;

/// Who is making the request.
#[derive(Debug, Clone)]
pub enum Principal {
    /// The pre-shared token from `.env` (or authentication switched off).
    ///
    /// It carries no identity: it is the operator's own key, and it maps to user
    /// #1. Kept as its own variant rather than resolved to an id at
    /// authentication time, because user #1 may not have an account row yet —
    /// that is the normal state of a deployment that predates the accounts
    /// system, and looking it up early would mean a failure at the gate instead
    /// of a fallback to `public`.
    LegacyOwner,
    /// An ordinary logged-in user.
    User { user_id: String },
    /// A sandbox token: bound to one user, one session, and a route allow-list.
    Delegated(DelegatedScope),
}

impl Principal {
    /// Which account this request belongs to.
    ///
    /// `None` only for [`Self::LegacyOwner`] when the deployment has no accounts
    /// system at all. Callers that need a string for quota accounting use
    /// [`Self::user_id_or_owner`].
    pub async fn user_id(&self, st: &AppState) -> Option<String> {
        match self {
            Self::LegacyOwner => crate::accounts::owner_user_id(st).await,
            Self::User { user_id } => Some(user_id.clone()),
            Self::Delegated(scope) => Some(scope.owner.clone()),
        }
    }

    /// The account id, with the historical `"owner"` placeholder for a
    /// deployment that has no accounts system.
    ///
    /// That placeholder is not cosmetic: it is the key usage is recorded under,
    /// and changing it would orphan every row already written under it.
    pub async fn user_id_or_owner(&self, st: &AppState) -> String {
        self.user_id(st).await.unwrap_or_else(|| "owner".into())
    }

    /// The sandbox scope, if this is a delegated caller.
    #[must_use]
    pub fn delegated(&self) -> Option<&DelegatedScope> {
        match self {
            Self::Delegated(scope) => Some(scope),
            _ => None,
        }
    }

    /// Which tenant this request belongs to.
    ///
    /// The mock backend and a deployment with no accounts system are answered
    /// without a database round trip; everything else resolves the account's
    /// schema. See [`AppState::tenant_of`].
    ///
    /// # Errors
    /// The account's schema name is invalid, or its pool cannot be created.
    pub async fn tenant(&self, st: &AppState) -> Result<Tenant, ApiError> {
        st.tenant_of(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The delegated caller's account is **the token's owner**, never user #1.
    ///
    /// This is the whole point of the type: before it, a sandbox token resolved
    /// through `current_user`, which did not recognise it and fell back to user
    /// #1. The symptom was "user B's agent reads user A's memory", with no error
    /// anywhere.
    ///
    /// No database is needed: `Delegated` carries its owner, so the assertion
    /// does not depend on a lookup that could itself be the thing under test.
    #[test]
    fn a_delegated_principal_names_its_own_owner() {
        let p = Principal::Delegated(DelegatedScope {
            owner: "user-b".into(),
            session_id: "s-1".into(),
            project: None,
        });
        assert_eq!(
            p.delegated().map(|s| s.owner.as_str()),
            Some("user-b"),
            "a delegated principal must expose its own owner — if this ever reports user #1, \
             the tenant of every sandbox is the first account's"
        );
    }

    /// Only the delegated variant is delegated.
    ///
    /// `write_episode` decides the trust tier with this: `Some` means the
    /// low-trust branch. A `User` principal that reported `Some` would let a
    /// prompt-injected agent's writes be recorded as the user's own words.
    #[test]
    fn only_the_delegated_variant_is_delegated() {
        assert!(Principal::LegacyOwner.delegated().is_none());
        assert!(
            Principal::User {
                user_id: "u".into()
            }
            .delegated()
            .is_none()
        );
    }

    // ══════════════════════════════════════════════════════════
    //  End to end, against a real database
    // ══════════════════════════════════════════════════════════

    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt as _;

    /// A brand-new database, so `cormex_auth` and `public` are both empty.
    ///
    /// The shared development database cannot be used: the first account lands
    /// in `public` by design, so the test would write into whatever memory is
    /// already there, and the assertion "A's database does not contain B's row"
    /// would be measured against somebody else's data.
    struct FreshDb {
        url: String,
        admin: sqlx::PgPool,
        name: String,
    }

    impl FreshDb {
        async fn create(base_url: &str) -> Self {
            let admin = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(base_url)
                .await
                .expect("the test role should be able to connect");
            let millis = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the clock should not predate 1970")
                .as_millis();
            let name = format!("cormex_principal_{}_{}", std::process::id(), millis);
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
                .execute(&admin)
                .await
                .expect("the test role should be allowed to create a database");
            let (base, _) = base_url
                .rsplit_once('/')
                .expect("DATABASE_URL should end in /<database>");
            Self {
                url: format!("{base}/{name}"),
                admin,
                name,
            }
        }

        async fn drop(self) {
            let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            )))
            .execute(&self.admin)
            .await;
            self.admin.close().await;
        }
    }

    fn database_url() -> Option<String> {
        let _ = dotenvy::dotenv();
        std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty())
    }

    /// Build a live state on `url`, with the accounts system attached.
    async fn live_state(url: &str) -> (crate::state::AppState, crate::state::Runtime) {
        let config = cormex_core::config::Config {
            database_url: url.to_owned(),
            bind: "127.0.0.1:0".into(),
            s3: cormex_core::config::S3Config {
                endpoint: String::new(),
                bucket: String::new(),
                region: String::new(),
                access_key: String::new(),
                secret_key: String::new(),
            },
            // Ollama needs no key, and construction touches no network — the
            // test never makes a model call, it only needs `Live` to exist
            llm: cormex_core::config::LlmConfig {
                provider: "ollama".into(),
                model: "test-model".into(),
                cheap_model: "test-model".into(),
                base_url: Some("http://127.0.0.1:1".into()),
            },
            device_id: "principal-test".into(),
        };

        // The hash embedder: no model download, no network. Retrieval quality is
        // irrelevant here, and the episode write path does not embed at all when
        // `retrieve = false`
        let embedder = cormex_memory::embed::build_embedder(cormex_memory::embed::Backend::Hash)
            .await
            .expect("the hash embedder should always build");

        let live = crate::live::Live::new(&config, embedder)
            .await
            .expect("the live backend should attach to the fresh database");
        let accounts = crate::state::Accounts::connect(url)
            .await
            .expect("the accounts pool should attach");

        let rt = crate::state::Runtime {
            auth: crate::auth::AuthMode::Token { digest: [0u8; 32] },
            tickets: std::sync::Arc::new(crate::auth::TicketBook::default()),
            accounts: Some(accounts),
            access: std::sync::Arc::new(crate::accounts::AccessBook::default()),
            delegations: std::sync::Arc::new(crate::delegated_token::DelegatedTokens::default()),
        };
        let st = crate::state::AppState::for_tests_live(live, rt.clone(), config);
        (st, rt)
    }

    async fn call(
        app: &axum::Router,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.map_or_else(String::new, |v| v.to_string())))
            .expect("building the request must not fail");
        let resp = app
            .clone()
            .oneshot(req)
            .await
            .expect("the router's error type is Infallible");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("the response body should be readable");
        let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, json)
    }

    /// **A delegated token acts as its own owner, not as user #1.**
    ///
    /// This is the bug the whole module exists for. Before it, a sandbox token
    /// issued to B resolved through `current_user`, which did not recognise it
    /// and fell back to user #1 — so B's agent read and wrote A's memory, and
    /// nothing anywhere reported an error.
    ///
    /// The ticket path is checked in the same test because it had the same
    /// defect for the same reason: a ticket carried no identity at all.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn tenant_isolation_by_principal() {
        let Some(base) = database_url() else {
            eprintln!("skipped: DATABASE_URL is not set");
            return;
        };
        let db = FreshDb::create(&base).await;
        let (st, _rt) = live_state(&db.url).await;

        // A is the first account, so it lands in `public`; B gets a schema of
        // its own. That is the shape production has, and it is exactly the
        // shape in which "everything falls back to user #1" is invisible
        let a = st
            .create_account("alpha", "correct horse battery")
            .await
            .expect("create A");
        let b = st
            .create_account("bravo", "correct horse battery")
            .await
            .expect("create B");
        assert_ne!(a, b, "the two accounts must be distinct");

        let app = crate::routes::router(st.clone());
        let b_token = st
            .access_book()
            .issue_for_test(&b, std::time::Duration::from_secs(600));

        // ── /auth/me says B ──
        let (status, me) = call(&app, Method::GET, "/auth/me", Some(&b_token), None).await;
        assert_eq!(status, StatusCode::OK, "/auth/me should answer 200");
        assert_eq!(
            me["user_id"], b,
            "an access token for B reported somebody else — the principal is not reaching the \
             handler"
        );

        // ── Issue a delegated token for one of B's sessions ──
        let session = "sess-bravo";
        let (status, delegation) = call(
            &app,
            Method::POST,
            "/delegated-tokens",
            Some(&b_token),
            Some(serde_json::json!({ "session_id": session })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "POST /delegated-tokens should succeed for B"
        );
        assert_eq!(
            delegation["owner"], b,
            "the delegation was signed for the wrong owner"
        );
        let delegated = delegation["token"]
            .as_str()
            .expect("the response should carry a token")
            .to_owned();

        // ── /auth/me with the delegated token still says B ──
        let (status, me) = call(&app, Method::GET, "/auth/me", Some(&delegated), None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            me["user_id"], b,
            "a delegated token answered as somebody other than its owner — this is the exact \
             defect: `current_user` did not recognise sandbox tokens and fell back to user #1"
        );

        // ── Write an episode through the delegated token ──
        let episode_id = cormex_core::Id::new().to_string();
        let (status, ack) = call(
            &app,
            Method::POST,
            "/episodes",
            Some(&delegated),
            Some(serde_json::json!({
                "id": episode_id,
                "session_id": session,
                "role": "user",
                "text": "bravo's private note",
                "retrieve": false
            })),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the delegated token should be able to write to its own session: {ack}"
        );

        // ── The row is in B's schema, and not in A's ──
        let tenant_b = st
            .tenant_of(&Principal::User { user_id: b.clone() })
            .await
            .expect("resolve B's tenant");
        let tenant_a = st
            .tenant_of(&Principal::User { user_id: a.clone() })
            .await
            .expect("resolve A's tenant");
        assert!(
            tenant_b
                .store()
                .expect("B has a database")
                .episode(&episode_id)
                .await
                .expect("query B")
                .is_some(),
            "the episode B's delegated token wrote is not in B's database"
        );
        assert!(
            tenant_a
                .store()
                .expect("A has a database")
                .episode(&episode_id)
                .await
                .expect("query A")
                .is_none(),
            "an episode written with B's delegated token landed in **A's** database. This is the \
             silent crossover: B's agent reads and writes user #1's memory, with no error anywhere"
        );

        // ── The ticket path carries the identity too ──
        let (status, ticket) = call(&app, Method::POST, "/auth/ticket", Some(&b_token), None).await;
        assert_eq!(status, StatusCode::OK, "issuing a ticket should succeed");
        let ticket = ticket["ticket"]
            .as_str()
            .expect("the response should carry a ticket")
            .to_owned();
        let (status, me) = call(
            &app,
            Method::GET,
            &format!("/auth/me?ticket={ticket}"),
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "a ticket should authenticate");
        assert_eq!(
            me["user_id"], b,
            "a ticket issued by B resolved to somebody else — tickets used to carry no identity \
             at all, so every consumer of this path fell back to user #1"
        );

        // ── The legacy pre-shared token still maps to user #1 ──
        //
        // It is the operator's own key. Breaking it would lose contact with the
        // CLI and every desktop client on the same day
        let legacy = st
            .tenant_of(&Principal::LegacyOwner)
            .await
            .expect("resolve the legacy tenant");
        assert_eq!(
            legacy.schema(),
            "public",
            "the pre-shared token must still land on public (user #1's schema)"
        );

        db.drop().await;
    }
}

//! Shared scaffolding for the tests that need a **real, empty** database.
//!
//! `#[cfg(test)]` only. It lives outside `principal.rs` because three test
//! modules now need it (principal isolation, device keys, and the capture API
//! later), and copying a "create a database, migrate it, drop it" helper three
//! times is how the copies drift.
//!
//! # Why a fresh database rather than the usual temporary schema
//!
//! The first account lands in `public` by design, so a test that registers two
//! users against the shared development database writes into whatever memory is
//! already there — and then "A's database does not contain B's row" is measured
//! against somebody else's data.

#![cfg(test)]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt as _;

/// A brand-new database, so `cormex_auth` and `public` are both empty.
///
/// The shared development database cannot be used: the first account lands
/// in `public` by design, so the test would write into whatever memory is
/// already there, and the assertion "A's database does not contain B's row"
/// would be measured against somebody else's data.
pub(crate) struct FreshDb {
    pub(crate) url: String,
    admin: sqlx::PgPool,
    name: String,
}

impl FreshDb {
    pub(crate) async fn create(base_url: &str) -> Self {
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

    pub(crate) async fn drop(self) {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
        self.admin.close().await;
    }
}

pub(crate) fn database_url() -> Option<String> {
    let _ = dotenvy::dotenv();
    std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty())
}

/// Build a live state on `url`, with the accounts system attached.
pub(crate) async fn live_state(url: &str) -> (crate::state::AppState, crate::state::Runtime) {
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
        device_keys: Some(std::sync::Arc::new(crate::device_keys::DeviceKeys::new(
            accounts.pool.clone(),
        ))),
        accounts: Some(accounts),
        access: std::sync::Arc::new(crate::accounts::AccessBook::default()),
        delegations: std::sync::Arc::new(crate::delegated_token::DelegatedTokens::default()),
    };
    let st = crate::state::AppState::for_tests_live(live, rt.clone(), config);
    (st, rt)
}

pub(crate) async fn call(
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

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

#![cfg(any(test, feature = "test-support"))]

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use tower::ServiceExt as _;

/// A brand-new database, so `cormex_auth` and `public` are both empty.
///
/// The shared development database cannot be used: the first account lands
/// in `public` by design, so the test would write into whatever memory is
/// already there, and the assertion "A's database does not contain B's row"
/// would be measured against somebody else's data.
pub struct FreshDb {
    pub url: String,
    admin: sqlx::PgPool,
    name: String,
}

impl FreshDb {
    pub async fn create(base_url: &str) -> Self {
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(base_url)
            .await
            .expect("the test role should be able to connect");
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the clock should not predate 1970")
            .as_millis();
        // A counter as well as the clock: two tests in the same process can
        // start in the same millisecond, and `CREATE DATABASE` then collides on
        // the name
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let name = format!("cormex_principal_{}_{}_{seq}", std::process::id(), millis);
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

    pub async fn drop(self) {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        )))
        .execute(&self.admin)
        .await;
        self.admin.close().await;
    }
}

pub fn database_url() -> Option<String> {
    let _ = dotenvy::dotenv();
    std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty())
}

/// Build a live state on `url`, with the accounts system attached.
pub async fn live_state(url: &str) -> (crate::state::AppState, crate::state::Runtime) {
    let (config, rt, live) = build_live(url).await;
    let st = crate::state::AppState::for_tests_live(live, rt.clone(), config);
    (st, rt)
}

/// A live state backed by a **temporary local object store**.
///
/// The capture tests need real bytes to land somewhere: `put_chunk` writes the
/// object before the row, and with the "unavailable" store the first upload
/// fails before any of the ordering it exists to check can be observed.
pub async fn live_state_with_blobs(
    url: &str,
) -> (
    crate::state::AppState,
    crate::state::Runtime,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().expect("a temp dir");
    let local = cormex_blob::LocalFsBlobStore::new(
        dir.path(),
        cormex_blob::TenantPrefix::new("public").expect("a legal tenant prefix"),
    )
    .await
    .expect("the local backend should come up");
    let blobs = crate::blobs::MediaStore::with_backend(std::sync::Arc::new(local));

    let (config, rt, live) = build_live(url).await;
    let st = crate::state::AppState::for_tests_live_with_blobs(live, rt.clone(), config, blobs);
    (st, rt, dir)
}

/// The pieces every live test state is built from.
async fn build_live(
    url: &str,
) -> (
    cormex_core::config::Config,
    crate::state::Runtime,
    crate::live::Live,
) {
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
    (config, rt, live)
}

/// Send a request with **raw bytes** and extra headers.
///
/// The capture endpoints take a byte body with metadata in headers, so the JSON
/// helper cannot express them. Returns the status and the raw response body.
pub async fn call_bytes(
    app: &axum::Router,
    method: Method,
    path: &str,
    bearer: &str,
    headers: &[(&str, String)],
    body: Vec<u8>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let req = req
        .body(Body::from(body))
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
    (status, bytes.to_vec())
}

pub async fn call(
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

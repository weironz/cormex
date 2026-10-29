//! `cormex login` against a fake server.
//!
//! # Why a fake server rather than a mock object
//!
//! The thing under test is the **sequence**: log in, mint a key with that
//! access token, then revoke the refresh token. A mock would assert the calls
//! were made; a fake server asserts that the second call actually carried the
//! token the first one returned — which is the step whose absence is invisible
//! until an end-to-end run.

use std::sync::{Arc, Mutex};

use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use cormex::cmd::login;
use cormex::config::Config;

/// What the fake server saw.
#[derive(Default)]
struct Seen {
    login_bodies: Vec<serde_json::Value>,
    device_auth: Vec<String>,
    device_bodies: Vec<serde_json::Value>,
    logout_bodies: Vec<serde_json::Value>,
}

async fn start(seen: Arc<Mutex<Seen>>) -> String {
    let app = Router::new()
        .route(
            "/auth/login",
            post({
                let seen = Arc::clone(&seen);
                move |Json(body): Json<serde_json::Value>| {
                    let seen = Arc::clone(&seen);
                    async move {
                        seen.lock().unwrap().login_bodies.push(body);
                        Json(json!({
                            "access_token": "access-1",
                            "access_expires_in_secs": 900,
                            "refresh_token": "refresh-1",
                            "refresh_expires_in_secs": 2592000,
                            "user_id": "01USER"
                        }))
                    }
                }
            }),
        )
        .route(
            "/devices",
            post({
                let seen = Arc::clone(&seen);
                move |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| {
                    let seen = Arc::clone(&seen);
                    async move {
                        let auth = headers
                            .get(axum::http::header::AUTHORIZATION)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or_default()
                            .to_owned();
                        seen.lock().unwrap().device_auth.push(auth);
                        seen.lock().unwrap().device_bodies.push(body);
                        Json(json!({
                            "id": "01DEVICE",
                            "key": "the-device-key",
                            "scopes": ["capture:write", "session:read", "memory:read"]
                        }))
                    }
                }
            }),
        )
        .route(
            "/auth/logout",
            post({
                let seen = Arc::clone(&seen);
                move |Json(body): Json<serde_json::Value>| {
                    let seen = Arc::clone(&seen);
                    async move {
                        seen.lock().unwrap().logout_bodies.push(body);
                        Json(json!({}))
                    }
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn login_mints_a_key_with_the_access_token_and_revokes_the_refresh_token() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let url = start(Arc::clone(&seen)).await;

    let dir = tempfile::tempdir().expect("a temp dir");
    let mut cfg = Config {
        server_url: url,
        device_label: "test-laptop".into(),
        ..Config::default()
    };

    let outcome = login::login_with_password(
        &mut cfg,
        dir.path(),
        "alice",
        "correct horse battery",
        &login::DEFAULT_SCOPES
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>(),
    )
    .await
    .expect("login should succeed");

    assert_eq!(outcome.device_id, "01DEVICE");
    assert_eq!(outcome.label, "test-laptop");

    let seen = seen.lock().unwrap();

    // 1. The password went to /auth/login, with the device label
    assert_eq!(seen.login_bodies.len(), 1);
    assert_eq!(seen.login_bodies[0]["username"], "alice");
    assert_eq!(
        seen.login_bodies[0]["device_label"], "test-laptop",
        "the label is the only thing that can answer 'whose credential leaked' when one account \
         is on three machines"
    );

    // 2. **The device key was minted with the access token the login returned.**
    assert_eq!(
        seen.device_auth,
        vec!["Bearer access-1".to_owned()],
        "POST /devices did not carry the access token from /auth/login"
    );
    assert_eq!(
        seen.device_bodies[0]["scopes"],
        json!(["capture:write", "session:read", "memory:read"]),
        "the default scopes are the three, and not the two that widen a lost laptop"
    );

    // 3. **The refresh token was revoked.**
    assert_eq!(
        seen.logout_bodies.len(),
        1,
        "the refresh token was not revoked; the CLI would hold a rotating credential that \
         concurrent hooks trip as a leak"
    );
    assert_eq!(seen.logout_bodies[0]["refresh_token"], "refresh-1");

    // 4. The key is readable back, from wherever it landed.
    //
    // **Not** asserting it is in the file: a developer machine has a keyring,
    // and the key goes there. Asserting the location would make this test pass
    // only in a container -- and then the file fallback, which is the path a
    // headless deployment actually uses, would never be exercised on the
    // machine where it matters.
    assert!(
        outcome.stored_in == "system keyring" || outcome.stored_in == "file (0600)",
        "the key was stored somewhere unexpected: {}",
        outcome.stored_in
    );
    // The key is where `stored_in` says it is. A keyring entry is keyed by the
    // **server URL**, and the fake server's port changes per run, so a keyring
    // write here is genuinely ephemeral -- what matters is that the file
    // fallback writes the right bytes, which is the path a headless deployment
    // uses.
    if outcome.stored_in == "file (0600)" {
        let stored = std::fs::read_to_string(dir.path().join("credentials")).expect("the key file");
        assert_eq!(
            stored.trim(),
            "the-device-key",
            "the file fallback wrote something other than the key the server issued"
        );
    }
}

#[tokio::test]
async fn a_wrong_password_does_not_store_anything() {
    let app = Router::new().route(
        "/auth/login",
        post(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "wrong username or password" })),
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let dir = tempfile::tempdir().expect("a temp dir");
    let mut cfg = Config {
        server_url: format!("http://{addr}"),
        ..Config::default()
    };
    let err = login::login_with_password(&mut cfg, dir.path(), "alice", "wrong", &[])
        .await
        .expect_err("a refused login is an error");
    assert!(
        err.to_string().contains("401") || err.to_string().contains("log in"),
        "the error should say the login failed: {err}"
    );
    assert!(
        !dir.path().join("credentials").exists(),
        "a failed login wrote a credential file -- there is no key to write, and a stale file \
         would be picked up by the next run"
    );
}

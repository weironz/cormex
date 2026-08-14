//! Verifying `ApiEmbedder`'s **wire format** — stand up a real HTTP server and
//! make it issue real requests.
//!
//! # Why the unit tests are not enough
//!
//! The unit tests in `embed_api.rs` cover pure functions (assembling the URL,
//! checking the dimension, reordering by index). They cannot catch this class:
//! which path the request hit, what shape the body has, whether the auth header
//! was attached.
//!
//! And that is precisely where something already went wrong once: the first
//! version of `resolve_url` tested the whole string with
//! `ends_with("/embeddings")`, so `http://embeddings` (**the default address of
//! that service in compose**) was taken for "already a complete path", the
//! request went to the root path verbatim and got a 404 — a symptom that looks
//! like "the service didn't come up".
//! The unit test only caught it incidentally, while this assertion was being
//! added.
//!
//! This file stands up a minimal fake embedding service with axum and asserts on
//! the **protocol layer**: path, method, `Authorization`, request-body fields,
//! and whether an out-of-order response can be put back in order.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use cortex_memory::embed::{EMBEDDING_DIM, Embedder};
use cortex_memory::embed_api::{ApiEmbedder, ApiEmbedderConfig};
use serde_json::{Value, json};

/// The one request the fake service recorded, for the tests to assert on.
#[derive(Default, Clone)]
struct Seen {
    path: String,
    auth: Option<String>,
    body: Option<Value>,
}

type Shared = Arc<Mutex<Seen>>;

/// Build a valid unit vector: dimension `i` is 1, everything else 0.
fn unit(i: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; EMBEDDING_DIM];
    v[i] = 1.0;
    v
}

async fn handler(
    State(seen): State<Shared>,
    headers: axum::http::HeaderMap,
    body: Json<Value>,
) -> Json<Value> {
    let n = body
        .get("input")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    {
        let mut g = seen.lock().expect("lock");
        g.auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        g.body = Some(body.0.clone());
    }
    // **Deliberately returned in reverse order**: the protocol makes no ordering
    // guarantee, and a misalignment raises no error at all — it just makes
    // retrieval inexplicably worse. The client must reorder by index
    let data: Vec<Value> = (0..n)
        .rev()
        .map(|i| json!({ "index": i, "embedding": unit(i) }))
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

/// Start the service and return its address. The path is deliberately the
/// standard `/v1/embeddings`.
async fn spawn(seen: Shared) -> SocketAddr {
    let app = Router::new()
        .route(
            "/v1/embeddings",
            post({
                let seen = seen.clone();
                move |s, h, b| {
                    seen.lock().expect("lock").path = "/v1/embeddings".into();
                    handler(s, h, b)
                }
            }),
        )
        .with_state(seen);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port");
    let addr = listener.local_addr().expect("read back the address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("fake server died");
    });
    addr
}

fn cfg(endpoint: String, api_key: Option<String>) -> ApiEmbedderConfig {
    ApiEmbedderConfig {
        endpoint,
        model: "bge-m3".into(),
        api_key,
        batch: 10,
    }
}

#[tokio::test]
async fn talks_the_openai_wire_format() {
    let seen: Shared = Arc::default();
    let addr = spawn(seen.clone()).await;

    // Host only, no path — the client has to fill in /v1/embeddings itself,
    // otherwise the request hits the root path and gets a 404.
    //
    // ⚠️ This does **not** cover the nastier case where the host name is
    // literally called embeddings: the fake service can only bind 127.0.0.1,
    // while that bug needs the `//embeddings` slashes inside `http://embeddings`
    // to trigger. Verified by fault injection: revert resolve_url to testing the
    // whole string and this test **still passes green**; only that one unit test
    // in embed_api.rs goes red. Do not let the "host only" assertion here fool
    // you into thinking this file guards that pitfall.
    let e = ApiEmbedder::new(cfg(format!("http://{addr}"), Some("sk-test-key".into())))
        .expect("construct");

    let texts = vec!["第一句".to_string(), "第二句".to_string()];
    let got = e.embed(&texts).await.expect("the request should succeed");

    let g = seen.lock().expect("lock").clone();
    assert_eq!(
        g.path, "/v1/embeddings",
        "given only a host it must be completed to /v1/embeddings — hitting the root \
         path is a 404, and that symptom looks like \"the service didn't come up\""
    );
    assert_eq!(
        g.auth.as_deref(),
        Some("Bearer sk-test-key"),
        "with an api_key configured, the Authorization header must be sent"
    );

    let body = g.body.expect("the request body was recorded");
    assert_eq!(
        body["model"], "bge-m3",
        "the model field must go out as configured"
    );
    assert_eq!(
        body["encoding_format"], "float",
        "float must be requested explicitly — leave it out and some services default \
         to base64, and since our deserialization only accepts arrays that surfaces \
         as an unintelligible parse error"
    );
    assert_eq!(
        body["input"],
        json!(["第一句", "第二句"]),
        "input should be the text array, verbatim"
    );

    // The fake service returns in reverse order; the client must reorder
    assert_eq!(got.len(), 2, "two inputs should yield two vectors");
    assert_eq!(
        (got[0][0], got[0][1]),
        (1.0, 0.0),
        "the first must be index=0 — return order is not guaranteed, reorder by index"
    );
    assert_eq!(
        (got[1][0], got[1][1]),
        (0.0, 1.0),
        "the second should be the index=1 one"
    );
}

#[tokio::test]
async fn omits_the_auth_header_when_no_key_is_configured() {
    // A self-hosted TEI carries no authentication. Sending a Bearer header it
    // does not need is usually harmless, but "no key configured yet an empty
    // Bearer sent" is judged an auth failure by some gateways — and that is a
    // 401 nobody is going to trace back to "I didn't configure a key"
    let seen: Shared = Arc::default();
    let addr = spawn(seen.clone()).await;
    let e = ApiEmbedder::new(cfg(format!("http://{addr}/v1/embeddings"), None)).expect("construct");

    e.embed(&["x".to_string()]).await.expect("request succeeds");

    assert_eq!(
        seen.lock().expect("lock").auth,
        None,
        "with no api_key configured, no Authorization header should be sent"
    );
}

#[tokio::test]
async fn batches_are_split_by_the_configured_size() {
    // Vendors' batch ceilings differ wildly (TEI defaults to 32, Alibaba
    // Bailian's text-embedding-v3 to 10). Getting the split wrong shows up as a
    // 400, not as slowness — so what this asserts is that it really did split
    let seen: Shared = Arc::default();
    let addr = spawn(seen.clone()).await;
    let mut c = cfg(format!("http://{addr}"), None);
    c.batch = 2;
    let e = ApiEmbedder::new(c).expect("construct");

    let texts: Vec<String> = (0..5).map(|i| format!("第{i}句")).collect();
    let got = e.embed(&texts).await.expect("the request should succeed");

    assert_eq!(
        got.len(),
        5,
        "five inputs should yield five vectors (stitched together across three requests)"
    );
    // The last request has only one left (5 = 2 + 2 + 1)
    let last = seen
        .lock()
        .expect("lock")
        .body
        .clone()
        .expect("the request body was recorded");
    assert_eq!(
        last["input"].as_array().map(Vec::len),
        Some(1),
        "with batch=2, five inputs should split into 2+2+1, leaving one in the last request"
    );
}

#[tokio::test]
async fn a_wrong_dimension_fails_loudly_rather_than_reaching_postgres() {
    // When you have wired up the wrong vendor (OpenAI's text-embedding-3-small
    // is 1536-dimensional), the worst outcome is not an error — it is pushing
    // the problem down to Postgres's VECTOR(1024) constraint, where the caller
    // takes it for an ordinary write failure
    let app = Router::new().route(
        "/v1/embeddings",
        post(|| async {
            Json(json!({ "data": [{ "index": 0, "embedding": vec![0.1f32; 1536] }] }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port");
    let addr = listener.local_addr().expect("read back the address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("fake server died") });

    let e = ApiEmbedder::new(ApiEmbedderConfig {
        endpoint: format!("http://{addr}"),
        model: "text-embedding-3-small".into(),
        api_key: None,
        batch: 10,
    })
    .expect("construct");

    let err = e
        .embed(&["x".to_string()])
        .await
        .expect_err("1536 dimensions must be rejected on the client side");
    let msg = err.to_string();
    assert!(
        msg.contains("1536") && msg.contains("1024"),
        "the error must state both the actual and the expected dimension: {msg}"
    );
}

#[tokio::test]
async fn a_4xx_is_not_retried() {
    // A wrong model name / failed authentication / malformed request gives the
    // same result no matter how many times you retry. Retrying only delays a
    // perfectly clear error by a factor of three before anyone sees it
    let hits = Arc::new(Mutex::new(0usize));
    let h = hits.clone();
    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let h = h.clone();
            async move {
                *h.lock().expect("lock") += 1;
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "model not found" })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a port");
    let addr = listener.local_addr().expect("read back the address");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("fake server died") });

    let e = ApiEmbedder::new(cfg(format!("http://{addr}"), None)).expect("construct");
    let err = e
        .embed(&["x".to_string()])
        .await
        .expect_err("a 400 should fail outright");

    assert_eq!(
        *hits.lock().expect("lock"),
        1,
        "a 4xx should be requested once — retrying cannot rescue it, only treble the delay"
    );
    assert!(
        err.to_string().contains("model not found"),
        "the server's error body must be carried back, otherwise diagnosis is guesswork: {err}"
    );
}

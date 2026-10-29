//! The stdio bridge against a fake MCP server.
//!
//! # What this checks that the unit tests cannot
//!
//! The unit tests cover header extraction and payload splitting in isolation.
//! What they cannot see is the **sequence**: a `tools/list` forwarded with the
//! right headers, a `tools/call` after it, and — on the legacy protocol — the
//! session id from `initialize` carried on every later request. A bridge that
//! dropped the session id would work against the stateless protocol and fail
//! against every older server, with the host seeing "no reply".

use std::sync::{Arc, Mutex};

use axum::routing::post;
use axum::{Json, Router};
use serde_json::json;

use cormex::cmd::mcp;

#[derive(Default)]
struct Seen {
    methods: Vec<String>,
    names: Vec<String>,
    sessions: Vec<Option<String>>,
}

async fn start(seen: Arc<Mutex<Seen>>) -> String {
    let app = Router::new().route(
        "/mcp",
        post({
            let seen = Arc::clone(&seen);
            move |headers: axum::http::HeaderMap, body: String| {
                let seen = Arc::clone(&seen);
                async move {
                    let method = headers
                        .get("mcp-method")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    let name = headers
                        .get("mcp-name")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_owned();
                    let session = headers
                        .get("mcp-session-id")
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    {
                        let mut s = seen.lock().unwrap();
                        s.methods.push(method.clone());
                        s.names.push(name);
                        s.sessions.push(session);
                    }
                    let _ = body;

                    // `initialize` answers with a session id, as the legacy
                    // protocol does
                    if method == "initialize" {
                        return (
                            axum::http::StatusCode::OK,
                            [("mcp-session-id", "session-1")],
                            Json(json!({
                                "jsonrpc": "2.0",
                                "id": 1,
                                "result": {
                                    "protocolVersion": "2025-06-18",
                                    "capabilities": {},
                                    "serverInfo": { "name": "fake", "version": "0" }
                                }
                            })),
                        );
                    }
                    (
                        axum::http::StatusCode::OK,
                        [("mcp-session-id", "")],
                        Json(json!({
                            "jsonrpc": "2.0",
                            "id": 2,
                            "result": { "tools": [] }
                        })),
                    )
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

/// The header extraction is the part that decides whether the server routes
/// the request at all.
#[tokio::test]
async fn the_bridge_sends_the_method_and_name_headers() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let url = start(Arc::clone(&seen)).await;
    let client = cormex::ApiClient::new(&url, "key").expect("client");

    // One `tools/call`, as the bridge would forward it
    let body = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"memory_search","arguments":{"query":"x"}}}"#;
    let (_, _, _) = client
        .post_raw("/mcp", &mcp::headers_for(body), body.as_bytes().to_vec())
        .await
        .expect("post");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.methods, vec!["tools/call"]);
    assert_eq!(seen.names, vec!["memory_search"]);
}

/// The session id from `initialize` is carried on later requests.
#[tokio::test]
async fn the_session_id_is_carried_forward() {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let url = start(Arc::clone(&seen)).await;
    let client = cormex::ApiClient::new(&url, "key").expect("client");

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;
    let (_, _, session) = client
        .post_raw("/mcp", &mcp::headers_for(init), init.as_bytes().to_vec())
        .await
        .expect("initialize");
    assert_eq!(
        session.as_deref(),
        Some("session-1"),
        "the server's session id should come back from initialize"
    );

    // The bridge would now send it on the next request
    let mut headers = mcp::headers_for(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
    headers.push(("mcp-session-id", "session-1".to_owned()));
    client
        .post_raw(
            "/mcp",
            &headers,
            br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_vec(),
        )
        .await
        .expect("tools/list");

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.sessions.last().cloned().flatten().as_deref(),
        Some("session-1"),
        "the session id must be carried on every later request, or the legacy protocol \
         rejects them and the host sees no reply"
    );
}

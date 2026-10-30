//! `cormex sessions ls` / `show` against a fake server.
//!
//! # Why a snapshot of the rendered text
//!
//! The output is the interface: a script greps it, an agent parses it, a person
//! reads it. A test that only checked "it did not error" would let the shape
//! drift — and the drift is silent, because the CLI still exits 0.

use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use cormex::ApiClient;
use cormex::cmd::sessions;

async fn start() -> String {
    let app = Router::new()
        .route(
            "/archive/sessions",
            get(|| async {
                Json(json!({
                    "sessions": [
                        {
                            "id": "01SESSIONA",
                            "agent": "claude_code",
                            "title": "对象存储用什么",
                            "project_key": "github.com/u/p",
                            "devices": ["01D1"],
                            "first_at": "2026-10-29T08:00:00Z",
                            "last_at": "2026-10-29T09:30:00Z",
                            "event_count": 12,
                            "sole_copy": false
                        },
                        {
                            "id": "01SESSIONB",
                            "agent": "codex",
                            "project_key": "github.com/u/q",
                            "devices": ["01D2"],
                            "first_at": "2026-10-28T08:00:00Z",
                            "last_at": "2026-10-28T09:00:00Z",
                            "event_count": 3,
                            "sole_copy": true
                        }
                    ]
                }))
            }),
        )
        .route(
            "/archive/sessions/01SESSIONA",
            get(|| async {
                Json(json!({
                    "session": {
                        "id": "01SESSIONA",
                        "agent": "claude_code",
                        "title": "对象存储用什么",
                        "project_key": "github.com/u/p",
                        "devices": ["01D1"],
                        "first_at": "2026-10-29T08:00:00Z",
                        "last_at": "2026-10-29T09:30:00Z",
                        "event_count": 3,
                        "sole_copy": false
                    },
                    "files": [{
                        "id": "01FILE",
                        "role": "transcript",
                        "device_id": "01D1",
                        "logical_path": "projects/-p/s1.jsonl",
                        "generations": [{
                            "id": "01GEN",
                            "ordinal": 0,
                            "reason": "initial",
                            "end_offset": 100,
                            "created_at": "2026-10-29T08:00:00Z"
                        }]
                    }],
                    "parser_version": "claude_code@1",
                    "projection_status": "ok"
                }))
            }),
        )
        .route(
            "/archive/sessions/01SESSIONA/events",
            get(|| async {
                Json(json!({
                    "events": [
                        {
                            "id": "01E1",
                            "seq": 0,
                            "kind": "user_message",
                            "occurred_at": "2026-10-29T08:00:00Z",
                            "content": [{"type": "text", "text": "对象存储用的什么"}],
                            "text": "对象存储用的什么",
                            "source": {"generation_id": "01GEN", "offset": 0, "length": 10}
                        },
                        {
                            "id": "01E2",
                            "seq": 1,
                            "kind": "meta",
                            "occurred_at": "2026-10-29T08:00:01Z",
                            "content": [{"type": "unknown", "raw_type": "mode"}],
                            "source": {"generation_id": "01GEN", "offset": 10, "length": 5}
                        },
                        {
                            "id": "01E3",
                            "seq": 2,
                            "kind": "agent_message",
                            "occurred_at": "2026-10-29T08:00:02Z",
                            "content": [{"type": "text", "text": "用的是 RustFS"}],
                            "text": "用的是 RustFS",
                            "source": {"generation_id": "01GEN", "offset": 15, "length": 10}
                        }
                    ]
                }))
            }),
        )
        .route(
            "/archive/raw/01GEN",
            get(|| async {
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/octet-stream")],
                    b"the original bytes\n".to_vec(),
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
    format!("http://{addr}")
}

#[tokio::test]
async fn ls_renders_a_table_and_marks_the_only_copy() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let out = sessions::ls(&client, None, None, 50, false)
        .await
        .expect("ls");

    assert!(out.contains("01SESSIONA"), "{out}");
    assert!(out.contains("对象存储用什么"), "{out}");
    assert!(
        out.contains("[only copy]"),
        "the session whose host files are gone must be marked: {out}"
    );
    // The header row is there, so a reader knows what the columns are
    assert!(
        out.contains("id") && out.contains("agent") && out.contains("events"),
        "{out}"
    );
}

#[tokio::test]
async fn ls_json_is_parseable() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let out = sessions::ls(&client, None, None, 50, true)
        .await
        .expect("ls");
    let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
    assert_eq!(
        v["sessions"].as_array().map(Vec::len),
        Some(2),
        "the JSON must be the server's response, not a re-rendered form"
    );
}

#[tokio::test]
async fn show_renders_time_role_and_text() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let out = sessions::show(&client, "01SESSIONA", "transcript", false, 100)
        .await
        .expect("show");

    assert!(out.contains("对象存储用什么"), "the header: {out}");
    assert!(
        out.contains("[2026-10-29 08:00:00] user: 对象存储用的什么"),
        "the documented shape is `[time] role: text`: {out}"
    );
    assert!(
        out.contains("[2026-10-29 08:00:02] agent: 用的是 RustFS"),
        "{out}"
    );
    assert!(
        !out.contains("mode"),
        "a meta event must not be printed -- it is part of the session but not something \
         anyone said: {out}"
    );
}

#[tokio::test]
async fn show_raw_prints_the_original_bytes() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let out = sessions::show(&client, "01SESSIONA", "transcript", true, 100)
        .await
        .expect("show --raw");
    assert_eq!(out, "the original bytes\n");
}

#[tokio::test]
async fn an_unreachable_server_is_an_error_not_an_empty_list() {
    // A port nothing listens on
    let client = ApiClient::new("http://127.0.0.1:1", "key").expect("client");
    let err = sessions::ls(&client, None, None, 50, false)
        .await
        .expect_err("an unreachable server is an error");
    assert!(
        err.to_string().contains("could not reach"),
        "the error should say the server is unreachable, not that there are no sessions: {err}"
    );
}

//! Export → restore → the bytes match.
//!
//! # Why a round trip rather than a unit test
//!
//! The restore is only correct if the tar the server builds is the tar the CLI
//! reads, with the paths the CLI expects. A test of either half alone would pass
//! while the two disagreed — and the disagreement shows up as "the session is on
//! disk but `--resume` cannot find it", which points at neither half.

use std::path::Path;

use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;

use cormex::ApiClient;
use cormex::cmd::restore;

/// The session's transcript, as the server would have stored it.
const TRANSCRIPT: &[u8] =
    b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n";

async fn start() -> String {
    let app = Router::new()
        .route(
            "/archive/sessions/01SESSION",
            get(|| async {
                Json(json!({
                    "session": {
                        "id": "01SESSION",
                        "agent": "claude_code",
                        "title": "t",
                        "devices": [],
                        "event_count": 1,
                        "sole_copy": true
                    },
                    "files": [],
                    "parser_version": "claude_code@1",
                    "projection_status": "ok"
                }))
            }),
        )
        .route(
            "/archive/sessions/01SESSION/export",
            get(|| async {
                // Build the same tar the server builds
                let mut builder = tar::Builder::new(Vec::new());
                let mut header = tar::Header::new_gnu();
                header.set_size(TRANSCRIPT.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, "projects/-home-u-proj/s1.jsonl", TRANSCRIPT)
                    .expect("append");
                let tar = builder.into_inner().expect("finish");
                let compressed = zstd::encode_all(tar.as_slice(), 3).expect("compress");
                (
                    axum::http::StatusCode::OK,
                    [("content-type", "application/zstd")],
                    compressed,
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
async fn an_export_restores_to_the_original_bytes() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let home = tempfile::tempdir().expect("home");

    let report = restore::run(&client, "01SESSION", home.path(), None, false)
        .await
        .expect("restore");

    assert_eq!(report.files.len(), 1);
    let restored = &report.files[0];
    assert_eq!(
        restored,
        &home.path().join("projects/-home-u-proj/s1.jsonl"),
        "the file should land at its logical path under the agent's home"
    );
    assert_eq!(
        std::fs::read(restored).expect("read"),
        TRANSCRIPT,
        "the restored bytes differ from the original"
    );
}

#[tokio::test]
async fn an_existing_file_is_refused_without_force() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let home = tempfile::tempdir().expect("home");

    // Put something in the way
    let target = home.path().join("projects/-home-u-proj/s1.jsonl");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
    std::fs::write(&target, b"the newer one").expect("write");

    let report = restore::run(&client, "01SESSION", home.path(), None, false)
        .await
        .expect("restore");

    assert!(
        report.files.is_empty(),
        "a restore overwrote an existing file without --force"
    );
    assert_eq!(report.skipped.len(), 1);
    assert_eq!(
        std::fs::read(&target).expect("read"),
        b"the newer one",
        "the existing file was replaced -- the file on disk may be newer than the one being \
         restored, and a silent replacement is the one operation here that loses data"
    );
}

#[tokio::test]
async fn force_overwrites() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let home = tempfile::tempdir().expect("home");

    let target = home.path().join("projects/-home-u-proj/s1.jsonl");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
    std::fs::write(&target, b"the old one").expect("write");

    let report = restore::run(&client, "01SESSION", home.path(), None, true)
        .await
        .expect("restore");

    assert_eq!(report.files.len(), 1);
    assert_eq!(std::fs::read(&target).expect("read"), TRANSCRIPT);
}

#[tokio::test]
async fn cwd_moves_the_project_directory() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let home = tempfile::tempdir().expect("home");

    let report = restore::run(
        &client,
        "01SESSION",
        home.path(),
        Some("/home/u/elsewhere"),
        false,
    )
    .await
    .expect("restore");

    assert_eq!(
        report.files[0],
        home.path().join("projects/-home-u-elsewhere/s1.jsonl"),
        "--cwd should move the file into the new project's directory"
    );
    // **The recorded cwd inside the file is not rewritten.** The restored file
    // is a derivative, and rewriting its history to match the new location
    // would make it disagree with what happened
    assert_eq!(
        std::fs::read(&report.files[0]).expect("read"),
        TRANSCRIPT,
        "the file's contents must be untouched -- only its location changes"
    );
}

#[tokio::test]
async fn no_temp_file_is_left_behind() {
    let url = start().await;
    let client = ApiClient::new(&url, "key").expect("client");
    let home = tempfile::tempdir().expect("home");

    restore::run(&client, "01SESSION", home.path(), None, false)
        .await
        .expect("restore");

    let leftovers: Vec<_> = walk(home.path())
        .into_iter()
        .filter(|p| p.to_string_lossy().contains("cormex-tmp"))
        .collect();
    assert!(leftovers.is_empty(), "a temp file was left: {leftovers:?}");
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(walk(&p));
        } else {
            out.push(p);
        }
    }
    out
}

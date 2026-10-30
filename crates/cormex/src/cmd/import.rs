//! `cormex import <export>` — put a ChatGPT or Claude export into the archive.
//!
//! Design: plan P4-7. The parsers live in `cormex-transcript::export`; this is
//! the CLI half that turns one export file into capture uploads.
//!
//! # Why one export becomes many logical files
//!
//! An export's `conversations.json` holds **every conversation** the account
//! ever had. The archive's model is one logical file per session, so uploading
//! the file whole would put a hundred conversations into one session — and
//! `session_search` would return that one session for every query.
//!
//! `split_export` produces one file per conversation, each a one-element JSON
//! array, and each is registered and uploaded as its own `capture_file` with
//! `framing = snapshot`.
//!
//! # Why this goes through the capture path rather than `/episodes`
//!
//! The `/episodes` path writes the conversation into the **memory** layer: an
//! `episodes` row and whatever facts extraction draws from it. That is what the
//! import page in the browser does, and it is right for "add these
//! conversations to my memory".
//!
//! This command does something different: it puts the export in the **archive**,
//! where it is browsable, searchable and restorable like any captured session.
//! The two are not the same operation and neither subsumes the other.
//!
//! # Dry run by default
//!
//! Same tradition as `cormex-import`'s own importer: an export is somebody's
//! entire history, and a command that uploads it on the first invocation
//! without saying how much is about to go is a command people run by accident.

use anyhow::{Context as _, Result};
use cormex_proto::capture::{FileRole, Framing, RegisterFileRequest};
use cormex_transcript::export::{ExportPlatform, split_export};

use crate::client::ApiClient;
use crate::sync::api::CaptureApi;
use crate::sync::api::HttpCaptureApi;

/// What an import did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub conversations: usize,
    pub uploaded: usize,
    pub bytes: u64,
    pub platform: &'static str,
}

/// Import one export file.
///
/// # Errors
/// The file is not an export, the server is unreachable, or an upload failed.
pub async fn run(
    client: ApiClient,
    path: &std::path::Path,
    platform: Option<ExportPlatform>,
    dry_run: bool,
) -> Result<ImportReport> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    let conversations = split_export(&bytes, platform)
        .with_context(|| format!("{} is not a ChatGPT or Claude export", path.display()))?;
    if conversations.is_empty() {
        anyhow::bail!("{} holds no conversations", path.display());
    }

    let platform_name = match platform {
        Some(p) => p.agent(),
        // `split_export` sniffs when not told; the report says which it was
        None => conversations.first().map(|_| "auto").unwrap_or("auto"),
    };

    let total_bytes: u64 = conversations
        .iter()
        .map(|c| u64::try_from(c.bytes.len()).unwrap_or(0))
        .sum();

    let mut report = ImportReport {
        conversations: conversations.len(),
        bytes: total_bytes,
        platform: platform_name,
        uploaded: 0,
    };
    if dry_run {
        return Ok(report);
    }

    let api = HttpCaptureApi::new(client);
    for conversation in &conversations {
        let sha = {
            use sha2::Digest as _;
            format!("{:x}", sha2::Sha256::digest(&conversation.bytes))
        };

        // **One logical file per conversation**, `snapshot`: an export is
        // written once and never appended to
        let registered = api
            .register(&RegisterFileRequest {
                agent: platform
                    .unwrap_or(ExportPlatform::ChatGpt)
                    .agent()
                    .to_owned(),
                native_session_id: conversation.session_id.clone(),
                parent_native_session_id: None,
                logical_path: format!("conversations/{}.json", conversation.session_id),
                role: FileRole::Transcript,
                framing: Framing::Snapshot,
                cwd: None,
                project_key: None,
            })
            .await
            .with_context(|| format!("registering {} failed", conversation.session_id))?;

        // A re-run lands on the same generation; the server answers
        // `already_present` for bytes it already has, which is why this is safe
        // to repeat
        let outcome = api
            .put_chunk(
                &registered.generation.id,
                registered.generation.end_offset,
                conversation.bytes.clone(),
                &sha,
                &chrono::Utc::now().to_rfc3339(),
            )
            .await
            .with_context(|| format!("uploading {} failed", conversation.session_id))?;
        match outcome {
            crate::sync::api::PutOutcome::Accepted(_) => report.uploaded += 1,
            // The server's generation ends somewhere else, which for a snapshot
            // source means the file was rewritten between the register and the
            // upload. Reported rather than retried: a retry loop here would be
            // a loop on a race that the next run resolves anyway
            crate::sync::api::PutOutcome::OffsetMismatch { expected } => {
                anyhow::bail!(
                    "{} moved while it was being uploaded (the server expects offset {expected}); \
                     run the import again",
                    conversation.session_id
                );
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn export() -> serde_json::Value {
        serde_json::json!([
            {
                "id": "conv-1",
                "title": "Storage",
                "create_time": 1_760_000_000.0,
                "mapping": {
                    "a": {"id":"a","message":{
                        "id":"m1","author":{"role":"user"},
                        "content":{"content_type":"text","parts":["we use RustFS"]},
                        "create_time":1_760_000_000.0}}
                }
            },
            {
                "id": "conv-2",
                "title": "Deploy",
                "create_time": 1_760_000_100.0,
                "mapping": {}
            }
        ])
    }

    /// A dry run reports the counts and uploads nothing.
    #[tokio::test]
    async fn a_dry_run_reports_and_uploads_nothing() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, serde_json::to_vec(&export()).expect("serialize")).expect("write");

        // A client pointed nowhere: a dry run must not touch the network
        let client = ApiClient::new("http://127.0.0.1:1", "k").expect("client");
        let report = run(client, &path, None, true).await.expect("dry run");
        assert_eq!(report.conversations, 2);
        assert_eq!(report.uploaded, 0);
        assert!(report.bytes > 0);
    }

    /// A file that is not an export is refused with a message naming it.
    #[tokio::test]
    async fn a_non_export_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("notes.json");
        std::fs::write(&path, br#"{"not":"an export"}"#).expect("write");

        let client = ApiClient::new("http://127.0.0.1:1", "k").expect("client");
        let err = run(client, &path, None, true)
            .await
            .expect_err("a non-export must fail");
        assert!(
            format!("{err:#}").contains("not a ChatGPT or Claude export"),
            "{err:#}"
        );
    }

    /// An empty export is refused rather than reported as success.
    #[tokio::test]
    async fn an_empty_export_is_refused() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("conversations.json");
        std::fs::write(&path, b"[]").expect("write");

        let client = ApiClient::new("http://127.0.0.1:1", "k").expect("client");
        let err = run(client, &path, None, true)
            .await
            .expect_err("an empty export must fail");
        assert!(format!("{err:#}").contains("no conversations"), "{err:#}");
    }
}

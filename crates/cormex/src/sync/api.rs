//! The capture API, as the sync engine sees it.
//!
//! # Why a trait rather than calling `ApiClient` directly
//!
//! The engine's interesting behaviour is **recovery**: a 409 that says "the
//! server is ahead of you", a 409 that says "you are ahead of the server", a
//! network error mid-upload. None of those can be produced by a real server on
//! demand, and a test that only exercises the happy path would miss exactly the
//! branches the design spends the most words on.
//!
//! `FakeCaptureApi` implements the server's contiguity rules in memory, so the
//! engine can be driven through every one of them without a database.

use anyhow::Result;
use async_trait::async_trait;
use cormex_proto::capture::{
    AdoptGenerationRequest, ChunkAck, FileClassification, FileEventRequest, GenerationDto,
    NewGenerationRequest, RegisterFileRequest, RegisterFileResponse, VerifyPrefixRequest,
    VerifyPrefixResponse,
};

use crate::client::{ApiClient, ClientError};

/// The server does not know this file (or generation) as **this device's**.
///
/// `state.db` remembers file ids by agent and path, not by which device key
/// registered them. When the machine changes key (`cormex login` again, a key
/// minted for a test, a restore) and then changes back, the ids it remembers
/// can belong to the other device, and every write to them is refused: 404
/// "no such file for this device" on the file routes, 403 "belongs to another
/// device" on a chunk. Retrying the same id never helps. Forgetting the file's
/// state does: the next pass registers it under the current device and
/// settles the bytes by prefix hash and adoption, the same path a lost
/// `state.db` or a second machine takes, so nothing is uploaded twice.
#[derive(Debug)]
pub struct NotThisDevicesFile {
    pub detail: String,
}

impl std::fmt::Display for NotThisDevicesFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the server does not know this file as this device's ({})",
            self.detail
        )
    }
}

impl std::error::Error for NotThisDevicesFile {}

/// Whether an error from a [`CaptureApi`] call is [`NotThisDevicesFile`].
#[must_use]
pub fn is_not_this_devices_file(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NotThisDevicesFile>().is_some()
}

/// Is this answer the server saying "not your file"?
///
/// By the server's own reason, not by the status alone: an older server
/// without the metadata route also answers 404, and reading that as "not your
/// file" would reset and re-register every Codex file on every run.
fn owned_elsewhere(status: Option<reqwest::StatusCode>, detail: &str) -> bool {
    match status {
        Some(reqwest::StatusCode::NOT_FOUND) => [
            "no such file for this device",
            "no such capture file",
            "no such generation",
        ]
        .iter()
        .any(|reason| detail.contains(reason)),
        Some(reqwest::StatusCode::FORBIDDEN) => detail.contains("belongs to another device"),
        _ => false,
    }
}

/// The error for a call on one file or generation.
fn file_scoped(e: ClientError) -> anyhow::Error {
    if owned_elsewhere(e.status, &e.detail) {
        anyhow::Error::new(NotThisDevicesFile {
            detail: e.detail.trim().to_owned(),
        })
    } else {
        anyhow::anyhow!("{e}")
    }
}

/// What the engine needs from the server.
#[async_trait]
pub trait CaptureApi: Send + Sync {
    /// `POST /capture/files`.
    async fn register(&self, req: &RegisterFileRequest) -> Result<RegisterFileResponse>;

    /// `POST /capture/files/{id}/classification`.
    async fn update_classification(&self, file_id: &str, req: &FileClassification) -> Result<()>;

    /// `POST /capture/files/{id}/metadata`: the host's title, project and pin.
    async fn update_metadata(
        &self,
        file_id: &str,
        metadata: &cormex_proto::capture::SessionUiMetadata,
    ) -> Result<()>;

    /// `POST /capture/files/{id}/generations`.
    async fn open_generation(&self, file_id: &str, reason: &str) -> Result<GenerationDto>;

    /// `PUT …/chunks/{offset}`.
    ///
    /// Returns the ack on 200, or the server's `expected_offset` on 409. The
    /// two are one `Result` variant each rather than an error, because a 409 is
    /// a **normal** outcome the engine acts on, not a failure.
    async fn put_chunk(
        &self,
        generation_id: &str,
        offset: i64,
        body: Vec<u8>,
        sha256: &str,
        observed_at: &str,
    ) -> Result<PutOutcome>;

    /// `POST /capture/files/{id}/events`.
    async fn file_event(&self, file_id: &str, req: &FileEventRequest) -> Result<()>;

    /// Do the first `length` bytes of this generation hash to `sha256`?
    async fn verify_prefix(
        &self,
        generation_id: &str,
        length: i64,
        sha256: &str,
    ) -> Result<VerifyPrefixResponse>;

    /// Make this file follow an existing generation of the same stream.
    async fn adopt(
        &self,
        file_id: &str,
        generation_id: &str,
        verified_through: i64,
    ) -> Result<GenerationDto>;
}

/// What one chunk upload produced.
#[derive(Debug, Clone, PartialEq)]
pub enum PutOutcome {
    /// Stored, or already there. Both advance the local offset.
    Accepted(ChunkAck),
    /// The server's generation ends somewhere else. The engine resynchronises
    /// from `expected`.
    OffsetMismatch { expected: i64 },
}

/// The real one.
pub struct HttpCaptureApi {
    client: ApiClient,
}

impl HttpCaptureApi {
    #[must_use]
    pub const fn new(client: ApiClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl CaptureApi for HttpCaptureApi {
    async fn register(&self, req: &RegisterFileRequest) -> Result<RegisterFileResponse> {
        self.client
            .post_json("/capture/files", req)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn update_classification(&self, file_id: &str, req: &FileClassification) -> Result<()> {
        self.client
            .post_json::<_, serde_json::Value>(
                &format!("/capture/files/{file_id}/classification"),
                req,
            )
            .await
            .map(|_| ())
            .map_err(file_scoped)
    }

    async fn update_metadata(
        &self,
        file_id: &str,
        metadata: &cormex_proto::capture::SessionUiMetadata,
    ) -> Result<()> {
        self.client
            .post_json::<_, serde_json::Value>(
                &format!("/capture/files/{file_id}/metadata"),
                metadata,
            )
            .await
            .map(|_| ())
            .map_err(file_scoped)
    }

    async fn open_generation(&self, file_id: &str, reason: &str) -> Result<GenerationDto> {
        self.client
            .post_json(
                &format!("/capture/files/{file_id}/generations"),
                &NewGenerationRequest {
                    reason: match reason {
                        "rewrite" => cormex_proto::capture::GenerationReason::Rewrite,
                        "truncate" => cormex_proto::capture::GenerationReason::Truncate,
                        _ => cormex_proto::capture::GenerationReason::Snapshot,
                    },
                },
            )
            .await
            .map_err(file_scoped)
    }

    async fn put_chunk(
        &self,
        generation_id: &str,
        offset: i64,
        body: Vec<u8>,
        sha256: &str,
        observed_at: &str,
    ) -> Result<PutOutcome> {
        let path = format!("/capture/generations/{generation_id}/chunks/{offset}");
        let headers = vec![
            (
                cormex_proto::capture::HEADER_CONTENT_SHA256,
                sha256.to_owned(),
            ),
            (
                cormex_proto::capture::HEADER_OBSERVED_AT,
                observed_at.to_owned(),
            ),
        ];
        let (status, bytes) = self
            .client
            .put_bytes(&path, &headers, body)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;

        if status == reqwest::StatusCode::CONFLICT {
            let body: cormex_proto::capture::OffsetMismatchBody = serde_json::from_slice(&bytes)
                .map_err(|e| anyhow::anyhow!("the 409 body does not parse: {e}"))?;
            return Ok(PutOutcome::OffsetMismatch {
                expected: body.expected_offset,
            });
        }
        if !status.is_success() {
            return Err(file_scoped(crate::client::status_error(
                status,
                &path,
                String::from_utf8_lossy(&bytes).into_owned(),
            )));
        }
        Ok(PutOutcome::Accepted(serde_json::from_slice(&bytes)?))
    }

    async fn file_event(&self, file_id: &str, req: &FileEventRequest) -> Result<()> {
        self.client
            .post_json::<_, serde_json::Value>(&format!("/capture/files/{file_id}/events"), req)
            .await
            .map(|_| ())
            .map_err(file_scoped)
    }

    async fn verify_prefix(
        &self,
        generation_id: &str,
        length: i64,
        sha256: &str,
    ) -> Result<VerifyPrefixResponse> {
        self.client
            .post_json(
                &format!("/capture/generations/{generation_id}/verify"),
                &VerifyPrefixRequest {
                    length,
                    sha256: sha256.to_owned(),
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn adopt(
        &self,
        file_id: &str,
        generation_id: &str,
        verified_through: i64,
    ) -> Result<GenerationDto> {
        self.client
            .post_json(
                &format!("/capture/files/{file_id}/adopt"),
                &AdoptGenerationRequest {
                    generation_id: generation_id.to_owned(),
                    verified_through,
                },
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

#[cfg(test)]
mod not_this_devices_file_tests {
    use reqwest::StatusCode;

    use super::owned_elsewhere;

    #[test]
    fn the_servers_own_refusals_are_recognised() {
        for (status, body) in [
            (
                StatusCode::NOT_FOUND,
                r#"{"error":"no such file for this device"}"#,
            ),
            (StatusCode::NOT_FOUND, r#"{"error":"no such capture file"}"#),
            (StatusCode::NOT_FOUND, r#"{"error":"no such generation"}"#),
            (
                StatusCode::FORBIDDEN,
                r#"{"error":"this generation belongs to another device"}"#,
            ),
            (
                StatusCode::FORBIDDEN,
                r#"{"error":"this file belongs to another device"}"#,
            ),
        ] {
            assert!(
                owned_elsewhere(Some(status), body),
                "{status} {body} is the server saying the id is another device's"
            );
        }
    }

    #[test]
    fn a_missing_route_or_a_bad_key_is_not_a_reason_to_register_again() {
        for (status, body) in [
            // An older server without the metadata route: axum's empty 404
            (StatusCode::NOT_FOUND, ""),
            // A proxy's HTML page
            (StatusCode::NOT_FOUND, "<html>404 Not Found</html>"),
            // A key without capture:write: registering again would fail too
            (StatusCode::FORBIDDEN, r#"{"error":"forbidden"}"#),
            (StatusCode::UNAUTHORIZED, ""),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "no such file for this device",
            ),
        ] {
            assert!(
                !owned_elsewhere(Some(status), body),
                "{status} {body:?} must not reset local state: every run would re-register \
                 every file"
            );
        }
        assert!(!owned_elsewhere(None, "no such file for this device"));
    }
}

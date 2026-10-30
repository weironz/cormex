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
    ChunkAck, FileEventRequest, GenerationDto, NewGenerationRequest, RegisterFileRequest,
    RegisterFileResponse,
};

use crate::client::ApiClient;

/// What the engine needs from the server.
#[async_trait]
pub trait CaptureApi: Send + Sync {
    /// `POST /capture/files`.
    async fn register(&self, req: &RegisterFileRequest) -> Result<RegisterFileResponse>;

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
            .map_err(|e| anyhow::anyhow!("{e}"))
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
            anyhow::bail!("{path} returned {status}");
        }
        Ok(PutOutcome::Accepted(serde_json::from_slice(&bytes)?))
    }

    async fn file_event(&self, file_id: &str, req: &FileEventRequest) -> Result<()> {
        self.client
            .post_json::<_, serde_json::Value>(&format!("/capture/files/{file_id}/events"), req)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

//! Errors of the blob layer.
//!
//! Why these are kept apart from `CortexError::Store`: object storage fails in
//! completely different ways from a relational database (network jitter is
//! retryable, a 404 is normal control flow, an illegal hash is a security
//! event). Merge them into one `Store(String)` and they can never be told apart
//! again. Convergence happens at the boundary, not before.

use thiserror::Error;

pub type Result<T, E = BlobError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum BlobError {
    /// The hash is not 64 lowercase hex digits. **This is simultaneously the
    /// fence against path traversal**: the hash is the only external input that
    /// enters a storage path, and none of `..` / `/` / `\` is a hex character,
    /// so blocking it dead here makes every downstream path concatenation
    /// permanently safe.
    #[error("invalid blob hash: {0}")]
    InvalidHash(String),

    /// The tenant prefix is not a legal unquoted Postgres identifier.
    ///
    /// Kept alongside [`InvalidHash`](Self::InvalidHash) rather than merged into
    /// one variant, because their **origins** are completely different: a bad
    /// hash is most likely a client sending the wrong thing, whereas a bad
    /// tenant prefix can only be the server itself assembling a schema name
    /// wrong (the prefix never comes from a request body). Blend them into one
    /// log line and "a user sent dirty data" can never again be separated from
    /// "something is wrong with our tenant registry".
    #[error("invalid tenant prefix: {0}")]
    InvalidTenant(String),

    #[error("no such blob: {0}")]
    NotFound(String),

    /// The requested byte range is out of bounds or has its ends reversed.
    #[error("invalid range: {0}")]
    InvalidRange(String),

    /// The backend does not support this capability (for instance
    /// `LocalFsBlobStore` cannot issue presigned URLs). It gets a variant of its
    /// own so that callers can **degrade explicitly** (falling back to relaying
    /// through the server) rather than treating it as an ordinary failure and
    /// retrying it.
    #[error("unsupported by this backend: {0}")]
    Unsupported(&'static str),

    #[error("object storage error: {0}")]
    Backend(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// There is deliberately **no** `is_retryable()` here.
//
// One was written, of the form `matches!(self, Backend(_) | Io(_))`, and then
// turned out to be a trap: `Backend` carries both "connection refused" (worth
// retrying) and "SignatureDoesNotMatch / AccessDenied / NoSuchBucket" (which
// ten thousand retries will not change). An offline write queue that believed
// it would replay forever against a misconfigured key, while all the logs show
// is "still retrying".
//
// Giving a trustworthy answer would mean keeping the HTTP status code and the
// S3 error code in the error, which is a separate structural change. Until
// then, better to let callers decide by variant themselves — a half-true
// predicate is worse than none.
//
// Also note: for transient failures across the network, aws-sdk-s3 already
// retries three times under its Standard policy.

impl From<BlobError> for cortex_core::CortexError {
    fn from(e: BlobError) -> Self {
        match e {
            BlobError::NotFound(hash) => Self::NotFound {
                kind: "blob",
                id: hash,
            },
            BlobError::InvalidHash(_) | BlobError::InvalidRange(_) => Self::Invalid(e.to_string()),
            // InvalidTenant deliberately does **not** go to Invalid: it falls
            // into `other` and becomes a 500. The tenant prefix does not come
            // from the request body, it is something the server itself took out
            // of the tenant registry, so returning 400 would be telling the
            // client to fix a field it never sent.
            other => Self::Store(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Error classification eventually becomes an HTTP status code. The cost of
    /// getting it wrong is very concrete: a client that reads a 404 as a 500
    /// will retry for an object that will never appear, and treating illegal
    /// input as a 500 masks the caller's own bug.
    #[test]
    fn errors_map_to_the_right_http_status() {
        let cases = [
            (BlobError::NotFound("abc".into()), 404),
            (BlobError::InvalidHash("../x".into()), 400),
            (BlobError::InvalidRange("9..4".into()), 400),
            // 500 rather than 400: the tenant prefix is assembled by the server
            // itself, and the client has no way to correct it.
            (BlobError::InvalidTenant("../x".into()), 500),
            (BlobError::Backend("rustfs down".into()), 500),
            (BlobError::Unsupported("presign"), 500),
        ];
        for (e, want) in cases {
            let desc = e.to_string();
            let got = cortex_core::CortexError::from(e).http_status();
            assert_eq!(got, want, "{desc} should map to HTTP {want}, got {got}");
        }
    }

    #[test]
    fn not_found_carries_the_hash_through_the_conversion() {
        let e = cortex_core::CortexError::from(BlobError::NotFound("deadbeef".into()));
        let msg = e.to_string();
        assert!(
            msg.contains("deadbeef") && msg.contains("blob"),
            "after conversion it must still be possible to see which blob was missing, \
             otherwise a 404 in the log cannot be located: {msg}"
        );
    }
}

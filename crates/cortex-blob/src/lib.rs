//! Cortex object storage: content-addressed blob reads and writes.
//!
//! The foundation of the "archive every modality" story — images, audio, video
//! and documents all land here keyed by their SHA-256, while the three tables
//! `blobs` / `episode_blobs` / `blob_transcripts` hold nothing but pointers and
//! derived text.
//!
//! # The three things content addressing gives for free
//!
//! The key is determined by the content, and therefore:
//!
//! - **Deduplication for free**: one image referenced by ten episodes exists
//!   once on disk — but **only within one tenant**. The key carries a tenant
//!   prefix (see [`hash::storage_key`]), and cross-tenant deduplication is
//!   deliberately given up; what it buys back is a whole class of cross-schema
//!   GC bugs.
//! - **Idempotence for free**: an interrupted upload is fixed by replaying the
//!   whole thing, with no resumable-upload protocol needed — which is exactly
//!   the premise on which `docs/memory.md` §9 ("blobs go in a fixed three-step
//!   order; interruptions are replayed from the queue") rests.
//! - **Immutability for free**: the bytes under a key are forever the same
//!   bytes, so the notion of an "older version" does not exist at all — which
//!   is why RustFS not supporting object versioning does not matter.
//!
//! # Two backends
//!
//! | | Purpose |
//! |---|---|
//! | [`S3BlobStore`] | Production. Works with RustFS / MinIO / R2 / AWS S3 alike |
//! | [`LocalFsBlobStore`] | Tests, and "the first version runs without standing up RustFS" |
//!
//! Both are sealed behind [`BlobStore`]. RustFS is still alpha (the community
//! has documented unrecoverable metadata corruption on a full disk), and this
//! abstraction is the way out.

#![forbid(unsafe_code)]

mod error;
mod local;
mod s3;

pub mod hash;
pub mod media;

use std::{ops::Range, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use url::Url;

pub use error::{BlobError, Result};
pub use hash::TenantPrefix;
pub use local::LocalFsBlobStore;
pub use media::{MediaMeta, probe as probe_media};
pub use s3::{MULTIPART_PART_SIZE, MULTIPART_THRESHOLD, S3BlobStore, S3Options};

/// The upper bound on a presigned URL's lifetime. A hard limit of SigV4, not
/// one we chose.
pub const MAX_PRESIGN_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// The result of one write. The fields deliberately line up with
/// `cortex_store::NewBlob`, but this does **not** depend on that crate — the
/// dependency direction is `core ← {llm, store}`, and the blob layer sits
/// beside store, so depending backwards would form a cycle. cortexd does the
/// conversion at the boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// SHA-256 as lowercase hex, the same shape as the migration's `sha256`
    /// domain.
    pub hash: String,
    pub mime: String,
    pub size_bytes: i64,
    /// The object key. Stored into `blobs.storage_key`, derived by
    /// [`hash::storage_key`] from "tenant prefix + hash"; both backends share
    /// one layout, so switching backends does not require rewriting this
    /// column.
    pub storage_key: String,
    /// Whether this write hit an existing object (i.e. no bytes were actually
    /// uploaded).
    ///
    /// For observability only (deduplication rate, bandwidth saved). Callers
    /// should **not** use it to decide whether to write the `blobs` row — that
    /// row's idempotence is guaranteed by `ON CONFLICT (hash) DO NOTHING`, and
    /// relying on this boolean loses rows under concurrency.
    pub deduplicated: bool,
}

/// Content-addressed object storage.
///
/// # Notes for implementors
///
/// Every method taking a `hash` **must** run it through [`hash::validate_hash`]
/// first. That is not merely a format check; it is the one and only fence
/// against path traversal (see that function's documentation).
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Content-addressed write, returning the SHA-256. Skips the upload if the
    /// content already exists (deduplication for free).
    ///
    /// `declared_mime` is the client's claim and serves only as a fallback:
    /// whenever the type can be recognized from the leading bytes, the bytes
    /// win (browsers guess from the extension, mobile clients routinely send
    /// `octet-stream`; neither is trustworthy).
    ///
    /// # Writing the same content concurrently
    ///
    /// When two requests upload the same image at once, both see "not present"
    /// and both upload. **This is benign**: under content addressing the two
    /// writes target the same key with the same bytes, and whichever arrives
    /// second changes nothing. So there is deliberately no distributed lock
    /// here — introducing one for a negligible duplicate upload trades
    /// availability for bandwidth, which is a losing trade.
    ///
    /// What does deserve care is a backend whose writes are **not atomic**: a
    /// half-written object seen by another reader is corrupt data.
    /// [`LocalFsBlobStore`] therefore goes through "temp file + rename", while
    /// S3's PUT is atomic in itself.
    async fn put(&self, bytes: Bytes, declared_mime: Option<&str>) -> Result<BlobRef>;

    /// Fetch the complete content by hash.
    async fn get(&self, hash: &str) -> Result<Bytes>;

    /// Fetch the byte range `[start, end)` (half-open, per Rust convention; the
    /// implementation is responsible for converting to HTTP Range's inclusive
    /// end).
    ///
    /// Seeking in audio and video depends on this: the player only wants the
    /// segment at 31:40, and without range support it has to pull the whole
    /// video down.
    async fn get_range(&self, hash: &str, range: Range<u64>) -> Result<Bytes>;

    /// Issue a direct-upload URL; the client `PUT`s the bytes to it without
    /// relaying through cortexd.
    ///
    /// This halves the bandwidth when a phone uploads a video (otherwise the
    /// same bytes travel up to cortexd first and then up again to object
    /// storage).
    ///
    /// The caller must **compute the hash first** and only then ask for the URL
    /// — content addressing does not permit "upload first, settle the key
    /// later".
    async fn presign_put(&self, hash: &str, ttl: Duration) -> Result<Url>;

    /// Issue a direct-download URL; halves the bandwidth for the same reason.
    async fn presign_get(&self, hash: &str, ttl: Duration) -> Result<Url>;

    /// Whether the object exists.
    async fn exists(&self, hash: &str) -> Result<bool>;

    /// Delete the object. **For purge only** (`docs/memory.md` §11).
    ///
    /// # The caller's obligations
    ///
    /// Content addressing means one set of bytes may be referenced by several
    /// episodes. Before a purge you must confirm there is no other reference
    /// left in `episode_blobs` — otherwise what gets deleted is somebody else's
    /// attachment. That check **can only be done by the caller**: the blob layer
    /// cannot see Postgres, and forcing it to query would invert the dependency
    /// direction.
    ///
    /// Deleting a non-existent object counts as success (idempotence), so the
    /// cascading cleanup job can be rerun.
    async fn delete(&self, hash: &str) -> Result<()>;
}

/// Check that a presign ttl falls inside the interval SigV4 permits.
///
/// Out-of-range values are not silently clamped: issuing a URL shorter than the
/// caller believed turns into an intermittent "expires occasionally" fault,
/// which is an order of magnitude harder to diagnose than an outright error.
pub(crate) fn validate_ttl(ttl: Duration) -> Result<()> {
    if ttl.is_zero() {
        return Err(BlobError::Unsupported("presign ttl 不能为 0"));
    }
    if ttl > MAX_PRESIGN_TTL {
        return Err(BlobError::Backend(format!(
            "presign ttl {}s 超过 SigV4 上限 {}s（7 天）",
            ttl.as_secs(),
            MAX_PRESIGN_TTL.as_secs()
        )));
    }
    Ok(())
}

/// Check the shape of a range. Both an empty interval and reversed ends have to
/// be caught before the request goes out — S3 answers `bytes=5-4` with a 416,
/// and by then the error has taken a round trip across the network.
pub(crate) fn validate_range(range: &Range<u64>) -> Result<()> {
    if range.start >= range.end {
        return Err(BlobError::InvalidRange(format!(
            "空或颠倒的区间 [{}, {})",
            range.start, range.end
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_bounds_are_enforced() {
        assert!(
            validate_ttl(Duration::from_secs(300)).is_ok(),
            "5 minutes should be a legal ttl"
        );
        assert!(
            validate_ttl(MAX_PRESIGN_TTL).is_ok(),
            "exactly 7 days should be within the permitted range"
        );
        assert!(
            validate_ttl(Duration::ZERO).is_err(),
            "a ttl of 0 must be refused — issuing a URL that expires immediately is pure waste"
        );
        assert!(
            validate_ttl(MAX_PRESIGN_TTL + Duration::from_secs(1)).is_err(),
            "beyond 7 days must error rather than silently clamp, otherwise it becomes an \
             intermittent fault that expires now and then"
        );
    }

    #[test]
    fn degenerate_ranges_are_rejected() {
        assert!(
            validate_range(&(0..1)).is_ok(),
            "a single-byte range should be legal"
        );
        assert!(
            validate_range(&(5..5)).is_err(),
            "an empty range must be caught locally, otherwise it takes a network round trip to \
             come back as a 416"
        );
        // A struct literal to sidestep clippy's reversed_empty_ranges — a
        // reversed range is exactly what is wanted here, being the input under
        // test.
        assert!(
            validate_range(&Range { start: 9, end: 4 }).is_err(),
            "a range with its ends reversed must be refused"
        );
    }
}

//! Wiring for object storage: backend selection, presign policy, HTTP Range
//! arithmetic.
//!
//! `cormex-blob` provides content-addressed byte read/write; this module joins
//! it to the seam between HTTP and the `blobs` table — that is, between step one
//! and step two of the fixed three-step order in docs/memory.md §9:
//!
//! ```text
//! ① upload to RustFS  ──►  ② blobs row (sync_log written in the same txn)  ──►  ③ episode + episode_blobs
//! ```
//!
//! Reversing the order leaves a dangling reference where the database says the
//! object exists and object storage does not have it, and that **cannot heal
//! itself**: the client will forever pull down a record whose content can never
//! be fetched. The opposite case (the object is there, the row is not written
//! yet) is merely an orphaned object, fixed by one replay and reclaimable by the
//! server's periodic GC. Content addressing makes the whole sequence naturally
//! idempotent to replay, so this direction of the trade-off is free.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use cormex_blob::{BlobStore, LocalFsBlobStore, S3BlobStore, S3Options};
use cormex_core::{Config, CormexError, Result};

/// Lifetime of a presigned URL.
///
/// 15 minutes: long enough for a phone on a weak connection to finish uploading
/// a video, short enough that a URL leaked into a log or a screenshot expires
/// quickly. A constant rather than a configuration item — this number has no
/// reason to be tuned per deployment, and one more knob is one more place that
/// can be set wrong.
pub const PRESIGN_TTL: Duration = Duration::from_secs(15 * 60);

/// Size ceiling for server-relayed upload (`POST /blobs`).
///
/// Above 32 MiB the caller should use presigned direct upload: relaying means
/// the same bytes travel upstream to cormexd first and then upstream again from
/// there to object storage, which on a phone uploading video is paying double
/// the upstream bandwidth for nothing. The threshold is not set lower because
/// direct upload costs two extra round trips, which does not pay off on small
/// files.
pub const DIRECT_UPLOAD_LIMIT: usize = 32 * 1024 * 1024;

/// How many bytes are read when sniffing MIME.
///
/// Every common format's magic number is within the first 32 bytes; 4 KiB is
/// taken to leave headroom for probes that need to walk a few boxes, such as
/// PNG's IHDR or MP4's ftyp. A range read rather than a full fetch, because on
/// the direct-upload path the server has never seen these bytes — pulling down
/// an entire video just to identify a MIME type spends back exactly the
/// bandwidth we just saved.
const SNIFF_BYTES: u64 = 4096;

/// Default root directory of the local fallback backend.
const LOCAL_ROOT_ENV: &str = "CORMEX_BLOB_DIR";
const LOCAL_ROOT_DEFAULT: &str = ".cormex/blobs";

/// An object storage handle plus which backend it is.
#[derive(Clone)]
pub struct MediaStore {
    /// `None` means neither backend came up. In that state the blob endpoints
    /// all report an error rather than pretending to succeed — an upload that
    /// reports success while no bytes landed is bad data that vouches for
    /// itself.
    inner: Option<Arc<dyn BlobStore>>,
    backend: &'static str,
}

impl MediaStore {
    /// Prefer real object storage; on failure fall back to the local filesystem
    /// and **warn explicitly**.
    ///
    /// Same lineage as the database degradation policy: on a development
    /// machine, one alpha-stage process not being up should not stop the whole
    /// service from starting, but the degradation has to be spelled out in the
    /// log — otherwise "I uploaded it, and it was gone on the other machine"
    /// turns into an unsolved case.
    pub async fn connect(config: &Config) -> Self {
        match Self::try_s3(&config.s3).await {
            Ok(s3) => {
                tracing::info!(
                    endpoint = %config.s3.endpoint,
                    bucket = %config.s3.bucket,
                    "object storage is attached to an S3-compatible backend"
                );
                return Self {
                    inner: Some(Arc::new(s3)),
                    backend: "s3",
                };
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    endpoint = %config.s3.endpoint,
                    "object storage is unavailable, falling back to the local filesystem \
                     — uploaded media then exists only on this machine, is not covered \
                     by the object store's backups, and cannot be signed as a presigned URL"
                );
            }
        }

        let root = std::env::var(LOCAL_ROOT_ENV).unwrap_or_else(|_| LOCAL_ROOT_DEFAULT.to_string());
        match LocalFsBlobStore::new(&root, Self::current_tenant()).await {
            Ok(local) => {
                tracing::warn!(
                    root,
                    "object storage has fallen back to the local filesystem"
                );
                Self {
                    inner: Some(Arc::new(local)),
                    backend: "local_fs",
                }
            }
            Err(e) => {
                // Even the local directory could not be created (read-only
                // filesystem, insufficient permissions). At this point we must
                // not pretend object storage exists: far better that /blobs
                // report an error outright than accept the bytes and drop them.
                tracing::error!(error = %e, root, "the local fallback could not be created either; media is \
                     unavailable");
                Self {
                    inner: None,
                    backend: "unavailable",
                }
            }
        }
    }

    /// The transitional tenant prefix.
    ///
    /// Right now the whole deployment has only user #1, and their schema name is
    /// literally `public` (see the header of
    /// `migrations-global/20260810000002`: existing data does not move). So the
    /// object key prefix is `public` too — **the two must be the same string**,
    /// or "this user's objects" and "this user's rows" point at two different
    /// namespaces.
    ///
    /// Once multi-tenancy is wired through, this becomes the tenant extracted
    /// from the request, not another env var.
    fn current_tenant() -> cormex_blob::TenantPrefix {
        cormex_blob::TenantPrefix::new("public").expect("public is a valid tenant prefix")
    }

    async fn try_s3(cfg: &cormex_core::config::S3Config) -> cormex_blob::Result<S3BlobStore> {
        let store = S3BlobStore::new(&S3Options::from_config(cfg, Self::current_tenant()))?;
        // ensure_bucket is the first real network call and doubles as a
        // connectivity probe — construction itself touches no network, so a
        // successful construction proves nothing about the peer being alive
        store.ensure_bucket().await?;
        Ok(store)
    }

    /// A media store connected to **nothing at all**, purely for the crate's
    /// own HTTP tests.
    ///
    /// It exists because [`Self::connect`] genuinely dials S3
    /// (`ensure_bucket` is a network round trip), and on failure it **creates a
    /// local fallback directory** under the process's working directory. A test
    /// that only wants to assert "no credential means 401" should not have to
    /// wait out a network timeout for that, let alone leave files behind in the
    /// repository.
    ///
    /// `inner: None` rather than wiring up a tempdir: in this state the media
    /// endpoints report an explicit error, which conveniently turns "a test
    /// accidentally reached real blob logic" into a loud failure rather than a
    /// quietly successful illusion.
    #[cfg(any(test, feature = "test-support"))]
    pub const fn unavailable_for_tests() -> Self {
        Self {
            inner: None,
            backend: "unavailable",
        }
    }

    /// A store backed by whatever the caller hands over, for tests that need
    /// real bytes without a network round trip.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_backend(store: std::sync::Arc<dyn cormex_blob::BlobStore>) -> Self {
        Self {
            inner: Some(store),
            backend: "test",
        }
    }

    /// The backend identifier reported to `/health`. Seeing `local_fs` in a
    /// production log is itself an alert.
    #[must_use]
    pub const fn backend(&self) -> &'static str {
        self.backend
    }

    /// Whether this backend can sign presigned URLs.
    ///
    /// Only the S3-compatible one can. The check happens **before the request is
    /// issued**, so the client gets a 501 "this deployment does not support
    /// direct upload" rather than a 500 — the former says "stop trying, switch
    /// to relaying", the latter says "the server broke, retry in a bit". Mobile
    /// retry logic takes that distinction seriously, and conflating the two
    /// means the client retries forever down a path that can never work.
    #[must_use]
    pub fn supports_presign(&self) -> bool {
        self.backend == "s3"
    }

    fn store(&self) -> Result<&Arc<dyn BlobStore>> {
        self.inner.as_ref().ok_or_else(|| {
            CormexError::Store(
                "object storage unavailable: S3 is unreachable and the local \
             fallback directory could not be created"
                    .into(),
            )
        })
    }

    /// Server-relayed upload. Returns the content hash and the sniffed MIME.
    pub async fn put(
        &self,
        bytes: axum::body::Bytes,
        declared_mime: Option<&str>,
    ) -> Result<cormex_blob::BlobRef> {
        Ok(self.store()?.put(bytes, declared_mime).await?)
    }

    pub async fn get(&self, hash: &str) -> Result<axum::body::Bytes> {
        Ok(self.store()?.get(hash).await?)
    }

    pub async fn get_range(&self, hash: &str, range: Range<u64>) -> Result<axum::body::Bytes> {
        Ok(self.store()?.get_range(hash, range).await?)
    }

    pub async fn exists(&self, hash: &str) -> Result<bool> {
        Ok(self.store()?.exists(hash).await?)
    }

    // ── Keyed access for captured session chunks (D16) ─────────
    //
    // Pure passthrough: the key is complete (`hash::capture_key` built it from
    // the **request's** tenant), and the backend must not prepend anything.
    // `MediaStore::current_tenant()` is hard-coded to `public` because
    // multi-tenancy has not reached object storage yet; deriving the capture
    // key from it would put every tenant's captured conversations under user
    // #1's namespace, with no error.
    //
    // `get_keyed` / `exists_keyed` / `delete_keyed` have no caller yet: reading
    // the raw bytes back is P2-5 (`/archive/raw`), and deletion is P6-2 (purge).
    // They are kept here rather than deleted because the trait methods they
    // forward to exist and are tested, and re-adding a two-line passthrough is
    // pure friction. `put_keyed` is used by `put_capture_object` and needs no
    // exemption.

    pub async fn put_keyed(&self, key: &str, bytes: axum::body::Bytes, mime: &str) -> Result<()> {
        Ok(self.store()?.put_keyed(key, bytes, mime).await?)
    }

    #[allow(dead_code)]
    pub async fn get_keyed(&self, key: &str) -> Result<axum::body::Bytes> {
        Ok(self.store()?.get_keyed(key).await?)
    }

    #[allow(dead_code)]
    pub async fn exists_keyed(&self, key: &str) -> Result<bool> {
        Ok(self.store()?.exists_keyed(key).await?)
    }

    #[allow(dead_code)]
    pub async fn delete_keyed(&self, key: &str) -> Result<()> {
        Ok(self.store()?.delete_keyed(key).await?)
    }

    pub async fn presign_put(&self, hash: &str) -> Result<String> {
        Ok(self.store()?.presign_put(hash, PRESIGN_TTL).await?.into())
    }

    pub async fn presign_get(&self, hash: &str) -> Result<String> {
        Ok(self.store()?.presign_get(hash, PRESIGN_TTL).await?.into())
    }

    /// MIME determination on the direct-upload path: sniff by pulling only the
    /// first few KB of the object.
    ///
    /// **The client-declared MIME is not trusted**, in line with the rule in
    /// [`BlobStore::put`] — browsers guess by file extension and mobile clients
    /// commonly send `application/octet-stream`, and once octet-stream is
    /// written into `blobs.mime` this content is a black hole for the future
    /// transcription pipeline (it cannot tell whether to route it to ASR or
    /// OCR), by which point it has already sunk to the bottom of the database.
    pub async fn sniff_mime(&self, hash: &str, declared: Option<&str>) -> Result<String> {
        let head = match self.get_range(hash, 0..SNIFF_BYTES).await {
            Ok(bytes) => bytes,
            // Some backends answer 416 when the object is shorter than
            // SNIFF_BYTES; fall back to a full fetch (such objects are small
            // anyway)
            Err(CormexError::Invalid(_)) => self.get(hash).await?,
            Err(e) => return Err(e),
        };
        Ok(cormex_blob::probe_media(&head).resolve_mime(declared))
    }
}

// ─────────────────────── HTTP Range arithmetic ─────────────────

/// A parsed byte interval, half-open (matching Rust convention).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeSpec {
    /// The whole object
    Full,
    /// `[start, end)`, already clamped to the object's actual size
    Partial(Range<u64>),
    /// Syntactically valid but entirely outside the object — must answer 416,
    /// never 200
    Unsatisfiable,
}

/// Parse the `Range` header.
///
/// # Why this is hand-written instead of pulling in a crate
///
/// All that is needed is the single-interval `bytes=` form — which is exactly
/// what a player sends when the user drags the scrub bar. Multiple intervals
/// (`bytes=0-99,200-299`) require a multipart/byteranges response body, no
/// player expects the server to support it, and implementing it would be dead
/// code.
///
/// All three forms have to be recognised; missing any one of them makes some
/// player fall through the floor:
///
/// | Form | Who sends it |
/// |---|---|
/// | `bytes=500-999` | asking for a specific span |
/// | `bytes=500-` | play from a point to the end — **the most common shape when scrubbing** |
/// | `bytes=-500` | asking for the last N bytes, which is how MP4 files with moov at the tail are handled |
///
/// A `header` of `None` (no Range sent) returns [`RangeSpec::Full`]; invalid
/// syntax also returns `Full` — RFC 9110 explicitly requires that a Range
/// header which cannot be understood be treated as absent, so answering 416
/// would itself be non-compliant.
#[must_use]
pub fn parse_range(header: Option<&str>, size: u64) -> RangeSpec {
    let Some(raw) = header else {
        return RangeSpec::Full;
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    // Multiple intervals: take only the first span rather than rejecting the
    // whole request — handing back fewer bytes just makes the client ask again,
    // whereas an error stops playback dead
    let spec = spec.split(',').next().unwrap_or("").trim();

    let Some((start, end)) = spec.split_once('-') else {
        return RangeSpec::Full;
    };
    let (start, end) = (start.trim(), end.trim());

    // An empty object has no satisfiable interval at all
    if size == 0 {
        return RangeSpec::Unsatisfiable;
    }

    match (start.is_empty(), end.is_empty()) {
        // `bytes=-N`: the last N bytes. When N exceeds the object, hand back the
        // whole object (as RFC 9110 requires)
        (true, false) => match end.parse::<u64>() {
            Ok(0) => RangeSpec::Unsatisfiable,
            Ok(n) => RangeSpec::Partial(size.saturating_sub(n)..size),
            Err(_) => RangeSpec::Full,
        },
        // `bytes=N-`: play from N to the end
        (false, true) => match start.parse::<u64>() {
            Ok(s) if s < size => RangeSpec::Partial(s..size),
            Ok(_) => RangeSpec::Unsatisfiable,
            Err(_) => RangeSpec::Full,
        },
        // `bytes=A-B`: HTTP's B is the **closed** end while Rust's Range is
        // exclusive, so the +1 happens here. This is the line most likely to
        // carry an off-by-one, and the symptom is "every video segment is
        // missing its last frame" — which nobody attributes to the storage
        // layer
        (false, false) => match (start.parse::<u64>(), end.parse::<u64>()) {
            (Ok(s), Ok(e)) if s <= e && s < size => RangeSpec::Partial(s..(e + 1).min(size)),
            (Ok(s), Ok(e)) if s <= e => RangeSpec::Unsatisfiable,
            _ => RangeSpec::Full,
        },
        (true, true) => RangeSpec::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The keyed path round-trips against the local backend, and **the key is
    /// used verbatim**.
    ///
    /// The second half is the point. `MediaStore` is built with the `public`
    /// tenant prefix (that is what `current_tenant()` returns until
    /// multi-tenancy reaches object storage), so a backend that prepended its
    /// own prefix would write this chunk under `public/` — and a tenant `u_…`
    /// reading its own capture data would find nothing, with the upload having
    /// reported success.
    #[tokio::test]
    async fn keyed_access_uses_the_key_verbatim_and_round_trips() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let local = cormex_blob::LocalFsBlobStore::new(
            dir.path(),
            cormex_blob::TenantPrefix::new("public").expect("a legal tenant prefix"),
        )
        .await
        .expect("the local backend should come up");
        let store = MediaStore::with_backend(std::sync::Arc::new(local));

        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        let tenant = cormex_blob::TenantPrefix::new("u_01abc").expect("a legal tenant prefix");
        let key = cormex_blob::hash::capture_key(&tenant, hash).expect("a legal capture key");

        let body = axum::body::Bytes::from_static(b"a captured chunk");
        store
            .put_keyed(&key, body.clone(), "application/zstd")
            .await
            .expect("the write should succeed");

        assert!(
            store.exists_keyed(&key).await.expect("exists should work"),
            "the object was written at {key} and is not there"
        );
        assert_eq!(
            store
                .get_keyed(&key)
                .await
                .expect("the read should succeed"),
            body,
            "the bytes that came back are not the bytes that went in"
        );

        // Verbatim: the object is at `<root>/u_01abc/capture/...`, not under
        // the store's own `public/` prefix
        assert!(
            dir.path().join(&key).exists(),
            "the object is not at the key it was given; the backend prepended its own tenant              prefix, which would put every tenant's capture data under user #1's namespace"
        );

        store
            .delete_keyed(&key)
            .await
            .expect("the delete should work");
        assert!(
            !store.exists_keyed(&key).await.expect("exists should work"),
            "the object is still there after delete"
        );
        // Idempotent, like `delete`
        store
            .delete_keyed(&key)
            .await
            .expect("deleting a missing object must succeed");
    }

    #[tokio::test]
    async fn keyed_access_refuses_a_traversal_key() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let local = cormex_blob::LocalFsBlobStore::new(
            dir.path(),
            cormex_blob::TenantPrefix::new("public").expect("a legal tenant prefix"),
        )
        .await
        .expect("the local backend should come up");
        let store = MediaStore::with_backend(std::sync::Arc::new(local));

        for bad in [
            "../../etc/passwd",
            "public/capture/00/00/e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855.zst",
        ] {
            assert!(
                store.get_keyed(bad).await.is_err(),
                "a malformed key was accepted: {bad}"
            );
        }
    }

    #[test]
    fn absent_or_unparsable_range_means_the_whole_object() {
        assert_eq!(parse_range(None, 100), RangeSpec::Full);
        // RFC 9110: a Range header that cannot be understood must be treated as
        // absent, not answered with 416
        for bad in ["items=0-1", "bytes=abc-def", "bytes", "bytes=", "garbage"] {
            assert_eq!(
                parse_range(Some(bad), 100),
                RangeSpec::Full,
                "the unintelligible Range header {bad:?} must be ignored; answering 416 is \
                 non-compliant"
            );
        }
    }

    #[test]
    fn closed_end_becomes_exclusive() {
        // HTTP's 0-99 is 100 bytes; transcribing it literally into Rust's 0..99
        // loses one
        assert_eq!(
            parse_range(Some("bytes=0-99"), 1000),
            RangeSpec::Partial(0..100),
            "the right end of an HTTP Range is closed, so the conversion must add 1"
        );
        assert_eq!(
            parse_range(Some("bytes=5-5"), 1000),
            RangeSpec::Partial(5..6),
            "the single-byte interval 5-5 must yield exactly 1 byte"
        );
    }

    #[test]
    fn open_ended_range_plays_to_the_end() {
        // This is the shape sent when the user drags the scrub bar
        assert_eq!(
            parse_range(Some("bytes=500-"), 1000),
            RangeSpec::Partial(500..1000)
        );
        // An out-of-bounds right end must be clamped to the end of the object
        // rather than raising an error — players routinely quote a number
        // larger than the file
        assert_eq!(
            parse_range(Some("bytes=900-99999"), 1000),
            RangeSpec::Partial(900..1000),
            "an out-of-bounds right end must be clamped to the end of the object"
        );
    }

    #[test]
    fn suffix_range_takes_the_tail() {
        assert_eq!(
            parse_range(Some("bytes=-500"), 1000),
            RangeSpec::Partial(500..1000)
        );
        // When more is asked for than the object holds, hand back the whole
        // object (RFC 9110)
        assert_eq!(
            parse_range(Some("bytes=-9999"), 1000),
            RangeSpec::Partial(0..1000),
            "a suffix length exceeding the object size must yield the whole object"
        );
        assert_eq!(
            parse_range(Some("bytes=-0"), 1000),
            RangeSpec::Unsatisfiable,
            "asking for 0 trailing bytes cannot be satisfied"
        );
    }

    #[test]
    fn out_of_bounds_start_is_unsatisfiable() {
        // An out-of-bounds start must answer 416; answering 200 leaves the
        // player believing it received that span and decoding misaligned bytes
        assert_eq!(
            parse_range(Some("bytes=1000-"), 1000),
            RangeSpec::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some("bytes=2000-3000"), 1000),
            RangeSpec::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some("bytes=0-0"), 0),
            RangeSpec::Unsatisfiable,
            "an empty object has no satisfiable interval at all"
        );
    }

    #[test]
    fn multi_range_falls_back_to_the_first_span() {
        // multipart/byteranges is not implemented: take only the first span, and
        // the client will come back for the rest
        assert_eq!(
            parse_range(Some("bytes=0-99,200-299"), 1000),
            RangeSpec::Partial(0..100)
        );
    }

    #[test]
    fn reversed_range_is_rejected_not_silently_swapped() {
        // If a reversed interval were quietly swapped, the bytes the client
        // receives are not the bytes it thinks it received
        assert_eq!(parse_range(Some("bytes=99-0"), 1000), RangeSpec::Full);
    }
}

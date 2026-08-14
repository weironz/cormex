//! The S3-compatible backend: works with RustFS / MinIO / Cloudflare R2 / AWS
//! S3 alike.
//!
//! It uses only the standard S3 API and touches nobody's proprietary extensions
//! — RustFS is alpha (the community has documented unrecoverable metadata
//! corruption on a full disk), and the day it gets replaced should be a
//! one-line endpoint change.

use std::{ops::Range, time::Duration};

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Region},
    error::{ProvideErrorMetadata, SdkError},
    operation::{get_object::GetObjectError, head_object::HeadObjectError},
    presigning::PresigningConfig,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use bytes::Bytes;
use url::Url;

use crate::{
    BlobError, BlobRef, BlobStore, Result, hash, hash::TenantPrefix, media, validate_range,
    validate_ttl,
};

/// The size threshold above which multipart is used.
///
/// Why 16 MiB: multipart takes at least three round trips (create / upload /
/// complete), whereas a single `PutObject` takes one. For small objects
/// multipart is a pure loss. Above the threshold it becomes worth it, and what
/// it buys is **per-part retry** — when a 500 MB video hits network jitter at
/// 480 MB, a single PUT starts over from the beginning while multipart
/// retransmits only 8 MiB.
pub const MULTIPART_THRESHOLD: usize = 16 * 1024 * 1024;

/// The part size. 8 MiB matches the AWS CLI's `multipart_chunksize` default —
/// not a number pulled out of the air, but one validated by a great deal of
/// production traffic.
///
/// The lower bound is imposed by S3: every part except the last must be at
/// least 5 MiB. 8 MiB leaves margin, and at the same time turns the 10000-part
/// limit into roughly 78 GiB per object, far beyond any media that would ever
/// enter memory.
pub const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

// S3's hard constraints are caught at compile time — if these two constants get
// adjusted in a way that breaks the rules, that should not wait until some
// video is halfway uploaded to be refused by the server.
const _: () = {
    assert!(
        MULTIPART_PART_SIZE >= 5 * 1024 * 1024,
        "S3 requires every part but the last to be at least 5 MiB; below that, \
         CompleteMultipartUpload is refused"
    );
    assert!(
        MULTIPART_THRESHOLD >= MULTIPART_PART_SIZE,
        "a threshold below the part size produces a one-part multipart — three round trips to do \
         the job of one PUT"
    );
    assert!(
        MULTIPART_PART_SIZE * 10_000 > 64 * 1024 * 1024 * 1024,
        "the part count limit is 10000; this part size works out to less than 64 GiB per object, \
         so long videos would fail to upload"
    );
};

/// The parameters needed to build an [`S3BlobStore`].
#[derive(Debug, Clone)]
pub struct S3Options {
    /// Of the form `http://localhost:9010`. Leave it empty to use the official
    /// AWS endpoint.
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// Path-style addressing (`host/bucket/key`) rather than virtual-host
    /// (`bucket.host/key`).
    ///
    /// **Must be `true` for RustFS / MinIO**; see [`S3BlobStore::new`] for why.
    pub force_path_style: bool,
    /// The tenant this instance serves, which becomes the leading segment of
    /// the object key.
    ///
    /// It sits in `S3Options` alongside endpoint and bucket rather than being a
    /// separate parameter of [`S3BlobStore::new`]: these things all have the
    /// same lifetime (one per store instance, fixed once constructed), and
    /// splitting them across two places would give "what does building a store
    /// need" two answers.
    pub tenant: TenantPrefix,
}

impl S3Options {
    /// Construct from a [`cortex_core::config::S3Config`] and a tenant.
    /// Path-style is on by default — self-hosted object storage is this
    /// project's default deployment shape, and those implementations uniformly
    /// speak only path-style.
    ///
    /// The tenant is a separate parameter rather than being taken from
    /// `S3Config`: endpoint, bucket and keys are deployment configuration
    /// held **once per process**, whereas the tenant is a runtime identity held
    /// **once per user**. Stuffing it into the same config struct would imply it
    /// too could be read as a global value out of an environment variable —
    /// which is precisely the thing that must never happen under
    /// multi-tenancy.
    #[must_use]
    pub fn from_config(cfg: &cortex_core::config::S3Config, tenant: TenantPrefix) -> Self {
        Self {
            endpoint: cfg.endpoint.clone(),
            bucket: cfg.bucket.clone(),
            region: cfg.region.clone(),
            access_key: cfg.access_key.clone(),
            secret_key: cfg.secret_key.clone(),
            force_path_style: true,
            tenant,
        }
    }
}

/// The S3-compatible object storage backend.
#[derive(Clone)]
pub struct S3BlobStore {
    client: Client,
    bucket: String,
    tenant: TenantPrefix,
}

/// Hand-written rather than derived: `Client`'s `Debug` unrolls the entire SDK
/// configuration (the endpoint regexes of every partition, the interceptor
/// chain, the retry policy) into tens of kilobytes. Once that lands in
/// `tracing`'s error context or in some assertion message, the logs become
/// unreadable.
impl std::fmt::Debug for S3BlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3BlobStore")
            .field("bucket", &self.bucket)
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl S3BlobStore {
    /// Build the client. Makes no network call at all — the bucket's existence
    /// is the separate responsibility of [`ensure_bucket`](Self::ensure_bucket),
    /// so that "construction" stays synchronous, side-effect free, and doable
    /// during configuration parsing.
    ///
    /// # Why `force_path_style` is non-negotiable
    ///
    /// Turn it off and the SDK folds the bucket name into a subdomain,
    /// requesting `cortex-blobs.localhost:9010` instead of
    /// `localhost:9010/cortex-blobs`. RustFS **only implements path-style**, so
    /// this fails without exception — but the shape of the failure depends on
    /// how the local DNS handles that subdomain, and both have been observed in
    /// practice:
    ///
    /// - The subdomain does not resolve: `dispatch failure: io error: failed to
    ///   lookup address information`.
    /// - The subdomain resolves to 127.0.0.1 (which `*.localhost` does on quite
    ///   a few systems, this machine included): the connection is established,
    ///   RustFS does not recognize the Host and simply disconnects, reporting
    ///   `connection closed before message completed`.
    ///
    /// **Neither error message contains the bucket name, nor the words
    /// "addressing style".** They look like plain network trouble, which is
    /// enough to send somebody chasing firewalls, container networking and
    /// endpoint typos for a whole day. So there is no opportunity to turn it
    /// off: `from_config` sets it to `true` unconditionally. (The official AWS
    /// endpoint supports both addressing styles, so leaving it on costs
    /// nothing.)
    ///
    /// The effect itself is pinned down by
    /// `path_style_flag_decides_where_the_bucket_name_goes`.
    ///
    /// # Errors
    /// Returns [`BlobError::Backend`] when the endpoint is not a legal URL.
    pub fn new(opts: &S3Options) -> Result<Self> {
        let creds = Credentials::new(
            &opts.access_key,
            &opts.secret_key,
            None,
            None,
            "cortex-static",
        );

        let mut builder = aws_sdk_s3::config::Builder::new()
            // The behavior version is not pinned: pinning it would mean
            // manually reviewing a change list on every SDK upgrade, whereas we
            // use only the six most basic APIs, whose surface for behavioural
            // change is close to zero.
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(opts.region.clone()))
            .credentials_provider(creds)
            .force_path_style(opts.force_path_style);

        if !opts.endpoint.is_empty() {
            // Validate up front: if a mistyped endpoint waited until the first
            // request to blow up, the error would blend into network failures
            // and be hard to tell apart.
            //
            // A successful `Url::parse` is not enough on its own — it parses
            // `localhost:9010` as "scheme = localhost, path = 9010" and happily
            // returns Ok, so the single most common mistake (forgetting
            // http://) is precisely the one it misses. Hence the explicit
            // demand for http/https plus a host.
            let url = Url::parse(&opts.endpoint).map_err(|e| {
                BlobError::Backend(format!(
                    "S3 endpoint {:?} is not a valid URL: {e}",
                    opts.endpoint
                ))
            })?;
            if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
                return Err(BlobError::Backend(format!(
                    "S3 endpoint {:?} is missing the http:// or https:// prefix",
                    opts.endpoint
                )));
            }
            builder = builder.endpoint_url(&opts.endpoint);
        }

        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket: opts.bucket.clone(),
            tenant: opts.tenant.clone(),
        })
    }

    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// The tenant this instance serves. Objects all land beneath the `<tenant>/`
    /// prefix.
    #[must_use]
    pub fn tenant(&self) -> &TenantPrefix {
        &self.tenant
    }

    fn key_of(&self, hash: &str) -> Result<String> {
        hash::storage_key(&self.tenant, hash)
    }

    /// Create the bucket if it does not exist. Idempotent, and safe to call
    /// unconditionally on every startup.
    ///
    /// It does not `HeadBucket` first and create second: that is a TOCTOU, and
    /// two instances starting at once collide. Create directly, then treat those
    /// two "already exists" error codes as success — that is the correct way to
    /// write idempotence.
    ///
    /// # Errors
    /// Returns [`BlobError::Backend`] when creation fails for a reason other
    /// than "already exists".
    pub async fn ensure_bucket(&self) -> Result<()> {
        match self
            .client
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(_) => {
                tracing::info!(bucket = %self.bucket, "created the object storage bucket");
                Ok(())
            }
            Err(e) => {
                let code = e.code().unwrap_or_default();
                if matches!(code, "BucketAlreadyOwnedByYou" | "BucketAlreadyExists") {
                    Ok(())
                } else {
                    Err(backend_err("CreateBucket", e))
                }
            }
        }
    }

    fn presign_cfg(ttl: Duration) -> Result<PresigningConfig> {
        validate_ttl(ttl)?;
        PresigningConfig::expires_in(ttl).map_err(|e| {
            BlobError::Backend(format!("failed to build the presigning configuration: {e}"))
        })
    }

    /// Large objects go through multipart. On failure it **must** abort,
    /// otherwise the parts already uploaded permanently occupy space in the
    /// bucket while being invisible to `ListObjects` — the hardest kind of leak
    /// to notice.
    async fn put_multipart(&self, key: &str, bytes: &Bytes, mime: &str) -> Result<()> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .content_type(mime)
            .send()
            .await
            .map_err(|e| backend_err("CreateMultipartUpload", e))?;

        let upload_id = created.upload_id().ok_or_else(|| {
            BlobError::Backend("CreateMultipartUpload returned no upload_id".into())
        })?;

        match self.upload_parts(key, upload_id, bytes).await {
            Ok(parts) => {
                let completed = CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build();
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                    .map_err(|e| backend_err("CompleteMultipartUpload", e))?;
                Ok(())
            }
            Err(e) => {
                // The abort itself can also fail (the network may already be
                // down), but reporting that instead would bury the real cause.
                // Log it rather than replacing the error.
                if let Err(abort_err) = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .send()
                    .await
                {
                    tracing::warn!(
                        key, %abort_err,
                        "the abort after a failed multipart also failed; stray parts may remain \
                         in the bucket and have to be reclaimed by a lifecycle rule"
                    );
                }
                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        bytes: &Bytes,
    ) -> Result<Vec<CompletedPart>> {
        let mut parts = Vec::with_capacity(bytes.len().div_ceil(MULTIPART_PART_SIZE));

        // Sequential rather than concurrent uploads: the bytes are already
        // entirely in memory, the bottleneck is upstream bandwidth, and
        // concurrent parts do not widen it — they only put several more slice
        // references in memory at once. Genuinely speeding this up would mean
        // first turning put into a streaming interface, which is a different
        // matter.
        for (i, chunk) in bytes.chunks(MULTIPART_PART_SIZE).enumerate() {
            // S3 part numbers start at 1, not 0.
            let part_number = i32::try_from(i + 1)
                .map_err(|_| BlobError::Backend("the part count overflows i32".into()))?;

            let out = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(Bytes::copy_from_slice(chunk)))
                .send()
                .await
                .map_err(|e| backend_err("UploadPart", e))?;

            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(out.e_tag().map(str::to_owned))
                    .build(),
            );
        }
        Ok(parts)
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, bytes: Bytes, declared_mime: Option<&str>) -> Result<BlobRef> {
        let digest = hash::sha256_hex(&bytes);
        let storage_key = self.key_of(&digest)?;

        let meta = media::probe(&bytes);
        let mime = meta.resolve_mime(declared_mime);
        let blob = BlobRef {
            mime: mime.clone(),
            size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            hash: digest,
            storage_key: storage_key.clone(),
            deduplicated: true,
        };

        // Probe first, upload second: a hit saves the entire upstream
        // bandwidth.
        //
        // There is a well-known race here — two requests both probe "not
        // present" and so both upload. **Deliberately ignored**: under content
        // addressing their keys and their bytes are identical, and S3's
        // PutObject is atomic, so whichever arrives second the result is the
        // same. Introducing a distributed lock to save one duplicate upload
        // trades availability for bandwidth, which is a losing trade.
        //
        // The only practical effect is that the observational field
        // `deduplicated` occasionally reads low, and it is already documented as
        // being for observation only.
        if self.exists(&blob.hash).await? {
            return Ok(blob);
        }

        if bytes.len() >= MULTIPART_THRESHOLD {
            self.put_multipart(&storage_key, &bytes, &mime).await?;
        } else {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&storage_key)
                .content_type(&mime)
                .body(ByteStream::from(bytes))
                .send()
                .await
                .map_err(|e| backend_err("PutObject", e))?;
        }

        Ok(BlobRef {
            deduplicated: false,
            ..blob
        })
    }

    async fn get(&self, hash: &str) -> Result<Bytes> {
        let key = self.key_of(hash)?;
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| map_get_err(hash, e))?;

        collect(out.body).await
    }

    async fn get_range(&self, hash: &str, range: Range<u64>) -> Result<Bytes> {
        validate_range(&range)?;
        let key = self.key_of(hash)?;

        // Both ends of an HTTP Range are inclusive, while Rust's Range is
        // half-open — this is where an off-by-one is easiest to make, and the
        // symptom is "every video segment is missing its last frame", which
        // nobody would ever attribute to the storage layer.
        let header = format!("bytes={}-{}", range.start, range.end - 1);

        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .range(header)
            .send()
            .await
            .map_err(|e| map_get_err(hash, e))?;

        collect(out.body).await
    }

    async fn presign_put(&self, hash: &str, ttl: Duration) -> Result<Url> {
        let key = self.key_of(hash)?;
        let cfg = Self::presign_cfg(ttl)?;

        // Deliberately no content_type / content_length: presigning signs the
        // request headers into SignedHeaders as well, and if a single byte of
        // what the client actually sends fails to match, the signature does not
        // verify (reported as SignatureDoesNotMatch, while the real cause is
        // "you set one header too many"). The MIME is decided by the server when
        // it writes the `blobs` row, so the client need not be involved at all.
        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .presigned(cfg)
            .await
            .map_err(|e| backend_err("presign PutObject", e))?;

        parse_presigned(req.uri())
    }

    async fn presign_get(&self, hash: &str, ttl: Duration) -> Result<Url> {
        let key = self.key_of(hash)?;
        let cfg = Self::presign_cfg(ttl)?;

        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .presigned(cfg)
            .await
            .map_err(|e| backend_err("presign GetObject", e))?;

        parse_presigned(req.uri())
    }

    async fn exists(&self, hash: &str) -> Result<bool> {
        let key = self.key_of(hash)?;
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(SdkError::ServiceError(e)) if matches!(e.err(), HeadObjectError::NotFound(_)) => {
                Ok(false)
            }
            // While the bucket has yet to be created, HeadObject reports
            // NoSuchBucket, which is semantically the same as "the object does
            // not exist". Without this special case, the first put after
            // startup would fail right at the probing step.
            Err(e) if is_no_such_bucket(&e) => Ok(false),
            Err(e) => Err(backend_err("HeadObject", e)),
        }
    }

    async fn delete(&self, hash: &str) -> Result<()> {
        let key = self.key_of(hash)?;
        // S3's DeleteObject returns 204 even for a key that does not exist, so
        // it is idempotent by nature — exactly what purge's cascading cleanup
        // job needs in order to be rerunnable.
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| backend_err("DeleteObject", e))?;
        Ok(())
    }
}

/// Flatten an SDK error into one readable line.
///
/// A plain `{e}` yields nothing but an utterly uninformative top-level
/// description like "service error" — the real cause is buried two or three
/// levels down the source chain. This explicitly takes `code`/`message`, and
/// when the connection could not be made at all (so there is no server response
/// whatsoever) drills all the way down to the deepest level. What that saves is
/// manually unrolling the error chain on every investigation.
fn backend_err<E>(op: &str, e: SdkError<E>) -> BlobError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let code = e.code().unwrap_or("-");
    let msg = e.message().map(str::to_owned).unwrap_or_else(|| {
        let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
        let mut deepest = e.to_string();
        while let Some(s) = src {
            deepest = s.to_string();
            src = s.source();
        }
        deepest
    });
    BlobError::Backend(format!("{op} failed [{code}]: {msg}"))
}

fn map_get_err(hash: &str, e: SdkError<GetObjectError>) -> BlobError {
    match http_status(&e) {
        // 416: the range falls outside the object. Classified as InvalidRange
        // rather than Backend, so that the HTTP layer maps it to 400 and not
        // 500.
        Some(416) => {
            BlobError::InvalidRange(format!("the requested range falls entirely outside {hash}"))
        }
        // Go by status code rather than only recognizing
        // `GetObjectError::NoSuchKey`: compatible implementations such as
        // RustFS answer a missing object with 404 but do not necessarily attach
        // that struct, and recognizing only the typed error would misreport a
        // 404 as a 500.
        Some(404) => BlobError::NotFound(hash.to_owned()),
        _ if matches!(&e, SdkError::ServiceError(se) if matches!(se.err(), GetObjectError::NoSuchKey(_))) => {
            BlobError::NotFound(hash.to_owned())
        }
        _ => backend_err("GetObject", e),
    }
}

/// Take the HTTP status code the server returned. When the connection could not
/// be made there is no response to take one from, so this returns `None`.
fn http_status<E>(e: &SdkError<E>) -> Option<u16> {
    match e {
        SdkError::ServiceError(se) => Some(se.raw().status().as_u16()),
        SdkError::ResponseError(re) => Some(re.raw().status().as_u16()),
        _ => None,
    }
}

fn is_no_such_bucket<E: ProvideErrorMetadata>(e: &SdkError<E>) -> bool {
    e.code() == Some("NoSuchBucket")
}

fn parse_presigned(uri: &str) -> Result<Url> {
    Url::parse(uri)
        .map_err(|e| BlobError::Backend(format!("presigned URL {uri:?} cannot be parsed: {e}")))
}

async fn collect(body: ByteStream) -> Result<Bytes> {
    body.collect()
        .await
        .map(aws_smithy_types::byte_stream::AggregatedBytes::into_bytes)
        .map_err(|e| BlobError::Backend(format!("failed to read the object body: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TENANT: &str = "public";

    fn opts() -> S3Options {
        S3Options {
            endpoint: "http://localhost:9010".into(),
            bucket: "cortex-blobs".into(),
            region: "us-east-1".into(),
            access_key: "k".into(),
            secret_key: "s".into(),
            force_path_style: true,
            tenant: TenantPrefix::new(TEST_TENANT)
                .expect("the test tenant prefix should itself be legal"),
        }
    }

    #[test]
    fn construction_makes_no_network_call() {
        // Point at an address that is guaranteed unreachable: if construction
        // hid a network call, this would hang or error.
        let mut o = opts();
        o.endpoint = "http://127.0.0.1:1".into();
        assert!(
            S3BlobStore::new(&o).is_ok(),
            "construction must not touch the network — it has to be doable during configuration \
             parsing"
        );
    }

    #[test]
    fn malformed_endpoint_fails_fast() {
        // "localhost:9010" is the single most common mistake (forgetting
        // http://), and it happens to parse successfully with Url::parse
        // (scheme = localhost). Relying on parse alone would let it through.
        for bad in ["localhost:9010", "9010", "ftp://localhost:9010", "http://"] {
            let mut o = opts();
            o.endpoint = bad.into();
            assert!(
                matches!(S3BlobStore::new(&o), Err(BlobError::Backend(_))),
                "the endpoint {bad:?} should be refused at construction — left to blow up on the \
                 first request it blends into network failures and is hard to tell apart"
            );
        }
    }

    #[test]
    fn empty_endpoint_means_aws() {
        let mut o = opts();
        o.endpoint = String::new();
        assert!(
            S3BlobStore::new(&o).is_ok(),
            "an empty endpoint means the official AWS address and must not be treated as illegal \
             input"
        );
    }

    #[tokio::test]
    async fn traversal_hash_never_reaches_the_network() {
        // The endpoint points at a port that is guaranteed unreachable: if a
        // request really went out, the result would be a Backend error.
        // Asserting InvalidHash proves the validation happens before any network
        // action.
        let mut o = opts();
        o.endpoint = "http://127.0.0.1:1".into();
        let s = S3BlobStore::new(&o).unwrap();
        let ttl = Duration::from_secs(60);

        for evil in ["../../../etc/passwd", "..", "a/../b", "NOTAHASH"] {
            assert!(
                matches!(s.get(evil).await, Err(BlobError::InvalidHash(_))),
                "get's hash validation did not come before the network call: {evil:?}"
            );
            assert!(
                matches!(s.exists(evil).await, Err(BlobError::InvalidHash(_))),
                "exists's hash validation did not come before the network call: {evil:?}"
            );
            assert!(
                matches!(s.delete(evil).await, Err(BlobError::InvalidHash(_))),
                "delete's hash validation did not come before the network call: {evil:?}"
            );
            assert!(
                matches!(
                    s.presign_get(evil, ttl).await,
                    Err(BlobError::InvalidHash(_))
                ),
                "presign_get signs the key straight into a URL, so validation has to come first: \
                 {evil:?}"
            );
            assert!(
                matches!(
                    s.presign_put(evil, ttl).await,
                    Err(BlobError::InvalidHash(_))
                ),
                "presign_put signs the key straight into a URL, so validation has to come first: \
                 {evil:?}"
            );
        }
    }

    /// What `force_path_style` actually does — signing makes no network call, so
    /// it can be inspected directly in a unit test.
    ///
    /// The value of this test is turning "what happens if you switch it off"
    /// into one readable assertion. Otherwise that consequence (DNS resolution
    /// of `cortex-blobs.localhost`) only reveals itself when things are actually
    /// running and somebody manages to read an io error that does not mention
    /// the bucket name.
    #[tokio::test]
    async fn path_style_flag_decides_where_the_bucket_name_goes() {
        let h = hash::sha256_hex(b"probe");
        let ttl = Duration::from_secs(60);

        let on = S3BlobStore::new(&opts()).unwrap();
        let url = on.presign_get(&h, ttl).await.unwrap();
        assert_eq!(
            url.host_str(),
            Some("localhost"),
            "with path-style on, the host should stay exactly as the endpoint gave it"
        );
        assert!(
            url.path().starts_with("/cortex-blobs/public/blobs/"),
            "with path-style on, the path should be \"bucket / tenant / blobs\", got {}",
            url.path()
        );

        let mut o = opts();
        o.force_path_style = false;
        let off = S3BlobStore::new(&o).unwrap();
        let url = off.presign_get(&h, ttl).await.unwrap();
        assert_eq!(
            url.host_str(),
            Some("cortex-blobs.localhost"),
            "with path-style off, the bucket name gets folded into a subdomain — which is exactly \
             the cause of the DNS resolution failure on RustFS, and the error reported contains \
             neither the bucket name nor the words \"addressing style\", making it extremely hard \
             to attribute"
        );
    }

    /// The tenant prefix has to genuinely reach the **signed URL**.
    ///
    /// This is verified on its own because presigning is the only path that can
    /// send a key out without going through `put`/`get`: if it ever bypassed
    /// `key_of`, clients would upload directly with a URL carrying no tenant,
    /// and those bytes would land under somebody else's prefix — with the
    /// server having nothing at all to report throughout.
    #[tokio::test]
    async fn tenant_prefix_reaches_the_signed_url() {
        let h = hash::sha256_hex(b"tenant in url");
        let ttl = Duration::from_secs(60);

        let mut other = opts();
        other.tenant = TenantPrefix::new("cortex_u2").unwrap();

        let a = S3BlobStore::new(&opts()).unwrap();
        let b = S3BlobStore::new(&other).unwrap();

        let ua = a.presign_get(&h, ttl).await.unwrap();
        let ub = b.presign_get(&h, ttl).await.unwrap();

        assert!(
            ua.path().contains("/public/blobs/"),
            "the signed URL carries no tenant prefix, so a direct client upload would land on a \
             tenantless key: {}",
            ua.path()
        );
        assert_ne!(
            ua.path(),
            ub.path(),
            "the two tenants signed the same key for the same content — direct uploads would \
             overwrite each other"
        );
    }

    #[test]
    fn part_boundaries_cover_the_last_partial_chunk() {
        // The easiest thing to get wrong in multipart is "the last part gets
        // dropped": computing the part count by plain division loses the tail,
        // and the symptom is that the video fetched back is a little shorter
        // than the original — which most players will still play, so nobody
        // notices.
        for (size, want_parts) in [
            (MULTIPART_PART_SIZE, 1),
            (MULTIPART_PART_SIZE + 1, 2),
            (MULTIPART_THRESHOLD, 2),
            (MULTIPART_PART_SIZE * 3 - 1, 3),
        ] {
            assert_eq!(
                size.div_ceil(MULTIPART_PART_SIZE),
                want_parts,
                "{size} bytes should split into {want_parts} parts; plain division loses the tail \
                 that is shorter than one part"
            );
        }
    }
}

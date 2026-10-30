//! Integration tests run against a **real RustFS**.
//!
//! All a unit test can prove is that the logic is self-consistent; whether
//! path-style addressing works, whether the peer accepts a presigned URL's
//! signature, whether multipart's parts get reassembled exactly — those are only
//! knowable by actually sending the requests. Every case in this file
//! corresponds to a problem that, untested, would first be discovered in
//! production.
//!
//! # Skip rather than fail when it cannot run
//!
//! On a development machine with no RustFS running, these tests all skip and
//! print the reason. But "silently skipping" is itself a trap: if RustFS is not
//! up in CI, the green light is a lie. Hence `CORMEX_REQUIRE_RUSTFS=1` — set it
//! and a skip becomes a failure.

use std::time::Duration;

use bytes::Bytes;
use cormex_blob::{
    BlobError, BlobStore, MULTIPART_THRESHOLD, S3BlobStore, S3Options, TenantPrefix,
    hash::{self, sha256_hex},
};

/// A real 1×1 PNG. A genuine image rather than a few arbitrary bytes, so that
/// MIME and dimension probing are actually exercised.
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// Obtain a usable store, or explain why there is none.
///
/// Returning `None` means "this machine has no RustFS", and callers skip
/// accordingly.
async fn store() -> Option<S3BlobStore> {
    let _ = dotenvy::dotenv();

    let opts = S3Options {
        endpoint: std::env::var("S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".into()),
        bucket: std::env::var("S3_BUCKET").unwrap_or_else(|_| "cormex-blobs".into()),
        region: std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        access_key: std::env::var("RUSTFS_ACCESS_KEY").unwrap_or_else(|_| "cormexadmin".into()),
        secret_key: std::env::var("RUSTFS_SECRET_KEY").unwrap_or_else(|_| "cormex_dev_only".into()),
        force_path_style: true,
        // A fixed tenant rather than a random one per run: random prefixes would
        // leave a pile of empty prefixes in the bucket that nobody ever cleans
        // up, and these tests already isolate instances from each other via
        // unique_payload, so a second layer of isolation by tenant is
        // unnecessary.
        tenant: TenantPrefix::new("cormex_test")
            .expect("the test tenant prefix should itself be legal"),
    };

    let s = S3BlobStore::new(&opts)
        .expect("the S3Options themselves are illegal, which has nothing to do with whether RustFS is running");

    // ensure_bucket is the first real network call, and doubles as a
    // connectivity probe.
    match s.ensure_bucket().await {
        Ok(()) => Some(s),
        Err(e) => {
            assert!(
                std::env::var("CORMEX_REQUIRE_RUSTFS").is_err(),
                "CORMEX_REQUIRE_RUSTFS is set but RustFS is unreachable ({}): {e}",
                opts.endpoint
            );
            eprintln!("skipped: cannot reach RustFS ({}): {e}", opts.endpoint);
            None
        }
    }
}

/// Give every test its own content, so that parallel runs do not interfere with
/// each other (under content addressing, identical content is the identical
/// object).
fn unique_payload(tag: &str, len: usize) -> Bytes {
    let seed = format!(
        "{tag}-{:?}-{}",
        std::time::SystemTime::now(),
        std::process::id()
    );
    let mut v = seed.into_bytes();
    v.resize(len.max(1), 0);
    // Fill it so it is not all zeroes: all-zero content gets sparse-optimized by
    // some storage systems, which would mask the real read/write path.
    for (i, b) in v.iter_mut().enumerate() {
        if *b == 0 {
            *b = (i % 251) as u8;
        }
    }
    Bytes::from(v)
}

macro_rules! skip_unless_rustfs {
    () => {
        match store().await {
            Some(s) => s,
            None => return,
        }
    };
}

#[tokio::test]
async fn image_round_trips_through_rustfs() {
    let s = skip_unless_rustfs!();
    let png = Bytes::from_static(PNG_1X1);

    let r = s
        .put(png.clone(), Some("application/octet-stream"))
        .await
        .expect("uploading the PNG failed");

    assert_eq!(
        r.hash,
        sha256_hex(&png),
        "the returned hash does not match the content"
    );
    assert_eq!(
        r.mime, "image/png",
        "when the leading bytes say PNG they must override the client's octet-stream claim"
    );
    assert_eq!(
        r.storage_key,
        hash::storage_key(s.tenant(), &r.hash).unwrap(),
        "storage_key has to be derived from \"tenant prefix + hash\" — it gets written into \
         blobs.storage_key and has to be common to both backends"
    );

    let got = s.get(&r.hash).await.expect("fetching back by hash failed");
    assert_eq!(
        got, png,
        "the bytes fetched back differ from the ones uploaded"
    );

    s.delete(&r.hash).await.expect("cleanup failed");
}

/// What path-style actually does: the URL has to be `host/bucket/key` and not
/// `bucket.host/key` (the latter fails DNS on RustFS).
///
/// A presigned URL is used to verify it because that is the **only place the
/// address the SDK assembles is directly visible**.
#[tokio::test]
async fn path_style_addressing_is_actually_used() {
    let s = skip_unless_rustfs!();
    let h = sha256_hex(b"path style probe");

    let url = s
        .presign_get(&h, Duration::from_secs(60))
        .await
        .expect("issuing a presigned URL failed");

    let host = url.host_str().expect("the presigned URL has no host");
    assert!(
        !host.starts_with(&format!("{}.", s.bucket())),
        "the bucket name was folded into a subdomain ({host}) — force_path_style did not take \
         effect, and on RustFS this fails at the DNS resolution stage"
    );
    assert!(
        url.path().starts_with(&format!("/{}/", s.bucket())),
        "under path-style the bucket name should be the leading path segment, but the path is {}",
        url.path()
    );
}

#[tokio::test]
async fn range_read_returns_exact_slice() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("range", 4096);
    let r = s.put(payload.clone(), None).await.expect("upload failed");

    let mid = s
        .get_range(&r.hash, 100..200)
        .await
        .expect("the range read failed");
    assert_eq!(
        &mid[..],
        &payload[100..200],
        "the slice the range returned does not match the original content — HTTP Range is \
         inclusive at both ends, and this is where an off-by-one most often shows up"
    );
    assert_eq!(
        mid.len(),
        100,
        "the half-open 100..200 should be exactly 100 bytes"
    );

    let tail = s
        .get_range(&r.hash, 4090..4096)
        .await
        .expect("reading the trailing range failed");
    assert_eq!(
        &tail[..],
        &payload[4090..4096],
        "the trailing range was read wrong"
    );

    // Entirely out of bounds should report InvalidRange (HTTP 400) and must not
    // be misreported as a 500.
    let e = s
        .get_range(&r.hash, 999_999..1_000_000)
        .await
        .expect_err("a range entirely out of bounds should error");
    assert!(
        matches!(e, BlobError::InvalidRange(_)),
        "out of bounds has to classify as InvalidRange for it to map to a 400; got: {e:?}"
    );

    s.delete(&r.hash).await.expect("cleanup failed");
}

/// A presigned GET has to be usable directly by a **completely uninformed HTTP
/// client** — which is exactly the premise of halving the bandwidth on mobile.
/// Verified with a bare reqwest call.
#[tokio::test]
async fn presigned_get_is_usable_by_a_plain_http_client() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("presign-get", 2048);
    let r = s.put(payload.clone(), None).await.expect("upload failed");

    let url = s
        .presign_get(&r.hash, Duration::from_secs(300))
        .await
        .expect("issuing the presigned GET failed");

    let resp = reqwest::get(url.as_str())
        .await
        .expect("the presigned GET request could not be sent");
    assert!(
        resp.status().is_success(),
        "the presigned GET was refused ({}) — either the signature or the addressing style is not \
         accepted by RustFS",
        resp.status()
    );

    let body = resp
        .bytes()
        .await
        .expect("reading the presigned GET response body failed");
    assert_eq!(
        body, payload,
        "the bytes fetched via the presigned URL differ from the ones uploaded"
    );

    s.delete(&r.hash).await.expect("cleanup failed");
}

/// A presigned PUT is the other half of direct client upload.
///
/// What especially needs verifying: **no request headers are preset at signing
/// time**. Sign in a content-type and a single byte of difference in what the
/// client sends yields SignatureDoesNotMatch — an error that points nowhere near
/// the real cause.
#[tokio::test]
async fn presigned_put_accepts_a_plain_upload() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("presign-put", 1024);
    let h = sha256_hex(&payload);

    assert!(
        !s.exists(&h).await.expect("the exists probe failed"),
        "the test content should not exist yet, otherwise the direct upload below verifies nothing"
    );

    let url = s
        .presign_put(&h, Duration::from_secs(300))
        .await
        .expect("issuing the presigned PUT failed");

    let resp = reqwest::Client::new()
        .put(url.as_str())
        .body(payload.clone())
        .send()
        .await
        .expect("the presigned PUT request could not be sent");
    assert!(
        resp.status().is_success(),
        "the presigned PUT was refused ({}) — if it is SignatureDoesNotMatch, request headers were \
         most likely preset at signing time",
        resp.status()
    );

    // The server has to be able to fetch back, under the same hash, the bytes
    // the client uploaded directly — only with this loop closed is "no relaying
    // through cormexd" genuinely usable.
    let got = s
        .get(&h)
        .await
        .expect("the server could not fetch back after the direct upload");
    assert_eq!(
        got, payload,
        "the directly uploaded bytes differ from the ones fetched back"
    );

    s.delete(&h).await.expect("cleanup failed");
}

#[tokio::test]
async fn duplicate_upload_is_deduplicated() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("dedup", 8192);

    let a = s
        .put(payload.clone(), None)
        .await
        .expect("the first upload failed");
    assert!(
        !a.deduplicated,
        "a first upload should not be judged a deduplication hit"
    );

    let b = s
        .put(payload.clone(), None)
        .await
        .expect("the repeated upload failed");
    assert_eq!(
        a.hash, b.hash,
        "two uploads of the same content must yield the same key"
    );
    assert_eq!(
        a.storage_key, b.storage_key,
        "the storage_key must match as well"
    );
    assert!(
        b.deduplicated,
        "the second upload should hit the existing object — deduplication failing means paying \
         the bandwidth again every time the same image is forwarded"
    );

    s.delete(&a.hash).await.expect("cleanup failed");
}

/// Uploading the same content concurrently: this does **not** require that it be
/// uploaded only once (that would need a distributed lock, which is not worth
/// it), only that the final object be complete and its hash correct — i.e. that
/// the race is benign.
#[tokio::test]
async fn concurrent_upload_of_same_content_is_benign() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("concurrent", 64 * 1024);
    let expected = sha256_hex(&payload);

    let s = std::sync::Arc::new(s);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let s = s.clone();
        let p = payload.clone();
        tasks.push(tokio::spawn(async move { s.put(p, None).await }));
    }
    for t in tasks {
        let r = t
            .await
            .expect("a task panicked")
            .expect("a concurrent upload errored");
        assert_eq!(
            r.hash, expected,
            "a concurrent upload returned a different hash"
        );
    }

    let got = s
        .get(&expected)
        .await
        .expect("could not fetch back after the concurrent uploads");
    assert_eq!(
        sha256_hex(&got),
        expected,
        "after the concurrent uploads the object's content does not match its hash — one of the \
         paths corrupted the object"
    );

    s.delete(&expected).await.expect("cleanup failed");
}

/// Objects above the threshold go through multipart. Whether RustFS reassembles
/// the parts exactly is the sort of problem that passes for every small file and
/// only blows up once a video is uploaded in production, so it has to be tested
/// for real.
#[tokio::test]
async fn multipart_upload_reassembles_exactly() {
    let s = skip_unless_rustfs!();
    // Deliberately threshold + 1 MiB: it crosses the threshold and also
    // guarantees the last part is shorter than a full one, thereby covering
    // multipart's classic mistake of "the last part gets dropped".
    let payload = unique_payload("multipart", MULTIPART_THRESHOLD + 1024 * 1024);
    let expected = sha256_hex(&payload);

    let r = s
        .put(payload.clone(), None)
        .await
        .expect("the multipart upload failed");
    assert_eq!(
        r.hash, expected,
        "the multipart upload returned the wrong hash"
    );
    assert_eq!(
        r.size_bytes,
        payload.len() as i64,
        "size_bytes does not match the actual byte count"
    );

    let got = s
        .get(&r.hash)
        .await
        .expect("fetching back the multipart object failed");
    assert_eq!(
        got.len(),
        payload.len(),
        "the length fetched back differs from the one uploaded — most likely the last part was \
         lost"
    );
    assert_eq!(
        sha256_hex(&got),
        expected,
        "the hash of the content fetched back differs from the one at upload; the parts were \
         reassembled wrong"
    );

    // A range read on a large object: seeking in a video takes exactly this
    // path.
    let slice = s
        .get_range(&r.hash, 8 * 1024 * 1024..8 * 1024 * 1024 + 256)
        .await
        .expect("the range read near a part boundary failed");
    assert_eq!(
        &slice[..],
        &payload[8 * 1024 * 1024..8 * 1024 * 1024 + 256],
        "a range read crossing a part boundary returned the wrong bytes"
    );

    s.delete(&r.hash).await.expect("cleanup failed");
}

#[tokio::test]
async fn missing_object_maps_to_not_found() {
    let s = skip_unless_rustfs!();
    let absent = sha256_hex(&unique_payload("never-uploaded", 32));

    assert!(
        !s.exists(&absent).await.expect("the exists probe failed"),
        "content that was never uploaded should not exist"
    );

    let e = s
        .get(&absent)
        .await
        .expect_err("fetching a non-existent object should error");
    assert!(
        matches!(e, BlobError::NotFound(_)),
        "absence must classify as NotFound (HTTP 404); misreporting it as Backend turns it into a \
         500 at the interface: {e:?}"
    );
}

#[tokio::test]
async fn delete_is_idempotent_on_rustfs() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("delete", 256);
    let r = s.put(payload, None).await.expect("upload failed");

    s.delete(&r.hash).await.expect("the first delete failed");
    assert!(
        !s.exists(&r.hash).await.expect("the exists probe failed"),
        "it should not still exist after deletion"
    );
    s.delete(&r.hash).await.expect(
        "a repeated delete must succeed — purge's cascading cleanup job has to be rerunnable",
    );
}

#[tokio::test]
async fn ensure_bucket_is_idempotent() {
    let s = skip_unless_rustfs!();
    // store() already created it once; creating it twice more here should both
    // succeed.
    s.ensure_bucket()
        .await
        .expect("repeated bucket creation should be idempotent");
    s.ensure_bucket()
        .await
        .expect("repeated bucket creation should be idempotent");
}

/// Keyed access round-trips through RustFS, and the key is used **verbatim**.
///
/// `#[ignore]` because it needs a live RustFS, like the rest of this file --
/// but unlike them it is also the only test that exercises the `capture/`
/// namespace against a real S3 implementation. The local backend cannot catch
/// "S3 rejects a key containing something we think is fine".
#[tokio::test]
#[ignore = "needs a live RustFS; run with just test-db or CORTEX_REQUIRE_RUSTFS=1"]
async fn keyed_capture_object_round_trips() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("capture-keyed", 4096);
    let digest = sha256_hex(&payload);
    let key = hash::capture_key(s.tenant(), &digest).expect("a legal capture key");

    s.put_keyed(&key, payload.clone(), "application/zstd")
        .await
        .expect("the keyed upload failed");

    assert!(
        s.exists_keyed(&key).await.expect("exists failed"),
        "the object was uploaded at {key} and HeadObject says it is not there"
    );
    assert_eq!(
        s.get_keyed(&key).await.expect("the keyed read failed"),
        payload,
        "the bytes fetched back differ from the ones uploaded"
    );

    // The key really is the capture layout, not the media one: if the backend
    // had prepended its own prefix the object would be somewhere else entirely,
    // and a tenant reading its own data would find nothing
    assert!(
        key.contains("/capture/"),
        "the key should carry the capture namespace, got {key}"
    );

    s.delete_keyed(&key).await.expect("cleanup failed");
    assert!(
        !s.exists_keyed(&key).await.expect("exists failed"),
        "the object is still there after delete_keyed"
    );
    // Idempotent
    s.delete_keyed(&key)
        .await
        .expect("deleting a missing object must succeed");
}

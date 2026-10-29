//! The local filesystem backend.
//!
//! Two reasons for it to exist:
//!
//! 1. **Tests**: unit tests run a real read/write round trip without needing a
//!    container.
//! 2. **Making "the first version can run without RustFS" true**: for
//!    single-machine personal use, one more alpha-stage object storage process
//!    is one more source of failure.
//!
//! It shares [`hash::storage_key`]'s layout with
//! [`S3BlobStore`](crate::S3BlobStore), so the directory structure corresponds
//! one to one with the keys inside a bucket — a future migration is a plain
//! `aws s3 sync`, and not one character of the `blobs.storage_key` column has
//! to change.

use std::{
    io::ErrorKind,
    ops::Range,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use url::Url;

use crate::{
    BlobError, BlobRef, BlobStore, Result, hash, hash::TenantPrefix, media, validate_range,
    validate_ttl,
};

/// Local storage bucketed by hash prefix. One instance serves one tenant.
#[derive(Debug, Clone)]
pub struct LocalFsBlobStore {
    root: PathBuf,
    tenant: TenantPrefix,
}

impl LocalFsBlobStore {
    /// Build a store rooted at `root` for tenant `tenant`. The directory is
    /// created if it does not exist.
    ///
    /// # Why the tenant is bound at construction rather than being one more
    /// argument on every method
    ///
    /// "Passing the wrong tenant" is completely silent at runtime: objects
    /// written cannot be fetched back, and what is fetched back belongs to
    /// somebody else, and neither raises an error. Meanwhile this store's actual
    /// caller is cormexd's **per-user instance registry** — the tenant is looked
    /// up there once and settled, and every read and write afterwards uses the
    /// same one. Making it a constructor argument compresses "every call might
    /// pass the wrong one" into "one chance to get it wrong when creating the
    /// instance", and that one place happens to be the only place with access
    /// to the user's identity, and thus the only one able to validate it.
    ///
    /// The parameter type is [`TenantPrefix`] rather than `&str`, so validation
    /// happens **further out** (at the moment the `TenantPrefix` is
    /// constructed) and what arrives here is already a value proven safe.
    ///
    /// # Errors
    /// Returns [`BlobError::Io`] if the root directory cannot be created.
    pub async fn new(root: impl Into<PathBuf>, tenant: TenantPrefix) -> Result<Self> {
        let root = root.into();
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self { root, tenant })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The tenant this instance serves. Objects all land beneath
    /// `root/<tenant>/`.
    #[must_use]
    pub fn tenant(&self) -> &TenantPrefix {
        &self.tenant
    }

    fn key_of(&self, hash: &str) -> Result<String> {
        hash::storage_key(&self.tenant, hash)
    }

    /// Hash → absolute path.
    ///
    /// Its safety rests entirely on the two checks over in
    /// [`hash::storage_key`]: the tenant prefix was constrained to the Postgres
    /// identifier character set when the [`TenantPrefix`] was constructed, and
    /// the hash is validated here on every call. So the key can only contain
    /// lowercase alphanumerics, underscores, and the `/` we inserted ourselves,
    /// and `root.join(key)` cannot possibly escape root.
    fn path_of(&self, hash: &str) -> Result<PathBuf> {
        Ok(self.root.join(self.key_of(hash)?))
    }
}

#[async_trait]
impl BlobStore for LocalFsBlobStore {
    async fn put(&self, bytes: Bytes, declared_mime: Option<&str>) -> Result<BlobRef> {
        let digest = hash::sha256_hex(&bytes);
        let storage_key = self.key_of(&digest)?;
        let path = self.root.join(&storage_key);

        let meta = media::probe(&bytes);
        let blob = BlobRef {
            mime: meta.resolve_mime(declared_mime),
            size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            hash: digest,
            storage_key,
            deduplicated: true,
        };

        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(blob);
        }

        let dir = path
            .parent()
            .expect("storage_key guarantees at least two levels of prefix directory");
        tokio::fs::create_dir_all(dir).await?;

        // Write a temporary file first and then rename: rename is atomic within
        // one filesystem, so "the path exists" and "the content is complete"
        // are forever true at the same moment.
        //
        // A direct create + write opens a window in which another reader sees a
        // half-written file — and under content addressing that half-written
        // file's key still claims to be the hash of the complete content,
        // making it the worst kind of corrupt data: corrupt data that vouches
        // for itself.
        //
        // The temporary filename carries the process id and the content hash,
        // guaranteeing that two concurrent writers never write into the same
        // temporary file.
        let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), &blob.hash[..16]));
        {
            let mut f = tokio::fs::File::create(&tmp).await?;
            f.write_all(&bytes).await?;
            // No fsync: this is the local development/test backend, and the real
            // durability promise is provided by RustFS's
            // durability_mode=strict. fsyncing every blob here would make the
            // tests an order of magnitude slower in exchange for durability
            // nobody consumes.
            f.flush().await?;
        }

        match tokio::fs::rename(&tmp, &path).await {
            Ok(()) => {}
            Err(e) => {
                // A race: another writer just renamed the very same content
                // into place. Under content addressing the bytes it wrote are
                // byte-for-byte ours, so simply accept it.
                let _ = tokio::fs::remove_file(&tmp).await;
                if tokio::fs::metadata(&path).await.is_err() {
                    return Err(BlobError::Io(e));
                }
            }
        }

        Ok(BlobRef {
            deduplicated: false,
            ..blob
        })
    }

    async fn get(&self, hash: &str) -> Result<Bytes> {
        let path = self.path_of(hash)?;
        match tokio::fs::read(&path).await {
            Ok(v) => Ok(Bytes::from(v)),
            Err(e) if e.kind() == ErrorKind::NotFound => Err(BlobError::NotFound(hash.to_owned())),
            Err(e) => Err(BlobError::Io(e)),
        }
    }

    async fn get_range(&self, hash: &str, range: Range<u64>) -> Result<Bytes> {
        validate_range(&range)?;
        let path = self.path_of(hash)?;

        let mut f = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(BlobError::NotFound(hash.to_owned()));
            }
            Err(e) => return Err(BlobError::Io(e)),
        };

        f.seek(std::io::SeekFrom::Start(range.start)).await?;

        // When the start is out of bounds, read simply reads 0 bytes, which is
        // indistinguishable from "the file happens to end right here" — but
        // either way the result is empty, so the emptiness check below handles
        // both uniformly.
        let want = usize::try_from(range.end - range.start).unwrap_or(usize::MAX);
        let mut buf = Vec::with_capacity(want.min(1 << 20));
        f.take(range.end - range.start)
            .read_to_end(&mut buf)
            .await?;

        if buf.is_empty() {
            return Err(BlobError::InvalidRange(format!(
                "range [{}, {}) falls entirely outside {hash}",
                range.start, range.end
            )));
        }
        Ok(Bytes::from(buf))
    }

    /// The local backend **cannot issue** presigned URLs: it has no signing key,
    /// and no HTTP endpoint a browser could PUT to directly.
    ///
    /// It returns [`BlobError::Unsupported`] rather than fabricating a `file://`
    /// — the latter would "appear to work" on the desktop (same machine, same
    /// user) and break the moment it reaches mobile or the web, by which point
    /// the error is far from the scene. A caller receiving `Unsupported` should
    /// explicitly degrade to relaying through the server.
    async fn presign_put(&self, hash: &str, ttl: Duration) -> Result<Url> {
        hash::validate_hash(hash)?;
        validate_ttl(ttl)?;
        Err(BlobError::Unsupported(
            "LocalFsBlobStore cannot issue presigned URLs; relay via the server or use S3BlobStore",
        ))
    }

    async fn presign_get(&self, hash: &str, ttl: Duration) -> Result<Url> {
        hash::validate_hash(hash)?;
        validate_ttl(ttl)?;
        Err(BlobError::Unsupported(
            "LocalFsBlobStore cannot issue presigned URLs; relay via the server or use S3BlobStore",
        ))
    }

    async fn exists(&self, hash: &str) -> Result<bool> {
        Ok(tokio::fs::metadata(self.path_of(hash)?).await.is_ok())
    }

    async fn delete(&self, hash: &str) -> Result<()> {
        match tokio::fs::remove_file(self.path_of(hash)?).await {
            Ok(()) => Ok(()),
            // Idempotent: purge's cascading cleanup job has to be rerunnable.
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(BlobError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests uniformly use `public` — that is the name roadmap R9 settled on
    /// for user number one's schema.
    const TEST_TENANT: &str = "public";

    fn tenant(s: &str) -> TenantPrefix {
        TenantPrefix::new(s).unwrap_or_else(|e| {
            panic!("{s:?} should have been a legal tenant prefix, but was rejected: {e}")
        })
    }

    async fn store() -> (LocalFsBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("failed to create a temporary directory");
        let s = LocalFsBlobStore::new(dir.path(), tenant(TEST_TENANT))
            .await
            .expect("failed to construct a LocalFsBlobStore");
        (s, dir)
    }

    #[tokio::test]
    async fn round_trip_preserves_bytes() {
        let (s, _d) = store().await;
        let payload = Bytes::from_static(b"cormex blob round trip \xE4\xB8\xAD\xE6\x96\x87");

        let r = s.put(payload.clone(), None).await.expect("write failed");
        assert_eq!(
            r.hash,
            hash::sha256_hex(&payload),
            "the returned hash does not match the content, which breaks content addressing"
        );
        assert_eq!(
            r.size_bytes,
            payload.len() as i64,
            "size_bytes does not match the actual byte count"
        );
        assert!(
            !r.deduplicated,
            "a first write should not be marked as a deduplication hit"
        );

        let got = s.get(&r.hash).await.expect("fetching back by hash failed");
        assert_eq!(
            got, payload,
            "the bytes fetched back differ from the ones written"
        );
    }

    #[tokio::test]
    async fn objects_land_under_the_shared_key_layout() {
        // The directory structure has to correspond segment by segment with the
        // keys inside an S3 bucket, so that a future `aws s3 sync` migrates
        // directly and the `blobs.storage_key` column never has to be rewritten.
        let (s, dir) = store().await;
        let r = s.put(Bytes::from_static(b"layout"), None).await.unwrap();

        assert_eq!(
            s.root(),
            dir.path(),
            "root() should return the directory given at construction"
        );
        let on_disk = dir.path().join(&r.storage_key);
        assert!(
            tokio::fs::metadata(&on_disk).await.is_ok(),
            "the file should land exactly at root/storage_key, but was not found there: {}",
            on_disk.display()
        );
        assert!(
            r.storage_key.starts_with(&format!(
                "{TEST_TENANT}/blobs/{}/{}/",
                &r.hash[0..2],
                &r.hash[2..4]
            )),
            "storage_key should be \"tenant / blobs / two levels of hash prefix\", got {}",
            r.storage_key
        );
    }

    #[tokio::test]
    async fn two_tenants_sharing_a_root_stay_separate() {
        // Two users' stores on the same machine share one root. Cross-tenant
        // deduplication has been deliberately given up, so the same bytes have
        // to leave two copies on disk, invisible to each other — otherwise A's
        // purge deletes an object B is still referencing, while A's schema
        // cannot see B's reference at all.
        let dir = tempfile::tempdir().expect("failed to create a temporary directory");
        let a = LocalFsBlobStore::new(dir.path(), tenant("public"))
            .await
            .unwrap();
        let b = LocalFsBlobStore::new(dir.path(), tenant("cormex_u2"))
            .await
            .unwrap();
        let payload = Bytes::from_static(b"one file, two owners");

        let ra = a
            .put(payload.clone(), None)
            .await
            .expect("A's write failed");
        let rb = b
            .put(payload.clone(), None)
            .await
            .expect("B's write failed");

        assert_eq!(
            ra.hash, rb.hash,
            "under content addressing the hash itself should still be independent of the tenant"
        );
        assert_ne!(
            ra.storage_key, rb.storage_key,
            "the two tenants' storage_keys have to differ, otherwise A's purge deletes B's bytes"
        );
        assert!(
            !rb.deduplicated,
            "B's write should not hit the object A already wrote — cross-tenant deduplication is \
             precisely what this change removed"
        );

        // The mutual-invisibility half matters more: after A has deleted
        // everything, B must be intact.
        a.delete(&ra.hash).await.expect("A's delete failed");
        assert!(
            !a.exists(&ra.hash).await.unwrap(),
            "after A's delete, A's own copy should be gone"
        );
        assert_eq!(
            b.get(&rb.hash)
                .await
                .expect("A's delete reached B's object"),
            payload,
            "A's purge must not affect B's bytes"
        );
    }

    #[tokio::test]
    async fn blob_ref_survives_a_json_round_trip() {
        // cormexd sends this out as an HTTP response body, so the field names
        // and types are an outward contract.
        let (s, _d) = store().await;
        let r = s.put(Bytes::from_static(b"serde"), None).await.unwrap();

        let json = serde_json::to_string(&r).expect("serializing BlobRef failed");
        let back: BlobRef = serde_json::from_str(&json).expect("deserializing BlobRef failed");
        assert_eq!(back, r, "BlobRef changed across a JSON round trip: {json}");
    }

    #[tokio::test]
    async fn second_put_is_deduplicated() {
        let (s, _d) = store().await;
        let payload = Bytes::from_static(b"same content twice");

        let a = s.put(payload.clone(), None).await.unwrap();
        let b = s.put(payload.clone(), None).await.unwrap();

        assert_eq!(
            a.hash, b.hash,
            "two writes of the same content must yield the same key"
        );
        assert_eq!(
            a.storage_key, b.storage_key,
            "the storage_key must match as well"
        );
        assert!(
            !a.deduplicated,
            "the first write is not a deduplication hit"
        );
        assert!(
            b.deduplicated,
            "the second write should hit the existing object rather than actually writing to disk \
             again"
        );
    }

    #[tokio::test]
    async fn concurrent_put_of_same_content_stays_intact() {
        let (s, _d) = store().await;
        // Big enough to give "seen half-written" a chance to happen — a small
        // file is done in a single write.
        let payload = Bytes::from(vec![0x5Au8; 3 << 20]);
        let expected = hash::sha256_hex(&payload);

        let s = std::sync::Arc::new(s);
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let s = s.clone();
            let p = payload.clone();
            tasks.push(tokio::spawn(async move { s.put(p, None).await }));
        }
        for t in tasks {
            let r = t
                .await
                .expect("a task panicked")
                .expect("a concurrent write errored");
            assert_eq!(
                r.hash, expected,
                "a concurrent write returned a different hash"
            );
        }

        let got = s
            .get(&expected)
            .await
            .expect("could not fetch back after the concurrent writes");
        assert_eq!(
            hash::sha256_hex(&got),
            expected,
            "the concurrent writes produced a half-written file — rename's atomicity did not hold"
        );
    }

    #[tokio::test]
    async fn range_read_returns_the_slice() {
        let (s, _d) = store().await;
        let payload = Bytes::from_static(b"0123456789abcdef");
        let r = s.put(payload.clone(), None).await.unwrap();

        let mid = s
            .get_range(&r.hash, 4..10)
            .await
            .expect("range read failed");
        assert_eq!(
            &mid[..],
            b"456789",
            "the range is half-open, and the slice fetched is wrong"
        );

        let tail = s
            .get_range(&r.hash, 14..999)
            .await
            .expect("an out-of-bounds end should be clamped rather than error");
        assert_eq!(
            &tail[..],
            b"ef",
            "with the end out of bounds it should return the bytes up to the end of the file"
        );

        assert!(
            s.get_range(&r.hash, 5..5).await.is_err(),
            "an empty range must be refused"
        );
        assert!(
            s.get_range(&r.hash, 99..120).await.is_err(),
            "with the start entirely out of bounds it should error, not quietly return empty \
             content"
        );
    }

    #[tokio::test]
    async fn missing_blob_is_not_found_not_io_error() {
        let (s, _d) = store().await;
        let absent = hash::sha256_hex(b"never written");

        assert!(
            !s.exists(&absent).await.unwrap(),
            "content that was never written should not exist"
        );
        let e = s
            .get(&absent)
            .await
            .expect_err("fetching a non-existent object should error");
        assert!(
            matches!(e, BlobError::NotFound(_)),
            "absence has to classify as NotFound (HTTP 404); misreporting it as an IO error makes \
             it a 500: {e:?}"
        );
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (s, _d) = store().await;
        let r = s
            .put(Bytes::from_static(b"to be purged"), None)
            .await
            .unwrap();

        s.delete(&r.hash).await.expect("the first delete failed");
        assert!(
            !s.exists(&r.hash).await.unwrap(),
            "it should not still exist after deletion"
        );
        s.delete(&r.hash).await.expect(
            "a repeated delete must succeed — purge's cascading cleanup job has to be rerunnable",
        );
    }

    #[tokio::test]
    async fn traversal_hash_never_touches_the_filesystem() {
        let (s, dir) = store().await;
        // The target file lies outside root. Were the validation to fall, any
        // of the calls below would touch it.
        let outside = dir.path().parent().unwrap().join("cormex-traversal-canary");
        tokio::fs::write(&outside, b"secret").await.unwrap();

        for evil in [
            "../cormex-traversal-canary",
            "../../etc/passwd",
            "..",
            "a/../b",
        ] {
            assert!(
                matches!(s.get(evil).await, Err(BlobError::InvalidHash(_))),
                "get did not block the path traversal payload {evil:?}; this is arbitrary file read"
            );
            assert!(
                matches!(s.exists(evil).await, Err(BlobError::InvalidHash(_))),
                "exists did not block the path traversal payload {evil:?}"
            );
            assert!(
                matches!(s.delete(evil).await, Err(BlobError::InvalidHash(_))),
                "delete did not block the path traversal payload {evil:?}; this is arbitrary file \
                 deletion"
            );
            assert!(
                matches!(
                    s.get_range(evil, 0..4).await,
                    Err(BlobError::InvalidHash(_))
                ),
                "get_range did not block the path traversal payload {evil:?}"
            );
        }

        assert!(
            tokio::fs::metadata(&outside).await.is_ok(),
            "a file outside root was deleted — the path traversal defence has been breached"
        );
        let _ = tokio::fs::remove_file(&outside).await;
    }

    #[tokio::test]
    async fn presign_is_explicitly_unsupported() {
        let (s, _d) = store().await;
        let h = hash::sha256_hex(b"x");
        let ttl = Duration::from_secs(60);

        assert!(
            matches!(s.presign_put(&h, ttl).await, Err(BlobError::Unsupported(_))),
            "the local backend should report Unsupported plainly so callers can degrade, rather \
             than fabricating a file:// URL"
        );
        assert!(matches!(
            s.presign_get(&h, ttl).await,
            Err(BlobError::Unsupported(_))
        ));
        // Illegal arguments still have to be refused ahead of "unsupported",
        // otherwise the caller never discovers it passed the wrong thing.
        assert!(matches!(
            s.presign_get("nope", ttl).await,
            Err(BlobError::InvalidHash(_))
        ));
    }

    #[tokio::test]
    async fn mime_is_sniffed_not_trusted() {
        let (s, _d) = store().await;
        let png = Bytes::from_static(&[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89,
        ]);

        let r = s.put(png, Some("application/octet-stream")).await.unwrap();
        assert_eq!(
            r.mime, "image/png",
            "when the leading bytes say PNG they have to override the client's octet-stream \
             claim, otherwise this content becomes a retrieval black hole"
        );

        let r = s
            .put(Bytes::from_static(b"# hi"), Some("text/markdown"))
            .await
            .unwrap();
        assert_eq!(
            r.mime, "text/markdown",
            "when the type cannot be recognized it should fall back to the client's claim"
        );

        let r = s.put(Bytes::from_static(b"?!"), None).await.unwrap();
        assert_eq!(
            r.mime,
            media::DEFAULT_MIME,
            "with neither available it must yield a fallback writable into blobs.mime NOT NULL"
        );
    }
}

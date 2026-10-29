//! Content addressing: computing hashes, validating them, and deriving storage
//! keys.
//!
//! # Why SHA-256 rather than the faster BLAKE3
//!
//! Not for speed, but for **contract alignment**: the `sha256` domain in the
//! migrations is `^[0-9a-f]{64}$`, and the S3 ecosystem
//! (`x-amz-checksum-sha256`) supports it natively. Switching to BLAKE3 would
//! mean changing the schema and the external interface at once, and the CPU
//! saved is not worth that price.

use sha2::{Digest, Sha256};

use crate::error::{BlobError, Result};

/// The number of characters in a hex hash. Matches the migrations' `sha256`
/// domain.
pub const HASH_HEX_LEN: usize = 64;

/// The segment that comes after the tenant prefix and before the hash buckets.
/// It reserves a namespace, so that putting other things under the same tenant
/// later (export bundles, thumbnails) will not collide with the bytes.
const KEY_PREFIX: &str = "blobs";

/// The namespace for captured session chunks, beside [`KEY_PREFIX`] inside the
/// tenant.
///
/// Separate from `blobs/` because the two are unlike in every way that matters:
/// media is content-addressed by the hash of the **stored** bytes and is
/// referenced from the `blobs` table (and therefore synced); capture chunks are
/// keyed by the hash of the **logical (uncompressed)** bytes and are never
/// synced (D3). Sharing one namespace would make "delete everything under
/// `blobs/`" — which is what a media GC does — silently take captured
/// conversations with it.
const CAPTURE_PREFIX: &str = "capture";

/// The upper bound on a tenant prefix's length, taken from Postgres's
/// `NAMEDATALEN - 1`.
///
/// Not an arbitrary margin: a name longer than this **cannot become a schema**
/// (Postgres truncates it silently), so two "different" tenants may land on the
/// same schema. Since such a name cannot correspond to a real tenant, it must
/// not be allowed into a key either.
const MAX_TENANT_LEN: usize = 63;

/// Compute the content hash (lowercase hex).
///
/// This is the **one** entry point of content addressing: identical bytes must
/// yield an identical key, which makes "upload" idempotent for free,
/// "deduplication" free for free, and objects immutable for free — and is also
/// why RustFS not supporting object versioning does not matter: the content
/// under a key is forever the same bytes, so the notion of an "older version"
/// does not exist at all.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Validate that a hash is 64 lowercase hex digits.
///
/// # This is a security boundary, not format fastidiousness
///
/// The hash is the only value that flows in from outside (a client's URL path,
/// the sync queue, a database row) and into a storage path.
/// [`LocalFsBlobStore`](crate::LocalFsBlobStore) uses it to assemble a file
/// path, and S3 uses it to assemble an object key. Let a
/// `../../../etc/passwd` through and the former becomes arbitrary file read and
/// write.
///
/// The hex character set contains no `.`, `/`, `\`, `%` or NUL, so **this one
/// check seals off every path traversal vector** — no downstream escaping is
/// needed, and none should be added: two independent checks are guaranteed to
/// disagree one day.
///
/// # Errors
/// Returns [`BlobError::InvalidHash`] when the shape is wrong.
pub fn validate_hash(hash: &str) -> Result<()> {
    let well_formed = hash.len() == HASH_HEX_LEN
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if well_formed {
        Ok(())
    } else {
        Err(BlobError::InvalidHash(hash.to_owned()))
    }
}

/// A tenant prefix that has already been validated.
///
/// # Why a newtype, rather than revalidating before each use as hashes do
///
/// Hashes have no choice: they flow in from outside **each and every time** (a
/// URL path, the sync queue, a database row), and each time is a fresh piece of
/// untrusted input. A tenant prefix is not like that — it arrives once when the
/// store is constructed, and every key thereafter reuses the same copy. Were it
/// also to go the "check again each time" route, the check would be scattered
/// across a dozen methods in two backends, and [`validate_hash`]'s
/// documentation already spells out why that is wrong: two independent checks
/// are guaranteed to disagree one day.
///
/// So instead: **construction is validation, and the type is the proof**. The
/// inner field is private and [`TenantPrefix::new`] is the only way in, so
/// **holding a `TenantPrefix` is holding proof that it has been validated** —
/// downstream neither has to ask again nor has any way to forge one around it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TenantPrefix(String);

impl TenantPrefix {
    /// Validate and construct. For the rules see [`validate_tenant`].
    ///
    /// # Errors
    /// Returns [`BlobError::InvalidTenant`] when the shape is wrong.
    pub fn new(prefix: impl Into<String>) -> Result<Self> {
        let prefix = prefix.into();
        validate_tenant(&prefix)?;
        Ok(Self(prefix))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for TenantPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Validate a tenant prefix, admitting only the "Postgres identifier" shape:
/// the first character is an ASCII letter or `_`, the rest are letters, digits
/// or `_`, and the length does not exceed [`MAX_TENANT_LEN`].
///
/// # This is the same line of defence as the hash check
///
/// Under multi-tenancy the prefix is the **second** external input that goes
/// straight into a storage path: it becomes a segment of
/// [`LocalFsBlobStore`](crate::LocalFsBlobStore)'s file path, and the leading
/// segment of the S3 object key. Let a `../../..` through and the consequences
/// are exactly those of letting a bad hash through.
///
/// It therefore follows the same approach: **admit by character set rather than
/// block by blacklist**. The permitted characters include no `.`, `/`, `\`,
/// `%`, NUL or whitespace, so payloads like `..`, `a/../b`, `..%2f..` and
/// `\..\windows` are all already rejected without any of them being enumerated
/// — a blacklist is always one entry short, a whitelist never is.
///
/// # Case: **both are accepted here**, but upstream only mints lowercase
///
/// `cormex_store::SchemaName` currently produces only `public` and `u_` plus a
/// **26-character lowercase** ULID. Lowercase was forced by a real observation:
/// an unquoted identifier in `search_path` gets folded to lowercase by
/// Postgres, while the schema was created by `CREATE SCHEMA "u_ABC"` and kept
/// its uppercase — and when the two disagree, `search_path` points at a schema
/// that **does not exist**, which Postgres **skips silently**, so every write
/// lands in `public` (i.e. user number one's database) and `migrate()` returns
/// `Ok`.
///
/// So why accept uppercase here? Because **this layer should not do upstream's
/// normalization for it**. Its job is to reject things that can traverse
/// directories or turn into two keys, and uppercase is neither.
///
/// The residual hazard is real: NTFS and APFS are case-insensitive, so `Ab/`
/// and `ab/` are the same directory, whereas S3 keys are case-sensitive and
/// they are two objects — the same code behaving differently on two backends.
/// It does not bite here only because the shapes upstream mints never differ by
/// case alone.
///
/// **From which follows a prohibition**: never add a "fold everything to
/// lowercase" normalization step here. That would make every object already
/// stored under an uppercase key collectively unfindable after one upgrade —
/// with no error, just "all the historical attachments are gone".
///
/// # Three further rules, each replacing a class of silent failure
///
/// - **Reject `-` and `$`.** A schema name containing a hyphen needs double
///   quotes everywhere in SQL, and the store layer has around 77 queries.
///   Blocking it here is far more reliable than hoping every one of them
///   remembers to quote.
/// - **Reject a leading digit.** Postgres does not permit it in an unquoted
///   identifier — again "unusable without quoting".
/// - **Reject the empty string.** An empty prefix degenerates the key into
///   `/blobs/…` (an empty leading segment), and on the local backend
///   `root.join("/blobs/…")` is treated as an absolute path on Unix and
///   **discards root** — a real, and silent, behaviour of `Path::join`.
///
/// # Errors
/// Returns [`BlobError::InvalidTenant`] when the shape is wrong.
fn validate_tenant(prefix: &str) -> Result<()> {
    let mut bytes = prefix.bytes();
    let head_ok = matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_');
    let tail_ok = bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_');

    if head_ok && tail_ok && prefix.len() <= MAX_TENANT_LEN {
        Ok(())
    } else {
        Err(BlobError::InvalidTenant(prefix.to_owned()))
    }
}

/// Derive the object key from a tenant prefix and a hash:
/// `<tenant>/blobs/ab/cd/<64-char hash>`.
///
/// The two levels of hash bucketing are not decoration:
/// - if `LocalFsBlobStore` laid a million files flat into one directory, both
///   ext4's htree and NTFS's directory index would degrade noticeably, and `ls`
///   would simply hang
/// - on the S3 side the prefix affects partitioning, and a hash prefix is
///   naturally uniform
///
/// Both backends **must share this function** for keys to be portable between
/// them — so that migrating from LocalFs to RustFS does not require rewriting
/// the `blobs.storage_key` column.
///
/// # Why the tenant is outermost, with `blobs/` kept inside it
///
/// With the tenant outermost, "erase all of one user's bytes" is a single
/// prefix delete (`aws s3 rm --recursive s3://bucket/<tenant>/`, or locally
/// `rm -rf root/<tenant>`), and per-user usage accounting is a single prefix
/// aggregation. Turn it around into `blobs/<tenant>/…` and both of those become
/// full-bucket scans.
///
/// `blobs/` is not dropped: its original purpose (other things in the same
/// bucket not colliding) now applies one level down, inside the tenant — a
/// future `<tenant>/exports/` should not be mixed in the same layer as the
/// bytes.
///
/// # The price is no cross-tenant deduplication, and that was bought, not
/// overlooked
///
/// The same bytes referenced by two tenants are stored twice. What that buys
/// back is GC becoming a problem internal to one schema again: when A deletes
/// its last reference it does not have to ask "is B still referencing this" —
/// and under schema isolation that question cannot be answered at all, since
/// B's tables are not visible in the `search_path` of A's connection pool. The
/// extra disk is bounded and estimable; cross-schema reference counting is not
/// solvable.
///
/// # Errors
/// Returns [`BlobError::InvalidHash`] when the hash is illegal.
pub fn storage_key(tenant: &TenantPrefix, hash: &str) -> Result<String> {
    validate_hash(hash)?;
    Ok(format!(
        "{tenant}/{KEY_PREFIX}/{}/{}/{hash}",
        &hash[0..2],
        &hash[2..4]
    ))
}

/// Derive the object key for a captured session chunk:
/// `<tenant>/capture/ab/cd/<64-char hash>.zst`.
///
/// # Why the hash is of the logical bytes, not the stored object
///
/// The same file can be captured as plaintext (`.jsonl`) or as a zstd frame
/// (Codex's `.jsonl.zst`), and both must land on **one** key — otherwise the
/// client's "plaintext wins, drop the compressed duplicate" rule would still
/// upload the same conversation twice, and the two copies would be
/// indistinguishable afterwards.
///
/// # Why `.zst` is in the key rather than a separate field
///
/// The encoding is a property of the stored object, and the key names the
/// stored object. A future `identity` encoding gets its own suffix; the
/// database row records which one it used (`capture_chunks.encoding`), so the
/// key and the row cannot disagree about what is inside.
///
/// # Errors
/// [`BlobError::InvalidHash`] when the hash is illegal — the same fence against
/// path traversal as [`storage_key`].
pub fn capture_key(tenant: &TenantPrefix, sha256: &str) -> Result<String> {
    validate_hash(sha256)?;
    Ok(format!(
        "{tenant}/{CAPTURE_PREFIX}/{}/{}/{sha256}.zst",
        &sha256[0..2],
        &sha256[2..4]
    ))
}

/// Check that a string is a well-formed capture key **and** that its two
/// directory levels agree with the hash they claim to bucket.
///
/// # Why the bucket check is not redundant
///
/// The shape alone admits `ab/cd/<hash starting with 11>`. Such a key is legal
/// to write, and then the object is unreachable by
/// [`capture_key`] — so a typo in the bucketing step produces "the chunk was
/// uploaded, and the reader cannot find it", with the upload reporting success.
///
/// # Two layouts
///
/// ```text
/// {tenant}/capture/aa/bb/{sha}.zst                 plaintext (P1)
/// {tenant}/capture/{key_id}/aa/bb/{sha}.zst.enc    encrypted  (P6-1)
/// ```
///
/// Both are accepted because both exist: a deployment that turns on
/// `CORMEX_SECRET_KEY` keeps reading every chunk written before it, and the
/// old keys must stay valid — the alternative is a migration that rewrites
/// every object, which is precisely what content addressing cannot do.
///
/// The key id is validated as strictly as the hash: it becomes a path segment,
/// and this function is the only thing standing between an outside value and
/// `root.join(key)`.
///
/// # Errors
/// [`BlobError::InvalidHash`] when the key does not have either capture layout.
pub fn validate_capture_key(key: &str) -> Result<()> {
    // Split from the right: the tenant prefix may itself contain no `/` (it is
    // a Postgres identifier), but splitting from the left would let a
    // malformed key with an extra segment pass by shifting everything along
    let mut parts = key.split('/');
    let (Some(tenant), Some(capture), Some(third), Some(fourth), Some(fifth), sixth, seventh) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(BlobError::InvalidHash(key.to_owned()));
    };

    if capture != CAPTURE_PREFIX || validate_tenant(tenant).is_err() {
        return Err(BlobError::InvalidHash(key.to_owned()));
    }

    // ── encrypted: tenant/capture/{key_id}/aa/bb/{sha}.zst.enc ──
    if let (Some(sixth), None) = (sixth, seventh) {
        let stem = sixth
            .strip_suffix(".zst.enc")
            .ok_or_else(|| BlobError::InvalidHash(key.to_owned()))?;
        let ok = is_safe_segment(third)
            && fourth.len() == 2
            && fifth.len() == 2
            && validate_hash(stem).is_ok()
            && stem.starts_with(fourth)
            && &stem[2..4] == fifth;
        return if ok {
            Ok(())
        } else {
            Err(BlobError::InvalidHash(key.to_owned()))
        };
    }

    // ── plaintext: tenant/capture/aa/bb/{sha}.zst ──
    if seventh.is_some() {
        return Err(BlobError::InvalidHash(key.to_owned()));
    }
    let stem = fifth
        .strip_suffix(".zst")
        .ok_or_else(|| BlobError::InvalidHash(key.to_owned()))?;
    let ok = third.len() == 2
        && fourth.len() == 2
        && validate_hash(stem).is_ok()
        && stem.starts_with(third)
        && &stem[2..4] == fourth;
    if ok {
        Ok(())
    } else {
        Err(BlobError::InvalidHash(key.to_owned()))
    }
}

/// Whether a string is safe to splice into a storage path as one segment.
///
/// ASCII alphanumerics only: no `.`, `/`, `\`, `%` or NUL, so no traversal and
/// no escaping. A key id is a ULID in practice, and this is deliberately
/// narrower than "whatever a ULID happens to be".
fn is_safe_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(s: &str) -> TenantPrefix {
        TenantPrefix::new(s).unwrap_or_else(|e| {
            panic!("{s:?} should have been a legal tenant prefix, but was rejected: {e}")
        })
    }

    /// Both capture layouts validate, and the encrypted one keeps the hash
    /// buckets honest too.
    #[test]
    fn both_capture_layouts_validate() {
        let sha = "ab".to_owned() + &"cd".repeat(31);
        assert!(validate_capture_key(&format!("public/capture/ab/cd/{sha}.zst")).is_ok());
        assert!(validate_capture_key(&format!("public/capture/01ABC/ab/cd/{sha}.zst.enc")).is_ok());

        // The bucket check applies to the encrypted layout as well: a key whose
        // levels disagree with its hash is legal to write and then unreachable
        assert!(
            validate_capture_key(&format!("public/capture/01ABC/zz/cd/{sha}.zst.enc")).is_err(),
            "the encrypted layout skipped the bucket check"
        );
    }

    /// A key id that could escape the root is refused.
    #[test]
    fn an_unsafe_key_id_is_refused() {
        let sha = "ab".to_owned() + &"cd".repeat(31);
        for bad in ["..", ".", "a/b", "a.b", "a%2f", ""] {
            assert!(
                validate_capture_key(&format!("public/capture/{bad}/ab/cd/{sha}.zst.enc")).is_err(),
                "the key id {bad:?} was accepted as a path segment"
            );
        }
    }

    /// An extra segment is refused in both layouts.
    #[test]
    fn an_extra_segment_is_refused() {
        let sha = "ab".to_owned() + &"cd".repeat(31);
        assert!(
            validate_capture_key(&format!("public/capture/x/ab/cd/{sha}.zst.enc")).is_ok(),
            "a well-formed encrypted key was rejected"
        );
        assert!(
            validate_capture_key(&format!("public/capture/x/ab/cd/extra/{sha}.zst.enc")).is_err(),
            "an extra segment was accepted"
        );
    }

    #[test]
    fn same_bytes_yield_same_key() {
        let a = sha256_hex(b"cormex");
        assert_eq!(
            a,
            sha256_hex(b"cormex"),
            "the foundation of content addressing: identical bytes must yield an identical key, \
             or deduplication and idempotence both stop working"
        );
        assert_ne!(
            a,
            sha256_hex(b"cortey"),
            "one character of difference should give an entirely different key, otherwise \
             deduplication hands back the wrong content"
        );
    }

    #[test]
    fn hash_matches_migration_domain() {
        let h = sha256_hex(b"");
        assert_eq!(
            h.len(),
            HASH_HEX_LEN,
            "the length has to match the migrations' sha256 domain, or the CHECK rejects it on \
             insert"
        );
        assert!(
            h.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "a character outside lowercase hex appeared, which Postgres's sha256 domain will \
             reject: {h}"
        );
        // The SHA-256 of the empty string is a well-known constant; use it to
        // pin down that the algorithm really is SHA-256.
        assert_eq!(
            h, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "the digest of empty input does not match the SHA-256 standard value, so either the \
             algorithm or the hex encoding is wrong"
        );
    }

    #[test]
    fn traversal_payloads_are_rejected() {
        // Every one of these could genuinely turn up in a URL path or in the
        // offline sync queue.
        let payloads = [
            "../../../etc/passwd",
            "..",
            "a/../b",
            "..%2f..%2fetc",
            // Exactly 64 characters long but with slashes stuffed in — an
            // implementation that only checks the length falls here.
            "../../../../../../../../../../../../../../../../../etc/passwd/aaa",
            "\\..\\..\\windows\\system32",
            "0000000000000000000000000000000000000000000000000000000000000/..",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b8\0\0",
        ];
        for h in payloads {
            assert!(
                validate_hash(h).is_err(),
                "a path traversal payload was not blocked; LocalFsBlobStore would get arbitrary \
                 file read and write: {h:?}"
            );
            assert!(
                storage_key(&tenant("public"), h).is_err(),
                "storage_key did not reuse the validation, so an illegal hash leaked into an \
                 object key: {h:?}"
            );
        }
    }

    #[test]
    fn malformed_hashes_are_rejected() {
        let valid = sha256_hex(b"x");
        assert!(
            validate_hash(&valid).is_ok(),
            "a legal hash was wrongly rejected: {valid}"
        );

        assert!(
            validate_hash(&valid.to_uppercase()).is_err(),
            "uppercase hex must be refused — admitting it makes one piece of content produce two \
             keys, and deduplication stops working outright"
        );
        assert!(
            validate_hash(&valid[..HASH_HEX_LEN - 1]).is_err(),
            "a truncated hash must be refused, otherwise prefix collisions map different content \
             onto the same object"
        );
        assert!(
            validate_hash(&format!("{valid}0")).is_err(),
            "an overlong hash must be refused"
        );
        assert!(validate_hash("").is_err(), "an empty hash must be refused");
    }

    #[test]
    fn storage_key_layout_is_stable() {
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            storage_key(&tenant("public"), hash).unwrap(),
            format!("public/blobs/e3/b0/{hash}"),
            "the key layout is a contract shared by both backends; changing it makes every \
             storage_key already in the database point at the wrong place"
        );
    }

    #[test]
    fn capture_key_layout_is_stable() {
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(
            capture_key(&tenant("public"), hash).unwrap(),
            format!("public/capture/e3/b0/{hash}.zst"),
            "the capture layout is what `capture_chunks.storage_key` already holds for every \
             chunk uploaded so far; changing it makes those rows point at nothing"
        );
        assert!(
            capture_key(&tenant("public"), hash).unwrap()
                != storage_key(&tenant("public"), hash).unwrap(),
            "capture chunks and media must not share a namespace: a media GC that deletes \
             everything under blobs/ would otherwise take captured conversations with it"
        );
    }

    #[test]
    fn capture_keys_round_trip_through_the_validator() {
        let hash = sha256_hex(b"a captured chunk");
        let key = capture_key(&tenant("u_01abc"), &hash).unwrap();
        assert!(
            validate_capture_key(&key).is_ok(),
            "a key this module produced was rejected by its own validator: {key}"
        );
    }

    #[test]
    fn capture_key_validation_rejects_traversal_and_mismatched_buckets() {
        let hash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        for bad in [
            // The bucket directories disagree with the hash they claim to
            // bucket. Legal to write, and then unreachable by capture_key --
            // the upload reports success and the reader finds nothing
            format!("public/capture/00/00/{hash}.zst"),
            // A traversal payload in the tenant segment
            format!("../../etc/capture/e3/b0/{hash}.zst"),
            // Missing the .zst suffix
            format!("public/capture/e3/b0/{hash}"),
            // An extra segment
            format!("public/capture/e3/b0/extra/{hash}.zst"),
            // The wrong namespace
            format!("public/blobs/e3/b0/{hash}.zst"),
            // A traversal payload in the hash position
            "public/capture/e3/b0/../../../etc/passwd.zst".to_owned(),
        ] {
            assert!(
                validate_capture_key(&bad).is_err(),
                "this key should have been rejected: {bad}"
            );
        }
    }

    #[test]
    fn tenant_is_the_outermost_segment() {
        // For "erase all of one user's bytes" to degenerate into a single
        // prefix delete, the tenant has to be the leading segment. If this is
        // ever changed to blobs/<tenant>/…, this test goes red.
        let hash = sha256_hex(b"tenancy");
        let key = storage_key(&tenant("cormex_u7"), &hash).unwrap();
        assert!(
            key.starts_with("cormex_u7/"),
            "the tenant has to be the key's leading segment, otherwise per-user deletion and \
             per-user accounting both need a full-bucket scan; got {key}"
        );
        assert_eq!(
            key,
            format!("cormex_u7/blobs/{}/{}/{hash}", &hash[0..2], &hash[2..4]),
            "after the tenant prefix it still has to be blobs/ plus two levels of hash bucketing \
             — both backends share this layout"
        );
    }

    #[test]
    fn different_tenants_never_share_an_object() {
        // This is exactly what that change bought: giving up cross-tenant
        // deduplication in exchange for eliminating the whole cross-schema GC
        // bug class of "A deletes its last reference while the bytes are still
        // referenced by B".
        let hash = sha256_hex(b"same bytes, two owners");
        let a = storage_key(&tenant("public"), &hash).unwrap();
        let b = storage_key(&tenant("cormex_u2"), &hash).unwrap();
        assert_ne!(
            a, b,
            "the same bytes under two tenants have to land on different keys — the same key \
             means GC has turned back into a cross-schema problem"
        );
    }

    #[test]
    fn realistic_schema_names_are_accepted() {
        // The cost of wrongly rejecting is just as concrete: every one of these
        // names already exists in the repository, and refusing any of them
        // stops the corresponding deployment or test from starting at all.
        for ok in [
            "public", // user number one's schema is called exactly this (roadmap R9)
            "u_01bx5zzkbkactav9wevgemmvrz", // the shape cormex_store::SchemaName mints today (lowercase)
            "u_01BX5ZZKBKACTAV9WEVGEMMVRZ", // uppercase accepted too — this layer does not normalize for upstream
            "cormex_auth",                  // the schema the authentication tables live in
            "cormex_test_1_2_3", // the temporary schema generated by the cormex-store test harness
            "_leading_underscore", // Postgres permits a leading underscore
            "t",                 // a single character
            "u2026",             // ending in digits
            &"a".repeat(MAX_TENANT_LEN), // exactly 63, inside the boundary
        ] {
            assert!(
                TenantPrefix::new(ok).is_ok(),
                "{ok:?} is a legal Postgres schema name; rejecting it wrongly leaves that tenant \
                 completely unable to read or write objects"
            );
        }
    }

    /// Report every shape that slipped through in one go, rather than panicking
    /// on the first one hit.
    ///
    /// A sequence of individual `assert!`s would be harmful here: once the
    /// validation is broken, the test would report only the **first** payload
    /// that got through, and after fixing it another run reports the next one —
    /// whereas what one actually needs to see is "which categories leaked in
    /// total".
    fn assert_all_rejected(payloads: &[(&str, &str)]) {
        let escaped: Vec<String> = payloads
            .iter()
            .filter(|(evil, _)| TenantPrefix::new(*evil).is_ok())
            .map(|(evil, why)| format!("  {evil:?} — {why}"))
            .collect();
        assert!(
            escaped.is_empty(),
            "the following {} prefixes should have been refused but were admitted:\n{}",
            escaped.len(),
            escaped.join("\n")
        );
    }

    #[test]
    fn traversal_prefixes_are_rejected() {
        // Each entry corresponds to one way for a prefix to escape root. Once
        // this validation falls, LocalFsBlobStore means arbitrary path read and
        // write — exactly the consequence of a bad hash.
        assert_all_rejected(&[
            ("..", "the parent directory"),
            (
                ".",
                "the current directory: would collapse two tenants onto one key",
            ),
            ("../..", "several levels up"),
            ("../etc", "up a level and into a system directory"),
            (
                "a/b",
                "an embedded slash: conjures one extra prefix level out of nothing",
            ),
            (
                "/abs",
                "an absolute path: on Unix root.join simply discards root",
            ),
            ("acme/", "a trailing slash"),
            ("..\\..", "Windows separators"),
            ("a\\b", "an embedded backslash"),
            (
                "..%2f..",
                "a URL-encoded slash: an implementation blocking only a literal / falls here",
            ),
            (
                "a\0b",
                "an embedded NUL: after truncation at the C layer the remaining path is not the one you saw",
            ),
        ]);
    }

    #[test]
    fn empty_prefix_is_rejected() {
        assert!(
            TenantPrefix::new("").is_err(),
            "an empty prefix must be refused: the key degenerates into /blobs/… with an empty \
             leading segment, and on the local backend join discards root"
        );
    }

    /// Uppercase must be **admitted**, and this has to be pinned down:
    /// the user schemas `cormex_store::SchemaName` mints are `u_` plus a
    /// 26-character uppercase ULID. If somebody ever "tightens it to lowercase
    /// only" in passing, the consequence is that no tenant other than user
    /// number one (`public`) can construct a store at all.
    ///
    /// It equally pins down "no lowercase normalization": that would make every
    /// object already stored under an uppercase ULID collectively unfindable
    /// after an upgrade — with no error, just all the historical attachments
    /// gone.
    #[test]
    fn uppercase_ulid_schemas_are_accepted_verbatim() {
        let ulid = "u_01ARZ3NDEKTSV4RRFFQ69G5FAV";
        let t = TenantPrefix::new(ulid).unwrap_or_else(|e| {
            panic!("a schema name cormex-store really mints was rejected: {e}")
        });
        assert_eq!(
            t.as_str(),
            ulid,
            "a tenant prefix must preserve case verbatim; apply lowercase normalization and the \
             objects already stored can never be retrieved again"
        );
        assert!(
            storage_key(&t, &sha256_hex(b"x"))
                .unwrap()
                .starts_with(ulid),
            "an uppercase ULID must appear verbatim as the key's leading segment"
        );
    }

    /// These characters, used in a schema name, need double quotes everywhere
    /// in SQL, and the store layer has around 77 queries — one missed is a
    /// runtime error. Blocking them at the source is more reliable.
    #[test]
    fn punctuation_that_would_need_sql_quoting_is_rejected() {
        assert_all_rejected(&[
            ("acme-corp", "a hyphen"),
            (
                "acme$1",
                "a dollar sign: Postgres identifiers allow it, but the shell expands it",
            ),
            ("acme.corp", "a dot: also raw material for path traversal"),
            ("acme corp", "a space"),
            (
                "1acme",
                "a leading digit: not permitted in an unquoted Postgres identifier",
            ),
        ]);
    }

    /// Non-ASCII must be refused: filesystems normalize Unicode differently
    /// (APFS applies NFD, most Linux filesystems store it verbatim), so the
    /// same name can land in different directories on different machines, and
    /// the S3 side compares bytes, which is a third behaviour again.
    #[test]
    fn non_ascii_is_rejected() {
        assert_all_rejected(&[
            ("租户", "Chinese characters"),
            (
                "café",
                "a Latin letter with a diacritic: the NFC and NFD encodings look identical",
            ),
        ]);
    }

    #[test]
    fn overlong_prefix_is_rejected() {
        let too_long = "a".repeat(MAX_TENANT_LEN + 1);
        assert!(
            TenantPrefix::new(&too_long).is_err(),
            "beyond {MAX_TENANT_LEN} bytes must be refused: Postgres truncates schema names \
             silently, so two \"different\" tenants may land on the same schema"
        );
    }
}

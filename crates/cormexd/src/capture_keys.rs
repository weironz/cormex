//! Per-file data keys for captured chunks (P6-1).
//!
//! # Why per file, and what it buys
//!
//! Crypto-shredding is the only way to make an append-only object store
//! actually forget something. Content-addressed objects cannot be rewritten in
//! place — that would change their key — so "delete this session" has to mean
//! "make its ciphertext unreadable", and that means the key has to be removable
//! on its own.
//!
//! Per **file** rather than per tenant: one key per file means destroying a
//! session is deleting a handful of rows, while everything else in the tenant
//! keeps working. Per tenant would make one deletion rewrite every object in
//! the account.
//!
//! # What is stored, and what that protects
//!
//! The database holds the data key **wrapped** by `CORMEX_SECRET_KEY`. So a
//! stolen database backup holds no usable key — provided the secret key is not
//! in the same backup (it lives in `.env`, not in the database).
//!
//! ⚠️ **The limitation, stated plainly rather than buried:** an older database
//! backup still contains the wrapped key, and if `CORMEX_SECRET_KEY` has not
//! been rotated, that key still opens the ciphertext from that era. **Until the
//! old backups expire, crypto-shredding is incomplete.** That is a property of
//! backup retention, not a defect in this code, and the operations doc has to
//! say so.
//!
//! # Without `CORMEX_SECRET_KEY`
//!
//! Chunks stay in plaintext, and the startup log and `doctor` say so. **No
//! ephemeral key is ever generated**: that would turn every existing chunk into
//! undecryptable bytes after a restart, and the symptom would be "it worked
//! yesterday, today nothing reads".
//!
//! # Why AES-256-GCM and not something simpler
//!
//! The same reasoning as `byo_key`: GCM authenticates as well as encrypts, so
//! a flipped bit in object storage is a decryption failure rather than silent
//! corruption of somebody's conversation. `ring` is already in the tree.

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom as _, SystemRandom};

/// The deployment's master key. 32 bytes, hex-encoded.
pub const SECRET_KEY_ENV: &str = "CORMEX_SECRET_KEY";

/// A data key, and the id it is stored under.
#[derive(Debug, Clone)]
pub struct DataKey {
    pub id: String,
    key: [u8; 32],
}

impl DataKey {
    /// A fresh random key.
    ///
    /// # Errors
    /// The kernel entropy source is unavailable. Refusing is the right answer:
    /// a predictable key is worse than no encryption, because it looks like
    /// encryption.
    pub fn generate() -> Result<Self, String> {
        let mut key = [0u8; 32];
        SystemRandom::new()
            .fill(&mut key)
            .map_err(|_| "the kernel entropy source is unavailable".to_owned())?;
        Ok(Self {
            id: cormex_core::Id::new().to_string(),
            key,
        })
    }

    /// Rebuild from stored bytes.
    #[must_use]
    pub fn from_bytes(id: String, key: [u8; 32]) -> Self {
        Self { id, key }
    }

    /// The raw key, for wrapping.
    #[must_use]
    pub fn raw(&self) -> &[u8; 32] {
        &self.key
    }

    /// Encrypt: `nonce(12) || ciphertext || tag(16)`.
    ///
    /// # Errors
    /// The entropy source is unavailable, or `ring` refused.
    pub fn seal(&self, plain: &[u8]) -> Result<Vec<u8>, String> {
        let unbound = UnboundKey::new(&AES_256_GCM, &self.key)
            .map_err(|_| "the data key is not usable for AES-256-GCM".to_owned())?;
        let key = LessSafeKey::new(unbound);

        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| "the kernel entropy source is unavailable".to_owned())?;

        let mut buf = plain.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce_bytes),
            Aad::empty(),
            &mut buf,
        )
        .map_err(|_| "encryption failed".to_owned())?;

        let mut out = nonce_bytes.to_vec();
        out.extend_from_slice(&buf);
        Ok(out)
    }

    /// Decrypt.
    ///
    /// # Errors
    /// Too short, or the tag does not verify — which means either the key is
    /// wrong or the bytes were altered.
    pub fn open(&self, blob: &[u8]) -> Result<Vec<u8>, String> {
        if blob.len() <= NONCE_LEN {
            return Err("the ciphertext is too short — that object is corrupt".to_owned());
        }
        let unbound = UnboundKey::new(&AES_256_GCM, &self.key)
            .map_err(|_| "the data key is not usable for AES-256-GCM".to_owned())?;
        let key = LessSafeKey::new(unbound);

        let (nonce_bytes, rest) = blob.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce_bytes
            .try_into()
            .map_err(|_| "the nonce is not 12 bytes".to_owned())?;
        let mut buf = rest.to_vec();
        let plain = key
            .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
            .map_err(|_| {
                "the chunk cannot be decrypted — the data key is gone, or the bytes changed"
                    .to_owned()
            })?;
        Ok(plain.to_vec())
    }
}

/// Wrap a data key with the deployment's master key.
///
/// # Errors
/// `CORMEX_SECRET_KEY` is unset or malformed.
pub fn wrap(data_key: &DataKey) -> Result<Vec<u8>, String> {
    let kek = master_key()?;
    let mut nonce_bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut nonce_bytes)
        .map_err(|_| "the kernel entropy source is unavailable".to_owned())?;
    let mut buf = data_key.raw().to_vec();
    kek.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::empty(),
        &mut buf,
    )
    .map_err(|_| "wrapping the data key failed".to_owned())?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&buf);
    Ok(out)
}

/// Unwrap a stored data key.
///
/// # Errors
/// `CORMEX_SECRET_KEY` is unset, or the wrapped bytes do not verify — the
/// latter almost always means the master key was rotated.
pub fn unwrap(id: &str, wrapped: &[u8]) -> Result<DataKey, String> {
    let kek = master_key()?;
    if wrapped.len() <= NONCE_LEN {
        return Err("the wrapped key is too short — that row is corrupt".to_owned());
    }
    let (nonce_bytes, rest) = wrapped.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .map_err(|_| "the nonce is not 12 bytes".to_owned())?;
    let mut buf = rest.to_vec();
    let plain = kek
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        .map_err(|_| {
            format!(
                "the data key cannot be unwrapped — most likely {SECRET_KEY_ENV} changed. \
                 Chunks written before the change cannot be read"
            )
        })?;
    let key: [u8; 32] = plain
        .try_into()
        .map_err(|_| "the unwrapped key is not 32 bytes".to_owned())?;
    Ok(DataKey::from_bytes(id.to_owned(), key))
}

/// Whether this deployment can encrypt capture chunks at all.
#[must_use]
pub fn enabled() -> bool {
    master_key().is_ok()
}

/// The master key, or an error naming what is wrong with it.
fn master_key() -> Result<LessSafeKey, String> {
    let raw = std::env::var(SECRET_KEY_ENV).map_err(|_| {
        format!("{SECRET_KEY_ENV} is not configured, so capture chunks stay in plaintext")
    })?;
    let bytes =
        hex::decode(raw.trim()).map_err(|_| format!("{SECRET_KEY_ENV} is not valid hex"))?;
    let unbound = UnboundKey::new(&AES_256_GCM, &bytes)
        .map_err(|_| format!("{SECRET_KEY_ENV} must be 32 bytes (64 hex characters)"))?;
    Ok(LessSafeKey::new(unbound))
}

/// The storage key for an encrypted chunk.
///
/// `{tenant}/capture/{key_id}/aa/bb/{sha}.zst.enc`. The key id is in the path
/// so that a reader can find the data key without a second lookup — and so that
/// two chunks under different keys never collide even when their content is
/// identical.
///
/// # Errors
/// The hash or the tenant prefix is malformed.
pub fn encrypted_key(
    tenant: &cormex_blob::hash::TenantPrefix,
    key_id: &str,
    sha256: &str,
) -> Result<String, String> {
    cormex_blob::hash::validate_hash(sha256).map_err(|e| e.to_string())?;
    if key_id.is_empty() || !key_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(format!("the key id {key_id:?} is not a safe path segment"));
    }
    Ok(format!(
        "{tenant}/capture/{key_id}/{}/{}/{sha256}.zst.enc",
        &sha256[0..2],
        &sha256[2..4]
    ))
}

/// Split an encrypted capture key into its key id, if it is one.
///
/// `None` for a plaintext key (`{tenant}/capture/aa/bb/{sha}.zst`), which is
/// what every chunk written before this feature has.
#[must_use]
pub fn key_id_of(storage_key: &str) -> Option<&str> {
    // `.zst.enc`, not just `.enc`: the suffix strip has to leave the bare hash,
    // and stripping only `.enc` leaves `<sha>.zst`
    let rest = storage_key.strip_suffix(".zst.enc")?;
    let mut parts = rest.split('/');
    // tenant / capture / key_id / aa / bb / sha
    let (_, capture, key_id, aa, bb, sha) = (
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
        parts.next()?,
    );
    if parts.next().is_some() || capture != "capture" || sha.len() != 64 {
        return None;
    }
    // The bucket levels must agree with the hash, or the key is malformed
    if !sha.starts_with(aa) || !sha[2..].starts_with(bb) {
        return None;
    }
    Some(key_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> DataKey {
        DataKey::from_bytes("k1".into(), [7u8; 32])
    }

    /// A sealed chunk opens back to the same bytes.
    #[test]
    fn a_sealed_chunk_round_trips() {
        let k = key();
        let plain = b"the original conversation bytes";
        let sealed = k.seal(plain).expect("seal");
        assert_ne!(&sealed[..plain.len().min(8)], &plain[..plain.len().min(8)]);
        assert_eq!(k.open(&sealed).expect("open"), plain);
    }

    /// Two seals of the same plaintext differ.
    ///
    /// GCM with a repeated nonce destroys confidentiality and integrity at
    /// once, so "fresh random each time" is the only approach here that needs
    /// no extra state.
    #[test]
    fn two_seals_of_the_same_bytes_differ() {
        let k = key();
        let a = k.seal(b"same").expect("seal");
        let b = k.seal(b"same").expect("seal");
        assert_ne!(a, b, "the nonce was reused");
    }

    /// The wrong key does not open it.
    #[test]
    fn a_wrong_key_fails() {
        let sealed = key().seal(b"secret").expect("seal");
        let other = DataKey::from_bytes("k2".into(), [9u8; 32]);
        assert!(other.open(&sealed).is_err());
    }

    /// A flipped byte fails rather than returning altered plaintext.
    ///
    /// That is what GCM's tag buys: silent corruption of somebody's
    /// conversation is the failure mode worth paying for.
    #[test]
    fn a_flipped_byte_is_detected() {
        let k = key();
        let mut sealed = k.seal(b"secret").expect("seal");
        let last = sealed.len() - 1;
        sealed[last] ^= 0x01;
        assert!(k.open(&sealed).is_err());
    }

    /// The encrypted key carries the key id and the hash buckets.
    #[test]
    fn the_encrypted_key_names_the_key_and_the_buckets() {
        let tenant = cormex_blob::hash::TenantPrefix::new("public").expect("a legal tenant");
        let sha = "ab".to_owned() + &"cd".repeat(31);
        let key = encrypted_key(&tenant, "01ABC", &sha).expect("key");
        assert_eq!(key, format!("public/capture/01ABC/ab/cd/{sha}.zst.enc"));
        assert_eq!(key_id_of(&key), Some("01ABC"));
    }

    /// A plaintext key has no key id.
    #[test]
    fn a_plaintext_key_has_no_key_id() {
        let sha = "ab".to_owned() + &"cd".repeat(31);
        assert_eq!(key_id_of(&format!("public/capture/ab/cd/{sha}.zst")), None);
    }

    /// A key whose buckets disagree with its hash is not one of ours.
    #[test]
    fn a_mismatched_bucket_is_rejected() {
        let sha = "ab".to_owned() + &"cd".repeat(31);
        assert_eq!(
            key_id_of(&format!("public/capture/k1/zz/cd/{sha}.zst.enc")),
            None
        );
    }

    /// An unsafe key id is refused rather than spliced into a path.
    #[test]
    fn an_unsafe_key_id_is_refused() {
        let tenant = cormex_blob::hash::TenantPrefix::new("public").expect("a legal tenant");
        let sha = "ab".to_owned() + &"cd".repeat(31);
        assert!(encrypted_key(&tenant, "../../etc", &sha).is_err());
    }
}

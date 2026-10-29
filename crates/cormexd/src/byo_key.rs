//! Bring your own API key — use your own key, and it does not count against the
//! quota.
//!
//! # This module exists to pay off a debt
//!
//! The over-quota message told the user they "can enter their own API key in
//! settings", and that entry point **did not exist**. A promise already
//! delivered to the user's face is worse than not having the feature at all: it
//! sends someone who is currently blocked hunting for a settings item that is
//! simply not there.
//!
//! # Encryption
//!
//! The key comes from the deployment's `CORMEX_SECRET_KEY` (32 bytes, hex) and
//! **never enters the database**. So a stolen backup contains no usable key —
//! that is the entire point of encrypted storage, and it is also why the
//! endpoint answers 501 when the variable is unset rather than storing the key
//! in plaintext: someone who believes they are protected by encryption is more
//! dangerous than someone who knows there is no protection.
//!
//! Uses AES-256-GCM (`ring`). The nonce is freshly random each time and
//! concatenated with the ciphertext — reusing a nonce under GCM destroys
//! confidentiality and integrity simultaneously, and "fresh random each time" is
//! the only approach here that requires no extra state.

use axum::{Json, extract::State};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use serde::{Deserialize, Serialize};

use crate::routes::ApiError;
use crate::state::AppState;

/// The deployment's master key. 32 bytes, hex-encoded.
const SECRET_KEY_ENV: &str = "CORMEX_SECRET_KEY";

/// Fetch the master key.
///
/// # Errors
/// Not configured, or not 32 bytes of hex. **It never generates an ephemeral
/// one** — that would turn every stored key into undecryptable bytes after a
/// restart, and the symptom would be "everything worked yesterday, today every
/// call fails".
fn kek() -> Result<LessSafeKey, ApiError> {
    let raw = std::env::var(SECRET_KEY_ENV).map_err(|_| {
        ApiError::unsupported(format!(
            "this deployment has no {SECRET_KEY_ENV} configured, so API keys cannot be \
             stored. An administrator can generate one with `openssl rand -hex 32` and \
             put it in .env"
        ))
    })?;
    let bytes = hex::decode(raw.trim())
        .map_err(|_| ApiError::internal(format!("{SECRET_KEY_ENV} is not valid hex")))?;
    let unbound = UnboundKey::new(&AES_256_GCM, &bytes).map_err(|_| {
        ApiError::internal(format!(
            "{SECRET_KEY_ENV} must be 32 bytes (64 hex characters)"
        ))
    })?;
    Ok(LessSafeKey::new(unbound))
}

/// Encrypt: `nonce(12) || ciphertext`.
fn seal(plain: &str) -> Result<Vec<u8>, ApiError> {
    seal_with(&kek()?, plain)
}

/// Same as above, but the caller supplies the key.
///
/// Split out so that **tests never have to touch environment variables**:
/// `CORMEX_SECRET_KEY` is process-global while cargo runs tests in parallel by
/// default — four tests each calling set_var would swap each other's keys out,
/// and the symptom is random failures. Actually observed.
fn seal_with(key: &LessSafeKey, plain: &str) -> Result<Vec<u8>, ApiError> {
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes).map_err(|_| {
        ApiError::internal(
            "the kernel entropy source is unavailable; refusing to encrypt \
         with a predictable nonce",
        )
    })?;
    let mut buf = plain.as_bytes().to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce_bytes),
        Aad::empty(),
        &mut buf,
    )
    .map_err(|_| ApiError::internal("encryption failed"))?;
    let mut out = nonce_bytes.to_vec();
    out.extend_from_slice(&buf);
    Ok(out)
}

/// Decrypt.
fn open(blob: &[u8]) -> Result<String, ApiError> {
    open_with(&kek()?, blob)
}

/// Same as above, but the caller supplies the key. Reasoning in [`seal_with`].
fn open_with(key: &LessSafeKey, blob: &[u8]) -> Result<String, ApiError> {
    if blob.len() <= NONCE_LEN {
        return Err(ApiError::internal(
            "the ciphertext is too short — that row in the database is corrupt",
        ));
    }
    let (nonce_bytes, rest) = blob.split_at(NONCE_LEN);
    let nonce: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .expect("we split at NONCE_LEN one line ago");
    let mut buf = rest.to_vec();
    let plain = key
        .open_in_place(Nonce::assume_unique_for_key(nonce), Aad::empty(), &mut buf)
        .map_err(|_| {
            // The most likely cause is that the master key was rotated. Say so,
            // or this error reads like a corrupted database
            ApiError::internal(format!(
                "the stored API key cannot be decrypted — most likely {SECRET_KEY_ENV} \
                 changed. Entering it again is all it takes"
            ))
        })?;
    String::from_utf8(plain.to_vec())
        .map_err(|_| ApiError::internal("what came out is not valid UTF-8"))
}

/// The stored key, already decrypted.
pub struct OwnKey {
    pub provider: String,
    pub api_key: String,
    /// The user's own endpoint. `None` = use the one from the built-in
    /// definition (the official endpoint).
    ///
    /// A person's "own key" is rarely the official one — far more often it is a
    /// corporate gateway, a one-api / LiteLLM relay, or some cheaper compatible
    /// service. Storing the key without being able to store the endpoint leaves
    /// that key still unable to reach the place it belongs to
    pub base_url: Option<String>,
}

#[derive(Deserialize)]
pub struct SetKeyRequest {
    /// Provider id, matching the provider names in `cormex-llm`
    pub provider: String,
    pub api_key: String,
    /// Optional: the user's own endpoint. Empty means use the built-in one
    #[serde(default)]
    pub base_url: Option<String>,
}

/// The shape sent down to the UI — **the tail only**; the plaintext is never
/// returned.
///
/// Returning the plaintext would put this key into the browser cache, the logs,
/// and every occasion where somebody pastes a request for someone else to look
/// at. A user who wants to change keys simply enters a new one.
#[derive(Serialize)]
pub struct KeyStatus {
    pub configured: bool,
    pub provider: Option<String>,
    /// The last 4 characters of the plaintext key, so the user can tell which
    /// key is stored
    pub key_tail: Option<String>,
    /// The self-hosted endpoint that was entered. `None` when none was (the
    /// official endpoint is used)
    pub base_url: Option<String>,
    pub updated_at: Option<String>,
    /// Whether this deployment supports storing a bring-your-own key (false
    /// when no master key is configured)
    pub supported: bool,
}

impl AppState {
    /// The bring-your-own key stored for this tenant. `None` when there is none.
    ///
    /// **Any failure is treated as "there is none" and logged**: a
    /// bring-your-own key is an optimization path, and falling back to the
    /// server's key when it cannot be read is safe (it merely counts against the
    /// quota), whereas failing the whole conversation is not.
    pub async fn own_key(&self, tenant: &crate::request_tenant::Tenant) -> Option<OwnKey> {
        let store = tenant.store().ok()?;
        let row = sqlx::query_as::<_, (String, Option<Vec<u8>>, bool, Option<String>)>(
            "SELECT provider, ciphertext, deleted, base_url
               FROM llm_keys ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(store.pool())
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not read the bring-your-own key; this turn uses the server's key");
            None
        })?;

        let (provider, ciphertext, deleted, base_url) = row;
        if deleted {
            return None;
        }
        match open(&ciphertext?) {
            Ok(api_key) => Some(OwnKey {
                provider,
                api_key,
                base_url,
            }),
            Err(e) => {
                tracing::warn!(error = ?e, "the bring-your-own key would not decrypt; this turn uses \
                 the server's key");
                None
            }
        }
    }
}

/// `GET /settings/llm-key` — whether one was entered, and which one.
///
/// # Errors
/// The database could not be queried.
pub async fn get(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<KeyStatus>, ApiError> {
    let supported = kek().is_ok();
    let tenant = st.tenant(&headers).await?;
    let Ok(store) = tenant.store() else {
        return Ok(Json(KeyStatus {
            configured: false,
            provider: None,
            key_tail: None,
            base_url: None,
            updated_at: None,
            supported,
        }));
    };

    let row = sqlx::query_as::<
        _,
        (
            String,
            Option<String>,
            bool,
            chrono::DateTime<chrono::Utc>,
            Option<String>,
        ),
    >(
        "SELECT provider, key_tail, deleted, created_at, base_url
           FROM llm_keys ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_optional(store.pool())
    .await
    .map_err(|e| {
        ApiError::internal(format!("could not read the bring-your-own key status: {e}"))
    })?;

    Ok(Json(match row {
        Some((provider, key_tail, false, at, base_url)) => KeyStatus {
            configured: true,
            provider: Some(provider),
            key_tail,
            base_url,
            updated_at: Some(at.to_rfc3339()),
            supported,
        },
        _ => KeyStatus {
            configured: false,
            provider: None,
            key_tail: None,
            base_url: None,
            updated_at: None,
            supported,
        },
    }))
}

/// `PUT /settings/llm-key` — store your own key.
///
/// # Errors
/// The deployment has no master key configured (501), the provider name is
/// unrecognised, or the write failed.
pub async fn put(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SetKeyRequest>,
) -> Result<Json<KeyStatus>, ApiError> {
    let api_key = req.api_key.trim();
    if api_key.is_empty() {
        return Err(ApiError::bad_request("the API key must not be empty"));
    }
    // Validate the provider name right here. Store a misspelled one and the
    // symptom does not appear until the next conversation — by which point the
    // user no longer remembers what they typed
    if !cormex_llm::provider::available()
        .iter()
        .any(|a| a == &req.provider)
    {
        return Err(ApiError::bad_request(format!(
            "unrecognised provider {}; available: {}",
            req.provider,
            cormex_llm::provider::available().join(" / ")
        )));
    }

    let ciphertext = seal(api_key)?;
    let tail: String = api_key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let tenant = st.tenant(&headers).await?;
    let store = tenant.store().map_err(|e| {
        ApiError::unsupported(format!(
            "this deployment cannot store a bring-your-own key: {e}"
        ))
    })?;

    sqlx::query(
        "INSERT INTO llm_keys (id, provider, ciphertext, key_tail, deleted, base_url)
         VALUES ($1, $2, $3, $4, false, $5)",
    )
    .bind(cormex_core::Id::new().to_string())
    .bind(&req.provider)
    .bind(&ciphertext)
    .bind(&tail)
    .bind(
        req.base_url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty()),
    )
    .execute(store.pool())
    .await
    .map_err(|e| ApiError::internal(format!("could not store it: {e}")))?;

    tracing::info!(provider = %req.provider, "the user stored a bring-your-own API key");
    get(State(st), headers).await
}

/// `DELETE /settings/llm-key` — stop using the bring-your-own key.
///
/// Appends a tombstone row rather than deleting the existing one: consistent
/// with the database-wide append-only rule, and "when was it withdrawn" is
/// useful during diagnosis.
///
/// # Errors
/// The write failed.
pub async fn delete(
    State(st): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<KeyStatus>, ApiError> {
    let tenant = st.tenant(&headers).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::unsupported(e.to_string()))?;

    sqlx::query(
        "INSERT INTO llm_keys (id, provider, ciphertext, key_tail, deleted)
         VALUES ($1, '', NULL, NULL, true)",
    )
    .bind(cormex_core::Id::new().to_string())
    .execute(store.pool())
    .await
    .map_err(|e| ApiError::internal(format!("could not revoke it: {e}")))?;

    tracing::info!("the user removed their bring-your-own API key");
    get(State(st), headers).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a test key. **Touches no environment variable** — see the comment
    /// on [`seal_with`].
    fn test_key(byte: u8) -> LessSafeKey {
        LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &[byte; 32]).expect("32 bytes"))
    }

    /// Encrypting and then decrypting must return the original text.
    #[test]
    fn a_key_survives_the_round_trip() {
        let k = test_key(0x11);
        let secret = "sk-deadbeef-0123456789";
        let blob = seal_with(&k, secret).expect("encrypt");
        assert!(
            !blob.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "the plaintext can be found verbatim inside the ciphertext — no encryption happened \
             at all"
        );
        assert_eq!(open_with(&k, &blob).expect("decrypt"), secret);
    }

    /// Encrypting the same plaintext twice **must not** produce the same
    /// ciphertext.
    ///
    /// Identical output means the nonce was reused, and reusing a nonce under
    /// GCM destroys confidentiality and integrity simultaneously — the single
    /// most classic way to get this kind of implementation wrong.
    #[test]
    fn the_same_secret_never_encrypts_to_the_same_bytes() {
        let k = test_key(0x22);
        let a = seal_with(&k, "sk-same").expect("first encryption");
        let b = seal_with(&k, "sk-same").expect("second encryption");
        assert_ne!(
            a, b,
            "two encryptions produced identical output — the nonce was reused"
        );
    }

    /// A rotated master key must fail to decrypt, and **must not** yield garbage
    /// that then gets used as a key.
    #[test]
    fn a_rotated_master_key_fails_instead_of_returning_garbage() {
        let blob = seal_with(&test_key(0x33), "sk-rotate-me").expect("encrypt");
        assert!(
            open_with(&test_key(0x44), &blob).is_err(),
            "a different key managed to decrypt it — GCM's integrity check is not working, which \
             means a tampered ciphertext would pass too"
        );
    }

    /// A tampered ciphertext must fail to decrypt (the integrity half of AEAD).
    #[test]
    fn a_tampered_ciphertext_is_rejected() {
        let k = test_key(0x55);
        let mut blob = seal_with(&k, "sk-tamper").expect("encrypt");
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(
            open_with(&k, &blob).is_err(),
            "flipping one bit still decrypted — that means this is not AEAD, and the row in the \
             database is replaceable by anyone who can write to it"
        );
    }

    /// With no master key configured, **nothing is stored in plaintext**; the
    /// endpoint simply says it is unsupported.
    ///
    /// This one reads an environment variable, so it deliberately asserts only
    /// the single fact "it is an error when the key cannot be read" and never
    /// calls set_var — other tests running in parallel must not be disturbed by
    /// it.
    #[test]
    fn without_a_master_key_there_is_no_storage_at_all() {
        if std::env::var(SECRET_KEY_ENV).is_ok() {
            // The development machine's .env sets it. What this test asserts
            // does not hold here, so skip
            return;
        }
        assert!(
            seal("sk-plain").is_err(),
            "encryption succeeded without a master key — the only way that happens is by storing \
             the plaintext, and someone who believes they are protected by encryption is more \
             dangerous than someone who knows there is no protection"
        );
    }
}

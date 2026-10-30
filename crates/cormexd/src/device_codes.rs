//! Device-code login: get a key onto a machine that has no browser (P6-4).
//!
//! # The problem this solves
//!
//! `cormex login` on a headless box has no way to show a password prompt that
//! anybody would trust, and pasting a long-lived token into a shell is exactly
//! the thing tokens exist to avoid. The device-code flow (RFC 8628, the same
//! shape `gh auth login` and `docker login` use) turns it into: the machine
//! shows a short code, the person types it into a browser **they are already
//! logged into**, and the machine polls until a key appears.
//!
//! # Why the codes live in memory
//!
//! A pending authorization is worthless after ten minutes and there is nothing
//! to audit about one that never completed. Putting it in Postgres would mean a
//! table whose rows are deleted after a minute — and DELETE is permitted only
//! in purge. A `HashMap` behind a mutex is the right size for the data: a few
//! entries, ten-minute lifetime, lost on restart (which is correct — a restart
//! should invalidate them).
//!
//! # Why the two codes are different
//!
//! - `user_code` is **short and typeable** (`ABCD-EFGH`), and is what the person
//!   reads off the machine's screen
//! - `device_code` is a **long random secret**, polled by the machine
//!
//! They must not be the same thing: the user code is spoken aloud and shown in
//! a terminal where anybody looking over a shoulder can read it. If it were
//! also the polling credential, that glance would be enough to steal the
//! resulting key.
//!
//! # Why polling is rate-limited rather than long-polled
//!
//! RFC 8628's `interval` exists because a client that polls in a tight loop is
//! a client that can be used to brute-force a `user_code`. Five seconds is the
//! spec's suggested default and is what the CLI uses.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use cormex_core::Id;

/// How long a pending authorization lives.
pub const CODE_TTL: Duration = Duration::from_secs(600);

/// The minimum interval between polls, in seconds. RFC 8628's default.
pub const POLL_INTERVAL_SECS: u64 = 5;

/// The alphabet a user code is drawn from.
///
/// Crockford base32 minus `I`, `L`, `O`, `U`: the first three because they are
/// read as 1 / 1 / 0, and `U` because it makes accidental obscenities likelier
/// than it needs to be. This is the same reasoning as ULID's alphabet, and the
/// same set.
const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// One pending authorization.
#[derive(Debug, Clone)]
pub struct Pending {
    pub user_code: String,
    pub device_code: String,
    /// The scopes the machine asked for. The person confirming sees these.
    pub scopes: Vec<String>,
    /// What the machine calls itself, shown on the confirmation screen.
    pub label: String,
    pub platform: Option<String>,
    pub created_at: Instant,
    /// Set once somebody confirmed. `None` means still waiting.
    pub approved_by: Option<String>,
    /// The key, once issued. Taken out on the first successful poll, so a
    /// second poll cannot get a second copy.
    pub issued: Option<cormex_proto::devices::DeviceKeyIssued>,
}

/// The in-memory table of pending authorizations.
///
/// Behind one mutex rather than a sharded map: the whole table is a handful of
/// entries, and a lock held for a `HashMap` insert is not a contention point.
#[derive(Default)]
pub struct DeviceCodes {
    pending: Mutex<HashMap<String, Pending>>,
}

impl std::fmt::Debug for DeviceCodes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written: the map holds device codes, which are credentials for
        // the ten minutes they live
        let n = self.pending.lock().map(|p| p.len()).unwrap_or(0);
        f.debug_struct("DeviceCodes").field("pending", &n).finish()
    }
}

/// The wire types live in `cormex-proto`, beside every other DTO: the CLI and
/// the server must agree on the shape, and two copies of it is one copy that
/// drifts.
pub use cormex_proto::devices::{AuthorizationStarted, TokenPoll};

impl DeviceCodes {
    /// Start an authorization.
    ///
    /// # Errors
    /// Never in practice; the signature is `Result`-free so the handler stays
    /// flat.
    #[must_use]
    pub fn start(
        &self,
        label: String,
        scopes: Vec<String>,
        platform: Option<String>,
    ) -> AuthorizationStarted {
        let user_code = random_user_code();
        let device_code = Id::new().to_string() + &Id::new().to_string();
        let mut map = self
            .pending
            .lock()
            .expect("the device-code lock is not poisoned");
        // Expired entries are swept on every start rather than by a timer: the
        // table is tiny, and a background task for it would be a task whose
        // only job is to delete a few strings
        map.retain(|_, p| p.created_at.elapsed() < CODE_TTL);
        map.insert(
            user_code.clone(),
            Pending {
                user_code: user_code.clone(),
                device_code: device_code.clone(),
                scopes,
                label,
                platform,
                created_at: Instant::now(),
                approved_by: None,
                issued: None,
            },
        );
        AuthorizationStarted {
            user_code,
            device_code,
            verification_uri: "/devices/authorize".to_owned(),
            expires_in: CODE_TTL.as_secs(),
            interval: POLL_INTERVAL_SECS,
        }
    }

    /// Look up a pending entry by its **user** code.
    ///
    /// Used by the confirmation screen, where the person typed the short code.
    #[must_use]
    pub fn by_user_code(&self, user_code: &str) -> Option<Pending> {
        let normalized = normalize_user_code(user_code);
        let map = self
            .pending
            .lock()
            .expect("the device-code lock is not poisoned");
        map.get(&normalized)
            .filter(|p| p.created_at.elapsed() < CODE_TTL)
            .cloned()
    }

    /// Mark an entry approved. Returns `false` when the code is unknown or
    /// expired.
    pub fn approve(&self, user_code: &str, user_id: &str) -> bool {
        let normalized = normalize_user_code(user_code);
        let mut map = self
            .pending
            .lock()
            .expect("the device-code lock is not poisoned");
        match map.get_mut(&normalized) {
            Some(p) if p.created_at.elapsed() < CODE_TTL => {
                p.approved_by = Some(user_id.to_owned());
                true
            }
            _ => false,
        }
    }

    /// Poll with the **device** code.
    ///
    /// # Why the issued key is taken out
    ///
    /// A second successful poll must not hand out a second copy: the first
    /// response is the only one the machine is guaranteed to have received, and
    /// a replayed poll from a network attacker would otherwise get a working
    /// key for free.
    #[must_use]
    pub fn poll(&self, device_code: &str) -> TokenPoll {
        let mut map = self
            .pending
            .lock()
            .expect("the device-code lock is not poisoned");
        let Some((code, entry)) = map.iter_mut().find(|(_, p)| p.device_code == device_code) else {
            return TokenPoll::Expired;
        };
        if entry.created_at.elapsed() >= CODE_TTL {
            let code = code.clone();
            map.remove(&code);
            return TokenPoll::Expired;
        }
        if entry.approved_by.is_none() {
            return TokenPoll::Pending;
        }
        match entry.issued.take() {
            Some(key) => {
                let code = code.clone();
                map.remove(&code);
                TokenPoll::Issued { key }
            }
            // Approved but the key has already been collected once
            None => TokenPoll::Expired,
        }
    }

    /// Put the issued key in, after the handler has minted it.
    pub fn set_issued(&self, user_code: &str, key: cormex_proto::devices::DeviceKeyIssued) {
        let mut map = self
            .pending
            .lock()
            .expect("the device-code lock is not poisoned");
        if let Some(p) = map.get_mut(user_code) {
            p.issued = Some(key);
        }
    }

    /// How many entries are waiting. For `/health` and the tests.
    #[must_use]
    pub fn len(&self) -> usize {
        self.pending
            .lock()
            .map(|p| {
                p.values()
                    .filter(|e| e.created_at.elapsed() < CODE_TTL)
                    .count()
            })
            .unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A fresh `XXXX-XXXX` user code.
fn random_user_code() -> String {
    let mut bytes = [0u8; 8];
    // `getrandom` rather than a counter: two servers behind a load balancer
    // would hand out the same sequence, and a user code is a capability for the
    // ten minutes it lives
    if getrandom::fill(&mut bytes).is_err() {
        // Falling back to the clock would make codes guessable, which is worse
        // than failing to start an authorization
        panic!("the kernel entropy source is unavailable; refusing to mint a guessable code");
    }
    let chars: Vec<char> = bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
        .collect();
    format!(
        "{}{}{}{}-{}{}{}{}",
        chars[0], chars[1], chars[2], chars[3], chars[4], chars[5], chars[6], chars[7]
    )
}

/// Normalize what a person typed: case-insensitive, dashes and spaces ignored.
///
/// People type `abcd efgh`, `ABCD-EFGH` and `abcdefgh`. Rejecting two of the
/// three would make the flow fail for reasons the person cannot see.
#[must_use]
pub fn normalize_user_code(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    if cleaned.len() == 8 {
        format!("{}-{}", &cleaned[..4], &cleaned[4..])
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes() -> DeviceCodes {
        DeviceCodes::default()
    }

    /// A started authorization polls as pending until approved.
    #[test]
    fn a_fresh_code_polls_as_pending() {
        let c = codes();
        let started = c.start("laptop".into(), vec!["capture:write".into()], None);
        assert!(matches!(c.poll(&started.device_code), TokenPoll::Pending));
        assert_eq!(c.len(), 1);
    }

    /// After approval the key comes out **once**.
    ///
    /// A second poll must not hand out a second copy: the first response is the
    /// only one the machine is guaranteed to have received.
    #[test]
    fn an_approved_code_issues_once() {
        let c = codes();
        let started = c.start("laptop".into(), vec![], None);
        assert!(c.approve(&started.user_code, "user-1"));
        c.set_issued(
            &started.user_code,
            cormex_proto::devices::DeviceKeyIssued {
                id: "k1".into(),
                key: "secret".into(),
                scopes: vec![],
            },
        );
        let first = c.poll(&started.device_code);
        assert!(matches!(first, TokenPoll::Issued { .. }), "{first:?}");
        assert!(
            matches!(c.poll(&started.device_code), TokenPoll::Expired),
            "the key was handed out twice"
        );
    }

    /// An unknown device code is expired, not pending.
    #[test]
    fn an_unknown_code_is_expired() {
        assert!(matches!(codes().poll("nope"), TokenPoll::Expired));
    }

    /// The user code is typeable: 8 characters from the unambiguous alphabet,
    /// in two groups of four.
    #[test]
    fn the_user_code_is_typeable() {
        let c = codes();
        let started = c.start("laptop".into(), vec![], None);
        let code = started.user_code;
        assert_eq!(code.len(), 9, "the code should be XXXX-XXXX: {code}");
        assert_eq!(code.chars().nth(4), Some('-'));
        for ch in code.chars().filter(|c| *c != '-') {
            assert!(
                ALPHABET.contains(&(ch as u8)),
                "{ch:?} is not in the unambiguous alphabet"
            );
        }
    }

    /// The user code and the device code are different values.
    ///
    /// The user code is read off a screen where anybody can see it; if it were
    /// also the polling credential, that glance would be enough to steal the
    /// key.
    #[test]
    fn the_two_codes_are_not_the_same() {
        let c = codes();
        let started = c.start("laptop".into(), vec![], None);
        assert_ne!(started.user_code, started.device_code);
        assert!(
            started.device_code.len() > started.user_code.len(),
            "the device code should be the long one"
        );
    }

    /// Normalization accepts what a person actually types.
    #[test]
    fn the_user_code_is_normalized() {
        assert_eq!(normalize_user_code("abcd-efgh"), "ABCD-EFGH");
        assert_eq!(normalize_user_code("ABCD EFGH"), "ABCD-EFGH");
        assert_eq!(normalize_user_code("abcdefgh"), "ABCD-EFGH");
        assert_eq!(normalize_user_code("  ABCD-EFGH  "), "ABCD-EFGH");
    }

    /// Approving an unknown code is a no-op, not a panic.
    #[test]
    fn approving_an_unknown_code_is_refused() {
        assert!(!codes().approve("ZZZZ-ZZZZ", "user-1"));
    }

    /// Two authorizations get different codes.
    #[test]
    fn two_authorizations_differ() {
        let c = codes();
        let a = c.start("a".into(), vec![], None);
        let b = c.start("b".into(), vec![], None);
        assert_ne!(a.user_code, b.user_code);
        assert_ne!(a.device_code, b.device_code);
        assert_eq!(c.len(), 2);
    }
}
#[cfg(test)]
mod flow_tests {
    //! The whole flow over HTTP, against a real database.
    //!
    //! The unit tests above drive the table directly; what they cannot show is
    //! that the **routing** is right — that `authorize` and `token` are
    //! anonymous, that `confirm` is not, and that a key minted through this
    //! path actually works.

    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode, header};
    use tower::ServiceExt as _;

    async fn post(
        app: &axum::Router,
        path: &str,
        bearer: Option<&str>,
        body: serde_json::Value,
    ) -> (StatusCode, Vec<u8>) {
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(t) = bearer {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = app
            .clone()
            .oneshot(req.body(Body::from(body.to_string())).expect("request"))
            .await
            .expect("infallible");
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .expect("body")
            .to_vec();
        (status, bytes)
    }

    /// The full flow: authorize → confirm → poll, and the key works.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_device_code_login_issues_a_working_key() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt) = crate::test_support::live_state(&db.url).await;
        let app = crate::routes::router(st.clone());

        // A user to confirm with
        let user = st
            .create_account("device-code-user", "correct horse battery")
            .await
            .expect("create the account");
        let token = st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));

        // ── The machine, with no credential ──
        let (status, body) = post(
            &app,
            "/devices/authorize",
            None,
            serde_json::json!({"label": "headless-box", "scopes": ["capture:write"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let started: AuthorizationStarted =
            serde_json::from_slice(&body).expect("an AuthorizationStarted");

        // Polling before confirmation is pending
        let (status, body) = post(
            &app,
            "/devices/token",
            None,
            serde_json::json!({"device_code": started.device_code}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let poll: TokenPoll = serde_json::from_slice(&body).expect("a TokenPoll");
        assert!(matches!(poll, TokenPoll::Pending), "{poll:?}");

        // ── Confirming requires a credential ──
        let (status, _) = post(
            &app,
            "/devices/confirm",
            None,
            serde_json::json!({"user_code": started.user_code}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "an anonymous caller confirmed a device login"
        );

        // ── The person confirms in the browser ──
        let (status, body) = post(
            &app,
            "/devices/confirm",
            Some(&token),
            serde_json::json!({"user_code": started.user_code}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

        // ── The machine polls again and gets the key ──
        let (status, body) = post(
            &app,
            "/devices/token",
            None,
            serde_json::json!({"device_code": started.device_code}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let poll: TokenPoll = serde_json::from_slice(&body).expect("a TokenPoll");
        let TokenPoll::Issued { key } = poll else {
            panic!("the key was not issued after confirmation: {poll:?}");
        };
        assert!(!key.key.is_empty());

        // ── And the key actually works on a capture endpoint ──
        let req = Request::builder()
            .method(Method::GET)
            .uri("/capture/catalog")
            .header(header::AUTHORIZATION, format!("Bearer {}", key.key))
            .body(Body::empty())
            .expect("request");
        let resp = app.clone().oneshot(req).await.expect("infallible");
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "the key issued through the device-code flow does not work"
        );

        // A second poll does not hand out a second copy
        let (_, body) = post(
            &app,
            "/devices/token",
            None,
            serde_json::json!({"device_code": started.device_code}),
        )
        .await;
        let poll: TokenPoll = serde_json::from_slice(&body).expect("a TokenPoll");
        assert!(
            matches!(poll, TokenPoll::Expired),
            "the key was handed out twice: {poll:?}"
        );

        db.drop().await;
    }

    /// A device key cannot confirm another device's login.
    ///
    /// Otherwise a compromised machine could approve its own next key, which is
    /// the one thing the scope list exists to prevent.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_device_key_cannot_confirm() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt) = crate::test_support::live_state(&db.url).await;
        let app = crate::routes::router(st.clone());

        let user = st
            .create_account("device-code-user-2", "correct horse battery")
            .await
            .expect("create the account");
        let token = st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));
        let issued: cormex_proto::devices::DeviceKeyIssued = {
            let (_, body) = post(
                &app,
                "/devices",
                Some(&token),
                serde_json::json!({"label": "existing", "scopes": ["capture:write"]}),
            )
            .await;
            serde_json::from_slice(&body).expect("a key")
        };

        let (_, body) = post(
            &app,
            "/devices/authorize",
            None,
            serde_json::json!({"label": "new-box", "scopes": ["capture:write"]}),
        )
        .await;
        let started: AuthorizationStarted =
            serde_json::from_slice(&body).expect("an AuthorizationStarted");

        let (status, _) = post(
            &app,
            "/devices/confirm",
            Some(&issued.key),
            serde_json::json!({"user_code": started.user_code}),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a device key approved another device's login"
        );

        db.drop().await;
    }
}

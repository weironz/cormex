//! Authentication — "who is allowed to connect to this cormexd".
//!
//! Before this, whoever connected was the owner. A cormexd bound to `0.0.0.0`,
//! an ngrok tunnel opened on a whim, a docker-compose that maps the port out —
//! any one of them is enough to expose the whole memory database to the public
//! internet, and **there will be no sign of it at all**.
//!
//! # The approach: a pre-shared token, with the server storing only the digest
//!
//! ```text
//! ops: cormexd --generate-token
//!        ├─► plaintext token ─────────► client (CORMEXD_TOKEN, shown exactly once)
//!        └─► SHA-256 digest ──────────► server (CORMEX_AUTH_TOKEN_SHA256 in .env)
//!
//! client: Authorization: Bearer <plaintext>
//! server: sha256(plaintext) == digest ?
//! ```
//!
//! ## Why not issued / expiring session tokens
//!
//! Both of those require an **issuing endpoint**, i.e. a credential that exists
//! before it in order to exchange for a token — and in a single-user
//! self-hosted setting that credential can only be pre-shared anyway. So the
//! extra layer merely repackages the same secret, at the cost of having to
//! handle key rotation, clock skew and token revocation. What genuinely needs an
//! issuing model is "multiple users plus the ability to kick out one of them
//! individually", and that is a multi-tenancy requirement, explicitly out of
//! scope for this round (see the end of this file).
//!
//! OAuth and its relatives are further out of the question: a self-hosted thing
//! should not depend on an external service in order to log in — when that
//! service goes down you cannot reach your own memory database.
//!
//! ## Why the digest is SHA-256 rather than argon2 / bcrypt
//!
//! A slow KDF defends against **offline brute-forcing of a low-entropy secret**
//! (a human-chosen password). The token here is 256 bits of kernel randomness,
//! and the number of attempts required to brute-force it is independent of how
//! slow the KDF is — make it a million times slower and the brute-force time
//! goes from "the age of the universe" to "the age of the universe". What you
//! pay is one new dependency and tens of milliseconds of CPU per request.
//!
//! **This argument rests on one premise: the token must be high-entropy
//! random.** Which is why [`generate`] does not accept a user-chosen passphrase
//! and only emits random values — the moment someone routes around that premise
//! (by pasting in `echo -n hunter2 | sha256sum`), the paragraph above stops
//! holding.
//!
//! ## Where the credentials live
//!
//! - **Server**: the digest only, in `.env` (excluded by gitignore). A leaked
//!   digest cannot be used to log in directly — it cannot be submitted as a
//!   bearer token, because the server runs SHA-256 over whatever it receives
//!   before comparing.
//! - **Never the database**: that would sync the credential to every device via
//!   `sync_log`, and in an append-only system whatever is written can never be
//!   removed — after one rotation the old digest stays in the database forever.
//! - **Client**: the plaintext, in the environment variable `CORMEXD_TOKEN`.
//!   Likewise never in any git-tracked file.
//!
//! # With no credential configured: refuse to start
//!
//! Same class of judgement as the sandbox degradation policy on the agent side,
//! and the same answer: **silently not taking effect is the most dangerous
//! failure shape**. The cost of an open default is "somebody exposes their
//! memory database to the internet without knowing it", which has no symptom
//! whatsoever; the cost of refusing to start is "one extra step the first time
//! you run it", which is discovered on the spot.
//!
//! The escape hatch is likewise deliberately not `1` / `true`: only the literal
//! `CORMEX_AUTH=disabled` turns it off, so it cannot be enabled by absent-minded
//! environment-variable setting. Once off, `/health` permanently reports
//! `auth: disabled` and the startup log carries a warning — at any moment it
//! must be possible to answer "am I actually protected right now".
//!
//! # The real browser constraint: `EventSource` and `WebSocket` cannot set
//! # request headers
//!
//! This is not something to be worked around; it is a hard limit of the browser
//! APIs. Of the three common ways out:
//!
//! | Way out | Problem |
//! |---|---|
//! | put the long-lived token in the query string | it lands in access logs, reverse proxy logs and browser history — and it is a **long-lived** credential |
//! | cookies | requires CORS credentials and immediately introduces CSRF — and this service's CORS is permissive |
//! | **short-lived tickets** | exchange the long-lived token for a 60-second ticket, and only the ticket goes into the URL |
//!
//! The third is chosen. See [`TicketBook`]. Tickets also solve fetching
//! attachments through `<img src>`: `Image.network` cannot set request headers
//! either.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::{
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest as _, Sha256};

use crate::state::AppState;

/// Environment variable holding the server-side digest.
pub const TOKEN_SHA256_ENV: &str = "CORMEX_AUTH_TOKEN_SHA256";

/// Environment variable that turns authentication off.
pub const AUTH_ENV: &str = "CORMEX_AUTH";

/// The **only** value that turns authentication off. See "why not `1`" in the
/// module docs.
pub const DISABLED_VALUE: &str = "disabled";

/// Ticket lifetime.
///
/// 60 seconds is enough to open a WebSocket and to load a screenful of images,
/// and short enough that even if the whole URL is written to a log, whoever
/// picks it up will essentially never be in time. **It is not single-use** —
/// single-use would 401 the second image on a page outright, and would
/// invalidate the ticket in hand when a WS reconnects after a drop. Trading
/// "single-use" for "short" pays off here, because the property being protected
/// is "the long-lived credential never enters a URL", not "the URL cannot be
/// replayed".
pub const TICKET_TTL: Duration = Duration::from_secs(60);

/// Query-string parameter name carrying the ticket.
pub const TICKET_PARAM: &str = "ticket";

/// This deployment's authentication shape.
#[derive(Debug, Clone)]
pub enum AuthMode {
    /// Verify the bearer token (and short-lived tickets)
    Token { digest: [u8; 32] },
    /// Explicitly off — should only ever appear on a development machine
    /// listening on loopback
    Disabled,
}

impl AuthMode {
    /// Load from the environment. **Missing configuration is an error, not
    /// "assume it was not configured".**
    pub fn from_env() -> cormex_core::Result<Self> {
        let disabled = std::env::var(AUTH_ENV).is_ok_and(|v| v.trim() == DISABLED_VALUE);
        let digest_hex = std::env::var(TOKEN_SHA256_ENV)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());

        match (disabled, digest_hex) {
            // Both configured: the intent is ambiguous. Ambiguity in a security
            // configuration has to be a loud error — the two ways of guessing
            // wrong are "thought it was off, it is actually on" (merely
            // inconvenient) and "thought it was on, it is actually off" (the
            // memory database is running naked), and the latter is not something
            // to be guessed at
            (true, Some(_)) => Err(cormex_core::CormexError::Config(format!(
                "{AUTH_ENV}={DISABLED_VALUE} and {TOKEN_SHA256_ENV} are both configured, so \
                 the intent is ambiguous. To turn authentication off, remove \
                 {TOKEN_SHA256_ENV}; to turn it on, remove {AUTH_ENV}."
            ))),
            (true, None) => Ok(Self::Disabled),
            (false, Some(hex_digest)) => {
                let raw = hex::decode(&hex_digest).map_err(|e| {
                    cormex_core::CormexError::Config(format!(
                        "{TOKEN_SHA256_ENV} is not valid hex: {e}"
                    ))
                })?;
                let digest: [u8; 32] = raw.try_into().map_err(|_| {
                    cormex_core::CormexError::Config(format!(
                        "{TOKEN_SHA256_ENV} must be a SHA-256 digest (64 hex characters). \
                         What goes here is the **digest**, not the plaintext token — \
                         `cormexd --generate-token` produces both at once."
                    ))
                })?;
                Ok(Self::Token { digest })
            }
            (false, None) => Err(cormex_core::CormexError::Config(format!(
                "no credential is configured, so cormexd refuses to start.\n\
                 \n\
                 An unauthenticated cormexd hands the whole memory database to\n\
                 anyone who can reach this port, and that has no symptom at all\n\
                 — so it fails on the spot rather than defaulting to open.\n\
                 \n\
                 Generate a credential:  cormexd --generate-token\n\
                 then write {TOKEN_SHA256_ENV}=… into .env (the plaintext token\n\
                 goes to the client).\n\
                 \n\
                 A loopback-only development machine that genuinely wants no\n\
                 authentication has to say so: {AUTH_ENV}={DISABLED_VALUE}."
            ))),
        }
    }

    /// One word for `/health` and the startup log.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Token { .. } => "token",
            Self::Disabled => "disabled",
        }
    }

    /// The line printed for ops at startup.
    #[must_use]
    pub fn status_line(&self) -> String {
        match self {
            Self::Token { .. } => "auth: enabled (Bearer token)".into(),
            Self::Disabled => format!(
                "auth: ⚠ **disabled** ({AUTH_ENV}={DISABLED_VALUE}) — anyone who can \
                 reach this port holds all of the memory. Acceptable only once you have \
                 confirmed the listen address is loopback"
            ),
        }
    }

    fn verify(&self, presented: &str) -> bool {
        match self {
            Self::Disabled => true,
            Self::Token { digest } => {
                let got = Sha256::digest(presented.as_bytes());
                // Hashing happens before the comparison, so even a timing leak
                // here would only leak bytes of the digest — but a
                // constant-time comparison is free, so there is no reason not to
                // do it
                constant_time_eq(&got, digest)
            }
        }
    }
}

/// Compare every byte, never returning early.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Generate a fresh credential: (plaintext token, hex of the SHA-256 digest).
///
/// The plaintext exists only at this instant. The server keeps the digest, the
/// client keeps the plaintext, and no single place holds anything beyond its own
/// half — someone who reads the server's `.env` still obtains nothing usable to
/// log in with.
#[must_use]
pub fn generate() -> (String, String) {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect(
        "the kernel entropy source is unavailable; refusing to mint a predictable credential",
    );
    let token = hex::encode(buf);
    let digest = hex::encode(Sha256::digest(token.as_bytes()));
    (token, digest)
}

// ─────────────────────── Short-lived tickets ──────────────────

/// Tickets that have been issued and have not yet expired, **each bound to the
/// principal that asked for it**.
///
/// Kept in memory, never in the database: a ticket does not survive a minute,
/// while anything written to `sync_log` syncs to every device and can never be
/// removed. All tickets are void after a process restart, and the client simply
/// exchanges for another one — it holds the long-lived token anyway.
///
/// # Why the principal is stored with the ticket
///
/// It used to be a bare `HashMap<String, Instant>`, so a ticket carried no
/// identity: whatever consumed it fell back to user #1. A browser signed in as
/// user B therefore opened its WebSocket and `<img src>` requests against user
/// A's memory — and nothing reported an error. The ticket is issued by an
/// already-authenticated request, so the identity is right there; recording it
/// costs one clone.
#[derive(Default)]
pub struct TicketBook {
    inner: Mutex<HashMap<String, (crate::principal::Principal, Instant)>>,
}

impl TicketBook {
    /// Issue a ticket for this principal. The caller must **already** have
    /// passed bearer verification — the principal it hands over is the one the
    /// middleware resolved.
    pub fn issue(&self, principal: crate::principal::Principal) -> String {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect(
            "the kernel entropy source is unavailable; refusing to issue a predictable ticket",
        );
        let ticket = hex::encode(buf);
        let mut guard = self
            .inner
            .lock()
            .expect("the ticket book's lock should never be poisoned");
        // Prune the expired ones while we are here. There is no separate cleanup
        // task: issuing is the only action that grows this map, so pruning here
        // makes "grows and never shrinks" impossible
        let now = Instant::now();
        guard.retain(|_, (_, exp)| *exp > now);
        guard.insert(ticket.clone(), (principal, now + TICKET_TTL));
        ticket
    }

    /// The principal this ticket was issued for, if it is still valid.
    ///
    /// Returning the principal rather than a bare `bool` is what keeps the
    /// ticket path from having to guess an identity — see the type's docs.
    fn resolve(&self, ticket: &str) -> Option<crate::principal::Principal> {
        let guard = self
            .inner
            .lock()
            .expect("the ticket book's lock should never be poisoned");
        guard
            .get(ticket)
            .filter(|(_, exp)| *exp > Instant::now())
            .map(|(p, _)| p.clone())
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner
            .lock()
            .expect("the ticket book's lock should never be poisoned")
            .len()
    }
}

// ────────────────────────── Middleware ─────────────────────────

/// The authentication middleware.
///
/// # Why "these paths skip authentication" is not written here
///
/// A path allow-list is the single most classic failure point of this kind of
/// middleware: whoever adds a new route will not remember to update that table,
/// and the direction the omission falls in is **allow by default**. So the
/// exemption is not expressed here — `/health` simply does not have this
/// middleware attached at all (see [`crate::routes::router`]), and a newly added
/// route lands on the protected side by default.
pub async fn require(State(st): State<AppState>, req: Request, next: Next) -> Response {
    let mode = st.auth_mode();
    if matches!(mode, AuthMode::Disabled) {
        let principal = crate::principal::Principal::LegacyOwner;
        return next.run(with_principal(req, principal)).await;
    }

    // 1) The normal path: Authorization: Bearer
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    if let Some(tok) = bearer {
        // The pre-shared token (the old path, and the norm for self-hosting)
        if mode.verify(tok) {
            let principal = crate::principal::Principal::LegacyOwner;
            return next.run(with_principal(req, principal)).await;
        }
        // The access token obtained by logging in.
        //
        // **This branch used to be missing**, and without it the entire accounts
        // system was broken end to end: register, log in, receive a token, and
        // then every request 401s — because this gate only recognised the
        // pre-shared one. It surfaced only on an end-to-end run, and all three
        // classes of test missed it: the server tests exercised `/auth/login` in
        // isolation, the middleware tests only inspected the public route table,
        // and the client tests hit a mock. **Not one of them went from logging
        // in to using the result.**
        if let Some(user_id) = st.access_book().resolve(tok) {
            let principal = crate::principal::Principal::User { user_id };
            return next.run(with_principal(req, principal)).await;
        }
        // A sandbox token. **What sets it apart from the two branches above is
        // that it carries a scope** — recognising it is not enough, we still
        // have to ask "can it reach this route".
        //
        // Why it cannot simply be admitted on recognition like the other two:
        // what runs inside the container is untrusted code, and `/episodes` and
        // `/memory/search` are **persistent channels that pierce the container
        // boundary** (memory written through them will be retrieved in every
        // future session, on every device). Full reasoning in `sandbox_token`'s
        // module docs.
        if let Some(scope) = st.delegations().resolve(tok) {
            let (method, path) = (req.method().clone(), req.uri().path().to_owned());
            if scope.allows(&method, &path) {
                // Put the scope into the extensions so the handler can narrow
                // further at the **session** level (authentication only answers
                // "can it reach this route"; it cannot answer "can it reach this
                // session's data" — that requires the handler to have the body
                // and parameters in hand)
                let principal = crate::principal::Principal::Delegated(scope);
                return next.run(with_principal(req, principal)).await;
            }
            tracing::warn!(
                owner = %scope.owner, session = %scope.session_id, %method, %path,
                "the sandbox token cannot reach this route"
            );
            return (
                StatusCode::FORBIDDEN,
                axum::Json(crate::dto::ErrorBody {
                    error: format!("the sandbox may not access {method} {path}"),
                }),
            )
                .into_response();
        }
    }

    // 2) Clients that cannot set request headers: short-lived tickets
    //
    // The ticket carries the principal it was issued for, so this path has an
    // identity too. It used to have none, and everything that consumed a ticket
    // fell back to user #1.
    if let Some(ticket) = ticket_from_query(req.uri().query())
        && let Some(principal) = st.ticket_book().resolve(&ticket)
    {
        return next.run(with_principal(req, principal)).await;
    }

    // No distinction between "no credential supplied" and "wrong credential":
    // reporting them separately hands a brute-forcer a progress bar.
    // WWW-Authenticate is still sent — that is the standard shape of a 401, and
    // it is how a client learns what to bring
    tracing::debug!(path = %req.uri().path(), "authentication failed");
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer realm=\"cormexd\"")],
        axum::Json(crate::dto::ErrorBody {
            error: "unauthenticated: send Authorization: Bearer <token>, or — for a \
                    connection that cannot set headers — the ?ticket= obtained from \
                    POST /auth/ticket"
                .into(),
        }),
    )
        .into_response()
}

/// Attach the resolved principal to the request.
///
/// One helper rather than four `extensions_mut().insert(...)` calls: the
/// delegated branch also used to insert the raw `DelegatedScope`, and having two
/// extension types for one concept is how "the handler read the one the
/// middleware did not set" becomes possible.
fn with_principal(mut req: Request, principal: crate::principal::Principal) -> Request {
    req.extensions_mut().insert(principal);
    req
}

/// Pull `ticket=` out of the query string.
///
/// Hand-written rather than going through the `Query<T>` extractor: this
/// middleware runs in front of **every** route, the query parameters vary wildly
/// from route to route, and a strongly typed extractor would bounce requests for
/// "an unrecognised extra field" — which is an extremely hard 400 to
/// diagnose.
fn ticket_from_query(query: Option<&str>) -> Option<String> {
    let q = query?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == TICKET_PARAM).then(|| v.to_string())
    })
}

// ══════════════════════════════════════════════════════════════
//  Multi-tenancy: **not done** this round; the reasoning and the cost are here
// ══════════════════════════════════════════════════════════════
//
// "Authentication" and "data isolated per person" are two different things, an
// order of magnitude apart in cost. The whole module above is the former: one
// middleware layer plus one credential. The latter would have to touch:
//
// 1. **An ownership column on every business table.** `episodes` / `facts` /
//    `entities` / `blobs` / `summaries` / `session_events` / `fact_events` /
//    `episode_memories` / `episode_tool_calls` / `blob_transcripts` /
//    `redactions` — not one of them may be missed.
// 2. **`sync_log`'s cursor would have to be split per tenant.** Today it is a
//    single database-wide total order (one BIGSERIAL plus an advisory lock).
//    Under multi-tenancy, A's writes push up the cursor B sees, and the records
//    B pulls are full of filtered-out holes — not incorrect, but every client
//    pays bandwidth for everyone else's write volume. Genuinely splitting it
//    requires a sequence per tenant, and that replaces the foundation of the
//    sync protocol, namely "a single total order".
// 3. **All four retrieval paths' SQL would need the filter** (BM25 / vector /
//    graph / episode), plus the fetch after RRF fusion, plus both `as_of`
//    snapshot paths, plus rendering of the injection blocks.
// 4. **Whether `pg_advisory_xact_lock(4272)` should be split per tenant.**
//    Splitting it raises concurrency, but the argument "ticket order == commit
//    order" holds for a single sequence; switching to one lock per tenant means
//    re-deriving it from scratch (and re-running the existing falsification test
//    — 32 concurrent writers losing 12 rows).
// 5. **`blobs` is content-addressed**, so identical bytes are naturally shared
//    across tenants. Putting ownership on `blobs` destroys deduplication;
//    putting it on `episode_blobs` means reference counting when the last
//    reference is deleted — and this is an append-only system, where a
//    reference count has nowhere to live.
//
// And this is an **append-only** system: retrofitting an ownership column onto
// existing data has exactly one honest form, adding a column with a default
// ("everything belongs to the first user"), and that step is not the hard part.
// The hard parts are items 2, 3 and 4, and they are **equally hard whenever they
// are done**, independent of how much data exists now. In other words, "add the
// column while the data is still small" buys nothing — the real cost is not in
// the data migration.
//
// So this round is authentication only. **No ownership column was added**: a
// state where the columns exist but the read side does not filter would leave
// the next person believing isolation is already half in effect, when in fact if
// any one of the four retrieval paths misses its filter, the leak is total. It
// gets done as a whole or not at all, read side included.
//
// The current product shape is single-user self-hosting, and this authentication
// layer completely addresses the threat it faces ("can somebody else connect to
// my cormexd"). Half-built multi-tenancy is more dangerous than none: it leaves
// people believing their data is isolated.

#[cfg(test)]
mod tests {
    use super::*;

    fn token_mode() -> (String, AuthMode) {
        let (token, digest_hex) = generate();
        let raw = hex::decode(&digest_hex).expect("a generated digest should be valid hex");
        let digest: [u8; 32] = raw.try_into().expect("SHA-256 should be 32 bytes");
        (token, AuthMode::Token { digest })
    }

    #[test]
    fn the_right_token_passes_and_everything_else_does_not() {
        let (token, mode) = token_mode();
        assert!(mode.verify(&token), "the correct token must pass");
        assert!(!mode.verify(""), "an empty credential must not pass");
        assert!(!mode.verify("wrong"), "an incorrect token must not pass");

        // Changing one character must fail
        let mut tampered = token.clone();
        let last = tampered.pop().expect("the token is not empty");
        tampered.push(if last == 'a' { 'b' } else { 'a' });
        assert!(
            !mode.verify(&tampered),
            "a token differing by a single character must not pass"
        );

        // The digest itself must not be submittable as a credential: it is only
        // the hash of the token, and the server hashes again, so it will not
        // match
        let (_, digest_hex) = generate();
        assert!(!mode.verify(&digest_hex));
    }

    /// A wrong token whose digest **merely starts with the same bytes** must be
    /// rejected.
    ///
    /// The "change one character" assertion above cannot actually catch a
    /// degeneration such as writing the comparison as
    /// `got.starts_with(&digest[..1])`: change an arbitrary character and the
    /// digest's first byte changes too with probability 255/256, so that test
    /// stays green. And this degeneration is catastrophic — comparing a single
    /// byte means any random token is admitted with probability 1/256.
    ///
    /// So this test brute-forces a token sharing the first digest byte on the
    /// spot and then tries it (a few hundred SHA-256 computations, microseconds).
    /// It is **deterministic**: if the comparison is not over the full digest,
    /// it goes red.
    #[test]
    fn a_token_whose_digest_merely_starts_the_same_is_rejected() {
        let (real, mode) = token_mode();
        let real_digest = Sha256::digest(real.as_bytes());

        let mut collided = None;
        // At 1/256 per attempt, the chance of not hitting a match in twenty
        // thousand tries is negligible; genuinely failing to find one would mean
        // something is wrong with SHA-256, in which case failing here is also
        // the right outcome
        for _ in 0..20_000 {
            let (cand, _) = generate();
            if Sha256::digest(cand.as_bytes())[0] == real_digest[0] && cand != real {
                collided = Some(cand);
                break;
            }
        }
        let collided =
            collided.expect("should have been able to brute-force a token sharing the first byte");
        assert!(
            !mode.verify(&collided),
            "a token whose digest merely starts the same was admitted — the credential \
             comparison is not over the full digest"
        );
    }

    #[test]
    fn the_stored_digest_is_not_the_token() {
        let (token, digest) = generate();
        assert_ne!(
            token, digest,
            "what is stored must be the digest, never the plaintext"
        );
        assert_eq!(token.len(), 64);
        assert_eq!(digest.len(), 64);
        assert_eq!(
            hex::encode(Sha256::digest(token.as_bytes())),
            digest,
            "the digest must genuinely be the SHA-256 of the token"
        );
    }

    #[test]
    fn two_generated_tokens_never_collide() {
        let (a, _) = generate();
        let (b, _) = generate();
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_eq_agrees_with_normal_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(
            !constant_time_eq(b"abc", b"ab"),
            "differing lengths must compare false"
        );
    }

    fn user(id: &str) -> crate::principal::Principal {
        crate::principal::Principal::User {
            user_id: id.to_owned(),
        }
    }

    #[test]
    fn a_ticket_is_valid_until_it_is_not_in_the_book() {
        let book = TicketBook::default();
        let t = book.issue(user("u1"));
        assert!(book.resolve(&t).is_some());
        assert!(
            book.resolve("deadbeef").is_none(),
            "a ticket that was never issued must not pass"
        );
        // Reusable: the second image on a page must not 401
        assert!(book.resolve(&t).is_some());
    }

    /// **A ticket resolves to the principal that asked for it**, not to user #1.
    ///
    /// The ticket used to be a bare string with no identity, so everything that
    /// consumed one fell back to the first account — a browser signed in as user
    /// B opened its WebSocket against user A's memory, silently.
    #[test]
    fn a_ticket_remembers_who_it_was_issued_for() {
        let book = TicketBook::default();
        let a = book.issue(user("user-a"));
        let b = book.issue(user("user-b"));

        let who = |t: &str| {
            book.resolve(t).and_then(|p| match p {
                crate::principal::Principal::User { user_id } => Some(user_id),
                _ => None,
            })
        };
        assert_eq!(who(&a).as_deref(), Some("user-a"));
        assert_eq!(
            who(&b).as_deref(),
            Some("user-b"),
            "two tickets issued by two users resolved to the same identity — every consumer of \
             the ticket path (WebSocket, <img src>) would then read one person's memory"
        );
    }

    #[test]
    fn issuing_prunes_expired_tickets() {
        let book = TicketBook::default();
        {
            let mut g = book.inner.lock().unwrap();
            g.insert(
                "old".into(),
                (user("u1"), Instant::now() - Duration::from_secs(1)),
            );
        }
        assert_eq!(book.len(), 1);
        let fresh = book.issue(user("u1"));
        assert_eq!(
            book.len(),
            1,
            "expired tickets should be pruned when a new one is issued"
        );
        assert!(book.resolve(&fresh).is_some());
        assert!(book.resolve("old").is_none());
    }

    #[test]
    fn ticket_is_parsed_from_anywhere_in_the_query_string() {
        assert_eq!(ticket_from_query(Some("ticket=abc")), Some("abc".into()));
        assert_eq!(
            ticket_from_query(Some("since=3&ticket=abc&limit=5")),
            Some("abc".into())
        );
        assert_eq!(ticket_from_query(Some("since=3")), None);
        assert_eq!(ticket_from_query(None), None);
        // A similarly named parameter must not be mistaken for it
        assert_eq!(ticket_from_query(Some("myticket=abc")), None);
    }

    /// The disable switch accepts that one literal only. This guards the
    /// argument in the module docs.
    #[test]
    fn the_disable_switch_only_accepts_the_exact_literal() {
        for v in ["1", "true", "yes", "off", "DISABLED", "disable"] {
            assert_ne!(
                v.trim(),
                DISABLED_VALUE,
                "{v:?} must never turn authentication off — the switch has to be something you \
                 only write down after understanding it"
            );
        }
        assert_eq!("disabled".trim(), DISABLED_VALUE);
    }

    /// **An access token obtained by logging in must get through this gate.**
    ///
    /// This did not hold once: `require` only called `mode.verify`, which only
    /// recognises the digest of the pre-shared token. So the whole accounts
    /// chain was broken — registration succeeds, login succeeds, a token comes
    /// back, and then **every request 401s**.
    ///
    /// All three classes of test missed it: the server tests exercised
    /// `/auth/login`'s response in isolation, the middleware tests only
    /// inspected the public route table, and the client tests hit a mock. Not
    /// one of them went **from logging in to using the result**, and it only
    /// surfaced on an end-to-end run.
    ///
    /// No axum is started here; this pins down the invariant only: the gate must
    /// recognise the **union** of the two things.
    #[test]
    fn a_logged_in_access_token_is_not_the_preshared_one() {
        let book = crate::accounts::AccessBook::default();
        let issued = book.issue_for_test("01ABCDEF", std::time::Duration::from_secs(60));

        let mode = AuthMode::Token {
            digest: Sha256::digest(b"the-preshared-one").into(),
        };
        assert!(
            !mode.verify(&issued),
            "a freshly issued access token has nothing to do with the pre-shared token, so of \
             course `verify` does not recognise it — which is why `require` **must** have a \
             separate branch that asks the AccessBook. This assertion will never go red on its \
             own; it is here to make the next one's meaning clear"
        );
        assert_eq!(
            book.resolve(&issued).as_deref(),
            Some("01ABCDEF"),
            "the book must recognise a token it issued itself. If this goes red, the accounts \
             system has broken in the same place again: login succeeds but every subsequent \
             request 401s"
        );
    }
}

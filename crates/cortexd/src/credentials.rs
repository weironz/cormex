//! Account credentials — how passwords are stored, how refresh tokens are
//! verified, and when the entire chain must be revoked.
//!
//! This module contains **pure logic** only: it touches neither the database nor
//! HTTP, and does no tenant isolation. It answers three questions, and a wrong
//! answer to any of them does not show up in tests — only afterwards.
//!
//! # 1. Why passwords cannot reuse [`crate::auth`]'s SHA-256
//!
//! `auth.rs` argued that "a SHA-256 digest is enough for a pre-shared token, no
//! slow KDF needed", and that argument is correct — but it **states a premise
//! explicitly**: the token is 256 bits of kernel randomness. A slow KDF defends
//! against "offline brute-forcing of a low-entropy secret", and for a
//! high-entropy random string, making each attempt a million times slower moves
//! the brute-force time from "the age of the universe" to "the age of the
//! universe" — paying for a dependency and tens of milliseconds of CPU per
//! request in exchange for nothing.
//!
//! **A human-chosen password does not satisfy that premise.** Its real entropy
//! is on the order of twenty or thirty bits, and its distribution is heavily
//! concentrated on a few million known strings. For a secret like that, the cost
//! of a single attempt **is** the only defence: after the database is stolen,
//! SHA-256 lets the attacker try billions per second while argon2id lets them
//! try thousands. One criterion (is the secret high-entropy), opposite
//! conclusions on either side of it.
//!
//! Which is why this file contains a combination that looks self-contradictory:
//!
//! | Secret | Chosen by | Entropy | Stored as |
//! |---|---|---|---|
//! | password | a human | low | **argon2id** (slow, resists offline brute force) |
//! | refresh token | kernel CSPRNG | 256 bits | **SHA-256** (fast, sufficient) |
//!
//! This is not an inconsistency, it is the two sides of one criterion. Anyone
//! who "unifies them while they are in here" is wrong whichever way they unify:
//! unifying on SHA-256 gives away the password database, and unifying on argon2
//! burns tens of milliseconds of CPU on every token refresh (and refreshes are
//! far more frequent than logins).
//!
//! # 2. Refresh token rotation and families
//!
//! Every time a refresh token is exchanged for a new access token, that token is
//! retired and rotates out a successor. One rotation chain is called a
//! **family**.
//!
//! The entire point of doing it this way: **an already-rotated old token showing
//! up again = it leaked**. A normal client never reuses the old one after
//! receiving its successor; only two kinds of party would — an attacker holding
//! a copy, or the real user whose successor the attacker consumed first. **We
//! cannot tell which, and we do not need to**: in both cases the correct action
//! is to revoke the entire family and make both sides log in again. See
//! [`judge_refresh`].

// This module is the **pure-logic half** of the accounts system, and the routes
// and repositories that call it have not landed yet. cortexd is a bin crate, so
// `pub` does not exempt anything from dead_code here, and until it is wired up
// every exported item is a compile error under `-D warnings`.
//
// That conflicts with CLAUDE.md's requirement to "make the code genuinely used
// rather than adding an allow", and the truth here is "it is about to be used"
// — so this allow is **scaffolding**: it should be deleted the moment the
// routes are wired in, and if anything still reports dead_code afterwards, then
// it genuinely has no callers and should be handled by that rule.
//
// `#[expect(dead_code)]` was not used (it warns automatically once no longer
// needed and thus nags for its own removal, which would have been the better
// choice): these items are **all used** inside the tests, so when
// `--all-targets` compiles the test target that expectation is "unfulfilled",
// which in turn triggers `unfulfilled_lint_expectations`. The two targets each
// demand a different spelling, and only allow passes both.

use argon2::{Algorithm, Argon2, Params, Version};
use cortex_core::Id;
use password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use sha2::{Digest as _, Sha256};

// ══════════════════════════════════════════════════════════════
//  1. Passwords
// ══════════════════════════════════════════════════════════════

/// Minimum password length.
///
/// 12 rather than 8: argon2id compresses offline brute force from "billions per
/// second" to "thousands per second", and that factor is not enough for an
/// 8-character password — common 8-character passphrases essentially all sit in
/// dictionaries of a few hundred million entries, and at thousands per second
/// that is a matter of days. 12 pushes the dictionary space up to where a real
/// compute budget is required.
///
/// **There are deliberately no composition rules** (must contain uppercase /
/// digits / symbols). The measured effect of such rules is to push people
/// towards the shape `Password1!` — which satisfies every rule and sits right
/// at the front of the dictionary. Length is the only requirement where
/// complying genuinely adds entropy, so length is all that is asked for here.
pub const MIN_PASSWORD_LEN: usize = 12;

/// Maximum password length.
///
/// argon2's cost is almost independent of input length, so a long password is
/// not itself a DoS — this bound exists to keep the shape of the "password"
/// field finite, so that a missing body limit somewhere upstream does not create
/// a path that feeds 100 MB into a hash function. 1 KiB is far more than enough
/// for any passphrase.
pub const MAX_PASSWORD_LEN: usize = 1024;

/// Salt length in bytes. 16 is PHC's recommended value and also
/// [`SaltString`]'s `RECOMMENDED_LENGTH`.
const SALT_BYTES: usize = 16;

/// Memory cost in KiB — 19 MiB.
///
/// # Why 19 MiB, rather than "bigger is safer"
///
/// This is OWASP's first-tier recommendation for argon2id (m=19 MiB, t=2, p=1).
/// It was chosen not because it is the default, but because **the production
/// machine is 2 cores and 3.5 GB, already running 19 containers**.
///
/// Memory is actually the cheapest of the three: 19 MiB is an **instantaneous
/// per-hash** allocation, so a handful of concurrent logins is tens of MB,
/// returned as soon as it is done. What actually goes wrong is CPU — see
/// [`T_COST`].
///
/// Raising it (to 64 MiB, say) buys limited GPU resistance on this machine while
/// turning "a handful of concurrent logins" into a several-hundred-MB
/// instantaneous peak — on a machine whose memory is already divided among 19
/// containers.
const M_COST_KIB: u32 = 19 * 1024;

/// Iteration count.
///
/// OWASP's recommended value paired with `M_COST_KIB`. **Measured** (release
/// build, single idle core on a development machine):
///
/// | Parameters | Per hash |
/// |---|---|
/// | m=19 MiB, t=2, p=1 (this module) | 16.5 ms |
/// | m=19 MiB, t=3, p=1 | 22.7 ms |
/// | m=64 MiB, t=3, p=1 (OWASP's other tier) | 89.6 ms |
///
/// On the production machine — 2 cores, CPU divided among 19 containers —
/// reckon 2–4x that, i.e. 35–70 ms.
///
/// So there is in fact **headroom** here, and raising t to 3 would still be
/// within the acceptable range. It was not raised, because the extra 6 ms buys
/// only a 1.5x brute-force resistance factor while scaling the following problem
/// proportionally:
///
/// **The login endpoint is itself the best DoS amplifier available.** An
/// attacker needing no credentials at all merely pours wrong passwords into
/// `/login`, and each request burns one argon2 on the server for nothing — on
/// this machine, twenty or thirty requests per second consumes one core (out of
/// two). Doubling the parameters doubles their amplification factor while
/// brute-force resistance improves only linearly.
///
/// The thing that should actually defend against this is rate limiting on the
/// login endpoint, not softening the KDF (whose cost is paid by every user on
/// the day the database is stolen). **This constant is worth revisiting once
/// rate limiting lands.**
///
/// Note that under a debug build these parameters take over 400 ms — that is
/// unoptimized argon2, not parameters chosen too aggressively. Do not tune this
/// by how it feels while running tests on a development machine.
const T_COST: u32 = 2;

/// Parallelism.
///
/// **It must be 1, and this matters more than it looks.** With the `argon2`
/// crate's `parallel` feature off, the lanes for p>1 are computed
/// **sequentially** — meaning that raising p to 2 merely doubles the wall-clock
/// time of a single hash with no parallel benefit whatsoever, purely doubling
/// your own DoS amplification factor.
///
/// Even with parallelism genuinely enabled: this machine has 2 cores. Letting one
/// login consume both means that for the duration of one person's login, the
/// whole machine (along with those 19 containers) is waiting.
const P_COST: u32 = 1;

/// This deployment's argon2id instance.
///
/// Constructed fresh on every call: `Argon2::new` only fills in a few u32s and
/// precomputes nothing, so caching it buys nothing while introducing a
/// `OnceLock` and the question "when were these parameters fixed".
fn hasher() -> Argon2<'static> {
    // The parameters are constants whose validity has been argued point by point
    // above (m ≥ 8p, t ≥ 1, p ≥ 1), so construction cannot fail. If it does
    // fail, somebody broke the constants above, and crashing on the startup path
    // is far better than silently degrading to weak parameters
    let params = Params::new(M_COST_KIB, T_COST, P_COST, None).expect(
        "the argon2 parameters are compile-time constants; a failed construction means this \
         module's constants were broken",
    );
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// Hash a plaintext password into a PHC string
/// (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<digest>`).
///
/// What it produces is a **self-describing** PHC string: the parameters and salt
/// are encoded inside it. Which is why [`verify_password`] does not read the
/// constants above and reads only this string — meaning that when the
/// parameters are raised later, old passwords still verify, and the user's next
/// login recomputes under the new parameters. No one-shot migration, and no
/// column in the database recording "which generation of parameters this row
/// used".
pub fn hash_password(plain: &str) -> cortex_core::Result<String> {
    check_password_shape(plain)?;

    // The salt comes straight from the kernel CSPRNG rather than going through
    // `SaltString::generate(&mut OsRng)`.
    //
    // The latter requires `password-hash` to enable `rand_core/getrandom`, so
    // "is the salt genuinely random" comes to depend on a **feature propagating
    // correctly across crates** — and somebody turning that feature off raises
    // no error, it merely weakens the salt, with no symptom at all. `getrandom`
    // is a direct dependency of this crate anyway (same reasoning as in
    // auth.rs).
    let mut salt_bytes = [0u8; SALT_BYTES];
    getrandom::fill(&mut salt_bytes)
        .expect("the kernel entropy source is unavailable; refusing to hash a password with a predictable salt");
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| {
        cortex_core::CortexError::Other(anyhow::anyhow!("could not encode the salt: {e}"))
    })?;

    hasher()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| {
            cortex_core::CortexError::Other(anyhow::anyhow!("could not hash the password: {e}"))
        })
}

/// Verify a plaintext password.
///
/// # Why this returns `bool` rather than `Result`
///
/// There are three failures here: the password is wrong, the PHC string is
/// corrupt (truncated / hand-edited / an unrecognised algorithm field), or the
/// parameters cannot be parsed. **The caller's correct response to all three is
/// identical: refuse the login.**
///
/// And reporting them separately causes two things, both bad:
///
/// 1. The difference leaks out through the HTTP response. "Wrong password" and
///    "this account's hash string is corrupt" are two different pieces of
///    information, and the latter tells an attacker "this account exists, and
///    there is something wrong with its record".
/// 2. One more `?` at the call site, so **a corrupt hash string becomes a 500
///    instead of a 401**. A 500 gets retried, raises alerts, and gets
///    investigated as a service outage — while it is in fact a login that
///    should have been quietly refused.
///
/// So the difference is swallowed right here, leaving only one log line
/// (`debug` level, carrying no password material). It never panics — half of
/// this function's input comes from the database and the other half from the
/// public internet.
#[must_use]
pub fn verify_password(plain: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        // This is not "the password is wrong", it is "this row of data is
        // broken". Ops needs to know; the user does not
        tracing::debug!("the PHC string will not parse — this account's password hash is corrupt");
        return false;
    };

    // **We decide the algorithm; the string being verified does not.**
    //
    // `PasswordVerifier`'s default implementation follows whatever the PHC
    // string says — `$argon2i$` / `$argon2d$` — recomputing under that and
    // admitting the result, meaning the string specifies which algorithm
    // verifies itself. This system only ever produces argon2id, so any other
    // algorithm appearing has exactly two explanations: the data was written by
    // somebody, or someone added a "compatible with legacy hashes" path that
    // nobody reviewed. Both should stop dead right here.
    //
    // argon2i is weak against GPUs and argon2d is weak against side channels;
    // id is the hybrid of the two — "follow the string while verifying" hands
    // the choice to the attack surface.
    if argon2::Algorithm::try_from(parsed.algorithm) != Ok(argon2::Algorithm::Argon2id) {
        tracing::debug!(algorithm = %parsed.algorithm, "refusing a password hash that is not argon2id");
        return false;
    }

    // The digest and the parameters are still read from the string (that is the
    // whole point of PHC being self-describing, and the reason old passwords
    // still verify after M_COST_KIB is raised) — the only thing pinned down is
    // which algorithm is used
    hasher().verify_password(plain.as_bytes(), &parsed).is_ok()
}

/// Length validation.
///
/// A separate function so that the registration path can reject **without
/// hashing** — otherwise a 4-character password would still burn 100 ms of
/// argon2 before being told it is too short.
fn check_password_shape(plain: &str) -> cortex_core::Result<()> {
    // Measured in **bytes**, not characters. The reason is not convenience:
    // entropy is measured in information, and `plain.chars().count()` counts one
    // emoji as 1, so 12 emoji (which on a real keyboard means 12 taps on a
    // picker panel) would satisfy the requirement
    let len = plain.len();
    if len < MIN_PASSWORD_LEN {
        return Err(cortex_core::CortexError::Invalid(format!(
            "a password needs at least {MIN_PASSWORD_LEN} bytes. No mixture of upper and \
             lower case, digits and symbols is demanded here — length is the only one of \
             those that actually helps."
        )));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(cortex_core::CortexError::Invalid(format!(
            "a password is at most {MAX_PASSWORD_LEN} bytes."
        )));
    }
    Ok(())
}

// ══════════════════════════════════════════════════════════════
//  2. Refresh tokens
// ══════════════════════════════════════════════════════════════

/// Entropy of a refresh token, in bytes.
///
/// 32 bytes = 256 bits, the same order of magnitude as `auth.rs::generate`, for
/// the same reason: this is the **premise** of the entire "no slow KDF needed"
/// argument, not a knob to be turned down for the sake of a shorter string.
pub const REFRESH_BYTES: usize = 32;

/// Issue one refresh token: `(plaintext, hex of the SHA-256 digest)`.
///
/// The plaintext exists only at this instant and is sent to the client; **only
/// the digest is persisted**. So after the database is stolen, not one refresh
/// token in it can be exchanged for an access token directly — the server runs
/// SHA-256 over whatever it receives and looks the row up by digest (see
/// [`refresh_digest`]).
///
/// The digest is SHA-256 rather than argon2: see the table in section 1 of the
/// module docs. This choice is the opposite of [`hash_password`]'s above, and
/// both follow from the same criterion.
#[must_use]
pub fn generate_refresh() -> (String, String) {
    let mut buf = [0u8; REFRESH_BYTES];
    getrandom::fill(&mut buf).expect(
        "the kernel entropy source is unavailable; refusing to issue a predictable refresh token",
    );
    let plain = hex::encode(buf);
    let digest = refresh_digest(&plain);
    (plain, digest)
}

/// The persisted form of a plaintext refresh token.
#[must_use]
/// # Why there is no `verify_refresh` / constant-time comparison here
///
/// There was one once, correctly written, but wiring it up revealed it was
/// **unused**: `auth_tokens.token_sha256` is a unique index, so after computing
/// the digest from the plaintext the server **looks the row up by digest**
/// rather than "fetching a row and comparing it byte by byte". The comparison
/// happens inside a B-tree, where an attacker timing it learns nothing — and
/// "does this token exist" is already written on the response code anyway.
///
/// Deleted rather than kept with `#[allow(dead_code)]`: a piece of security code
/// with no callers leaves the next person who reads it believing that path is
/// protected.
pub fn refresh_digest(plain: &str) -> String {
    hex::encode(Sha256::digest(plain.as_bytes()))
}

// ══════════════════════════════════════════════════════════════
//  3. Replay adjudication
// ══════════════════════════════════════════════════════════════

/// One refresh token's state in the database — everything [`judge_refresh`]
/// needs to know.
///
/// It deliberately **contains no digest, no plaintext and no user**: the
/// adjudication has nothing to do with "is this token genuine", which is settled
/// by looking the row up by digest and must happen before the adjudication.
/// Cramming both into one function produces the failure "revoke the family even
/// though the digest did not match" — a denial of service anyone can help
/// themselves to, where an attacker merely guesses a token at random and kicks
/// somebody else's session offline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshRecord {
    /// Id of this rotation chain. What gets revoked is **it**, not this single
    /// token.
    pub family: Id,
    /// Already used once, and rotated out a successor.
    pub rotated: bool,
    /// The entire family has been revoked (this token or one of its siblings
    /// triggered a replay detection at some point).
    pub revoked: bool,
    /// The absolute expiry instant has passed.
    ///
    /// A **pre-computed boolean** rather than a timestamp: the adjudication is a
    /// pure function, and letting it read `Utc::now()` would make it untestable.
    pub expired: bool,
}

/// Why it was rejected — for logs and metrics only, **never for the HTTP
/// response body**.
///
/// To the client the three rejections are indistinguishable; all of them mean
/// "log in again". Telling it the difference tells an attacker whether the token
/// they hold expired or was consumed by somebody else first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// The whole family died long ago.
    FamilyRevoked,
    /// The absolute lifetime has elapsed.
    Expired,
}

/// The verdict. **Three states; it must not collapse into a boolean.**
///
/// Merging [`Self::Reject`] and [`Self::RevokeFamily`] into one "reject" means
/// that after a replay is detected, the **not yet rotated** successor token in
/// the attacker's hands remains valid — i.e. the leak was detected and nothing
/// happened. The entire value of replay detection lives in the third state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshVerdict {
    /// Accept: issue a new access token, mark this one rotated, and sign a
    /// successor.
    Rotate,
    /// Refuse this attempt; leave the family alone.
    Reject(RejectReason),
    /// Refuse, and revoke the entire family — a replay was detected.
    RevokeFamily { family: Id },
}

/// Adjudicate one refresh request. **A pure function**: the caller has already
/// confirmed that the digest matches.
///
/// # The check order is the entire content of this function, and it is not
/// # arbitrary
///
/// ```text
/// revoked  ─► Reject(FamilyRevoked)   ← family already dead, do not re-revoke
/// rotated  ─► RevokeFamily            ← replay! must be checked before expired
/// expired  ─► Reject(Expired)
/// else     ─► Rotate
/// ```
///
/// ## Why `rotated` must come before `expired`
///
/// This is the easiest thing here to get backwards, and once it is backwards the
/// tests will most likely still be green.
///
/// Intuition says "it has expired anyway, why bother checking for a replay". But
/// **every rotation on the chain signs a successor with a fresh lifetime**: an
/// old token that should have expired back in 2020 may well belong to a family
/// that is alive right now and being used by the real user. Somebody presenting
/// that ancient token for exchange — that is conclusive proof they hold a
/// historical copy, and precisely the moment the whole chain must be cut.
/// Checking `expired` first downgrades that signal into an unremarkable "it
/// expired, please log in again", leaving the attacker's **unexpired** successor
/// perfectly usable.
///
/// ## Why `revoked` comes first, and is only a Reject
///
/// The family has already been revoked, and revoking it again makes nothing
/// safer; it merely writes another entirely uninformative record into an
/// append-only database. Worse, it drowns the signal "a **new** leak was just
/// detected" — which should be rare, and which is what raises the alert — in
/// noise.
#[must_use]
pub fn judge_refresh(record: &RefreshRecord) -> RefreshVerdict {
    if record.revoked {
        return RefreshVerdict::Reject(RejectReason::FamilyRevoked);
    }
    if record.rotated {
        return RefreshVerdict::RevokeFamily {
            family: record.family,
        };
    }
    if record.expired {
        return RefreshVerdict::Reject(RejectReason::Expired);
    }
    RefreshVerdict::Rotate
}

#[cfg(test)]
mod tests {
    use super::*;

    // ───────────────────────── Passwords ─────────────────────

    #[test]
    fn a_password_hash_is_phc_with_the_parameters_we_chose() {
        let phc =
            hash_password("correct horse battery staple").expect("a valid password should hash");
        assert!(
            phc.starts_with("$argon2id$"),
            "must be argon2id (not argon2i / argon2d); actually got: {phc}"
        );
        assert!(
            phc.contains("v=19"),
            "must be argon2 v1.3 (0x13); older versions have known weaknesses. Actual: {phc}"
        );
        assert!(
            phc.contains(&format!("m={M_COST_KIB},t={T_COST},p={P_COST}")),
            "the parameters inside the PHC string must match the constants argued for in this \
             module, or the property \"parameters can be raised smoothly later\" does not hold. \
             Actual: {phc}"
        );
    }

    #[test]
    fn the_same_password_never_hashes_to_the_same_string() {
        let a =
            hash_password("correct horse battery staple").expect("a valid password should hash");
        let b =
            hash_password("correct horse battery staple").expect("a valid password should hash");
        assert_ne!(
            a, b,
            "hashing the same password twice produced the same string — which means the salt is \
             not random, so after the database is stolen one can group by digest and find out \
             who shares a password with whom"
        );
        // But both verify the same password
        assert!(verify_password("correct horse battery staple", &a));
        assert!(verify_password("correct horse battery staple", &b));
    }

    #[test]
    fn the_plaintext_password_never_appears_in_the_hash() {
        let phc = hash_password("hunter2-hunter2-hunter2").expect("a valid password should hash");
        assert!(
            !phc.contains("hunter2"),
            "the plaintext password appeared inside the PHC string: {phc}"
        );
    }

    #[test]
    fn only_the_right_password_verifies() {
        let phc =
            hash_password("correct horse battery staple").expect("a valid password should hash");
        assert!(
            verify_password("correct horse battery staple", &phc),
            "the correct password must verify"
        );
        assert!(
            !verify_password("correct horse battery stapl", &phc),
            "a password one character short must not verify"
        );
        assert!(
            !verify_password("", &phc),
            "an empty password must not verify"
        );
        assert!(
            !verify_password("Correct horse battery staple", &phc),
            "a password differing only in case must not verify — verification has to be \
             case-sensitive"
        );
    }

    /// A corrupt PHC string is always `false`, and never panics.
    ///
    /// This function's second argument comes from the database and its first
    /// from the public internet. Any panic here is a hole where one specific
    /// request to the login endpoint kills a worker.
    #[test]
    fn a_corrupt_hash_is_a_quiet_false_never_a_panic() {
        let good =
            hash_password("correct horse battery staple").expect("a valid password should hash");
        let corrupt = [
            ("", "empty string"),
            ("not a phc string at all", "not PHC-shaped at all"),
            ("$argon2id$", "only the algorithm segment"),
            ("$argon2id$v=19$m=19456,t=2,p=1$", "missing salt and digest"),
            (
                "$bcrypt$v=19$m=1,t=1,p=1$c2FsdA$aGFzaA",
                "unrecognised algorithm",
            ),
            (
                "$argon2id$v=19$m=19456,t=2,p=1$!!!!$!!!!",
                "salt and digest are not b64",
            ),
            (&good[..good.len() - 5], "a truncated valid string"),
        ];
        for (phc, what) in corrupt {
            assert!(
                !verify_password("correct horse battery staple", phc),
                "a corrupt PHC string ({what}) must compare false rather than verify: {phc:?}"
            );
        }
    }

    /// A string computed with argon2i / argon2d must not be admitted as
    /// argon2id.
    ///
    /// `PasswordVerifier`'s default implementation verifies using whatever
    /// algorithm the string names, and we should only accept argon2id —
    /// argon2i is weak against GPUs and argon2d is weak against side channels.
    #[test]
    fn only_argon2id_is_accepted() {
        let salt = SaltString::encode_b64(&[7u8; SALT_BYTES]).expect("a fixed salt should encode");
        let params =
            Params::new(M_COST_KIB, T_COST, P_COST, None).expect("the parameters are valid");
        let weak = Argon2::new(Algorithm::Argon2i, Version::V0x13, params)
            .hash_password(b"correct horse battery staple", &salt)
            .expect("the argon2i hash should compute")
            .to_string();
        assert!(
            weak.starts_with("$argon2i$"),
            "the constructed sample should be argon2i"
        );
        assert!(
            !verify_password("correct horse battery staple", &weak),
            "an argon2i hash string was admitted — that is the entry point for an algorithm \
             downgrade attack"
        );
    }

    #[test]
    fn a_too_short_password_is_rejected_before_any_hashing() {
        let err = hash_password(&"a".repeat(MIN_PASSWORD_LEN - 1))
            .expect_err("a password one byte below the minimum must be rejected");
        assert!(
            matches!(err, cortex_core::CortexError::Invalid(_)),
            "too short should be classified as invalid input, not something else; actual: {err:?}"
        );
        assert!(
            hash_password(&"a".repeat(MIN_PASSWORD_LEN)).is_ok(),
            "a password exactly at the minimum must be accepted — the boundary must not be off \
             by one"
        );
        assert!(
            hash_password(&"a".repeat(MAX_PASSWORD_LEN + 1)).is_err(),
            "a password above the maximum must be rejected"
        );
    }

    /// 12 emoji do not count as "long" in the sense of 12 bytes, but by byte
    /// count they are far above the minimum. What this test pins down is the
    /// choice to measure in bytes rather than chars.
    #[test]
    fn length_is_measured_in_bytes() {
        // 3 Chinese characters = 9 bytes < 12, so it is rejected both by char
        // count (3) and by byte count (9)
        assert!(
            hash_password("密码短").is_err(),
            "a 9-byte password must be rejected"
        );
        // 5 Chinese characters = 15 bytes: enough by byte count, only 5 by char
        // count
        assert!(
            hash_password("密码够长了").is_ok(),
            "a 15-byte password must pass when length is measured in bytes"
        );
    }

    // ─────────────────────── Refresh tokens ──────────────────────

    #[test]
    fn a_refresh_token_is_high_entropy_and_only_its_digest_is_stored() {
        let (plain, digest) = generate_refresh();
        assert_eq!(
            plain.len(),
            REFRESH_BYTES * 2,
            "the plaintext should be {REFRESH_BYTES} bytes of hex"
        );
        assert_ne!(
            plain, digest,
            "what is persisted must be the digest, never the plaintext"
        );
        assert_eq!(
            digest,
            refresh_digest(&plain),
            "the digest must genuinely be the SHA-256 of the plaintext"
        );
        let (other, _) = generate_refresh();
        assert_ne!(
            plain, other,
            "two issuances must not collide — only a broken entropy source does that"
        );
    }

    // ───────────────────── Replay adjudication ───────────────────

    fn fresh() -> RefreshRecord {
        RefreshRecord {
            family: Id::new(),
            rotated: false,
            revoked: false,
            expired: false,
        }
    }

    #[test]
    fn a_fresh_token_rotates() {
        assert_eq!(
            judge_refresh(&fresh()),
            RefreshVerdict::Rotate,
            "a token that has not been rotated, revoked or expired must be accepted and rotated"
        );
    }

    #[test]
    fn a_replayed_token_takes_down_the_whole_family() {
        let rec = RefreshRecord {
            rotated: true,
            ..fresh()
        };
        assert_eq!(
            judge_refresh(&rec),
            RefreshVerdict::RevokeFamily { family: rec.family },
            "an already-rotated token showing up again = it leaked. Merely refusing is not \
             enough here: the not-yet-rotated successor in the attacker's hands stays valid, so \
             the leak was detected and nothing happened"
        );
    }

    /// The verdict must land on **this record's** family.
    ///
    /// Returning a freshly minted or wrong family kicks somebody else's session
    /// offline while leaving the genuinely leaked one alive — both directions of
    /// error happening at once.
    #[test]
    fn the_revoked_family_is_the_one_from_the_record() {
        let mine = Id::new();
        let someone_else = Id::new();
        let rec = RefreshRecord {
            family: mine,
            rotated: true,
            ..fresh()
        };
        let RefreshVerdict::RevokeFamily { family } = judge_refresh(&rec) else {
            panic!("a replay must be adjudicated as RevokeFamily");
        };
        assert_eq!(
            family, mine,
            "what gets revoked must be this token's own family"
        );
        assert_ne!(
            family, someone_else,
            "it must never revoke somebody else's family"
        );
    }

    /// **This test guards the check order**, the thing in this module most
    /// easily written backwards.
    ///
    /// A token that is both expired and already rotated showing up means
    /// somebody holds a historical copy — while the successor that is on that
    /// chain **right now** may well be unexpired and in use by the real user.
    /// Checking `expired` first downgrades that signal into an unremarkable "it
    /// expired", leaving the attacker's unexpired successor perfectly usable.
    #[test]
    fn an_expired_token_that_was_also_rotated_still_takes_down_the_family() {
        let rec = RefreshRecord {
            rotated: true,
            expired: true,
            ..fresh()
        };
        assert_eq!(
            judge_refresh(&rec),
            RefreshVerdict::RevokeFamily { family: rec.family },
            "both expired and rotated — expiry must not mask a replay. Every rotation on the \
             chain signs a successor with a fresh lifetime, so \"the old one expired\" says \
             nothing whatsoever about \"this chain is dead\""
        );
    }

    #[test]
    fn a_merely_expired_token_is_rejected_without_touching_the_family() {
        let rec = RefreshRecord {
            expired: true,
            ..fresh()
        };
        assert_eq!(
            judge_refresh(&rec),
            RefreshVerdict::Reject(RejectReason::Expired),
            "merely expired (never rotated) means the user simply stayed away too long, not a \
             leak signal; revoking the whole family would turn a normal \"unused for too long\" \
             into an inexplicable logout on every device"
        );
    }

    /// An already-dead family only gets a `Reject`, never a second revocation.
    ///
    /// Repeated revocation is permanent noise in an append-only database, and it
    /// drowns the signal "a **new** leak was detected" — which should be rare
    /// and which should raise an alert.
    #[test]
    fn an_already_revoked_family_is_not_revoked_again() {
        let rec = RefreshRecord {
            revoked: true,
            rotated: true,
            expired: true,
            ..fresh()
        };
        assert_eq!(
            judge_refresh(&rec),
            RefreshVerdict::Reject(RejectReason::FamilyRevoked),
            "the family is already dead; revoking it again makes nothing safer, it only writes \
             noise into an append-only database and drowns new leak alerts"
        );
    }

    /// The three states must not collapse into a boolean — this test pins down
    /// "do not merge Reject and RevokeFamily" directly.
    #[test]
    fn reject_and_revoke_family_are_not_the_same_verdict() {
        let replayed = RefreshRecord {
            rotated: true,
            ..fresh()
        };
        let expired = RefreshRecord {
            expired: true,
            ..fresh()
        };
        assert_ne!(
            judge_refresh(&replayed),
            judge_refresh(&expired),
            "\"refuse\" and \"refuse and revoke the whole family\" must be two different \
             outcomes. Merging them amounts to: after a leak is detected, the attacker can keep \
             using the other tokens on that chain"
        );
        assert!(
            matches!(
                judge_refresh(&replayed),
                RefreshVerdict::RevokeFamily { .. }
            ),
            "a replay must land on the third state"
        );
    }
}

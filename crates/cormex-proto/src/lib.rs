//! Everything that travels on the memory service's wire.
//!
//! This is the contract a client codes against: the browser-based memory
//! browser, a CLI, or a third-party agent talking to `/mcp`. **They are all
//! equal citizens** — the same endpoints, the same DTOs, no private shortcut
//! for a first-party client. That is deliberate: an API whose only caller is
//! its own author is an API nobody notices the rot in.
//!
//! # Why this is a crate of its own rather than types inlined in the server
//!
//! Because a Rust client can depend on *just this* — no sqlx, no pgvector, no
//! embedding backend — and get the definitions rather than re-typing them.
//! Two independently released binaries each maintaining their own copy of the
//! DTOs will drift; it is only a matter of time. And drift shows up as **fields
//! going silently missing**: `serde` ignores unknown fields by default, so one
//! absent `path` merely means the UI draws one row fewer — nothing goes red.
//! One shared crate removes that whole class of mismatch **at compile time**.
//!
//! # What is deliberately *not* here
//!
//! No chat stream, no tool-confirmation ledger, no permission mode. Those
//! belong to whatever agent is driving; this service stores and retrieves
//! memory and never runs a turn. They did live here once — carried over
//! wholesale when this repository was split out — and every one of them
//! documented itself in terms of crates that do not ship with this one, so
//! their rustdoc links pointed at nothing. An integrator reading a memory
//! service's wire protocol should not have to work out which half applies.

pub mod auth;
pub mod capture;
pub mod catalog;
pub mod delegate;
pub mod devices;
pub mod dto;
pub mod episodes;
pub mod import;
/// The LLM proxy's wire protocol. Behind the default-on `llm` feature: it is the only module
/// that names `cormex-llm` (the provider layer), and the capture CLI turns the feature off so
/// that goose-providers and a TLS stack stay out of its binary. See `Cargo.toml`.
#[cfg(feature = "llm")]
pub mod llm;

/// The wire protocol version this side speaks. **Bump it only when the
/// contract changes incompatibly.**
///
/// # Why not compare release version numbers (`cormex_core::VERSION`)
///
/// The overwhelming majority of changes between 0.1.3 and 0.1.4 have nothing
/// to do with the wire protocol. Comparing those amounts to declaring an
/// incompatibility on every patch release — so the check gets written off as
/// noise and switched off, taking the one occasion it would have been useful
/// with it.
///
/// # What counts as "incompatible"
///
/// Only **removing a field, changing what a field means, changing a field's
/// type, or removing an endpoint**. Adding a field does not count: `serde`
/// ignores keys it does not recognize, and every new field carries
/// `#[serde(default)]` (see [`dto::FactDto::source_channel`] and the ones
/// around it).
///
/// Bumping on every added field would mean one purely additive change locks
/// out every old client — clients that would have worked perfectly well.
pub const PROTOCOL_VERSION: u32 = 1;

/// The **oldest** peer protocol version this side can still hold a
/// conversation with.
///
/// Kept separate from [`PROTOCOL_VERSION`] so that "stay backward compatible
/// for a version or two" is something that can actually be expressed. Raise
/// this number when dropping support for an old version — rather than finding
/// out the two sides no longer line up once something breaks.
pub const MIN_PEER_PROTOCOL: u32 = 1;

/// A version has to be able to talk to itself.
///
/// It reads like a tautology, but what it catches is a slip that really
/// happens: while bumping [`PROTOCOL_VERSION`] you raise [`MIN_PEER_PROTOCOL`]
/// one step too far, and now the new version refuses even itself.
///
/// This is **untestable in a dev environment** — there both sides come out of
/// the same build, and both constants are wrong in the same way. Hence compile
/// time: the build goes red on the spot, instead of the problem surfacing when
/// two processes shake hands on some machine.
const _: () = assert!(
    MIN_PEER_PROTOCOL <= PROTOCOL_VERSION,
    "MIN_PEER_PROTOCOL exceeds PROTOCOL_VERSION — this version would refuse to talk to itself"
);

/// Whether the two ends' protocols can hold a conversation.
///
/// # Both directions have to be checked
///
/// Checking one end is not checking:
///
/// - `peer_protocol < MIN_PEER_PROTOCOL` — **the peer is too old**, it will not
///   speak what I need.
/// - `PROTOCOL_VERSION < peer_min_peer` — **I am too old**, the peer has
///   already dropped support for me. Only the peer knows this one, which is why
///   it has to report its own floor as well.
///
/// The cost of skipping the second check: after the server upgrades and drops
/// support for the old protocol, old clients still connect cheerfully and then
/// fail on one specific request in a way nobody can make sense of.
///
/// # Errors
/// On incompatibility, returns a sentence **meant for a human**: both sides'
/// numbers, and which side needs upgrading.
pub fn check_peer(peer_protocol: u32, peer_min_peer: u32, peer_label: &str) -> Result<(), String> {
    if peer_protocol < MIN_PEER_PROTOCOL {
        return Err(format!(
            "{peer_label} speaks protocol v{peer_protocol}, but this side supports no older \
             than v{MIN_PEER_PROTOCOL}. Upgrade {peer_label} to a newer version."
        ));
    }
    if PROTOCOL_VERSION < peer_min_peer {
        return Err(format!(
            "this side speaks protocol v{PROTOCOL_VERSION}, but {peer_label} requires no older \
             than v{peer_min_peer}. Upgrade this machine's side to a newer version."
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both directions have to actually block, and have to say which side to
    /// upgrade.
    ///
    /// Blocking only one end is the easiest version to write, and the useless
    /// one: when the server unilaterally drops support for the old protocol,
    /// old clients go right on connecting.
    #[test]
    fn both_directions_are_checked_and_say_which_side_to_upgrade() {
        assert!(check_peer(PROTOCOL_VERSION, MIN_PEER_PROTOCOL, "the remote").is_ok());

        let too_old = check_peer(MIN_PEER_PROTOCOL - 1, 0, "the remote")
            .expect_err("a peer that is too old must be blocked");
        assert!(
            too_old.contains("Upgrade the remote"),
            "the error has to say which side to upgrade, got: {too_old}"
        );

        let we_are_old = check_peer(PROTOCOL_VERSION, PROTOCOL_VERSION + 1, "the remote")
            .expect_err(
                "must block when the peer no longer supports this side — only the peer knows this",
            );
        assert!(
            we_are_old.contains("this machine"),
            "in this direction the user should upgrade the **local** side, got: {we_are_old}"
        );
    }

    /// **The 0.1.3 currently running in production has to be judged
    /// compatible.**
    ///
    /// The JSON below is not made up; it is the verbatim body scraped off the
    /// `/health` of a real 0.1.3 daemon — it has neither `protocol` nor
    /// `min_peer_protocol`, because this check did not exist back then.
    ///
    /// Defaulting those to 0 is the most natural spelling here, and the most
    /// lethal: on the day the check ships, every desktop client refuses to
    /// start because "the remote speaks protocol v0, too old" — when in fact
    /// they would work perfectly, no contract having changed at all.
    ///
    /// The correct meaning of an absent field is "a daemon predating this
    /// check", i.e. v1.
    #[test]
    fn a_daemon_that_predates_this_check_is_still_compatible() {
        let real_0_1_3 = r#"{"status":"ok","version":"0.1.3","database":"not_wired",
                             "blob_backend":"s3","auth":"token"}"#;
        let peer: crate::dto::PeerProtocol =
            serde_json::from_str(real_0_1_3).expect("an old daemon's /health must still parse");

        assert!(
            check_peer(peer.protocol, peer.min_peer_protocol, "the remote").is_ok(),
            "0.1.3 judged incompatible — the day this check ships it would lock out every \
             desktop client. Parsed protocol={} min_peer={}",
            peer.protocol,
            peer.min_peer_protocol
        );
    }

    /// However the other fields on `/health` change, they must not disturb the
    /// protocol check.
    ///
    /// Taking just the two numbers instead of the whole `Health` is exactly for
    /// this: database status, object storage backend and vectorization health
    /// all live in the same response, and they change far more often than the
    /// protocol version does. Parse with the whole `Health` and any change to
    /// those fields can make the protocol check itself fail to parse — and the
    /// way that failure presents is precisely "nobody blocks an incompatible
    /// peer".
    #[test]
    fn unrelated_health_fields_cannot_break_the_check() {
        let future = r#"{"status":"degraded","version":"9.9.9","protocol":1,
                         "min_peer_protocol":1,"database":{"pool":"nested object"},
                         "something_invented_later":[1,2,3]}"#;
        let peer: crate::dto::PeerProtocol = serde_json::from_str(future)
            .expect("unrelated fields must not affect the protocol check");
        assert_eq!((peer.protocol, peer.min_peer_protocol), (1, 1));
    }

    /// Two processes from the same build must be able to shake hands.
    ///
    /// The ordering of the two constants is guaranteed at compile time by the
    /// `const _: () = assert!(…)` at the top of the crate; this test goes
    /// through the **actual handshake function**, because getting the ordering
    /// right is not the same as `check_peer` using them right — a reversed
    /// comparison or swapped arguments can make two same-version processes
    /// reject each other while both constants are perfectly correct.
    #[test]
    fn a_build_is_always_compatible_with_itself() {
        assert!(
            check_peer(PROTOCOL_VERSION, MIN_PEER_PROTOCOL, "the peer").is_ok(),
            "two processes from the same build cannot shake hands — the comparison in \
             check_peer is the wrong way round"
        );
    }
}

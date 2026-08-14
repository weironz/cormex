//! Identifiers.
//!
//! ULIDs everywhere: 26 characters, millisecond timestamp + random bits.
//! Clients can generate them offline with no central issuer, and they sort by
//! time for free — that is the precondition for conflict-free writes from
//! multiple devices under an append-only data model.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A ULID identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(ulid::Ulid);

impl Id {
    /// Generate a new ID from the current time.
    #[must_use]
    pub fn new() -> Self {
        Self(ulid::Ulid::generate())
    }

    /// **Deterministically** derive an ID from a moment plus a seed. The same
    /// inputs always produce the same result.
    ///
    /// # This exists for import
    ///
    /// Pouring three years of ChatGPT / Claude history in is a job that *will*
    /// get interrupted: the machine shuts down, the network drops, Ctrl-C, the
    /// process gets killed. And `POST /episodes` already deduplicates by id —
    /// so as long as a re-run computes **the same batch of ids**, resuming is
    /// free and needs no progress file at all.
    ///
    /// [`Self::new`] cannot do that: it draws fresh random bits every time, so
    /// re-running means importing the entire history a second time.
    ///
    /// # The time part is the message's **original moment**, not now
    ///
    /// The high 48 bits of a ULID are a millisecond timestamp, and that
    /// ordering is relied on all over the system: episodes sort by it, and the
    /// recency retrieval path computes freshness from it. With `Id::new()`, a
    /// conversation from three years ago would carry today's timestamp — and
    /// then "what did I talk about recently" would forever return that
    /// three-year-old history.
    ///
    /// # How to pick a seed
    ///
    /// The caller assembles a string that is **unique within this use**, e.g.
    /// `"chatgpt:{conversation_id}:{message_id}"`. The prefix is necessary:
    /// without a source, two messages that happen to share a number across two
    /// platforms would collide into the same id.
    ///
    /// # Out-of-range moments clamp rather than wrap
    ///
    /// Anything before 1970 (negative) or after the year 10889 falls outside
    /// what 48 bits can represent. A straight conversion **silently wraps** to
    /// a timestamp that looks perfectly normal. Here we clamp to the boundary
    /// explicitly: the ordering will be wrong, but the id stays deterministic,
    /// stays unique, and does not masquerade as some other year.
    #[must_use]
    pub fn derived(at: chrono::DateTime<chrono::Utc>, seed: &str) -> Self {
        use sha2::{Digest as _, Sha256};

        /// The ULID time part is 48 bits of milliseconds.
        const MAX_TIME_MS: i64 = (1 << 48) - 1;

        let ms = at.timestamp_millis().clamp(0, MAX_TIME_MS) as u64;

        // Domain-separation prefix: this function may get called for other
        // purposes later, and two purposes computing the same id from the same
        // seed string is the kind of thing you only discover once the data
        // stops adding up
        let mut h = Sha256::new();
        h.update(b"cortex/id/derived/v1\0");
        h.update(seed.as_bytes());
        let digest = h.finalize();

        // Take the first 16 bytes as a u128; `from_parts` masks it down to 80
        // bits itself
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&digest[..16]);
        Self(ulid::Ulid::from_parts(ms, u128::from_be_bytes(buf)))
    }

    /// The generation moment encoded in this ID.
    #[must_use]
    pub fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(self.0.datetime())
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Id {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(ulid::Ulid::from_str(s)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_roundtrips_through_string() {
        let id = Id::new();
        let parsed: Id = id
            .to_string()
            .parse()
            .expect("a generated ID should parse back");
        assert_eq!(id, parsed);
    }

    #[test]
    fn ids_sort_in_generation_order() {
        let first = Id::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = Id::new();
        assert!(first < second, "ULIDs should sort by generation time");
    }

    #[test]
    fn id_renders_as_26_chars() {
        assert_eq!(Id::new().to_string().len(), 26);
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse().expect("time constant used by the test")
    }

    /// **The same inputs always yield the same id.** Resumable import rests
    /// entirely on this one property.
    ///
    /// It is more fragile than it looks: if somebody swaps the hash for
    /// `DefaultHasher` (the most convenient choice at hand), this test stays
    /// green **within a single run** — because `DefaultHasher`'s randomization
    /// is per process. The real failure only shows up after compiling with a
    /// different Rust version, and it shows up as "re-running the import
    /// poured three years of history in a second time".
    ///
    /// So this also pins a **literal**: the moment the algorithm changes, it
    /// goes red on the spot.
    #[test]
    fn the_same_seed_always_yields_the_same_id() {
        let t = at("2023-04-15T08:30:00Z");
        let a = Id::derived(t, "chatgpt:conv-1:msg-7");
        let b = Id::derived(t, "chatgpt:conv-1:msg-7");
        assert_eq!(
            a, b,
            "same input must give same output, otherwise re-running an import \
             redoes the whole thing"
        );

        assert_eq!(
            a.to_string(),
            "01GY20J4T0X1CZNXCWNNQX3E72",
            "the derivation algorithm changed. This is not an implementation \
             detail you can casually adjust — users who already imported will \
             get an entire duplicate history on their next run"
        );
    }

    /// Different seeds must give different ids, and **differing by a single
    /// character counts as different**.
    #[test]
    fn different_seeds_do_not_collide() {
        let t = at("2023-04-15T08:30:00Z");
        let ids = [
            Id::derived(t, "chatgpt:conv-1:msg-7"),
            Id::derived(t, "chatgpt:conv-1:msg-8"),
            Id::derived(t, "chatgpt:conv-2:msg-7"),
            // Same message number, different platform. Without the source
            // prefix these two would collide
            Id::derived(t, "claude:conv-1:msg-7"),
        ];
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            unique.len(),
            ids.len(),
            "seeds differed yet the ids collided"
        );
    }

    /// The time part must be the message's **original moment**, and the
    /// ordering must come out right.
    ///
    /// With `Id::new()` a conversation from three years ago would carry
    /// today's timestamp — and then the "what did I talk about recently"
    /// retrieval path would forever return that three-year-old history.
    #[test]
    fn the_time_part_is_the_original_moment() {
        let old = at("2023-04-15T08:30:00Z");
        let recent = at("2026-08-09T12:00:00Z");

        let a = Id::derived(old, "x");
        assert_eq!(
            a.timestamp().timestamp_millis(),
            old.timestamp_millis(),
            "the time part did not preserve the original moment"
        );

        assert!(
            a < Id::derived(recent, "x"),
            "ULIDs must sort by the original time — episode ordering and \
             recency retrieval both depend on it"
        );
    }

    /// Out-of-range moments clamp to the boundary and **do not wrap**.
    ///
    /// With a plain `as u64`, a negative value from before 1970 turns into a
    /// huge positive number and is then masked to 48 bits — yielding a
    /// timestamp that looks entirely normal while pointing at a different
    /// year. Export files containing `created_at: 0`, or a bogus negative
    /// value, are not rare.
    #[test]
    fn out_of_range_moments_clamp_instead_of_wrapping() {
        let ancient = at("1900-01-01T00:00:00Z");
        let id = Id::derived(ancient, "x");
        assert_eq!(
            id.timestamp().timestamp_millis(),
            0,
            "anything before 1970 should clamp to the epoch rather than \
             wrapping to some moment in the future"
        );
        // Still deterministic, still unique
        assert_eq!(id, Id::derived(ancient, "x"));
        assert_ne!(id, Id::derived(ancient, "y"));
    }
}

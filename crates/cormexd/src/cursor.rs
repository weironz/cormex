//! Encoding and decoding of the pagination cursor for session messages.
//!
//! The cursor sits on the two columns `(occurred_at, id)`; the reasoning is in
//! [`cormex_store::EpisodeCursor`]: `occurred_at` comes from the client's clock,
//! two rows landing in the same microsecond is routine, and a single-column
//! cursor will either skip or repeat the rows sharing that instant.
//!
//! # Why plaintext rather than base64
//!
//! base64 can only create the illusion of opacity: for a client to forge a
//! cursor, one base64 decode is enough. The real defence is **server-side
//! validation** (see [`decode`]) — both fields in the cursor must pass a shape
//! check before they are handed to SQL. Since security does not rest on the
//! encoding, pick the readable one: while working a "pagination is stuck"
//! ticket, the log shows directly which instant the client was holding.

use chrono::{DateTime, Utc};
use cormex_core::{CormexError, Result};
use cormex_store::EpisodeCursor;

/// Separator between the two segments. Neither RFC3339 nor a ULID contains it,
/// so a single split from the right is unambiguous.
const SEP: char = '|';

/// Character count of a ULID.
const ULID_LEN: usize = 26;

/// Encode as `<RFC3339>|<ULID>`.
#[must_use]
pub fn encode(occurred_at: DateTime<Utc>, id: &str) -> String {
    format!("{}{SEP}{id}", occurred_at.to_rfc3339())
}

/// Parse a cursor sent back by the client.
///
/// # Why the ULID shape is validated
///
/// `episodes.id` has type `ulid` (a domain with a regex CHECK). Comparing a
/// malformed string against it makes Postgres raise a domain constraint error,
/// and that error path surfaces over HTTP as a 500 — the client is told "the
/// server broke" when the truth is "your cursor is malformed". Catching it here
/// turns it into a 400 whose message says plainly that the cursor is the
/// problem.
///
/// # Errors
///
/// Missing separator, a timestamp that is not valid RFC3339, or an id that is
/// not 26 characters of Crockford base32.
pub fn decode(raw: &str) -> Result<EpisodeCursor> {
    // Split from the **right**: RFC3339 itself contains no '|', but splitting
    // from the left would silently change the meaning here the day a third
    // segment is added to the cursor, whereas splitting from the right only
    // ever has to recognise the last segment
    let (time_part, id) = raw.rsplit_once(SEP).ok_or_else(|| {
        CormexError::Invalid(format!("malformed cursor, missing {SEP:?}: {raw:?}"))
    })?;

    let occurred_at = DateTime::parse_from_rfc3339(time_part)
        .map_err(|e| {
            CormexError::Invalid(format!(
                "the time part of the cursor is not valid RFC3339: {e}"
            ))
        })?
        .with_timezone(&Utc);

    if id.len() != ULID_LEN || !id.bytes().all(is_crockford_upper) {
        return Err(CormexError::Invalid(format!(
            "the id in the cursor is not a 26-character uppercase Crockford base32 ULID: {id:?}"
        )));
    }

    Ok(EpisodeCursor {
        occurred_at,
        id: id.to_owned(),
    })
}

/// The uppercase Crockford base32 alphabet: I / L / O / U are excluded.
/// Kept identical to the regex on the `ulid` domain in
/// `migrations/20260807000001_init.sql`.
const fn is_crockford_upper(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn roundtrips_through_encode_decode() {
        let t: DateTime<Utc> = "2026-08-07T11:22:33.456789Z"
            .parse()
            .expect("a valid timestamp");
        let got = decode(&encode(t, SAMPLE_ID)).expect("a cursor we encoded ourselves must decode");
        assert_eq!(got.id, SAMPLE_ID);
        assert_eq!(
            got.occurred_at, t,
            "microseconds must round-trip losslessly — losing precision makes messages within \
             the same second repeat or disappear while paginating"
        );
    }

    #[test]
    fn a_malformed_id_is_rejected_before_it_reaches_sql() {
        // episodes.id is a ulid domain with a CHECK; a malformed value hitting
        // it produces a 500. Catching it here is what makes an intelligible 400
        // possible
        for bad in [
            "2026-08-07T00:00:00Z|short",
            // I / L / O / U are not in the Crockford alphabet
            "2026-08-07T00:00:00Z|01ARZ3NDEKTSV4RRFFQ69G5FAI",
            "2026-08-07T00:00:00Z|01arz3ndektsv4rrffq69g5fav",
            "2026-08-07T00:00:00Z|01ARZ3NDEKTSV4RRFFQ69G5FA'",
        ] {
            assert!(
                decode(bad).is_err(),
                "the malformed cursor {bad:?} must be rejected before it reaches SQL"
            );
        }
    }

    #[test]
    fn a_missing_or_broken_timestamp_is_rejected() {
        assert!(
            decode(SAMPLE_ID).is_err(),
            "a cursor with no separator must be rejected"
        );
        assert!(
            decode(&format!("yesterday|{SAMPLE_ID}")).is_err(),
            "a time part that is not RFC3339 must be rejected"
        );
    }

    #[test]
    fn the_separator_is_taken_from_the_right() {
        // Splitting from the left would silently change the meaning here the day
        // a third segment is added to the cursor. Pin the direction down now, so
        // that breakage is not deferred until runtime
        let raw = format!("2026-08-07T00:00:00Z{SEP}{SAMPLE_ID}");
        assert_eq!(decode(&raw).expect("should parse").id, SAMPLE_ID);
    }
}

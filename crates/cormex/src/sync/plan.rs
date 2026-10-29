//! The decision: given what the server has and what is on disk, what next?
//!
//! Design: docs/design/session-archive-architecture.md §5.3 step 4.
//!
//! # Why this is a pure function
//!
//! It is the one place in the client where a mistake is **silent and
//! permanent**: "append" when the file was rewritten uploads a second copy of
//! the tail under the same offsets (which the server rejects, loudly), but
//! "skip" when the file grew loses those bytes until somebody notices the
//! conversation is missing its end. Every branch is a rule from the design, and
//! every rule gets a case here — the card asks for coverage "close to 100%",
//! and a pure function is the only way to get it without a server and a
//! filesystem.
//!
//! # The three observations it needs
//!
//! - `size`: the file's current length
//! - `tail_sha256`: the hash of the 4 KiB ending at the **uploaded** offset
//! - `whole_sha256`: the whole-file hash, only for snapshot sources
//!
//! The tail hash is what makes "append" cheap: an in-place rewrite of the same
//! length is invisible to a size comparison, and re-reading the whole file on
//! every run would defeat the point of a hook-driven client.

use crate::state::FileState;

/// What the client observed about the file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    /// The current length, in logical (uncompressed) bytes.
    pub size: i64,
    /// The sha256 of `[uploaded_offset - 4096, uploaded_offset)`, when the file
    /// is at least that long.
    pub tail_sha256: Option<String>,
    /// The whole-file hash. Only computed for snapshot sources, where the
    /// decision is "did it change at all".
    pub whole_sha256: Option<String>,
    /// Whether the source is append-only or snapshot.
    pub snapshot: bool,
}

/// What to do with one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Never seen: register it, then upload from 0.
    Register,
    /// Upload `[from, size)`.
    Append { from: i64 },
    /// Open a new generation for this reason, then upload from 0.
    NewGeneration { reason: NewGenerationReason },
    /// Upload the whole file as a snapshot.
    Snapshot,
    /// Nothing to do.
    Skip,
}

/// Why a new generation is being opened. Mirrors `cormex_proto::capture::
/// GenerationReason` minus `initial`, which only registration uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewGenerationReason {
    Rewrite,
    Truncate,
    Snapshot,
}

/// Decide.
///
/// The rules, in the order they must be checked:
///
/// ```text
/// never seen                     → Register
/// snapshot source:
///     whole hash unchanged       → Skip
///     otherwise                  → NewGeneration(Snapshot)   (or Snapshot on the first)
/// append source:
///     size < uploaded_offset     → NewGeneration(Truncate)
///     tail hash differs          → NewGeneration(Rewrite)
///     size == uploaded_offset    → Skip
///     otherwise                  → Append { from: uploaded_offset }
/// ```
///
/// # Why truncation is checked before the tail hash
///
/// A truncated file may have fewer than 4096 bytes before the old upload point,
/// so the tail hash is either unavailable or describes bytes that no longer
/// exist. Checking the size first means the tail comparison only runs when it
/// is meaningful.
#[must_use]
pub fn decide(local: Option<&FileState>, observed: &Observed) -> Action {
    let Some(local) = local else {
        return Action::Register;
    };

    if observed.snapshot {
        // A snapshot source is rewritten wholesale; the question is only
        // "changed or not". `whole_sha256` is `None` when it has never been
        // uploaded, which counts as changed.
        return match (&local.whole_sha256, &observed.whole_sha256) {
            (Some(previous), Some(current)) if previous == current => Action::Skip,
            _ => Action::NewGeneration {
                reason: NewGenerationReason::Snapshot,
            },
        };
    }

    if observed.size < local.uploaded_offset {
        return Action::NewGeneration {
            reason: NewGenerationReason::Truncate,
        };
    }

    // The tail only exists once something has been uploaded; with nothing
    // uploaded there is nothing to compare, and the file can only have grown
    if local.uploaded_offset > 0 {
        let matches = match (&local.tail_sha256, &observed.tail_sha256) {
            (Some(previous), Some(current)) => previous == current,
            // A state row with an offset but no tail hash predates this
            // version, or the file is shorter than the window. Either way the
            // safe answer is "assume it changed": a spurious new generation
            // costs a duplicate copy of one file, while a missed rewrite
            // silently concatenates two different files
            _ => false,
        };
        if !matches {
            return Action::NewGeneration {
                reason: NewGenerationReason::Rewrite,
            };
        }
    }

    if observed.size == local.uploaded_offset {
        Action::Skip
    } else {
        Action::Append {
            from: local.uploaded_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(uploaded: i64, tail: Option<&str>) -> FileState {
        FileState {
            local_key: "claude_code|/p/s.jsonl".into(),
            agent: "claude_code".into(),
            path: "/p/s.jsonl".into(),
            file_id: Some("01F".into()),
            generation_id: Some("01G".into()),
            generation_ordinal: Some(0),
            uploaded_offset: uploaded,
            tail_sha256: tail.map(str::to_owned),
            whole_sha256: None,
            last_size: Some(uploaded),
            last_mtime_ns: None,
            missing_reported: false,
            last_ok_at: None,
        }
    }

    fn append_obs(size: i64, tail: Option<&str>) -> Observed {
        Observed {
            size,
            tail_sha256: tail.map(str::to_owned),
            whole_sha256: None,
            snapshot: false,
        }
    }

    #[test]
    fn a_file_never_seen_is_registered() {
        assert_eq!(
            decide(None, &append_obs(100, None)),
            Action::Register,
            "a file with no local state must be registered first -- there is no generation to \
             append to"
        );
    }

    #[test]
    fn a_grown_file_is_appended_from_where_it_stopped() {
        assert_eq!(
            decide(Some(&state(100, Some("aa"))), &append_obs(250, Some("aa"))),
            Action::Append { from: 100 },
            "the tail matches, so the bytes before the upload point are unchanged: append the \
             rest rather than re-reading the file"
        );
    }

    #[test]
    fn an_unchanged_file_is_skipped() {
        assert_eq!(
            decide(Some(&state(100, Some("aa"))), &append_obs(100, Some("aa"))),
            Action::Skip,
            "same size and same tail: nothing to do. Re-uploading would be a no-op on the server, \
             but it would still cost the round trip on every hook"
        );
    }

    #[test]
    fn a_shrunk_file_opens_a_truncate_generation() {
        assert_eq!(
            decide(Some(&state(500, Some("aa"))), &append_obs(100, None)),
            Action::NewGeneration {
                reason: NewGenerationReason::Truncate
            },
            "the file is shorter than what was uploaded: it was truncated, and the old bytes \
             must stay in the old generation"
        );
    }

    #[test]
    fn a_rewritten_file_opens_a_rewrite_generation() {
        assert_eq!(
            decide(Some(&state(100, Some("aa"))), &append_obs(100, Some("bb"))),
            Action::NewGeneration {
                reason: NewGenerationReason::Rewrite
            },
            "same length, different tail: the file was rewritten in place. A size comparison \
             cannot see this, which is why the tail hash exists"
        );
    }

    /// A state row with an offset but no tail hash assumes a rewrite.
    ///
    /// The safe direction: a spurious generation costs one duplicate copy of one
    /// file, while a missed rewrite concatenates two different files into one
    /// generation and the result is not a valid transcript at all.
    #[test]
    fn a_missing_tail_hash_assumes_a_rewrite() {
        assert_eq!(
            decide(Some(&state(100, None)), &append_obs(100, Some("bb"))),
            Action::NewGeneration {
                reason: NewGenerationReason::Rewrite
            }
        );
    }

    /// With nothing uploaded yet, there is no tail to compare — so a file that
    /// grew is simply appended from 0.
    #[test]
    fn a_file_with_nothing_uploaded_appends_from_zero() {
        assert_eq!(
            decide(Some(&state(0, None)), &append_obs(100, None)),
            Action::Append { from: 0 }
        );
    }

    #[test]
    fn a_changed_snapshot_opens_a_snapshot_generation() {
        let mut local = state(100, None);
        local.whole_sha256 = Some("old".into());
        let observed = Observed {
            size: 100,
            tail_sha256: None,
            whole_sha256: Some("new".into()),
            snapshot: true,
        };
        assert_eq!(
            decide(Some(&local), &observed),
            Action::NewGeneration {
                reason: NewGenerationReason::Snapshot
            }
        );
    }

    #[test]
    fn an_unchanged_snapshot_is_skipped() {
        let mut local = state(100, None);
        local.whole_sha256 = Some("same".into());
        let observed = Observed {
            size: 100,
            tail_sha256: None,
            whole_sha256: Some("same".into()),
            snapshot: true,
        };
        assert_eq!(
            decide(Some(&local), &observed),
            Action::Skip,
            "a snapshot source that has not changed must not be re-uploaded -- a SQLite row \
             source would otherwise send every row on every run"
        );
    }

    /// A snapshot that has never been uploaded is uploaded.
    #[test]
    fn a_snapshot_with_no_previous_hash_is_uploaded() {
        let local = state(0, None);
        let observed = Observed {
            size: 10,
            tail_sha256: None,
            whole_sha256: Some("new".into()),
            snapshot: true,
        };
        assert_eq!(
            decide(Some(&local), &observed),
            Action::NewGeneration {
                reason: NewGenerationReason::Snapshot
            }
        );
    }
}

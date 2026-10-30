//! Uploading one logical file: chunking, the 409 dance, and the budget.
//!
//! Design: docs/design/session-archive-architecture.md §5.3 steps 5–6.
//!
//! # The 409 is two different situations
//!
//! `OffsetMismatch { expected }` means the server's generation ends somewhere
//! other than where the client thought. The two directions need opposite
//! responses, and getting either wrong is silent:
//!
//! - **`expected` > local offset** — the server already has more than the client
//!   recorded. That happens when a previous run's 200 was lost on the way back.
//!   The client must **skip ahead**; re-sending would be a replay the server
//!   accepts, but the local state would never converge.
//! - **`expected` < local offset** — the client's state is ahead, which happens
//!   after a state database restore from an older backup. The client must
//!   **rewind** and re-send from `expected`.
//!
//! # The budget is checked between chunks, not during one
//!
//! A chunk is at most 4 MiB, so the overshoot is bounded. Checking mid-chunk
//! would mean abandoning a request the server may already have committed —
//! which is exactly the "the 200 was lost" case above, and it would happen on
//! every run that hits the budget.

use anyhow::Result;
use cormex_proto::capture::{ChunkAck, MAX_CHUNK_BYTES};

use crate::state::{FileState, StateDb};
use crate::sync::api::{CaptureApi, PutOutcome};
use crate::sync::reader::LogicalReader;

/// How many times one chunk may be retried before the run gives up on the file.
///
/// Three, with 1 s / 2 s / 4 s backoff. A hook has 1.5 seconds in total, so
/// anything longer belongs to the next run — the queue keeps the work.
const MAX_RETRIES: u32 = 3;

/// What one file's upload did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Uploaded {
    pub bytes: i64,
    pub chunks: usize,
    /// Whether the budget ran out before the file was finished.
    pub deferred: bool,
}

/// Upload `[from, len)` of a generation.
///
/// # Errors
/// The file cannot be read, or the server refused a chunk in a way the engine
/// cannot recover from.
pub async fn upload(
    api: &dyn CaptureApi,
    db: &StateDb,
    reader: &LogicalReader,
    state: &mut FileState,
    generation_id: &str,
    from: i64,
    budget: &mut Budget,
) -> Result<Uploaded> {
    let mut out = Uploaded::default();
    let mut offset = from;

    while offset < i64::try_from(reader.len()).unwrap_or(i64::MAX) {
        if budget.exhausted() {
            out.deferred = true;
            break;
        }

        let want = usize::try_from(
            (i64::try_from(reader.len()).unwrap_or(i64::MAX) - offset).min(MAX_CHUNK_BYTES as i64),
        )
        .unwrap_or(MAX_CHUNK_BYTES);
        let body = reader.read_range(u64::try_from(offset).unwrap_or(0), want)?;
        if body.is_empty() {
            break;
        }
        let sha = crate::sync::reader::sha256_hex(&body);
        let observed_at = chrono::Utc::now().to_rfc3339();

        let mut attempt = 0;
        let outcome = loop {
            match api
                .put_chunk(generation_id, offset, body.clone(), &sha, &observed_at)
                .await
            {
                Ok(o) => break o,
                Err(e) if attempt < MAX_RETRIES => {
                    attempt += 1;
                    // 1 s, 2 s, 4 s. The hook's host will have moved on long
                    // before this finishes; the queue is what makes the work
                    // survive
                    let backoff = std::time::Duration::from_secs(1 << (attempt - 1));
                    tracing::debug!(error = %e, attempt, "chunk upload failed; backing off");
                    tokio::time::sleep(backoff).await;
                }
                Err(e) => return Err(e),
            }
        };

        match outcome {
            PutOutcome::Accepted(ack) => {
                offset = ack.end_offset;
                out.chunks += 1;
                out.bytes += i64::try_from(body.len()).unwrap_or(0);
                budget.spend(body.len() as u64);
                // **The offset and the tail hash move together, in one write.**
                // Writing them separately leaves a window where the offset says
                // "uploaded" and the tail hash still describes the old bytes —
                // and the next run would then decide the file was rewritten
                state.uploaded_offset = offset;
                state.tail_sha256 = reader.tail_hash(u64::try_from(offset).unwrap_or(0))?;
                state.last_ok_at = Some(chrono::Utc::now().to_rfc3339());
                db.put_file(state)?;
            }
            PutOutcome::OffsetMismatch { expected } => {
                if expected > offset {
                    // The server has more than we recorded: a previous 200 was
                    // lost. Skip ahead rather than re-sending.
                    tracing::debug!(
                        from = offset,
                        to = expected,
                        "the server is ahead; skipping"
                    );
                    offset = expected;
                    state.uploaded_offset = offset;
                    state.tail_sha256 = reader.tail_hash(u64::try_from(offset).unwrap_or(0))?;
                    db.put_file(state)?;
                } else {
                    // The client is ahead: rewind and re-send from where the
                    // server actually is
                    tracing::debug!(
                        from = offset,
                        to = expected,
                        "the client is ahead; rewinding"
                    );
                    offset = expected;
                    state.uploaded_offset = offset;
                    state.tail_sha256 = reader.tail_hash(u64::try_from(offset).unwrap_or(0))?;
                    db.put_file(state)?;
                }
            }
        }
    }

    Ok(out)
}

/// The run's time and byte budget.
///
/// Defaults from the design: 60 seconds and 512 MiB. The first backfill does
/// not go through here — `cormex init` runs it with the budget lifted, because
/// a user who just said "yes, capture everything" is not well served by a
/// client that stops after a minute.
#[derive(Debug, Clone)]
pub struct Budget {
    deadline: std::time::Instant,
    bytes_left: u64,
}

impl Budget {
    /// A budget of `secs` seconds and `bytes` bytes.
    #[must_use]
    pub fn new(secs: u64, bytes: u64) -> Self {
        Self {
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(secs),
            bytes_left: bytes,
        }
    }

    /// The design's default.
    #[must_use]
    pub fn default_budget() -> Self {
        Self::new(60, 512 << 20)
    }

    /// An effectively unlimited budget, for `cormex init`'s backfill.
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(24 * 3600, u64::MAX)
    }

    /// Whether the budget is spent.
    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.bytes_left == 0 || std::time::Instant::now() >= self.deadline
    }

    fn spend(&mut self, bytes: u64) {
        self.bytes_left = self.bytes_left.saturating_sub(bytes);
    }
}

/// Turn the server's ack into the local state's next offset.
///
/// Exists so that the "advance offset and tail together" rule has one
/// implementation; the upload loop and any future caller share it.
#[must_use]
pub fn next_offset(ack: &ChunkAck, fallback: i64) -> i64 {
    if ack.end_offset >= 0 {
        ack.end_offset
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_budget_matches_the_design() {
        let b = Budget::default_budget();
        assert!(!b.exhausted(), "a fresh budget is not spent");
        assert_eq!(
            b.bytes_left,
            512 << 20,
            "the design says 512 MiB; a smaller default would silently cap the first sync"
        );
    }

    #[test]
    fn a_zero_byte_budget_is_immediately_exhausted() {
        assert!(
            Budget::new(60, 0).exhausted(),
            "a zero-byte budget must stop before the first chunk, not after it"
        );
    }

    #[test]
    fn spending_reduces_the_remainder_and_never_underflows() {
        let mut b = Budget::new(60, 100);
        b.spend(60);
        assert!(!b.exhausted());
        b.spend(60);
        assert!(
            b.exhausted(),
            "overspending must saturate at zero, not wrap"
        );
    }

    #[test]
    fn the_unlimited_budget_is_not_exhausted_by_a_large_file() {
        let mut b = Budget::unlimited();
        b.spend(1 << 40);
        assert!(!b.exhausted());
    }
}

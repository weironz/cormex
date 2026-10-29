//! The transcription rescanner: bring back into memory the media that presigned
//! direct upload skipped.
//!
//! # Why it is needed
//!
//! There are two upload paths:
//!
//! | Path | Did the server see the bytes | Transcription |
//! |---|---|---|
//! | relayed via `POST /blobs` (≤ 32 MiB) | yes | queued right there during upload |
//! | `presign` + `commit` (> 32 MiB) | **never** | nothing to trigger it |
//!
//! In other words **large files are pure archive and cannot be retrieved**. And
//! large files are precisely the valuable ones (long screenshots, scans, screen
//! recordings) — exactly what the user goes looking for three months later.
//!
//! This module treats the `blobs` table as the authoritative inventory,
//! periodically finding objects that are "a transcribable type + have no
//! `blob_transcripts` row at all", pulling them back from object storage and
//! running the pipeline again. It also catches the cases on the relayed path
//! where transcription failed or a process restart lost the job.
//!
//! # Four things that must hold
//!
//! 1. **Respect redaction.** Blobs carrying a tombstone are excluded by the
//!    candidate query itself (see
//!    [`cormex_store::Store::blobs_missing_transcript`]), and
//!    [`cormex_memory::transcribe::RedactionGuard`] rechecks inside the
//!    pipeline. Without the first of those, the rescanner would periodically
//!    pull erased content out of object storage into memory and send it over the
//!    network — and even if the result is ultimately discarded, the secret has
//!    already left the process.
//! 2. **Rate-limit and bound concurrency.** Pulling several hundred large files
//!    at once blows out memory and the vision quota simultaneously. All three
//!    gates — batch size, concurrency, per-object size — are constants below.
//! 3. **Failures may be retried, but one bad blob must not be retried
//!    forever.** See [`Ledger`].
//! 4. **With no `CORMEX_VISION_PROVIDER` configured, the whole thing is off**,
//!    consistent with the upload path.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use cormex_core::CormexError;

use crate::blobs::MediaStore;
use crate::live::{Live, TranscribeOutcome};

/// Interval between rescan rounds.
///
/// Five minutes: the upper bound on the delay between a direct upload finishing
/// and the object becoming retrievable. Making it shorter is pointless — a
/// single vision call already takes on the order of ten seconds — while making
/// it longer turns "the large image I just uploaded is not searchable" from
/// "wait a moment" into "is this broken?".
const SCAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long after startup the first round runs.
///
/// Not immediately at startup: right after the process comes up, the connection
/// pool is warming and the embedding model may still be loading, so pushing a
/// batch of vision calls in only makes startup slower — and the rescan is not
/// urgent to begin with.
const FIRST_SCAN_DELAY: Duration = Duration::from_secs(30);

/// Maximum number of blobs handled in one round.
///
/// The gate applies **per round**, not to the total: a backlog of a thousand is
/// consumed over a few dozen rounds, while the memory and quota footprint at any
/// instant stays bounded.
const BATCH: usize = 8;

/// How many transcriptions run at once.
///
/// 2 rather than 8: every running task holds the entire byte payload in memory
/// (see [`MAX_BYTES`]), and vision providers usually rate-limit concurrency on
/// their side too — saturating it just earns a batch of 429s, after which all
/// of these blobs get recorded as failures.
const CONCURRENCY: usize = 2;

/// Per-blob size ceiling.
///
/// Transcription has to read the whole payload into memory (`BlobStore::get` has
/// no streaming interface), and a 2 GB screen recording can kill the process
/// outright. Anything larger is **skipped permanently** and said so out loud —
/// skipping silently leaves "why is this video not searchable" with nothing to
/// investigate.
///
/// 64 MiB is twice the direct-upload threshold (32 MiB): that band is exactly
/// what the rescan exists to rescue.
const MAX_BYTES: i64 = 64 * 1024 * 1024;

/// Maximum attempts per blob.
const MAX_ATTEMPTS: u32 = 3;

/// Coarse filter on the SQL side.
///
/// It is **only** a coarse filter — whether an object is actually claimed is
/// decided by the transcription pipeline based on MIME. Its sole reason to exist
/// is to avoid pulling the entire `blobs` table into memory.
///
/// Images only for now: `cormex-memory` has no usable ASR implementation (the
/// gap is documented on [`cormex_memory::transcribe::Transcriber`]), and adding
/// `audio/%` and `video/%` here would just make the rescanner claim the same
/// batch of videos every five minutes, get `Unsupported`, and log it again. When
/// ASR lands, one line here changes.
const MIME_PREFIXES: &[&str] = &["image/%"];

/// Spawn the background rescan task.
///
/// With no vision model configured it does **nothing at all** (the task is not
/// even spawned), consistent with the upload path: images are still archived,
/// they simply cannot be retrieved.
pub fn spawn(st: crate::state::AppState, blobs: MediaStore) {
    let Some(live) = st.live().cloned() else {
        return; // mock backend: no database to rescan
    };
    if !live.transcription_enabled() {
        tracing::info!("no vision model configured; the transcription backfill will not start");
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SCAN_DELAY).await;
        // The ledger (which blacklists bad blobs) is **one per tenant**: hashes
        // are content-addressed, so two tenants may each hold an image with
        // identical bytes, and one of them being untranscribable does not mean
        // the other is (object storage keys carry a tenant prefix, so the file
        // may be missing on only one side)
        let mut ledgers: std::collections::HashMap<String, Ledger> =
            std::collections::HashMap::new();
        let mut tick = tokio::time::interval(SCAN_INTERVAL);
        loop {
            tick.tick().await;

            // Re-enumerated each round: someone who just registered has to be
            // covered by the rescan too. This used to hold only the startup
            // store connected to public, so images uploaded by new users were
            // **never transcribed** — with no symptom at all, they simply were
            // not searchable
            let schemas = match st.tenant_schemas().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "could not list the tenants; skipping this backfill round");
                    continue;
                }
            };
            for schema in &schemas {
                let Ok(store) = st.tenant_store(schema).await else {
                    tracing::warn!(
                        schema = schema.as_str(),
                        "could not connect to this tenant; skipping its backfill"
                    );
                    continue;
                };
                let bound = live.bind(&store);
                let ledger = ledgers.entry(schema.as_str().to_owned()).or_default();
                scan_once(bound.as_live(), &blobs, ledger).await;
            }
            // There is no reason to keep a blacklist for a tenant that no
            // longer exists
            ledgers.retain(|k, _| schemas.iter().any(|s| s.as_str() == k));
        }
    });
}

/// One rescan round.
async fn scan_once(live: &Arc<Live>, blobs: &MediaStore, ledger: &mut Ledger) {
    let prefixes: Vec<String> = MIME_PREFIXES.iter().map(|s| (*s).to_string()).collect();

    // Ask for more candidates than needed: the ones already blacklisted by the
    // ledger still occupy slots in the query result, so asking for exactly
    // BATCH would let one bad blob permanently block the queue behind it
    let want = (BATCH * 4) as i64;
    let candidates = match live.store().blobs_missing_transcript(&prefixes, want).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "the backfill candidate query failed; skipping this round");
            return;
        }
    };

    let todo: Vec<cormex_store::Blob> = candidates
        .into_iter()
        .filter(|b| !ledger.is_blocked(&b.hash))
        .take(BATCH)
        .collect();
    if todo.is_empty() {
        return;
    }

    tracing::info!(count = todo.len(), "starting the transcription backfill");

    // A Semaphore rather than `buffer_unordered`: the gate has to be bypassable
    // for free by the early-return path from `MAX_BYTES` (only a task that
    // cannot get a permit waits), whereas the combinator would queue every
    // future regardless
    let permits = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    let mut tasks = Vec::with_capacity(todo.len());

    for blob in todo {
        let permits = Arc::clone(&permits);
        let live = Arc::clone(live);
        let blobs = blobs.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permits.acquire_owned().await;
            let verdict = transcribe_one(&live, &blobs, &blob).await;
            (blob.hash, verdict)
        }));
    }

    for t in tasks {
        match t.await {
            Ok((hash, verdict)) => ledger.record(&hash, verdict),
            Err(e) => tracing::warn!(error = %e, "a backfill subtask ended abnormally"),
        }
    }
}

/// How one blob's rescan ended, from the ledger's point of view.
enum Verdict {
    /// Finished here; no need to come back (succeeded, redacted, unclaimed, or
    /// too large)
    Done,
    /// Did not work this time, but worth another attempt
    Retry,
}

async fn transcribe_one(live: &Live, blobs: &MediaStore, blob: &cormex_store::Blob) -> Verdict {
    if blob.size_bytes > MAX_BYTES {
        // Say so out loud rather than skipping silently: otherwise "why is this
        // large file not searchable" has nothing to investigate
        tracing::warn!(
            hash = %blob.hash,
            size_bytes = blob.size_bytes,
            limit = MAX_BYTES,
            "the object is over the backfill size ceiling and is skipped for good — \
             transcription needs the whole of it in memory"
        );
        return Verdict::Done;
    }

    let bytes = match blobs.get(&blob.hash).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!(hash = %blob.hash, error = %e, "the backfill could not fetch the bytes");
            return classify(&e);
        }
    };

    match live
        .transcribe_and_store(&blob.hash, &bytes, &blob.mime)
        .await
    {
        Ok(outcome) => {
            match &outcome {
                TranscribeOutcome::Stored { segments } => {
                    tracing::info!(hash = %blob.hash, segments, "the backfill filled in the transcript");
                }
                TranscribeOutcome::Unsupported { mime } => {
                    // The coarse filter let it in but no transcriber claims it
                    // (image/heic, for instance). Any number of retries yields
                    // the same result, so blacklist it
                    tracing::info!(hash = %blob.hash, mime, "no transcriber claimed it; the backfill will not retry");
                }
                TranscribeOutcome::Redacted | TranscribeOutcome::Disabled => {
                    tracing::debug!(hash = %blob.hash, "nothing left for the backfill to do");
                }
            }
            // All four outcomes are **deterministic**: running the same bytes
            // again produces the same answer. Only an Err is worth retrying
            Verdict::Done
        }
        Err(e) => {
            tracing::warn!(hash = %blob.hash, error = %e, "the backfill transcription failed");
            classify(&e)
        }
    }
}

/// "It failed this time" versus "this can never be transcribed".
///
/// [`CormexError::Invalid`] is a problem with the content itself (the bytes are
/// not a valid image, the transcriber produced invalid segments) — running it
/// again over a different network connection gives exactly the same result.
/// Everything else (provider 5xx, rate limiting, database jitter, object storage
/// timeouts) is environmental and worth retrying.
///
/// The cost of guessing wrong is asymmetric: treating a permanent error as
/// retryable burns one model call every five minutes for nothing; treating a
/// retryable error as permanent merely means this one blob has to wait for a
/// process restart to get another chance. So when in doubt, lean towards
/// "retryable" — [`MAX_ATTEMPTS`] is backing us up above.
const fn classify(e: &CormexError) -> Verdict {
    match e {
        CormexError::Invalid(_) => Verdict::Done,
        _ => Verdict::Retry,
    }
}

/// The retry ledger — **in-process**, never persisted.
///
/// # Why there is no table for this
///
/// Persisting it would either go through
/// [`cormex_store::Store::write_txn`] (which appends a `sync_log` row for every
/// failure, pushing the server's operational state out to every device), or
/// bypass it with a raw write (which punches through the structural guarantee
/// that WriteTxn is the only write entry point — a guarantee whose entire value
/// is that nobody can bypass it). Both costs exceed the problem being solved.
///
/// What is given up is **memory across restarts**: after a process restart, one
/// bad blob gets tried again. That is acceptable — the cost of a single attempt
/// is bounded (one byte fetch plus one model call), permanently broken blobs are
/// a rare event to begin with, and restarts are not frequent. The thing actually
/// worth preventing — retrying the same bad blob every five minutes until the
/// end of time — is blocked.
#[derive(Debug, Default)]
struct Ledger {
    /// hash → how many attempts so far. Blacklisted on reaching
    /// [`MAX_ATTEMPTS`] or on any deterministic outcome
    attempts: HashMap<String, u32>,
    blocked: std::collections::HashSet<String>,
}

impl Ledger {
    fn is_blocked(&self, hash: &str) -> bool {
        self.blocked.contains(hash)
    }

    fn record(&mut self, hash: &str, verdict: Verdict) {
        match verdict {
            Verdict::Done => {
                // The successful ones would not show up in the next round
                // anyway (they now have a blob_transcripts row); the
                // blacklisting is for the Unsupported / oversized / corrupt
                // cases, which satisfy the "no transcript row" candidate
                // condition forever
                self.attempts.remove(hash);
                self.blocked.insert(hash.to_owned());
            }
            Verdict::Retry => {
                let n = self.attempts.entry(hash.to_owned()).or_default();
                *n += 1;
                if *n >= MAX_ATTEMPTS {
                    tracing::warn!(
                        hash,
                        attempts = *n,
                        "the consecutive-failure ceiling was reached; this process will not retry that blob"
                    );
                    self.blocked.insert(hash.to_owned());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deterministic_outcome_blocks_immediately() {
        // The Unsupported / oversized / corrupt-bytes cases satisfy the "no
        // transcript row" candidate condition forever. Without an immediate
        // blacklisting, the rescanner claims them again every five minutes
        let mut l = Ledger::default();
        l.record("a", Verdict::Done);
        assert!(
            l.is_blocked("a"),
            "a deterministic outcome must be blacklisted immediately, or it will be claimed \
             forever"
        );
    }

    #[test]
    fn transient_failures_are_retried_but_not_forever() {
        let mut l = Ledger::default();
        for i in 1..MAX_ATTEMPTS {
            l.record("b", Verdict::Retry);
            assert!(
                !l.is_blocked("b"),
                "giving up on failure #{i} is far too early — provider 5xx and rate limiting \
                 both recover on their own"
            );
        }
        l.record("b", Verdict::Retry);
        assert!(
            l.is_blocked("b"),
            "after {MAX_ATTEMPTS} consecutive failures it must stop, or one bad blob gets \
             retried forever"
        );
    }

    #[test]
    fn only_content_errors_are_treated_as_permanent() {
        // The cost of guessing wrong is asymmetric: treating a permanent error
        // as retryable burns one model call per round for nothing, while
        // treating a retryable one as permanent only makes this blob wait for a
        // restart. When in doubt, lean towards "retryable"
        assert!(matches!(
            classify(&CormexError::Invalid("that is not a valid image".into())),
            Verdict::Done
        ));
        for transient in [
            CormexError::Provider("429 Too Many Requests".into()),
            CormexError::Store("connection reset".into()),
            CormexError::Memory("the embedder is temporarily unavailable".into()),
        ] {
            assert!(
                matches!(classify(&transient), Verdict::Retry),
                "{transient} is an environmental problem and should be retryable"
            );
        }
    }

    #[test]
    fn the_size_ceiling_is_above_the_direct_upload_threshold() {
        // The band the rescan exists to rescue is exactly "large enough that it
        // had to go through direct upload". If the ceiling were below the
        // direct-upload threshold, this module would rescue not a single blob —
        // and would report nothing at all
        assert!(
            MAX_BYTES > crate::blobs::DIRECT_UPLOAD_LIMIT as i64,
            "the rescan size ceiling must exceed the direct-upload threshold, or it rescues \
             none of the objects that arrived by direct upload"
        );
    }
}

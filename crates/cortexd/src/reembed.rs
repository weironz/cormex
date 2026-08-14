//! After switching embedding models, fill in the vectors for the old facts.
//!
//! # What it is filling in
//!
//! All four vector queries filter on `embedding_model` (see
//! `cortex_store::Store::recall_vector`), so on the day the model changes, **not
//! one old fact can enter vector retrieval**. Meanwhile BM25, graph traversal
//! and recency all keep working — so it does not look broken, it just looks
//! like "semantic retrieval seems a bit worse lately". Without this backfiller
//! that state is **permanent**: new facts pile up day by day while the old
//! memories never return to the semantic space.
//!
//! # Why it is on by default
//!
//! The cost of turning it off is exactly the paragraph above, and it has **no
//! symptom at all**. The cost of leaving it on is visible: one full backfill =
//! one embedding call per row in the database, and against a cloud service that
//! costs money. So it is on, but it **states the numbers out loud before doing
//! anything** — the startup log spells out "found N rows belonging to old model
//! X, will backfill M per round". Set `CORTEX_REEMBED=off` to stop it running.
//!
//! # Relationship to `backfill` (the transcription rescan)
//!
//! The skeleton is the same (delayed first round / periodic scan / a per-round
//! ceiling), but the two know nothing about each other. Running them separately
//! is deliberate: transcription depends on a vision provider and this backfill
//! depends on an embedding service, and one being down must not drag the other
//! down with it.

use std::sync::Arc;
use std::time::Duration;

use cortex_memory::Embedder as _;
use cortex_store::{NewEntityEmbedding, NewFactEmbedding, Vector};

use crate::live::Live;

/// The switch. Its accepted values are deliberately not `1` / `true` — it
/// should only be something you write down after understanding it.
const ENV_ENABLED: &str = "CORTEX_REEMBED";
/// How many rows to fill per round.
const ENV_BATCH: &str = "CORTEX_REEMBED_BATCH";
/// How long to wait between rounds (seconds).
const ENV_INTERVAL: &str = "CORTEX_REEMBED_INTERVAL_SECS";

/// Default rows per round.
///
/// 64 is not a performance trade-off, it is **cost predictability**: against a
/// cloud embedding service one round is 64 calls, and with the default 5-minute
/// interval that is roughly 768 rows per hour at worst. A database of a hundred
/// thousand rows takes days to finish — slow, but every step is bounded, and
/// "newest first" (see the ordering in `Store::facts_missing_embedding`) means
/// the batch most likely to be retrieved comes back first.
///
/// A deployment in a hurry raises this number, which is an explicit choice.
const DEFAULT_BATCH: usize = 64;

/// Default interval between rounds.
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// How long after startup the first round runs.
///
/// Same reasoning as the transcription rescan: right after the process starts,
/// the connection pool is still warming up and the embedding service may not be
/// ready yet (in compose, cortexd and embeddings are brought up simultaneously).
/// Pushing a batch of requests in at that moment buys nothing but a string of
/// timeout failures.
const FIRST_SCAN_DELAY: Duration = Duration::from_secs(45);

/// How many consecutive failed rounds before slowing down.
///
/// While the embedding service is down, hammering away at the original cadence
/// only floods the log. Back off to ten times the interval, and reset on the
/// first successful round once the service returns — there is no exponential
/// backoff because the ceiling here is already on the order of minutes, and
/// backing off further would turn into "wait half an hour even after it
/// recovers".
const SLOW_AFTER_FAILURES: u32 = 3;

/// Spawn the background backfill task.
pub fn spawn(st: crate::state::AppState) {
    if !enabled() {
        tracing::info!(
            "{ENV_ENABLED}=off: the vector backfill that follows a model change will \
             **not** run. Facts embedded under the old model stay out of vector \
             retrieval for good"
        );
        return;
    }
    let batch = usize_env(ENV_BATCH, DEFAULT_BATCH);
    let interval = Duration::from_secs(u64::from(
        u32::try_from(usize_env(ENV_INTERVAL, DEFAULT_INTERVAL.as_secs() as usize)).unwrap_or(300),
    ));

    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SCAN_DELAY).await;

        let Some(live) = st.live().cloned() else {
            return; // mock backend: no database to backfill
        };

        // State the numbers before acting. This is the precondition for the
        // "on by default" choice: spending money automatically is acceptable,
        // but not while the user has no idea how much is being spent
        live.report_embedding_state().await;

        let mut failures: u32 = 0;
        loop {
            let outcome = scan_all_tenants(&st, &live, batch).await;
            match outcome {
                Outcome::Idle => failures = 0,
                Outcome::Progressed => failures = 0,
                Outcome::Failed => failures = failures.saturating_add(1),
            }
            let wait = if failures >= SLOW_AFTER_FAILURES {
                interval * 10
            } else {
                interval
            };
            tokio::time::sleep(wait).await;
        }
    });
}

/// How one round ended.
enum Outcome {
    /// Nothing pending — this is the case the vast majority of the time
    Idle,
    /// At least one row was filled in
    Progressed,
    /// This round filled in nothing at all
    Failed,
}

/// Run one round across **every** tenant.
///
/// # Why this layer is mandatory
///
/// The backfiller originally knew only the startup `Store` connected to
/// `public`. Once there are multiple users, the memory of anyone who registers
/// later is **never backfilled** — and this has no symptom: they simply feel
/// that "search seems a bit off", because the vector retrieval path is empty for
/// them.
///
/// The tenant list is re-enumerated each round rather than cached: someone who
/// just registered must be covered on the very next round.
///
/// One tenant failing does not affect the others — they are independent
/// databases.
async fn scan_all_tenants(st: &crate::state::AppState, live: &Arc<Live>, batch: usize) -> Outcome {
    let schemas = match st.tenant_schemas().await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "could not list the tenants; skipping this round");
            return Outcome::Failed;
        }
    };

    let mut any_progress = false;
    let mut any_failure = false;
    for schema in &schemas {
        let store = match st.tenant_store(schema).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(schema = schema.as_str(), error = %e, "could not connect to this tenant; skipping it");
                any_failure = true;
                continue;
            }
        };
        let bound = live.bind(&store);
        match scan_once(bound.as_live(), batch).await {
            Outcome::Progressed => any_progress = true,
            Outcome::Failed => any_failure = true,
            Outcome::Idle => {}
        }
    }

    // Any progress counts as progress — the backoff exists so we stop hammering
    // a service that is down, and as long as even one tenant is making headway,
    // the service is not down
    if any_progress {
        Outcome::Progressed
    } else if any_failure {
        Outcome::Failed
    } else {
        Outcome::Idle
    }
}

/// One backfill round: take a batch of pending rows → compute the vectors
/// **outside the transaction** → insert them in bulk inside a short transaction.
///
/// "Compute vectors outside the transaction" is a hard constraint from
/// CLAUDE.md: `write_txn`'s very first statement takes an advisory lock to
/// serialize commit order, so crossing the network inside a lock-holding
/// transaction pins global write throughput to that HTTP round trip.
async fn scan_once(live: &Arc<Live>, batch: usize) -> Outcome {
    let model = live.embedder().model_id().to_owned();
    let store = live.store();

    let todo = match store.facts_missing_embedding(&model, batch as i64).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "the backfill to-do query failed; skipping this round");
            return Outcome::Failed;
        }
    };
    let entities = store
        .entities_missing_embedding(&model, batch as i64)
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "the entity backfill to-do query failed; this round fills in facts only");
            Vec::new()
        });

    if todo.is_empty() && entities.is_empty() {
        return Outcome::Idle;
    }

    tracing::info!(
        facts = todo.len(),
        entities = entities.len(),
        model = %model,
        "starting the embedding backfill"
    );

    // ── Outside the transaction: compute the vectors ────────
    let texts: Vec<String> = todo
        .iter()
        .map(|(_, s)| s.clone())
        .chain(entities.iter().map(|(_, n)| n.clone()))
        .collect();

    let vectors = match live.embedder().embed(&texts).await {
        Ok(v) => v,
        Err(e) => {
            // No per-row retry: a whole batch failing is almost always
            // "service unreachable / wrong credentials / wrong model name",
            // and retrying row by row would just print the same error 64 times
            tracing::warn!(error = %e, "embedding failed; nothing is written this round");
            return Outcome::Failed;
        }
    };
    if vectors.len() != texts.len() {
        tracing::warn!(
            got = vectors.len(),
            want = texts.len(),
            "the embedder returned a different number of rows; discarding this \
                 round — pairing by position would write vectors onto the wrong rows"
        );
        return Outcome::Failed;
    }

    // ── Inside the transaction: writes only ─────────────────
    let split = todo.len();
    let written = store
        .write_txn(async |tx| {
            for ((id, _), v) in todo.iter().zip(&vectors[..split]) {
                let Ok(fact_id) = id.parse() else {
                    // An id in the database that is not a ULID means somebody
                    // inserted a row bypassing the write channel. Skip it
                    // rather than failing the whole batch — one bad row must
                    // not block the rest
                    tracing::warn!(id, "the fact id is not a valid ULID; skipping it");
                    continue;
                };
                tx.insert_fact_embedding(&NewFactEmbedding {
                    fact_id,
                    embedding_model: model.clone(),
                    embedding: Vector::from(v.clone()),
                })
                .await?;
            }
            for ((id, _), v) in entities.iter().zip(&vectors[split..]) {
                let Ok(entity_id) = id.parse() else {
                    tracing::warn!(id, "the entity id is not a valid ULID; skipping it");
                    continue;
                };
                tx.insert_entity_embedding(&NewEntityEmbedding {
                    entity_id,
                    embedding_model: model.clone(),
                    embedding: Vector::from(v.clone()),
                })
                .await?;
            }
            Ok(())
        })
        .await;

    match written {
        Ok(()) => {
            let (covered, stale) = store.embedding_coverage(&model).await.unwrap_or((0, 0));
            tracing::info!(
                facts = todo.len(),
                entities = entities.len(),
                covered,
                stale,
                "finished one backfill round"
            );
            Outcome::Progressed
        }
        Err(e) => {
            tracing::warn!(error = %e, "the backfill write failed; the whole round was rolled back");
            Outcome::Failed
        }
    }
}

/// Read the switch. Defaults to **on**.
///
/// Only `off` / `false` / `0` are recognised, deliberately: anything else
/// (`no`, say) is treated as on and leaves a warn line, rather than silently
/// turning it off — silently turning off is precisely the shape of the problem
/// this whole thing exists to solve.
fn enabled() -> bool {
    match std::env::var(ENV_ENABLED) {
        Err(_) => true,
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "" => true,
            "off" | "false" | "0" | "no" => false,
            other => {
                tracing::warn!(
                    value = other,
                    "{ENV_ENABLED} has an unrecognised value and is treated as **on**. \
                     Write off to turn it off"
                );
                true
            }
        },
    }
}

fn usize_env(key: &str, default: usize) -> usize {
    match std::env::var(key) {
        Err(_) => default,
        Ok(v) if v.trim().is_empty() => default,
        Ok(v) => match v.trim().parse::<usize>() {
            Ok(n) if n > 0 => n,
            _ => {
                tracing::warn!(key, value = %v, default, "the value is invalid; falling back to the default");
                default
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value semantics of the switch. **Must not run in parallel under
    /// separate `#[test]` functions** — `set_var` is process-global.
    ///
    /// Verified sequentially inside one test function, for the same reason as
    /// `cortex-agent`'s sandbox switch test.
    #[test]
    fn the_switch_defaults_to_on_and_only_off_turns_it_off() {
        // SAFETY: this is the only test function in this file that reads or
        // writes this variable
        unsafe { std::env::remove_var(ENV_ENABLED) };
        assert!(
            enabled(),
            "with nothing set it must be on — defaulting to off would leave the silent amnesia \
             exactly as it was"
        );

        for v in ["off", "OFF", " false ", "0", "no"] {
            unsafe { std::env::set_var(ENV_ENABLED, v) };
            assert!(!enabled(), "{v:?} should turn the backfill off");
        }

        // A mistyped value is treated as **on**: silently turning off is
        // precisely the shape of the problem this exists to solve
        for v in ["yes", "disable", "turn-off"] {
            unsafe { std::env::set_var(ENV_ENABLED, v) };
            assert!(
                enabled(),
                "{v:?} is not a recognised off value, so it must be treated as on and leave a \
                 warning, rather than silently switching the backfill off"
            );
        }

        unsafe { std::env::remove_var(ENV_ENABLED) };
    }

    #[test]
    fn batch_size_falls_back_on_garbage() {
        unsafe { std::env::remove_var(ENV_BATCH) };
        assert_eq!(usize_env(ENV_BATCH, DEFAULT_BATCH), DEFAULT_BATCH);

        for v in ["0", "-5", "abc", ""] {
            unsafe { std::env::set_var(ENV_BATCH, v) };
            assert_eq!(
                usize_env(ENV_BATCH, DEFAULT_BATCH),
                DEFAULT_BATCH,
                "{v:?} is not a positive integer, so it must fall back to the default rather \
                 than leaving the backfiller filling 0 rows per round (which is the same as \
                 silently switching it off)"
            );
        }
        unsafe { std::env::set_var(ENV_BATCH, "8") };
        assert_eq!(usize_env(ENV_BATCH, DEFAULT_BATCH), 8);
        unsafe { std::env::remove_var(ENV_BATCH) };
    }
}

//! Lay out the bill first, then act.
//!
//! # Why dry-run is the default
//!
//! Every message pair triggers one extraction, i.e. one LLM call. Three years
//! of history may be thousands of them — that is real money, and once it is
//! running there is no "undo" (memory is append-only, and undoing means redact,
//! which must be explicitly triggered, confirmed twice, and leaves a
//! tombstone).
//!
//! So the default is to count without doing: put the conversation count,
//! message count, number of extractions to be triggered, token count, and time
//! span all on the table, then stop. **This rule holds for the UI too** — the
//! desktop and web import pages must also show this bill before showing a
//! "Start" button.
//!
//! # Why report tokens rather than money
//!
//! Each vendor prices differently and the prices keep changing, and extraction
//! runs on the cheap model **configured on the server** — the client has no
//! idea which one that is. Hardcoding a price would only produce a wrong number
//! that looks precise. Report tokens, and let whoever knows their own price do
//! the multiplication.

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use chrono::{DateTime, Utc};
use cortex_proto::episodes::{EpisodeAck, NewEpisodeRequest};

use crate::{Conversation, Platform, requests_for};

/// The minimum gap between two deliveries.
///
/// The server has an extraction concurrency gate, but **that is not
/// backpressure** — pouring too fast merely grows the queue in server memory,
/// and the production box has 2 cores and 3.5 GB. Slowing down voluntarily here
/// is the client's job.
///
/// 150ms ≈ 6–7 pairs per second. Three years of history (say 5000 pairs) takes
/// roughly 15 minutes — a one-time operation, so that pace is entirely
/// acceptable, and it leaves the server with headroom to serve normal
/// conversations throughout.
pub const PACE: Duration = Duration::from_millis(150);

/// What one import is going to do.
#[derive(Debug)]
pub struct Plan {
    pub platform: Platform,
    pub conversations: Vec<Conversation>,
}

/// The bill, laid out.
///
/// `serde` is derived so this can go over HTTP to the UI unchanged — the
/// numbers the UI shows and the numbers the CLI prints **must come from the
/// same computation**; computing them separately will disagree sooner or later.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Estimate {
    pub conversations: usize,
    pub messages: usize,
    /// How many extractions will be triggered = how many LLM calls
    pub pairs: usize,
    /// The volume of text fed into extraction (rough estimate)
    pub tokens: usize,
    pub earliest: Option<DateTime<Utc>>,
    pub latest: Option<DateTime<Utc>>,
    /// How many messages are left dangling (they enter episodes, but produce no
    /// facts)
    pub unpaired: usize,
}

impl Estimate {
    /// Convert into the shape sent down to clients.
    ///
    /// The conversion lives on this side rather than in `cortex-proto`: that
    /// crate is **pure contract** and depends on no implementation. Having it
    /// `use cortex_import` instead would invert the dependency direction.
    #[must_use]
    pub fn to_wire(&self, platform: Platform) -> cortex_proto::import::ImportEstimate {
        cortex_proto::import::ImportEstimate {
            platform: platform.label().to_string(),
            conversations: self.conversations,
            messages: self.messages,
            pairs: self.pairs,
            tokens: self.tokens,
            earliest: self.earliest.map(|t| t.to_rfc3339()),
            latest: self.latest.map(|t| t.to_rfc3339()),
            unpaired: self.unpaired,
            minutes: self.minutes(),
        }
    }

    /// At the current pace, the minimum number of minutes this will take.
    #[must_use]
    pub fn minutes(&self) -> f64 {
        #[allow(clippy::cast_precision_loss)]
        let m = self.pairs as f64 * PACE.as_secs_f64() / 60.0;
        m.ceil()
    }
}

impl Plan {
    #[must_use]
    pub fn estimate(&self) -> Estimate {
        let mut e = Estimate {
            conversations: self.conversations.len(),
            messages: 0,
            pairs: 0,
            tokens: 0,
            earliest: None,
            latest: None,
            unpaired: 0,
        };
        for c in &self.conversations {
            e.messages += c.messages.len();
            let pairs = c.pairs();
            e.pairs += pairs.len();
            e.unpaired += c.messages.len() - pairs.len() * 2;
            for (u, a) in pairs {
                // Isomorphic to the passage the server assembles for the model
                // (`用户：…\n助手：…`), so this number is "the volume that will
                // really be fed in", not the file size
                e.tokens += cortex_core::injection::estimate_tokens(&u.text)
                    + cortex_core::injection::estimate_tokens(&a.text);
            }
            for m in &c.messages {
                e.earliest = Some(e.earliest.map_or(m.at, |x: DateTime<Utc>| x.min(m.at)));
                e.latest = Some(e.latest.map_or(m.at, |x: DateTime<Utc>| x.max(m.at)));
            }
        }
        e
    }
}

/// The outlet for pouring data in. Implemented once in each of three places:
/// the CLI over remote HTTP, the local agent over remote HTTP, and cortexd
/// straight down its own write path.
///
/// # Why abstract this into a trait rather than copy [`execute`] three times
///
/// That loop contains three **order-sensitive** things, and copying any of them
/// wrong raises no error — the result is simply wrong:
///
/// 1. user must land before assistant (the anchor points at it, and the server
///    reads the original text back)
/// 2. deliveries must be rate-limited (otherwise the queue grows in server
///    memory)
/// 3. the title must come after the content (the session has to exist first)
///
/// As a trait, those three exist in exactly one implementation.
pub trait Sink {
    /// Write one episode. `already_existed` is true when it is already there
    /// (idempotency rests on it).
    fn write_episode(
        &self,
        req: &NewEpisodeRequest,
    ) -> impl Future<Output = Result<EpisodeAck>> + Send;

    /// Rename a session. Failure is not fatal — the content is already in, and
    /// only a title is missing.
    fn rename_session(
        &self,
        session_id: &str,
        title: &str,
    ) -> impl Future<Output = Result<()>> + Send;
}

/// How far an import has got.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct Progress {
    /// Conversations processed
    pub conversations_done: usize,
    pub conversations_total: usize,
    /// Pairs poured in
    pub pairs_done: usize,
    /// Entries the server recognized as "already there" — no double billing
    pub skipped: usize,
    pub failures: usize,
}

/// Actually run it.
///
/// # Resuming is free
///
/// Episode ids are computed by [`cortex_core::Id::derived`] from "original
/// moment + stable seed", and `POST /episodes` deduplicates by id. So after an
/// interruption, re-running the same command is all it takes — anything already
/// written is recognized by the server (`already_existed`) and is not billed
/// again. No progress file is needed, and so the failure mode of "the progress
/// file disagrees with the actual state" does not exist.
///
/// # Errors
/// Currently never returns `Err` — a single failed write is counted without
/// interrupting the run (a re-run fills it in). The signature keeps `Result` so
/// criteria like "stop after too many consecutive failures" can be added later,
/// and changing the signature at that point would ripple through three callers.
pub async fn execute<S: Sink, F: FnMut(Progress)>(
    sink: &S,
    plan: &Plan,
    mut on_progress: F,
) -> Result<Progress> {
    let mut p = Progress {
        conversations_done: 0,
        conversations_total: plan.conversations.len(),
        pairs_done: 0,
        skipped: 0,
        failures: 0,
    };

    for conv in &plan.conversations {
        let pairs = conv.pairs();
        if pairs.is_empty() {
            p.conversations_done += 1;
            on_progress(p);
            continue;
        }
        let session = conv.session_id(plan.platform).to_string();

        for (u, a) in pairs {
            let [user_req, asst_req] = requests_for(plan.platform, conv, u, a);
            // user has to land first: the assistant entry's anchor points at
            // it, and the server uses that anchor to read the user's original
            // text back out of the store to assemble the extraction input. The
            // other way round, extraction skips the whole turn because "the
            // corresponding user episode cannot be found"
            match sink.write_episode(&user_req).await {
                Ok(ack) if ack.already_existed => p.skipped += 1,
                Ok(_) => {}
                Err(e) => {
                    p.failures += 1;
                    tracing::warn!(
                        error = %e,
                        "failed to write the user episode; skipping this pair"
                    );
                    continue;
                }
            }
            match sink.write_episode(&asst_req).await {
                Ok(ack) => {
                    if ack.already_existed {
                        p.skipped += 1;
                    }
                    p.pairs_done += 1;
                }
                Err(e) => {
                    p.failures += 1;
                    tracing::warn!(error = %e, "failed to write the assistant episode");
                }
            }

            tokio::time::sleep(PACE).await;
        }

        // The title goes last: the session needs content before there is
        // anything to rename. Failure is not fatal — the content is already in,
        // and only a title is missing
        if let Err(e) = sink
            .rename_session(&session, &conv.display_title(plan.platform))
            .await
        {
            tracing::debug!(error = %e, session, "failed to rename the session");
        }

        p.conversations_done += 1;
        on_progress(p);
    }
    Ok(p)
}

/// Parse an export file already read into memory, applying the time and count
/// filters along the way.
///
/// # Errors
/// It cannot be parsed, or no conversation remains after filtering.
pub fn plan_from_bytes(
    bytes: &[u8],
    platform: Option<Platform>,
    since: Option<DateTime<Utc>>,
    max: Option<usize>,
) -> Result<Plan> {
    let (platform, mut conversations) = crate::parse(bytes, platform)?;

    if let Some(since) = since {
        conversations.retain(|c| c.messages.iter().any(|m| m.at >= since));
    }
    // Newest first: when testing the water with `--max-conversations 5`, the
    // ones to test with are obviously the most recent
    conversations.sort_by_key(|c| std::cmp::Reverse(c.at));
    if let Some(n) = max {
        conversations.truncate(n);
    }

    if conversations.is_empty() {
        bail!("no conversation is left after the current time / count filters");
    }
    Ok(Plan {
        platform,
        conversations,
    })
}

/// Read a file and parse it into a `Plan`.
///
/// # Errors
/// The file cannot be read, or any error reported by [`plan_from_bytes`].
pub fn load(
    path: &std::path::Path,
    platform: Option<Platform>,
    since: Option<DateTime<Utc>>,
    max: Option<usize>,
) -> Result<Plan> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    plan_from_bytes(&bytes, platform, since, max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Message, Role};

    fn conv(n_pairs: usize, extra_dangling: bool) -> Conversation {
        let mut messages = Vec::new();
        for i in 0..n_pairs {
            messages.push(Message {
                source_id: format!("u{i}"),
                role: Role::User,
                text: "问一个不长不短的问题".into(),
                at: format!("2024-03-0{}T10:00:00Z", i + 1).parse().unwrap(),
            });
            messages.push(Message {
                source_id: format!("a{i}"),
                role: Role::Assistant,
                text: "给一个不长不短的回答".into(),
                at: format!("2024-03-0{}T10:00:01Z", i + 1).parse().unwrap(),
            });
        }
        if extra_dangling {
            messages.push(Message {
                source_id: "dangling".into(),
                role: Role::User,
                text: "最后一问没人答".into(),
                at: "2024-03-09T10:00:00Z".parse().unwrap(),
            });
        }
        Conversation {
            source_id: "c".into(),
            title: "t".into(),
            at: "2024-03-01T10:00:00Z".parse().unwrap(),
            messages,
        }
    }

    /// The bill has to be right: **the number of extractions equals the number
    /// of pairs**, not the number of messages.
    ///
    /// This number is money, directly. Reporting by message count would leave
    /// the user thinking it costs twice as much.
    #[test]
    fn the_estimate_counts_extraction_calls_not_messages() {
        let plan = Plan {
            platform: Platform::ChatGpt,
            conversations: vec![conv(3, false), conv(2, false)],
        };
        let e = plan.estimate();
        assert_eq!(e.conversations, 2);
        assert_eq!(e.messages, 10);
        assert_eq!(e.pairs, 5, "5 pairs = 5 LLM calls, not 10");
        assert!(e.tokens > 0);
        assert_eq!(e.unpaired, 0);
    }

    /// Dangling messages have to be reported separately.
    ///
    /// They land as raw text yet produce no facts — when a user sees "imported
    /// 100 messages" but only 40 memories appeared, something has to explain
    /// that gap.
    #[test]
    fn dangling_messages_are_reported_separately() {
        let plan = Plan {
            platform: Platform::Claude,
            conversations: vec![conv(2, true)],
        };
        let e = plan.estimate();
        assert_eq!(e.pairs, 2);
        assert_eq!(
            e.unpaired, 1,
            "the last question went unanswered and has to be reported"
        );
    }

    /// The time span comes from the **messages'** moments, not the
    /// conversation's creation moment.
    #[test]
    fn the_span_comes_from_the_messages() {
        let plan = Plan {
            platform: Platform::ChatGpt,
            conversations: vec![conv(3, false)],
        };
        let e = plan.estimate();
        assert_eq!(
            e.earliest.unwrap().format("%Y-%m-%d").to_string(),
            "2024-03-01"
        );
        assert_eq!(
            e.latest.unwrap().format("%Y-%m-%d").to_string(),
            "2024-03-03"
        );
    }

    /// `Estimate` has to survive HTTP unchanged — the numbers in the UI and the
    /// numbers the CLI prints must come from **the same computation**;
    /// computing them separately will disagree sooner or later.
    #[test]
    fn the_estimate_survives_a_round_trip() {
        let plan = Plan {
            platform: Platform::Claude,
            conversations: vec![conv(2, true)],
        };
        let e = plan.estimate();
        let back: Estimate = serde_json::from_str(&serde_json::to_string(&e).expect("serialize"))
            .expect("deserialize");
        assert_eq!(back.pairs, e.pairs);
        assert_eq!(back.tokens, e.tokens);
        assert_eq!(back.unpaired, e.unpaired);
        assert_eq!(
            back.earliest, e.earliest,
            "moments must not lose precision in the round trip"
        );
    }

    /// A fake outlet, used to verify [`execute`]'s three order-sensitive rules.
    #[derive(Default)]
    struct Recorder {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl Sink for Recorder {
        async fn write_episode(&self, req: &NewEpisodeRequest) -> Result<EpisodeAck> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("write:{}:{}", req.role, req.id));
            Ok(EpisodeAck {
                episode_id: req.id.clone(),
                already_existed: false,
                memories: Vec::new(),
            })
        }
        async fn rename_session(&self, session_id: &str, _title: &str) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("rename:{session_id}"));
            Ok(())
        }
    }

    /// **user must land first, and the title must come last.**
    ///
    /// Either order being reversed raises no error: assistant landing first
    /// leaves the server unable to read the user's original text back, so the
    /// whole turn's extraction is skipped — the raw text went in and not a
    /// single fact came out.
    #[tokio::test]
    async fn writes_go_user_first_and_the_title_comes_last() {
        let sink = Recorder::default();
        let plan = Plan {
            platform: Platform::Claude,
            conversations: vec![conv(2, false)],
        };
        execute(&sink, &plan, |_| {})
            .await
            .expect("should run through");

        let calls = sink.calls.lock().unwrap().clone();
        assert_eq!(
            calls.len(),
            5,
            "two pairs write four entries plus one rename, got: {calls:?}"
        );
        assert!(
            calls[0].starts_with("write:user"),
            "the first entry must be user"
        );
        assert!(calls[1].starts_with("write:assistant"));
        assert!(
            calls[4].starts_with("rename:"),
            "the title must come after the content — the session needs \
             something in it before it can be renamed"
        );
    }
}

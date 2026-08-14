//! Budget trimming for session history entering the context.
//!
//! # This layer simply did not exist before
//!
//! Both conversation paths of the time (cortexd's `run_turn`, the local
//! agent's `Engine::run_turn`) started like this — the cortexd one was later
//! deleted along with its duplicate agent loop, so only the local agent's
//! remains:
//!
//! ```ignore
//! let mut messages = vec![Message::user().with_text(&user_content)];
//! ```
//!
//! **Only the current message; session history was never loaded even once.**
//! Measured: within one session, turn one says "my lucky number is 4173", turn
//! two asks "what was that number again", and the model answers "in the
//! current conversation I don't see you mentioning a lucky number". It was
//! telling the truth.
//!
//! This directly contradicts the diagram in the [`crate::injection`] module
//! docs (which shows `history: user₁ ⟨memory block₁⟩, asst₁, …`), and
//! `needs_compaction` only means anything if history actually accumulates —
//! **the design was there all along, the implementation was never wired up.**
//!
//! # Why the memory system did not cover for it
//!
//! Memory can recover the part that got **extracted into facts**, and only
//! after asynchronous extraction has run. Questions like "what did I just say"
//! or "summarize this conversation" land where it cannot reach: that content
//! does not satisfy the extraction criterion of "not knowing this next time
//! would lead to a wrong decision" in the first place.
//!
//! # Keep the newest, drop the oldest
//!
//! The opposite of display pagination: there, truncation keeps the **oldest**
//! (the user reads from the top down); here it keeps the **newest** (the model
//! continues from these last few turns). The same LIMIT applied in the wrong
//! direction produces wildly different symptoms, so the two paths use separate
//! queries.

use crate::injection::estimate_tokens;

/// One turn of speech in the history.
///
/// A bool rather than a `Role` enum: this module is in `cortex-core`, while
/// `Role` lives in `cortex-store` (with sqlx derives). Dragging the storage
/// layer in for a two-state thing points the dependency the wrong way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTurn {
    pub is_user: bool,
    pub text: String,
}

/// The result of trimming.
#[derive(Debug)]
pub struct FittedHistory {
    /// The turns that fit, **in chronological order** (old → new)
    pub turns: Vec<HistoryTurn>,
    /// How many were dropped for lack of budget (always the older ones)
    pub dropped: usize,
}

/// How much of the context window history may occupy.
///
/// The memory block takes 12% ([`crate::injection::Budget`]); this gives 50%,
/// and the remainder is left for the system prompt, tool schemas, and the
/// model's own reply.
///
/// We do not give more because **more buys nothing**: across a conversation of
/// dozens of turns, the most relevant part is always the last few, and filling
/// 90% of the window with history leaves nowhere for tool calls and long
/// replies — the symptom being "mid-conversation it suddenly cuts off before
/// finishing an answer".
const HISTORY_RATIO: f64 = 0.50;

/// Absolute ceiling. Same reasoning as `Budget::absolute_max_tokens`: context
/// windows now reach the 1M scale, and a pure ratio would let history balloon
/// to an absurd size — and that is money paid over again on every single turn.
const HISTORY_MAX_TOKENS: usize = 24_000;

/// How many tokens history gets this time.
#[must_use]
pub fn history_budget(context_window: usize) -> usize {
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    let by_ratio = (context_window as f64 * HISTORY_RATIO) as usize;
    by_ratio.min(HISTORY_MAX_TOKENS)
}

/// Collect backwards from the **tail** within the budget, keeping the most
/// recent turns.
///
/// # Why turns are dropped in pairs
///
/// Cutting halfway and leaving a lone assistant message at the front means the
/// model sees "a passage answering who-knows-what". Better to drop one turn too
/// many than to keep an answer with no question.
///
/// # Empty text is skipped
///
/// Imported history contains entire messages that are only tool calls with no
/// body. In the context they are just noise, and they consume one of the
/// "pairing" slots.
#[must_use]
pub fn fit_history(turns: Vec<HistoryTurn>, max_tokens: usize) -> FittedHistory {
    let turns: Vec<HistoryTurn> = turns
        .into_iter()
        .filter(|t| !t.text.trim().is_empty())
        .collect();
    let total = turns.len();

    let mut used = 0usize;
    let mut keep_from = total;
    for (i, t) in turns.iter().enumerate().rev() {
        // Rough 8-token allowance per message for the role marker plus
        // separators
        let cost = estimate_tokens(&t.text) + 8;
        if used + cost > max_tokens {
            break;
        }
        used += cost;
        keep_from = i;
    }

    // Do not let the first entry be an assistant turn: that is an answer with
    // no question
    if turns.get(keep_from).is_some_and(|t| !t.is_user) {
        keep_from += 1;
    }

    FittedHistory {
        dropped: keep_from,
        turns: turns.into_iter().skip(keep_from).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(t: &str) -> HistoryTurn {
        HistoryTurn {
            is_user: true,
            text: t.into(),
        }
    }
    fn a(t: &str) -> HistoryTurn {
        HistoryTurn {
            is_user: false,
            text: t.into(),
        }
    }

    /// If it fits, nothing is dropped.
    #[test]
    fn a_short_conversation_survives_intact() {
        let h = fit_history(vec![u("问一"), a("答一"), u("问二"), a("答二")], 10_000);
        assert_eq!(h.dropped, 0);
        assert_eq!(h.turns.len(), 4);
        assert_eq!(h.turns[0].text, "问一", "the order must be old → new");
    }

    /// **The oldest are dropped, the newest are kept.**
    ///
    /// The opposite of display pagination — there the oldest are kept (the user
    /// reads from the top), here the newest are (the model continues from
    /// them). The symptom of getting the direction backwards is "it remembers
    /// three months ago but forgot the previous sentence".
    #[test]
    fn the_oldest_turns_are_the_ones_dropped() {
        let turns: Vec<HistoryTurn> = (0..40)
            .flat_map(|i| {
                [
                    u(&format!("第{i}个问题，内容写长一点占住预算")),
                    a(&format!("第{i}个回答，内容也写长一点占住预算")),
                ]
            })
            .collect();
        let h = fit_history(turns, 400);

        assert!(h.dropped > 0, "a budget this small must drop something");
        assert!(!h.turns.is_empty(), "it should not leave nothing at all");
        let last = h.turns.last().unwrap();
        assert!(
            last.text.contains("第39个回答"),
            "the last entry must be the most recent one, got: {}",
            last.text
        );
        assert!(
            !h.turns.iter().any(|t| t.text.contains("第0个")),
            "the oldest ones should already have been dropped"
        );

        // **What is kept must actually fit.** Without this assertion, an
        // implementation that collects forwards from the head (the direction
        // reversed) would also pass every assertion above — it too leaves the
        // most recent entry at the end, it just overshoots the budget by
        // dozens of times along the way, and overshooting shows up as a 400
        // from the model
        let used: usize = h.turns.iter().map(|t| estimate_tokens(&t.text) + 8).sum();
        assert!(
            used <= 400,
            "after trimming it still occupies {used} tokens, over the budget \
             of 400: {:?}",
            h.turns.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
    }

    /// **The trimmed window must not start with an assistant turn.**
    ///
    /// Leaving a lone answer means the model sees "a passage answering
    /// who-knows-what", and it will take it as context at face value. Better to
    /// drop one turn too many.
    #[test]
    fn the_kept_window_never_starts_with_an_answer() {
        // A budget that lands somewhere in the middle
        let turns = vec![
            u("很久以前的问题"),
            a("很久以前的回答"),
            u("新问题"),
            a("新回答"),
        ];
        for budget in 1..200 {
            let h = fit_history(turns.clone(), budget);
            if let Some(first) = h.turns.first() {
                assert!(
                    first.is_user,
                    "budget {budget} produced a window starting with an \
                     assistant turn: {:?}",
                    h.turns
                );
            }
        }
    }

    /// Empty text is skipped — imported history contains whole messages that
    /// are nothing but tool calls.
    #[test]
    fn empty_messages_are_skipped() {
        let h = fit_history(vec![u("问"), a("   "), u("再问"), a("答")], 10_000);
        assert_eq!(h.turns.len(), 3);
        assert!(!h.turns.iter().any(|t| t.text.trim().is_empty()));
    }

    /// When the budget is too small to fit even one turn, return empty rather
    /// than panicking.
    #[test]
    fn an_impossible_budget_yields_nothing_rather_than_panicking() {
        let h = fit_history(vec![u("一段挺长的问题"), a("一段挺长的回答")], 1);
        assert!(h.turns.is_empty());
        assert_eq!(h.dropped, 2);
    }

    /// The budget scales with the context window, but has an absolute ceiling.
    #[test]
    fn the_budget_scales_but_is_capped() {
        assert_eq!(history_budget(20_000), 10_000);
        assert_eq!(
            history_budget(1_000_000),
            HISTORY_MAX_TOKENS,
            "even with a million-token window history must not balloon to an \
             absurd size — that is money paid again every turn"
        );
    }
}

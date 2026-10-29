//! Fusion ranking over the multi-channel recall.
//!
//! Uses **RRF (Reciprocal Rank Fusion)** rather than a weighted sum: the score
//! scales of the channels have nothing in common (BM25 is unbounded tf-idf, the
//! vector channel is a [-1,1] cosine, graph traversal is a hop count, recency is
//! a timestamp), so there is simply no way to tune weights for a weighted sum.
//! RRF uses only **rank**, never the score, which makes it immune to scale,
//! training-free, and free of tuning parameters.
//!
//! ```text
//! score(d) = Σ  1 / (k + rank_i(d))       k = 60
//!           i∈recall channels
//! ```
//!
//! A side benefit: while an embedding model is being swapped, old and new vectors
//! do not share a semantic space and their scores are not comparable — but RRF
//! looks only at ranks within each channel, so it is **inherently compatible with
//! that mixed state** (see memory.md §7).
//!
//! # Its two costs, and the antidote to each
//!
//! 1. **Weak consensus outranks a strong hit** — see [`apply_peak_bonus`].
//! 2. **The score carries no absolute relevance.** An RRF score is a function of
//!    rank, so when the answer simply isn't in the database, every channel still
//!    returns its own best few, they still corroborate each other, and the score
//!    is still high. This one **cannot be solved in this module**; the retriever
//!    has to judge it using cosine distance
//!    (`retrieval::BGE_M3_SEMANTIC_FLOOR`) *before* fusion happens.

use std::collections::HashMap;

/// RRF's smoothing constant. 60 is the empirical value from the original paper,
/// and the de facto default across implementations.
pub const RRF_K: f64 = 60.0;

/// Which recall channel a result came from, for observability (which channel
/// contributed this result).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// BM25 full text — proper nouns and exact matches
    Bm25,
    /// Vector cosine — semantic proximity
    Vector,
    /// Graph traversal — expanding from a matched entity to related facts
    Graph,
    /// Recency — continuity of context
    Recency,
    /// L0 raw full text — going backwards from an episode to the facts extracted
    /// from it (following up on provenance)
    Episode,
}

impl Channel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bm25 => "bm25",
            Self::Vector => "vector",
            Self::Graph => "graph",
            Self::Recency => "recency",
            Self::Episode => "episode",
        }
    }
}

/// One fused result.
#[derive(Debug, Clone)]
pub struct Fused<T> {
    pub item: T,
    pub score: f64,
    /// The recall channels that hit it, and its rank within each (0-based).
    /// This is kept so that a bad case can be attributed to a specific channel —
    /// without it, all you can do when retrieval misbehaves is guess.
    pub hits: Vec<(Channel, usize)>,
}

/// Fuse multi-channel recall results with RRF.
///
/// Each channel in `channels` is a candidate list **already sorted descending by
/// that channel's own notion of relevance**. `key` pulls a stable identifier out
/// of a candidate for deduplication (usually the fact id).
///
/// Returns the results sorted descending by fused score.
pub fn rrf<T, K, F>(channels: &[(Channel, Vec<T>)], key: F) -> Vec<Fused<T>>
where
    T: Clone,
    K: std::hash::Hash + Eq + Ord,
    F: Fn(&T) -> K,
{
    let mut acc: HashMap<K, Fused<T>> = HashMap::new();

    for (channel, items) in channels {
        for (rank, item) in items.iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
            let k = key(item);
            acc.entry(k)
                .and_modify(|f| {
                    f.score += contribution;
                    f.hits.push((*channel, rank));
                })
                .or_insert_with(|| Fused {
                    item: item.clone(),
                    score: contribution,
                    hits: vec![(*channel, rank)],
                });
        }
    }

    // Descending by score; on a tie, more channel hits wins (consensus across
    // channels is more trustworthy); on a further tie, order by key — **this last
    // layer is not fastidiousness**:
    //
    // `into_values()` yields `HashMap` iteration order, which varies per process
    // with the hash seed. Ties are the norm here (entries picked up by exactly one
    // channel at the same rank come by the handful), so running the same corpus
    // with the same parameters twice would change who lands at top-1. Measured:
    // three eval runs put R@1 at 0.630/0.640/0.650 and moved MRR by 0.012 —
    // meaning **the eval suite at that point could not resolve any change smaller
    // than 0.02**, and the vast majority of tuning changes are exactly that size.
    // The `apply_*` steps that follow use stable sorts, so pinning the order down
    // here pins down the whole chain.
    let mut out: Vec<(K, Fused<T>)> = acc.into_iter().collect();
    out.sort_by(|(ka, a), (kb, b)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.hits.len().cmp(&a.hits.len()))
            .then_with(|| ka.cmp(kb))
    });
    out.into_iter().map(|(_, f)| f).collect()
}

/// The compensation term for a lone strong hit — the antidote to a known cost of
/// RRF.
///
/// # What it fixes
///
/// RRF only sees rank, so "rank 10 in two channels" (0.0141×2 ≈ 0.028) beats
/// "rank 1 in one channel" (0.0164). Most of the time that is exactly what you
/// want — cross-channel consensus really is more trustworthy — but when one
/// channel is delivering a **strong signal** it becomes collateral damage: a fact
/// that BM25 matched exactly at the very top gets pushed down by a handful of
/// topically-adjacent entries that sit somewhere in the teens in every channel.
///
/// The fix is not to weight one channel (that would have to be tuned per question
/// type, and it cannot be tuned), but to **stack on a second, much steeper
/// kernel**:
///
/// ```text
/// score(d) += weight / (PEAK_K + min_i rank_i(d))
/// ```
///
/// `PEAK_K` is 2 rather than 60: under k=60, rank 0 and rank 10 differ by only
/// 15%, which cannot distinguish a "strong hit" at all; under k=2 they differ by
/// 6×, so only genuine top-of-list entries collect the compensation. A `weight`
/// of 0 disables it.
///
/// This is **not** the "guarantee a slot in top-N" kind of hard insertion. Hard
/// insertion either rewrites the ordering or reserves a fixed slot, and both push
/// an error to the very front whenever the strong hit turns out to be wrong;
/// here we merely add a term, and genuine strong consensus (three or more
/// channels) still outweighs it. For how the weight was chosen, see the comments
/// in `retrieval::Retriever::new` and the sweep curve there.
pub const PEAK_K: f64 = 2.0;

/// See [`PEAK_K`]. Returns immediately without re-sorting when `weight` is 0.
pub fn apply_peak_bonus<T>(fused: &mut [Fused<T>], weight: f64) {
    if weight <= 0.0 {
        return;
    }
    for f in fused.iter_mut() {
        let best = f.hits.iter().map(|(_, r)| *r).min().unwrap_or(usize::MAX);
        if best == usize::MAX {
            continue;
        }
        f.score += weight / (PEAK_K + best as f64);
    }
    sort_by_score(fused);
}

/// Freshness decay: the newer a fact is, the higher it scores.
///
/// This is **the other form** recency can take — it does not occupy a recall
/// channel, it acts purely as a multiplier on the fused score. The two forms are
/// mutually exclusive; which is better, and the eval numbers behind that, are in
/// the comments on `retrieval::RecencyMode`.
///
/// The coefficient is `1 + strength · 0.5^(age / half_life)`: the newest facts
/// get the full `1 + strength`, one half-life back gets half of it, and anything
/// long past approaches 1 (that is, no weighting at all). It is a **multiplier**
/// rather than an addend so as not to disturb the "a score of zero means
/// abstain" semantics — an addend would lift every fact's score above the
/// abstention threshold.
pub fn apply_recency_decay<T, F>(
    fused: &mut [Fused<T>],
    now: chrono::DateTime<chrono::Utc>,
    strength: f64,
    half_life_days: f64,
    created_at_of: F,
) where
    F: Fn(&T) -> chrono::DateTime<chrono::Utc>,
{
    if strength <= 0.0 || half_life_days <= 0.0 {
        return;
    }
    for f in fused.iter_mut() {
        let age_days = (now - created_at_of(&f.item)).num_seconds().max(0) as f64 / 86_400.0;
        f.score *= 1.0 + strength * 0.5f64.powf(age_days / half_life_days);
    }
    sort_by_score(fused);
}

fn sort_by_score<T>(fused: &mut [Fused<T>]) {
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.hits.len().cmp(&a.hits.len()))
    });
}

/// Domain weighting: boost facts that are in the current conversation's domain.
///
/// This is an adjustment **after** fusion, not a fifth recall channel — domain is
/// a filtering dimension, not a relevance dimension. A `boost` of 1.0 means no
/// weighting.
///
/// # `exempt` — why an exemption hatch is needed
///
/// Boosting "the current domain" is equivalent to penalizing everything
/// cross-domain. Almost always that is what you want: talking about code should
/// not be interrupted by last week's meeting notes. But one class of result is
/// **cross-domain by definition**, and applying the same penalty to it amounts to
/// pushing a bridge you just built back under water — namely the far side brought
/// in by [`crate::crosslink`]'s `relates_to` edges.
///
/// Entries for which `exempt` returns true **do not take the penalty** (that is,
/// they also receive `boost`, putting them on equal footing with the current
/// domain) rather than receiving double. The meaning of an exemption is "don't
/// punish this", not "favour this". For the trade-offs in how broadly that is
/// decided, see `retrieval::bridged_entities`.
pub fn apply_domain_boost<T, F, X>(
    fused: &mut [Fused<T>],
    current_domain: Option<&str>,
    boost: f64,
    domain_of: F,
    exempt: X,
) where
    F: Fn(&T) -> Option<&str>,
    X: Fn(&T) -> bool,
{
    let Some(cur) = current_domain else { return };
    for f in fused.iter_mut() {
        if domain_of(&f.item) == Some(cur) || exempt(&f.item) {
            f.score *= boost;
        }
    }
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Doc {
        id: &'static str,
        domain: Option<&'static str>,
    }

    fn d(id: &'static str) -> Doc {
        Doc { id, domain: None }
    }

    #[test]
    fn multi_channel_consensus_wins() {
        // A is second in both channels; B is first in only one.
        // The core RRF behaviour: cross-channel consensus should beat a high rank
        // in a single channel.
        let channels = vec![
            (Channel::Bm25, vec![d("B"), d("A")]),
            (Channel::Vector, vec![d("C"), d("A")]),
        ];
        let out = rrf(&channels, |x| x.id);
        assert_eq!(
            out[0].item.id, "A",
            "consensus across two channels should rank first: {out:?}"
        );
        assert_eq!(out[0].hits.len(), 2);
    }

    #[test]
    fn dedups_across_channels() {
        let channels = vec![
            (Channel::Bm25, vec![d("A"), d("B")]),
            (Channel::Vector, vec![d("A")]),
            (Channel::Graph, vec![d("A")]),
        ];
        let out = rrf(&channels, |x| x.id);
        assert_eq!(out.len(), 2, "the same entry must not appear twice");
        assert_eq!(
            out[0].hits.len(),
            3,
            "hits from all three channels should be recorded"
        );
    }

    #[test]
    fn preserves_attribution() {
        let channels = vec![(Channel::Recency, vec![d("A")])];
        let out = rrf(&channels, |x| x.id);
        assert_eq!(out[0].hits, vec![(Channel::Recency, 0)]);
    }

    #[test]
    fn rank_order_within_channel_matters() {
        let channels = vec![(Channel::Bm25, vec![d("first"), d("second"), d("third")])];
        let out = rrf(&channels, |x| x.id);
        assert_eq!(out[0].item.id, "first");
        assert!(out[0].score > out[1].score && out[1].score > out[2].score);
    }

    #[test]
    fn domain_boost_reorders() {
        let mut fused = rrf(
            &[(
                Channel::Bm25,
                vec![
                    Doc {
                        id: "work",
                        domain: Some("work"),
                    },
                    Doc {
                        id: "code",
                        domain: Some("coding"),
                    },
                ],
            )],
            |x| x.id,
        );
        assert_eq!(fused[0].item.id, "work");
        apply_domain_boost(&mut fused, Some("coding"), 2.0, |x| x.domain, |_| false);
        assert_eq!(
            fused[0].item.id, "code",
            "facts in the current domain should be lifted up"
        );
    }

    #[test]
    fn domain_boost_exemption_spares_the_bridged_side() {
        // The far side brought in by a cross-domain link is cross-domain by
        // definition: not exempting it amounts to building a bridge and then
        // pushing it back under water
        let mut fused = rrf(
            &[(
                Channel::Bm25,
                vec![
                    Doc {
                        id: "same-domain",
                        domain: Some("coding"),
                    },
                    Doc {
                        id: "bridged",
                        domain: Some("office"),
                    },
                ],
            )],
            |x| x.id,
        );
        let before = fused
            .iter()
            .position(|f| f.item.id == "bridged")
            .expect("should be in the list");

        apply_domain_boost(
            &mut fused,
            Some("coding"),
            1.3,
            |x| x.domain,
            |x| x.id == "bridged",
        );
        let after = fused
            .iter()
            .position(|f| f.item.id == "bridged")
            .expect("should be in the list");
        assert!(
            fused[0].score / fused[1].score < 1.3,
            "the exempted side should carry the same weight as the current domain, actual score ratio {:.3}",
            fused[0].score / fused[1].score
        );
        assert_eq!(
            before, after,
            "an exemption only spares the penalty; it must not push the entry ahead of others"
        );
    }

    #[test]
    fn exemption_is_not_a_double_boost() {
        // "Sparing a penalty" and "granting a favour" are different things:
        // exempting a fact that is already in the current domain must be the
        // identity operation, otherwise entries matching both conditions collect
        // boost² and the ordering quietly drifts
        let build = || {
            rrf(
                &[(
                    Channel::Bm25,
                    vec![
                        Doc {
                            id: "a",
                            domain: Some("coding"),
                        },
                        Doc {
                            id: "b",
                            domain: Some("coding"),
                        },
                    ],
                )],
                |x| x.id,
            )
        };
        let mut plain = build();
        apply_domain_boost(&mut plain, Some("coding"), 1.3, |x| x.domain, |_| false);
        let mut exempted = build();
        apply_domain_boost(
            &mut exempted,
            Some("coding"),
            1.3,
            |x| x.domain,
            |x| x.id == "a",
        );
        assert_eq!(
            plain[0].score, exempted[0].score,
            "exempting an entry that is already in the current domain must not change its score"
        );
    }

    #[test]
    fn ties_break_by_key_not_by_hash_order() {
        // Each entry occupies one channel at rank 0 ⇒ scores and hit counts are
        // all equal, and the only thing left that can order them is the key.
        // Twelve entries rather than three: `HashMap` iteration order varies per
        // process with the hash seed, and with three entries there is a one-in-six
        // chance it happens to come out ascending anyway, which would turn this
        // test into a dud that only goes red occasionally
        let ids = [
            "m4", "b7", "z1", "a9", "q3", "c8", "k2", "w5", "e6", "t0", "n1", "h4",
        ];
        let channels: Vec<(Channel, Vec<Doc>)> =
            ids.iter().map(|id| (Channel::Bm25, vec![d(id)])).collect();
        let out = rrf(&channels, |x| x.id);

        let mut want = ids;
        want.sort_unstable();
        assert_eq!(
            out.iter().map(|f| f.item.id).collect::<Vec<_>>(),
            want.to_vec(),
            "on an exact score tie the order must be ascending by key — otherwise it falls back to HashMap bucket order, and running the same corpus twice changes who lands at top-1 (measured: it made the eval's R@1 jump between 0.630/0.640/0.650)"
        );
    }

    #[test]
    fn empty_input_yields_empty() {
        let out = rrf::<Doc, _, _>(&[], |x| x.id);
        assert!(out.is_empty());
    }

    /// Two channels put `consensus` in 11th place (rank 10); one channel puts
    /// `strong` first. The filler entries differ between channels so that they
    /// don't accumulate consensus of their own.
    fn lone_strong_vs_deep_consensus() -> Vec<(Channel, Vec<Doc>)> {
        let deep = |tag: &'static str| -> Vec<Doc> {
            (0..10)
                .map(|i| Doc {
                    id: Box::leak(format!("{tag}{i}").into_boxed_str()),
                    domain: None,
                })
                .chain(std::iter::once(d("consensus")))
                .collect()
        };
        vec![
            (Channel::Bm25, vec![d("strong")]),
            (Channel::Vector, deep("v")),
            (Channel::Episode, deep("e")),
        ]
    }

    #[test]
    fn peak_bonus_rescues_a_lone_strong_hit() {
        // Only the relative positions of these two entries are compared: the
        // filler entries also sit near the top of their own channels and collect
        // the same compensation, so dragging them into the assertion would only
        // prove the triviality that "everything at rank 0 went up"
        let rank_of = |out: &[Fused<Doc>], id: &str| {
            out.iter()
                .position(|f| f.item.id == id)
                .expect("should be in the list")
        };

        let mut out = rrf(&lone_strong_vs_deep_consensus(), |x| x.id);
        assert!(
            rank_of(&out, "consensus") < rank_of(&out, "strong"),
            "without compensation, deep consensus across two channels (≈0.028) does outrank a lone first place (≈0.016) — that is exactly the cost being fixed"
        );

        // The production default weight. 0.04/2 = 0.02, roughly "hand it one
        // channel's worth of consensus for free"
        apply_peak_bonus(&mut out, 0.04);
        assert!(
            rank_of(&out, "strong") < rank_of(&out, "consensus"),
            "a strong hit ranked first in some channel should be lifted above deep consensus by the compensation term: {out:?}"
        );
    }

    #[test]
    fn peak_bonus_does_not_beat_real_consensus() {
        // It is **compensation**, not a **guaranteed slot**: a result near the top
        // of three channels should still win, otherwise RRF's consensus semantics
        // exist in name only
        let channels = vec![
            (Channel::Bm25, vec![d("strong")]),
            (Channel::Vector, vec![d("agreed"), d("strong")]),
            (Channel::Graph, vec![d("agreed")]),
            (Channel::Episode, vec![d("agreed")]),
        ];
        let mut out = rrf(&channels, |x| x.id);
        apply_peak_bonus(&mut out, 0.04);
        assert_eq!(
            out[0].item.id,
            "agreed",
            "consensus across three channels must not be overturned by a single-channel compensation: {:?}",
            &out[..2]
        );
    }

    #[test]
    fn peak_bonus_of_zero_changes_nothing() {
        let channels = vec![(Channel::Bm25, vec![d("a"), d("b")])];
        let mut out = rrf(&channels, |x| x.id);
        let before: Vec<f64> = out.iter().map(|f| f.score).collect();
        apply_peak_bonus(&mut out, 0.0);
        let after: Vec<f64> = out.iter().map(|f| f.score).collect();
        assert_eq!(
            before, after,
            "a weight of 0 must be the identity operation, otherwise the default behaviour drifts"
        );
    }

    #[test]
    fn recency_decay_reorders_by_age() {
        // This assertion is the only correctness guarantee `RecencyMode::Decay`
        // has: the eval dataset's corpus is written in a single ingest, so
        // created_at has no spread and decay cannot be measured there at all
        let now = chrono::Utc::now();
        let channels = vec![(Channel::Bm25, vec![d("old"), d("new")])];
        let mut out = rrf(&channels, |x| x.id);
        assert_eq!(
            out[0].item.id, "old",
            "before decay, the order follows the BM25 ranking"
        );

        apply_recency_decay(&mut out, now, 0.5, 3.0, |x: &Doc| {
            if x.id == "new" {
                now
            } else {
                now - chrono::Duration::days(90)
            }
        });
        assert_eq!(
            out[0].item.id, "new",
            "when both are hit by the same single channel, one from ninety days ago must not outrank one that just happened"
        );
    }
}

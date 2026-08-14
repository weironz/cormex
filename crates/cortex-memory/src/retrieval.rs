//! The retriever — stringing four-channel recall, RRF fusion, and budget
//! truncation into one path.
//!
//! Division of labour:
//! - `cortex-store::recall_*` handles the **fetching** for each channel and
//!   makes no ranking-policy decisions
//! - [`fusion`](crate::fusion) handles **fusion**
//! - [`injection`](crate::injection) handles **the egress format**
//! - this module only orchestrates them, and decides each channel's recall width
//!
//! # Which channels, and why
//!
//! Each channel has a well-defined blind spot, and the blind spots do not
//! overlap — that is the entire justification for having several. The "exclusive
//! question count" (how many questions only this channel found the gold for) is
//! the one number that can falsify it.
//!
//! | Channel | Strong at | Blind to | Default |
//! |---|---|---|---|
//! | BM25 | Proper nouns, exact wording | Say it differently and nothing comes back | on |
//! | Vector | Semantic proximity | Very short proper-noun queries | on |
//! | Graph | Relational reasoning ("what does the project he owns use") | Doesn't fire when the query's surface words are unrelated | on |
//! | episodes | Provenance follow-ups (the user is quoting **the original words**) | Sibling facts take the top slots as a block | **off**, see [`Retriever::episode_channel`] |
//! | Recency | — | Completely unrelated to the query | **off**, see [`RecencyMode`] |
//!
//! The last two were not cut (both the implementation and the switch are still
//! here); they are **not enabled by default**: in the evals each of them made
//! ranking worse, and the one metric that improved (false recall) turned out on
//! inspection to be dilution rather than discrimination.
//!
//! After fusion there are two more gates that have nothing to do with "which
//! channels", and neither can be skipped:
//!
//! - [`fusion::apply_peak_bonus`] — fixes RRF's "weak consensus outranks a strong
//!   hit" cost
//! - [`BGE_M3_SEMANTIC_FLOOR`] — fixes the cost of RRF scores carrying no
//!   absolute relevance

use chrono::{DateTime, Utc};
use cortex_core::{CortexError, Result};
use cortex_store::{Fact, Store};

use crate::embed::Embedder;
use crate::fusion::{self, Channel, Fused};
use crate::injection::{Budget, MemoryItem};
use crate::tokenize;

/// The fixed value of the abstention threshold. For what it means and how the
/// value was chosen, see [`Retriever::abstain_below`].
///
/// It is public for exactly one purpose: reports and the regression gate need to
/// be able to record which threshold a given run used.
pub const DEFAULT_ABSTAIN_BELOW: f64 = 0.030;

/// The semantic floor: if the **cosine distance** of the nearest fact in the
/// whole database exceeds it, abstain entirely.
///
/// # This number holds only inside bge-m3's vector space
///
/// It was swept out by the evals (`evals/sweep.py
/// floor:0.40,0.45,0.50,0.55,0.60`):
///
/// | Floor | R@5 | MRR | False recall@5 | Injected count on "should find nothing" |
/// |---|---|---|---|---|
/// | none | 0.943 | 0.833 | 0.393 | 32.9 |
/// | 0.55 | 0.923 | 0.817 | 0.393 | 32.9 |
/// | **0.50** | **0.923** | **0.812** | **0.286** | **22.4** |
/// | 0.45 | 0.851 | 0.766 | 0.143 | 6.4 |
/// | 0.40 | 0.675 | 0.617 | 0.107 | 3.5 |
///
/// 0.55 pays 0.50's full recall cost and buys back not one bit of noise
/// reduction, so it is strictly worse than 0.50; 0.45 starts out losing seven
/// questions to gain four, an inverted ratio. 0.50 trades 0.020 of R@5 (two
/// questions) for 0.107 of false recall (three questions) — under this module's
/// own preference ("better to give no memory than to give the wrong memory")
/// that trade holds up.
///
/// The cost is two specific, known questions: `赵敏是谁` and `jieba-rs`. Both are
/// **very short proper-noun queries** — the query vector for two or three
/// characters is inherently diffuse, so its nearest-neighbour distance is bound to
/// run large even when BM25 hits it exactly. Their outcome is **abstention**
/// (nothing returned at all) rather than a wrong answer, which is precisely the
/// failure mode this design prefers.
///
/// **Changing the embedding model requires re-sweeping this curve.** Distance is
/// calibrated by the model, and in a different space this number means nothing —
/// which is why [`Retriever::new`] only installs it for a genuinely semantic
/// backend.
pub const BGE_M3_SEMANTIC_FLOOR: f64 = 0.50;

/// Whether a semantic floor should be installed for this vector space.
///
/// This is an **allowlist**, and that direction is deliberate. The costs of the
/// two ways of being wrong are not symmetric:
///
/// - Should have installed one but didn't → a bit more noise gets in; visible and
///   tunable
/// - Installed one that shouldn't be there → recall is **silently zeroed**;
///   retrieval returns as usual, it just always returns empty
///
/// The hash stub (`hash-stub-v1`) is exactly the latter case: its "distance"
/// reflects only character-bigram overlap and has nothing to do with relevance, so
/// putting 0.50 on it would zero out offline development and network-free CI
/// across the board.
///
/// # A note on the `api:` prefix
///
/// Backends going through a remote `/v1/embeddings` write `api:<model name>` into
/// the database. That is a genuine semantic space, so a floor is installed;
/// **but the number 0.50 was swept on bge-m3-int8**, and against a different
/// model (text-embedding-v3, jina-v3, …) it means nothing — distance is calibrated
/// by the model. Changing models means re-sweeping that curve in `evals/`, or
/// passing an explicit value via [`Retriever::with_semantic_floor`].
fn default_semantic_floor(model_id: &str) -> Option<f64> {
    let semantic = model_id.starts_with("fastembed:") || model_id.starts_with("api:");
    semantic.then_some(BGE_M3_SEMANTIC_FLOOR)
}

/// How many seed entities graph traversal starts from.
///
/// It is public because the evals' per-channel diagnostics have to reproduce this
/// same orchestration — it used to be a private hard-coded constant, so the eval
/// side could only copy a 12 across, and changing it here would not change it
/// there, quietly distorting the per-channel numbers.
pub const GRAPH_SEED_LIMIT: i64 = 12;

/// Wrap a list of facts already ordered newest-first into fusion results.
///
/// The score is `1/(i+1)` rather than RRF's `1/(k+i+1)`: there is no second
/// channel to fuse with on this path, and a steeper kernel keeps the abstention
/// threshold discriminating on a full-snapshot replay — otherwise every score in
/// the snapshot bunches up around 0.0164 and it becomes all-in or all-out.
fn chronological(facts: Vec<Fact>) -> Vec<Fused<Fact>> {
    facts
        .into_iter()
        .enumerate()
        .map(|(i, item)| Fused {
            item,
            score: 1.0 / (i as f64 + 1.0),
            hits: vec![(Channel::Recency, i)],
        })
        .collect()
}

/// The entities connected by a `relates_to` edge within the graph channel.
///
/// # Scope of the exemption: why "the edge's two endpoints" and not "the whole
/// channel"
///
/// For the bridges [`crate::crosslink`] builds to actually do anything, the domain
/// penalty has to let them past (the roadmap's words: "that channel needs an
/// exemption from the domain penalty during fusion"). But "exempt the entire graph
/// channel" is far too broad: graph traversal naturally drags in a pile of
/// cross-domain facts that have nothing to do with any bridge (entities like
/// "张伟" that inherently straddle two domains), and sparing all of them would
/// amount to using this change to quietly cut away half of domain weighting's
/// scope, with no eval evidence for it whatsoever.
///
/// What is taken here is the narrowest self-consistent scope: **only when a
/// `relates_to` edge actually appeared in this recall are the facts anchored to
/// its two connected entities exempted.** When the bridge was never walked (the
/// query didn't hit either of its ends at all) this set is empty, and behaviour is
/// bit-for-bit what it was before the change.
///
/// Both endpoints are exempted rather than only the "far" one because which end
/// counts as far depends on the current domain, and the near end is already in the
/// current domain, where sparing the penalty is the identity operation (see
/// `fusion::exemption_is_not_a_double_boost`). Not having to work out the far end
/// means not having to thread the seed entities all the way down here.
///
/// # A known imprecision
///
/// **All** of an endpoint entity's facts are spared, not just the ones that got
/// pulled in because of this bridge. Doing it exactly would require replaying the
/// traversal path, and `recall_graph` returns a truncated result set, so the
/// reachability such a replay reconstructs would itself be wrong. So this takes an
/// approximation that is slightly too broad but whose blast radius never exceeds
/// graph traversal's own — the degree cap (`crosslink::DEFAULT_DEGREE_CAP`) is
/// what actually pins that radius down.
fn bridged_entities(channels: &[(Channel, Vec<Fact>)]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for (channel, facts) in channels {
        if *channel != Channel::Graph {
            continue;
        }
        for f in facts {
            if f.predicate != crate::crosslink::RELATES_TO {
                continue;
            }
            out.insert(f.subject_id.clone());
            if let Some(obj) = &f.object_entity_id {
                out.insert(obj.clone());
            }
        }
    }
    out
}

/// Merge the vector recalls from the current space and the old space into **one**
/// [`Channel::Vector`].
///
/// # Why interleave by rank instead of putting the whole current space first
///
/// During a backfill there are **more** facts in the old space (the entire
/// existing corpus is over there) and only the newly written ones in the current
/// space. Putting the current space wholesale in front hands each of those few
/// new facts a front-row rank while pushing every historical memory past position
/// 41 — and since RRF's kernel is `1/(k+rank)`, that is essentially the same as
/// not recalling them at all.
///
/// Interleaving (new 1, old 1, new 2, old 2, …) puts the two spaces on equal
/// footing by rank. This isn't "more accurate"; it is **the only legitimate way
/// to avoid introducing a bias given that the two sides' distances are not
/// comparable**.
///
/// # Why not merge by distance
///
/// The cosine distances of two models are not on the same scale. They look
/// comparable (both are reals in 0..2), but the resulting order is meaningless —
/// and it raises no error, it just makes retrieval quality inexplicably bad.
///
/// When no legacy model is attached `legacy` is empty and the function degenerates
/// into "return the current space unchanged".
fn merge_vector_spaces(current: Vec<(Fact, f64)>, legacy: Vec<(Fact, f64)>) -> Vec<Fact> {
    if legacy.is_empty() {
        return current.into_iter().map(|(f, _)| f).collect();
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(current.len() + legacy.len());
    let mut a = current.into_iter();
    let mut b = legacy.into_iter();
    loop {
        let mut progressed = false;
        for it in [&mut a as &mut dyn Iterator<Item = (Fact, f64)>, &mut b] {
            if let Some((f, _)) = it.next() {
                progressed = true;
                // When the same fact is hit on both sides, keep only the first
                // occurrence — that is, the position given to it by whichever
                // space ranked it higher
                if seen.insert(f.id.clone()) {
                    out.push(f);
                }
            }
        }
        if !progressed {
            break;
        }
    }
    out
}

fn is_bridged(f: &Fact, bridged: &std::collections::HashSet<String>) -> bool {
    if bridged.is_empty() {
        return false;
    }
    bridged.contains(&f.subject_id)
        || f.object_entity_id
            .as_deref()
            .is_some_and(|id| bridged.contains(id))
}

/// The recall width of each channel.
///
/// An order of magnitude larger than the final injected count — RRF only means
/// anything if it has room to fuse, and with five entries per channel
/// "cross-channel consensus" has no way to show itself at all.
#[derive(Debug, Clone, Copy)]
pub struct RecallWidth {
    pub bm25: i64,
    pub vector: i64,
    pub graph: i64,
    pub recent: i64,
    /// How many facts the episodes channel takes
    pub episode: i64,
    /// How many raw documents the episodes channel takes first (they are then
    /// expanded into facts)
    pub episode_docs: i64,
    /// How many days back the recency channel looks
    pub recent_days: i64,
    /// The hop count for graph traversal
    pub graph_hops: i32,
}

impl Default for RecallWidth {
    fn default() -> Self {
        Self {
            bm25: 40,
            vector: 40,
            graph: 30,
            recent: 20,
            // A notch narrower than the other channels: one episode routinely
            // expands into three to five facts, so a width of 40 would fill this
            // channel's tail entirely with "other things mentioned in passing
            // during the same conversation"
            episode: 20,
            episode_docs: 8,
            recent_days: 7,
            graph_hops: 2,
        }
    }
}

/// What form "recency" should take, if any.
///
/// # Decision: `Off`
///
/// The roadmap listed this as undecided ("keep it as a separate recall channel"
/// versus "change it to a recency decay multiplied into the fused score"). The
/// answer the evals gave is **neither**:
///
/// | Form | R@1 | R@5 | R@10 | MRR | Mean gold rank | Mean injected | False recall@5 |
/// |---|---|---|---|---|---|---|---|
/// | `Channel` | 0.570 | 0.928 | 0.959 | 0.745 | 3.05 | 38.3 | 0.250 |
/// | **`Off`** | **0.673** | 0.923 | **0.969** | **0.812** | **2.78** | **29.5** | 0.286 |
/// | `Decay` 0.3/3d | 0.673 | 0.923 | 0.969 | 0.812 | 2.78 | 40.1 | 0.286 |
///
/// **`Channel` is out**: its exclusive question count is **0**, its own R@5 is
/// only 0.094, and yet it drags R@1 down by 0.103 and MRR by 0.067 while pushing
/// the injected count up 30%. The one column it "wins" (false recall 0.250 <
/// 0.286) is fake — it simultaneously raises the injected count on "should find
/// nothing" questions from 22.4 to 28.2, which means **it used more irrelevant
/// memories to push the known distractor out of the top 5**. The forbidden set is
/// only a sample of the wrong answers, so swapping a sampled wrong answer for an
/// unsampled one just makes the metric look better.
///
/// The more fundamental problem is that it is aimed at the wrong thing:
/// `recall_recent` returns the facts **most recently persisted**, not "what we
/// were just talking about". Extraction is asynchronous, so those two do not
/// coincide in time. "That thing we just said" is already carried directly by the
/// conversation history and needs no retrieval at all; and genuinely tracing the
/// original wording back out of memory is [`Channel::Episode`]'s job.
///
/// **`Decay` can be neither falsified nor confirmed**: every fact in this dataset
/// was written within seconds during a single ingest, so `created_at` has no
/// spread at all and the decay coefficient is identical for every entry — the
/// ranking metrics for `Decay` and `Off` in the table match digit for digit, and
/// the only difference is the injected count (the multiplier lifts every score
/// past the abstention threshold together). Judging it requires the dataset to
/// grow a "corpus spanning weeks" dimension first, which is the dataset's next
/// piece of work and not something that can be settled now. The implementation
/// stays here, disabled by default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecencyMode {
    /// As its own channel feeding into RRF (the historical behaviour, since ruled
    /// out by the data)
    Channel,
    /// Occupies no recall channel; merely multiplies a freshness coefficient into
    /// the fused score (undecided, off by default)
    Decay { strength: f64, half_life_days: f64 },
    /// Not at all — **the current decision**
    Off,
}

/// The complete result of one retrieval, with attributable per-channel hits.
#[derive(Debug)]
pub struct Retrieved {
    /// Already sorted descending by fused score and already truncated to budget
    pub items: Vec<MemoryItem>,
    /// Attribution information, the same length as `items` — on a bad case this
    /// is the only thing that can locate the problem
    pub attribution: Vec<Attribution>,
}

#[derive(Debug, Clone)]
pub struct Attribution {
    pub fact_id: String,
    pub score: f64,
    /// The recall channels that hit it
    pub channels: Vec<&'static str>,
}

/// Cloning is cheap: `E` is a `SharedEmbedder` (an `Arc`) and everything else is
/// a scalar. Multi-tenant wiring swaps the database per request, and that step
/// clones the retriever along with it.
#[derive(Clone)]
pub struct Retriever<E: Embedder> {
    embedder: E,
    width: RecallWidth,
    budget: Budget,
    /// The weighting coefficient for the current conversation's domain. 1.0 means
    /// no weighting.
    domain_boost: f64,
    /// Entries scoring below this after fusion are not injected.
    ///
    /// # It governs "how many to inject", **not** "is this relevant"
    ///
    /// This was misunderstood at first, and is worth spelling out. An RRF score is
    /// a function of **rank** and carries no absolute relevance whatsoever: when
    /// the answer simply isn't in the database, every channel still returns its
    /// own best few, they still corroborate each other, and the fused score is
    /// still high. Sweeping the threshold from 0 all the way to 0.030 left the
    /// false-recall rate sitting at 0.286, **unmoved to the last digit**; it only
    /// starts falling at 0.055, by which point R@5 has already dropped from 0.923
    /// to 0.840.
    ///
    /// In other words: **any threshold that can hold false recall down has already
    /// killed recall.** The "should we inject at all" judgement is made by
    /// [`Self::semantic_floor`] using cosine distance, which is the signal that
    /// actually carries an absolute scale.
    ///
    /// Its real value is cutting the injected volume, and that volume is itself
    /// cost and attack surface: an irrelevant memory that reaches the prompt is
    /// both raw material for hallucination and a vehicle for memory poisoning —
    /// an attacker only has to get one malicious fact persisted and it will be
    /// injected into **every** conversation.
    ///
    /// | Threshold | R@5 | R@10 | MRR | False recall@5 | Mean injected |
    /// |---|---|---|---|---|---|
    /// | 0 (off) | 0.923 | 0.969 | 0.817 | 0.286 | 45.2 |
    /// | 0.017 (old value) | 0.923 | 0.969 | 0.817 | 0.286 | 29.5 |
    /// | **0.030** | **0.923** | **0.969** | **0.817** | 0.286 | **12.9** |
    /// | 0.040 | 0.923 | 0.938 | 0.809 | 0.250 | 6.0 |
    /// | 0.055 | 0.840 | 0.845 | 0.769 | 0.179 | 2.3 |
    ///
    /// For how the value was chosen, see [`Retriever::new`].
    abstain_below: f64,
    /// What form recency takes. See [`RecencyMode`].
    recency: RecencyMode,
    /// Whether the episodes channel (tracing back through L0 raw text) is
    /// enabled.
    ///
    /// # Decision: off by default, but both the implementation and the eval
    /// questions stay
    ///
    /// The motivation is real: all four other channels look only at
    /// `facts.statement`, i.e. the single sentence the extractor rewrote, whereas
    /// in a **provenance follow-up** like "that thing we talked about last time"
    /// the user is quoting the original words — and the original phrasing often
    /// doesn't have a single character in the statement. The GIN index on
    /// `episodes.tsv` was built and then never used by anyone. Six provenance
    /// follow-up questions (q106–q111) were added specifically for this.
    ///
    /// But the data does not support making it the default. Across 111 questions,
    /// the 2×2 of `episodes × peak bonus`:
    ///
    /// | | R@5 | R@10 | MRR | Mean rank | False recall@5 | Mean injected |
    /// |---|---|---|---|---|---|---|
    /// | Neither on | 0.887 | 0.931 | 0.786 | 2.92 | 0.286 | 22.9 |
    /// | episodes only | 0.887 | 0.911 | 0.778 | 3.27 | 0.214 | 27.5 |
    /// | **peak bonus only** | **0.923** | **0.969** | **0.812** | **2.78** | 0.286 | 29.5 |
    /// | Both on | 0.912 | 0.948 | 0.804 | 2.93 | 0.214 | 33.6 |
    ///
    /// Turning it on makes ranking worse under both bonus settings (R@10 −0.020,
    /// mean rank +0.35), and the only thing that improves is false recall — which
    /// is **dilution**, not **discrimination**: within the same pairing, the
    /// injected count on "should find nothing" questions goes from 22.4 to 26.9,
    /// so it is the extra irrelevant facts crowding the one distractor the dataset
    /// happens to have labelled out of the top 5. The forbidden set is only a
    /// sample of the wrong answers, and swapping an unsampled wrong answer for a
    /// sampled one just makes the metric look better. (The same ruler also ruled
    /// out the recency channel; see [`RecencyMode`].)
    ///
    /// The structural cause: this channel sorts by **the episode's `ts_rank`**, so
    /// **every** fact extracted from the matching stretch of conversation occupies
    /// the top slots as a block — the gold's sibling facts push the gold itself
    /// down. Overturning this decision means solving that first, not widening the
    /// channel.
    ///
    /// Of the six added questions, q106/q107/q108 did rise because of it and none
    /// fell — but with a 69-entry corpus they **answer correctly with it off too**
    /// (R@5 is already 1.0). The corpus is simply too small; BM25 blundering
    /// around a few dozen entries hits the target anyway. This channel's real
    /// value can only be measured once the corpus is in the thousands and the
    /// original wording genuinely diverges from the statement.
    episode_channel: bool,
    /// The compensation weight for a lone strong hit; see
    /// [`fusion::apply_peak_bonus`]. 0 = off.
    peak_bonus: f64,
    /// The semantic floor: if the cosine distance of the nearest fact in the whole
    /// database exceeds it, abstain entirely.
    ///
    /// This and [`Self::abstain_below`] are two different gates and neither can be
    /// dropped: the latter governs **how many to inject**, this one governs
    /// **whether to inject at all**. An RRF score is a function of rank and stays
    /// high even when the answer isn't in the database; cosine distance is the
    /// absolute quantity the model calibrates. `None` = no floor.
    semantic_floor: Option<f64>,
    /// Whether the side pulled in by a `relates_to` edge is spared the domain
    /// penalty. See [`bridged_entities`]. It is kept as a switch so it can be
    /// A/B tested.
    bridge_exemption: bool,
    /// The **previous** embedding model. When set, the old space is queried too
    /// during a backfill.
    ///
    /// See [`Self::with_legacy`]. `None` by default — the overwhelming majority of
    /// deployments have never changed models.
    legacy: Option<crate::embed::SharedEmbedder>,
}

impl<E: Embedder> Retriever<E> {
    pub fn new(embedder: E) -> Self {
        Self {
            semantic_floor: default_semantic_floor(embedder.model_id()),
            embedder,
            width: RecallWidth::default(),
            budget: Budget::default(),
            domain_boost: 1.3,
            // 0.030 = the **last point on the curve at which all five headline
            // metrics still match "no threshold at all" digit for digit** (R@1 /
            // R@5 / R@10 / MRR / false recall@5 are all identical), while the
            // injected count drops from 45.2 to 12.9. Only above that (0.040) does
            // it start trading away R@10 and MRR.
            //
            // The previous version used 0.017, on the reasoning that it is "just
            // above a single channel's first place at 1/61≈0.0164, i.e. discard
            // anything only one channel found". That reasoning is not wrong in
            // itself, but it anchors the threshold to a quantity that has
            // **nothing to do with the scores of true hits**: true hits sit
            // broadly above 0.03, so 0.017 was letting in twice as much noise for
            // nothing. The anchor should be "the distribution of true hits", and
            // the only way to know where that is is to sweep — which is exactly
            // what "build the ruler before tuning the parameters" means.
            //
            // Note that it interacts with `peak_bonus` (the compensation term
            // lifts true hits by roughly 0.02 across the board), so moving either
            // one means re-sweeping this curve.
            abstain_below: DEFAULT_ABSTAIN_BELOW,
            // For the decision see the docs on `RecencyMode`: 0 exclusive
            // questions, an own R@5 of only 0.101, and yet it drags R@1 and MRR
            // down and pushes injection up 27%. Cut.
            recency: RecencyMode::Off,
            // For the decision see the long comment on the episodes channel above
            // `Retriever::retrieve`: the implementation stays and the questions
            // were added, but measured ranking is neutral-to-negative, so it is
            // off by default.
            episode_channel: false,
            // See `fusion::apply_peak_bonus`. The basis for 0.04 is scale, not the
            // curve's maximum: for a result ranked first in some channel the
            // compensation term is worth 0.04/2 = 0.02, roughly 1.2 RRF units
            // (0.0164) — that is, "hand it one channel's worth of consensus for
            // free". Enough to rescue a lone strong hit from being drowned by weak
            // three-channel consensus, and not enough to override real consensus.
            //
            // Measured (111 questions, everything else at production defaults):
            // turning it on moves R@5 0.887→0.923, R@10 0.931→0.969, MRR
            // 0.786→0.812, and mean gold rank 2.92→2.78, with false recall@5
            // completely unmoved — this one is pure gain.
            //
            // Larger weights score higher still on the curve (R@5 is 0.948 at
            // w=0.12), but by then the compensation term exceeds the sum of the
            // channels' RRF contributions, the ranking has effectively degenerated
            // into "take the best rank across channels", RRF's consensus semantics
            // exist in name only, and the injected count has gone from 33 to 41.
            // The maximum of a curve is not the same thing as the right default.
            peak_bonus: 0.04,
            // The roadmap's words: "that channel needs an exemption from the domain
            // penalty during fusion". For the scope of the exemption, and why it
            // isn't "the whole graph channel", see `bridged_entities`
            bridge_exemption: true,
            legacy: None,
        }
    }

    /// Turn off the domain-penalty exemption for cross-domain bridges. For eval
    /// A/B testing.
    #[must_use]
    pub fn with_bridge_exemption(mut self, on: bool) -> Self {
        self.bridge_exemption = on;
        self
    }

    #[must_use]
    pub fn with_width(mut self, width: RecallWidth) -> Self {
        self.width = width;
        self
    }

    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// Adjust the abstention threshold. Setting it to 0 disables abstention (for
    /// eval A/B testing).
    #[must_use]
    pub fn with_abstain_below(mut self, threshold: f64) -> Self {
        self.abstain_below = threshold;
        self
    }

    /// Switch to a different form of recency. For eval A/B testing.
    #[must_use]
    pub fn with_recency(mut self, mode: RecencyMode) -> Self {
        self.recency = mode;
        self
    }

    /// Toggle the episodes channel. For eval A/B testing.
    #[must_use]
    pub fn with_episode_channel(mut self, on: bool) -> Self {
        self.episode_channel = on;
        self
    }

    /// The weight of the lone-strong-hit compensation. For eval A/B testing.
    #[must_use]
    pub fn with_peak_bonus(mut self, weight: f64) -> Self {
        self.peak_bonus = weight;
        self
    }

    /// Attach the **previous** embedding model so the old space is queried too
    /// during a backfill.
    ///
    /// # Why it is needed
    ///
    /// After a model change, the vectors of old facts exist only in the old space,
    /// and vector queries filter on `embedding_model` — so those facts cannot
    /// enter vector recall at all **until the backfill reaches them**. On a
    /// database large enough that the backfill takes days, that is days of partial
    /// amnesia.
    ///
    /// With it attached, the query is **encoded into both spaces**, each is
    /// queried once, and the results are deduplicated by id and merged into a
    /// single [`Channel::Vector`].
    ///
    /// # Why not add a new channel
    ///
    /// The moment RRF's input structure changes, every number in
    /// `scripts/evals-baseline.*.json` becomes incomparable — and that baseline is
    /// the **only** basis for judging whether a change made retrieval worse. A
    /// temporary state that exists solely during a backfill is not worth
    /// invalidating the baseline for.
    ///
    /// # ⚠️ It cannot rescue the fastembed → api migration
    ///
    /// There the old model is in-process fastembed, which is no longer compiled
    /// into the binary by default. See
    /// [`crate::embed_api::ApiEmbedder::legacy_from_env`].
    #[must_use]
    pub fn with_legacy(mut self, legacy: Option<crate::embed::SharedEmbedder>) -> Self {
        self.legacy = legacy;
        self
    }

    /// The semantic floor (an upper bound on cosine distance). `None` disables it.
    #[must_use]
    pub fn with_semantic_floor(mut self, max_distance: Option<f64>) -> Self {
        self.semantic_floor = max_distance;
        self
    }

    /// The semantic floor currently in effect. The default depends on the
    /// embedding backend, so without a way for callers to ask, there is no way for
    /// them to record it in a log or an eval report.
    #[must_use]
    pub fn semantic_floor(&self) -> Option<f64> {
        self.semantic_floor
    }

    /// Four-channel recall + RRF + domain weighting + budget truncation.
    ///
    /// `context_window` is used to compute the injection budget; `domain` is the
    /// current conversation's domain.
    pub async fn retrieve(
        &self,
        store: &Store,
        query: &str,
        domain: Option<&str>,
        context_window: usize,
    ) -> Result<Retrieved> {
        let (channels, nearest) = self.recall_all(store, query).await?;

        // The semantic floor: if even the nearest fact in the whole database is out
        // of reach, the answer to this question simply isn't in memory. This has to
        // be judged **before** fusion, because a fused score reflects only
        // cross-channel consensus, never relevance.
        if let (Some(floor), Some(d)) = (self.semantic_floor, nearest)
            && d > floor
        {
            return Ok(Retrieved {
                items: Vec::new(),
                attribution: Vec::new(),
            });
        }

        // The domain-penalty exemption list has to be computed from the graph
        // channel **before** fusion: fusion kneads the per-channel raw lists into
        // one flat table, and `Fused::hits` keeps only the channel names, so by
        // that point there is no longer any way to tell which entry was pulled in
        // by a bridge
        let bridged = if self.bridge_exemption {
            bridged_entities(&channels)
        } else {
            std::collections::HashSet::new()
        };

        let mut fused = fusion::rrf(&channels, |f: &Fact| f.id.clone());
        self.post_fuse(&mut fused);
        fusion::apply_domain_boost(
            &mut fused,
            domain,
            self.domain_boost,
            |f| f.domain.as_deref(),
            |f| is_bridged(f, &bridged),
        );

        Ok(self.finish(fused, context_window))
    }

    /// Replay by **system time** to get "the whole picture as it stood": a snapshot
    /// ordered descending by `created_at`.
    ///
    /// This path should only be taken when there are **no usable query terms**
    /// (for example a bulk export along the lines of "dump the state as of then").
    /// Any replay that comes with a question goes through
    /// [`Retriever::retrieve_as_of_ranked`] — see the eval numbers there on why
    /// "the whole picture" is not the same as "usable".
    ///
    /// **TODO (outside the boundary of this change)**:
    /// `cortexd::live::AppState::retrieve` currently routes every request carrying
    /// an `as_of` through this method, even though `q.q` (the user's question) is
    /// right there in hand. Changing that one call site to
    /// `retrieve_as_of_ranked(&self.store, &q.q, …)` would lift R@5 on the time
    /// replay questions from 0.417 to 0.833.
    pub async fn retrieve_as_of(
        &self,
        store: &Store,
        as_of: DateTime<Utc>,
        limit: i64,
        context_window: usize,
    ) -> Result<Retrieved> {
        let facts = store
            .facts_as_of(as_of, limit)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))?;

        Ok(self.finish(chronological(facts), context_window))
    }

    /// Replay + retrieval: relevance ranking **on top of** the snapshot as it stood
    /// at `as_of`.
    ///
    /// # Why the original version didn't work
    ///
    /// The first implementation had only [`Retriever::retrieve_as_of`], on the
    /// stated reasoning that "a replay wants the whole picture as it stood, not
    /// what is most relevant to the query; mixing in relevance ranking would make
    /// the replay result depend on how the question was phrased". That reasoning
    /// shows no problems with a corpus of a few dozen entries, and the evals
    /// falsified it:
    ///
    /// - R@5 on the time replay questions was only **0.417**, while the gold's mean
    ///   rank was **23.4**
    /// - That is, the gold was **in the snapshot every time**, just sitting dozens
    ///   of positions down
    /// - Add budget truncation and even R@∞ fell to 0.806 — the gold was being
    ///   truncated away entirely
    ///
    /// "Ask about the object storage plan from three months ago, get back the 40
    /// most recent **unrelated** facts" is not the whole picture, it is noise. At a
    /// hundred thousand memories it only gets worse.
    ///
    /// # What it does now
    ///
    /// Three channels, all **confined to the same as-of snapshot**, then RRF:
    ///
    /// | Channel | Role |
    /// |---|---|
    /// | BM25 (within the snapshot) | Exact wording like "MinIO" or "5442" |
    /// | Vector (within the snapshot) | Rephrasings like "the storage plan from back then" |
    /// | Reverse chronological (within the snapshot) | The fallback — degenerates to the whole picture when the first two come up empty |
    ///
    /// The key point is that **the snapshot is a shared precondition, not a ranking
    /// criterion**: all three channels query the same set of facts with
    /// `created_at <= as_of` and not invalidated as of then, so the "what was
    /// believed at the time" semantics are entirely preserved. What is discarded is
    /// only "sorted reverse chronologically", an ordering unrelated to the question.
    /// The third channel stays so that a phrasing with no retrieval signal at all,
    /// like "walk me through every conclusion from back then", doesn't degenerate
    /// into coming back empty-handed.
    pub async fn retrieve_as_of_ranked(
        &self,
        store: &Store,
        query: &str,
        as_of: DateTime<Utc>,
        limit: i64,
        context_window: usize,
    ) -> Result<Retrieved> {
        let tsquery = tokenize::to_tsquery_input(query);
        let qvec = self.embedder.embed_one(query).await?;
        let embedding = cortex_store::Vector::from(qvec);

        let (bm25, vector, chrono) = tokio::join!(
            store.facts_as_of_bm25(as_of, &tsquery, limit),
            store.facts_as_of_vector(as_of, &embedding, self.embedder.model_id(), limit),
            store.facts_as_of(as_of, limit),
        );

        let to_err = |e: cortex_store::StoreError| CortexError::Store(e.to_string());
        let channels = vec![
            (Channel::Bm25, bm25.map_err(to_err)?),
            (Channel::Vector, vector.map_err(to_err)?),
            (Channel::Recency, chrono.map_err(to_err)?),
        ];

        let mut fused = fusion::rrf(&channels, |f: &Fact| f.id.clone());
        fusion::apply_peak_bonus(&mut fused, self.peak_bonus);
        // Recency decay is **not** stacked on here: within a snapshot, "new" is
        // relative to `as_of` rather than to now, and computing age against now
        // would flatten the entire snapshot uniformly
        Ok(self.finish(fused, context_window))
    }

    // ─────────────────────── Internals ───────────────────────

    /// The raw recall from each channel, plus **the nearest cosine distance in the
    /// whole database** (for the semantic floor decision).
    async fn recall_all(
        &self,
        store: &Store,
        query: &str,
    ) -> Result<(Vec<(Channel, Vec<Fact>)>, Option<f64>)> {
        let tsquery = tokenize::to_tsquery_input(query);
        let qvec = self.embedder.embed_one(query).await?;
        let embedding = cortex_store::Vector::from(qvec);

        // Seeds for graph traversal: the tokens cut out of the query are matched
        // against entity names
        let terms: Vec<String> = tokenize::to_tsvector_input(query)
            .split_whitespace()
            .map(str::to_string)
            .collect();

        let seeds = store
            .entities_matching(&terms, GRAPH_SEED_LIMIT)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))?;

        // The channels run concurrently — they don't depend on each other, and
        // running them serially is pure waste. A disabled channel that still issues
        // its query costs a round trip for nothing, and simply setting its width to
        // 0 does not work (the storage layer clamps it to 1), so the whole channel
        // has to be skipped.
        let want_recent = self.recency == RecencyMode::Channel;
        // The old-space channel goes into this join too: it has to run an embedding
        // first (one HTTP call), and putting it in front in series would add a round
        // trip to every retrieval for nothing. When no legacy model is attached it
        // is a future that returns empty immediately, at zero cost
        let legacy_vector = async {
            let Some(legacy) = &self.legacy else {
                return Vec::new();
            };
            let Ok(qvec) = legacy.embed_one(query).await else {
                // The old model being unreachable **must not fail the whole
                // retrieval** — it is a supplement that exists during the
                // backfill, and the main path has already returned the current
                // space's results. Degrade to "query the new space only", which is
                // exactly the behaviour when no legacy model is attached
                tracing::warn!("legacy-model embedding failed; querying the current space only");
                return Vec::new();
            };
            let v = cortex_store::Vector::from(qvec);
            store
                .recall_vector_scored(&v, legacy.model_id(), self.width.vector)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(
                        error = %e,
                        "recall from the legacy space failed; querying the current space only"
                    );
                    Vec::new()
                })
        };
        let (bm25, vector, graph, recent, episode, legacy_vector) = tokio::join!(
            store.recall_bm25(&tsquery, self.width.bm25),
            store.recall_vector_scored(&embedding, self.embedder.model_id(), self.width.vector),
            store.recall_graph(&seeds, self.width.graph_hops, self.width.graph),
            async {
                if want_recent {
                    store
                        .recall_recent(self.width.recent_days, self.width.recent)
                        .await
                } else {
                    Ok(Vec::new())
                }
            },
            async {
                if self.episode_channel {
                    store
                        .recall_via_episodes(&tsquery, self.width.episode_docs, self.width.episode)
                        .await
                } else {
                    Ok(Vec::new())
                }
            },
            legacy_vector,
        );

        let to_err = |e: cortex_store::StoreError| CortexError::Store(e.to_string());
        let scored = vector.map_err(to_err)?;
        // Already sorted ascending by distance, so the first entry is the nearest in
        // the whole database.
        //
        // **Taken from the current space only**: the semantic floor of 0.50 is
        // calibrated on bge-m3, whereas the old space's distances are calibrated by
        // the old model, and the two are not comparable. Running an old-space
        // distance against this threshold yields a judgement with no meaning — and
        // the way that manifests is "whether we abstain becomes random", with no
        // error raised anywhere. When the current space is empty, nearest is None,
        // which means no abstention decision is made at all
        let nearest = scored.first().map(|(_, d)| *d);
        let vector = merge_vector_spaces(scored, legacy_vector);

        Ok((
            vec![
                (Channel::Bm25, bm25.map_err(to_err)?),
                (Channel::Vector, vector),
                (Channel::Graph, graph.map_err(to_err)?),
                (Channel::Recency, recent.map_err(to_err)?),
                (Channel::Episode, episode.map_err(to_err)?),
            ],
            nearest,
        ))
    }

    /// Adjustments made after RRF and before domain weighting.
    fn post_fuse(&self, fused: &mut [Fused<Fact>]) {
        fusion::apply_peak_bonus(fused, self.peak_bonus);
        if let RecencyMode::Decay {
            strength,
            half_life_days,
        } = self.recency
        {
            fusion::apply_recency_decay(fused, Utc::now(), strength, half_life_days, |f: &Fact| {
                f.created_at
            });
        }
    }

    /// Fusion results → injection items + attribution, truncated to budget.
    fn finish(&self, fused: Vec<Fused<Fact>>, context_window: usize) -> Retrieved {
        let max_tokens = self.budget.tokens_for(context_window);

        // Abstention: if not a single entry qualifies, give back none at all.
        // Note this is **abstaining wholesale** rather than filtering entry by
        // entry — filtering entry by entry would let a single barely-qualifying
        // entry into the injection block on its own, and one weakly related memory
        // by itself misleads the model more easily than zero do.
        let fused: Vec<Fused<Fact>> = if fused
            .first()
            .is_none_or(|top| top.score < self.abstain_below)
        {
            Vec::new()
        } else {
            fused
                .into_iter()
                .filter(|f| f.score >= self.abstain_below)
                .collect()
        };

        let attribution: Vec<Attribution> = fused
            .iter()
            .map(|f| Attribution {
                fact_id: f.item.id.clone(),
                score: f.score,
                channels: f.hits.iter().map(|(c, _)| c.as_str()).collect(),
            })
            .collect();

        let items: Vec<MemoryItem> = fused
            .into_iter()
            .map(|f| MemoryItem {
                id: f.item.id,
                statement: f.item.statement,
                valid_at: f.item.valid_at.map(|t| t.to_rfc3339()),
                known_since: f.item.created_at.to_rfc3339(),
                source_episode_id: Some(f.item.source_episode_id),
                domain: f.item.domain,
                // The four below **do not enter the injection block**; they are for
                // the client's memory drawer (see the MemoryItem docs). They used
                // to be dropped right here, which is why every memory in the drawer
                // displayed "100%" — that number was a constant hard-coded in
                // cortexd, not this fact's score
                predicate: Some(f.item.predicate),
                confidence: Some(f.item.confidence),
                source_channel: Some(f.item.source_channel.as_str().to_string()),
                trust_tier: f.item.trust_tier,
            })
            .collect();

        let kept = crate::injection::fit_to_budget(items, max_tokens);
        // The attribution table has to line up with the truncated items, otherwise
        // the scores shown in the UI are offset against the wrong rows
        let attribution = attribution.into_iter().take(kept.len()).collect();

        Retrieved {
            items: kept,
            attribution,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_width_leaves_room_for_fusion() {
        let w = RecallWidth::default();
        // Each channel's width must be substantially larger than the final injected
        // count, otherwise RRF has no room to fuse: with three to five entries per
        // channel, the core signal of cross-channel consensus has no way to show
        // itself
        assert!(
            w.bm25 >= 20 && w.vector >= 20,
            "recall that is too narrow degenerates RRF"
        );
        assert!(
            w.graph_hops <= 3,
            "too many hops lets graph traversal eat the entire latency budget"
        );
    }

    #[test]
    fn abstains_when_nothing_is_relevant() {
        // The recency channel invariably returns the N most recent facts,
        // unrelated to the query. Without abstention, any question at all brings
        // back a pile of memories — a breeding ground for hallucination and memory
        // poisoning.
        let r = Retriever::new(crate::embed::HashEmbedder::new());
        // A single channel's rank-40 contribution is ≈ 1/(60+41) ≈ 0.0099, below
        // the threshold
        let weak = vec![Fused {
            item: dummy_fact(),
            score: 0.0099,
            hits: vec![(Channel::Recency, 40)],
        }];
        assert!(
            r.finish(weak, 128_000).items.is_empty(),
            "found only by the recency channel and ranked low: no memory should be injected at all"
        );
    }

    #[test]
    fn keeps_results_with_cross_channel_consensus() {
        let r = Retriever::new(crate::embed::HashEmbedder::new());
        // First place in two channels: 0.0164 × 2 ≈ 0.0328, plus `peak_bonus`'s
        // 0.02, gives 0.053 — the threshold of 0.030 was set against exactly this
        // distribution; see `abstain_below`
        let strong = vec![Fused {
            item: dummy_fact(),
            score: 0.0328 + 0.02,
            hits: vec![(Channel::Bm25, 0), (Channel::Vector, 0)],
        }];
        assert_eq!(
            r.finish(strong, 128_000).items.len(),
            1,
            "a strong hit with cross-channel consensus must be kept"
        );
    }

    /// Build a fact with a given id, for asserting on merge order.
    fn fact_with_id(id: &str) -> Fact {
        let mut f = dummy_fact();
        f.id = id.to_owned();
        f
    }

    #[test]
    fn merging_two_spaces_interleaves_by_rank() {
        // During a backfill there are more facts in the old space (the entire
        // existing corpus is over there) and only newly written ones in the current
        // space. Putting the current space wholesale in front pushes every
        // historical memory past position 41 — and since RRF's kernel is
        // 1/(k+rank), that is essentially the same as not recalling them at all
        let current = vec![
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA1"), 0.1),
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA2"), 0.2),
        ];
        let legacy = vec![
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB1"), 0.3),
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB2"), 0.4),
        ];
        let ids: Vec<String> = merge_vector_spaces(current, legacy)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                "01AAAAAAAAAAAAAAAAAAAAAAA1",
                "01BBBBBBBBBBBBBBBBBBBBBBB1",
                "01AAAAAAAAAAAAAAAAAAAAAAA2",
                "01BBBBBBBBBBBBBBBBBBBBBBB2",
            ],
            "the two spaces must interleave by rank, otherwise one side gets pushed wholesale to the back"
        );
    }

    #[test]
    fn merging_dedups_and_keeps_the_better_rank() {
        // The same fact hit on both sides: keep it once, at the position where it
        // **appeared first**. Keeping it twice means RRF credits it with two ranks
        // — a quietly amplified bias, not an error
        let shared = "01CCCCCCCCCCCCCCCCCCCCCCC1";
        let current = vec![
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA1"), 0.1),
            (fact_with_id(shared), 0.2),
        ];
        let legacy = vec![(fact_with_id(shared), 0.05)];
        let ids: Vec<String> = merge_vector_spaces(current, legacy)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            ids,
            vec!["01AAAAAAAAAAAAAAAAAAAAAAA1", shared],
            "a duplicated fact should appear only once, at the better of the two positions the two sides gave it"
        );
    }

    #[test]
    fn merging_without_legacy_is_the_identity() {
        // The overwhelming majority of deployments have never changed models — that
        // path must be bit-for-bit what it was before the change
        let current = vec![
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA1"), 0.1),
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA2"), 0.2),
        ];
        let ids: Vec<String> = merge_vector_spaces(current, Vec::new())
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            ids,
            vec!["01AAAAAAAAAAAAAAAAAAAAAAA1", "01AAAAAAAAAAAAAAAAAAAAAAA2"],
            "with no legacy model attached, the current space must be returned unchanged"
        );
    }

    #[test]
    fn merging_handles_uneven_lengths() {
        // When one side runs out first the other must keep going to exhaustion,
        // never stopping early — stopping early manifests as "only half the old
        // memories came back", and which half is arbitrary
        let current = vec![(fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA1"), 0.1)];
        let legacy = vec![
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB1"), 0.2),
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB2"), 0.3),
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB3"), 0.4),
        ];
        assert_eq!(
            merge_vector_spaces(current, legacy).len(),
            4,
            "no entries may be dropped when the two sides differ in length"
        );
    }

    fn dummy_fact() -> Fact {
        Fact {
            id: "01JZZZZZZZZZZZZZZZZZZZZZZ1".into(),
            subject_id: "01JZZZZZZZZZZZZZZZZZZZZZZ2".into(),
            predicate: "uses".into(),
            object_text: Some("x".into()),
            object_entity_id: None,
            statement: "测试事实".into(),
            embedding: None,
            embedding_model: "hash-stub-v1".into(),
            domain: None,
            confidence: 1.0,
            valid_at: None,
            source_episode_id: "01JZZZZZZZZZZZZZZZZZZZZZZ3".into(),
            source_channel: cortex_store::SourceChannel::Conversation,
            trust_tier: Some(2),
            extracted_by: "test".into(),
            device_id: "test".into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn budget_bounds_injection() {
        let b = Budget::default();
        assert!(b.tokens_for(128_000) <= b.absolute_max_tokens);
    }

    #[test]
    fn semantic_floor_only_applies_to_a_semantic_space() {
        // The hash stub's "distance" has nothing to do with relevance. Installing
        // 0.50 on it would **silently** zero out recall for offline development and
        // network-free CI — retrieval returns as usual, it just always returns empty
        assert_eq!(
            default_semantic_floor("hash-stub-v1"),
            None,
            "the hash stub is not a semantic space, so a floor calibrated on a vector model must not be applied to it"
        );
        assert_eq!(
            default_semantic_floor("fastembed:gpahal/bge-m3-onnx-int8"),
            Some(BGE_M3_SEMANTIC_FLOOR)
        );

        let r = Retriever::new(crate::embed::HashEmbedder::new());
        assert_eq!(
            r.semantic_floor(),
            None,
            "this should be decided from the backend at construction time, not patched up afterwards"
        );
    }

    #[test]
    fn chronological_replay_is_not_gated_by_the_abstain_threshold() {
        // A full-picture replay has no second channel to fuse with, so keeping RRF's
        // 1/(60+i+1) would leave even the top entry at 0.0164 and the abstention
        // threshold of 0.017 would swallow the whole thing — the user explicitly
        // asked for a replay and got back nothing
        let r = Retriever::new(crate::embed::HashEmbedder::new());
        let fused = chronological(vec![dummy_fact()]);
        assert_eq!(
            r.finish(fused, 128_000).items.len(),
            1,
            "an explicit time replay must not be blocked by a threshold calibrated for four-channel recall"
        );
    }

    #[test]
    fn recency_is_off_by_default() {
        // This guards against someone casually switching it back to Channel: the
        // decision rests on eval numbers (0 exclusive questions, own R@5 of 0.101,
        // R@1 dragged down by 0.088, injection up 27%), and switching it back means
        // re-running the evals along with it — see the table on `RecencyMode`
        let r = Retriever::new(crate::embed::HashEmbedder::new());
        assert_eq!(r.recency, RecencyMode::Off);
        assert!(
            !r.episode_channel,
            "the episodes channel measures neutral-to-negative on ranking, so it is off by default; overturning that means re-running the 2×2 as well"
        );
    }
}

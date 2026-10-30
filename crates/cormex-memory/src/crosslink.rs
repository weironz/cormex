//! Cross-domain linking — using `relates_to` edges to connect the two memory
//! islands of "coding" and "office".
//!
//! ```text
//! Nightly scan
//!     │
//!     ├─① candidates  vector neighbours → cross-domain → not yet connected → distance inside the band   ← cheap, governs recall
//!     ├─② judgement   a cheap model decides pair by pair whether the link is worth it                   ← expensive, governs precision, outside the transaction
//!     ├─③ plan        entity pair → two directed relates_to NewFacts                                    ← pure logic
//!     └─④ persist     written by a single write_txn                                                     ← the only step inside a transaction
//! ```
//!
//! # Why none of this requires touching the retrieval code
//!
//! `recall_graph` walks the adjacency relation in `facts`
//! (`subject_id → object_entity_id`), and `facts.predicate` is deliberately not
//! enumerated in the schema. So "write one more edge" **is by itself** "make graph
//! traversal pull in one more subgraph" — not a single line changes on the
//! retrieval side. This is the prettiest thing about this schema design, and the
//! property this module exists to preserve.
//!
//! The one exception is on the fusion side: what `relates_to` pulls in is
//! **cross-domain by definition**, and [`crate::fusion::apply_domain_boost`]
//! boosts facts in the current domain, which is equivalent to penalizing
//! cross-domain ones. Building a bridge and immediately pushing it back down is
//! doing the work for nothing, so an exemption is needed over there (for the
//! trade-offs in its scope, see the comments on `bridged_entities` in
//! [`crate::retrieval`]).
//!
//! # Three rules
//!
//! 1. **LLM calls and embeddings are always computed outside the transaction**
//!    (a corollary of constraint 2 in CLAUDE.md). This module splits "decide what
//!    to write" from "write" into [`Linker::plan`] and [`Linker::write`], the same
//!    shape as `extract` / `write_candidates` in [`crate::extract`].
//! 2. **A unique constraint cannot be added** (the schema is outside this module's
//!    scope of change), so idempotency comes from checking before writing: see
//!    [`LinkCorpus::out_edges`].
//! 3. **Noisy edges are contagious under graph traversal** — one wrong edge causes
//!    an entire unrelated subgraph to be recalled. Hence the degree cap, the batch
//!    cap, and a default judge that is not "link it if the vectors are close
//!    enough".

use std::collections::{HashMap, HashSet};

use cormex_core::{CormexError, Id, Result};
use cormex_llm::{LlmClient, Message};
use cormex_store::{Fact, FactSource, NewFact, ProvenanceRef, RedactionTarget, Store, Vector};
use serde_json::Value;

use crate::embed::Embedder;
use crate::tokenize;

/// The predicate used for cross-domain links.
///
/// It is deliberately absent from the functional set that
/// [`crate::extract::PredicateRules`] is built with by default, which makes it a
/// **multi-valued** predicate: one entity can have several edges out without them
/// superseding each other. That is what we want — one module may perfectly well
/// correspond to both a decision from a meeting and a budget constraint at the
/// same time.
pub const RELATES_TO: &str = "relates_to";

/// The value written into `facts.confidence`.
///
/// # Why 0.30, and why it currently **does nothing**
///
/// The roadmap says only "low confidence". The basis for settling on 0.30 is that
/// it must sit clearly below what the extractor produces: the parser's default is
/// 0.80, the dataset's hand-written values are 0.90, and nothing below 0.50 has
/// been seen in practice. That way, any future gate along the lines of "only write
/// facts with confidence ≥ 0.5 into the prompt as assertions" will naturally keep
/// these inferred edges out of the assertions, without stopping graph traversal
/// from walking them.
///
/// **But the current state has to be stated plainly**: grep the whole repository
/// for `confidence` and neither the retrieval side (`fusion` / `retrieval`) nor
/// the injection side (`injection`) reads it anywhere; only the DTOs and the sync
/// payload pass it through untouched. Which means that writing 0.30 versus 0.99
/// today makes **no difference whatsoever** to recall or ranking. This number is
/// currently for human readers and for the future, not a gate that is in force —
/// anyone wanting to treat it as a security boundary has to go build the gate
/// first.
pub const DEFAULT_LINK_CONFIDENCE: f32 = 0.30;

/// The upper bound on a candidate's cosine distance. Beyond it, treat the pair as
/// unrelated.
///
/// The reference point is [`crate::retrieval::BGE_M3_SEMANTIC_FLOOR`] (0.50),
/// which is the criterion for "even the nearest fact in the whole database is out
/// of reach ⇒ the answer to this question is not in memory". Deciding that two
/// **facts** are worth linking ought to be stricter than "a query can just barely
/// reach an answer", so this value stays inside it. 0.45 is a starting point, not
/// a settled conclusion — changing the embedding model requires recalibrating it.
///
/// Under the hash stub (`hash-stub-v1`) this band picks up almost nothing (1 pair
/// out of a 69-entry corpus), and that is **the right way to fail**: its
/// "distance" reflects only character-bigram overlap rather than meaning, and
/// rather than linking a pile of edges by glyph shape in offline CI, it is better
/// to link nothing at all.
pub const DEFAULT_MAX_DISTANCE: f64 = 0.45;

/// The lower bound on a candidate's cosine distance. Anything closer than this is
/// **not** linked.
///
/// # Semantically close ≠ worth linking
///
/// The distance between 「Cormex 用 Rust 写」 and 「Cormex 用 Rust 写」 approaches
/// 0, and linking them produces not one bit of information while costing an extra
/// edge, an extra slice of injection budget, and an extra branch for graph
/// traversal. What actually has value is **one topic in two vocabularies**: "the
/// plan we settled on at last week's meeting" against "that module in the code" —
/// a relationship that has some distance in vector space without being far. So a
/// candidate must fall inside a **band** rather than merely under a ceiling.
///
/// The lower bound also blocks a real class of noise along the way: the same thing
/// extracted once in each of two domains (meeting notes and a code comment
/// restating one decision). That is **duplication that should be merged**, not a
/// relationship that needs a bridge; it belongs to entity resolution and conflict
/// resolution, and should not be papered over with an edge here.
pub const DEFAULT_MIN_DISTANCE: f64 = 0.12;

/// How many `relates_to` edges a single entity may have out at most.
///
/// # This is the main gate for noise control
///
/// The cost structure of graph traversal makes the degree cap more important than
/// the distance threshold: one wrong edge merely drags in one extra fact, but a
/// **hub entity** (connected by 20 edges) drags twenty unrelated subgraphs along
/// on any query that mentions it — noise is contagious in a graph. The distance
/// threshold cannot govern this, because each individual edge looks "close
/// enough" on its own.
///
/// The basis for 4 is scale rather than a curve: graph traversal defaults to 2
/// hops, and a hub of degree d can pull in O(d²) subgraphs within two hops. At
/// d=4 that is 16, the same order of magnitude as `RecallWidth::graph` (30
/// entries); at d=10 it is 100, which eats the entire channel's recall outright.
pub const DEFAULT_DEGREE_CAP: usize = 4;

/// How many vector neighbours to take per fact under examination.
///
/// The same width as `RecallWidth::vector`. The overwhelming majority of
/// neighbours are same-domain (same-domain corpus is simply denser), leaving only
/// a few cross-domain ones in the tail — take it too narrow and this whole channel
/// goes silent.
pub const DEFAULT_NEIGHBOR_WIDTH: i64 = 40;

// ══════════════════════════════════════════════════════════
//  Corpus source
// ══════════════════════════════════════════════════════════

/// What the scan needs to read.
///
/// This is a trait rather than taking `&Store` directly, for the same reason as
/// [`crate::extract::EntityIndex`]: `cormex-store` currently has no method for
/// "walk all facts paginated by cursor", and adding methods to the storage layer
/// is outside this module's scope of change. The default implementation,
/// [`StoreCorpus`], is assembled from the three existing public queries, which is
/// enough to run; once the storage layer grows a `facts_since(cursor)`, swapping
/// in another implementation is all it takes and `Linker` doesn't change by a
/// line.
///
/// **A known limit**: [`StoreCorpus::scan_batch`] is clamped by the storage
/// layer's `MAX_LIMIT` (200), so it cannot cover a larger increment in one pass.
/// If the nightly scan runs daily, 200 entries per day is sufficient — but this is
/// a real ceiling, not a detail that can be waved away.
#[async_trait::async_trait]
pub trait LinkCorpus: Send + Sync {
    /// The facts to examine as the "left" side this round.
    async fn scan_batch(&self) -> Result<Vec<Fact>>;

    /// Vector neighbours, ascending by cosine **distance** (not similarity).
    async fn neighbors(
        &self,
        embedding: &Vector,
        model: &str,
        limit: i64,
    ) -> Result<Vec<(Fact, f64)>>;

    /// Every entity this entity already links out to (any predicate, considering
    /// only **valid** facts).
    ///
    /// One method delivers two things at once:
    ///
    /// 1. **Idempotency** — on the second scan, the edges written by the first one
    ///    are already in here, so they don't get written again. Given append-only
    ///    storage and no way to add a unique constraint, checking before writing is
    ///    the only deduplication mechanism available.
    /// 2. **Don't do pointless work** — if two facts already share an entity or are
    ///    already directly connected, graph traversal can walk from one to the
    ///    other as it is, and adding another `relates_to` is just noise. The real
    ///    gap lies only in the pairs that are semantically related but unreachable
    ///    in the graph.
    async fn out_edges(&self, subject_id: &str) -> Result<Vec<String>>;

    /// An entity's display name. Both the `statement` and the judgement prompt need
    /// it.
    async fn entity_name(&self, entity_id: &str) -> Result<Option<String>>;
}

/// The default implementation, assembled from `Store`'s existing public queries.
pub struct StoreCorpus<'a> {
    store: &'a Store,
    /// How many days back `scan_batch` looks
    within_days: i64,
    /// How many entries `scan_batch` takes at most (the storage layer clamps this
    /// further, to 200)
    limit: i64,
}

impl<'a> StoreCorpus<'a> {
    #[must_use]
    pub fn new(store: &'a Store) -> Self {
        Self {
            store,
            within_days: 1,
            limit: 200,
        }
    }

    /// The lookback window. The nightly scan runs over "what was written
    /// yesterday"; it can be widened when rebuilding the index.
    #[must_use]
    pub fn with_window(mut self, within_days: i64, limit: i64) -> Self {
        self.within_days = within_days;
        self.limit = limit;
        self
    }
}

#[async_trait::async_trait]
impl LinkCorpus for StoreCorpus<'_> {
    async fn scan_batch(&self) -> Result<Vec<Fact>> {
        self.store
            .recall_recent(self.within_days, self.limit)
            .await
            .map_err(|e| CormexError::Store(e.to_string()))
    }

    async fn neighbors(
        &self,
        embedding: &Vector,
        model: &str,
        limit: i64,
    ) -> Result<Vec<(Fact, f64)>> {
        self.store
            .recall_vector_scored(embedding, model, limit)
            .await
            .map_err(|e| CormexError::Store(e.to_string()))
    }

    async fn out_edges(&self, subject_id: &str) -> Result<Vec<String>> {
        let facts = self
            .store
            .active_facts_by_subject(subject_id, 200)
            .await
            .map_err(|e| CormexError::Store(e.to_string()))?;
        Ok(facts
            .into_iter()
            .filter_map(|f| f.object_entity_id)
            .collect())
    }

    async fn entity_name(&self, entity_id: &str) -> Result<Option<String>> {
        Ok(self
            .store
            .entity(entity_id)
            .await
            .map_err(|e| CormexError::Store(e.to_string()))?
            .map(|e| e.name))
    }
}

// ══════════════════════════════════════════════════════════
//  Candidates and judgement
// ══════════════════════════════════════════════════════════

/// The anchor on one side of a candidate: this fact, and which entity it hangs
/// off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub fact_id: String,
    /// The anchor entity = that fact's `subject_id`.
    ///
    /// The subject rather than the object: a `statement` is organized around its
    /// subject, and the object is frequently a literal (with no entity id at all).
    /// The consequence of anchoring in the wrong place is not "recall too little"
    /// but "recall a pile of unrelated things", so it is better to anchor at the
    /// most stable position available.
    pub entity_id: String,
    pub entity_name: String,
    pub statement: String,
    pub domain: String,
    pub source_episode_id: String,
}

/// One pair that passed the pre-filter.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub left: Anchor,
    pub right: Anchor,
    /// The cosine distance between the two facts' statement vectors
    pub distance: f64,
}

impl Candidate {
    /// The unordered canonical key for an entity pair — both deduplication and
    /// degree counting go by it.
    #[must_use]
    pub fn pair_key(&self) -> (String, String) {
        pair_key(&self.left.entity_id, &self.right.entity_id)
    }
}

fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_owned(), b.to_owned())
    } else {
        (b.to_owned(), a.to_owned())
    }
}

/// The result of a judgement.
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    pub candidate: Candidate,
    /// Why the link is worth making. It gets written into the `statement` and is
    /// the **only** auditable explanation this edge has — without it, a user
    /// looking at a `relates_to` can do nothing but guess why it exists.
    pub reason: String,
}

/// The judge deciding "is this pair worth linking".
///
/// # Why it has to be swappable
///
/// Pure vector distance produces a great many noisy edges, and noisy edges are
/// contagious under graph traversal; LLM judgement is high quality but costs money
/// per pair. **Where to spend the money** is the single most important trade-off
/// in this module, which is why it is an explicit interface rather than hard-coded
/// logic:
///
/// | Implementation | Cost | Purpose |
/// |---|---|---|
/// | [`AcceptAll`] | 0 | The control group measuring what the pre-filter achieves on its own; deterministic, safe for CI |
/// | [`CheapModelJudge`] | One cheap-model call per batch | Production |
///
/// Note that [`AcceptAll`] is **not** a synonym for "turn judgement off" — it is a
/// meaningful control group: the pre-filter has already applied three filters
/// (cross-domain, not yet connected, inside the distance band), and AcceptAll
/// measures exactly what those three filters achieve in precision by themselves.
/// Using it as the baseline in the evals is the only way to know what model
/// judgement actually buys.
#[async_trait::async_trait]
pub trait LinkJudge: Send + Sync {
    /// Judge a batch of candidates. The return value contains only the **accepted**
    /// ones, and need not preserve the input order.
    async fn judge(&self, candidates: &[Candidate]) -> Result<Vec<Accepted>>;
}

/// Accept everything — the pre-filter's control group; deterministic and free.
#[derive(Debug, Clone, Copy, Default)]
pub struct AcceptAll;

#[async_trait::async_trait]
impl LinkJudge for AcceptAll {
    async fn judge(&self, candidates: &[Candidate]) -> Result<Vec<Accepted>> {
        Ok(candidates
            .iter()
            .map(|c| Accepted {
                candidate: c.clone(),
                reason: "向量近邻预筛".to_owned(),
            })
            .collect())
    }
}

const JUDGE_SYSTEM: &str = r#"你在为一个记忆系统判定「两条来自不同领域的记忆，值不值得建立一条关联」。

# 判据只有一条

> 用户下次在聊 A 的时候，被顺带提醒 B，是**帮忙**还是**打扰**？

是帮忙才连。**宁可漏连也不要错连** —— 这条关联会让检索把 B 那一侧的
整片记忆一起带出来，连错一条，之后每次聊到 A 都会多出一堆无关内容。

# 明确不连

- 只是话题相邻、词面相近，但一方对另一方没有任何实际影响
- 两条说的其实是同一件事（重复陈述，该去重不该连）
- 只靠「都提到了同一类东西」（都是人、都是工具、都是日期）建立的联想
- 泛泛的上下位关系（「都属于这个项目」）

# 该连

一方**约束**、**解释**、**触发**或**落实**另一方。典型：
会上定的方案 ↔ 代码里的实现；预算上限 ↔ 选型决定；
个人偏好 ↔ 它影响的工程做法；人员分工 ↔ 他负责的那个模块。

# 输出格式

只输出 JSON，不要任何解释文字：

{"links": [{"id": 候选编号, "reason": "不超过 30 字，说清这两者是怎么互相影响的"}]}

一对都不该连时输出 {"links": []}。reason 写不出具体影响的，就是不该连。"#;

/// Judge using the cheap model. **Never the main conversation model** — this is
/// something that runs every single night.
pub struct CheapModelJudge {
    llm: LlmClient,
    /// How many pairs go into one request. Batching is the only cost-saving lever
    /// here: the system prompt is far longer than a single candidate pair, so
    /// one request per pair means paying for it over again every time.
    batch: usize,
}

impl CheapModelJudge {
    #[must_use]
    pub fn new(llm: LlmClient) -> Self {
        Self { llm, batch: 12 }
    }

    #[must_use]
    pub fn with_batch(mut self, batch: usize) -> Self {
        self.batch = batch.max(1);
        self
    }
}

impl std::fmt::Debug for CheapModelJudge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheapModelJudge")
            .field("batch", &self.batch)
            .finish_non_exhaustive()
    }
}

/// The length limit on a judgement's reason.
///
/// Not merely cosmetic: `reason` goes into the `statement` verbatim, and the
/// `statement` goes into the prompt. Letting the model write as much as it likes
/// amounts to opening an unbounded hole in the injection budget.
const MAX_REASON_CHARS: usize = 40;

#[async_trait::async_trait]
impl LinkJudge for CheapModelJudge {
    async fn judge(&self, candidates: &[Candidate]) -> Result<Vec<Accepted>> {
        let mut out = Vec::new();
        for chunk in candidates.chunks(self.batch) {
            let prompt = build_judge_prompt(chunk);
            let messages = [Message::user().with_text(prompt)];
            let (reply, _usage) = self
                .llm
                .complete(self.llm.cheap_model(), JUDGE_SYSTEM, &messages, &[])
                .await?;
            out.extend(parse_judgements(&reply.as_concat_text(), chunk));
        }
        Ok(out)
    }
}

fn build_judge_prompt(chunk: &[Candidate]) -> String {
    let mut s = String::with_capacity(chunk.len() * 256 + 128);
    s.push_str("# 待判定的候选\n\n");
    for (i, c) in chunk.iter().enumerate() {
        s.push_str(&format!(
            "## 候选 {i}\n- A（{}）：{}\n- B（{}）：{}\n\n",
            c.left.domain, c.left.statement, c.right.domain, c.right.statement
        ));
    }
    s.push_str("只输出 JSON。");
    s
}

/// Parse the judgement results. **Fault-tolerant per entry**: an out-of-range
/// index drops only that entry.
///
/// Unlike [`crate::extract::parse_candidates`], a parse failure here does not
/// raise an error but returns empty — the correct behaviour on a failed judgement
/// is "link nothing in this batch", not turning the entire nightly scan red. One
/// missing edge is a little less recall; a crashed scan is something nobody
/// notices until the following night.
fn parse_judgements(raw: &str, chunk: &[Candidate]) -> Vec<Accepted> {
    let Some(blob) = crate::extract::extract_json_blob(raw) else {
        tracing::warn!("no JSON in the cross-domain judgement; linking nothing in this batch");
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(blob) else {
        tracing::warn!("the cross-domain judgement is not valid JSON; linking nothing");
        return Vec::new();
    };

    let items = match &value {
        Value::Array(items) => items.as_slice(),
        Value::Object(map) => map
            .get("links")
            .or_else(|| map.get("results"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let Some(idx) = item.get("id").and_then(Value::as_u64).map(|i| i as usize) else {
            continue;
        };
        let Some(candidate) = chunk.get(idx) else {
            tracing::debug!(
                idx,
                "the judgement references a candidate index that does not exist"
            );
            continue;
        };
        if !seen.insert(idx) {
            continue;
        }
        let reason: String = item
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("模型判定相关")
            .chars()
            .take(MAX_REASON_CHARS)
            .collect();
        out.push(Accepted {
            candidate: candidate.clone(),
            reason,
        });
    }
    out
}

// ══════════════════════════════════════════════════════════
//  The scanner
// ══════════════════════════════════════════════════════════

/// One planned edge. Two **directed** `NewFact`s together constitute one
/// undirected relationship; the reasoning is in [`Linker::plan`].
#[derive(Debug, Clone)]
pub struct PlannedLink {
    pub forward: NewFact,
    pub backward: NewFact,
    pub reason: String,
    pub distance: f64,
}

/// The result of one scan.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkReport {
    /// How many facts were examined as the left side
    pub scanned: usize,
    /// How many candidate entity pairs passed the pre-filter
    pub candidates: usize,
    /// How many pairs the judge accepted
    pub accepted: usize,
    /// How many pairs were dropped by the degree cap — the moment this number is
    /// non-zero, a hub entity has appeared in the corpus, and it is worth a human
    /// glancing at whether extraction has turned something into a
    /// catch-all subject
    pub degree_capped: usize,
    /// The fact ids actually written (two per pair)
    pub written: Vec<Id>,
}

/// The cross-domain link scanner.
#[derive(Debug, Clone)]
pub struct Linker {
    min_distance: f64,
    max_distance: f64,
    degree_cap: usize,
    neighbor_width: i64,
    confidence: f32,
    /// How many pairs one scan may write at most. Prevents one anomalous corpus
    /// from flooding the database with noisy edges
    batch_cap: usize,
    /// Whether an edge's statement enters the `tsv` and `embedding` indexes.
    /// **It does not by default.**
    ///
    /// # A counterintuitive decision the evals forced
    ///
    /// The first version indexed them as a matter of course, and overall R@5 fell
    /// from 0.923 to 0.907 and MRR from 0.817 to 0.799 — and what fell were
    /// **questions with nothing whatsoever to do with cross-domain linking**.
    /// Going through them question by question makes the mechanism obvious: an
    /// edge's statement has the form 「「A」（domain）与「B」（domain）相关：reason」,
    /// packing two entity names and a one-line summary into a single sentence —
    /// which is precisely the shape BM25's `ts_rank` likes best (high term
    /// frequency, short document). So for any query mentioning A or B, the top of
    /// the list is this edge that **says nothing extra at all**, and the entry that
    /// actually answers the question gets pushed down.
    /// `q072「v1 发布这件事上都定了什么」` was the worst: four of its top five were
    /// edges.
    ///
    /// The fix is not to adjust weights but to be clear about **what this row
    /// is**: it is an **edge**, not a statement about the world. Its purpose is to
    /// be **traversed**, not to be **retrieved**. Leaving both `tsv` and
    /// `embedding` NULL means it can only ever surface via graph traversal — that
    /// is, only when the query genuinely hit one end of the bridge, which is
    /// exactly the semantics we want.
    ///
    /// It is kept as a switch because this conclusion depends on corpus size (69
    /// facts and 12 edge rows, 11% of the total); once the corpus is in the
    /// thousands the crowding effect will dilute, and it is worth re-measuring.
    indexed: bool,
    device_id: String,
}

impl Linker {
    #[must_use]
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            min_distance: DEFAULT_MIN_DISTANCE,
            max_distance: DEFAULT_MAX_DISTANCE,
            degree_cap: DEFAULT_DEGREE_CAP,
            neighbor_width: DEFAULT_NEIGHBOR_WIDTH,
            confidence: DEFAULT_LINK_CONFIDENCE,
            batch_cap: 64,
            indexed: false,
            device_id: device_id.into(),
        }
    }

    /// Let edge statements enter the `tsv` / `embedding` indexes too. See the
    /// comment on [`Self::indexed`] and the numbers there — off by default, and
    /// re-run the evals before turning it on.
    #[must_use]
    pub fn with_indexed(mut self, indexed: bool) -> Self {
        self.indexed = indexed;
        self
    }

    /// The candidate distance band. For eval parameter sweeps.
    #[must_use]
    pub fn with_distance_band(mut self, min: f64, max: f64) -> Self {
        self.min_distance = min;
        self.max_distance = max;
        self
    }

    #[must_use]
    pub fn with_degree_cap(mut self, cap: usize) -> Self {
        self.degree_cap = cap;
        self
    }

    #[must_use]
    pub fn with_batch_cap(mut self, cap: usize) -> Self {
        self.batch_cap = cap;
        self
    }

    #[must_use]
    pub fn with_neighbor_width(mut self, width: i64) -> Self {
        self.neighbor_width = width;
        self
    }

    // ── ① Candidates ───────────────────────────────────────

    /// Pre-filter cross-domain candidate pairs.
    ///
    /// # Why not compare all pairs exhaustively
    ///
    /// O(n²) stops being runnable at a few thousand facts, and the overwhelming
    /// majority of pairs have nothing to do with each other. Here, one vector
    /// nearest-neighbour query is issued per fact under examination (HNSW,
    /// O(log n)), and the cross-domain ones are filtered out of those neighbours —
    /// total cost O(n·k·log n). The price is that **it misses things**: the
    /// same-domain corpus is denser, so cross-domain neighbours may be crowded out
    /// of the top k. That direction is a deliberate choice: a missed edge is
    /// merely a little less recall, whereas an extra wrong edge is contagious in
    /// the graph.
    ///
    /// Four filters, each answering a different question:
    ///
    /// | Filter | What it blocks |
    /// |---|---|
    /// | Same `embedding_model` | Distances across models are not comparable (memory.md §7) |
    /// | Both sides have a domain, and they differ | Being cross-domain is this edge's **entire** justification |
    /// | Distance falls inside the `[min, max]` band | Too far is noise; too close is a restatement, and linking it produces no information |
    /// | Not yet connected in the graph | For pairs already reachable, another edge is just noise (see [`LinkCorpus::out_edges`]) |
    pub async fn propose(&self, corpus: &dyn LinkCorpus) -> Result<(Vec<Candidate>, usize)> {
        let batch = corpus.scan_batch().await?;
        let scanned = batch.len();

        let mut names: HashMap<String, String> = HashMap::new();
        let mut edges: HashMap<String, HashSet<String>> = HashMap::new();
        let mut best: HashMap<(String, String), Candidate> = HashMap::new();

        for left in &batch {
            let Some(embedding) = left.embedding.as_ref() else {
                continue;
            };
            let Some(left_domain) = left.domain.clone() else {
                continue;
            };

            let neighbors = corpus
                .neighbors(embedding, &left.embedding_model, self.neighbor_width)
                .await?;

            for (right, distance) in neighbors {
                if right.id == left.id
                    || right.embedding_model != left.embedding_model
                    || !(self.min_distance..=self.max_distance).contains(&distance)
                {
                    continue;
                }
                let Some(right_domain) = right.domain.clone() else {
                    continue;
                };
                if right_domain == left_domain || left.subject_id == right.subject_id {
                    continue;
                }
                // Two facts sharing any entity ⇒ graph traversal already gets
                // through, so this edge is not needed
                if shares_entity(left, &right) {
                    continue;
                }
                if self
                    .already_connected(corpus, &mut edges, &left.subject_id, &right.subject_id)
                    .await?
                {
                    continue;
                }

                let key = pair_key(&left.subject_id, &right.subject_id);
                // The same entity pair may be hit by several sets of facts; keep
                // only the closest set as the evidence
                if best.get(&key).is_some_and(|c| c.distance <= distance) {
                    continue;
                }

                let left_name = resolve_name(corpus, &mut names, &left.subject_id).await?;
                let right_name = resolve_name(corpus, &mut names, &right.subject_id).await?;
                let (Some(left_name), Some(right_name)) = (left_name, right_name) else {
                    // The entity row cannot be found ⇒ this fact's anchor is
                    // broken, and skipping it is preferable
                    continue;
                };

                best.insert(
                    key,
                    Candidate {
                        left: anchor(left, left_name, left_domain.clone()),
                        right: anchor(&right, right_name, right_domain),
                        distance,
                    },
                );
            }
        }

        let mut out: Vec<Candidate> = best.into_values().collect();
        // A stable order: ascending by distance, and lexicographic by entity pair
        // on ties. The scan has to be reproducible, and HashMap iteration order is
        // not
        out.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.pair_key().cmp(&b.pair_key()))
        });
        Ok((out, scanned))
    }

    async fn already_connected(
        &self,
        corpus: &dyn LinkCorpus,
        cache: &mut HashMap<String, HashSet<String>>,
        a: &str,
        b: &str,
    ) -> Result<bool> {
        for (from, to) in [(a, b), (b, a)] {
            if !cache.contains_key(from) {
                let out = corpus.out_edges(from).await?;
                cache.insert(from.to_owned(), out.into_iter().collect());
            }
            if cache[from].contains(to) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── ③ Plan ─────────────────────────────────────────────

    /// Turn accepted candidates into `NewFact`s ready to write. **Pure logic plus
    /// vectorization; still outside the transaction.**
    ///
    /// # Why one relationship requires writing two rows
    ///
    /// `recall_graph`'s entity expansion is **directed**:
    /// `SELECT COALESCE(f.object_entity_id, …) FROM reach r JOIN active_facts f
    /// ON f.subject_id = r.entity_id`. That is, you can walk from subject to
    /// object but not the other way (going backwards collects only the edge
    /// itself, never the other entity's other facts).
    ///
    /// But "related to" is inherently undirected, and the demo scenario needs both
    /// directions to hold: asking about office matters in a coding context, and the
    /// reverse. Either `recall_graph` is changed to expand bidirectionally (that is
    /// the storage layer, and it would double the width of **every** traversal), or
    /// the undirected relationship is materialized as two directed edges. The
    /// latter was chosen: the cost is confined to this one predicate, and it is
    /// precisely what makes "no retrieval code needs to change" actually true.
    ///
    /// The price is honest: each relationship occupies two rows and two slices of
    /// injection budget. This is why degree_cap counts **pairs** rather than rows.
    pub async fn plan(
        &self,
        embedder: &dyn Embedder,
        accepted: Vec<Accepted>,
        existing_degree: &HashMap<String, usize>,
    ) -> Result<(Vec<PlannedLink>, usize)> {
        let mut degree: HashMap<String, usize> = existing_degree.clone();
        let mut kept: Vec<Accepted> = Vec::new();
        let mut capped = 0usize;

        // Already ascending by distance: when the degree cap is hit, what survives
        // is the closer pairs
        for a in accepted {
            if kept.len() >= self.batch_cap {
                capped += 1;
                continue;
            }
            let l = a.candidate.left.entity_id.clone();
            let r = a.candidate.right.entity_id.clone();
            let over = degree.get(&l).copied().unwrap_or(0) >= self.degree_cap
                || degree.get(&r).copied().unwrap_or(0) >= self.degree_cap;
            if over {
                capped += 1;
                continue;
            }
            *degree.entry(l).or_insert(0) += 1;
            *degree.entry(r).or_insert(0) += 1;
            kept.push(a);
        }

        if kept.is_empty() {
            return Ok((Vec::new(), capped));
        }

        let statements: Vec<String> = kept
            .iter()
            .flat_map(|a| {
                [
                    link_statement(&a.candidate.left, &a.candidate.right, &a.reason),
                    link_statement(&a.candidate.right, &a.candidate.left, &a.reason),
                ]
            })
            .collect();

        // When not indexing there is no need to compute them at all — computing
        // vectors for nothing is pure waste, and this runs every single night. And
        // if they are computed, it has to finish here: a network round trip inside
        // the transaction is a capital offence
        let mut vectors: Vec<Option<Vec<f32>>> = if self.indexed {
            let v = embedder.embed(&statements).await?;
            if v.len() != statements.len() {
                return Err(CormexError::Memory(format!(
                    "the embedder returned {} vectors, which does not match {} statements",
                    v.len(),
                    statements.len()
                )));
            }
            v.into_iter().map(Some).collect()
        } else {
            vec![None; statements.len()]
        };

        let model = embedder.model_id().to_owned();
        let mut out = Vec::with_capacity(kept.len());
        let mut texts = statements.into_iter();
        let mut vecs = vectors.drain(..);

        for a in kept {
            let c = &a.candidate;
            let forward = self.new_fact(
                &c.left,
                &c.right,
                texts
                    .next()
                    .expect("statements correspond one-to-one with candidates"),
                vecs.next()
                    .expect("vector slots correspond one-to-one with statements"),
                &model,
            )?;
            let backward = self.new_fact(
                &c.right,
                &c.left,
                texts
                    .next()
                    .expect("statements correspond one-to-one with candidates"),
                vecs.next()
                    .expect("vector slots correspond one-to-one with statements"),
                &model,
            )?;
            out.push(PlannedLink {
                forward,
                backward,
                reason: a.reason,
                distance: c.distance,
            });
        }
        Ok((out, capped))
    }

    fn new_fact(
        &self,
        from: &Anchor,
        to: &Anchor,
        statement: String,
        vector: Option<Vec<f32>>,
        model: &str,
    ) -> Result<NewFact> {
        let parse = |s: &str, what: &str| -> Result<Id> {
            s.parse::<Id>()
                .map_err(|e| CormexError::Store(format!("{what} {s} is not a valid ULID: {e}")))
        };
        Ok(NewFact {
            id: Id::new(),
            subject_id: parse(&from.entity_id, "entity id")?,
            predicate: RELATES_TO.to_owned(),
            object_text: None,
            object_entity_id: Some(parse(&to.entity_id, "entity id")?),
            // See `Linker::indexed`: neither index by default — this row is an
            // **edge**, meant to be traversed rather than retrieved. Leaving `tsv`
            // NULL is a **decision**, not an omission, and does not conflict with
            // the "never backfill tsv asynchronously" rule (that one forbids an
            // after-the-fact UPDATE)
            tsv_source: self
                .indexed
                .then(|| tokenize::to_tsvector_input(&statement)),
            statement,
            embedding: vector.map(Vector::from),
            embedding_model: model.to_owned(),
            // Deliberately left empty. This edge belongs to no single domain —
            // giving it one would make `apply_domain_boost` favour one direction
            // over the other, when the entire reason it exists is that "both
            // domains count"
            domain: None,
            confidence: self.confidence,
            // Event time is unknown: there is no instant at which "these two
            // started being related". Leaving it NULL is the honest choice; filling
            // in now() would pass system time off as event time, and the whole
            // bitemporal design would have been for nothing
            valid_at: None,
            // Provenance points at the original conversation behind the fact on
            // **this direction's left side**. It does not itself state this
            // relationship, but it is the only anchor that genuinely exists, and it
            // lets a redact cascade take this edge along with it.
            source_episode_id: parse(&from.source_episode_id, "episode id")?,
            // This edge was inferred by a model from two **existing facts**, not
            // read out of a conversation
            source: FactSource::Derived,
            // Record the source facts on both ends.
            //
            // This closes the hole left by `source_episode_id` recording only the
            // left side: previously, redacting only the right side would leave this
            // row alive carrying the right-hand entity's name, with no query able to
            // find it. Now, looking up (fact, right-hand id) through derivations
            // locates this edge (source fact → its source_episode_id is the second
            // hop).
            //
            // ⚠️ Still unsolved is "trust laundering": this edge is fixed at tier 2
            // and does not inherit the tier of the source facts on either end. The
            // prerequisite for fixing that is exactly these two rows (see migration
            // 20260807000006)
            derived_from: vec![
                ProvenanceRef::fact(parse(&from.fact_id, "fact id")?),
                ProvenanceRef::fact(parse(&to.fact_id, "fact id")?),
            ],
            extracted_by: format!("crosslink/{RELATES_TO}"),
            device_id: self.device_id.clone(),
        })
    }

    // ── ④ Persist ──────────────────────────────────────────

    /// The only step that enters a transaction. The transaction contains nothing
    /// but INSERTs — no reads and no network calls of any kind.
    pub async fn write(&self, store: &Store, planned: Vec<PlannedLink>) -> Result<Vec<Id>> {
        if planned.is_empty() {
            return Ok(Vec::new());
        }

        // In-flight task guard, the same as in `extract::write_candidates`:
        // somebody may have run a redact after the scan started, and without this
        // check the edges derived from the erased content would get written back in
        let mut rows: Vec<&NewFact> = Vec::with_capacity(planned.len() * 2);
        for p in &planned {
            for f in [&p.forward, &p.backward] {
                if store
                    .is_redacted(RedactionTarget::Episode, &f.source_episode_id.to_string())
                    .await?
                {
                    tracing::warn!(
                        episode = %f.source_episode_id,
                        "the source has been redacted; dropping this cross-domain link"
                    );
                    continue;
                }
                rows.push(f);
            }
        }
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<Id> = rows.iter().map(|f| f.id).collect();
        store
            .write_txn(async |tx| {
                for f in &rows {
                    tx.insert_fact(f).await?;
                }
                Ok(())
            })
            .await?;
        Ok(ids)
    }

    /// End to end: candidates → judgement → plan → persist.
    pub async fn run(
        &self,
        store: &Store,
        corpus: &dyn LinkCorpus,
        judge: &dyn LinkJudge,
        embedder: &dyn Embedder,
    ) -> Result<LinkReport> {
        let (candidates, scanned) = self.propose(corpus).await?;
        let n_candidates = candidates.len();
        if candidates.is_empty() {
            return Ok(LinkReport {
                scanned,
                ..LinkReport::default()
            });
        }

        let accepted = judge.judge(&candidates).await?;
        let n_accepted = accepted.len();

        // Existing degrees: counted on top of the previous scan, otherwise every
        // night could fill up another full round
        let mut existing: HashMap<String, usize> = HashMap::new();
        for c in &candidates {
            for id in [&c.left.entity_id, &c.right.entity_id] {
                if existing.contains_key(id) {
                    continue;
                }
                let n = corpus
                    .out_edges(id)
                    .await?
                    .len()
                    .min(self.degree_cap.saturating_mul(4));
                existing.insert(id.clone(), n);
            }
        }

        let (planned, degree_capped) = self.plan(embedder, accepted, &existing).await?;
        let written = self.write(store, planned).await?;

        Ok(LinkReport {
            scanned,
            candidates: n_candidates,
            accepted: n_accepted,
            degree_capped,
            written,
        })
    }
}

fn anchor(f: &Fact, entity_name: String, domain: String) -> Anchor {
    Anchor {
        fact_id: f.id.clone(),
        entity_id: f.subject_id.clone(),
        entity_name,
        statement: f.statement.clone(),
        domain,
        source_episode_id: f.source_episode_id.clone(),
    }
}

fn shares_entity(a: &Fact, b: &Fact) -> bool {
    let ids_a: HashSet<&str> = [Some(a.subject_id.as_str()), a.object_entity_id.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    [Some(b.subject_id.as_str()), b.object_entity_id.as_deref()]
        .into_iter()
        .flatten()
        .any(|id| ids_a.contains(id))
}

async fn resolve_name(
    corpus: &dyn LinkCorpus,
    cache: &mut HashMap<String, String>,
    id: &str,
) -> Result<Option<String>> {
    if let Some(n) = cache.get(id) {
        return Ok(Some(n.clone()));
    }
    let name = corpus.entity_name(id).await?;
    if let Some(n) = &name {
        cache.insert(id.to_owned(), n.clone());
    }
    Ok(name)
}

/// The natural-language phrasing of an edge.
///
/// Deliberately uses **only the entity names** and does not restate either side's
/// `statement`: restating would copy the raw text from both domains into the
/// injection block, which both blows the budget and enlarges redact's cascade
/// scope out of nowhere (erase one side's raw text and the copy is still sitting
/// on the other edge).
fn link_statement(from: &Anchor, to: &Anchor, reason: &str) -> String {
    format!(
        "「{}」（{}）与「{}」（{}）相关：{}",
        from.entity_name, from.domain, to.entity_name, to.domain, reason
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use chrono::Utc;
    use std::sync::Mutex;

    /// Build a structurally valid ULID. `Linker::plan` parses entity ids into
    /// [`Id`], so using `"E1"` as an id would only prove the triviality that "the
    /// parser can recognize a bad id".
    fn eid(n: usize) -> String {
        let s = format!("01JAAAAAAAAAAAAAAAAAA{n:05}");
        assert_eq!(s.len(), 26, "a ULID must be 26 characters: {s}");
        s
    }

    /// Pad a readable short label like `"F1"` out into a structurally valid ULID.
    ///
    /// Same reasoning as [`eid`], now for fact ids: a cross-domain edge now has to
    /// record the **source facts** on both ends into `derived_from` (otherwise
    /// redacting only the right side leaves the edge alive carrying the right-hand
    /// entity's name), so `Linker::plan` parses fact ids too.
    fn fid(label: &str) -> String {
        // Crockford base32 excludes I / L / O / U, so any of those in a short label
        // has to be swapped out
        let safe: String = label
            .to_ascii_uppercase()
            .chars()
            .map(|c| match c {
                'I' | 'L' => '1',
                'O' => '0',
                'U' => 'V',
                c if c.is_ascii_alphanumeric() => c,
                _ => '0',
            })
            .collect();
        let s = format!("01JFFFFFFFFFFFFFFFFFF{safe:0>5}");
        assert_eq!(s.len(), 26, "a ULID must be 26 characters: {s}");
        s
    }

    fn fact(id: &str, subject: &str, object: Option<&str>, domain: Option<&str>) -> Fact {
        Fact {
            id: fid(id),
            subject_id: subject.to_owned(),
            predicate: "uses".to_owned(),
            object_text: Some("x".to_owned()),
            object_entity_id: object.map(str::to_owned),
            statement: format!("{subject} 的事实 {id}"),
            embedding: Some(Vector::from(vec![0.1_f32; 4])),
            embedding_model: "hash-stub-v1".to_owned(),
            domain: domain.map(str::to_owned),
            confidence: 0.9,
            valid_at: None,
            source_episode_id: eid(999),
            source_channel: cormex_store::SourceChannel::Conversation,
            trust_tier: Some(2),
            extracted_by: "test".to_owned(),
            device_id: "test".to_owned(),
            created_at: Utc::now(),
        }
    }

    /// A scriptable fake corpus. `neighbors` returns the same list for **any**
    /// query — what is being tested here is the filtering logic, not the
    /// nearest-neighbour algorithm.
    struct FakeCorpus {
        batch: Vec<Fact>,
        neighbors: Vec<(Fact, f64)>,
        edges: HashMap<String, Vec<String>>,
        out_edge_calls: Mutex<usize>,
    }

    impl FakeCorpus {
        fn new(batch: Vec<Fact>, neighbors: Vec<(Fact, f64)>) -> Self {
            Self {
                batch,
                neighbors,
                edges: HashMap::new(),
                out_edge_calls: Mutex::new(0),
            }
        }

        fn with_edge(mut self, from: &str, to: &str) -> Self {
            self.edges
                .entry(from.to_owned())
                .or_default()
                .push(to.to_owned());
            self
        }
    }

    #[async_trait::async_trait]
    impl LinkCorpus for FakeCorpus {
        async fn scan_batch(&self) -> Result<Vec<Fact>> {
            Ok(self.batch.clone())
        }

        async fn neighbors(
            &self,
            _embedding: &Vector,
            _model: &str,
            _limit: i64,
        ) -> Result<Vec<(Fact, f64)>> {
            Ok(self.neighbors.clone())
        }

        async fn out_edges(&self, subject_id: &str) -> Result<Vec<String>> {
            *self.out_edge_calls.lock().expect("lock not poisoned") += 1;
            Ok(self.edges.get(subject_id).cloned().unwrap_or_default())
        }

        async fn entity_name(&self, entity_id: &str) -> Result<Option<String>> {
            Ok(Some(format!("实体{entity_id}")))
        }
    }

    fn linker() -> Linker {
        Linker::new("test")
    }

    // ── Candidate pre-filtering ────────────────────────────

    #[tokio::test]
    async fn proposes_a_cross_domain_pair() {
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]);
        let (out, scanned) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert_eq!(scanned, 1);
        assert_eq!(
            out.len(),
            1,
            "a cross-domain neighbour pair should become a candidate, actual {}",
            out.len()
        );
        assert_eq!(out[0].pair_key(), ("E1".into(), "E2".into()));
    }

    #[tokio::test]
    async fn same_domain_is_not_a_candidate() {
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("coding"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]);
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert!(
            out.is_empty(),
            "a same-domain pair must not be linked — being cross-domain is this edge's entire justification, actually produced {} pairs",
            out.len()
        );
    }

    #[tokio::test]
    async fn near_duplicates_are_not_candidates() {
        // Distance 0.02: the two are really saying the same thing. Linking them
        // produces not one bit
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.02)]);
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert!(
            out.is_empty(),
            "a near-duplicate pair must not be linked, actually produced {} pairs",
            out.len()
        );
    }

    #[tokio::test]
    async fn distant_pairs_are_not_candidates() {
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.80)]);
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert!(
            out.is_empty(),
            "a distance outside the band must not be linked, actual {} pairs",
            out.len()
        );
    }

    #[tokio::test]
    async fn facts_sharing_an_entity_need_no_new_edge() {
        // The left side's object is the right side's subject — graph traversal
        // already gets through
        let left = fact("F1", "E1", Some("E2"), Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]);
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert!(
            out.is_empty(),
            "adding an edge between two facts that already share an entity is just noise, actual {} pairs",
            out.len()
        );
    }

    #[tokio::test]
    async fn existing_edge_makes_the_scan_idempotent() {
        // This is the only deduplication mechanism available under append-only
        // storage with no unique constraint: on the second scan, the edge written
        // the previous night is already in out_edges
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]).with_edge("E1", "E2");
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert!(
            out.is_empty(),
            "the same pair must not be written a second time, actual {} pairs",
            out.len()
        );
    }

    #[tokio::test]
    async fn existing_edge_is_detected_in_either_direction() {
        // Edges are undirected, so checking only one direction would cause pairs
        // that already exist in reverse to be written again
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]).with_edge("E2", "E1");
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert!(
            out.is_empty(),
            "connected in reverse counts as connected, actual {} pairs",
            out.len()
        );
    }

    #[tokio::test]
    async fn same_entity_pair_is_proposed_once_with_the_closest_evidence() {
        // The same entity pair is often hit by several sets of facts at once;
        // writing them as several edges just makes one thing occupy the budget
        // repeatedly
        let left = fact("F1", "E1", None, Some("coding"));
        let near = fact("F3", "E2", None, Some("office"));
        let far = fact("F2", "E2", None, Some("office"));
        // The near one arrives first and the far one second: without the "keep the
        // closest set" check, the later arrival would overwrite the earlier one
        let corpus = FakeCorpus::new(vec![left], vec![(near, 0.20), (far, 0.40)]);
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert_eq!(
            out.len(),
            1,
            "the same entity pair should be proposed only once, actual {} pairs",
            out.len()
        );
        assert!(
            (out[0].distance - 0.20).abs() < 1e-9,
            "the closer set of evidence should be the one kept, actual distance {}",
            out[0].distance
        );
        assert_eq!(out[0].right.fact_id, fid("F3"));
    }

    #[tokio::test]
    async fn out_edges_are_cached_across_the_scan() {
        // Issuing two queries per candidate pair would make the scan's database
        // round trips grow with the square of the candidate count
        let left = fact("F1", "E1", None, Some("coding"));
        let r1 = fact("F2", "E2", None, Some("office"));
        let r2 = fact("F3", "E3", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(r1, 0.30), (r2, 0.35)]);
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert_eq!(out.len(), 2);
        let calls = *corpus.out_edge_calls.lock().expect("lock not poisoned");
        assert_eq!(
            calls, 3,
            "one query each for E1/E2/E3 is enough, actually queried {calls} times"
        );
    }

    #[tokio::test]
    async fn candidates_are_ordered_deterministically() {
        // HashMap iteration order is unstable, and the scan has to be reproducible,
        // otherwise which pairs the degree cap cuts is pure luck.
        // The left side's id is deliberately not named F*: when it collides with a
        // neighbour's name it gets blocked by the "don't link to yourself" rule,
        // producing one candidate fewer and making it look like the ordering broke
        let left = fact("L0", "E1", None, Some("coding"));
        // Ten neighbours rather than two: `HashMap` iteration order varies per
        // process with the hash seed, and with two there is a fifty-fifty chance it
        // happens to come out correct anyway, turning this test into a dud that
        // only goes red occasionally
        let neighbors: Vec<(Fact, f64)> = (0..10)
            .map(|i| {
                (
                    fact(&format!("F{i}"), &format!("E{i:02}"), None, Some("office")),
                    0.40 - f64::from(i) * 0.02,
                )
            })
            .collect();
        let want: Vec<String> = (0..10).rev().map(|i| fid(&format!("F{i}"))).collect();
        let corpus = FakeCorpus::new(vec![left], neighbors);
        let (out, _) = linker()
            .propose(&corpus)
            .await
            .expect("the pre-filter should succeed");
        assert_eq!(
            out.iter()
                .map(|c| c.right.fact_id.clone())
                .collect::<Vec<_>>(),
            want,
            "candidates must be stably ordered ascending by distance — otherwise which ones the degree cap cuts depends entirely on the hash seed"
        );
    }

    // ── Degree cap ─────────────────────────────────────────

    fn accepted(left: usize, right: usize, distance: f64) -> Accepted {
        let l = fact("FL", &eid(left), None, Some("coding"));
        let r = fact("FR", &eid(right), None, Some("office"));
        Accepted {
            candidate: Candidate {
                left: anchor(&l, format!("实体{left}"), "coding".into()),
                right: anchor(&r, format!("实体{right}"), "office".into()),
                distance,
            },
            reason: "测试".into(),
        }
    }

    #[tokio::test]
    async fn degree_cap_stops_hub_entities() {
        // Hub entities are the main route by which noise spreads through the graph:
        // within two hops one can pull in O(d²) subgraphs
        let embedder = HashEmbedder::new();
        let links: Vec<Accepted> = (0..6).map(|i| accepted(1, 100 + i, 0.3)).collect();
        let (planned, capped) = linker()
            .with_degree_cap(2)
            .plan(&embedder, links, &HashMap::new())
            .await
            .expect("planning should succeed");
        assert_eq!(
            planned.len(),
            2,
            "a degree cap of 2 should let exactly two pairs through"
        );
        assert_eq!(
            capped, 4,
            "the remaining four pairs should be recorded as degree_capped"
        );
    }

    #[tokio::test]
    async fn existing_degree_carries_over_between_scans() {
        // Without carrying historical degrees over, every night could fill the same
        // entity up to the cap all over again
        let embedder = HashEmbedder::new();
        let mut existing = HashMap::new();
        existing.insert(eid(1), 4usize);
        let (planned, capped) = linker()
            .plan(&embedder, vec![accepted(1, 9, 0.3)], &existing)
            .await
            .expect("planning should succeed");
        assert!(
            planned.is_empty(),
            "once the historical degree is at the cap, nothing more should be added"
        );
        assert_eq!(capped, 1);
    }

    #[tokio::test]
    async fn batch_cap_bounds_a_single_scan() {
        let embedder = HashEmbedder::new();
        let links: Vec<Accepted> = (0..5).map(|i| accepted(200 + i, 300 + i, 0.3)).collect();
        let (planned, capped) = linker()
            .with_batch_cap(2)
            .plan(&embedder, links, &HashMap::new())
            .await
            .expect("planning should succeed");
        assert_eq!(
            planned.len(),
            2,
            "the total volume of a single scan must have an upper bound"
        );
        assert_eq!(capped, 3);
    }

    // ── The persistence plan ───────────────────────────────

    #[tokio::test]
    async fn plans_both_directions() {
        let embedder = HashEmbedder::new();
        let (planned, _) = linker()
            .plan(&embedder, vec![accepted(1, 2, 0.3)], &HashMap::new())
            .await
            .expect("planning should succeed");
        let p = &planned[0];
        assert_eq!(p.forward.subject_id.to_string(), eid(1));
        assert_eq!(
            p.backward.subject_id.to_string(),
            eid(2),
            "without the reverse edge, graph traversal only walks one way — half the demo scenario is dead"
        );
        assert_eq!(
            p.forward.object_entity_id.map(|i| i.to_string()),
            Some(eid(2))
        );
        assert_ne!(p.forward.id, p.backward.id);
    }

    #[tokio::test]
    async fn planned_facts_carry_no_domain_and_low_confidence() {
        let embedder = HashEmbedder::new();
        let (planned, _) = linker()
            .plan(&embedder, vec![accepted(1, 2, 0.3)], &HashMap::new())
            .await
            .expect("planning should succeed");
        let f = &planned[0].forward;
        assert_eq!(
            f.domain, None,
            "give this edge any domain at all and domain weighting will favour one direction over the other"
        );
        assert!(
            f.confidence < 0.5,
            "an inferred edge must sit clearly below what the extractor produces, actual {}",
            f.confidence
        );
        assert_eq!(f.predicate, RELATES_TO);
        assert!(
            f.valid_at.is_none(),
            "there is no instant at which 'these two started being related'"
        );
    }

    #[tokio::test]
    async fn links_are_traversable_but_not_searchable_by_default() {
        // This guards against someone casually filling in tsv/embedding: an edge's
        // statement packs two entity names and a one-line summary into a single
        // sentence, precisely the shape BM25's ts_rank likes best, so once indexed
        // it dominates the results of any query mentioning either end. Measured
        // cost: overall R@5 0.923→0.907 and MRR 0.817→0.799, with every one of the
        // regressions on questions unrelated to cross-domain linking
        let embedder = HashEmbedder::new();
        let (planned, _) = linker()
            .plan(&embedder, vec![accepted(1, 2, 0.3)], &HashMap::new())
            .await
            .expect("planning should succeed");
        let f = &planned[0].forward;
        assert!(
            f.tsv_source.is_none() && f.embedding.is_none(),
            "by default an edge enters neither the BM25 nor the vector index — its purpose is to be traversed, not retrieved"
        );
        assert!(
            f.object_entity_id.is_some(),
            "but it must still be an edge in the graph, otherwise the whole exercise is pointless"
        );
    }

    #[tokio::test]
    async fn indexed_mode_fills_both_indexes() {
        let embedder = HashEmbedder::new();
        let (planned, _) = linker()
            .with_indexed(true)
            .plan(&embedder, vec![accepted(1, 2, 0.3)], &HashMap::new())
            .await
            .expect("planning should succeed");
        let f = &planned[0].forward;
        assert!(
            f.tsv_source.as_ref().is_some_and(|s| !s.trim().is_empty()),
            "with indexing on, tsv must be written in the same transaction as the main row, never left to an asynchronous backfill"
        );
        assert!(f.embedding.is_some());
    }

    #[test]
    fn statement_names_both_sides_but_quotes_neither() {
        let l = fact("F1", "E1", None, Some("coding"));
        let r = fact("F2", "E2", None, Some("office"));
        let s = link_statement(
            &anchor(&l, "融合模块".into(), "coding".into()),
            &anchor(&r, "评测周会".into(), "office".into()),
            "周会决定了融合参数",
        );
        assert!(s.contains("融合模块") && s.contains("评测周会"));
        assert!(
            !s.contains("的事实 F1") && !s.contains("的事实 F2"),
            "restating both sides' raw text would copy the body from both domains into the injection block: {s}"
        );
    }

    // ── Judgement ──────────────────────────────────────────

    fn two_candidates() -> Vec<Candidate> {
        vec![accepted(1, 2, 0.3).candidate, accepted(3, 4, 0.4).candidate]
    }

    #[test]
    fn judgement_parsing_picks_only_the_accepted_ones() {
        let out = parse_judgements(
            r#"{"links":[{"id":1,"reason":"预算限制选型"}]}"#,
            &two_candidates(),
        );
        assert_eq!(
            out.len(),
            1,
            "when the model accepts only one, the other must not be linked"
        );
        assert_eq!(out[0].candidate.left.entity_id, eid(3));
        assert_eq!(out[0].reason, "预算限制选型");
    }

    #[test]
    fn judgement_parsing_ignores_out_of_range_ids() {
        let out = parse_judgements(
            r#"{"links":[{"id":99,"reason":"x"},{"id":0,"reason":"y"}]}"#,
            &two_candidates(),
        );
        assert_eq!(
            out.len(),
            1,
            "an out-of-range index should drop only that entry, actual {} entries",
            out.len()
        );
    }

    #[test]
    fn judgement_parsing_dedups_repeated_ids() {
        let out = parse_judgements(
            r#"{"links":[{"id":0,"reason":"a"},{"id":0,"reason":"b"}]}"#,
            &two_candidates(),
        );
        assert_eq!(
            out.len(),
            1,
            "the same pair judged twice must not produce two edges"
        );
    }

    #[test]
    fn judgement_parsing_truncates_long_reasons() {
        let long = "很".repeat(200);
        let raw = format!(r#"{{"links":[{{"id":0,"reason":"{long}"}}]}}"#);
        let out = parse_judgements(&raw, &two_candidates());
        assert_eq!(
            out[0].reason.chars().count(),
            MAX_REASON_CHARS,
            "reason goes straight into the prompt, so its length must have a hard ceiling"
        );
    }

    #[test]
    fn unparseable_judgement_links_nothing_instead_of_failing() {
        // The correct behaviour on a failed judgement is "link nothing in this
        // batch": one missing edge is a little less recall, whereas turning the
        // entire nightly scan red is something nobody notices until the next day
        let out = parse_judgements("模型今天不想输出 JSON", &two_candidates());
        assert!(out.is_empty());
    }

    #[test]
    fn judge_prompt_states_the_only_criterion() {
        assert!(
            JUDGE_SYSTEM.contains("帮忙") && JUDGE_SYSTEM.contains("宁可漏连"),
            "the criterion and the direction of preference must be written into the prompt, or the model will start linking mere topical adjacency"
        );
    }

    #[tokio::test]
    async fn accept_all_is_the_prefilter_control_group() {
        let c = two_candidates();
        let out = AcceptAll
            .judge(&c)
            .await
            .expect("the control group cannot fail");
        assert_eq!(out.len(), c.len());
    }
}

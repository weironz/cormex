//! The Cormex memory engine.
//!
//! **The one part of this project we have to write ourselves.** Provider
//! adapters, the sandbox, diff application — all of those can be lifted from
//! goose / codex. Memory is the only thing nobody else already has.
//!
//! The submodules map onto the stages a memory passes through:
//!
//! | Module | Stage |
//! |---|---|
//! | [`tokenize`] | Ingest: Chinese word segmentation → `tsvector` (the prerequisite for the BM25 channel) |
//! | [`embed`]    | Ingest: vectorization (local inference, so memory content never leaves the machine) |
//! | [`extract`]  | Ingest: one conversational turn → structured facts → persisted |
//! | [`trace`]    | Ingest: one turn of **tool traces** → structured facts (hard signal, no model asked) |
//! | [`transcribe`]| Ingest: media → text (media that never becomes text is a retrieval black hole) |
//! | [`crosslink`]| Offline: cross-domain `relates_to` edges (so graph traversal can walk between two domains on its own) |
//! | [`fusion`]   | Retrieval: RRF fusion over the four recall channels |
//! | [`injection`]| Egress: what shape memory takes inside the prompt (this decides the caching cost structure) |

/// Egress: what shape memory takes inside the prompt.
///
/// **The implementation now lives in [`cormex_core::injection`].** The local
/// agent and cormexd must render byte-for-byte identical memory blocks, and the
/// local agent should not have to drag the whole memory engine (and with it
/// `cormex-store` → sqlx) in just for those 245 lines. The re-export is kept
/// here so existing call sites don't have to move.
pub use cormex_core::injection;

pub mod brief;
pub mod crosslink;
pub mod distill;
pub mod embed;
pub mod embed_api;
/// In-process ONNX inference backend. **Not built by default** — see the
/// module docs on [`embed_api`] for why.
#[cfg(feature = "local-embed")]
pub mod embed_local;
pub mod extract;
pub mod fusion;
pub mod redaction;
pub mod retrieval;
pub mod tokenize;
pub mod trace;
pub mod transcribe;

pub use brief::{Brief, BriefLine, Briefer, CheapModelBriefer};
pub use crosslink::{
    AcceptAll, Accepted, Candidate, CheapModelJudge, LinkCorpus, LinkJudge, LinkReport, Linker,
    RELATES_TO, StoreCorpus,
};
pub use distill::{CheapModelObserver, DistillEvent, Observation, ObservationOutcome, Observer};
pub use embed::{EMBEDDING_DIM, Embedder};
pub use extract::{
    CandidateFact, EntityIndex, EntityRef, ExtractContext, Extractor, InProcessEntityIndex,
    IngestReport, ObjectRef, PredicateRules, Rival, Verdict,
};
pub use fusion::{Channel, Fused};
pub use injection::{Budget, MemoryItem};
pub use retrieval::{Attribution, RecallWidth, Retrieved, Retriever};
pub use trace::{TraceRules, candidates_from_trace};
pub use transcribe::{
    NeverRedacted, Outcome, RedactionGuard, Segment, StubTranscriber, TranscribePipeline,
    Transcriber, VisionTranscriber,
};

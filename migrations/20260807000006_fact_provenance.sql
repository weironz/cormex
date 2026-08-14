-- Source trust tier (facts.source_channel / trust_tier) + derivation lineage (derivations)
--
-- Design basis: docs/memory-content.md §4.3 (source trust tiers), §5.3 (propagating a
-- deletion to derivations), §8.3 (source grading and PROV-O), §13 first batch 1/2.
--
-- The two live in one migration because they answer two halves of the same question:
-- **where did this memory come from**. The first answers "through which channel", the
-- second answers "from which sources".


-- ══════════════════════════════════════════════════════════
--  1. Source channel and trust tier for facts
--
--  Every published memory-poisoning attack (MINJA / AgentPoison / SpAIware) happens on
--  the **write side**: the attacker never touches the injection channel, it is enough to
--  induce the extractor to write malicious content as a fact. And append-only keeps a
--  poisoned record around forever and keeps feeding it into retrieval -- with no source
--  marking there is no way whatsoever to sweep those out in bulk.
--
--  Three uses (this change covers the write side only; the read-side weighting has
--  prerequisites, see §9 #11):
--    1. high-risk sources go into quarantine (not done)
--    2. retrieval ranking weighted by trust tier (not done; the prerequisite is wiring
--       confidence into the fusion first)
--    3. **bulk invalidation by source after an incident** -- append a batch of
--       fact_events, which append-only supports natively
-- ══════════════════════════════════════════════════════════

-- ── 🔴 Existing rows: why 'unknown_legacy' and not some real tier ──
--
-- ADD COLUMN ... DEFAULT is a metadata operation on PG11+, not an UPDATE, so it **does
-- not break append-only**. The genuinely irreversible trouble comes afterwards:
--
--   1. the value the existing rows receive **can never be changed again** -- correcting it
--      would take an UPDATE, and UPDATE is allowed only under the one redact/purge exception;
--   2. it **cannot be reconstructed after the fact**: by construction, the
--      source_episode_id of every existing fact points at the role='user' episode (the
--      extraction ctx uses user_episode_id, see cortexd::live), while the text handed to
--      the extractor is one whole block glued together as "User: ... / Assistant: ...".
--      There is **no trace at all** in the database that could tell "the user said it"
--      apart from "the model inferred it".
--
-- So what goes in here is an honest "unknown" rather than a tier that a query would take
-- at face value:
--   · source_channel = 'unknown_legacy'
--   · trust_tier     = NULL          ← "unknown" is not a point on the trust scale
--
-- The price, stated plainly: of the three uses, the third (bulk invalidation by source) is
-- **permanently unavailable** for every fact that predates this migration; they can only
-- be dealt with as one undifferentiated lump. That is a price already paid, not a gap that
-- can be patched later -- it is the cautionary counterexample to the principle in §8.3:
-- "the extractor must explicitly record which input fragment each result corresponds to;
-- it cannot be reconstructed afterwards".
--
-- Why trust_tier is not given a sentinel tier such as 0 or 5: the tiers are ordered (the
-- smaller the number, the more trustworthy), and forcing "unknown" onto that scale means
-- any `WHERE trust_tier <= n` will quietly count it in or quietly leave it out, depending
-- on which end the sentinel sits. The contagiousness of NULL in SQL is exactly what we
-- want here: a query that forgot to handle "unknown" comes back empty, rather than wrong.

ALTER TABLE facts
    ADD COLUMN source_channel TEXT NOT NULL DEFAULT 'unknown_legacy';

-- Drop the default straight away. The value of the existing rows is preserved by PG's
-- attmissingval (DROP DEFAULT neither backfills nor rewrites the table), while **from now
-- on** any INSERT that forgets this column hits the NOT NULL and errors out -- instead of
-- silently manufacturing another batch of "unknown"s.
ALTER TABLE facts
    ALTER COLUMN source_channel DROP DEFAULT;

-- Nullable on purpose, see above: NULL belongs exclusively to the "unknown" of the
-- existing rows. New writes are forced to state a tier by the CHECK below.
ALTER TABLE facts
    ADD COLUMN trust_tier SMALLINT;

-- The channel-to-tier mapping. Written as a CHECK rather than left to the good intentions
-- of the application layer: once the two drift apart, the symptom is "the set filtered by
-- tier is not the set filtered by channel", and that inconsistency only comes to light
-- when a security incident actually happens and bulk invalidation is needed.
--
--   1  stated by the user           highest
--   2  inferred by the model from the conversation
--   2  derived by the model from existing facts  (crosslink's cross-domain edges; see
--                                                 "trust laundering" below)
--   3  tool output (executed locally)
--   4  external content (web page / someone else's document / MCP)
--                                                ← not extracted by default, must be
--                                                  enabled explicitly
--
-- ⚠️ Known and unsolved, "trust laundering": derived does **not** currently inherit the
-- tier of its input facts. A derived edge built on tier-4 external content ends up
-- labelled tier 2. This is precisely the problem §8.3 cites C2PA for, reproduced on the
-- inside ("a memory loses its provenance after one round of summarization"). The fix is to
-- make derived take the worst tier among all of its inputs -- the prerequisite being that
-- a derived edge can find its inputs at all, which is the derivations table below. Left to
-- the §9 #11 batch to be done together; by then it will be fixable (unlike the existing
-- rows, which are lost for good), because derivations records the inputs from today onward.
ALTER TABLE facts
    ADD CONSTRAINT facts_trust_tier_matches_channel CHECK (
        (source_channel = 'user_stated'    AND trust_tier = 1) OR
        (source_channel = 'conversation'   AND trust_tier = 2) OR
        (source_channel = 'derived'        AND trust_tier = 2) OR
        (source_channel = 'tool_output'    AND trust_tier = 3) OR
        (source_channel = 'external'       AND trust_tier = 4) OR
        (source_channel = 'unknown_legacy' AND trust_tier IS NULL)
    );

COMMENT ON COLUMN facts.source_channel IS
    'Which channel this fact came in through. unknown_legacy belongs exclusively to rows
     that predate this column -- their real source cannot be reconstructed afterwards, so
     they are marked as unknown rather than given a fabricated tier.';
COMMENT ON COLUMN facts.trust_tier IS
    'Source trust tier, 1 is the highest. NULL = unknown (legacy rows only).
     Its correspondence with source_channel is nailed down by facts_trust_tier_matches_channel.';

-- The query path for "bulk invalidation by source after an incident".
-- Not built on trust_tier: that column is functionally dependent on source_channel, so a
-- second index would only be one more thing to maintain, with worse selectivity as well.
CREATE INDEX idx_facts_channel ON facts (source_channel);

-- ── ⚠️ Adding a column to facts means rebuilding active_facts in the same breath ──
--
-- `active_facts` was created as `SELECT f.*`, and Postgres expands `*` into a fixed column
-- list at **the moment the view is created**. Add a column to facts later and the view
-- does not grow with it -- while everyday retrieval goes through active_facts **in its
-- entirety** (the four-way retrieval, the rivals query of contradiction resolution, the
-- reverse hop of graph traversal), so adding a column makes whole swathes of it report
-- `column "source_channel" does not exist`.
--
-- This pit is not like "forgetting to update the match in sync.rs", which only goes off
-- the first time some particular table is written: this one blows up immediately and
-- across the board, so it is in fact far less dangerous. It is written down here because
-- **the next person who adds a column to facts will step in it again**, and nothing in a
-- migration reminds them that the view exists.
--
-- `CREATE OR REPLACE VIEW` only permits **appending at the end** (no dropping, no renaming,
-- no type changes). That happens to be satisfied here: `ALTER TABLE ADD COLUMN` puts new
-- columns at the end of the table, so `f.*` expands to "the original columns in their
-- original order + the two new ones".
CREATE OR REPLACE VIEW active_facts AS
SELECT f.*
FROM facts f
LEFT JOIN fact_status s ON s.fact_id = f.id
WHERE s.op IS NULL OR s.op = 'revoke';


-- ══════════════════════════════════════════════════════════
--  2. Derivation lineage -- derivations
--
--  The erasure obligation of GDPR Art.17 reaches **derived data**, and in an append-only
--  system the tombstone of a source fact does not automatically invalidate the summaries /
--  graph edges / profile blocks. If a redacted fact has already taken part in some
--  consolidation summary or some graph edge, those derivations go on leaking it.
--
--  The correct shape of purge is: find every derivation along the lineage → tombstone →
--  replay the projection from the remaining sources.
--  With no lineage edges, a summary built on top of a source fact that was later
--  invalidated becomes zombie memory that nobody can be held accountable for.
--
--  The academic side supports the same conclusion independently: the because-of citations
--  of Generative Agents, the tree parent/child of RAPTOR, the contains edges of
--  HippoRAG2 -- an abstraction must carry an edge pointing back at the source record.
--
--  **This requires the schema to store derivation edges from day one; retrofitting them is
--  impossible** (§8.3: if one extraction emits several results, the step that produced them
--  must explicitly record which inputs each one corresponds to).
-- ══════════════════════════════════════════════════════════

-- ── Why a separate edge table, instead of an id-array column on each derived table ──
--
-- An array column (summaries.source_episode_ids ulid[]) is simpler, and could even enforce
-- non-emptiness at the database layer with CHECK (cardinality(...) > 0). What rules it out
-- is the **reverse lookup**:
--
--   redact/purge asks "given this episode / fact, what derives from it", across every kind
--   of derivation. With array columns the executor has to ask that **table by table** with
--   `WHERE $1 = ANY(...)`, which means every new kind of derivation requires remembering
--   to go and change the redact executor.
--   That is exactly the "secret scattered across several places" that migration
--   20260807000005 already fell into once.
--   One edge table turns it into a single `WHERE source_kind = ? AND source_id = ?`, and a
--   future profile block gets covered automatically without touching the executor.
--
-- Besides, the source kinds are inherently heterogeneous: summaries derive from episodes,
-- crosslink's edges derive from facts, a future profile block derives from facts +
-- summaries. An array column would need either three columns, or a kind encoded into the
-- string.
--
-- ── Why there are no foreign keys ──
--
-- Postgres has no polymorphic foreign keys, and the target table of derived_id / source_id
-- varies with kind. This table did not invent that: redactions.target_id is already the
-- same shape (target_kind + a bare id). The price is that dangling references are not
-- caught by the database -- and it is paid on the write side: the only entry point that
-- writes a derivation edge is the same method that writes the derived row (see
-- cortex_store::WriteTxn::insert_summary), and both are in the same transaction.

CREATE TABLE derivations (
    id            ulid        PRIMARY KEY,

    -- The derivation side
    derived_kind  TEXT        NOT NULL CHECK (derived_kind IN ('fact', 'summary')),
    derived_id    ulid        NOT NULL,

    -- The source side. episode is the L0 original text, fact / summary are lower-order
    -- derivations -- lineage can have several levels (profile block → summary → episode),
    -- and purge has to walk the transitive closure
    source_kind   TEXT        NOT NULL CHECK (source_kind IN ('episode', 'fact', 'summary')),
    source_id     ulid        NOT NULL,

    device_id     TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    -- A self-loop sends lineage traversal straight into an infinite loop, and the symptom
    -- is purge hanging rather than raising an error
    CHECK (NOT (derived_kind = source_kind AND derived_id = source_id)),

    -- Writing the same edge twice is not "one extra row", it gives the question "how many
    -- sources does this summary have" two different answers. The constraint is here to turn
    -- a duplicate write into an error that makes noise
    UNIQUE (derived_kind, derived_id, source_kind, source_id)
);

COMMENT ON TABLE derivations IS
    'Lineage edges from derivation → source (append-only). redact/purge uses them to find
     the derivations that have to be invalidated along with the source; without them, a
     summary built on top of erased content keeps leaking it.';

-- The hot path of redact/purge: given a source, what derives from it.
-- The reverse direction (given a derivation, find its sources) is covered by
-- (derived_kind, derived_id), the index prefix of the UNIQUE constraint, so no separate
-- index is created.
CREATE INDEX idx_derivations_source ON derivations (source_kind, source_id);

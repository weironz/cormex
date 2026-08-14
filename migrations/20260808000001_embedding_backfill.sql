-- No more silent amnesia when the embedding model changes -- the vector side tables
--
-- Design basis: steps 2 and 3 of docs/memory.md §7, "swapping models without downtime".
-- Step 1 (new writes use the new model) was already done in init; steps 2 and 3 have been
-- empty all along.
--
-- ══════════════════════════════════════════════════════════
--  What hole this table plugs
--
--  All four vector queries carry an `embedding_model = $N` filter (recall.rs), so the
--  failure mode "distances computed across models come out as garbage" does not exist --
--  which is good.
--  The price, however, is this: **on the day the model changes, the vector leg returns 0
--  rows.** Not one old fact can match the new model_id.
--
--  Meanwhile BM25 / graph traversal / recency keep working as usual, so it **does not look
--  broken**, only like "semantic retrieval seems to have got a bit worse". Nobody is told.
--  The filter traded "giving a wrong answer" for "silent partial amnesia" -- strictly
--  better, and still silent.
--
--  This is not hypothetical: after the default backend went from fastembed to a remote API,
--  model_id changed from `fastembed:gpahal/bge-m3-onnx-int8` to `api:bge-m3`, and in any
--  database that upgrades, every existing fact drops out of vector retrieval on the spot.
-- ══════════════════════════════════════════════════════════
--
-- ── Why a **side table**, rather than editing facts.embedding in place ──
--
--   1. CLAUDE.md: "no table does UPDATE / DELETE". cortex-store currently has zero
--      UPDATEs, and the existing precedent is blob_transcripts -- derived data gets new
--      rows in a table of its own. A backfill is a pure append, so this rule holds as it
--      stands
--   2. "retrieval from both models during the backfill" (§7 step 3) requires one and the
--      same fact to hold vectors from two spaces **at once**. One column cannot hold two
--
-- ── Why facts.embedding is **not moved out** ──
--
--   Moving it would mean touching the Fact struct, the three inserts in txn.rs and the row
--   reconstruction in sync.rs -- an enormous ripple whose only payoff is tidiness. It is
--   written as a pure append instead:
--
--       all vectors under model M
--         = facts WHERE embedding_model = M           (the model in force when it was written)
--         ∪ fact_embeddings WHERE embedding_model = M (backfilled afterwards)
--
--   New writes are unchanged; the backfill only inserts into the side table. Expand-only:
--   **no copying of existing rows, no dropping of the old column**, so this migration can
--   be applied to a running database without disturbing any existing query.
--
-- ── Why only facts and entities ──
--
--   summaries and blob_transcripts also carry an embedding plus an HNSW index, but
--   **no vector query currently uses them** (all four vector queries in recall.rs touch
--   facts and entities only). Building a second set of indexes for tables nothing reads is
--   pure write amplification.
--   Whether those two indexes deserve to stay at all is a separate question, noted in the
--   roadmap.

-- ══════════════════════════════════════════════════════════
--  facts
-- ══════════════════════════════════════════════════════════

CREATE TABLE fact_embeddings (
    fact_id         ulid         NOT NULL REFERENCES facts(id),
    embedding_model TEXT         NOT NULL,
    -- NOT NULL: a row in the side table exists **only** to say "the vector of this fact
    -- under this model". Allowing NULL would allow a row that states nothing, and would
    -- cost the query side one more IS NOT NULL
    embedding       VECTOR(1024) NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),

    -- One fact has exactly one vector under one model. When the backfiller runs over the
    -- same row twice, this constraint is what stops it (ON CONFLICT DO NOTHING), rather
    -- than the backfiller keeping books of its own -- a process restart, or two devices
    -- backfilling at the same time, both end up here
    PRIMARY KEY (fact_id, embedding_model)
);

-- ── This index is the entire reason the table exists ──
--
-- Retrieval was changed to "the original column UNION the side table", two subqueries each
-- sorted and then merged, precisely so that **both sides can use their own HNSW**. Write
-- it as a LATERAL / JOIN with the ORDER BY afterwards and the index stops being used
-- altogether, degenerating into a full table scan -- and that kind of query is blazingly
-- fast on a small database and only shows itself once the database has grown, by which
-- point it looks like "the database got slower" rather than "the index is not being used"
CREATE INDEX idx_fact_embeddings_vec ON fact_embeddings
    USING hnsw (embedding vector_cosine_ops);

-- Both the backfill's to-do query and the census (the comparison done at startup) filter
-- by model
CREATE INDEX idx_fact_embeddings_model ON fact_embeddings (embedding_model);

-- ══════════════════════════════════════════════════════════
--  entities
--
--  Entity vectors take no part in the four-way retrieval; they serve **entity resolution
--  during extraction** only (same-name / near-synonym matching).
--  But a model swap hits them just the same, and their failure mode is more insidious: a
--  resolution that fails creates a second entity for the same person, from then on his
--  facts are split across two nodes, and graph traversal can never get through again.
-- ══════════════════════════════════════════════════════════

CREATE TABLE entity_embeddings (
    entity_id       ulid         NOT NULL REFERENCES entities(id),
    embedding_model TEXT         NOT NULL,
    embedding       VECTOR(1024) NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (entity_id, embedding_model)
);

CREATE INDEX idx_entity_embeddings_vec ON entity_embeddings
    USING hnsw (embedding vector_cosine_ops);

CREATE INDEX idx_entity_embeddings_model ON entity_embeddings (embedding_model);

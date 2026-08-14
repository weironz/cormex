-- Initial schema of the Cortex memory system
-- Design rationale: see docs/memory.md. This file is the authoritative version of
-- the schema; where the docs show SQL fragments, this file wins.
--
-- Core constraint: append-only, end to end. Apart from redact/purge there is no
-- UPDATE and no DELETE anywhere.
-- Primary keys are uniformly ULIDs (26-character Crockford base32, uppercase), so
-- clients can generate them offline.
-- Vector dimension is 1024, matching bge-m3. To change models see docs/memory.md §7.
--
-- [Write discipline] (details in docs/memory.md §9; break any of these and you
-- break sync correctness):
--   1. every write to a table that has to sync must append to sync_log inside the
--      same transaction;
--   2. that transaction opens with pg_advisory_xact_lock(4272), which serializes
--      commit order and so guarantees that sync_log.seq order == the order in which
--      rows become visible to readers (a bare BIGSERIAL cannot do this: the sequence
--      value is handed out at INSERT time, so out-of-order commits make a client that
--      already pulled skip those rows permanently);
--   3. the sequence-taking transaction must be short and write-only. LLM calls and
--      embedding computation always happen outside the transaction;
--   4. tsv is written in the same transaction as its main row, never backfilled
--      asynchronously (a backfill is an UPDATE, and without a sync_log row it is
--      invisible to every other device).

CREATE EXTENSION IF NOT EXISTS vector;

-- ULID: 26 characters of Crockford base32 (no I/L/O/U), uppercase.
-- COLLATE "C" makes the B-tree compare byte by byte -- which is both fast and keeps
-- sort order identical to generation order in time.
CREATE DOMAIN ulid AS TEXT COLLATE "C"
    CHECK (VALUE ~ '^[0-9A-HJKMNP-TV-Z]{26}$');

-- SHA-256, lowercase hex
CREATE DOMAIN sha256 AS TEXT COLLATE "C"
    CHECK (VALUE ~ '^[0-9a-f]{64}$');

-- ══════════════════════════════════════════════════════════
--  Sync log (outbox) -- the one true ordering for multi-device sync
--
--  Every business write transaction appends one row here. Incremental client pulls
--  look at nothing but this table:
--    GET /sync?since={seq} → returns the records with seq > since plus their
--    corresponding business rows.
--  A total order across tables falls out for free (the nine tables' individual
--  BIGSERIALs are not comparable with each other, so a single cursor over them is
--  mathematically impossible -- which is exactly why this table exists).
--  Replaying in log order also guarantees FK order (facts always arrive after the
--  episode they came from).
--  Bonus: hang LISTEN/NOTIFY off this table and you have the event source for
--  WebSocket live push.
-- ══════════════════════════════════════════════════════════

CREATE TABLE sync_log (
    seq        BIGSERIAL   PRIMARY KEY,
    table_name TEXT        NOT NULL,
    record_id  TEXT        NOT NULL,   -- primary key of the target row; episode_blobs uses 'episode_id:blob_hash'
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

-- ══════════════════════════════════════════════════════════
--  L0 raw layer -- the single source of truth, appended to and never modified
-- ══════════════════════════════════════════════════════════

CREATE TABLE episodes (
    id          ulid        PRIMARY KEY,
    session_id  TEXT        NOT NULL,
    role        TEXT        NOT NULL CHECK (role IN ('user', 'assistant', 'tool', 'system')),
    -- The raw message, including provider-specific opaque blocks such as
    -- thinking / reasoning, preserved losslessly.
    -- Once redacted, the placeholder '{"redacted":true}' is written here
    -- (see docs/memory.md §11).
    content     JSONB       NOT NULL,
    -- Plain text extracted out of content, for full-text search
    text        TEXT,
    -- Written after tokenization by jieba-rs on the Rust side; same transaction as
    -- the main row, never backfilled asynchronously
    tsv         TSVECTOR,
    domain      TEXT,
    device_id   TEXT        NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON COLUMN episodes.occurred_at IS 'When the event happened (client clock, after monotonization)';
COMMENT ON COLUMN episodes.created_at  IS 'When the server stored it';

-- ── Binary content: SHA-256 content addressing, so dedup and immutability come free ──

CREATE TABLE blobs (
    hash        sha256      PRIMARY KEY,
    mime        TEXT        NOT NULL,
    size_bytes  BIGINT      NOT NULL CHECK (size_bytes >= 0),
    storage_key TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE episode_blobs (
    episode_id ulid   NOT NULL REFERENCES episodes(id),
    blob_hash  sha256 NOT NULL REFERENCES blobs(hash),
    kind       TEXT,
    PRIMARY KEY (episode_id, blob_hash)
);

-- Output of the async pipeline: media that never becomes text is a retrieval black hole.
-- Before writing, the redactions table must be consulted, so that an in-flight job cannot
-- refill content that has already been erased (docs/memory.md §11).
CREATE TABLE blob_transcripts (
    id              ulid         PRIMARY KEY,
    blob_hash       sha256       NOT NULL REFERENCES blobs(hash),
    kind            TEXT         NOT NULL
                    CHECK (kind IN ('asr', 'vision_caption', 'ocr', 'frame_caption')),
    text            TEXT         NOT NULL,
    tsv             TSVECTOR,
    embedding       VECTOR(1024),
    -- Audio/video transcripts get one row per segment and record the segment's
    -- millisecond offsets -- this is what makes "jump to the source at 31:40" work.
    -- NULL for images and OCR.
    span_start_ms   BIGINT,
    span_end_ms     BIGINT,
    transcribed_by  TEXT         NOT NULL,
    embedding_model TEXT         NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),
    CHECK (span_end_ms IS NULL OR span_start_ms IS NOT NULL)
);

-- ══════════════════════════════════════════════════════════
--  L1 fact layer
-- ══════════════════════════════════════════════════════════

CREATE TABLE entities (
    id              ulid         PRIMARY KEY,
    -- person / project / file / org / concept / tool ... open-ended, deliberately not an enum
    kind            TEXT         NOT NULL,
    name            TEXT         NOT NULL,
    summary         TEXT,
    -- Entity vectors serve entity resolution during extraction (matching same-name /
    -- near-synonymous entities); they take no part in the four-way retrieval
    embedding       VECTOR(1024),
    embedding_model TEXT         NOT NULL,
    device_id       TEXT         NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);

-- Alias resolution: entities is not modified, a merge record is appended instead.
--
-- UNIQUE(from_entity) gives every entity at most one outgoing edge:
--   · the ownership graph becomes a functional graph, so chained merges resolve
--     deterministically;
--   · a concurrent fork (two devices merging A into B and into C respectively)
--     physically becomes first-writer-wins -- the later one is rejected by the unique
--     constraint, and the server's receipt turns it into a no-op on the client;
--   · cycles are still possible (A→B and B→A are two edges with different from), and
--     are rejected by cortexd walking the into_entity chain to its end before writing
--     (see docs/memory.md §9).
CREATE TABLE entity_merges (
    id                ulid         PRIMARY KEY,
    from_entity       ulid         NOT NULL REFERENCES entities(id) UNIQUE,
    into_entity       ulid         NOT NULL REFERENCES entities(id),
    reason            TEXT,
    source_episode_id ulid         REFERENCES episodes(id),
    device_id         TEXT         NOT NULL,
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),
    CHECK (from_entity <> into_entity)
);

CREATE TABLE facts (
    id                ulid         PRIMARY KEY,
    subject_id        ulid         NOT NULL REFERENCES entities(id),
    -- prefers / decided / owns / blocked_by ... open-ended, deliberately not an enum:
    -- enumerate it and no new domain can ever get in
    predicate         TEXT         NOT NULL,
    object_text       TEXT,
    -- When it points at another entity, this row is an edge in the knowledge graph
    object_entity_id  ulid         REFERENCES entities(id),
    -- Natural-language phrasing, used for embedding and for display in the UI.
    -- Once redacted, the placeholder '[redacted]' is written here
    statement         TEXT         NOT NULL,
    tsv               TSVECTOR,
    embedding         VECTOR(1024),
    embedding_model   TEXT         NOT NULL,
    domain            TEXT,
    -- Scored once by the extractor at write time, immutable thereafter (append-only).
    -- Semantics: the extractor's confidence that this fact faithfully reflects the
    -- source text. Usable as a secondary signal when fusing retrieval results.
    confidence        REAL         NOT NULL DEFAULT 1.0
                      CHECK (confidence >= 0.0 AND confidence <= 1.0),

    -- [Valid time] when it started being true. NULL = unknown, or always has been
    valid_at          TIMESTAMPTZ,

    -- Provenance: the bedrock that makes any of this auditable
    source_episode_id ulid         NOT NULL REFERENCES episodes(id),
    extracted_by      TEXT         NOT NULL,
    device_id         TEXT         NOT NULL,
    -- [Transaction time] when Cortex came to know this
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),

    CHECK (object_text IS NOT NULL OR object_entity_id IS NOT NULL)
);

-- Lifecycle events of a fact. Automatic system invalidation and manual user edits
-- share this table.
-- Invalidation is expressed by appending a record rather than an in-place UPDATE --
-- that is the premise of both append-only and conflict-free multi-device sync.
CREATE TABLE fact_events (
    id                ulid         PRIMARY KEY,
    fact_id           ulid         NOT NULL REFERENCES facts(id),

    -- invalidate makes a fact invalid; revoke undoes an invalidation (state-machine
    -- semantics: the final state is whatever the last state event says); flag marks
    -- "contradiction, needs human confirmation" -- an annotation that does not change state.
    op                TEXT         NOT NULL CHECK (op IN ('invalidate', 'revoke', 'flag')),
    -- Reason category when op = invalidate. The four differ in their time semantics, and
    -- mixing them up makes epistemic replay come out wrong:
    --   superseded was true once, then the world changed (superseded_by is mandatory,
    --              otherwise the evolution chain breaks)
    --   corrected  never was true, the extraction was wrong
    --   retracted  the user deleted it deliberately; takes no position on truth
    --   expired    was true once, then lapsed
    -- Note: the share of corrected is only "a biased, lagging lower bound on extraction
    -- precision" -- it depends on retrieval exposure and on how diligent the user is, and
    -- it cannot see missed extractions or errors that were never retrieved, so it must not
    -- be taken as a direct measure of extraction quality.
    kind              TEXT         CHECK (kind IN ('superseded', 'corrected', 'retracted', 'expired')),

    -- [Valid time] when it stopped being true
    invalid_at        TIMESTAMPTZ,
    superseded_by     ulid         REFERENCES facts(id),

    actor             TEXT         NOT NULL CHECK (actor IN ('system', 'user')),
    reason            TEXT,
    source_episode_id ulid         REFERENCES episodes(id),
    device_id         TEXT         NOT NULL,
    -- [Transaction time] when it was discovered / when the action was taken
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),

    -- invalidate must state a reason category and an invalidation time
    CHECK (op <> 'invalidate' OR (kind IS NOT NULL AND invalid_at IS NOT NULL)),
    -- revoke / flag do not carry the invalidate-only fields
    CHECK (op = 'invalidate' OR (kind IS NULL AND invalid_at IS NULL AND superseded_by IS NULL)),
    -- superseded must name what superseded it, otherwise the "fact evolution chain" breaks
    CHECK (kind IS DISTINCT FROM 'superseded' OR superseded_by IS NOT NULL)
);

-- ══════════════════════════════════════════════════════════
--  L2 summary layer
-- ══════════════════════════════════════════════════════════

CREATE TABLE summaries (
    id              ulid         PRIMARY KEY,
    scope           TEXT         NOT NULL CHECK (scope IN ('session', 'topic', 'period')),
    scope_key       TEXT         NOT NULL,
    text            TEXT         NOT NULL,
    tsv             TSVECTOR,
    embedding       VECTOR(1024),
    embedding_model TEXT         NOT NULL,
    covers_from     TIMESTAMPTZ,
    covers_to       TIMESTAMPTZ,
    device_id       TEXT         NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);

-- ══════════════════════════════════════════════════════════
--  Redaction records -- the one place in the whole system where breaking
--  append-only is permitted
--
--  For the cases where content genuinely has to be destroyed: a secret pasted by
--  mistake, privacy.
--  target_kind is only episode|blob: a redact cascades along source_episode_id and
--  clears every derived landing point (facts.statement/tsv/embedding,
--  blob_transcripts, and regenerates the affected summaries); the semantics and the
--  execution rules are in docs/memory.md §11.
--  This table ships through sync_log; on receipt a client must idempotently perform
--  the equivalent local erasure.
--  What gives way is the content -- the tombstone and the audit chain are kept forever.
-- ══════════════════════════════════════════════════════════

CREATE TABLE redactions (
    id          ulid         PRIMARY KEY,
    target_kind TEXT         NOT NULL CHECK (target_kind IN ('episode', 'blob')),
    target_id   TEXT         NOT NULL,
    mode        TEXT         NOT NULL CHECK (mode IN ('redact', 'purge')),
    reason      TEXT         NOT NULL,
    actor       TEXT         NOT NULL,
    device_id   TEXT         NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);

-- ══════════════════════════════════════════════════════════
--  Views
-- ══════════════════════════════════════════════════════════

-- The most recent [state] event of each fact (flag is an annotation and takes no part
-- in deciding state).
-- Ordered by (created_at, id) -- created_at is clock_timestamp() at microsecond
-- resolution, and id is only a deterministic tiebreaker for the extreme case of two
-- events at the very same instant.
CREATE VIEW fact_status AS
SELECT DISTINCT ON (fact_id)
       fact_id, op, kind, invalid_at, superseded_by, actor,
       created_at AS decided_at
FROM fact_events
WHERE op IN ('invalidate', 'revoke')
ORDER BY fact_id, created_at DESC, id DESC;

-- Facts that are currently valid: never invalidated, or the latest state event was a
-- revoke (restored)
CREATE VIEW active_facts AS
SELECT f.*
FROM facts f
LEFT JOIN fact_status s ON s.fact_id = f.id
WHERE s.op IS NULL OR s.op = 'revoke';

-- Where an entity finally belongs after alias merges.
-- UNIQUE(from_entity) guarantees a functional graph (at most one outgoing edge per
-- entity), so walking the chain to its end lands on the terminus; the depth cap is a
-- backstop against cycles (cycles are stopped by the write-time guard, see the
-- entity_merges comment).
CREATE VIEW canonical_entities AS
WITH RECURSIVE walk(id, canonical, depth) AS (
    SELECT e.id, e.id, 0
    FROM entities e
  UNION ALL
    SELECT w.id, m.into_entity, w.depth + 1
    FROM walk w
    JOIN entity_merges m ON m.from_entity = w.canonical
    WHERE w.depth < 32
)
SELECT DISTINCT ON (id) id, canonical
FROM walk
ORDER BY id, depth DESC;

-- ══════════════════════════════════════════════════════════
--  Indexes
--
--  Vector indexes cover facts / summaries / blob_transcripts (the four-way retrieval)
--  plus entities (used only for entity resolution during extraction, not for retrieval).
--  L0 episodes outnumber facts by tens of times, so they get a full-text index only --
--  exact retrospection can be pinpointed with "full text + time". This strategy keeps
--  the retrievable vector population at roughly 1/50 of episodes, so a hundred thousand
--  conversations will not blow it up.
-- ══════════════════════════════════════════════════════════

-- Vectors (HNSW, cosine distance)
CREATE INDEX idx_facts_vec       ON facts            USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_entities_vec    ON entities         USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_summaries_vec   ON summaries        USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_transcripts_vec ON blob_transcripts USING hnsw (embedding vector_cosine_ops);

-- Full text (the BM25 leg of the retrieval)
CREATE INDEX idx_episodes_tsv    ON episodes         USING gin (tsv);
CREATE INDEX idx_facts_tsv       ON facts            USING gin (tsv);
CREATE INDEX idx_summaries_tsv   ON summaries        USING gin (tsv);
CREATE INDEX idx_transcripts_tsv ON blob_transcripts USING gin (tsv);

-- Graph traversal (the prefix of idx_facts_sp already covers subject_id-only queries,
-- so no separate index)
CREATE INDEX idx_facts_object    ON facts (object_entity_id) WHERE object_entity_id IS NOT NULL;
-- Contradiction detection: look up existing facts by (subject, predicate)
CREATE INDEX idx_facts_sp        ON facts (subject_id, predicate);

-- Entity-resolution hot path: exact match on (kind, name)
CREATE INDEX idx_entities_kn     ON entities (kind, name);

-- Time recency
CREATE INDEX idx_episodes_time   ON episodes (occurred_at DESC);
CREATE INDEX idx_episodes_sess   ON episodes (session_id, occurred_at);

-- Lifecycle (aligned with the ordering of the fact_status view)
CREATE INDEX idx_fact_events     ON fact_events (fact_id, created_at DESC);

-- Redact cascade: find the derived rows from the provenance
CREATE INDEX idx_facts_source    ON facts (source_episode_id);

-- Backfill in batches when swapping the embedding model
CREATE INDEX idx_facts_model     ON facts (embedding_model);
CREATE INDEX idx_entities_model  ON entities (embedding_model);
CREATE INDEX idx_summaries_model ON summaries (embedding_model);

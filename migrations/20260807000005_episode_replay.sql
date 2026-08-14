-- Replay fidelity: persist "which memories were injected this turn / which tools were called"
--
-- ══════════════════════════════════════════════════════════
--  Why it has to be persisted
-- ══════════════════════════════════════════════════════════
--
-- Until now these two things lived only in the SSE stream: right after generation the
-- client could expand "why do you remember this", and after reopening the session the
-- drawer was empty -- **the same answer looked different before and after a refresh**.
-- "Provenance you can check" is this project's core selling point, and alive only in
-- memory is the same as not existing.
--
-- ══════════════════════════════════════════════════════════
--  Store fact ids, or a snapshot of what was injected? -- Decided: store ids
-- ══════════════════════════════════════════════════════════
--
-- Each road has its price:
--
-- | | store fact ids | store a snapshot of the injected text |
-- |---|---|---|
-- | space | a few dozen bytes per row | one more copy of the whole injection block, every turn |
-- | replay completeness | depends on the facts row still being there | reproduces verbatim what was on screen then |
-- | relationship with redact | **compatible** | **in conflict** |
--
-- The last row is what decides it. redact promises "the content is genuinely destroyed,
-- only a tombstone is left" (docs/memory.md §11), and the original text inside a snapshot
-- cannot be wiped -- not unless the redact executor grows one more cascade to clear this
-- table too, which is digging the very pit we have already fallen into once: the secret
-- scattered across several places. With ids, once a fact is redacted its statement turns
-- into a placeholder, the drawer follows and shows a placeholder as well -- erase in one
-- place, effective everywhere.
--
-- What is given up is **verbatim fidelity**: after a fact has been superseded, replay
-- shows the way it looks now plus an "invalidated since then" marker, not the sentence as
-- it stood at the time. That price is acceptable -- a fact's statement is itself
-- append-only (invalidation is expressed by appending to fact_events, the original row is
-- never touched), so "the way it looks now" and "the way it looked then" differ only when
-- it has been redacted, which is precisely the one occasion where we **want** them to differ.
--
-- ══════════════════════════════════════════════════════════
--  Why it is anchored on the user episode
-- ══════════════════════════════════════════════════════════
--
-- A turn has two episodes (user and assistant), and hanging the drawer on either of them
-- can be argued for. user was chosen:
--
-- 1. **It is always there.** The assistant row is deliberately not persisted when the
--    model errors out (a half-finished answer landing in memory would be extracted as
--    fact on the next turn), and if the drawer hung off that row, this turn's attribution
--    would be lost entirely -- while the turn that went wrong is exactly the turn where
--    you most need to ask "what made it answer that".
-- 2. **The injection physically sits right next to the user message** (see step 3 of
--    cortexd::live), so hanging it here is stating a fact, not making a UI decision.
--
-- Which bubble the client draws the drawer on is the client's own business; the server
-- does not decide that on its behalf.

-- ══════════════════════════════════════════════════════════
--  Memories injected this turn
-- ══════════════════════════════════════════════════════════

CREATE TABLE episode_memories (
    id          ulid        PRIMARY KEY,

    -- This turn's anchor -- the user episode, see above
    episode_id  ulid        NOT NULL REFERENCES episodes(id),
    -- The foreign key points at facts and not at active_facts: invalidated facts **must
    -- still be replayable**, and "a memory that was injected then and no longer holds" is
    -- exactly what an audit most wants to see
    fact_id     ulid        NOT NULL REFERENCES facts(id),

    -- Injection order (from 0). It is the outcome of ranking by fusion score, and replay
    -- restores the original sequence from it -- re-sorting by score will not do: score is
    -- nullable (old rows, or a fusion algorithm swapped in later)
    ordinal     INTEGER     NOT NULL CHECK (ordinal >= 0),

    -- Which legs of the retrieval hit. In a bad case this is the only thing that can
    -- pinpoint "why was this recalled at all".
    -- An array rather than a comma-separated string: channel names are enumerated values,
    -- and once they are glued into a string, counting by channel means doing string
    -- splitting inside SQL
    channels    TEXT[],
    -- The RRF fusion score. Nullable -- when the fusion algorithm changes, old rows'
    -- scores are no longer comparable, and leaving it empty beats faking one
    score       DOUBLE PRECISION,

    device_id   TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    -- Within one turn the same fact is injected only once (the retrieval result is already
    -- deduplicated). The constraint is not here to prevent a duplicate write, it is here to
    -- turn a duplicate write into an error that makes noise: a silent duplicate would put
    -- two identical memories in the drawer, and the user would simply conclude the UI is buggy
    UNIQUE (episode_id, fact_id)
);

COMMENT ON TABLE episode_memories IS
    'Which memories were injected in one turn (append-only). It keeps fact ids only and no
     snapshot -- the original text inside a snapshot cannot be wiped, which would turn the
     destruction that redact promises into a lie.';

-- Replay hot path: fetch by episode, restore in injection order
CREATE INDEX idx_episode_memories ON episode_memories (episode_id, ordinal);
-- The question asked backwards: "which answers has this memory been used in"
CREATE INDEX idx_episode_memories_fact ON episode_memories (fact_id);

-- ══════════════════════════════════════════════════════════
--  Tools called this turn
-- ══════════════════════════════════════════════════════════

CREATE TABLE episode_tool_calls (
    id          ulid        PRIMARY KEY,
    episode_id  ulid        NOT NULL REFERENCES episodes(id),

    -- Call order (from 0). Tool calls stand in causal relations to each other (list_dir
    -- first, then read_file), and replaying them out of order makes it impossible to read
    -- what the agent was doing
    ordinal     INTEGER     NOT NULL CHECK (ordinal >= 0),

    name        TEXT        NOT NULL,

    -- The file path this call touched; NULL for tools that touch no file.
    --
    -- Its own column, rather than making the client dig it out of summary: summary is
    -- natural language written for humans, its wording will change at any time, and once a
    -- client pulls the path out of it with a regex, one rewording means **silently showing
    -- the wrong file** -- no error, no crash, it just points at some other file
    path        TEXT,

    -- A one-line summary for humans ("returned 12 lines / 340 characters",
    -- "failed: path ... was denied")
    summary     TEXT        NOT NULL,
    ok          BOOLEAN     NOT NULL,

    device_id   TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    UNIQUE (episode_id, ordinal),

    -- The caps are defensive: all three columns hold model output, and going through
    -- sync_log means shipping it to every device
    CHECK (length(name) BETWEEN 1 AND 128),
    CHECK (path IS NULL OR length(path) <= 4096),
    CHECK (length(summary) <= 2048)
);

COMMENT ON TABLE episode_tool_calls IS
    'Which tools were called in one turn (append-only). path lives in a column of its own,
     so a client never has to regex a filename out of the wording of summary.';

CREATE INDEX idx_episode_tool_calls ON episode_tool_calls (episode_id, ordinal);

-- Snapshot index for the sandbox workspace.
--
-- # Why it is needed
--
-- The `/workspace` of a container sandbox is a persistent volume, and for office-type users
-- it is often the **only copy there is**. The invariant from the research (the sandbox
-- filesystem of Codex / Claude web / Devin is never the system of record -- the
-- authoritative copy lives on a git remote) does not hold for us -- so the very same
-- `rm -rf` that costs them one re-run costs us a permanent loss.
--
-- git inside the volume already catches overwrites and mistaken edits (see
-- `cortex-local/src/checkpoint.rs`), but `.git` sits on that same volume: **when the whole
-- volume goes, this is the only layer that can bring it back**.
--
-- # Why the host drives the snapshot and the index lands here
--
-- The sandbox token admits only 5 callback routes, and not one of them can reach this table
-- or the object storage. Which is to say **a compromised container cannot delete its own
-- backups** -- and that property is granted by the topology rather than by a permission
-- check, so it will not quietly stop holding the day one more route gets admitted.
--
-- # append-only
--
-- As everywhere else in this database: no UPDATE, no DELETE. Retiring old snapshots goes
-- down the purge road (triggered explicitly, confirmed twice, leaving a tombstone), not an
-- in-place delete here.
CREATE TABLE sandbox_snapshots (
    id           TEXT PRIMARY KEY,
    -- The sandbox's owner. Today one user has one sandbox, so this also identifies "which
    -- volume". Once one user can have several sandboxes, this column becomes sandbox_id and
    -- owner gets a column of its own.
    --
    -- Note that this table lives in **the user's own schema**, so owner is always the owner
    -- of that schema. It is kept so that it can be seen at a glance while investigating,
    -- not for filtering.
    owner        TEXT        NOT NULL,
    -- The content hash in the object storage (the return value of `BlobStore::put`).
    --
    -- Content addressing dedupes for free: when no file has changed between two snapshots,
    -- the second hash equals the first and the object storage does not really store another
    -- copy -- while this table still records two rows, because "when it was taken" is the
    -- only information kept here.
    blob_hash    TEXT        NOT NULL,
    -- Byte size of the tar, used for quota arithmetic and for saying something human in the UI.
    size_bytes   BIGINT      NOT NULL CHECK (size_bytes >= 0),
    taken_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- "The most recent snapshots of this owner" is the only hot query (listing snapshots,
-- taking the newest one to restore from).
CREATE INDEX sandbox_snapshots_owner_time
    ON sandbox_snapshots (owner, taken_at DESC);

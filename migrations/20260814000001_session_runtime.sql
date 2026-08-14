-- The session's **execution home**: where this conversation runs.
--
-- # Why it is needed
--
-- One and the same session runs quietly on both clients while the two point at
-- **completely different filesystems**: the web client goes to /workspace in the cloud
-- container, the desktop client goes to that local directory in workspaces.json. Nothing in
-- the UI says so, so the user has the agent write a file on client A, carries the
-- conversation on to client B, and the agent says "there is no such file" -- and it is
-- telling the truth, it has merely changed worlds.
--
-- The worse half is the desktop client: with no local binding on the session the agent
-- takes `Turn::sealed()`, which has **not one file tool, and reports no error**. All the
-- user notices is that it suddenly went stupid.
--
-- Not one of the four products studied (Claude Code / Codex / Cursor / OpenHands) allows
-- "the same session running separately on two clients" -- the execution environment is part
-- of a session's identity. This column is that fact, written down.
--
-- # Why only two values, instead of "which machine"
--
-- Writing `local:<device_id>` would require a client device identity first, and this path
-- has none: `session_events.device_id` carries **cortexd's own** stamp (`config.device_id`),
-- not that of the machine that started it.
--
-- But that identity **does not need to be introduced**: the local binding lives in
-- `workspaces.json`, which by its nature exists only on that one machine. So "does this
-- machine have a binding for this session" is itself the device check, and its criterion is
-- harder than an id -- an id will lie to you after a reinstall or a clone, a binding will not.
--
-- The price is being unable to say "on which machine", only "it is bound to a directory on
-- some other machine". If a real device identity ever arrives, this column becomes
-- `local:<id>` and the meaning is unchanged.
--
-- # Why NULL is cloud
--
-- Not one existing session has this event, and **the overwhelming majority** of them are
-- either pure chat or web sessions -- both of which ought to be cloud. The other way round
-- (NULL taken as local) would turn every old session on the desktop client into "bound to
-- some other machine", which is a wholesale invalidation conjured out of nothing.
--
-- Those old sessions on a desktop client that really are bound to a local directory get a
-- set_runtime='local' appended by the client the first time they are opened on that machine
-- (the moment at which it happens to be holding workspaces.json).

ALTER TABLE session_events ADD COLUMN runtime TEXT;

-- The old allowlist has no room for this new action. **No IF EXISTS**: if the name does not
-- match it should fail right there; the consequence of silently keeping the old constraint
-- is that this op gets rejected by the database at runtime, by which time the code has long
-- since been released
ALTER TABLE session_events DROP CONSTRAINT session_events_op_check;
ALTER TABLE session_events ADD CONSTRAINT session_events_op_check CHECK (op IN (
    'rename', 'archive', 'unarchive',
    'bind_workspace', 'unbind_workspace',
    'move_to_project', 'remove_from_project',
    'set_runtime'));

-- Each op carries only its own payload, stated in both directions (the reason given for the
-- other CHECKs in this table)
ALTER TABLE session_events ADD CONSTRAINT session_events_runtime_required
    CHECK (op <> 'set_runtime' OR runtime IS NOT NULL);
ALTER TABLE session_events ADD CONSTRAINT session_events_runtime_absent
    CHECK (op =  'set_runtime' OR runtime IS NULL);
ALTER TABLE session_events ADD CONSTRAINT session_events_runtime_value
    CHECK (runtime IS NULL OR runtime IN ('cloud', 'local'));

COMMENT ON COLUMN session_events.runtime IS
    'The payload of set_runtime: cloud (the cloud container workspace, resumable anywhere)
     or local (pinned to a local directory on one particular machine).
     NULL = this op is absent.';

-- ══════════════════════════════════════════════════════════
--  session_state -- rebuilt, with the fifth state machine added
--
--  The five dimensions remain mutually independent: after [set_runtime, archive] the
--  session both has an execution home and is archived. The last row in the whole table is
--  still not the final state of any single dimension.
-- ══════════════════════════════════════════════════════════

DROP VIEW session_state;

CREATE VIEW session_state AS
WITH known AS (
    SELECT DISTINCT session_id FROM session_events
),
last_title AS (
    SELECT DISTINCT ON (session_id) session_id, title, created_at
      FROM session_events WHERE op = 'rename'
     ORDER BY session_id, created_at DESC, id DESC
),
last_archive AS (
    SELECT DISTINCT ON (session_id) session_id, op, created_at
      FROM session_events WHERE op IN ('archive', 'unarchive')
     ORDER BY session_id, created_at DESC, id DESC
),
last_workspace AS (
    SELECT DISTINCT ON (session_id) session_id, op, workspace, created_at
      FROM session_events WHERE op IN ('bind_workspace', 'unbind_workspace')
     ORDER BY session_id, created_at DESC, id DESC
),
last_project AS (
    SELECT DISTINCT ON (session_id) session_id, op, project_id, created_at
      FROM session_events WHERE op IN ('move_to_project', 'remove_from_project')
     ORDER BY session_id, created_at DESC, id DESC
),
last_runtime AS (
    SELECT DISTINCT ON (session_id) session_id, runtime, created_at
      FROM session_events WHERE op = 'set_runtime'
     ORDER BY session_id, created_at DESC, id DESC
)
SELECT k.session_id,
       t.title,
       coalesce(a.op = 'archive', false)                            AS archived,
       CASE WHEN w.op = 'bind_workspace' THEN w.workspace END       AS workspace,
       -- A binding that points at a **deleted project** is treated as ungrouped.
       --
       -- The judgement lives in the view instead of being written out again by every
       -- caller: deleting a project deliberately does not cascade into rewriting the
       -- sessions (see the header of project_events), so a "dangling project_id" is the
       -- normal case rather than an anomaly.
       CASE WHEN p.op = 'move_to_project'
             AND EXISTS (SELECT 1 FROM project_state ps
                          WHERE ps.project_id = p.project_id)
            THEN p.project_id END                                   AS project_id,
       -- **The default is nailed down in the view, not left to each caller.** Left to the
       -- callers, "does NULL count as cloud or as local" gets decided all over again in
       -- cortexd, in cortex-local and in the two clients, and getting it backwards in any
       -- one of them means "every old session stops working"
       coalesce(r.runtime, 'cloud')                                 AS runtime,
       -- GREATEST ignores NULL in Postgres, so this is NULL only when all five dimensions
       -- have no events at all -- and such a session never appears in known
       greatest(t.created_at, a.created_at, w.created_at,
                p.created_at, r.created_at)                         AS decided_at
  FROM known k
  LEFT JOIN last_title     t ON t.session_id = k.session_id
  LEFT JOIN last_archive   a ON a.session_id = k.session_id
  LEFT JOIN last_workspace w ON w.session_id = k.session_id
  LEFT JOIN last_project   p ON p.session_id = k.session_id
  LEFT JOIN last_runtime   r ON r.session_id = k.session_id;

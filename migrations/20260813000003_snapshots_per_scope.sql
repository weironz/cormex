-- Snapshots are indexed by **sandbox scope**, no longer by owner.
--
-- # Why the change comes now
--
-- The comments on the previous table (20260813000002) already wrote this day down:
-- "Today one user has one sandbox, so owner also identifies 'which volume'. Once one user
--  can have several sandboxes, this column becomes sandbox_id and owner gets a column of
--  its own."
--
-- Once containers and volumes were split up per project, one user holding several volumes
-- at once became the normal case. Without this column, "list snapshots" mixes the snapshots
-- of three projects together, and "restore" unpacks project A's tar into project B's
-- workspace -- neither of which raises an error, the files are simply the wrong ones.
--
-- # Why a new column rather than redefining what owner means
--
-- owner is still "the owner of this schema", visible at a glance while investigating.
-- Quietly turning it into a holder of scope keys would make one column mean different
-- things in old and new rows, with nothing written down anywhere about where the dividing
-- line falls.
--
-- # Backfilling it as owner is **correct**, not a compromise
--
-- For an ungrouped session the scope key is identically owner (see `SandboxScope::key`) --
-- and every existing snapshot comes from that era, so `scope = owner` is their real scope.
ALTER TABLE sandbox_snapshots ADD COLUMN scope TEXT;

UPDATE sandbox_snapshots SET scope = owner WHERE scope IS NULL;

-- Tighten it only after the backfill. The other way round, the existing rows would make the
-- ALTER fail on the spot
ALTER TABLE sandbox_snapshots ALTER COLUMN scope SET NOT NULL;

COMMENT ON COLUMN sandbox_snapshots.scope IS
    'Which sandbox this snapshot belongs to (SandboxScope::key: the user id, or
     "user id--p-project id"). When ungrouped it is identically owner, so backfilling the
     existing rows as owner gives them their real scope.';

-- The hot query goes from "the most recent of this owner" to "the most recent of this scope".
-- Keeping the old index would be pointless: not one path is left that looks snapshots up by owner
DROP INDEX sandbox_snapshots_owner_time;
CREATE INDEX sandbox_snapshots_scope_time
    ON sandbox_snapshots (scope, taken_at DESC);

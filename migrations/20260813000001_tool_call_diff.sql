-- Tool calls record "what this write actually changed".
--
-- Why it goes into the database: without it, reopening the session leaves nothing but one
-- line, `write_file  pubspec.yaml` -- and the tool row is precisely the thing people come
-- back to look at afterwards. This is the same claim the R1-R5 batch made about "replay
-- fidelity": whatever was visible on screen at the time has to be visible in replay too.
--
-- Only write_file fills it in: that is the one tool that holds the old content and the new
-- content at the same time. What a file looks like after a shell command has run is
-- something the agent has no idea about.
ALTER TABLE episode_tool_calls ADD COLUMN diff TEXT;

-- An 8 KiB cap -- the same class of defence as the 2048 on summary, only at a different
-- order of magnitude.
--
-- This column goes **out to every device** through `sync_log`: one refactor that touches a
-- few hundred lines easily makes tens of KB of complete diff, multiplied by the number of
-- devices and by several write_file calls per turn -- which is traffic on the sync channel
-- that nobody ever budgeted for.
--
-- The real truncation is already done on the agent side (the twin line-count and
-- character-count caps in `cortex_agent::diff`), where it can also write "the rest is not
-- shown" at the end. The one in the database is the last fence: what it stops is not the
-- normal path but "the day someone bypasses the agent and stuffs a row straight in".
ALTER TABLE episode_tool_calls
    ADD CONSTRAINT episode_tool_calls_diff_len
    CHECK (diff IS NULL OR length(diff) <= 8192);

COMMENT ON COLUMN episode_tool_calls.diff IS
    'The unified diff of this write (already truncated, see cortex_agent::diff).
     NULL = this tool has no change worth looking at: tools that touch no file, writes whose
     content did not change by a single character, and the time the write failed.';

-- active_facts was blocking every index on facts -- the HNSW above all
--
-- ── The lesion ──
--
-- The original definition was `facts LEFT JOIN fact_status`, and fact_status is a
-- DISTINCT ON over fact_events. The planner has to **finish the join** before it can sort
-- by distance, so `ORDER BY embedding <=> $1 LIMIT 24` gets handed a pile of rows that
-- have already been materialized, and idx_facts_vec was never used once. EXPLAIN
-- (50,000 facts):
--
--   Limit                                    ← 289.7 ms, 198k buffer accesses
--     Sort (top-N heapsort)
--       Hash Left Join  rows=43842           ← 43.8k 1024-dimensional distances computed
--         Seq Scan on facts        rows=50092
--         Hash ← Seq Scan on fact_events
--
-- On a small database it is entirely invisible, which is how this hole survived from the
-- init migration onwards. The moment it does surface is precisely the day "the user
-- imports several years of history".
--
-- ── Why switching to a two-level anti-join fixes it ──
--
-- The decisive part is **not** NOT EXISTS itself but the **absence of a LIMIT**: a
-- correlated subquery with LIMIT 1 is pinned down into a row-by-row subplan -- blazing
-- with few rows, catastrophic with many. Written as a two-level anti-join, the planner
-- gets choices: few rows take a nested loop + index probe, many rows take a hash anti-join.
-- All three formulations were actually measured (50k facts / 11.7k status events /
-- 12.2% already invalidated):
--
--   definition                   vector ANN  BM25 narrow  BM25 wide  full count  by subject
--   old LEFT JOIN                  289.7ms      8.1ms      46.6ms      17.3ms      8.5ms
--   NOT EXISTS + LIMIT 1             1.2ms      0.0ms     109.1ms      67.4ms      0.1ms  ← the wide query collapses
--   NOT EXISTS(fact_status)         12.5ms      4.6ms      30.4ms      10.9ms     11.3ms  ← point lookups get slower
--   two-level anti-join (this one)   0.5ms      0.0ms      25.2ms      13.1ms      0.1ms  ← wins all five
--
-- ── Strictly equivalent in meaning to the old definition ──
--
-- Old: the **last** of the {invalidate, revoke} events is an invalidate → invalid.
-- New: an invalidate exists and no revoke comes after it → invalid.
--
-- Checked ordering by ordering: invalidate only / invalidate→revoke /
-- invalidate→revoke→invalidate / revoke only / no events at all. On 50,000 rows the two
-- definitions produce row sets whose difference is 0 in both directions.
--
-- The sort key `(created_at, id)` agrees with fact_status's `ORDER BY created_at DESC,
-- id DESC` -- created_at is clock_timestamp() at microsecond resolution, and id is only a
-- deterministic tiebreaker at the extreme same instant. **The two places must be changed
-- together**, otherwise "one revoke and one invalidate inside the same microsecond" gets
-- judged in opposite ways by the two definitions.

CREATE OR REPLACE VIEW active_facts AS
SELECT f.*
FROM facts f
WHERE NOT EXISTS (
    SELECT 1
      FROM fact_events e
     WHERE e.fact_id = f.id
       AND e.op = 'invalidate'
       AND NOT EXISTS (
           SELECT 1
             FROM fact_events r
            WHERE r.fact_id = e.fact_id
              AND r.op = 'revoke'
              AND (r.created_at, r.id) > (e.created_at, e.id)
       )
);

-- A partial index dedicated to state probing.
--
-- The existing idx_fact_events (fact_id, created_at DESC) would work too, but it covers
-- **every** event (including annotations of the flag kind, which take no part in deciding
-- state). A partial index with a predicate is smaller, and once id is part of the index
-- key even the tiebreaker needs no heap fetch.
CREATE INDEX IF NOT EXISTS idx_fact_events_status
    ON fact_events (fact_id, created_at DESC, id DESC)
 WHERE op IN ('invalidate', 'revoke');

-- ⚠️ Adding a column to facts **still** requires rebuilding this view.
-- `SELECT f.*` is expanded into a fixed column list at the moment the view is created, and
-- new columns will not show up by themselves (see the identical warning in 20260807000006).
-- Switching to an anti-join changes nothing about that.

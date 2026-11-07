-- The first user message of a session, found through an index.
--
-- Why this exists: every archive list row carries a title, and a session
-- without host metadata takes it from its first non-empty user message
-- (`archive_sessions_filtered_at`, `archive_session`, and the search branch of
-- `archive_projects`). The only index that could serve that lookup was
-- (session_id, parser_version, occurred_at), so each title meant reading the
-- session's events and sorting them by seq. On a page of thirty that was the
-- bulk of the work, and it inflated the planner's estimate enough to trigger a
-- JIT compile that cost more than the query itself.
--
-- Partial on exactly the predicate those queries use, so it stays a small
-- fraction of the table.
CREATE INDEX idx_transcript_events_user_text
    ON transcript_events (session_id, seq)
 WHERE kind = 'user_message' AND text IS NOT NULL AND text <> '';

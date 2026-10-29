//! Read-only lookups over the archive tables.
//!
//! These are the reads the capture API needs **before** it enters a write
//! transaction: which file is this, which generation is current, which chunks
//! cover this byte range. They deliberately live outside [`super::ArchiveTxn`]
//! for the same reason [`crate::txn`] has no read methods — a transaction that
//! holds a lock stays short and write-only.

use chrono::{DateTime, Utc};

use crate::archive::model::{
    ArchiveHit, ArchiveSessionRow, CaptureChunk, CaptureFile, CaptureMask, ConversationHit,
    Generation, Projection, TranscriptEvent,
};
use crate::error::Result;
use crate::store::Store;

impl Store {
    /// One logical file, by the key that identifies it.
    pub async fn capture_file(
        &self,
        device_id: &str,
        agent: &str,
        logical_path: &str,
    ) -> Result<Option<CaptureFile>> {
        let row = sqlx::query_as::<_, CaptureFile>(
            "SELECT id, session_id, device_id, agent, logical_path, role, framing,
                    cwd, project_key, first_seen_at
               FROM capture_files
              WHERE device_id = $1 AND agent = $2 AND logical_path = $3",
        )
        .bind(device_id)
        .bind(agent)
        .bind(logical_path)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// Generations that have bytes the masker has not scanned yet.
    ///
    /// The todo is derived from the data rather than kept in a queue: a
    /// generation is done when its end is at or below the scanner's progress,
    /// so a restarted process loses nothing and there is no job table to fall
    /// out of step with the rows.
    pub async fn generations_needing_masks(
        &self,
        scanner: &str,
        limit: i64,
    ) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT g.id, coalesce(max(c.byte_offset + c.byte_length), 0) AS end
               FROM capture_generations g
               JOIN capture_chunks c ON c.generation_id = g.id
               LEFT JOIN LATERAL (
                    SELECT p.through_offset
                      FROM capture_mask_progress p
                     WHERE p.generation_id = g.id AND p.scanner = $1
                     ORDER BY p.created_at DESC, p.id DESC
                     LIMIT 1
               ) prog ON true
              GROUP BY g.id, prog.through_offset
             HAVING coalesce(max(c.byte_offset + c.byte_length), 0) > coalesce(prog.through_offset, 0)
              ORDER BY g.id
              LIMIT $2",
        )
        .bind(scanner)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Sessions that have bytes the projector has not projected yet.
    ///
    /// Returns `(session_id, agent)` — the agent picks the parser.
    pub async fn sessions_needing_projection(
        &self,
        parser_version_filter: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        // A session is done when every one of its generations is fully
        // projected. `through` is a JSON map from generation id to a **record**
        // — `{offset, parser}` — not a bare number: the offset alone cannot
        // resume a parser, because the parser holds a line buffer and feeding
        // it a chunk that starts mid-file fails its contiguity check.
        //
        // `-> g.id ->> 'offset'` reaches the number; `coalesce(…, 0)` treats
        // "never projected" as 0.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT DISTINCT s.id, s.agent
               FROM capture_sessions s
               JOIN capture_files f ON f.session_id = s.id
               JOIN capture_generations g ON g.file_id = f.id
               JOIN capture_chunks c ON c.generation_id = g.id
               LEFT JOIN LATERAL (
                    SELECT p.through
                      FROM transcript_projections p
                     WHERE p.session_id = s.id AND p.parser_version = $1
                     ORDER BY p.created_at DESC, p.id DESC
                     LIMIT 1
               ) prog ON true
              GROUP BY s.id, s.agent, g.id, prog.through
             HAVING coalesce(max(c.byte_offset + c.byte_length), 0)
                    > coalesce(
                        ((prog.through -> g.id::text) ->> 'offset')::bigint,
                        0
                      )
              ORDER BY s.id
              LIMIT $2",
        )
        .bind(parser_version_filter)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The archive's session list, filtered and paginated.
    ///
    /// # Why the filters are in SQL
    ///
    /// Same reason as `session_digests`: take `limit` rows first and filter in
    /// memory, and the filtered-out ones **consume the quota** — filter by
    /// agent and the list shows fewer sessions than it should, with nothing
    /// saying why.
    ///
    /// # The cursor
    ///
    /// `(last_at DESC, id DESC)`, encoded as `"<rfc3339>|<id>"`. A bare
    /// timestamp cursor repeats or skips rows when two sessions share one, and
    /// two sessions finishing in the same second is ordinary.
    pub async fn archive_sessions(
        &self,
        agent: Option<&str>,
        project_key: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        after: Option<(&DateTime<Utc>, &str)>,
        limit: i64,
    ) -> Result<Vec<ArchiveSessionRow>> {
        let rows = sqlx::query_as::<_, ArchiveSessionRow>(
            "WITH agg AS (
                 SELECT s.id, s.agent,
                        min(e.occurred_at) AS first_at,
                        max(e.occurred_at) AS last_at,
                        count(*)           AS event_count
                   FROM capture_sessions s
                   JOIN transcript_events e ON e.session_id = s.id
                  GROUP BY s.id, s.agent
             )
             SELECT a.id, a.agent, a.first_at, a.last_at, a.event_count,
                    (SELECT e.text FROM transcript_events e
                      WHERE e.session_id = a.id AND e.kind = 'user_message'
                        AND e.text IS NOT NULL AND e.text <> ''
                      ORDER BY e.seq ASC LIMIT 1) AS title_source,
                    (SELECT min(f.project_key) FROM capture_files f
                      WHERE f.session_id = a.id AND f.project_key IS NOT NULL) AS project_key,
                    (SELECT array_agg(DISTINCT f.device_id::text) FROM capture_files f
                      WHERE f.session_id = a.id) AS devices
               FROM agg a
              WHERE ($1::text IS NULL OR a.agent = $1)
                AND ($2::text IS NULL OR EXISTS (
                        SELECT 1 FROM capture_files f
                         WHERE f.session_id = a.id AND f.project_key = $2))
                AND ($3::timestamptz IS NULL OR a.last_at >= $3)
                AND ($4::timestamptz IS NULL OR a.first_at <= $4)
                AND ($5::timestamptz IS NULL OR (a.last_at, a.id) < ($5, $6))
              ORDER BY a.last_at DESC, a.id DESC
              LIMIT $7",
        )
        .bind(agent)
        .bind(project_key)
        .bind(since)
        .bind(until)
        .bind(after.map(|(t, _)| *t))
        .bind(after.map(|(_, id)| id))
        .bind(limit.clamp(1, 200))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// One archive session's aggregate, or `None` if it has no events.
    pub async fn archive_session(&self, session_id: &str) -> Result<Option<ArchiveSessionRow>> {
        let row = sqlx::query_as::<_, ArchiveSessionRow>(
            "SELECT s.id, s.agent,
                    min(e.occurred_at) AS first_at,
                    max(e.occurred_at) AS last_at,
                    count(*)           AS event_count,
                    (SELECT e.text FROM transcript_events e
                      WHERE e.session_id = s.id AND e.kind = 'user_message'
                        AND e.text IS NOT NULL AND e.text <> ''
                      ORDER BY e.seq ASC LIMIT 1) AS title_source,
                    (SELECT min(f.project_key) FROM capture_files f
                      WHERE f.session_id = s.id AND f.project_key IS NOT NULL) AS project_key,
                    (SELECT array_agg(DISTINCT f.device_id::text) FROM capture_files f
                      WHERE f.session_id = s.id) AS devices
               FROM capture_sessions s
               JOIN transcript_events e ON e.session_id = s.id
              WHERE s.id = $1
              GROUP BY s.id, s.agent",
        )
        .bind(session_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// Every file of a session, with its generations.
    pub async fn archive_files(
        &self,
        session_id: &str,
    ) -> Result<Vec<(CaptureFile, Vec<Generation>)>> {
        let files = self.capture_files_of_session(session_id).await?;
        let mut out = Vec::with_capacity(files.len());
        for f in files {
            let gens = sqlx::query_as::<_, Generation>(
                "SELECT id, file_id, ordinal, reason, created_at
                   FROM capture_generations WHERE file_id = $1 ORDER BY ordinal ASC",
            )
            .bind(&f.id)
            .fetch_all(self.pool())
            .await?;
            out.push((f, gens));
        }
        Ok(out)
    }

    /// Whether this session is Cormex's **only copy**.
    ///
    /// True when every file of the session, on every device, has `missing` as
    /// its newest lifecycle event. A file with no event at all is **not**
    /// missing: it has never been observed to disappear.
    ///
    /// # Why this is a question worth answering
    ///
    /// The host deletes its own sessions after `cleanupPeriodDays`. Once that
    /// happens Cormex is the only copy, and a user who deletes the session here
    /// is deleting the last one. The UI has to say so before the click, not
    /// after.
    pub async fn session_is_sole_copy(&self, session_id: &str) -> Result<bool> {
        let (total, missing): (i64, i64) = sqlx::query_as(
            "SELECT count(*) AS total,
                    count(*) FILTER (
                        WHERE (SELECT ev.op FROM capture_file_events ev
                                WHERE ev.file_id = f.id
                                ORDER BY ev.created_at DESC, ev.id DESC LIMIT 1) = 'missing'
                    ) AS missing
               FROM capture_files f WHERE f.session_id = $1",
        )
        .bind(session_id)
        .fetch_one(self.pool())
        .await?;
        // An empty session is not "the only copy of anything"
        Ok(total > 0 && total == missing)
    }

    /// Tool events that have no archive-index chunks yet.
    ///
    /// The todo is derived, like the masker's and the projector's: an event is
    /// done when it has at least one chunk. A restarted process loses nothing.
    pub async fn events_needing_index(&self, limit: i64) -> Result<Vec<TranscriptEvent>> {
        let rows = sqlx::query_as::<_, TranscriptEvent>(
            "SELECT e.id, e.session_id, e.parser_version, e.seq, e.kind, e.occurred_at,
                    e.content, e.text, e.tool_name, e.tool_call_id,
                    e.src_generation_id, e.src_offset, e.src_length, e.created_at
               FROM transcript_events e
              WHERE e.kind IN ('tool_call', 'tool_result')
                AND e.text IS NOT NULL AND e.text <> ''
                AND NOT EXISTS (SELECT 1 FROM archive_chunks c WHERE c.event_id = e.id)
              ORDER BY e.session_id, e.seq
              LIMIT $1",
        )
        .bind(limit.clamp(1, 2000))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The project key of a session, from any of its files.
    ///
    /// Used by the indexer to honour a project's exclusion. `None` = the
    /// session has no project, which is **not** excluded: excluding by default
    /// would mean the index silently stops covering new projects.
    pub async fn session_project_key(&self, session_id: &str) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT project_key FROM capture_files
              WHERE session_id = $1 AND project_key IS NOT NULL LIMIT 1",
        )
        .bind(session_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|(k,)| k))
    }

    /// Search the **conversation** layer: what people and agents said.
    ///
    /// Only `user_message` / `agent_message` / `plan` — **not** tool output.
    /// That is D4: a tool's output is archived and searchable through
    /// [`Self::search_archive`], but it does not enter the conversation index,
    /// because a build log would otherwise outrank the sentence somebody
    /// actually typed.
    ///
    /// Results are aggregated **per session**, taking the best-scoring event:
    /// a search that returns twelve hits from one long session buries the other
    /// sessions that matched.
    pub async fn search_conversation(
        &self,
        tsquery_input: &str,
        agent: Option<&str>,
        project_key: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<ConversationHit>> {
        if tsquery_input.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, ConversationHit>(
            "WITH scored AS (
                 SELECT e.session_id, e.id AS event_id, e.occurred_at, e.text,
                        ts_rank(e.tsv, to_tsquery('simple', $1)) AS score,
                        row_number() OVER (
                            PARTITION BY e.session_id
                            ORDER BY ts_rank(e.tsv, to_tsquery('simple', $1)) DESC,
                                     e.seq ASC
                        ) AS rn
                   FROM transcript_events e
                   JOIN capture_sessions s ON s.id = e.session_id
                  WHERE e.tsv @@ to_tsquery('simple', $1)
                    AND e.kind IN ('user_message', 'agent_message', 'plan')
                    AND ($2::text IS NULL OR s.agent = $2)
                    AND ($3::text IS NULL OR EXISTS (
                            SELECT 1 FROM capture_files f
                             WHERE f.session_id = e.session_id AND f.project_key = $3))
                    AND ($4::timestamptz IS NULL OR e.occurred_at >= $4)
                    AND ($5::timestamptz IS NULL OR e.occurred_at <= $5)
             )
             SELECT session_id, event_id, occurred_at, text, score
               FROM scored WHERE rn = 1
              ORDER BY score DESC, occurred_at DESC NULLS LAST
              LIMIT $6",
        )
        .bind(tsquery_input)
        .bind(agent)
        .bind(project_key)
        .bind(since)
        .bind(until)
        .bind(limit.clamp(1, 200))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Search the **archive** layer: everything, tool output included.
    ///
    /// This is the `scope=archive` path, and it requires `raw:read` — a tool's
    /// output is where a secret or a private document actually appears, and
    /// this is the index that reaches it.
    ///
    /// A project can be excluded from this index by
    /// [`Self::archive_index_excluded`]; the exclusion is applied here rather
    /// than at write time, so turning it back on does not require re-indexing.
    pub async fn search_archive(
        &self,
        tsquery_input: &str,
        agent: Option<&str>,
        project_key: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<ArchiveHit>> {
        if tsquery_input.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, ArchiveHit>(
            "WITH scored AS (
                 SELECT c.session_id, c.event_id, c.part, c.text,
                        e.occurred_at,
                        ts_rank(c.tsv, to_tsquery('simple', $1)) AS score,
                        row_number() OVER (
                            PARTITION BY c.session_id
                            ORDER BY ts_rank(c.tsv, to_tsquery('simple', $1)) DESC,
                                     c.part ASC
                        ) AS rn
                   FROM archive_chunks c
                   JOIN transcript_events e ON e.id = c.event_id
                   JOIN capture_sessions s ON s.id = c.session_id
                  WHERE c.tsv @@ to_tsquery('simple', $1)
                    AND ($2::text IS NULL OR s.agent = $2)
                    AND ($3::text IS NULL OR EXISTS (
                            SELECT 1 FROM capture_files f
                             WHERE f.session_id = c.session_id AND f.project_key = $3))
                    AND ($4::timestamptz IS NULL OR e.occurred_at >= $4)
                    AND ($5::timestamptz IS NULL OR e.occurred_at <= $5)
                    -- A project excluded from the archive index is skipped
                    -- here, not at write time: turning it back on must not
                    -- require re-indexing
                    AND NOT EXISTS (
                            SELECT 1 FROM capture_files f
                             WHERE f.session_id = c.session_id
                               AND f.project_key IS NOT NULL
                               AND (SELECT ev.op FROM archive_index_events ev
                                     WHERE ev.project_key = f.project_key
                                     ORDER BY ev.created_at DESC, ev.id DESC LIMIT 1) = 'exclude')
             )
             SELECT session_id, event_id, part, text, occurred_at, score
               FROM scored WHERE rn = 1
              ORDER BY score DESC, occurred_at DESC NULLS LAST
              LIMIT $6",
        )
        .bind(tsquery_input)
        .bind(agent)
        .bind(project_key)
        .bind(since)
        .bind(until)
        .bind(limit.clamp(1, 200))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// Whether a project is excluded from the archive index.
    ///
    /// The newest event wins; a project with no event is **included** (the
    /// default), because excluding by default would mean the index silently
    /// stops covering new projects.
    pub async fn archive_index_excluded(&self, project_key: &str) -> Result<bool> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT op FROM archive_index_events WHERE project_key = $1
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(project_key)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.is_some_and(|(op,)| op == "exclude"))
    }

    /// Record an exclusion or an inclusion.
    ///
    /// # Errors
    /// The write failed.
    pub async fn set_archive_index(&self, project_key: &str, excluded: bool) -> Result<()> {
        self.archive_txn(async |t| {
            t.insert_archive_index_event(&crate::archive::NewArchiveIndexEvent {
                id: cormex_core::Id::new(),
                project_key: project_key.to_owned(),
                excluded,
            })
            .await
        })
        .await
    }

    /// Every file of a session.
    ///
    /// The projector needs all of them: a session's conversation may be spread
    /// across the main transcript, a sub-agent's own file, and the host's
    /// memory pseudo-session.
    pub async fn capture_files_of_session(&self, session_id: &str) -> Result<Vec<CaptureFile>> {
        let rows = sqlx::query_as::<_, CaptureFile>(
            "SELECT id, session_id, device_id, agent, logical_path, role, framing,
                    cwd, project_key, first_seen_at
               FROM capture_files WHERE session_id = $1
              ORDER BY id",
        )
        .bind(session_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// One capture file by its id.
    ///
    /// Used by the capture API to answer "does this file belong to the device
    /// asking" before it writes anything into it.
    pub async fn capture_file_by_id(&self, file_id: &str) -> Result<Option<CaptureFile>> {
        let row = sqlx::query_as::<_, CaptureFile>(
            "SELECT id, session_id, device_id, agent, logical_path, role, framing,
                    cwd, project_key, first_seen_at
               FROM capture_files WHERE id = $1",
        )
        .bind(file_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// One generation by its id.
    pub async fn generation_by_id(&self, generation_id: &str) -> Result<Option<Generation>> {
        let row = sqlx::query_as::<_, Generation>(
            "SELECT id, file_id, ordinal, reason, created_at
               FROM capture_generations WHERE id = $1",
        )
        .bind(generation_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// The newest generation of a file, and how far it currently extends.
    ///
    /// `None` means the file has no generation at all, which the registration
    /// path guarantees cannot happen — but the signature must not claim what it
    /// cannot know.
    pub async fn latest_generation(&self, file_id: &str) -> Result<Option<(Generation, i64)>> {
        let row = sqlx::query_as::<_, Generation>(
            "SELECT id, file_id, ordinal, reason, created_at
               FROM capture_generations WHERE file_id = $1
              ORDER BY ordinal DESC LIMIT 1",
        )
        .bind(file_id)
        .fetch_optional(self.pool())
        .await?;
        let Some(generation) = row else {
            return Ok(None);
        };
        let end = self.generation_end(&generation.id).await?;
        Ok(Some((generation, end)))
    }

    /// How many logical bytes a generation holds.
    ///
    /// `max(byte_offset + byte_length)` rather than `sum(byte_length)`: the
    /// contiguity invariant makes them equal, and the max is the one that still
    /// gives a sane answer if a gap ever appears — a sum would silently report
    /// a total that no offset can be appended at.
    pub async fn generation_end(&self, generation_id: &str) -> Result<i64> {
        let (end,): (i64,) = sqlx::query_as(
            "SELECT coalesce(max(byte_offset + byte_length), 0)
               FROM capture_chunks WHERE generation_id = $1",
        )
        .bind(generation_id)
        .fetch_one(self.pool())
        .await?;
        Ok(end)
    }

    /// One page of a session's transcript events, ascending by `seq`.
    ///
    /// `after_seq` is exclusive: pass the last `seq` of the previous page. The
    /// cursor is `seq` rather than an offset because `seq` is the projection's
    /// own order, and it is unique per `(session, parser_version)` — an offset
    /// cursor would skip or repeat rows when two events share a timestamp.
    pub async fn transcript_page(
        &self,
        session_id: &str,
        parser_version: &str,
        after_seq: i64,
        limit: i64,
    ) -> Result<Vec<TranscriptEvent>> {
        let rows = sqlx::query_as::<_, TranscriptEvent>(
            "SELECT id, session_id, parser_version, seq, kind, occurred_at, content,
                    text, tool_name, tool_call_id,
                    src_generation_id, src_offset, src_length, created_at
               FROM transcript_events
              WHERE session_id = $1 AND parser_version = $2 AND seq > $3
              ORDER BY seq ASC
              LIMIT $4",
        )
        .bind(session_id)
        .bind(parser_version)
        .bind(after_seq)
        .bind(limit.clamp(1, 1000))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The newest projection for a session, or `None` if it has never been
    /// projected.
    pub async fn latest_projection(
        &self,
        session_id: &str,
        parser_version: &str,
    ) -> Result<Option<Projection>> {
        let row = sqlx::query_as::<_, Projection>(
            "SELECT id, session_id, parser_version, through, next_seq, status, error, created_at
               FROM transcript_projections
              WHERE session_id = $1 AND parser_version = $2
              ORDER BY created_at DESC, id DESC
              LIMIT 1",
        )
        .bind(session_id)
        .bind(parser_version)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// How far the masker has scanned a generation.
    ///
    /// `0` when it has never run: the scanner starts at the beginning, which is
    /// the right answer for "nothing has been masked yet".
    pub async fn mask_progress(&self, generation_id: &str, scanner: &str) -> Result<i64> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT through_offset FROM capture_mask_progress
              WHERE generation_id = $1 AND scanner = $2
              ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(generation_id)
        .bind(scanner)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map_or(0, |(n,)| n))
    }

    /// The mask intervals overlapping `[offset, offset + len)`.
    ///
    /// Used by the raw view: the bytes come from object storage unchanged, and
    /// the intervals are replaced with `[masked:<rule>]` unless the caller has
    /// `raw:read` **and** asked for `unmask=true`.
    pub async fn masks_in_range(
        &self,
        generation_id: &str,
        offset: i64,
        len: i64,
    ) -> Result<Vec<CaptureMask>> {
        let rows = sqlx::query_as::<_, CaptureMask>(
            "SELECT id, generation_id, byte_offset, byte_length, rule, scanner, created_at
               FROM capture_masks
              WHERE generation_id = $1
                AND byte_offset < $2 + $3
                AND byte_offset + byte_length > $2
              ORDER BY byte_offset ASC",
        )
        .bind(generation_id)
        .bind(offset)
        .bind(len)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// The chunks covering `[offset, offset + len)`, ascending by offset.
    ///
    /// Used to serve a byte range of the original file (the raw view, and
    /// restore). It returns whole chunks, so the caller may receive bytes on
    /// either side of the range it asked for — slicing them is the caller's
    /// job, because only the caller knows whether it wants the surrounding
    /// context.
    pub async fn chunks_overlapping(
        &self,
        generation_id: &str,
        offset: i64,
        len: i64,
    ) -> Result<Vec<CaptureChunk>> {
        let rows = sqlx::query_as::<_, CaptureChunk>(
            "SELECT id, generation_id, byte_offset, byte_length, content_sha256,
                    storage_key, encoding, observed_at, created_at
               FROM capture_chunks
              WHERE generation_id = $1
                AND byte_offset < $2 + $3
                AND byte_offset + byte_length > $2
              ORDER BY byte_offset ASC",
        )
        .bind(generation_id)
        .bind(offset)
        .bind(len)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }
}

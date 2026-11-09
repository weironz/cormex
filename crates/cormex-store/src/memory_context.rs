//! Lightweight context for displaying memory facts. No source body is returned.
use crate::{Store, error::Result};
use serde::Serialize;

#[derive(sqlx::FromRow, Serialize)]
pub struct MemoryContext {
    pub fact_id: String,
    pub subject: String,
    pub episode_id: String,
    pub session_id: String,
    pub source_role: String,
    pub extraction_unit: String,
    pub source_time: chrono::DateTime<chrono::Utc>,
    pub agent: Option<String>,
    pub chat_title: Option<String>,
    pub project_name: Option<String>,
    pub reason: Option<String>,
    pub verification: Option<String>,
}

impl Store {
    pub async fn memory_contexts(&self, ids: &[String]) -> Result<Vec<MemoryContext>> {
        Ok(sqlx::query_as(
            "SELECT f.id AS fact_id,n.name AS subject,e.id AS episode_id,e.session_id,
                 CASE WHEN e.content->>'extraction_unit'='qa_turn' THEN 'mixed' ELSE e.role END AS source_role,CASE WHEN e.content->>'extraction_unit'='qa_turn' THEN 'qa_turn' WHEN e.content ? 'transcript_event_id' THEN 'single_message' ELSE 'input_record' END AS extraction_unit,e.occurred_at AS source_time,s.agent,
                 left(m.title,160) AS chat_title,coalesce(m.project_name,m.project_id) AS project_name,
                 (SELECT reason FROM memory_qa_artifacts WHERE fact_id=f.id::text AND kind IN ('memory','confirmed') ORDER BY CASE status WHEN 'evidence_supported' THEN 3 WHEN 'user_stated' THEN 2 ELSE 1 END DESC,created_at DESC LIMIT 1) AS reason,
                 (SELECT status FROM memory_qa_artifacts WHERE fact_id=f.id::text AND kind='memory' ORDER BY CASE status WHEN 'evidence_supported' THEN 3 WHEN 'user_stated' THEN 2 ELSE 1 END DESC,created_at DESC LIMIT 1) AS verification
             FROM facts f JOIN entities n ON n.id=f.subject_id JOIN episodes e ON e.id=f.source_episode_id
             LEFT JOIN capture_sessions s ON s.id=e.session_id
             LEFT JOIN LATERAL(SELECT title,project_name,project_id FROM capture_session_ui_metadata
               WHERE session_id=e.session_id ORDER BY updated_at DESC LIMIT 1)m ON true
             WHERE f.id=ANY($1::text[]::ulid[])")
            .bind(ids).fetch_all(self.pool()).await?)
    }
}

/// One memory as `GET /memory/search` returns it, so a client renders both the
/// same way. `invalidated` is "not in `active_facts` now": a session's history
/// keeps the facts a later turn superseded, marked rather than hidden.
#[derive(sqlx::FromRow, Serialize)]
pub struct SessionMemory {
    pub id: String,
    pub statement: String,
    pub predicate: String,
    pub domain: Option<String>,
    pub confidence: f32,
    pub valid_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub source_episode_id: String,
    pub source_channel: String,
    pub trust_tier: Option<i16>,
    pub invalidated: bool,
}

/// What an id names. Sessions and memories are both ULIDs from one space, so a
/// pasted id is resolved by looking, not by its shape.
#[derive(Serialize, Debug, PartialEq)]
pub struct IdLookup {
    pub kind: &'static str,
    pub id: String,
    /// The conversation it belongs to; the session itself for `session`.
    pub session_id: Option<String>,
    pub title: Option<String>,
}

impl Store {
    /// Every memory extracted from one session, newest first. A fact belongs to
    /// the session of the episode it was extracted from (`source_episode_id`).
    pub async fn session_memories(&self, session: &str, limit: i64) -> Result<Vec<SessionMemory>> {
        Ok(sqlx::query_as(
            "SELECT f.id::text AS id,f.statement,f.predicate,f.domain,f.confidence,f.valid_at,f.created_at,
                    f.source_episode_id::text AS source_episode_id,f.source_channel,f.trust_tier,
                    NOT EXISTS(SELECT 1 FROM active_facts a WHERE a.id=f.id) AS invalidated
             FROM episodes e JOIN facts f ON f.source_episode_id=e.id
             WHERE e.session_id=$1 ORDER BY f.created_at DESC,f.id DESC LIMIT $2",
        )
        .bind(session)
        .bind(limit)
        .fetch_all(self.pool())
        .await?)
    }

    /// Resolve an id to a session, memory, summary or source episode.
    pub async fn lookup_id(&self, id: &str) -> Result<Option<IdLookup>> {
        let session: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT s.id::text,(SELECT left(m.title,160) FROM capture_session_ui_metadata m
               WHERE m.session_id=s.id ORDER BY m.updated_at DESC LIMIT 1)
             FROM capture_sessions s WHERE s.id=$1::ulid",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        if let Some((sid, title)) = session {
            return Ok(Some(IdLookup {
                kind: "session",
                id: sid.clone(),
                session_id: Some(sid),
                title,
            }));
        }
        let fact: Option<(String, String, String)> = sqlx::query_as(
            "SELECT f.id::text,e.session_id,left(f.statement,160) FROM facts f
             JOIN episodes e ON e.id=f.source_episode_id WHERE f.id=$1::ulid",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        if let Some((fid, sid, statement)) = fact {
            return Ok(Some(IdLookup {
                kind: "memory",
                id: fid,
                session_id: Some(sid),
                title: Some(statement),
            }));
        }
        let summary: Option<(String, String, String)> = sqlx::query_as(
            "SELECT id,session_id,left(title,160) FROM memory_qa_summaries WHERE id=$1
             UNION ALL SELECT id,session_id,left(title,160) FROM memory_qa_artifacts WHERE id=$1 LIMIT 1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        if let Some((aid, sid, title)) = summary {
            return Ok(Some(IdLookup {
                kind: "summary",
                id: aid,
                session_id: Some(sid),
                title: Some(title),
            }));
        }
        let episode: Option<(String, String)> =
            sqlx::query_as("SELECT id::text,session_id FROM episodes WHERE id=$1::ulid")
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        Ok(episode.map(|(eid, sid)| IdLookup {
            kind: "episode",
            id: eid,
            session_id: Some(sid),
            title: None,
        }))
    }
}

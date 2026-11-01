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

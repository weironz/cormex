//! Derived, versioned dialogue assessments, separate from durable facts.
use crate::{Store, error::Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize, sqlx::FromRow)]
pub struct MemoryArtifact {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub markdown: String,
    pub project_name: Option<String>,
    pub session_id: String,
    pub chat_title: Option<String>,
    pub status: String,
    pub source_event_ids: Value,
    pub reason: Option<String>,
    pub evidence: Value,
    pub created_at: DateTime<Utc>,
}
impl Store {
    pub async fn qa_analysis_stage(&self, id: &str) -> Result<Option<(Value, Value, bool, bool)>> {
        Ok(sqlx::query_as(
            "SELECT input,output,persisted,auto_verified FROM memory_qa_analyses WHERE id=$1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?)
    }
    pub async fn save_qa_verified(&self, id: &str, output: &Value) -> Result<()> {
        sqlx::query("UPDATE memory_qa_analyses SET output=$2,auto_verified=true WHERE id=$1")
            .bind(id)
            .bind(output)
            .execute(self.pool())
            .await?;
        Ok(())
    }
    pub async fn strongest_memory_rank(
        &self,
        subject: cormex_core::Id,
        predicate: &str,
    ) -> Result<i32> {
        Ok(sqlx::query_scalar("SELECT coalesce(max(CASE a.status WHEN 'evidence_supported' THEN 3 WHEN 'user_stated' THEN 2 ELSE 1 END),0) FROM active_facts f JOIN memory_qa_artifacts a ON a.fact_id=f.id::text AND a.kind='memory' WHERE f.subject_id=$1 AND f.predicate=$2")
         .bind(subject.to_string()).bind(predicate).fetch_one(self.pool()).await?)
    }
    #[allow(clippy::type_complexity)]
    pub async fn qa_existing_memories(
        &self,
        project: Option<&str>,
        at: DateTime<Utc>,
        keys: Option<&[String]>,
    ) -> Result<Vec<(String, String, String, String, String, String)>> {
        Ok(sqlx::query_as("SELECT DISTINCT ON(f.subject_id,a.memory_key) f.id::text,a.memory_key,n.name,left(f.statement,600),a.status,f.valid_at::text FROM active_facts f JOIN entities n ON n.id=f.subject_id JOIN LATERAL(SELECT a.memory_key,a.status,a.project_name FROM memory_qa_artifacts a JOIN memory_qa_analyses q ON q.id=a.analysis_id WHERE a.fact_id=f.id::text AND a.kind='memory' AND q.rule_version='qa@4' ORDER BY CASE a.status WHEN 'evidence_supported' THEN 3 WHEN 'user_stated' THEN 2 ELSE 1 END DESC,a.created_at DESC LIMIT 1)a ON true WHERE a.project_name IS NOT DISTINCT FROM $1::text AND f.valid_at<=$2 AND ($3::text[] IS NULL OR a.memory_key=ANY($3)) ORDER BY f.subject_id,a.memory_key,f.valid_at DESC,f.id DESC LIMIT 8")
         .bind(project).bind(at).bind(keys).fetch_all(self.pool()).await?)
    }
    pub async fn qa_delegated_scope(&self, session: &str) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memory_qa_analyses WHERE session_id=$1 AND input->>'source_mode'='delegated_task')")
         .bind(session).fetch_one(self.pool()).await?)
    }
    pub async fn qa_related_episodes(&self, episode: &str) -> Result<Vec<String>> {
        Ok(sqlx::query_scalar("SELECT a.episode_id FROM memory_qa_analyses a JOIN episodes e ON e.session_id=a.session_id WHERE e.id::text=$1 AND (e.content ? 'transcript_event_id' OR e.content->>'extraction_unit'='qa_turn') ORDER BY a.episode_id").bind(episode).fetch_all(self.pool()).await?)
    }
    pub async fn qa_sources_redacted(&self, session: &str, ids: &[String]) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memory_qa_redacted_sources WHERE event_id=ANY($2)) OR EXISTS(SELECT 1 FROM redactions WHERE target_kind='capture_session' AND target_id=$1) OR EXISTS(SELECT 1 FROM transcript_events e JOIN capture_generations g ON g.id=e.src_generation_id JOIN redactions r ON r.target_kind='capture_file' AND r.target_id=g.file_id::text WHERE e.id::text=ANY($2))")
         .bind(session).bind(ids).fetch_one(self.pool()).await?)
    }
    pub async fn clear_qa_for_episode(&self, episode: &str) -> Result<()> {
        let mut tx = self.pool().begin().await?;
        sqlx::query("INSERT INTO memory_qa_redacted_sources(event_id) SELECT content->>'transcript_event_id' FROM episodes WHERE id::text=$1 AND content ? 'transcript_event_id' ON CONFLICT DO NOTHING")
         .bind(episode).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO memory_qa_redacted_sources(event_id) SELECT v->>'id' FROM memory_qa_analyses a CROSS JOIN LATERAL jsonb_array_elements(coalesce(a.input->'messages','[]')||coalesce(a.input->'recent_context','[]')||coalesce(a.input->'evidence_records','[]')) v WHERE a.episode_id=$1 AND v->>'id' IS NOT NULL ON CONFLICT DO NOTHING")
         .bind(episode).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM memory_qa_summaries WHERE session_id IN(SELECT session_id FROM episodes WHERE id::text=$1 AND (content ? 'transcript_event_id' OR content->>'extraction_unit'='qa_turn'))")
         .bind(episode).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM memory_qa_artifacts WHERE fact_id IN(SELECT id::text FROM facts WHERE source_episode_id::text=$1)")
         .bind(episode).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM memory_qa_analyses WHERE episode_id=$1")
            .bind(episode)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn advance_qa_progress(
        &self,
        session: &str,
        version: &str,
        through: i64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO memory_qa_progress(session_id,rule_version,through_seq) VALUES($1,$2,$3) ON CONFLICT(session_id,rule_version) DO UPDATE SET through_seq=greatest(memory_qa_progress.through_seq,excluded.through_seq),retry_after=NULL,updated_at=clock_timestamp()")
   .bind(session).bind(version).bind(through).execute(self.pool()).await?;
        Ok(())
    }
    /// Where analysis of one session stands: the last analysed source event,
    /// -1 when none. The same cursor `qa_pending` compares against.
    pub async fn qa_cursor(&self, session: &str, version: &str) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT coalesce((SELECT through_seq FROM memory_qa_progress WHERE session_id=$1 AND rule_version=$2),-1)")
            .bind(session).bind(version).fetch_one(self.pool()).await?)
    }
    /// Sessions with turns not yet analysed, most recently captured first.
    ///
    /// `since` is the extraction scope: a session is in it when its last event
    /// happened at or after that time. Conversation time (`occurred_at`), not
    /// capture time: an export imported today holds conversations from years
    /// ago, and they are not "recent" for having arrived this week.
    pub async fn qa_pending(
        &self,
        version: &str,
        limit: i64,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<(String, i64)>> {
        Ok(sqlx::query_as("SELECT s.id::text,coalesce(p.through_seq,-1) FROM capture_sessions s JOIN LATERAL(SELECT seq,created_at,coalesce(occurred_at,created_at) AS at FROM transcript_events WHERE session_id=s.id ORDER BY seq DESC LIMIT 1)e ON true LEFT JOIN memory_qa_progress p ON p.session_id=s.id::text AND p.rule_version=$1 WHERE coalesce(p.through_seq,-1)<e.seq AND (p.retry_after IS NULL OR p.retry_after<clock_timestamp() OR e.created_at>p.updated_at) AND ($3::timestamptz IS NULL OR e.at>=$3) ORDER BY e.created_at DESC LIMIT $2")
   .bind(version).bind(limit.clamp(1,20)).bind(since).fetch_all(self.pool()).await?)
    }
    pub async fn defer_qa_progress(&self, session: &str, version: &str) -> Result<()> {
        sqlx::query("INSERT INTO memory_qa_progress(session_id,rule_version,through_seq,retry_after) VALUES($1,$2,-1,clock_timestamp()+interval '60 seconds') ON CONFLICT(session_id,rule_version) DO UPDATE SET retry_after=clock_timestamp()+interval '60 seconds',updated_at=clock_timestamp()")
         .bind(session).bind(version).execute(self.pool()).await?;
        Ok(())
    }
    pub async fn qa_analysis(&self, id: &str) -> Result<Option<(Value, Value, bool)>> {
        Ok(
            sqlx::query_as("SELECT input,output,persisted FROM memory_qa_analyses WHERE id=$1")
                .bind(id)
                .fetch_optional(self.pool())
                .await?,
        )
    }
    pub async fn save_qa_analysis(
        &self,
        id: &str,
        session: &str,
        version: &str,
        input: &Value,
        output: &Value,
        through: i64,
    ) -> Result<()> {
        sqlx::query("INSERT INTO memory_qa_analyses(id,session_id,episode_id,rule_version,input,output,proposed_output,through_seq) VALUES($1,$2,$3,$4,$5,$6,$6,$7) ON CONFLICT(id) DO NOTHING")
   .bind(id).bind(session).bind(id).bind(version).bind(input).bind(output).bind(through).execute(self.pool()).await?;
        Ok(())
    }
    pub async fn qa_previous_summary(
        &self,
        session: &str,
        before: i64,
        version: &str,
    ) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT output->>'summary_markdown' FROM memory_qa_analyses WHERE session_id=$1 AND rule_version=$3 AND through_seq<$2 AND persisted ORDER BY through_seq DESC LIMIT 1")
    .bind(session).bind(before).bind(version).fetch_optional(self.pool()).await?)
    }
    pub async fn qa_through(&self, session: &str, version: &str) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT coalesce(max(through_seq),-1) FROM memory_qa_analyses WHERE session_id=$1 AND rule_version=$2 AND persisted")
   .bind(session).bind(version).fetch_one(self.pool()).await?)
    }
    pub async fn memory_artifacts(
        &self,
        kind: &str,
        cursor: Option<&str>,
        limit: i64,
    ) -> Result<Vec<MemoryArtifact>> {
        let sql = if kind == "summary" {
            "SELECT id,'summary'::text kind,title,markdown,project_name,session_id,chat_title,'summary'::text status,source_event_ids,NULL::text reason,'[]'::jsonb evidence,updated_at created_at FROM (SELECT DISTINCT ON(session_id) * FROM memory_qa_summaries ORDER BY session_id,updated_at DESC,id DESC)s WHERE ($1::text IS NULL OR id<$1) ORDER BY id DESC LIMIT $2"
        } else {
            "SELECT id,kind,title,markdown,project_name,session_id,chat_title,status,source_event_ids,reason,evidence,created_at FROM memory_qa_artifacts WHERE ($1::text IS NULL OR id<$1) AND kind=$3 AND (kind!='legacy' OR status='retired') ORDER BY id DESC LIMIT $2"
        };
        let q = sqlx::query_as::<_, MemoryArtifact>(sql)
            .bind(cursor)
            .bind(limit.clamp(1, 101));
        Ok(if kind == "summary" {
            q.fetch_all(self.pool()).await?
        } else {
            q.bind(kind).fetch_all(self.pool()).await?
        })
    }
    pub async fn qa_counts(&self) -> Result<(i64, i64, i64, i64, i64)> {
        Ok(sqlx::query_as("SELECT (SELECT count(*) FROM active_facts),(SELECT count(*) FROM memory_qa_artifacts WHERE kind='suggestion'),(SELECT count(*) FROM memory_qa_artifacts WHERE kind='temporary'),(SELECT count(DISTINCT session_id) FROM memory_qa_summaries),(SELECT count(*) FROM memory_qa_artifacts WHERE kind='legacy' AND status='retired')").fetch_one(self.pool()).await?)
    }
    pub async fn associate_qa_trial(&self, trial: &str, analysis: &str) -> Result<()> {
        sqlx::query("INSERT INTO memory_qa_trial_analyses(trial_id,analysis_id) VALUES($1,$2) ON CONFLICT DO NOTHING")
    .bind(trial).bind(analysis).execute(self.pool()).await?;
        Ok(())
    }
    /// The model response is already durable before this step. Retrying a vector
    /// failure therefore reuses the assessment rather than paying to ask again.
    pub async fn finish_qa_analysis(&self, id: &str) -> Result<()> {
        let (session,episode,version,input,output,through):(String,String,String,Value,Value,i64)=sqlx::query_as("SELECT session_id,episode_id,rule_version,input,output,through_seq FROM memory_qa_analyses WHERE id=$1")
   .bind(id).fetch_one(self.pool()).await?;
        let title = input["chat_title"].as_str().unwrap_or("未命名聊天");
        let project = input["project_name"].as_str();
        let ids = Value::Array(
            input["messages"]
                .as_array()
                .unwrap_or(&Vec::new())
                .iter()
                .map(|m| m["id"].clone())
                .collect(),
        );
        let mut tx = self.pool().begin().await?;
        for (ordinal, item) in output["memories"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .enumerate()
        {
            if item["status"].as_str() != Some("retain") {
                continue;
            }
            let status = item["verification"].as_str().unwrap_or("reported");
            let statement = item["fact"]["statement"]
                .as_str()
                .unwrap_or_default()
                .trim();
            let artifact =
                cormex_core::Id::derived(Utc::now(), &format!("{id}|{ordinal}")).to_string();
            sqlx::query("INSERT INTO memory_qa_artifacts(id,analysis_id,kind,title,markdown,project_name,session_id,chat_title,status,source_event_ids,reason,evidence,fact_id,ordinal,memory_key) VALUES($1,$2,'memory',$4,$5,$6,$7,$8,$3,$9,$10,$11,coalesce((SELECT id::text FROM facts WHERE source_episode_id::text=$12 AND statement=$5 LIMIT 1),(SELECT f.id::text FROM active_facts f JOIN entities n ON n.id=f.subject_id WHERE lower(n.name)=lower($4) AND f.predicate='memory/'||$14 AND f.valid_at<=$15 ORDER BY f.created_at DESC LIMIT 1)),$13,$14) ON CONFLICT(analysis_id,ordinal) DO NOTHING")
    .bind(artifact).bind(id).bind(status).bind(item["fact"]["subject"]["name"].as_str().unwrap_or("待核对内容"))
    .bind(statement).bind(project).bind(&session).bind(title).bind(&ids).bind(item["reason"].as_str())
    .bind(&item["evidence"]).bind(&episode).bind(ordinal as i32).bind(item["memory_key"].as_str())
    .bind(chrono::DateTime::parse_from_rfc3339(input["messages"].as_array().and_then(|m|m.last()).and_then(|m|m["occurred_at"].as_str()).unwrap_or_default()).ok().map(|d|d.with_timezone(&Utc)))
    .execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO memory_qa_summaries(id,session_id,rule_version,title,markdown,project_name,chat_title,through_seq,source_event_ids) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(session_id,rule_version) DO UPDATE SET title=excluded.title,markdown=excluded.markdown,through_seq=excluded.through_seq,source_event_ids=(SELECT jsonb_agg(DISTINCT value) FROM jsonb_array_elements(memory_qa_summaries.source_event_ids||excluded.source_event_ids)),updated_at=clock_timestamp() WHERE excluded.through_seq>=memory_qa_summaries.through_seq")
   .bind(cormex_core::Id::new().to_string()).bind(&session).bind(&version).bind(output["summary_title"].as_str().unwrap_or(title))
   .bind(output["summary_markdown"].as_str().unwrap_or_default()).bind(project).bind(title).bind(through).bind(ids).execute(&mut *tx).await?;
        sqlx::query("UPDATE memory_qa_analyses SET persisted=true WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
}

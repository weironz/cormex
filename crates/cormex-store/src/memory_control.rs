//! Account preferences and derived health for archive-to-memory extraction.
use crate::{Store, error::Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Clone, Serialize, sqlx::FromRow)]
pub struct MemoryExtractionTrial {
    pub id: String,
    pub status: String,
    pub selections: serde_json::Value,
    #[serde(skip_serializing)]
    pub event_ids: serde_json::Value,
    pub total_messages: i32,
    pub processed_messages: i32,
    pub processed_chats: i32,
    pub facts_created: i64,
    pub memory_count: i64,
    pub confirmed_count: i64,
    pub suggestion_count: i64,
    pub temporary_count: i64,
    pub summary_count: i64,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Clone, sqlx::FromRow)]
pub struct MemoryExtractionSettings {
    pub id: String,
    pub enabled: bool,
    pub use_server_model: bool,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub ciphertext: Option<Vec<u8>>,
    pub key_tail: Option<String>,
    /// Full extraction only takes sessions whose conversation was active at or
    /// after this time. `None` is no boundary.
    pub since: Option<DateTime<Utc>>,
}

#[derive(sqlx::FromRow)]
pub struct MemoryExtractionWork {
    pub running: bool,
    pub last_error: Option<String>,
    pub updated_at: DateTime<Utc>,
}

impl Store {
    pub async fn latest_memory_trial(&self) -> Result<Option<MemoryExtractionTrial>> {
        // A process restart cannot silently resume a paid job.
        sqlx::query("UPDATE memory_extraction_trials SET status='failed',last_error='任务已超时或服务已重启，请重新选择样本',updated_at=clock_timestamp() WHERE status='running' AND updated_at<now()-interval '6 minutes'").execute(self.pool()).await?;
        Ok(sqlx::query_as(
            "SELECT * FROM memory_extraction_trials ORDER BY created_at DESC,id DESC LIMIT 1",
        )
        .fetch_optional(self.pool())
        .await?)
    }

    /// Claim the single explicit-job slot.
    ///
    /// `memory_extraction_one_trial` is a partial unique index over *all*
    /// running rows. Checking only the newest row before inserting lets an
    /// older running job block the INSERT with an opaque database error; asking
    /// the index directly makes "who holds the slot" a single, honest answer.
    pub async fn start_memory_trial(
        &self,
        id: &str,
        selections: &serde_json::Value,
        events: &serde_json::Value,
        total: i32,
    ) -> Result<bool> {
        let claimed: Option<String> = sqlx::query_scalar(
            "INSERT INTO memory_extraction_trials(id,status,selections,event_ids,total_messages) \
             VALUES($1,'running',$2,$3,$4) ON CONFLICT DO NOTHING RETURNING id",
        )
        .bind(id)
        .bind(selections)
        .bind(events)
        .bind(total)
        .fetch_optional(self.pool())
        .await?;
        Ok(claimed.is_some())
    }

    /// Whether *any* job holds the slot, not just the newest one.
    pub async fn memory_trial_any_running(&self) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM memory_extraction_trials WHERE status='running')",
        )
        .fetch_one(self.pool())
        .await?)
    }

    /// Forget a session's extraction cursor so its turns can be analysed again.
    ///
    /// Used only when a session was fully analysed but yielded no memory at
    /// all: without this the user is stuck on "已全部分析" with zero results
    /// and no button to retry.
    pub async fn reset_qa_cursor(&self, session: &str, version: &str) -> Result<()> {
        sqlx::query("DELETE FROM memory_qa_progress WHERE session_id=$1 AND rule_version=$2")
            .bind(session)
            .bind(version)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    pub async fn memory_trial_running(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM memory_extraction_trials WHERE id=$1 AND status='running')").bind(id).fetch_one(self.pool()).await?)
    }

    pub async fn advance_memory_trial(&self, id: &str, messages: i32, chats: i32) -> Result<()> {
        sqlx::query("UPDATE memory_extraction_trials j SET processed_messages=$2,processed_chats=$3,facts_created=(SELECT count(*) FROM facts f JOIN memory_qa_analyses a ON a.episode_id=f.source_episode_id JOIN memory_qa_trial_analyses x ON x.analysis_id=a.id WHERE x.trial_id=j.id),confirmed_count=(SELECT count(*) FROM memory_qa_artifacts a JOIN memory_qa_trial_analyses x ON x.analysis_id=a.analysis_id WHERE x.trial_id=j.id AND a.kind='confirmed'),suggestion_count=(SELECT count(*) FROM memory_qa_artifacts a JOIN memory_qa_trial_analyses x ON x.analysis_id=a.analysis_id WHERE x.trial_id=j.id AND a.kind='suggestion'),temporary_count=(SELECT count(*) FROM memory_qa_artifacts a JOIN memory_qa_trial_analyses x ON x.analysis_id=a.analysis_id WHERE x.trial_id=j.id AND a.kind='temporary'),summary_count=(SELECT count(DISTINCT a.session_id) FROM memory_qa_analyses a JOIN memory_qa_trial_analyses x ON x.analysis_id=a.id WHERE x.trial_id=j.id AND a.persisted),updated_at=clock_timestamp() WHERE j.id=$1")
            .bind(id).bind(messages).bind(chats).execute(self.pool()).await?;
        sqlx::query("UPDATE memory_extraction_trials j SET memory_count=(SELECT count(*) FROM memory_qa_artifacts a JOIN memory_qa_trial_analyses x ON x.analysis_id=a.analysis_id WHERE x.trial_id=j.id AND a.kind='memory') WHERE j.id=$1")
            .bind(id).execute(self.pool()).await?;
        Ok(())
    }

    pub async fn finish_memory_trial(
        &self,
        id: &str,
        status: &str,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE memory_extraction_trials SET status=$2,last_error=$3,updated_at=clock_timestamp() WHERE id=$1 AND status='running'")
            .bind(id).bind(status).bind(error).execute(self.pool()).await?;
        Ok(())
    }
    pub async fn memory_extraction_settings(&self) -> Result<Option<MemoryExtractionSettings>> {
        Ok(sqlx::query_as("SELECT id,enabled,use_server_model,provider,model,base_url,ciphertext,key_tail,since FROM memory_extraction_settings ORDER BY created_at DESC,id DESC LIMIT 1").fetch_optional(self.pool()).await?)
    }

    pub async fn save_memory_extraction_settings(
        &self,
        s: &MemoryExtractionSettings,
    ) -> Result<()> {
        sqlx::query("INSERT INTO memory_extraction_settings(id,enabled,use_server_model,provider,model,base_url,ciphertext,key_tail,since) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)")
            .bind(&s.id).bind(s.enabled).bind(s.use_server_model).bind(&s.provider).bind(&s.model).bind(&s.base_url).bind(&s.ciphertext).bind(&s.key_tail).bind(s.since).execute(self.pool()).await?;
        // Saving is also an explicit retry; it releases the failure backoff.
        self.set_memory_extraction_work(false, None).await
    }

    pub async fn memory_extraction_work(&self) -> Result<Option<MemoryExtractionWork>> {
        Ok(sqlx::query_as(
            "SELECT running,last_error,updated_at FROM memory_extraction_work WHERE singleton",
        )
        .fetch_optional(self.pool())
        .await?)
    }

    pub async fn set_memory_extraction_work(
        &self,
        running: bool,
        error: Option<&str>,
    ) -> Result<()> {
        sqlx::query("INSERT INTO memory_extraction_work(singleton,running,last_error) VALUES(true,$1,$2) ON CONFLICT(singleton) DO UPDATE SET running=excluded.running,last_error=excluded.last_error,updated_at=clock_timestamp()")
            .bind(running).bind(error).execute(self.pool()).await?;
        Ok(())
    }

    /// Totals use index lookups for last seq, not a scan of every transcript.
    /// A chat with no usable text can finish without producing any facts.
    ///
    /// With `since`, only sessions inside the extraction scope count: "3 of 40
    /// checked" for the last week is the honest progress, "3 of 4510" is not.
    pub async fn memory_extraction_totals(
        &self,
        since: Option<DateTime<Utc>>,
    ) -> Result<(i64, i64, i64)> {
        let (total,done): (i64,i64)=sqlx::query_as("SELECT count(*),count(*) FILTER(WHERE coalesce(p.through_seq,-1)>=e.seq) FROM capture_sessions s JOIN LATERAL(SELECT seq,coalesce(occurred_at,created_at) AS at FROM transcript_events WHERE session_id=s.id ORDER BY seq DESC LIMIT 1) e ON true LEFT JOIN memory_qa_progress p ON p.session_id=s.id::text AND p.rule_version='qa@4' WHERE $1::timestamptz IS NULL OR e.at>=$1").bind(since).fetch_one(self.pool()).await?;
        let facts: i64 = sqlx::query_scalar("SELECT count(*) FROM facts")
            .fetch_one(self.pool())
            .await?;
        Ok((total, done, facts))
    }
}

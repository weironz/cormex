//! Explicit, finite extraction jobs. They never turn on the global worker.
use crate::{principal::Principal, routes::ApiError, state::AppState};
use axum::{Extension, Json, extract::State};
use cormex_core::Id;
use cormex_store::memory_control::MemoryExtractionTrial;
use cormex_store::{Store, TranscriptEvent};
use serde::Deserialize;

const MAX_MESSAGES: usize = 8;
const MAX_CHARS: usize = 24_000;

pub(crate) async fn embedding_ready(st: &AppState) -> bool {
    let Some(live) = st.live() else {
        return false;
    };
    matches!(tokio::time::timeout(std::time::Duration::from_secs(8),live.embedder().embed_one("Cormex memory readiness probe")).await,Ok(Ok(v)) if v.len()==cormex_memory::embed::EMBEDDING_DIM)
}

pub async fn dependencies(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::memory_settings::require_user(&p)?;
    Ok(Json(
        serde_json::json!({"embedding_ready":embedding_ready(&st).await,
        "embedding_model":st.live().map(|l|l.embedder().model_id())}),
    ))
}
struct ChatPlan {
    id: String,
    row: cormex_store::ArchiveSessionRow,
    events: Vec<TranscriptEvent>,
    through: i64,
}

#[derive(Deserialize)]
pub struct TrialRequest {
    pub session_ids: Vec<String>,
}
fn db_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(e.to_string())
}

pub async fn start(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(req): Json<TrialRequest>,
) -> Result<Json<MemoryExtractionTrial>, ApiError> {
    crate::memory_settings::require_user(&p)?;
    if req.session_ids.is_empty() || req.session_ids.len() > 3 {
        return Err(ApiError::bad_request("小样本请选择 1–3 个短聊天"));
    }
    if req
        .session_ids
        .iter()
        .collect::<std::collections::HashSet<_>>()
        .len()
        != req.session_ids.len()
    {
        return Err(ApiError::bad_request("不能重复选择同一个聊天"));
    }
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    let settings = store
        .memory_extraction_settings()
        .await
        .map_err(db_error)?
        .unwrap_or_else(|| crate::memory_settings::defaults(&st));
    if settings.enabled {
        return Err(ApiError::bad_request(
            "请先关闭全量自动提取，再启动小样本验证",
        ));
    }
    if store
        .latest_memory_trial()
        .await
        .map_err(db_error)?
        .is_some_and(|j| j.status == "running")
    {
        return Err(ApiError::bad_request("已有小样本任务正在运行"));
    }
    // Building a client validates configuration without sending chat history.
    let llm = crate::memory_settings::client(&st, &settings)
        .map_err(|_| ApiError::bad_request("请先在设置页配置可用模型"))?;
    if !embedding_ready(&st).await {
        return Err(ApiError::bad_request(
            "向量服务不可用，尚未启动试提取，也未发送聊天；请在设置页检查向量服务状态",
        ));
    }
    let mut plan = Vec::new();
    let mut selections = Vec::new();
    let mut event_ids = Vec::new();
    let mut chars = 0;
    for id in &req.session_ids {
        let (row, events, through) =
            crate::archive_api::memory_trial_events(&st, store, id).await?;
        let events = events
            .into_iter()
            .filter(|e| e.text.as_deref().is_some_and(|t| !t.trim().is_empty()))
            .collect::<Vec<_>>();
        if events.is_empty() {
            return Err(ApiError::bad_request("所选聊天没有可提取的对话消息"));
        }
        for e in &events {
            let text = e.text.as_deref().unwrap_or_default();
            if cormex_memory::redaction::scan(text).is_some() {
                return Err(ApiError::bad_request("样本仍含凭据特征，未提交给模型"));
            }
            chars += text.chars().count();
            event_ids.push(e.id.clone());
        }
        selections.push(serde_json::json!({"session_id":id,"agent":row.agent,
            "title":row.original_title.clone().or(row.title_source.clone()).unwrap_or_else(||"未命名聊天".into()).chars().take(120).collect::<String>(),
            "message_count":events.len()}));
        plan.push(ChatPlan {
            id: id.clone(),
            row,
            events,
            through,
        });
    }
    if event_ids.len() > MAX_MESSAGES || chars > MAX_CHARS {
        return Err(ApiError::bad_request(
            "小样本最多 8 条对话消息、24000 字符，请选择更短的聊天",
        ));
    }
    let id = Id::new().to_string();
    if store
        .memory_extraction_settings()
        .await
        .map_err(db_error)?
        .is_none()
    {
        store
            .save_memory_extraction_settings(&settings)
            .await
            .map_err(db_error)?;
    }
    store
        .start_memory_trial(
            &id,
            &selections.into(),
            &serde_json::to_value(&event_ids).map_err(db_error)?,
            event_ids.len() as i32,
        )
        .await
        .map_err(|_| ApiError::bad_request("无法启动小样本任务，请确认没有其他任务运行"))?;
    let result = store
        .latest_memory_trial()
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::internal("trial missing"))?;
    let store = store.clone();
    let revision = settings.id;
    tokio::spawn(async move {
        let Some(live) = st.live() else {
            let _ = store
                .finish_memory_trial(&id, "failed", Some("模型服务不可用"))
                .await;
            return;
        };
        let (base, gate) = live.extractor();
        let extractor = base.clone().with_llm(llm.clone());
        let analyzer = cormex_memory::dialogue::ModelDialogueAnalyzer(llm);
        let model_key = format!(
            "{}:{}:{}",
            settings.provider,
            settings.model,
            settings.base_url.as_deref().unwrap_or_default()
        );
        let run = run(
            &store, &id, &revision, &plan, &analyzer, &extractor, gate, &model_key,
        );
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(300), run).await;
        match outcome {
            Ok(Ok(())) => {
                let _ = store.finish_memory_trial(&id, "completed", None).await;
            }
            _ => {
                let _ = store
                    .finish_memory_trial(
                        &id,
                        "failed",
                        Some("试提取未完成；未启动全量提取，请检查模型或重新选择样本"),
                    )
                    .await;
            }
        }
    });
    Ok(Json(result))
}

#[allow(clippy::too_many_arguments)] // Explicit, testable model/gate dependencies.
async fn run(
    store: &Store,
    id: &str,
    revision: &str,
    plan: &[ChatPlan],
    analyzer: &dyn cormex_memory::dialogue::DialogueAnalyzer,
    extractor: &cormex_memory::Extractor,
    gate: &tokio::sync::Semaphore,
    model_key: &str,
) -> anyhow::Result<()> {
    let mut messages = 0;
    let mut chats = 0;
    for chat in plan {
        let turns = cormex_memory::dialogue::qa_turns(
            &chat
                .events
                .iter()
                .map(crate::memory_qa::message)
                .collect::<Vec<_>>(),
        );
        for (i, turn) in turns.iter().enumerate() {
            let current = store.memory_extraction_settings().await?;
            if !store.memory_trial_running(id).await?
                || current
                    .as_ref()
                    .is_none_or(|s| s.enabled || s.id != revision)
            {
                anyhow::bail!("trial stopped or model configuration changed");
            }
            let _permit = gate.acquire().await?;
            tokio::time::timeout(
                std::time::Duration::from_secs(90),
                crate::memory_qa::process(
                    store,
                    crate::memory_qa::input(&chat.row, &turns, i),
                    model_key,
                    analyzer,
                    extractor,
                    Some(id),
                ),
            )
            .await??;
            messages += turn.len() as i32;
            store.advance_memory_trial(id, messages, chats).await?;
        }
        crate::memory_qa::retire_legacy(store, &chat.id).await?;
        store
            .advance_qa_progress(
                &chat.id,
                cormex_memory::dialogue::RULE_VERSION,
                chat.through,
            )
            .await?;
        // Every visible message in this short chat was processed. Future normal
        // extraction can continue at the same source cursor without replaying it.
        store
            .archive_txn(async |t| t.insert_promote_progress(&chat.id, chat.through).await)
            .await?;
        chats += 1;
        store.advance_memory_trial(id, messages, chats).await?;
    }
    Ok(())
}

pub async fn cancel(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::memory_settings::require_user(&p)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    if let Some(trial) = store.latest_memory_trial().await.map_err(db_error)? {
        store
            .finish_memory_trial(
                &trial.id,
                "cancelled",
                Some("已停止；已发出的模型请求可能继续完成"),
            )
            .await
            .map_err(db_error)?;
    }
    Ok(Json(serde_json::json!({"ok":true})))
}

/// Suggestions only; starting a job still validates the complete conversation.
pub async fn candidates(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::memory_settings::require_user(&p)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    let rows = store
        .archive_sessions(None, None, None, None, None, 200)
        .await
        .map_err(db_error)?;
    let candidates=rows.into_iter().filter(|r|r.message_count.is_some_and(|n|(1..=6).contains(&n))&&r.event_count<=200)
        .take(24).map(|r|serde_json::json!({"id":r.id,"agent":r.agent,
            "title":r.original_title.or(r.title_source).unwrap_or_else(||"未命名聊天".into()).chars().take(120).collect::<String>(),
            "message_count":r.message_count})).collect::<Vec<_>>();
    Ok(Json(serde_json::json!({"chats":candidates})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_workers::tests::seed_projected_session;

    #[tokio::test]
    #[ignore = "needs a database"]
    async fn bounded_job_processes_only_selected_chats_and_cancellation_prevents_calls() {
        let base = crate::test_support::database_url().unwrap();
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap();
        let mut settings = crate::memory_settings::defaults(&st);
        settings.enabled = false;
        store
            .save_memory_extraction_settings(&settings)
            .await
            .unwrap();
        let body = br#"{"type":"user","message":{"role":"user","content":"selected preference"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"AI suggests Rust"}]}}
"#;
        let mut plan = Vec::new();
        let mut event_ids = Vec::new();
        for _ in 0..2 {
            let (id, _) = seed_projected_session(&st, &store, body).await;
            let (row, events, through) = crate::archive_api::memory_trial_events(&st, &store, &id)
                .await
                .unwrap();
            event_ids.extend(events.iter().map(|e| e.id.clone()));
            plan.push(ChatPlan {
                id,
                row,
                events,
                through,
            });
        }
        let (_outside, _) = seed_projected_session(&st, &store, body).await;
        struct Counter(std::sync::atomic::AtomicUsize);
        #[async_trait::async_trait]
        impl cormex_memory::dialogue::DialogueAnalyzer for Counter {
            async fn analyze(
                &self,
                input: &cormex_memory::dialogue::TurnInput,
            ) -> cormex_core::Result<cormex_memory::dialogue::TurnAnalysis> {
                assert_eq!(input.messages.len(), 2);
                assert_eq!(input.messages[0].text, "selected preference");
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(cormex_memory::dialogue::TurnAnalysis {
                    summary_title: "方案评审".into(),
                    summary_markdown: "## 背景\n讨论 Rust；尚未确认。".into(),
                    memories: vec![],
                })
            }
        }
        let sink = Counter(std::sync::atomic::AtomicUsize::new(0));
        let id = Id::new().to_string();
        store
            .start_memory_trial(
                &id,
                &serde_json::json!([]),
                &serde_json::to_value(event_ids).unwrap(),
                4,
            )
            .await
            .unwrap();
        let (extractor, gate) = st.live().unwrap().extractor();
        run(
            &store,
            &id,
            &settings.id,
            &plan,
            &sink,
            extractor,
            gate,
            "test",
        )
        .await
        .unwrap();
        store
            .finish_memory_trial(&id, "completed", None)
            .await
            .unwrap();
        let job = store.latest_memory_trial().await.unwrap().unwrap();
        assert_eq!(job.processed_messages, 4);
        assert_eq!(job.processed_chats, 2);
        assert_eq!(
            store.memory_extraction_totals(None).await.unwrap(),
            (3, 2, 0)
        );
        assert_eq!(job.summary_count, 2);
        assert_eq!(job.confirmed_count, 0);
        assert_eq!(store.qa_counts().await.unwrap(), (0, 0, 0, 2, 0));
        assert!(
            !store
                .memory_extraction_settings()
                .await
                .unwrap()
                .unwrap()
                .enabled
        );
        assert_eq!(sink.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        let mut altered = crate::memory_qa::input(
            &plan[0].row,
            &cormex_memory::dialogue::qa_turns(
                &plan[0]
                    .events
                    .iter()
                    .map(crate::memory_qa::message)
                    .collect::<Vec<_>>(),
            ),
            0,
        );
        altered.recent_context = vec![cormex_memory::dialogue::DialogueMessage {
            id: "recovered-context".into(),
            seq: -1,
            role: "assistant".into(),
            text: "A different bounded context recovered on retry".into(),
            occurred_at: String::new(),
        }];
        crate::memory_qa::process(&store, altered, "test", &sink, extractor, None)
            .await
            .unwrap();
        assert_eq!(
            sink.0.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "context recovery must not cause a second paid analysis"
        );
        let cached_job = Id::new().to_string();
        store
            .start_memory_trial(
                &cached_job,
                &serde_json::json!([]),
                &serde_json::json!([]),
                4,
            )
            .await
            .unwrap();
        run(
            &store,
            &cached_job,
            &settings.id,
            &plan,
            &sink,
            extractor,
            gate,
            "test",
        )
        .await
        .unwrap();
        store
            .finish_memory_trial(&cached_job, "completed", None)
            .await
            .unwrap();
        assert_eq!(
            sink.0.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "unchanged QA assessments are reused without another model call"
        );
        assert_eq!(store.qa_counts().await.unwrap(), (0, 0, 0, 2, 0));
        let cancelled = Id::new().to_string();
        store
            .start_memory_trial(
                &cancelled,
                &serde_json::json!([]),
                &serde_json::json!([]),
                1,
            )
            .await
            .unwrap();
        store
            .finish_memory_trial(&cancelled, "cancelled", None)
            .await
            .unwrap();
        assert!(
            run(
                &store,
                &cancelled,
                &settings.id,
                &plan,
                &sink,
                extractor,
                gate,
                "test"
            )
            .await
            .is_err()
        );
        assert_eq!(sink.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        for _ in 0..3 {
            seed_projected_session(&st, &store, body).await;
        }
        let newest = store
            .qa_pending(cormex_memory::dialogue::RULE_VERSION, 3, None)
            .await
            .unwrap();
        for (chat, _) in &newest {
            store
                .defer_qa_progress(chat, cormex_memory::dialogue::RULE_VERSION)
                .await
                .unwrap();
        }
        let pending = store
            .qa_pending(cormex_memory::dialogue::RULE_VERSION, 3, None)
            .await
            .unwrap();
        assert_eq!(
            pending.len(),
            1,
            "unanswered newest chats must not starve the older pending chat"
        );
        assert!(newest.iter().all(|(id, _)| id != &pending[0].0));
        let paired: Vec<String> =
            sqlx::query_scalar("SELECT episode_id FROM memory_qa_analyses WHERE session_id=$1")
                .bind(&plan[0].id)
                .fetch_all(store.pool())
                .await
                .unwrap();
        store.redact_episode_content(&paired[0]).await.unwrap();
        let erased = crate::memory_qa::input(
            &plan[0].row,
            &cormex_memory::dialogue::qa_turns(
                &plan[0]
                    .events
                    .iter()
                    .map(crate::memory_qa::message)
                    .collect::<Vec<_>>(),
            ),
            0,
        );
        assert!(
            crate::memory_qa::process(&store, erased, "test", &sink, extractor, None)
                .await
                .is_err()
        );
        assert_eq!(store.qa_counts().await.unwrap().3, 1);
        assert_eq!(
            sink.0.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "erased sources must not reach the model again"
        );
        db.drop().await;
    }
    #[tokio::test]
    #[ignore = "needs a database"]
    async fn semantic_validator_retry_reuses_saved_extraction() {
        use cormex_memory::dialogue::*;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let db =
            crate::test_support::FreshDb::create(&crate::test_support::database_url().unwrap())
                .await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap();
        let body=b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"SQLite\"}}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"SQLite\"}]}}\n";
        let (chat, _) = seed_projected_session(&st, &store, body).await;
        let (row, events, _) = crate::archive_api::memory_trial_events(&st, &store, &chat)
            .await
            .unwrap();
        let turns = qa_turns(
            &events
                .iter()
                .map(crate::memory_qa::message)
                .collect::<Vec<_>>(),
        );
        let input = crate::memory_qa::input(&row, &turns, 0);
        struct Retry {
            extract: AtomicUsize,
            verify: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl DialogueAnalyzer for Retry {
            async fn analyze(&self, input: &TurnInput) -> cormex_core::Result<TurnAnalysis> {
                self.extract.fetch_add(1, Ordering::SeqCst);
                Ok(TurnAnalysis {
                    summary_title: "数据库".into(),
                    summary_markdown: "## 结果\nSQLite".into(),
                    memories: vec![AssessedMemory {
                        status: "retain".into(),
                        reason: "数据库配置可复用".into(),
                        memory_key: "database/engine".into(),
                        memory_kind: "fact".into(),
                        verification: String::new(),
                        evidence: vec![Evidence {
                            message_id: input.messages[0].id.clone(),
                            role: "user".into(),
                            quote: "SQLite".into(),
                        }],
                        fact: serde_json::json!({"subject":{"name":"retry-test","kind":"project"},"predicate":"describes","object":{"text":"SQLite"},"statement":"项目使用 SQLite。"}),
                    }],
                })
            }
            async fn verify(
                &self,
                _input: &TurnInput,
                proposed: &TurnAnalysis,
            ) -> cormex_core::Result<TurnAnalysis> {
                if self.verify.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(cormex_core::CormexError::Memory(
                        "transient validator failure".into(),
                    ));
                }
                Ok(proposed.clone())
            }
        }
        let analyzer = Retry {
            extract: AtomicUsize::new(0),
            verify: AtomicUsize::new(0),
        };
        let extractor = st.live().unwrap().extractor().0;
        assert!(
            crate::memory_qa::process(&store, input.clone(), "fake", &analyzer, extractor, None)
                .await
                .is_err()
        );
        crate::memory_qa::process(&store, input.clone(), "fake", &analyzer, extractor, None)
            .await
            .unwrap();
        crate::memory_qa::process(&store, input, "fake", &analyzer, extractor, None)
            .await
            .unwrap();
        assert_eq!(analyzer.extract.load(Ordering::SeqCst), 1);
        assert_eq!(analyzer.verify.load(Ordering::SeqCst), 2);
        assert_eq!(store.qa_counts().await.unwrap().0, 1);
        db.drop().await;
    }
    #[tokio::test]
    #[ignore = "needs a database"]
    async fn automatic_memory_updates_deduplicates_and_rejects_weaker_reports() {
        use cormex_memory::dialogue::*;
        let db =
            crate::test_support::FreshDb::create(&crate::test_support::database_url().unwrap())
                .await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap();
        struct Automatic;
        #[async_trait::async_trait]
        impl DialogueAnalyzer for Automatic {
            async fn analyze(&self, input: &TurnInput) -> cormex_core::Result<TurnAnalysis> {
                let report = input.messages[0].text.starts_with("Report ");
                let msg = &input.messages[usize::from(report)];
                let value = if msg.text.contains("SQLite") {
                    "SQLite"
                } else {
                    "PostgreSQL"
                };
                Ok(TurnAnalysis {
                    summary_title: "数据库".into(),
                    summary_markdown: "## 结果\n项目数据库".into(),
                    memories: vec![AssessedMemory {
                        status: "retain".into(),
                        reason: "后续数据库兼容与连接配置会复用".into(),
                        evidence: vec![if !report
                            && input.evidence_records.iter().any(|t| t.successful)
                        {
                            let t = input
                                .evidence_records
                                .iter()
                                .find(|t| t.successful)
                                .unwrap();
                            Evidence {
                                message_id: t.id.clone(),
                                role: "tool".into(),
                                quote: t.text.clone(),
                            }
                        } else {
                            Evidence {
                                message_id: msg.id.clone(),
                                role: msg.role.clone(),
                                quote: msg.text.clone(),
                            }
                        }],
                        memory_key: "database/engine".into(),
                        memory_kind: "fact".into(),
                        verification: String::new(),
                        fact: serde_json::json!({"subject":{"name":"automatic-test","kind":"project"},"predicate":"describes","object":{"text":value},"statement":format!("项目使用 {value} 数据库。"),"confidence":0.9}),
                    }],
                })
            }
        }
        let mut upgrade_input = None;
        for (step, user) in [
            "SQLite",
            "SQLite",
            "PostgreSQL",
            "PostgreSQL",
            "Report PostgreSQL",
            "Report PostgreSQL",
        ]
        .into_iter()
        .enumerate()
        {
            let tool = if (1..=3).contains(&step) {
                format!(
                    "{}\n{}\n",
                    serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"db-check","name":"exec_command","input":{"cmd":"db engine"}}]}}),
                    serde_json::json!({"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"db-check","content":format!("Process exited with code 0\n{user}")}]}})
                )
            } else {
                String::new()
            };
            let body = format!(
                "{}\n{}{}\n",
                serde_json::json!({"type":"user","message":{"role":"user","content":user}}),
                tool,
                serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":user.replace("Report ","")}]}})
            );
            let (chat, _) = seed_projected_session(&st, &store, body.as_bytes()).await;
            let (row, events, _) = crate::archive_api::memory_trial_events(&st, &store, &chat)
                .await
                .unwrap();
            let turns = qa_turns(
                &events
                    .iter()
                    .map(crate::memory_qa::message)
                    .collect::<Vec<_>>(),
            );
            let mut input = crate::memory_qa::input(&row, &turns, 0);
            input.project_name = None;
            if step == 1 {
                upgrade_input = Some(input.clone());
            }
            if step == 5 {
                for m in &mut input.messages {
                    m.occurred_at = "2000-01-01T00:00:00Z".into();
                }
            }
            crate::memory_qa::process(
                &store,
                input,
                "fake",
                &Automatic,
                st.live().unwrap().extractor().0,
                None,
            )
            .await
            .unwrap();
        }
        let active: i64 = sqlx::query_scalar("SELECT count(*) FROM active_facts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        let history: i64 = sqlx::query_scalar("SELECT count(*) FROM facts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            (active, history),
            (1, 3),
            "new states supersede; repeats and weaker reports add no facts"
        );
        let state: String = sqlx::query_scalar("SELECT statement FROM active_facts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert!(state.contains("PostgreSQL"));
        let corrections: i64 =
            sqlx::query_scalar("SELECT count(*) FROM fact_events WHERE kind='superseded'")
                .fetch_one(store.pool())
                .await
                .unwrap();
        assert_eq!(
            corrections, 2,
            "versions are appended rather than overwriting the old fact"
        );
        let upgrade_input = upgrade_input.unwrap();
        sqlx::query("UPDATE memory_qa_analyses SET persisted=false WHERE session_id=$1")
            .bind(&upgrade_input.session_id)
            .execute(store.pool())
            .await
            .unwrap();
        crate::memory_qa::process(
            &store,
            upgrade_input,
            "fake",
            &Automatic,
            st.live().unwrap().extractor().0,
            None,
        )
        .await
        .unwrap();
        let after: i64 = sqlx::query_scalar("SELECT count(*) FROM facts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            after, 3,
            "a committed evidence upgrade must not create another version on retry"
        );
        let id: String = sqlx::query_scalar("SELECT id::text FROM active_facts")
            .fetch_one(store.pool())
            .await
            .unwrap();
        assert_eq!(
            store.memory_contexts(&[id]).await.unwrap()[0]
                .verification
                .as_deref(),
            Some("evidence_supported"),
            "older reports cannot downgrade current provenance"
        );
        db.drop().await;
    }
}

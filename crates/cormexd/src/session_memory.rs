//! Memories of one session: generate them on request, list them, and resolve
//! any id to the session or memory it names.
//!
//! The job is an explicit, bounded, paid action on one conversation. It runs
//! the automatic worker's own per-session pass (`memory_qa::extract_session`),
//! so a session extracted by the button and one extracted automatically are
//! extracted the same way, and resumes from the same cursor. It is recorded as
//! a row of `memory_extraction_trials` (with `mode: "session"` in its
//! selections): that row already carries progress, counts, cancellation and the
//! "a restart never silently resumes a paid job" rule, and keeps at most one
//! explicit job running per account.
use crate::{
    device_keys::Scope, memory_qa, principal::Principal, routes::ApiError, state::AppState,
};
use axum::{
    Extension, Json,
    extract::{Path, Query, State},
};
use cormex_core::Id;
use cormex_store::Store;
use serde::Deserialize;
use std::sync::{
    Arc,
    atomic::{AtomicI32, Ordering},
};

/// Turns one click may send to the model. A long coding session can hold
/// hundreds; a bound keeps one click's cost predictable, and the next click
/// resumes where this one stopped.
pub const MAX_TURNS: usize = 30;
/// Each turn is bounded to 90 s; this bounds the whole job.
const JOB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(MAX_TURNS as u64 * 90);

fn db_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(e.to_string())
}

fn valid_id(id: &str) -> Result<(), ApiError> {
    id.parse::<Id>()
        .map(|_| ())
        .map_err(|_| ApiError::bad_request("ID 格式不正确"))
}

fn require_read(p: &Principal) -> Result<(), ApiError> {
    if !p.has_scope(Scope::MemoryRead) || !p.has_scope(Scope::SessionRead) {
        return Err(ApiError::forbidden("需要记忆和聊天读取权限"));
    }
    Ok(())
}

/// `POST .../memory?redo=1` re-analyses a session that was fully analysed but
/// produced nothing. It costs another model call per turn, so it is explicit
/// and the button says so before it is pressed.
#[derive(Debug, Default, Deserialize)]
pub struct StartOptions {
    #[serde(default)]
    pub redo: bool,
}

/// The latest explicit job, when it was started for this session.
fn job_for<'a>(
    job: &'a Option<cormex_store::memory_control::MemoryExtractionTrial>,
    session: &str,
) -> Option<&'a cormex_store::memory_control::MemoryExtractionTrial> {
    job.as_ref().filter(|j| {
        j.selections
            .get(0)
            .is_some_and(|s| s["mode"] == "session" && s["session_id"] == session)
    })
}

/// User messages after the cursor: an upper bound on the turns left. Cheap,
/// and shown as "about"; the pass itself decides what a complete turn is.
async fn pending_turns(store: &Store, session: &str, version: &str) -> Result<i64, ApiError> {
    let cursor = store
        .qa_cursor(session, cormex_memory::dialogue::RULE_VERSION)
        .await
        .map_err(db_error)?;
    sqlx::query_scalar("SELECT count(*) FROM transcript_events WHERE session_id=$1 AND parser_version=$2 AND kind='user_message' AND seq>$3")
        .bind(session)
        .bind(version)
        .bind(cursor)
        .fetch_one(store.pool())
        .await
        .map_err(db_error)
}

/// `GET /archive/sessions/{id}/memory`: what generating would do, and the job.
pub async fn status(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_read(&p)?;
    valid_id(&id)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    let row = store
        .archive_session(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("会话不存在或尚未解析"))?;
    let supported = crate::archive_api::parser_version_for_agent(&row.agent);
    let pending = match supported {
        Some(version) => pending_turns(store, &id, version).await?,
        None => 0,
    };
    let memories: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM episodes e JOIN facts f ON f.source_episode_id=e.id WHERE e.session_id=$1",
    )
    .bind(&id)
    .fetch_one(store.pool())
    .await
    .map_err(db_error)?;
    let latest = store.latest_memory_trial().await.map_err(db_error)?;
    let other_running = latest
        .as_ref()
        .is_some_and(|j| j.status == "running" && job_for(&latest, &id).is_none());
    // The conversation's own distilled account. It is the "what was this
    // session about" answer, not the fact list; showing it here is what makes
    // a session with zero extracted facts still explain itself.
    let summary: Option<(String, String)> = sqlx::query_as(
        "SELECT title, markdown FROM memory_qa_summaries WHERE session_id=$1 \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(store.pool())
    .await
    .map_err(db_error)?;
    Ok(Json(serde_json::json!({
        "session_id": id,
        "supported": supported.is_some(),
        "pending_turns": pending,
        "max_turns_per_job": MAX_TURNS,
        "memory_count": memories,
        "job": job_for(&latest, &id),
        "other_job_running": other_running,
        "summary": summary.as_ref().map(|(t, m)| serde_json::json!({"title": t, "markdown": m})),
        "can_redo": pending == 0 && memories == 0,
    })))
}

/// `POST /archive/sessions/{id}/memory`: start generating this session's memories.
pub async fn start(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    Query(opts): Query<StartOptions>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::memory_settings::require_user(&p)?;
    valid_id(&id)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?.clone();
    let row = store
        .archive_session(&id)
        .await
        .map_err(db_error)?
        .ok_or_else(|| ApiError::not_found("会话不存在或尚未解析"))?;
    let version = crate::archive_api::parser_version_for_agent(&row.agent)
        .ok_or_else(|| ApiError::bad_request("暂不支持为这个来源的会话生成记忆"))?;
    // `latest_memory_trial` sweeps running rows older than six minutes into
    // `failed`. Ask the whole table afterwards: the unique index covers every
    // running row, and checking only the newest one used to let an older job
    // block the INSERT with a confusing "无法启动任务".
    if store.memory_trial_any_running().await.map_err(db_error)? {
        return Err(ApiError::conflict(
            "已有记忆生成任务在运行，请等它结束或先停止",
        ));
    }
    let memories: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM episodes e JOIN facts f ON f.source_episode_id=e.id WHERE e.session_id=$1",
    )
    .bind(&id)
    .fetch_one(store.pool())
    .await
    .map_err(db_error)?;
    let mut pending = pending_turns(&store, &id, version).await?;
    if pending == 0 {
        // Fully analysed with nothing to show. Without an explicit redo the
        // cursor makes this terminal: "已全部分析" and zero memories forever.
        if !opts.redo || memories > 0 {
            return Err(ApiError::bad_request(
                "这个会话已经全部分析过，没有新的对话",
            ));
        }
        store
            .reset_qa_cursor(&id, cormex_memory::dialogue::RULE_VERSION)
            .await
            .map_err(db_error)?;
        pending = pending_turns(&store, &id, version).await?;
        if pending == 0 {
            return Err(ApiError::bad_request("这个会话没有可用于生成记忆的对话"));
        }
    }
    let settings = store
        .memory_extraction_settings()
        .await
        .map_err(db_error)?
        .unwrap_or_else(|| crate::memory_settings::defaults(&st));
    // Building a client validates configuration without sending chat history.
    let llm = crate::memory_settings::client(&st, &settings)
        .map_err(|_| ApiError::bad_request("请先在设置页配置可用模型"))?;
    if !crate::memory_trial::embedding_ready(&st).await {
        return Err(ApiError::bad_request(
            "向量服务不可用，没有发送聊天；请在设置页检查向量服务状态",
        ));
    }
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
    let job = Id::new().to_string();
    let title = row
        .original_title
        .clone()
        .or(row.title_source.clone())
        .unwrap_or_else(|| "未命名聊天".into())
        .chars()
        .take(120)
        .collect::<String>();
    let selections =
        serde_json::json!([{"mode":"session","session_id":id,"agent":row.agent,"title":title}]);
    let total = pending.min(MAX_TURNS as i64) as i32;
    if !store
        .start_memory_trial(&job, &selections, &serde_json::json!([]), total)
        .await
        .map_err(db_error)?
    {
        return Err(ApiError::conflict(
            "已有另一个记忆生成任务占用了生成槽位，请稍后重试",
        ));
    }
    let started = store.latest_memory_trial().await.map_err(db_error)?;
    let revision = settings.id.clone();
    let model_key = memory_qa::model_key(&settings);
    let (session, job_id, background) = (id.clone(), job.clone(), store.clone());
    tokio::spawn(async move {
        let Some(live) = st.live().cloned() else {
            let _ = background
                .finish_memory_trial(&job_id, "failed", Some("模型服务不可用"))
                .await;
            return;
        };
        let (base, gate) = live.extractor();
        let extractor = base.clone().with_llm(llm.clone());
        let analyzer = cormex_memory::dialogue::ModelDialogueAnalyzer(llm);
        let run = run(
            &st,
            &background,
            &session,
            &job_id,
            &revision,
            &model_key,
            &analyzer,
            &extractor,
            gate,
        );
        let (status, error) = match tokio::time::timeout(JOB_TIMEOUT, run).await {
            Ok(Ok(_)) => ("completed", None),
            Ok(Err(e)) if e.to_string() == STOPPED => ("cancelled", Some("已停止")),
            Ok(Err(e)) if e.to_string() == BUSY => {
                ("failed", Some("这个会话正在被自动提取处理，稍后再试"))
            }
            _ => (
                "failed",
                Some("生成未完成，已分析的部分已保存；可以稍后继续"),
            ),
        };
        let _ = background.finish_memory_trial(&job_id, status, error).await;
    });
    Ok(Json(serde_json::json!({"job": started})))
}

const STOPPED: &str = "session memory job stopped";
const BUSY: &str = "session is being extracted by another pass";

/// Passes over the session until it is caught up, the budget is spent, or a
/// pass makes no progress (only an open tail remains). Returns turns analysed.
#[allow(clippy::too_many_arguments)] // Explicit, testable model/gate dependencies.
pub(crate) async fn run(
    st: &AppState,
    store: &Store,
    session: &str,
    job: &str,
    revision: &str,
    model_key: &str,
    analyzer: &dyn cormex_memory::dialogue::DialogueAnalyzer,
    extractor: &cormex_memory::Extractor,
    gate: &tokio::sync::Semaphore,
) -> anyhow::Result<usize> {
    let done = Arc::new(AtomicI32::new(0));
    let (check, id, revision) = (store.clone(), job.to_owned(), revision.to_owned());
    let counter = done.clone();
    // Asked before every paid call: it records progress (keeping the job's
    // heartbeat fresh) and stops on cancellation or a model change.
    let mut keep_going = move || {
        let (check, id, revision, counter) =
            (check.clone(), id.clone(), revision.clone(), counter.clone());
        Box::pin(async move {
            check
                .advance_memory_trial(&id, counter.load(Ordering::SeqCst), 0)
                .await?;
            let same_model = check
                .memory_extraction_settings()
                .await?
                .is_some_and(|s| s.id == revision);
            let running = check.memory_trial_running(&id).await?;
            if !running || !same_model {
                anyhow::bail!(STOPPED);
            }
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }) as futures::future::BoxFuture<'static, anyhow::Result<bool>>
    };
    let mut total = 0;
    loop {
        let pass = memory_qa::extract_session(
            st,
            store,
            session,
            MAX_TURNS - total,
            model_key,
            analyzer,
            extractor,
            gate,
            Some(job),
            &mut keep_going,
        )
        .await?;
        if pass.busy {
            anyhow::bail!(BUSY);
        }
        total += pass.processed;
        done.store(total as i32, Ordering::SeqCst);
        if pass.caught_up || pass.processed == 0 || total >= MAX_TURNS {
            store
                .advance_memory_trial(job, total as i32, i32::from(pass.caught_up))
                .await?;
            return Ok(total);
        }
    }
}

#[derive(Deserialize)]
pub struct ListQuery {
    pub limit: Option<i64>,
}

/// `GET /archive/sessions/{id}/memories`: the memories extracted from it.
pub async fn memories(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_read(&p)?;
    valid_id(&id)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    let facts = store
        .session_memories(&id, q.limit.unwrap_or(200).clamp(1, 500))
        .await
        .map_err(db_error)?;
    Ok(Json(serde_json::json!({"session_id": id, "facts": facts})))
}

/// `GET /lookup/{id}`: whether an id is a session, memory, summary or episode.
pub async fn lookup(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Path(id): Path<String>,
) -> Result<Json<cormex_store::memory_context::IdLookup>, ApiError> {
    require_read(&p)?;
    let id = id.trim().to_ascii_uppercase();
    valid_id(&id)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    store
        .lookup_id(&id)
        .await
        .map_err(db_error)?
        .map(Json)
        .ok_or_else(|| ApiError::not_found("没有找到这个 ID 对应的会话或记忆"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_workers::tests::seed_projected_session;
    use cormex_memory::dialogue::*;
    use std::sync::atomic::AtomicUsize;

    /// Retains one memory per turn, keyed by the user's words.
    struct OnePerTurn(AtomicUsize);
    #[async_trait::async_trait]
    impl DialogueAnalyzer for OnePerTurn {
        async fn analyze(&self, input: &TurnInput) -> cormex_core::Result<TurnAnalysis> {
            self.0.fetch_add(1, Ordering::SeqCst);
            let msg = &input.messages[0];
            Ok(TurnAnalysis {
                summary_title: "会话".into(),
                summary_markdown: format!("## 结果\n{}", msg.text),
                memories: vec![AssessedMemory {
                    status: "retain".into(),
                    reason: "后续会复用".into(),
                    memory_key: format!("topic/{}", msg.text.to_lowercase()),
                    memory_kind: "fact".into(),
                    verification: String::new(),
                    evidence: vec![Evidence {
                        message_id: msg.id.clone(),
                        role: "user".into(),
                        quote: msg.text.clone(),
                    }],
                    fact: serde_json::json!({"subject":{"name":"session-memory-test","kind":"project"},
                        "predicate":"describes","object":{"text":msg.text},
                        "statement":format!("项目选用 {}。", msg.text)}),
                }],
            })
        }
    }

    fn chat(turns: &[&str]) -> Vec<u8> {
        let mut body = String::new();
        for t in turns {
            body += &format!(
                "{}\n{}\n",
                serde_json::json!({"type":"user","message":{"role":"user","content":t}}),
                serde_json::json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":format!("好的，用 {t}")}]}})
            );
        }
        body.into_bytes()
    }

    /// A finished conversation: its last event is older than the open-tail window.
    async fn settle(store: &Store, session: &str) {
        sqlx::query("UPDATE transcript_events SET created_at=created_at-interval '10 minutes' WHERE session_id=$1")
            .bind(session)
            .execute(store.pool())
            .await
            .unwrap();
    }

    async fn job(store: &Store, session: &str) -> String {
        let id = Id::new().to_string();
        store
            .start_memory_trial(
                &id,
                &serde_json::json!([{"mode":"session","session_id":session}]),
                &serde_json::json!([]),
                2,
            )
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    #[ignore = "needs a database"]
    async fn a_session_job_extracts_that_session_maps_memories_back_and_resolves_ids() {
        let db =
            crate::test_support::FreshDb::create(&crate::test_support::database_url().unwrap())
                .await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap().clone();
        let settings = crate::memory_settings::defaults(&st);
        store
            .save_memory_extraction_settings(&settings)
            .await
            .unwrap();
        let (session, _) = seed_projected_session(&st, &store, &chat(&["Rust", "Postgres"])).await;
        let (other, _) = seed_projected_session(&st, &store, &chat(&["Go"])).await;
        settle(&store, &session).await;
        settle(&store, &other).await;
        let analyzer = OnePerTurn(AtomicUsize::new(0));
        let (extractor, gate) = st.live().unwrap().extractor();

        let id = job(&store, &session).await;
        let done = run(
            &st,
            &store,
            &session,
            &id,
            &settings.id,
            "test",
            &analyzer,
            extractor,
            gate,
        )
        .await
        .unwrap();
        store
            .finish_memory_trial(&id, "completed", None)
            .await
            .unwrap();
        assert_eq!(
            done, 2,
            "both complete turns of the selected session are analysed"
        );
        assert_eq!(
            analyzer.0.load(Ordering::SeqCst),
            2,
            "only the selected session reaches the model"
        );
        let recorded = store.latest_memory_trial().await.unwrap().unwrap();
        assert_eq!(
            (recorded.processed_messages, recorded.processed_chats),
            (2, 1)
        );
        assert!(
            recorded.memory_count >= 2,
            "the job's counts read through its analyses: {}",
            recorded.memory_count
        );

        let memories = store.session_memories(&session, 50).await.unwrap();
        assert_eq!(
            memories.len(),
            2,
            "each turn's memory maps back to its session"
        );
        assert!(memories.iter().all(|m| !m.invalidated));
        assert!(
            store.session_memories(&other, 50).await.unwrap().is_empty(),
            "the other session was not touched"
        );

        let found = store.lookup_id(&session).await.unwrap().unwrap();
        assert_eq!(
            (found.kind, found.session_id.as_deref()),
            ("session", Some(session.as_str()))
        );
        let found = store.lookup_id(&memories[0].id).await.unwrap().unwrap();
        assert_eq!(found.kind, "memory");
        assert_eq!(
            found.session_id.as_deref(),
            Some(session.as_str()),
            "a memory resolves to its session"
        );
        let found = store
            .lookup_id(&memories[0].source_episode_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (found.kind, found.session_id.as_deref()),
            ("episode", Some(session.as_str()))
        );
        assert_eq!(store.lookup_id(&Id::new().to_string()).await.unwrap(), None);

        // Caught up: a second click sends nothing
        let agent = store
            .archive_session(&session)
            .await
            .unwrap()
            .unwrap()
            .agent;
        let version = crate::archive_api::parser_version_for_agent(&agent).unwrap();
        assert_eq!(
            pending_turns(&store, &session, version).await.unwrap(),
            0,
            "the status reports nothing left"
        );
        let again = job(&store, &session).await;
        let done = run(
            &st,
            &store,
            &session,
            &again,
            &settings.id,
            "test",
            &analyzer,
            extractor,
            gate,
        )
        .await
        .unwrap();
        store
            .finish_memory_trial(&again, "completed", None)
            .await
            .unwrap();
        assert_eq!(done, 0);
        assert_eq!(
            analyzer.0.load(Ordering::SeqCst),
            2,
            "an analysed session is not paid for twice"
        );

        // Another pass holds the session: the job stops without a model call
        let mut holder = store.pool().acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock(4274, hashtext($1))")
            .bind(&other)
            .execute(&mut *holder)
            .await
            .unwrap();
        let busy = job(&store, &other).await;
        let err = run(
            &st,
            &store,
            &other,
            &busy,
            &settings.id,
            "test",
            &analyzer,
            extractor,
            gate,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), BUSY);
        sqlx::query("SELECT pg_advisory_unlock(4274, hashtext($1))")
            .bind(&other)
            .execute(&mut *holder)
            .await
            .unwrap();
        store
            .finish_memory_trial(&busy, "failed", None)
            .await
            .unwrap();

        // A cancelled job stops before its first paid call
        let cancelled = job(&store, &other).await;
        store
            .finish_memory_trial(&cancelled, "cancelled", None)
            .await
            .unwrap();
        let err = run(
            &st,
            &store,
            &other,
            &cancelled,
            &settings.id,
            "test",
            &analyzer,
            extractor,
            gate,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), STOPPED);
        assert_eq!(analyzer.0.load(Ordering::SeqCst), 2);

        // Released: the other session now runs, and the lock did not leak
        let free = job(&store, &other).await;
        assert_eq!(
            run(
                &st,
                &store,
                &other,
                &free,
                &settings.id,
                "test",
                &analyzer,
                extractor,
                gate
            )
            .await
            .unwrap(),
            1
        );
        db.drop().await;
    }

    #[tokio::test]
    #[ignore = "needs a database"]
    async fn an_open_tail_is_left_for_later() {
        let db =
            crate::test_support::FreshDb::create(&crate::test_support::database_url().unwrap())
                .await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap().clone();
        let settings = crate::memory_settings::defaults(&st);
        store
            .save_memory_extraction_settings(&settings)
            .await
            .unwrap();
        // Just captured: its last turn may still be answered further
        let (session, _) = seed_projected_session(&st, &store, &chat(&["Rust", "Postgres"])).await;
        let analyzer = OnePerTurn(AtomicUsize::new(0));
        let (extractor, gate) = st.live().unwrap().extractor();
        let id = job(&store, &session).await;
        let done = run(
            &st,
            &store,
            &session,
            &id,
            &settings.id,
            "test",
            &analyzer,
            extractor,
            gate,
        )
        .await
        .unwrap();
        assert_eq!(
            done, 1,
            "the complete first turn is analysed, the open last one is not"
        );
        db.drop().await;
    }

    /// A session analysed down to **zero** memories used to be a dead end.
    ///
    /// The cursor advanced anyway, so `start()` answered "这个会话已经全部分析过"
    /// and the panel rendered "已全部分析" with no button — the state production
    /// got stuck in. The 409s were a second half of the same bug: the slot was
    /// checked against the newest row only, while a unique index covers every
    /// running row. Both halves are pinned here, because fixing one is what
    /// left the other in place.
    #[tokio::test]
    #[ignore = "needs a database"]
    async fn a_redo_reopens_a_session_that_was_analysed_into_nothing() {
        let db =
            crate::test_support::FreshDb::create(&crate::test_support::database_url().unwrap())
                .await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let store = st.public_store().unwrap().clone();
        let (session, _) = seed_projected_session(&st, &store, &chat(&["Rust"])).await;
        settle(&store, &session).await;
        let version: String =
            sqlx::query_scalar("SELECT parser_version FROM transcript_events WHERE session_id=$1")
                .bind(&session)
                .fetch_one(store.pool())
                .await
                .unwrap();

        let selections = serde_json::json!([{"mode":"session","session_id":session}]);
        let first = Id::new().to_string();
        assert!(
            store
                .start_memory_trial(&first, &selections, &serde_json::json!([]), 1)
                .await
                .unwrap()
        );
        let second = Id::new().to_string();
        assert!(
            !store
                .start_memory_trial(&second, &selections, &serde_json::json!([]), 1)
                .await
                .unwrap(),
            "a second job is refused, not an error: the insert must not surface the index violation"
        );
        assert!(
            store.memory_trial_any_running().await.unwrap(),
            "the slot is held by whichever row is running, not only the newest"
        );
        store
            .finish_memory_trial(&first, "completed", None)
            .await
            .unwrap();
        assert!(!store.memory_trial_any_running().await.unwrap());

        // The analysis ran and kept nothing, but the cursor still moved.
        let last: i64 =
            sqlx::query_scalar("SELECT max(seq) FROM transcript_events WHERE session_id=$1")
                .bind(&session)
                .fetch_one(store.pool())
                .await
                .unwrap();
        store
            .advance_qa_progress(&session, RULE_VERSION, last)
            .await
            .unwrap();
        assert_eq!(pending_turns(&store, &session, &version).await.unwrap(), 0);

        store.reset_qa_cursor(&session, RULE_VERSION).await.unwrap();
        assert_eq!(
            pending_turns(&store, &session, &version).await.unwrap(),
            1,
            "redo gives every turn back, so the button can produce a different answer"
        );
        db.drop().await;
    }
}

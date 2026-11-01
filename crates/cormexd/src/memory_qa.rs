//! Automatic archive memory with bounded QA/tool evidence and separate summaries.
//! Never enables account extraction itself.
use crate::{device_keys::Scope, principal::Principal, routes::ApiError, state::AppState};
use axum::{
    Extension, Json,
    extract::{Query, State},
};
use cormex_core::Id;
use cormex_memory::{
    ExtractContext, Extractor,
    dialogue::{self, DialogueAnalyzer, DialogueMessage, TurnInput},
};
use cormex_store::{NewEpisode, Store, TranscriptEvent, TranscriptEventKind};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub fn message(event: &TranscriptEvent) -> DialogueMessage {
    DialogueMessage {
        id: event.id.clone(),
        seq: event.seq,
        role: if event.kind == TranscriptEventKind::UserMessage {
            "user"
        } else {
            "assistant"
        }
        .into(),
        text: event.text.clone().unwrap_or_default().trim().to_owned(),
        occurred_at: event.occurred_at.unwrap_or(event.created_at).to_rfc3339(),
    }
}
pub fn input(
    row: &cormex_store::ArchiveSessionRow,
    turns: &[Vec<DialogueMessage>],
    i: usize,
) -> TurnInput {
    TurnInput {
        session_id: row.id.clone(),
        chat_title: row
            .original_title
            .clone()
            .or_else(|| row.title_source.clone())
            .unwrap_or_else(|| "未命名聊天".into())
            .chars()
            .take(160)
            .collect(),
        project_name: row.project_name.clone(),
        source_mode: if row
            .title_source
            .as_deref()
            .is_some_and(dialogue::delegated_text)
            || turns[..=i].iter().any(|t| dialogue::delegated_task(t))
        {
            "delegated_task"
        } else {
            "conversation"
        }
        .into(),
        messages: turns[i].clone(),
        recent_context: dialogue::bounded_context(turns, i),
        previous_summary: None,
        evidence_records: Vec::new(),
        existing_memories: Vec::new(),
    }
}
/// Saves the assessment before vectors. Same source/rules/model reuses the
/// stored assessment on retry. The caller owns scope, cancellation and timeout.
pub async fn process(
    store: &Store,
    mut input: TurnInput,
    model_key: &str,
    analyzer: &dyn DialogueAnalyzer,
    extractor: &Extractor,
    trial: Option<&str>,
) -> anyhow::Result<()> {
    anyhow::ensure!(!input.messages.is_empty(), "empty QA input");
    if store.qa_delegated_scope(&input.session_id).await? {
        input.source_mode = "delegated_task".into();
    }
    let last = input.messages.last().expect("nonempty");
    let through = last.seq;
    let stamp =
        chrono::DateTime::parse_from_rfc3339(&last.occurred_at)?.with_timezone(&chrono::Utc);
    // A retry may recover a smaller context page. Key the *source turn*, then
    // reuse the saved original context and assessment together.
    let stable = serde_json::to_string(&(&input.session_id, &input.messages, &input.source_mode))?;
    let hash = format!(
        "{:x}",
        Sha256::digest(format!("{}|{}|{}", dialogue::RULE_VERSION, model_key, stable).as_bytes())
    );
    let id = Id::derived(stamp, &hash);
    let key = id.to_string();
    let cached = store.qa_analysis_stage(&key).await?;
    let legacy_hash = format!(
        "{:x}",
        Sha256::digest(format!("qa@3|{}|{}", model_key, stable).as_bytes())
    );
    let legacy = if cached.is_none() {
        store
            .qa_analysis(&Id::derived(stamp, &legacy_hash).to_string())
            .await?
    } else {
        None
    };
    let mut reused_proposal = None;
    if let Some((saved, _, _, _)) = &cached {
        input = serde_json::from_value(saved.clone())?;
    } else if let Some((saved, output, true)) = legacy {
        input = serde_json::from_value(saved)?;
        reused_proposal = Some(serde_json::from_value(output)?);
    } else {
        input.previous_summary = store
            .qa_previous_summary(
                &input.session_id,
                input.messages[0].seq,
                dialogue::RULE_VERSION,
            )
            .await?;
        if let Some(row) = store.archive_session(&input.session_id).await?
            && let Some(version) = crate::archive_api::parser_version_for_agent(&row.agent)
        {
            input.evidence_records = crate::qa_evidence::collect(
                store,
                &input.session_id,
                version,
                input.messages[0].seq,
                through,
            )
            .await?;
        }
        let at = chrono::DateTime::parse_from_rfc3339(
            &input.messages.last().expect("nonempty").occurred_at,
        )?
        .with_timezone(&chrono::Utc);
        input.existing_memories = store
            .qa_existing_memories(input.project_name.as_deref(), at, None)
            .await?
            .into_iter()
            .map(|(id, key, subject, statement, verification, occurred_at)| {
                dialogue::ExistingMemory {
                    id,
                    key,
                    subject,
                    statement,
                    verification,
                    occurred_at,
                }
            })
            .collect();
    }
    let source_ids = input
        .messages
        .iter()
        .chain(&input.recent_context)
        .map(|m| m.id.clone())
        .chain(input.evidence_records.iter().map(|t| t.id.clone()))
        .collect::<Vec<_>>();
    anyhow::ensure!(
        !store
            .qa_sources_redacted(&input.session_id, &source_ids)
            .await?,
        "QA sources were erased"
    );
    let encoded = serde_json::to_value(&input)?;
    let text = serde_json::to_string_pretty(&input)?;
    anyhow::ensure!(
        input
            .messages
            .iter()
            .map(|m| m.text.chars().count())
            .sum::<usize>()
            <= 24000,
        "QA turn too large"
    );
    anyhow::ensure!(
        cormex_memory::redaction::scan(&text).is_none(),
        "credential-shaped QA input blocked"
    );
    if cached.as_ref().is_some_and(|r| r.2 && r.3) {
        if let Some(trial) = trial {
            store.associate_qa_trial(trial, &key).await?;
        }
        return Ok(());
    }
    let already_verified = cached.as_ref().is_some_and(|r| r.3);
    let proposed = if let Some((_, output, _, _)) = cached {
        serde_json::from_value(output)?
    } else {
        let proposed = if let Some(previous) = reused_proposal {
            previous
        } else {
            analyzer.analyze(&input).await?
        };
        let keys = proposed
            .memories
            .iter()
            .filter(|m| m.status == "retain")
            .map(|m| m.memory_key.clone())
            .collect::<Vec<_>>();
        let mut validation = input.clone();
        // Lookup the proposed keys even when they were outside the prompt's
        // bounded context, so weak reports cannot silently overwrite stronger facts.
        validation.existing_memories.extend(
            store
                .qa_existing_memories(input.project_name.as_deref(), stamp, Some(&keys))
                .await?
                .into_iter()
                .map(|(id, key, subject, statement, verification, occurred_at)| {
                    dialogue::ExistingMemory {
                        id,
                        key,
                        subject,
                        statement,
                        verification,
                        occurred_at,
                    }
                }),
        );
        let value = dialogue::validate_analysis(proposed, &validation);
        anyhow::ensure!(
            !value.summary_markdown.trim().is_empty(),
            "analysis returned no summary"
        );
        anyhow::ensure!(
            cormex_memory::redaction::scan(&serde_json::to_string(&value)?).is_none(),
            "credential-shaped analysis blocked"
        );
        store
            .save_qa_analysis(
                &key,
                &input.session_id,
                dialogue::RULE_VERSION,
                &encoded,
                &serde_json::to_value(&value)?,
                through,
            )
            .await?;
        value
    };
    let analysis = if already_verified {
        proposed
    } else {
        let verified =
            dialogue::validate_analysis(analyzer.verify(&input, &proposed).await?, &input);
        anyhow::ensure!(
            cormex_memory::redaction::scan(&serde_json::to_string(&verified)?).is_none(),
            "credential-shaped verified output blocked"
        );
        store
            .save_qa_verified(&key, &serde_json::to_value(&verified)?)
            .await?;
        verified
    };
    anyhow::ensure!(
        !store
            .qa_sources_redacted(&input.session_id, &source_ids)
            .await?,
        "QA sources were erased while analyzing"
    );
    retire_previous_automatic_version(store, &input.session_id).await?;
    let episode = NewEpisode {
        id,
        session_id: input.session_id.clone(),
        role: cormex_store::Role::System,
        content: serde_json::json!({"extraction_unit":"qa_turn","rule_version":dialogue::RULE_VERSION,"source_event_ids":input.messages.iter().map(|m|&m.id).collect::<Vec<_>>(),"context_event_ids":input.recent_context.iter().map(|m|&m.id).collect::<Vec<_>>(),"source_mode":input.source_mode,"text":text}),
        text: Some(text),
        tsv_source: None,
        domain: None,
        device_id: "cormex-archive-qa".into(),
        occurred_at: stamp,
    };
    match store
        .write_txn(async |t| t.insert_episode(&episode).await)
        .await
    {
        Ok(_) => {}
        Err(cormex_store::StoreError::Sql(sqlx::Error::Database(e)))
            if e.code().as_deref() == Some("23505") => {}
        Err(e) => return Err(e.into()),
    }
    let retained = analysis
        .memories
        .iter()
        .filter(|m| m.status == "retain")
        .map(|m| m.retained_candidate(stamp))
        .collect::<cormex_core::Result<Vec<_>>>()?;
    if !retained.is_empty() && store.facts_from_episode(&key).await?.is_empty() {
        let ctx = ExtractContext::new(id, stamp);
        let predicates = retained
            .iter()
            .map(|c| c.predicate.clone())
            .collect::<Vec<_>>();
        let upgrades = analysis
            .memories
            .iter()
            .filter(|m| {
                m.status == "retain"
                    && input.existing_memories.iter().any(|old| {
                        old.key == m.memory_key
                            && dialogue::evidence_rank(&m.verification)
                                > dialogue::evidence_rank(&old.verification)
                    })
            })
            .map(|m| format!("memory/{}", m.memory_key))
            .collect::<Vec<_>>();
        extractor
            .clone()
            .with_functional_predicates(predicates)
            .with_revalidated_predicates(upgrades)
            .with_memory_evidence(
                analysis
                    .memories
                    .iter()
                    .filter(|m| m.status == "retain")
                    .map(|m| {
                        (
                            format!("memory/{}", m.memory_key),
                            dialogue::evidence_rank(&m.verification),
                        )
                    })
                    .collect(),
            )
            .write_candidates(store, retained, &ctx)
            .await?;
    }
    store.finish_qa_analysis(&key).await?;
    if let Some(trial) = trial {
        store.associate_qa_trial(trial, &key).await?;
    }
    Ok(())
}

/// Re-evaluated chats replace the earlier automatic pass only after the new
/// verifier succeeded. The original version and correction remain auditable.
async fn retire_previous_automatic_version(store: &Store, session: &str) -> anyhow::Result<()> {
    let old:Vec<(String,String)>=sqlx::query_as("SELECT f.id::text,e.id::text FROM active_facts f JOIN episodes e ON e.id=f.source_episode_id WHERE e.session_id=$1 AND e.content->>'rule_version'='qa@3'")
        .bind(session).fetch_all(store.pool()).await?;
    for (fact, episode) in old {
        let event = cormex_store::NewFactEvent {
            id: Id::new(),
            fact_id: fact.parse()?,
            op: cormex_store::FactOp::Invalidate,
            kind: Some(cormex_store::InvalidationKind::Corrected),
            invalid_at: Some(chrono::Utc::now()),
            superseded_by: None,
            actor: cormex_store::Actor::System,
            reason: Some("qa@4 自动语义校验替代旧抽取结果；原记录保留".into()),
            source_episode_id: Some(episode.parse()?),
            device_id: "cormex-archive-qa".into(),
        };
        store
            .write_txn(async |t| t.insert_fact_event(&event).await)
            .await?;
    }
    Ok(())
}

/// Retire only old single-message facts in the explicitly reviewed chat. The
/// original fact and source remain auditable; this is an append-only correction.
pub async fn retire_legacy(store: &Store, session: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE memory_qa_artifacts a SET status='retired' WHERE a.kind='legacy' AND a.status='reviewing' AND a.session_id=$1 AND EXISTS(SELECT 1 FROM fact_events e WHERE e.fact_id::text=a.fact_id AND e.reason LIKE 'qa@2 复核完整问答%')")
  .bind(session).execute(store.pool()).await?;
    let old:Vec<(String,String,String,String,String,Option<String>)>=sqlx::query_as(
  "SELECT f.id::text,f.statement,e.id::text,coalesce(e.content->>'transcript_event_id',''),n.name,e.text FROM active_facts f JOIN episodes e ON e.id=f.source_episode_id JOIN entities n ON n.id=f.subject_id WHERE e.session_id=$1 AND e.content ? 'transcript_event_id'")
  .bind(session).fetch_all(store.pool()).await?;
    for (fact, statement, episode, event, subject, text) in old {
        let record = serde_json::json!([{"message_id":event,"role":"legacy","quote":text.unwrap_or_default().chars().take(250).collect::<String>()}]);
        sqlx::query("INSERT INTO memory_qa_artifacts(id,kind,title,markdown,session_id,status,source_event_ids,reason,evidence,fact_id,chat_title,project_name) VALUES($1,'legacy',$2,$3,$4,'reviewing',$5,$6,$7,$8,(SELECT chat_title FROM memory_qa_summaries WHERE session_id=$4 ORDER BY updated_at DESC LIMIT 1),(SELECT project_name FROM memory_qa_summaries WHERE session_id=$4 ORDER BY updated_at DESC LIMIT 1)) ON CONFLICT(fact_id) WHERE kind='legacy' DO NOTHING")
   .bind(Id::new().to_string()).bind(subject).bind(statement).bind(session).bind(serde_json::json!([event]))
   .bind("旧单消息策略未结合完整问答与用户确认；重新评审后退出默认长期检索，保留原文和记录作对照。")
   .bind(record).bind(&fact).execute(store.pool()).await?;
        let correction = cormex_store::NewFactEvent {
            id: Id::new(),
            fact_id: fact.parse()?,
            op: cormex_store::FactOp::Invalidate,
            kind: Some(cormex_store::InvalidationKind::Corrected),
            invalid_at: Some(chrono::Utc::now()),
            superseded_by: None,
            actor: cormex_store::Actor::System,
            reason: Some("qa@2 复核完整问答：旧单消息策略结果退役，见旧策略记录与会话摘要".into()),
            source_episode_id: Some(episode.parse()?),
            device_id: "cormex-archive-qa".into(),
        };
        store
            .write_txn(async |t| t.insert_fact_event(&correction).await)
            .await?;
        sqlx::query(
            "UPDATE memory_qa_artifacts SET status='retired' WHERE kind='legacy' AND fact_id=$1",
        )
        .bind(&fact)
        .execute(store.pool())
        .await?;
    }
    Ok(())
}

/// Automatic imports use the same QA processor as explicit trials. The cursor
/// advances only after a complete turn; active, unanswered tails are deferred.
pub async fn normal_round(
    st: &AppState,
    store: &Store,
    settings: &cormex_store::memory_control::MemoryExtractionSettings,
    analyzer: &dyn DialogueAnalyzer,
    extractor: &Extractor,
    gate: &tokio::sync::Semaphore,
) -> anyhow::Result<usize> {
    let mut processed = 0;
    let model_key = format!(
        "{}:{}:{}",
        settings.provider,
        settings.model,
        settings.base_url.as_deref().unwrap_or_default()
    );
    for (session, cursor) in store.qa_pending(dialogue::RULE_VERSION, 3).await? {
        let current = store.memory_extraction_settings().await?;
        if current
            .as_ref()
            .is_some_and(|s| !s.enabled || s.id != settings.id)
        {
            return Ok(processed);
        }
        let Some(row) = store.archive_session(&session).await? else {
            continue;
        };
        let Some(version) = crate::archive_api::parser_version_for_agent(&row.agent) else {
            continue;
        };
        let Some((latest,last_at)):Option<(i64,chrono::DateTime<chrono::Utc>)>=sqlx::query_as("SELECT seq,created_at FROM transcript_events WHERE session_id=$1 AND parser_version=$2 ORDER BY seq DESC LIMIT 1")
   .bind(&session).bind(version).fetch_optional(store.pool()).await? else {continue};
        let raw = store
            .conversation_candidates(&session, version, cursor)
            .await?;
        let whole_window = raw.len() < 200;
        let preceding:Vec<TranscriptEvent>=sqlx::query_as("SELECT * FROM transcript_events WHERE session_id=$1 AND parser_version=$2 AND seq<=$3 AND kind IN ('user_message','agent_message') ORDER BY seq DESC LIMIT 32")
   .bind(&session).bind(version).bind(cursor).fetch_all(store.pool()).await?;
        let mut context = preceding;
        context.reverse();
        context.extend(raw);
        let display =
            crate::archive_api::memory_conversation_events(st, store, &row.agent, context)
                .await
                .map_err(|_| anyhow::anyhow!("archive source unreadable"))?;
        let turns = dialogue::qa_turns(&display.iter().map(message).collect::<Vec<_>>());
        let idle = last_at < chrono::Utc::now() - chrono::Duration::seconds(60);
        let mut all_done = true;
        for (i, turn) in turns.iter().enumerate() {
            let last = turn.last().expect("turn nonempty");
            if last.seq <= cursor {
                continue;
            }
            if i + 1 == turns.len()
                && (!whole_window || !idle || !turn.iter().any(|m| m.role == "assistant"))
            {
                all_done = false;
                break;
            }
            if processed >= st.distill_config().promote_per_round.max(1) as usize {
                return Ok(processed);
            }
            let current = store.memory_extraction_settings().await?;
            if current
                .as_ref()
                .is_some_and(|s| !s.enabled || s.id != settings.id)
            {
                return Ok(processed);
            }
            let _permit = gate.acquire().await?;
            tokio::time::timeout(
                std::time::Duration::from_secs(90),
                process(
                    store,
                    input(&row, &turns, i),
                    &model_key,
                    analyzer,
                    extractor,
                    None,
                ),
            )
            .await??;
            store
                .advance_qa_progress(&session, dialogue::RULE_VERSION, last.seq)
                .await?;
            processed += 1;
        }
        if whole_window && idle && all_done {
            retire_legacy(store, &session).await?;
            store
                .advance_qa_progress(&session, dialogue::RULE_VERSION, latest)
                .await?;
            store
                .archive_txn(async |t| t.insert_promote_progress(&session, latest).await)
                .await?;
        } else {
            store
                .defer_qa_progress(&session, dialogue::RULE_VERSION)
                .await?;
        }
    }
    Ok(processed)
}

#[derive(Deserialize)]
pub struct ArtifactQuery {
    pub kind: String,
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}
pub async fn artifacts(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<ArtifactQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !p.has_scope(Scope::MemoryRead) || !p.has_scope(Scope::SessionRead) {
        return Err(ApiError::forbidden("需要记忆和聊天读取权限"));
    }
    if !matches!(
        q.kind.as_str(),
        "summary" | "suggestion" | "temporary" | "legacy"
    ) || q.cursor.as_ref().is_some_and(|c| c.parse::<Id>().is_err())
    {
        return Err(ApiError::bad_request("记忆类别或分页游标无效"));
    }
    let tenant = p.tenant(&st).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    let mut items = store
        .memory_artifacts(&q.kind, q.cursor.as_deref(), limit + 1)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    let next = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(|i| i.id.clone())
    } else {
        None
    };
    Ok(Json(serde_json::json!({"items":items,"next_cursor":next})))
}

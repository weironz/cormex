//! Persistent, tenant-scoped controls for archived-chat fact extraction.
use crate::{principal::Principal, routes::ApiError, state::AppState};
use axum::{Extension, Json, extract::State};
use chrono::{Duration, Utc};
use cormex_core::Id;
use cormex_llm::{LlmClient, Message};
use cormex_store::memory_control::MemoryExtractionSettings;
use serde::{Deserialize, Serialize};

fn db_error(e: impl std::fmt::Display) -> ApiError {
    ApiError::internal(e.to_string())
}

fn public_base_url(raw: Option<String>) -> Option<String> {
    let mut u = url::Url::parse(raw.as_deref()?).ok()?;
    let _ = u.set_username("");
    let _ = u.set_password(None);
    u.set_query(None);
    u.set_fragment(None);
    Some(u.to_string())
}
pub(crate) fn require_user(p: &Principal) -> Result<(), ApiError> {
    if matches!(p, Principal::User { .. } | Principal::LegacyOwner) {
        Ok(())
    } else {
        Err(ApiError::forbidden("请使用账号登录后管理记忆提取设置"))
    }
}

#[derive(Serialize)]
pub struct ExtractionStatus {
    pub enabled: bool,
    pub use_server_model: bool,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub key_configured: bool,
    pub key_tail: Option<String>,
    pub model_configured: bool,
    pub providers: Vec<String>,
    pub phase: String,
    pub total_chats: i64,
    pub processed_chats: i64,
    pub pending_chats: i64,
    pub facts_created: i64,
    pub active_facts: i64,
    pub memory_count: i64,
    pub suggestion_count: i64,
    pub temporary_count: i64,
    pub summary_count: i64,
    pub legacy_count: i64,
    pub last_error: Option<String>,
    pub updated_at: Option<String>,
    pub trial: Option<cormex_store::memory_control::MemoryExtractionTrial>,
}

// API keys are write-only. An empty field preserves the key only for the same
// provider AND endpoint, so changing a relay never forwards an old secret.
#[derive(Deserialize)]
pub struct SaveExtractionSettings {
    pub enabled: bool,
    pub use_server_model: bool,
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    #[serde(default)]
    pub clear_api_key: bool,
}

pub(crate) fn defaults(st: &AppState) -> MemoryExtractionSettings {
    let cfg = st.llm_config();
    MemoryExtractionSettings {
        id: Id::new().to_string(),
        enabled: st.distill_config().promote,
        use_server_model: true,
        provider: cfg.provider.clone(),
        model: cfg.cheap_model.clone(),
        base_url: cfg.base_url.clone(),
        ciphertext: None,
        key_tail: None,
    }
}

pub(crate) fn client(st: &AppState, s: &MemoryExtractionSettings) -> anyhow::Result<LlmClient> {
    if s.use_server_model {
        return st
            .live()
            .map(|l| l.llm().clone())
            .ok_or_else(|| anyhow::anyhow!("model unavailable"));
    }
    let key = if let Some(cipher) = &s.ciphertext {
        crate::byo_key::open(cipher).map_err(|_| anyhow::anyhow!("stored key unavailable"))?
    } else if cormex_llm::provider::api_key_env(&s.provider)?.is_empty() {
        String::new()
    } else {
        anyhow::bail!("API key missing")
    };
    Ok(LlmClient::from_config(
        &cormex_core::config::LlmConfig {
            provider: s.provider.clone(),
            model: s.model.clone(),
            cheap_model: s.model.clone(),
            base_url: s.base_url.clone(),
        },
        &key,
    )?)
}

pub async fn get(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
) -> Result<Json<ExtractionStatus>, ApiError> {
    require_user(&p)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    let mut s = store
        .memory_extraction_settings()
        .await
        .map_err(db_error)?
        .unwrap_or_else(|| defaults(&st));
    if s.use_server_model {
        let d = defaults(&st);
        s.provider = d.provider;
        s.model = d.model;
        s.base_url = d.base_url;
    }
    let ready = client(&st, &s).is_ok();
    let (total, done, facts) = store.memory_extraction_totals().await.map_err(db_error)?;
    let (active_facts, suggestion_count, temporary_count, summary_count, legacy_count) =
        store.qa_counts().await.map_err(db_error)?;
    let work = store.memory_extraction_work().await.map_err(db_error)?;
    let error = work.as_ref().and_then(|w| w.last_error.clone());
    let running = work
        .as_ref()
        .is_some_and(|w| w.running && w.updated_at > Utc::now() - Duration::minutes(10));
    let trial = store.latest_memory_trial().await.map_err(db_error)?;
    let phase = if trial.as_ref().is_some_and(|j| j.status == "running") {
        "trial_running"
    } else if !s.enabled {
        "paused"
    } else if !ready {
        "needs_configuration"
    } else if error.is_some() {
        "error"
    } else if running {
        "processing"
    } else if total == done {
        "caught_up"
    } else {
        "queued"
    };
    Ok(Json(ExtractionStatus {
        enabled: s.enabled,
        use_server_model: s.use_server_model,
        provider: s.provider,
        model: s.model,
        base_url: if s.use_server_model {
            public_base_url(s.base_url)
        } else {
            s.base_url
        },
        key_configured: s.ciphertext.is_some(),
        key_tail: s.key_tail,
        model_configured: ready,
        providers: cormex_llm::provider::available(),
        phase: phase.into(),
        total_chats: total,
        processed_chats: done,
        pending_chats: (total - done).max(0),
        facts_created: facts,
        active_facts,
        memory_count: active_facts,
        suggestion_count,
        temporary_count,
        summary_count,
        legacy_count,
        last_error: error,
        updated_at: work.map(|w| w.updated_at.to_rfc3339()),
        trial,
    }))
}

pub async fn put(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Json(req): Json<SaveExtractionSettings>,
) -> Result<Json<ExtractionStatus>, ApiError> {
    require_user(&p)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    if req.enabled
        && store
            .latest_memory_trial()
            .await
            .map_err(db_error)?
            .is_some_and(|j| j.status == "running")
    {
        return Err(ApiError::bad_request(
            "小样本验证期间不能开启全量提取，请先完成或停止验证",
        ));
    }
    let old = store.memory_extraction_settings().await.map_err(db_error)?;
    let base_url = req
        .base_url
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    if let Some(url) = &base_url
        && !req.use_server_model
    {
        let u = url::Url::parse(url).map_err(|_| ApiError::bad_request("模型地址格式不正确"))?;
        if !matches!(u.scheme(), "http" | "https")
            || u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
        {
            return Err(ApiError::bad_request(
                "模型地址需使用 HTTP 或 HTTPS，不能包含凭据、查询参数或锚点",
            ));
        }
    }
    let mut s = MemoryExtractionSettings {
        id: Id::new().to_string(),
        enabled: req.enabled,
        use_server_model: req.use_server_model,
        provider: req.provider.trim().into(),
        model: req.model.trim().into(),
        base_url,
        ciphertext: None,
        key_tail: None,
    };
    if !s.use_server_model {
        if !cormex_llm::provider::available().contains(&s.provider)
            || s.model.is_empty()
            || s.model.len() > 200
        {
            return Err(ApiError::bad_request(
                "请选择支持的服务商并填写模型名称（最多 200 字节）",
            ));
        }
        if !req.clear_api_key {
            if let Some(key) = req
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
            {
                if key.len() > 8192 {
                    return Err(ApiError::bad_request("API Key 过长"));
                }
                s.ciphertext = Some(crate::byo_key::seal(key)?);
                s.key_tail = Some(
                    key.chars()
                        .rev()
                        .take(4)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect(),
                );
            } else if let Some(old) =
                old.filter(|o| o.provider == s.provider && o.base_url == s.base_url)
            {
                s.ciphertext = old.ciphertext;
                s.key_tail = old.key_tail;
            }
        }
    }
    if s.enabled && client(&st, &s).is_err() {
        return Err(ApiError::bad_request(
            "模型配置尚未就绪，请填写 API Key 或选择服务端默认模型",
        ));
    }
    store
        .save_memory_extraction_settings(&s)
        .await
        .map_err(db_error)?;
    get(State(st), Extension(p)).await
}

/// Tests the saved model using a short fixed prompt, never chat history.
pub async fn test(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
) -> Result<Json<serde_json::Value>, ApiError> {
    require_user(&p)?;
    let tenant = p.tenant(&st).await?;
    let store = tenant.store().map_err(db_error)?;
    let s = store
        .memory_extraction_settings()
        .await
        .map_err(db_error)?
        .unwrap_or_else(|| defaults(&st));
    let llm = client(&st, &s).map_err(|_| ApiError::bad_request("模型配置尚未就绪"))?;
    let messages = [Message::user().with_text("Reply OK.")];
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(25),
        llm.complete(
            llm.cheap_model(),
            "Connection test. Reply briefly.",
            &messages,
            &[],
        ),
    )
    .await;
    match result {
        Ok(Ok(_)) => Ok(Json(serde_json::json!({"ok":true}))),
        _ => Err(ApiError::bad_request(
            "模型连接失败，请检查服务地址、模型名称和 API Key；未发送聊天历史",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_credentials_cannot_manage_extraction() {
        let d = crate::device_keys::DeviceScope {
            user_id: "u".into(),
            device_id: "d".into(),
            scopes: crate::device_keys::Scope::all().to_vec(),
        };
        assert!(require_user(&Principal::Device(d.clone())).is_err());
        for path in [
            "/settings/memory-extraction",
            "/settings/memory-extraction/test",
        ] {
            assert!(!d.allows(&axum::http::Method::GET, path));
            assert!(!d.allows(&axum::http::Method::PUT, path));
            assert!(!d.allows(&axum::http::Method::POST, path));
        }
    }

    #[tokio::test]
    #[ignore = "needs a database and CORMEX_SECRET_KEY"]
    async fn preferences_are_persistent_private_and_secrets_are_write_only() {
        let base = crate::test_support::database_url().expect("database URL");
        assert!(
            std::env::var("CORMEX_SECRET_KEY").is_ok(),
            "test requires encryption key"
        );
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;
        let a = st
            .create_account("owner", "test-owner-password")
            .await
            .unwrap();
        let b = st
            .create_account("other", "test-other-password")
            .await
            .unwrap();
        let p = Principal::User { user_id: a };
        let request = |url: &str, key: Option<&str>| SaveExtractionSettings {
            enabled: false,
            use_server_model: false,
            provider: "openai".into(),
            model: "memory-test".into(),
            base_url: Some(url.into()),
            api_key: key.map(str::to_owned),
            clear_api_key: false,
        };
        let Json(status) = put(
            State(st.clone()),
            Extension(p.clone()),
            Json(request(
                "http://127.0.0.1:9/v1",
                Some("test-only-secret-key"),
            )),
        )
        .await
        .unwrap();
        assert!(!status.enabled);
        assert!(status.key_configured);
        assert_eq!(status.key_tail.as_deref(), Some("-key"));
        assert!(
            !serde_json::to_string(&status)
                .unwrap()
                .contains("test-only-secret-key")
        );
        let Json(other) = get(State(st.clone()), Extension(Principal::User { user_id: b }))
            .await
            .unwrap();
        assert!(other.use_server_model);
        assert!(!other.key_configured);
        let Json(kept) = put(
            State(st.clone()),
            Extension(p.clone()),
            Json(request("http://127.0.0.1:9/v1", None)),
        )
        .await
        .unwrap();
        assert!(kept.key_configured);
        let Json(changed) = put(
            State(st.clone()),
            Extension(p.clone()),
            Json(request("http://127.0.0.1:9/another", None)),
        )
        .await
        .unwrap();
        assert!(!changed.key_configured);
        assert!(!changed.model_configured);
        let bad = put(
            State(st.clone()),
            Extension(p.clone()),
            Json(request("https://user:secret@example.test", None)),
        )
        .await;
        assert!(bad.is_err());
        let Json(restored) = get(State(st.clone()), Extension(p)).await.unwrap();
        assert!(!restored.enabled);
        assert_eq!(
            restored.base_url.as_deref(),
            Some("http://127.0.0.1:9/another")
        );
        db.drop().await;
    }
}

use crate::{device_keys::Scope, principal::Principal, routes::ApiError, state::AppState};
use axum::{
    Extension, Json,
    extract::{Query, State},
};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct ContextQuery {
    pub ids: String,
}

pub async fn get(
    State(st): State<AppState>,
    Extension(p): Extension<Principal>,
    Query(q): Query<ContextQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !p.has_scope(Scope::MemoryRead) || !p.has_scope(Scope::SessionRead) {
        return Err(ApiError::forbidden("查看记忆来源需要记忆和聊天读取权限"));
    }
    let ids = q
        .ids
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if ids.is_empty()
        || ids.len() > 100
        || ids.iter().any(|id| id.parse::<cormex_core::Id>().is_err())
    {
        return Err(ApiError::bad_request(
            "每次最多读取 100 条有效记忆的来源信息",
        ));
    }
    let tenant = p.tenant(&st).await?;
    let store = tenant
        .store()
        .map_err(|e| ApiError::unsupported(e.to_string()))?;
    let contexts = store
        .memory_contexts(&ids)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;
    Ok(Json(serde_json::json!({"contexts":contexts})))
}

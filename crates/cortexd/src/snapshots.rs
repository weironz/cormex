//! 沙箱工作区快照的**存储那一半**。
//!
//! # 为什么一个功能被劈成两半
//!
//! 拍快照要两样东西：从 docker 卷里导出 tar（**只有 `cortex-agentd` 有
//! docker**），以及把字节与索引持久化（**只有这里有对象存储与库**）。
//! 整块放哪一边都得给那一边补上它按设计不该有的东西。
//!
//! 所以切在中间：agentd 出 tar，走已有的 `POST /blobs` 把字节交上来，
//! 再调这里的 `POST /sandbox-snapshots` 记一行；恢复时反过来 ——
//! 从 `GET /sandbox-snapshots` 问出 `blob_hash`，走 `GET /blobs/{hash}`
//! 取字节，自己解进卷里。
//!
//! 字节走 `/blobs` 而不是新开一条上传路：那条路已经有分片、去重、租户前缀
//! 与直传，而快照就是一坨字节，没有任何特殊之处。
//!
//! # 恢复那条路的授权在哪儿
//!
//! 在 [`list`]：它的查询跑在**调用者自己的租户 schema** 里，并且按作用域
//! 过滤。agentd 只能从这个列表里挑 id —— 它挑不到别人的，也挑不到自己
//! 另一个项目的。直接让调用方给 `blob_hash` 就没有这层：对象存储是内容
//! 寻址的，哈希本身不带归属。

use axum::extract::{Query, State};
use axum::{Json, http::HeaderMap};
use cortex_core::{CortexError, Id, Result};

use crate::routes::ApiError;
use crate::state::AppState;

/// 列快照时一次最多给多少条。够界面翻一屏，也够「拿最近一份来恢复」。
const LIST_LIMIT: i64 = 50;

/// 一行快照记录。
#[derive(Debug, Clone, sqlx::FromRow, serde::Serialize, serde::Deserialize)]
pub struct SnapshotRow {
    pub id: String,
    pub blob_hash: String,
    pub size_bytes: i64,
    pub taken_at: chrono::DateTime<chrono::Utc>,
}

/// `?scope=` —— 快照按**作用域**分，不按 owner。
///
/// 混在一起的话，用户在 A 项目里看到的是三个项目的快照，而点「恢复」会把
/// 别的项目的 tar 解进这个工作区。
#[derive(serde::Deserialize)]
pub struct ScopeQuery {
    scope: String,
}

/// 记一行新快照。字节由调用方**先**走 `POST /blobs` 传好。
#[derive(serde::Deserialize)]
pub struct RecordRequest {
    pub scope: String,
    pub blob_hash: String,
    pub size_bytes: i64,
}

/// `GET /sandbox-snapshots?scope=` —— 这个作用域的快照，新的在前。
///
/// # Errors
/// 租户解析或查询失败。
pub async fn list(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<ScopeQuery>,
) -> std::result::Result<Json<Vec<SnapshotRow>>, ApiError> {
    let owner = crate::accounts::current_user(&st, &headers).await;
    Ok(Json(rows_for(&st, &owner, &q.scope).await?))
}

/// `POST /sandbox-snapshots` —— 记一行。
///
/// # owner 从凭据来，不从请求体来
///
/// 让调用方指定 owner 的话，任何登录用户都能往别人名下塞一条记录，
/// 而恢复时那条记录会被当成他自己的。
///
/// # Errors
/// 租户解析或写入失败。
pub async fn record(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<RecordRequest>,
) -> std::result::Result<Json<SnapshotRow>, ApiError> {
    let owner = crate::accounts::current_user(&st, &headers).await;
    let row = SnapshotRow {
        id: Id::new().to_string(),
        blob_hash: req.blob_hash,
        size_bytes: req.size_bytes,
        taken_at: chrono::Utc::now(),
    };
    let store = st.tenant_store_for_user(&owner).await?;
    sqlx::query(
        "INSERT INTO sandbox_snapshots (id, owner, scope, blob_hash, size_bytes, taken_at) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(&row.id)
    .bind(&owner)
    .bind(&req.scope)
    .bind(&row.blob_hash)
    .bind(row.size_bytes)
    .bind(row.taken_at)
    .execute(store.pool())
    .await
    .map_err(|e| CortexError::Store(format!("快照索引落库失败：{e}")))?;
    Ok(Json(row))
}

/// 这个作用域的快照行。
async fn rows_for(st: &AppState, owner: &str, scope: &str) -> Result<Vec<SnapshotRow>> {
    let store = st.tenant_store_for_user(owner).await?;
    sqlx::query_as::<_, SnapshotRow>(
        "SELECT id, blob_hash, size_bytes, taken_at FROM sandbox_snapshots \
         WHERE scope = $1 ORDER BY taken_at DESC LIMIT $2",
    )
    .bind(scope)
    .bind(LIST_LIMIT)
    .fetch_all(store.pool())
    .await
    .map_err(|e| CortexError::Store(format!("列快照失败：{e}")))
}

//! 一次请求属于谁 —— 解析成一个真的连到那个 schema 的 [`Store`]。
//!
//! # 为什么要有这么一个类型，而不是让 handler 自己查
//!
//! 隔离本身早就有了：[`cortex_store::TenantPools`] 把 `search_path` 焊在
//! 连接选项里，一个连接**物理上看不见别的 schema**。缺的是**没人调它** ——
//! 请求路径一直握着启动时建的那一个 `Store`（它连的是 `public`），
//! 于是两个登录的用户读写同一片库。
//!
//! 这里补的就是那一段。做成一个类型而不是一句 `let store = …`，
//! 是因为**漏传是这类 bug 唯一的形态**：
//!
//! - 让 `AppState` 的数据方法**都要求 `&Tenant`**，漏了就编译不过
//! - 不提供 `Default`、不提供「取当前租户」的全局函数、不用 task-local
//!
//! 最后一条是刻意的。task-local 能少改很多代码，但它与
//! 「共享池 + 每次 `SET search_path`」是同一个形状：**忘记设置不会报错，
//! 只会安静地落到 `public`**，而那正是要防的事故本身。宁可改 16 个签名。
//!
//! # 单用户自托管不受影响
//!
//! 没配账号体系时，租户恒为 `public` —— 与今天的行为逐字节相同。
//! 老的预共享 token 那条路也一样：它认不出 access token，
//! 回落到 1 号用户，而 1 号用户的 schema 名就叫 `public`。

use cortex_store::{SchemaName, Store};
use std::sync::Arc;

use crate::routes::ApiError;
use crate::state::AppState;

/// 一次请求所属的租户。
///
/// **拿到它 = 已经解析完毕**。没有「还没解析」的状态，也就没有
/// 「忘了解析」这个 bug。
pub enum Tenant {
    /// 真实后端：已经连到某个 schema 的池。
    Live {
        schema: SchemaName,
        store: Arc<Store>,
    },
    /// mock 后端：没有库。
    ///
    /// 单开一支而不是塞 `Option<Store>`：`Option` 会诱导
    /// `unwrap_or_default()`，而那在这里等于回落到别人的库。
    Mock,
}

impl Tenant {
    /// 这个租户的 `Store`。
    ///
    /// # Errors
    /// mock 后端下没有库可给。调用方（`Backend::Live` 分支）不会撞到，
    /// 但签名不能骗人。
    /// 错误类型跟着 `AppState` 的数据方法走（`CortexError`），
    /// 不是 `ApiError` —— 那样每个调用点都要多一次转换。
    pub fn store(&self) -> cortex_core::Result<&Store> {
        match self {
            Self::Live { store, .. } => Ok(store),
            Self::Mock => Err(cortex_core::CortexError::Unavailable(
                "本实例跑在 mock 后端上，没有可访问的记忆库".into(),
            )),
        }
    }

    /// 落在哪个 schema 上。日志与诊断用。
    #[must_use]
    pub fn schema(&self) -> &str {
        match self {
            Self::Live { schema, .. } => schema.as_str(),
            Self::Mock => "mock",
        }
    }
}

impl AppState {
    /// 解析这个请求属于哪个租户。
    ///
    /// 解析顺序刻意是「先看后端、再看有没有账号体系、最后才查库」：
    /// 前两步覆盖了绝大多数部署（mock 开发机、单用户自托管），
    /// 它们一次数据库往返都不做。
    ///
    /// # Errors
    /// 用户存在但 schema 名不合法，或者那个 schema 的池建不起来。
    pub async fn tenant(&self, headers: &axum::http::HeaderMap) -> Result<Tenant, ApiError> {
        // mock 后端：没有库，也就没有租户可分
        let Some(public_store) = self.public_store() else {
            return Ok(Tenant::Mock);
        };

        // 没接账号体系 = 单用户自托管。恒 public，与今天逐字节相同
        let Ok(acc) = self.accounts() else {
            return Ok(Tenant::Live {
                schema: SchemaName::public(),
                store: public_store,
            });
        };

        let user_id = crate::accounts::current_user(self, headers).await;
        let schema = resolve_schema(&acc.pool, &user_id).await?;
        self.tenant_on(schema).await
    }

    /// 同样的解析，但入口是**一个用户 id**而不是请求头。
    ///
    /// # 为什么单开一个而不是让后台任务伪造一份 header
    ///
    /// 伪造 header 要先造一个 access token，而后台任务手上没有 —— 它只有
    /// 沙箱令牌里记着的那个 owner。真造一个的话，等于给后台任务开一条
    /// 「凭空生成凭据」的能力，那比多一个方法贵得多。
    ///
    /// # 为什么不直接暴露 `resolve_schema`
    ///
    /// 光有 schema 名没用，调用方还要自己去建池、还要记得 `public` 那一个
    /// 是启动时就有的、不该再建一份。把那三步留给调用方，就是把
    /// [`Tenant`] 这个类型当初要消灭的「漏一步」重新放回来。
    ///
    /// # Errors
    /// 查不动 `cortex_auth.users`，或者那个 schema 的池建不起来。
    pub async fn tenant_for_user(&self, user_id: &str) -> Result<Tenant, ApiError> {
        let Some(public_store) = self.public_store() else {
            return Ok(Tenant::Mock);
        };
        let Ok(acc) = self.accounts() else {
            return Ok(Tenant::Live {
                schema: SchemaName::public(),
                store: public_store,
            });
        };
        let schema = resolve_schema(&acc.pool, user_id).await?;
        self.tenant_on(schema).await
    }

    /// schema 名 → 真的连上去的 `Tenant`。两条解析路的共同尾巴。
    ///
    /// 抽出来是因为「`public` 复用启动时那个池」这一条**两边都要记得**，
    /// 而漏掉它不报错 —— 只是给 1 号用户多开一个连着同一个 schema 的池，
    /// 在 `max_connections` 只有 100 的机器上悄悄吃掉配额。
    async fn tenant_on(&self, schema: SchemaName) -> Result<Tenant, ApiError> {
        let Some(public_store) = self.public_store() else {
            return Ok(Tenant::Mock);
        };
        if schema.as_str() == SchemaName::public().as_str() {
            return Ok(Tenant::Live {
                schema,
                store: public_store,
            });
        }
        let acc = self
            .accounts()
            .map_err(|_| ApiError::internal("没接账号体系却要解析非 public 租户"))?;
        let store = acc
            .tenants
            .get(&schema)
            .await
            .map_err(|e| ApiError::internal(format!("连不上 {} 的库：{e}", schema.as_str())))?;
        Ok(Tenant::Live { schema, store })
    }
}

/// 这个用户住在哪个 schema。
///
/// 单拎出来是为了**能被测到**：它是这次改动里唯一的新判断，
/// 而 `AppState::tenant` 整体要一个 Live 后端（LLM 客户端、embedder、
/// 转录器）才构造得出来，在测试里立不起来。
///
/// # Errors
/// 查不动库；或者库里那个 schema 名不合法。
async fn resolve_schema(pool: &sqlx::PgPool, user_id: &str) -> Result<SchemaName, ApiError> {
    let name: Option<String> =
        sqlx::query_scalar("SELECT schema_name FROM cortex_auth.users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| ApiError::internal(format!("查不到这个账号落在哪个 schema：{e}")))?;

    // 查不到就是老 token 那条路 —— 它已经过了入站那道门，而 1 号用户住在 public
    let Some(name) = name else {
        return Ok(SchemaName::public());
    };

    // `users_schema_shape` 那条 CHECK 已经把非法形状挡在表外，所以这里
    // 正常永远不会失败。**仍然校验**：约束是可以被 DROP 的，而这一步
    // 失败的后果（回落到 public = 读到 1 号用户的记忆）比多写一行贵得多。
    //
    // 这里没有配套的单元测试，是因为写不出有意义的那一条：CHECK 挡着，
    // 塞不进一个非法名字。`SchemaName::new` 自己的边界由 cortex-store 测。
    SchemaName::new(&name).map_err(|e| {
        ApiError::internal(format!("账号 {user_id} 的 schema 名不合法（{name}）：{e}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没有真库就跳过。`search_path` 与这张表都是 Postgres 的行为，
    /// 造替身等于把被测对象一起造掉。
    async fn pool() -> Option<sqlx::PgPool> {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|s| !s.is_empty())?;
        crate::state::Accounts::connect(&url)
            .await
            .ok()
            .map(|a| a.pool)
    }

    async fn make_user(pool: &sqlx::PgPool, schema: &str) -> String {
        let id = cortex_core::Id::new().to_string();
        sqlx::query(
            "INSERT INTO cortex_auth.users (id, username, password_hash, schema_name)
             VALUES ($1, $2, 'x', $3)",
        )
        .bind(&id)
        .bind(format!("probe-{id}"))
        .bind(schema)
        .execute(pool)
        .await
        .expect("插测试用户");
        id
    }

    /// 两个账号解析到**各自**的 schema。
    ///
    /// 这条如果错了，两个人读写同一片库 —— 而那不会报任何错。
    #[tokio::test]
    async fn each_account_resolves_to_its_own_schema() {
        let Some(pool) = pool().await else {
            eprintln!("跳过：没有 DATABASE_URL");
            return;
        };
        let a_schema = SchemaName::derive(&cortex_core::Id::new().to_string()).unwrap();
        let b_schema = SchemaName::derive(&cortex_core::Id::new().to_string()).unwrap();
        let a = make_user(&pool, a_schema.as_str()).await;
        let b = make_user(&pool, b_schema.as_str()).await;

        let ra = resolve_schema(&pool, &a).await.expect("解析 A");
        let rb = resolve_schema(&pool, &b).await.expect("解析 B");
        assert_eq!(ra.as_str(), a_schema.as_str(), "A 没有落在自己的 schema 上");
        assert_eq!(rb.as_str(), b_schema.as_str(), "B 没有落在自己的 schema 上");
        assert_ne!(
            ra.as_str(),
            rb.as_str(),
            "两个账号解析到了同一个 schema —— 这就是「登录了两个人，读的是同一片库」"
        );
    }

    /// 认不出的用户回落到 `public` —— 老的预共享 token 那条路要继续能用。
    #[tokio::test]
    async fn an_unknown_caller_is_the_legacy_token_path() {
        let Some(pool) = pool().await else { return };
        let r = resolve_schema(&pool, "no-such-user-at-all")
            .await
            .expect("解析");
        assert_eq!(
            r.as_str(),
            "public",
            "老 token 那条路必须仍然落在 public 上，否则 CLI 与现有桌面端当天全部失联"
        );
    }
}

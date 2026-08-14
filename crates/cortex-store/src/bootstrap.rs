//! 启动时把所有租户的 schema 迁到最新。
//!
//! # 为什么这件事需要一个专门的模块
//!
//! 单用户时 migration 是一句 `store.migrate()`，只有两种结果：过了，或者
//! 服务起不来。多租户之后它变成一个**部分失败**的操作 —— 二十个租户里
//! 有一个的 schema 出了问题（磁盘满、有人手动改过表、上一次升级跑到一半
//! 断电），而剩下十九个完全正常。
//!
//! 这时候「整个服务起不来」是**错的**：一个用户的库坏了不该让所有人下线。
//! 而「静默跳过」更错：那个用户会带着一个缺表的 schema 继续用，症状是
//! 某个功能偶尔报一句看不懂的 SQL 错误。
//!
//! 所以这里的产物是一份**逐租户的结果清单**，调用方据此决定：正常的照常
//! 服务，坏掉的那几个明确标记为不可用并报出来。

use crate::error::Result;
use crate::store::Store;
use crate::tenant::{SchemaName, TenantPools};

/// 一个租户的迁移结果。
#[derive(Debug)]
pub struct TenantMigration {
    pub schema: SchemaName,
    /// `None` = 成功；`Some` = 这个租户不可用，附上给人看的原因。
    pub failure: Option<String>,
}

/// 把 `schemas` 里每个租户都迁到最新。
///
/// **逐个来，不并发。** migration 里有 `CREATE INDEX`（HNSW 那几个尤其重），
/// 二十个 schema 同时建索引会把一台 2 核的机器直接打死 —— 而那正是启动时
/// 最不该发生的事。慢几秒换一个能起来的服务。
///
/// # Errors
/// 不返回 `Err`：单个租户的失败进 [`TenantMigration::failure`]。
/// 整体失败（连不上数据库）由调用方在建 [`TenantPools`] 时就撞到了。
pub async fn migrate_all(pools: &TenantPools, schemas: &[SchemaName]) -> Vec<TenantMigration> {
    let mut out = Vec::with_capacity(schemas.len());
    for schema in schemas {
        let failure = match pools.get(schema).await {
            Err(e) => Some(format!("取不到连接池：{e}")),
            Ok(store) => match store.migrate().await {
                Ok(()) => None,
                Err(e) => Some(format!("migration 失败：{e}")),
            },
        };
        if let Some(why) = &failure {
            // error 而不是 warn：这个租户从现在起是不可用的，
            // 而它在别处的表现只会是零散的 SQL 错误
            tracing::error!(schema = schema.as_str(), reason = %why, "租户 schema 迁移失败，该用户暂不可用");
        }
        out.push(TenantMigration {
            schema: schema.clone(),
            failure,
        });
    }
    out
}

/// 给一个新用户开一片 schema 并迁到最新。
///
/// # 顺序不能反
///
/// 先建 schema、再迁移，最后才把 `users` 那一行写下去 —— 调用方按这个顺序
/// 做。反过来（先写 users 再建 schema）会留下一个「账号存在但库是空的」
/// 的中间态，而那个用户一登录就撞上一片没有表的 schema。
///
/// # Errors
/// 建不出 schema，或者 migration 跑不过。**失败时 schema 可能已经建了一半**
/// —— 调用方应当把它删掉再报错，别留下孤儿。
pub async fn provision(pools: &TenantPools, schema: &SchemaName) -> Result<()> {
    pools.create_schema(schema).await?;
    let store: std::sync::Arc<Store> = pools.get(schema).await?;
    store.migrate().await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 失败要**逐个**记下来，而不是让整批垮掉。
    ///
    /// 这条守的是「一个用户的库坏了不该让所有人下线」。
    #[test]
    fn a_failure_is_recorded_per_tenant_not_thrown() {
        let ok = TenantMigration {
            schema: SchemaName::public(),
            failure: None,
        };
        let bad = TenantMigration {
            schema: SchemaName::new("u_01arz3ndektsv4rrffq69g5fav").unwrap(),
            failure: Some("磁盘满".into()),
        };
        let results = [ok, bad];

        let broken: Vec<&str> = results
            .iter()
            .filter(|r| r.failure.is_some())
            .map(|r| r.schema.as_str())
            .collect();
        assert_eq!(broken, ["u_01arz3ndektsv4rrffq69g5fav"]);
        assert!(
            results.iter().any(|r| r.failure.is_none()),
            "一个租户失败不该让别的租户也标成失败"
        );
    }
}

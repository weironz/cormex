//! 多用户：一个用户一个 schema，一个 schema 一个池。
//!
//! # 为什么不是给每张表加 `user_id`
//!
//! 现在有 17 张表、约 77 条查询。加一列意味着 77 个地方都要**记得**过滤，
//! 而漏掉任何一个的后果是**一个用户读到另一个用户的记忆** —— 没有编译期
//! 帮助，没有任何现存测试会自然覆盖到，而且症状不是报错，是「多了几条
//! 不认识的记忆」。
//!
//! 换成 schema 隔离之后，那 77 条查询**一条都不用改**：连接的
//! `search_path` 里只有这个用户的 schema，别人的表根本不在名字解析范围内。
//! 隔离从「每处都记得写 WHERE」变成「连接建立时定死一次」。
//!
//! 这条路在这个仓库里早就跑通了 —— 集成测试的 `common::setup` 就是这么给
//! 每个测试建独立 schema 的（`tests/common/mod.rs`）。这里做的是把同一个
//! 手法从测试搬到生产。
//!
//! # 为什么是「每用户一个池」，不是「共享池 + 每次 SET search_path」
//!
//! 共享池省连接，但它把隔离从**结构**降级成**纪律**：每借出一条连接都要
//! 记得先 `SET search_path`，漏一次就是拿着上一个用户的 search_path 去查。
//! 而连接是复用的，所以漏掉的那次**大概率真的落在别人的 schema 上**。
//!
//! 焊在 [`PgConnectOptions`] 里则物理上不可能出这个错：这条连接从建立到
//! 关闭都只看得见一个 schema。代价是连接数，而那是个可以算清楚的数
//! （见 [`TenantPools::new`] 的注释），不是一个会静默发生的数据泄露。
//!
//! # 上限是真的上限
//!
//! Postgres 的 `max_connections` 默认 100。每用户 [`PER_TENANT_CONNECTIONS`]
//! 条，于是同时驻留的用户数有硬上限。满了**明确报错**而不是排队等 ——
//! 排队的表现是「所有人都变慢」，而报错至少指得出原因。

use std::collections::HashMap;
use std::str::FromStr as _;
use std::sync::Arc;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use tokio::sync::Mutex;

use crate::error::{Result, StoreError};
use crate::store::Store;

/// 每个用户的连接池上限。
///
/// 比单用户时的 16 小得多，而这是**故意的**：单用户部署里池子是给一个人
/// 的四路召回用的；多用户时同样的总预算要分给所有在线的人。
///
/// 4 条够一次对话用（四路召回并发 + 一条写事务），再多的收益递减 ——
/// 写事务本来就被 advisory lock 串行化。
pub const PER_TENANT_CONNECTIONS: u32 = 4;

/// 同时驻留多少个用户的池。
///
/// `4 × 20 = 80`，给 Postgres 默认的 100 留出运维连接与
/// `pg_basebackup` 的余量。**这个数是从 `max_connections` 倒推的**，
/// 不是拍脑袋 —— 改它之前先改数据库那边。
pub const MAX_RESIDENT_TENANTS: usize = 20;

/// 一个用户的 schema 名。
///
/// 新建时形如 `u_<26 位 ULID>`；**1 号用户是 `public`** —— 生产库里
/// 已经有存量数据，把 17 张表搬进新 schema 是一次没有回头路的 DDL，
/// 而它换来的只是好看。见 `migrations/20260810000002` 的文件头。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchemaName(String);

impl SchemaName {
    /// # Errors
    /// 形状不对。**这是注入面**：这个字符串会被拼进 `search_path`，
    /// 而那条语句没法用绑定参数（标识符不能参数化）。
    ///
    /// 所以这里不做转义、不做引号，只做**白名单**：转义要求每一处拼接
    /// 都记得做，而白名单只要写一次，且拼错的后果是拒绝而不是执行。
    /// # 为什么必须是**小写**
    ///
    /// `search_path` 里不带引号的标识符会被 Postgres **折成小写**。而这个
    /// 名字同时要用来 `CREATE SCHEMA "u_..."`（带引号，保留大小写）——
    /// 两边一旦不一致，`search_path` 指向的就是一个**不存在的 schema**。
    ///
    /// 而 Postgres 对 `search_path` 里不存在的 schema 是**静默跳过**的：
    /// 不报错、不警告，直接落到下一个（也就是 `public`）。实测后果是
    /// 新租户的 migration 一张表都没建、所有写入都进了 1 号用户的库，
    /// 而 `migrate()` 返回 `Ok`。
    ///
    /// 小写让「带引号」与「不带引号」两种写法等价，这条路就堵死了。
    /// ULID 用的 Crockford base32 本来就大小写不敏感，小写不丢任何信息。
    pub fn new(raw: &str) -> Result<Self> {
        let ok = raw == "public"
            || (raw.len() == 28
                && raw.starts_with("u_")
                && raw[2..].bytes().all(|b| {
                    b.is_ascii_digit() || (b.is_ascii_lowercase() && !b"ilou".contains(&b))
                }));
        if !ok {
            return Err(StoreError::Invalid(format!(
                "schema 名不合法：{raw}（只允许 public 或 u_ + 26 位**小写** ULID）"
            )));
        }
        Ok(Self(raw.to_owned()))
    }

    /// 由用户 id 派生一个新 schema 名。
    ///
    /// **转小写**，理由见 [`Self::new`] —— 大写的名字会让 `search_path`
    /// 指向一个不存在的 schema，而那是静默的。
    ///
    /// # Errors
    /// `user_id` 不是合法 ULID。
    ///
    /// **一定要校验**：这个函数一度直接拼串返回，于是一个畸形的 user_id
    /// 能造出一个连 [`Self::new`] 都过不了的 `SchemaName` —— 而这个类型的
    /// 全部价值就在于「持有它 == 已经验过」。构造函数不校验，
    /// 下游每一处（`search_path`、`pg_notify`、对象存储前缀）都得自己再验一遍，
    /// 而那正是这个 newtype 要消灭的东西。
    pub fn derive(user_id: &str) -> Result<Self> {
        Self::new(&format!("u_{}", user_id.to_ascii_lowercase()))
    }

    /// 1 号用户 / 单用户部署的 schema。
    ///
    /// 是个常量而不是 `new("public").unwrap()`：后者在每个调用点都留下一个
    /// 「这里可能 panic」的印象，而它其实永远不会。
    #[must_use]
    pub fn public() -> Self {
        Self("public".to_owned())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 这个用户的写事务锁键。
    ///
    /// `pg_advisory_xact_lock` 原来是全局单键（`SYNC_ADVISORY_LOCK_KEY`），
    /// 那把所有用户的写事务串行化了 —— 一个人导入三年历史，别人一句话
    /// 都写不进去。而那把锁保证的是「`sync_log.seq` 的顺序 == 对读端可见的
    /// 顺序」，**这件事本来就是每 schema 各论各的**。
    #[must_use]
    pub fn lock_key(&self) -> i32 {
        // 取 schema 名的稳定哈希。撞了也只是两个用户共用一把锁 ——
        // 结果仍然正确，只是那两个人的写入互相排队。所以这里不需要
        // 密码学哈希，需要的是**跨进程、跨版本稳定**
        let mut h: u32 = 2_166_136_261;
        for b in self.0.as_bytes() {
            h ^= u32::from(*b);
            h = h.wrapping_mul(16_777_619);
        }
        #[allow(clippy::cast_possible_wrap)]
        (h as i32)
    }

    /// 这个用户的 NOTIFY 通道名。
    ///
    /// 原来是全局一个 `cortex_sync`，于是任何人写入都会唤醒**所有人**的
    /// 监听器。不是正确性问题（各自按自己的游标补拉），但它让每个用户都能
    /// 观察到「别人正在写」，而且规模一上来就是全员风暴。
    #[must_use]
    pub fn notify_channel(&self) -> String {
        // 通道名同样进不了绑定参数，靠的是这个类型已经被白名单校验过。
        // 前缀取自 sync 模块，那边是监听方 —— 两处各写一个字面量的话，
        // 改了一处就是「触发器往 A 发、监听器听 B」，而那不报错，只是不刷新
        format!("{}_{}", crate::sync::NOTIFY_CHANNEL_PREFIX, self.0)
    }
}

/// 按用户取 [`Store`]，池子按需建、按 LRU 淘汰。
pub struct TenantPools {
    database_url: String,
    resident: Mutex<Resident>,
}

/// 驻留中的池子 + 使用顺序。
///
/// 用 `Mutex<..>` 而不是 `DashMap`：热路径是「命中已有的池」，而那只是一次
/// 哈希查找加一次 `Arc::clone`，锁的持有时间以纳秒计。真正慢的是**建池**
/// （一次 TCP + 认证），而那必须发生在锁外 —— 见 [`TenantPools::get`]。
struct Resident {
    pools: HashMap<SchemaName, Arc<Store>>,
    /// 最近使用在后。条目少（≤20），线性查找比再维护一个索引便宜。
    order: Vec<SchemaName>,
}

impl TenantPools {
    #[must_use]
    pub fn new(database_url: impl Into<String>) -> Self {
        Self {
            database_url: database_url.into(),
            resident: Mutex::new(Resident {
                pools: HashMap::new(),
                order: Vec::new(),
            }),
        }
    }

    /// 拿这个用户的 `Store`。没有就建一个。
    ///
    /// # Errors
    /// 连不上数据库，或者驻留的用户数已经到顶（见 [`MAX_RESIDENT_TENANTS`]）。
    pub async fn get(&self, schema: &SchemaName) -> Result<Arc<Store>> {
        // 先在锁内看一眼。命中就直接走 —— 绝大多数请求走的是这条
        {
            let mut r = self.resident.lock().await;
            if let Some(store) = r.pools.get(schema).cloned() {
                r.touch(schema);
                return Ok(store);
            }
        }

        // **建池在锁外**：它要跑一次 TCP 握手 + 认证，几十毫秒。
        // 拿着锁做这件事会让所有别的用户的请求排在后面 ——
        // 而那正是「一个人登录卡住全服」的经典形状
        let store = Arc::new(self.connect(schema).await?);

        let mut r = self.resident.lock().await;
        // 可能有另一个请求在我们建池期间也建了一个。用先到的那个，
        // 把自己这份丢掉（`Arc` 析构时池子自己关连接）——
        // 两个池指向同一个 schema 不会出错，但白占 4 条连接
        if let Some(existing) = r.pools.get(schema).cloned() {
            r.touch(schema);
            return Ok(existing);
        }
        if r.pools.len() >= MAX_RESIDENT_TENANTS {
            r.evict_oldest();
        }
        // 淘汰之后仍然满，说明上限本身就配得太小
        if r.pools.len() >= MAX_RESIDENT_TENANTS {
            return Err(StoreError::Invalid(format!(
                "同时在线的用户已达上限 {MAX_RESIDENT_TENANTS}（每人 {PER_TENANT_CONNECTIONS} 条连接）。\
                 这是从 Postgres 的 max_connections 倒推出来的 —— 要提高，先调数据库那边"
            )));
        }
        r.pools.insert(schema.clone(), Arc::clone(&store));
        r.order.push(schema.clone());
        Ok(store)
    }

    /// 建一个 `search_path` 焊死的池。
    ///
    /// `public` 留在 `search_path` 尾部是**必需的**：`vector` 扩展的类型
    /// 装在那儿，去掉之后 `::vector` 会报「type does not exist」——
    /// 一条看起来像「扩展没装」的错误。测试 harness 里踩过同一个坑。
    async fn connect(&self, schema: &SchemaName) -> Result<Store> {
        let options = PgConnectOptions::from_str(&self.database_url)
            .map_err(|e| StoreError::Invalid(format!("DATABASE_URL 不合法：{e}")))?
            .options([("search_path", format!("{},public", schema.as_str()))]);

        let pool = PgPoolOptions::new()
            .max_connections(PER_TENANT_CONNECTIONS)
            .connect_with(options)
            .await?;
        Ok(Store::from_pool_in(pool, schema.clone()))
    }

    /// 建出这片 schema（幂等）。
    ///
    /// 用 admin 连接而不是租户池：租户池的 `search_path` 指向一个还不存在的
    /// schema，而 Postgres 对此是**静默跳过**的 —— 那条连接会落到 `public`，
    /// 于是「建 schema」这个动作本身看起来成了，实际什么都没发生在正确的地方。
    ///
    /// # Errors
    /// 连不上，或者没有建 schema 的权限。
    pub async fn create_schema(&self, schema: &SchemaName) -> Result<()> {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.database_url)
            .await?;
        // AssertSqlSafe 背得起：`SchemaName` 的构造函数是白名单校验过的。
        // 双引号是必需的 —— 不带引号的标识符会被折成小写，而我们铸的名字
        // 本来就是小写，加引号只是让「建的」与「search_path 找的」逐字一致
        let sql = format!(r#"CREATE SCHEMA IF NOT EXISTS "{}""#, schema.as_str());
        let result = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&admin).await;
        admin.close().await;
        result?;
        Ok(())
    }

    /// 删掉这片 schema 与里面的一切。
    ///
    /// **只在建号中途失败时用**（schema 建了但账号没写成，那是个孤儿）。
    /// 删一个真实用户的 schema 是不可逆的数据销毁，那条路要走 purge，
    /// 而 purge 要显式触发、二次确认、留墓碑。
    ///
    /// # Errors
    /// 连不上，或者没有 DROP 的权限。
    pub async fn drop_schema(&self, schema: &SchemaName) -> Result<()> {
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&self.database_url)
            .await?;
        let sql = format!(r#"DROP SCHEMA IF EXISTS "{}" CASCADE"#, schema.as_str());
        let result = sqlx::query(sqlx::AssertSqlSafe(sql)).execute(&admin).await;
        admin.close().await;
        result?;
        Ok(())
    }

    /// 现在驻留着几个池。给 `/health` 与测试用。
    pub async fn resident_count(&self) -> usize {
        self.resident.lock().await.pools.len()
    }
}

impl Resident {
    fn touch(&mut self, schema: &SchemaName) {
        if let Some(i) = self.order.iter().position(|s| s == schema) {
            let s = self.order.remove(i);
            self.order.push(s);
        }
    }

    /// 淘汰最久没用的。
    ///
    /// 淘汰只是**丢掉引用**：正在用它的请求手上还有 `Arc`，池子要等最后
    /// 一个用完才真的关。所以淘汰不会打断在途请求，只会让下一次请求重建。
    fn evict_oldest(&mut self) {
        if self.order.is_empty() {
            return;
        }
        let victim = self.order.remove(0);
        self.pools.remove(&victim);
        tracing::info!(schema = victim.as_str(), "驻留池已满，淘汰最久未用的租户池");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **schema 名是注入面，白名单必须严。**
    ///
    /// 它会被拼进 `search_path` 与 `pg_notify` 的通道名，而标识符不能用
    /// 绑定参数。放行一个带引号或逗号的名字，等于让调用方指定要查哪个
    /// schema —— 也就是读别人的记忆。
    #[test]
    fn a_schema_name_can_never_be_anything_but_the_two_shapes_we_mint() {
        for bad in [
            "public, evil",               // 逗号 = 往 search_path 里塞第二个
            "u_01ABC\"; DROP SCHEMA x--", // 引号
            "u_01ABC'",
            "U_01arz3ndektsv4rrffq69g5fav",  // 大写前缀
            "u_01ARZ3NDEKTSV4RRFFQ69G5FAV", // 大写 ULID —— search_path 会把它折成小写，指向不存在的 schema
            "u_01arz3ndektsv4rrffq69g5fa",  // 25 位，短一位
            "u_01arz3ndektsv4rrffq69g5favx", // 27 位
            "u_01arz3ndektsv4rrffq69g5fai", // 含 i，不在 ULID 字母表里
            "u_",
            "",
            "pg_catalog",
            "information_schema",
        ] {
            assert!(
                SchemaName::new(bad).is_err(),
                "这个 schema 名必须被拒：{bad:?} —— 它会被拼进 search_path"
            );
        }

        assert!(SchemaName::new("public").is_ok(), "1 号用户就住在 public");
        assert!(SchemaName::new("u_01arz3ndektsv4rrffq69g5fav").is_ok());
    }

    /// 派生出来的名字必须能过自己的校验 —— 否则新用户建完就用不了。
    #[test]
    fn a_derived_name_passes_its_own_validation() {
        // ULID 本身是大写的（`Id::new().to_string()` 就是），派生时必须转小写
        let s = SchemaName::derive("01ARZ3NDEKTSV4RRFFQ69G5FAV").expect("合法 ULID");
        assert_eq!(
            s.as_str(),
            "u_01arz3ndektsv4rrffq69g5fav",
            "派生出大写名字的话，search_path 会指向一个不存在的 schema ——              而 Postgres 对此是静默跳过的，实测后果是写进了别人的库"
        );
        assert!(
            SchemaName::new(s.as_str()).is_ok(),
            "自己派生的名字过不了自己的校验，说明两处规则漂开了"
        );

        // 畸形输入必须在这里就被挡住，而不是造出一个「验过」的假象
        for bad in ["", "not-a-ulid", "01ARZ3NDEKTSV4RRFFQ69G5FA", "../etc"] {
            assert!(
                SchemaName::derive(bad).is_err(),
                "derive 放行了畸形 user_id：{bad:?} —— 这个类型的全部价值就是「持有它 == 已经验过」"
            );
        }
    }

    /// 锁键与通道名都必须**只由 schema 决定**，且不同用户不同。
    ///
    /// 前半句是跨进程要求：两个 cortexd 进程算出不同的键，那把锁就白上了，
    /// 而症状是 `sync_log` 偶发漏行 —— 极难查。
    #[test]
    fn the_lock_key_and_channel_are_stable_and_per_tenant() {
        let a = SchemaName::new("u_01arz3ndektsv4rrffq69g5fav").unwrap();
        let b = SchemaName::new("u_01bx5zzkbkactav9wevgemmvrz").unwrap();

        assert_eq!(
            a.lock_key(),
            a.lock_key(),
            "同一个 schema 必须每次都算出同一个键"
        );
        assert_ne!(
            a.lock_key(),
            b.lock_key(),
            "不同用户拿到同一把锁只是互相排队（仍然正确），但这里应当分开"
        );
        // 钉死字面量：换一个哈希实现会让**已经在跑的两个进程**算出不同的键，
        // 而那时锁就形同虚设。改它必须是有意识的
        assert_eq!(
            a.lock_key(),
            304_439_192,
            "锁键的算法变了，跨进程一致性会断"
        );

        assert_eq!(
            a.notify_channel(),
            "cortex_sync_u_01arz3ndektsv4rrffq69g5fav"
        );
        assert_ne!(a.notify_channel(), b.notify_channel());
    }
}

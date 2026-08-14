//! 同步查询侧：`GET /sync?since=` 的底层，以及它的实时事件源。
//!
//! 客户端只持有**一个**游标（`sync_log.seq`），跨全部表。全序由 `sync_log`
//! 提供，按 log 序回放天然满足 FK 顺序（facts 永远在其 source episode 之后到达）。
//!
//! 写入侧的 advisory lock（见 [`crate::txn`]）保证 seq 顺序 == 可见顺序，
//! 因此「拉到 seq=N 就把游标推到 N」是安全的 —— 不会有 seq < N 的行在此之后
//! 才变得可见。
//!
//! 拉取之外还有一条推送链路（[`Store::listen_sync`]）：`sync_log` 的插入触发
//! Postgres `NOTIFY`，服务端据此把「游标推进了」这个**信号**扇出给各端。
//! 它与拉取共用同一个游标语义，不是第二套协议。

use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::postgres::{PgListener, PgPool, PgPoolOptions};

use crate::error::{Result, StoreError};
use crate::model::{
    Blob, BlobTranscript, Derivation, Entity, EntityMerge, Episode, EpisodeBlob, EpisodeMemory,
    EpisodeToolCall, Fact, FactEvent, ProjectEvent, Redaction, SessionEvent, Summary, fact_columns,
    table,
};
use crate::store::Store;

/// 一条同步记录：`sync_log` 行 + 它指向的业务行。
#[derive(Debug, Clone, PartialEq)]
pub struct SyncRecord {
    pub seq: i64,
    pub table_name: String,
    pub record_id: String,
    pub logged_at: DateTime<Utc>,
    /// 业务行。理论上永不为 `None` —— 日志行与业务行同事务提交，
    /// 日志可见即业务行可见。为 `None` 说明有人绕过本层删了数据。
    pub payload: Option<SyncPayload>,
}

/// 业务行的类型化载荷。变体与 `sync_log.table_name` 一一对应。
#[derive(Debug, Clone, PartialEq)]
pub enum SyncPayload {
    Episode(Episode),
    Blob(Blob),
    EpisodeBlob(EpisodeBlob),
    EpisodeMemory(EpisodeMemory),
    EpisodeToolCall(EpisodeToolCall),
    BlobTranscript(BlobTranscript),
    Entity(Entity),
    EntityMerge(EntityMerge),
    Fact(Fact),
    FactEvent(FactEvent),
    Summary(Summary),
    Derivation(Derivation),
    Redaction(Redaction),
    SessionEvent(SessionEvent),
    ProjectEvent(ProjectEvent),
}

impl SyncPayload {
    /// 该载荷所属的表名。
    #[must_use]
    pub const fn table_name(&self) -> &'static str {
        match self {
            Self::Episode(_) => table::EPISODES,
            Self::Blob(_) => table::BLOBS,
            Self::EpisodeBlob(_) => table::EPISODE_BLOBS,
            Self::EpisodeMemory(_) => table::EPISODE_MEMORIES,
            Self::EpisodeToolCall(_) => table::EPISODE_TOOL_CALLS,
            Self::BlobTranscript(_) => table::BLOB_TRANSCRIPTS,
            Self::Entity(_) => table::ENTITIES,
            Self::EntityMerge(_) => table::ENTITY_MERGES,
            Self::Fact(_) => table::FACTS,
            Self::FactEvent(_) => table::FACT_EVENTS,
            Self::Summary(_) => table::SUMMARIES,
            Self::Derivation(_) => table::DERIVATIONS,
            Self::Redaction(_) => table::REDACTIONS,
            Self::SessionEvent(_) => table::SESSION_EVENTS,
            Self::ProjectEvent(_) => table::PROJECT_EVENTS,
        }
    }
}

/// `sync_log` 的裸行。
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct SyncLogEntry {
    pub seq: i64,
    pub table_name: String,
    pub record_id: String,
    pub created_at: DateTime<Utc>,
}

impl Store {
    /// 当前同步全序的末端。新设备从 0 起拉，老设备用自己的游标。
    pub async fn latest_seq(&self) -> Result<i64> {
        let (seq,): (i64,) = sqlx::query_as("SELECT coalesce(max(seq), 0) FROM sync_log")
            .fetch_one(self.pool())
            .await?;
        Ok(seq)
    }

    /// 只取日志行，不取业务行。给「先看看有没有新东西」这类轻量探测用。
    pub async fn sync_log_since(&self, cursor: i64, limit: i64) -> Result<Vec<SyncLogEntry>> {
        let rows = sqlx::query_as::<_, SyncLogEntry>(
            "SELECT seq, table_name, record_id, created_at
               FROM sync_log WHERE seq > $1 ORDER BY seq ASC LIMIT $2",
        )
        .bind(cursor)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 增量拉取：`seq > cursor` 的记录及其业务行，按 seq 升序。
    ///
    /// 实现上是「一次取日志 + 每张涉及的表各取一次业务行」，而不是逐行 N+1。
    pub async fn fetch_since(&self, cursor: i64, limit: i64) -> Result<Vec<SyncRecord>> {
        let entries = self.sync_log_since(cursor, limit).await?;
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        // 按表归拢待取的主键
        let mut wanted: HashMap<String, Vec<String>> = HashMap::new();
        for entry in &entries {
            wanted
                .entry(entry.table_name.clone())
                .or_default()
                .push(entry.record_id.clone());
        }

        // (table_name, record_id) → 业务行
        let mut payloads: HashMap<(String, String), SyncPayload> = HashMap::new();
        for (table_name, ids) in &wanted {
            self.load_payloads(table_name, ids, &mut payloads).await?;
        }

        Ok(entries
            .into_iter()
            .map(|entry| {
                let payload = payloads
                    .get(&(entry.table_name.clone(), entry.record_id.clone()))
                    .cloned();
                SyncRecord {
                    seq: entry.seq,
                    table_name: entry.table_name,
                    record_id: entry.record_id,
                    logged_at: entry.created_at,
                    payload,
                }
            })
            .collect())
    }

    /// 批量取一张表的业务行，塞进 `out`。
    async fn load_payloads(
        &self,
        table_name: &str,
        ids: &[String],
        out: &mut HashMap<(String, String), SyncPayload>,
    ) -> Result<()> {
        // `$sql` 收 `expr` 而不是 `literal`：facts 那一支用 `concat!` 拼列名，
        // 而 `concat!(...)` 在展开前还是一个表达式，匹配不上 `literal`
        macro_rules! collect {
            ($sql:expr, $ty:ty, $key:expr, $wrap:expr) => {{
                let rows = sqlx::query_as::<_, $ty>($sql)
                    .bind(ids)
                    .fetch_all(self.pool())
                    .await?;
                for row in rows {
                    #[allow(clippy::redundant_closure_call)]
                    let key = ($key)(&row);
                    #[allow(clippy::redundant_closure_call)]
                    out.insert((table_name.to_owned(), key), ($wrap)(row));
                }
            }};
        }

        match table_name {
            table::EPISODES => collect!(
                "SELECT id, session_id, role, content, text, domain, device_id,
                        occurred_at, created_at
                   FROM episodes WHERE id = ANY($1)",
                Episode,
                |row: &Episode| row.id.clone(),
                SyncPayload::Episode
            ),
            table::BLOBS => collect!(
                "SELECT hash, mime, size_bytes, storage_key, created_at
                   FROM blobs WHERE hash = ANY($1)",
                Blob,
                |row: &Blob| row.hash.clone(),
                SyncPayload::Blob
            ),
            table::EPISODE_BLOBS => collect!(
                "SELECT episode_id, blob_hash, kind, filename FROM episode_blobs
                   WHERE episode_id || ':' || blob_hash = ANY($1)",
                EpisodeBlob,
                |row: &EpisodeBlob| format!("{}:{}", row.episode_id, row.blob_hash),
                SyncPayload::EpisodeBlob
            ),
            // 回放抽屉的两张表。漏掉任一支的后果与 session_events 那条注释
            // 说的一样：一条日志就让整批 /sync 拉取失败，客户端游标从此卡死
            table::EPISODE_MEMORIES => collect!(
                "SELECT id, episode_id, fact_id, ordinal, channels, score, device_id, created_at
                   FROM episode_memories WHERE id = ANY($1)",
                EpisodeMemory,
                |row: &EpisodeMemory| row.id.clone(),
                SyncPayload::EpisodeMemory
            ),
            table::EPISODE_TOOL_CALLS => collect!(
                "SELECT id, episode_id, ordinal, name, path, summary, ok, device_id, diff,
                        created_at
                   FROM episode_tool_calls WHERE id = ANY($1)",
                EpisodeToolCall,
                |row: &EpisodeToolCall| row.id.clone(),
                SyncPayload::EpisodeToolCall
            ),
            table::BLOB_TRANSCRIPTS => collect!(
                "SELECT id, blob_hash, kind, text, embedding, span_start_ms, span_end_ms,
                        transcribed_by, embedding_model, created_at
                   FROM blob_transcripts WHERE id = ANY($1)",
                BlobTranscript,
                |row: &BlobTranscript| row.id.clone(),
                SyncPayload::BlobTranscript
            ),
            table::ENTITIES => collect!(
                "SELECT id, kind, name, summary, embedding, embedding_model, device_id, created_at
                   FROM entities WHERE id = ANY($1)",
                Entity,
                |row: &Entity| row.id.clone(),
                SyncPayload::Entity
            ),
            table::ENTITY_MERGES => collect!(
                "SELECT id, from_entity, into_entity, reason, source_episode_id,
                        device_id, created_at
                   FROM entity_merges WHERE id = ANY($1)",
                EntityMerge,
                |row: &EntityMerge| row.id.clone(),
                SyncPayload::EntityMerge
            ),
            table::FACTS => collect!(
                concat!(
                    "SELECT ",
                    fact_columns!(),
                    "  FROM facts WHERE id = ANY($1)"
                ),
                Fact,
                |row: &Fact| row.id.clone(),
                SyncPayload::Fact
            ),
            table::FACT_EVENTS => collect!(
                "SELECT id, fact_id, op, kind, invalid_at, superseded_by, actor, reason,
                        source_episode_id, device_id, created_at
                   FROM fact_events WHERE id = ANY($1)",
                FactEvent,
                |row: &FactEvent| row.id.clone(),
                SyncPayload::FactEvent
            ),
            table::SUMMARIES => collect!(
                "SELECT id, scope, scope_key, text, embedding, embedding_model,
                        covers_from, covers_to, device_id, created_at
                   FROM summaries WHERE id = ANY($1)",
                Summary,
                |row: &Summary| row.id.clone(),
                SyncPayload::Summary
            ),
            // 派生血缘。客户端拿它做的事与服务端一样：收到 redaction 墓碑后
            // 顺着边找到本地那些必须一并失效的摘要 / 图边（memory.md §九 的
            // 传播义务）。不下发的话客户端只能清源、清不掉派生物
            table::DERIVATIONS => collect!(
                "SELECT id, derived_kind, derived_id, source_kind, source_id,
                        device_id, created_at
                   FROM derivations WHERE id = ANY($1)",
                Derivation,
                |row: &Derivation| row.id.clone(),
                SyncPayload::Derivation
            ),
            table::REDACTIONS => collect!(
                "SELECT id, target_kind, target_id, mode, reason, actor, device_id, created_at
                   FROM redactions WHERE id = ANY($1)",
                Redaction,
                |row: &Redaction| row.id.clone(),
                SyncPayload::Redaction
            ),
            // 漏掉这一支的后果不是「会话改名同步不了」，而是**整条 /sync 断掉**：
            // 一条 session_events 日志会让 load_payloads 返回 UnknownTable，
            // 那一批拉取整个失败，客户端的游标从此卡死在它前面
            table::SESSION_EVENTS => collect!(
                "SELECT id, session_id, op, title, workspace, project_id, runtime,
                        actor, device_id, created_at
                   FROM session_events WHERE id = ANY($1)",
                SessionEvent,
                |row: &SessionEvent| row.id.clone(),
                SyncPayload::SessionEvent
            ),
            // 项目事件同理。客户端拿它按同一套状态机自己算末态 ——
            // 服务端不下发「当前项目列表」，那种快照没有全序可言
            table::PROJECT_EVENTS => collect!(
                "SELECT id, project_id, op, name, actor, device_id, created_at
                   FROM project_events WHERE id = ANY($1)",
                ProjectEvent,
                |row: &ProjectEvent| row.id.clone(),
                SyncPayload::ProjectEvent
            ),
            other => return Err(StoreError::UnknownTable(other.to_owned())),
        }

        Ok(())
    }
}

// ══════════════════════════════════════════════════════════
//  实时事件源：LISTEN / NOTIFY 挂在 sync_log 上
// ══════════════════════════════════════════════════════════
//
// 这是 sync_log outbox 设计的免费附带收益（docs/memory.md §九）：写入侧
// 已经保证「每笔业务写事务都往 sync_log 追加一行」且「seq 顺序 == 可见
// 顺序」，那么监听这张表的插入就等于监听了**全部**数据变更，一张表都不会漏。
//
// # 为什么走数据库触发器，而不是在应用层写完 pg_notify
//
// 两条路都能通，取舍是「谁保证不漏」：
//
// - **应用层 notify**（在 `write_txn` 之后手动 `SELECT pg_notify`）要求
//   **每一条写入路径**都记得调。对话路径好办，但事实抽取跑在
//   `cortex-memory::extract` 里、将来的摘要与 redact 执行器也各自写库 ——
//   每加一个写入方就多一个漏掉的机会，而漏掉的表现是「某台设备偶尔不刷新」，
//   几乎不可能在测试里复现。
// - **触发器**挂在 `sync_log` 这个所有写入的必经之路上。写入纪律已经用类型
//   系统强制了「业务行必与 sync_log 同事务」（见 [`crate::txn`]），于是触发器
//   天然覆盖全部写入方，包括还没写出来的那些。
//
// 还有一条不明显但决定性的好处：Postgres 的 NOTIFY **在事务提交时才投递**。
// 触发器在写事务内触发、随该事务一起提交，因此「收到通知」严格晚于
// 「数据可见」。应用层 notify 若写在事务外，就会出现「通知到了、拉取却看不到
// 那一行」的竞态。
//
// # 为什么这段 DDL 不进 migration
//
// migration 是 schema 的权威版本、只增不改；这段 DDL 是**运行时组件**的一部分：
// 幂等、可重复执行、换实现时不需要写反向 migration。

/// 通道名的**前缀**。真正的通道名是 `cortex_sync_<schema>`，
/// 由 [`crate::SchemaName::notify_channel`] 拼出来。
///
/// 改这里等于改服务端内部约定，客户端不需要知道它（客户端只连 `/ws`）。
///
/// # 为什么必须带上 schema
///
/// 通道名的作用域是**数据库**而非 schema。单用户时这只是「同一个库里跑
/// 两套 Cortex 会互相收到通知」，无害 —— 信号只说「去拉一次」，游标各自独立。
///
/// 多租户之后同一件事变成两个问题：一是**任何人写入都会唤醒所有人**，
/// 一百个用户就是一百倍的空拉；二是每个用户都能观察到「别人正在写」，
/// 那是个不该有的旁路信道。
pub(crate) const NOTIFY_CHANNEL_PREFIX: &str = "cortex_sync";

/// 安装触发器时用的 advisory lock key。
///
/// 刻意不复用写事务那把 [`crate::SYNC_ADVISORY_LOCK_KEY`]：那把锁的语义是
/// 「同步取号串行化」，拿它来串行化 DDL 会让两件毫不相干的事互相阻塞，
/// 日后排查锁等待时误导人。
const DDL_LOCK_KEY: i64 = 4273;

const RECONNECT_MIN: Duration = Duration::from_secs(1);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// 通知函数的 DDL。
///
/// 通道名现在要拼进来（每租户一个），所以它从常量变成了函数。
///
/// **拼字符串进 DDL 本来是坏习惯**，这里立得住的唯一理由是：`channel`
/// 来自 [`crate::SchemaName::notify_channel`]，而那个类型的构造函数是
/// 白名单校验过的（只可能是 `public` 或 `u_` + 26 位小写 ULID）。
/// 安全性来自**类型**，不是来自这一行。
///
/// 函数本身也建在租户 schema 里（靠连接的 `search_path`），
/// 所以不同租户的 `cortex_notify_sync()` 互不覆盖。
fn create_function_sql(channel: &str) -> String {
    format!(
        "CREATE OR REPLACE FUNCTION cortex_notify_sync() RETURNS trigger
LANGUAGE plpgsql AS $fn$
BEGIN
    -- 载荷只带 seq。NOTIFY 载荷上限 8000 字节，塞业务行迟早会炸；
    -- 更要紧的是客户端本来就该按自己的游标去 /sync 拉
    PERFORM pg_notify('{channel}', NEW.seq::text);
    RETURN NULL;
END;
$fn$"
    )
}

/// 触发器的 DDL。
///
/// 先查存在性再建，不用 `DROP TRIGGER IF EXISTS` + `CREATE`：DROP 会取
/// `sync_log` 的 ACCESS EXCLUSIVE 锁，把所有在途写事务堵住。启动是高频动作
/// （开发期每次改代码都重启），没必要每次都去抖一下写入路径。
const CREATE_TRIGGER_SQL: &str = "\
DO $do$
BEGIN
    -- 存在性检查必须绑到具体的表上（'sync_log'::regclass 按 search_path 解析），
    -- 光比 tgname 会让「另一个 schema 里已经有同名触发器」被误判成已装好 ——
    -- 集成测试跑在临时 schema 里，正是这个场景
    IF NOT EXISTS (
        SELECT 1 FROM pg_trigger
         WHERE tgname = 'cortex_sync_notify'
           AND NOT tgisinternal
           AND tgrelid = 'sync_log'::regclass
    ) THEN
        CREATE TRIGGER cortex_sync_notify
            AFTER INSERT ON sync_log
            FOR EACH ROW EXECUTE FUNCTION cortex_notify_sync();
    END IF;
END
$do$";

/// 一次「游标推进」的信号。
///
/// **只是信号，不带业务数据**。收到后按自己的游标去 [`Store::fetch_since`]
/// 拉 —— 事件里的 `cursor` 只用于判断落后多少与展示，绝不能拿它当下次拉取的
/// 起点（那样会跳过自己还没拉的区间）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncSignal {
    /// 有新数据，服务端游标已推进到 `cursor`。
    Bump { cursor: i64 },
    /// 中间可能漏过若干次推进 —— 与 Postgres 的通知连接断过。
    ///
    /// 语义与 [`Self::Bump`] 相同（都是「去拉」），单列一个变体是为了让上层能把
    /// 「正常增量」与「可能有缺口」分开度量：后者频繁出现就是运维信号。
    Resync { cursor: i64 },
}

/// `sync_log` 推进通知的订阅句柄，由 [`Store::listen_sync`] 交出。
///
/// 一个句柄独占存储层连接池里的一条连接（`LISTEN` 是连接级状态，
/// 池化连接上做不了）。这是 daemon 里唯一的常驻长连接，池子按此留了余量。
pub struct SyncNotifications {
    store: Store,
    /// `None` 表示尚未连上或刚断开，下一次 [`Self::recv`] 会去（重）连。
    listener: Option<PgListener>,
    backoff: Duration,
}

impl std::fmt::Debug for SyncNotifications {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncNotifications")
            .field("connected", &self.listener.is_some())
            .field("backoff", &self.backoff)
            .finish_non_exhaustive()
    }
}

impl SyncNotifications {
    /// 等下一条信号。
    ///
    /// **不返回错误**：连接断了在内部指数退避重连，重连成功后先补一条
    /// [`SyncSignal::Resync`]（断开期间的通知永久丢失，必须让客户端主动追平 ——
    /// 这正是「只推信号、不推数据」的价值：补救只有一句话）。
    ///
    /// 返回 `None` **只有一个原因**：存储层连接池已关闭，即进程正在退出。
    /// 上层据此结束转发任务，而不是对着一个永远连不上的池子退避到天荒地老。
    pub async fn recv(&mut self) -> Option<SyncSignal> {
        loop {
            if self.store.pool().is_closed() {
                return None;
            }

            if self.listener.is_none() {
                let channel = self.store.schema().notify_channel();
                match subscribe(self.store.pool(), &channel).await {
                    Ok(listener) => {
                        self.backoff = RECONNECT_MIN;
                        self.listener = Some(listener);
                        tracing::info!(channel = %channel, "已订阅 Postgres 通知通道");
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            backoff_secs = self.backoff.as_secs(),
                            "订阅通知通道失败"
                        );
                        self.sleep_backoff().await;
                        continue;
                    }
                }
            }

            // 监听连接不在主池子里（见 [`subscribe`]），主池子关掉时不会打断
            // `try_recv`。因此显式跟池子的关闭事件赛跑 —— 否则进程退出时这个
            // 任务会一直挂在一条没人再写的连接上。
            let closed = self.store.pool().close_event();

            // 单独取出结果再 match：否则 listener 的可变借用会一直活到 match 体内，
            // 而分支里要用 &self.store 去查游标
            let received = {
                let listener = self.listener.as_mut().expect("上一步保证了它已就位");
                let closed = std::pin::pin!(closed);
                tokio::select! {
                    received = listener.try_recv() => received,
                    () = closed => return None,
                }
            };

            match received {
                Ok(Some(note)) => {
                    let cursor = match note.payload().parse::<i64>() {
                        Ok(v) => v,
                        // 载荷不是数字说明有别人在往同一个通道发东西。
                        // 不猜、去库里问一次真实游标
                        Err(_) => self.cursor_or_zero().await,
                    };
                    return Some(SyncSignal::Bump { cursor });
                }
                // try_recv 返回 None = 连接断过且已重连
                Ok(None) => {
                    let cursor = self.cursor_or_zero().await;
                    tracing::warn!(cursor, "通知连接曾中断，降级为 resync");
                    return Some(SyncSignal::Resync { cursor });
                }
                Err(e) => {
                    // 池子关闭也会走到这里（PoolClosed）。下一圈开头的 is_closed
                    // 会认出它并收工，不会白退避
                    self.listener = None;
                    tracing::warn!(error = %e, "通知连接出错，将重连");
                    self.sleep_backoff().await;
                }
            }
        }
    }

    /// 查当前游标末端。查不到时返回 0 —— 上层拿到偏小的 cursor 只会多拉一次，
    /// 拿到偏大的才会漏，所以失败必须往小了取。
    async fn cursor_or_zero(&self) -> i64 {
        self.store.latest_seq().await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "读取 latest_seq 失败，按 0 下发");
            0
        })
    }

    async fn sleep_backoff(&mut self) {
        tokio::time::sleep(self.backoff).await;
        self.backoff = (self.backoff * 2).min(RECONNECT_MAX);
    }
}

/// 用主池子的**连接选项**另建一个只放一条连接的池子，在其上订阅通道。
///
/// # 为什么不直接 `PgListener::connect_with(主池子)`
///
/// `LISTEN` 是连接级状态，监听会把那条连接永久占住。从 16 条的主池里少一条
/// 是小事，真正的问题是 [`Store::close`] 要等所有连接归还，而监听那条只在
/// `try_recv` 被轮询到时才归还 —— 谁要是在同一个任务里先 `await close()`
/// 再 `await recv()`，两边互等，直接死锁。实测踩过。
///
/// # 为什么不重新解析连接串
///
/// 复用 `connect_options` 意味着 TLS、`search_path` 等一切选项自动跟上。
/// 拿连接串重拼是「监听连的库/schema 与写入的不是同一个」这类问题的来源，
/// 而它的症状是「一切正常，就是不推送」。
async fn subscribe(pool: &PgPool, channel: &str) -> Result<PgListener> {
    let options = pool.connect_options().as_ref().clone();
    let dedicated = PgPoolOptions::new()
        .max_connections(1)
        // 监听连接必须一直活着：池子的生命周期上限与空闲回收在这里只会造成
        // 「静默换连接」，而换连接的瞬间通知就漏了，且不报错
        .max_lifetime(None)
        .idle_timeout(None)
        .connect_with(options)
        .await?;

    let mut listener = PgListener::connect_with(&dedicated).await?;
    // 这个池子是监听专用的，除了它自己没人持有，也就没人会去 close 它
    listener.ignore_pool_close_event(true);
    listener.listen(channel).await?;
    Ok(listener)
}

impl Store {
    /// 幂等地安装 `sync_log` 的通知触发器。
    ///
    /// 函数用 `CREATE OR REPLACE`，触发器先查存在性再建；两者都在一个持有
    /// [`DDL_LOCK_KEY`] 的事务里，于是并发启动的多个实例会排队而不是撞名字。
    ///
    /// # Errors
    /// 权限不足或连不上数据库时返回 [`StoreError`]。
    pub async fn install_sync_notify(&self) -> Result<()> {
        let mut tx = self.pool().begin().await?;

        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(DDL_LOCK_KEY)
            .execute(&mut *tx)
            .await?;

        let channel = self.schema().notify_channel();
        // AssertSqlSafe 背得起：`channel` 由 `SchemaName::notify_channel`
        // 拼出，而那个类型的构造函数是白名单校验过的。安全性来自类型
        sqlx::query(sqlx::AssertSqlSafe(create_function_sql(&channel)))
            .execute(&mut *tx)
            .await?;
        sqlx::query(CREATE_TRIGGER_SQL).execute(&mut *tx).await?;

        tx.commit().await?;
        tracing::info!(channel = %channel, "sync_log 通知触发器就绪");
        Ok(())
    }

    /// 安装触发器并订阅「游标推进」通知。
    ///
    /// 触发器安装失败**不**中止订阅：它可能只是已被同集群的另一个实例建好，
    /// 而即使真的没建上，返回一个永不出声的订阅也比让调用方启动失败要好 ——
    /// `GET /sync` 轮询这条底线路径不依赖推送。失败以 `error` 级别记日志，
    /// 因为它的后果（推送静默失效）在界面上只表现为「偶尔不刷新」。
    pub async fn listen_sync(&self) -> SyncNotifications {
        if let Err(e) = self.install_sync_notify().await {
            tracing::error!(error = %e, "安装 sync_log 通知触发器失败，实时推送可能不可用");
        }
        SyncNotifications {
            store: self.clone(),
            listener: None,
            backoff: RECONNECT_MIN,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_and_channel_name_agree() {
        // 通道名在 DDL 里是字面量、在 Rust 里是常量，两处必须一致，
        // 否则触发器往一个没人听的通道发消息 —— 症状是「静默不推送」
        assert!(
            create_function_sql("cortex_sync_public").contains("'cortex_sync_public'"),
            "DDL 里没有拼进通道名 —— 触发器会往一个没人监听的通道上发"
        );
    }

    #[test]
    fn ddl_lock_key_differs_from_the_sync_write_lock() {
        assert_ne!(
            DDL_LOCK_KEY,
            i64::from(crate::SYNC_ADVISORY_LOCK_KEY),
            "复用写事务那把锁会让建 DDL 与同步取号互相阻塞"
        );
    }

    #[test]
    fn backoff_is_bounded() {
        // 退避无上界的话，一次长时间的数据库维护会让重连间隔涨到几小时，
        // 数据库恢复后界面却要等很久才重新有推送
        let mut d = RECONNECT_MIN;
        for _ in 0..20 {
            d = (d * 2).min(RECONNECT_MAX);
        }
        assert_eq!(d, RECONNECT_MAX, "退避应收敛到上限而不是无限增长");
    }
}

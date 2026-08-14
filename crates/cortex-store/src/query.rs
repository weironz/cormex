//! 读路径。
//!
//! 全部 SQL 都是字面量（sqlx 0.9 的 `SqlSafeStr` 在类型层面就拒绝拼接出来的
//! 查询串），列名逐个写全而不用 `SELECT *` —— `tsv` 是纯派生列，
//! 没有 Rust 类型也没有读回来的必要，不该白白占用带宽。

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::model::{
    Blob, BlobTranscript, CanonicalEntity, Entity, EntityMerge, Episode, EpisodeAttachment,
    EpisodeToolCall, Fact, FactEvent, FactStatus, InjectedMemory, Redaction, RedactionTarget,
    Summary, SummaryScope, fact_columns,
};
use crate::recall::MAX_LIMIT;
use crate::store::Store;

/// 会话消息分页的游标：`(occurred_at, id)`。
///
/// # 为什么不能只用 `occurred_at`
///
/// `occurred_at` 来自客户端时钟，同一微秒落两行是常态（一次工具调用连着落
/// user + tool 就够了）。单列游标在同刻的那几行上要么跳过、要么重复 ——
/// 两种症状在界面上都只表现为「历史看着怪怪的」，没人会去怀疑分页。
///
/// `id` 是 ULID（`COLLATE "C"` 逐字节可比），做同刻的确定性 tiebreaker。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpisodeCursor {
    pub occurred_at: DateTime<Utc>,
    pub id: String,
}

impl Store {
    // ── L0：episodes ───────────────────────────────────────

    pub async fn episode(&self, id: &str) -> Result<Option<Episode>> {
        let row = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// 一次会话内的消息，按发生时间升序（走 `idx_episodes_sess`）。
    ///
    /// **只适合确定条数有限的场合**（测试、导出小会话）。会话详情那条路
    /// 一律走 [`Self::episodes_by_session_page`] —— 升序 + LIMIT 在长会话上
    /// 截掉的是**最新**那些消息，用户看到一段没有结尾的对话，而且看不出被截断了。
    pub async fn episodes_by_session(&self, session_id: &str, limit: i64) -> Result<Vec<Episode>> {
        let rows = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes WHERE session_id = $1
              ORDER BY occurred_at ASC, id ASC
              LIMIT $2",
        )
        .bind(session_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 会话消息的一页，**从新到旧**（走 `idx_episodes_sess_desc`）。
    ///
    /// `before` 为 `None` 时取最新的一页；否则取严格早于该游标的一页。
    /// 返回顺序是**降序**（最新在前）—— 这是分页的自然方向，调用方要正序
    /// 渲染就自己反转（cortexd 就是这么做的，理由见那边的注释）。
    ///
    /// # 为什么游标要落在两列上
    ///
    /// 见 [`EpisodeCursor`]。这里的比较写成
    /// `occurred_at < $2 OR (occurred_at = $2 AND id < $3)` 而不是行比较
    /// `(occurred_at, id) < ($2, $3)`：后者要求两侧的排序规则可确定，而 `id`
    /// 是 `COLLATE "C"` 的域、绑定参数却是默认排序规则，Postgres 会在
    /// 某些版本上直接拒绝这个比较。拆开写让排序规则只由列那一侧决定。
    pub async fn episodes_by_session_page(
        &self,
        session_id: &str,
        limit: i64,
        before: Option<&EpisodeCursor>,
    ) -> Result<Vec<Episode>> {
        let rows = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes
              WHERE session_id = $1
                AND ($2::timestamptz IS NULL
                     OR occurred_at < $2::timestamptz
                     OR (occurred_at = $2::timestamptz AND id < $3))
              ORDER BY occurred_at DESC, id DESC
              LIMIT $4",
        )
        .bind(session_id)
        .bind(before.map(|c| c.occurred_at))
        .bind(before.map(|c| c.id.as_str()))
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 时间近因那一路召回：最近 N 条（走 `idx_episodes_time`）。
    pub async fn recent_episodes(&self, limit: i64) -> Result<Vec<Episode>> {
        let rows = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes ORDER BY occurred_at DESC, id DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 按事件时间开区间回溯，升序返回。
    pub async fn episodes_in_range(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: i64,
    ) -> Result<Vec<Episode>> {
        let rows = sqlx::query_as::<_, Episode>(
            "SELECT id, session_id, role, content, text, domain, device_id, occurred_at, created_at
               FROM episodes
              WHERE occurred_at >= $1 AND occurred_at < $2
              ORDER BY occurred_at ASC, id ASC
              LIMIT $3",
        )
        .bind(from)
        .bind(to)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── L0：blobs ──────────────────────────────────────────

    pub async fn blob(&self, hash: &str) -> Result<Option<Blob>> {
        let row = sqlx::query_as::<_, Blob>(
            "SELECT hash, mime, size_bytes, storage_key, created_at FROM blobs WHERE hash = $1",
        )
        .bind(hash)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// 一条消息的附件，**连同内容元信息**（mime / 大小）。
    ///
    /// 内连 `blobs` 而不是左连：`episode_blobs.blob_hash` 有外键指向
    /// `blobs.hash`，缺行是不可能的；写成 LEFT JOIN 只会让调用方为一个
    /// 永远不会出现的 `Option` 写分支。
    pub async fn episode_attachments(&self, episode_id: &str) -> Result<Vec<EpisodeAttachment>> {
        let rows = sqlx::query_as::<_, EpisodeAttachment>(
            "SELECT eb.episode_id, eb.blob_hash, eb.kind, eb.filename, b.mime, b.size_bytes
               FROM episode_blobs eb
               JOIN blobs b ON b.hash = eb.blob_hash
              WHERE eb.episode_id = $1 ORDER BY eb.blob_hash ASC",
        )
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// blob 是否还被别的 episode 引用 —— purge 前必须问这一句。
    pub async fn blob_reference_count(&self, blob_hash: &str) -> Result<i64> {
        let (count,): (i64,) =
            sqlx::query_as("SELECT count(*) FROM episode_blobs WHERE blob_hash = $1")
                .bind(blob_hash)
                .fetch_one(self.pool())
                .await?;
        Ok(count)
    }

    /// 媒体转录，按片段偏移升序（图片 / OCR 的偏移为 NULL，排在最后）。
    pub async fn blob_transcripts(&self, blob_hash: &str) -> Result<Vec<BlobTranscript>> {
        let rows = sqlx::query_as::<_, BlobTranscript>(
            "SELECT id, blob_hash, kind, text, embedding, span_start_ms, span_end_ms,
                    transcribed_by, embedding_model, created_at
               FROM blob_transcripts WHERE blob_hash = $1
              ORDER BY span_start_ms ASC NULLS LAST, id ASC",
        )
        .bind(blob_hash)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 待补转录的 blob：MIME 前缀匹配、且还没有任何 `blob_transcripts` 行。
    ///
    /// 预签名直传那条路上服务端从不经手字节，因此上传时无从触发转录 ——
    /// 大文件成了纯归档，检索一条也命中不了。后台回扫器以 `blobs` 表为
    /// 权威清单，把这批捡回来补跑。
    ///
    /// # 为什么已被抹除的必须在 SQL 里排掉
    ///
    /// 不排的话，回扫器会**周期性地**把已 redact 的 blob 从对象存储拉回内存、
    /// 送进 vision 模型 —— 即使管线那侧的 [`crate::Store::is_redacted`] 复检
    /// 最终会丢弃结果，秘密也已经出过一次进程、上过一次网。redact 的销毁承诺
    /// 经不起这个。
    ///
    /// `mime_prefixes` 是 SQL `LIKE` 模式（如 `image/%`）。它只是**粗筛**，
    /// 真正认领与否由转录管线决定 —— 粗筛存在的理由是不要把全表拉进内存。
    pub async fn blobs_missing_transcript(
        &self,
        mime_prefixes: &[String],
        limit: i64,
    ) -> Result<Vec<Blob>> {
        let rows = sqlx::query_as::<_, Blob>(
            "SELECT b.hash, b.mime, b.size_bytes, b.storage_key, b.created_at
               FROM blobs b
              WHERE b.mime LIKE ANY($1)
                AND NOT EXISTS (
                    SELECT 1 FROM blob_transcripts t WHERE t.blob_hash = b.hash)
                AND NOT EXISTS (
                    SELECT 1 FROM redactions r
                     WHERE r.target_kind = 'blob' AND r.target_id = b.hash)
              ORDER BY b.created_at ASC, b.hash ASC
              LIMIT $2",
        )
        .bind(mime_prefixes)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── 回放抽屉 ───────────────────────────────────────────

    /// 一轮对话注入过的记忆，按注入顺序。
    ///
    /// 表里只有 `fact_id`，`statement` 是这里 JOIN 出来的**现状** ——
    /// 于是一条事实被 redact 之后抽屉里跟着变成占位符，不需要再清一遍这张表。
    /// 代价是失去逐字保真，换来的是 redact 不必在多处清同一个秘密。
    ///
    /// 失效的事实**照样返回**，只是带上 `invalidated = true`：
    /// 「当时依据的这条现在已经不成立了」正是审计最想看到的东西，
    /// 把它藏起来等于篡改回放。
    pub async fn injected_memories(&self, episode_id: &str) -> Result<Vec<InjectedMemory>> {
        let rows = sqlx::query_as::<_, InjectedMemory>(
            "SELECT m.episode_id, m.fact_id, m.ordinal, m.channels, m.score,
                    f.statement, f.domain, f.source_episode_id,
                    coalesce(s.op = 'invalidate', false) AS invalidated
               FROM episode_memories m
               LEFT JOIN facts f       ON f.id = m.fact_id
               LEFT JOIN fact_status s ON s.fact_id = m.fact_id
              WHERE m.episode_id = $1
              ORDER BY m.ordinal ASC",
        )
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 批量版本。会话详情一次要给一整页消息挂抽屉，逐条查就是 N+1。
    pub async fn injected_memories_bulk(
        &self,
        episode_ids: &[String],
    ) -> Result<Vec<InjectedMemory>> {
        if episode_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, InjectedMemory>(
            "SELECT m.episode_id, m.fact_id, m.ordinal, m.channels, m.score,
                    f.statement, f.domain, f.source_episode_id,
                    coalesce(s.op = 'invalidate', false) AS invalidated
               FROM episode_memories m
               LEFT JOIN facts f       ON f.id = m.fact_id
               LEFT JOIN fact_status s ON s.fact_id = m.fact_id
              WHERE m.episode_id = ANY($1)
              ORDER BY m.episode_id ASC, m.ordinal ASC",
        )
        .bind(episode_ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 一轮对话的工具调用，按调用顺序。
    pub async fn episode_tool_calls(&self, episode_id: &str) -> Result<Vec<EpisodeToolCall>> {
        let rows = sqlx::query_as::<_, EpisodeToolCall>(
            "SELECT id, episode_id, ordinal, name, path, summary, ok, device_id, diff, created_at
               FROM episode_tool_calls WHERE episode_id = $1 ORDER BY ordinal ASC",
        )
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 批量版本，同 [`Self::injected_memories_bulk`] 的理由。
    pub async fn episode_tool_calls_bulk(
        &self,
        episode_ids: &[String],
    ) -> Result<Vec<EpisodeToolCall>> {
        if episode_ids.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, EpisodeToolCall>(
            "SELECT id, episode_id, ordinal, name, path, summary, ok, device_id, diff, created_at
               FROM episode_tool_calls WHERE episode_id = ANY($1)
              ORDER BY episode_id ASC, ordinal ASC",
        )
        .bind(episode_ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 最近的工具调用（跨 episode，最新的在前）。
    ///
    /// 轨迹抽取判「同一命令反复出现 = 这是这个项目的惯例」需要越过本轮的
    /// 视野，而那条判据在单轮内是算不出来的。
    ///
    /// **按 `id` 倒序而不是 `created_at`**：`id` 是 ULID，字典序即时间序，
    /// 而它是主键、天然有索引；`created_at` 上一个索引都没有，按它排序在
    /// 这张只增不减的表上就是一次全表扫描 + 排序。
    pub async fn recent_tool_calls(&self, limit: i64) -> Result<Vec<EpisodeToolCall>> {
        let rows = sqlx::query_as::<_, EpisodeToolCall>(
            "SELECT id, episode_id, ordinal, name, path, summary, ok, device_id, diff, created_at
               FROM episode_tool_calls ORDER BY id DESC LIMIT $1",
        )
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── L1：entities ───────────────────────────────────────

    pub async fn entity(&self, id: &str) -> Result<Option<Entity>> {
        let row = sqlx::query_as::<_, Entity>(
            "SELECT id, kind, name, summary, embedding, embedding_model, device_id, created_at
               FROM entities WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// 实体消解热路径：按 (kind, name) 精确匹配（走 `idx_entities_kn`）。
    pub async fn entities_named(&self, kind: &str, name: &str) -> Result<Vec<Entity>> {
        let rows = sqlx::query_as::<_, Entity>(
            "SELECT id, kind, name, summary, embedding, embedding_model, device_id, created_at
               FROM entities WHERE kind = $1 AND name = $2 ORDER BY created_at ASC, id ASC",
        )
        .bind(kind)
        .bind(name)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 沿别名合并链走到底的最终归属。链式合并 A→B→C 解析为 A→C。
    ///
    /// 写 `entity_merges` 前用它做成环检测：若目标的归属终点就是待合并的源，
    /// 这条边会成环，必须拒绝。
    pub async fn canonical_entity(&self, id: &str) -> Result<Option<String>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT canonical FROM canonical_entities WHERE id = $1")
                .bind(id)
                .fetch_optional(self.pool())
                .await?;
        Ok(row.map(|(canonical,)| canonical))
    }

    /// 批量解析归属。
    pub async fn canonical_entities(&self, ids: &[String]) -> Result<Vec<CanonicalEntity>> {
        let rows = sqlx::query_as::<_, CanonicalEntity>(
            "SELECT id, canonical FROM canonical_entities WHERE id = ANY($1) ORDER BY id ASC",
        )
        .bind(ids)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 某实体的出边（至多一条 —— `UNIQUE(from_entity)`）。
    pub async fn entity_merge_from(&self, from_entity: &str) -> Result<Option<EntityMerge>> {
        let row = sqlx::query_as::<_, EntityMerge>(
            "SELECT id, from_entity, into_entity, reason, source_episode_id, device_id, created_at
               FROM entity_merges WHERE from_entity = $1",
        )
        .bind(from_entity)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    // ── L1：facts ──────────────────────────────────────────

    /// 按主键取事实，**不过滤失效**（审计视图要看得见已删除的）。
    pub async fn fact(&self, id: &str) -> Result<Option<Fact>> {
        let row = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM facts WHERE id = $1"
        ))
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// 当前有效的事实 —— 日常检索的入口。
    pub async fn active_facts_by_subject(&self, subject_id: &str, limit: i64) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts WHERE subject_id = $1
              ORDER BY created_at DESC, id DESC LIMIT $2"
        ))
        .bind(subject_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 矛盾检测热路径：同一 (主语, 谓词) 下现存的有效事实（走 `idx_facts_sp`）。
    pub async fn active_facts_by_subject_predicate(
        &self,
        subject_id: &str,
        predicate: &str,
    ) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts WHERE subject_id = $1 AND predicate = $2
              ORDER BY created_at DESC, id DESC"
        ))
        .bind(subject_id)
        .bind(predicate)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 图遍历的反向一跳：指向该实体的有效事实（走 `idx_facts_object`）。
    pub async fn active_facts_by_object(
        &self,
        object_entity_id: &str,
        limit: i64,
    ) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM active_facts WHERE object_entity_id = $1
              ORDER BY created_at DESC, id DESC LIMIT $2"
        ))
        .bind(object_entity_id)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// redact 级联：按出处定位派生事实（走 `idx_facts_source`）。
    pub async fn facts_by_source_episode(&self, episode_id: &str) -> Result<Vec<Fact>> {
        let rows = sqlx::query_as::<_, Fact>(concat!(
            "SELECT ",
            fact_columns!(),
            "  FROM facts WHERE source_episode_id = $1 ORDER BY created_at ASC, id ASC"
        ))
        .bind(episode_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 一条事实最近一次**状态**事件（`flag` 是批注，不参与判定）。
    /// 返回 `None` 表示从未被失效过。
    pub async fn fact_status(&self, fact_id: &str) -> Result<Option<FactStatus>> {
        let row = sqlx::query_as::<_, FactStatus>(
            "SELECT fact_id, op, kind, invalid_at, superseded_by, actor, decided_at
               FROM fact_status WHERE fact_id = $1",
        )
        .bind(fact_id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    /// 一条事实的完整生命周期时间线，含 `flag`。界面的「它是怎么变的」。
    pub async fn fact_events(&self, fact_id: &str) -> Result<Vec<FactEvent>> {
        let rows = sqlx::query_as::<_, FactEvent>(
            "SELECT id, fact_id, op, kind, invalid_at, superseded_by, actor, reason,
                    source_episode_id, device_id, created_at
               FROM fact_events WHERE fact_id = $1 ORDER BY created_at ASC, id ASC",
        )
        .bind(fact_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── L2：summaries ──────────────────────────────────────

    pub async fn summary(&self, id: &str) -> Result<Option<Summary>> {
        let row = sqlx::query_as::<_, Summary>(
            "SELECT id, scope, scope_key, text, embedding, embedding_model,
                    covers_from, covers_to, device_id, created_at
               FROM summaries WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row)
    }

    pub async fn summaries_for_scope(
        &self,
        scope: SummaryScope,
        scope_key: &str,
        limit: i64,
    ) -> Result<Vec<Summary>> {
        let rows = sqlx::query_as::<_, Summary>(
            "SELECT id, scope, scope_key, text, embedding, embedding_model,
                    covers_from, covers_to, device_id, created_at
               FROM summaries WHERE scope = $1 AND scope_key = $2
              ORDER BY created_at DESC, id DESC LIMIT $3",
        )
        .bind(scope)
        .bind(scope_key)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    // ── 抹除墓碑 ───────────────────────────────────────────

    pub async fn redactions_for_target(
        &self,
        target_kind: RedactionTarget,
        target_id: &str,
    ) -> Result<Vec<Redaction>> {
        let rows = sqlx::query_as::<_, Redaction>(
            "SELECT id, target_kind, target_id, mode, reason, actor, device_id, created_at
               FROM redactions WHERE target_kind = $1 AND target_id = $2
              ORDER BY created_at ASC, id ASC",
        )
        .bind(target_kind)
        .bind(target_id)
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 在途任务防护：抽取 / 转录 pipeline 在写入任何派生行**之前**必须问这一句，
    /// 否则 redact 执行时在途的异步任务会把已抹除的内容重新写回来
    /// （docs/memory.md §十一）。
    pub async fn is_redacted(&self, target_kind: RedactionTarget, target_id: &str) -> Result<bool> {
        let (exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM redactions WHERE target_kind = $1 AND target_id = $2)",
        )
        .bind(target_kind)
        .bind(target_id)
        .fetch_one(self.pool())
        .await?;
        Ok(exists)
    }

    // ─────────────── 换 embedding 模型：普查与待办 ───────────────

    /// 库里现有哪些 embedding 模型、各覆盖多少条**有效**事实。
    ///
    /// # 为什么需要它
    ///
    /// 换模型之后旧向量还在，但四条向量查询全部按 `embedding_model` 过滤
    /// （见 [`Store::recall_vector`]），于是旧事实**当天就退出向量召回**。
    /// 而 BM25 / 图 / recency 三路照常，所以它看起来不像坏了，
    /// 只像「语义召回好像变差了点」—— 没有任何人被告知。
    ///
    /// 这个查询是 cortexd 启动时那声告警的依据。**它只负责数数，
    /// 不负责判断**：当前模型是哪个由调用方决定。
    ///
    /// 统计的是 `facts` 原列与 `fact_embeddings` 旁表的**并集**：
    /// 同一条事实在两个模型下各算一次，这正是想要的 ——
    /// 「模型 M 覆盖了多少条」问的就是这个。
    ///
    /// 按覆盖数降序，让最大的那一档排在最前面（告警只印得下几行）。
    pub async fn embedding_census(&self) -> Result<Vec<(String, i64)>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT embedding_model, count(DISTINCT fact_id) AS n
               FROM (
                        SELECT id AS fact_id, embedding_model
                          FROM active_facts
                         WHERE embedding IS NOT NULL
                        UNION ALL
                        SELECT fe.fact_id, fe.embedding_model
                          FROM fact_embeddings fe
                          JOIN active_facts f ON f.id = fe.fact_id
                    ) t
              GROUP BY embedding_model
              ORDER BY n DESC, embedding_model ASC",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 当前模型覆盖了多少条有效事实、还有多少条覆盖不到。
    ///
    /// # 为什么不能拿 [`Store::embedding_census`] 相减
    ///
    /// 回填期间一条事实**同时**在两个模型下有向量，普查里它被数了两次。
    /// 用「总数 − 当前模型数」算出来的 stale 会偏小，而且偏小的幅度正好
    /// 等于已回填的条数 —— 也就是说回填跑得越多，这个数字越接近 0，
    /// 看起来「快好了」，实际与真实进度无关。
    ///
    /// `covered` 的判据与 [`Store::recall_vector`] 的并集**逐字一致**：
    /// 原列在当前模型下且非空，**或**旁表里有当前模型的行。
    /// 两处判据漂开的症状是 `/health` 报「全覆盖」而召回依然是空的。
    pub async fn embedding_coverage(&self, model: &str) -> Result<(i64, i64)> {
        let (covered, stale): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE ok), count(*) FILTER (WHERE NOT ok)
               FROM (
                 SELECT (f.embedding IS NOT NULL AND f.embedding_model = $1)
                        OR EXISTS (
                             SELECT 1 FROM fact_embeddings fe
                              WHERE fe.fact_id = f.id AND fe.embedding_model = $1
                           ) AS ok
                   FROM active_facts f
               ) t",
        )
        .bind(model)
        .fetch_one(self.pool())
        .await?;
        Ok((covered, stale))
    }

    /// 还没有 `model` 这个空间下向量的**有效**事实，供回填器取一批。
    ///
    /// 按 `created_at DESC`：**新的先补**。近期记忆被检索到的概率高得多，
    /// 从旧的补起意味着回填跑了一半时，用户最可能问到的那些反而还没回来。
    ///
    /// 返回 `(id, statement)` 而不是整行 [`Fact`]：回填只需要文本，
    /// 把 1024 维向量与 tsv 一起搬回来是白费带宽 —— 而这条查询会被跑很多轮。
    ///
    /// # 判据是「没有可用向量」，不是「模型名不同」
    ///
    /// 写成 `embedding_model <> $1` 会漏掉一类：模型名**就是**当前这个、
    /// 但 `embedding` 是 NULL 的行（写入时向量化失败过）。那种事实同样
    /// 进不了向量召回，而且永远不会被回填器捡起来 —— 它在待办里不存在，
    /// 却在 [`Store::embedding_coverage`] 的 stale 里算一条，
    /// 于是 `/health` 上那个数字会**卡在一个下不去的值**，
    /// 看起来像回填坏了。两处必须用同一个判据。
    pub async fn facts_missing_embedding(
        &self,
        model: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT f.id, f.statement
               FROM active_facts f
              WHERE NOT (f.embedding IS NOT NULL AND f.embedding_model = $1)
                AND NOT EXISTS (
                        SELECT 1 FROM fact_embeddings fe
                         WHERE fe.fact_id = f.id AND fe.embedding_model = $1
                    )
              ORDER BY f.created_at DESC, f.id DESC
              LIMIT $2",
        )
        .bind(model)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }

    /// 同上，实体侧。实体向量不参与四路召回，只用于**抽取期的实体消解** ——
    /// 但它失效的方式更隐蔽：消解不上就会给同一个人建出第二个实体，
    /// 从此他的事实分裂在两个节点上，图遍历再也走不通。
    pub async fn entities_missing_embedding(
        &self,
        model: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT e.id, e.name
               FROM entities e
              WHERE NOT (e.embedding IS NOT NULL AND e.embedding_model = $1)
                -- 从没有过向量的实体不补：实体向量是抽取期消解用的，
                -- 没有过就说明当初那条路没走到，补一个也没有对应的语义
                AND e.embedding IS NOT NULL
                AND NOT EXISTS (
                        SELECT 1 FROM entity_embeddings ee
                         WHERE ee.entity_id = e.id AND ee.embedding_model = $1
                    )
              ORDER BY e.created_at DESC, e.id DESC
              LIMIT $2",
        )
        .bind(model)
        .bind(limit.clamp(1, MAX_LIMIT))
        .fetch_all(self.pool())
        .await?;
        Ok(rows)
    }
}

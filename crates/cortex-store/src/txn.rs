//! 写事务：把 docs/memory.md §九 的写入纪律做成**拿不到的裸事务**。
//!
//! ## 纪律
//!
//! 1. 事务开头 `SELECT pg_advisory_xact_lock(4272)`，把并发写事务串行化，
//!    保证 `sync_log.seq` 的顺序 == 对读端的可见顺序。
//!    裸 `BIGSERIAL` 做不到：序列值在 INSERT 时分配，行却按提交顺序可见 ——
//!    T1 取到 100 未提交、T2 取到 101 先提交，客户端把游标推到 101 之后，
//!    T1 提交的 100 就永久不可见。静默丢数据，除全量重同步外不可修复。
//! 2. 每写一行业务数据，同事务追加一行 `sync_log`。
//! 3. 取号事务必须**短小、纯写**：LLM 调用与 embedding 计算一律在事务外完成。
//!
//! ## 纪律是怎么被强制的
//!
//! 第 2 条不靠调用方自觉。裸 [`sqlx::Transaction`] 被封在私有模块 `guarded`
//! 里，字段对本文件其余部分都不可见，因此**唯一**能落库的路径是
//! `Guarded::insert_row` —— 而它把「写业务行」和「追加 sync_log」绑成一个
//! 不可分割的动作。将来给 [`WriteTxn`] 新增写方法的人，即使完全不知道
//! sync_log 的存在，也写不出漏追加的代码。
//!
//! 第 3 条同样是结构性的：[`WriteTxn`] **不提供任何读方法**。需要先查后写的
//! 流程（矛盾消解、实体消解、合并环检测）应在事务外读完、算完，再进事务写。

use sqlx::{PgPool, Postgres, postgres::PgArguments, query::Query};

use cortex_core::Id;

use crate::error::{Result, StoreError};
use crate::model::{
    DerivedKind, NewBlob, NewBlobTranscript, NewEntity, NewEntityEmbedding, NewEntityMerge,
    NewEpisode, NewEpisodeBlob, NewEpisodeMemory, NewEpisodeToolCall, NewFact, NewFactEmbedding,
    NewFactEvent, NewProjectEvent, NewRedaction, NewSessionEvent, NewSummary, ProvenanceRef, table,
};

/// 同步取号锁的 advisory lock key。
///
/// 取值 4272 无特殊含义，只需全库唯一且不与他人冲突；
/// 与 `migrations/20260807000001_init.sql` 头部注释保持一致。
///
/// 多租户之后它是**两参数形式的 classid**，第二个参数由租户 schema 派生
/// （见 `SchemaName::lock_key`）。类型因此是 `i32` 而不是 `i64`：
/// `pg_advisory_xact_lock` 的双参数重载收两个 int4。
pub const SYNC_ADVISORY_LOCK_KEY: i32 = 4272;

mod guarded {
    use sqlx::{PgPool, Postgres, Transaction, postgres::PgArguments, query::Query};

    use super::SYNC_ADVISORY_LOCK_KEY;
    use crate::error::Result;

    /// 裸事务的看守。字段私有于本模块 —— 这就是纪律的执行机构。
    pub(super) struct Guarded {
        tx: Transaction<'static, Postgres>,
    }

    impl Guarded {
        /// 开启事务并**立即**取 advisory lock。
        ///
        /// 锁必须是事务里的第一条语句：它要早于任何 `nextval`，
        /// 才能让「取号顺序 == 提交顺序」成立。
        pub(super) async fn begin(pool: &PgPool, tenant_key: i32) -> Result<Self> {
            let mut tx = pool.begin().await?;
            // **两参数形式**，第二个由租户 schema 派生。
            //
            // 原来是单参数的全局键，那把**所有租户**的写事务串行化了 ——
            // 一个人导入三年历史，别人一句话都写不进去。而这把锁保证的是
            // 「`sync_log.seq` 的顺序 == 对读端可见的顺序」，
            // 那件事本来就是每 schema 各论各的。
            //
            // classid 仍是 SYNC_ADVISORY_LOCK_KEY，好让 `pg_locks` 里
            // 一眼认得出这是同步取号锁而不是别的什么
            sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
                .bind(SYNC_ADVISORY_LOCK_KEY)
                .bind(tenant_key)
                .execute(&mut *tx)
                .await?;
            Ok(Self { tx })
        }

        /// 唯一的业务写入通道：执行业务 INSERT，并在同一事务里追加 sync_log。
        ///
        /// 返回该行在同步全序中的 `seq`。
        pub(super) async fn insert_row<'q>(
            &mut self,
            table: &'static str,
            record_id: &str,
            stmt: Query<'q, Postgres, PgArguments>,
        ) -> Result<i64> {
            stmt.execute(&mut *self.tx).await?;

            let (seq,): (i64,) = sqlx::query_as(
                "INSERT INTO sync_log (table_name, record_id) VALUES ($1, $2) RETURNING seq",
            )
            .bind(table)
            .bind(record_id)
            .fetch_one(&mut *self.tx)
            .await?;

            Ok(seq)
        }

        /// **派生行**专用通道：执行 INSERT，**不写 sync_log**。
        ///
        /// # 只有一类东西配走这条路
        ///
        /// 换 embedding 模型时补出来的向量（`fact_embeddings` /
        /// `entity_embeddings`）。它们同时满足三条，缺一条都不行：
        ///
        /// 1. **纯派生**：由 `statement` + 模型唯一决定，丢了能重算
        /// 2. **客户端根本收不到**：`sync_payload.rs` 明确规定
        ///    「tsv 与 embedding 是服务端派生列，不进同步 payload」——
        ///    走 sync_log 只会推出一堆载荷里没有任何有用字段的行
        /// 3. **量级是全库级的**：十万条事实的一次回填就是十万条 sync_log。
        ///    `sync_payload.rs` 的注释里那句「换 embedding 模型时会全量变化，
        ///    **白白撑爆增量同步**」说的正是这件事
        ///
        /// # 加新表之前先想清楚
        ///
        /// 业务行走 [`Self::insert_row`]。把业务行放到这条路上，
        /// 后果是**其它设备永远看不到它**，而且没有任何症状 ——
        /// 那正是 sync_log 强制机制存在的理由。
        /// 拿不准就用 `insert_row`：多一行 sync_log 是可见的浪费，
        /// 少一行是不可见的丢失。
        pub(super) async fn insert_derived_row<'q>(
            &mut self,
            stmt: Query<'q, Postgres, PgArguments>,
        ) -> Result<()> {
            stmt.execute(&mut *self.tx).await?;
            Ok(())
        }

        pub(super) async fn commit(self) -> Result<()> {
            self.tx.commit().await?;
            Ok(())
        }

        pub(super) async fn rollback(self) -> Result<()> {
            self.tx.rollback().await?;
            Ok(())
        }
    }
}

/// 一个写事务。只能由 [`crate::Store::write_txn`] 交到调用方手里。
///
/// 每个写方法返回该行在同步全序中的 `seq`（`sync_log.seq`）——
/// 上行 ingest 的逐条 ack 直接用它。
pub struct WriteTxn {
    inner: guarded::Guarded,
}

impl WriteTxn {
    pub(crate) async fn begin(pool: &PgPool, tenant_key: i32) -> Result<Self> {
        Ok(Self {
            inner: guarded::Guarded::begin(pool, tenant_key).await?,
        })
    }

    pub(crate) async fn commit(self) -> Result<()> {
        self.inner.commit().await
    }

    pub(crate) async fn rollback(self) -> Result<()> {
        self.inner.rollback().await
    }

    async fn insert_row<'q>(
        &mut self,
        table: &'static str,
        record_id: &str,
        stmt: Query<'q, Postgres, PgArguments>,
    ) -> Result<i64> {
        self.inner.insert_row(table, record_id, stmt).await
    }

    // ── L0 原始层 ──────────────────────────────────────────

    /// 落一条原始消息。`tsv_source` 与主行同事务写入，永不异步补写。
    pub async fn insert_episode(&mut self, new: &NewEpisode) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO episodes
                 (id, session_id, role, content, text, tsv, domain, device_id, occurred_at)
             VALUES ($1, $2, $3, $4, $5, to_tsvector('simple', $6), $7, $8, $9)",
        )
        .bind(&id)
        .bind(&new.session_id)
        .bind(new.role)
        .bind(sqlx::types::Json(&new.content))
        .bind(&new.text)
        .bind(&new.tsv_source)
        .bind(&new.domain)
        .bind(&new.device_id)
        .bind(new.occurred_at);

        self.insert_row(table::EPISODES, &id, stmt).await
    }

    /// 登记一个内容寻址的二进制对象。
    ///
    /// 三步固定顺序的第二步：对象存储上传 → `blobs` 行 → episode + 关联行。
    pub async fn insert_blob(&mut self, new: &NewBlob) -> Result<i64> {
        let stmt = sqlx::query(
            "INSERT INTO blobs (hash, mime, size_bytes, storage_key) VALUES ($1, $2, $3, $4)",
        )
        .bind(&new.hash)
        .bind(&new.mime)
        .bind(new.size_bytes)
        .bind(&new.storage_key);

        self.insert_row(table::BLOBS, &new.hash, stmt).await
    }

    /// 把 blob 挂到 episode 上。`sync_log.record_id` 用复合键 `episode_id:blob_hash`。
    ///
    /// `filename` 存在这里而不是 `blobs` 里：内容寻址下同一份字节可以有多个
    /// 文件名，写进 blobs 就成了「谁先传谁定名」。见
    /// `migrations/20260807000004_attachment_filename.sql`。
    pub async fn link_episode_blob(&mut self, new: &NewEpisodeBlob) -> Result<i64> {
        let record_id = new.record_id();
        let stmt = sqlx::query(
            "INSERT INTO episode_blobs (episode_id, blob_hash, kind, filename)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(new.episode_id.to_string())
        .bind(&new.blob_hash)
        .bind(&new.kind)
        .bind(&new.filename);

        self.insert_row(table::EPISODE_BLOBS, &record_id, stmt)
            .await
    }

    // ── 回放抽屉 ───────────────────────────────────────────

    /// 记下本轮注入了哪一条记忆。
    ///
    /// 只存 `fact_id`，不存注入当时的原文快照 —— 快照里的原文抹不掉，
    /// 会让 redact 的销毁承诺变成假的（见 migration 20260807000005 的表头）。
    pub async fn insert_episode_memory(&mut self, new: &NewEpisodeMemory) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO episode_memories
                 (id, episode_id, fact_id, ordinal, channels, score, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(new.episode_id.to_string())
        .bind(new.fact_id.to_string())
        .bind(new.ordinal)
        .bind(&new.channels)
        .bind(new.score)
        .bind(&new.device_id);

        self.insert_row(table::EPISODE_MEMORIES, &id, stmt).await
    }

    /// 记下本轮的一次工具调用。
    ///
    /// `path` 是独立一列而不是塞在 `summary` 里：summary 是给人看的自然语言，
    /// 措辞随时会改，客户端一旦用正则从里面抠路径，改一次措辞就是静默显示错文件。
    pub async fn insert_episode_tool_call(&mut self, new: &NewEpisodeToolCall) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO episode_tool_calls
                 (id, episode_id, ordinal, name, path, summary, ok, device_id, diff)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&id)
        .bind(new.episode_id.to_string())
        .bind(new.ordinal)
        .bind(&new.name)
        .bind(&new.path)
        .bind(&new.summary)
        .bind(new.ok)
        .bind(&new.device_id)
        .bind(&new.diff);

        self.insert_row(table::EPISODE_TOOL_CALLS, &id, stmt).await
    }

    /// 写入媒体转录。
    ///
    /// 调用方在写入前必须查过 [`crate::Store::is_redacted`] ——
    /// 防止 redact 执行时在途的异步任务事后把已抹除内容回填（docs/memory.md §十一）。
    pub async fn insert_blob_transcript(&mut self, new: &NewBlobTranscript) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO blob_transcripts
                 (id, blob_hash, kind, text, tsv, embedding,
                  span_start_ms, span_end_ms, transcribed_by, embedding_model)
             VALUES ($1, $2, $3, $4, to_tsvector('simple', $5), $6, $7, $8, $9, $10)",
        )
        .bind(&id)
        .bind(&new.blob_hash)
        .bind(new.kind)
        .bind(&new.text)
        .bind(&new.tsv_source)
        .bind(&new.embedding)
        .bind(new.span_start_ms)
        .bind(new.span_end_ms)
        .bind(&new.transcribed_by)
        .bind(&new.embedding_model);

        self.insert_row(table::BLOB_TRANSCRIPTS, &id, stmt).await
    }

    // ── L1 事实层 ──────────────────────────────────────────

    pub async fn insert_entity(&mut self, new: &NewEntity) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO entities (id, kind, name, summary, embedding, embedding_model, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(&new.kind)
        .bind(&new.name)
        .bind(&new.summary)
        .bind(&new.embedding)
        .bind(&new.embedding_model)
        .bind(&new.device_id);

        self.insert_row(table::ENTITIES, &id, stmt).await
    }

    /// 追加一条别名合并。
    ///
    /// `UNIQUE(from_entity)` 使并发分叉在物理层就是 first-writer-wins：
    /// 后到者会拿到唯一约束冲突（`sqlx::Error::Database`，SQLSTATE 23505），
    /// 调用方据此转 no-op 并回执客户端。
    ///
    /// 环（A→B 与 B→A）不由数据库拦截，须由调用方在**事务外**沿 `into_entity`
    /// 链走到底做成环检测（`canonical_entity` 即为此提供）。
    pub async fn insert_entity_merge(&mut self, new: &NewEntityMerge) -> Result<i64> {
        let id = new.id.to_string();
        let source_episode_id = new.source_episode_id.map(|v| v.to_string());
        let stmt = sqlx::query(
            "INSERT INTO entity_merges
                 (id, from_entity, into_entity, reason, source_episode_id, device_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(new.from_entity.to_string())
        .bind(new.into_entity.to_string())
        .bind(&new.reason)
        .bind(&source_episode_id)
        .bind(&new.device_id);

        self.insert_row(table::ENTITY_MERGES, &id, stmt).await
    }

    /// 落一条事实。
    ///
    /// `source` 连带写出 `trust_tier`：两列的对应由 schema 的
    /// `facts_trust_tier_matches_channel` 锁死，这里只是把它算出来 ——
    /// 让调用方自己填两列，迟早会出现「通道说是网页、档位说是用户亲述」
    /// 这种自相矛盾的行，而它只在真出安全事件、要按来源批量失效时才暴露。
    ///
    /// `derived_from` 里的每一条源都会在**同一事务**里追加一行 `derivations`
    /// （各自带自己的 sync_log）。单源派生留空即可，`source_episode_id`
    /// 就是它的血缘。
    pub async fn insert_fact(&mut self, new: &NewFact) -> Result<i64> {
        let id = new.id.to_string();
        let object_entity_id = new.object_entity_id.map(|v| v.to_string());
        let stmt = sqlx::query(
            "INSERT INTO facts
                 (id, subject_id, predicate, object_text, object_entity_id, statement, tsv,
                  embedding, embedding_model, domain, confidence, valid_at,
                  source_episode_id, source_channel, trust_tier, extracted_by, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, to_tsvector('simple', $7),
                     $8, $9, $10, $11, $12, $13, $14, $15, $16, $17)",
        )
        .bind(&id)
        .bind(new.subject_id.to_string())
        .bind(&new.predicate)
        .bind(&new.object_text)
        .bind(&object_entity_id)
        .bind(&new.statement)
        .bind(&new.tsv_source)
        .bind(&new.embedding)
        .bind(&new.embedding_model)
        .bind(&new.domain)
        .bind(new.confidence)
        .bind(new.valid_at)
        .bind(new.source_episode_id.to_string())
        .bind(new.source.channel())
        .bind(new.source.trust_tier())
        .bind(&new.extracted_by)
        .bind(&new.device_id);

        let seq = self.insert_row(table::FACTS, &id, stmt).await?;

        for source in &new.derived_from {
            self.insert_derivation(DerivedKind::Fact, new.id, *source, &new.device_id)
                .await?;
        }

        Ok(seq)
    }

    /// 换模型时给一条已有事实补一个向量。
    ///
    /// # 为什么不是 `UPDATE facts SET embedding = …`
    ///
    /// 两条理由，第二条是硬的：
    ///
    /// 1. CLAUDE.md：「任何表都不做 UPDATE / DELETE」。这个 crate 里
    ///    到目前为止零处 UPDATE，既有先例是 `blob_transcripts` ——
    ///    派生数据往独立表插新行
    /// 2. 「回填期间双模型召回」（memory.md §七 第 3 步）要求同一条事实
    ///    **同时**持有两个空间的向量。一个列装不下两个
    ///
    /// # 幂等
    ///
    /// `ON CONFLICT DO NOTHING`：进程重启、两轮回填撞到同一条、
    /// 两台设备同时回填，都会走到这里。让唯一约束兜底，
    /// 而不是让回填器自己记账 —— 后者要么漏要么重，且错了没有报错。
    ///
    /// **不进 sync_log**（走 `insert_derived_row`）。理由见
    /// `model.rs` 里 `table::FACT_EMBEDDINGS` 那段注释。
    pub async fn insert_fact_embedding(&mut self, new: &NewFactEmbedding) -> Result<()> {
        let stmt = sqlx::query(
            "INSERT INTO fact_embeddings (fact_id, embedding_model, embedding)
             VALUES ($1, $2, $3)
             ON CONFLICT (fact_id, embedding_model) DO NOTHING",
        )
        .bind(new.fact_id.to_string())
        .bind(&new.embedding_model)
        .bind(&new.embedding);

        self.inner.insert_derived_row(stmt).await
    }

    /// 同上，实体侧。
    ///
    /// 实体向量不参与四路召回，只用于**抽取期的实体消解**。它失效的方式
    /// 比事实更隐蔽：消解不上就会给同一个人建出第二个实体，
    /// 从此他的事实分裂在两个节点上，图遍历再也走不通 ——
    /// 而这一切在界面上只表现为「它好像忘了我说过的一些事」。
    pub async fn insert_entity_embedding(&mut self, new: &NewEntityEmbedding) -> Result<()> {
        let stmt = sqlx::query(
            "INSERT INTO entity_embeddings (entity_id, embedding_model, embedding)
             VALUES ($1, $2, $3)
             ON CONFLICT (entity_id, embedding_model) DO NOTHING",
        )
        .bind(new.entity_id.to_string())
        .bind(&new.embedding_model)
        .bind(&new.embedding);

        self.inner.insert_derived_row(stmt).await
    }

    /// 追加一条生命周期事件（失效 / 恢复 / 标记）。
    ///
    /// 失效以追加表达，而非就地 UPDATE —— 这是 append-only 与多端无冲突的前提。
    pub async fn insert_fact_event(&mut self, new: &NewFactEvent) -> Result<i64> {
        let id = new.id.to_string();
        let superseded_by = new.superseded_by.map(|v| v.to_string());
        let source_episode_id = new.source_episode_id.map(|v| v.to_string());
        let stmt = sqlx::query(
            "INSERT INTO fact_events
                 (id, fact_id, op, kind, invalid_at, superseded_by,
                  actor, reason, source_episode_id, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        )
        .bind(&id)
        .bind(new.fact_id.to_string())
        .bind(new.op)
        .bind(new.kind)
        .bind(new.invalid_at)
        .bind(&superseded_by)
        .bind(new.actor)
        .bind(&new.reason)
        .bind(&source_episode_id)
        .bind(&new.device_id);

        self.insert_row(table::FACT_EVENTS, &id, stmt).await
    }

    // ── L2 摘要层 ──────────────────────────────────────────

    /// 写一份摘要**连同它的血缘**。
    ///
    /// # 为什么两件事绑在一个方法里
    ///
    /// 与 [`Guarded::insert_row`](guarded) 把「业务行 + sync_log」绑死是同一个手法。
    /// 摘要没有 `source_episode_id NOT NULL` 那样的兜底列，血缘全在
    /// `derivations` 里；分成两次调用就意味着「先写摘要、再写边」中间可以断，
    /// 而断掉的表现是一条**永远找不到源头**的摘要 —— 它在源被 redact 之后
    /// 会继续泄露内容，且事后补不出来（谁摘的、摘了什么，只有生成它的那步知道）。
    ///
    /// 返回的是**摘要行**的 seq；它的血缘边紧随其后，seq 更大。
    /// 客户端按 sync_log 序回放时先看到摘要再看到边，正好是 FK 方向。
    ///
    /// # Errors
    /// `new.sources` 为空时返回 [`StoreError::MissingProvenance`]，且**不写任何行**。
    pub async fn insert_summary(&mut self, new: &NewSummary) -> Result<i64> {
        if new.sources.is_empty() {
            return Err(StoreError::MissingProvenance {
                kind: table::SUMMARIES,
                id: new.id.to_string(),
            });
        }

        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO summaries
                 (id, scope, scope_key, text, tsv, embedding, embedding_model,
                  covers_from, covers_to, device_id)
             VALUES ($1, $2, $3, $4, to_tsvector('simple', $5), $6, $7, $8, $9, $10)",
        )
        .bind(&id)
        .bind(new.scope)
        .bind(&new.scope_key)
        .bind(&new.text)
        .bind(&new.tsv_source)
        .bind(&new.embedding)
        .bind(&new.embedding_model)
        .bind(new.covers_from)
        .bind(new.covers_to)
        .bind(&new.device_id);

        let seq = self.insert_row(table::SUMMARIES, &id, stmt).await?;

        for source in &new.sources {
            self.insert_derivation(DerivedKind::Summary, new.id, *source, &new.device_id)
                .await?;
        }

        Ok(seq)
    }

    // ── 派生血缘 ───────────────────────────────────────────

    /// 追加一条「派生物 ← 源」的血缘边。
    ///
    /// 私有：血缘边只应与它描述的那条派生行在同一个方法里写出去
    /// （见 [`Self::insert_summary`]）。开成 public 就等于允许「先写派生物、
    /// 回头再补边」，而那正是 §5.3 说的「事后补是不可能的」那种补法。
    async fn insert_derivation(
        &mut self,
        derived_kind: DerivedKind,
        derived_id: Id,
        source: ProvenanceRef,
        device_id: &str,
    ) -> Result<i64> {
        let id = Id::new().to_string();
        let stmt = sqlx::query(
            "INSERT INTO derivations
                 (id, derived_kind, derived_id, source_kind, source_id, device_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(derived_kind)
        .bind(derived_id.to_string())
        .bind(source.kind)
        .bind(source.id.to_string())
        .bind(device_id);

        self.insert_row(table::DERIVATIONS, &id, stmt).await
    }

    // ── 会话生命周期 ───────────────────────────────────────

    /// 追加一条会话生命周期事件（改名 / 归档 / 绑定工作区）。
    ///
    /// 和 [`Self::insert_fact_event`] 一样是追加而非就地改：会话的末态由
    /// 每个维度最后一条事件决定（`session_state` 视图）。这不只是为了守
    /// append-only —— 它让多端并发改名有确定结果：按 `sync_log` 全序回放，
    /// 每台设备算出的标题都一样，而 UPDATE 语义下后到的写会盖掉先到的，
    /// 两台设备各自「后到」，最终收敛到哪个取决于网络抖动。
    pub async fn insert_session_event(&mut self, new: &NewSessionEvent) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO session_events
                 (id, session_id, op, title, workspace, project_id, runtime, actor, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&id)
        .bind(&new.session_id)
        .bind(new.op)
        .bind(&new.title)
        .bind(&new.workspace)
        .bind(&new.project_id)
        .bind(new.runtime)
        .bind(new.actor)
        .bind(&new.device_id);

        self.insert_row(table::SESSION_EVENTS, &id, stmt).await
    }

    // ── 项目生命周期 ───────────────────────────────────────

    /// 追加一条项目生命周期事件（建 / 改名 / 删）。
    ///
    /// 与 [`Self::insert_session_event`] 同款：末态由每台状态机最后一条事件
    /// 决定（`project_state` 视图），而不是就地改一张 projects 表。
    /// 删项目在这里只是**追加一条 delete**，里面的会话一条都不动 ——
    /// 级联改写会让「撤销误删」变成不可能。
    pub async fn insert_project_event(&mut self, new: &NewProjectEvent) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO project_events (id, project_id, op, name, actor, device_id)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&id)
        .bind(&new.project_id)
        .bind(new.op)
        .bind(&new.name)
        .bind(new.actor)
        .bind(&new.device_id);

        self.insert_row(table::PROJECT_EVENTS, &id, stmt).await
    }

    // ── 抹除墓碑 ───────────────────────────────────────────

    /// 写下一块抹除墓碑。
    ///
    /// 只是墓碑：真正的级联清除（清 episode 正文、派生 fact 的 statement /
    /// tsv / embedding、blob_transcripts、重生成 summaries）是 redact 执行器
    /// 的工作，且是全系统唯一允许 UPDATE 的地方。墓碑本身走 sync_log 下发，
    /// 客户端收到后必须幂等执行同等本地清除。
    pub async fn insert_redaction(&mut self, new: &NewRedaction) -> Result<i64> {
        let id = new.id.to_string();
        let stmt = sqlx::query(
            "INSERT INTO redactions (id, target_kind, target_id, mode, reason, actor, device_id)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&id)
        .bind(new.target_kind)
        .bind(&new.target_id)
        .bind(new.mode)
        .bind(&new.reason)
        .bind(&new.actor)
        .bind(&new.device_id);

        self.insert_row(table::REDACTIONS, &id, stmt).await
    }
}

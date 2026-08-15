-- 换 embedding 模型时不再静默失忆 —— 向量旁表
--
-- 设计依据：docs/memory.md §七「换模型不停机」的第 2、3 步。
-- 第 1 步（新写入用新模型）在 init 里就做了，第 2、3 步一直是空的。
--
-- ══════════════════════════════════════════════════════════
--  这张表在补什么洞
--
--  四条向量查询全部带 `embedding_model = $N` 过滤（recall.rs），
--  所以「跨模型算距离得到垃圾」这个失效模式不存在 —— 很好。
--  但代价是：**换模型的当天，向量那一路返回 0 行**。
--  一条旧事实都匹配不上新的 model_id。
--
--  而 BM25 / 图遍历 / recency 三路照常工作，所以它**看起来不像坏了**，
--  只像「语义召回好像变差了点」。没有任何人被告知。
--  过滤把「给错答案」换成了「静默的部分失忆」—— 严格更好，依然静默。
--
--  这不是假想：默认后端从 fastembed 换成远端 API 之后，model_id 从
--  `fastembed:gpahal/bge-m3-onnx-int8` 变成 `api:bge-m3`，
--  任何升级上来的库里全部存量事实会当场退出向量召回。
-- ══════════════════════════════════════════════════════════
--
-- ── 为什么是**旁表**，不是就地改 facts.embedding ──
--
--   1. CLAUDE.md：「任何表都不做 UPDATE / DELETE」。cortex-store 里现在
--      零处 UPDATE，既有先例是 blob_transcripts —— 派生数据往独立表插新行。
--      回填是纯 append，这条规则原样成立
--   2. 「回填期间双模型召回」（§七 第 3 步）要求同一条事实**同时**持有
--      两个空间的向量。一个列装不下两个
--
-- ── 为什么**不搬走** facts.embedding ──
--
--   搬走要动 Fact 结构、txn.rs 三条 insert、sync.rs 的行重建，涟漪极大，
--   而收益只是「整齐」。这里改成纯追加：
--
--       模型 M 下的全部向量
--         = facts WHERE embedding_model = M          （写入当时那个模型）
--         ∪ fact_embeddings WHERE embedding_model = M（后来回填的）
--
--   新写入一行不改，回填只往旁表插。expand-only：**不 copy 存量、不 drop 旧列**，
--   于是这个 migration 可以在跑着的库上应用而不影响任何现有查询。
--
-- ── 为什么只有 facts 与 entities ──
--
--   summaries 与 blob_transcripts 也有 embedding + HNSW 索引，但
--   **当前没有任何向量查询用到它们**（recall.rs 里四条向量查询全部只碰
--   facts 与 entities）。给用不上的表建第二套索引是纯粹的写放大。
--   那两个索引本身是不是该留着，是另一个问题，记在 roadmap 里。

-- ══════════════════════════════════════════════════════════
--  facts
-- ══════════════════════════════════════════════════════════

CREATE TABLE fact_embeddings (
    fact_id         ulid         NOT NULL REFERENCES facts(id),
    embedding_model TEXT         NOT NULL,
    -- NOT NULL：旁表里的行**只**为了「这个模型下这条事实的向量」而存在。
    -- 允许 NULL 等于允许一行什么也不表示，而查询侧还得多一个 IS NOT NULL
    embedding       VECTOR(1024) NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),

    -- 一条事实在一个模型下只有一个向量。回填器重复跑到同一条时
    -- 由这个约束挡下来（ON CONFLICT DO NOTHING），而不是靠它自己记账 ——
    -- 进程重启、两台设备同时回填，都会走到这里
    PRIMARY KEY (fact_id, embedding_model)
);

-- ── 这个索引是整张表存在的意义 ──
--
-- 召回改成「原列 UNION 旁表」两个子查询各自排序再合并，正是为了让**两侧
-- 都能走自己的 HNSW**。写成 LATERAL / JOIN 之后再 ORDER BY 会让索引整个失效，
-- 退化成全表扫描 —— 那种查询在小库上跑得飞快，等库长大了才暴露，
-- 而那时它看起来像「数据库变慢了」而不是「索引没被用上」
CREATE INDEX idx_fact_embeddings_vec ON fact_embeddings
    USING hnsw (embedding vector_cosine_ops);

-- 回填的待办查询与 census（启动比对）都按模型过滤
CREATE INDEX idx_fact_embeddings_model ON fact_embeddings (embedding_model);

-- ══════════════════════════════════════════════════════════
--  entities
--
--  实体向量不参与四路召回，只用于**抽取期的实体消解**（同名/近义匹配）。
--  但它同样受换模型影响，而且失效方式更隐蔽：消解不上就会给同一个人
--  建出第二个实体，从此他的事实分裂在两个节点上，图遍历再也走不通。
-- ══════════════════════════════════════════════════════════

CREATE TABLE entity_embeddings (
    entity_id       ulid         NOT NULL REFERENCES entities(id),
    embedding_model TEXT         NOT NULL,
    embedding       VECTOR(1024) NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (entity_id, embedding_model)
);

CREATE INDEX idx_entity_embeddings_vec ON entity_embeddings
    USING hnsw (embedding vector_cosine_ops);

CREATE INDEX idx_entity_embeddings_model ON entity_embeddings (embedding_model);

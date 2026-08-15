-- Cortex 记忆系统初始 schema
-- 设计见 docs/memory.md；本文件是 schema 的权威版本，文档中的 SQL 片段以此为准。
--
-- 核心约束：全链路 append-only。除 redact/purge 外，不存在 UPDATE 与 DELETE。
-- 主键统一为 ULID（26 字符 Crockford base32 大写），客户端可离线生成。
-- 向量维度 1024，对应 bge-m3。更换模型见 docs/memory.md §七。
--
-- 【写入纪律】（详见 docs/memory.md §九，违反即破坏同步正确性）：
--   1. 所有需同步表的写入必须与 sync_log 追加在同一事务内完成；
--   2. 该事务开头执行 pg_advisory_xact_lock(4272)，把提交顺序串行化，
--      保证 sync_log.seq 的顺序 == 对读端的可见顺序（裸 BIGSERIAL 做不到这一点：
--      序列值在 INSERT 时分配，提交乱序会让先拉取的客户端永久漏行）；
--   3. 取号事务必须短小、纯写。LLM 调用与 embedding 计算一律在事务外完成；
--   4. tsv 与主行同事务写入，永不异步补写（补写是 UPDATE，且不进 sync_log 就不可见）。

CREATE EXTENSION IF NOT EXISTS vector;

-- ULID：26 字符 Crockford base32（无 I/L/O/U），大写。
-- COLLATE "C" 使 B-tree 逐字节比较——既快，又保证排序与生成时间一致。
CREATE DOMAIN ulid AS TEXT COLLATE "C"
    CHECK (VALUE ~ '^[0-9A-HJKMNP-TV-Z]{26}$');

-- SHA-256 十六进制小写
CREATE DOMAIN sha256 AS TEXT COLLATE "C"
    CHECK (VALUE ~ '^[0-9a-f]{64}$');

-- ══════════════════════════════════════════════════════════
--  同步日志（outbox）—— 多端同步的唯一事实序
--
--  每个业务写事务同时向此表追加一行。客户端增量拉取只面向本表：
--    GET /sync?since={seq} → 返回 seq > since 的记录及其对应业务行。
--  跨表全序天然成立（九张表各自的 BIGSERIAL 无法比较，单一游标在
--  数学上不成立——这正是本表存在的原因）。
--  按 log 序回放也保证了 FK 顺序（facts 永远在其 source episode 之后到达）。
--  附带收益：LISTEN/NOTIFY 挂在本表上即是 WebSocket 实时推送的事件源。
-- ══════════════════════════════════════════════════════════

CREATE TABLE sync_log (
    seq        BIGSERIAL   PRIMARY KEY,
    table_name TEXT        NOT NULL,
    record_id  TEXT        NOT NULL,   -- 目标行主键；episode_blobs 用 'episode_id:blob_hash'
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

-- ══════════════════════════════════════════════════════════
--  L0 原始层 —— 唯一真相来源，只追加，永不修改
-- ══════════════════════════════════════════════════════════

CREATE TABLE episodes (
    id          ulid        PRIMARY KEY,
    session_id  TEXT        NOT NULL,
    role        TEXT        NOT NULL CHECK (role IN ('user', 'assistant', 'tool', 'system')),
    -- 原始消息，含供应商特有的 thinking / reasoning 等不透明块，无损保存。
    -- 被 redact 后写占位 '{"redacted":true}'（见 docs/memory.md §十一）。
    content     JSONB       NOT NULL,
    -- 从 content 提取的纯文本，供全文检索
    text        TEXT,
    -- 由 Rust 侧 jieba-rs 分词后写入；与主行同事务，永不异步补写
    tsv         TSVECTOR,
    domain      TEXT,
    device_id   TEXT        NOT NULL,
    occurred_at TIMESTAMPTZ NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON COLUMN episodes.occurred_at IS '事件发生时间（客户端时钟，经单调化处理）';
COMMENT ON COLUMN episodes.created_at  IS '服务端入库时间';

-- ── 二进制内容：SHA-256 内容寻址，天然去重、天然不可变 ──

CREATE TABLE blobs (
    hash        sha256      PRIMARY KEY,
    mime        TEXT        NOT NULL,
    size_bytes  BIGINT      NOT NULL CHECK (size_bytes >= 0),
    storage_key TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE episode_blobs (
    episode_id ulid   NOT NULL REFERENCES episodes(id),
    blob_hash  sha256 NOT NULL REFERENCES blobs(hash),
    kind       TEXT,
    PRIMARY KEY (episode_id, blob_hash)
);

-- 异步 pipeline 产出：不转成文本的媒体是检索黑洞。
-- 写入前必须查 redactions 表，防止在途任务把已抹除内容回填（docs/memory.md §十一）。
CREATE TABLE blob_transcripts (
    id              ulid         PRIMARY KEY,
    blob_hash       sha256       NOT NULL REFERENCES blobs(hash),
    kind            TEXT         NOT NULL
                    CHECK (kind IN ('asr', 'vision_caption', 'ocr', 'frame_caption')),
    text            TEXT         NOT NULL,
    tsv             TSVECTOR,
    embedding       VECTOR(1024),
    -- 音视频转录按段落行，记录片段的毫秒偏移——支撑「出处跳转到 31:40」。
    -- 图片/OCR 为 NULL。
    span_start_ms   BIGINT,
    span_end_ms     BIGINT,
    transcribed_by  TEXT         NOT NULL,
    embedding_model TEXT         NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),
    CHECK (span_end_ms IS NULL OR span_start_ms IS NOT NULL)
);

-- ══════════════════════════════════════════════════════════
--  L1 事实层
-- ══════════════════════════════════════════════════════════

CREATE TABLE entities (
    id              ulid         PRIMARY KEY,
    -- person / project / file / org / concept / tool …… 开放不枚举
    kind            TEXT         NOT NULL,
    name            TEXT         NOT NULL,
    summary         TEXT,
    -- 实体向量用于抽取期的实体消解（同名/近义实体匹配），不参与四路召回
    embedding       VECTOR(1024),
    embedding_model TEXT         NOT NULL,
    device_id       TEXT         NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);

-- 别名消解：不修改 entities，追加一条合并记录。
--
-- UNIQUE(from_entity) 使每个实体至多一条出边：
--   · 归属图成为函数图，链式归属可确定性解析；
--   · 并发分叉（两台设备把 A 分别并进 B 和 C）物理上变为 first-writer-wins，
--     后到者被唯一约束拒绝，服务端回执客户端转 no-op；
--   · 环仍可能出现（A→B 与 B→A 是两条不同 from 的边），由 cortexd 写入前
--     沿 into_entity 链走到底做成环检测拒绝（见 docs/memory.md §九）。
CREATE TABLE entity_merges (
    id                ulid         PRIMARY KEY,
    from_entity       ulid         NOT NULL REFERENCES entities(id) UNIQUE,
    into_entity       ulid         NOT NULL REFERENCES entities(id),
    reason            TEXT,
    source_episode_id ulid         REFERENCES episodes(id),
    device_id         TEXT         NOT NULL,
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),
    CHECK (from_entity <> into_entity)
);

CREATE TABLE facts (
    id                ulid         PRIMARY KEY,
    subject_id        ulid         NOT NULL REFERENCES entities(id),
    -- prefers / decided / owns / blocked_by …… 开放不枚举，枚举则新领域进不来
    predicate         TEXT         NOT NULL,
    object_text       TEXT,
    -- 指向另一实体时，本行即构成知识图谱中的一条边
    object_entity_id  ulid         REFERENCES entities(id),
    -- 自然语言表述，用于向量化与界面展示。被 redact 后写占位 '[redacted]'
    statement         TEXT         NOT NULL,
    tsv               TSVECTOR,
    embedding         VECTOR(1024),
    embedding_model   TEXT         NOT NULL,
    domain            TEXT,
    -- 抽取器写入时一次性打分，此后不可变（append-only）。
    -- 语义：抽取器对「该事实忠实反映了原文」的置信度。可作检索融合的次要信号。
    confidence        REAL         NOT NULL DEFAULT 1.0
                      CHECK (confidence >= 0.0 AND confidence <= 1.0),

    -- 【事件时间】何时开始为真。NULL = 未知或一直如此
    valid_at          TIMESTAMPTZ,

    -- 出处：可审计的根基
    source_episode_id ulid         NOT NULL REFERENCES episodes(id),
    extracted_by      TEXT         NOT NULL,
    device_id         TEXT         NOT NULL,
    -- 【系统时间】Cortex 何时知道这件事
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),

    CHECK (object_text IS NOT NULL OR object_entity_id IS NOT NULL)
);

-- 事实的生命周期事件。系统自动失效与用户手工编辑共用此表。
-- 失效以追加记录表达，而非就地 UPDATE —— 这是 append-only 与多端无冲突的前提。
CREATE TABLE fact_events (
    id                ulid         PRIMARY KEY,
    fact_id           ulid         NOT NULL REFERENCES facts(id),

    -- invalidate 使事实失效；revoke 撤销失效（状态机语义：末态由最后一条
    -- 状态事件决定）；flag 标记「矛盾待人工确认」，是批注、不改变状态。
    op                TEXT         NOT NULL CHECK (op IN ('invalidate', 'revoke', 'flag')),
    -- op = invalidate 时的原因分类。四者时间语义不同，混用会导致认知回放出错：
    --   superseded 曾经为真，世界变了（superseded_by 必填，否则演化链断裂）
    --   corrected  从未为真，抽取错误
    --   retracted  用户主动删除，不对真值表态
    --   expired    曾经为真，到期失效
    -- 注意：corrected 占比只是「抽取精确率的有偏滞后下界」——它依赖检索曝光
    -- 与用户勤快度，测不到漏抽与未被检索的错误，不可当作抽取质量的直接度量。
    kind              TEXT         CHECK (kind IN ('superseded', 'corrected', 'retracted', 'expired')),

    -- 【事件时间】何时停止为真
    invalid_at        TIMESTAMPTZ,
    superseded_by     ulid         REFERENCES facts(id),

    actor             TEXT         NOT NULL CHECK (actor IN ('system', 'user')),
    reason            TEXT,
    source_episode_id ulid         REFERENCES episodes(id),
    device_id         TEXT         NOT NULL,
    -- 【系统时间】何时发现 / 何时操作
    created_at        TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp(),

    -- invalidate 必须给出原因分类与失效时间
    CHECK (op <> 'invalidate' OR (kind IS NOT NULL AND invalid_at IS NOT NULL)),
    -- revoke / flag 不携带 invalidate 专属字段
    CHECK (op = 'invalidate' OR (kind IS NULL AND invalid_at IS NULL AND superseded_by IS NULL)),
    -- superseded 必须指明被谁取代，否则「事实演化链」断裂
    CHECK (kind IS DISTINCT FROM 'superseded' OR superseded_by IS NOT NULL)
);

-- ══════════════════════════════════════════════════════════
--  L2 摘要层
-- ══════════════════════════════════════════════════════════

CREATE TABLE summaries (
    id              ulid         PRIMARY KEY,
    scope           TEXT         NOT NULL CHECK (scope IN ('session', 'topic', 'period')),
    scope_key       TEXT         NOT NULL,
    text            TEXT         NOT NULL,
    tsv             TSVECTOR,
    embedding       VECTOR(1024),
    embedding_model TEXT         NOT NULL,
    covers_from     TIMESTAMPTZ,
    covers_to       TIMESTAMPTZ,
    device_id       TEXT         NOT NULL,
    created_at      TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);

-- ══════════════════════════════════════════════════════════
--  抹除记录 —— 全系统唯一允许破坏 append-only 之处
--
--  用于密钥误粘贴、隐私等必须真正销毁内容的场景。
--  target_kind 只有 episode|blob：redact 按 source_episode_id 级联清除
--  全部派生落点（facts.statement/tsv/embedding、blob_transcripts、
--  相关 summaries 重新生成），语义与执行规则见 docs/memory.md §十一。
--  本表进 sync_log 下发；客户端收到后必须幂等执行同等本地清除。
--  让位的是内容，墓碑与审计链永久保留。
-- ══════════════════════════════════════════════════════════

CREATE TABLE redactions (
    id          ulid         PRIMARY KEY,
    target_kind TEXT         NOT NULL CHECK (target_kind IN ('episode', 'blob')),
    target_id   TEXT         NOT NULL,
    mode        TEXT         NOT NULL CHECK (mode IN ('redact', 'purge')),
    reason      TEXT         NOT NULL,
    actor       TEXT         NOT NULL,
    device_id   TEXT         NOT NULL,
    created_at  TIMESTAMPTZ  NOT NULL DEFAULT clock_timestamp()
);

-- ══════════════════════════════════════════════════════════
--  视图
-- ══════════════════════════════════════════════════════════

-- 每条事实的最新一次【状态】事件（flag 是批注，不参与状态判定）。
-- 排序用 (created_at, id) —— created_at 为 clock_timestamp() 微秒级，
-- id 仅作极端同刻的确定性 tiebreaker。
CREATE VIEW fact_status AS
SELECT DISTINCT ON (fact_id)
       fact_id, op, kind, invalid_at, superseded_by, actor,
       created_at AS decided_at
FROM fact_events
WHERE op IN ('invalidate', 'revoke')
ORDER BY fact_id, created_at DESC, id DESC;

-- 当前有效的事实：从未失效，或最近一次状态事件是 revoke（已恢复）
CREATE VIEW active_facts AS
SELECT f.*
FROM facts f
LEFT JOIN fact_status s ON s.fact_id = f.id
WHERE s.op IS NULL OR s.op = 'revoke';

-- 实体经别名合并后的最终归属。
-- UNIQUE(from_entity) 保证函数图（每实体至多一条出边），沿链走到底即终点；
-- depth 上限是环的兜底保险（环由写时守护阻止，见 entity_merges 注释）。
CREATE VIEW canonical_entities AS
WITH RECURSIVE walk(id, canonical, depth) AS (
    SELECT e.id, e.id, 0
    FROM entities e
  UNION ALL
    SELECT w.id, m.into_entity, w.depth + 1
    FROM walk w
    JOIN entity_merges m ON m.from_entity = w.canonical
    WHERE w.depth < 32
)
SELECT DISTINCT ON (id) id, canonical
FROM walk
ORDER BY id, depth DESC;

-- ══════════════════════════════════════════════════════════
--  索引
--
--  向量索引覆盖 facts / summaries / blob_transcripts（四路召回）
--  以及 entities（仅抽取期实体消解用，不参与召回）。
--  L0 episodes 数量是事实的几十倍，仅建全文索引 ——
--  精确回溯用「全文 + 时间」定位即可。此策略使召回向量规模
--  约为 episodes 的 1/50，十万级对话不会爆。
-- ══════════════════════════════════════════════════════════

-- 向量（HNSW，余弦距离）
CREATE INDEX idx_facts_vec       ON facts            USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_entities_vec    ON entities         USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_summaries_vec   ON summaries        USING hnsw (embedding vector_cosine_ops);
CREATE INDEX idx_transcripts_vec ON blob_transcripts USING hnsw (embedding vector_cosine_ops);

-- 全文（BM25 那一路召回）
CREATE INDEX idx_episodes_tsv    ON episodes         USING gin (tsv);
CREATE INDEX idx_facts_tsv       ON facts            USING gin (tsv);
CREATE INDEX idx_summaries_tsv   ON summaries        USING gin (tsv);
CREATE INDEX idx_transcripts_tsv ON blob_transcripts USING gin (tsv);

-- 图遍历（idx_facts_sp 前缀即覆盖 subject_id 单列查询，不另建）
CREATE INDEX idx_facts_object    ON facts (object_entity_id) WHERE object_entity_id IS NOT NULL;
-- 矛盾检测：按 (主语, 谓词) 查现存事实
CREATE INDEX idx_facts_sp        ON facts (subject_id, predicate);

-- 实体消解热路径：按 (kind, name) 精确匹配
CREATE INDEX idx_entities_kn     ON entities (kind, name);

-- 时间近因
CREATE INDEX idx_episodes_time   ON episodes (occurred_at DESC);
CREATE INDEX idx_episodes_sess   ON episodes (session_id, occurred_at);

-- 生命周期（与 fact_status 视图排序对齐）
CREATE INDEX idx_fact_events     ON fact_events (fact_id, created_at DESC);

-- redact 级联：按出处找派生行
CREATE INDEX idx_facts_source    ON facts (source_episode_id);

-- 换 embedding 模型时分批回填
CREATE INDEX idx_facts_model     ON facts (embedding_model);
CREATE INDEX idx_entities_model  ON entities (embedding_model);
CREATE INDEX idx_summaries_model ON summaries (embedding_model);

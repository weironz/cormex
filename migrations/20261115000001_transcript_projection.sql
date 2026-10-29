-- P 层（归一化投影）与原文档案索引。
--
-- 设计权威：docs/design/session-archive-architecture.md §4.2（D2 / D4 / D5 / D11 / D15）。
--
-- # 这一层是派生的，不是事实
--
-- `transcript_events` 完全由 R 层的字节算出来：换一个 `parser_version` 就是另一套
-- 投影，两套并存而不是互相覆盖。这是「解析器改了，历史会话也能跟着修好」的实现
-- 方式 —— 原文一直在，所以任何时候都能重算。
--
-- # 文本里没有密钥
--
-- `text` / `content` 里的疑似密钥**已经被替换成 `[masked:<rule>]`**（D5）。所以
-- 这张表及其下游（`archive_chunks`、索引、注入）都不含密钥，而 R 层保留原文。
-- 遮罩区间本身记在 `capture_masks`，不进任何索引。
--
-- # `tsv` 与主行同事务
--
-- 约束 1 在这里照样成立：`transcript_events.tsv` 与 `archive_chunks.tsv` 都在
-- INSERT 时一并写入，`ArchiveTxn::insert_transcript_events` 不接受「稍后补写」。
-- 分开写的症状是全文检索少了刚写进去的那条，而向量检索找得到 —— 看起来像
-- 「BM25 效果不好」。

-- ══════════════════════════════════════════════════════════
--  一、归一化事件
-- ══════════════════════════════════════════════════════════

CREATE TABLE transcript_events (
    id                 ulid        PRIMARY KEY,     -- Id::derived(occurred_at, "<session>|<parser_version>|<generation>|<offset>")
    session_id         ulid        NOT NULL REFERENCES capture_sessions(id),
    -- 例如 "claude_code@1"。**换解析器 = 换投影**，旧的那套原样保留
    parser_version     TEXT        NOT NULL,
    -- 会话内的位置，单调。分页游标用它
    seq                BIGINT      NOT NULL CHECK (seq >= 0),
    kind               TEXT        NOT NULL CHECK (kind IN
                           ('user_message', 'agent_message', 'agent_thought', 'tool_call',
                            'tool_result', 'plan', 'system', 'meta', 'unknown')),
    -- 【事件时间轴】会话里何时发生（`at` 用它）。来源没有时间戳时为 NULL
    occurred_at        TIMESTAMPTZ,
    -- ACP 对齐的内容块。超过阈值的块换成指回 R 层的引用（ResourceRef）
    content            JSONB       NOT NULL,
    -- 纯文本，供展示与检索。**已遮罩**
    text               TEXT,
    -- 与主行同一条 INSERT 写入（约束 1）
    tsv                TSVECTOR,
    tool_name          TEXT,
    tool_call_id       TEXT,
    -- 出处：这条事件来自哪一代的哪个字节区间
    src_generation_id  ulid        NOT NULL REFERENCES capture_generations(id),
    src_offset         BIGINT      NOT NULL,
    src_length         BIGINT      NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- projector 靠派生 id + ON CONFLICT DO NOTHING 实现幂等；这条约束是它的前提
    UNIQUE (session_id, parser_version, seq)
);

COMMENT ON TABLE transcript_events IS
    'P 层：一条归一化的对话事件（append-only，不进 sync_log）。由解析器从 R 层投影而来，
     可重算；文本里的疑似密钥已被替换成 [masked:…]。';

CREATE INDEX idx_transcript_events_time ON transcript_events (session_id, parser_version, occurred_at);
CREATE INDEX idx_transcript_events_tsv  ON transcript_events USING gin (tsv);

-- ══════════════════════════════════════════════════════════
--  二、projector 的进度
-- ══════════════════════════════════════════════════════════

-- append-only，取最新一行。`through` 是 {generation_id: 已投影到的字节偏移}：
-- 一个会话可能有多个文件、每个文件有自己的代，所以进度是一个映射而不是一个数
CREATE TABLE transcript_projections (
    id              ulid        PRIMARY KEY,
    session_id      ulid        NOT NULL REFERENCES capture_sessions(id),
    parser_version  TEXT        NOT NULL,
    through         JSONB       NOT NULL,
    next_seq        BIGINT      NOT NULL,
    status          TEXT        NOT NULL CHECK (status IN ('ok', 'partial', 'failed')),
    -- 失败时的错误摘要。**不重试风暴**：同一个错误 1 小时内不重试
    error           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE transcript_projections IS
    'projector 的进度（append-only，不进 sync_log）。取最新一行即当前状态；
     through 是每个代已投影到的偏移。';

CREATE INDEX idx_transcript_projections ON transcript_projections (session_id, parser_version, created_at DESC);

-- ══════════════════════════════════════════════════════════
--  三、原文档案索引（D4 / D15）
-- ══════════════════════════════════════════════════════════

-- 含工具输出的全文，**只在 scope=archive 时使用**（D4）。默认检索与注入不碰它。
-- 遮罩区间早已在 P 层替换掉，所以这里也不含密钥。
CREATE TABLE archive_chunks (
    id          ulid        PRIMARY KEY,
    session_id  ulid        NOT NULL REFERENCES capture_sessions(id),
    event_id    ulid        NOT NULL REFERENCES transcript_events(id),
    -- 长事件切成多段，每段 2 KiB
    part        INTEGER     NOT NULL CHECK (part >= 0),
    text        TEXT        NOT NULL,
    tsv         TSVECTOR    NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (event_id, part)
);

COMMENT ON TABLE archive_chunks IS
    '原文档案索引（append-only，不进 sync_log）：含工具输出的全文，只在 scope=archive
     时使用。默认检索与注入不碰它。';

CREATE INDEX idx_archive_chunks_tsv ON archive_chunks USING gin (tsv);

-- ══════════════════════════════════════════════════════════
--  四、原文访问审计
-- ══════════════════════════════════════════════════════════

-- 每一次读原文都记一笔。**为什么值得一张表**：`raw:read` 是唯一能把未遮罩原文
-- 交出去的权限，而「谁在什么时候读了哪一段」是事后唯一能回答的问题。不记的话，
-- 一次越权读和一次正常读在日志里长得一样。
CREATE TABLE archive_access_log (
    id           ulid        PRIMARY KEY,
    principal    TEXT        NOT NULL,
    generation_id ulid       NOT NULL REFERENCES capture_generations(id),
    byte_offset  BIGINT      NOT NULL,
    byte_length  BIGINT      NOT NULL,
    -- 这一读是否绕过了遮罩
    unmasked     BOOLEAN     NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE archive_access_log IS
    '原文访问审计（append-only，不进 sync_log）。每次读原文记一笔，unmasked 标明
     是否绕过了遮罩。';

CREATE INDEX idx_archive_access_log ON archive_access_log (generation_id, created_at DESC);
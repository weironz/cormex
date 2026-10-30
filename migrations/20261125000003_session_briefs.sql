-- 交接简报的指针。
--
-- 设计：docs/design/session-archive-architecture.md §8.4（D14）。
--
-- # 简报的内容存在 summaries 里，这张表只记「哪一条是当前简报」
--
-- 设计说「存为 summary」，那一条 summary 就是简报本身（可检索、可同步、可被
-- 血缘找到）。但 summaries 没有「这是简报还是观察条目」这一列，而给同步表加列
-- 是一次全量 schema 变更 —— 为了一个可以由旁表回答的问题不值得。
--
-- 更重要的是：这张表回答的是**「有没有比最后一条事件更新的简报」**，而那是一个
-- 关于进度的判断，不是一个关于内容的判断。用 `through_seq` 直接比较，比在
-- summaries 里按 created_at 猜要准确得多 —— 后者在时钟回拨或并发写入时会给出
-- 「有简报」而实际上它覆盖不到最新事件。
--
-- # 为什么旧简报保留
--
-- 会话有新事件就生成新的一条，旧的留着。这不是浪费：交接简报的价值一半在于
-- 「上次交接时我们以为的状态是什么」，而那需要旧的那条还在。

CREATE TABLE session_briefs (
    id          ulid        PRIMARY KEY,
    session_id  ulid        NOT NULL REFERENCES capture_sessions(id),
    -- 简报内容所在的 summaries 行
    summary_id  ulid        NOT NULL,
    -- 这条简报覆盖到的最后一个事件 seq。与「会话当前最大 seq」比较即知是否过期
    through_seq BIGINT      NOT NULL CHECK (through_seq >= 0),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE session_briefs IS
    '交接简报的指针（append-only，不进 sync_log）：哪一条 summary 是当前简报，
     以及它覆盖到哪个事件。取最新一行即当前状态；旧简报保留。';

CREATE INDEX idx_session_briefs ON session_briefs (session_id, created_at DESC);
-- 按项目关闭原文档案索引。
--
-- 设计：docs/plan/session-archive-plan.md P3-2。
--
-- # 为什么是事件表而不是一个开关列
--
-- 全链路 append-only：状态列要 UPDATE，而这里没有 UPDATE。事件表取最新一行
-- 即当前状态，与 `session_events` / `project_events` 同一个形状。
--
-- 而且「什么时候关的」本身是用户会问的问题 —— 关掉之后搜不到某段时间的原文，
-- 那是预期的，但「从哪一天起搜不到了」需要一个答案。
--
-- # 为什么按项目而不是按会话
--
-- 用户的实际诉求是「这个项目别索引」，而不是「这一条别索引」。按会话关会让
-- 他每来一段新会话就要关一次，而那件事一定会被忘掉 —— 忘掉的症状是「我以为
-- 关了，其实还在索引」。

CREATE TABLE archive_index_events (
    id          ulid        PRIMARY KEY,
    -- 由 cwd 推出的项目标识（git remote 规范化后的 host/owner/repo）
    project_key TEXT        NOT NULL CHECK (length(project_key) BETWEEN 1 AND 512),
    op          TEXT        NOT NULL CHECK (op IN ('exclude', 'include')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE archive_index_events IS
    '按项目关闭 / 恢复原文档案索引（append-only，不进 sync_log）。取最新一行即当前状态。';

CREATE INDEX idx_archive_index_events ON archive_index_events (project_key, created_at DESC);
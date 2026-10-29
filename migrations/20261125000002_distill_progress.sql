-- distiller 的进度。
--
-- 设计：docs/design/session-archive-architecture.md §8.4（D14）。
--
-- # 为什么需要一张进度表
--
-- masker 和 projector 的待办都能从数据本身推出来（「末尾 > 进度」），所以它们
-- 没有队列表。distiller 不行：它的输入是「上次之后的新事件」，而**上次是哪一次
-- 只存在于上一次运行里**。想从 `summaries` / `derivations` 反推也不行 —— 模型
-- 不保证引用每一个输入事件，于是「引用了的最大 seq」会把中间没被引用的那些
-- 当成已处理，或者反过来重复处理。
--
-- 所以这里和 `capture_mask_progress` / `transcript_projections` 是同一个形状：
-- append-only，取最新一行即当前状态。设计 §5 明确把「显式进度表」列为下游消费者
-- 的做法，这不是例外。
--
-- # 为什么不进 sync_log
--
-- 归档层（`table::ARCHIVE`）一律不下发（D3）。进度是服务端的记账，客户端拿到
-- 它没有任何用处。

CREATE TABLE distill_progress (
    id          ulid        PRIMARY KEY,
    session_id  ulid        NOT NULL REFERENCES capture_sessions(id),
    -- 已加工到的最后一个事件的 seq（含）。-1 表示「一个都还没加工」，所以这里
    -- 允许负数：0 是一个合法的 seq，用 0 当哨兵会把第一条事件永远漏掉
    through_seq BIGINT      NOT NULL CHECK (through_seq >= -1),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE distill_progress IS
    'distiller 的进度（append-only，不进 sync_log）。取最新一行即当前状态：
     through_seq 是已加工到的最后一个事件 seq。';

CREATE INDEX idx_distill_progress ON distill_progress (session_id, created_at DESC);
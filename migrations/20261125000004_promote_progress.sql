-- 晋升（P 层事件 → episodes）的进度。
--
-- 设计：docs/design/session-archive-architecture.md §8.4（D14）。
--
-- # 为什么与 distill_progress 分开
--
-- 两者加工的是同一批事件，但**不是同一件事**：观察日志按批走（一批事件出一条
-- 观察），晋升按事件走（每条被选中的消息变成一个 episode）。用一张表记两种进度，
-- 其中一个先跑完就会让另一个以为「已经处理过了」—— 而症状是「有的事实永远抽不
-- 出来」，没有任何报错。
--
-- 所以这里是设计 §5 那个形状的第三次应用：append-only，取最新一行即当前状态。

CREATE TABLE promote_progress (
    id          ulid        PRIMARY KEY,
    session_id  ulid        NOT NULL REFERENCES capture_sessions(id),
    -- 已晋升到的最后一个事件的 seq（含）。-1 = 一个都还没晋升
    through_seq BIGINT      NOT NULL CHECK (through_seq >= -1),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE promote_progress IS
    '晋升的进度（append-only，不进 sync_log）：已把哪些事件写成 episodes。
     与 distill_progress 分开，因为两者按不同的粒度加工同一批事件。';

CREATE INDEX idx_promote_progress ON promote_progress (session_id, created_at DESC);
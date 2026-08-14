-- active_facts 挡住了 facts 上的每一个索引 —— 尤其是 HNSW
--
-- ── 病灶 ──
--
-- 原定义是 `facts LEFT JOIN fact_status`，而 fact_status 是 fact_events 上的
-- 一个 DISTINCT ON。planner 必须**先把 join 做完**才能按距离排序，于是
-- `ORDER BY embedding <=> $1 LIMIT 24` 拿到的是一堆已经落地的行，
-- idx_facts_vec 从来没被用上过。EXPLAIN（5 万条事实）：
--
--   Limit                                    ← 289.7 ms, 19.8 万次缓冲访问
--     Sort (top-N heapsort)
--       Hash Left Join  rows=43842           ← 算了 4.3 万次 1024 维距离
--         Seq Scan on facts        rows=50092
--         Hash ← Seq Scan on fact_events
--
-- 库小的时候完全看不出来，所以这个洞从 init migration 起就一直在。
-- 它暴露的时机恰好是「用户把几年历史导进来」的那一天。
--
-- ── 为什么换成两层反连接就好了 ──
--
-- 关键**不是** NOT EXISTS 本身，而是**没有 LIMIT**：带 LIMIT 1 的相关子查询
-- 会被按死成逐行子计划，行少时飞快、行多时灾难。写成两层反连接之后
-- planner 有得选 —— 行少走嵌套循环 + 索引探测，行多走哈希反连接。
-- 三种写法都实测过（5 万事实 / 1.17 万状态事件 / 12.2% 已失效）：
--
--   定义                     向量 ANN   BM25 窄   BM25 宽   全量 count   按 subject
--   老 LEFT JOIN              289.7ms    8.1ms    46.6ms      17.3ms       8.5ms
--   NOT EXISTS + LIMIT 1        1.2ms    0.0ms   109.1ms      67.4ms       0.1ms  ← 宽查询崩了
--   NOT EXISTS(fact_status)    12.5ms    4.6ms    30.4ms      10.9ms      11.3ms  ← 点查变慢
--   两层反连接（本次）           0.5ms    0.0ms    25.2ms      13.1ms       0.1ms  ← 五项全胜
--
-- ── 语义与老定义严格等价 ──
--
-- 老：{invalidate, revoke} 里**最后**那条是 invalidate → 无效。
-- 新：存在一条 invalidate，且它之后没有任何 revoke → 无效。
--
-- 逐种时序对过：只 invalidate / invalidate→revoke / invalidate→revoke→invalidate
-- / 只 revoke / 无事件。两个定义的行集在 5 万条数据上双向差集都是 0。
--
-- 排序键 `(created_at, id)` 与 fact_status 的 `ORDER BY created_at DESC, id DESC`
-- 一致 —— created_at 是 clock_timestamp() 微秒级，id 只在极端同刻做确定性
-- tiebreaker。**这两处必须同时改**，不然「同一微秒内 revoke 与 invalidate
-- 各一条」会被两个定义判成相反的结果。

CREATE OR REPLACE VIEW active_facts AS
SELECT f.*
FROM facts f
WHERE NOT EXISTS (
    SELECT 1
      FROM fact_events e
     WHERE e.fact_id = f.id
       AND e.op = 'invalidate'
       AND NOT EXISTS (
           SELECT 1
             FROM fact_events r
            WHERE r.fact_id = e.fact_id
              AND r.op = 'revoke'
              AND (r.created_at, r.id) > (e.created_at, e.id)
       )
);

-- 状态探测专用的偏索引。
--
-- 已有的 idx_fact_events (fact_id, created_at DESC) 也能用，但它覆盖**全部**
-- 事件（含 flag 那类批注，那些不参与状态判定）。带谓词的偏索引更小，
-- 而且 id 进了索引键之后连 tiebreaker 都不用回表。
CREATE INDEX IF NOT EXISTS idx_fact_events_status
    ON fact_events (fact_id, created_at DESC, id DESC)
 WHERE op IN ('invalidate', 'revoke');

-- ⚠️ 给 facts 加列时**仍然**要重建这个视图。
-- `SELECT f.*` 在建视图那一刻就被展开成固定列表，新列不会自动出现
-- （见 20260807000006 的同款告警）。换成反连接不改变这条。

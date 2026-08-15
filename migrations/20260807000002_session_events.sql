-- 会话生命周期：改名、归档、工作区绑定
--
-- 【为什么要一张新表，而不是往 episodes 里塞控制记录】
--
-- 改名要存一份用户给的标题，归档要存一个状态，绑定工作区要存一个路径 ——
-- 三者都不是「原始消息」。用 episodes 模拟等于在 L0 原始层开一条控制通道，
-- 而 L0 的契约是「原始消息无损保存」：此后事实抽取会把它当语料、时间近因
-- 那一路召回会命中它、/sync 会把它下发给三端、每个客户端都要学会认出并跳过它。
-- 一次 migration 的代价会被摊成永久的复杂度。见 docs/memory.md §四。
--
-- 【为什么是事件流，不是可 UPDATE 的 sessions 表】
--
-- 全链路 append-only（CLAUDE.md 约束一）。改名不是就地改一个字段，而是追加
-- 一条 rename 事件；末态由最后一条同类事件决定 —— 与 fact_events 同款状态机。
-- 这样多端并发改名不会互相覆盖成一个谁也没写过的中间态：按 sync_log 全序回放，
-- 每台设备算出的末态都一样。

CREATE TABLE session_events (
    id          ulid        PRIMARY KEY,

    -- 没有 sessions 表，也就没有外键。session_id 与 episodes.session_id 同源，
    -- 由客户端生成。刻意允许「给一个还没有任何消息的会话写事件」：
    -- 用户在界面上新建会话、先选好工作区、再发第一句话，是最自然的顺序。
    session_id  TEXT        NOT NULL,

    -- 三个互相独立的维度，各自是一台状态机（见 session_state 视图）：
    --   rename                          → 标题
    --   archive / unarchive             → 归档
    --   bind_workspace / unbind_workspace → 工作区
    -- 合成一张表而不是三张：它们共享「会话的末态」这一个概念，
    -- 拆开会让 session_state 变成三次 UNION，而收益是零。
    op          TEXT        NOT NULL CHECK (op IN (
                    'rename', 'archive', 'unarchive',
                    'bind_workspace', 'unbind_workspace')),

    -- op = rename 时的新标题
    title       TEXT,
    -- op = bind_workspace 时的本机目录绝对路径。
    -- 服务端在写入前必须校验（存在、是目录、不是系统目录），见 cortexd。
    -- 路径是**本机**概念：多端同步下发这一行时，别的设备上同一个路径可能
    -- 根本不存在 —— 客户端应把它当作「这台机器上的绑定」，不存在就当未绑定。
    workspace   TEXT,

    actor       TEXT        NOT NULL CHECK (actor IN ('system', 'user')),
    device_id   TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    -- 每个 op 只能带自己那一个载荷字段。两侧都要写：
    -- 只写「rename 必须有 title」会放过「archive 带着 title」这种脏数据，
    -- 而脏数据在视图里表现为「归档一下标题就变了」，极难往回查。
    CHECK (op <> 'rename' OR title IS NOT NULL),
    CHECK (op =  'rename' OR title IS NULL),
    CHECK (op <> 'bind_workspace' OR workspace IS NOT NULL),
    CHECK (op =  'bind_workspace' OR workspace IS NULL),

    -- 上限是防御性的：标题与路径都来自客户端，没有上限时一条 10 MB 的
    -- 「标题」会进 sync_log 下发给所有设备
    CHECK (title IS NULL OR (length(title) BETWEEN 1 AND 200)),
    CHECK (workspace IS NULL OR (length(workspace) BETWEEN 1 AND 4096))
);

COMMENT ON TABLE session_events IS
    '会话生命周期事件（append-only）。归档 ≠ 删除：归档只是从默认列表隐藏，
     消息与派生记忆一概不动，随时可 unarchive 恢复。真正销毁内容的是
     redactions 表的 redact / purge —— 那是全系统唯一允许破坏 append-only
     的操作，必须显式触发、二次确认、留墓碑（docs/memory.md §十一）。
     两者语义不同，界面上也不该用同一个按钮。';

-- 与 session_state 视图的排序对齐（同 idx_fact_events 的思路）
CREATE INDEX idx_session_events ON session_events (session_id, created_at DESC);
-- 视图按 op 分维度取末条，全表扫描在会话数上是线性的；
-- 这个索引让每个维度各自走一次有序扫描
CREATE INDEX idx_session_events_op ON session_events (op, session_id, created_at DESC);

-- ══════════════════════════════════════════════════════════
--  session_state —— 每个会话的当前末态
--
--  【为什么不是一句 DISTINCT ON (session_id)】
--
--  三个维度互相独立，「整表最后一条事件」根本不是任何一个维度的末态：
--  序列 [archive, rename] 的最后一条是 rename，但会话仍然处于归档中。
--  因此每个维度各取各的末条，再拼起来。
--
--  排序 (created_at DESC, id DESC) 与 fact_status 一致：created_at 是
--  clock_timestamp() 的微秒值，id（ULID，COLLATE "C" 逐字节可比）只在
--  极端同刻做确定性 tiebreaker —— 没有它，两台设备在同一微秒改名会让
--  不同副本算出不同的标题，而这种不一致既不报错也不收敛。
-- ══════════════════════════════════════════════════════════

CREATE VIEW session_state AS
WITH known AS (
    SELECT DISTINCT session_id FROM session_events
),
last_title AS (
    SELECT DISTINCT ON (session_id) session_id, title, created_at
      FROM session_events WHERE op = 'rename'
     ORDER BY session_id, created_at DESC, id DESC
),
last_archive AS (
    SELECT DISTINCT ON (session_id) session_id, op, created_at
      FROM session_events WHERE op IN ('archive', 'unarchive')
     ORDER BY session_id, created_at DESC, id DESC
),
last_workspace AS (
    SELECT DISTINCT ON (session_id) session_id, op, workspace, created_at
      FROM session_events WHERE op IN ('bind_workspace', 'unbind_workspace')
     ORDER BY session_id, created_at DESC, id DESC
)
SELECT k.session_id,
       t.title,
       -- 从未有过归档事件 = 未归档。coalesce 而非裸比较：
       -- NULL 会让 WHERE NOT archived 把这些会话全过滤掉
       coalesce(a.op = 'archive', false)                            AS archived,
       CASE WHEN w.op = 'bind_workspace' THEN w.workspace END       AS workspace,
       -- GREATEST 在 Postgres 里忽略 NULL，因此只有三个维度都没事件时才为 NULL，
       -- 而那种会话根本不会出现在 known 里
       greatest(t.created_at, a.created_at, w.created_at)           AS decided_at
  FROM known k
  LEFT JOIN last_title     t ON t.session_id = k.session_id
  LEFT JOIN last_archive   a ON a.session_id = k.session_id
  LEFT JOIN last_workspace w ON w.session_id = k.session_id;

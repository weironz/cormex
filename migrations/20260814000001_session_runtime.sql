-- 会话的**执行归属**：这段对话在哪儿跑。
--
-- # 为什么需要它
--
-- 同一个会话在两端各自静默跑，而两端指着**完全不同的文件系统**：
-- Web 走云端容器的 /workspace，桌面端走 workspaces.json 里那个本机目录。
-- 界面上没有任何东西说明这件事，于是用户在 A 端让 agent 写了个文件、
-- 到 B 端接着聊，agent 说「没有这个文件」——而它说的是实话，只是换了个世界。
--
-- 更糟的一半是桌面端：会话没有本地绑定时 agent 走 `Turn::sealed()`，
-- **一个文件工具都没有，而且不报错**。用户只会觉得它突然变笨了。
--
-- 调研过的四家（Claude Code / Codex / Cursor / OpenHands）没有一家允许
-- 「同一个会话在两端各跑各的」——执行环境是会话身份的一部分。这一列就是
-- 把那件事写下来。
--
-- # 为什么只有两个值，而不是「哪一台设备」
--
-- 想写 `local:<device_id>` 的话得先有客户端设备身份，而链路上没有：
-- `session_events.device_id` 盖的是 **cortexd 自己**的章（`config.device_id`），
-- 不是发起那台机器的。
--
-- 但那个身份**不必引入**：本地绑定存在 `workspaces.json`，它天然只存在于
-- 那一台机器上。于是「这台机器有没有这个会话的绑定」本身就是设备检查，
-- 而且判据比一个 id 更硬——id 会在重装/克隆后骗人，绑定不会。
--
-- 代价是说不出「在哪一台」，只能说「绑的是另一台机器上的目录」。
-- 将来真有了设备身份，这一列换成 `local:<id>` 即可，语义不变。
--
-- # 为什么 NULL 就是 cloud
--
-- 存量会话一条都没有这个事件，而它们**绝大多数**要么是纯聊天、要么是
-- Web 会话——两者都该是 cloud。反过来（NULL 当 local）会让每一条老会话
-- 在桌面端变成「绑的是别的机器」，那是一次凭空的全量失效。
--
-- 桌面端上那些真的绑了本机目录的老会话，第一次在本机打开时由客户端
-- 补写一条 set_runtime='local'（它此刻正好握着 workspaces.json）。

ALTER TABLE session_events ADD COLUMN runtime TEXT;

-- 老的白名单容不下这个新动作。**不加 IF EXISTS**：名字对不上就该当场失败，
-- 静默保留旧约束的后果是这个 op 在运行时被数据库拒，而那时代码早已发布
ALTER TABLE session_events DROP CONSTRAINT session_events_op_check;
ALTER TABLE session_events ADD CONSTRAINT session_events_op_check CHECK (op IN (
    'rename', 'archive', 'unarchive',
    'bind_workspace', 'unbind_workspace',
    'move_to_project', 'remove_from_project',
    'set_runtime'));

-- 每个 op 只带自己那一个载荷，两侧都写（同表其余 CHECK 的理由）
ALTER TABLE session_events ADD CONSTRAINT session_events_runtime_required
    CHECK (op <> 'set_runtime' OR runtime IS NOT NULL);
ALTER TABLE session_events ADD CONSTRAINT session_events_runtime_absent
    CHECK (op =  'set_runtime' OR runtime IS NULL);
ALTER TABLE session_events ADD CONSTRAINT session_events_runtime_value
    CHECK (runtime IS NULL OR runtime IN ('cloud', 'local'));

COMMENT ON COLUMN session_events.runtime IS
    'set_runtime 的载荷：cloud（云端容器工作区，处处可续）或 local
     （钉在某台机器的本机目录上）。NULL = 没有这个 op。';

-- ══════════════════════════════════════════════════════════
--  session_state —— 重建，加上第五台状态机
--
--  五个维度仍然互相独立：[set_runtime, archive] 之后会话既有执行归属
--  也在归档中。整表最后一条依旧不是任何一个维度的末态。
-- ══════════════════════════════════════════════════════════

DROP VIEW session_state;

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
),
last_project AS (
    SELECT DISTINCT ON (session_id) session_id, op, project_id, created_at
      FROM session_events WHERE op IN ('move_to_project', 'remove_from_project')
     ORDER BY session_id, created_at DESC, id DESC
),
last_runtime AS (
    SELECT DISTINCT ON (session_id) session_id, runtime, created_at
      FROM session_events WHERE op = 'set_runtime'
     ORDER BY session_id, created_at DESC, id DESC
)
SELECT k.session_id,
       t.title,
       coalesce(a.op = 'archive', false)                            AS archived,
       CASE WHEN w.op = 'bind_workspace' THEN w.workspace END       AS workspace,
       -- 指向**已删除项目**的绑定按未分组处理。
       --
       -- 判定放在视图里而不是每个调用方各写一遍：删项目刻意不级联改写会话
       -- （见 project_events 表头），于是「悬挂的 project_id」是常态而非异常。
       CASE WHEN p.op = 'move_to_project'
             AND EXISTS (SELECT 1 FROM project_state ps
                          WHERE ps.project_id = p.project_id)
            THEN p.project_id END                                   AS project_id,
       -- **缺省在视图里定死，不留给每个调用方**。留给调用方的话，
       -- 「NULL 算 cloud 还是算 local」会在 cortexd、cortex-local、
       -- 两个客户端里各判一遍，而其中一处判反就是「老会话全部失效」
       coalesce(r.runtime, 'cloud')                                 AS runtime,
       -- GREATEST 在 Postgres 里忽略 NULL，因此只有五个维度都没事件时才为 NULL，
       -- 而那种会话根本不会出现在 known 里
       greatest(t.created_at, a.created_at, w.created_at,
                p.created_at, r.created_at)                         AS decided_at
  FROM known k
  LEFT JOIN last_title     t ON t.session_id = k.session_id
  LEFT JOIN last_archive   a ON a.session_id = k.session_id
  LEFT JOIN last_workspace w ON w.session_id = k.session_id
  LEFT JOIN last_project   p ON p.session_id = k.session_id
  LEFT JOIN last_runtime   r ON r.session_id = k.session_id;

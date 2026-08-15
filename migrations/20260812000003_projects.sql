-- 项目：会话的分组容器
--
-- 【为什么又是一张事件表，而不是可 UPDATE 的 projects 表】
--
-- 与 session_events 同一个理由（见 20260807000002 的表头）：全链路 append-only，
-- 改名靠追加一条 rename 而不是就地改字段。多端并发改名按 sync_log 全序回放，
-- 每台设备算出的末态都一样；UPDATE 语义下则是「谁最后到谁赢」，
-- 而两台设备各自都认为自己最后到。
--
-- 【为什么只有 create / rename / delete 三个 op，没有 archive】
--
-- 项目本身就是个轻量分组，它的内容（会话与记忆）另有归档与抹除两条路。
-- 再给容器加一层「已归档的项目」，界面上就要多一个过滤器 —— 而那个过滤器
-- 没人会用：想把一个项目收起来的人，直觉是删掉它，不是归档它。
-- 少一个状态维度，也就少一整类「项目归档了但里面的会话还在默认列表里」
-- 这种要逐处定义的组合语义。
--
-- 【delete 删掉的是分组，不是内容】
--
-- delete 是墓碑事件：项目从列表消失，里面的会话变成**未分组**
-- （session_state 里指向已删除项目的绑定按 NULL 处理，见下面视图里那句 EXISTS），
-- 会话、消息、附件、已抽取的事实一概不动。这与「归档会话」是同一套克制：
-- 真正销毁内容的只有 redactions 的 redact / purge，那条路要显式触发、
-- 二次确认、留墓碑（docs/memory.md §十一）。
--
-- 刻意**不**在 delete 时级联写一批 remove_from_project 事件：那是 N 条写入，
-- 且与「用户只是想收起这个分组」的意图不符 —— 万一是误删，重建一个同 id 的
-- create 事件就能让所有会话原样回来。级联写过去则回不来了。

CREATE TABLE project_events (
    id          ulid        PRIMARY KEY,

    -- 与 session_events.session_id 同源：没有 projects 表，也就没有外键，
    -- id 由客户端生成。允许「先建项目、里面一个会话都没有」——
    -- 用户新建项目再往里拖会话是最自然的顺序
    project_id  TEXT        NOT NULL,

    -- 两台状态机（见 project_state 视图）：
    --   create / delete → 存在与否
    --   create / rename → 当前名字
    -- create 同时是两台机的起点，所以它出现在两边
    op          TEXT        NOT NULL CHECK (op IN ('create', 'rename', 'delete')),

    -- op = create / rename 时的名字
    name        TEXT,

    actor       TEXT        NOT NULL CHECK (actor IN ('system', 'user')),
    device_id   TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    -- 两侧都要写，理由同 session_events：只写「create 必须有 name」会放过
    -- 「delete 带着 name」这种脏数据，而它在视图里表现为「删一下名字就变了」
    CHECK (op =  'delete' OR name IS NOT NULL),
    CHECK (op <> 'delete' OR name IS NULL),

    -- 上限是防御性的：名字与 id 都来自客户端，没有上限时一条 10 MB 的
    -- 「项目名」会进 sync_log 下发给所有设备
    CHECK (name IS NULL OR (length(name) BETWEEN 1 AND 100)),
    CHECK (length(project_id) BETWEEN 1 AND 64)
);

COMMENT ON TABLE project_events IS
    '项目生命周期事件（append-only）。项目是会话的分组容器，删除它只是
     解散分组：里面的会话变成未分组，消息与派生记忆一概不动。
     真正销毁内容的是 redactions 的 redact / purge，语义不同，
     界面上也不该用同一个按钮。';

-- 与 project_state 视图里两次 DISTINCT ON 的排序对齐
CREATE INDEX idx_project_events ON project_events (project_id, created_at DESC);
CREATE INDEX idx_project_events_op ON project_events (op, project_id, created_at DESC);

-- ══════════════════════════════════════════════════════════
--  project_state —— 现存的项目及其当前名字
--
--  【为什么存在与否要单独一台状态机】
--
--  与 session_state 同款：序列 [create, delete, rename] 的最后一条是 rename，
--  但项目已经被删了。「整表最后一条」在任何多维度事件表上都是错的判据。
--
--  排序 (created_at DESC, id DESC) 与 session_state / fact_status 一致：
--  created_at 是 clock_timestamp() 的微秒值，id（ULID，COLLATE "C" 逐字节
--  可比）只在极端同刻做确定性 tiebreaker —— 没有它，两台设备在同一微秒
--  改名会让不同副本算出不同的名字，而这种不一致既不报错也不收敛。
-- ══════════════════════════════════════════════════════════

CREATE VIEW project_state AS
WITH last_name AS (
    SELECT DISTINCT ON (project_id) project_id, name
      FROM project_events WHERE op IN ('create', 'rename')
     ORDER BY project_id, created_at DESC, id DESC
),
last_life AS (
    SELECT DISTINCT ON (project_id) project_id, op, created_at
      FROM project_events WHERE op IN ('create', 'delete')
     ORDER BY project_id, created_at DESC, id DESC
)
SELECT l.project_id,
       n.name,
       -- 末条生命周期事件就是那条 create，所以它的时间即「建于何时」。
       -- 不另取「首条 create」：同 id 的重建（撤销误删）之后，用户心里的
       -- 创建时间是重建那一刻，而不是一个已经不存在过的旧项目的时间
       l.created_at
  FROM last_life l
  -- 内连而非左连：活着的项目必然至少有那条 create，而 create 必带 name。
  -- 写成 LEFT JOIN 只会让调用方为一个不可能出现的 NULL 写分支
  JOIN last_name n ON n.project_id = l.project_id
 -- 只有「最后一条生命周期事件是 create」才算存在。
 -- 写成 l.op <> 'delete' 是等价的，但这句更直白地说出了判据
 WHERE l.op = 'create';

-- ══════════════════════════════════════════════════════════
--  会话侧：第四个维度
-- ══════════════════════════════════════════════════════════

-- 归属哪个项目。NULL = 未分组
ALTER TABLE session_events ADD COLUMN project_id TEXT;

-- 原来的 op 白名单容不下两个新动作。**不加 IF EXISTS**：
-- 名字对不上时就该让 migration 当场失败 —— 静默保留旧约束的后果是
-- 「移入项目」这个动作在运行时被数据库拒绝，而那时代码早已发布
ALTER TABLE session_events DROP CONSTRAINT session_events_op_check;
ALTER TABLE session_events ADD CONSTRAINT session_events_op_check CHECK (op IN (
    'rename', 'archive', 'unarchive',
    'bind_workspace', 'unbind_workspace',
    'move_to_project', 'remove_from_project'));

-- 每个 op 只能带自己那一个载荷字段，两侧都写（同表内其余 CHECK 的理由）
ALTER TABLE session_events ADD CONSTRAINT session_events_project_required
    CHECK (op <> 'move_to_project' OR project_id IS NOT NULL);
ALTER TABLE session_events ADD CONSTRAINT session_events_project_absent
    CHECK (op =  'move_to_project' OR project_id IS NULL);
ALTER TABLE session_events ADD CONSTRAINT session_events_project_id_len
    CHECK (project_id IS NULL OR (length(project_id) BETWEEN 1 AND 64));

-- ══════════════════════════════════════════════════════════
--  session_state —— 重建，加上第四台状态机
--
--  四个维度仍然互相独立：[move_to_project, archive] 之后会话既在项目里
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
)
SELECT k.session_id,
       t.title,
       coalesce(a.op = 'archive', false)                            AS archived,
       CASE WHEN w.op = 'bind_workspace' THEN w.workspace END       AS workspace,
       -- 指向**已删除项目**的绑定按未分组处理。
       --
       -- 判定放在视图里而不是每个调用方各写一遍：删项目刻意不级联改写会话
       -- （见表头），于是「悬挂的 project_id」是常态而非异常。少写一处的后果
       -- 是会话列表里出现一个属于不存在项目的分组标签，点进去空空如也 ——
       -- 而那一处一定是某个新加的查询，不会是这里
       CASE WHEN p.op = 'move_to_project'
             AND EXISTS (SELECT 1 FROM project_state ps
                          WHERE ps.project_id = p.project_id)
            THEN p.project_id END                                   AS project_id,
       -- GREATEST 在 Postgres 里忽略 NULL，因此只有四个维度都没事件时才为 NULL，
       -- 而那种会话根本不会出现在 known 里
       greatest(t.created_at, a.created_at, w.created_at, p.created_at) AS decided_at
  FROM known k
  LEFT JOIN last_title     t ON t.session_id = k.session_id
  LEFT JOIN last_archive   a ON a.session_id = k.session_id
  LEFT JOIN last_workspace w ON w.session_id = k.session_id
  LEFT JOIN last_project   p ON p.session_id = k.session_id;

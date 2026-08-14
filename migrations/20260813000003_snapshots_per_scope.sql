-- 快照按**沙箱作用域**索引，不再按 owner。
--
-- # 为什么现在改
--
-- 上一张表（20260813000002）的注释里已经写了这一天：
-- 「当前一个用户一个沙箱，所以 owner 同时也标识『哪个卷』。将来一个用户
--  多个沙箱时，这一列换成 sandbox_id，owner 另立一列。」
--
-- 容器与卷改成按项目分之后，一个用户同时有几个卷成了常态。不加这一列的话，
-- 「列快照」会把三个项目的快照混在一起，而「恢复」会把 A 项目的 tar 解进
-- B 项目的工作区 —— 两件事都不报错，只是文件不对。
--
-- # 为什么加列而不是改 owner 的语义
--
-- owner 仍然是「这个 schema 的主人」，排查时一眼看得出。把它悄悄改成存
-- 作用域键的话，同一列在新旧行里含义不同，而没有任何地方写着分界线在哪。
--
-- # 回填成 owner 是**正确**的，不是将就
--
-- 作用域键对未分组会话恒等于 owner（见 `SandboxScope::key`）——
-- 存量快照全部来自那个时期，所以 `scope = owner` 就是它们真实的作用域。
ALTER TABLE sandbox_snapshots ADD COLUMN scope TEXT;

UPDATE sandbox_snapshots SET scope = owner WHERE scope IS NULL;

-- 回填之后才收紧。反过来写的话，已有行会让 ALTER 当场失败
ALTER TABLE sandbox_snapshots ALTER COLUMN scope SET NOT NULL;

COMMENT ON COLUMN sandbox_snapshots.scope IS
    '这份快照属于哪个沙箱（SandboxScope::key：用户 id，或「用户 id--p-项目 id」）。
     未分组时恒等于 owner，所以存量行回填成 owner 是它们真实的作用域。';

-- 热查询从「这个 owner 最近的」变成「这个作用域最近的」。
-- 旧索引留着没用：按 owner 查快照的路已经一条都不剩
DROP INDEX sandbox_snapshots_owner_time;
CREATE INDEX sandbox_snapshots_scope_time
    ON sandbox_snapshots (scope, taken_at DESC);

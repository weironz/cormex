-- 沙箱工作区的快照索引。
--
-- # 为什么需要它
--
-- 容器沙箱的 `/workspace` 是一个持久卷，对办公类用户往往是**唯一一份**。
-- 调研里那条不变式（Codex / Claude web / Devin 的沙箱文件系统从不是
-- system of record，权威副本在 git 远端）在我们这里不成立 ——
-- 于是同一个 `rm -rf`，在它们那儿是重跑一次，在这儿是永久损失。
--
-- 卷内 git 已经能兜住覆盖写 / 误改（见 `cortex-local/src/checkpoint.rs`），
-- 但 `.git` 就在同一个卷上：**整卷删只有这一层救得回来**。
--
-- # 为什么快照由宿主驱动、索引落在这儿
--
-- 沙箱令牌只放行 5 条回调路由，里面没有任何一条碰得到这张表或对象存储。
-- 也就是说**被攻陷的容器删不掉自己的备份** —— 这条性质是拓扑给的，
-- 不是靠权限判断，所以它不会因为哪天多放行了一条路由而悄悄失效。
--
-- # append-only
--
-- 与全库一致：不 UPDATE、不 DELETE。淘汰旧快照走 purge 那条路
-- （显式触发、二次确认、留墓碑），不在这里就地删。
CREATE TABLE sandbox_snapshots (
    id           TEXT PRIMARY KEY,
    -- 沙箱的 owner。当前一个用户一个沙箱，所以它同时也标识「哪个卷」。
    -- 将来一个用户多个沙箱时，这一列换成 sandbox_id，owner 另立一列。
    --
    -- 注意这张表在**用户自己的 schema** 里，所以 owner 恒等于这个 schema
    -- 的主人。留着它是为了排查时一眼看得出，不是为了过滤。
    owner        TEXT        NOT NULL,
    -- 对象存储里的内容哈希（`BlobStore::put` 的返回）。
    --
    -- 内容寻址天然去重：两次快照之间没改过任何文件时，第二次的哈希与第一次
    -- 相同，对象存储不会真的再存一份 —— 而这张表仍然记两行，
    -- 因为「什么时候拍的」是这里唯一的信息。
    blob_hash    TEXT        NOT NULL,
    -- tar 的字节数，用来算配额与在界面上说人话。
    size_bytes   BIGINT      NOT NULL CHECK (size_bytes >= 0),
    taken_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- 「这个 owner 最近的快照」是唯一的热查询（列快照、拿最新一份来恢复）。
CREATE INDEX sandbox_snapshots_owner_time
    ON sandbox_snapshots (owner, taken_at DESC);

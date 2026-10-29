-- purge 覆盖 R 层（P6-2）。
--
-- 设计：docs/plan/session-archive-plan.md P6-2。
--
-- # 为什么重建 CHECK 而不是加一张表
--
-- `redactions` 是墓碑表，`target_kind` 是它的判别列。加两个取值就是改那条
-- CHECK —— 而 CHECK 是内联写的，Postgres 给的名字是
-- `redactions_target_kind_check`。按那个名字 DROP 再以同名 ADD，与
-- `20261125000001_derivation_transcript_source.sql` 是同一个写法。
--
-- # 为什么 target_id 是 TEXT 而不是 ulid
--
-- 原表就是 TEXT（`episode` / `blob` 两种目标共用一个列，而 blob 的 id 是
-- sha256）。新增的两种目标都是 ulid，但把列改成 ulid 会让已有的 blob 墓碑
-- 全部失效 —— 那些行的 target_id 是 64 位十六进制，不是 26 位 ULID。
--
-- # 顺序：先加 Rust 变体再跑这个
--
-- 与 P5-1 同一个理由：`target_kind` 读进 `RedactionTarget` 是 `FromStr`，
-- 数据库先接受一个代码还不认识的值，**整张表的读取都会失败**。

ALTER TABLE redactions DROP CONSTRAINT redactions_target_kind_check;
ALTER TABLE redactions ADD CONSTRAINT redactions_target_kind_check
    CHECK (target_kind IN ('episode', 'blob', 'capture_session', 'capture_file'));
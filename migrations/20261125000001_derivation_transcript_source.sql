-- 血缘边的源一侧增加 `transcript_event`。
--
-- 设计：docs/plan/session-archive-plan.md P5-1。
--
-- # 为什么需要它
--
-- 阶段 5 的观察日志与交接简报是从 P 层的事件算出来的。没有这条边，「这条
-- 摘要是从哪段对话来的」就没有答案 —— 而 GDPR Art.17 的擦除义务及于派生数据：
-- 一段被 purge 的会话，它派生出的摘要必须能被找到并一并处理。
--
-- # 顺序：先加 Rust 变体，再写数据
--
-- `derivations.source_kind` 读进 `SourceKind` 时是一个 `FromStr`。数据库先
-- 接受一个 Rust 还不认识的取值，**整张表的读取都会失败** —— 不是那一行，
-- 是整张表，因为 `query_as` 在遇到第一个解析不了的值时就返回错误。所以代码
-- 必须先上线，迁移后跑。这条顺序写在 docs/memory-content.md §二⑤。
--
-- # 约束名
--
-- 原始 CHECK 是内联写的（`migrations/20260807000006_fact_provenance.sql`），
-- 没有显式名字，所以 Postgres 给它的是 `derivations_source_kind_check`。
-- 这里按那个名字 DROP，再以**同名** ADD —— 名字保持一致，下一个改这条约束
-- 的人不用再猜一次。

ALTER TABLE derivations DROP CONSTRAINT derivations_source_kind_check;
ALTER TABLE derivations ADD CONSTRAINT derivations_source_kind_check
    CHECK (source_kind IN ('episode', 'fact', 'summary', 'transcript_event'));
-- 来源信任级（facts.source_channel / trust_tier）+ 派生物血缘（derivations）
--
-- 设计依据：docs/memory-content.md §4.3（来源信任级）、§5.3（删除的派生传播）、
-- §8.3（来源分级与 PROV-O）、§十三 第一批 1/2。
--
-- 两件事放在同一个 migration 里，因为它们回答的是同一个问题的两半：
-- **这条记忆是从哪来的**。前者答「什么通道」，后者答「哪几条源」。


-- ══════════════════════════════════════════════════════════
--  一、facts 的来源通道与信任级
--
--  已公开的记忆投毒攻击（MINJA / AgentPoison / SpAIware）全部发生在**写侧**：
--  攻击者不碰注入通道，只要诱导抽取器把恶意内容写成事实即可。而 append-only
--  会让毒化记录永久留存并持续进入召回 —— 没有来源标记就没有任何办法把它们
--  批量清出去。
--
--  三处用途（本次只做写入侧，读侧加权见 §九 #11 的前置条件）：
--    1. 高风险来源进隔离区（未做）
--    2. 召回排序按信任级加权（未做，前置是 confidence 先接进融合）
--    3. **出事后可按来源批量失效** —— 追加一批 fact_events，append-only 天然支持
-- ══════════════════════════════════════════════════════════

-- ── 🔴 存量行：为什么是 'unknown_legacy' 而不是某个真实档位 ──
--
-- ADD COLUMN ... DEFAULT 在 PG11+ 是元数据操作，不是 UPDATE，因此**不违反
-- append-only**。真正不可逆的麻烦在后面：
--
--   1. 存量行拿到的这个值**此后永远改不了** —— 纠正它需要 UPDATE，
--      而 UPDATE 只在 redact/purge 那一条例外里被允许；
--   2. 它**无法事后重建**：现存每一条 fact 的 source_episode_id 按构造都指向
--      role='user' 的那条 episode（抽取 ctx 用的是 user_episode_id，
--      见 cortexd::live），而送进抽取器的文本是「用户：… / 助手：…」拼起来的
--      一整块。库里**不存在**任何痕迹能区分「用户亲述」与「模型推断」。
--
-- 所以这里给的是一个诚实的「不知道」，而不是一个查询时会被当真的档位：
--   · source_channel = 'unknown_legacy'
--   · trust_tier     = NULL          ← 「不知道」不是信任标尺上的一个点
--
-- 代价明说：三处用途里的第 3 条（按来源批量失效）对本 migration 之前的
-- 全部事实**永久失效**，它们只能被当作一个整体处理。这是已经付出的代价，
-- 不是可以补救的缺口 —— 正是 §8.3 那条原则的反面教材：
-- 「必须由抽取器显式记录每条对应哪个输入片段，不能事后重建」。
--
-- 为什么 trust_tier 不给 0 或 5 这类「哨兵档位」：档位是有序的（数字越小越
-- 可信），把「不知道」塞进这把标尺，任何 `WHERE trust_tier <= n` 都会把它
-- 悄悄算进去或悄悄排除掉，取决于哨兵取在哪一头。NULL 在 SQL 里的传染性
-- 恰好是我们想要的：忘了处理「不知道」的查询会得到空结果，而不是错结果。

ALTER TABLE facts
    ADD COLUMN source_channel TEXT NOT NULL DEFAULT 'unknown_legacy';

-- 立刻撤掉默认值。存量行的取值由 PG 的 attmissingval 保留（DROP DEFAULT
-- 不会回填、不会重写表），而**今后**任何忘记写这一列的 INSERT 会直接撞
-- NOT NULL 报错 —— 而不是静默地又造出一批「不知道」。
ALTER TABLE facts
    ALTER COLUMN source_channel DROP DEFAULT;

-- 可空是刻意的，见上文：NULL 专属于存量行的「不知道」。
-- 新写入由下面的 CHECK 强制必须给出档位。
ALTER TABLE facts
    ADD COLUMN trust_tier SMALLINT;

-- 通道与档位的对应表。写成 CHECK 而不是靠应用层自觉：
-- 两处一旦漂移，症状是「按档位筛出来的集合与按通道筛出来的不一样」，
-- 而这种不一致只有在真出了安全事件、要批量失效时才会暴露。
--
--   1  用户亲述                最高
--   2  模型从对话推断
--   2  模型从已有事实派生      （crosslink 的跨域边；见下方「信任洗白」）
--   3  工具输出（本地执行）
--   4  外部内容（网页 / 他人文档 / MCP）   ← 默认不进抽取，需显式开启
--
-- ⚠️ 已知未解的「信任洗白」：derived 现在**不继承**输入事实的档位。
-- 一条建立在 tier-4 外部内容之上的派生边会被标成 tier 2。这正是 §8.3
-- 引用 C2PA 时说的那个问题在内部复现（「一条记忆经过一次摘要就丢失
-- provenance」）。补法是让 derived 取全部输入的最差档位 —— 前置是
-- 派生边先能查到它的输入，也就是下面这张 derivations 表。留给 §九 #11
-- 那一批一起做，届时它是可补的（不像存量行那样永久失效），因为
-- derivations 从今天起就把输入记下来了。
ALTER TABLE facts
    ADD CONSTRAINT facts_trust_tier_matches_channel CHECK (
        (source_channel = 'user_stated'    AND trust_tier = 1) OR
        (source_channel = 'conversation'   AND trust_tier = 2) OR
        (source_channel = 'derived'        AND trust_tier = 2) OR
        (source_channel = 'tool_output'    AND trust_tier = 3) OR
        (source_channel = 'external'       AND trust_tier = 4) OR
        (source_channel = 'unknown_legacy' AND trust_tier IS NULL)
    );

COMMENT ON COLUMN facts.source_channel IS
    '这条事实从哪个通道进来。unknown_legacy 专属于加列之前的存量行 ——
     它们的真实来源无法事后重建，标「不知道」而不是伪造一个档位。';
COMMENT ON COLUMN facts.trust_tier IS
    '来源信任级，1 最高。NULL = 不知道（仅存量行）。
     与 source_channel 的对应由 facts_trust_tier_matches_channel 锁死。';

-- 「出事后按来源批量失效」的查询路径。
-- 不建在 trust_tier 上：它与 source_channel 函数相关，多一个索引只是
-- 多一份要维护的东西，选择率还更差。
CREATE INDEX idx_facts_channel ON facts (source_channel);

-- ── ⚠️ 给 facts 加列必须顺手重建 active_facts ──
--
-- `active_facts` 建表时写的是 `SELECT f.*`，而 Postgres 在**建视图那一刻**
-- 就把 `*` 展开成了固定列表。之后给 facts 加列，视图不会跟着长 ——
-- 而日常召回**全部**走 active_facts（四路召回、矛盾消解的 rivals 查询、
-- 图遍历的反向一跳），于是一加列就整片报
-- `column "source_channel" does not exist`。
--
-- 这个坑不像「漏改 sync.rs 的 match」那样只在某张表第一次被写时才发作：
-- 它当场、全面地炸，所以危险性其实低得多。记在这里是因为**下一个给 facts
-- 加列的人会再踩一次**，而 migration 里没有任何东西提醒他视图的存在。
--
-- `CREATE OR REPLACE VIEW` 只允许在**末尾追加**列（不许删、不许改名改类型）。
-- 这里正好满足：`ALTER TABLE ADD COLUMN` 把新列放在表的最后，`f.*` 展开
-- 出来就是「原有列原序 + 两个新列」。
CREATE OR REPLACE VIEW active_facts AS
SELECT f.*
FROM facts f
LEFT JOIN fact_status s ON s.fact_id = f.id
WHERE s.op IS NULL OR s.op = 'revoke';


-- ══════════════════════════════════════════════════════════
--  二、派生物血缘 —— derivations
--
--  GDPR Art.17 的擦除义务及于**派生数据**，而 append-only 体系里源事实的
--  墓碑并不自动使摘要 / 图边 / 画像失效。一条被 redact 的事实若已经参与过
--  某次整固摘要或某条图边，那些派生物会继续泄露它。
--
--  purge 的正确形态是：按血缘找到全部派生物 → 墓碑 → 从剩余源重放投影。
--  没有血缘边，源事实失效后建立其上的摘要就成了无法追责的僵尸记忆。
--
--  学术侧独立支持同一结论：Generative Agents 的 because-of 引用、
--  RAPTOR 的树父子、HippoRAG2 的 contains 边 —— 抽象必须携带指向源记录的边。
--
--  **这要求 schema 从第一天就存派生边，事后补是不可能的**（§8.3：一次抽取
--  若吐出多条结果，必须由产生它的那一步显式记录每条对应哪些输入）。
-- ══════════════════════════════════════════════════════════

-- ── 为什么是一张独立的边表，而不是各派生表上加一个 id 数组列 ──
--
-- 数组列（summaries.source_episode_ids ulid[]）更简单，还能用
-- CHECK (cardinality(...) > 0) 在库层强制非空。放弃它的理由是**反查**：
--
--   redact/purge 的问法是「给定这条 episode / fact，谁派生自它」，
--   跨全部派生种类。数组列下这个问法必须由执行器**逐表**发一遍
--   `WHERE $1 = ANY(...)`，也就是每加一种派生物就得记得去改 redact 执行器。
--   那正是 migration 20260807000005 里已经栽过一次的「秘密散落在几个地方」。
--   一张边表让它变成一句 `WHERE source_kind = ? AND source_id = ?`，
--   将来的画像块不改执行器就自动被覆盖。
--
-- 而且源的种类天生是异质的：摘要派生自 episodes，crosslink 的边派生自
-- facts，将来的画像块派生自 facts + summaries。数组列要么开三列，
-- 要么把 kind 编进字符串。
--
-- ── 为什么没有外键 ──
--
-- Postgres 没有多态外键，而 derived_id / source_id 的目标表随 kind 变化。
-- 这不是本表首创：redactions.target_id 已经是同一个形状（target_kind +
-- 裸 id）。代价是悬空引用不会被数据库拦住 —— 由写入侧承担：写派生边的
-- 唯一入口与写派生行是同一个方法（见 cortex_store::WriteTxn::insert_summary），
-- 两者在同一个事务里。

CREATE TABLE derivations (
    id            ulid        PRIMARY KEY,

    -- 派生物一侧
    derived_kind  TEXT        NOT NULL CHECK (derived_kind IN ('fact', 'summary')),
    derived_id    ulid        NOT NULL,

    -- 源一侧。episode 是 L0 原文，fact / summary 是更低阶的派生物 ——
    -- 血缘是可以多层的（画像块 → 摘要 → episode），purge 时要走传递闭包
    source_kind   TEXT        NOT NULL CHECK (source_kind IN ('episode', 'fact', 'summary')),
    source_id     ulid        NOT NULL,

    device_id     TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    -- 自环会让血缘遍历直接死循环，而症状是 purge 挂住不是报错
    CHECK (NOT (derived_kind = source_kind AND derived_id = source_id)),

    -- 同一条边写两遍不是「多一行」而是让「这条摘要有几个源」这个问题
    -- 有两个不同的答案。约束在这里是为了让重复写变成一个会响的错误
    UNIQUE (derived_kind, derived_id, source_kind, source_id)
);

COMMENT ON TABLE derivations IS
    '派生物 → 源 的血缘边（append-only）。redact/purge 靠它找到必须一并
     失效的派生物；没有它，源被抹除后建立其上的摘要会继续泄露内容。';

-- redact/purge 的热路径：给定一个源，谁派生自它。
-- 反向（给定派生物找它的源）由 UNIQUE 约束的索引前缀
-- (derived_kind, derived_id) 覆盖，不另建。
CREATE INDEX idx_derivations_source ON derivations (source_kind, source_id);

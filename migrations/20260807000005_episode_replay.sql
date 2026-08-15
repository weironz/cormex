-- 回放保真：把「这一轮注入了哪些记忆 / 调用了哪些工具」落盘
--
-- ══════════════════════════════════════════════════════════
--  为什么必须落盘
-- ══════════════════════════════════════════════════════════
--
-- 在此之前这两样东西只活在 SSE 流里：刚生成时客户端能展开「为什么记得这个」，
-- 重开会话后抽屉是空的 —— **同一个回答刷新前后长得不一样**。
-- 「出处可查」是这个项目的核心卖点，只在内存里活着等于没有。
--
-- ══════════════════════════════════════════════════════════
--  存 fact id，还是存注入时的快照？—— 定案：存 id
-- ══════════════════════════════════════════════════════════
--
-- 两条路各有代价：
--
-- | | 存 fact id | 存注入原文快照 |
-- |---|---|---|
-- | 空间 | 一行几十字节 | 每轮把整段注入块再抄一份 |
-- | 回放完整度 | 依赖 facts 行还在 | 逐字复现当时看到的东西 |
-- | 与 redact 的关系 | **兼容** | **冲突** |
--
-- 最后一行是决定性的。redact 的承诺是「内容真的被销毁，只留墓碑」
-- （docs/memory.md §十一），而快照里的原文抹不掉 —— 除非 redact 执行器
-- 再多一条级联去清这张表，也就是把「秘密散落在几个地方」这个已经栽过一次的
-- 坑再挖一遍。存 id 的话，事实被 redact 后 statement 变成占位符，
-- 抽屉自然跟着变成占位符，一处清除处处生效。
--
-- 放弃的是**逐字保真**：一条事实被 superseded 之后，回放看到的是它现在的
-- 样子加一个「此后已失效」标记，而不是当时那句话。这个代价是可接受的 ——
-- 事实的 statement 本身是 append-only 的（失效靠追加 fact_events 表达，
-- 不改原行），所以「现在的样子」与「当时的样子」只在被 redact 时才不同，
-- 而那正是我们**希望**它不同的唯一场合。
--
-- ══════════════════════════════════════════════════════════
--  为什么锚在 user 那条 episode 上
-- ══════════════════════════════════════════════════════════
--
-- 一轮对话有两条 episode（user 与 assistant），抽屉挂哪条都说得通。选 user：
--
-- 1. **它一定存在**。assistant 那条在模型出错时刻意不落库（半截回答进了记忆，
--    下一轮会被当成事实抽取），此时若把抽屉挂在它上面，本轮的归因就整个丢了 ——
--    而出错那一轮恰恰是最需要查「它凭什么这么答」的一轮。
-- 2. **注入物理上就贴在 user 消息一侧**（见 cortexd::live 的第 3 步），
--    挂在这里是在陈述事实，不是做 UI 决策。
--
-- 客户端把抽屉画在哪个气泡上是它自己的事，服务端不替它决定。

-- ══════════════════════════════════════════════════════════
--  本轮注入的记忆
-- ══════════════════════════════════════════════════════════

CREATE TABLE episode_memories (
    id          ulid        PRIMARY KEY,

    -- 本轮的锚点 —— user 那条 episode，见上文
    episode_id  ulid        NOT NULL REFERENCES episodes(id),
    -- 外键指向 facts 而非 active_facts：失效的事实**仍然要能回放**，
    -- 「当时注入过一条现在已经不成立的记忆」正是审计最想看到的东西
    fact_id     ulid        NOT NULL REFERENCES facts(id),

    -- 注入顺序（0 起）。融合分排序的结果，回放时按它还原当时的先后 ——
    -- 靠 score 重排是不行的：score 可空（老行、或将来换了融合算法）
    ordinal     INTEGER     NOT NULL CHECK (ordinal >= 0),

    -- 命中了哪几路召回。bad case 时这是唯一能定位「为什么召回了它」的东西。
    -- 用数组而不是逗号分隔串：通道名是枚举量，拼成串之后想按通道统计
    -- 就得在 SQL 里做字符串切分
    channels    TEXT[],
    -- RRF 融合分。可空 —— 换融合算法时老行的分数没有可比性，
    -- 与其伪造一个不如留空
    score       DOUBLE PRECISION,

    device_id   TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    -- 一轮里同一条事实只会被注入一次（检索结果本身已去重）。
    -- 约束在这里不是为了防重复写，是为了让「重复写」变成一个会响的错误：
    -- 静默的重复会让抽屉里出现两条一样的记忆，而用户只会觉得界面有 bug
    UNIQUE (episode_id, fact_id)
);

COMMENT ON TABLE episode_memories IS
    '一轮对话注入了哪些记忆（append-only）。只存 fact id 不存快照 ——
     快照里的原文抹不掉，会让 redact 的销毁承诺变成假的。';

-- 回放热路径：按 episode 取，按注入顺序还原
CREATE INDEX idx_episode_memories ON episode_memories (episode_id, ordinal);
-- 反向问法：「这条记忆被用在过哪些回答里」
CREATE INDEX idx_episode_memories_fact ON episode_memories (fact_id);

-- ══════════════════════════════════════════════════════════
--  本轮调用的工具
-- ══════════════════════════════════════════════════════════

CREATE TABLE episode_tool_calls (
    id          ulid        PRIMARY KEY,
    episode_id  ulid        NOT NULL REFERENCES episodes(id),

    -- 调用顺序（0 起）。工具调用之间有因果（先 list_dir 再 read_file），
    -- 乱序回放会让人读不懂 agent 当时在干什么
    ordinal     INTEGER     NOT NULL CHECK (ordinal >= 0),

    name        TEXT        NOT NULL,

    -- 这次调用碰的文件路径，没碰文件的工具为 NULL。
    --
    -- 单独一列而不是让客户端从 summary 里抠：summary 是给人看的自然语言，
    -- 措辞随时会改，而客户端一旦用正则从里面提路径，改一次措辞就是
    -- **静默显示错文件** —— 不报错、不崩溃，只是指向了另一个文件
    path        TEXT,

    -- 给人看的一行摘要（「返回 12 行 / 340 字符」「失败：路径 … 已拒绝」）
    summary     TEXT        NOT NULL,
    ok          BOOLEAN     NOT NULL,

    device_id   TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),

    UNIQUE (episode_id, ordinal),

    -- 上限是防御性的：这三列的内容都来自模型输出，进 sync_log 会下发到所有设备
    CHECK (length(name) BETWEEN 1 AND 128),
    CHECK (path IS NULL OR length(path) <= 4096),
    CHECK (length(summary) <= 2048)
);

COMMENT ON TABLE episode_tool_calls IS
    '一轮对话调用了哪些工具（append-only）。path 单列存放，
     客户端不必再从 summary 的措辞里正则抠文件名。';

CREATE INDEX idx_episode_tool_calls ON episode_tool_calls (episode_id, ordinal);

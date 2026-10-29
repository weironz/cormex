-- 对话轮向量（可选，P5-6）。
--
-- 设计：docs/design/session-archive-architecture.md §8.3（第 5 阶段加
-- `transcript_embeddings`，只给对话轮）。
--
-- # 为什么单独一张表而不是 `transcript_events` 加一列
--
-- 换 embedding 模型时，旧向量必须原样保留 —— 这与 `fact_embeddings` /
-- `entity_embeddings` 是同一个理由（`migrations/20260808000001`）：向量是模型的
-- 函数，换模型就是换一套空间，两套要能并存而不是互相覆盖。
--
-- 而且这张表**不进 sync_log**（归档层，D3）：向量是服务端派生的，客户端拿到它
-- 没有任何用处，一次回填却会推出几十万行同步记录。
--
-- # 只给对话轮
--
-- `user_message` / `agent_message` / `plan`。工具输出不进这里 —— 它走
-- `archive_chunks` 的 BM25，语义检索对一段构建日志没有意义，而给每段工具输出
-- 算一次向量是纯粹的浪费。
--
-- # 默认关闭
--
-- 这张表存在不代表它被写入。`CORMEX_SESSION_EMBEDDINGS` 打开后 `index_tick` 才
-- 会生成向量，`search_conversation` 才会做 RRF 融合。验收是「在 P7-2 的评测集上
-- 有可对照的数字证明值得开」，没有数字就保持关闭。

CREATE TABLE transcript_embeddings (
    event_id        ulid        NOT NULL REFERENCES transcript_events(id),
    embedding       VECTOR(1024) NOT NULL,
    embedding_model TEXT        NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- 同一个事件在同一模型下只有一个向量；换模型是另一行
    PRIMARY KEY (event_id, embedding_model)
);

COMMENT ON TABLE transcript_embeddings IS
    '对话轮向量（append-only，不进 sync_log）。只给 user / agent / plan 事件；
     同一事件在不同模型下各有一行。默认不写入，见 CORMEX_SESSION_EMBEDDINGS。';

-- 向量检索的形状固定是「这个模型下、按余弦距离取最近的几个」
CREATE INDEX idx_transcript_embeddings_model ON transcript_embeddings (embedding_model);
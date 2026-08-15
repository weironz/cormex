-- 自带 key 也要能带自己的端点。
--
-- ## 为什么补这一列
--
-- 上一条 migration 只存了 (provider, key)。但一个人「自己的 key」很少是
-- 官方那把 —— 更常见的是公司网关、one-api / LiteLLM 这类中转、某个更便宜
-- 的兼容服务，或者自建的 vLLM。那些端点的 URL 与内置定义里编译进去的
-- 那个不一样，而没有这一列的话，填了 key 也打不到自己的地方。
--
-- 这与服务端那个 `CORTEX_LLM_BASE_URL` 是同一件事的两半：部署级的覆盖
-- 给运维，用户级的覆盖给用户。
--
-- NULL = 用内置定义里的那个（官方端点）。

ALTER TABLE llm_keys ADD COLUMN base_url text;

-- 墓碑行不带端点，与不带密文同理：一行「不再使用自带 key」的记录
-- 不该携带任何配置，否则读取时要多判一次「这行到底算不算数」
ALTER TABLE llm_keys DROP CONSTRAINT llm_keys_shape;
ALTER TABLE llm_keys ADD CONSTRAINT llm_keys_shape CHECK (
    (deleted AND ciphertext IS NULL AND key_tail IS NULL AND base_url IS NULL)
    OR (NOT deleted AND ciphertext IS NOT NULL AND key_tail IS NOT NULL)
);

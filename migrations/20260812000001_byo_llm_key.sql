-- 用户自带的 LLM API key。
--
-- ## 为什么在用户自己的 schema 里，而不是 cortex_auth
--
-- 这是**这个用户的**一份秘密，与他的记忆同生共死：drop 掉他的 schema
-- 就该把 key 一起带走。放在 cortex_auth 里则要额外记得清理，
-- 而「删账号时忘了删 key」不会报错。
--
-- ## 为什么是 append-only（与全库一致）
--
-- 换 key 追加一行，删 key 追加一行墓碑。理由与 fact_events 相同：
-- 就地 UPDATE 会让「这把 key 是什么时候换的」永久不可知，
-- 而那正是排查「从某天起调用开始失败」时唯一有用的信息。
--
-- 读取时取最新一行；`deleted` 为真表示这一行是墓碑。
--
-- ## 密文格式
--
-- `ciphertext` = nonce(12) || AES-256-GCM(明文)。密钥来自部署的
-- `CORTEX_SECRET_KEY`，**不在库里**。所以一份被拖走的数据库备份
-- 里没有可用的 key —— 这是把它加密存储的全部意义。
--
-- `key_tail` 是明文 key 的**后 4 位**，只用于在界面上认出「填的是哪一把」。
-- 后 4 位不足以恢复任何东西，而没有它，用户在设置页看到的是一片空白，
-- 分不清自己到底填没填过。

CREATE TABLE llm_keys (
    id          ulid PRIMARY KEY,
    -- 供应商 id，与 cortex-llm 的 provider 名一致（deepseek / anthropic / …）
    provider    text        NOT NULL,
    -- 墓碑行为 NULL
    ciphertext  bytea,
    key_tail    text,
    -- 真 = 这一行是「不再使用自带 key」的墓碑
    deleted     boolean     NOT NULL DEFAULT false,
    created_at  timestamptz NOT NULL DEFAULT now(),

    -- 墓碑不带密文，正常行必须带。写反了在插入时就炸，
    -- 而不是等到某次调用拿着 NULL 去解密
    CONSTRAINT llm_keys_shape CHECK (
        (deleted AND ciphertext IS NULL AND key_tail IS NULL)
        OR (NOT deleted AND ciphertext IS NOT NULL AND key_tail IS NOT NULL)
    )
);

-- 只查「最新一行」，所以按时间倒序取第一条
CREATE INDEX idx_llm_keys_latest ON llm_keys (created_at DESC);

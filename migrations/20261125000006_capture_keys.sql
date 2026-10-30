-- 每个采集文件一把数据密钥（P6-1）。
--
-- 设计：docs/design/session-archive-plan.md P6-1。
--
-- # 为什么要按文件而不是按租户
--
-- 按租户的话，「销毁这个会话」就得重写该租户下所有分块 —— 而分块是内容寻址的，
-- 重写等于把所有对象都换一遍。按文件的话，销毁 = 删掉这一行（密钥没了，密文就是
-- 随机字节），其余对象一个不动。这就是 crypto-shredding 的全部要点。
--
-- # 密钥本身被 `CORMEX_SECRET_KEY` 包裹
--
-- 数据库里存的是 `wrapped_key`，不是明文密钥。所以一份被偷走的数据库备份里没有
-- 任何可用的密钥 —— 前提是 `CORMEX_SECRET_KEY` 不在同一份备份里（它只在 `.env`）。
--
-- ⚠️ **限制，必须写进文档**：旧的数据库备份里仍然有被包裹的密钥，而如果
-- `CORMEX_SECRET_KEY` 没有轮换，那些备份里的密钥仍然能解开当时的密文。所以
-- **在旧备份过期之前，crypto-shredding 是不完整的**。这不是实现缺陷，是备份
-- 保留期的性质。
--
-- # 没有配置 `CORMEX_SECRET_KEY` 时
--
-- 保持明文，并在启动日志与 `doctor` 里如实说明。**不生成临时密钥** —— 那会让
-- 重启之后所有旧分块变成解不开的字节，症状是「昨天还好好的，今天全读不出来」。

CREATE TABLE capture_keys (
    id          ulid        PRIMARY KEY,
    -- 一把密钥服务一个文件。同一个文件的后续分块继续用同一把
    file_id     ulid        NOT NULL REFERENCES capture_files(id),
    -- AES-256-GCM 密文：nonce(12) || ciphertext || tag(16)，用 `CORMEX_SECRET_KEY` 包裹
    wrapped_key BYTEA       NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE capture_keys IS
    '每个采集文件一把数据密钥（append-only，不进 sync_log）。密钥用 CORMEX_SECRET_KEY
     包裹后存这里；删掉这一行即销毁该文件的全部密文。';

-- 查一个文件当前的密钥：取最新一行
CREATE INDEX idx_capture_keys_file ON capture_keys (file_id, created_at DESC);
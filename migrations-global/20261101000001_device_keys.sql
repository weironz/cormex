-- 设备密钥：`cormex login` 换来的、按设备吊销的长期凭据。
--
-- 设计：docs/design/session-archive-architecture.md §4.5 / §6.1（D9）。
--
-- # 为什么不是复用 refresh token
--
-- refresh token 是**轮换**的，而且带重放检测：同一个 token 用两次就判定泄露，
-- 整条链吊销。而采集端的调用形态恰恰是「多个 hook 进程并发、各自读到同一个
-- 本地凭据」——那在轮换 + 重放检测看来就是泄露，于是**整条链被吊销**，用户
-- 的采集静默停摆。设备密钥不轮换、不检测重放，吊销的粒度是「这台设备」。
--
-- # 为什么只存 SHA-256
--
-- 与 `cortexd::auth` 同一段论证：慢 KDF 防的是**低熵秘密**被离线爆破，而这个
-- 密钥是 256 位内核随机数 —— 快哈希足够。明文只在签发那一刻出现一次。
--
-- # 为什么这张表可以有 UPDATE
--
-- append-only 约束的是**记忆**（事实、会话、出处），不是认证状态。
-- `last_seen_at` / `revoked_at` 是「现在是什么状态」，不是「曾经发生过什么」；
-- 把它们写成事件表只会让每次鉴权都多一次扫描。`cortex_auth` 里的其它表
-- （`auth_tokens.rotated_at`）本来就是这个形状。
--
-- `last_seen_at` 每个请求都写一次会让鉴权路径变成写路径：一次 sync 是几十个
-- 请求，全部串行在同一个行锁上。所以应用层最多每 10 分钟写一次。

CREATE TABLE cortex_auth.device_keys (
    id            ulid        PRIMARY KEY,
    user_id       ulid        NOT NULL REFERENCES cortex_auth.users(id),
    -- 明文密钥的 SHA-256。UNIQUE：两个设备拿到同一把密钥时，后一个签发必须
    -- 失败而不是让「这把密钥是谁的」有两个答案
    key_sha256    sha256      NOT NULL UNIQUE,
    -- 人给设备起的名字（"laptop"），用于列表与吊销
    label         TEXT        NOT NULL CHECK (length(label) BETWEEN 1 AND 100),
    -- 作用域。CHECK 把取值限死在这里：多一个拼错的作用域会让「这个设备能干什么」
    -- 多出一个无人检查的答案，而那正是越权面的形状
    scopes        TEXT[]      NOT NULL CHECK (scopes <@ ARRAY[
                      'capture:write', 'session:read', 'memory:read', 'memory:write', 'raw:read']::TEXT[]),
    -- linux-x86_64 / macos-aarch64 / ……，纯诊断用
    platform      TEXT,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- 最近一次使用。最多 10 分钟更新一次（见上面的注释）
    last_seen_at  TIMESTAMPTZ,
    -- 吊销是**记时间**而不是删行：删掉之后「这台设备曾经存在过」就没有答案了，
    -- 而排查「为什么这台机器不再上传」正需要它
    revoked_at    TIMESTAMPTZ
);

COMMENT ON TABLE cortex_auth.device_keys IS
    'cormex login 签发的设备密钥（只存 SHA-256）。不轮换、不检测重放，
     按设备吊销 —— 采集端是多进程并发的，轮换型凭据会把并发读判成泄露。';

CREATE INDEX idx_device_keys_user ON cortex_auth.device_keys (user_id);
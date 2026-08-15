-- 多用户的地基：账号、会话令牌、邀请、用量
--
-- 设计依据：docs/roadmap.md「R9 · 认证与多租户」。
--
-- ══════════════════════════════════════════════════════════
--  ⚠️ 这个文件在 `migrations-global/`，不在 `migrations/`
--
--  多租户把 migration 集劈成了两份，而这个区分是**必须**的：
--
--  - `migrations/`        每个租户各跑一遍（17 张表 + 视图，各人一份记忆）
--  - `migrations-global/` 整个部署只跑一遍（账号表，跨租户唯一）
--
--  混在一起会怎样：新租户建号时把整套 migration 灌进自己的 schema，
--  跑到这一条就撞上 `relation "users" already exists` —— 因为它是
--  `cortex_auth.users`，全限定的，跟租户 schema 无关。**实测撞到过**，
--  而失败点在建号中途：schema 建了一半，用户记录还没写。
--
--  全局这一套的 `_sqlx_migrations` 落在 `cortex_auth` 里（连接时
--  `search_path=cortex_auth,public`），与租户那套的版本号互不干扰。
--
-- ══════════════════════════════════════════════════════════
--  为什么这些表在自己的 schema 里
--
--  隔离方式是**每个用户一个 Postgres schema**（77 条查询一条都不用改，
--  隔离是结构性的 —— 连接根本看不见别的 schema）。而账号表是**跨用户**的：
--  它必须在所有用户 schema 之外，否则每个用户都会有一份自己的用户表，
--  而「谁能登录」这件事就没有唯一的答案了。
--
--  所以：`cortex_auth` 放账号，`public` 与 `u_<ulid>` 放各人的记忆。
--
-- ══════════════════════════════════════════════════════════
--  1 号用户的 schema 就叫 public
--
--  生产库里已经有存量数据。把 17 张表 + 视图 `ALTER TABLE … SET SCHEMA`
--  搬进 `u_<ulid>` 是一次**没有回头路**的 DDL，而它换来的只是好看。
--  所以 schema 名存在 users 表里，1 号用户填 `public`，新用户才拿 `u_<ulid>`。
--
--  推论：`users.schema_name` 不是派生值，不能由 id 算出来。别加那种「优化」。
-- ══════════════════════════════════════════════════════════

-- schema 本身由 `Store::migrate_global` 在跑 migrator **之前**建好 ——
-- 因为 `_sqlx_migrations` 要落在里面，而那发生在第一条 migration 之前。
-- ══════════════════════════════════════════════════════════
--  账号
-- ══════════════════════════════════════════════════════════

CREATE TABLE cortex_auth.users (
    id            ulid        PRIMARY KEY,
    -- 登录名。大小写不敏感地唯一 —— 见下面那个函数索引。
    --
    -- 不叫 email：这个部署没有邮件通道（没有找回密码、没有验证邮件），
    -- 叫 email 会让人以为那些功能存在。等真接上邮件再加一列。
    username      TEXT        NOT NULL,
    -- argon2id 的 PHC 串（`$argon2id$v=19$m=…`）。
    --
    -- **不能沿用 cortexd::auth 里那个 SHA-256 摘要。** 那段论证
    --（「慢 KDF 防的是低熵秘密被离线爆破」）是对的，但它有个前提：
    -- token 是高熵随机的。人选的密码不满足，所以这里必须是慢 KDF。
    password_hash TEXT        NOT NULL,
    -- 这个人的记忆住在哪个 schema。1 号用户是 `public`，见文件头。
    schema_name   TEXT        NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- 停用而不是删除：删掉一行用户，他的 schema 就成了没人认领的孤儿，
    -- 而里面是几万条记忆。停用之后登录被拒，数据原样还在。
    disabled_at   TIMESTAMPTZ,

    CONSTRAINT users_username_shape CHECK (
        username ~ '^[a-zA-Z0-9][a-zA-Z0-9._-]{1,62}$'
    ),
    -- schema 名会被拼进 `search_path`，所以它是**注入面**。
    -- 限死成「public 或 u_ + 26 位小写 ULID」，比引号转义可靠得多：
    -- 转义要求每一处拼接都记得做，而这条 CHECK 只要写一次。
    --
    -- **必须小写。** `search_path` 里不带引号的标识符会被折成小写，而
    -- `CREATE SCHEMA "u_ABC"` 保留大小写 —— 两者不一致时 `search_path`
    -- 指向一个不存在的 schema，而 Postgres 对此是**静默跳过**的：
    -- 新租户一张表都没建、写入全落进 public，且 migrate() 返回 Ok。
    -- 实测撞到过（见 cortex_store::tenant::SchemaName::new）
    CONSTRAINT users_schema_shape CHECK (
        schema_name = 'public'
        OR schema_name ~ '^u_[0-9a-hjkmnp-tv-z]{26}$'
    ),
    CONSTRAINT users_schema_unique UNIQUE (schema_name)
);

-- 大小写不敏感的唯一。
--
-- 用函数索引而不是 citext：citext 是扩展，而这个项目已经因为
-- 「托管 PG 装不了 zhparser」把中文分词挪到了 Rust 侧（见 architecture.md）。
-- 同一条理由 —— 少一个扩展依赖，多一个能跑的地方。
CREATE UNIQUE INDEX idx_users_username ON cortex_auth.users (lower(username));

-- ══════════════════════════════════════════════════════════
--  会话令牌
--
--  只存 refresh token 的摘要。明文只在签发那一刻存在于响应体里 ——
--  与 cortexd::auth 对预共享 token 的处理同构，理由也相同：
--  库被读走时不能等于账号被接管。
-- ══════════════════════════════════════════════════════════

CREATE TABLE cortex_auth.auth_tokens (
    id            ulid        PRIMARY KEY,
    user_id       ulid        NOT NULL REFERENCES cortex_auth.users(id),
    -- SHA-256(明文)。**这里用快摘要是对的** —— 与密码那一列的慢 KDF
    -- 不矛盾：判据是同一条（低熵才需要慢 KDF），而 refresh token
    -- 是我们自己生成的高熵随机串。
    token_sha256  sha256      NOT NULL UNIQUE,
    -- 一条轮转链。refresh 每用一次就换一个新的，新的继承同一个 family。
    --
    -- **重放检测靠它**：拿一个已经轮转过的旧 token 来换，说明它泄露了，
    -- 此时整条链一起作废 —— 只废那一个是不够的，攻击者手上还有后续的。
    family_id     ulid        NOT NULL,
    issued_at     TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at    TIMESTAMPTZ NOT NULL,
    -- 已经被用掉并换出新 token 的时刻。非空 = 这一个不该再被接受。
    rotated_at    TIMESTAMPTZ,
    -- 显式作废（登出，或者重放检测把整条链废掉）。
    revoked_at    TIMESTAMPTZ,
    -- 签发给谁用的。纯诊断字段：同一个账号在三台机器上登录时，
    -- 「哪一台的凭据泄露了」只有这里能回答
    device_label  TEXT
);

CREATE INDEX idx_auth_tokens_user   ON cortex_auth.auth_tokens (user_id);
-- 重放检测要按 family 批量作废，所以这条是热路径而不是运维用的
CREATE INDEX idx_auth_tokens_family ON cortex_auth.auth_tokens (family_id);

-- ══════════════════════════════════════════════════════════
--  邀请码
--
--  开放注册默认是**关**的：任何人都能注册意味着任何人都能拿服务端那把
--  DeepSeek key 烧钱，而生产是一台 2 核 3.5 GB、已经跑着 19 个容器的机器。
--  邀请码是「想让谁进来就让谁进来」的最小实现。
-- ══════════════════════════════════════════════════════════

CREATE TABLE cortex_auth.invites (
    id           ulid        PRIMARY KEY,
    -- 同样只存摘要。邀请码泄露的后果比 token 小，但没有理由存明文
    code_sha256  sha256      NOT NULL UNIQUE,
    created_by   ulid        REFERENCES cortex_auth.users(id),
    created_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at   TIMESTAMPTZ,
    used_at      TIMESTAMPTZ,
    used_by      ulid        REFERENCES cortex_auth.users(id),

    -- 用过就必须知道是谁用的。两列一起为空或一起非空 ——
    -- 分开允许会让「这个码被谁用了」出现一个查不出来的中间态
    CONSTRAINT invites_used_together CHECK (
        (used_at IS NULL) = (used_by IS NULL)
    )
);

-- ══════════════════════════════════════════════════════════
--  用量
--
--  逐次调用追加一行，与全库的 append-only 纪律一致：配额是个会被争议的
--  数字（「我明明没用这么多」），而只有逐条记录才答得上来。
--
--  刻意**不做**「当前用量」那一列 —— 维护它要 UPDATE，而 UPDATE 在这个
--  项目里只在 redact/purge 里被允许。用量按时间窗聚合算出来。
-- ══════════════════════════════════════════════════════════

CREATE TABLE cortex_auth.usage (
    id            ulid        PRIMARY KEY,
    user_id       ulid        NOT NULL REFERENCES cortex_auth.users(id),
    occurred_at   TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- 'chat' / 'extract' / 'embed' / 'transcribe' —— 哪条管线花的钱。
    -- 不做 CHECK 枚举：加一条新管线不该需要一次 migration
    kind          TEXT        NOT NULL,
    input_tokens  BIGINT      NOT NULL DEFAULT 0,
    output_tokens BIGINT      NOT NULL DEFAULT 0,
    -- 走的是服务端那把 key 还是用户自带的。
    --
    -- **必须分开记**：自带 key 的用量不占配额，但仍然要能看见 ——
    -- 「我这个月花了多少」这个问题与「我还剩多少额度」是两个问题
    own_key       BOOLEAN     NOT NULL DEFAULT FALSE,
    model         TEXT
);

-- 配额查询的形状固定是「这个用户、这段时间、占配额的那些」
CREATE INDEX idx_usage_user_time ON cortex_auth.usage (user_id, occurred_at DESC);

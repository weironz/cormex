-- 会话档案的 R 层（原始层）：采集进来的会话文件、它们的代与分块，以及围绕它们的观察。
--
-- 设计权威：docs/design/session-archive-architecture.md §4.1（D2 / D3 / D6 / D16 / D23）。
--
-- # 这批表为什么和别的表不一样
--
-- 1. **不进 sync_log，不下发到任何设备**（D3）。R 层只在服务端：它是「所有机器上
--    所有 agent 说过什么」的中央副本，下发到设备既没有用途，又把每一台设备变成
--    了别人的会话副本。所以它们走 `Store::archive_txn`（不拿同步的 advisory lock、
--    不写 sync_log），不走 `write_txn`。`table::ARCHIVE` 与 `table::ALL` 不相交，
--    有测试钉着。
--
-- 2. **全链路 append-only，而且比别处更严格**：这里连「作废」都没有。改写 / 截断
--    不修改旧行，而是开**新一代**（`capture_generations`），旧代原样保留 —— 「三个月
--    前这个文件长什么样」因此是一个可以直接回答的问题，而不是靠审计日志猜。
--
-- 3. **字节在对象存储，这里只有位置与哈希**（D16）。分块的 `content_sha256` 是
--    **逻辑（未压缩）字节**的哈希，不是存储对象的哈希：同一个文件在明文与 `.zst`
--    两种表示下必须算出同一个值，否则「明文优先、压缩去重」那条规则会重复上传。
--
-- 4. **连续性由服务端保证**，不靠客户端自觉：新分块的 `byte_offset` 必须等于这一代
--    当前的末尾（`ArchiveTxn::append_chunk` 在事务内用
--    `pg_advisory_xact_lock(4273, hashtext(generation_id))` 串行化）。客户端算错了
--    会拿到 409 和一个明确的 `expected`，而不是在库里留下一个洞 —— 洞是**读的时候
--    才发现**的，而那时已经不知道缺的是哪一段了。
--
-- 5. **`device_id` 跨 schema，不建外键**：它指向 `cortex_auth.device_keys.id`，而
--    租户 schema 里的表不能引用另一个 schema 的行（那会让删账号被一张业务表的
--    FK 挡住）。这条约束由应用层守。

-- ══════════════════════════════════════════════════════════
--  一、会话
-- ══════════════════════════════════════════════════════════

CREATE TABLE capture_sessions (
    id                 ulid        PRIMARY KEY,
    -- agent 的标识。形状与 catalog 里的 id 一致（P1-5 的测试钉着同一个正则）：
    -- 它会被拼进日志、指标标签和 CLI 参数，所以限死成小写标识符
    agent              TEXT        NOT NULL CHECK (agent ~ '^[a-z][a-z0-9_]{1,31}$'),
    -- 宿主自己的会话 id（Claude Code 的文件名主干、Codex rollout 里的 UUID……）。
    -- **跨设备合并靠它**：同一台机器上同一个会话被采两次、两台机器采到同一个
    -- 会话，都只有一行
    native_session_id  TEXT        NOT NULL CHECK (length(native_session_id) BETWEEN 1 AND 256),
    -- 子 agent 会话指向父会话。自引用，所以父会话必须已经存在 —— 采集端按
    -- 「先父后子」的顺序注册
    parent_session_id  ulid        REFERENCES capture_sessions(id),
    first_seen_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (agent, native_session_id)
);

COMMENT ON TABLE capture_sessions IS
    '一个被采集的会话 = 某个宿主里的一段对话（append-only，不进 sync_log）。
     跨设备合并：同一个 agent 的同一个原生会话 id 只有一行。';

-- ══════════════════════════════════════════════════════════
--  二、逻辑文件
-- ══════════════════════════════════════════════════════════

-- 一个会话在**一台设备**上的一个逻辑文件：主 transcript、子 agent 的 transcript、
-- 溢出的工具输出（sidecar）、附件、memory 伪会话、SQLite 来源的一行。
--
-- 「逻辑」是相对「物理」说的：Codex 会把冷文件压成 `.jsonl.zst`，而采集端采的是
-- 解压后的字节 —— 所以 `logical_path` 一律是明文名，`framing` 说这份文件是追加型
-- 还是快照型。
CREATE TABLE capture_files (
    id             ulid        PRIMARY KEY,
    session_id     ulid        NOT NULL REFERENCES capture_sessions(id),
    -- cortex_auth.device_keys.id。**跨 schema，不建外键**，见文件头第 5 条
    device_id      ulid        NOT NULL,
    agent          TEXT        NOT NULL,
    -- 相对 agent home 的路径，正斜杠分隔。上限 4096 是路径长度的实际上限；
    -- 它也会出现在 URL 与日志里，所以不是「随便多长都行」
    logical_path   TEXT        NOT NULL CHECK (length(logical_path) BETWEEN 1 AND 4096),
    role           TEXT        NOT NULL CHECK (role IN
                       ('transcript', 'subagent', 'sidecar', 'attachment', 'memory', 'row')),
    -- append：只追加的日志（Claude Code / Codex 的 jsonl）
    -- snapshot：整体重写的文件（SQLite 行来源、导出文件）
    framing        TEXT        NOT NULL CHECK (framing IN ('append', 'snapshot')),
    -- 会话的工作目录。**只记路径，不读目录里的内容**（D7）
    cwd            TEXT,
    -- 由 cwd 推出的项目标识（git remote 规范化后的 host/owner/repo）。
    -- NULL = 未知或没开 git 元数据采集
    project_key    TEXT        CHECK (project_key IS NULL OR length(project_key) <= 512),
    first_seen_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- 同一台设备上，同一个 agent 的同一个逻辑路径只有一个文件。**不含 session_id**：
    -- 一个文件换了会话归属（Claude Code 的 resume 会写进同一个文件）时，那是同一个
    -- 物理文件，不该变成两行
    UNIQUE (device_id, agent, logical_path)
);

COMMENT ON TABLE capture_files IS
    '会话在某台设备上的一个逻辑文件（append-only，不进 sync_log）。
     逻辑 = 解压后的字节；Codex 的 .jsonl.zst 与 .jsonl 是同一个逻辑文件。';

CREATE INDEX idx_capture_files_session ON capture_files (session_id);

-- ══════════════════════════════════════════════════════════
--  三、代
-- ══════════════════════════════════════════════════════════

-- 逻辑文件的「代」。**追加不开新代**；改写 / 截断 / 换 inode 之后内容对不上，
-- 开新代，旧代原样保留。
--
-- 为什么不是「就地更新到一个新版本」：那样「三个月前这个文件长什么样」就答不了，
-- 而这正是本产品的卖点。代是一次性的分支点，不是版本号。
CREATE TABLE capture_generations (
    id          ulid        PRIMARY KEY,
    file_id     ulid        NOT NULL REFERENCES capture_files(id),
    ordinal     INTEGER     NOT NULL CHECK (ordinal >= 0),
    -- initial：第一次采到这个文件
    -- rewrite：同长度区间内容变了（客户端用尾随哈希发现）
    -- truncate：文件变短了
    -- snapshot：快照型来源每次重新采
    reason      TEXT        NOT NULL CHECK (reason IN ('initial', 'rewrite', 'truncate', 'snapshot')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (file_id, ordinal)
);

COMMENT ON TABLE capture_generations IS
    '逻辑文件的一代（append-only，不进 sync_log）。追加不开新代；改写 / 截断开新代，
     旧代原样保留 —— 「当时这个文件长什么样」因此可以直接回答。';

-- ══════════════════════════════════════════════════════════
--  四、分块
-- ══════════════════════════════════════════════════════════

-- 一代里的一个分块。字节在 RustFS（zstd 压缩），这里只有位置与哈希。
CREATE TABLE capture_chunks (
    id              ulid        PRIMARY KEY,
    generation_id   ulid        NOT NULL REFERENCES capture_generations(id),
    byte_offset     BIGINT      NOT NULL CHECK (byte_offset >= 0),
    -- 上限 32 MiB：客户端按 4 MiB 切块，这个上限是给「服务端解压后」留的余量，
    -- 同时挡住一个声称 8 GiB 的分块把内存吃光
    byte_length     BIGINT      NOT NULL CHECK (byte_length > 0 AND byte_length <= 33554432),
    -- **逻辑（未压缩）字节**的哈希，不是存储对象的哈希。同一个文件在明文与 .zst
    -- 两种表示下必须算出同一个值，否则「明文优先、压缩去重」那条规则会重复上传
    content_sha256  sha256      NOT NULL,
    -- RustFS 对象 key（D16）。**存下来而不是算出来**：布局以后会变（P6-1 会加上
    -- key id），而旧行的 key 不能跟着变
    storage_key     TEXT        NOT NULL,
    encoding        TEXT        NOT NULL CHECK (encoding IN ('zstd', 'identity')),
    -- 客户端读到这些字节的时刻（客户端时钟）。与 created_at 是两条时间轴：
    -- at 用它，as_of 用 created_at
    observed_at     TIMESTAMPTZ NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    -- 连续性的一半：同一代里同一个偏移只能有一个块。另一半（「偏移必须等于当前
    -- 末尾」）在 ArchiveTxn::append_chunk 里，因为那需要先读末尾
    UNIQUE (generation_id, byte_offset)
);

COMMENT ON TABLE capture_chunks IS
    '一代里的一个分块（append-only，不进 sync_log）。字节在对象存储的 capture/
     命名空间，这里只有位置与哈希。content_sha256 是逻辑字节的哈希，也是幂等键。';

-- ══════════════════════════════════════════════════════════
--  五、文件的生命周期观察
-- ══════════════════════════════════════════════════════════

-- 宿主删了它、用户排除了它……「Cormex 是否是唯一副本」由此得出。
--
-- 为什么是事件而不是文件上的一个状态列：状态列要 UPDATE，而这里全链路 append-only；
-- 而且「什么时候消失的」本身是用户会问的问题。
CREATE TABLE capture_file_events (
    id          ulid        PRIMARY KEY,
    file_id     ulid        NOT NULL REFERENCES capture_files(id),
    op          TEXT        NOT NULL CHECK (op IN ('missing', 'reappeared', 'excluded', 'included')),
    detail      TEXT        CHECK (detail IS NULL OR length(detail) <= 1024),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE capture_file_events IS
    '文件在设备上的生命周期观察（append-only，不进 sync_log）。
     「Cormex 是否是唯一副本」= 所有设备上最新一条 op 都是 missing。';

CREATE INDEX idx_capture_file_events ON capture_file_events (file_id, created_at DESC);

-- ══════════════════════════════════════════════════════════
--  六、密钥遮罩覆盖层（D5）
-- ══════════════════════════════════════════════════════════

-- R 层的遮罩是**字节区间覆盖层**：只记区间，不改原字节。原文永远对所有者可读，
-- 而默认展示与检索走的是 P 层（那里已经替换成 [masked:…]）。
--
-- 为什么不是「写的时候就把密钥抹掉」：那样原文就不再是原文，而「字节级保真」是
-- 整个采集层的前提 —— 还原回去的会话文件 sha256 必须和原文件一致。
CREATE TABLE capture_masks (
    id             ulid        PRIMARY KEY,
    generation_id  ulid        NOT NULL REFERENCES capture_generations(id),
    byte_offset    BIGINT      NOT NULL CHECK (byte_offset >= 0),
    byte_length    INTEGER     NOT NULL CHECK (byte_length > 0),
    -- redaction::Rule::name()
    rule           TEXT        NOT NULL,
    -- 扫描器版本。规则升级后可以重扫，新旧结果并存而不是互相覆盖
    scanner        TEXT        NOT NULL,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (generation_id, byte_offset, rule, scanner)
);

COMMENT ON TABLE capture_masks IS
    'R 层的密钥遮罩覆盖层（append-only，不进 sync_log）。只记字节区间，不改原字节；
     遮罩区间不进任何索引。';

-- masker 扫到哪了。派生进度，append-only，取最新一行 —— 与 sync_log 的游标不同，
-- 这里不需要「跳过未提交行」的保证，所以没有 advisory lock（D23）。
CREATE TABLE capture_mask_progress (
    id             ulid        PRIMARY KEY,
    generation_id  ulid        NOT NULL REFERENCES capture_generations(id),
    scanner        TEXT        NOT NULL,
    through_offset BIGINT      NOT NULL CHECK (through_offset >= 0),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

COMMENT ON TABLE capture_mask_progress IS
    'masker 扫到哪了（append-only，不进 sync_log）。取最新一行即当前进度。';

CREATE INDEX idx_capture_mask_progress ON capture_mask_progress (generation_id, scanner, created_at DESC);
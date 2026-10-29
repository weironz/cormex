# 会话档案（Session Archive）—— 技术选型与架构设计

> 日期：2026-10-29 · 状态：**定稿，待实施**
>
> 上游：[调研报告与需求评审](../research/2026-10-agent-memory-survey.md)（所有者的决定见其 §9）、
> [memory.md](../memory.md)（记忆系统怎么工作）、[memory-content.md](../memory-content.md)（什么有资格进记忆）。
> 下游：[代码级阶段开发计划](../plan/session-archive-plan.md)。
>
> 本文是**唯一的设计权威**。调研报告 §8.4–§8.7 是评审阶段的草图，与本文冲突时以本文为准。
> 实施中发现本文有错，先改本文、再改代码，不要让代码和文档各说各话。

---

## 0. 一页纸

**要做的**：每台机器上一个轻量的 `cormex` 命令行工具，只读采集各家 AI agent 写在磁盘上的会话文件，原样（字节级）送进中央的 Cormex（Postgres + RustFS）；服务端把原文解析成统一的对话视图，供 UI 像原始对话一样浏览；再在此之上做摘要、事实抽取和检索，让任何 agent 通过 MCP 取回「交接简报」或按需取回原文。

**一句话架构**：

```
各机器                          中央 cortexd                                   读者
cormex CLI ──(设备密钥, HTTPS)──▶ R 原始层（RustFS 字节 + Postgres 元数据，只追加，不同步）
  ▲ 被 hooks / 定时任务调起       │ 解析（服务端，带版本，可重算）
  │ 做完即退                      ▼
  │                              P 归一化投影（对齐 ACP 内容块，密钥已遮罩）──▶ Web UI / CLI 浏览
  │                              │ 加工（后台，便宜模型，按策略）
  │                              ▼
  │                              D 派生物：观察日志 / 交接简报 / 事实 / 索引 ──▶ MCP / REST 取回
```

**不做的**：

- 不在客户端解析会话内容（解析器只有服务端一份）
- 不读项目目录，不打包代码库（只读 agent 自己的目录）
- 不常驻进程（`cormex watch` 是可选项，默认不装）
- 不加第二个存储或检索后端（不引入 Tantivy / 图数据库 / 第二个向量库）
- 不把 agent 循环搬进来（CLI 是采集器和查询工具，不是 agent）
- 不做会话格式之间的互转（Claude Code 的会话不转成 Codex 的格式）

---

## 1. 已拍板的决定

后续文档和代码注释用编号引用这些决定。改任何一条都要先改这里。

| # | 决定 | 来源 |
|---|---|---|
| **D1** | 录入者是本机的 `cormex` CLI；默认不常驻，由宿主 hooks 和系统定时任务调起，传完增量即退出。agent 可以**触发**采集，不**搬运**内容 | 报告 §8.2①、§8.4.8 |
| **D2** | 三层：R 原始层（逻辑字节，只追加）→ P 归一化投影（带 `parser_version`，可重算）→ D 派生物。**解析只在服务端** | 报告 §8.2② |
| **D3** | R 层和 P 层**只在服务端**：不进 `sync_log`，不下发到任何设备 | 报告 §10.2 |
| **D4** | 工具输出原文：归档 ✅；默认检索和注入 ❌；**原文档案索引** ✅（只在显式 `scope=archive` 时使用）；原文永远对所有者可读；agent 令牌默认没有 `raw:read` | 报告 §10.2（所有者 2026-10-29 选 A） |
| **D5** | 密钥遮罩：R 层用**字节区间覆盖层**（不改原字节）；P 层及以下存的文本里疑似密钥**已被替换**。遮罩区间不进任何索引 | 报告 §8.2③、§10.2 第 6 条 |
| **D6** | 采集单元是「逻辑文件 + 代（generation）」：追加 → 尾随；改写 / 截断 → 开新一代；压缩表示（Codex `.jsonl.zst`）→ 透明解压，采逻辑字节；SQLite 来源 → 按行快照 | 报告 §8.2④（复核） |
| **D7** | 采集边界：只读 agent 自己的目录（含宿主的附件目录、子 agent 目录、溢出的工具输出目录）。**不读项目目录**。唯一的例外是对 cwd 执行 `git remote get-url origin` 和 `git rev-parse HEAD` 拿元数据，可关闭。`~/.claude/file-history/` 默认不采 | 报告 §8.4.7 |
| **D8** | 「哪个 agent 的文件在哪」是**数据**（来源目录 catalog），CLI 内置一份，服务端提供可更新的一份 | 报告 §8.4.8 |
| **D9** | 设备凭据是**不轮换的设备密钥**（256 位随机数，服务端只存 SHA-256），由用户用**自己的** bearer 签发，带作用域，可按设备吊销。**不复用** refresh token（轮换 + 重放检测会把并发的 hook 进程判成「泄露」并吊销整条链） | 本文 §6.1 |
| **D10** | 「这个请求是谁」在认证中间件里**解析一次**，以扩展形式交给 handler；不再在 handler 里重新解析 header 并回落到 1 号用户 | 本文 §6.2（修掉现存 bug） |
| **D11** | P 层 schema 自定义（`cortex-proto`），但**对齐 ACP 的内容块**（消息、思考、工具调用 / 结果、diff、计划） | 报告 §8.3 |
| **D12** | 取回走三层渐进披露（索引 → 简报 / 摘要视图 → 原文）；所有输出经 `cortex_core::injection` 框定 | 报告 §8.4.4 |
| **D13** | 两条时间轴：`as_of`（Cormex 何时知道，系统时间）与 `at`（会话里何时发生，事件时间） | 报告 §8.2⑥ |
| **D14** | 加工：观察日志（追加式）+ 交接简报（懒生成）写进现有 `summaries`；事实抽取通过**按策略晋升**为 `episodes` 复用现有抽取管线 | 报告 §8.4.3 |
| **D15** | 原文档案索引用 **Postgres tsvector（jieba 分词，复用 `cortex_memory::tokenize`）**，不引入 Tantivy | CLAUDE.md「不加第二个存储后端」 |
| **D16** | RustFS 里采集数据用独立的 key 命名空间 `capture/`，与媒体 `blobs/` 分开；每个分块记自己的 `storage_key` | 本文 §4.3 |
| **D17** | 加密：MVP 依赖传输 TLS + 部署层的静态加密；第 6 阶段引入每来源数据密钥（用 `CORTEX_SECRET_KEY` 包裹）做 crypto-shredding，并如实写清备份的限制 | 报告 §10.2、memory-content §8.6② |
| **D18** | CLI 分发：单个静态二进制；GitHub Release + `install.sh` / `install.ps1` + npm 包装（主包 + 平台子包）；版本号与 workspace 同一条线 | 报告 §8.4.5 |
| **D19** | MCP 依赖 rmcp 1.8 → 3.x（MCP 2026-07-28 无状态版），作为独立阶段，**早于**新增会话 MCP 工具。rmcp 钉在 1.8 是因为 goose-providers 0.1.0-alpha.1 的 `Tool` 类型用的是它；goose-providers 0.1.0-alpha.13 要求 rmcp `^3.4.1`，所以**两者必须在同一次改动里一起升**，否则会有两份 rmcp 共存 | 报告 §6.5（复核）、`crates/cortexd/Cargo.toml` 注释 |
| **D20** | 空库启动修复：`Store::migrate_global` 内部先对 `public` 跑一次租户 `MIGRATOR`，一处覆盖全部三个调用点 | 本文 §9.1 |
| **D21** | `cortex-proto` 的 `llm` 模块放进默认开启的 feature；CLI 关掉它，不把 goose-providers 拖进客户端 | 本文 §3 |
| **D22** | CLI 本机状态用 SQLite（rusqlite，bundled），不用 JSON 文件：多个 hook 进程会并发写状态 | 本文 §5.4 |
| **D23** | 归档表走新的 `Store::archive_txn`：**不拿 sync 的 advisory lock、不写 sync_log**，只开放归档表的写方法；`write_txn` 的纪律对同步表不变 | 本文 §4.4 |

---

## 2. 组件与职责

```
┌───────────────────────── 每台机器 / 每个环境 ──────────────────────────┐
│                                                                        │
│  agent 宿主（Claude Code / Codex / Cursor / Gemini CLI / …）          │
│     │ 写自己的会话文件（只读，不改）                                   │
│     │ hooks ──调起──▶ cormex hook <agent>   （记一笔，起后台 sync，立刻退出）
│     │ 系统定时任务 ──调起──▶ cormex sync --once                        │
│     ▼                                                                  │
│  会话文件 ◀──只读── cormex（crates/cormex，单二进制）                   │
│                       ├ catalog：按来源目录发现文件                     │
│                       ├ state.db：每个逻辑文件采到哪了（SQLite）        │
│                       ├ sync：增量上传 / 换代 / 断网暂存 / 续传          │
│                       ├ init / doctor / uninstall：装卸 hooks 与定时任务 │
│                       ├ sessions：ls / show / brief / restore           │
│                       └ mcp：stdio ↔ 远端 /mcp 的桥                     │
└───────────────────────────────┬────────────────────────────────────────┘
                                │ HTTPS，Authorization: Bearer <设备密钥>
┌───────────────────────────────▼──────────── 中央 ─────────────────────┐
│ cortexd                                                                │
│  auth::require ── Principal 扩展（D10）                                 │
│  /devices        设备密钥（D9）                    cortex_auth.device_keys
│  /capture/*      采集写入（R 层）                   capture_* 表 + RustFS capture/
│  /archive/*      会话浏览、原文读取、检索           transcript_* / archive_* 表
│  /mcp            memory_search / remember / session_search / session_get
│  后台 worker（每租户轮询，backfill.rs 的模式）：
│    projector   R → P（cortex-transcript 解析器 + 文本遮罩）
│    masker      R 层字节区间遮罩覆盖层
│    indexer     P → 原文档案索引 / 会话检索索引 / 向量
│    distiller   P → 观察日志、交接简报（summaries）；按策略晋升 episodes → 现有抽取
└────────────────────────────────────────────────────────────────────────┘
        │                              │
   Postgres + pgvector             RustFS（S3 兼容）
                                        ▲
cormex-web（Flutter）：会话列表 / 对话视图 / 原始切换 / 两条时间轴 / 「在原文中搜索」
```

### 2.1 Crate 布局

| crate | 新 / 改 | 职责 | 关键依赖 |
|---|---|---|---|
| `cortex-proto` | 改 | 新增 `capture` 模块：采集线协议 DTO、来源目录（catalog）类型与内置数据、P 层事件类型（ACP 对齐）；`llm` 模块挪到默认 feature 后面（D21） | serde、chrono |
| `cortex-transcript` | **新** | 服务端解析器：逻辑字节 → `TranscriptEvent` 序列。纯函数，无 IO，按 agent 分模块，夹具测试 | cortex-proto、serde_json |
| `cortex-store` | 改 | 新增归档表的模型、`ArchiveTxn`（D23）、查询；新 migration | sqlx |
| `cortex-blob` | 改 | 新增按 key 读写的接口与 `capture/` 命名空间（D16） | — |
| `cortex-memory` | 改 | `redaction::scan_spans`（返回全部命中区间）；文本遮罩函数 | — |
| `cortexd` | 改 | `/devices`、`/capture/*`、`/archive/*`、MCP 工具、后台 worker、Principal 中间件 | — |
| `cormex` | **新** | 客户端二进制 | clap、reqwest、rusqlite、zstd、keyring、directories、fs4、notify（可选） |
| `app/` | 改 | Flutter 会话视图 | Riverpod |

**依赖方向**（不得反转）：`cortex-core ← cortex-proto ← {cortex-transcript, cormex}`；`cortex-store`、`cortex-blob` 只被 `cortexd` 用；`cormex` **不依赖** `cortex-store` / `cortex-blob` / `cortex-memory` / `cortex-llm` / `sqlx`。

---

## 3. 技术选型

| 关注点 | 选择 | 没选的 | 理由 |
|---|---|---|---|
| 语言 / 运行时 | Rust 2024 + tokio（与现有一致） | — | 单一技术栈；客户端要静态单二进制 |
| CLI 参数 | `clap` 4（workspace 已有） | — | 已在用 |
| HTTP 客户端 | `reqwest` 0.13（rustls，workspace 已有） | ureq | 与服务端一致；需要流式上传和异步 |
| 客户端本机状态 | **`rusqlite` 0.41，`bundled`**（D22） | JSON 文件、sled | 多个 hook 进程会并发改状态：SQLite 的 WAL + `busy_timeout` + `BEGIN IMMEDIATE` 是现成的跨进程事务；读宿主 SQLite 来源（Cursor / OpenCode / Goose）本来也要它 |
| 读宿主 SQLite | rusqlite 只读打开（`file:…?mode=ro`），**按行读取** | 复制 `.db` 文件 | WAL 模式下复制文件拿到的是不一致的状态；Cursor 的 `state.vscdb` 可能几百 MB |
| 压缩 | `zstd` 0.14（两端） | gzip | Codex 本来就用 zstd 压冷文件；JSONL 可压缩（实测约 3.5 倍，见 §9.4） |
| 内容哈希 | SHA-256（`sha2`） | blake3 | 数据库的 `sha256` DOMAIN 已经定了；与 `blobs` 一致 |
| 凭据存储 | `keyring` 4（系统钥匙串），失败时退回 0600 文件 | 只用文件 | 设备密钥是长期凭据；无钥匙串的服务器 / 容器上退回文件并在 `doctor` 里提示 |
| 平台目录 | `directories` 6 | 手写 | 三平台的 config / data / cache 目录规则 |
| 进程互斥 | `fs4` 文件锁（每个逻辑文件的采集只有一个进程在做） | 只靠 SQLite | SQLite 管状态的原子性，文件锁避免两个进程白跑同一份活 |
| 文件监听（可选） | `notify` 8，只用于 `cormex watch` | — | 默认不装（D1） |
| 服务端存储 | Postgres + pgvector（元数据、P 层、索引）+ RustFS（字节） | — | 现有唯一硬依赖 |
| 原文档案索引 | Postgres `tsvector` + jieba（D15） | Tantivy、Meilisearch | 不加第二个检索后端；中文分词复用 `cortex_memory::tokenize` |
| 解析 | 新 crate `cortex-transcript`，serde_json 流式逐行 | 让客户端解析 | 解析器只存在一份（D2） |
| P 层 schema | 自定义，对齐 ACP 内容块（D11） | 直接依赖 `agent-client-protocol` crate | 不被 ACP 的版本节奏绑住；字段对齐即可 |
| LLM 加工 | `cortex-llm`（goose providers），便宜模型 | — | 现有供应商层；约束 5 |
| MCP | rmcp 3.x，连同 goose-providers alpha.1 → alpha.13 一起升（D19） | 继续用 1.8 | 2026-07-28 无状态版；`ttlMs` / `cacheScope` 正好表达可缓存的简报资源 |
| Web | Flutter + Riverpod（现有） | — | 现有 |
| 发布 | GitHub Actions 矩阵构建（Linux musl 用 `cargo-zigbuild`；macOS / Windows 用原生 runner）+ 手写 npm 包装（D18） | cargo-dist | cargo-dist 会接管 `release.yml`，和现有的镜像发布流水线冲突；手写矩阵可控 |

**`cortex-proto` 的 feature 拆分**（D21）：

```toml
# crates/cortex-proto/Cargo.toml
[features]
default = ["llm"]
llm = ["dep:cortex-llm"]

[dependencies]
cortex-llm = { workspace = true, optional = true }
```

`src/lib.rs` 里 `#[cfg(feature = "llm")] pub mod llm;`。`cormex` 用 `cortex-proto = { workspace = true, default-features = false }`。`cortexd` 不变（默认开启）。

---

## 4. 服务端数据模型

### 4.1 新的租户 migration：`migrations/20261101000001_capture_archive.sql`

> 规矩：新文件、LF 行尾、写进 `crates/cortex-store/tests/migrations_are_immutable.rs` 的 `PINNED`。主键一律 `ulid`，时间一律 `clock_timestamp()`。**全部 append-only**：没有 UPDATE，没有 DELETE（purge 例外，见 §8.4）。

```sql
-- 一个「被采集的会话」= 某个宿主里的一段对话。跨设备合并：同一个 agent 的同一个原生会话 id 只有一行。
CREATE TABLE capture_sessions (
    id                 ulid        PRIMARY KEY,
    agent              TEXT        NOT NULL CHECK (agent ~ '^[a-z][a-z0-9_]{1,31}$'),
    native_session_id  TEXT        NOT NULL CHECK (length(native_session_id) BETWEEN 1 AND 256),
    parent_session_id  ulid        REFERENCES capture_sessions(id),   -- 子 agent 会话
    first_seen_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (agent, native_session_id)
);

-- 会话在某台设备上的一个逻辑文件。一个会话可能有多个文件（主 transcript、子 agent、溢出的工具输出、附件……）
CREATE TABLE capture_files (
    id             ulid        PRIMARY KEY,
    session_id     ulid        NOT NULL REFERENCES capture_sessions(id),
    device_id      ulid        NOT NULL,          -- cortex_auth.device_keys.id；跨 schema，不建外键
    agent          TEXT        NOT NULL,
    logical_path   TEXT        NOT NULL CHECK (length(logical_path) BETWEEN 1 AND 4096),
    role           TEXT        NOT NULL CHECK (role IN
                       ('transcript', 'subagent', 'sidecar', 'attachment', 'memory', 'row')),
    framing        TEXT        NOT NULL CHECK (framing IN ('append', 'snapshot')),
    cwd            TEXT,
    project_key    TEXT        CHECK (project_key IS NULL OR length(project_key) <= 512),
    first_seen_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (device_id, agent, logical_path)
);
CREATE INDEX idx_capture_files_session ON capture_files (session_id);

-- 逻辑文件的「代」。追加不开新代；改写 / 截断 / 换 inode 后内容对不上，开新代，旧代原样保留。
CREATE TABLE capture_generations (
    id          ulid        PRIMARY KEY,
    file_id     ulid        NOT NULL REFERENCES capture_files(id),
    ordinal     INTEGER     NOT NULL CHECK (ordinal >= 0),
    reason      TEXT        NOT NULL CHECK (reason IN ('initial', 'rewrite', 'truncate', 'snapshot')),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (file_id, ordinal)
);

-- 一代里的一个分块。字节在 RustFS（zstd 压缩），这里只有位置与哈希。
-- 连续性由服务端保证：新块的 byte_offset 必须等于这一代当前的末尾（见 §4.4）。
CREATE TABLE capture_chunks (
    id              ulid        PRIMARY KEY,
    generation_id   ulid        NOT NULL REFERENCES capture_generations(id),
    byte_offset     BIGINT      NOT NULL CHECK (byte_offset >= 0),
    byte_length     BIGINT      NOT NULL CHECK (byte_length > 0 AND byte_length <= 33554432),
    content_sha256  sha256      NOT NULL,          -- 逻辑（未压缩）字节的哈希：幂等键
    storage_key     TEXT        NOT NULL,          -- RustFS 对象 key（D16），布局变了也不用改旧行
    encoding        TEXT        NOT NULL CHECK (encoding IN ('zstd', 'identity')),
    observed_at     TIMESTAMPTZ NOT NULL,          -- 客户端读到这些字节的时刻（客户端时钟）
    created_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),   -- 系统时间轴（as_of 用它）
    UNIQUE (generation_id, byte_offset)
);

-- 文件在设备上的生命周期观察：宿主删了它、用户排除了它……「Cormex 是否是唯一副本」由此得出。
CREATE TABLE capture_file_events (
    id          ulid        PRIMARY KEY,
    file_id     ulid        NOT NULL REFERENCES capture_files(id),
    op          TEXT        NOT NULL CHECK (op IN ('missing', 'reappeared', 'excluded', 'included')),
    detail      TEXT        CHECK (detail IS NULL OR length(detail) <= 1024),
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX idx_capture_file_events ON capture_file_events (file_id, created_at DESC);

-- R 层的密钥遮罩覆盖层（D5）：只记字节区间，不改原字节。
CREATE TABLE capture_masks (
    id             ulid        PRIMARY KEY,
    generation_id  ulid        NOT NULL REFERENCES capture_generations(id),
    byte_offset    BIGINT      NOT NULL CHECK (byte_offset >= 0),
    byte_length    INTEGER     NOT NULL CHECK (byte_length > 0),
    rule           TEXT        NOT NULL,           -- redaction::Rule::name()
    scanner        TEXT        NOT NULL,           -- 扫描器版本，规则升级后可重扫
    created_at     TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (generation_id, byte_offset, rule, scanner)
);

-- masker 扫到哪了（派生进度，append-only，取最新一行）
CREATE TABLE capture_mask_progress (
    id             ulid        PRIMARY KEY,
    generation_id  ulid        NOT NULL REFERENCES capture_generations(id),
    scanner        TEXT        NOT NULL,
    through_offset BIGINT      NOT NULL CHECK (through_offset >= 0),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX idx_capture_mask_progress ON capture_mask_progress (generation_id, scanner, created_at DESC);
```

### 4.2 第二个租户 migration：`migrations/20261115000001_transcript_projection.sql`（第 2 阶段）

```sql
-- P 层：一条归一化的对话事件。由解析器从 R 层投影而来，可重算（parser_version 不同 = 另一套投影）。
-- 文本里的疑似密钥已经被替换（D5），所以这张表及其下游都不含密钥。
CREATE TABLE transcript_events (
    id                 ulid        PRIMARY KEY,     -- Id::derived(occurred_at, "<session>|<parser_version>|<generation>|<offset>")
    session_id         ulid        NOT NULL REFERENCES capture_sessions(id),
    parser_version     TEXT        NOT NULL,        -- 例如 "claude_code@3"
    seq                BIGINT      NOT NULL CHECK (seq >= 0),
    kind               TEXT        NOT NULL CHECK (kind IN
                           ('user_message', 'agent_message', 'agent_thought', 'tool_call',
                            'tool_result', 'plan', 'system', 'meta', 'unknown')),
    occurred_at        TIMESTAMPTZ,                 -- 事件时间轴（at 用它）；来源没有时间戳时为 NULL
    content            JSONB       NOT NULL,        -- ACP 对齐的内容块；超过阈值的块换成指回 R 层的引用
    text               TEXT,                        -- 纯文本，供展示与检索（已遮罩）
    tsv                TSVECTOR,                    -- 与主行同事务写入（约束 1）
    tool_name          TEXT,
    tool_call_id       TEXT,
    src_generation_id  ulid        NOT NULL REFERENCES capture_generations(id),
    src_offset         BIGINT      NOT NULL,
    src_length         BIGINT      NOT NULL,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (session_id, parser_version, seq)
);
CREATE INDEX idx_transcript_events_time ON transcript_events (session_id, parser_version, occurred_at);
CREATE INDEX idx_transcript_events_tsv  ON transcript_events USING gin (tsv);

-- projector 的进度（append-only，取最新）。through 是 {generation_id: 已投影到的字节偏移}
CREATE TABLE transcript_projections (
    id              ulid        PRIMARY KEY,
    session_id      ulid        NOT NULL REFERENCES capture_sessions(id),
    parser_version  TEXT        NOT NULL,
    through         JSONB       NOT NULL,
    next_seq        BIGINT      NOT NULL,
    status          TEXT        NOT NULL CHECK (status IN ('ok', 'partial', 'failed')),
    error           TEXT,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);
CREATE INDEX idx_transcript_projections ON transcript_projections (session_id, parser_version, created_at DESC);

-- 原文档案索引（D4 / D15）：含工具输出的全文，只在 scope=archive 时使用。遮罩区间早已在 P 层替换掉。
CREATE TABLE archive_chunks (
    id          ulid        PRIMARY KEY,
    session_id  ulid        NOT NULL REFERENCES capture_sessions(id),
    event_id    ulid        NOT NULL REFERENCES transcript_events(id),
    part        INTEGER     NOT NULL CHECK (part >= 0),      -- 长事件切成多段
    text        TEXT        NOT NULL,
    tsv         TSVECTOR    NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    UNIQUE (event_id, part)
);
CREATE INDEX idx_archive_chunks_tsv ON archive_chunks USING gin (tsv);

-- 会话的显示元数据（标题、项目归属）复用现有 session_events / project_events 的思路，但会话 id 是 capture_sessions.id
-- （第 5 阶段把 derivations.source_kind 的 CHECK 扩展出 'transcript_event'，另开 migration）
```

### 4.3 RustFS 布局（D16）

```
{tenant}/blobs/aa/bb/{sha256}              现有：媒体，hash = 存储字节的 SHA-256
{tenant}/capture/aa/bb/{sha256}.zst        新增：采集分块，hash = 逻辑（未压缩）字节的 SHA-256，内容是 zstd 帧
```

- `cortex-blob` 增加 `BlobStore::put_keyed(key, bytes, mime)` / `get_keyed(key)` / `delete_keyed(key)`，以及 `hash::capture_key(tenant, sha256) -> String`。key 仍然经过 `validate_hash` 那一道防路径穿越
- **key 由调用方给出完整值，实现不得再拼接构造时配置的前缀**：现在 `MediaStore::current_tenant()` 把媒体的租户前缀写死成 `public`（多租户尚未接到对象存储这一层），采集数据不能沿用这个过渡做法。`capture_key` 的 `tenant` 参数取请求所属租户的 schema 名（`Tenant::schema()`）
- 采集分块**不写 `blobs` 表**：`blobs` 会进 `sync_log` 下发到设备，而 R 层不下发（D3）。将来给媒体做 GC 时以 `blobs` 表为准，`capture/` 命名空间天然不受影响
- 同一租户内，相同的逻辑分块只存一份（内容寻址），比如同一个会话在两台机器上都被采到

### 4.4 归档写事务 `ArchiveTxn`（D23）

现有的 `write_txn` 为了 `sync_log` 的游标正确性，会拿每个租户一把的 advisory lock 并强制写 `sync_log`。归档表两者都不需要：

- 它们**不进 sync_log**（D3），没有「游标跳过未提交行」的问题
- 下游消费者（projector / indexer / distiller）用显式的进度表（`capture_mask_progress`、`transcript_projections`），不用游标

所以新增一条**并列的**写通道：

```rust
// crates/cortex-store/src/archive.rs（新文件）
impl Store {
    /// Write transaction for the server-only archive tables (capture_* / transcript_* / archive_*).
    pub async fn archive_txn<T, F>(&self, f: F) -> Result<T>
    where F: AsyncFnOnce(&mut ArchiveTxn) -> Result<T>;
}

pub struct ArchiveTxn { /* sealed: raw transaction is private, like WriteTxn */ }

impl ArchiveTxn {
    pub async fn upsert_capture_session(&mut self, new: &NewCaptureSession) -> Result<CaptureSessionId>;
    pub async fn register_capture_file(&mut self, new: &NewCaptureFile) -> Result<CaptureFileRow>;
    pub async fn open_generation(&mut self, file_id: Id, reason: GenerationReason) -> Result<GenerationRow>;
    /// Serializes per generation with pg_advisory_xact_lock(4273, hashtext(generation_id)).
    pub async fn append_chunk(&mut self, new: &NewCaptureChunk) -> Result<AppendOutcome>;
    pub async fn insert_file_event(&mut self, new: &NewCaptureFileEvent) -> Result<()>;
    pub async fn insert_masks(&mut self, masks: &[NewCaptureMask], progress: &NewMaskProgress) -> Result<()>;
    pub async fn insert_transcript_events(&mut self, events: &[NewTranscriptEvent], progress: &NewProjection) -> Result<()>;
    pub async fn insert_archive_chunks(&mut self, chunks: &[NewArchiveChunk]) -> Result<()>;
}

pub enum AppendOutcome {
    Appended { end_offset: i64 },
    /// Same (offset, content_sha256) already stored: idempotent replay.
    AlreadyPresent { end_offset: i64 },
    /// offset != current end of generation. Client must resume from `expected`.
    OffsetMismatch { expected: i64 },
}
```

要守住的四条：

1. **`table::ALL` 里不能出现归档表**。新增 `table::ARCHIVE`，并加测试断言两者不相交，以及 `ArchiveTxn` 的任何方法都不会往 `sync_log` 写行
2. **`tsv` 与主行同事务**（约束 1）在 `ArchiveTxn` 里照样成立：`transcript_events.tsv`、`archive_chunks.tsv` 都在 INSERT 时一并写
3. **事务短小、只写**：读 RustFS、解压、解析、调模型都在事务外完成；一次事务最多写 500 行
4. **`append_chunk` 的连续性**：事务内先 `pg_advisory_xact_lock(4273, hashtext(generation_id))`，再算当前末尾（`max(byte_offset + byte_length)`），只接受 `offset == 末尾`；`UNIQUE(generation_id, byte_offset)` 撞上时，`content_sha256` 相同即幂等成功，不同即 409

CLAUDE.md 约束 1 的「写事务必须走 `store.write_txn`」要改成：**同步表**必须走 `write_txn`；**归档表**必须走 `archive_txn`；两者都不允许裸事务。（计划 P1-3 里一并改 CLAUDE.md。）

### 4.5 全局 migration：`migrations-global/20261101000001_device_keys.sql`

```sql
CREATE TABLE cortex_auth.device_keys (
    id            ulid        PRIMARY KEY,
    user_id       ulid        NOT NULL REFERENCES cortex_auth.users(id),
    key_sha256    sha256      NOT NULL UNIQUE,
    label         TEXT        NOT NULL CHECK (length(label) BETWEEN 1 AND 100),
    scopes        TEXT[]      NOT NULL CHECK (scopes <@ ARRAY[
                      'capture:write', 'session:read', 'memory:read', 'memory:write', 'raw:read']::TEXT[]),
    platform      TEXT,                              -- linux-x86_64 / macos-aarch64 / …，诊断用
    created_at    TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    last_seen_at  TIMESTAMPTZ,
    revoked_at    TIMESTAMPTZ
);
CREATE INDEX idx_device_keys_user ON cortex_auth.device_keys (user_id);
```

`cortex_auth` 本来就用 UPDATE 记 `rotated_at` / `revoked_at`（它不是记忆表，不受 append-only 约束）；`last_seen_at` 最多每 10 分钟更新一次，避免每个请求一次写。

---

## 5. 客户端 `cormex`

### 5.1 命令

| 命令 | 作用 | 谁来调 |
|---|---|---|
| `cormex login [--server URL] [--label NAME] [--scopes …] [--key KEY]` | 交互输入账号密码 → `/auth/login` → `POST /devices` 拿设备密钥 → 存钥匙串；`--key` 直接用 Web 里生成的设备密钥（无交互环境） | 人 |
| `cormex logout` | 吊销本设备密钥并删除本地凭据 | 人 |
| `cormex status` | 服务端可达性、登录身份、来源数、积压量、最近一次成功 | 人 / agent |
| `cormex detect [--json]` | 按 catalog 探测本机 agent，列出会话数和体积，**只看路径和元数据** | 人 / agent |
| `cormex init [--yes] [--dry-run] [--agents a,b]` | 登录（如未登录）→ detect → 逐项征求同意：装 hooks、注册 MCP、调大 `cleanupPeriodDays`、注册定时任务 → 首次回填估算 → 回填 | 人 / agent（经 Skill） |
| `cormex uninstall [--keep-data]` | 撤销 init 改过的每一处配置（从备份还原） | 人 |
| `cormex doctor [--json]` | hooks 最近一次触发、定时任务最近一次成功、暂存量、长期无新数据的来源、凭据存储方式、`cleanupPeriodDays` | 人 / agent |
| `cormex hook <agent>` | 宿主 hook 调用：读 stdin JSON，记入本地队列，起后台 sync，**立刻退出** | 宿主 |
| `cormex sync [--once] [--queue] [--source PATH] [--dry-run]` | 增量采集 | 定时任务 / 后台 / 人 / agent |
| `cormex watch` | 常驻监听（可选） | 人 |
| `cormex sources ls / add / exclude / include` | 管理来源与排除规则 | 人 / agent |
| `cormex sessions ls / show / brief / restore` | 浏览与还原 | 人 / agent |
| `cormex mcp` | stdio MCP ↔ 远端 `/mcp` 的桥（给只支持 stdio 的宿主） | 宿主 |
| `cormex import <path>` | 导入打包带出来的会话目录，或 ChatGPT / Claude 导出文件 | 人 |
| `cormex catalog update` | 从服务端拉最新来源目录 | 定时任务 / 人 |

退出码：`0` 成功；`2` 用法错误；`3` 未登录 / 凭据失效；`4` 服务端不可达（已暂存）；`5` 部分来源失败。`hook` 子命令**永远退出 0**：它不该因为任何原因阻塞或干扰宿主。

### 5.2 来源目录（catalog，D8）

数据文件 `crates/cortex-proto/catalog/agents.toml`，`include_str!` 进 `cortex-proto`，CLI 与服务端共用；服务端的 `GET /capture/catalog` 可以用环境变量 `CORMEX_CAPTURE_CATALOG` 指向一份更新的文件。

```toml
schema = 1

[[agent]]
id = "claude_code"
name = "Claude Code"
home = [
  { env = "CLAUDE_CONFIG_DIR" },
  { path = "~/.claude" },
]

  [[agent.source]]
  role = "transcript"
  glob = "projects/*/*.jsonl"
  framing = "append"
  session_id = { from = "stem" }
  cwd = { from = "parent_dir", decode = "claude_project_dir" }

  [[agent.source]]
  role = "subagent"
  glob = "projects/*/*/subagents/*.jsonl"
  framing = "append"
  session_id = { from = "stem" }                        # 子 agent 自己的会话
  parent_session_id = { from = "path_segment", index = 2 }   # projects/<项目>/<父会话>/subagents/…

  [[agent.source]]
  role = "sidecar"
  glob = "projects/*/*/tool-results/**"
  framing = "snapshot"
  session_id = { from = "path_segment", index = 2 }    # 归属父会话；用正向下标，因为 ** 的深度不定

  [[agent.source]]
  role = "memory"
  glob = "projects/*/memory/**"
  framing = "snapshot"
  session_id = { from = "path_segment", index = 1, prefix = "memory:" }   # 每个项目一个伪会话

  [agent.hooks]
  kind = "claude_code_settings"                         # ~/.claude/settings.json 的 hooks 段
  events = ["Stop", "SubagentStop", "PreCompact", "SessionEnd"]

  [agent.retention]
  setting = "cleanupPeriodDays"
  file = "settings.json"
  default_days = 30

[[agent]]
id = "codex"
name = "Codex CLI"
home = [ { env = "CODEX_HOME" }, { path = "~/.codex" } ]

  [[agent.source]]
  role = "transcript"
  glob = "{sessions,archived_sessions}/**/rollout-*.jsonl"
  also_compressed = ".zst"                              # 同名 .jsonl.zst 视为同一逻辑文件
  framing = "append"
  session_id = { from = "stem_regex", regex = "rollout-[0-9T-]+-(?P<id>[0-9a-f-]{36})" }

  [[agent.source]]
  role = "memory"
  glob = "memories/{raw_memories.md,rollout_summaries/**}"
  framing = "snapshot"
  session_id = { from = "const", value = "memory", per_device = true }   # 每台机器一个伪会话

  [agent.hooks]
  kind = "codex_hooks_json"                             # ~/.codex/hooks.json
  events = ["Stop", "PreCompact", "SessionEnd"]
```

每加一个 agent 只改这份数据加一个服务端解析器，CLI 代码不动。解析 `cwd` 的 `decode` 和 `session_id` 的提取都**只看路径**，不读文件内容（D2）。

`session_id` 规则的语义：`path_segment` 的 `index` 是 `logical_path` 按 `/` 切开后的下标（负数从末尾数；遇到 `**` 时只用正数）；`prefix` 拼在结果前面，避免伪会话 id 与真实会话 id 撞车；`per_device = true` 时 CLI 在结果后面拼 `@<device_id>`，用于每台机器各一份的东西（比如 Codex 的全局记忆），否则同一个 agent 在不同机器上的这类文件会被合并成一个会话。

### 5.3 采集算法（`sync`）

对每个逻辑文件：

```
1. 发现：catalog glob 展开 → 候选路径；应用排除规则（项目 / 会话粒度，D7）
2. 读本地状态：(file_id, generation_ordinal, uploaded_offset, tail_sha256, last_size, last_mtime)
3. 读逻辑字节：普通文件直接读；有 .zst 兄弟且明文不存在时透明解压
4. 判断：
   - 从未见过                         → POST /capture/files 注册 → 从 0 上传
   - size < uploaded_offset           → 截断：开新代（reason=truncate）→ 从 0 上传
   - bytes[off-4096..off] 的哈希 ≠ tail_sha256 → 改写：开新代（reason=rewrite）→ 从 0 上传
   - 否则                             → 追加：从 uploaded_offset 上传到 EOF
   - framing=snapshot：首次用第 0 代；之后整文件哈希 = 上次 → 跳过，否则开新代（reason=snapshot）上传整份
5. 上传：每块 ≤ 4 MiB 逻辑字节；PUT /capture/generations/{g}/chunks/{offset}
   - 200 / AlreadyPresent → 推进本地 offset 和 tail_sha256（同一个 SQLite 事务）
   - 409 OffsetMismatch   → 以服务端的 expected 为准重算（客户端状态落后或领先）
   - 网络错误             → 指数退避；超出预算则留在队列里，下次任何触发再补
6. 文件消失：发 POST /capture/files/{id}/events {op: missing}（只发一次）
```

- 「追加」判断只比较已上传区间的尾部 4 KiB，**不需要**整份重读
- 写到一半的最后一行可以照传：R 层只认字节，后续追加会接上；解析器负责处理跨块的行
- 每个逻辑文件处理前先拿 `fs4` 文件锁（`<data>/locks/<hash>.lock`），拿不到就跳过（别的进程正在做）
- 一次 `sync` 的总时间和总字节有预算（默认 60 秒 / 512 MiB），超出留给下一次；首次回填走 `cormex init` 里的显式步骤
- SQLite 来源（第 4 阶段）：只读打开，用 catalog 里给的查询逐行读取，每行是一个 `role=row`、`framing=snapshot` 的逻辑文件（`logical_path = "sqlite:<db 相对路径>#<表>/<主键>"`），值的哈希没变就跳过；批量走 `POST /capture/snapshots`

### 5.4 本机状态 `state.db`（SQLite，D22）

```sql
CREATE TABLE files (
  local_key         TEXT PRIMARY KEY,   -- agent + 逻辑路径
  agent             TEXT NOT NULL,
  path              TEXT NOT NULL,      -- 当前物理路径
  file_id           TEXT,               -- 服务端 ulid；未注册为 NULL
  generation_id     TEXT,
  generation_ordinal INTEGER,
  uploaded_offset   INTEGER NOT NULL DEFAULT 0,
  tail_sha256       TEXT,
  whole_sha256      TEXT,               -- snapshot 类
  last_size         INTEGER,
  last_mtime_ns     INTEGER,
  missing_reported  INTEGER NOT NULL DEFAULT 0,
  last_ok_at        TEXT
);
CREATE TABLE queue (                     -- hook 记的「这个文件变了」
  id INTEGER PRIMARY KEY, agent TEXT NOT NULL, path TEXT NOT NULL,
  cwd TEXT,                             -- hook 输入里的 cwd，比从路径反推的准
  event TEXT, enqueued_at TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE runs (                      -- doctor 用
  id INTEGER PRIMARY KEY, trigger TEXT NOT NULL, started_at TEXT, finished_at TEXT,
  files INTEGER, bytes INTEGER, errors INTEGER, last_error TEXT
);
CREATE TABLE installs (                  -- init 改过哪些配置，uninstall 据此还原
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL, target TEXT NOT NULL,
  backup_path TEXT, installed_at TEXT NOT NULL
);
```

打开时 `PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;`，写用 `BEGIN IMMEDIATE`。`state.db` 有 `user_version` 做本地 schema 迁移。

### 5.5 hooks 与定时任务

| 宿主 | 写到哪里 | 事件 | 命令 | 备注 |
|---|---|---|---|---|
| Claude Code | `~/.claude/settings.json` 的 `hooks` | Stop、SubagentStop、PreCompact、SessionEnd | `cormex hook claude_code` | 输入带 `transcript_path`；SessionEnd 只有 1.5 秒预算，必须秒退 |
| Codex | `~/.codex/hooks.json` | Stop、PreCompact、SessionEnd | `cormex hook codex` | hooks 默认开启；SessionEnd 总是同步、默认 1 秒 |
| Gemini CLI | `~/.gemini/settings.json` 的 `hooks` | AfterAgent、PreCompress、SessionEnd | `cormex hook gemini_cli` | **stdout 必须是 JSON**：输出 `{}` |
| Cursor | `~/.cursor/hooks.json` | stop、preCompact、sessionEnd | `cormex hook cursor` | 云端 agent 不触发 sessionEnd；stdout 输出 `{}` |

`cormex hook` 的实现要点：

1. 读 stdin（最多 1 MiB，超时 200 ms），取 `transcript_path`（以及 `agent_transcript_path`），解析失败也照样退出 0
2. `INSERT INTO queue`（SQLite，`busy_timeout` 100 ms；拿不到锁就写一行到 `<data>/spool/*.json`）
3. 起后台进程 `cormex sync --queue`：Unix 用 `setsid` 脱离会话、关掉标准输入输出；Windows 用 `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP`
4. 按宿主要求打印 stdout（Gemini / Cursor 打 `{}`，其余不打），退出 0。**目标 50 ms 内返回**

定时任务（`init` 注册，`uninstall` 移除）：Linux 用 systemd `--user` timer，没有 systemd 时用 crontab；macOS 用 `~/Library/LaunchAgents/ai.cormex.sync.plist`；Windows 用 `schtasks`。默认每 20 分钟一次，跑 `cormex sync --once`。

修改宿主配置文件的规矩：先备份（`<file>.cormex-backup-<时间>`），用结构化方式合并（JSON / TOML 解析后插入，保留其他条目），写临时文件再原子改名，在 `installs` 表登记；`uninstall` 只移除自己加的条目。

### 5.6 凭据

- `cormex login`：交互输入账号密码 → `POST /auth/login`（带 `device_label`）拿 access token → `POST /devices`（用这个 access token）拿设备密钥明文（只出现这一次）→ 存进系统钥匙串（服务名 `cormex`，账号 = 服务端 URL）→ 调 `/auth/logout` 作废这次的 refresh token
- 无交互环境：在 Web UI 的「设备」页生成设备密钥，`cormex login --key <KEY>`（或者环境变量 `CORMEX_DEVICE_KEY`）
- 钥匙串不可用时退回 `<config>/credentials`（权限 0600），`doctor` 给出提示
- 第 6 阶段加设备码登录（RFC 8628 的形状：CLI 显示短码，用户在 Web 上确认）

---

## 6. 认证与授权

### 6.1 设备密钥（D9）

- 生成：`auth::generate()` 同款，256 位内核随机数；服务端只存 SHA-256（理由与 `auth.rs` 模块文档相同：高熵秘密用快哈希足够）
- 签发：`POST /devices`，**必须**带用户自己的 bearer（账号 access token 或旧的预共享 token），不接受设备密钥或委托令牌来签发新的设备密钥（不允许凭据自我繁殖）
- 校验：认证中间件在现有三个分支之后加一个分支：`st.device_keys().resolve(tok)` → 有缓存的 `DeviceScope { user_id, device_id, scopes }`（缓存 60 秒，吊销最多延迟 60 秒生效）
- 吊销：`DELETE /devices/{id}`（用户自己的 bearer，或该设备密钥本身）
- 作用域到路由（允许名单，与 `DelegatedScope::allows` 同一个思路：漏掉一条是响亮的 403，多放一条是静默的越权）：

| scope | 允许 |
|---|---|
| `capture:write` | `GET /capture/catalog`、`POST /capture/files`、`POST /capture/files/{id}/generations`、`PUT /capture/generations/{id}/chunks/{offset}`、`POST /capture/files/{id}/events`、`POST /capture/snapshots` |
| `session:read` | `GET /archive/sessions`、`GET /archive/sessions/{id}`、`GET /archive/sessions/{id}/events`、`GET /archive/sessions/{id}/brief`、`GET /archive/search`（`scope=conversation`）、`/mcp`（会话工具） |
| `memory:read` | `GET /memory/search`、`/mcp`（`memory_search`、profile 资源） |
| `memory:write` | `/mcp`（`remember`） |
| `raw:read` | `GET /archive/raw/{generation_id}`、`GET /archive/sessions/{id}/export`、`GET /archive/search`（`scope=archive`）、`session_get(view=raw)` |
| 永远都有 | `GET /auth/me`、`DELETE /devices/{自己}` |

`cormex login` 默认申请 `capture:write session:read memory:read`；`raw:read` 和 `memory:write` 要显式加 `--scopes`。

### 6.2 Principal（D10）

现状的问题（已作为单独任务提出）：`accounts::current_user` 只认 access token，不认委托令牌，回落到 1 号用户。多用户部署下，B 用户的沙箱令牌会读写 1 号用户的记忆。设备密钥如果照这个路子接进来，会有同样的问题。

改法：认证中间件在验证通过的分支里插入一个扩展

```rust
// crates/cortexd/src/principal.rs（新文件）
#[derive(Debug, Clone)]
pub enum Principal {
    /// The legacy pre-shared token: maps to user #1 by definition.
    LegacyOwner,
    /// A logged-in account (access token).
    User { user_id: String },
    /// A sandbox token, bound to one session.
    Delegated(DelegatedScope),
    /// A device key from `cormex login`.
    Device(DeviceScope),
}
impl Principal {
    pub fn user_id(&self, owner_fallback: &str) -> String;   // LegacyOwner → owner_fallback
    pub fn has_scope(&self, scope: Scope) -> bool;           // User / LegacyOwner → true
}
```

`AppState::tenant(&headers)` 改成 `AppState::tenant_of(&Principal)`；`current_user` 删除或改为读 `Principal`；MCP 的 `tenant()` 从 `Parts.extensions` 取 `Principal`。旧签名保留一个过渡期，内部转发，避免一次改动面过大。

---

## 7. 线协议

所有 DTO 在 `cortex-proto::capture`。JSON 字段 snake_case；时间 RFC 3339；id 是 ULID 字符串。

### 7.1 采集（`capture:write`）

```
GET  /capture/catalog
  200 { "schema": 1, "version": "2026-11-01", "agents": [...] }      // 即 agents.toml 的 JSON 形态

POST /capture/files
  { "agent": "claude_code", "native_session_id": "…", "parent_native_session_id": null,
    "logical_path": "projects/-home-u-proj/…jsonl", "role": "transcript", "framing": "append",
    "cwd": "/home/u/proj", "project_key": "github.com/u/proj" }
  200 { "file_id": "…", "session_id": "…",
        "generation": { "id": "…", "ordinal": 0, "end_offset": 12345 } }   // 幂等：已注册则返回当前最新一代

POST /capture/files/{file_id}/generations
  { "reason": "rewrite" | "truncate" | "snapshot" }
  200 { "id": "…", "ordinal": 3, "end_offset": 0 }

PUT  /capture/generations/{generation_id}/chunks/{offset}
  headers: Content-Type: application/octet-stream
           Content-Encoding: zstd            // 可选：客户端已压缩；服务端解压校验
           X-Content-SHA256: <逻辑字节的 sha256>
           X-Observed-At: <RFC 3339>
  body: 逻辑字节（≤ 4 MiB；单行超长时 ≤ 32 MiB）
  200 { "end_offset": 23456, "already_present": false }
  409 { "error": "offset_mismatch", "expected_offset": 20000 }
  422 { "error": "sha256_mismatch" }

POST /capture/files/{file_id}/events
  { "op": "missing" | "reappeared" | "excluded" | "included", "detail": null }
  204

POST /capture/snapshots                       // 第 4 阶段，SQLite 行等小快照的批量
  NDJSON：每行 { "file": {…同 /capture/files…}, "content_sha256": "…", "data_b64": "…", "observed_at": "…" }
  200 { "results": [ { "file_id": "…", "generation_id": "…", "status": "stored" | "unchanged" } ] }
```

服务端处理 `PUT …/chunks/{offset}` 的顺序（与 `blobs.rs` 的「先对象、后数据库行」同一个理由：反过来会留下指向不存在对象的行，且无法自愈）：

```
① 读 body → 解压（如有）→ 校验 sha256 和长度
② zstd 压缩 → put_keyed(capture_key(tenant, sha256)) 到 RustFS
③ archive_txn { append_chunk(...) }   // 连续性检查 + 幂等
```

### 7.2 浏览与原文（`session:read` / `raw:read`）

```
GET /archive/sessions?agent=&project=&device=&q=&since=&until=&at=&as_of=&cursor=&limit=
  200 { "sessions": [ { "id", "agent", "title", "project_key", "devices": [...], "first_at", "last_at",
                        "event_count", "summary": "一行摘要或 null", "sole_copy": bool } ], "next_cursor" }

GET /archive/sessions/{id}
  200 { 会话元数据, "files": [ { "id", "role", "device_id", "logical_path", "generations": [...] } ],
        "parser_version", "projection_status" }

GET /archive/sessions/{id}/events?view=digest|transcript&cursor=&limit=&at=
  200 { "events": [ TranscriptEventDto ], "next_cursor" }
      // digest：工具调用折叠成一行、工具结果截断到 2 KiB；transcript：完整（大块仍是引用）

GET /archive/raw/{generation_id}?offset=&length=&unmask=false          （raw:read）
  200 application/octet-stream（默认把遮罩区间替换成 `[masked:<rule>]`；unmask=true 时原样返回并写审计日志）

GET /archive/search?q=&scope=conversation|archive&agent=&project=&since=&until=&limit=
  200 { "hits": [ { "session_id", "event_id", "snippet", "occurred_at", "score" } ] }

GET /archive/sessions/{id}/export                                        （raw:read，第 3 阶段）
  200 application/zstd —— tar：按原始逻辑路径还原的全部文件（每个文件取最新一代）
```

`TranscriptEventDto`（ACP 对齐，D11）：

```json
{ "id": "…", "seq": 42, "kind": "tool_call", "occurred_at": "2026-10-29T08:00:00Z",
  "role": "assistant",
  "content": [ { "type": "tool_call", "tool_call_id": "toolu_…", "name": "Bash",
                 "input": { "command": "cargo test" } } ],
  "text": "Bash: cargo test",
  "source": { "generation_id": "…", "offset": 1024, "length": 512 } }
```

内容块类型：`text`、`thought`、`tool_call`、`tool_result`、`diff`、`plan`、`image`（引用 blob）、`resource_ref`（指回 R 层的大块）、`unknown`（原样保留解析器不认识的记录，不丢弃）。

### 7.3 设备

```
POST   /devices          { "label": "laptop", "scopes": [...], "platform": "macos-aarch64" }
                         200 { "id": "…", "key": "<只出现这一次>", "scopes": [...] }
GET    /devices          200 { "devices": [ { "id", "label", "scopes", "platform", "created_at", "last_seen_at", "revoked_at" } ] }
DELETE /devices/{id}     204
```

### 7.4 MCP 工具（D12，第 3 阶段，rmcp 3.x 之后）

| 工具 / 资源 | 参数 | 返回 | 作用域 |
|---|---|---|---|
| `session_search` | `query`、`agent?`、`project?`、`since?`、`until?`、`as_of?`、`scope?`（`conversation` 默认 / `archive`） | 紧凑索引：每条 id、标题、agent、项目、时间、一行摘要（约 50–100 token） | `session:read`（`archive` 需 `raw:read`） |
| `session_get` | `id`、`view`（`brief` 默认 / `digest` / `transcript` / `raw`）、`cursor?`、`max_tokens?`（默认 4000） | 经 `injection` 框定的文本块，超出预算则给续页 cursor | `session:read`（`raw` 需 `raw:read`） |
| `memory_search` / `remember` | 不变 | 不变 | `memory:read` / `memory:write` |
| 资源 `cortex://session/{id}/brief` | — | 交接简报，带 `ttlMs`、`cacheScope: private` | `session:read` |
| 资源 `cortex://project/{key}/brief` | — | 项目简报 | `session:read` |

URI scheme 沿用代码里的 `cortex://`（README 写成 `cormex://` 是文档错误，计划 P0-5 修正）。

---

## 8. 后台加工

全部沿用 `crates/cortexd/src/backfill.rs` 的模式：`spawn(st)` 起一个周期任务，每轮列出租户（`st.tenant_schemas()`），每个租户查「待办」，有界并发处理，失败记日志、下一轮重来。**以表为准，不另建作业队列**：待办永远可以从数据推出来，进程重启不丢活。

### 8.1 masker（R 层遮罩，第 2 阶段）

- 待办：每一代 `max(end_offset)` > 最新 `capture_mask_progress.through_offset`
- 处理：读取 `[through - 1 KiB, end)` 的逻辑字节（1 KiB 重叠，避免跨块漏扫），`redaction::scan_spans` → `insert_masks` + 新进度行
- `scanner` 字段记规则版本；规则升级时整库重扫，旧行保留（append-only）

### 8.2 projector（R → P，第 2 阶段）

- 待办：会话的任一文件的最新一代末尾 > 最新 `transcript_projections.through`，或 `parser_version` 升级
- 处理：取该会话全部相关代的新增逻辑字节 → `cortex_transcript::parse(agent, role, bytes, state)` → 事件序列 → 对每个文本字段做**文本级遮罩**（`redaction::mask_text`，替换为 `[已遮罩:<rule>]`）→ 大于 16 KiB 的内容块换成 `resource_ref` → 分批（≤ 500 行）`insert_transcript_events`
- 解析器有内部状态（跨块的未完成行、Copilot 变更日志的重放状态），每次从上一个进度点恢复：状态序列化在 `transcript_projections.through` 里
- 事件 id 用 `Id::derived(occurred_at, seed)`，重跑同一版本的解析器是幂等的
- 新一代（改写）：同一会话的新代按新代重新投影，旧代的事件保留；UI 默认展示最新一代

### 8.3 indexer（P → 索引，第 3 阶段）

- 会话检索（`scope=conversation`）：直接用 `transcript_events.tsv`（只含 user_message / agent_message / plan），按会话聚合打分；第 5 阶段加 `transcript_embeddings`（只给对话轮）做 RRF
- 原文档案索引（`scope=archive`）：把 `tool_call` / `tool_result` 的文本按 2 KiB 切段写进 `archive_chunks`；可按项目关闭（`archive_index_excludes`，用 `session_events` 风格的事件表记录）

### 8.4 distiller（P → D，第 5 阶段）

- **观察日志**：会话空闲 ≥ 30 分钟或结束后，取新增事件（工具结果截断到 512 字节）→ 便宜模型生成**带日期的观察条目** → 追加进 `summaries`（`scope='session'`、`scope_key=<capture_session_id>`），并写 `derivations`（`source_kind='transcript_event'`，需要扩展 CHECK 的新 migration）
- **交接简报**：`session_get(view=brief)` 第一次被请求时生成并缓存为一条 summary；会话有新事件后失效，下次请求时重新生成（新行，旧行保留）
- **晋升与抽取**（D14）：按策略选出值得抽事实的轮次（默认：用户消息 + 每轮最后一条 agent 消息；工具输出不晋升）→ 写成 `episodes`（`session_id` = capture 会话 id，`content` 里带 `transcript_event_id`，`device_id` = 设备 id）→ **复用现有抽取管线**（`/episodes` 的 anchor 机制和 `extract.rs`）。抽取前照样过 `redaction::scan`（约束：写侧负过滤在 LLM 之前）
- **成本控制**：每租户每日预算（token 数，复用 `quota.rs`）；历史回填单独排队、单独限速；`cormex init` 在回填前给出估算（沿用 `cortex-import` 默认 dry-run 的传统）

### 8.5 purge（第 6 阶段）

`redactions` 的 `target_kind` 扩展出 `capture_session` / `capture_file`（新 migration 重建 CHECK）。执行：删 RustFS 对象 → 删 P / 索引 / 派生行（purge 是全系统唯一允许 DELETE 的地方）→ 留墓碑。有了每来源数据密钥后，先销毁密钥。

---

## 9. 运维与部署

### 9.1 空库启动修复（D20，第 0 阶段）

`Store::migrate_global(database_url)` 开头先连 `public`、跑一次 `MIGRATOR`（租户那套），再跑 `GLOBAL_MIGRATOR`。理由：

- 依赖是真实存在的：全局那套用到的 `ulid` / `sha256` DOMAIN 由租户那套建在 `public`
- 一处修改覆盖全部三个调用点（`state.rs` 的 `Accounts::connect`、`main.rs` 的 `create_user`、`live.rs` 的 `Live::new`），没有「漏改一处」的可能
- 不复制 DOMAIN 定义（roadmap 方案 1 的代价），不新增编号更早的 migration（方案 3 的冲突）
- 租户 migration 本身幂等，`Live::new` 之后再跑一次是空操作

配一条测试：建一个全新数据库，只调 `migrate_global`，断言成功且 `cortex_auth.users` 存在。

### 9.2 发布（D18）

- 现有 `release.yml` 增加 `cli` job（打 `vX.Y.Z` tag 时触发，与镜像并行）：矩阵 `x86_64-unknown-linux-musl`、`aarch64-unknown-linux-musl`（`cargo-zigbuild`）、`x86_64-apple-darwin`、`aarch64-apple-darwin`、`x86_64-pc-windows-msvc`、`aarch64-pc-windows-msvc`；产物 `cormex-<version>-<target>.tar.gz|.zip` + `SHA256SUMS`，上传到同一个 GitHub Release
- `scripts/install.sh` / `scripts/install.ps1`：按平台下载、校验 SHA256、放到 `~/.local/bin` / `%LOCALAPPDATA%\cormex\bin`
- `npm/`：主包 `cormex`（`bin/cormex.js` 按平台找子包）+ 六个平台子包 `@cormex/cli-<platform>`，同一个 job 发布（需要 `NPM_TOKEN` secret；没配就跳过这一步，不让整个发布失败）
- 版本号来自 workspace（`cormex --version` = `env!("CARGO_PKG_VERSION")`），preflight 检查不变

### 9.3 配置项（服务端，新增）

| 变量 | 默认 | 说明 |
|---|---|---|
| `CORMEX_CAPTURE_CATALOG` | 内置 | 覆盖来源目录文件路径 |
| `CORMEX_CAPTURE_MAX_CHUNK` | 33554432 | 单块逻辑字节上限 |
| `CORMEX_ARCHIVE_WORKERS` | 2 | projector / masker / indexer 的每轮并发 |
| `CORMEX_DISTILL_DAILY_TOKENS` | 200000 | 每租户每日加工预算 |
| `CORMEX_DISTILL_MODEL` | 同抽取模型 | 观察日志与简报用的模型 |

### 9.4 容量估算方法

不靠人工测量：`cormex detect` 报每个 agent 的会话数和字节数，`cormex init` 在回填前给出估算（逻辑字节、预计压缩后体积、预计事件数）。存储（RustFS）按「压缩后体积 × 1.1」规划，Postgres 按「事件数 × 约 2 KiB」规划。

一个实测数据点（2026-10-29，本仓库的一个云端 Claude Code 会话，约 3 小时的持续工作）：1455 条记录、5.7 MB 逻辑字节，gzip -9 后 1.66 MB，**约 3.5 倍**。比通常对 JSONL 的预期（5–10 倍）低，原因之一是 thinking 块的签名一类字符串几乎不可压缩。规划时按 3 倍算。

---

## 10. 测试策略

| 层 | 测什么 | 怎么测 |
|---|---|---|
| 解析器 | 每个 agent 的每种记录类型；跨块的行；未知记录保留为 `unknown`；解析器版本号变化 | `crates/cortex-transcript/tests/fixtures/<agent>/*.jsonl` + 金标准 `*.expected.json`（`cargo insta` 风格的快照，或手写对比） |
| R 层往返 | **还原出的逻辑文件 sha256 = 原文件**；追加、截断、改写、zstd 表示切换 | `cormex` 的同步引擎对一个临时 home 目录跑，服务端用进程内 axum + 测试库（`#[ignore]`，`just test-db`） |
| 连续性 | 并发追加同一代；重复上传；乱序；409 后恢复 | `cortex-store` 集成测试 |
| 隔离 | 设备密钥 / 委托令牌只能落到自己的租户；作用域越界 403 | `cortexd` 测试，两个账号 |
| 不同步 | 归档表永不出现在 `sync_log`；`table::ALL ∩ table::ARCHIVE = ∅` | 单元 + 集成 |
| 遮罩 | 遮罩区间不进 P 层文本、不进任何索引；`unmask=true` 才见原文 | 集成测试，用 `redaction.rs` 已有的样本密钥 |
| hook | `cormex hook` 在 stdin 为空、坏 JSON、服务端不可达、SQLite 被锁时都在 200 ms 内退出 0 | CLI 集成测试（计时） |
| 安装器 | 合并配置不破坏用户已有条目；uninstall 还原到字节一致 | 用夹具配置文件做前后对比 |
| 检索 | 会话检索和原文档案检索的召回 | `evals/` 新题集（第 7 阶段） |

夹具来源：执行者在所在机器上用采样工具（计划 P2-0）扫描已装的 agent，做**结构保持的匿名化**（保留结构、键名、枚举值和引用关系，替换全部自由文本）；本机没有的 agent 照上游源码类型手写，标 `synthetic`。**夹具里不能有真实密钥、节点地址、域名、账号、对话正文**（公开仓库）。

---

## 11. 风险与对策

| 风险 | 对策 |
|---|---|
| 宿主格式变化（Codex 官方说 transcript 不是稳定接口） | R 层只认字节，解析坏了不丢数据；解析器带版本，修好后重投影；`doctor` 报告「某来源长期无新数据」；投影失败的会话在 UI 上显示「解析失败，原文可读」 |
| hook 拖慢或干扰宿主 | `cormex hook` 只做本地 IPC，50 ms 目标；永远退出 0；遵守各宿主的 stdout 规则；集成测试计时 |
| 并发进程损坏状态 | SQLite WAL + `BEGIN IMMEDIATE`；文件锁；服务端连续性检查 + 幂等 |
| 中央库泄露 | 设备密钥按设备吊销；`raw:read` 默认不给；P 层及以下不含密钥；第 6 阶段每来源数据密钥 |
| 存储膨胀 | 内容寻址 + zstd；按项目排除；`sole_copy` 标记帮助判断哪些必须留 |
| LLM 成本 | 加工全部异步、懒生成、有预算；默认不对工具输出调模型 |
| 归档写入与记忆写入争锁 | 归档走 `archive_txn`，不拿 sync 的 advisory lock（D23） |
| 多用户隔离 | Principal 中间件（D10）；隔离测试 |

---

## 12. 与现有约束的对照

| CLAUDE.md 约束 | 本设计 |
|---|---|
| 1. 存储三条纪律 | 归档表 append-only；同步表照旧走 `write_txn`；归档表走 `archive_txn`（D23，CLAUDE.md 同步修订）；`tsv` 同事务 |
| 2. migration 只增不改、LF | 新文件 + PINNED；`.gitattributes` 已覆盖 `*.sql` |
| 3. 两套 migration 的 search_path | 全局新表在 `cortex_auth`，用 `public` 的 DOMAIN；D20 修空库顺序 |
| 4. 注入走 `cortex_core::injection` | 会话工具的输出一律经 `render_*` 框定（D12）；需要新的渲染函数时**两个仓库一起改**并跑 `scripts/injection-drift.py` |
| 5. 不做最小公倍数供应商抽象 | 归一化只用于展示和检索，不进模型调用链 |
| 6. 委托凭据用调用者自己的 bearer | 设备密钥由用户自己的 bearer 签发，不能用设备密钥再签发（D9） |
| 这里不跑 agent 循环 | CLI 是采集器；`routes::cortexd_serves_no_agent_shaped_route` 不动 |
| 这里不知道 agent 服务在哪 | 不引入任何「agent 服务地址」配置 |

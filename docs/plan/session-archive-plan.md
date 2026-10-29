# 会话档案 —— 代码级阶段开发计划

> 日期：2026-10-29 · 设计权威：[session-archive-architecture.md](../design/session-archive-architecture.md)（下文引用为「设计 §x」「D<n>」）
>
> 这份计划写给**执行开发的 AI agent**。每张任务卡自成一体：读完 CLAUDE.md、设计文档和这张卡，就能开工。

---

## 0. 怎么用这份计划

### 0.1 阅读顺序

1. `CLAUDE.md`（不可违反的约束、代码风格、提交前检查）
2. 设计文档 §0–§1（一页纸 + 决定表）
3. 你领到的任务卡；卡里「依赖」列出的卡必须已经合并

### 0.2 一张卡 = 一个分支 = 一个 PR（人工逐张分派时）

> **一次性自主执行全部开发时，改用 §12 的规则**（一个集成分支、一卡一提交、每阶段一个 PR）。

- 分支名 `feat/<卡号小写>-<短名>`，例如 `feat/p1-3-archive-txn`
- 只做卡片范围内的事。发现范围外的问题：写进 PR 描述的「顺带发现」一节，**不要顺手改**
- 设计与现实冲突（比如上游格式和设计假设不符）：先在 PR 里写明证据和建议的修改，由所有者决定是否改设计；**不要让代码悄悄偏离设计**

### 0.3 通用完成标准（每张卡都要满足，卡里不再重复）

- [ ] `cargo fmt --all` 无改动
- [ ] `cargo clippy --workspace --all-targets --locked -- -D warnings` 通过
- [ ] `cargo test --workspace --locked` 通过；动到数据库的卡还要 `just test-db`（它带 `--include-ignored`；**「跳过」在输出里和「通过」长得一样**，要看到测试真的跑了）
- [ ] 动到 `app/` 的卡：`cd app && flutter analyze && flutter test`
- [ ] 新 migration：新文件、LF 行尾、sha256 加进 `crates/cortex-store/tests/migrations_are_immutable.rs` 的 `PINNED`；**已有 migration 一个字节都不改**
- [ ] crate 里的注释和 rustdoc 用**英文**，解释**为什么**（参考 `crates/cortex-core/src/id.rs` 的密度）；`docs/` 用中文；`app/` 新写的注释用英文
- [ ] 测试的断言消息能独立读懂失败原因
- [ ] 不出现密钥、节点地址、域名、账号（公开仓库）；夹具里的「密钥」只能用 `redaction.rs` 测试里那种明显假的样本；**来自真实会话的夹具必须经过 P2-0 的工具**，不得手工复制粘贴真实会话片段
- [ ] 遇到 dead_code 警告，优先让代码真的被用起来，不加 `#[allow]`
- [ ] 卡片里的「验收」逐条在 PR 描述里打勾，并贴出关键命令的输出

### 0.4 全局禁止

- 在客户端（`crates/cormex`）解析会话**内容**（D2）
- 归档表进 `sync_log`，或者用 `write_txn` 写归档表（D3、D23）
- hook 里联网，或 `cormex hook` 以非 0 退出（D1）
- 读项目目录里的文件内容（D7）
- 在 cortexd 里加任何 agent 循环、`/chat`、`/sandbox/*`（CLAUDE.md；`routes::cortexd_serves_no_agent_shaped_route` 不许改绿）
- 依赖、复制、vendor `coding_agent_session_search`（cass）的代码（许可证 rider，见调研报告 §3.1）

---

## 1. 阶段总览

```
阶段 0 前置修复 ──┬─▶ 阶段 1 采集骨架 ──▶ 阶段 2 解析与浏览 ──▶ 阶段 3 取回 ──▶ 阶段 5 加工
（可全部并行）    │        │                    │                    │
                  │        └──────────▶ 阶段 4 铺开适配器（每个 agent 一张卡，可并行）
                  └─▶ 阶段 6 安全与运维（P6-1/P6-2 依赖阶段 2；其余可提前）
                                                       阶段 7 评测（随时可开始，越早越好）
```

| 阶段 | 目标 | 出口标准 |
|---|---|---|
| **0** | 修掉会挡路的现存问题 | 空库能起；委托令牌不再串租户；CLI 能不带 goose 编译 `cortex-proto` |
| **1** | 两台机器上 Claude Code 和 Codex 的会话**字节级**进中央库 | 端到端测试：还原出的逻辑文件 sha256 与原文件一致；hooks 和定时任务能装能卸 |
| **2** | 原文能像对话一样浏览 | Web 上能看到两种 agent 的会话；每条消息能切到原始记录；密钥默认遮罩 |
| **3** | 任何 agent 能取回 | `session_search` / `session_get` 可用；`cormex restore` 后 `claude --resume` 能接着聊 |
| **4** | 覆盖主流 agent | Gemini CLI、Cursor、VS Code Copilot、Cline / Roo、OpenCode、Goose、Copilot CLI 各有夹具和解析器 |
| **5** | 精简、摘要、抽取 | 会话有观察日志和交接简报；按策略抽出的事实能被 `memory_search` 找到；成本可预估 |
| **6** | 安全与运维 | 每来源数据密钥；purge 覆盖到 R 层；备份恢复演练；设备码登录；npm 分发 |
| **7** | 评测 | 采集完整性测试进 CI；会话检索有基线数字 |

体量标记：**S** ≈ 一个 agent 一次会话能完成；**M** ≈ 两三次；**L** ≈ 需要拆成子 PR。

---

## 2. 阶段 0：前置修复

### P0-1 空库启动修复（D20）· S

- **依赖**：无
- **目标**：全新数据库上 cortexd 能起来（roadmap A 的 ❌）
- **改动**：
  - `crates/cortex-store/src/store.rs`：`Store::migrate_global` 在 `CREATE SCHEMA IF NOT EXISTS cortex_auth` 之后、跑 `GLOBAL_MIGRATOR` 之前，用同一个 `boot` 连接对 `public` 跑一次 `MIGRATOR.run(&boot)`。`boot` 用默认 `search_path`；为防止库里恰好有与登录用户同名的 schema，`boot` 连接显式设置 `search_path=public`（和第二个连接设置 `cortex_auth,public` 的方式一样，用 `PgConnectOptions::options`）
  - 函数的 rustdoc 写清：为什么全局那套依赖租户那套（DOMAIN 在 `public`）；为什么在这里修而不是在三个调用点调序（一处覆盖全部，没有「漏一处」）
  - `README.md`：删掉「Known gap」那一段，quickstart 不变
  - `docs/roadmap.md`：A 节 ❌ 改 ✅，写一句修法
- **测试**：`crates/cortex-store/tests/fresh_database.rs`（`#[ignore]`）：用 `DATABASE_URL` 的管理连接 `CREATE DATABASE cormex_fresh_<ulid小写>`；对新库调 `Store::migrate_global`；断言 `to_regclass('cortex_auth.users')` 与 `to_regclass('public.episodes')` 都非空；最后 `DROP DATABASE`（失败也要尝试清理）
- **验收**：新测试在修改前失败（`type "ulid" does not exist`）、修改后通过；`just test-db` 全绿

### P0-2 认证主体一次解析（Principal，D10）· M

- **依赖**：无（如果所有者已经启动了「Fix delegated tokens resolving to user #1's tenant」那张建议任务，本卡与它合并，不要重复做）
- **背景**：`crates/cortexd/src/accounts.rs::current_user` 只认 access token，其余回落到 1 号用户，所以 B 用户的委托令牌会落进 1 号用户的租户。`/auth/ticket` 签出的 ticket 也不绑定用户（`TicketBook::issue()` 只返回字符串），同样会回落到 1 号用户
- **改动**：
  - 新文件 `crates/cortexd/src/principal.rs`：
    ```rust
    #[derive(Debug, Clone)]
    pub enum Principal {
        LegacyOwner,                 // pre-shared token
        User { user_id: String },    // access token
        Delegated(DelegatedScope),   // sandbox token
        // Device(DeviceScope) is added by P1-1
    }
    impl Principal {
        pub async fn user_id(&self, st: &AppState) -> Option<String>; // LegacyOwner -> owner_user_id()
    }
    ```
  - `crates/cortexd/src/auth.rs::require`：每个验证成功的分支 `req.extensions_mut().insert(principal)`；ticket 分支插入 ticket 绑定的主体
  - `crates/cortexd/src/auth.rs::TicketBook`：`issue(principal: Principal) -> String`，`valid(&ticket) -> Option<Principal>`；`routes.rs::issue_ticket` 传入当前请求的 `Principal`
  - `crates/cortexd/src/request_tenant.rs`：新增 `AppState::tenant_of(&Principal) -> Result<Tenant, ApiError>`；`tenant(&headers)` 暂时保留，内部改为从 `Principal` 求值（handler 拿不到扩展时退回旧逻辑并 `tracing::warn!`，便于发现漏网的调用点）
  - `crates/cortexd/src/accounts.rs::current_user`：改为读 `Principal`
  - `crates/cortexd/src/mcp.rs::tenant`：从 `Parts.extensions` 取 `Principal`
  - 所有 `st.tenant(&headers)` 的 handler 改为 `Extension(principal): Extension<Principal>` + `st.tenant_of(&principal)`
- **测试**：在 `crates/cortexd` 加 `#[ignore]` 集成测试 `tenant_isolation_by_principal`：建两个账号 A（1 号）、B；B 登录 → `POST /delegated-tokens` → 用委托令牌 `POST /episodes` → 断言行在 B 的 schema、A 的 schema 里没有；`/auth/me` 用委托令牌返回 B；ticket 同样验证
- **验收**：新测试在旧代码上失败、新代码上通过；预共享 token 仍然映射到 1 号用户；现有路由测试全绿

### P0-3 `as_of` 回放忽略 revoke · S

- **依赖**：无（所有者若已启动「Fix as_of replay ignoring fact revoke events」建议任务，以那张为准）
- **改动**：`crates/cortex-store/src/recall.rs` 的 `as_of_snapshot!`：在 `created_at <= $1` 的状态事件（`invalidate` / `revoke`）中，按 `(created_at DESC, id DESC)` 取最后一条，它不是 `invalidate` 才算有效（与 `fact_status` 视图同一排序）
- **测试**：`crates/cortex-store/tests/replay.rs` 增加「失效后又恢复」：失效前、失效与恢复之间、恢复之后三个时刻
- **验收**：新测试旧代码失败、新代码通过

### P0-4 `cortex-proto` 的 `llm` feature（D21）· S

- **依赖**：无
- **改动**：
  - `crates/cortex-proto/Cargo.toml`：`[features] default = ["llm"]`，`llm = ["dep:cortex-llm"]`；`cortex-llm` 改为 `optional = true`。顺手看 `tokio` 是否只被 `llm.rs` 用到，是的话也挂到 `llm` feature 下
  - `crates/cortex-proto/src/lib.rs`：`#[cfg(feature = "llm")] pub mod llm;`
  - `.github/workflows/ci.yml` 的 lint job：加一步 `cargo check -p cortex-proto --no-default-features --locked`
- **验收**：`cargo tree -p cortex-proto --no-default-features` 里没有 `goose-providers`；CI 新步骤通过

### P0-5 文档纠错 · S

- **依赖**：无
- **改动**：
  - `README.md`：`cormex://profile` 改为 `cortex://profile`（以 `crates/cortexd/src/mcp.rs` 的 `PROFILE_URI` 为准）；「Three things make that possible, and no other memory layer does all three」改为如实的说法（双时间轴不再独有；独有的是注入回放、确定性失效、信任分层的出处。依据见调研报告 §6.1）
  - `CLAUDE.md`「差异点在哪」一节：同样改掉「9 个同类项目没有一个做双时间轴回放」
- **验收**：措辞与调研报告 §6.1 一致；没有改动任何代码

---

## 3. 阶段 1：采集骨架

### P1-1 设备密钥（D9）· M

- **依赖**：P0-2
- **改动**：
  - 新 migration `migrations-global/20261101000001_device_keys.sql`（DDL 见设计 §4.5），加进 `PINNED`
  - 新文件 `crates/cortex-proto/src/devices.rs`：`NewDeviceRequest { label, scopes: Vec<String>, platform: Option<String> }`、`DeviceKeyIssued { id, key, scopes }`、`DeviceDto`、`DevicesResponse`；在 `lib.rs` 里 `pub mod devices;`
  - 新文件 `crates/cortexd/src/device_keys.rs`：
    ```rust
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum Scope { CaptureWrite, SessionRead, MemoryRead, MemoryWrite, RawRead }
    impl Scope { pub fn as_wire(self) -> &'static str; pub fn from_wire(s: &str) -> Option<Self>; }

    #[derive(Debug, Clone)]
    pub struct DeviceScope { pub user_id: String, pub device_id: String, pub scopes: Vec<Scope> }
    impl DeviceScope {
        /// Route allow-list, same philosophy as DelegatedScope::allows (see its docs).
        pub fn allows(&self, method: &Method, path: &str) -> bool;
        pub fn has(&self, s: Scope) -> bool;
    }

    pub struct DeviceKeys { pool: PgPool, cache: Mutex<HashMap<String /*sha256*/, (DeviceScope, Instant)>> }
    impl DeviceKeys {
        pub async fn resolve(&self, token: &str) -> Option<DeviceScope>;            // 60 s cache
        pub async fn issue(&self, user_id: &str, req: &NewDeviceRequest) -> Result<DeviceKeyIssued, ApiError>;
        pub async fn list(&self, user_id: &str) -> Result<Vec<DeviceDto>, ApiError>;
        pub async fn revoke(&self, user_id: &str, device_id: &str) -> Result<(), ApiError>;
        async fn touch(&self, device_id: &str);                                      // last_seen_at, ≤ once / 10 min
    }
    ```
    明文密钥的生成复用 `auth::generate()`；摘要用 SHA-256
  - `crates/cortexd/src/state.rs`：`Runtime` 加 `device_keys: Option<Arc<DeviceKeys>>`（有账号库时创建）
  - `crates/cortexd/src/principal.rs`：加 `Principal::Device(DeviceScope)`；`has_scope`
  - `crates/cortexd/src/auth.rs::require`：委托令牌分支之后加设备密钥分支：`resolve` → `allows` → 插入 `Principal::Device`；不允许的路由回 403（文案格式同委托令牌）
  - `crates/cortexd/src/routes.rs` 的 `protected_routes!`：`"/devices" [GET, POST]`、`"/devices/{id}" [DELETE]`；`POST /devices` 的 handler 拒绝 `Principal::Device` 和 `Principal::Delegated`（403：「a device key cannot mint another device key」）
- **测试**：
  - 单元：`allows` 的允许名单逐条断言（照 `delegated_token.rs::the_allowlist_is_exactly_what_the_agent_actually_calls` 的写法），包括「`raw:read` 没给时 `/archive/raw/*` 403」
  - 集成（`#[ignore]`）：登录 → 签发 → 用设备密钥 `GET /auth/me` 得到自己 → 吊销 → 60 秒缓存过期后 401（测试里把缓存 TTL 做成可注入）；设备密钥签发设备密钥 → 403；B 的设备密钥只能落到 B 的租户
- **验收**：上述测试通过；`every_protected_route_rejects_anonymous_requests` 自动覆盖新路由

### P1-2 归档表 migration（R 层）· S

- **依赖**：无
- **改动**：新文件 `migrations/20261101000001_capture_archive.sql`，内容 = 设计 §4.1 的 DDL，每张表配 `COMMENT ON TABLE` 写清用途与「append-only、不进 sync_log」；加进 `PINNED`
- **测试**：`just test-db` 下 migration 能在现有测试库与空库上运行（P0-1 的测试会覆盖空库）
- **验收**：`sqlx migrate run --source migrations` 成功；PINNED 测试通过

### P1-3 `ArchiveTxn` 与归档模型（D23）· M

- **依赖**：P1-2
- **改动**：
  - 新目录 `crates/cortex-store/src/archive/`：
    - `mod.rs`：`Store::archive_txn`（开事务，**不拿** advisory lock，**不写** sync_log）、`ArchiveTxn`（原始事务封在私有模块里，照 `txn.rs` 的 `guarded` 写法）
    - `model.rs`：`NewCaptureSession`、`CaptureSession`、`NewCaptureFile`、`CaptureFile`、`FileRole`、`Framing`、`GenerationReason`、`Generation`、`NewCaptureChunk`、`CaptureChunk`、`AppendOutcome`、`FileEventOp`、`NewCaptureFileEvent`（字段与 DDL 一一对应；枚举带 `as_str` / `FromStr`，未知值返回 `StoreError::UnknownVariant`）
    - `query.rs`：`impl Store` 的只读方法：`capture_file(device_id, agent, logical_path)`、`latest_generation(file_id) -> Option<(Generation, i64 /*end_offset*/)>`、`chunks_overlapping(generation_id, offset, len) -> Vec<CaptureChunk>`、`generation_end(generation_id) -> i64`
  - `ArchiveTxn` 方法（签名见设计 §4.4）。`register_capture_file` 一次完成：会话 `INSERT … ON CONFLICT (agent, native_session_id) DO NOTHING` 后查回 id；文件同理；文件没有任何一代时建第 0 代（`reason='initial'`）；返回文件、会话 id 与最新一代及其末尾
  - `append_chunk`：
    ```sql
    SELECT pg_advisory_xact_lock(4273, hashtext($1));
    SELECT coalesce(max(byte_offset + byte_length), 0) FROM capture_chunks WHERE generation_id = $1;
    -- offset == end  → INSERT, Appended
    -- offset <  end  → 查 (generation_id, offset) 处的块：sha256 与长度都相同 → AlreadyPresent；否则 OffsetMismatch{expected: end}
    -- offset >  end  → OffsetMismatch{expected: end}
    ```
    常量 `ARCHIVE_GENERATION_LOCK_CLASS: i32 = 4273`，注释说明与 4272 的区别
  - `crates/cortex-store/src/model.rs`：`table` 模块加 `ARCHIVE: &[&str]`（`capture_sessions`、`capture_files`、`capture_generations`、`capture_chunks`、`capture_file_events`、`capture_masks`、`capture_mask_progress`；阶段 2 再追加）
  - `crates/cortex-store/src/lib.rs`：导出新类型
  - `CLAUDE.md` 约束 1：「写事务必须走 `store.write_txn`」改为「**同步表**必须走 `write_txn`；**归档表**（`table::ARCHIVE`）必须走 `archive_txn`；两者都不允许裸事务」，并写一句为什么归档表不需要 advisory lock
- **测试**（`crates/cortex-store/tests/archive.rs`，`#[ignore]`）：
  - `register_is_idempotent`：同一文件注册两次，id 相同，只有一代
  - `append_is_contiguous`：0 → 100 → 200 成功；跳到 300 返回 `OffsetMismatch{expected:200}`
  - `append_replay_is_idempotent`：同 offset 同 sha 返回 `AlreadyPresent`；同 offset 不同 sha 返回 `OffsetMismatch`
  - `concurrent_appends_stay_contiguous`：8 个任务各自按「读末尾 → 追加」循环写 50 块，最后检查区间连续无空洞、无重叠
  - `archive_rows_never_enter_sync_log`：执行全部 ArchiveTxn 方法后 `SELECT count(*) FROM sync_log WHERE table_name = ANY($ARCHIVE)` 为 0
  - 单元：`table::ALL` 与 `table::ARCHIVE` 不相交
- **验收**：上述测试通过；`ArchiveTxn` 没有任何读方法以外的公开逃生口

### P1-4 对象存储的 keyed 接口（D16）· S

- **依赖**：无
- **改动**：
  - `crates/cortex-blob/src/hash.rs`：`pub fn capture_key(tenant: &TenantPrefix, sha256: &str) -> Result<String>` → `"{tenant}/capture/{aa}/{bb}/{sha256}.zst"`；`pub fn validate_capture_key(key: &str) -> Result<()>`（正则 `^[a-z0-9_]{1,63}/capture/[0-9a-f]{2}/[0-9a-f]{2}/[0-9a-f]{64}\.zst$`，并校验两级目录与哈希前缀一致）
  - `crates/cortex-blob/src/lib.rs` 的 `BlobStore` trait：`put_keyed(&self, key: &str, bytes: Bytes, mime: &str) -> Result<()>`、`get_keyed(&self, key: &str) -> Result<Bytes>`、`exists_keyed`、`delete_keyed`。**key 是桶内的完整 key，实现不得再拼接构造时配置的租户前缀**（rustdoc 写清原因：`MediaStore::current_tenant()` 是写死 `public` 的过渡做法）
  - `crates/cortex-blob/src/s3.rs`、`local.rs`：实现上述方法；本地实现照样「临时文件 + rename」
  - `crates/cortexd/src/blobs.rs` 的 `MediaStore`：透传这四个方法
- **测试**：`capture_key_layout_is_stable`（照 `storage_key_layout_is_stable`）；路径穿越样例被拒；本地后端往返；`tests/rustfs.rs` 加一条 `#[ignore]` 往返
- **验收**：测试通过；原有媒体 key 布局测试不变

### P1-5 线协议与来源目录（cortex-proto）· M

- **依赖**：P0-4
- **改动**：
  - 新文件 `crates/cortex-proto/src/capture.rs`：设计 §7.1 的全部 DTO（`RegisterFileRequest`、`RegisterFileResponse`、`GenerationDto`、`NewGenerationRequest`、`ChunkAck`、`OffsetMismatchBody`、`FileEventRequest`、`FileRole`、`Framing`、`GenerationReason`），以及请求头常量 `HEADER_CONTENT_SHA256 = "x-content-sha256"`、`HEADER_OBSERVED_AT = "x-observed-at"`、`MAX_CHUNK_BYTES: usize = 4 << 20`
  - 新文件 `crates/cortex-proto/src/catalog.rs`：`Catalog { schema, version, agents }`、`AgentEntry { id, name, home: Vec<HomeRoot>, sources: Vec<SourceSpec>, hooks: Option<HookSpec>, retention: Option<RetentionSpec> }`、`HomeRoot { env | path }`、`SourceSpec { role, glob, framing, also_compressed, session_id: SessionIdRule, parent_session_id: Option<SessionIdRule>, cwd: Option<CwdRule> }`、`SessionIdRule { Stem | StemRegex{regex} | PathSegment{index, prefix} | Const{value, per_device} }`（语义见设计 §5.2 末尾）；`Catalog::builtin() -> &'static Catalog`（`LazyLock` + `include_str!("../catalog/agents.toml")`）
  - 新文件 `crates/cortex-proto/catalog/agents.toml`：先只放 `claude_code` 和 `codex`（内容见设计 §5.2）
  - `crates/cortex-proto/Cargo.toml`：加 `toml`（只用于解析内置目录）
- **测试**：内置目录能解析；Claude Code 的 memory 伪会话按项目区分、Codex 的按设备区分（各有一个用例）；agent id 唯一且满足 `^[a-z][a-z0-9_]{1,31}$`（与 DDL 的 CHECK 一致）；所有正则能编译；DTO serde 往返；`OffsetMismatchBody` 的 JSON 形状快照
- **验收**：`cargo check -p cortex-proto --no-default-features` 通过

### P1-6 服务端采集接口 · M

- **依赖**：P1-1、P1-3、P1-4、P1-5
- **改动**：
  - 新文件 `crates/cortexd/src/capture.rs`：handlers `get_catalog`、`register_file`、`open_generation`、`put_chunk`、`file_event`
    - 只接受 `Principal::Device` 且带 `capture:write`；其他主体 403（「capture requires a device key: run `cormex login`」）
    - `put_chunk`：解析 `{generation_id}`、`{offset}`；读 body（`DefaultBodyLimit::max(CORMEX_CAPTURE_MAX_CHUNK + 64 KiB)`）；`Content-Encoding: zstd` 时用**有上限**的解码（输出超过上限即 413，防解压炸弹）；校验 `X-Content-SHA256`（不符 422）；按「① 对象 ② 行」顺序写（设计 §7.1）；`OffsetMismatch` → 409 + `OffsetMismatchBody`
    - 检查 `generation_id` 属于这个设备的文件（防止拿别的设备的代写入）
  - `crates/cortexd/src/state.rs`：`AppState` 加 `capture_*` 方法，`Backend::Mock` 返回 `Unavailable`（与现有方法一致）；`Backend::Live` 转到新文件 `crates/cortexd/src/live_archive.rs` 里的 `impl BoundLive`（`live.rs` 已有 2500 行，新逻辑不往里堆）
  - `crates/cortexd/src/routes.rs` 的 `protected_routes!`：`"/capture/catalog" [GET]`、`"/capture/files" [POST]`、`"/capture/files/{id}/generations" [POST]`、`"/capture/generations/{id}/chunks/{offset}" [PUT]`、`"/capture/files/{id}/events" [POST]`
  - 环境变量 `CORMEX_CAPTURE_CATALOG`、`CORMEX_CAPTURE_MAX_CHUNK`（`crates/cortex-core/src/config.rs`；启动时打印状态行）
- **测试**（`#[ignore]`，进程内 axum + 测试库 + 本地对象存储）：注册 → 追加两块 → 重放同一块 → 错位 409 → 改写开新代 → sha 不符 422 → zstd 编码 → 无 `capture:write` 403 → 别的设备的代 403；对象在 `capture/` 命名空间下
- **验收**：测试通过；`/capture/*` 出现在受保护路由清单里

### P1-7 CLI 骨架 · M

- **依赖**：P0-4、P1-1、P1-5
- **改动**：
  - 新 crate `crates/cormex/`（workspace member），同时是 lib + bin（`src/lib.rs` 暴露引擎给端到端测试，`src/main.rs` 只做参数解析和退出码映射）
  - `Cargo.toml` 依赖：`cortex-proto = { workspace = true, default-features = false }`、`clap`、`tokio`（`rt-multi-thread`、`macros`、`fs`、`time`、`process`）、`reqwest`、`serde`、`serde_json`、`toml`、`rusqlite = { version = "0.41", features = ["bundled"] }`、`zstd = "0.14"`、`sha2`、`keyring = "4"`、`directories = "6"`、`fs4 = "1"`、`rpassword`、`chrono`、`anyhow`、`tracing`、`tracing-subscriber`、`globset`、`walkdir`
  - 模块：
    - `src/config.rs`：`Config { server_url, device_label, schedule_minutes, exclusions: Vec<Exclusion>, git_metadata: bool }`，读写 `<config_dir>/config.toml`
    - `src/credentials.rs`：`trait CredentialStore { get, set, delete }`；`KeyringStore`（服务名 `cormex`，账号 = 服务端 URL）与 `FileStore`（`<config_dir>/credentials`，Unix 下 0600）；优先钥匙串，失败退回文件；`CORMEX_DEVICE_KEY` 环境变量优先于两者
    - `src/client.rs`：`ApiClient { base, http, key }`；`get_json` / `post_json` / `put_bytes`；错误映射到 `ExitCode`（设计 §5.1）；超时 30 秒，连接超时 5 秒
    - `src/state.rs`：打开 `<data_dir>/state.db`，`PRAGMA journal_mode=WAL; busy_timeout=5000`，用 `user_version` 做本地迁移；表结构见设计 §5.4
    - `src/cmd/login.rs`、`logout.rs`、`status.rs`
  - `cormex --version` 输出 workspace 版本
- **测试**：配置往返；`state.db` 从空到最新版本的迁移；`FileStore` 的权限（Unix）；`login` 用假服务端（`axum` 测试路由）走一遍密码 → 设备密钥
- **验收**：`cargo tree -p cormex` 里没有 `sqlx`、`goose-providers`、`cortex-store`、`cortex-llm`

### P1-8 CLI 发现（catalog 展开 + `detect`）· M

- **依赖**：P1-7
- **改动**：`src/catalog.rs`：
  - `fn resolve_home(entry: &AgentEntry) -> Option<PathBuf>`：依次试环境变量、路径（`~` 用 `directories::BaseDirs` 展开）
  - `fn discover(catalog: &Catalog, cfg: &Config) -> Vec<DiscoveredFile>`：`DiscoveredFile { agent, role, framing, logical_path /*相对 home，正斜杠*/, physical_path, compressed: bool, native_session_id, parent_native_session_id, cwd }`
  - `also_compressed`：同名 `.jsonl` 与 `.jsonl.zst` 只保留一个（明文优先，对齐 Codex 的规则），`logical_path` 一律是明文名
  - `cwd` 的 `claude_project_dir` 解码是**有损**的（Claude Code 把 `/` 换成 `-`），只作为 hook 没给 cwd 时的兜底，结果标记 `cwd_guessed = true`
  - 排除规则：按项目（cwd 前缀）或会话 id
  - `src/git_meta.rs`：`fn project_key(cwd) -> Option<String>`：只执行 `git -C <cwd> remote get-url origin`（规范化成 `host/owner/repo`）和 `git rev-parse HEAD`，超时 1 秒；`git_metadata = false` 时完全不调用（D7）
  - `src/cmd/detect.rs`：表格和 `--json` 两种输出：每个 agent 的 home、会话数、文件数、总字节
- **测试**：用 `tempfile` 造一个假 home（Claude Code 两个项目、一个子 agent、一个 `tool-results` 目录；Codex 三个 rollout，其中一个只有 `.zst`）；断言发现结果、会话 id、明文与压缩去重、排除规则
- **验收**：`detect` 不打开任何会话文件读内容（用一个计数的文件系统抽象或在测试里断言只调用了 `metadata`）

### P1-9 CLI 同步引擎 · L（建议拆成 a / b）

- **依赖**：P1-6、P1-8
- **改动**（`src/sync/`）：
  - `reader.rs`：`LogicalReader`：普通文件直接读；只有 `.zst` 时流式解压；提供 `len()`、`read_range(offset, len)`、`tail_hash(offset)`（`[offset-4096, offset)` 的 sha256）
  - `plan.rs`：纯函数 `fn decide(local: Option<&FileState>, observed: &Observed) -> Action`，`Action = Register | Append{from} | NewGeneration{reason} | Snapshot | Skip`（规则见设计 §5.3）。**这是最需要单测的地方**
  - `api.rs`：`#[async_trait] trait CaptureApi { register, open_generation, put_chunk, file_event, catalog }`；`HttpCaptureApi`（用 `ApiClient`）与测试用的 `FakeCaptureApi`（内存里模拟服务端的连续性语义）
  - `upload.rs`：按 4 MiB 切块；可选 zstd（`Content-Encoding`）；200 / `AlreadyPresent` → 在同一个 SQLite 事务里推进 `uploaded_offset` 与 `tail_sha256`；409 → 以 `expected_offset` 为准（大于本地值：服务端已有，跳过；小于本地值：本地状态领先，回退重传）；网络错误 → 退避 1 s / 2 s / 4 s，三次后留在队列
  - `lock.rs`：每个逻辑文件一把 `fs4` 锁（`<data_dir>/locks/<sha256(local_key)>.lock`），`try_lock` 失败跳过
  - `mod.rs`：`pub async fn run(opts: SyncOptions) -> SyncReport`；`SyncOptions { trigger, only: Option<PathBuf>, from_queue: bool, budget: Budget{secs:60, bytes:512 MiB}, dry_run }`；运行结束写 `runs` 表
  - `src/cmd/sync.rs`：`--once`、`--queue`、`--source`、`--dry-run`
- **测试**（全部用 `FakeCaptureApi` + 临时目录）：首次上传；追加；截断 → 新代；改写（同长度不同内容）→ 新代；明文变 `.zst` → 不重传；`.zst` 解压回明文并追加 → 只传新增；最后一行写到一半；409 两个方向的恢复；网络错误后重启续传；并发两个 `run` 不重复上传（锁）；预算耗尽留到下一次
- **验收**：上述测试通过；`plan.rs` 的 `decide` 覆盖率接近 100%（每条规则至少一个用例）

### P1-10 hook 处理器与安装器（Claude Code、Codex）+ 定时任务 · M

- **依赖**：P1-9
- **改动**：
  - `src/hook.rs`：`pub fn run_hook(agent: &str, stdin: impl Read) -> !`：读最多 1 MiB / 200 ms；取 `transcript_path`、`agent_transcript_path`；写 `queue`（`busy_timeout` 100 ms，失败写 `<data_dir>/spool/<ulid>.json`）；起后台 `cormex sync --queue`（Unix：`std::process::Command` + `pre_exec(libc::setsid)`，标准流全部 `null`；Windows：`creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)`）；按 `HookSpec` 决定 stdout（`{}` 或不输出）；**任何错误都退出 0**
  - `src/install/mod.rs`：`trait Installer { fn plan(&self) -> Vec<Change>; fn apply(&self) -> Result<()>; fn uninstall(&self) -> Result<()>; }`；`Change` 用于 `--dry-run` 展示
  - `src/install/backup.rs`：改文件前备份为 `<file>.cormex-backup-<时间>`；写临时文件 + 原子改名；登记到 `installs` 表
  - `src/install/claude_code.rs`：合并 `~/.claude/settings.json` 的 `hooks`：对 `Stop`、`SubagentStop`、`PreCompact`、`SessionEnd` 各加一个 `{ "matcher": "", "hooks": [ { "type": "command", "command": "<cormex 绝对路径> hook claude_code", "timeout": 5 } ] }`；已有含 `cormex hook` 的条目就不再加；**保留用户已有的一切内容**（用 `serde_json::Value` 操作，`preserve_order` feature 保持键序）
  - `src/install/codex.rs`：同理写 `~/.codex/hooks.json`（结构以 Codex 官方 hooks 文档为准；实现前把文档里的示例存成夹具 `tests/fixtures/codex_hooks_example.json`，用它做合并测试）
  - `src/install/retention.rs`：`~/.claude/settings.json` 的 `cleanupPeriodDays` 小于 3650 时提议改为 3650
  - `src/install/scheduler.rs`：Linux 优先 systemd `--user`（`~/.config/systemd/user/cormex-sync.{service,timer}`，`systemctl --user enable --now`），否则 crontab 加一行（带 `# cormex` 标记）；macOS `~/Library/LaunchAgents/ai.cormex.sync.plist` + `launchctl bootstrap gui/<uid>`；Windows `schtasks /Create /SC MINUTE /MO 20 /TN cormex-sync`
- **测试**：合并不破坏已有条目（夹具：带别的 hooks、带注释外的未知字段）；重复安装幂等；卸载后与安装前**字节一致**；`run_hook` 在 stdin 为空、坏 JSON、服务端不可达、`state.db` 被另一个进程独占锁住时都在 200 ms 内返回 0（测试里计时）
- **验收**：上述测试通过；Windows 上 hook 路径的引号与空格处理有用例

### P1-11 `init` / `doctor` / `uninstall` · M

- **依赖**：P1-10
- **改动**：`src/cmd/init.rs`、`doctor.rs`、`uninstall.rs`
  - `init`：未登录先 `login` → `detect` → 列出将做的改动（hooks、定时任务、`cleanupPeriodDays`、MCP 注册在阶段 3 加）→ 逐项确认（`--yes` 全部接受）→ 回填估算（文件数、逻辑字节数）→ 确认后回填（复用 `sync::run`，预算放开）
  - `doctor`：检查服务端可达、凭据有效与存储方式、每个已装 hook 的配置仍在、`runs` 里最近一次成功、队列与 spool 积压、长期（7 天）无新数据的来源、`cleanupPeriodDays`；`--json` 输出；每个问题带一行修复建议
  - `uninstall`：按 `installs` 表逆序还原；`--keep-data` 保留 `state.db`
- **测试**：用假 home 跑 `init --yes --dry-run` 的输出快照；`uninstall` 后所有被改文件与备份一致
- **验收**：在一台干净的 Linux 容器里按 README 步骤从零跑通（PR 里贴命令输出）

### P1-12 端到端测试 · M

- **依赖**：P1-6、P1-9
- **改动**：新的测试专用 crate `crates/e2e-capture/`（`publish = false`，workspace member），依赖 `cortexd`（作为库，必要时给 cortexd 加 `lib.rs` 暴露 `router(state)`）和 `cormex`
  - `tests/roundtrip.rs`（`#[ignore]`）：进程内起 cortexd（测试库 + 本地对象存储）→ 用 store 直接签发两把设备密钥（两台「设备」）→ 两个临时 home，各放 Claude Code 与 Codex 的夹具 → 跑 `sync::run` → **从服务端按代拼接分块并解压，sha256 与原文件逐个比较** → 追加几行 → 再跑 → 再比 → 改写一个文件 → 再跑 → 断言出现新一代且旧代完好
- **验收**：测试通过，并接进 `just test-db`

### P1-13 CLI 发布 · M

- **依赖**：P1-11
- **改动**：
  - `.github/workflows/release.yml`：新 job `cli`（`needs: preflight`），矩阵见设计 §9.2；Linux 用 `cargo-zigbuild`；产物打包 + `SHA256SUMS`；`softprops/action-gh-release` 上传到同一个 Release
  - `scripts/install.sh`、`scripts/install.ps1`：探测平台 → 下载 → 校验 → 安装到用户目录 → 打印下一步（`cormex init`）
  - `README.md` 新增一节「Capture your agent sessions」（英文）：安装、`cormex init`、隐私边界（读什么、不读什么）、怎么卸载
- **验收**：在 fork 上打一个测试 tag 跑通 release（或 `workflow_dispatch` 演练），三平台产物齐全

---

## 4. 阶段 2：解析与浏览

### P2-0 本机采样与结构保持脱敏（夹具来源）· M

- **依赖**：P1-8（复用 catalog 发现）
- **为什么**：解析器要用真实会话当夹具，但仓库是公开的。会话文件里有对话正文、读过的文件内容、命令输出，还会有 system prompt 和身份信息（Claude Code 官方文档写明 transcript 会包含发给模型的 system prompt 与工具定义）。**正文没法靠「抹掉密钥」脱敏干净**，所以这里只保留结构、替换全部自由文本
- **怎么取样**：**直接扫描执行者所在机器上装了的 agent**（所有者 2026-10-29 的决定），不需要所有者提供样本
- **改动**：
  - 建 `crates/cortex-transcript/` 的骨架（`Cargo.toml` + 空 `lib.rs`），P2-1 在其上继续
  - `crates/cortex-transcript/examples/sample_fixtures.rs`（dev 工具，`cargo run -p cortex-transcript --example sample_fixtures -- --out <dir>`）；dev-dependencies：`cormex`（发现）、`cortex-memory`（`redaction`）、`cortex-proto`（catalog）
  - 选样：每个 agent 最多 3 个会话：最小的一个、中位数大小的一个、记录类型最多样的一个；每个文件截到 2000 行或 2 MiB（按行边界）
  - **结构保持的匿名化**（规则版本号写进输出）：
    - 保留：JSON 结构、全部键名、布尔、数字、null；白名单键的值（按 agent 配置：`type`、`role`、`subtype`、工具名 `name`、`stop_reason`、`media_type`、`operation`、`mode`、`level` 等枚举类字段）
    - 时间戳：整体平移一个常量（保留先后顺序和格式）
    - UUID / id：**一致映射**（同一输入映射到同一个假 id），保住 `parentUuid ↔ uuid`、`tool_use_id ↔ tool_result` 这类引用
    - 路径：home 目录、用户名、项目名换成一致的占位（`/home/user/project-a`）
    - 其余所有字符串：换成长度相近、文字种类相同（ASCII / 中文 / 混合）的合成文本，保留换行数
    - 长的 base64 / 签名类字符串（thinking 签名、内联图片）：换成同长度级别的占位；图片块换成一张合法的 1×1 PNG，保证解码路径能测
  - **安全网**（任一命中则工具失败退出，不写文件）：输出过 `redaction::scan`；再扫一遍本机的 hostname、用户名、邮箱样式、IPv4、`/home/<真实用户名>` 样式
  - 输出 `crates/cortex-transcript/tests/fixtures/<agent>/<名字>.jsonl` 与同目录的 `MANIFEST.md`（来源 agent 版本、记录类型统计、匿名化规则版本、`real-derived` / `synthetic` 标记）
- **本机没有装的 agent**：按上游开源的类型定义手写夹具（Codex：`openai/codex` 的 `codex-rs/protocol`；Gemini CLI：`packages/core/src/services/chatRecordingTypes.ts`；OpenCode、Goose 同理），在 `MANIFEST.md` 里标 `synthetic`。云端会话通常**只有 Claude Code**（参考：2026-10-29 在本仓库的云端会话里扫描，只找到 `~/.claude/projects` 下当前会话自己的 transcript）。所有者之后在自己的机器上跑一次这个工具，就能把 `synthetic` 换成 `real-derived`
- **测试**：匿名化函数的单元测试（id 一致映射、引用不断、白名单值保留、自由文本被替换、安全网能拦住植入的假密钥和假邮箱）
- **验收**：PR 附 `MANIFEST.md`；PR 描述里写明执行者已人工过目输出、确认没有残留的私人内容

### P2-1 `cortex-transcript` crate 与两个解析器 · L

- **依赖**：P1-5、P2-0（夹具）
- **改动**：
  - `crates/cortex-proto/src/transcript.rs`：`TranscriptKind`、`ContentBlock`（`Text{text}`、`Thought{text}`、`ToolCall{tool_call_id,name,input}`、`ToolResult{tool_call_id,output,is_error}`、`Diff{path,old,new}`、`Plan{entries}`、`Image{blob_ref}`、`ResourceRef{generation_id,offset,length,mime}`、`Unknown{raw_type}`）、`TranscriptEventDto`（设计 §7.2）
  - 新 crate `crates/cortex-transcript/`：
    ```rust
    pub struct ParsedEvent {
        pub kind: TranscriptKind, pub role: Option<String>, pub occurred_at: Option<DateTime<Utc>>,
        pub content: Vec<ContentBlock>, pub text: String,
        pub tool_name: Option<String>, pub tool_call_id: Option<String>,
        pub src_offset: u64, pub src_length: u64,
    }
    pub trait Parser: Send {
        fn version(&self) -> &'static str;                         // e.g. "claude_code@1"
        /// Feed the next contiguous logical bytes starting at `offset`. Returns complete events.
        fn feed(&mut self, offset: u64, bytes: &[u8]) -> Vec<ParsedEvent>;
        fn checkpoint(&self) -> serde_json::Value;                 // resumable state
        fn restore(state: &serde_json::Value) -> Self where Self: Sized;
    }
    pub fn parser_for(agent: &str, role: FileRole) -> Option<Box<dyn Parser>>;
    ```
  - `src/jsonl.rs`：行切分（跨 `feed` 调用的半行缓冲）、坏行 → `Unknown` 事件（**不丢弃**，保留偏移）
  - **非消息记录占多数**：2026-10-29 在一个真实的 Claude Code 会话（1455 行）里数到的记录类型有 `assistant` 487、`attachment` 375、`user` 287、`last-prompt` 103、`atis-latch` 102、`mode` 62、`tools-used` 16、`queue-operation` 14、`system` 6、`cost-state` 3。只认 `user` / `assistant` 会把一半以上的记录当成坏行，所以每种不认识的 `type` 都要落成 `Meta` 或 `Unknown`，并在夹具里各有一例
  - `src/claude_code.rs`：逐行按 `type` 分派（`user` / `assistant` / `system` / `summary` / 其他）；`message.content` 可能是字符串或块数组；`tool_use` → `ToolCall`；`tool_result`（在 user 消息里）→ `ToolResult`，并用 `tool_use_id → name` 映射补上工具名（映射放进 checkpoint）；`thinking` → `Thought`；不认识的 `type` → `Unknown`
  - `src/codex.rs`：`RolloutLine { timestamp, type, payload }`；`session_meta` / `turn_context` → `Meta`；`response_item` 的 `message`（`input_text` / `output_text`）、`reasoning`、`function_call`、`function_call_output`、`local_shell_call` 等 → 对应块；`event_msg` 默认 `Meta`（不重复展示）；`compacted` → `System`。字段以上游 `openai/codex` 的 `codex-rs/protocol` 类型定义为准（Apache-2.0；若照抄定义需在 NOTICE 署名）
  - **独立实现**：不得参考或复制 cass / franken_agent_detection 的代码（前者许可证 rider；后者为保持边界干净也不抄）
- **测试**：`tests/fixtures/{claude_code,codex}/*.jsonl` + `*.expected.json`（事件列表）；**切块不变性**：把同一输入在每个可能的位置切成两段喂进去，结果与一次喂完相同；checkpoint / restore 后续喂结果相同；坏行变 `Unknown`
- **验收**：测试通过；每种已知记录类型至少一个夹具

### P2-2 P 层 migration 与存储方法 · M

- **依赖**：P1-3、P2-1
- **改动**：
  - 新 migration `migrations/20261115000001_transcript_projection.sql`：设计 §4.2 的 `transcript_events`、`transcript_projections`、`archive_chunks`，外加审计表：
    ```sql
    CREATE TABLE archive_access_log (
        id ulid PRIMARY KEY, principal TEXT NOT NULL, generation_id ulid NOT NULL REFERENCES capture_generations(id),
        byte_offset BIGINT NOT NULL, byte_length BIGINT NOT NULL, unmasked BOOLEAN NOT NULL,
        created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp());
    ```
  - `table::ARCHIVE` 追加四张表
  - `ArchiveTxn`：`insert_masks`、`insert_transcript_events`（`tsv` 由调用方传入 jieba 分词结果，与主行同一条 INSERT；照 `insert_episode` 的 `to_tsvector('simple', $n)` 写法）、`insert_archive_chunks`、`insert_access_log`
  - `query.rs`：`transcript_page(session_id, parser_version, after_seq, limit)`、`latest_projection(session_id, parser_version)`、`mask_progress(generation_id, scanner)`、`masks_in_range(generation_id, offset, len)`
- **测试**：插入与分页；`tsv` 非空；重复插入同一 `(session_id, parser_version, seq)` 报唯一约束（projector 靠派生 id + `ON CONFLICT DO NOTHING` 实现幂等，这里测约束本身）
- **验收**：PINNED 更新；测试通过

### P2-3 密钥区间扫描 · S

- **依赖**：无
- **改动**：`crates/cortex-memory/src/redaction.rs`：
  - 把现有每条规则的正则源码提成 `const PATTERN_*: &str`，`regex::Regex` 与新的 `regex::bytes::Regex` **从同一个常量编译**（单一来源）
  - `pub fn scan_spans(text: &str) -> Vec<(Range<usize>, Rule)>`：全部规则、全部命中、按起点排序、重叠合并
  - `pub fn scan_spans_bytes(bytes: &[u8]) -> Vec<(Range<usize>, Rule)>`
  - `pub fn mask_text(text: &str) -> Cow<'_, str>`：命中区间替换为 `[masked:<rule>]`
  - 现有 `scan` 行为不变
- **测试**：每条规则的正例与反例；`scan_spans` 与 `scan_spans_bytes` 在 UTF-8 输入上结果一致；`mask_text` 之后 `scan` 返回 `None`
- **验收**：原有 redaction 测试全部不变且通过

### P2-4 后台 worker：masker 与 projector · L

- **依赖**：P2-2、P2-3
- **改动**：新文件 `crates/cortexd/src/archive_workers.rs`：
  - `pub fn spawn(st: AppState)`：照 `backfill.rs` 的结构（`tokio::time::interval`，每轮 `st.tenant_schemas()`，每租户有界并发 `CORMEX_ARCHIVE_WORKERS`）；在 `state.rs` 里和 `backfill::spawn` 并列启动
  - `async fn mask_tick(st, tenant) -> Result<usize>`：待办查询见设计 §8.1；读块：`get_keyed` → zstd 解压 → 拼出 `[through - 1024, end)` → `scan_spans_bytes` → `insert_masks`（scanner = `redaction@<版本常量>`）
  - `async fn project_tick(st, tenant) -> Result<usize>`：待办见设计 §8.2；按会话处理：恢复 checkpoint → 逐代逐块 `feed` → 每个事件的文本字段过 `mask_text` → 大于 16 KiB 的块换成 `ResourceRef` → `Id::derived(occurred_at 或 块的 created_at, seed)` → 每 500 行一个 `archive_txn`（`ON CONFLICT (id) DO NOTHING`）→ 写进度
  - 一个会话解析失败：记 `status='failed'` + 错误摘要，继续下一个；不重试风暴（同一个错误 1 小时内不重试）
  - 两个 tick 函数 `pub(crate)`，测试可以直接调用
- **测试**（`#[ignore]`）：通过 `ArchiveTxn` 直接灌夹具分块 → 调 `mask_tick`、`project_tick` → 断言事件数、顺序、遮罩（夹具里放一个假密钥，断言 P 层文本里是 `[masked:…]`，`capture_masks` 里有区间）；再追加一块 → 再 tick → 只多出新事件；重复 tick 幂等
- **验收**：测试通过；worker 在 mock 后端下不启动

### P2-5 浏览与原文 API · M

- **依赖**：P2-4
- **改动**：新文件 `crates/cortexd/src/archive_api.rs`：
  - `GET /archive/sessions`、`/archive/sessions/{id}`、`/archive/sessions/{id}/events`、`/archive/raw/{generation_id}`（设计 §7.2）
  - 会话标题：在有摘要（阶段 5）之前取第一条 `user_message` 的前 80 个字符
  - `sole_copy`：该会话所有文件在所有设备上的最新 `capture_file_events.op` 都是 `missing`
  - `view=digest`：工具调用折叠成一行（`<tool>: <摘要>`）、工具结果截到 2 KiB
  - `/archive/raw`：要求 `raw:read`（`User` / `LegacyOwner` 天然具备）；默认把 `masks_in_range` 的区间替换为 `[masked:<rule>]`；`unmask=true` 原样返回；两种情况都写 `archive_access_log`
  - DTO 放 `crates/cortex-proto/src/archive.rs`
  - 路由进 `protected_routes!`；`DeviceScope::allows` 加上这些路由（P1-1 的名单测试同步更新）
- **测试**：列表过滤（agent / 时间）；分页游标；`raw` 的遮罩与 `unmask`；无 `raw:read` 的设备 403；访问日志写入
- **验收**：测试通过

### P2-6 Web 会话视图 · L

- **依赖**：P2-5
- **改动**（`app/`）：
  - `lib/models/archive_session.dart`、`transcript_event.dart`（含 `ContentBlock` 的解析）
  - `lib/api/cormex_api.dart`：`listArchiveSessions`、`archiveSession`、`sessionEvents`、`rawSlice({unmask})`
  - `lib/state/sessions_controller.dart`（Riverpod）
  - `lib/features/sessions/sessions_page.dart`（按 agent / 设备 / 项目筛选，`sole_copy` 标记）、`session_detail_page.dart`（虚拟列表，滚动到底自动翻页）、`widgets/event_tile.dart`（消息、思考折叠、工具调用折叠、diff）、`widgets/raw_view.dart`（等宽显示原始记录，「解除遮罩」按钮二次确认）
  - 导航里加入口
- **测试**：模型解析测试（每种内容块）；`event_tile` 的 widget 测试；`sessions_page` 用假 API 的 widget 测试
- **验收**：`flutter analyze && flutter test` 通过；PR 附截图

### P2-7 CLI `sessions ls / show` · S

- **依赖**：P2-5
- **改动**：`crates/cormex/src/cmd/sessions.rs`：`ls`（表格 / `--json`）、`show <id> [--view digest|transcript] [--raw]`（文本渲染：`[时间] 角色: 内容`）
- **测试**：用假服务端的输出快照
- **验收**：在端到端环境里能列出并显示 P1-12 灌进去的会话

---

## 5. 阶段 3：取回

### P3-1 rmcp 3.x + goose-providers 升级（D19）· L

- **依赖**：无（但必须早于 P3-4）
- **改动**：
  - workspace `Cargo.toml`：`goose-providers` 升到 `0.1.0-alpha.13`（它要求 `rmcp ^3.4.1`）；`crates/cortexd/Cargo.toml`：`rmcp = "3.8"`，`schemars` 跟随 rmcp 3.x 实际使用的版本（看它的 `Cargo.toml`，**必须是同一份**，原因见现有注释）
  - `cargo tree -d | grep rmcp` 必须只有一份
  - 修 `crates/cortex-llm` 与 `crates/cortexd/src/mcp.rs` 的编译错误：`StreamableHttpServerConfig::stateful_mode` → `legacy_session_mode`（保持为 true，兼容旧客户端）；资源读取结果加 `ttlMs` / `cacheScope`（profile 资源 `private`、TTL 5 分钟）；`ProtocolVersion::LATEST` 的语义变化
  - 注意 CLAUDE.md「依赖上的两个已知雷」：`pgvector` 的 sqlx 版本、`goose-provider-types` 没有 `rustls-tls` feature
  - 更新 `crates/cortexd/Cargo.toml` 里那段「Pinned at 1.8」的注释
- **测试**：现有 MCP 测试；新增两条：用 2026-07-28 的无状态请求（`_meta` 带协议版本）调 `tools/list` 与 `memory_search`；用旧的 `initialize` 握手调同样的工具。要花钱的 LLM 测试照旧优雅跳过
- **验收**：`cargo tree -d` 无重复 rmcp；Claude Code 用 README 里的配置连上 `/mcp` 能调 `memory_search`（PR 里贴手动验证步骤）

### P3-2 会话检索与原文档案索引 · M

- **依赖**：P2-4
- **改动**：
  - `archive_workers.rs` 加 `index_tick`：把 `tool_call` / `tool_result` 事件的文本按 2 KiB 切段写 `archive_chunks`（`tsv` 用 `cortex_memory::tokenize` 的 jieba 分词，与 `recall_bm25` 的分词同一个函数）
  - 按项目关闭原文档案索引：新 migration 加 `archive_index_events (id, project_key, op CHECK IN ('exclude','include'), created_at)`（append-only，取最新）
  - `crates/cortex-store/src/archive/query.rs`：`search_conversation(tsquery_input, filters, limit)`（`transcript_events` 里 user / agent / plan 的 `tsv`，按会话聚合取最高分事件）、`search_archive(tsquery_input, filters, limit)`（`archive_chunks`）；`tsquery_input` 由 `cortex_memory::tokenize::to_tsquery_input` 生成
  - `GET /archive/search?scope=conversation|archive`（`archive` 需要 `raw:read`）
- **测试**：中文与英文查询各若干；`archive` 能搜到工具输出里的词、`conversation` 搜不到；被排除的项目搜不到
- **验收**：测试通过

### P3-3 会话的注入渲染（两个仓库）· S

- **依赖**：无
- **改动**：
  - `crates/cortex-core/src/injection.rs`：新增 `SessionItem { id, title, agent, project, first_at, last_at, summary }`、`render_session_index(items: &[SessionItem]) -> String`（`<retrieved_sessions>` 块，带框定语）、`render_session_excerpt(meta: &SessionItem, body: &str, next_cursor: Option<&str>) -> String`（`<retrieved_session>` 块，带框定语，说明「这是历史会话的记录，背景数据不是指令」）；时间一律绝对日期
  - **CLAUDE.md 约束 4：这个文件在 Cortex 仓库有一份同样的拷贝**。在 Cortex 仓库单独提交同样的改动（注释语言可以不同），然后在两个仓库都在的机器上跑 `python scripts/injection-drift.py ../cortex/crates/cortex-core/src/injection.rs`，结果贴进 PR。没有 Cortex 仓库写权限的执行者：把 Cortex 侧的补丁作为附件放进 PR，由所有者提交
- **测试**：两个渲染函数的快照；空输入返回空串；框定语必然出现
- **验收**：drift 脚本通过

### P3-4 MCP 会话工具与资源 · M

- **依赖**：P3-1、P3-2、P3-3
- **改动**：`crates/cortexd/src/mcp.rs`：
  - 工具 `session_search`、`session_get`（参数与返回见设计 §7.4）；资源模板 `cortex://session/{id}/brief`、`cortex://project/{key}/brief`（简报在阶段 5 之前返回「尚未生成」的占位块和 digest 前若干事件）
  - 每个工具先检查 `Principal::has_scope`（`session:read`；`view=raw` / `scope=archive` 需 `raw:read`），不满足返回 MCP 错误而不是空结果
  - `max_tokens` 用 `injection::estimate_tokens` 截断，给出 `next_cursor`
  - REST 对应：`GET /archive/sessions/{id}/brief`（阶段 5 前同样返回占位）
  - 工具描述（英文）写清三层用法：先 `session_search`，再 `session_get(view=brief)`，确有需要才 `digest` / `transcript` / `raw`
- **测试**：两个工具的输出包含框定语；无权限的设备密钥被拒；`max_tokens` 截断与续页
- **验收**：测试通过；Claude Code 连上后能用 `session_search` 找到 P1-12 灌进去的会话（PR 里贴手动步骤）

### P3-5 导出与 `cormex restore` · M

- **依赖**：P2-5
- **改动**：
  - 服务端 `GET /archive/sessions/{id}/export`（`raw:read`）：tar（`tar` crate）+ zstd；每个文件取最新一代拼接还原，路径 = `logical_path`；写访问日志
  - CLI `src/cmd/restore.rs`：`cormex sessions restore <id> [--to <agent home>] [--cwd <新项目路径>] [--force]`
    - Claude Code：写到 `<home>/projects/<按新 cwd 编码的目录>/<session>.jsonl`（编码规则与 Claude Code 一致：路径分隔符换成 `-`），子 agent 与 sidecar 跟着放
    - Codex：按原 `sessions/YYYY/MM/DD/` 路径放回
    - 目标已存在且未加 `--force` 时拒绝；写临时文件再改名
    - 会话文件里记录的旧 cwd 不改写（还原出来的是派生物，但我们尽量保持原样；需要改写时再加选项）
- **测试**：导出 → 还原 → 文件 sha256 与原始一致；已存在时拒绝
- **验收**：在一台机器上采集一个 Claude Code 会话，在另一台（或另一个 home）还原后 `claude --resume <id>` 能看到历史（手动验证，PR 里写步骤和结果；若 Claude Code 因 cwd 不一致拒绝，记录现象并在设计文档 §5 补充限制）

### P3-6 `cormex mcp` stdio 桥 · S

- **依赖**：P3-1
- **改动**：`src/cmd/mcp.rs`：从 stdin 逐行读 JSON-RPC，POST 到 `<server>/mcp`（带设备密钥；按 2026-07-28 要求带 `Mcp-Method` / `Mcp-Name` 头，从请求体的 `method` / `params.name` 取值）；把响应（JSON 或 SSE 里的 data 帧）写回 stdout；服务端若返回 `Mcp-Session-Id`（旧协议），后续请求带上
- **测试**：用假服务端回放一段 `tools/list` → `tools/call` 交互
- **验收**：只支持 stdio 的宿主（例如在 Codex 的 `config.toml` 里配置 `command = "cormex", args = ["mcp"]`）能调到 `memory_search`

### P3-7 MCP 注册、Agent Skill 与文档 · S

- **依赖**：P3-4、P3-6
- **改动**：
  - `cormex init` 增加一项「把 Cormex 的 MCP 注册进各 agent」：Claude Code 用 `claude mcp add --transport http cormex <url>/mcp --header "Authorization: Bearer …"`（存在 `claude` 命令时）或写配置；Codex 写 `config.toml` 的 `[mcp_servers.cormex]`（stdio 走 `cormex mcp`）
  - `skills/cormex/SKILL.md`：什么时候用 `session_search` / `session_get`、默认先拿 brief、不要把 raw 贴进对话除非用户要求、`as_of` 与 `at` 的区别
  - `README.md`：MCP 接入片段（roadmap B 的欠账）
- **验收**：Skill 文件通过 `npx skills add weironz/cormex` 能装上（或在 PR 里说明验证方式）

---

## 6. 阶段 4：铺开适配器

每个 agent 一张卡，可并行。每张卡的固定内容：

1. `crates/cortex-proto/catalog/agents.toml` 增加条目（home、sources、hooks、retention）
2. `crates/cortex-transcript/src/<agent>.rs` 解析器 + 夹具 + 切块不变性测试
3. 若宿主有 hooks：`crates/cormex/src/install/<agent>.rs` + 合并 / 卸载测试
4. `cormex detect` 的夹具 home 增加该 agent
5. 端到端测试（P1-12）增加该 agent 的往返用例

| 卡 | agent | 采集要点 | 体量 |
|---|---|---|---|
| **P4-0** | （基础设施） | `POST /capture/snapshots` 批量接口（设计 §7.1）+ CLI 的 SQLite 行来源读取器（只读 URI `file:<path>?mode=ro`，`busy_timeout`，catalog 里给查询语句与主键列）+ 每行一个 `role=row` 的逻辑文件 | M |
| P4-1 | Gemini CLI | `~/.gemini/tmp/*/chats/session-*.jsonl`（旧 `.json` 走 snapshot）；子 agent 在父会话 id 目录下；hooks 写 `~/.gemini/settings.json`，stdout 输出 `{}`；整体改名覆盖靠换代检测 | M |
| P4-2 | Cursor | hooks 的 `transcript_path`（及 `agent_transcript_path`）按 append 采；历史数据从 `state.vscdb` 的 `cursorDiskKV` 按行（`composerData:%`、`bubbleId:%`）采，依赖 P4-0；hooks 写 `~/.cursor/hooks.json`，stdout `{}` | L |
| P4-3 | VS Code Copilot Chat | `workspaceStorage/*/chatSessions/*.jsonl`（对象变更日志：kind 0 快照、1 设值、2 数组追加、3 删除；解析器需要重放出会话对象再出事件，checkpoint 里存重放后的对象）；`.json` 走 snapshot；无 hooks，靠定时任务 | L |
| P4-4 | Cline / Roo Code | VS Code `globalStorage/{saoudrizwan.claude-dev,rooveterinaryinc.roo-cline}/tasks/*/{api_conversation_history,ui_messages}.json`，snapshot | M |
| P4-5 | OpenCode、Goose | SQLite 行（OpenCode 的 `opencode.db`，Goose 的 `sessions.db`），依赖 P4-0；各自的表结构以上游源码为准 | M |
| P4-6 | Copilot CLI、Qwen Code、Aider | `~/.copilot/session-state/*/events.jsonl`（append）；`~/.qwen/tmp/*/chats/`（同 Gemini 系）；`.aider.chat.history.md` 在**项目目录里**，违反 D7，**默认不采**，只在用户 `cormex sources add` 显式指定时采 | M |
| P4-7 | 导出文件 | `cormex import <ChatGPT/Claude 导出>`：作为 `agent = chatgpt_export / claude_export` 的 snapshot 来源进 R 层，解析器复用 `cortex-import` 里的解析逻辑（把它挪成可被 `cortex-transcript` 调用的纯函数，避免两份解析器） | M |

---

## 7. 阶段 5：加工

### P5-1 派生边扩展 · S

- **依赖**：P2-2
- **改动**：新 migration：`derivations` 的 `source_kind` CHECK 增加 `'transcript_event'`（照 `20260812000003_projects.sql` 里 `DROP CONSTRAINT` + `ADD CONSTRAINT` 的写法）；`cortex-store` 的 `SourceKind` 加变体，**先加 Rust 变体再写数据**（memory-content §二⑤ 讲过顺序反了会让整表读取失败）
- **验收**：PINNED 更新；旧数据读取不受影响

### P5-2 观察日志（distiller）· M

- **依赖**：P5-1、P2-4
- **改动**：`archive_workers.rs` 加 `distill_tick`：会话空闲 ≥ 30 分钟后，取上次之后的新事件（工具结果截到 512 字节）→ 便宜模型（`CORMEX_DISTILL_MODEL`）→ 带日期的观察条目 → `write_txn` 写 `summaries`（`scope='session'`，`scope_key=<capture_session_id>`）+ `derivations`（这两张是**同步表**，走 `write_txn`）
  - 提示词放 `crates/cortex-memory/src/distill.rs`，风格参照 `extract.rs`（提示词字符串是行为，改动要跑 evals）；模型调用在事务外（约束 1）
  - 调用前对输入做 `redaction::scan`（写侧负过滤在 LLM 之前）
  - 预算：`quota.rs` 记账（`kind = "distill"`），每租户每日超过 `CORMEX_DISTILL_DAILY_TOKENS` 即停
- **测试**：用假 LLM 客户端测「什么时候触发、输入怎么截断、输出怎么落表、超预算停」
- **验收**：测试通过；真实模型的冒烟测试在没有 key 时优雅跳过

### P5-3 交接简报 · M

- **依赖**：P5-2、P3-4
- **改动**：`session_get(view=brief)` 与 `GET /archive/sessions/{id}/brief`：有新于最后事件的简报就返回；否则同步生成（带超时，超时返回 digest 并后台继续生成）→ 存为 summary → 用 `render_session_excerpt` 框定。简报结构：目标、约束、已做决定（带日期）、当前状态、下一步、未决问题、关键文件与命令，每条带指回事件的出处
- **验收**：简报有出处；会话有新事件后再次请求会生成新的一条（旧的保留）

### P5-4 按策略晋升与事实抽取（D14）· M

- **依赖**：P5-2
- **改动**：`archive_workers.rs` 加 `promote_tick`：按策略（默认：每个用户消息 + 每轮最后一条 agent 消息；工具输出不晋升；可在配置里关）把事件写成 `episodes`（`session_id` = capture 会话 id，`content` 带 `transcript_event_id`，`device_id` = 设备 id），然后走现有抽取路径（与 `POST /episodes` 带 anchor 时的流程一致，复用 `extract_gate` 并发门）；`source_channel` 用 `conversation`
  - 历史回填的晋升单独限速；`cormex init` 的回填估算里加上「预计抽取调用次数」
- **测试**：晋升幂等（同一事件不会晋升两次：`episodes.id` 用 `Id::derived`）；抽取出的事实能被 `memory_search` 找到（用假抽取器）
- **验收**：测试通过

### P5-5 事件时间轴 `at`（D13）· S

- **依赖**：P2-5
- **改动**：`/archive/sessions`、`/archive/sessions/{id}/events`、`session_search` 支持 `at`（按 `transcript_events.occurred_at`）；与 `as_of`（按 `capture_chunks.created_at` / `facts.created_at`）同时出现时两者都生效；工具描述写清区别
- **验收**：回填的历史会话用 `at` 能查到、用 `as_of` 查不到（这是对的），测试覆盖

### P5-6 对话轮向量（可选）· M

- **依赖**：P3-2
- **改动**：新 migration `transcript_embeddings (event_id, embedding VECTOR(1024), embedding_model)`（只给 user / agent 消息）；`index_tick` 生成；`search_conversation` 用 RRF 融合 BM25 与向量（复用 `cortex_memory::fusion`）
- **验收**：evals（P7-2）上有可对照的数字证明值得开；否则保持关闭

---

## 8. 阶段 6：安全与运维

| 卡 | 内容 | 依赖 | 体量 |
|---|---|---|---|
| **P6-1** | 每来源数据密钥：新 migration `capture_keys (id, file_id, wrapped_key BYTEA, created_at)`；分块用 AES-256-GCM 加密后再存（`ring`，同 `byo_key.rs`），数据密钥用 `CORTEX_SECRET_KEY` 包裹；新分块的 `storage_key` 带 key id（`{tenant}/capture/{key_id}/aa/bb/{sha}.zst.enc`），旧分块不迁移；没配 `CORTEX_SECRET_KEY` 时保持明文并在启动日志和 `doctor` 里如实说明。文档写清限制：**旧的数据库备份里仍有被包裹的密钥**，备份过期之前 crypto-shredding 不完整 | P1-6 | M |
| **P6-2** | purge 覆盖 R 层：`redactions.target_kind` 增加 `capture_session` / `capture_file`（新 migration 重建 CHECK）；执行顺序：销毁数据密钥（有的话）→ 删 RustFS 对象 → 删 P 层、索引、晋升出的 episodes 及其派生（走现有 redact 级联）→ 留墓碑；UI 与 CLI 都要二次确认 | P6-1、P2-4 | M |
| **P6-3** | 备份恢复演练：`docs/operations.md`（新）写「pg_dump + RustFS 同步 → 空库恢复 → 对上记录数 → 重放删除（purge 墓碑在恢复后重新执行）」，并做一次真实演练（roadmap C） | P6-2 | S |
| **P6-4** | 设备码登录：`POST /devices/authorize`（匿名，返回 `user_code` + `device_code`，10 分钟有效，内存表）→ Web「设备」页输入短码并确认 → `POST /devices/token` 轮询拿到设备密钥；Web「设备」页同时提供列表、吊销、手动生成密钥 | P1-1、P2-6 | M |
| **P6-5** | npm 分发：`npm/cormex`（主包，`bin/cormex.js` 选平台子包）+ `npm/cli-<platform>` 子包；release 的 `cli` job 在有 `NPM_TOKEN` 时发布，没有就跳过并在日志里说明 | P1-13 | S |

---

## 9. 阶段 7：评测

| 卡 | 内容 | 依赖 | 体量 |
|---|---|---|---|
| **P7-1** | 不需要数据库的采集测试进 CI：`cortex-transcript` 的夹具与切块不变性、`cormex` 的 `decide` 与安装器测试；`cargo run -p cortex-evals -- validate` 也接进 CI（roadmap C） | P2-1 | S |
| **P7-2** | 会话检索评测集：`evals/suites/sessions-zh-en-v1.json`，取材于脱敏后的真实会话，覆盖「只有原文档案能找到」「只有对话能找到」「应当找不到」；`cortex-evals` 增加 `--target sessions` | P3-2 | M |
| **P7-3** | LongMemEval-S 检索召回（R@k），报告里**写清口径**（召回不是 QA 准确率），不与别家 QA 分数并列 | P3-2 | M |

---

## 10. 开工前的决定（2026-10-29 已全部拍板）

所有者把剩下的问题交给设计方决定，结论如下。执行者**不必再询问**。

| # | 问题 | 决定 |
|---|---|---|
| O1 | 真实会话样本从哪来 | 执行者用 P2-0 的工具扫描所在机器自动取样并做结构保持匿名化；本机没装的 agent 按上游开源类型手写，标 `synthetic` |
| O2 | 容量 | 不人工测量；`cormex detect` / `cormex init` 自动报告和估算；规划按压缩 3 倍算（设计 §9.4） |
| O3 | 是否多用户部署 | **按「是」处理**：P0-2 是整个计划的**第一张卡**，当作安全修复。查不到部署情况时，按最坏情况处理是对的 |
| O4 | `NPM_TOKEN` | 现在没有。P6-5 照样实现，发布步骤在没有 token 时跳过并在日志里说明 |
| O5 | 晋升策略默认值 | 确认默认「每个用户消息 + 每轮最后一条 agent 消息；工具输出不晋升」；做成配置 `CORMEX_PROMOTE_POLICY = user_and_final（默认）\| user_only \| off` |
| O6 | 对话轮向量（P5-6） | 实现，但默认关闭（`CORMEX_SESSION_VECTORS=off`）；P7-2 的评测证明有提升才在文档里建议打开 |
| O7 | `restore` 是否改写会话里的旧 cwd | 不改写；`claude --resume` 的实际表现记进验收记录（§12.6） |
| O8 | Cortex 仓库里那份 `injection.rs`（P3-3） | 执行者对 `weironz/cortex` 开一个独立分支和 PR，只改 `injection.rs`，**不合并**；拿不到写权限就把补丁放到 `docs/plan/patches/cortex-injection-session.patch` |
| O9 | 平台相关的安装器（launchd / schtasks / systemd） | P1-13 在 CI 里加 `macos-latest`、`windows-latest` 两个 job 跑 `cargo test -p cormex`；本地只能测 Linux 的部分 |
| O10 | CLI 发布流水线怎么验证 | P1-13 让 `cli` 矩阵在 PR 上也构建（只构建、不发布）；真正发布只在打 tag 时，**tag 只由所有者打** |

## 11. 交接提示词模板

把下面这段连同卡号发给执行的 agent：

```text
你在 weironz/cormex 仓库工作。

先读：
1. CLAUDE.md（约束与代码风格，必须遵守）
2. docs/design/session-archive-architecture.md 的 §0、§1，以及任务卡里引用到的章节
3. docs/plan/session-archive-plan.md 的 §0（通用完成标准与全局禁止）和任务卡 <卡号>

只做这张卡的范围。依赖的卡（见卡片「依赖」）应该已经合并；如果没有，停下来告诉我。
分支名 feat/<卡号小写>-<短名>。完成后开 PR，描述里逐条勾选卡片的「验收」，贴出关键命令输出，
并单列「顺带发现」和「与设计不一致之处」。不要修改设计文档里已拍板的决定（D1–D23）；
认为需要改的，在 PR 里写明证据和建议。
```

---

## 12. 一次性自主执行全部开发的规则

所有者要求一个 agent 设定目标、一次性完成 P0-1 … P7-3。下面的规则**覆盖** §0.2（其余 §0 照旧有效）。

### 12.1 环境准备（每个新会话开始时检查一遍）

云端开发环境的实测（2026-10-29）：有 Rust 工具链、Postgres 16 和 `psql`、Docker CLI（**没有** Docker daemon）、Node（可以 `npm install -g` 装 Codex / Gemini CLI，但没有它们的登录凭据）、`claude` CLI；**没有** pgvector、`just`、`sqlx-cli`、Flutter。

```bash
# 1. pgvector：优先用包，不行就从源码编
sudo apt-get update && sudo apt-get install -y postgresql-16-pgvector \
  || { sudo apt-get install -y postgresql-server-dev-16 build-essential git && \
       git clone --depth 1 --branch v0.8.7 https://github.com/pgvector/pgvector /tmp/pgvector && \
       make -C /tmp/pgvector && sudo make -C /tmp/pgvector install; }

# 2. 起库、建用户和测试库
sudo pg_ctlcluster 16 main start
sudo -u postgres psql -c "CREATE USER cormex WITH SUPERUSER PASSWORD 'cormex';"
sudo -u postgres psql -c "CREATE DATABASE cormex_test OWNER cormex;"
export DATABASE_URL=postgres://cormex:cormex@127.0.0.1:5432/cormex_test

# 3. 工具
cargo install just --locked
cargo install sqlx-cli --version 0.9.0 --no-default-features --features rustls,postgres --locked

# 4. 迁移（与 CI 的顺序和 search_path 完全一致）
psql "$DATABASE_URL" -c 'CREATE SCHEMA IF NOT EXISTS cortex_auth'
sqlx migrate run --source migrations
sqlx migrate run --source migrations-global \
  --database-url "$DATABASE_URL?options=-csearch_path%3Dcortex_auth%2Cpublic"
```

- **这套步骤 2026-10-29 在云端环境实测过**：Ubuntu 24.04 的 `postgresql-16-pgvector` 包是 0.6.0，现有全部 migration（含 HNSW + `vector_cosine_ops`）按 CI 的顺序在 Postgres 16 上都能跑通。CI 和生产用的是 `pgvector/pgvector:pg17`（更新的 pgvector）；如果某张卡用到 0.6 没有的特性（`halfvec`、`sparsevec`、二值量化），改走上面的源码编译分支
- `just test-db` 依赖 `docker compose`，在没有 daemon 的环境里不能用；改为直接运行 `cargo test --workspace --locked -- --include-ignored`（`DATABASE_URL` 已设置）。**要在输出里看到带数据库的测试真的跑了**，而不是全部 `ignored`
- Flutter：尝试装 CI 用的版本（`3.44.1`，见 `.github/workflows/ci.yml`）；装不上就依赖 CI 的 `app` job，并在验收记录里注明
- RustFS：本地测试用对象存储的本地文件后端；需要 RustFS 的 `#[ignore]` 测试若因没有 RustFS 而跳过，在验收记录里注明

### 12.2 分支、提交、PR、合并

- 每个阶段一个分支：`feat/session-archive-p<阶段号>`，从**最新的 `main`** 切出
- **一张卡一个提交**（卡内需要拆分时可以多个，但不跨卡）。提交信息第一行 `feat(<卡号>): <中文短句>`，正文写清改了什么、为什么、怎么验证；结尾带你所在环境要求的署名行
- 每完成一张卡：跑完 §0.3 的检查 → 提交 → `git push` → 更新 `docs/plan/progress.md`（同一个提交里，或紧跟着的一个 `docs(progress)` 提交）
- 一个阶段的全部卡完成后：开一个 PR（标题「会话档案 阶段 N：<目标>」，正文列出本阶段的卡和验收情况，链接 `progress.md` 与 `verification-log.md`）→ 等 CI 全绿 → **用 merge commit 合并到 `main`**（不 squash，保留一卡一提交）→ 下一阶段从新的 `main` 切分支
- CI 红了：先修再前进，**不许**跳过、禁用、`#[ignore]` 测试来变绿；确认是与本改动无关的既有失败时，在 PR 里写明证据再合并
- **绝不**：打 tag、发 Release、改部署文件里的生产配置、连接任何生产节点、force-push `main`

### 12.3 执行顺序（已按依赖排好，照着走）

```
阶段 0：P0-2 → P0-1 → P0-4 → P0-3 → P0-5
阶段 1：P1-2 → P1-4 → P1-5 → P1-3 → P1-1 → P1-6 → P1-7 → P1-8 → P1-9 → P1-10 → P1-11 → P1-12 → P1-13
阶段 2：P2-3 → P2-0 → P2-1 → P2-2 → P2-4 → P2-5 → P2-7 → P2-6
阶段 3：P3-1 → P3-2 → P3-3 → P3-4 → P3-5 → P3-6 → P3-7
阶段 4：P4-0 → P4-1 → P4-2 → P4-3 → P4-4 → P4-5 → P4-6 → P4-7
阶段 5：P5-1 → P5-2 → P5-3 → P5-4 → P5-5 → P5-6
阶段 6：P6-1 → P6-2 → P6-3 → P6-4 → P6-5
阶段 7：P7-1 → P7-2 → P7-3
```

### 12.4 卡住了怎么办

- **外部条件缺失**（没有某个凭据、某个宿主装不上、某个网站访问不到）：在 `progress.md` 把这张卡标 `blocked` 并写明缺什么，**跳到下一张不依赖它的卡**继续；阶段结束时在 PR 里汇总
- **设计与现实冲突**（上游格式和设计假设不符、某个决定实现不了）：选一个最小的、可回退的偏离，在 `progress.md` 的「偏离设计」一栏写清证据和做法，同时**在同一个提交里更新设计文档**对应段落（设计文档是权威，不能让代码和文档分家）。D1–D23 的**原则**不得推翻
- **不确定某个改动是否越界**：按 CLAUDE.md 和设计 §12 的约束对照表判断；仍不确定就选更保守的做法并记录

### 12.5 进度文件 `docs/plan/progress.md`

每张卡一行：状态（`todo` / `doing` / `done` / `blocked`）、提交、备注。会话被压缩或重启后，**先读这个文件**，从第一张不是 `done` 的卡继续。

### 12.6 验收记录 `docs/plan/verification-log.md`

卡片里写着「手动验证」「PR 里贴步骤」的验收项，能自动化的写成测试；不能自动化的（比如 `claude --resume` 的真实行为、Claude Code 连上 `/mcp`、三平台安装）在这个文件里逐条记录：做了什么、结果、或者「待所有者验证」以及验证步骤。

### 12.7 完成的定义

- `progress.md` 里所有卡都是 `done`，或是 `blocked` 且原因属于 §12.4 的「外部条件缺失」
- 每个阶段的 PR 都已合并进 `main`，`main` 上 CI 全绿
- `docs/roadmap.md` 的 F 节更新为实际完成情况；`README.md` 有采集、浏览、MCP 接入的英文说明
- 最后在 `docs/plan/progress.md` 末尾写一段总结：完成了什么、`blocked` 了什么、偏离了设计的哪些地方、所有者还需要做的事（按优先级排）


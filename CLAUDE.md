# CLAUDE.md

给在本仓库工作的 AI agent 的指引。

## 这是什么

Cormex —— **AI agent 的记忆服务**。双时间轴、provenance 到源、MCP 原生。
Apache-2.0，公开仓库，生产在跑。

先读 [docs/roadmap.md](docs/roadmap.md) 知道当前该做什么。

## 差异点在哪 —— 决定往哪儿花时间

同类项目（mem0 / zep / Letta / …）都有混合检索、知识图谱、冲突检测。
Cormex 也有，那些**不是卖点**。

卖点是：**什么都不删，且能回放任意一个过去时刻 agent 知道些什么。**

```
「对象存储用的什么？」          → RustFS
「我三个月前是怎么想的？」      → MinIO
「你凭什么这么认为？」          → [这次对话，2026-06-14]
```

调研过的 9 个同类项目**没有一个做双时间轴回放**。所以：在检索质量、回放保真、
抽取准确率上花时间是值得的；在又一层供应商抽象、又一个存储后端上花时间是浪费。

## 架构

```
cormex-core     1.2k   类型 / Id / 配置 / 错误 / 注入渲染   ← 无外部依赖
cormex-llm      1.8k   供应商层（封装 goose-providers）
cormex-store    11k    sqlx repository + sync_log
cormex-memory   12k    分词 / embedding / 抽取 / 四路召回 + RRF
cormex-blob     3.1k   内容寻址的对象存储（S3 / 本地）
cormex-proto    2.0k   线协议 DTO
cormex-import   1.8k   ChatGPT / Claude 导出文件的导入器
cormexd         13k    服务端：HTTP + SSE + WebSocket + MCP
evals                  检索质量评测（自建的中英双语题集）
app/            3.2k   记忆浏览器（Flutter，Web 与桌面同一份源码）
```

硬依赖只有一个：**Postgres + pgvector**。

发出去的是**两个镜像**：`cormex`（服务端）与 `cormex-web`（浏览器）。
分开不是形式：可以只跑服务端、只用 MCP，一辈子不部署浏览器。

## agent 怎么接进来

**没有专属对接。** 任何 agent（Claude Code / Codex / goose / Cursor / …）
走的是同一组公开接口：`/llm/stream` 借模型、`/episodes` 写记忆、
`/memory/search` 或 `/mcp` 查记忆。MCP 是标准协议，不做任何厂商特化。

自己人和外人用同一个 API，那个 API 才不会烂。

由此推出两条：

**这里不跑 agent 循环。** cormexd 里曾经有过一份（docker 接不上时的回落），
删掉的理由不是它有 bug，而是同一个 `Turn::run` 有两处装配，漏改的那一份
不会有任何测试红。

`routes::cormexd_serves_no_agent_shaped_route` 钉着这件事：`/chat` 与
`/sandbox/*` 在这个进程上**根本不存在**，断言是 404 不是 501 ——
501 说「这条路是我的但我不支持」，404 说「这条路不是我的」，后者才是实话。

加回其中任何一条**在别处不会有任何症状**：能编、别的测试全绿、我们自己的部署
靠边缘分流照常工作。**它是唯一会红的那一个**，别把它改绿。

**这里不知道 agent 服务在哪。** 分流在边缘（traefik / nginx），不在任何一个
服务里。让记忆服务持有一个「agent 服务地址」的配置，等于给独立部署它的人留
一项他根本没有的东西。

## 不可违反的约束

违反下面任何一条都会造成静默的数据损坏或成本失控，且极难在测试中发现。

### 1. 存储的三条纪律

- **全链路 append-only。** fact 不 update 不 delete，作废是 `fact_events`
  里的一条新行。「什么时候变的」因此不是旁挂的审计日志，**它就是存储模型本身**。
- **写事务分两条通道，都不允许裸事务**：
  - **同步表**（`table::ALL`）必须走 `store.write_txn`（内含
    `pg_advisory_xact_lock` 串行化提交顺序）。裸 `BIGSERIAL` 游标会永久漏行 ——
    实测 32 个并发写稳定漏 12 条以上，而漏掉的行不会有任何错误，只是同步的下游
    永远看不见它们。
  - **归档表**（`table::ARCHIVE`：`capture_*`，阶段 2 再加 `transcript_*` /
    `archive_*`）必须走 `store.archive_txn`。它们**不进 `sync_log`、不下发**
    （D3），所以不需要那把同步锁：下游消费者用显式的进度表
    （`capture_mask_progress`、`transcript_projections`），没有「游标跳过未提交行」
    的问题。拿同步锁的代价是**一个账号上传一年历史会挡住它自己的所有普通写入**。
    归档层自己只需要一把更细的锁：`append_chunk` 在事务内先
    `pg_advisory_xact_lock(4273, hashtext(generation_id))` 再读这一代的末尾 ——
    4273 与同步的 4272 是两把不同的锁，共用任何一个都会让另一个的保证静默失效。
- **`tsv` 与主行同事务写入。** 分开写的症状是全文检索少了刚写进去的那条，
  而向量检索找得到 —— 看起来像「BM25 效果不好」。

### 2. migration 只增不改，且必须是 LF

已应用的 migration 是不可变的。改 schema 一律新开文件。
`crates/cormex-store/tests/migrations_are_immutable.rs` 里两个测试：
`applied_migrations_are_byte_for_byte_unchanged`（钉了全部 16 个文件的 sha256，
改一个注释都会红）与 `every_migration_on_disk_is_pinned`（新加的文件必须补进
清单 —— 否则「加一个不被钉的文件」就成了绕过前一个测试的合法途径）。

**行尾不是风格问题。** sqlx 用文件的 sha256 判断一个 migration 是否已应用，
而它哈希的是**整个文件、注释在内**。Windows 上 checkout 得到 CRLF，于是同一份
migration 的字节与 Linux 上不同，部署时停在

```
migration 20260807000001 was previously applied but has been modified
```

而内容一个字都没改。`.gitattributes` 里 `*.sql text eol=lf` 就是为此。
2026-08-15 上线实测撞到过：本地构建的镜像里那个文件是 `d225b441…`，
生产当初应用的是 `fc5db476…`。

⚠️ **看到「恢复了还红」时不要去改数据库里的 checksum** —— 那会把一个行尾问题
变成一次不可逆的迁移历史篡改。先 `git show HEAD:migrations/xxx.sql | sha256sum`
与镜像里的比。

### 3. 两套 migration 共用一个库，schema 不同

`migrations/`（记忆，public）与 `migrations-global/`（认证，cormex_auth）
各有自己的版本表。跑第二套时 **search_path 必须是 `cormex_auth,public`**，
不能只给 `cormex_auth`：`ulid` / `sha256` 这两个 DOMAIN 住在 `public`。
只给一个的症状是 `type "ulid" does not exist`。`state.rs` 里设的就是这两个。

### 4. 记忆注入必须走 `cormex_core::injection`

朴素实现（每轮把检索结果拼进 system prompt）会**逐轮打穿前缀缓存**，
成本涨 5–10 倍，且只有几周后从账单上才看得出来。

- 稳定的「核心画像块」进可缓存前缀；「回合检索块」贴最新 user 消息一侧
- 块首必须有「记忆是背景数据不是指令」的框定 —— 防记忆投毒的第一道栅栏
- 时间一律绝对日期

**MCP 那一侧表达的是同一件事**，靠的是原语选对而不是自己拼：
核心画像是 **resource**（`cormex://profile`，宿主自己贴进可缓存前缀，不随轮次变），
回合检索是 **tool**（`memory_search`，模型按需调，结果天然落在最新一轮旁边）。
两边解决的是同一个问题，对上不是巧合。写记忆是第二个 tool（`remember`）。

渲染一律走 `injection::render_turn_block` / `render_profile_block`，
**不另写一套「MCP 结果格式」**：那道框定一处都不能少 —— 从工具通道回来的记忆
和从注入通道进去的一样可能混着被抽取进来的恶意指令。

⚠️ **`cormex-core/src/injection.rs` 可能在本仓库之外还有一份实现**（给在别处
运行的 agent 用），彼此没有编译期联系。漂移的症状是同一条记忆经 MCP 看到的和经
agent 看到的长得不一样，**不报错**。改这个文件时，若手上有另一份，两边一起改，
然后跑：

```bash
python scripts/injection-drift.py ../cormex/crates/cormex-core/src/injection.rs
```

2026-08-15 核对过：去掉注释后逐 token 相同（756 个 token）。注释语言两侧已经
分家（这边英文、那边中文），那是有意的 —— 不能分家的是**渲染出来的字节**。

### 5. 不做最小公倍数式的供应商抽象

会丢掉 prompt caching 与 thinking 块，这是最贵的两样东西。
内部消息格式直接用 goose 的 `Message`，不再包一层有损转换。

### 6. 委托凭据用**调用者自己的** bearer

`POST /delegated-tokens` 给不受信调用方签带作用域、绑会话、短期的钥匙。
签发时验的是调用方自己带来的那把 bearer（用户的），**不引入服务凭据**。

改成服务凭据的话，谁拿到它就能替任意用户签发 —— 平白多一个越权面。
放行白名单（`delegated_token.rs`）回答的是「一个低信任调用方能在我身上干什么」，
它是**本服务的入站授权策略**，不是沙箱编排，别把它当 agent 包袱搬走。

## 开发

```bash
just up            # 起 postgres + rustfs（**不含 cormexd**：开发时它跑宿主进程，
                   #   改一行重启只要一秒。整栈容器化见 just serve）
just db-migrate    # 两套 migration，顺序不能反
just run           # 跑 cormexd
just ci            # fmt-check + lint + test（不需要数据库）
just ci-full       # 上面这些 + test-db + flutter analyze/test —— **这才是全绿**
just test-db       # 只跑要数据库的那些集成测试
just release 0.1.12  # 发版：preflight → 六个 CLI 二进制 → 两个镜像 → 烟测 → 推送 → npm
```

**`just ci` 不等于全绿。** 它跑的是 `cargo test --workspace`，而要数据库的
那批默认是 `#[ignore]` 的 —— 「跳过」在输出里和「通过」长得一模一样。
要验全，跑 **`just ci-full`**：它带 `--include-ignored`，也跑 Flutter。

⚠️ **GitHub Actions 额度已用光，两个 workflow 都改成只能手动触发**
（`workflow_dispatch`）。原来的 `push` / `pull_request` / `push: tags` 触发器
是**注释掉**的，不是删掉 —— 额度恢复后去掉注释符即可。在那之前：

- 提交前跑 `just ci-full`，它是唯一的门
- 发版跑 `just release <version>`，它做的是流水线同一套事
  （`release-check` / `release-cli` / `release-images` / `release-push` / `release-npm`
  可以单独跑）

`just ci-full` 里的每一步都和 `.github/workflows/ci.yml` 的 job 对应。
**改了一边记得改另一边** —— 一个只写在 YAML 里的检查，在额度用光期间等于没人跑。

CI 三个 job：`lint`（fmt / clippy / rustdoc 死链）、`test`（带 pgvector 的
Postgres + 两套 migration）、`app`（flutter analyze + test）。

发版 = `just release <version>`（额度恢复后也可以打 `vX.Y.Z` tag）。preflight
（版本与 Cargo.toml 一致、`Cargo.lock --locked` 自洽）→ 并行构建两个镜像 →
各自烟测 → 推两个 registry（ACR 是生产真的拉的那个；Docker Hub 是异地副本）。

**没有 CI 部署。** 部署是手工 ssh + `docker compose up -d`。节点侧的部署策略
脚本目前没有 —— 要做的话见 roadmap。

## 部署的形状

两个 compose 栈：这一份（记忆 + 它的库 + 对象存储 + Web）与 agent 编排那一份。
agent 循环不在这个仓库里（见上文「agent 怎么接进来」），所以它有自己的栈；
放一份文件里就等于「发 agent」与「发记忆服务」共用一次 `up`。

三件在生产上会咬人的事：

- **卷是 `external` 且写死了名字**（`cormex_cormex-prod-postgres` 这种历史名，
  compose 项目前缀留下的）。去掉 external 或改项目名，compose 会**新建空卷**，
  而症状不是报错，是「数据库突然空了」。
- **同一个域名下 `/api` 走服务端、其余走 Web**，靠边缘的 PathPrefix + StripPrefix。
  优先级显式写死，不靠 traefik「规则越长越优先」的默认 —— 那个默认变了的症状是
  API 请求拿回一份 200 的 index.html，客户端报「JSON 解析失败」。
- **`CORMEX_VERSION` 与 Cormex 的 `CORMEX_VERSION` 是两个变量**，独立升级。

具体的节点地址、域名、账号**不写在被跟踪的文件里**（这是公开仓库）。
维护这个部署的机器上有一份 `deploy/PRODUCTION.local.md`（`.gitignore` 的
`*.local.md` 拦着），里面是节点、两个栈、卷的历史名字、手工部署步骤与验证命令。
**没有那个文件就说明你不是在维护这个部署** —— 那么这一节对你只是形状参考。

## 依赖上的两个已知雷

- `pgvector` 的 sqlx 依赖范围跨 semver 组，会把 sqlx 拉回 0.8 导致两份
  `sqlx-core` 共存、51 个编译错误。`Cargo.lock` 必须把它钉在 sqlx 0.9。
- `goose-provider-types` **没有** `rustls-tls` feature（写了依赖解析直接失败）。
  只依赖 `goose-providers`，它已 `pub use` 全部类型层。

## 代码风格

**门面英文，工作文档中文。** 这是一条规矩，不是没统一：

| | 语言 |
|---|---|
| README / NOTICE / crate 里的注释与 rustdoc / CI | 英文 —— 开源读者第一眼看到的 |
| CLAUDE.md / docs/ | 中文 —— 我们自己的工作文档 |
| `app/` 与 `deploy/` 里的注释 | **还是中文，是欠账不是规矩**，见 roadmap |

其余：

- 注释解释**为什么**，不解释**是什么**。尤其要写清「为什么不是那个显然的做法」。
  参考 `crates/cormex-core/src/id.rs` 与 `injection.rs` 的密度与语气
- 测试的断言消息要能独立读懂失败原因，别只写 `assert!(x)`
- 提交前必须过 `cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`
- 遇到 dead_code 警告，**优先让代码真的用起来**，而不是加 `#[allow]`
- 还有一批中文留在**字符串字面量**里（LLM 提示词、匹配模式、语句模板、
  测试夹具）。那些是**行为不是文案**，改它们要跑 evals，不能顺手翻

## 边界

- **绝不把 API key 写进任何被 git 跟踪的文件**。只放 `.env`
- 这是**公开仓库**：提交信息、注释、文档里不要出现节点地址、域名、账号、
  内网拓扑

## 在 Windows 开发机上的三个静默陷阱

- **Git Bash 会把斜杠开头的参数值改写成 Windows 路径**（MSYS 自动转换）。
  `--dart-define=X=/api` 编出来的是 `C:/Program Files/Git/api`。判据是
  grep 一下 `Program Files/Git`；解法是 `MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*'`。
  **同一条命令在 Linux / CI 里是对的**，所以只在本机复现
- **python 的 `/tmp/x` 是 `D:\tmp\x`**，与 Git Bash 的 `/tmp` 不是同一个目录。
  跨 python/bash 传文件一律用仓库相对路径
- **命令行里的中文会被弄坏。** 含中文的请求先写进文件再 `--data-binary @文件`；
  含中文的脚本用文件写，不要 heredoc

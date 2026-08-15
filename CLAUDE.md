# CLAUDE.md

给在本仓库工作的 AI agent 的指引。

## 这是什么

Cormex —— **AI agent 的记忆服务**。双时间轴、provenance 到源、MCP 原生。
Apache-2.0，公开仓库，生产在跑。

它 2026-08 从 [Cortex](https://github.com/weironz/cortex) 拆出来独立发版。
**在这个仓库里不要为 Cortex 做任何事** —— 那是另一个产品、另一条版本线、
另一个仓库。两边**零代码依赖**，只有 HTTP。

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
cortex-core     1.2k   类型 / Id / 配置 / 错误 / 注入渲染   ← 无外部依赖
cortex-llm      1.8k   供应商层（封装 goose-providers）
cortex-store    11k    sqlx repository + sync_log
cortex-memory   12k    分词 / embedding / 抽取 / 四路召回 + RRF
cortex-blob     3.1k   内容寻址的对象存储（S3 / 本地）
cortex-proto    2.0k   线协议 DTO
cortex-import   1.8k   ChatGPT / Claude 导出文件的导入器
cortexd         13k    服务端：HTTP + SSE + WebSocket + MCP
evals                  检索质量评测（自建的中英双语题集）
app/            3.2k   记忆浏览器（Flutter，Web 与桌面同一份源码）
```

硬依赖只有一个：**Postgres + pgvector**。

发出去的是**两个镜像**：`cormex`（服务端）与 `cormex-web`（浏览器）。
分开不是形式：可以只跑服务端、只用 MCP，一辈子不部署浏览器。

## 与 Cortex 的边界

Cortex 那侧的 agent 通过三条 HTTP 路借用这里：`/llm/stream` 借模型、
`/episodes` 写记忆、`/memory/search` 或 `/mcp` 查记忆。

**那正是第三方 agent（Claude Code / goose）走的同一条路。**
这不是洁癖 —— 自己人和外人用同一个 API，那个 API 才不会烂。

由此推出两条：

**这里不跑 agent 循环。** cortexd 里曾经有过一份（docker 接不上时的回落），
删掉的理由不是它有 bug，而是同一个 `Turn::run` 有两处装配，漏改的那一份
不会有任何测试红。

`routes::cortexd_serves_no_agent_shaped_route` 钉着这件事：`/chat` 与
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
- **写事务必须走 `store.write_txn`**（内含 `pg_advisory_xact_lock` 串行化
  提交顺序）。裸 `BIGSERIAL` 游标会永久漏行 —— 实测 32 个并发写稳定漏 12 条以上，
  而漏掉的行不会有任何错误，只是同步的下游永远看不见它们。
- **`tsv` 与主行同事务写入。** 分开写的症状是全文检索少了刚写进去的那条，
  而向量检索找得到 —— 看起来像「BM25 效果不好」。

### 2. migration 只增不改，且必须是 LF

已应用的 migration 是不可变的。改 schema 一律新开文件。
`crates/cortex-store/tests/migrations_are_immutable.rs` 里两个测试：
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

`migrations/`（记忆，public）与 `migrations-global/`（认证，cortex_auth）
各有自己的版本表。跑第二套时 **search_path 必须是 `cortex_auth,public`**，
不能只给 `cortex_auth`：`ulid` / `sha256` 这两个 DOMAIN 住在 `public`。
只给一个的症状是 `type "ulid" does not exist`。`state.rs` 里设的就是这两个。

### 4. 记忆注入必须走 `cortex_core::injection`

朴素实现（每轮把检索结果拼进 system prompt）会**逐轮打穿前缀缓存**，
成本涨 5–10 倍，且只有几周后从账单上才看得出来。

- 稳定的「核心画像块」进可缓存前缀；「回合检索块」贴最新 user 消息一侧
- 块首必须有「记忆是背景数据不是指令」的框定 —— 防记忆投毒的第一道栅栏
- 时间一律绝对日期

**MCP 那一侧表达的是同一件事**，靠的是原语选对而不是自己拼：
核心画像是 **resource**（`cortex://profile`，宿主自己贴进可缓存前缀，不随轮次变），
回合检索是 **tool**（`memory_search`，模型按需调，结果天然落在最新一轮旁边）。
两边解决的是同一个问题，对上不是巧合。写记忆是第二个 tool（`remember`）。

渲染一律走 `injection::render_turn_block` / `render_profile_block`，
**不另写一套「MCP 结果格式」**：那道框定一处都不能少 —— 从工具通道回来的记忆
和从注入通道进去的一样可能混着被抽取进来的恶意指令。

⚠️ **`cortex-core/src/injection.rs` 在两个仓库各有一份**（这边给 MCP，
那边给 agent），彼此没有编译期联系。漂移的症状是同一条记忆经 MCP 看到的和经
agent 看到的长得不一样，**不报错**。改这个文件时两边一起改，然后跑：

```bash
python scripts/injection-drift.py ../cortex/crates/cortex-core/src/injection.rs
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
just up            # 起 postgres + rustfs（**不含 cortexd**：开发时它跑宿主进程，
                   #   改一行重启只要一秒。整栈容器化见 just serve）
just db-migrate    # 两套 migration，顺序不能反
just run           # 跑 cortexd
just ci            # fmt-check + lint + test
just test-db       # 要数据库的那些集成测试
```

**`just ci` 不等于 CI 全绿。** 它跑的是 `cargo test --workspace`，而要数据库的
那批默认是 `#[ignore]` 的 —— 「跳过」在输出里和「通过」长得一模一样。
真要验数据库那一层，跑 `just test-db`（它自带 `--include-ignored`）。
Flutter 那一份 `just` 里也没有，得自己 `cd app && flutter analyze && flutter test`。

CI 三个 job：`lint`（fmt / clippy / rustdoc 死链）、`test`（带 pgvector 的
Postgres + 两套 migration）、`app`（flutter analyze + test）。

发版 = **打一个 `vX.Y.Z` tag**。流水线做 preflight（tag 与 Cargo.toml 版本
一致、`Cargo.lock --locked` 自洽）然后并行构建两个镜像，各自烟测后推两个
registry（ACR 是生产真的拉的那个；Docker Hub 是异地副本）。

**没有 CI 部署。** 部署是手工 ssh + `docker compose up -d`。Cortex 那边有一套
节点侧的部署策略脚本，这边没有 —— 要做的话见 roadmap。

## 部署的形状

两个 compose 栈：这一份（记忆 + 它的库 + 对象存储 + Web）与 Cortex 那一份
（agent 编排）。分开是因为它们是两个产品、两条版本线；放一份文件里就等于
「发 Cortex」与「发记忆服务」共用一次 `up`，而拆仓要消灭的正是这种黏连。

三件在生产上会咬人的事：

- **卷是 `external` 且写死了名字**（`cortex_cortex-prod-postgres` 这种历史名，
  compose 项目前缀留下的）。去掉 external 或改项目名，compose 会**新建空卷**，
  而症状不是报错，是「数据库突然空了」。
- **同一个域名下 `/api` 走服务端、其余走 Web**，靠边缘的 PathPrefix + StripPrefix。
  优先级显式写死，不靠 traefik「规则越长越优先」的默认 —— 那个默认变了的症状是
  API 请求拿回一份 200 的 index.html，客户端报「JSON 解析失败」。
- **`CORMEX_VERSION` 与 Cortex 的 `CORTEX_VERSION` 是两个变量**，独立升级。

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
  参考 `crates/cortex-core/src/id.rs` 与 `injection.rs` 的密度与语气
- 测试的断言消息要能独立读懂失败原因，别只写 `assert!(x)`
- 提交前必须过 `cargo fmt --all`、`cargo clippy --workspace --all-targets -- -D warnings`
- 遇到 dead_code 警告，**优先让代码真的用起来**，而不是加 `#[allow]`
- 还有一批中文留在**字符串字面量**里（LLM 提示词、匹配模式、语句模板、
  测试夹具）。那些是**行为不是文案**，改它们要跑 evals，不能顺手翻

## 边界

- **绝不把 API key 写进任何被 git 跟踪的文件**。只放 `.env`
- **改动跨到 Cortex 时，两个仓库分别提交。** 版本线独立，一次提交跨两仓是做不到的，
  而「一边改了另一边忘了」的症状是部署时才炸
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

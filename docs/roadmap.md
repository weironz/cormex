# Roadmap

## 现在在哪

**v0.1.11。生产在跑，仓库已公开，两个镜像在发。**

对外只有 HTTP 契约，没有代码依赖 —— 任何 agent 走的都是同一组公开接口。

```
                        ┌─────────────────────────────┐
   浏览器  ──────────▶  │  cormex-web   （静态 nginx）│
                        └─────────────────────────────┘
                                     │  同源 /api
                                     ▼
   Claude Code ──/mcp──▶ ┌───────────────────────────┐ ──▶ Postgres + pgvector
   goose        ────────▶│  cormex（cormexd 二进制）  │ ──▶ S3 兼容对象存储
   Cormex 的 agent ─────▶└───────────────────────────┘ ──▶ embedding 服务
        /llm/stream · /episodes · /memory/search
```

自己人和外人**走同一个 API** —— 那是这套东西不会烂掉的唯一原因。

## 已经做完的（别重做）

- **MCP 门面**：`cormex://profile`（resource）+ `memory_search` / `remember`（tool）。
  两个原语的分工对应注入的两半，见 CLAUDE.md 约束 4
- **委托凭据**：`POST/DELETE /delegated-tokens`，用调用者自己的 bearer 签发
- **agent 循环已从这里删除**，且有测试钉着不许回来
- **记忆浏览器**：检索、provenance、可拖的时间轴。Flutter 一份源码出 Web 与桌面，
  Web 发成 `cormex-web` 镜像，**镜像里不认识任何域名**（API base 是 `/api` 这个
  同源相对前缀）
- **Rust crate 的注释与 rustdoc 已英文化**，rustdoc 死链进了 CI
- **migration 不可变性**有 sha256 测试钉着，行尾有 `.gitattributes` 钉着

---

## A. 这个仓库现在对外是坏的（P0）

**2026-08-30：compose 那一半做完了，而做的过程中撞出一个更硬的。**

### ✅ 做完的：入口文件不再是「保证失败」的那一份

原来的失败是第一步就撞墙，而且看不出是为什么：

```console
$ cd deploy && docker compose up -d
external volume "cormex_cormex-prod-postgres" not found
```

现在 `deploy/` 里是三份职责分明的东西：

| | 是什么 |
|---|---|
| `docker-compose.yml` | **自足**。自建卷、无 traefik、端口发到 127.0.0.1、镜像默认指 Docker Hub。README 指的就是它 |
| `docker-compose.prod.yml` | 我们生产那份，原样搬过来 + 一段英文抬头写清「别跑这个」 |
| `proxy.conf` | 自足那份缺的那一块 —— 见下 |

`.env.example` 跟着拆成两份（`.env.example` / `.env.prod.example`）。
根那份 dev compose 的 `name:` 是 `cormex` —— **那才是所有 `cormex_` 卷名前缀
的来源**。

**自足那份必须自带一层薄反代，这不是可选项。** `cormex-web` 镜像刻意不反代
API（部署事实不该编进镜像），生产上那一刀是 traefik 切的。没有它的话，浏览器
的 `/api` 请求会落到 web 镜像的 SPA 回落上，**回 200 + index.html** ——
客户端随即报「JSON 解析失败」，一句完全不像路由配错的错。`proxy.conf` 里还
钉了两件同类的坑：SSE 必须 `proxy_buffering off`（否则整轮答案憋到结束才
一次吐出来，功能「正常」只是不再是流），以及 upstream 走变量 + resolver
（否则服务端还没起来时 nginx 直接 `host not found in upstream` 退出，且不重试）。

顺带修了一处静默的：`.gitignore` 按「任何以 `.env` 结尾的」拦，只放行了
`.env.example`，于是新加的 `.env.prod.example` **会被静默忽略** ——
本机好好的、`git status` 干净、提交也成功，而 clone 的人没有那个文件。

### ✅ 撞出来的：**全新数据库上根本起不来**（2026-10-29 修）

修法取的是上面列的第 1 种：`Store::migrate_global` 在跑全局那套之前，**先用
同一个 `boot` 连接对 `public` 跑一次租户那套**（`boot` 显式设
`search_path=public`，否则登录用户同名的 schema 会截胡）。一处覆盖三个调用点，
不存在「漏一处」；DOMAIN 仍然只有一份定义，没有抄第二遍。重复执行是幂等的
—— sqlx 在 `public._sqlx_migrations` 里记着已应用的版本。

`crates/cortex-store/tests/fresh_database.rs` 建一个真正空的库来钉这件事
（`#[ignore]`，`just test-db` 会跑到）。改之前它报
`type "ulid" does not exist`，改之后通过。

### 原来的记录：**全新数据库上根本起不来**

照着新 README 从零跑一遍（这是这一条一直缺的那次验证），服务端进了重启循环：

```
the global schema migration failed: while executing migration 20260810000002:
type "ulid" does not exist
```

两套 migration 的**顺序**错了，而它错得只在干净库上现形：

- `ulid` / `sha256` 两个 DOMAIN 由**租户**那套建在 `public`
  （`migrations/20260807000001_init.sql`，08-07）
- **全局**那套（`migrations-global/`，08-10）用 `ulid`，而启动时它**先跑**
  （`Live::new` 与 `Accounts::connect` 都是先 global）

**我们自己的部署永远撞不到**：那个库的 `public` 比账号表早三天，DOMAIN 早就在。
所以这条路自从多租户落地以来，一次都没有在空库上跑过。

⚠️ 症状还分两段，别只看后一段：`main.rs` 里 `Accounts::connect` 先失败，
但那里**只 WARN**（「账号表不可达，/auth/* 答 501」），于是就算后面把 global
跑通了，**这一轮启动的账号功能已经废了**。所以只在 `Live::new` 里调个序是
半个修复。

**怎么修要定一次，别顺手**（migration 是不可变的，有 sha256 测试钉着）：

1. 让 `migrate_global` **自足** —— 跑自己那套之前先幂等地建出两个 DOMAIN。
   覆盖所有调用点，但把 DOMAIN 定义抄成了两份，正是这个仓库盯得最紧的
   「同一个定义两处」。要配一条比对两处文本的测试
2. 把租户那套排到全局前面。SQL 层面**没有**反向依赖（核过：`migrations/`
   里唯一提到 `cormex_auth` 的地方是一句注释），但调用点有三处，漏一处就是
   上面那个「账号这一轮废了」
3. 新加一条编号更早的全局 migration —— 与 sqlx 的顺序检查冲突

### 还欠的

- **在一台干净机器上完整走一遍**：能起、能开第一个账号、能搜到东西。
  空库那一条已经修了（见上），这一条现在只剩「真的走一遍」本身
- README 的「Known gap」已经摘掉 —— 它挡着的那条修好了。摘掉是**跟着修复走
  的**，不是单独的动作：一份声称能跑而跑不起来的 quickstart，比一份诚实说明
  卡在哪的更糟，反过来也一样

## B. 第一次用的人卡在哪

**开第一个账号的流程只散在注释里。** 实际是：`cormexd --generate-token` →
拿那串令牌连上 → 建账号 → 之后用账号密码。没有一处文档从头讲到尾，
而这是每个自托管的人必经的第一步。

- README 补一节 quickstart：起服务 → 生成令牌 → 开账号 → 打开浏览器
- 补 **MCP 接入片段**：Claude Code 与 goose 各一段能直接抄的配置。
  MCP 原生是主要卖点，而现在没有一行「怎么接上去」
- 补 `CHANGELOG.md`：现在完全没有。0.1.9 起的版本都在 GitHub tag 上，
  但没人知道每版改了什么

## C. 欠账

- **`app/` 与 `deploy/` 里的注释还是中文。** Rust crate 那一批翻完了，这两处没有。
  README 的 Status 已经如实写着这件事
- **`injection-drift.py` 只是手工脚本。** 它守的是 CLAUDE.md 约束 4 —— 一条
  漂了不报错的约束。要进 CI 得先解决「CI 上拿不到另一份实现」：可行的做法是
  把渲染实现连同测试一起做成一个可以被两边 vendor 的单文件并比对 sha256
- **evals 不在 CI 里。** 它是 workspace member，所以 `cargo test` 会编它，
  但 `cargo run -p cormex-evals -- validate` 这条几秒就跑完的题集校验没有接上。
  它的价值恰恰是「给每一次『优化』一个可对照的数字」—— 不跑就等于没有
- **备份只写了没验过恢复。** `data/backup` 有东西，但没有一次「从备份起一个空库
  并对上记录数」的演练

## D. 产品与 API

- **时间回放已经在 MCP 上了，但没人看得见。** `memory_search` 收 `as_of`
  （RFC 3339），第三方 agent 现在就能问「三个月前你怎么想的」—— 别人一个都没有
  的能力，而 README 里没有一个例子演示它，工具描述里那句话也只有连上之后才看得到。
  差的不是能力，是**摆出来**：README 一段真实的 as_of 对照，胜过三段形容词
- **API 没有冻结。** 公开之后每一次不兼容变更都会打到别人。至少要有一份
  「哪些端点算稳定、协议版本怎么走」的声明（`protocol: 1` 已经在 `/health` 里了，
  但没有文档说它意味着什么）
- **没有 `cargo install` 路径，crates 没发。** Docker 是唯一的打包方式

## E. 运维

- **没有 CI 部署。** 现在是手工 ssh + `docker compose up -d`。节点侧的部署策略
  脚本目前没有 —— 要不要做取决于发布频率。**在有之前，别把手工步骤写进文档
  当成流程**，写清楚它就是手工的
- **升级路径没有验证过。** 从 v0.1.9 升到 v0.1.11 走的是「pull + up」，
  中间没有 schema 变更所以顺利。第一次带 migration 的升级需要一份先 dump 的退路

---

## F. 会话档案（下一条主线）

**2026-10-29 立项。** 把各台机器上各家 agent 的原始会话字节级采进中央库，在 Web 上像原始对话一样浏览，并让任何 agent 经 MCP 取回交接简报或原文。

| | |
|---|---|
| 为什么做、评审结论 | [research/2026-10-agent-memory-survey.md](research/2026-10-agent-memory-survey.md) §7–§10 |
| 技术选型与架构（唯一设计权威，D1–D23） | [design/session-archive-architecture.md](design/session-archive-architecture.md) |
| 代码级阶段开发计划（任务卡 P0-1 … P7-3） | [plan/session-archive-plan.md](plan/session-archive-plan.md) |

阶段 0 的 P0-1 就是上面 A 节的「全新数据库上起不来」，修法已定（D20）。P0-2 修的是委托令牌会落进 1 号用户租户的隔离问题，多用户部署下应优先处理。

## 不做

- **不加第二个存储后端。** Postgres + pgvector 是唯一硬依赖，那是特性不是限制 ——
  双时间轴与四路召回都建在 SQL 上，抽象掉它等于把卖点抽象掉
- **不做最小公倍数式的供应商抽象**（会丢 prompt caching 与 thinking 块）
- **不把 agent 循环搬回来**

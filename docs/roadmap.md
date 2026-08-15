# Roadmap

## 现在在哪

**v0.1.11。生产在跑，仓库已公开，两个镜像在发。**

拆分这件事**做完了**：与 Cortex 之间零代码依赖，只有 HTTP。
`cargo tree` 上没有一条边通向那边，两条版本线独立。

```
                        ┌─────────────────────────────┐
   浏览器  ──────────▶  │  cormex-web   （静态 nginx）│
                        └─────────────────────────────┘
                                     │  同源 /api
                                     ▼
   Claude Code ──/mcp──▶ ┌───────────────────────────┐ ──▶ Postgres + pgvector
   goose        ────────▶│  cormex（cortexd 二进制）  │ ──▶ S3 兼容对象存储
   Cortex 的 agent ─────▶└───────────────────────────┘ ──▶ embedding 服务
        /llm/stream · /episodes · /memory/search
```

自己人和外人**走同一个 API** —— 那是这套东西不会烂掉的唯一原因。

## 已经做完的（别重做）

- **MCP 门面**：`cortex://profile`（resource）+ `memory_search` / `remember`（tool）。
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

**别人 clone 下来跑不起来。** 这不是「文档不够」，是一条实测的失败：

```console
$ cd deploy && docker compose up -d
external volume "cortex_cortex-prod-postgres" not found
```

`deploy/docker-compose.yml` 是**我们生产那一份**：卷声明成 `external` 且写死了
历史名字（compose 项目前缀留下的）。那对生产是**必须的** —— 去掉 external 会让
compose 新建空卷，症状是「数据库突然空了」。但对一个新用户，它是一份**保证失败**
的入口文件，而 README 的 quickstart 正指着它。

还有两处同源的混乱：

- 根 `docker-compose.yml` 是**开发**用的（`just up` 用它），但它 `name: cortex`、
  服务叫 `cortexd`。两份 compose、两个名字体系、职责重叠
- `README.md` 的 Running it 一节是照生产那份写的

**要做的**：

1. 新写一份 `deploy/docker-compose.yml` —— 单机自足：卷自建（无 external）、
   无 traefik（发布端口到 127.0.0.1）、无跨栈网络。**这一份是 README 指的那份**
2. 生产那份改名 `deploy/docker-compose.prod.yml`，文件头写清「卷是 external
   且名字不能改」的理由
3. 根那份改名 `docker-compose.dev.yml` 或并进 `deploy/`，`name:` 改成 `cormex`
4. 真的验一次：在一台干净机器上照 README 走一遍，能起、能开第一个账号、能搜到东西

做完这条之前，「已开源」在实质上只是「代码可读」。

## B. 第一次用的人卡在哪

**开第一个账号的流程只散在注释里。** 实际是：`cortexd --generate-token` →
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
  漂了不报错的约束。要进 CI 得先解决「CI 上没有另一个仓库」：可行的做法是
  在 Cortex 那侧的 CI 里跑（那边 checkout 两个仓库更自然），或者把渲染实现
  连同测试一起做成一个可以被两边 vendor 的单文件并比对 sha256
- **evals 不在 CI 里。** 它是 workspace member，所以 `cargo test` 会编它，
  但 `cargo run -p cortex-evals -- validate` 这条几秒就跑完的题集校验没有接上。
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

- **没有 CI 部署。** 现在是手工 ssh + `docker compose up -d`。Cortex 那边有一套
  节点侧的部署策略脚本（CI 只能执行它、不能改它）—— 这边要不要照做，取决于
  发布频率。**在有之前，别把手工步骤写进文档当成流程**，写清楚它就是手工的
- **升级路径没有验证过。** 从 v0.1.9 升到 v0.1.11 走的是「pull + up」，
  中间没有 schema 变更所以顺利。第一次带 migration 的升级需要一份先 dump 的退路

---

## 不做

- **不为 Cortex 做任何事。** 它是另一个产品、另一个仓库。这边只保证 HTTP 契约
- **不加第二个存储后端。** Postgres + pgvector 是唯一硬依赖，那是特性不是限制 ——
  双时间轴与四路召回都建在 SQL 上，抽象掉它等于把卖点抽象掉
- **不做最小公倍数式的供应商抽象**（会丢 prompt caching 与 thinking 块）
- **不把 agent 循环搬回来**

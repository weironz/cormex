# Roadmap

> 2026-11-08 重写，2026-11-09 v0.1.20 上线后更新。上一版停在 v0.1.12，把 CHANGELOG、MCP 接入片段、
> CI 部署都列成「没做」，而它们都已经做了。这一版按**代码和生产数据**核对过；核对不到的写「未验证」，
> 不写「完成」。

## 现在在哪

**生产运行 v0.1.20（2026-11-09 用 `deploy-upgrade` 升级并通过验收，单节点）。仓库公开，两个镜像在发；
CLI 本地构建只出 Linux x64 与 Windows x64（见欠账）。**

```
                        ┌─────────────────────────────┐
   浏览器  ──────────▶  │  cormex-web   （静态 nginx）│
                        └─────────────────────────────┘
                                     │  同源 /api
                                     ▼
   Claude Code ──/mcp──▶ ┌───────────────────────────┐ ──▶ Postgres + pgvector
   Codex（cormex mcp）──▶│  cormex（cormexd 二进制）  │ ──▶ S3 兼容对象存储
   任何 agent ─────────▶ └───────────────────────────┘ ──▶ embedding / LLM 供应商
        /mcp · /episodes · /memory/search · /capture/* · /archive/*
             ▲
   cormex CLI（hooks + 定时任务）把各台机器上各家 agent 的原始会话字节级采上来
```

两条主线，成熟度不同：

| | 状态 | 一句话 |
|---|---|---|
| **聊天档案**（采集 → 解析 → 浏览 → 取回） | 主体完成，生产验证 | 4510 个会话在库里，13+ 种 agent，导入四家厂商导出 |
| **记忆**（抽取 → 检索 → 注入 / 回放） | 能用，没打磨完 | 检索有基线数字；**抽取质量没有数字**，全量抽取在生产上还没开过 |

## 已经做完的（别重做）

**底座**
- 全链路 append-only，双时间轴（`valid_at` / `created_at` + `fact_events`），每条事实指回出处
- 四路召回 + RRF（向量 / BM25 / 图 / 时间）；`memory_search` 收 `as_of`，能回放任意过去时刻
- migration 不可变（sha256 测试 + `.gitattributes` 钉 LF）；空库能起（D20）
- 委托凭据：调用者自己的 bearer 签发，带作用域、绑会话、短期

**接口**
- MCP：`cormex://profile`（resource）、`memory_search` / `remember` / `session_search` / `session_get`（tool）、
  会话与项目简报资源；`cormex mcp` stdio 桥给 Codex 一类宿主
- agent 循环已从本仓库删除，`routes::cormexd_serves_no_agent_shaped_route` 钉着

**聊天档案**（[设计](design/session-archive-architecture.md) · [计划](plan/session-archive-plan.md)）
- 采集：设备密钥、R 层归档表、keyed 对象存储、CLI 同步引擎（4 MiB 分块、409 双向恢复、预算、锁）
- 幂等（[设计](design/capture-idempotency.md)）：重装 / 丢 state.db / 换设备 key / 多台机器都只录一次，
  靠前缀 hash 校验 + 代际收养；换 key 后残留的文件 id 自愈（v0.1.19 之后）
- 多 home 采集、Codex 桌面元数据随每次同步发送（只发变化）
- 解析与浏览：`cormex-transcript` 解析器、masker + projector、Web 对话视图（跨消息拖选、逐条 / 整段复制）
- 导入：ChatGPT / Claude / Grok / Gemini；浏览器直接吃厂商整包 zip（只上传聊天文件）、
  ChatGPT 分页导出、语音转写
- 加工：distiller（观察日志）、交接简报、按策略晋升与抽取、小样本抽取验证

**交付**
- 双镜像、六平台 CLI、`cormex update` 自更新（校验后原子替换）
- 构建与发布各两条路径（本地 / Actions），共用 `scripts/release.py`；`BUNDLE_SHA256` 显式审批（[release.md](release.md)）
- 部署：`just deploy`（Ansible）与手动触发的 `deploy.yml`，都只拉已发布镜像；`deploy-rehearsal` 演练栈
- 本地构建有诊断、版本准备和续跑：`doctor` 不需要 tag，`prepare-version` 只改版本文件，`build-resume`
  校验已有产物且每次重跑 `ci-full`；阶段与耗时写在候选之外的回执里，编译缓存跨版本复用
- 升级有明确入口与失败边界：`deploy-upgrade FROM_TAG TO_TAG BUNDLE_SHA256` 校验已公开候选，先备份
  数据库再更新 pin / digest，只启动服务端与 Web，再做共享验收；尝试启动后不自动回滚 schema
  （[部署指南](deployment-guide.md#9-用-justfile--ansible-部署验收与演练)）
- `CHANGELOG.md`、README 的 MCP 接入片段与采集说明、`just ci` 里的 evals 题集校验

## 这一轮做了什么（2026-11-08）

1. ✅ **小规模开启抽取。** 设置页「提取范围」：最近 7 / 30 / 90 天或全部，按**对话发生的时间**
   算（今天导入的旧聊天不算「最近」），存成固定起点。migration `20261129000001`。
   **已随 v0.1.20 上线，开关还没开**：由人在设置里选范围、打开开关 —— 这一步要花钱，不由 agent 代做
2. ✅ **抽取侧评测。** `cormex-evals extract` 量的是生产真正在用的抽取器（对话分析 `qa@4`），
   原来的 `run --mode llm` 量的是已经不走的旧路径。24 例、13 类、中英各半；`validate --extraction`
   进了 `just eval-check`。第一跑就抓到一个真 bug：**用户用英文采纳的决定一律被丢弃**（只认中文
   「同意 / 采用」），已修。修后两跑 recall 100%、泄漏 0% —— 这是题集饱和，不是抽取完美
3. ✅ **README 里真实的 `as_of` 对照。** 本地起服务、经 MCP 真跑出来的。录制时又抓到一个回放 bug：
   **同一天里后说的更新，事件时间只写了日期（零点），被当成历史回填，不取代旧值**，两条同时
   「当前」。已修（`effective_time`）
4. ✅ **备份恢复演练**（`scripts/pg-restore-drill.sh`，对着生产库跑）：**通过**，57 张表、
   206 万行恢复回来，差异只有快照之后的新写入。但第一步就发现**生产记忆库此前根本没有备份**：
   节点上的备份容器备份的是另一个 13 MB 的库。演练留下的 dump 是目前唯一一份，见
   [operations.md](operations.md)

## v0.1.20 发版与上线（2026-11-09）

本地构建 → 发布 → `deploy-upgrade v0.1.19 v0.1.20`，第一次完整走新流程，途中修了三处脚本
（都只动工具，不动产物，记在 CHANGELOG 的 Unreleased）：

- **构建器冷启动误报。** `doctor` 只读构建器；停着的构建器不报 BuildKit 版本，第一次构建镜像把它
  拉起来后版本行出现，构建末尾判「运行环境变了」。现在 `build-local` 先 bootstrap 再记录身份
- **Docker Hub 偶发 401。** 查一个还不存在的 tag，约 24 次里有 1 次回 401 而不是 404，发布脚本照设计
  失败关闭，连停两次（都在推送之前）。现在含糊的回答再问三次，仍含糊才停
- **目标机是 Python 3.10。** 目标侧部署脚本用了 3.11 的 `hashlib.file_digest`，第一次升级在备份之后、
  改任何配置之前停下，生产未动。已改，并用测试把目标侧脚本钉在 3.10 语法

上线结果：备份 519 s（1.3 GB），拉镜像、起服务、验收合计约 35 s；新 migration 已应用；
镜像按 digest 钉住，与发布到 registry 的内容指纹一致；Web 公网 `main.dart.js` 与容器内一致。

## 欠账

**质量**
- 检索的弱项：`cross_domain` R@5 0.667；`unanswerable` 的误召回率 0.5（答不出的问题有一半会塞进无关记忆）。
  数字见 [evals/README.md](../evals/README.md)，基线是 2026-08-07 的
- 数据库测试默认 ignored，只有 `just ci-full` 覆盖。2026-11-08 跑出两个迁移测试因为用当前代码
  往旧 schema 里写数据而失败（修了）—— 这类测试要有人定期跑，否则失效了也没人知道
- `injection-drift.py` 只是手工脚本，守的是一条漂了不报错的约束（AGENTS.md 约束 4）
- **谓词漂移让取代失效。** 「用 MinIO 存对象」被抽成 `stores_uploaded_objects_in`，「改用 RustFS」
  被抽成 `uses`：谓词不同就不算矛盾，两条都留在「当前」，回放与现状都会给出过时答案。
  `normalize_predicate` 只合并拼写变体，不合并同义谓词。录 `as_of` 例子时实测撞到（2026-11-08）
- 抽取题集只有 24 例且已饱和：要从生产里真实的误抽 / 漏抽补题，数字才有区分度

**运维**
- `deploy-upgrade` 已经「先 dump 再升」，但**回退没演练过**：尝试启动后不自动回滚 schema，带破坏性
  migration 时怎么用那份 dump 退回旧版本，还没有走过一遍
- **生产记忆库没有定时备份**（2026-11-08 演练发现）。现有的只是演练 dump 与 v0.1.20 升级前的两份
  dump，全部与库同机，没有异地副本，也不定时。要配定时备份 + 异地副本，并定期跑
  `scripts/pg-restore-drill.sh` —— 这是现在最大的风险（按决定排在后面做）
- 生产是单节点
- 目标环境实测，**v0.1.20 上线已验过**：跨 Docker store 的镜像内容身份、带（只增列的）migration 的升级、
  HTTPS 返回的 Web 脚本。**仍未验**：真实客户端在新版本上的端到端、带破坏性 migration 的升级与回退。
  自动 dump 的 `pg_restore --list` 不等于恢复演练，表行数估计不能证明无损

**交付**
- 本地构建只出宿主能编的 CLI（Linux x64、Windows x64）：macOS 与 ARM 用户的 `cormex update` 拿不到
  v0.1.20。要么 Actions 额度够时走 `build-github` 出全六平台，要么给本地补交叉工具链
- 发版脚本的单测有一个需要 Docker（`test_rehearsal_refuses_a_port_that_is_taken`），没 Docker 的环境里
  必然失败，只在 `ci-full` 里真跑

**对外**
- API 没冻结：`/health` 里有 `protocol: 1`，但没有文档说哪些端点算稳定
- 没有 `cargo install`，crates 没发
- README 的 docker 示例钉着 `v0.1.12` 镜像，每次发版都会过时
- `app/` 与 `deploy/` 里的注释还是中文（README 的 Status 如实写着）

## 不做

- **不加第二个存储后端。** Postgres + pgvector 是唯一硬依赖，那是特性不是限制 ——
  双时间轴与四路召回都建在 SQL 上，抽象掉它等于把卖点抽象掉
- **不做最小公倍数式的供应商抽象**（会丢 prompt caching 与 thinking 块）
- **不把 agent 循环搬回来**

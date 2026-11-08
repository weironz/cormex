# Roadmap

> 2026-11-08 重写。上一版停在 v0.1.12，把 CHANGELOG、MCP 接入片段、CI 部署都列成「没做」，
> 而它们都已经做了。这一版按**代码和生产数据**核对过；核对不到的写「未验证」，不写「完成」。

## 现在在哪

**v0.1.19。生产在跑（单节点），仓库公开，两个镜像 + 六平台 CLI 在发。**

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
- `CHANGELOG.md`、README 的 MCP 接入片段与采集说明、`just ci` 里的 evals 题集校验

## 正在做（2026-11-08 起）

按性价比排，前四条是这一轮的工作：

1. **小规模开启抽取。** 全量抽取今天只有一个开关：开了就处理全部会话（生产上 4510 个 + 待导入的
   2258 段），成本和效果都是未知数。加一个「只处理某日之后有活动的会话」的范围，先开最近一段，
   看完成本和事实质量再放大。`qa_pending` 已经按最近活动倒序取，缺的只是边界
2. **抽取侧评测。** 评测门禁今天只管检索：8 道「工具经验」题全是 gold missing，
   「不该抽的没被抽」在 seed 模式下恒为真。要给抽取一个能进门禁的数字
3. **README 里一段真实的 `as_of` 对照。** 双时间轴回放是调研过的 9 个同类项目都没有的能力，
   而 README 里一个例子都没有。要真实跑出来的，不是编的
4. **备份恢复演练。** 备份一直在写，从没恢复过一次。要一份能重复跑的脚本：
   只读 dump 生产库 → 恢复进一次性的空库 → 对上各表行数

## 欠账

**质量**
- 检索的弱项：`cross_domain` R@5 0.667；`unanswerable` 的误召回率 0.5（答不出的问题有一半会塞进无关记忆）。
  数字见 [evals/README.md](../evals/README.md)，基线是 2026-08-07 的
- 数据库测试默认 ignored，只有 `just ci-full` 覆盖。2026-11-08 跑出两个迁移测试因为用当前代码
  往旧 schema 里写数据而失败（修了）—— 这类测试要有人定期跑，否则失效了也没人知道
- `injection-drift.py` 只是手工脚本，守的是一条漂了不报错的约束（AGENTS.md 约束 4）

**运维**
- 第一次带破坏性 schema 变更的升级还没有「先 dump 再升」的退路写成流程
- 生产是单节点，没有异地备份

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

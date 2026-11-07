# 聊天档案：来源、导入与浏览 —— 最终方案

> 状态：**第 1 阶段已实施**（2026-11-07）：来源两维度与 `/archive/sources`、只列有数据的来源、聊天导入入口（ChatGPT / Claude / Grok / Gemini，网页与 CLI）、pi / omp / DeepSeek Harness / Grok Build / Hermes 五个新来源。多目录采集（§6.1）与 Codex 元数据自动同步（§6.3）也已实施。原决策如下。本文汇总 2026-11-07 前后几轮讨论里的问题，每一条都给出结论和理由。
> 写入规则、幂等等既有约束见 [session-archive-architecture.md](session-archive-architecture.md)
> 与 [capture-idempotency.md](capture-idempotency.md)，本文不重复。

## 0. 一页纸

| # | 问题 | 结论 |
|---|---|---|
| 1 | 哪些对话能自动采集 | **只有对话文件落在本机磁盘上的 agent**（编程类为主）。网页 / App 里的聊天存在厂商服务器上，只能导出后导入 |
| 2 | 聊天怎么进来 | 聊天档案里加**导入入口**（拖入导出包 → 预览 → 确认），写进**聊天档案**，与 CLI 的 `cormex import` 走同一条路径。重复导入幂等 |
| 3 | 来源要不要细化成 `chatgpt-chat` 这类 | 要细化，但拆成**两个维度**：厂商 × 形态（聊天 / 编程），而不是一串扁平名字。采集格式 id（`codex`、`claude_code`…）保持不变 |
| 4 | 来源类型要不要让用户增删改查 | **不做来源类型的增删改查。** 没数据的来源不显示；每台设备可以停用某个来源、加自定义目录；用户可自定义的分类交给**标签** |
| 5 | 聊天档案怎么展示 | **平铺 + 分面筛选 + 可切换分组**：默认按最近活动平铺，左侧按形态 / 厂商 / 项目 / 设备 / 时间 / 标签筛选，可切换为「按项目」或「按日期」分组。全文检索能直接跳到命中的那条消息 |
| 6 | 之前遗留的问题 | 多目录采集、项目归属规则、Codex 元数据自动同步、客户端属性、历史重复修复，见 §6 |

## 1. 采集边界：什么能自动采

### 1.1 结论

**自动采集的前提是「对话文件在本机磁盘上」**，与它是聊天还是编程无关。现实中满足这个前提的几乎都是编程类 agent：

| 产品 | 对话存在哪 | 能否自动采 |
|---|---|---|
| Claude Code（CLI / IDE / 桌面 App 的 Code 页） | `~/.claude/projects/` | ✅ 已支持 |
| Codex（CLI / IDE / 桌面版） | `~/.codex/sessions/` | ✅ 已支持 |
| Gemini CLI | `~/.gemini/tmp/` | ✅ 已支持（个人账号 2026-06-18 起停服，历史仍可读，企业账号仍在用） |
| Antigravity CLI（`agy`） | `~/.gemini/antigravity-cli/brain/<id>/…/transcript.jsonl` | ⏳ 待加（§3.4） |
| Antigravity IDE 桌面版 | `~/.gemini/antigravity/conversations/`（据报告是加密的） | ❌ 读不了 |
| Cursor / Cline / Roo / Copilot / Qwen / OpenCode / goose / Aider | 各自本地目录 | ✅ 已在采集目录里 |
| ChatGPT 网页 / App | OpenAI 服务器 | ❌ 只能导出 |
| Claude 网页 / 桌面 App 的聊天 | Anthropic 服务器 | ❌ 只能导出 |
| Gemini 网页 / App | Google 服务器 | ❌ 只能用 Google Takeout 导出 |

### 1.2 为什么不做聊天的自动抓取

可选的路只有两条，都不可取：

- **浏览器扩展**在页面上抓对话：依赖厂商的页面结构，人家一改版就静默失效。而静默失效恰恰是采集系统最不能出的问题：用户以为在录，其实没录。
- **非官方接口**拉历史：违反服务条款，还要 Cormex 保管用户的厂商登录凭据。这和「委托凭据只用调用者自己的」（AGENTS.md 约束 6）是同一类问题：平白多一个越权面。

所以聊天只走**官方导出**，然后把导入做到足够顺手（§2）。浏览器扩展留作以后评估，不进本方案。

## 2. 导入入口

### 2.1 现状

| 路径 | 写到哪 | 有没有界面 |
|---|---|---|
| CLI `cormex import <conversations.json>` | **聊天档案**（每段对话一个会话，snapshot 帧） | 命令行 |
| 服务端 `/import/upload` → `/preview` → `/run` | **记忆**（episodes） | **App 里没有任何入口** |

两条路去向不同：Web 那条是「聊天档案」出现之前的遗留，把导出内容直接写进记忆，绕过了档案。

### 2.2 结论

1. **一条路径：导入只写聊天档案。** 记忆由提取流程从档案里来（现在已经是这样，记忆页上的「已检查 x / N 个聊天」就是从档案数的）。服务端 `/import/*` 改成写档案，与 CLI 共用 `cormex-import` 的解析结果和同一套注册 / 上传逻辑。旧的「直接写 episodes」路径删掉。
2. **App 里的入口**：聊天档案页头加一个「导入」按钮。
   - 拖入或选择文件：ChatGPT 导出 zip、Claude 导出 zip，或者里面的 `conversations.json`。CLI 也直接接受 zip。
   - 上传先落临时文件（现有实现已经是流式落盘），解析后**预览**：识别到哪个平台、共多少段对话、其中新增 / 有变化 / 未变各多少、时间跨度。
   - 用户确认后写入，界面显示进度。完成后筛选器自动切到这批来源。
3. **幂等**：导出里每段对话的 id 就是会话 id（`native_session_id`）。
   - 内容没变：跳过，一个字节都不传。整段对话按快照处理，比较整段内容的哈希。
   - 内容变了（比如你在 ChatGPT 里接着聊过）：开新的一代快照，旧版本保留。
   - 新的导出里少了某段对话（你在厂商那边删了）：**档案里什么都不删**。
   - 所以「每个月导出一次全量、整包拖进来」是推荐用法：只有变化的部分会写入。
4. **附件**：导出包里的图片 / 文件按 `attachment` 角色存进原始层，阅读界面先显示占位。与现有 `imageBuilder` 一致，打开对话不会去请求任何 URL。
5. **以后再加**（第 3 阶段）：
   - Gemini Takeout 解析器（`gemini_takeout`，仅导入）；
   - 「上传会话文件」：给装不了 CLI 的机器用，把 `~/.claude/projects` 之类打成 zip 上传，按采集目录规则拆分；
   - `cormex import --watch <目录>`：盯着下载目录，出现新的导出包就自动走预览和导入，作为可选功能。

## 3. 来源模型

### 3.1 为什么不用 `chatgpt-chat / chatgpt-code / …` 这种扁平名字

方向是对的：用户确实需要按「哪家的」和「聊天还是写代码」来区分。但扁平名字有三个问题：

- **很多工具不属于任何一家模型厂商。** Cursor、Cline、Roo、Aider、OpenCode、goose 可以接任何模型，放不进「厂商-形态」这张格子。
- **名字不准。** Codex 不是 ChatGPT，`chatgpt-code` 会误导人。
- **两个维度挤在一个字段里没法分别筛选。**「所有编程会话」和「所有 OpenAI 的会话」都是常见需求，扁平名字两个都做不好。

### 3.2 结论：一个身份字段 + 三个展示维度

| 字段 | 含义 | 取值举例 | 谁定义 |
|---|---|---|---|
| `agent` | **采集格式 id**，是会话身份 `(agent, native_session_id)` 的一部分 | `codex`、`claude_code`、`chatgpt_export` | 采集目录，**永不改名** |
| `provider` | 厂商 / 出品方 | OpenAI、Anthropic、Google、Cursor、GitHub、阿里、开源社区 | 采集目录的新字段 |
| `kind` | 形态 | `code` 编程 / `chat` 聊天 | 采集目录的新字段 |
| `client` | 客户端（可选） | CLI、桌面版、IDE、导出 | 解析器从原始字节里取（§6.5） |

`agent` 不改名，是因为它已经写进了每一行会话身份里。改名等于让所有已归档的会话换了身份，幂等机制会把它们当成新会话。

你列的六个，在新模型下这样对应：

| 你的叫法 | provider | kind | agent | 界面上显示 |
|---|---|---|---|---|
| chatgpt-chat | OpenAI | chat | `chatgpt_export` | ChatGPT |
| chatgpt-code | OpenAI | code | `codex` | Codex |
| claude-chat | Anthropic | chat | `claude_export` | Claude |
| claude-code | Anthropic | code | `claude_code` | Claude Code |
| gemini-chat | Google | chat | `gemini_takeout`（新增） | Gemini |
| gemini-code | Google | code | `gemini_cli`、`antigravity_cli`（新增） | Gemini CLI / Antigravity CLI |

其余来源同理，例如 Cursor 的 provider 就是 Cursor、kind 是 code。

### 3.3 定义放在哪

`provider`、`kind`、显示名都写进采集目录 `crates/cormex-proto/catalog/agents.toml`。它本来就是「数据不是代码」（D8），CLI 和服务端共用。服务端通过接口把这些字段下发给 App，**App 不再硬编码来源表**（上一轮加的 `app/lib/core/agents.dart` 届时只保留颜色）。

### 3.4 Gemini → Antigravity

- `gemini_cli` **保留**：已归档的历史要能读，企业账号还在用。
- 新增 `antigravity_cli`：对话在 `~/.gemini/antigravity-cli/brain/<conversation-id>/.system_generated/logs/transcript.jsonl`。
  - **风险**：有报告说这个文件是「滚动窗口」，旧内容会被截掉。按现有规则，文件变短就开新一代，那每滚动一次就多存一份几乎相同的内容。
  - 动手前先拿真实样本确认它到底怎么写：如果真的滚动，按**快照**处理，靠内容寻址的块存储去重；如果同目录的 `transcript_full.jsonl` 是完整版，就采那一个。
- Antigravity IDE 桌面版的对话据报告是加密的，**不支持**，并在 `cormex detect` 里明确说明。

## 4. 来源要不要可自定义增删改查

### 4.1 结论：不做来源类型的增删改查

一个来源类型 = 路径规则 + 解析器。解析器是代码，用户在界面上「新增一个来源」什么也解析不了；改名又会破坏会话身份（§3.2）。真正要解决的是下面三件事，各自给出对应的做法：

| 真实需求 | 做法 |
|---|---|
| 「没用过的来源空着摆在那里」 | 筛选器**只列有数据的来源**，每项带数量。数量由服务端按当前筛选条件实时统计（§5.3） |
| 「我用的工具装在别的目录」 | 每台设备可以给任何内置来源加**自定义目录**（§6.1），也可以**停用**某个来源（`cormex sources disable <agent>`）。设备页只读展示每台设备在采什么 |
| 「我想按自己的方式分类」 | **标签**（§5.5）：用户自己建、自己贴，这才是用户可以增删改查的那一层 |

另外两条保留给少数情况：
- 运维可以用现有的 `CORMEX_CAPTURE_CATALOG` 指向一份更新的采集目录，不用等发版就能加路径规则；
- 不认识的工具，以后可以加一个「通用原文」导入：只存原文、按纯文本显示，不解析。放在第 3 阶段之后。

## 5. 聊天档案怎么展示

### 5.1 三个候选

| 方案 | 好处 | 问题 |
|---|---|---|
| A. 按 agent 做顶级分类 | 实现简单 | 同一个项目经常同时用 Claude Code 和 Codex，按工具分会把一件事拆到两处。人回忆的是「那件事」「那周」，不是「用的哪个工具」 |
| B. 全部平铺 | 找最近的东西最快 | 量一大就要靠筛选和检索，没有这两样就是一面墙 |
| C. 平铺 + 标签 / 分面 | 任何维度都能切 | 需要配套的筛选器和检索，开发成本最高 |

### 5.2 结论：C，并保留「按项目」作为一种分组方式

- **默认视图**：所有会话按最近活动平铺。
- **左侧分面筛选**（每项带数量，没数据的不显示）：
  - 形态：编程 / 聊天
  - 来源：按厂商分组列出，如 OpenAI → Codex、ChatGPT
  - 项目
  - 设备
  - 时间：近 7 天 / 近 30 天 / 自定义区间
  - 状态：唯一副本（宿主已删）、来自已归档聊天
  - 标签
- **分组切换**：
  - **按项目**：就是现在这棵树，写代码时最常用；
  - **按日期**：分组标题用绝对日期，如「2026-11」「11月7日」，不用「上周」，理由同 `formatDate` 的注释：档案可以回放到任意时点，相对时间没有基准；
  - **不分组**。
- **三栏布局**（宽屏）：筛选栏 → 结果列表 → 阅读区。窄屏时筛选收进底部弹层，列表和阅读分两页。
- **筛选条件写进地址**（Web）：一个筛好的视图可以收藏、可以发给别人。
- **回放**：加「截至某日」开关，对应 API 已有的 `as_of`。这是产品卖点（「三个月前我知道什么」），聊天档案不该没有。

### 5.3 检索

- **一个搜索框**，同时搜标题、项目名和**对话正文**。正文检索用现有的 `GET /archive/search`（scope=conversation，`transcript_events.tsv`），不另起一套。
- **结果行显示命中片段**，命中词高亮。点开后**直接滚到命中的那条消息**并短暂高亮，不是打开对话再让用户自己找。
- 筛选条件对检索结果同样生效：「近 30 天 + 编程 + cormex 项目 + 搜 `migration`」。
- 以后可以加检索语法（`project:cormex`、`kind:chat`），本方案不做；筛选器能覆盖同样的需求。

### 5.4 让筛选快起来：会话摘要表

上一轮的优化让每次请求按项目现算聚合。加上分面计数以后，一次页面加载要统计五六个维度，每次都现算就又慢回去了。

**结论**：新增派生表 `archive_session_summaries`，每个会话一行：agent、provider、kind、项目、设备集合、首末时间、消息数、标题、唯一副本标记、标签集合。
- 由投影器在会话变化时更新。它和 `archive_message_counts` 一样属于**派生层**，可以随时整表重算，所以允许 upsert。append-only 约束管的是事实、原始层和投影层，不管可重算的缓存。
- 列表、分面计数、分组全部改成查这张表的索引，开销只和结果数量有关。
- 现有的 `/archive/projects` 变成分面接口里「项目」这一维。

### 5.5 标签与手工归类

- **自动维度**（来源、形态、项目、设备、日期）是**派生的**，不存成标签，所以永远不会和数据对不上。
- **用户标签**：用户自己建，可以给会话贴、可以批量贴。存成 append-only 事件表 `archive_session_tag_events`（打标 / 去标），当前标签由事件推导，和 `fact_events` 同一个思路。
- **手工归入项目**：用户可以在 Cormex 里把一段会话归到某个项目，或移出项目。同样是 append-only 事件，**优先级最高**（§6.2）。这也解决了「导入的聊天没有项目」：ChatGPT / Claude 的聊天可以手工归到你的项目里，和相关的编程会话放在一起。

## 6. 之前讨论遗留的问题

### 6.1 多目录采集

**问题**：每个来源只扫第一个存在的目录。设了 `CODEX_HOME` 就不再看 `~/.codex`；定时任务的进程里往往没有你 shell 里设的变量。

**结论**：
- 每个来源扫**所有存在的目录**：环境变量指向的、默认的，再加配置项 `extra_homes`。不同目录里的同一段会话由幂等机制合并。
- `cormex init` 安装定时任务时，把当时看到的 `CODEX_HOME` / `CLAUDE_CONFIG_DIR` 写进 cormex 自己的配置，不依赖定时任务的环境。
- `cormex doctor` 发现多个目录都有会话、或者变量指向的目录不存在时，给出提示。

### 6.2 项目归属规则

按优先级取第一个有值的：

1. **Cormex 里的手工归类**（§5.5）
2. **宿主元数据**：每台设备一份，取最新的；宿主明确标了「不属于任何项目」也照此处理
3. **会话最新文件的起始工作目录**：有 git remote 用 remote，否则用本地路径

**会话中途 `cd` 到别的目录，不改变项目归属**，以会话开始时所在的目录为准。理由：一段会话的主题由它从哪里开始决定，中途切目录多是临时查东西；而且按最后所在目录算，同一段会话的项目会随着聊天变来变去。项目目录改名这类情况，用第 1 条手工归类兜底。

### 6.3 Codex 桌面版元数据自动同步

每次同步（hook 和定时任务都算）都读取 Codex 桌面版的标题、项目归属、置顶信息，并把每个会话的元数据哈希记在 `state.db` 里，**只在变化时上报**。`--codex-metadata` 参数保留，用于强制全量重传。

### 6.4 Claude 桌面版

- 对话正文据现有资料就在 `~/.claude/projects/`，已经在采。
- 桌面版另有一份自己的会话索引（macOS 上在 `~/Library/Application Support/Claude/local-agent-mode-sessions/`，Windows 上在 `Claude-3p/claude-code-sessions/`），里面可能有标题等元数据。
- **先在真实机器上看文件**：只确认文件结构，不看内容。确认里面只有元数据、没有正文后，按 Codex 桌面版的方式把标题当元数据采进来。如果发现桌面版有正文只存在这个目录里，就把它加进采集目录。

### 6.5 客户端属性

解析器从原始字节里取出客户端：Codex 的 `session_meta.originator` / `source`，Claude Code 的 `entrypoint`。解析器版本加一，已归档的会话会重新投影，自动补上这个属性，**不用重新采集**。动手前先拿到各客户端的真实取值（脱敏样本）。

### 6.6 已经发生的历史重复

修复之前已经录重复的会话，用一次性修复任务处理：补跟随记录、重新投影，只增不删。任务已单独排队（见 [capture-idempotency.md](capture-idempotency.md) §8）。

### 6.7 记忆页

视觉风格向聊天档案看齐，放在第 2 阶段末尾。

## 7. 分阶段

| 阶段 | 内容 | 验收 |
|---|---|---|
| **1** | 采集目录加 `provider` / `kind`；分面接口（先按现算实现）；筛选器只列有数据的来源；导入入口（ChatGPT / Claude，写聊天档案，删掉写 episodes 的旧路径，CLI 接受 zip）；Codex 元数据自动同步；多目录采集 | 同一个导出包导入两次，第二次写入 0 字节；改过一段对话后再导入，只多出这一段的新一代；空来源不出现在筛选里；设了 `CODEX_HOME` 的机器上，定时任务和 hook 采到同一批会话 |
| **2** | 会话摘要表；新的浏览界面（平铺 + 分面 + 分组切换 + 地址里带筛选条件）；正文检索并跳到命中消息；手工归入项目；回放开关；记忆页改版 | 10 万段会话的档案上，任意筛选组合的列表加分面计数 < 100 ms；检索结果点开后命中的消息在可视区内 |
| **3** | `antigravity_cli`（先验证滚动窗口行为）；Gemini Takeout；客户端属性；用户标签；上传会话文件；Claude 桌面版元数据；`import --watch` | 每个新来源至少两份脱敏的真实样本进测试集 |

## 8. 需要在真实机器上确认的

以下几点在这台云端机器上确认不了，开工对应的部分之前，要在装了这些工具的机器上看一眼（只看结构和字段名，不看对话内容）：

1. Claude 桌面版那两个目录里有什么文件（§6.4）
2. Codex `originator`、Claude Code `entrypoint` 在各个客户端下的实际取值（§6.5）
3. Antigravity CLI 的 `transcript.jsonl` 会不会截掉旧内容，`transcript_full.jsonl` 是否完整（§3.4）

## 参考

- Gemini CLI 停用与 Antigravity CLI：[The New Stack](https://thenewstack.io/gemini-cli-antigravity-replacement/)、[Google Cloud 博客](https://cloud.google.com/blog/ja/topics/developers-practitioners/choosing-antigravity-or-gemini-cli)
- Antigravity CLI 的存储位置：[Antigravity CLI Local History](https://jazzyalex.github.io/agent-sessions/guides/antigravity-cli-local-history.html)、[Where Antigravity CLI stores session history](https://sessions-viewer.js-bridge.com/agents/antigravity-cli)
- Claude 桌面版的会话索引：[anthropics/claude-code#62427](https://github.com/anthropics/claude-code/issues/62427)、[#50891](https://github.com/anthropics/claude-code/issues/50891)
- Codex 桌面版与 CLI 共用 `CODEX_HOME`：[openai/codex#39195](https://github.com/openai/codex/issues/39195)、[#21079](https://github.com/openai/codex/issues/21079)

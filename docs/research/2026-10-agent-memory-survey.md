# AI agent 记忆系统调研 + 「多 agent 会话中央采集」需求评审

> 日期：2026-10-29 · 状态：**待评审**（评审通过后再出架构设计与代码级开发计划）
>
> 这份文档回答三件事：
> 1. 主流 agent 记忆系统 / 记忆插件现在长什么样，哪些值得 Cormex 吸收（§1–§5）
> 2. 这些对 Cormex 的定位和重构意味着什么（§6）
> 3. 对「多 agent 会话中央采集」需求的评审：哪里对、哪里要改、推荐怎么做（§7–§9）
>
> 不想读全文的话，§0 和 §9 各一页。
>
> **相关文档**（2026-08 的上一轮调研与设计，2026-10-29 从 Cortex 仓库迁入，正文未改）：
> - [memory.md](../memory.md)：记忆系统怎么工作（存储模型、双时间轴、检索、同步）
> - [memory-content.md](../memory-content.md)：什么东西有资格进记忆（58 个系统的全景调研）
>
> 两轮调研的一致、冲突与裁决见 §10。

---

## 0. 摘要

**关于主流记忆系统：**

- **检索已经收敛**：语义、BM25、实体/图、时间几路并行召回再融合，是 Mem0（2026-04 新算法）、Graphiti、Hindsight 的共同答案。Cormex 的四路召回加 RRF 只够入场，算不上差异。
- **写入从「改写」转向「只追加」**：Mem0 2026-04 的新算法明确改成 ADD-only，不再 UPDATE/DELETE；Graphiti 让失效的边留在原处，不删除。Cormex 的 append-only 得到了验证，但也不再是独有的。
- **⚠️ CLAUDE.md 里「调研过的 9 个同类项目没有一个做双时间轴回放」这句已经不成立**（README 里「no other memory layer does all three」也站不住）。Graphiti/Zep 明确是双时间轴（`valid_at/invalid_at` + `created_at/expired_at`），文档还写了「某时刻系统知道什么」这种查询。Cormex 还独有的东西变成了：**注入回放**（`episode_memories` 记下每一轮实际注入了哪些记忆，所以能回放「agent 当时看到了什么」，而不只是「库里当时有什么」）、**确定性失效**（不让 LLM 裁决，失效原因分四种），以及**按信任分层的出处**。见 §6.1。
- **原文的价值被重新认识**：MemPalace 逐字存原文，用普通 embedding 检索，在 LongMemEval 上自报 R@5 96.6%（这是召回，不是 QA 准确率，有争议，见 §2.6）。它自家的压缩方案让同一指标掉到 84.2%。结论是：**抽取和压缩一定会丢信息，原文必须留着，而且要能检索。**
- **采集方式在变**：越来越少靠 agent 主动调 `remember`，越来越多靠**宿主 hooks 加读磁盘上的会话日志**。claude-mem 挂了 6 个生命周期 hook，宿主没有 hook 就直接监听日志文件；MemPalace 给 Claude Code、Codex、Cursor 都配了自动保存 hook。2026 年 Codex、Cursor、Gemini CLI 都有了 hooks。

**关于你的需求：**

- **方向对，而且是 Cormex 最该做的一件事**。所有 agent、所有机器上的原始会话，字节级保真地进中央库，再由 Cormex 自己加工。这正好落在 Cormex 已有的三条纪律上（append-only、出处追到源、双时间轴），还会长出一条比「双时间轴」难复制得多的护城河：**跨 agent 的会话飞行记录仪**。
- **但有三处主语和边界要改**（详见 §8.2）：
  1. **录入的主语不是 agent，是本机采集器**。各 agent 的会话本来就在磁盘上，由一个 `cormex` 守护进程只读去采；MCP 不承担批量录入，hooks 只负责「通知」。
  2. **「按原始格式」说的是存储层字节保真，不是展示层**。UI 不可能直接渲染二十几种格式，所以要分三层：原始字节 → 归一化投影（带版本，可重算） → 派生物。
  3. **「取回继续聊」要拆成两条路**。同一个 agent 换台机器，把原始文件还原回去，用它自己的 `--resume` 原生续聊；换一个 agent，给一份**交接简报**，原文按需逐层钻取。不要把原文直接灌进上下文。
- **✅ 已拍板（2026-10-29）**：2026-08 的调研把「工具输出的原始字节」定为**明确不存**（理由是检索污染、密钥永久化、无限膨胀）。而 Claude Code、Codex 的会话文件里本来就逐字带着工具输出，接受「按原始格式录入」就等于推翻那条定案。**决定：只在归档层推翻**（选项 A），默认的检索和注入不碰工具输出原文；同时**原文永远对所有者可读、可显式搜索**，每条记忆都能还原到原文。见 §10.2。
- **还有一件今天就该做的事**：Claude Code **默认在启动时删除 30 天前的会话文件**（`cleanupPeriodDays`）。在采集器上线之前，先把每台机器上的这个值调大，并备份 `~/.claude/projects`，见 §8.2 第 8 条。

---

## 1. 调研范围与方法

**看了什么**：13 个记忆系统和插件（§2），3 个会话采集类工具（§3），各家 agent 的会话存储格式和 hooks 现状（§8.3），以及记忆投毒、基准测试、MCP 协议这几个横切话题。

**信息来自哪**：

- 第一手：各项目发在 **PyPI / npm / crates.io 上的 README、包元数据和源码**（这个会话的网络策略拦掉了大部分网站，包仓库能访问）
- 第二手：网页搜索得到的官方文档摘要和第三方评测。**凡是只来自第二手的说法，文中都标了「据报道」或「未核实」**
- 上一轮：2026-08 的 [memory-content.md](../memory-content.md)（58 个系统，四轮一手来源核证）。本文是它之后的**增量**，重点在它没覆盖的采集与取回，不重复它已经核证过的内容

**读数字之前要知道**：

- **几乎所有 benchmark 分数都是厂商自报**，而且口径不同：有的是检索召回（R@k），有的是端到端 QA 准确率；top_k 也不一样；还有托管版和开源版的区别（Mem0 的 README 自己就写了托管版有开源版没有的优化）。§4 的表**只列口径，不排名次**
- 这是一份 2026-10 的快照。记忆这个领域每月都在变，会话格式也是（比如 VS Code Copilot 一年里换了三代存储，见 §8.3）

---

## 2. 主流记忆系统逐个看

### 2.1 Mem0（Apache-2.0，`mem0ai` 2.2.1，2026-09）

- **2026-04 换了新算法**，README 原话：*"Single-pass ADD-only extraction — one LLM call, no UPDATE/DELETE. Memories accumulate; nothing is overwritten."* 这是对它自己老设计（LLM 决定 ADD/UPDATE/DELETE/NOOP）的否定
- 新算法的另外几点：agent 生成的事实（agent 确认做了某个动作）和用户的话同等对待；实体抽取后做向量化和跨记忆链接；**多信号检索**（语义 + BM25 + 实体匹配并行打分再融合）；**时间推理**（查询「现在」「过去」「将来」时能挑对日期的那一条）
- 自报分数（托管版）：LoCoMo 92.5、LongMemEval 94.4、BEAM-1M 64.1、BEAM-10M 48.6。第三方对开源版的测试低得多（据报道 LongMemEval 在 32–49% 之间）
- **分发方式值得学**：`mem0 init --agent` 让 agent 自己在 5 秒内开出 API key，人之后再认领；`npx skills add` 发 Agent Skills；有 CLI
- 有自托管服务（docker compose）和云

**对 Cormex 的启示**：最大的同类自己走到了 append-only，说明方向对，但这不再是差异点。「agent 生成的事实」在 Cormex 里由信任分层表达（`conversation` / `tool_output`），比 Mem0 的「同等权重」更细。

### 2.2 Graphiti / Zep（Graphiti Apache-2.0，`graphiti-core` 0.31.2，2026-10；Zep 云闭源）

- **时间上下文图谱**：实体（节点）、事实（带有效期窗口的边）、episodes（原始数据，所有派生物都追溯到它）
- **显式双时间轴**：边上有 `valid_at` / `invalid_at`（现实中何时为真）和 `created_at` / `expired_at`（系统何时记下、何时作废）。文档原话：*"What did the system know about X on date Y?" versus "What was actually true about X on date Y?"*
- 失效靠 **LLM 判定矛盾**，然后在**旧边上**写 `expired_at` / `invalid_at`（据其文档和论文的描述）。不删，但会改原边
- 混合检索：语义 + BM25 + 图遍历
- 需要单独的图数据库：Neo4j / FalkorDB / Neptune（Kuzu 已弃用）
- Zep 论文（arXiv 2501.13956）自报 LongMemEval 71.2%（GPT-4o）

**对 Cormex 的启示**：这是和 Cormex 理念最接近的一个，也正是它让「没有人做双时间轴」这句话不成立了。两边真正的分歧在于：**Cormex 不让 LLM 裁决失效**（`extract.rs` 第 2 条规则：失效是不可逆的 append-only 写入，模型的判断既不稳定也不可复现），失效原因分四种并且各有不同的时间语义，并且只依赖 Postgres。

### 2.3 Letta（Apache-2.0，`letta` 0.34.x；Letta Code 0.34.27，2026-10）

- 前身 MemGPT。**core memory blocks**（常驻上下文，agent 用工具自己改写）、**recall memory**（对话历史）、**archival memory**（向量库）
- **sleep-time agents**：主 agent 没有改记忆的工具，由后台的 sleep-time agent 异步整理、去重、归档（文档标为 experimental）
- **MemFS / context repositories**（2026）：记忆是 **git 仓库里的 Markdown 文件**，每次修改都是一次 commit。`system/` 下的文件每轮进 system prompt，其余文件只露出文件树、按需读；后台的 dreaming / defrag 子 agent 在 git worktree 里整理，不阻塞主 agent
- 默认没有向量索引（要装 MemFS Search mod）
- **前提是你得把 Letta 当成 agent 运行时用**

**对 Cormex 的启示**：git 历史本身就是一条「系统时间轴」，Letta 也在往「历史可回溯」走。「常驻块 + 按需文件树」这种分法和 Cormex 的「profile resource + search tool」是同一个思路。Cormex 不做运行时（CLAUDE.md：这里不跑 agent 循环），所以只借思想，不借形态。

### 2.4 Hindsight（MIT，`hindsight-api` 1.1.0，2026-10）

- **Postgres + pgvector**，自带 REST 和 `/mcp`。**这是架构上和 Cormex 最像的同类**
- 三个操作：retain / recall / reflect。四张逻辑网络：world（世界事实）、experience（agent 经历）、opinion（会演化的信念）、observation / entity summary（不同版本叫法不同）
- **TEMPR** 召回：语义、关键词、图、**时间** 四路并行，融合后重排，再按 token 预算截断
- **CARA** reflect：可配置的「性格」参数（怀疑度、字面度、共情）和一张会演化的观点网络
- 论文 arXiv 2512.12818；自报 LongMemEval 91.4%，BEAM-10M 64.1%（自家对比页）

**对 Cormex 的启示**：说明「只依赖 Postgres」这条路有人走通了，Cormex 不必为了「专业」去加图数据库。时间作为召回的一路（不只是过滤条件），值得在 evals 里做一次 A/B（Cormex 的 recency 通道现在默认关，原因是评测里它让排序变差了，见 `retrieval.rs`）。

### 2.5 Mastra Observational Memory（TypeScript 框架的一项功能）

- 两个后台 agent：**Observer** 在对话历史达到阈值（默认 30k token）时，把历史压成**带日期的观察条目**；**Reflector** 在观察条目达到阈值（默认 40k）时合并、去冗余
- **观察是追加的，只有 reflect 时才整体重建**，所以对前缀缓存友好。这和 Cormex 约束 4 担心的是同一件事
- 压缩比：纯文本 3–6 倍，**工具输出多的 agent 负载 5–40 倍**
- 自报 LongMemEval 94.87%（GPT-5-mini）、84.23%（GPT-4o，它自家的 RAG 实现是 80.05%）

**对 Cormex 的启示**：**coding 会话的体积大头是工具输出**。§7 的需求里那句「后续精简、总结、压缩」，形态就应该是这种追加式的观察日志：它天然是 append-only 的，天然对缓存友好，也天然带日期，正好接上双时间轴。

### 2.6 MemPalace（MIT，`mempalace` 3.11.0，2026-10）

- **逐字存原文，不摘要、不抽取、不改写**。索引按 wing（人 / 项目）→ room（主题）→ drawer（原文）组织，检索可以限定范围
- 默认后端 ChromaDB，可插拔（还有 sqlite_exact、Rust 原生、milvus、qdrant、**pgvector**）
- **直接「挖」Claude Code 会话**：`mempalace mine ~/.claude/projects/ --mode convos`。给 Claude Code、Codex、Cursor 配**自动保存 hooks**（定期保存，以及上下文压缩之前保存）
- README 顶上醒目地警告：**"Claude Code sessions expire in 30 days without auto-save hooks wired."**
- 带有效期窗口的实体关系图（本地 SQLite）；47 个 MCP 工具；有团队 hub 模式
- 自报：LongMemEval R@5 96.6%（纯原文 + 语义检索，不用 LLM），held-out 98.4%
- **争议**（据报道，多为第三方）：
  - 它报的是**检索召回**，同一张对比表里别家报的是**端到端 QA 准确率**，不可比
  - 96.6% 基本就是 ChromaDB 默认 embedding（all-MiniLM-L6-v2）作用在原文分块上的效果，不需要 palace 结构就能复现
  - 曾经的「100%」是针对失败题做定向修复加 LLM 重排后得到的，已撤下
  - 它主打「无损压缩」的 AAAK 让同一指标从 96.6% 掉到 84.2%
  - LoCoMo「100%」用的是 top_k=50，差不多等于把整段对话都取回来了
  - 有一篇专门批评它的论文（arXiv 2604.21284）

**对 Cormex 的启示**：两条，都很重要。① **原文 + 好的 embedding 已经是一条很强的基线**，任何抽取 / 压缩方案都要先打过它。② **不要追分**，MemPalace 是现成的反面教材；Cormex 的 evals 要坚持「口径写清楚、不教题」。

### 2.7 claude-mem（npm 元数据为 Apache-2.0，`claude-mem` 15.5.0，2026-10）

- Claude Code 插件，现在也支持 OpenCode、Codex（经由 T3 Code）、Antigravity 等十来个宿主
- **6 个生命周期 hook**：SessionStart、UserPromptSubmit、PreToolUse、PostToolUse、Stop、SessionEnd。每个 hook 只是一条命令，交给本机 worker（Bun 管理的 HTTP 服务）处理
- **宿主没有 hooks 时就监听宿主的聊天日志文件**（README 原话：*"Grok Bot has no host hooks, so we watch the chat log files."*）
- 存储：SQLite + FTS5 + Chroma。存的是 LLM **压缩后的 observation**，不是完整原文
- **三层渐进披露的 MCP 工具**：`search`（紧凑索引，每条约 50–100 token）→ `timeline`（某条前后的时间线）→ `get_observations`（按 id 批量取详情，每条约 500–1000 token），自称省 10 倍 token
- `<private>` 标签可以把敏感内容排除在存储之外；有 Web viewer 和团队共享（TeamBrain）

**对 Cormex 的启示**：**这几乎就是 §7 需求的「单宿主、本机版」**：hooks 采集、本机 worker、Web 浏览、MCP 取回。Cormex 要做的是它的「多宿主、多机器、中央存储、原文保真」版。三层渐进披露应该直接借用到 Cormex 的会话取回工具上（§8.4.4）。

### 2.8 Cognee（Apache-2.0，`cognee` 1.6.x，2026-10）

- ECL 管线（Extract → Cognify → Load）：文档、代码、对话都转成知识图谱 + 向量
- **可以不用 LLM**：用 CPU 上的本地小模型（gliner）做抽取
- 支持自定义本体，有 MCP，有云服务

**对 Cormex 的启示**：「无 LLM 也能跑」的降级路径对自托管用户有价值。Cormex 已经有本地 embedding（`embed_local.rs`），抽取这边还没有。

### 2.9 Supermemory（SDK Apache-2.0，服务以托管为主）

- 一个 API 覆盖摄取、抽取、用户画像、生命周期、连接器、检索
- 连接器：Notion、Google Drive、OneDrive、GitHub、S3、网页爬虫；音频自动转写
- MCP server 带**交互式记忆图谱**（MCP Apps），有 CLI
- 开源了一个评测平台 **MemoryBench**，可以横向比较不同的记忆提供方

### 2.10 Basic Memory（AGPL-3.0，`basic-memory` 0.24.x）

- **本地 Markdown 文件**就是记忆，人和 AI 写同一批文件，双向同步；观察 + wikilink 构成知识图谱；有语义检索
- MCP 工具标了行为提示（只读 / 破坏性 / 幂等），方便 agent 挑工具

### 2.11 MemOS / EverMemOS（国内）

- **MemOS**（MemTensor）：把记忆当成「系统资源」来管理，基本单位 MemCube 同时封装内容和元数据（出处、版本）。2.0「Stardust」发布于 2025-12；2026 年出了 OpenClaw / Hermes Agent 插件；自报 LoCoMo 88.83、LongMemEval 89.20
- **EverMemOS**（EverMind）：2026-02 发布产品级版本，开源核心加云服务，生命周期设计受「记忆印迹」（engram）启发

### 2.12 宿主自带的记忆

- **Claude Code**：用户写的 `CLAUDE.md`（多级作用域），加上 2026-02 起默认开启的 **auto memory**：模型自己记笔记，写在 `~/.claude/projects/<project>/memory/MEMORY.md` 加主题文件里。据报道只自动加载 MEMORY.md 的前 200 行或前 25KB，**只在本机，不共享**
- Codex、Gemini CLI 也各有一套「把记忆写进 Markdown 文件」的机制

**对 Cormex 的启示**：宿主自带的记忆都是**单机、单宿主**的。「跨宿主、跨机器」正是它们的空白，也是 §7 需求的价值所在。

### 2.13 LangMem（MIT）

- semantic / episodic / procedural 三类记忆，分 hot-path 和后台两种写入模式
- `langmem` 最后一次发版是 2025-10-27（0.0.30），**一年没更新了**。思想还可以参考，项目本身已经不活跃

---

## 3. 相邻品类：会话采集 / 检索工具

这一类不做「记忆」，只做「把各 agent 散落的会话收拢起来」，和 §7 的需求直接重叠。

### 3.1 cass —— coding_agent_session_search（Rust，crate `coding-agent-search` 0.7.0）

- 把 **26 个本地 agent** 的历史索引成一个统一归档：SQLite 是真相来源，Tantivy 做词法检索，可选本地语义检索；有 TUI、HTML 导出，以及给 agent 用的 `--robot` / `--json` 模式
- 带一个**原始镜像**（blake3 内容寻址），因为宿主会自己清理日志，cass 的归档可能是唯一剩下的副本
- 多机器靠 **SSH / rsync 拉取**（pull 模式）
- ⚠️ **许可证不是标准 MIT**：附加条款（rider）明确规定，**OpenAI、Anthropic 以及任何代表它们行事的人**都不享有任何权利，并且把「use」定义为包括 *analyzing, indexing … incorporating into any … pipeline for machine learning or other automated systems*

**结论：Cormex 不依赖、不 vendor、不抄 cass 的代码。** 这不只是许可证兼容问题（Cormex 是 Apache-2.0）。这个仓库日常就是由 Claude Code / Codex 之类的 agent 开发的，引入一个对 Anthropic / OpenAI 关联使用设限的依赖，等于给自己的开发方式埋雷。各家 agent 的文件放在哪、是什么格式，这类**事实**属于公开知识，可以独立实现。

### 3.2 franken_agent_detection（Rust，MIT，crate 0.3.10）

- cass 同一个作者拆出来的连接器库，40 多个连接器，内部有归一化的会话模型；crate 里的 LICENSE 是**标准 MIT**（不带 rider）
- 依赖了作者自研的 `frankensqlite`（SQLite 实现）和 `asupersync`（异步运行时），**和 Cormex 的 tokio + sqlx 体系不合**
- 它的模块文档对各格式的说明很有价值（比如 VS Code Copilot 的三代存储、Cursor 的 KV 布局），§8.3 的表参考了它

**结论**：作为**格式参考**有价值；不作为依赖（运行时不合）。如果将来借鉴它的具体代码，按 MIT 要求在 NOTICE 里署名，并且每次都核对当时版本的许可证。

### 3.3 ccusage（MIT，20.0.45）

- 只读本地日志做**用量统计**，覆盖 18 个来源（Claude Code、Codex、OpenCode、Amp、Droid、Goose、Copilot CLI、Gemini CLI、Antigravity、Kimi、Qwen、OpenClaw、Grok Build……）
- 已经改成**原生二进制 + npm 包装**的分发方式（`npx ccusage` 会按平台拉对应的原生包，源码结构是 `adapters/<agent>/src/{loader,parser}.rs`）

**结论**：这种分发方式（原生二进制、npm 包装、各平台子包）值得照搬给 `cormex` CLI。用 AI 编程工具的人几乎都有 node，`npx cormex` 是摩擦最小的一条安装路径。

---

## 4. 横向对比

| 项目 | 形态 | 存储 | 写入方式 | 时间模型 | 原文保留 | 取回接口 | 许可 |
|---|---|---|---|---|---|---|---|
| Mem0 | 库 / 自托管 / 云 | 向量库 + 可选图 | 单次 LLM，**只 ADD**（2026-04 起） | 时间感知排序 | 不以原文为中心 | SDK / REST / CLI / MCP | Apache-2.0 |
| Graphiti / Zep | 框架 / 托管 | Neo4j / FalkorDB / Neptune | LLM 抽实体和边，LLM 判冲突 | **显式双时间轴** | episodes 作为出处 | SDK / MCP | Apache-2.0（Zep 闭源） |
| Letta | agent 运行时 | Postgres / git（MemFS） | agent 自己改记忆块，sleep-time 后台整理 | git 历史 | recall memory | 必须用 Letta 运行时 | Apache-2.0 |
| Hindsight | 服务 | **Postgres + pgvector** | retain：LLM 抽取 + 实体 / 时间 | 时间是召回的一路 | — | REST / `/mcp` | MIT |
| Mastra OM | 框架功能 | 框架存储 | Observer / Reflector 后台压缩 | 观察带日期 | 被观察日志取代 | 框架 API | 见其仓库 |
| MemPalace | 本地 CLI / MCP / 团队 hub | ChromaDB（可换 pgvector） | **不抽取，逐字入库** | 图谱带有效期窗口 | **逐字** | CLI / 47 个 MCP 工具 / hooks | MIT |
| claude-mem | 宿主插件 + 本机 worker | SQLite + FTS5 + Chroma | hooks 采集 → LLM 压缩成 observation | — | 只存压缩后的观察 | 三层渐进披露 MCP + viewer | Apache-2.0 |
| Cognee | 库 / 服务 | 图 + 向量 | ECL，可以不用 LLM | — | — | SDK / MCP | Apache-2.0 |
| Supermemory | 托管 API | 托管 | 抽取 + 画像 + 生命周期 | — | 文档 + 记忆 | API / MCP / CLI | SDK Apache-2.0 |
| Basic Memory | 本地 Markdown | 文件 + 索引 | 人和 AI 共写 | 文件历史 | 笔记即原文 | MCP | AGPL-3.0 |
| cass | 本地 CLI / TUI | SQLite + Tantivy + 原始镜像 | 只读采集 26 个 agent | — | **原始镜像** | CLI / robot 模式 | MIT + rider ⚠️ |
| **Cormex（现状）** | 服务 / MCP / Web | **Postgres + pgvector** + S3 | 便宜模型抽取 + **确定性冲突裁决**，append-only | **双时间轴 + 注入回放** | L0 `episodes` | REST / MCP / WS | Apache-2.0 |

自报分数只列口径，不比高低：

| 项目 | 指标 | 数值 | 口径备注 |
|---|---|---|---|
| Mem0（托管） | LongMemEval / LoCoMo / BEAM-1M | 94.4 / 92.5 / 64.1 | QA 准确率，top_200 检索预算；开源版第三方测得低很多 |
| Mastra OM | LongMemEval | 94.87（GPT-5-mini） | QA 准确率 |
| Hindsight | LongMemEval / BEAM-10M | 91.4 / 64.1 | QA 准确率 |
| MemOS | LongMemEval / LoCoMo | 89.20 / 88.83 | 自报 |
| Zep | LongMemEval | 71.2（GPT-4o） | 论文 |
| MemPalace | LongMemEval R@5 | 96.6（raw）/ 98.4（held-out） | **检索召回，不是 QA**，有争议 |

---

## 5. 趋势与共识

1. **检索：多路并行 + 融合 + 时间感知**。Mem0（语义 + BM25 + 实体）、Graphiti（语义 + BM25 + 图）、Hindsight（语义 + 关键词 + 图 + 时间）。差别在于时间是一路召回、一个过滤条件，还是一个排序信号
2. **写入：只追加，不覆盖**。Mem0 推翻了自己的 UPDATE/DELETE，Graphiti 失效不删除。分歧在于失效由谁判：LLM（Graphiti）还是确定性规则（Cormex）
3. **原文回潮**。MemPalace 那组数字的争议在于口径，但「原文 + 好的 embedding 是很强的基线」这一点没人能推翻。工具输出多的 coding 会话里，抽取和压缩丢掉的恰恰是之后最想查的细节（报错原文、命令、路径）
4. **压缩：追加式观察日志 > 反复重写摘要**。Mastra 的 Observer / Reflector、claude-mem 的 observation 都是这样。对前缀缓存友好，每条观察都带日期
5. **后台整理成了标配**：Letta 的 sleep-time / dreaming、Mastra 的 Reflector、Hindsight 的 reflect。整理不阻塞对话，也可以换便宜模型做
6. **取回：渐进披露**。claude-mem 的索引 → 时间线 → 详情，Letta 的常驻 `system/` + 按需文件树，Claude Code MEMORY.md 只加载前 200 行。**先给目录，再给正文**
7. **采集：从「agent 主动记」转向「宿主 hooks + 读日志」**。Codex（2026，PreCompact/PostCompact 等事件持续在加）、Cursor（beforeSubmitPrompt / afterAgentResponse / stop / preCompact）、Gemini CLI（v0.26+）都有了 hooks；hook 的输入普遍带 `transcript_path`
8. **Markdown 文件作为记忆介质**：Letta MemFS、Basic Memory、Claude Code auto memory。agent 天然会读写文件，git 天然有历史
9. **分发：一行安装 + agent 自助开通 + Agent Skills**。`mem0 init --agent`、`npx skills add`、MemPalace 的安装 skill、`npx ccusage`
10. **评测：没有可信的公共排行榜**。LongMemEval / LoCoMo 被刷到 90 分以上，口径各异；更难的 BEAM（ICLR 2026，到 10M token）开始流行；开源评测框架有 Mem0 的 `memory-benchmarks` 和 Supermemory 的 MemoryBench
11. **安全：记忆投毒进了 OWASP Agentic Top 10（ASI06）**。MINJA 仅凭普通查询就能把恶意条目写进 agent 的长期记忆（据引用，注入成功率超过 95%）；AgentPoison 投毒率低于 0.1% 时攻击成功率仍达 80% 以上。防御大多停留在推理时过滤

---

## 6. 对 Cormex 的含义

### 6.1 差异点要重新校准

**「没有人做双时间轴回放」这句要改。** Graphiti 有了，而且写在 README 第一屏。继续这么说，读者一查就会觉得 Cormex 不老实。其实 2026-08 的 [memory-content.md](../memory-content.md) §一.5 早就写了「双时间轴是通用语，不是方言」，所以 CLAUDE.md 那句话和本项目自己的调研结论也是矛盾的。那份调研给出的准确说法是：**编码 agent 里**没有一家有这个能力。

在本次调研范围内，Cormex **仍然独有**、并且站得住的：

| | Cormex | 最接近的同类 |
|---|---|---|
| **注入回放** | `episode_memories` 记下每一轮**实际注入了哪些记忆**，回放的是「agent 当时看到了什么」 | 没看到别家做。Graphiti 能回放「库里当时有什么」，但回放不了「当时喂给模型的是什么」 |
| **确定性失效** | 冲突裁决不问 LLM；失效原因分四种（superseded / corrected / retracted / expired），时间语义各不相同 | Graphiti 由 LLM 判冲突，并改写旧边的时间戳 |
| **信任分层的出处** | `source_channel` + `trust_tier` 由 CHECK 约束锁死 | Mem0 新算法反而把 agent 生成的事实提到了「同等权重」 |
| **只依赖 Postgres** | 双时间轴和四路召回都建在 SQL 上 | Hindsight 同路；Graphiti 要图数据库 |
| **缓存友好的注入分工** | profile 走 resource，检索走 tool；框定只在一处实现 | Mastra 的观察日志在思路上一致 |
| **中文** | jieba 分词的 BM25，中英双语评测集 | 主流评测集全是英文 |

**§7 的需求落地之后，会多出一条更硬的：**

> **跨 agent、跨机器、字节级保真的原始会话档案，带双时间轴索引。**
> 「我所有的 agent 在任意一个过去的时刻知道什么、看到了什么、做了什么，凭什么这么认为。」

它比「双时间轴」难复制得多。难的不是算法，是**二十几种格式的长期维护**加上**append-only 的原始层**。MemPalace（原文 + 挖 Claude Code）、claude-mem（hooks + 本机 viewer）、cass（多 agent 采集 + 原始镜像）各做了一部分，**没有人把它们和服务端记忆、双时间轴、MCP 放在一起**。

建议评审通过后再改 README 和 CLAUDE.md 的措辞（这次提交没有改）。

### 6.2 保留什么

CLAUDE.md 里 6 条不可违反的约束全部保留，并且 §7 的需求每一条都用得上：

- append-only 和 `write_txn`：原始层天生 append-only
- `tsv` 与主行同事务：归一化投影写入时同样适用
- migration 只增不改：采集相关的表全部开新文件
- 注入框定：取回的原文同样可能混着恶意指令（会话里的工具输出常常来自网页）
- 不做最小公倍数式供应商抽象：会话归一化**不是**供应商抽象，它不进模型调用链，只用于展示和检索
- 委托凭据用调用者自己的 bearer：设备令牌也由用户自己登录签发

### 6.3 吸收什么（按优先级）

| # | 吸收 | 来源 | 落在哪 |
|---|---|---|---|
| 1 | **原始层 + 原文可检索**（轮次级分块、embedding），作为独立的会话检索工具，**不和事实混排** | MemPalace、cass | §8.4.2。`episodes` 通道默认关是 evals 的结论，不要推翻它，另开一条路 |
| 2 | **追加式观察日志**作为会话压缩形态，后台生成 | Mastra OM、claude-mem | L2 `summaries` 表。它现在**有写入 API 但没有任何生产调用方**，这会是它第一个生产者 |
| 3 | **三层渐进披露**的会话取回工具 | claude-mem、Letta MemFS | §8.4.4 |
| 4 | **hooks + 读日志**的采集方式 | claude-mem、MemPalace | §8.4.1 |
| 5 | 后台整理任务（Postgres 作业队列，`SKIP LOCKED`） | Letta、Mastra、Hindsight | 整理只产出新行，不改旧行 |
| 6 | 时间作为召回的一路，在 evals 里 A/B | Hindsight TEMPR、Mem0 | `evals/` |
| 7 | 可选的 cross-encoder 重排 | MemPalace（LLM 重排）、Basic Memory | 检索管线末端；先用 evals 证明值得再开 |
| 8 | 一行安装 + `cormex init` + Agent Skill | Mem0、MemPalace、ccusage | §8.4.5 |
| 9 | 用公共基准**补充**而不替代自建题集：LongMemEval-S 检索召回（口径写清楚），之后上 BEAM | 全行业 | `evals/`，并且先把 roadmap C 的「evals 不在 CI 里」修掉 |

### 6.4 不做什么

- **不做第二个存储后端**。MemPalace 的可插拔后端是反例：六个后端，每个都要一份兼容矩阵
- **不让 LLM 裁决失效**（Graphiti 那样）。理由写在 `extract.rs` 的第 2 条规则里，这次调研没有发现推翻它的证据
- **不追 benchmark 分数**。MemPalace 的教训：口径错位、对着题修、撤回，一样都不想要
- **不把 agent 循环搬回来**。采集器是采集器，不是 agent（§8.4.1）
- **不依赖 cass**（§3.1）

### 6.5 顺带发现的问题（不属于本需求，单独处理）

1. **`as_of` 回放忽略了 `revoke`**。`crates/cortex-store/src/recall.rs` 里的 `as_of_snapshot!` 只要看到时刻 T 之前有过 `invalidate` 就排除这条事实，**不看之后有没有 `revoke` 把它恢复**；而 `fact_status` 视图是「以最后一条状态事件为准」。两者在「失效后又恢复」这种情况下结论相反。现在线上**没有任何代码写 `revoke`**（只有测试写），所以还是潜在问题；一旦做「用户手工恢复一条记忆」，回放就会静默出错。`tests/replay.rs` 里也没有覆盖这种情况
2. **MCP 依赖落后两个大版本**：仓库锁的是 `rmcp` 1.8，crates.io 上已经是 3.8（2026-10-26）。MCP 的 **2026-07-28 修订版是一次无状态化的破坏性变更**：去掉了 session 和 `initialize` 握手，`resources/subscribe` 换成 `subscriptions/listen`，资源读取结果新增 `ttlMs` / `cacheScope`，Streamable HTTP 要求带 `Mcp-Method` / `Mcp-Name` 头。`ttlMs` / `cacheScope` 恰好能更好地表达「profile 是可缓存前缀」。升级前要先确认 rmcp 3.x 对这一版的支持程度
3. **L2 `summaries` 建了没人调**：只有测试写它。这正是 `routes.rs` 注释里说的「这个仓库头号复发病：built and never called」
4. **profile 资源的 URI 文档与代码不一致**：代码里是 `cortex://profile`（`mcp.rs` 的 `PROFILE_URI`），CLAUDE.md 和 roadmap 也这么写，但 README 写成了 `cormex://profile`。照 README 配置的人会读不到资源。新增的会话资源用哪个 scheme，要和这一处一起定

---

## 7. 需求原文

> 另外我希望cormex有个cli或者mcp能力，可以快速部署到各种环境，因为我在不同电脑不同环境用了很多不同ai agent,各种会话散落的哪里都是，我希望能够让各种ai agent调用cormex cli或者mcp接口，手动、定时或实时追加方式把该ai agent持有的所有项目及会话内容录入到cormex中央存储库(postgresql+rustfs)
> 录入按原始格式，不做过滤加工，这些是最原始的对话数据，后续cormex自己可以对数据加工精简总结压缩都可以，然后有ui可以直接加载像原始对话模式一样展示数据。
> 其他ai agent也可以调用cormex接口取回数据到自己上下文继续聊天，取回的数据可以是精炼总结的压缩数据，也可以是原始数据
> 这个需求只是我个人的初步设想，你可以帮我评审和纠正，看这样实现是否合适有没有更优方法，该需求本身及最终评审结论也输出到调研报告里，推送到github仓库

拆成可评审的条目：

| # | 原始诉求 |
|---|---|
| R1 | Cormex 要有 CLI 或 MCP，能快速部署到各种环境 |
| R2 | 让各种 agent 调用 CLI / MCP，把**该 agent 持有的所有项目及会话**录入中央库（Postgres + RustFS） |
| R3 | 录入方式：手动、定时、实时追加 |
| R4 | 按原始格式录入，不过滤、不加工 |
| R5 | Cormex 后续自己做精简、总结、压缩 |
| R6 | UI 能像原始对话那样展示 |
| R7 | 其他 agent 能取回数据到自己的上下文继续聊；取回的可以是压缩数据，也可以是原始数据 |

---

## 8. 需求评审

### 8.1 总体判断

**成立，而且应该排在 Cormex 功能路线的最前面**（排在 roadmap A 的「空库起不来」之后，见 §8.7）。理由有三：

1. **它把 Cormex 已有的纪律变成了用户看得见的价值**。append-only、出处追到源、双时间轴，现在只体现在「记忆」这一层；有了原始会话层，「凭什么这么认为」可以一路追到**某台机器上某个 agent 的某一行原始记录**
2. **它解决的是真实存在的数据丢失**。Claude Code 默认 30 天删会话；OpenClaw 删除、重置会话时会给文件改名（从 ccusage 对 `.jsonl.deleted` / `.jsonl.reset` 的处理推断）；格式升级时旧数据可能读不出来。散落本身就意味着在丢
3. **raw-first 把「采集」和「解析」解耦了**，这是对抗格式频繁变化的唯一办法：采集只认字节，简单稳定；解析可以随时升级并对历史重跑。**解析器坏了不会丢数据**

但原始设想里有几处**主语、边界和语义**需要纠正，下面逐条说。

### 8.2 逐条纠正

#### ① R2 的主语错了：录入不该由 agent 自己来做

「让 agent 调用 CLI / MCP 把它持有的会话录入」有三个问题：

- **agent（模型）手里并没有它的会话**。会话是宿主程序写在磁盘上的文件，模型看到的只是当前上下文窗口，早被压缩、截断过了。让模型「录入自己的会话」，录进去的是它对会话的**复述**，不是会话本身
- **走 MCP 批量写原文 = 把整段会话塞进模型上下文再吐出来**：烧 token，速度慢，而且**不保真**（模型会截断、改写，甚至编造）。R4 要的「最原始的数据」恰恰没法这么拿到
- **依赖模型「记得去调」就一定会漏**。这正是主流从 `remember` 转向 hooks 的原因（§5 第 7 条）

**改成**：由每台机器上的 **`cormex` 守护进程只读采集宿主写在磁盘上的会话文件**。agent 什么都不用做。

| 角色 | 职责 |
|---|---|
| `cormex` 守护进程 | 发现、监听、只读采集、断网时本机落盘、续传。**唯一的录入者** |
| 宿主 hooks | **只做通知**（「`transcript_path` 这个文件变了」），通过本机 IPC 交给守护进程，几毫秒内返回，**不联网**。hook 里联网的话会拖慢 agent 的每一轮，断网时还会报错 |
| MCP | **只做取回**，以及少量主动写入（已有的 `remember`）。不承担批量录入 |
| CLI 手动命令 | `cormex sync` / `cormex import <path>`：补采、导入导出文件、离线环境搬运 |

#### ② R4「按原始格式」：存储层保真，展示层必须归一化

R4 和 R6 之间有一个没说出来的矛盾：**按原始格式存**，又要**像原始对话一样展示**。UI 不可能直接渲染二十几种格式（JSONL 事件流、整文件 JSON、SQLite KV、对象变更日志、protobuf……），而且这些格式每几个月就变一次。

**改成三层**：

```
R   原始层（新）   各 agent 的原始字节，内容寻址进 RustFS；元数据 append-only 进 Postgres
                   ── 唯一的真相来源，永不改写
P   归一化投影（新）由服务端解析器从 R 生成，带 parser_version，指回 R 中的具体位置
                   ── 可重算；解析器升级 = 追加一套新投影，旧的保留
D   派生物（现有）  观察日志 / 交接简报（L2 summaries）、事实（L1 facts）、向量
                   ── 全部经 derivations 追到 P，再追到 R
```

「按原始格式」落在 R 层，「像原始对话一样展示」落在 P 层。UI 默认显示 P，每条消息都能一键切到「看原始记录」（R 里对应的那一行或那条记录）。

#### ③ R4「不过滤」：存储不过滤 ≠ 不保护

原始会话里**一定有密钥**：agent 读过的 `.env`、打印出来的 token、`env` 的输出。据报道 Claude Code 的 transcript 本身不加密，唯一的保护是本机文件权限。把所有机器的会话汇到一个中央库，这个库就成了**高价值目标**。

**改成**：采纳「存储不过滤」，但出口和权限上要做足：

- **入库时做密钥检测**，复用 `redaction.rs` 的 Grade A 规则，生成一层**遮罩覆盖层**（只记录哪些字节区间是疑似密钥），**不改原始字节**。所有派生物和默认的取回结果都套用这层遮罩
- **原文永远对所有者开放**：UI、CLI、API 都能读原文，疑似密钥默认遮罩，一键解除（留审计记录）。给 agent 的令牌默认没有 `raw:read`，由所有者按设备或按会话授权。详见 §10.2 第 6 条
- **R 层只在服务端，不进 `/sync`**：不下发到各端，免得一份密钥跟着同步复制到每台设备
- **静态加密**：RustFS 开服务端加密；**每个来源一把数据密钥**，purge 时销毁密钥（crypto-shredding，memory-content §8.6②），连备份里的副本一起失效；对少数敏感来源可选客户端加密（代价是服务端没法加工这部分）
- **源头排除**：用户可以配置项目 / 路径黑名单，或者用 `.cormexignore`，决定哪些**根本不采**。这是**范围选择，不是加工**，和「不过滤」不冲突
- 已有的 `redactions`（redact / purge）是全系统唯一允许破坏 append-only 的口子，要覆盖到 R 层（删 blob、留墓碑）

#### ④ R3「实时追加」：很多格式根本不是追加的

| 写入方式 | 代表 | 采集单元 |
|---|---|---|
| 追加式 JSONL | Claude Code、Codex、Copilot CLI、Kimi、pi | 按字节偏移尾随，只传新增区间 |
| 对象变更日志（会周期性压实重写） | VS Code Copilot Chat 1.109+ | 尾随；检测到压实重写后换一代 |
| 整文件重写的 JSON | Gemini CLI（json 版）、Qwen Code、Cline / Roo | **快照**，内容寻址去重 |
| SQLite | Cursor、OpenCode、Goose、Crush、Hermes、Kilo | **只读连接，按行采集**（主键 + 原始值字节）；**不要**每次把整个 `.db` 文件传一遍（Cursor 的 `state.vscdb` 可能几百 MB），也**不要**在宿主写入时直接复制 db 文件（WAL 模式下拷到的是不一致的状态） |
| 不透明 / 加密 | Windsurf（据报道是 `.pb`）、ChatGPT mac 版 v2+ | 能拿到字节就存字节，不承诺解析 |

所以「实时追加」在实现上是：**hooks 或文件监听触发 → 按格式选采集单元 → 分块上传（内容寻址、幂等、可续传）**。另外还要一条**定时全量扫描**兜底：hooks 会漏（Cursor 论坛里有队列消息不触发 hook 的 bug 报告），文件监听也会漏（休眠、网络盘）。

手动、定时、实时三种模式**共用同一条采集管线**，区别只在触发方式：

| 模式 | 触发 | 用途 |
|---|---|---|
| 实时 | hooks 通知 + 文件系统监听 | 日常 |
| 定时 | 守护进程周期性全扫；没有守护进程的环境用 systemd timer / launchd / 计划任务跑 `cormex sync --once` | 兜底；容器 / CI 等跑完即走的环境 |
| 手动 | `cormex sync` / `cormex import` | 首次回填、离线环境、导出文件 |

#### ⑤ R7「取回原始数据到上下文继续聊」：通常是坏主意，要拆两条路

把原始会话灌进另一个 agent 的上下文，会撞上这些问题：

- **体积**：coding 会话的大头是工具输出，原文动辄几十万 token
- **格式不兼容**：A 家的 tool_use / tool_result 结构，B 家的模型不认识；**thinking 块的签名不能跨供应商使用**
- **前缀缓存**：大段原文贴在中间会打穿缓存（CLAUDE.md 约束 4 讲的就是这个）
- **注入风险**：原文里的工具输出常常来自网页，正是记忆投毒的载体（§5 第 11 条）

**改成两条路**：

| 场景 | 做法 | 为什么 |
|---|---|---|
| **同一个 agent，换台机器继续** | `cormex restore <session>`：把 R 层的原始字节还原成该 agent 的会话文件，再用它自己的 `claude --resume` / `codex resume` 之类原生续聊 | **这是字节级保真最大的回报**：不用转换、不丢信息、thinking 签名和缓存行为都由宿主自己负责。需要处理 cwd 路径的映射（还原出来的是派生物，可以改路径，R 层不动）；跨宿主版本的兼容性不保证 |
| **换一个 agent 继续** | 给一份**交接简报**（目标、约束、已做的决定和日期、当前状态、下一步、未决问题、关键文件和命令，每条带出处链接），原文按需逐层钻取 | 成本可控，对缓存友好；出处链接让 agent 需要时自己去拿原文 |

取回的东西不管是简报还是原文，**一律经过 `cortex_core::injection` 的框定**再出去。

#### ⑥ 时间语义：回填历史会让 `as_of` 变得反直觉

现在的 `as_of` 用的是**系统时间**（`recall.rs`：`f.created_at <= $1`）。今天回填三个月前的会话，`as_of=三个月前` 查出来是**空的**，因为三个月前 Cormex 确实还不知道这些。语义上没错，但用户想问的多半是「三个月前那个会话里发生了什么」，这是**事件时间**。

**改成**：两条轴都存，在 API 上明确分开：

- `as_of`：Cormex 在那一刻**知道**什么（系统时间，现有语义不变）
- `at`（或 `occurred_before`）：那一刻**发生**了什么（事件时间，取 agent 原始记录里的时间戳）

UI 的时间轴要标明拖的是哪条轴。原始记录里的时间戳来自客户端时钟，可能不准，要保留原值并单独记一份「入库时间」。现有 `episodes.occurred_at` 的注释已经写了「客户端时钟，经单调化处理」，沿用这个思路。

#### ⑦ 范围：不是所有「agent」都有可采的本地文件

- **网页聊天**（ChatGPT、Claude.ai、Gemini 网页版等）：本地没有文件，只能走**官方导出 → 现有的 `cortex-import`**（已经支持 ChatGPT / Claude 的导出文件）。浏览器扩展可以另议，不在第一期
- **ChatGPT mac 桌面版** v2 以后本地数据是加密的；**Windsurf** 据报道是不透明的 `.pb`。只能存字节，不承诺解析
- **Antigravity** 的会话库里是 protobuf，但它另外有一份干净的 `transcript.jsonl`，优先采后者

预期要定清楚：**coding agent CLI 和 IDE 插件是主战场**，网页聊天走导出。

#### ⑧ 保留期：今天就要处理

- **Claude Code 默认在启动时删除 30 天前的会话文件**（`~/.claude/settings.json` 里的 `cleanupPeriodDays`，最小 1，不能设 0）。在采集器上线之前，**先在每台机器上把它调大**（比如 3650），并备份一次 `~/.claude/projects`。`cormex init` 以后应该主动检查并提示这一项
- 一旦 Cormex 成了某些会话**唯一的副本**，roadmap C 的「备份只写了没验过恢复」就从欠账升级为 **P0**。没有演练过的备份等于没有备份

#### ⑨ 「快速部署到各种环境」：客户端和服务端要分开说

- **服务端**已经有两个镜像，但**空库起不来**（roadmap A ❌）。「到处部署」的前提是服务端能从零装起来，所以那一条要先修
- **客户端**是新东西：一个**静态单二进制** `cormex`，不链接 sqlx、pgvector 这些服务端依赖，只依赖 `cortex-proto` 和 HTTP 客户端，体积小，冷启动快。见 §8.4.5

#### ⑩ 身份：设备、会话、项目都要能跨机器对上

- **设备**：每台机器 `cormex login`（设备码流程，浏览器里确认）拿一把**设备令牌**，作用域是 `capture:write`、`memory:read`、`session:read`，按设备可以吊销（电脑丢了只吊销那一台）。**不要复用 `/delegated-tokens`**：那是给不受信调用方的短期、绑会话的钥匙，语义不同。签发时仍然遵守约束 6，用的是用户自己的 bearer
- **会话**：以「agent + 原生会话 id」为键合并，设备作为来源维度（同一个会话可能因为同步盘出现在两台机器上）
- **项目**：同一个项目在不同机器上路径不同（`/Users/a/proj` 和 `D:\proj`）。用**规范化后的 git remote URL** 作主键，没有 remote 时退回路径；映射关系由用户编辑，用 append-only 事件记录（沿用 `project_events` 的风格）

### 8.3 各 agent 会话存储现状（2026-10 快照）

> 路径和格式会随版本变。来源：各宿主的 hooks 官方文档、franken_agent_detection 的模块文档、ccusage 的数据源说明，以及第三方报道（标「据报道」的未核实）。

| Agent | 默认位置 | 格式 / 写入方式 | 实时信号 | 采集单元 | 优先级 |
|---|---|---|---|---|---|
| **Claude Code** | `~/.claude/projects/<编码后的 cwd>/<session>.jsonl`；子 agent 在 `<session>/subagents/agent-*.jsonl` | JSONL，追加 | hooks：SessionStart / UserPromptSubmit / PreToolUse / PostToolUse / Stop / PreCompact / SessionEnd，输入带 `transcript_path` | 字节尾随 | **P0** |
| **Codex CLI** | `$CODEX_HOME`（默认 `~/.codex`）`/sessions/YYYY/MM/DD/rollout-*.jsonl`，另有 `archived_sessions/` | JSONL（`session_meta` / `response_item` / `event_msg` …），追加 | hooks（要在 `config.toml` 打开 feature；SessionStart 带 `transcript_path`，可能为空） | 字节尾随 | **P0** |
| Gemini CLI | `~/.gemini/tmp/<项目哈希>/chats/session-*.json`（新版本出现了 `.jsonl`） | JSON 整文件重写 / JSONL | hooks（v0.26+；SessionStart 带 `transcript_path`；有 AfterAgent / SessionEnd） | 快照 / 尾随 | P1 |
| Cursor（IDE） | `state.vscdb`（`globalStorage` 和 `workspaceStorage/<id>/`）的 `cursorDiskKV`：`composerData:{id}`、`bubbleId:{composer}:{bubble}`；据报道 hooks 的 `transcript_path` 指向 `~/.cursor/projects` 下的 `.jsonl` | SQLite KV（值是 JSON）/ JSONL | hooks：beforeSubmitPrompt / afterAgentResponse / afterAgentThought / stop / preCompact / 子 agent 起止 | 按行 / 尾随 | P1 |
| VS Code Copilot Chat | `workspaceStorage/<id>/chatSessions/<id>.jsonl`（1.109+）、`.json`（2025-03 到 1.108）、`state.vscdb`（更早） | **对象变更日志**（kind 0 快照、1 设值、2 数组追加、3 删除），会压实重写 | 无 | 尾随 + 换代检测 | P1 |
| Copilot CLI | `~/.copilot/session-state/<session>/events.jsonl` | JSONL，追加 | — | 尾随 | P2 |
| Cline / Roo Code | VS Code `globalStorage/<扩展 id>/tasks/<task>/api_conversation_history.json` 和 `ui_messages.json` | JSON 整文件重写 | — | 快照 | P2 |
| OpenCode | `~/.local/share/opencode/` 下的 `opencode.db`（早期是 `storage/` 下的 JSON 文件） | SQLite | 插件事件（JS） | 按行（按 `seq` / `time_created` 增量） | P2 |
| Goose | `~/.local/share/goose/sessions/sessions.db`（v1.20+；早期是 `~/.goose/sessions/*.jsonl`） | SQLite | — | 按行 | P2 |
| Qwen Code | `~/.qwen/tmp/*/chats/session-*.json` | 同 Gemini 系 | — | 快照 | P3 |
| Aider | 项目里的 `.aider.chat.history.md` | Markdown，追加 | — | 尾随 | P3 |
| Factory Droid / pi / Kimi Code / Grok Build / OpenClaw / OpenHands / Amp | 各自的 home 目录，多数是 JSONL | 多为追加 | 少数有 | 尾随 / 快照 | P3 |
| Antigravity | `~/.gemini/antigravity-cli/brain/<uuid>/.system_generated/logs/transcript.jsonl`，另有 `conversations/<uuid>.db`（SQLite 里存 protobuf） | 混合 | — | 采 `transcript.jsonl`，db 只存不解析 | P3 |
| Windsurf | 据报道是 `~/.codeium/windsurf/cascade/*.pb` | 不透明 | — | 只存字节 | 不承诺 |
| ChatGPT / Claude.ai / Gemini 网页版 | 本地没有文件 | — | — | 官方导出 → `cortex-import` | 已有 |

两个值得留意的信号：

- **Grok Build 直接把 ACP 的 session-update 流作为它的落盘格式**（`updates.jsonl`）。ACP（Agent Client Protocol，Zed 发起，JetBrains 内置，Zed 1.0 主打）正在成为「编辑器 ↔ agent」的事实标准，Rust 有官方 crate（`agent-client-protocol` 3.6.0）
- **Copilot CLI 能把会话导出成 OpenTelemetry GenAI 语义约定（`gen_ai.*`）的事件文件**

所以 P 层的归一化 schema **建议对齐 ACP 的内容块**（消息块、tool_call / tool_call_update、diff、plan、thought），但定义在 Cormex 自己的 `cortex-proto` 里，不直接用 ACP 的类型，免得被它的版本节奏绑住。这样以后原生说 ACP 的 agent，事件流几乎可以直接收。

### 8.4 推荐方案（概要；细节留给架构设计阶段）

#### 8.4.1 组件与数据流

```
 每台机器 / 每个环境                                         中央（cormex 服务端，已有）
┌────────────────────────────────────────────────┐        ┌────────────────────────────────────────────┐
│ agent 宿主：Claude Code / Codex / Cursor / …    │        │ cortexd                                    │
│   │ 写自己的会话文件（我们只读，不改）          │        │  POST /captures/*  ← 新：分块、幂等、可续传 │
│   │                                             │        │   ├─ 原始字节 → RustFS（内容寻址，zstd）    │
│   │ hooks ──本机 IPC（毫秒级，不联网）──┐       │        │   ├─ 采集元数据 → Postgres（append-only）   │
│   ▼                                      ▼      │ HTTPS  │   └─ 作业队列（SKIP LOCKED）                │
│ 会话文件 ◀── 只读 ── cormex 守护进程 ───────────┼───────▶│        ├─ 解析 → 归一化投影（P 层）         │
│                     · 发现 / 监听 / 定时全扫    │        │        ├─ 遮罩覆盖层（密钥检测）            │
│                     · 断网时本机落盘、续传      │        │        ├─ 观察日志 / 交接简报（L2）         │
│                     · 不解析内容（只认边界）    │        │        ├─ 事实抽取（L1，按策略）            │
│                                                 │        │        └─ 轮次级向量                        │
│ cormex mcp（可选 stdio 桥）─────────────────────┼───────▶│  GET /sessions/* · /mcp · /ws（取回与推送）│
└────────────────────────────────────────────────┘        └────────────────────────────────────────────┘
                                                                         ▲
                                                      cormex-web（Flutter）：会话视图 / 原始切换 / 时间轴
```

**最关键的一个取舍：解析放在服务端，不放在客户端。**

- 客户端只需要知道「去哪找文件、文件边界在哪、这是哪个 agent 的哪个会话」，这一层很薄，也很少变
- 解析器在服务端升级之后，**可以对全部历史重跑**，不需要每台机器都升级客户端
- 解析器坏了（格式变了）的时候，采集照常进行，字节一个不丢，修好解析器再补投影

这和 CLAUDE.md 讲 `cortex-import` 时说的「同一个解析器在几处跑，漂移的症状很难看」是同一个教训。解析器只存在一份。

#### 8.4.2 数据分层与现有 schema 的关系

| 层 | 表（建议） | 和现有的关系 |
|---|---|---|
| R 原始层 | `capture_sources`（设备 × agent × 原生会话 id × 路径）、`capture_chunks`（偏移、长度、sha256、blob 引用、观察时间）、`capture_snapshots`（整文件 / 整行快照） | 新表，新 migration。字节放在 RustFS，复用 `cortex-blob` 的内容寻址 |
| P 归一化投影 | `transcript_events`（会话、序号、角色 / 类型、ACP 风格的内容块、`parser_version`、指回 R 的定位符） | **建议不往 `episodes` 里塞**，理由见下 |
| 原文档案索引 | `archive_chunks`（P 层文本分块的 BM25；遮罩区间不进） | 新表；只在 `scope="archive"` 时使用，不参与默认排序；可按项目关掉，可从 R 层重建（§10.2 第 6 条） |
| L2 | `summaries`（scope 用 `session`；观察日志、交接简报） | 现有表，**第一次有生产者** |
| L1 | `facts`（从 P 按策略抽取；`source_channel` 区分会话里的用户发言和工具输出） | 现有表 |
| 出处 | `derivations`（新增 `source_kind`：transcript_event） | 现有表的扩展（新 migration） |

**为什么不往 `episodes` 里塞**：`episodes` 是「Cormex 参与过的对话」，它会进 `/sync` 下发给各端，`POST /episodes` 会成对送去抽取，`episodes` 召回通道也在它上面。把几百万条工具事件塞进去，同步会被淹没，抽取成本会失控，召回会被稀释。`session_events` 迁移里的注释已经讲过同一个道理：在 L0 里开控制通道，代价会摊成永久的复杂度。

#### 8.4.3 加工管线（R5）

| 阶段 | 产出 | 成本 | 默认 |
|---|---|---|---|
| 解析 | P 层投影 | 无 LLM | 总是 |
| 遮罩 | 密钥区间覆盖层 | 无 LLM | 总是 |
| 分块 + 向量 | 轮次级分块的 embedding（MemPalace 的教训：原文要能检索） | embedding | 总是（本地 embedding 可选） |
| 观察日志 | 会话级、带日期、追加式的观察条目（Mastra 的形态） | 便宜模型 | 会话结束或空闲时；**工具输出先截断**再送模型 |
| 交接简报 | 结构化的续聊简报 | 便宜模型 | **懒生成**：第一次被取回时才生成，生成后缓存 |
| 事实抽取 | L1 facts | 便宜模型 | **按策略**：默认只抽用户发言和最终结论，工具输出不抽（trust tier 3） |

成本控制沿用 `cortex-import` 的传统：**默认 dry-run，先把数字摆出来**；设每日预算上限；历史回填单独排队、单独限速。

#### 8.4.4 取回接口（R7）

MCP 工具按**三层渐进披露**设计（借 claude-mem）：

| 工具 | 返回 | 典型体积 |
|---|---|---|
| `session_search(query, agent?, project?, device?, since?, until?, as_of?)` | 紧凑索引：id、标题、agent、设备、项目、时间、一行摘要 | 每条约 50–100 token |
| `session_get(id, view, range?, max_tokens)` | `view=brief`：交接简报<br>`view=digest`：归一化对话，工具调用折叠成一行，长输出截断，可续页<br>`view=transcript`：完整归一化对话，分页<br>`view=raw`：原始记录，需要 `raw:read`，默认遮罩 | brief ≤ 约 2k；其余按 `max_tokens` 分页 |
| `session_search(…, scope="archive")` | 在**原文档案索引**里检索（含工具输出），返回命中的会话和位置。需要 `raw:read` | 同上 |
| `memory_search`（已有） | 事实；每条带出处，可一路还原到原文 | 不变 |

可缓存的内容（项目简报、会话简报）**同时提供成 resource**（比如 `cortex://session/{id}/brief`、`cortex://project/{key}/brief`；scheme 见 §6.5 第 4 条），配上 2026-07-28 版 MCP 的 `ttlMs` / `cacheScope`，让宿主放进可缓存前缀，和 `cortex://profile` 是同一个思路。

CLI 提供对应的命令：`cormex sessions ls / show / brief / restore`。

所有输出都走 `injection::render_*` 的框定，**不另写一套「会话结果格式」**（约束 4）。

#### 8.4.5 部署形态（R1）

- **服务端**：现有两个镜像，先修空库启动
- **客户端**：一个 `cormex` 二进制
  - 目标平台：Linux musl（x86_64 / aarch64）、macOS（universal）、Windows（x64 / arm64）
  - 渠道：GitHub Releases 加 `install.sh` / `install.ps1`、Homebrew、Scoop / winget、**npm 包装（`npx cormex`，ccusage 的模式）**、给无界面服务器和 CI 用的 Docker 镜像
  - 跟随现有发版流水线：打 `vX.Y.Z` tag 时一起出
- **`cormex init`**（幂等，支持 `--dry-run`）：
  1. `login`：设备码流程
  2. 探测本机装了哪些 agent，列出发现的会话数和总体积
  3. 逐项征求同意：安装 hooks、把 Cormex 的 MCP 注册进各 agent 的配置、调大 Claude Code 的 `cleanupPeriodDays`、注册系统服务（systemd --user / launchd / Windows 计划任务）
  4. 首次回填（dry-run 给出估算后再执行）
- **`cormex doctor`**：hooks 是否生效、守护进程是否在跑、积压量、哪些来源长期没有新数据（可能是格式变了）
- **`cormex uninstall`**：撤销 `init` 改过的每一处配置（改之前先备份原文件）
- **一个 Agent Skill**（`SKILL.md`）：告诉 agent 什么时候该调 `session_search` / `session_get`

**这里不跑 agent 循环**的约束对客户端同样适用：`cormex` 是采集器加命令行查询工具，**不是 agent**。

#### 8.4.6 UI（R6）

在现有 Flutter 记忆浏览器里加一个会话视图：

- 按设备、agent、项目分组的会话列表
- 对话渲染：消息、思考、工具调用（可折叠）、diff
- 每条消息都能**切换到原始记录**
- 时间轴两条轴可切换（§8.2 第 ⑥ 条）
- 大会话用虚拟列表加分页 API；新数据经 `/ws` 实时推过来（现有 `sync_log` 上的 LISTEN/NOTIFY 就是现成的事件源）
- 搜索框带一个「在原文中搜索」开关，对应 `scope="archive"`（§10.2 第 6 条）

#### 8.4.7 各类内容怎么处理：代码、图片、Office、二进制

**总原则：会话文件里有什么就采什么，会话文件以外的一律不碰。** 采集器只读各 agent 自己的会话目录（`~/.claude/projects`、`~/.codex/sessions` 等），**不读项目目录，不顺着会话里提到的路径去抓文件，更不打包代码库。**

| 类型 | 它在会话文件里的样子 | R 层（原样） | P 层（展示） | D 层（记忆 / 检索） |
|---|---|---|---|---|
| **代码** | agent 读文件时，工具结果里有它读到的那一段；改文件时，工具参数里有改动前后的片段或整份新文件；对话里贴的代码片段 | 随会话原样存 | 代码块、diff，可折叠 | **不索引读到的文件内容**（原文档案索引除外，§10.2 第 6 条）。只记「路径 + 行范围」引用，以及「改了什么、为什么改」的结论。代码以 git 为准（memory-content §二④） |
| **图片** | Claude Code 把粘贴的图片和读取的截图以 base64 直接写进会话文件；其他宿主有的只存本地路径，适配时逐家确认 | 随会话原样存 | 解码后另存为 RustFS 里的独立 blob（去重，UI 直接显示） | 可选生成文字描述（现有的 vision caption / OCR 管线，`transcribe.rs`），描述进 `blob_transcripts`，可检索 |
| **Office / PDF** | 用户直接拖进对话的文件会被内联；agent 用脚本读 Excel 时，会话里只有脚本打印出来的文字 | 随会话原样存 | 内联的文件可下载、预览 | 目前**只存不抽**：`blob_transcripts.kind` 还没有 Office 文本这一档（memory-content §二⑤） |
| **其他二进制** | agent 一般读不了，会话里通常只有一行报错或几句描述 | 随会话原样存 | — | — |

**边界上的两处细节：**

1. **宿主自己的附件目录算会话的一部分**。有些宿主把用户贴进对话的附件另存到它自己的缓存目录，会话文件里只留路径。这些附件是对话的一部分，而且宿主清理时会一起消失，所以要采；但**只限宿主自己的附件目录**，用户项目里的文件永远不采
2. **Claude Code 的 `~/.claude/file-history/`**（它给改过的文件做的备份，用于回退）属于代码内容，**默认不采**，留一个开关

**可选**：采集器在会话开始时读一次 git 的当前提交号（只读一个哈希值，不读任何文件内容），让「路径 + 行范围」的引用能精确对回 git 里的版本。

**存储上要实测的变量**：base64 图片会让会话文件明显变大，而 PNG / JPG 本身已经压缩过，zstd 压不动。

### 8.5 留给架构阶段拍板的问题（附建议）

| # | 问题 | 建议 |
|---|---|---|
| 1 | P 层放 `episodes` 还是新表 | **新表**（§8.4.2） |
| 2 | 解析放客户端还是服务端 | **服务端**；客户端只做发现和划边界（§8.4.1） |
| 3 | 归一化 schema 自定义还是直接用 ACP 类型 | **自定义，但对齐 ACP 内容块**（§8.3） |
| 4 | SQLite 来源的「原始」粒度 | **行级**（主键 + 原始值字节）；不传整个 db 文件 |
| 5 | 回填数据的时间轴 | 两轴都存；API 分开 `as_of` 和 `at`（§8.2 第 ⑥ 条） |
| 6 | 加密：服务端可读还是端到端 | 默认服务端可读加静态加密；端到端按来源可选（代价是该来源不能加工） |
| 7 | 设备凭据 | 新的凭据类型，用户自己登录签发，可按设备吊销；不复用 `/delegated-tokens` |
| 8 | 项目身份 | 规范化的 git remote 优先；映射用 append-only 事件 |
| 9 | 客户端 crate 命名和依赖 | 二进制叫 `cormex`；crate 只依赖 `cortex-proto` 和 HTTP 客户端，**不依赖 sqlx**。现有 crate 都叫 `cortex-*`，新 crate 是否改用 `cormex-*` 前缀要定一次 |
| 10 | MCP 升级 | rmcp 1.8 → 3.x，对齐 2026-07-28 无状态版本；**和本需求并行，但要早于新增 MCP 工具** |
| 11 | 采集上传协议 | 分块，幂等键 =（来源文件 id、偏移、长度、sha256）；文件变短或 inode 变了视为换代 |
| 12 | 解析器回归测试 | 每个适配器一组**金标准夹具**（真实文件脱敏后入库）+ **字节级往返测试**（R 层还原出的文件和原文件 sha256 一致）；遇到不认识的记录类型保留为不透明事件，**不丢弃** |
| 13 | **是否接受在 R 层推翻 memory-content ③「工具输出原始字节不存」的定案** | **✅ 已拍板（2026-10-29）：接受，仅限 R 层**（选项 A）；默认的检索和注入继续执行 ③。附加要求：原文永远对所有者可读、可显式搜索，每条记忆可还原到原文（§10.2 第 6 条） |

### 8.6 风险与代价

| 风险 | 表现 | 缓解 |
|---|---|---|
| **格式维护是跑步机** | 一年里 Copilot 换了三代存储，OpenCode 从 JSON 换到 SQLite，Goose 从 JSONL 换到 SQLite | raw-first（坏了不丢数据）；金标准夹具；`doctor` 里报告「某来源长期没有新数据」；适配器按优先级分期做，不追求全覆盖 |
| 中央库成为高价值目标 | 一处泄露等于所有机器、所有项目一起泄露 | §8.2 第 ③ 条；设备令牌可吊销；`raw:read` 单独授权 |
| LLM 加工成本失控 | 账单 | 懒生成、工具输出先截断、按策略抽取、每日上限、默认 dry-run |
| 存储增长 | RustFS 体积 | 内容寻址去重 + zstd；按来源统计用量；保留策略要**显式并且可审计**（R 层原则上不删，要删走 purge） |
| 唯一副本 | 宿主清理之后 Cormex 是唯一的拷贝 | 备份恢复演练升到 P0（§8.2 第 ⑧ 条） |
| 原文进上下文引发投毒 | 取回的工具输出里带着恶意指令 | 统一框定；工具输出 trust tier 3/4；默认给简报而不是原文 |
| 范围蔓延成第二个产品 | 采集器长成 agent，或者长成通用的同步盘 | 两条红线：不跑 agent 循环；只采 agent 会话，不采任意文件 |

### 8.7 分阶段路线（粗粒度，代码级计划在下一份文档）

| 阶段 | 内容 | 完成的判据 |
|---|---|---|
| **0（今天，不写代码）** | 每台机器调大 Claude Code 的 `cleanupPeriodDays`；备份 `~/.claude/projects`、`~/.codex/sessions` 等目录 | 不再有会话过期被删 |
| **前置** | roadmap A：空库能起来；roadmap C：一次备份恢复演练 | 干净机器上从零装起服务端；从备份恢复后记录数能对上 |
| **1：能采、能看** | R 层表和采集 API；`cormex` 的 `login / init / sync / watch / doctor`；Claude Code + Codex 两个适配器和 hooks；会话列表和归一化 API；UI 会话视图 | 两台机器上两种 agent 的会话实时出现在 UI 里；字节级往返测试通过 |
| **2：能取回** | MCP `session_search` / `session_get`（brief / digest / transcript / raw）；遮罩覆盖层；`cormex restore`；rmcp 升级 | 在另一台机器上用 `claude --resume` 续上；在另一个 agent 里靠 brief 续上 |
| **3：铺开** | Gemini CLI、Cursor、VS Code Copilot、Cline / Roo、OpenCode、Goose 的适配器 | P1 和 P2 全部有金标准夹具 |
| **4：加工** | 观察日志、交接简报、按策略抽取事实、轮次级向量；作业队列和预算 | 成本可预估；evals 里会话检索有基线数字 |
| **5：评测** | 采集完整性测试；会话检索的 evals；LongMemEval-S 召回（口径写清楚）；evals 进 CI | 每次「优化」都有可对照的数字 |

---

## 9. 评审结论

**需求成立，建议立项，优先级排在 roadmap A 修完之后的第一位。** 原始设想要按下面几条修正后再进入架构设计：

1. **录入的主语从「agent」改成「本机 `cormex` 守护进程」**。agent 什么都不用做；hooks 只负责通知，MCP 只负责取回。让模型通过 MCP 录入自己的会话，既不保真也不省钱
2. **「按原始格式、不过滤」保留，但只落在存储层**。原始字节 → 归一化投影（带版本，可重算）→ 派生物，三层分开；UI 展示投影，随时可以切回原始记录
3. **不过滤 ≠ 不保护**：入库时生成遮罩覆盖层（不改原字节），出口默认遮罩，原始视图单独授权，静态加密，支持源头排除
4. **「实时追加」按格式区分采集单元**：尾随、快照、SQLite 按行。hooks 加文件监听加定时全扫三重触发，共用一条管线
5. **「取回继续聊」拆两条路**：同一个 agent 用原始文件还原后原生续聊（字节级保真最大的回报）；换一个 agent 用交接简报加渐进披露，**不直接灌原文**
6. **解析放服务端**，客户端保持轻薄，解析器升级后可以对全部历史重跑
7. **时间轴明确分开**「系统知道」（`as_of`）和「会话里发生」（`at`），否则回填的历史在回放时看起来像不存在
8. **今天就处理 Claude Code 的 30 天删除**；Cormex 成为唯一副本之后，备份恢复演练是 P0
9. **不依赖 cass**（许可证 rider 对 Anthropic / OpenAI 关联使用设限）；格式知识独立实现
10. **对外的定位要更新**：「双时间轴」已经不独有了（Graphiti 有）。新的说法应该是**注入回放 + 确定性失效 + 跨 agent 原始会话档案**。README 和 CLAUDE.md 的措辞评审通过后再改
11. **「原样录入」推翻了 2026-08 的一条定案**（工具输出原始字节不存）。**已拍板（2026-10-29）：只在归档层推翻**。R 层只在服务端、不同步、每来源一把密钥；默认的检索和注入继续执行原定案；原文永远对所有者可读、可显式搜索（§10.2）

**更优的方法**，相对原始设想，核心只有一句：**让文件自己说话，不要让模型转述**。磁盘上的会话文件就是最原始的数据，采集器只读它的字节，Cormex 在服务端慢慢加工；回到上下文的时候，默认给一份带出处的简报，原文留给真正需要的那一步。

---

## 10. 与 2026-08 调研的合并

2026-08 在 Cortex 仓库里做过一轮更大的调研，产出两份文档：[memory.md](../memory.md)（记忆系统怎么工作）和 [memory-content.md](../memory-content.md)（什么东西有资格进记忆）。两份开头都写着「这份文档描述的是 Cormex，不是本仓库」，但拆仓时没有跟过来。于是本仓库的代码和 migration 注释一直在引用 `docs/memory.md` §七 / §九 / §十一，这个文件却不存在。这次一并迁入，正文未改。

| | 2026-08（memory-content.md） | 2026-10（本文） |
|---|---|---|
| 范围 | 58 个系统，6 个视角（开源源码 / 闭源产品 / 编码 agent / 论文 / 经验持久化 / 治理与风险） | 13 个记忆系统、3 个采集工具、各 agent 的会话格式 |
| 核心问题 | 什么东西有资格进记忆 | 主流方案现在怎样；多 agent 会话采集该怎么做 |
| 方法 | 源码级阅读 + 四轮一手来源核证 + 对着真实代码的对抗核验 | 包仓库上的 README 和源码 + 搜索摘要（这个会话里大部分网站访问不到） |

**可信度上，2026-08 那一轮更扎实。** 两边说法冲突时，除非本文给出了更新的一手证据，否则以那一轮为准。

### 10.1 两轮一致的结论

| 结论 | 2026-08 | 2026-10 |
|---|---|---|
| 派生层之下必须保留原始层 | §一.1（Graphiti、Hindsight、Mem0 v3、MemOS 四个独立证据） | §5 第 3 条（又加上 MemPalace 的数据） |
| ADD-only 战胜 LLM 仲裁的 UPDATE/DELETE | §一.4 | §2.1（Mem0 README 原话） |
| 双时间轴是通用语，**编码 agent 里**没人有 | §一.5 | §6.1（并据此要求改 CLAUDE.md 的说法） |
| 来源分级是全行业空白 | §8.3 | §6.1 |
| 失效由确定性代码裁决 | §5.1「LLM 提名，确定性代码裁决」 | §2.2、§6.4 |
| 不让 LLM 整体重写画像 | §6.2（ACE 的 context collapse） | §2.5、§6.3：推荐**追加式**观察日志，与之相容 |
| 写侧过滤先做来源级路径排除，再做内容检测 | §8.7① | §8.2 第 ③ 条的「源头排除」 |
| 派生物必须带出处边，供失效传播 | §5.3 | §8.4.2（`derivations` 扩展到 R / P 层） |

### 10.2 🔴 冲突：「工具输出原始字节」从「已定案不存」变成「原样归档」

2026-08 的五类输入表里，第 ③ 行「工具输出原始字节（文件全文、命令 stdout）」在归档、抽取、索引三列都是 ❌，并且注明「**已定案的不做，不是待补的缺口**」。同一份文档的 §二④（版控内文件存引用、不存内容）和 §三（按写入存，不按读取存）也是同一个方向。

§7 需求的 R4 要求原样录入各 agent 的会话文件，而 Claude Code、Codex 的会话文件里**本来就逐字带着工具输出**：读过的文件全文、命令的 stdout。**接受 R4，就是在「归档」这一列上推翻 ③（连带 ④ 和 §三）。** 这一条要你明确拍板，不能在实现里悄悄发生。

逐条对照 ③ 的三条理由：

| 2026-08 的理由 | 在新方案里还成立吗 | 本方案怎么应对 |
|---|---|---|
| **(a) 检索污染**：只多索引一句话的边，R@5 就掉了 0.015 | **成立** | R 层**不进事实检索索引**。会话检索是一个独立工具，工具输出默认也不进它的索引，只索引「命令 + 结论」，也就是 ③ 原来的做法。**默认检索**这一列维持 ❌；显式使用的原文档案索引不参与混排，见第 6 条 |
| **(b) 密钥永久留下**，还会跟着同步复制到每台设备；写前过滤必然漏 | **部分成立，而且是本方案最大的代价** | R 层**只在服务端，不进 `/sync`**，去掉了「复制到每台设备」。安全不建立在过滤器上（遮罩只是出口的一层），而是靠权限加 purge。purge 用每个来源一把的数据密钥做 crypto-shredding（memory-content §8.6②），备份里的副本一起失效。但「中央库里永久有一份带密钥的原文」这个事实不会消失 |
| **(c) 没有失效机制的逐字存储会无限膨胀**（Mem0 procedural memory） | **成立，但对象不同** | R 层是档案，不是记忆：不参与召回，不进上下文，膨胀只是存储成本（内容寻址 + zstd），不会让系统变笨。保留策略显式、可审计 |

还有一条 2026-08 的论据，本文 §8 没有覆盖到，这里补上。memory-content §8.6③：**「能证明『我们从不持有』的系统，比能证明『我们已删除』的系统，在司法保全场景下强得多。」** 全量采集让 Cormex 从「只持有结论」变成「什么都持有」。个人自托管时风险可控；但如果将来有人用 Cormex 给别人托管（现在已经有账号注册和每用户一个 schema 的多租户，见 §10.4），这一条就是实打实的暴露面。

**裁决（2026-10-29 已拍板，选项 A）**：

1. 接受 R4，但**只在 R 层（归档）推翻 ③**。D 层（抽取、默认检索、注入）**继续执行 ③ 的全部定案**
2. 采不采由用户的源头排除规则决定，但**粒度只能到项目或会话**。memory-content §8.7① 的路径白名单（`.env` / `*.pem` / `id_rsa`）在这里用不上：会话文件是一个整体，没法按 agent 读过哪个文件拆开存。这一点要写进用户文档
3. R 层不进 `/sync`
4. 每个来源一把数据密钥，purge 就是销毁密钥
5. 五类输入表加第 ⑥ 行：

| 输入 | 归档 | 进抽取管线 | 进检索索引 |
|---|:---:|:---:|:---:|
| **⑥ 采集到的外部 agent 会话原文**（含其中的工具输出） | ✅ R 层，只在服务端，每来源一把密钥 | 部分：对话按 ① 的判据；工具轨迹按 ② 的特化判据；工具输出字节 ❌ | 对话 ✅（会话检索）；工具输出：默认检索 ❌，**原文档案索引 ✅**（只在显式要求时使用，见第 6 条） |

6. **原文永远可读、可还原、可显式搜索**（所有者的附加要求）。「默认不进检索」只是不让原文挤进默认结果，**不是不让读**：
   - **随时能读**：UI 里每条消息一键切到原始记录，CLI 用 `cormex sessions show --raw`，API 用 `session_get(view=raw)`。疑似密钥默认遮罩，一键解除，解除会留审计记录。**唯一能让原文读不到的，是所有者自己执行的 purge**
   - **每条记忆都能还原到原文**：事实、摘要、交接简报都经 `derivations` 指回 P 层，再指回 R 层的具体字节区间。「凭什么这么认为」可以一路点到原始的那一行
   - **原文也能搜，但要显式**：另建一个**原文档案索引**（只做 BM25 全文，不算向量；可按项目关掉，可从 R 层重建），它**不参与 `memory_search` 和默认 `session_search` 的排序**，只在显式指定 `scope="archive"`（UI 里的「在原文中搜索」开关）时使用。这样能搜到「上个月 agent 读过的那份 nginx 配置」，又不会触发 2026-08 实测到的检索污染：污染只在混排时发生
   - **两条限制**，目的是防误伤，不是不让读：
     1. **被遮罩的疑似密钥区间不进任何索引**，原文档案索引也不进，否则索引本身就成了密钥的又一份拷贝。读原文、解除遮罩之后仍然能看到
     2. **给 agent 的令牌默认没有 `raw:read`**，由所有者按设备或按会话授权。原文里的工具输出是记忆投毒的主要载体，要不要让 agent 读原文，由人决定

### 10.3 2026-10 的新证据对 2026-08 结论的修正

- **§一.2「没有一家默认存工具输出原始字节」**：在**记忆层**仍然成立（Mem0、claude-mem 都只存结论或压缩后的观察）。但出现了一个**新品类：会话档案 / 检索工具**（MemPalace 挖完整的 Claude Code 会话，cass 维护原始镜像），它们存原始字节。Cormex 的 R 层属于这个新品类，D 层属于记忆层，两个结论并不矛盾，只是 2026-08 的时候这个品类还不成气候
- **§一.5「编码 agent 里没有一家有双时间轴」**：仍然成立。宿主自带的记忆（Claude Code auto memory 等）都还是文件 mtime 和使用频度，本轮没看到反例
- **Mem0**：2026-08 写的「v3 单趟 ADD」和 2026-04 新算法的 README 一致；2026-10 补上了它自报的 BEAM 分数和开源版与托管版的差距

### 10.4 迁入文档里已经过时的陈述

只列核对过的，其余现状陈述**没有逐条核对**：

| 位置 | 原文说的 | 现在 |
|---|---|---|
| memory-content §二②、§九、§十三 等 | `live.rs` 的 `tool_path()`、agent 的 `read_file`、「把 `render_profile_block` 接进 live 的 prompt 组装」 | agent 循环已从本仓库删除（CLAUDE.md「这里不跑 agent 循环」），`tool_path()` 已不存在；`render_profile_block` 现在由 MCP 的 profile 资源调用（`live.rs`） |
| memory-content §8.6① | 多租户「被五条具体论证明确否决」，没有加归属列 | 已改成**每用户一个 schema**（`crates/cortex-store/src/tenant.rs`），并有账号注册。「没有归属列」仍然成立，但「不为他人处理数据」已不成立 |
| memory-content §十三 第一批第 1、2 条 | `trust_tier`、派生边是待办 | 已落地：`source_channel` / `trust_tier`（`migrations/20260807000006_fact_provenance.sql`）、`derivations` 表；`insert_summary` 拒绝没有出处的摘要 |
| memory.md §十二「检索评测体系」 | LongMemEval-S 回放 + 自建中英双语集 | 只有自建集（`evals/`）；LongMemEval-S 没做 |
| 两份文档里提到的 `roadmap.md` / `operations.md` / `CLAUDE.md` | 指 Cortex 仓库里的同名文件 | 本仓库的 `roadmap.md` 和 `CLAUDE.md` 是拆仓后重写的，内容不同；`operations.md` 本仓库没有 |

---

## 参考来源

记忆系统：

- Mem0 README（PyPI `mem0ai` 2.2.1）：<https://pypi.org/project/mem0ai/> · 评测框架 <https://github.com/mem0ai/memory-benchmarks>
- Graphiti README（PyPI `graphiti-core` 0.31.2）：<https://pypi.org/project/graphiti-core/> · Zep 论文 <https://arxiv.org/abs/2501.13956> · 时间模型 <https://blog.getzep.com/beyond-static-knowledge-graphs/>
- Letta MemFS：<https://docs.letta.com/letta-code/memfs> · sleep-time：<https://letta.com/blog/sleep-time-compute> · 共享记忆 <https://docs.letta.com/concepts/shared-memory>
- Hindsight（PyPI `hindsight-api`）：<https://pypi.org/project/hindsight-api/> · 论文 <https://arxiv.org/abs/2512.12818> · 基准对比 <https://hindsight.vectorize.io/guides/2026/04/21/comparison-agent-memory-benchmark-hindsight-vs-alternatives>
- Mastra Observational Memory：<https://mastra.ai/research/observational-memory> · <https://mastra.ai/docs/memory/observational-memory>
- MemPalace（PyPI `mempalace` 3.11.0）：<https://pypi.org/project/mempalace/> · 批评论文 <https://arxiv.org/abs/2604.21284> · 争议综述 <https://vectorize.io/articles/mempalace-review>
- claude-mem（npm `claude-mem` 15.5.0）：<https://www.npmjs.com/package/claude-mem> · <https://www.datacamp.com/tutorial/claude-mem-guide>
- Cognee（PyPI `cognee`）：<https://pypi.org/project/cognee/>
- Supermemory changelog：<https://supermemory.ai/docs/changelog>
- Basic Memory（PyPI `basic-memory`）：<https://pypi.org/project/basic-memory/>
- MemOS：<https://github.com/MemTensor/MemOS> · 论文 <https://arxiv.org/abs/2507.03724>
- Claude Code auto memory：<https://blog.memoryplugin.com/claude-code-memory/>
- LangMem（PyPI `langmem`）：<https://pypi.org/project/langmem/>
- 第三方横评（自报数字的汇总，仅供口径参考）：<https://atlan.com/know/best-ai-agent-memory-frameworks-2026/> · <https://www.braintrust.dev/articles/best-ai-agent-memory-tools-2026>

会话采集与格式：

- cass（crate `coding-agent-search` 0.7.0）：<https://crates.io/crates/coding-agent-search> · <https://github.com/Dicklesworthstone/coding_agent_session_search>
- franken_agent_detection（crate 0.3.10）：<https://crates.io/crates/franken-agent-detection>
- ccusage（npm 20.0.45）：<https://www.npmjs.com/package/ccusage>
- Claude Code 的 30 天删除：<https://brycewatson.com/blog/28-claude-code-deletes-old-logs> · <https://claudeissues.com/issue/62476-bug-claude-code-silently-deletes-conversation-transcripts-after-30-days-by-defau>
- Codex hooks：<https://developers.openai.com/codex/hooks>
- Cursor hooks：<https://cursor.com/docs/agent/hooks>
- Gemini CLI hooks：<https://geminicli.com/docs/hooks.md>
- Windsurf 导出需求：<https://feedback.windsurf.com/feature-requests/p/export-chat>
- Agent Client Protocol：<https://agentclientprotocol.com/updates> · crate <https://crates.io/crates/agent-client-protocol>

协议、基准与安全：

- MCP 2026-07-28 changelog：<https://modelcontextprotocol.io/specification/2026-07-28/changelog> · RC 说明 <https://blog.modelcontextprotocol.io/posts/2026-07-28-release-candidate/>
- rmcp：<https://crates.io/crates/rmcp>
- BEAM（ICLR 2026）：<https://arxiv.org/abs/2510.27246>
- 记忆投毒：MINJA <https://arxiv.org/abs/2503.03704> · 攻防评估 <https://arxiv.org/abs/2601.05504> · 综述 <https://vectorize.io/articles/ai-memory-poisoning>

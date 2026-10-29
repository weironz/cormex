# 记忆系统设计

> **迁入说明（2026-10-29）**：本文档原在 Cortex 仓库（`weironz/cortex@4f29191` 的 `docs/memory.md`）。
> 2026-08 拆仓时它没有跟过来，而本仓库的代码与 migration 注释一直在引用它（`docs/memory.md` §七 / §九 / §十一）。
> 正文按原样迁入，只把一处指向 Cortex 侧 `references.md` 的相对链接改成了绝对地址；文中的「Cortex」「本仓库」指当时的记忆引擎，也就是现在的 Cormex。
> 哪些陈述已经过时、与 2026-10 新调研的一致与冲突之处，见
> [research/2026-10-agent-memory-survey.md](research/2026-10-agent-memory-survey.md) §10。

Cortex 的核心。本文档定义记忆的存储模型、写入流程、检索策略与索引方案。

> **地基文档。** schema 变更代价极高，动手写代码前应先在此达成共识。
> 参考调研见 [references.md](https://github.com/weironz/cortex/blob/main/docs/references.md)。
>
> 本文档回答**记忆系统怎么工作**。「**什么东西有资格进记忆**」是另一个问题，
> 见 [memory-content.md](memory-content.md) —— 那份文档基于一轮 58 个系统的
> 全景调研，给出了五类输入（对话 / 工具轨迹 / 工具输出字节 / 版控内文件 /
> 版控外文件）各自的处理定论，以及本文档 §五抽取判据需要补的三个维度。

---

## 一、不可协商的设计原则

| 原则 | 含义 | 为什么 |
|---|---|---|
| **全链路 append-only** | 任何表都不做 `UPDATE` / `DELETE` | 多端并发写的冲突从根本消失。注意：它消灭的是**应用层删改路径**，物理持久性由备份体系兑现（见 architecture.md 备份章节） |
| **双时间轴** | 区分"事情何时变"与"我何时知道" | 支持"三个月前我以为……"这类回溯 |
| **出处可追溯** | 每条提炼的知识都能定位到原始对话 | 差异化据点：可审计，对抗黑盒记忆 |
| **领域无关** | schema 中不出现任何编码专有字段 | 编码与办公必须共用一套底座 |
| **提炼节制** | 原始层全存，事实层严格筛选 | 检索是 top-K，垃圾会挤掉有用的 |

### 关于 append-only 与失效的调和

事实会过期。业界做法（Graphiti）是把旧边的 `invalid_at` 就地更新——**那是 `UPDATE`，会破坏本项目的无冲突性质。**

Cortex 的做法：**失效本身也是一条追加记录**（`fact_events`）。

后果是两台设备可能各自独立地记录同一条事实的失效——这不是冲突，取最早的一条即可。查询有效事实时排除掉被失效记录指向的即可。

---

## 二、分层模型

```
L0  原始层   每一轮对话原样落盘，永不修改      ← 唯一真相来源
L1  事实层   提炼的实体 / 事实 / 关系          ← 日常检索主力
L2  摘要层   会话 / 主题 / 时段摘要
L3  索引层   全文 + 向量 + 图邻接
```

L0 只写不读，仅在需要精确回溯时（"上次我们说的那个"）按全文 + 时间定位。

---

## 三、双时间轴模型

这是本设计最关键的部分。每条事实活在**两条独立的时间线**上：

| 时间轴 | 含义 | 对应字段 |
|---|---|---|
| **事件时间**<br>（valid time） | 事情在**真实世界**中何时为真 | `facts.valid_at` → `fact_events.invalid_at` |
| **系统时间**<br>（transaction time） | Cortex **何时知道**这件事 | `facts.created_at` → `fact_events.created_at` |

### 为什么必须两条

| 场景 | 需要哪条 |
|---|---|
| "我们现在用什么数据库" | 事件时间 |
| "三个月前我以为我们用什么" | **系统时间**——回放当时的认知状态 |
| 今天补录去年发生的事 | 两条必然不同：`valid_at` 是去年，`created_at` 是今天 |
| "我什么时候才知道这件事的" | 系统时间 |

单时间轴无法区分"当时就是这样"和"当时我以为是这样"。这两者的差别，正是可审计记忆的价值所在。

### 两个典型查询

```sql
-- ① 在 T 时刻，Cortex 认为哪些事实为真（认知回放）
SELECT f.* FROM facts f
WHERE f.created_at <= :T
  AND NOT EXISTS (
      SELECT 1 FROM fact_events e
      WHERE e.fact_id = f.id AND e.op = 'invalidate' AND e.created_at <= :T
  );

-- ② 就我们现在所知，真实世界在 T 时刻是什么样（史实重建）
SELECT f.* FROM facts f
WHERE (f.valid_at IS NULL OR f.valid_at <= :T)
  AND NOT EXISTS (
      SELECT 1 FROM fact_events e
      WHERE e.fact_id = f.id AND e.op = 'invalidate' AND e.invalid_at <= :T
  );
```

---

## 四、Schema 设计要点

> **权威版本是 `migrations/20260807000001_init.sql`（在 Cormex 仓库里）。**
> 本节只讲设计意图，不复制 SQL —— 两份拷贝必然漂移。

所有主键为 **ULID**（26 字符 Crockford base32 **大写**，`COLLATE "C"` + 正则 CHECK 强制），
客户端可离线生成，无需协调。向量维度按 `bge-m3` 取 **1024**。

### sync_log —— 同步的唯一事实序

单独一张 outbox 表 `sync_log(seq BIGSERIAL PK, table_name, record_id)`。
**每个业务写事务同时向它追加一行**，客户端增量拉取只面向本表（详见 §九）。

为什么不用各表自带 sync_seq 列（曾经的设计，已废弃）：

1. **九张表各自的 BIGSERIAL 互不可比**，单一游标在数学上不成立；
2. **裸序列做游标必然漏行**：序列值在 INSERT 时分配，但行按提交顺序对读端可见。
   T1 拿到 seq=100 未提交、T2 拿到 101 先提交，客户端把游标推进到 101 后，
   T1 提交的 100 就永久不可见——静默丢数据，除全量重同步外不可修复。

因此写入纪律是 schema 的一部分（migration 头部注释同款）：
业务行 + sync_log 同事务；事务开头 `pg_advisory_xact_lock(4272)` 串行化提交序；
取号事务短小纯写，LLM 与 embedding 计算一律在事务外。

### 分层表

| 层 | 表 | 要点 |
|---|---|---|
| L0 | `episodes` | 原始消息 JSONB 无损保存（含 thinking 块）；`text`+`tsv` 供全文检索；`occurred_at`（客户端、经单调化）与 `created_at`（服务端）分离 |
| L0 | `blobs` / `episode_blobs` | SHA-256 内容寻址，天然去重、天然不可变 |
| L0 | `blob_transcripts` | 媒体转录（ASR/vision/OCR）；`span_start_ms/span_end_ms` 支撑「出处跳转到 31:40」；`transcribed_by` 与 `embedding_model` 相互独立 |
| L1 | `entities` | kind 开放不枚举；向量仅用于抽取期实体消解，不参与召回 |
| L1 | `entity_merges` | 别名消解，追加式；**`UNIQUE(from_entity)`** 使归属图成为函数图 |
| L1 | `facts` | 主谓宾 + `statement` 自然语言表述；`source_episode_id NOT NULL` 是可审计的根基 |
| L1 | `fact_events` | 生命周期事件（见下） |
| L2 | `summaries` | 会话 / 主题 / 时段摘要 |
| — | `session_events` | 会话生命周期（改名 / 归档 / 绑定工作区），见下 |
| — | `redactions` | 抹除墓碑（见 §十一） |

### fact_events —— 事实的生命周期

三种 `op`：

| op | 语义 | 约束 |
|---|---|---|
| `invalidate` | 使事实失效 | 必须给 `kind` + `invalid_at`；`kind=superseded` 时 `superseded_by` 必填（否则演化链断裂） |
| `revoke` | 撤销失效（恢复） | 不得携带 invalidate 专属字段 |
| `flag` | 标记「矛盾待人工确认」 | **批注，不改变状态**，`active_facts` 不受影响 |

**revoke 取状态机语义**：一条事实的当前状态由**最后一条状态事件**（invalidate/revoke，
flag 除外）决定，不是「撤销栈」——序列 `[invalidate(A), invalidate(B), revoke]` 之后事实**有效**。

`confidence` 的定义：抽取器写入时对「该事实忠实反映原文」的一次性打分，**此后不可变**
（append-only）；矛盾待确认不降 confidence，而是追加 `flag` 事件。

### entity_merges —— first-writer-wins

并发分叉（两台设备把 A 分别并进 B 和 C）由 `UNIQUE(from_entity)` 在物理层拒绝后到者：
**first-writer-wins**，服务端回执后到客户端转 no-op。
环（A→B + B→A）由 cortexd 写入前沿 `into_entity` 链走到底做成环检测拒绝；
视图中的 depth 上限只是兜底保险。

### session_events —— 会话的生命周期

会话本身没有实体表（`session_id` 由客户端生成，与 `episodes` 一致），
状态全部由这张 append-only 的事件表推导。

`op` 覆盖**三个互相独立的维度**：`rename` / `archive`+`unarchive` /
`bind_workspace`+`unbind_workspace`。

> **状态机是按维度的，不是按表的。** 用单个 `DISTINCT ON (session_id)`
> 取「最后一条事件」是错的：序列 `[archive, rename]` 的末条是 rename，
> 但会话仍然处于归档状态。`session_state` 视图因此取**三条各自的**
> 末事件再 join，每条都按 `(created_at DESC, id DESC)` 排序（与 `fact_status`
> 同一个 tiebreaker，两台设备在同一微秒改名才会收敛到同一结果而非各自分叉）。

**归档 ≠ 删除。** 归档只是从列表隐藏，数据完好；真正的销毁是
`redact`/`purge`（§十一），语义不同、要二次确认。

CHECK 是双向的：`rename` 必须有 title，且**只有** `rename` 可以有。
单向版本会让 `archive` 携带 title，在视图里表现为「归档顺便改了名」——
无从追查。

### 视图

- `fact_status`：每条事实最近一次**状态**事件（过滤掉 flag），排序 `(created_at DESC, id DESC)`
- `active_facts`：从未失效或已恢复的事实——日常检索的入口
- `canonical_entities`：沿函数图走到终点的归属解析（链式合并 A→B→C 正确解析为 A→C）
- `session_state`：会话的当前标题 / 归档状态 / 绑定的工作区（三维度各取末事件后 join）


## 五、写入流程

```
一轮对话结束
      │
      ├─► 同步：episodes 落库（含 blob 引用）        ← 绝不阻塞对话
      │
      └─► 异步任务队列
             ├─ 媒体转录（ASR / vision / OCR）
             ├─ 事实抽取
             └─ 摘要生成（会话结束时触发）
```

### 事实抽取的唯一判据

> **下次对话如果不知道这条，会不会做错决定？**

不满足则不抽。明确不抽的：

- 寒暄与情绪表达
- 一次性的事实查询（"现在几点"）
- 代码片段本身（L0 已完整保留；L1 只记"决定采用 X 方案"）
- 模型自身的推理过程

抽多了比抽少了更糟——检索是 top-K，噪声会把有用的挤出去。

### 矛盾消解：确定性逻辑，不交给 LLM 判断

新事实入库时：

```
1. 检索同一 (subject_id, predicate) 的现存有效事实
2. 若无 → 直接写入，结束
3. 若有 → 判定关系：
      补充   → 两条并存
      取代   → 写入新 fact，并追加 fact_events:
                 op            = 'invalidate'
                 kind          = 'superseded'
                 fact_id       = 旧事实
                 invalid_at    = 新事实的 valid_at
                 superseded_by = 新事实 id
                 actor         = 'system'
                 created_at    = now()  ← 系统时间自动记录
      矛盾且无法判定 → 两条并存，各追加一条 op='flag' 事件（批注，不改状态），
                       进入 UI 的「待确认列表」。confidence 不可变，不存在「降低」操作
```

时间区间不重叠的事实**不构成矛盾**（Graphiti 的 `resolve_edge_contradictions` 同此逻辑）。

---

## 六、检索流程

```
用户输入
    │
    ├─ ① BM25 全文        tsv @@ query          ← 专有名词、精确匹配
    ├─ ② 向量余弦          pgvector HNSW         ← 语义相近
    ├─ ③ 图遍历 1–2 跳     从命中实体扩展关联事实   ← 关系推理
    └─ ④ 时间近因          最近 N 条              ← 上下文连续性
              │
              ▼
       RRF 融合（Reciprocal Rank Fusion）
              │
              ▼
       域加权（当前 domain 的事实加分）
              │
              ▼
       时间过滤（排除已失效，除非用户显式问历史）
              │
              ▼
       token 预算硬截断（≤ context 的 10–15%）
```

### 为什么用 RRF

不需要调权重、对各路分数尺度不敏感，是混合检索的默认最优解：

```
score(d) = Σ  1 / (k + rank_i(d))      k 取 60
          i∈召回路
```

**不要一开始就用加权求和**——四路分数量纲不同，权重调不出来。

### token 预算是硬约束

不设上限的话，记忆会缓慢吃掉整个上下文窗口。超预算按融合分数截断，宁可少给。

---

## 七、索引与冷热分层

**核心认知：向量索引才是最贵的部分**（HNSW 的内存占用与构建开销）。
所以分层的实质不是"存哪里"，而是**"哪些内容值得建向量索引"**。

| 内容 | 向量 | 全文 | 理由 |
|---|:---:|:---:|---|
| L1 事实 | ✅ | ✅ | 数量少、价值密度高 |
| L2 摘要 | ✅ | ✅ | 同上 |
| 媒体转录文本 | ✅ | ✅ | 媒体的唯一检索入口 |
| **L0 原始对话** | ❌ | ✅ | **数量是事实的几十倍**；精确回溯用全文 + 时间足矣 |
| 媒体二进制 | — | — | 永远在 RustFS，按需拉取 |

此策略下向量索引规模约为 episodes 的 1/50，**十万级对话不会爆**。

完整索引清单见 `migration`（在 Cormex 仓库里）。要点：

- `entities` 也建向量索引，但**仅用于抽取期实体消解**（同名/近义实体匹配），不参与四路召回
- `idx_facts_sp (subject_id, predicate)` 的前缀即覆盖按主语查询，不另建单列索引
- `idx_facts_source (source_episode_id)` 供 redact 级联按出处定位派生行

**冷迁移暂不实现。** L0 超过约十万条后，再考虑将老 episodes 的正文移入 RustFS，库中仅保留元数据与 `tsv`。

### 换模型不停机

不同 embedding 模型产生的向量**不在同一个语义空间**——坐标系不同，混在一起算距离毫无意义。所以更换模型意味着全部向量作废、索引重建。

十万条记忆时这是数十分钟到数小时的停机。为避免这种局面，所有带 `embedding` 的表都必须记录 `embedding_model`：

| 表 | 字段 |
|---|---|
| `facts` / `entities` / `summaries` / `blob_transcripts` | `embedding_model TEXT NOT NULL` |

有了它，换模型可以渐进进行而无需停服：

```
1. 新写入的一律用新模型
2. 后台任务按 embedding_model 分批回填旧记录
3. 检索期间按模型分组，各自召回后再由 RRF 融合
   （不同空间的分数不可直接比较，但 RRF 只用排名，天然兼容）
4. 全部回填完成后，删除旧模型的向量与索引
```

**维度变化需额外处理**：`vector(1024)` 写死在列定义中，换成其他维度须新增一列（如 `embedding_v2 vector(1536)`），迁移完毕后再删除旧列。

---

## 八、中文分词 —— 必须提前决定

PostgreSQL 默认不支持中文分词，`to_tsvector('simple', ...)` 对中文等于按整句切分，全文检索基本失效。

| 方案 | 评价 |
|---|---|
| `zhparser` / `pg_jieba` 扩展 | 效果好，但**依赖数据库能装扩展**；托管 PG 常不允许 |
| `pg_trgm` 三元组 | 不需分词，但召回噪声大 |
| **应用层分词后写入 `tsvector`** | ✅ **采用** |

**决定：在 Rust 侧用 `jieba-rs` 分词，拼成空格分隔的词串后 `to_tsvector('simple', ...)` 写入 `tsv` 列。**

理由：不依赖任何 PG 扩展，部署自由；分词逻辑在应用内可控可调；将来换分词器只需重跑一遍。

---

## 九、多端同步

### 下行：单游标增量拉取，面向 sync_log

```
客户端持有 last_seq（单一游标，跨全部表）
      │
      ├─► GET /sync?since={last_seq}
      │
      └─◄ 服务端返回 sync_log 中 seq > since 的记录及对应业务行，按 seq 升序
```

- 全序由 `sync_log` 提供（见 §四）；按 log 序回放天然满足 FK 顺序
  （facts 永远在其 source episode 之后到达）
- 服务端写入纪律（advisory lock 串行化提交）保证 **seq 顺序 == 可见顺序**，
  游标推进永不漏行
- `sync_log` + `LISTEN/NOTIFY` 即 WebSocket 实时推送的事件源——
  「实时同步」不是轮询

### 上行：本地 op-log 队列

- 客户端离线写入记录到本地 SQLite 的**单一全局 op-log**（跨表按本地写入序），
  按 log 序上传，不按表分批（否则 FK 必然违反）
- 服务端批量 ingest：单事务应用、按 ULID 主键 `ON CONFLICT (id) DO NOTHING`、
  逐条返回 ack；客户端收到 ack 才出队（at-least-once + 幂等 = exactly-once 效果）
- blob 三步固定顺序：RustFS 上传 → `blobs` 行 → episode + `episode_blobs` 行；
  中断按队列重放（内容寻址天然幂等）；孤儿 blob 由服务端周期 GC
- 回声去重：自己上传的记录会随下行同步回流，按 ULID 幂等跳过
- 队列出队与游标推进在本地 SQLite 同一事务持久化

### 时钟

- `occurred_at` 用客户端**单调化墙钟**：发号时取 `max(now, last_issued + 1ms)`
  并持久化 `last_issued`——防时钟回跳，设备内事件序永不倒流（约 20 行代码，
  拿走 HLC 九成收益）；ULID 时间戳位复用此值
- 跨设备排序一律以服务端 `sync_log.seq` 为准

### 冲突面

append-only 下数据级冲突不存在，仅剩两类语义仲裁：

| 冲突 | 仲裁 |
|---|---|
| `entity_merges` 并发分叉 | **first-writer-wins**（`UNIQUE(from_entity)` 物理拒绝后到者，服务端回执后客户端转 no-op） |
| 同一 fact 并发 invalidate + revoke | 状态机语义：按 `sync_log` 全序回放，末态生效 |

### redaction 的传播义务

redact/purge 是 UPDATE，不会产生新的业务行版本。传播靠 `redactions` 墓碑行
（本身走 sync_log 下发）：

> **客户端收到 redaction 行后，必须幂等执行同等本地清除**——
> 清 episode 正文与 tsv、删本地 blob 缓存、清关联派生行。
> 这是同步协议的一等公民，列入验收测试。

新设备全量同步天然拿到已清空版本，无需特殊处理。

### 边界条件（写死，防无意识越界）

当前全部同步简化（无 HLC、无 CRDT、服务端定序）的前提是**星型拓扑**
（所有设备只与一个 hub 同步）。若未来引入设备间 P2P 或多 hub 部署，
本节全部决策需要重评。

## 十、成本控制

图谱式记忆的持续成本在于**每轮对话都要调用 LLM 做抽取**。

| 措施 | 说明 |
|---|---|
| 抽取用廉价模型 | flash / mini 级即可，**不要用主对话模型** |
| 异步 + 批量 | 累积数轮一次性抽取，而非每轮一次 |
| 预筛 | 纯查询类、无新信息的轮次直接跳过抽取 |
| Embedding 本地化 | 本地 ONNX 运行 `bge-m3`，零 API 成本，且**记忆内容不出网** |

---

## 十一、记忆的可编辑性

**"可审计"是 Cortex 的核心差异化。** 用户必须能看到记忆、追问出处、修正错误、删除不想要的——同时审计链保持完整。

### 四类操作

用户的编辑与系统的自动失效**共用 `fact_events` 表**，靠 `kind` 与 `actor` 区分：

| 用户动作 | 落库 | 语义 |
|---|---|---|
| **修正**一条事实 | 追加新 `fact` + `op=invalidate, kind=corrected, superseded_by=新事实` | 抽取错了，旧的**从未为真** |
| **删除**一条记忆 | `op=invalidate, kind=retracted` | 不表态真值，仅不再使用 |
| **恢复**已删除的 | `op=revoke` | 撤销上一次失效 |
| **抹除**原始内容 | 见下文「redact 与 purge」 | 真正销毁数据 |

`actor` 一律记为 `user`，并要求填写 `reason`。

### 为什么必须区分 corrected / retracted / superseded

三者在时间轴上的语义完全不同，混为一谈会让认知回放出错：

| kind | 旧事实曾经为真吗 | `invalid_at` 取值 |
|---|---|---|
| `superseded` | ✅ 曾经为真，世界变了 | 世界变化的时刻 |
| `corrected` | ❌ **从未为真**，抽取错误 | 等于 `facts.valid_at`，即自始无效 |
| `retracted` | 🤷 不表态 | 用户操作的时刻 |
| `expired` | ✅ 曾经为真，到期了 | 到期时刻 |

这个区分还有一个实际价值：`corrected` 占比是**抽取精确率的有偏滞后下界**——它依赖检索曝光与用户勤快度，测不到漏抽、未被检索的错误与实体挂错，不可当作抽取质量的直接度量，但仍是最便宜的一路信号。

### 删除后，历史回放里还看得见吗

**看得见，而且这是正确行为。**

认知回放查询（§三 查询①）的条件是 `e.created_at <= :T`。用户今天删除的记忆，其 `fact_events.created_at` 是今天——查询三个月前的认知状态时不会被排除。

> 三个月前你**确实**是这么认为的。事后删除不能改变当时的事实。

这正是双时间轴存在的意义。日常检索走 `active_facts` 视图，看不到已删除的；审计视图则完整可见。

### redact 与 purge —— 唯一允许破坏 append-only 的操作

有些内容必须真正销毁：误粘贴的 API key、他人隐私、法律要求删除的数据。对这些，"隐藏"是不够的。

| 操作 | 行为 | 审计链 |
|---|---|---|
| **redact** | 按 `source_episode_id` **级联清除全部派生落点**（见下） | ✅ 完整——知道此处曾有内容、何时被抹除、由谁 |
| **purge** | 同上，并从 RustFS 删除关联 blob 及备份镜像中的对象 | ✅ 同上 |

**级联清除范围**（秘密的所有落点，缺一个承诺就是假的）：

| 落点 | 处理 |
|---|---|
| `episodes.content` / `text` / `tsv` | content 写占位 `{"redacted":true}`，text/tsv 置 NULL |
| 派生 `facts.statement` / `tsv` / `embedding` | statement 写占位 `[redacted]`，tsv/embedding 置 NULL（向量可近似反演原文，必须一并清） |
| 关联 `blob_transcripts.text` / `tsv` / `embedding` | 同上 |
| 涉及的 `summaries` | **删除后排除被抹除 episode 重新生成**（整体清空会连带销毁无辜内容） |
| 各设备本地缓存 | 靠 redaction 墓碑传播，客户端义务见 §九 |

**在途任务防护**：抽取 / 转录 pipeline 在写入任何派生行之前必须查 `redactions` 表，
防止 redact 执行时在途的异步任务事后把秘密回填。级联清除任务本身可重跑（幂等）。

表结构见 `migration`（在 Cormex 仓库里） 的 `redactions`
（墓碑行本身走 `sync_log` 下发到所有设备）。

约束：

- **必须显式触发**，需二次确认，绝不自动执行
- **墓碑记录本身不可删除**——可以抹掉内容，但"这里发生过抹除"永远留存
- 由被抹除 episode 派生的 `facts` 除内容被级联清空外，还需一并 `invalidate`（`kind=retracted`），使其退出检索
- 内容寻址的 blob 需先确认无其他 episode 引用，方可 purge

这是全系统唯一的例外。文档开头「不可协商的设计原则」中的 append-only，在此处让位于用户对自己数据的处置权——但**让位的是内容，不是审计链**。

### 界面应提供的能力

| 能力 | 说明 |
|---|---|
| **为什么记得这个** | 沿 `source_episode_id` 跳转到产生该记忆的原始对话 |
| **它是怎么变的** | 展示该事实的完整 `fact_events` 时间线 |
| **当时我以为什么** | 按系统时间回放任意历史时点的认知状态 |
| 修正 / 删除 / 恢复 | 对应上表四类操作 |
| 抹除 | 危险操作，独立入口 + 二次确认 |

---

## 十二、待决问题

| 问题 | 现状 |
|---|---|
| Embedding 模型与维度 | **已定** `bge-m3` / 1024。所有向量表记录 `embedding_model`，支持不停机迁移（§七） |
| 同步协议 | **已定** sync_log outbox + advisory lock 串行化 + 上行 op-log（§九），2026-08 复审重造 |
| 记忆的用户可编辑界面 | **已定** 见 §十一。编辑/删除均追加 `fact_events`，`redact`/`purge` 为唯一例外 |
| cross-encoder 重排是否引入 | 提升质量但增加延迟。第一版先只用 RRF，实测不足再加 |
| 摘要的触发时机与粒度 | 会话结束触发已定；主题级与时段级摘要的划分方式未定 |
| 视频处理策略 | 抽帧频率、是否做场景切分，待第一版媒体 pipeline 落地后评估 |
| `entity_merges` 的撤销 | 现为 first-writer-wins + 不可逆。引入 revoke 需把 `UNIQUE(from_entity)` 改 partial index，推迟到有真实需求时 |
| 检索评测体系 | **P1 必做**：LongMemEval-S 回放 + 自建中英双语私有集 + retrieval_traces 遥测表，动检索代码前定，详见 roadmap.md |
| 记忆注入契约 | **P1 必做**：注入位置（缓存前缀 vs 回合块）、格式（带 fact id 的定界块）、框定语义（防记忆投毒）、预算双帽，写 agent loop 前定，详见 roadmap.md |

---

## 附：与业界方案的对照

| 流派 | 代表 | 我们的取舍 |
|---|---|---|
| 向量派 | mem0 | ❌ 无法表达关系与时间，给不了出处链 |
| **图谱派** | **Graphiti / Zep**、cognee | ✅ **采用**——可审计与时间维度只有这一派能提供 |
| 上下文派 | Letta / MemGPT、QwenPaw | ⚠️ 借鉴其"索引召回而非摘要丢弃"的理念，但不采用其不可控的换页机制 |

相对 Graphiti 的两处改动：

1. **失效改为追加记录**而非就地更新，以保全 append-only 与无冲突同步
2. **不引入图数据库**，图结构直接由 `facts` 表的邻接关系承载，减少一个组件

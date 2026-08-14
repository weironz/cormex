//! 跨域连接 —— 用 `relates_to` 边把「编码」与「办公」两座记忆孤岛接上。
//!
//! ```text
//! 夜间扫描
//!     │
//!     ├─① 候选   向量近邻 → 跨域 → 尚未连通 → 相似度落在带内   ← 便宜，管召回
//!     ├─② 判定   廉价模型逐对判「值不值得连」                  ← 贵，管精度，事务外
//!     ├─③ 计划   实体对 → 两条有向 relates_to 的 NewFact       ← 纯逻辑
//!     └─④ 落库   一个 write_txn 写完                           ← 唯一进事务的一步
//! ```
//!
//! # 为什么这件事不需要动检索代码
//!
//! `recall_graph` 沿 `facts` 的邻接关系走（`subject_id → object_entity_id`），
//! 而 `facts.predicate` 在 schema 里刻意不做枚举。于是「多写一条边」这件事
//! **本身就是**「让图遍历多带出一片子图」——检索侧一行都不用改。
//! 这是这套 schema 设计最漂亮的地方，也是本模块要保住的性质。
//!
//! 唯一的例外在融合侧：`relates_to` 带出来的东西**按定义就是跨域的**，
//! 而 [`crate::fusion::apply_domain_boost`] 会给当前领域的事实加分、
//! 等价于给跨域的减分。刚架好桥又把它压下去等于白做，所以那里需要一处豁免
//! （范围的取舍见 [`crate::retrieval`] 里 `bridged_entities` 的注释）。
//!
//! # 三条纪律
//!
//! 1. **LLM 与 embedding 一律在事务外算完**（CLAUDE.md 约束 2 的推论）。
//!    本模块把「决定写什么」与「写」拆成 [`Linker::plan`] 与 [`Linker::write`]，
//!    形状与 [`crate::extract`] 的 `extract` / `write_candidates` 一致。
//! 2. **不能加唯一约束**（schema 不在本模块的改动范围内），所以幂等靠写前查：
//!    见 [`LinkCorpus::out_edges`]。
//! 3. **噪声边在图遍历里会传染** —— 一条错边让整个无关子图被召回。
//!    因此有度数上限、批次上限，且默认判定器不是「向量够近就连」。

use std::collections::{HashMap, HashSet};

use cortex_core::{CortexError, Id, Result};
use cortex_llm::{LlmClient, Message};
use cortex_store::{Fact, FactSource, NewFact, ProvenanceRef, RedactionTarget, Store, Vector};
use serde_json::Value;

use crate::embed::Embedder;
use crate::tokenize;

/// 跨域连接用的谓词。
///
/// 它不在 [`crate::extract::DEFAULT_FUNCTIONAL`] 里，因此是**多值**谓词：
/// 同一个实体可以连出多条边而不会互相取代。这是想要的 —— 一个模块本来就
/// 可能同时对应一次会议决策和一条预算约束。
pub const RELATES_TO: &str = "relates_to";

/// 写进 `facts.confidence` 的值。
///
/// # 为什么是 0.30，以及它现在**并不起作用**
///
/// roadmap 只说「低 confidence」。定成 0.30 的依据是它必须明显低于抽取器的
/// 产物：解析器缺省给 0.80，题集手写的是 0.90，实践中没见过低于 0.50 的。
/// 于是将来任何「只把 confidence ≥ 0.5 的事实当作断言写进 prompt」的闸门
/// 都会天然把这些推断出来的边挡在断言之外，同时不影响图遍历把它们走通。
///
/// **但必须说清楚现状**：把 `confidence` 全仓库 grep 一遍，检索侧
/// （`fusion` / `retrieval`）与注入侧（`injection`）一处都没有读它，
/// 只有 DTO 与同步载荷在原样透传。也就是说今天写 0.30 还是 0.99，
/// 对召回与排序**完全没有区别**。这个数字现在是给人看的与给将来留的，
/// 不是一道生效中的闸门 —— 谁要拿它当安全边界，得先去把闸门建出来。
pub const DEFAULT_LINK_CONFIDENCE: f32 = 0.30;

/// 候选的余弦距离上界。超过它就当作无关。
///
/// 参照物是 [`crate::retrieval::BGE_M3_SEMANTIC_FLOOR`]（0.50）：那是
/// 「全库最近的一条都够不着 ⇒ 这个问题的答案不在记忆里」的判据。
/// 两条**事实之间**要算「值得连」，理应比「查询勉强够得着答案」更严，
/// 所以取到它以内。0.45 是起点不是定论 —— 换 embedding 模型必须重标。
///
/// 散列桩（`hash-stub-v1`）下这个带几乎捞不到东西（69 条语料只出 1 对），
/// 那是**对的失败方式**：它的「距离」只反映字符 bigram 重合度，不是语义，
/// 与其在离线 CI 里凭字形连一堆边，不如什么都不连。
pub const DEFAULT_MAX_DISTANCE: f64 = 0.45;

/// 候选的余弦距离下界。比它还近的**不连**。
///
/// # 语义相近 ≠ 值得连
///
/// 「Cortex 用 Rust 写」与「Cortex 用 Rust 写」之间距离趋近 0，连起来
/// 一个 bit 的信息都不产生，却白白多一条边、多一份注入预算、多一个
/// 图遍历的分叉。真正有价值的是**同一个话题、两套词汇**：
/// 「上周会上定的那个方案」对「代码里那个模块」—— 那种关系在向量空间里
/// 有距离但不远。所以候选是一个**带**而不是一个上界。
///
/// 下界还顺带挡住一类真实噪声：同一件事在两个 domain 各被抽了一遍
/// （会议纪要与代码注释重复陈述同一个决定）。那是**该合并的重复**，
/// 不是该架桥的关联，交给实体消解与矛盾消解，不该在这里补一条边。
pub const DEFAULT_MIN_DISTANCE: f64 = 0.12;

/// 单个实体最多连出多少条 `relates_to`。
///
/// # 这是噪声控制的主闸门
///
/// 图遍历的代价结构决定了度数上限比距离阈值更要紧：一条错边只是多带一条
/// 事实，但一个**枢纽实体**（被 20 条边连着）会在任何提到它的查询上
/// 把二十片无关子图一起拖进来 —— 噪声在图里是会传染的。距离阈值管不了
/// 这个，因为每一条边单独看都「够近」。
///
/// 4 的依据是量纲而非曲线：图遍历默认 2 跳，度数 d 的枢纽在两跳内
/// 能带出 O(d²) 片子图，d=4 时是 16，与 `RecallWidth::graph`（30 条）
/// 同量级；d=10 就是 100，直接吃满整路召回。
pub const DEFAULT_DEGREE_CAP: usize = 4;

/// 每条待考察事实取多少个向量近邻。
///
/// 与 `RecallWidth::vector` 同宽。近邻里绝大多数是同域的（同域语料本来
/// 就更密），跨域的只剩尾巴上几条 —— 取窄了这一路就直接哑火。
pub const DEFAULT_NEIGHBOR_WIDTH: i64 = 40;

// ══════════════════════════════════════════════════════════
//  语料来源
// ══════════════════════════════════════════════════════════

/// 扫描要读的东西。
///
/// 抽成 trait 而不是直接吃 `&Store`，理由与 [`crate::extract::EntityIndex`]
/// 同款：`cortex-store` 现在没有「按游标分页遍历全部事实」这样的方法，
/// 而给存储层加方法不在本模块的改动范围内。默认实现 [`StoreCorpus`] 用
/// 现有的三个公开查询拼出来，够跑；等存储层补上 `facts_since(cursor)`
/// 之后换个实现即可，`Linker` 一行不用改。
///
/// **已知的界**：[`StoreCorpus::scan_batch`] 受存储层 `MAX_LIMIT`（200）
/// 的钳制，一次扫不完更大的增量。夜间扫描按天跑的话 200 条/天是够的，
/// 但这是个真实的天花板，不是可以忽略的细节。
#[async_trait::async_trait]
pub trait LinkCorpus: Send + Sync {
    /// 本轮要作为「左侧」考察的事实。
    async fn scan_batch(&self) -> Result<Vec<Fact>>;

    /// 向量近邻，按余弦**距离**升序（不是相似度）。
    async fn neighbors(
        &self,
        embedding: &Vector,
        model: &str,
        limit: i64,
    ) -> Result<Vec<(Fact, f64)>>;

    /// 该实体已经连出去的全部实体（任意谓词，只看**有效**事实）。
    ///
    /// 一个方法同时兑现两件事：
    ///
    /// 1. **幂等** —— 第二次扫描时上一次写的边就在这里面，不会重复写。
    ///    append-only 且不能加唯一约束，写前查是唯一的去重手段。
    /// 2. **别做无用功** —— 两条事实若已经共享实体或已经直接相连，
    ///    图遍历本来就能从一边走到另一边，再补一条 `relates_to` 只是噪声。
    ///    真正的缺口只在「语义相关但图上不通」的那些对上。
    async fn out_edges(&self, subject_id: &str) -> Result<Vec<String>>;

    /// 实体的显示名。`statement` 与判定 prompt 都要用它。
    async fn entity_name(&self, entity_id: &str) -> Result<Option<String>>;
}

/// 用 `Store` 现有的公开查询拼出来的默认实现。
pub struct StoreCorpus<'a> {
    store: &'a Store,
    /// `scan_batch` 回看多少天
    within_days: i64,
    /// `scan_batch` 最多取多少条（存储层还会再钳到 200）
    limit: i64,
}

impl<'a> StoreCorpus<'a> {
    #[must_use]
    pub fn new(store: &'a Store) -> Self {
        Self {
            store,
            within_days: 1,
            limit: 200,
        }
    }

    /// 回看窗口。夜间扫描跑「昨天写进来的」，重建索引时可以放宽。
    #[must_use]
    pub fn with_window(mut self, within_days: i64, limit: i64) -> Self {
        self.within_days = within_days;
        self.limit = limit;
        self
    }
}

#[async_trait::async_trait]
impl LinkCorpus for StoreCorpus<'_> {
    async fn scan_batch(&self) -> Result<Vec<Fact>> {
        self.store
            .recall_recent(self.within_days, self.limit)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))
    }

    async fn neighbors(
        &self,
        embedding: &Vector,
        model: &str,
        limit: i64,
    ) -> Result<Vec<(Fact, f64)>> {
        self.store
            .recall_vector_scored(embedding, model, limit)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))
    }

    async fn out_edges(&self, subject_id: &str) -> Result<Vec<String>> {
        let facts = self
            .store
            .active_facts_by_subject(subject_id, 200)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))?;
        Ok(facts
            .into_iter()
            .filter_map(|f| f.object_entity_id)
            .collect())
    }

    async fn entity_name(&self, entity_id: &str) -> Result<Option<String>> {
        Ok(self
            .store
            .entity(entity_id)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))?
            .map(|e| e.name))
    }
}

// ══════════════════════════════════════════════════════════
//  候选与判定
// ══════════════════════════════════════════════════════════

/// 候选一侧的锚点：这条事实，以及它挂在哪个实体上。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub fact_id: String,
    /// 锚点实体 = 该事实的 `subject_id`。
    ///
    /// 用主语而不是宾语：`statement` 是围绕主语组织的，宾语常常是字面量
    /// （根本没有实体 id）。挂错锚点的后果不是「少召回」而是「召回一片
    /// 不相干的东西」，所以宁可挂在最稳的那个位置上。
    pub entity_id: String,
    pub entity_name: String,
    pub statement: String,
    pub domain: String,
    pub source_episode_id: String,
}

/// 一对通过预筛的候选。
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub left: Anchor,
    pub right: Anchor,
    /// 两条事实 statement 向量的余弦距离
    pub distance: f64,
}

impl Candidate {
    /// 实体对的无序规范键 —— 去重与度数统计都按它。
    #[must_use]
    pub fn pair_key(&self) -> (String, String) {
        pair_key(&self.left.entity_id, &self.right.entity_id)
    }
}

fn pair_key(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_owned(), b.to_owned())
    } else {
        (b.to_owned(), a.to_owned())
    }
}

/// 判定结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    pub candidate: Candidate,
    /// 为什么值得连。会写进 `statement`，是这条边**唯一**的可审计说明 ——
    /// 没有它，用户看到一条 `relates_to` 只能猜它凭什么存在。
    pub reason: String,
}

/// 「这一对值不值得连」的判定器。
///
/// # 为什么必须是可换的
///
/// 纯向量距离会产出大量噪声边，而噪声边在图遍历里会传染；LLM 判定质量高
/// 但每对都要花钱。**在哪里花钱**是这个模块最重要的一个取舍，所以它是
/// 一个显式的接口而不是写死的逻辑：
///
/// | 实现 | 成本 | 用途 |
/// |---|---|---|
/// | [`AcceptAll`] | 0 | 测量「预筛单独能做到什么」的对照组；确定性，可进 CI |
/// | [`CheapModelJudge`] | 每批一次廉价模型调用 | 生产 |
///
/// 注意 [`AcceptAll`] **不是**「关掉判定」的同义词 —— 它是一个有意义的
/// 对照组：预筛已经做了跨域、未连通、距离带三道过滤，AcceptAll 量的
/// 正是这三道过滤单独的精度。评测里拿它当基线，才知道模型判定买到了什么。
#[async_trait::async_trait]
pub trait LinkJudge: Send + Sync {
    /// 判定一批候选。返回值只含**接受**的那些，顺序不必与输入一致。
    async fn judge(&self, candidates: &[Candidate]) -> Result<Vec<Accepted>>;
}

/// 全部接受 —— 预筛的对照组，确定性，不花钱。
#[derive(Debug, Clone, Copy, Default)]
pub struct AcceptAll;

#[async_trait::async_trait]
impl LinkJudge for AcceptAll {
    async fn judge(&self, candidates: &[Candidate]) -> Result<Vec<Accepted>> {
        Ok(candidates
            .iter()
            .map(|c| Accepted {
                candidate: c.clone(),
                reason: "向量近邻预筛".to_owned(),
            })
            .collect())
    }
}

const JUDGE_SYSTEM: &str = r#"你在为一个记忆系统判定「两条来自不同领域的记忆，值不值得建立一条关联」。

# 判据只有一条

> 用户下次在聊 A 的时候，被顺带提醒 B，是**帮忙**还是**打扰**？

是帮忙才连。**宁可漏连也不要错连** —— 这条关联会让检索把 B 那一侧的
整片记忆一起带出来，连错一条，之后每次聊到 A 都会多出一堆无关内容。

# 明确不连

- 只是话题相邻、词面相近，但一方对另一方没有任何实际影响
- 两条说的其实是同一件事（重复陈述，该去重不该连）
- 只靠「都提到了同一类东西」（都是人、都是工具、都是日期）建立的联想
- 泛泛的上下位关系（「都属于这个项目」）

# 该连

一方**约束**、**解释**、**触发**或**落实**另一方。典型：
会上定的方案 ↔ 代码里的实现；预算上限 ↔ 选型决定；
个人偏好 ↔ 它影响的工程做法；人员分工 ↔ 他负责的那个模块。

# 输出格式

只输出 JSON，不要任何解释文字：

{"links": [{"id": 候选编号, "reason": "不超过 30 字，说清这两者是怎么互相影响的"}]}

一对都不该连时输出 {"links": []}。reason 写不出具体影响的，就是不该连。"#;

/// 用廉价模型判定。**绝不用主对话模型** —— 这是每晚都要跑的东西。
pub struct CheapModelJudge {
    llm: LlmClient,
    /// 一次请求塞多少对。批量是这里唯一的省钱手段：system prompt 比
    /// 单对候选长得多，一对一请求等于每次重付一遍它。
    batch: usize,
}

impl CheapModelJudge {
    #[must_use]
    pub fn new(llm: LlmClient) -> Self {
        Self { llm, batch: 12 }
    }

    #[must_use]
    pub fn with_batch(mut self, batch: usize) -> Self {
        self.batch = batch.max(1);
        self
    }
}

impl std::fmt::Debug for CheapModelJudge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CheapModelJudge")
            .field("batch", &self.batch)
            .finish_non_exhaustive()
    }
}

/// 判定 reason 的长度上限。
///
/// 不只是为了好看：`reason` 会原样进 `statement`，而 `statement` 会进
/// prompt。让模型自由发挥等于给注入预算开了一个没有上界的口子。
const MAX_REASON_CHARS: usize = 40;

#[async_trait::async_trait]
impl LinkJudge for CheapModelJudge {
    async fn judge(&self, candidates: &[Candidate]) -> Result<Vec<Accepted>> {
        let mut out = Vec::new();
        for chunk in candidates.chunks(self.batch) {
            let prompt = build_judge_prompt(chunk);
            let messages = [Message::user().with_text(prompt)];
            let (reply, _usage) = self
                .llm
                .complete(self.llm.cheap_model(), JUDGE_SYSTEM, &messages, &[])
                .await?;
            out.extend(parse_judgements(&reply.as_concat_text(), chunk));
        }
        Ok(out)
    }
}

fn build_judge_prompt(chunk: &[Candidate]) -> String {
    let mut s = String::with_capacity(chunk.len() * 256 + 128);
    s.push_str("# 待判定的候选\n\n");
    for (i, c) in chunk.iter().enumerate() {
        s.push_str(&format!(
            "## 候选 {i}\n- A（{}）：{}\n- B（{}）：{}\n\n",
            c.left.domain, c.left.statement, c.right.domain, c.right.statement
        ));
    }
    s.push_str("只输出 JSON。");
    s
}

/// 解析判定结果。**逐条容错**：某一条编号越界只丢那一条。
///
/// 与 [`crate::extract::parse_candidates`] 不同，这里解析失败不报错而是
/// 返回空 —— 判定失败的正确行为是「这批不连」，而不是让整个夜间扫描红掉。
/// 少一条边只是少一点召回，扫描崩掉则是下一晚才有人发现。
fn parse_judgements(raw: &str, chunk: &[Candidate]) -> Vec<Accepted> {
    let Some(blob) = crate::extract::extract_json_blob(raw) else {
        tracing::warn!("跨域连接判定结果里找不到 JSON，本批全部不连");
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(blob) else {
        tracing::warn!("跨域连接判定结果不是合法 JSON，本批全部不连");
        return Vec::new();
    };

    let items = match &value {
        Value::Array(items) => items.as_slice(),
        Value::Object(map) => map
            .get("links")
            .or_else(|| map.get("results"))
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        _ => &[],
    };

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let Some(idx) = item.get("id").and_then(Value::as_u64).map(|i| i as usize) else {
            continue;
        };
        let Some(candidate) = chunk.get(idx) else {
            tracing::debug!(idx, "判定结果引用了不存在的候选编号");
            continue;
        };
        if !seen.insert(idx) {
            continue;
        }
        let reason: String = item
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("模型判定相关")
            .chars()
            .take(MAX_REASON_CHARS)
            .collect();
        out.push(Accepted {
            candidate: candidate.clone(),
            reason,
        });
    }
    out
}

// ══════════════════════════════════════════════════════════
//  扫描器
// ══════════════════════════════════════════════════════════

/// 一条计划中的边。两条**有向**的 `NewFact` 一起构成一条无向关联，
/// 理由见 [`Linker::plan`]。
#[derive(Debug, Clone)]
pub struct PlannedLink {
    pub forward: NewFact,
    pub backward: NewFact,
    pub reason: String,
    pub distance: f64,
}

/// 一次扫描的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkReport {
    /// 作为左侧考察了多少条事实
    pub scanned: usize,
    /// 通过预筛的候选实体对数
    pub candidates: usize,
    /// 判定器接受的对数
    pub accepted: usize,
    /// 因度数上限被丢掉的对数 —— 这个数字一旦不为 0，就说明语料里出现了
    /// 枢纽实体，值得人看一眼是不是抽取把什么东西抽成了万能主语
    pub degree_capped: usize,
    /// 实际写入的 fact id（每对两条）
    pub written: Vec<Id>,
}

/// 跨域连接扫描器。
#[derive(Debug, Clone)]
pub struct Linker {
    min_distance: f64,
    max_distance: f64,
    degree_cap: usize,
    neighbor_width: i64,
    confidence: f32,
    /// 一次扫描最多写多少对。防止某次语料异常把库灌满噪声边
    batch_cap: usize,
    /// 边的 statement 要不要进 `tsv` 与 `embedding` 索引。**默认不进。**
    ///
    /// # 这是评测逼出来的一个反直觉决定
    ///
    /// 第一版照常建了索引，结果整体 R@5 从 0.923 掉到 0.907、
    /// MRR 0.817→0.799，而且掉的是**跟跨域毫无关系的题**。逐题看下去，
    /// 机制一目了然：边的 statement 形如「「A」（域）与「B」（域）相关：理由」，
    /// 它把两个实体名和一句摘要挤在一句话里 —— 这正是 BM25 的 `ts_rank`
    /// 最偏爱的形状（词频高、文档短）。于是任何提到 A 或 B 的查询，
    /// 榜首都是这条**什么也没多说**的边，真正回答问题的那条被挤下去。
    /// `q072「v1 发布这件事上都定了什么」`最惨：top-5 里四条是边。
    ///
    /// 修法不是调权重，是想清楚这行**是什么**：它是一条**边**，不是一条
    /// 关于世界的陈述。它的用途是被**遍历**，不是被**检索**。
    /// `tsv`/`embedding` 都留 NULL，它就只会经图遍历出现 ——
    /// 也就是只在「查询确实命中了桥的某一端」时出现，正是想要的语义。
    ///
    /// 留成开关是因为这个结论依赖语料规模（69 条事实、12 行边，占比 11%），
    /// 语料上千条之后挤占效应会稀释，值得重测。
    indexed: bool,
    device_id: String,
}

impl Linker {
    #[must_use]
    pub fn new(device_id: impl Into<String>) -> Self {
        Self {
            min_distance: DEFAULT_MIN_DISTANCE,
            max_distance: DEFAULT_MAX_DISTANCE,
            degree_cap: DEFAULT_DEGREE_CAP,
            neighbor_width: DEFAULT_NEIGHBOR_WIDTH,
            confidence: DEFAULT_LINK_CONFIDENCE,
            batch_cap: 64,
            indexed: false,
            device_id: device_id.into(),
        }
    }

    /// 让边的 statement 也进 `tsv` / `embedding` 索引。见 [`Self::indexed`]
    /// 上的注释与那组数字 —— 默认关，开之前先重跑评测。
    #[must_use]
    pub fn with_indexed(mut self, indexed: bool) -> Self {
        self.indexed = indexed;
        self
    }

    /// 候选的距离带。评测扫参用。
    #[must_use]
    pub fn with_distance_band(mut self, min: f64, max: f64) -> Self {
        self.min_distance = min;
        self.max_distance = max;
        self
    }

    #[must_use]
    pub fn with_degree_cap(mut self, cap: usize) -> Self {
        self.degree_cap = cap;
        self
    }

    #[must_use]
    pub fn with_batch_cap(mut self, cap: usize) -> Self {
        self.batch_cap = cap;
        self
    }

    #[must_use]
    pub fn with_neighbor_width(mut self, width: i64) -> Self {
        self.neighbor_width = width;
        self
    }

    // ── ① 候选 ─────────────────────────────────────────────

    /// 预筛出跨域候选对。
    ///
    /// # 为什么不是全量两两比对
    ///
    /// O(n²) 在几千条事实上就跑不动了，而且绝大多数对根本不沾边。
    /// 这里对每条待考察事实发一次向量近邻（HNSW，O(log n)），再在近邻里
    /// 筛跨域 —— 总代价 O(n·k·log n)。代价是**会漏**：同域语料更密，
    /// 跨域的邻居可能被挤出前 k。这是刻意选的方向：漏一条边只是少一点
    /// 召回，多一条错边则会在图里传染。
    ///
    /// 四道过滤，每一道都在回答一个不同的问题：
    ///
    /// | 过滤 | 挡掉什么 |
    /// |---|---|
    /// | 同一 `embedding_model` | 跨模型的距离不可比（memory.md §七） |
    /// | 两侧 domain 都有且不同 | 这条边的**全部**理由就是跨域 |
    /// | 距离落在 `[min, max]` 带内 | 太远是噪声；太近是重复陈述，连了不产生信息 |
    /// | 图上尚未连通 | 已经能走到的，补一条边只是噪声（见 [`LinkCorpus::out_edges`]） |
    pub async fn propose(&self, corpus: &dyn LinkCorpus) -> Result<(Vec<Candidate>, usize)> {
        let batch = corpus.scan_batch().await?;
        let scanned = batch.len();

        let mut names: HashMap<String, String> = HashMap::new();
        let mut edges: HashMap<String, HashSet<String>> = HashMap::new();
        let mut best: HashMap<(String, String), Candidate> = HashMap::new();

        for left in &batch {
            let Some(embedding) = left.embedding.as_ref() else {
                continue;
            };
            let Some(left_domain) = left.domain.clone() else {
                continue;
            };

            let neighbors = corpus
                .neighbors(embedding, &left.embedding_model, self.neighbor_width)
                .await?;

            for (right, distance) in neighbors {
                if right.id == left.id
                    || right.embedding_model != left.embedding_model
                    || !(self.min_distance..=self.max_distance).contains(&distance)
                {
                    continue;
                }
                let Some(right_domain) = right.domain.clone() else {
                    continue;
                };
                if right_domain == left_domain || left.subject_id == right.subject_id {
                    continue;
                }
                // 两条事实共享任一实体 ⇒ 图遍历本来就通，不需要这条边
                if shares_entity(left, &right) {
                    continue;
                }
                if self
                    .already_connected(corpus, &mut edges, &left.subject_id, &right.subject_id)
                    .await?
                {
                    continue;
                }

                let key = pair_key(&left.subject_id, &right.subject_id);
                // 同一实体对可能被多组事实撞上，只留最近的那一组作为证据
                if best.get(&key).is_some_and(|c| c.distance <= distance) {
                    continue;
                }

                let left_name = resolve_name(corpus, &mut names, &left.subject_id).await?;
                let right_name = resolve_name(corpus, &mut names, &right.subject_id).await?;
                let (Some(left_name), Some(right_name)) = (left_name, right_name) else {
                    // 实体行查不到 ⇒ 这条事实的锚点是坏的，宁可跳过
                    continue;
                };

                best.insert(
                    key,
                    Candidate {
                        left: anchor(left, left_name, left_domain.clone()),
                        right: anchor(&right, right_name, right_domain),
                        distance,
                    },
                );
            }
        }

        let mut out: Vec<Candidate> = best.into_values().collect();
        // 稳定顺序：距离升序，同距离按实体对字典序。扫描要可复现，
        // HashMap 的迭代序不是
        out.sort_by(|a, b| {
            a.distance
                .total_cmp(&b.distance)
                .then_with(|| a.pair_key().cmp(&b.pair_key()))
        });
        Ok((out, scanned))
    }

    async fn already_connected(
        &self,
        corpus: &dyn LinkCorpus,
        cache: &mut HashMap<String, HashSet<String>>,
        a: &str,
        b: &str,
    ) -> Result<bool> {
        for (from, to) in [(a, b), (b, a)] {
            if !cache.contains_key(from) {
                let out = corpus.out_edges(from).await?;
                cache.insert(from.to_owned(), out.into_iter().collect());
            }
            if cache[from].contains(to) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── ③ 计划 ─────────────────────────────────────────────

    /// 把接受的候选变成待写的 `NewFact`。**纯逻辑 + 向量化，仍在事务外。**
    ///
    /// # 为什么一条关联要写两行
    ///
    /// `recall_graph` 的实体扩展是**有向**的：
    /// `SELECT COALESCE(f.object_entity_id, …) FROM reach r JOIN active_facts f
    /// ON f.subject_id = r.entity_id`。也就是说从主语能走到宾语，反过来走不动
    /// （反向只能收集到那条边本身，收集不到对面实体的其它事实）。
    ///
    /// 而「相关」这个关系本身是无向的，演示场景两个方向都要成立：
    /// 在 coding 语境问 office 的事，以及反过来。要么改 `recall_graph` 让它
    /// 双向扩展（那是存储层，且会让**所有**遍历的宽度翻倍），要么把无向关系
    /// 物化成两条有向边。选后者：代价局限在这一个谓词上，而且正是它让
    /// 「无需改检索代码」这句话真的成立。
    ///
    /// 代价是诚实的：每对关联占两行、两份注入预算。degree_cap 因此按
    /// **对**计而不是按行计。
    pub async fn plan(
        &self,
        embedder: &dyn Embedder,
        accepted: Vec<Accepted>,
        existing_degree: &HashMap<String, usize>,
    ) -> Result<(Vec<PlannedLink>, usize)> {
        let mut degree: HashMap<String, usize> = existing_degree.clone();
        let mut kept: Vec<Accepted> = Vec::new();
        let mut capped = 0usize;

        // 已按距离升序：度数上限撞上时留下的是更近的那些
        for a in accepted {
            if kept.len() >= self.batch_cap {
                capped += 1;
                continue;
            }
            let l = a.candidate.left.entity_id.clone();
            let r = a.candidate.right.entity_id.clone();
            let over = degree.get(&l).copied().unwrap_or(0) >= self.degree_cap
                || degree.get(&r).copied().unwrap_or(0) >= self.degree_cap;
            if over {
                capped += 1;
                continue;
            }
            *degree.entry(l).or_insert(0) += 1;
            *degree.entry(r).or_insert(0) += 1;
            kept.push(a);
        }

        if kept.is_empty() {
            return Ok((Vec::new(), capped));
        }

        let statements: Vec<String> = kept
            .iter()
            .flat_map(|a| {
                [
                    link_statement(&a.candidate.left, &a.candidate.right, &a.reason),
                    link_statement(&a.candidate.right, &a.candidate.left, &a.reason),
                ]
            })
            .collect();

        // 不建索引时连算都不用算 —— 向量白算一遍是纯浪费，而且这是每晚
        // 都要跑的东西。要算的话也必须在这里算完：事务里跨网络是死罪
        let mut vectors: Vec<Option<Vec<f32>>> = if self.indexed {
            let v = embedder.embed(&statements).await?;
            if v.len() != statements.len() {
                return Err(CortexError::Memory(format!(
                    "embedder 返回 {} 条向量，与 {} 条 statement 不匹配",
                    v.len(),
                    statements.len()
                )));
            }
            v.into_iter().map(Some).collect()
        } else {
            vec![None; statements.len()]
        };

        let model = embedder.model_id().to_owned();
        let mut out = Vec::with_capacity(kept.len());
        let mut texts = statements.into_iter();
        let mut vecs = vectors.drain(..);

        for a in kept {
            let c = &a.candidate;
            let forward = self.new_fact(
                &c.left,
                &c.right,
                texts.next().expect("statement 与候选一一对应"),
                vecs.next().expect("向量槽与 statement 一一对应"),
                &model,
            )?;
            let backward = self.new_fact(
                &c.right,
                &c.left,
                texts.next().expect("statement 与候选一一对应"),
                vecs.next().expect("向量槽与 statement 一一对应"),
                &model,
            )?;
            out.push(PlannedLink {
                forward,
                backward,
                reason: a.reason,
                distance: c.distance,
            });
        }
        Ok((out, capped))
    }

    fn new_fact(
        &self,
        from: &Anchor,
        to: &Anchor,
        statement: String,
        vector: Option<Vec<f32>>,
        model: &str,
    ) -> Result<NewFact> {
        let parse = |s: &str, what: &str| -> Result<Id> {
            s.parse::<Id>()
                .map_err(|e| CortexError::Store(format!("{what} {s} 不是合法 ULID：{e}")))
        };
        Ok(NewFact {
            id: Id::new(),
            subject_id: parse(&from.entity_id, "实体 id")?,
            predicate: RELATES_TO.to_owned(),
            object_text: None,
            object_entity_id: Some(parse(&to.entity_id, "实体 id")?),
            // 见 `Linker::indexed`：默认两个索引都不进 —— 这行是一条**边**，
            // 用途是被遍历而不是被检索。`tsv` 留 NULL 是**决定**不是遗漏，
            // 与「永不异步补写 tsv」那条纪律不冲突（那条禁的是事后 UPDATE）
            tsv_source: self
                .indexed
                .then(|| tokenize::to_tsvector_input(&statement)),
            statement,
            embedding: vector.map(Vector::from),
            embedding_model: model.to_owned(),
            // 刻意留空。这条边不属于任何一个领域 —— 给它一个 domain 会让
            // `apply_domain_boost` 偏袒其中一个方向，而它存在的全部理由
            // 恰恰是「两个领域都算数」
            domain: None,
            confidence: self.confidence,
            // 事件时间未知：没有任何一刻「这两者开始相关」。留 NULL 才诚实，
            // 填 now() 等于用系统时间冒充事件时间，双时间轴就白设计了
            valid_at: None,
            // 出处指向**本方向左侧**那条事实的原始对话。它并没有陈述这条关联，
            // 但它是唯一真实存在的锚点，而且能让 redact 级联把这条边一起带走。
            source_episode_id: parse(&from.source_episode_id, "episode id")?,
            // 这条边是模型从两条**已有事实**推出来的，不是从对话里读出来的
            source: FactSource::Derived,
            // 两端的源事实都记下来。
            //
            // 这补上了 `source_episode_id` 只能记左侧留下的那个洞：在此之前
            // 只 redact 右侧时，本行会带着右侧实体名活下来，而且没有任何
            // 查询能找到它。现在顺着 derivations 反查 (fact, 右侧 id) 就能
            // 定位到这条边（源事实 → 其 source_episode_id 是第二跳）。
            //
            // ⚠️ 仍未解的是「信任洗白」：这条边固定 tier 2，不继承两端源事实
            // 的档位。补法的前置正是这两行边（见 migration 20260807000006）
            derived_from: vec![
                ProvenanceRef::fact(parse(&from.fact_id, "事实 id")?),
                ProvenanceRef::fact(parse(&to.fact_id, "事实 id")?),
            ],
            extracted_by: format!("crosslink/{RELATES_TO}"),
            device_id: self.device_id.clone(),
        })
    }

    // ── ④ 落库 ─────────────────────────────────────────────

    /// 唯一进事务的一步。事务里只有 INSERT，没有任何读、没有任何网络调用。
    pub async fn write(&self, store: &Store, planned: Vec<PlannedLink>) -> Result<Vec<Id>> {
        if planned.is_empty() {
            return Ok(Vec::new());
        }

        // 在途任务防护，与 `extract::write_candidates` 同款：扫描跑起来之后
        // 可能有人执行了 redact，不查这一句就会把已抹除内容派生的边写回去
        let mut rows: Vec<&NewFact> = Vec::with_capacity(planned.len() * 2);
        for p in &planned {
            for f in [&p.forward, &p.backward] {
                if store
                    .is_redacted(RedactionTarget::Episode, &f.source_episode_id.to_string())
                    .await?
                {
                    tracing::warn!(episode = %f.source_episode_id, "出处已被抹除，放弃这条跨域连接");
                    continue;
                }
                rows.push(f);
            }
        }
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let ids: Vec<Id> = rows.iter().map(|f| f.id).collect();
        store
            .write_txn(async |tx| {
                for f in &rows {
                    tx.insert_fact(f).await?;
                }
                Ok(())
            })
            .await?;
        Ok(ids)
    }

    /// 一条龙：候选 → 判定 → 计划 → 落库。
    pub async fn run(
        &self,
        store: &Store,
        corpus: &dyn LinkCorpus,
        judge: &dyn LinkJudge,
        embedder: &dyn Embedder,
    ) -> Result<LinkReport> {
        let (candidates, scanned) = self.propose(corpus).await?;
        let n_candidates = candidates.len();
        if candidates.is_empty() {
            return Ok(LinkReport {
                scanned,
                ..LinkReport::default()
            });
        }

        let accepted = judge.judge(&candidates).await?;
        let n_accepted = accepted.len();

        // 已有的度数：接着上一次扫描往上加，否则每晚都能再加满一轮
        let mut existing: HashMap<String, usize> = HashMap::new();
        for c in &candidates {
            for id in [&c.left.entity_id, &c.right.entity_id] {
                if existing.contains_key(id) {
                    continue;
                }
                let n = corpus
                    .out_edges(id)
                    .await?
                    .len()
                    .min(self.degree_cap.saturating_mul(4));
                existing.insert(id.clone(), n);
            }
        }

        let (planned, degree_capped) = self.plan(embedder, accepted, &existing).await?;
        let written = self.write(store, planned).await?;

        Ok(LinkReport {
            scanned,
            candidates: n_candidates,
            accepted: n_accepted,
            degree_capped,
            written,
        })
    }
}

fn anchor(f: &Fact, entity_name: String, domain: String) -> Anchor {
    Anchor {
        fact_id: f.id.clone(),
        entity_id: f.subject_id.clone(),
        entity_name,
        statement: f.statement.clone(),
        domain,
        source_episode_id: f.source_episode_id.clone(),
    }
}

fn shares_entity(a: &Fact, b: &Fact) -> bool {
    let ids_a: HashSet<&str> = [Some(a.subject_id.as_str()), a.object_entity_id.as_deref()]
        .into_iter()
        .flatten()
        .collect();
    [Some(b.subject_id.as_str()), b.object_entity_id.as_deref()]
        .into_iter()
        .flatten()
        .any(|id| ids_a.contains(id))
}

async fn resolve_name(
    corpus: &dyn LinkCorpus,
    cache: &mut HashMap<String, String>,
    id: &str,
) -> Result<Option<String>> {
    if let Some(n) = cache.get(id) {
        return Ok(Some(n.clone()));
    }
    let name = corpus.entity_name(id).await?;
    if let Some(n) = &name {
        cache.insert(id.to_owned(), n.clone());
    }
    Ok(name)
}

/// 边的自然语言表述。
///
/// 刻意**只用实体名**，不复述任何一侧的 `statement`：复述会把两个领域的
/// 原文各抄一份进注入块，既撑爆预算，也让 redact 的级联范围凭空变大
/// （抹掉一侧的原文，抄件还在另一条边上）。
fn link_statement(from: &Anchor, to: &Anchor, reason: &str) -> String {
    format!(
        "「{}」（{}）与「{}」（{}）相关：{}",
        from.entity_name, from.domain, to.entity_name, to.domain, reason
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::HashEmbedder;
    use chrono::Utc;
    use std::sync::Mutex;

    /// 造一个形状合法的 ULID。`Linker::plan` 要把实体 id 解析成 [`Id`]，
    /// 拿 `"E1"` 当 id 只会测出「解析器认得出坏 id」这种废话。
    fn eid(n: usize) -> String {
        let s = format!("01JAAAAAAAAAAAAAAAAAA{n:05}");
        assert_eq!(s.len(), 26, "ULID 必须是 26 位：{s}");
        s
    }

    /// 把 `"F1"` 这种便于阅读的短标签补成形状合法的 ULID。
    ///
    /// 与 [`eid`] 同理，只是轮到 fact id 了：跨域边现在要把两端的**源事实**
    /// 记进 `derived_from`（否则只 redact 右侧时这条边会带着右侧实体名活下来），
    /// 于是 `Linker::plan` 也要解析 fact id。
    fn fid(label: &str) -> String {
        // Crockford base32 不含 I / L / O / U，短标签里出现就得换掉
        let safe: String = label
            .to_ascii_uppercase()
            .chars()
            .map(|c| match c {
                'I' | 'L' => '1',
                'O' => '0',
                'U' => 'V',
                c if c.is_ascii_alphanumeric() => c,
                _ => '0',
            })
            .collect();
        let s = format!("01JFFFFFFFFFFFFFFFFFF{safe:0>5}");
        assert_eq!(s.len(), 26, "ULID 必须是 26 位：{s}");
        s
    }

    fn fact(id: &str, subject: &str, object: Option<&str>, domain: Option<&str>) -> Fact {
        Fact {
            id: fid(id),
            subject_id: subject.to_owned(),
            predicate: "uses".to_owned(),
            object_text: Some("x".to_owned()),
            object_entity_id: object.map(str::to_owned),
            statement: format!("{subject} 的事实 {id}"),
            embedding: Some(Vector::from(vec![0.1_f32; 4])),
            embedding_model: "hash-stub-v1".to_owned(),
            domain: domain.map(str::to_owned),
            confidence: 0.9,
            valid_at: None,
            source_episode_id: eid(999),
            source_channel: cortex_store::SourceChannel::Conversation,
            trust_tier: Some(2),
            extracted_by: "test".to_owned(),
            device_id: "test".to_owned(),
            created_at: Utc::now(),
        }
    }

    /// 可编排的假语料。`neighbors` 对**任何**查询都返回同一份列表 ——
    /// 这里测的是过滤逻辑，不是近邻算法。
    struct FakeCorpus {
        batch: Vec<Fact>,
        neighbors: Vec<(Fact, f64)>,
        edges: HashMap<String, Vec<String>>,
        out_edge_calls: Mutex<usize>,
    }

    impl FakeCorpus {
        fn new(batch: Vec<Fact>, neighbors: Vec<(Fact, f64)>) -> Self {
            Self {
                batch,
                neighbors,
                edges: HashMap::new(),
                out_edge_calls: Mutex::new(0),
            }
        }

        fn with_edge(mut self, from: &str, to: &str) -> Self {
            self.edges
                .entry(from.to_owned())
                .or_default()
                .push(to.to_owned());
            self
        }
    }

    #[async_trait::async_trait]
    impl LinkCorpus for FakeCorpus {
        async fn scan_batch(&self) -> Result<Vec<Fact>> {
            Ok(self.batch.clone())
        }

        async fn neighbors(
            &self,
            _embedding: &Vector,
            _model: &str,
            _limit: i64,
        ) -> Result<Vec<(Fact, f64)>> {
            Ok(self.neighbors.clone())
        }

        async fn out_edges(&self, subject_id: &str) -> Result<Vec<String>> {
            *self.out_edge_calls.lock().expect("锁未中毒") += 1;
            Ok(self.edges.get(subject_id).cloned().unwrap_or_default())
        }

        async fn entity_name(&self, entity_id: &str) -> Result<Option<String>> {
            Ok(Some(format!("实体{entity_id}")))
        }
    }

    fn linker() -> Linker {
        Linker::new("test")
    }

    // ── 候选预筛 ───────────────────────────────────────────

    #[tokio::test]
    async fn proposes_a_cross_domain_pair() {
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]);
        let (out, scanned) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert_eq!(scanned, 1);
        assert_eq!(out.len(), 1, "一对跨域近邻应当成为候选，实际 {}", out.len());
        assert_eq!(out[0].pair_key(), ("E1".into(), "E2".into()));
    }

    #[tokio::test]
    async fn same_domain_is_not_a_candidate() {
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("coding"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]);
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert!(
            out.is_empty(),
            "同域对不该连 —— 这条边的全部理由就是跨域，实际产出 {} 对",
            out.len()
        );
    }

    #[tokio::test]
    async fn near_duplicates_are_not_candidates() {
        // 距离 0.02：两条说的其实是同一件事。连起来一个 bit 都不产生
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.02)]);
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert!(
            out.is_empty(),
            "近乎重复的一对不该连，实际产出 {} 对",
            out.len()
        );
    }

    #[tokio::test]
    async fn distant_pairs_are_not_candidates() {
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.80)]);
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert!(out.is_empty(), "距离超出带外不该连，实际 {} 对", out.len());
    }

    #[tokio::test]
    async fn facts_sharing_an_entity_need_no_new_edge() {
        // 左侧的宾语就是右侧的主语 —— 图遍历本来就走得通
        let left = fact("F1", "E1", Some("E2"), Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]);
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert!(
            out.is_empty(),
            "已共享实体的两条事实再补一条边只是噪声，实际 {} 对",
            out.len()
        );
    }

    #[tokio::test]
    async fn existing_edge_makes_the_scan_idempotent() {
        // 这条是 append-only + 不能加唯一约束下唯一的去重手段：
        // 第二次扫描时上一晚写的边已经在 out_edges 里
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]).with_edge("E1", "E2");
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert!(
            out.is_empty(),
            "同一对不该被写第二遍，实际 {} 对",
            out.len()
        );
    }

    #[tokio::test]
    async fn existing_edge_is_detected_in_either_direction() {
        // 边是无向的，只查一个方向会让「反向已存在」的对被重复写
        let left = fact("F1", "E1", None, Some("coding"));
        let right = fact("F2", "E2", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(right, 0.30)]).with_edge("E2", "E1");
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert!(
            out.is_empty(),
            "反向已连通也算已连通，实际 {} 对",
            out.len()
        );
    }

    #[tokio::test]
    async fn same_entity_pair_is_proposed_once_with_the_closest_evidence() {
        // 同一对实体常被好几组事实同时撞上；写成几条边只是同一件事重复占预算
        let left = fact("F1", "E1", None, Some("coding"));
        let near = fact("F3", "E2", None, Some("office"));
        let far = fact("F2", "E2", None, Some("office"));
        // 近的先来、远的后到：没有「留最近的那组」这道判断时，后到的会覆盖先到的
        let corpus = FakeCorpus::new(vec![left], vec![(near, 0.20), (far, 0.40)]);
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert_eq!(out.len(), 1, "同一实体对只该出一次，实际 {} 对", out.len());
        assert!(
            (out[0].distance - 0.20).abs() < 1e-9,
            "应留下更近的那组证据，实际距离 {}",
            out[0].distance
        );
        assert_eq!(out[0].right.fact_id, fid("F3"));
    }

    #[tokio::test]
    async fn out_edges_are_cached_across_the_scan() {
        // 每对候选都发两次查询的话，扫描的数据库往返会随候选数平方增长
        let left = fact("F1", "E1", None, Some("coding"));
        let r1 = fact("F2", "E2", None, Some("office"));
        let r2 = fact("F3", "E3", None, Some("office"));
        let corpus = FakeCorpus::new(vec![left], vec![(r1, 0.30), (r2, 0.35)]);
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert_eq!(out.len(), 2);
        let calls = *corpus.out_edge_calls.lock().expect("锁未中毒");
        assert_eq!(calls, 3, "E1/E2/E3 各查一次即可，实际查了 {calls} 次");
    }

    #[tokio::test]
    async fn candidates_are_ordered_deterministically() {
        // HashMap 的迭代序不稳定；扫描必须可复现，否则度数上限截掉谁全看运气
        // 左侧的 id 刻意不叫 F*：它与近邻同名时会被「不跟自己连」那条挡掉，
        // 于是少一条候选，看起来像排序错了
        let left = fact("L0", "E1", None, Some("coding"));
        // 十条而不是两条：`HashMap` 的迭代序随 hash 种子逐进程变化，
        // 两条时它有一半概率碰巧就是对的，测试会变成偶尔才红的哑弹
        let neighbors: Vec<(Fact, f64)> = (0..10)
            .map(|i| {
                (
                    fact(&format!("F{i}"), &format!("E{i:02}"), None, Some("office")),
                    0.40 - f64::from(i) * 0.02,
                )
            })
            .collect();
        let want: Vec<String> = (0..10).rev().map(|i| fid(&format!("F{i}"))).collect();
        let corpus = FakeCorpus::new(vec![left], neighbors);
        let (out, _) = linker().propose(&corpus).await.expect("预筛应当成功");
        assert_eq!(
            out.iter()
                .map(|c| c.right.fact_id.clone())
                .collect::<Vec<_>>(),
            want,
            "候选必须按距离升序稳定排列 —— 否则度数上限截掉谁全看 hash 种子"
        );
    }

    // ── 度数上限 ───────────────────────────────────────────

    fn accepted(left: usize, right: usize, distance: f64) -> Accepted {
        let l = fact("FL", &eid(left), None, Some("coding"));
        let r = fact("FR", &eid(right), None, Some("office"));
        Accepted {
            candidate: Candidate {
                left: anchor(&l, format!("实体{left}"), "coding".into()),
                right: anchor(&r, format!("实体{right}"), "office".into()),
                distance,
            },
            reason: "测试".into(),
        }
    }

    #[tokio::test]
    async fn degree_cap_stops_hub_entities() {
        // 枢纽实体是噪声在图里传染的主要途径：2 跳内它能带出 O(d²) 片子图
        let embedder = HashEmbedder::new();
        let links: Vec<Accepted> = (0..6).map(|i| accepted(1, 100 + i, 0.3)).collect();
        let (planned, capped) = linker()
            .with_degree_cap(2)
            .plan(&embedder, links, &HashMap::new())
            .await
            .expect("计划应当成功");
        assert_eq!(planned.len(), 2, "度数上限 2 只该放行两对");
        assert_eq!(capped, 4, "其余四对应当被记为 degree_capped");
    }

    #[tokio::test]
    async fn existing_degree_carries_over_between_scans() {
        // 不带上历史度数的话，每晚都能把同一个实体再加满一轮上限
        let embedder = HashEmbedder::new();
        let mut existing = HashMap::new();
        existing.insert(eid(1), 4usize);
        let (planned, capped) = linker()
            .plan(&embedder, vec![accepted(1, 9, 0.3)], &existing)
            .await
            .expect("计划应当成功");
        assert!(planned.is_empty(), "历史度数已满就不该再加");
        assert_eq!(capped, 1);
    }

    #[tokio::test]
    async fn batch_cap_bounds_a_single_scan() {
        let embedder = HashEmbedder::new();
        let links: Vec<Accepted> = (0..5).map(|i| accepted(200 + i, 300 + i, 0.3)).collect();
        let (planned, capped) = linker()
            .with_batch_cap(2)
            .plan(&embedder, links, &HashMap::new())
            .await
            .expect("计划应当成功");
        assert_eq!(planned.len(), 2, "单次扫描的总量必须有上界");
        assert_eq!(capped, 3);
    }

    // ── 落库计划 ───────────────────────────────────────────

    #[tokio::test]
    async fn plans_both_directions() {
        let embedder = HashEmbedder::new();
        let (planned, _) = linker()
            .plan(&embedder, vec![accepted(1, 2, 0.3)], &HashMap::new())
            .await
            .expect("计划应当成功");
        let p = &planned[0];
        assert_eq!(p.forward.subject_id.to_string(), eid(1));
        assert_eq!(
            p.backward.subject_id.to_string(),
            eid(2),
            "反向边缺失时，图遍历只能从一侧走到另一侧 —— 演示场景有一半是废的"
        );
        assert_eq!(
            p.forward.object_entity_id.map(|i| i.to_string()),
            Some(eid(2))
        );
        assert_ne!(p.forward.id, p.backward.id);
    }

    #[tokio::test]
    async fn planned_facts_carry_no_domain_and_low_confidence() {
        let embedder = HashEmbedder::new();
        let (planned, _) = linker()
            .plan(&embedder, vec![accepted(1, 2, 0.3)], &HashMap::new())
            .await
            .expect("计划应当成功");
        let f = &planned[0].forward;
        assert_eq!(
            f.domain, None,
            "给这条边任何一个 domain，域加权都会偏袒其中一个方向"
        );
        assert!(
            f.confidence < 0.5,
            "推断出来的边必须明显低于抽取器产物，实际 {}",
            f.confidence
        );
        assert_eq!(f.predicate, RELATES_TO);
        assert!(f.valid_at.is_none(), "没有任何一刻「这两者开始相关」");
    }

    #[tokio::test]
    async fn links_are_traversable_but_not_searchable_by_default() {
        // 这条挡的是「有人顺手把 tsv/embedding 补上」：边的 statement 把
        // 两个实体名和一句摘要挤在一句话里，正是 BM25 的 ts_rank 最偏爱的
        // 形状，建了索引它就会在任何提到两端的查询上霸榜。实测代价：
        // 整体 R@5 0.923→0.907、MRR 0.817→0.799，掉的还全是跨域以外的题
        let embedder = HashEmbedder::new();
        let (planned, _) = linker()
            .plan(&embedder, vec![accepted(1, 2, 0.3)], &HashMap::new())
            .await
            .expect("计划应当成功");
        let f = &planned[0].forward;
        assert!(
            f.tsv_source.is_none() && f.embedding.is_none(),
            "边默认不进 BM25 与向量索引 —— 它的用途是被遍历，不是被检索"
        );
        assert!(
            f.object_entity_id.is_some(),
            "但它必须仍然是图上的一条边，否则整件事就没有意义了"
        );
    }

    #[tokio::test]
    async fn indexed_mode_fills_both_indexes() {
        let embedder = HashEmbedder::new();
        let (planned, _) = linker()
            .with_indexed(true)
            .plan(&embedder, vec![accepted(1, 2, 0.3)], &HashMap::new())
            .await
            .expect("计划应当成功");
        let f = &planned[0].forward;
        assert!(
            f.tsv_source.as_ref().is_some_and(|s| !s.trim().is_empty()),
            "开了索引就必须与主行同事务写入 tsv，不能留给异步补写"
        );
        assert!(f.embedding.is_some());
    }

    #[test]
    fn statement_names_both_sides_but_quotes_neither() {
        let l = fact("F1", "E1", None, Some("coding"));
        let r = fact("F2", "E2", None, Some("office"));
        let s = link_statement(
            &anchor(&l, "融合模块".into(), "coding".into()),
            &anchor(&r, "评测周会".into(), "office".into()),
            "周会决定了融合参数",
        );
        assert!(s.contains("融合模块") && s.contains("评测周会"));
        assert!(
            !s.contains("的事实 F1") && !s.contains("的事实 F2"),
            "复述两侧原文会把两个领域的正文各抄一份进注入块：{s}"
        );
    }

    // ── 判定 ───────────────────────────────────────────────

    fn two_candidates() -> Vec<Candidate> {
        vec![accepted(1, 2, 0.3).candidate, accepted(3, 4, 0.4).candidate]
    }

    #[test]
    fn judgement_parsing_picks_only_the_accepted_ones() {
        let out = parse_judgements(
            r#"{"links":[{"id":1,"reason":"预算限制选型"}]}"#,
            &two_candidates(),
        );
        assert_eq!(out.len(), 1, "模型只接受一条时不该连另一条");
        assert_eq!(out[0].candidate.left.entity_id, eid(3));
        assert_eq!(out[0].reason, "预算限制选型");
    }

    #[test]
    fn judgement_parsing_ignores_out_of_range_ids() {
        let out = parse_judgements(
            r#"{"links":[{"id":99,"reason":"x"},{"id":0,"reason":"y"}]}"#,
            &two_candidates(),
        );
        assert_eq!(out.len(), 1, "越界编号只该丢那一条，实际 {} 条", out.len());
    }

    #[test]
    fn judgement_parsing_dedups_repeated_ids() {
        let out = parse_judgements(
            r#"{"links":[{"id":0,"reason":"a"},{"id":0,"reason":"b"}]}"#,
            &two_candidates(),
        );
        assert_eq!(out.len(), 1, "同一对被判两次不该写两条边");
    }

    #[test]
    fn judgement_parsing_truncates_long_reasons() {
        let long = "很".repeat(200);
        let raw = format!(r#"{{"links":[{{"id":0,"reason":"{long}"}}]}}"#);
        let out = parse_judgements(&raw, &two_candidates());
        assert_eq!(
            out[0].reason.chars().count(),
            MAX_REASON_CHARS,
            "reason 直接进 prompt，长度必须有硬上界"
        );
    }

    #[test]
    fn unparseable_judgement_links_nothing_instead_of_failing() {
        // 判定失败的正确行为是「这批不连」：少一条边只是少一点召回，
        // 而让整个夜间扫描红掉要到第二天才有人发现
        let out = parse_judgements("模型今天不想输出 JSON", &two_candidates());
        assert!(out.is_empty());
    }

    #[test]
    fn judge_prompt_states_the_only_criterion() {
        assert!(
            JUDGE_SYSTEM.contains("帮忙") && JUDGE_SYSTEM.contains("宁可漏连"),
            "判据与偏好方向必须写进 prompt，否则模型会把「话题相邻」也连上"
        );
    }

    #[tokio::test]
    async fn accept_all_is_the_prefilter_control_group() {
        let c = two_candidates();
        let out = AcceptAll.judge(&c).await.expect("对照组不会失败");
        assert_eq!(out.len(), c.len());
    }
}

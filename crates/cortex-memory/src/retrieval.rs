//! 检索器 —— 把四路召回、RRF 融合、预算截断串成一条。
//!
//! 分工：
//! - `cortex-store::recall_*` 负责各路的**捞取**，不做排序策略
//! - [`fusion`](crate::fusion) 负责**融合**
//! - [`injection`](crate::injection) 负责**出库格式**
//! - 本模块只负责把它们编排起来，并决定各路的召回宽度
//!
//! # 哪几路，以及为什么
//!
//! 每一路都有明确的盲区，且盲区互不重叠 —— 这是多路的全部理由。
//! 「独占题数」（只有这一路捞到 gold 的题数）是唯一能证伪它的数字。
//!
//! | 路 | 强 | 盲 | 默认 |
//! |---|---|---|---|
//! | BM25 | 专有名词、精确措辞 | 换个说法就召不回 | 开 |
//! | 向量 | 语义相近 | 极短的专名查询 | 开 |
//! | 图遍历 | 关系推理（「他负责的那个项目用什么」） | 与查询词面无关时不触发 | 开 |
//! | episodes | 出处追问（用户复述的是**原话**） | 兄弟事实成块挤占前排 | **关**，见 [`Retriever::episode_channel`] |
//! | 近因 | —— | 与查询完全无关 | **关**，见 [`RecencyMode`] |
//!
//! 后两条不是被砍掉的（实现与开关都在），是**默认不启用**：评测里它们
//! 各自都让排序变差，唯一变好的指标（误召）经查是稀释而非判别。
//!
//! 融合之后还有两道与「哪几路」无关的闸门，两道都不可省：
//!
//! - [`fusion::apply_peak_bonus`] —— 修 RRF「弱共识压过强命中」的代价
//! - [`BGE_M3_SEMANTIC_FLOOR`] —— 修 RRF 分不含绝对相关性的代价

use chrono::{DateTime, Utc};
use cortex_core::{CortexError, Result};
use cortex_store::{Fact, Store};

use crate::embed::Embedder;
use crate::fusion::{self, Channel, Fused};
use crate::injection::{Budget, MemoryItem};
use crate::tokenize;

/// 弃权阈值的定值。含义与取值依据见 [`Retriever::abstain_below`]。
///
/// 公开出来只为一个用途：报告与回归门要能记下「这次跑的是哪个阈值」。
pub const DEFAULT_ABSTAIN_BELOW: f64 = 0.030;

/// 语义地板：全库最近的一条事实的**余弦距离**超过它就整体弃权。
///
/// # 这个数字只在 bge-m3 的向量空间里成立
///
/// 它是评测扫出来的（`evals/sweep.py floor:0.40,0.45,0.50,0.55,0.60`）：
///
/// | 地板 | R@5 | MRR | 误召@5 | 「应召不到」注入条数 |
/// |---|---|---|---|---|
/// | 不设 | 0.943 | 0.833 | 0.393 | 32.9 |
/// | 0.55 | 0.923 | 0.817 | 0.393 | 32.9 |
/// | **0.50** | **0.923** | **0.812** | **0.286** | **22.4** |
/// | 0.45 | 0.851 | 0.766 | 0.143 | 6.4 |
/// | 0.40 | 0.675 | 0.617 | 0.107 | 3.5 |
///
/// 0.55 白白付掉了 0.50 的召回代价却一点噪声都没换到，严格劣于 0.50；
/// 0.45 起步就是丢七道题换四道，比例倒挂。0.50 用 0.020 的 R@5
/// （两道题）换掉了 0.107 的误召（三道题）—— 按本模块自己的取舍
/// （「宁可不给记忆，也不要给错记忆」）这笔买卖成立。
///
/// 代价是具体的、已知的两道题：`赵敏是谁`、`jieba-rs`。
/// 都是**极短的专名查询** —— 两三个字的 query 向量本来就发散，
/// 最近邻距离必然偏大，哪怕 BM25 精准命中。它们的结果是**弃权**
/// （一条都不给）而不是给错，正是设计上偏好的那种失败。
///
/// **换 embedding 模型必须重扫这条曲线。** 距离由模型定标，
/// 换个空间这个数字什么也不是 —— 所以 [`Retriever::new`] 只在
/// 真实语义后端下才装上它。
pub const BGE_M3_SEMANTIC_FLOOR: f64 = 0.50;

/// 该不该给这个向量空间装语义地板。
///
/// 这是一张**白名单**，方向是刻意的。两种错法的代价不对称：
///
/// - 该装不装 → 噪声多进来一些，看得见、可调
/// - 不该装却装了 → 召回**静默清零**，检索照常返回、只是永远返回空
///
/// 散列桩（`hash-stub-v1`）正是后者：它的「距离」只反映字符 bigram
/// 的重合度，与相关性无关，套上 0.50 会让离线开发与无网 CI 全线归零。
///
/// # `api:` 前缀的注意事项
///
/// 走远端 `/v1/embeddings` 的后端写进库的是 `api:<模型名>`。它是真实语义
/// 空间，所以装地板；**但 0.50 这个数是在 bge-m3-int8 上扫出来的**，
/// 换成别的模型（text-embedding-v3、jina-v3……）时它什么也不是 ——
/// 距离由模型定标。换模型要重扫 `evals/` 里那条曲线，
/// 或者用 [`Retriever::with_semantic_floor`] 显式给一个。
fn default_semantic_floor(model_id: &str) -> Option<f64> {
    let semantic = model_id.starts_with("fastembed:") || model_id.starts_with("api:");
    semantic.then_some(BGE_M3_SEMANTIC_FLOOR)
}

/// 图遍历取多少个种子实体。
///
/// 公开出来是因为评测的逐路诊断要复刻同一份编排 —— 原来它是私有硬编码，
/// 评测侧只能抄一个 12 过去，改了这边那边不会跟着变，逐路数字就悄悄失真。
pub const GRAPH_SEED_LIMIT: i64 = 12;

/// 把一串已按时间倒序的事实包装成融合结果。
///
/// 分数用 `1/(i+1)` 而非 RRF 的 `1/(k+i+1)`：这条路径上没有第二路可融合，
/// 用陡峭的核可以让「弃权阈值」在全貌回放上仍有区分度 ——
/// 否则整份快照的分都挤在 0.0164 附近，要么全进要么全不进。
fn chronological(facts: Vec<Fact>) -> Vec<Fused<Fact>> {
    facts
        .into_iter()
        .enumerate()
        .map(|(i, item)| Fused {
            item,
            score: 1.0 / (i as f64 + 1.0),
            hits: vec![(Channel::Recency, i)],
        })
        .collect()
}

/// 图遍历这一路里，被 `relates_to` 边连上的实体。
///
/// # 豁免范围：为什么是「边的两个端点」而不是「整条通道」
///
/// [`crate::crosslink`] 架的桥要能真的起作用，域惩罚就得放它一马
/// （roadmap 原话：「融合时该通道需豁免域惩罚」）。但「豁免整条图遍历通道」
/// 太宽：图遍历本来就会带出一堆与桥无关的跨域事实（比如「张伟」这种
/// 天然横跨两域的实体），把它们一并免罚等于借着这个改动顺手削掉了
/// 域加权的一半作用域，而且没有任何评测依据。
///
/// 这里取的是最窄的那个能自洽的范围：**只有当这次召回里确实出现了一条
/// `relates_to` 边时，才豁免它连着的那两个实体所锚定的事实。** 桥没被走到
/// （查询压根没命中桥的任何一端）时，这个集合是空的，行为与改动前逐位相同。
///
/// 两个端点一起豁免而不是只豁免「远端」，是因为「哪端是远端」取决于当前
/// 领域，而近端本来就在当前领域、免罚对它是恒等操作
/// （见 `fusion::exemption_is_not_a_double_boost`）。不用算远端，也就不用
/// 把种子实体一路传下来。
///
/// # 已知的粗糙处
///
/// 端点实体的**全部**事实都被免罚，不只是「因为这座桥才被带出来的那些」。
/// 精确做法要重放一遍遍历路径，而 `recall_graph` 返回的是截断过的结果集，
/// 重放出来的可达性本身就是错的。所以这里选了一个会略微过宽、但爆炸半径
/// 不超过图遍历自身的近似 —— 度数上限（`crosslink::DEFAULT_DEGREE_CAP`）
/// 才是真正把这个半径钉住的东西。
fn bridged_entities(channels: &[(Channel, Vec<Fact>)]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for (channel, facts) in channels {
        if *channel != Channel::Graph {
            continue;
        }
        for f in facts {
            if f.predicate != crate::crosslink::RELATES_TO {
                continue;
            }
            out.insert(f.subject_id.clone());
            if let Some(obj) = &f.object_entity_id {
                out.insert(obj.clone());
            }
        }
    }
    out
}

/// 把当前空间与旧空间的向量召回合成**同一条** [`Channel::Vector`]。
///
/// # 为什么按名次交替，而不是当前空间全排前面
///
/// 回填期间旧空间里的事实**更多**（存量全在那边），当前空间只有新写的那些。
/// 让当前空间整体排前面，等于给刚写的几条事实每人一个前排名次，
/// 而把全部历史记忆压到 41 名之后 —— RRF 的核是 `1/(k+rank)`，
/// 那基本等于没召回。
///
/// 交替取（新1、旧1、新2、旧2……）让两个空间在名次上平权。
/// 这不是「更准」，是**在两边距离不可比的前提下唯一不引入偏置的合法做法**。
///
/// # 为什么不按距离归并
///
/// 两个模型的余弦距离不在同一个尺度上。看起来能比（都是 0~2 的实数），
/// 排出来的序却没有意义 —— 而且它不会报错，只会让检索质量莫名其妙地差。
///
/// 没挂 legacy 时 `legacy` 是空的，函数退化成「原样返回当前空间」。
fn merge_vector_spaces(current: Vec<(Fact, f64)>, legacy: Vec<(Fact, f64)>) -> Vec<Fact> {
    if legacy.is_empty() {
        return current.into_iter().map(|(f, _)| f).collect();
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(current.len() + legacy.len());
    let mut a = current.into_iter();
    let mut b = legacy.into_iter();
    loop {
        let mut progressed = false;
        for it in [&mut a as &mut dyn Iterator<Item = (Fact, f64)>, &mut b] {
            if let Some((f, _)) = it.next() {
                progressed = true;
                // 同一条事实两边都命中时只留先出现的那个 —— 也就是
                // 名次更靠前的那个空间给它的位置
                if seen.insert(f.id.clone()) {
                    out.push(f);
                }
            }
        }
        if !progressed {
            break;
        }
    }
    out
}

fn is_bridged(f: &Fact, bridged: &std::collections::HashSet<String>) -> bool {
    if bridged.is_empty() {
        return false;
    }
    bridged.contains(&f.subject_id)
        || f.object_entity_id
            .as_deref()
            .is_some_and(|id| bridged.contains(id))
}

/// 各路的召回宽度。
///
/// 比最终注入条数大一个量级 —— RRF 要有融合空间才有意义，
/// 每路只给 5 条的话「跨路共识」根本无从体现。
#[derive(Debug, Clone, Copy)]
pub struct RecallWidth {
    pub bm25: i64,
    pub vector: i64,
    pub graph: i64,
    pub recent: i64,
    /// episodes 那一路取多少条事实
    pub episode: i64,
    /// episodes 那一路先取多少条原文（再由它们展开成事实）
    pub episode_docs: i64,
    /// 近因那一路回看多少天
    pub recent_days: i64,
    /// 图遍历的跳数
    pub graph_hops: i32,
}

impl Default for RecallWidth {
    fn default() -> Self {
        Self {
            bm25: 40,
            vector: 40,
            graph: 30,
            recent: 20,
            // 比其它路窄一截：一条 episode 常能展开出三五条事实，
            // 给到 40 会让这一路的尾部全是「同一段对话里顺带提到的别的事」
            episode: 20,
            episode_docs: 8,
            recent_days: 7,
            graph_hops: 2,
        }
    }
}

/// 「时间近因」这件事该以什么形态存在。
///
/// # 定案：`Off`
///
/// roadmap 把它列为待定项（「保留为独立召回路」还是「改为对融合分乘
/// recency decay」）。评测给出的答案是**两个都不选**：
///
/// | 形态 | R@1 | R@5 | R@10 | MRR | gold 均名次 | 平均注入 | 误召@5 |
/// |---|---|---|---|---|---|---|---|
/// | `Channel` | 0.570 | 0.928 | 0.959 | 0.745 | 3.05 | 38.3 | 0.250 |
/// | **`Off`** | **0.673** | 0.923 | **0.969** | **0.812** | **2.78** | **29.5** | 0.286 |
/// | `Decay` 0.3/3d | 0.673 | 0.923 | 0.969 | 0.812 | 2.78 | 40.1 | 0.286 |
///
/// **`Channel` 出局**：独占题数 **0**，自己的 R@5 只有 0.094，
/// 却把 R@1 压低 0.103、MRR 压低 0.067，还让注入条数涨了 30%。
/// 它唯一「赢」的那一列（误召 0.250 < 0.286）是假的 —— 它同时把
/// 「应召不到」题的注入量从 22.4 抬到 28.2，也就是**用更多无关记忆
/// 把那条已知的干扰项挤出了 top-5**。forbidden 只是错误答案的一个抽样，
/// 拿没被抽到的错误答案换掉被抽到的，指标好看而已。
///
/// 更根本的问题是它的对象不对：`recall_recent` 返回的是**最近入库**的
/// 事实，而不是「刚才在聊的」。抽取是异步的，两者在时间上并不重合。
/// 「刚才说的那个」本来就由对话 history 直接承载，不需要检索；
/// 真要从记忆里倒查原话，那是 [`Channel::Episode`] 那一路的活。
///
/// **`Decay` 无法证伪也无法证实**：本题集的全部事实是在一次 ingest 里
/// 几秒内写完的，`created_at` 没有任何跨度，衰减系数对每条都一样 ——
/// 表里 `Decay` 与 `Off` 的排序指标逐位相同，差别只有注入条数（乘数把
/// 所有分一起抬过了弃权阈值）。要判它，题集得先有「跨越数周的语料」
/// 这一维，那是题集的下一步工作，不是现在能拍板的事。
/// 实现保留在这里，默认不启用。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RecencyMode {
    /// 作为独立一路进 RRF（历史行为，已被数据否掉）
    Channel,
    /// 不占召回路，只对融合分乘一个新鲜度系数（未定案，默认不用）
    Decay { strength: f64, half_life_days: f64 },
    /// 完全不要 —— **当前定案**
    Off,
}

/// 一次检索的完整结果，含可归因的逐路命中。
#[derive(Debug)]
pub struct Retrieved {
    /// 已按融合分降序、已按预算截断
    pub items: Vec<MemoryItem>,
    /// 与 `items` 等长的归因信息 —— bad case 时这是唯一能定位问题的东西
    pub attribution: Vec<Attribution>,
}

#[derive(Debug, Clone)]
pub struct Attribution {
    pub fact_id: String,
    pub score: f64,
    /// 命中它的召回路
    pub channels: Vec<&'static str>,
}

/// 克隆是廉价的：`E` 是 `SharedEmbedder`（`Arc`），其余全是标量。
/// 多租户接线要按请求换库，那一步会连带克隆检索器。
#[derive(Clone)]
pub struct Retriever<E: Embedder> {
    embedder: E,
    width: RecallWidth,
    budget: Budget,
    /// 当前对话领域的加权系数。1.0 即不加权。
    domain_boost: f64,
    /// 融合分低于此值的条目不进注入。
    ///
    /// # 它管的是「注入多少条」，**不是**「相不相关」
    ///
    /// 这一点一开始搞错了，值得写清楚。RRF 分是**排名**的函数，不含任何
    /// 绝对相关性：库里根本没有答案时，各路照样各自返回自己最好的几条、
    /// 照样互相印证、融合分照样很高。评测把阈值从 0 一路扫到 0.030，
    /// 误召率**逐位不动**地停在 0.286；一直扫到 0.055 才开始降，
    /// 而那时 R@5 已经从 0.923 掉到 0.840。
    ///
    /// 换句话说：**任何能压住误召的阈值都已经把召回打死了。**
    /// 「该不该注入」那道判断由 [`Self::semantic_floor`] 用余弦距离做，
    /// 那才是带绝对量纲的信号。
    ///
    /// 它真正的价值是砍注入量，而注入量本身就是成本与攻击面：无关记忆
    /// 进了 prompt，既是幻觉的素材，也是记忆投毒的载体 —— 攻击者只要
    /// 让一条恶意事实入库，它就会在**所有**对话里被注入。
    ///
    /// | 阈值 | R@5 | R@10 | MRR | 误召@5 | 平均注入 |
    /// |---|---|---|---|---|---|
    /// | 0（关） | 0.923 | 0.969 | 0.817 | 0.286 | 45.2 |
    /// | 0.017（旧值） | 0.923 | 0.969 | 0.817 | 0.286 | 29.5 |
    /// | **0.030** | **0.923** | **0.969** | **0.817** | 0.286 | **12.9** |
    /// | 0.040 | 0.923 | 0.938 | 0.809 | 0.250 | 6.0 |
    /// | 0.055 | 0.840 | 0.845 | 0.769 | 0.179 | 2.3 |
    ///
    /// 取值依据见 [`Retriever::new`]。
    abstain_below: f64,
    /// 时间近因的形态。见 [`RecencyMode`]。
    recency: RecencyMode,
    /// 是否启用 episodes 那一路（L0 原文倒查）。
    ///
    /// # 定案：默认关，但实现与题目都留着
    ///
    /// 动机是真的：前四路全部只看 `facts.statement`，也就是抽取器改写过的
    /// 一句话，「上次我们说的那个」这类**出处追问**里用户复述的是原话，
    /// 而原话的措辞常常一个字都没进 statement。`episodes.tsv` 的 GIN 索引
    /// 建了却从来没人用。为此专门补了六道出处追问题（q106–q111）。
    ///
    /// 但数据不支持把它设成默认。111 题、`episodes × 强命中补偿` 的 2×2：
    ///
    /// | | R@5 | R@10 | MRR | 均名次 | 误召@5 | 平均注入 |
    /// |---|---|---|---|---|---|---|
    /// | 都不开 | 0.887 | 0.931 | 0.786 | 2.92 | 0.286 | 22.9 |
    /// | 只开 episodes | 0.887 | 0.911 | 0.778 | 3.27 | 0.214 | 27.5 |
    /// | **只开补偿** | **0.923** | **0.969** | **0.812** | **2.78** | 0.286 | 29.5 |
    /// | 两个都开 | 0.912 | 0.948 | 0.804 | 2.93 | 0.214 | 33.6 |
    ///
    /// 开它在两种补偿设置下都让排序变差（R@10 −0.020，均名次 +0.35），
    /// 唯一变好的是误召 —— 而那是**稀释**不是**判别**：同一组里
    /// 「应召不到」题的注入条数从 22.4 涨到 26.9，是多塞进来的无关事实
    /// 把题集恰好标注了的那条干扰项挤出了 top-5。forbidden 只是错误答案的
    /// 一个抽样，用没被抽到的错误答案换掉被抽到的，指标好看而已。
    /// （同一把尺子也否掉了近因那一路，见 [`RecencyMode`]。）
    ///
    /// 结构性原因：这一路按 **episode 的 `ts_rank`** 排序，于是命中那一段
    /// 对话里抽出来的**全部**事实会成块地占住前几名 —— gold 的兄弟事实
    /// 把 gold 自己挤下去。要翻案得先解决这个，而不是调宽度。
    ///
    /// 补的六道题里 q106/q107/q108 确实因它上升、无一下降，但它们在
    /// 69 条语料下**不开也答得对**（R@5 已经是 1.0）—— 语料太小，
    /// BM25 在几十条里瞎撞也撞得上。这一路的真实价值要到语料上千条、
    /// 原话与 statement 真正分叉之后才测得出来。
    episode_channel: bool,
    /// 单路强命中的补偿权重，见 [`fusion::apply_peak_bonus`]。0 = 关。
    peak_bonus: f64,
    /// 语义地板：全库最近的一条事实的余弦距离超过它，就整体弃权。
    ///
    /// 与 [`Self::abstain_below`] 是两种不同的闸门，缺一不可：
    /// 前者管**注入多少条**，后者管**该不该注入**。RRF 分是排名的函数，
    /// 库里没有答案时它照样很高；余弦距离才是模型定标的绝对量。
    /// `None` = 不设地板。
    semantic_floor: Option<f64>,
    /// 被 `relates_to` 边带出来的那一侧，要不要免掉域惩罚。
    /// 见 [`bridged_entities`]。留成开关是为了能 A/B。
    bridge_exemption: bool,
    /// **上一个** embedding 模型。设了就在回填期间也从旧空间捞一遍。
    ///
    /// 见 [`Self::with_legacy`]。默认 `None` —— 绝大多数部署从没换过模型。
    legacy: Option<crate::embed::SharedEmbedder>,
}

impl<E: Embedder> Retriever<E> {
    pub fn new(embedder: E) -> Self {
        Self {
            semantic_floor: default_semantic_floor(embedder.model_id()),
            embedder,
            width: RecallWidth::default(),
            budget: Budget::default(),
            domain_boost: 1.3,
            // 0.030 = 曲线上**最后一个五项主指标与「完全不设阈值」逐位相同**
            // 的点（R@1 / R@5 / R@10 / MRR / 误召@5 全都一样），而注入条数
            // 从 45.2 砍到 12.9。再往上（0.040）才开始拿 R@10 与 MRR 换。
            //
            // 前一版取 0.017，理由是「略高于单路第一名的 1/61≈0.0164，
            // 即只被一路捞到就不要」。那个推理本身没错，但它把阈值锚在了
            // 一个**与真命中的分数无关**的量上：真命中普遍在 0.03 以上，
            // 所以 0.017 白白多放进了一倍的噪声。锚点应该是「真命中的分布」，
            // 只有扫一遍才知道它在哪 —— 这正是先有尺子再调参的意思。
            //
            // 注意它与 `peak_bonus` 有交互（补偿项给真命中普遍抬了 0.02），
            // 动其中任何一个都要重扫这条曲线。
            abstain_below: DEFAULT_ABSTAIN_BELOW,
            // 定案见 `RecencyMode` 的文档：独占 0 题、自己 R@5 只有 0.101，
            // 却压低 R@1 与 MRR 并让注入涨 27%。砍掉。
            recency: RecencyMode::Off,
            // 定案见 `Retriever::retrieve` 上方关于 episodes 一路的长注释：
            // 实现留着、题目补了，但实测排序上是中性偏负，默认不开。
            episode_channel: false,
            // 见 `fusion::apply_peak_bonus`。取 0.04 的依据是量纲而非曲线极值：
            // 补偿项对一个「某路排第一」的结果值 0.04/2 = 0.02，
            // 约等于 1.2 个 RRF 单位（0.0164）——也就是「白送它一路共识」。
            // 够把单路强命中从被三路弱共识淹没里捞出来，又不足以盖过真共识。
            //
            // 实测（111 题，其余按生产默认）：开它 R@5 0.887→0.923、
            // R@10 0.931→0.969、MRR 0.786→0.812、gold 均名次 2.92→2.78，
            // 误召@5 纹丝不动 —— 这一项是纯赚的。
            //
            // 曲线上更大的权重分数还能更高（w=0.12 时 R@5 0.948），但那时
            // 补偿项已经超过各路 RRF 的总和，排序实际上退化成「取各路最好名次」，
            // RRF 的共识语义名存实亡，注入条数也从 33 涨到 41。
            // 曲线极值不等于对的默认值。
            peak_bonus: 0.04,
            // roadmap 原话：「融合时该通道需豁免域惩罚」。豁免范围与它
            // 为什么不是「整条图遍历通道」，见 `bridged_entities`
            bridge_exemption: true,
            legacy: None,
        }
    }

    /// 关掉跨域桥的域惩罚豁免。评测 A/B 用。
    #[must_use]
    pub fn with_bridge_exemption(mut self, on: bool) -> Self {
        self.bridge_exemption = on;
        self
    }

    #[must_use]
    pub fn with_width(mut self, width: RecallWidth) -> Self {
        self.width = width;
        self
    }

    #[must_use]
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = budget;
        self
    }

    /// 调整弃权阈值。设为 0 即关闭弃权（评测 A/B 用）。
    #[must_use]
    pub fn with_abstain_below(mut self, threshold: f64) -> Self {
        self.abstain_below = threshold;
        self
    }

    /// 换一种时间近因的形态。评测 A/B 用。
    #[must_use]
    pub fn with_recency(mut self, mode: RecencyMode) -> Self {
        self.recency = mode;
        self
    }

    /// 开关 episodes 那一路。评测 A/B 用。
    #[must_use]
    pub fn with_episode_channel(mut self, on: bool) -> Self {
        self.episode_channel = on;
        self
    }

    /// 单路强命中补偿的权重。评测 A/B 用。
    #[must_use]
    pub fn with_peak_bonus(mut self, weight: f64) -> Self {
        self.peak_bonus = weight;
        self
    }

    /// 挂上**上一个** embedding 模型，回填期间也从旧空间捞一遍。
    ///
    /// # 为什么需要它
    ///
    /// 换模型之后旧事实的向量只在旧空间里，而向量查询按 `embedding_model`
    /// 过滤，于是它们**在回填补到它们之前**一直进不了向量召回。库大到
    /// 回填要跑几天时，那是几天的部分失忆。
    ///
    /// 挂上之后查询会被**同时编到两个空间**，各捞一次，按 id 去重后
    /// 合成同一条 [`Channel::Vector`]。
    ///
    /// # 为什么不新开一条 channel
    ///
    /// RRF 的输入结构一变，`scripts/evals-baseline.*.json` 里的每个数字就
    /// 不可比了 —— 而那份基线是判断「这次改动有没有让检索变差」的**唯一**
    /// 依据。一个只在回填期间存在的临时状态，不值得让基线作废。
    ///
    /// # ⚠️ 它救不了 fastembed → api 那一次迁移
    ///
    /// 旧模型是进程内的 fastembed，而它默认已经不编进二进制。见
    /// [`crate::embed_api::ApiEmbedder::legacy_from_env`]。
    #[must_use]
    pub fn with_legacy(mut self, legacy: Option<crate::embed::SharedEmbedder>) -> Self {
        self.legacy = legacy;
        self
    }

    /// 语义地板（余弦距离上限）。`None` 关闭。
    #[must_use]
    pub fn with_semantic_floor(mut self, max_distance: Option<f64>) -> Self {
        self.semantic_floor = max_distance;
        self
    }

    /// 当前生效的语义地板。默认值取决于 embedding 后端，
    /// 所以调用方问不出来就没法把它写进日志或评测报告。
    #[must_use]
    pub fn semantic_floor(&self) -> Option<f64> {
        self.semantic_floor
    }

    /// 四路召回 + RRF + 域加权 + 预算截断。
    ///
    /// `context_window` 用于算注入预算；`domain` 是当前对话领域。
    pub async fn retrieve(
        &self,
        store: &Store,
        query: &str,
        domain: Option<&str>,
        context_window: usize,
    ) -> Result<Retrieved> {
        let (channels, nearest) = self.recall_all(store, query).await?;

        // 语义地板：全库最近的一条都够不着，说明这个问题的答案根本不在记忆里。
        // 必须在融合**之前**判，因为融合分只反映跨路共识，不反映相关性。
        if let (Some(floor), Some(d)) = (self.semantic_floor, nearest)
            && d > floor
        {
            return Ok(Retrieved {
                items: Vec::new(),
                attribution: Vec::new(),
            });
        }

        // 域惩罚的豁免名单必须在融合**之前**从图遍历那一路算出来：
        // 融合会把逐路的原始列表揉成一张扁平表，`Fused::hits` 只留了通道名，
        // 走到那一步就已经分不清「哪条是被桥带出来的」了
        let bridged = if self.bridge_exemption {
            bridged_entities(&channels)
        } else {
            std::collections::HashSet::new()
        };

        let mut fused = fusion::rrf(&channels, |f: &Fact| f.id.clone());
        self.post_fuse(&mut fused);
        fusion::apply_domain_boost(
            &mut fused,
            domain,
            self.domain_boost,
            |f| f.domain.as_deref(),
            |f| is_bridged(f, &bridged),
        );

        Ok(self.finish(fused, context_window))
    }

    /// 按**系统时间**回放，取「当时的全貌」：按 `created_at` 降序的快照。
    ///
    /// 只在**没有可用查询词**时才该走这条（例如「把当时的状态导出来」这类
    /// 全量导出）。带问题的回放一律走 [`Retriever::retrieve_as_of_ranked`] ——
    /// 详见那里关于「全貌不等于可用」的评测数字。
    ///
    /// **待办（不在本次改动的边界内）**：`cortexd::live::AppState::retrieve`
    /// 目前对所有带 `as_of` 的请求都走这条，而它那里 `q.q`（用户的问题）
    /// 就在手边。把那一处换成 `retrieve_as_of_ranked(&self.store, &q.q, …)`
    /// 就能把时间回放题的 R@5 从 0.417 抬到 0.833。
    pub async fn retrieve_as_of(
        &self,
        store: &Store,
        as_of: DateTime<Utc>,
        limit: i64,
        context_window: usize,
    ) -> Result<Retrieved> {
        let facts = store
            .facts_as_of(as_of, limit)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))?;

        Ok(self.finish(chronological(facts), context_window))
    }

    /// 回放 + 检索：在 `as_of` 那一刻的快照**之上**做相关性排序。
    ///
    /// # 为什么原来那版不行
    ///
    /// 最初的实现只有 [`Retriever::retrieve_as_of`]，理由写的是「回放要的是
    /// 当时的全貌而非与查询最相关的，混入相关性排序会让回放结果取决于问法」。
    /// 这个理由在语料只有几十条时看不出问题，评测把它证伪了：
    ///
    /// - 时间回放题 R@5 只有 **0.417**，而 gold 的平均名次是 **23.4**
    /// - 也就是说 gold **全都在快照里**，只是排在几十名开外
    /// - 预算截断一上，连 R@∞ 都掉到 0.806 —— gold 被整个截没了
    ///
    /// 「问三个月前的对象存储方案，拿回最近 40 条**无关**事实」，
    /// 这不是全貌，是噪声。十万条记忆时它只会更糟。
    ///
    /// # 现在的做法
    ///
    /// 三路都**限定在同一份 as-of 快照上**，再 RRF：
    ///
    /// | 路 | 作用 |
    /// |---|---|
    /// | BM25（快照内） | 「MinIO」「5442」这种精确措辞 |
    /// | 向量（快照内） | 「那会儿的存储方案」这种换说法 |
    /// | 时间倒序（快照内） | 兜底 —— 前两路都哑火时退化回全貌 |
    ///
    /// 关键在于**快照是共同的前提而不是排序依据**：三路查的都是
    /// 「`created_at <= as_of` 且当时未被 invalidate」的那一批，
    /// 所以「当时以为什么」这个语义一点没丢，丢掉的只是「按时间倒序」
    /// 这个跟问题无关的排序。第三路留着，`把当时所有结论过一遍`
    /// 这种没有检索信号的问法才不会退化成空手而归。
    pub async fn retrieve_as_of_ranked(
        &self,
        store: &Store,
        query: &str,
        as_of: DateTime<Utc>,
        limit: i64,
        context_window: usize,
    ) -> Result<Retrieved> {
        let tsquery = tokenize::to_tsquery_input(query);
        let qvec = self.embedder.embed_one(query).await?;
        let embedding = cortex_store::Vector::from(qvec);

        let (bm25, vector, chrono) = tokio::join!(
            store.facts_as_of_bm25(as_of, &tsquery, limit),
            store.facts_as_of_vector(as_of, &embedding, self.embedder.model_id(), limit),
            store.facts_as_of(as_of, limit),
        );

        let to_err = |e: cortex_store::StoreError| CortexError::Store(e.to_string());
        let channels = vec![
            (Channel::Bm25, bm25.map_err(to_err)?),
            (Channel::Vector, vector.map_err(to_err)?),
            (Channel::Recency, chrono.map_err(to_err)?),
        ];

        let mut fused = fusion::rrf(&channels, |f: &Fact| f.id.clone());
        fusion::apply_peak_bonus(&mut fused, self.peak_bonus);
        // 这里**不**叠 recency decay：快照内的「新」是相对 `as_of` 而不是
        // 相对 now，用 now 去算 age 会把整个快照一视同仁地压平
        Ok(self.finish(fused, context_window))
    }

    // ─────────────────────── 内部 ───────────────────────

    /// 各路的原始召回，外加**全库最近的余弦距离**（供语义地板判定）。
    async fn recall_all(
        &self,
        store: &Store,
        query: &str,
    ) -> Result<(Vec<(Channel, Vec<Fact>)>, Option<f64>)> {
        let tsquery = tokenize::to_tsquery_input(query);
        let qvec = self.embedder.embed_one(query).await?;
        let embedding = cortex_store::Vector::from(qvec);

        // 图遍历的种子：从查询里切出的词去匹配实体名
        let terms: Vec<String> = tokenize::to_tsvector_input(query)
            .split_whitespace()
            .map(str::to_string)
            .collect();

        let seeds = store
            .entities_matching(&terms, GRAPH_SEED_LIMIT)
            .await
            .map_err(|e| CortexError::Store(e.to_string()))?;

        // 各路并发 —— 它们互不依赖，串行跑纯属浪费。
        // 关掉的路仍然发查询会白白多一次往返，所以宽度直接置 0 是不行的
        // （存储层会 clamp 到 1），必须整条跳过。
        let want_recent = self.recency == RecencyMode::Channel;
        // 旧空间那一路也放进这个 join：它要先跑一次 embedding（一次 HTTP），
        // 串在前面等于给每次检索白加一个往返。没挂 legacy 时是一个立刻
        // 返回空的 future，代价为零
        let legacy_vector = async {
            let Some(legacy) = &self.legacy else {
                return Vec::new();
            };
            let Ok(qvec) = legacy.embed_one(query).await else {
                // 旧模型那侧连不上**不该让整次检索失败** —— 它是回填期间的
                // 补充，主路径已经返回了当前空间的结果。降级成「只查新空间」，
                // 也就是没挂 legacy 时的行为
                tracing::warn!("旧模型 embedding 失败，本次只查当前空间");
                return Vec::new();
            };
            let v = cortex_store::Vector::from(qvec);
            store
                .recall_vector_scored(&v, legacy.model_id(), self.width.vector)
                .await
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "旧空间召回失败，本次只查当前空间");
                    Vec::new()
                })
        };
        let (bm25, vector, graph, recent, episode, legacy_vector) = tokio::join!(
            store.recall_bm25(&tsquery, self.width.bm25),
            store.recall_vector_scored(&embedding, self.embedder.model_id(), self.width.vector),
            store.recall_graph(&seeds, self.width.graph_hops, self.width.graph),
            async {
                if want_recent {
                    store
                        .recall_recent(self.width.recent_days, self.width.recent)
                        .await
                } else {
                    Ok(Vec::new())
                }
            },
            async {
                if self.episode_channel {
                    store
                        .recall_via_episodes(&tsquery, self.width.episode_docs, self.width.episode)
                        .await
                } else {
                    Ok(Vec::new())
                }
            },
            legacy_vector,
        );

        let to_err = |e: cortex_store::StoreError| CortexError::Store(e.to_string());
        let scored = vector.map_err(to_err)?;
        // 已按距离升序，第一条就是全库最近的。
        //
        // **只取当前空间的**：语义地板 0.50 是在 bge-m3 上标定的，
        // 旧空间的距离由旧模型定标，两者不可比。拿旧空间的距离去撞这个阈值
        // 得到的是一个没有含义的判断 —— 而它的表现是「弃权与否变得随机」，
        // 没有任何报错。当前空间为空时 nearest 是 None，也就是不做弃权判断
        let nearest = scored.first().map(|(_, d)| *d);
        let vector = merge_vector_spaces(scored, legacy_vector);

        Ok((
            vec![
                (Channel::Bm25, bm25.map_err(to_err)?),
                (Channel::Vector, vector),
                (Channel::Graph, graph.map_err(to_err)?),
                (Channel::Recency, recent.map_err(to_err)?),
                (Channel::Episode, episode.map_err(to_err)?),
            ],
            nearest,
        ))
    }

    /// RRF 之后、域加权之前的调整。
    fn post_fuse(&self, fused: &mut [Fused<Fact>]) {
        fusion::apply_peak_bonus(fused, self.peak_bonus);
        if let RecencyMode::Decay {
            strength,
            half_life_days,
        } = self.recency
        {
            fusion::apply_recency_decay(fused, Utc::now(), strength, half_life_days, |f: &Fact| {
                f.created_at
            });
        }
    }

    /// 融合结果 → 注入条目 + 归因，按预算截断。
    fn finish(&self, fused: Vec<Fused<Fact>>, context_window: usize) -> Retrieved {
        let max_tokens = self.budget.tokens_for(context_window);

        // 弃权：没有任何一条够格就一条都不给。
        // 注意是**整体弃权**而非逐条过滤 —— 逐条过滤会让「勉强够格的一条」
        // 单独进注入块，而单独一条弱相关记忆比零条更容易误导模型。
        let fused: Vec<Fused<Fact>> = if fused
            .first()
            .is_none_or(|top| top.score < self.abstain_below)
        {
            Vec::new()
        } else {
            fused
                .into_iter()
                .filter(|f| f.score >= self.abstain_below)
                .collect()
        };

        let attribution: Vec<Attribution> = fused
            .iter()
            .map(|f| Attribution {
                fact_id: f.item.id.clone(),
                score: f.score,
                channels: f.hits.iter().map(|(c, _)| c.as_str()).collect(),
            })
            .collect();

        let items: Vec<MemoryItem> = fused
            .into_iter()
            .map(|f| MemoryItem {
                id: f.item.id,
                statement: f.item.statement,
                valid_at: f.item.valid_at.map(|t| t.to_rfc3339()),
                known_since: f.item.created_at.to_rfc3339(),
                source_episode_id: Some(f.item.source_episode_id),
                domain: f.item.domain,
                // 下面四个**不进注入块**，是给客户端记忆抽屉的（见 MemoryItem 文档）。
                // 它们此前在这里被丢掉，于是抽屉上每条记忆都显示「100%」——
                // 那个数字是 cortexd 写死的常量，不是这条事实的打分
                predicate: Some(f.item.predicate),
                confidence: Some(f.item.confidence),
                source_channel: Some(f.item.source_channel.as_str().to_string()),
                trust_tier: f.item.trust_tier,
            })
            .collect();

        let kept = crate::injection::fit_to_budget(items, max_tokens);
        // 归因表要与截断后的条目对齐，否则 UI 上分数会错位
        let attribution = attribution.into_iter().take(kept.len()).collect();

        Retrieved {
            items: kept,
            attribution,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_width_leaves_room_for_fusion() {
        let w = RecallWidth::default();
        // 每路的宽度必须显著大于最终注入条数，否则 RRF 没有融合空间：
        // 各路只给三五条时，「跨路共识」这个核心信号根本无从体现
        assert!(w.bm25 >= 20 && w.vector >= 20, "召回过窄会让 RRF 退化");
        assert!(w.graph_hops <= 3, "跳数过大会让图遍历吃掉整个延迟预算");
    }

    #[test]
    fn abstains_when_nothing_is_relevant() {
        // recency 一路恒定返回最近 N 条与查询无关的事实。
        // 若不弃权，任何问题都会带回一堆记忆 —— 这是幻觉与记忆投毒的温床。
        let r = Retriever::new(crate::embed::HashEmbedder::new());
        // 单路排名第 40 名的贡献 ≈ 1/(60+41) ≈ 0.0099，低于阈值
        let weak = vec![Fused {
            item: dummy_fact(),
            score: 0.0099,
            hits: vec![(Channel::Recency, 40)],
        }];
        assert!(
            r.finish(weak, 128_000).items.is_empty(),
            "只被近因一路捞到且排名靠后，不应注入任何记忆"
        );
    }

    #[test]
    fn keeps_results_with_cross_channel_consensus() {
        let r = Retriever::new(crate::embed::HashEmbedder::new());
        // 两路都排第一：0.0164 × 2 ≈ 0.0328，再叠上 `peak_bonus` 的 0.02
        // 就是 0.053 —— 阈值 0.030 正是照着这个分布定的，见 `abstain_below`
        let strong = vec![Fused {
            item: dummy_fact(),
            score: 0.0328 + 0.02,
            hits: vec![(Channel::Bm25, 0), (Channel::Vector, 0)],
        }];
        assert_eq!(
            r.finish(strong, 128_000).items.len(),
            1,
            "跨路共识的强命中必须保留"
        );
    }

    /// 造一条指定 id 的事实，用来断言合并顺序。
    fn fact_with_id(id: &str) -> Fact {
        let mut f = dummy_fact();
        f.id = id.to_owned();
        f
    }

    #[test]
    fn merging_two_spaces_interleaves_by_rank() {
        // 回填期间旧空间里的事实更多（存量全在那边），当前空间只有新写的。
        // 让当前空间整体排前面，等于把全部历史记忆压到 41 名之后 ——
        // RRF 的核是 1/(k+rank)，那基本等于没召回
        let current = vec![
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA1"), 0.1),
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA2"), 0.2),
        ];
        let legacy = vec![
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB1"), 0.3),
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB2"), 0.4),
        ];
        let ids: Vec<String> = merge_vector_spaces(current, legacy)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                "01AAAAAAAAAAAAAAAAAAAAAAA1",
                "01BBBBBBBBBBBBBBBBBBBBBBB1",
                "01AAAAAAAAAAAAAAAAAAAAAAA2",
                "01BBBBBBBBBBBBBBBBBBBBBBB2",
            ],
            "两个空间必须按名次交替，否则其中一边会被整体压到后面"
        );
    }

    #[test]
    fn merging_dedups_and_keeps_the_better_rank() {
        // 同一条事实两边都命中：只留一次，且留在**先出现**的位置上。
        // 留两次的后果是 RRF 给它双份排名 —— 一个悄悄放大的偏置，不是报错
        let shared = "01CCCCCCCCCCCCCCCCCCCCCCC1";
        let current = vec![
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA1"), 0.1),
            (fact_with_id(shared), 0.2),
        ];
        let legacy = vec![(fact_with_id(shared), 0.05)];
        let ids: Vec<String> = merge_vector_spaces(current, legacy)
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            ids,
            vec!["01AAAAAAAAAAAAAAAAAAAAAAA1", shared],
            "重复的事实只该出现一次，且在两边给它的更靠前的那个位置上"
        );
    }

    #[test]
    fn merging_without_legacy_is_the_identity() {
        // 绝大多数部署从没换过模型 —— 那条路径必须与改动前逐位相同
        let current = vec![
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA1"), 0.1),
            (fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA2"), 0.2),
        ];
        let ids: Vec<String> = merge_vector_spaces(current, Vec::new())
            .into_iter()
            .map(|f| f.id)
            .collect();
        assert_eq!(
            ids,
            vec!["01AAAAAAAAAAAAAAAAAAAAAAA1", "01AAAAAAAAAAAAAAAAAAAAAAA2"],
            "没挂 legacy 时必须原样返回当前空间"
        );
    }

    #[test]
    fn merging_handles_uneven_lengths() {
        // 一边先取完时另一边要继续取完，不能提前停 ——
        // 提前停的表现是「旧记忆只回来一半」，而那一半是随机的
        let current = vec![(fact_with_id("01AAAAAAAAAAAAAAAAAAAAAAA1"), 0.1)];
        let legacy = vec![
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB1"), 0.2),
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB2"), 0.3),
            (fact_with_id("01BBBBBBBBBBBBBBBBBBBBBBB3"), 0.4),
        ];
        assert_eq!(
            merge_vector_spaces(current, legacy).len(),
            4,
            "长度不等时不能丢条目"
        );
    }

    fn dummy_fact() -> Fact {
        Fact {
            id: "01JZZZZZZZZZZZZZZZZZZZZZZ1".into(),
            subject_id: "01JZZZZZZZZZZZZZZZZZZZZZZ2".into(),
            predicate: "uses".into(),
            object_text: Some("x".into()),
            object_entity_id: None,
            statement: "测试事实".into(),
            embedding: None,
            embedding_model: "hash-stub-v1".into(),
            domain: None,
            confidence: 1.0,
            valid_at: None,
            source_episode_id: "01JZZZZZZZZZZZZZZZZZZZZZZ3".into(),
            source_channel: cortex_store::SourceChannel::Conversation,
            trust_tier: Some(2),
            extracted_by: "test".into(),
            device_id: "test".into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn budget_bounds_injection() {
        let b = Budget::default();
        assert!(b.tokens_for(128_000) <= b.absolute_max_tokens);
    }

    #[test]
    fn semantic_floor_only_applies_to_a_semantic_space() {
        // 散列桩的「距离」跟相关性无关。给它装上 0.50 会把离线开发与无网 CI
        // 的召回**静默**清零 —— 检索照常返回，只是永远返回空
        assert_eq!(
            default_semantic_floor("hash-stub-v1"),
            None,
            "散列桩不是语义空间，不能套用向量模型标定出来的地板"
        );
        assert_eq!(
            default_semantic_floor("fastembed:gpahal/bge-m3-onnx-int8"),
            Some(BGE_M3_SEMANTIC_FLOOR)
        );

        let r = Retriever::new(crate::embed::HashEmbedder::new());
        assert_eq!(
            r.semantic_floor(),
            None,
            "构造时就该按后端决定，而不是事后打补丁"
        );
    }

    #[test]
    fn chronological_replay_is_not_gated_by_the_abstain_threshold() {
        // 全貌回放没有第二路可融合，若沿用 RRF 的 1/(60+i+1)，头一条只有
        // 0.0164，会被 0.017 的弃权阈值整个吃掉 —— 用户明确要求回放，
        // 却拿回空的
        let r = Retriever::new(crate::embed::HashEmbedder::new());
        let fused = chronological(vec![dummy_fact()]);
        assert_eq!(
            r.finish(fused, 128_000).items.len(),
            1,
            "显式的时间回放不该被为四路召回标定的阈值挡掉"
        );
    }

    #[test]
    fn recency_is_off_by_default() {
        // 这条挡的是「有人顺手把它改回 Channel」：定案的依据是评测数字
        // （独占 0、R@5 0.101、压低 R@1 0.088、注入涨 27%），
        // 改回去必须连带重跑评测，见 `RecencyMode` 的表
        let r = Retriever::new(crate::embed::HashEmbedder::new());
        assert_eq!(r.recency, RecencyMode::Off);
        assert!(
            !r.episode_channel,
            "episodes 一路实测排序上中性偏负，默认关；翻案要连带重跑 2×2"
        );
    }
}

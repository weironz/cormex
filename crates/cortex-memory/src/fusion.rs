//! 多路召回的融合排序。
//!
//! 用 **RRF（Reciprocal Rank Fusion）**而非加权求和：
//! 各路的分数量纲完全不同（BM25 是无上界的 tf-idf、向量是 [-1,1] 余弦、
//! 图遍历是跳数、时间近因是时间戳），加权求和的权重根本调不出来。
//! RRF 只用**排名**不用分数，因此对量纲免疫、无需训练、无需调参。
//!
//! ```text
//! score(d) = Σ  1 / (k + rank_i(d))       k = 60
//!           i∈召回路
//! ```
//!
//! 附带好处：换 embedding 模型期间新旧向量不在同一语义空间、分数不可比，
//! 但 RRF 只看各自路内的排名，**天然兼容混合状态**（见 memory.md §七）。
//!
//! # 它的两个代价，以及各自的解药
//!
//! 1. **弱共识压过强命中** —— 见 [`apply_peak_bonus`]。
//! 2. **分数不含绝对相关性** —— RRF 分是排名的函数，库里根本没有答案时
//!    各路照样各自返回最好的几条、照样互相印证、分照样很高。
//!    这一条**在这个模块里解决不了**，必须由检索器拿余弦距离
//!    （`retrieval::BGE_M3_SEMANTIC_FLOOR`）在融合之前判。

use std::collections::HashMap;

/// RRF 的平滑常数。60 是原论文的经验值，也是各家实现的事实默认。
pub const RRF_K: f64 = 60.0;

/// 一路召回的来源标识，用于可观测性（哪条路贡献了这个结果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// BM25 全文 —— 专有名词与精确匹配
    Bm25,
    /// 向量余弦 —— 语义相近
    Vector,
    /// 图遍历 —— 从命中实体扩展关联事实
    Graph,
    /// 时间近因 —— 上下文连续性
    Recency,
    /// L0 原文全文 —— 从 episode 倒查它抽出来的事实（出处追问）
    Episode,
}

impl Channel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bm25 => "bm25",
            Self::Vector => "vector",
            Self::Graph => "graph",
            Self::Recency => "recency",
            Self::Episode => "episode",
        }
    }
}

/// 融合后的一条结果。
#[derive(Debug, Clone)]
pub struct Fused<T> {
    pub item: T,
    pub score: f64,
    /// 命中它的召回路及在该路内的排名（从 0 起）。
    /// 保留下来是为了 bad case 能归因到具体通道——没有这个，
    /// 检索出问题时只能靠猜。
    pub hits: Vec<(Channel, usize)>,
}

/// 对多路召回结果做 RRF 融合。
///
/// `channels` 中每一路是**已按该路自身相关性降序排好**的候选列表。
/// `key` 从候选中取出去重用的稳定标识（通常是 fact id）。
///
/// 返回按融合分降序排列的结果。
pub fn rrf<T, K, F>(channels: &[(Channel, Vec<T>)], key: F) -> Vec<Fused<T>>
where
    T: Clone,
    K: std::hash::Hash + Eq + Ord,
    F: Fn(&T) -> K,
{
    let mut acc: HashMap<K, Fused<T>> = HashMap::new();

    for (channel, items) in channels {
        for (rank, item) in items.iter().enumerate() {
            let contribution = 1.0 / (RRF_K + rank as f64 + 1.0);
            let k = key(item);
            acc.entry(k)
                .and_modify(|f| {
                    f.score += contribution;
                    f.hits.push((*channel, rank));
                })
                .or_insert_with(|| Fused {
                    item: item.clone(),
                    score: contribution,
                    hits: vec![(*channel, rank)],
                });
        }
    }

    // 分数降序；同分时按命中路数多的优先（多路共识更可信）；
    // 再同则按 key 定序 —— **这一层不是洁癖**：
    //
    // `into_values()` 给出的是 `HashMap` 的迭代序，它随 hash 种子逐进程变化。
    // 同分在这里是常态（只被一路捞到、名次又相同的条目一抓一大把），于是
    // 同一份语料同一套参数跑两遍，top-1 会换人。实测评测跑三遍
    // R@1 在 0.630/0.640/0.650 之间跳、MRR 跳 0.012 —— 也就是说
    // **这套评测当时分辨不了小于 0.02 的改动**，而绝大多数调参改动都在这个量级。
    // 后续的 `apply_*` 用的是稳定排序，只要这里定死，全链路就定死了。
    let mut out: Vec<(K, Fused<T>)> = acc.into_iter().collect();
    out.sort_by(|(ka, a), (kb, b)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.hits.len().cmp(&a.hits.len()))
            .then_with(|| ka.cmp(kb))
    });
    out.into_iter().map(|(_, f)| f).collect()
}

/// 单路强命中的补偿项 —— RRF 的已知代价的解药。
///
/// # 它修的是什么
///
/// RRF 只认排名，于是「两路都排第 10」（0.0141×2 ≈ 0.028）会压过
/// 「一路排第 1」（0.0164）。多数时候这正是想要的 —— 跨路共识确实更可信 ——
/// 但当某一路给出的是**强信号**时它就成了误伤：一条被 BM25 精确命中在
/// 榜首的事实，会被几条「哪一路都排在十几名」的话题相邻项挤下去。
///
/// 解法不是给某一路加权（那要按题型调，调不出来），而是**再叠一个更陡的核**：
///
/// ```text
/// score(d) += weight / (PEAK_K + min_i rank_i(d))
/// ```
///
/// `PEAK_K` 取 2 而不是 60：排名 0 与排名 10 在 k=60 下只差 15%，
/// 根本区分不出「强命中」；k=2 下差 6 倍，只有真正的头部才拿得到补偿。
/// `weight` 为 0 即关闭。
///
/// 它**不是**「保底进 top-N」那种硬插队。硬插队要么改写排序、要么占死名额，
/// 两者都会在「强命中其实是错的」时把错误顶到最前；而这里只是加一项，
/// 真正的强共识（三路以上）仍然压得过它。权重取值见
/// `retrieval::Retriever::new` 里的注释与那条扫描曲线。
pub const PEAK_K: f64 = 2.0;

/// 见 [`PEAK_K`]。`weight` 为 0 时直接返回，不重排。
pub fn apply_peak_bonus<T>(fused: &mut [Fused<T>], weight: f64) {
    if weight <= 0.0 {
        return;
    }
    for f in fused.iter_mut() {
        let best = f.hits.iter().map(|(_, r)| *r).min().unwrap_or(usize::MAX);
        if best == usize::MAX {
            continue;
        }
        f.score += weight / (PEAK_K + best as f64);
    }
    sort_by_score(fused);
}

/// 新鲜度衰减：越新的事实分越高。
///
/// 这是「时间近因」的**另一种形态** —— 不占一路召回，只当融合分的乘数。
/// 两种形态互斥，孰优孰劣见 `retrieval::RecencyMode` 的注释与评测数字。
///
/// 系数是 `1 + strength · 0.5^(age / half_life)`：最新的事实拿满
/// `1 + strength`，一个半衰期前的拿一半，很久以前的趋近 1（即不加权）。
/// 用**乘数**而不是加数，是为了不改变「零分即弃权」这个语义 ——
/// 加数会把所有事实的分抬到弃权阈值之上。
pub fn apply_recency_decay<T, F>(
    fused: &mut [Fused<T>],
    now: chrono::DateTime<chrono::Utc>,
    strength: f64,
    half_life_days: f64,
    created_at_of: F,
) where
    F: Fn(&T) -> chrono::DateTime<chrono::Utc>,
{
    if strength <= 0.0 || half_life_days <= 0.0 {
        return;
    }
    for f in fused.iter_mut() {
        let age_days = (now - created_at_of(&f.item)).num_seconds().max(0) as f64 / 86_400.0;
        f.score *= 1.0 + strength * 0.5f64.powf(age_days / half_life_days);
    }
    sort_by_score(fused);
}

fn sort_by_score<T>(fused: &mut [Fused<T>]) {
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.hits.len().cmp(&a.hits.len()))
    });
}

/// 领域加权：当前对话领域内的事实加分。
///
/// 这是**融合之后**的调整，不是第五路召回——领域是过滤维度不是相关性维度。
/// `boost` 取 1.0 即不加权。
///
/// # `exempt` —— 为什么需要一个豁免口
///
/// 给「当前领域」加分等价于给跨域的减分。绝大多数时候这正是想要的：
/// 聊代码时不该被上周的会议纪要打断。但有一类结果**按定义就是跨域的**，
/// 对它们施加同一份惩罚等于把刚架好的桥又压回水里 ——
/// 那就是 [`crate::crosslink`] 的 `relates_to` 边带出来的那一侧。
///
/// `exempt` 返回真的条目**不参与惩罚**（即也拿到 `boost`，与当前领域同权），
/// 而不是「拿双倍」：豁免的语义是「别惩罚它」，不是「优待它」。
/// 判定范围的取舍见 `retrieval::bridged_entities`。
pub fn apply_domain_boost<T, F, X>(
    fused: &mut [Fused<T>],
    current_domain: Option<&str>,
    boost: f64,
    domain_of: F,
    exempt: X,
) where
    F: Fn(&T) -> Option<&str>,
    X: Fn(&T) -> bool,
{
    let Some(cur) = current_domain else { return };
    for f in fused.iter_mut() {
        if domain_of(&f.item) == Some(cur) || exempt(&f.item) {
            f.score *= boost;
        }
    }
    fused.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Doc {
        id: &'static str,
        domain: Option<&'static str>,
    }

    fn d(id: &'static str) -> Doc {
        Doc { id, domain: None }
    }

    #[test]
    fn multi_channel_consensus_wins() {
        // A 在两路都排第二，B 只在一路排第一。
        // RRF 的核心行为：跨路共识应当压过单路的高排名。
        let channels = vec![
            (Channel::Bm25, vec![d("B"), d("A")]),
            (Channel::Vector, vec![d("C"), d("A")]),
        ];
        let out = rrf(&channels, |x| x.id);
        assert_eq!(out[0].item.id, "A", "两路共识应排第一：{out:?}");
        assert_eq!(out[0].hits.len(), 2);
    }

    #[test]
    fn dedups_across_channels() {
        let channels = vec![
            (Channel::Bm25, vec![d("A"), d("B")]),
            (Channel::Vector, vec![d("A")]),
            (Channel::Graph, vec![d("A")]),
        ];
        let out = rrf(&channels, |x| x.id);
        assert_eq!(out.len(), 2, "同一条不应重复出现");
        assert_eq!(out[0].hits.len(), 3, "应记录全部三路的命中");
    }

    #[test]
    fn preserves_attribution() {
        let channels = vec![(Channel::Recency, vec![d("A")])];
        let out = rrf(&channels, |x| x.id);
        assert_eq!(out[0].hits, vec![(Channel::Recency, 0)]);
    }

    #[test]
    fn rank_order_within_channel_matters() {
        let channels = vec![(Channel::Bm25, vec![d("first"), d("second"), d("third")])];
        let out = rrf(&channels, |x| x.id);
        assert_eq!(out[0].item.id, "first");
        assert!(out[0].score > out[1].score && out[1].score > out[2].score);
    }

    #[test]
    fn domain_boost_reorders() {
        let mut fused = rrf(
            &[(
                Channel::Bm25,
                vec![
                    Doc {
                        id: "work",
                        domain: Some("work"),
                    },
                    Doc {
                        id: "code",
                        domain: Some("coding"),
                    },
                ],
            )],
            |x| x.id,
        );
        assert_eq!(fused[0].item.id, "work");
        apply_domain_boost(&mut fused, Some("coding"), 2.0, |x| x.domain, |_| false);
        assert_eq!(fused[0].item.id, "code", "当前领域的事实应被提上来");
    }

    #[test]
    fn domain_boost_exemption_spares_the_bridged_side() {
        // 跨域连接带出来的那一侧按定义就是跨域的：不豁免就等于架了桥
        // 又把它压回水里
        let mut fused = rrf(
            &[(
                Channel::Bm25,
                vec![
                    Doc {
                        id: "same-domain",
                        domain: Some("coding"),
                    },
                    Doc {
                        id: "bridged",
                        domain: Some("office"),
                    },
                ],
            )],
            |x| x.id,
        );
        let before = fused
            .iter()
            .position(|f| f.item.id == "bridged")
            .expect("应在列表里");

        apply_domain_boost(
            &mut fused,
            Some("coding"),
            1.3,
            |x| x.domain,
            |x| x.id == "bridged",
        );
        let after = fused
            .iter()
            .position(|f| f.item.id == "bridged")
            .expect("应在列表里");
        assert!(
            fused[0].score / fused[1].score < 1.3,
            "被豁免的一侧应当与当前领域同权，实际分差 {:.3}",
            fused[0].score / fused[1].score
        );
        assert_eq!(before, after, "豁免只是免罚，不该把它顶到别人前面");
    }

    #[test]
    fn exemption_is_not_a_double_boost() {
        // 「免罚」与「优待」是两回事：豁免一条当前领域的事实必须是恒等操作，
        // 否则同时命中两个条件的条目会拿到 boost²，排序悄悄跑偏
        let build = || {
            rrf(
                &[(
                    Channel::Bm25,
                    vec![
                        Doc {
                            id: "a",
                            domain: Some("coding"),
                        },
                        Doc {
                            id: "b",
                            domain: Some("coding"),
                        },
                    ],
                )],
                |x| x.id,
            )
        };
        let mut plain = build();
        apply_domain_boost(&mut plain, Some("coding"), 1.3, |x| x.domain, |_| false);
        let mut exempted = build();
        apply_domain_boost(
            &mut exempted,
            Some("coding"),
            1.3,
            |x| x.domain,
            |x| x.id == "a",
        );
        assert_eq!(
            plain[0].score, exempted[0].score,
            "已在当前领域的条目再被豁免一次，分数不该变"
        );
    }

    #[test]
    fn ties_break_by_key_not_by_hash_order() {
        // 每条各占一路、名次都是 0 ⇒ 分数与命中路数全相等，唯一还能定序的只有 key。
        // 用十二条而不是三条：`HashMap` 的迭代序随 hash 种子逐进程变化，
        // 三条时它有六分之一的概率碰巧就是升序，测试会变成偶尔才红的哑弹
        let ids = [
            "m4", "b7", "z1", "a9", "q3", "c8", "k2", "w5", "e6", "t0", "n1", "h4",
        ];
        let channels: Vec<(Channel, Vec<Doc>)> =
            ids.iter().map(|id| (Channel::Bm25, vec![d(id)])).collect();
        let out = rrf(&channels, |x| x.id);

        let mut want = ids;
        want.sort_unstable();
        assert_eq!(
            out.iter().map(|f| f.item.id).collect::<Vec<_>>(),
            want.to_vec(),
            "完全同分时必须按 key 升序 —— 否则顺序落回 HashMap 的桶序，             同一份语料跑两遍 top-1 就会换人（实测让评测的 R@1 在 0.630/0.640/0.650 之间跳）"
        );
    }

    #[test]
    fn empty_input_yields_empty() {
        let out = rrf::<Doc, _, _>(&[], |x| x.id);
        assert!(out.is_empty());
    }

    /// 两路都把 `consensus` 排在第 11 名（rank 10），一路把 `strong` 排第一。
    /// 填充项各路互不相同，免得它们自己攒出共识来。
    fn lone_strong_vs_deep_consensus() -> Vec<(Channel, Vec<Doc>)> {
        let deep = |tag: &'static str| -> Vec<Doc> {
            (0..10)
                .map(|i| Doc {
                    id: Box::leak(format!("{tag}{i}").into_boxed_str()),
                    domain: None,
                })
                .chain(std::iter::once(d("consensus")))
                .collect()
        };
        vec![
            (Channel::Bm25, vec![d("strong")]),
            (Channel::Vector, deep("v")),
            (Channel::Episode, deep("e")),
        ]
    }

    #[test]
    fn peak_bonus_rescues_a_lone_strong_hit() {
        // 只比这两条的相对位置：填充项自己也在各路排前面，会拿到同样的补偿，
        // 把它们卷进断言只会测出「rank 0 的都涨了」这种废话
        let rank_of = |out: &[Fused<Doc>], id: &str| {
            out.iter()
                .position(|f| f.item.id == id)
                .expect("应当在列表里")
        };

        let mut out = rrf(&lone_strong_vs_deep_consensus(), |x| x.id);
        assert!(
            rank_of(&out, "consensus") < rank_of(&out, "strong"),
            "没有补偿时两路的深共识（≈0.028）本就压过单路第一（≈0.016）—— 这正是要修的那个代价"
        );

        // 生产默认权重。0.04/2 = 0.02，约等于「白送它一路共识」
        apply_peak_bonus(&mut out, 0.04);
        assert!(
            rank_of(&out, "strong") < rank_of(&out, "consensus"),
            "某一路排第一的强命中应当被补偿项抬到深共识之上：{out:?}"
        );
    }

    #[test]
    fn peak_bonus_does_not_beat_real_consensus() {
        // 它是**补偿**不是**保底**：三路都排前列的结果仍然该赢，
        // 否则 RRF 的共识语义就名存实亡了
        let channels = vec![
            (Channel::Bm25, vec![d("strong")]),
            (Channel::Vector, vec![d("agreed"), d("strong")]),
            (Channel::Graph, vec![d("agreed")]),
            (Channel::Episode, vec![d("agreed")]),
        ];
        let mut out = rrf(&channels, |x| x.id);
        apply_peak_bonus(&mut out, 0.04);
        assert_eq!(
            out[0].item.id,
            "agreed",
            "三路共识不该被单路补偿掀翻：{:?}",
            &out[..2]
        );
    }

    #[test]
    fn peak_bonus_of_zero_changes_nothing() {
        let channels = vec![(Channel::Bm25, vec![d("a"), d("b")])];
        let mut out = rrf(&channels, |x| x.id);
        let before: Vec<f64> = out.iter().map(|f| f.score).collect();
        apply_peak_bonus(&mut out, 0.0);
        let after: Vec<f64> = out.iter().map(|f| f.score).collect();
        assert_eq!(before, after, "权重为 0 必须是恒等操作，否则默认行为会漂");
    }

    #[test]
    fn recency_decay_reorders_by_age() {
        // 这条断言是 `RecencyMode::Decay` 唯一的正确性保证：评测题集的语料
        // 是一次 ingest 写完的，created_at 没有跨度，衰减在那里量不出来
        let now = chrono::Utc::now();
        let channels = vec![(Channel::Bm25, vec![d("old"), d("new")])];
        let mut out = rrf(&channels, |x| x.id);
        assert_eq!(out[0].item.id, "old", "衰减前按 BM25 名次");

        apply_recency_decay(&mut out, now, 0.5, 3.0, |x: &Doc| {
            if x.id == "new" {
                now
            } else {
                now - chrono::Duration::days(90)
            }
        });
        assert_eq!(
            out[0].item.id, "new",
            "同为一路命中时，九十天前的那条不该压过刚发生的"
        );
    }
}

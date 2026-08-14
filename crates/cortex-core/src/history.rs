//! 会话历史进上下文时的预算裁剪。
//!
//! # 这一层此前根本不存在
//!
//! 当时的两条对话路径（cortexd 的 `run_turn`、本地 agent 的
//! `Engine::run_turn`）都是这么开头的 —— cortexd 那一条后来连同它那份
//! 重复的 agent 循环一起删了，现在只剩本地 agent 这一条：
//!
//! ```ignore
//! let mut messages = vec![Message::user().with_text(&user_content)];
//! ```
//!
//! **只有当前这一条消息，会话历史一次都没被加载过。** 实测：同一会话里
//! 第一轮说「我的幸运数字是 4173」，第二轮问「刚才说的是多少」，
//! 模型答「在当前对话中，我没有看到你提到过幸运数字」。它说的是实话。
//!
//! 这与 [`crate::injection`] 模块文档画的那张图直接矛盾（那里写着
//! `history: user₁ ⟨记忆块₁⟩, asst₁, …`），而 `needs_compaction` 也只有在
//! history 会累积时才有意义 —— **那套设计一直在，实现从来没接上。**
//!
//! # 为什么记忆系统没能盖住它
//!
//! 记忆能捞回**被抽取成事实**的那部分，而且要等异步抽取跑完。
//! 「我刚才说了什么」「总结一下这段对话」这类问题落在它够不到的地方：
//! 那些内容压根不满足「下次不知道会做错决定」这条抽取判据。
//!
//! # 保留最新的，丢最老的
//!
//! 与显示分页相反：那边截断要保留**最老**的（用户从头往下读），
//! 这边要保留**最新**的（模型要接着这几轮往下说）。
//! 同一个 LIMIT 用错方向的症状截然不同，所以两条路各走各的查询。

use crate::injection::estimate_tokens;

/// 历史里的一轮发言。
///
/// 不用 `Role` 枚举而用一个布尔：这个模块在 `cortex-core` 里，
/// 而 `Role` 住在 `cortex-store`（带 sqlx 派生）。为一个两态的东西
/// 把存储层拽进来，方向是反的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryTurn {
    pub is_user: bool,
    pub text: String,
}

/// 裁剪的结果。
#[derive(Debug)]
pub struct FittedHistory {
    /// 装得下的那些，**按时间正序**（老 → 新）
    pub turns: Vec<HistoryTurn>,
    /// 因为预算不够被丢掉的条数（全是更老的那些）
    pub dropped: usize,
}

/// 历史能占 context window 的多少。
///
/// 记忆块占 12%（[`crate::injection::Budget`]），这里给 50%，剩下的留给
/// system prompt、工具 schema、以及模型自己的回复。
///
/// 不给更多是因为**再多也换不来什么**：一段几十轮的对话里，最相关的
/// 永远是最近几轮，而把 90% 的窗口填满历史会让工具调用与长回复没地方放，
/// 症状是「聊着聊着突然答不完就断了」。
const HISTORY_RATIO: f64 = 0.50;

/// 绝对上限。理由与 `Budget::absolute_max_tokens` 相同：context window
/// 已经有 1M 级的了，只按比例算会让历史膨胀到荒谬的尺寸 —— 而那是
/// 每一轮都要重新付一遍的钱。
const HISTORY_MAX_TOKENS: usize = 24_000;

/// 这次能给历史多少 token。
#[must_use]
pub fn history_budget(context_window: usize) -> usize {
    #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss)]
    let by_ratio = (context_window as f64 * HISTORY_RATIO) as usize;
    by_ratio.min(HISTORY_MAX_TOKENS)
}

/// 按预算从**尾部**往前收，保留最新的若干轮。
///
/// # 为什么成对地丢
///
/// 丢到一半留下一个孤零零的 assistant 开头，模型看到的是「不知道在回答
/// 什么的一段话」。宁可多丢一条，也不要留一个没有问题的答案。
///
/// # 空文本会被跳过
///
/// 导入的历史里有整条只有工具调用、没有正文的消息。它们进上下文只是噪声，
/// 而且会占掉一个「配对」名额。
#[must_use]
pub fn fit_history(turns: Vec<HistoryTurn>, max_tokens: usize) -> FittedHistory {
    let turns: Vec<HistoryTurn> = turns
        .into_iter()
        .filter(|t| !t.text.trim().is_empty())
        .collect();
    let total = turns.len();

    let mut used = 0usize;
    let mut keep_from = total;
    for (i, t) in turns.iter().enumerate().rev() {
        // 每条按角色标记 + 分隔的开销粗估 8 token
        let cost = estimate_tokens(&t.text) + 8;
        if used + cost > max_tokens {
            break;
        }
        used += cost;
        keep_from = i;
    }

    // 别让第一条是 assistant：那是一段没有问题的回答
    if turns.get(keep_from).is_some_and(|t| !t.is_user) {
        keep_from += 1;
    }

    FittedHistory {
        dropped: keep_from,
        turns: turns.into_iter().skip(keep_from).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(t: &str) -> HistoryTurn {
        HistoryTurn {
            is_user: true,
            text: t.into(),
        }
    }
    fn a(t: &str) -> HistoryTurn {
        HistoryTurn {
            is_user: false,
            text: t.into(),
        }
    }

    /// 装得下就一条不丢。
    #[test]
    fn a_short_conversation_survives_intact() {
        let h = fit_history(vec![u("问一"), a("答一"), u("问二"), a("答二")], 10_000);
        assert_eq!(h.dropped, 0);
        assert_eq!(h.turns.len(), 4);
        assert_eq!(h.turns[0].text, "问一", "顺序必须是老→新");
    }

    /// **丢的是最老的，留的是最新的。**
    ///
    /// 与显示分页相反 —— 那边保留最老的（用户从头读），这边保留最新的
    /// （模型接着往下说）。方向弄反的症状是「它记得三个月前，却忘了上一句」。
    #[test]
    fn the_oldest_turns_are_the_ones_dropped() {
        let turns: Vec<HistoryTurn> = (0..40)
            .flat_map(|i| {
                [
                    u(&format!("第{i}个问题，内容写长一点占住预算")),
                    a(&format!("第{i}个回答，内容也写长一点占住预算")),
                ]
            })
            .collect();
        let h = fit_history(turns, 400);

        assert!(h.dropped > 0, "预算这么小必须丢掉一些");
        assert!(!h.turns.is_empty(), "不该一条都不剩");
        let last = h.turns.last().unwrap();
        assert!(
            last.text.contains("第39个回答"),
            "最后一条必须是最新那条，实际：{}",
            last.text
        );
        assert!(
            !h.turns.iter().any(|t| t.text.contains("第0个")),
            "最老的那些应当已经被丢掉"
        );

        // **留下的必须真的装得下。** 没有这一条，一个从头往后收的实现
        // （方向弄反）也能让上面几条断言全部通过 —— 它同样把最新那条留在
        // 末尾，只是顺手超支了几十倍，而超支的表现是模型那边报 400
        let used: usize = h.turns.iter().map(|t| estimate_tokens(&t.text) + 8).sum();
        assert!(
            used <= 400,
            "裁剪后仍占 {used} token，超了 400 的预算：{:?}",
            h.turns.iter().map(|t| &t.text).collect::<Vec<_>>()
        );
    }

    /// **裁完不能以 assistant 开头。**
    ///
    /// 留下一个孤零零的回答，模型看到的是「不知道在回答什么的一段话」，
    /// 而它会当成上下文照单全收。宁可多丢一条。
    #[test]
    fn the_kept_window_never_starts_with_an_answer() {
        // 预算刚好卡在中间某处
        let turns = vec![
            u("很久以前的问题"),
            a("很久以前的回答"),
            u("新问题"),
            a("新回答"),
        ];
        for budget in 1..200 {
            let h = fit_history(turns.clone(), budget);
            if let Some(first) = h.turns.first() {
                assert!(
                    first.is_user,
                    "预算 {budget} 下裁出了以 assistant 开头的窗口：{:?}",
                    h.turns
                );
            }
        }
    }

    /// 空文本跳过 —— 导入的历史里有整条只有工具调用的消息。
    #[test]
    fn empty_messages_are_skipped() {
        let h = fit_history(vec![u("问"), a("   "), u("再问"), a("答")], 10_000);
        assert_eq!(h.turns.len(), 3);
        assert!(!h.turns.iter().any(|t| t.text.trim().is_empty()));
    }

    /// 预算小到装不下任何一轮时，返回空而不是 panic。
    #[test]
    fn an_impossible_budget_yields_nothing_rather_than_panicking() {
        let h = fit_history(vec![u("一段挺长的问题"), a("一段挺长的回答")], 1);
        assert!(h.turns.is_empty());
        assert_eq!(h.dropped, 2);
    }

    /// 预算随 context window 走，但有绝对上限。
    #[test]
    fn the_budget_scales_but_is_capped() {
        assert_eq!(history_budget(20_000), 10_000);
        assert_eq!(
            history_budget(1_000_000),
            HISTORY_MAX_TOKENS,
            "百万级窗口下也不该让历史膨胀到荒谬的尺寸 —— 那是每轮都要重付的钱"
        );
    }
}

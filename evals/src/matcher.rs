//! gold / forbidden 的匹配规则。
//!
//! 一组子串全部出现在同一条事实的 `statement` 里，才算命中这条 gold。
//!
//! # 为什么不按 fact id
//!
//! 事实的 id 是落库时现生成的 ULID，题集里写不出来。llm 模式下连
//! statement 的措辞都由模型决定。「这几个关键词得在同一条事实里」
//! 是唯一在 seed 与 llm 两种模式下都成立的标识方式。
//!
//! # 为什么只看 statement
//!
//! `statement` 是唯一被要求「能独立读懂」的字段（抽取 prompt 里明写了），
//! 也是向量化与界面展示用的字段。把 predicate / object 也拼进去匹配会
//! 显著放宽命中条件 —— gold 越松，分数越虚高，评测就越没用。

use crate::suite::Question;

/// 一条 gold（或 forbidden）：一组必须同时出现的子串。
#[derive(Debug, Clone)]
pub struct Matcher {
    /// 已小写化。中文不受影响，英文标识符（RustFS / rustfs）则需要
    parts: Vec<String>,
    /// 原样保留，报告里要打出来
    raw: Vec<String>,
}

impl Matcher {
    #[must_use]
    pub fn new(parts: &[String]) -> Self {
        Self {
            parts: parts.iter().map(|s| s.trim().to_lowercase()).collect(),
            raw: parts.to_vec(),
        }
    }

    /// 全部子串都出现才算命中。
    #[must_use]
    pub fn matches(&self, statement: &str) -> bool {
        if self.parts.is_empty() {
            return false;
        }
        let hay = statement.to_lowercase();
        self.parts.iter().all(|p| hay.contains(p))
    }

    /// 在一个已排序的候选列表里找**首个**命中的名次（从 0 起）。
    #[must_use]
    pub fn first_hit<'a, I>(&self, statements: I) -> Option<usize>
    where
        I: IntoIterator<Item = &'a str>,
    {
        statements.into_iter().position(|s| self.matches(s))
    }

    #[must_use]
    pub fn describe(&self) -> String {
        self.raw.join(" + ")
    }
}

/// 一道题的全部匹配器。
#[derive(Debug, Clone)]
pub struct QuestionMatchers {
    pub gold: Vec<Matcher>,
    pub forbidden: Vec<Matcher>,
    /// 「根本不该在库里」—— 比对的是语料快照而不是检索结果，见
    /// [`crate::suite::Question::never_extracted`]
    pub never_extracted: Vec<Matcher>,
}

impl QuestionMatchers {
    #[must_use]
    pub fn of(q: &Question) -> Self {
        Self {
            gold: q.gold.iter().map(|g| Matcher::new(g)).collect(),
            forbidden: q.forbidden.iter().map(|g| Matcher::new(g)).collect(),
            never_extracted: q.never_extracted.iter().map(|g| Matcher::new(g)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(parts: &[&str]) -> Matcher {
        Matcher::new(&parts.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn all_parts_must_appear() {
        let matcher = m(&["对象存储", "RustFS"]);
        assert!(matcher.matches("Cortex 的对象存储换成了 RustFS。"));
        assert!(
            !matcher.matches("Cortex 的对象存储先用 MinIO。"),
            "少一个关键词就不该算命中，否则 gold 形同虚设"
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(
            m(&["rustfs"]).matches("对象存储用 RustFS"),
            "英文标识符的大小写不该决定命中与否"
        );
    }

    #[test]
    fn first_hit_returns_rank_zero_based() {
        let matcher = m(&["RRF"]);
        let list = ["用 MinIO", "融合用 RRF，k 取 60", "另一条"];
        assert_eq!(matcher.first_hit(list.iter().copied()), Some(1));
    }

    #[test]
    fn first_hit_is_none_when_absent() {
        assert_eq!(m(&["不存在的东西"]).first_hit(["a", "b"]), None);
    }

    #[test]
    fn empty_matcher_never_matches() {
        // 空匹配器若返回 true，一道题就会恒定满分 —— 静默地把评测变成噪声
        assert!(!Matcher::new(&[]).matches("随便什么内容"));
    }
}

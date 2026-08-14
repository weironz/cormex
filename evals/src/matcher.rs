//! Matching rules for gold / forbidden.
//!
//! A gold entry counts as hit only when every substring in its group appears in
//! the same fact's `statement`.
//!
//! # Why not by fact id
//!
//! A fact's id is a ULID generated at write time, which cannot be written into
//! a suite. In llm mode even the statement's wording is decided by the model.
//! "These few keywords have to be in the same fact" is the only identification
//! scheme that holds in both seed and llm mode.
//!
//! # Why only the statement
//!
//! `statement` is the only field required to "be readable on its own" (the
//! extraction prompt says so explicitly), and it is also the field used for
//! vectorization and UI display. Folding predicate / object into the match too
//! would loosen the hit condition considerably — the looser the gold, the more
//! inflated the score, and the more useless the eval.

use crate::suite::Question;

/// One gold (or forbidden) entry: a group of substrings that must all appear
/// together.
#[derive(Debug, Clone)]
pub struct Matcher {
    /// Already lowercased. CJK is unaffected; English identifiers (RustFS /
    /// rustfs) need it
    parts: Vec<String>,
    /// Kept verbatim, for printing in the report
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

    /// A hit requires every substring to appear.
    #[must_use]
    pub fn matches(&self, statement: &str) -> bool {
        if self.parts.is_empty() {
            return false;
        }
        let hay = statement.to_lowercase();
        self.parts.iter().all(|p| hay.contains(p))
    }

    /// Find the rank (zero-based) of the **first** hit in an already-sorted
    /// candidate list.
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

/// All the matchers for one question.
#[derive(Debug, Clone)]
pub struct QuestionMatchers {
    pub gold: Vec<Matcher>,
    pub forbidden: Vec<Matcher>,
    /// "Should not be in the store at all" — compared against the corpus
    /// snapshot rather than the retrieval results; see
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
            "one keyword short must not count as a hit, otherwise the gold is \
             meaningless"
        );
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(
            m(&["rustfs"]).matches("对象存储用 RustFS"),
            "the case of an English identifier must not decide whether it hits"
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
        // If an empty matcher returned true, a question would score full marks
        // unconditionally — silently turning the eval into noise
        assert!(!Matcher::new(&[]).matches("随便什么内容"));
    }
}

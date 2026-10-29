//! Chinese word segmentation — the prerequisite for the BM25 recall channel.
//!
//! PostgreSQL has no built-in Chinese segmentation: `to_tsvector('simple',
//! '记忆系统设计')` treats the whole phrase as a single token, which knocks the
//! BM25 channel out of the four-way recall entirely.
//!
//! Segmenting on the **Rust side** rather than via the `zhparser` / `pg_jieba`
//! extensions is a deliberate choice: it removes any dependency on the database
//! being able to install extensions. That costs nothing while we self-host, but
//! if we ever move to managed PG (RDS / Supabase / Neon) those extensions almost
//! certainly won't be installable, and migrating at that point would be brutal.
//!
//! The output is written to each table's `tsv` column, and **must go in the same
//! transaction as the main row** — never backfilled asynchronously. A backfill is
//! an UPDATE, and since it never reaches `sync_log` it stays permanently
//! invisible to every other device.

use std::sync::OnceLock;

use jieba_rs::Jieba;

fn jieba() -> &'static Jieba {
    static JIEBA: OnceLock<Jieba> = OnceLock::new();
    JIEBA.get_or_init(Jieba::new)
}

/// Cut text into a space-separated token string for `to_tsvector('simple', $1)`.
///
/// Uses search-engine mode (`cut_for_search`): on top of the long tokens it also
/// emits the shorter ones inside them, trading precision for recall — in a
/// retrieval setting it is better to recall too much and let fusion ranking push
/// the noise back down.
///
/// English and digits pass through unchanged (jieba leaves them alone), so mixed
/// Chinese/English text can take this same path.
#[must_use]
pub fn to_tsvector_input(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let tokens = jieba().cut_for_search(text, true);
    let mut out = String::with_capacity(text.len() + tokens.len());
    for tok in tokens {
        let t = tok.word.trim();
        // Drop pure whitespace and lone punctuation; keep single ASCII
        // letters/digits (they may be variable names)
        if t.is_empty() || (t.chars().count() == 1 && !t.chars().all(char::is_alphanumeric)) {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t);
    }
    out
}

/// Cut a user query into a form `to_tsquery` will accept.
///
/// Joins with `|` (OR) rather than `&` (AND): memory retrieval is "recall as much
/// as possible, then rank". With AND, any moderately long natural-language query
/// returns zero results almost every time.
#[must_use]
pub fn to_tsquery_input(query: &str) -> String {
    let tokens = jieba().cut_for_search(query, true);
    let mut parts: Vec<String> = Vec::new();
    for tok in tokens {
        let t = tok.word.trim();
        if t.is_empty() || (t.chars().count() == 1 && !t.chars().all(char::is_alphanumeric)) {
            continue;
        }
        // tsquery metacharacters must be stripped, or a user typing "a & b"
        // turns into a syntax error
        let escaped: String = t
            .chars()
            .filter(|c| c.is_alphanumeric() || !c.is_ascii())
            .collect();
        if !escaped.is_empty() {
            parts.push(escaped);
        }
    }
    parts.join(" | ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_chinese_into_words() {
        let out = to_tsvector_input("对象存储先用 RustFS");
        // What matters is that 「对象」and「存储」got split apart, instead of the
        // whole phrase staying one token
        assert!(out.contains("对象"), "actual: {out}");
        assert!(out.contains("存储"), "actual: {out}");
        assert!(
            out.contains("RustFS"),
            "an English proper noun should pass through unchanged: {out}"
        );
        assert!(out.contains(' '), "expected a space-separated token string");
    }

    #[test]
    fn keeps_ascii_identifiers_intact() {
        let out = to_tsvector_input("用 pg_advisory_xact_lock 串行化");
        assert!(
            out.contains("pg_advisory_xact_lock") || out.contains("advisory"),
            "an identifier must not be shredded past the point of being searchable: {out}"
        );
    }

    #[test]
    fn query_uses_or_semantics() {
        let q = to_tsquery_input("上次说的存储方案");
        assert!(
            q.contains('|'),
            "a query should be OR-joined, so that long queries don't come back empty: {q}"
        );
        assert!(!q.contains('&'));
    }

    #[test]
    fn query_escapes_tsquery_metacharacters() {
        let q = to_tsquery_input("a & b ! c");
        assert!(
            !q.contains('&'),
            "metacharacters must be stripped, otherwise tsquery raises a syntax error: {q}"
        );
        assert!(!q.contains('!'), "actual: {q}");
    }

    #[test]
    fn empty_input_is_safe() {
        assert_eq!(to_tsvector_input(""), "");
        assert_eq!(to_tsquery_input("   "), "");
    }
}

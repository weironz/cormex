//! 中文分词 —— BM25 那一路召回的前提。
//!
//! PostgreSQL 默认不支持中文分词：`to_tsvector('simple', '记忆系统设计')`
//! 会把整句当作一个词，四路召回里的 BM25 直接失效。
//!
//! 选择在 **Rust 侧**分词而非用 `zhparser` / `pg_jieba` 扩展，理由是不依赖
//! 数据库能装扩展——当前自托管无碍，但将来若迁到托管 PG（RDS / Supabase /
//! Neon）大概率装不了，届时迁移极痛。
//!
//! 产物写入各表的 `tsv` 列，**必须与主行同一事务**，永不异步补写
//! （补写是 UPDATE，且不进 `sync_log` 就对其他设备永久不可见）。

use std::sync::OnceLock;

use jieba_rs::Jieba;

fn jieba() -> &'static Jieba {
    static JIEBA: OnceLock<Jieba> = OnceLock::new();
    JIEBA.get_or_init(Jieba::new)
}

/// 把文本切成空格分隔的词串，供 `to_tsvector('simple', $1)` 使用。
///
/// 用搜索引擎模式（`cut_for_search`）：它在长词基础上额外切出短词，
/// 召回率优先——检索场景下宁可多召回再靠融合排序压下去。
///
/// 英文与数字原样保留（jieba 不会破坏它们），因此中英混排文本可以走同一条路径。
#[must_use]
pub fn to_tsvector_input(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let tokens = jieba().cut_for_search(text, true);
    let mut out = String::with_capacity(text.len() + tokens.len());
    for tok in tokens {
        let t = tok.word.trim();
        // 丢掉纯空白与单个标点；单个 ASCII 字母/数字保留（可能是变量名）
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

/// 把用户查询切成 `to_tsquery` 可用的形式。
///
/// 用 `|`（OR）连接而非 `&`（AND）：记忆检索是"尽量召回再排序"，
/// AND 会让稍长的自然语言查询几乎必然零结果。
#[must_use]
pub fn to_tsquery_input(query: &str) -> String {
    let tokens = jieba().cut_for_search(query, true);
    let mut parts: Vec<String> = Vec::new();
    for tok in tokens {
        let t = tok.word.trim();
        if t.is_empty() || (t.chars().count() == 1 && !t.chars().all(char::is_alphanumeric)) {
            continue;
        }
        // tsquery 的元字符必须转义，否则用户输入 "a & b" 会变成语法错误
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
        // 关键在于「对象」「存储」被切开了，而不是整句一个词
        assert!(out.contains("对象"), "实际：{out}");
        assert!(out.contains("存储"), "实际：{out}");
        assert!(out.contains("RustFS"), "英文专有名词应原样保留：{out}");
        assert!(out.contains(' '), "应当是空格分隔的词串");
    }

    #[test]
    fn keeps_ascii_identifiers_intact() {
        let out = to_tsvector_input("用 pg_advisory_xact_lock 串行化");
        assert!(
            out.contains("pg_advisory_xact_lock") || out.contains("advisory"),
            "标识符不应被打碎到无法检索：{out}"
        );
    }

    #[test]
    fn query_uses_or_semantics() {
        let q = to_tsquery_input("上次说的存储方案");
        assert!(q.contains('|'), "查询应为 OR 连接，避免长查询零结果：{q}");
        assert!(!q.contains('&'));
    }

    #[test]
    fn query_escapes_tsquery_metacharacters() {
        let q = to_tsquery_input("a & b ! c");
        assert!(
            !q.contains('&'),
            "元字符必须被剔除，否则 tsquery 语法错误：{q}"
        );
        assert!(!q.contains('!'), "实际：{q}");
    }

    #[test]
    fn empty_input_is_safe() {
        assert_eq!(to_tsvector_input(""), "");
        assert_eq!(to_tsquery_input("   "), "");
    }
}

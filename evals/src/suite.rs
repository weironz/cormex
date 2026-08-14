//! 题集的数据模型、加载与静态校验。
//!
//! # 为什么是自建题集
//!
//! LoCoMo / LongMemEval 这类公开基准全是英文，且只测「问一句答一句」。
//! Cortex 的差异化能力——jieba + tsvector 的中文 BM25、双时间轴回放、
//! 出处追问、跨 domain 检索——**没有任何公开基准覆盖**。不自建就永远
//! 无法证明卖点成立，也就永远只能凭感觉调 RRF 的 k 和四路的召回宽度。
//!
//! # 为什么用 JSON 而不是 YAML
//!
//! 工作区当前没有任何 YAML crate，为一个题集文件引入一个新依赖（且
//! `serde_yaml` 已停止维护）不划算。JSON 冗长一点，但 `serde_json`
//! 本来就在依赖里。
//!
//! # 格式
//!
//! ```text
//! scenarios[]        铺垫对话。按数组顺序灌进真实 ingest 管线
//!   turns[]          一轮对话
//!     text           原文，落成 episode；llm 模式下就是抽取器的输入
//!     facts[]        seed 模式下手写的候选事实，走真实的消解 + 落库
//!     checkpoint     可选。记下本轮提交后的**系统时间**，供时间回放题引用
//!     tool_calls[]   本轮的工具轨迹，落成 episode_tool_calls（两种模式都落）
//!     tool_expect[]  一个正确的抽取器应当从轨迹里得出的陈述（**不落库**）
//!     tool_avoid[]   绝对不该从轨迹里抽出来的东西（**不落库**）
//! questions[]
//!   gold[]           每个元素是一组子串，全部出现在某条事实的 statement 里才算命中
//!   forbidden[]      同样的匹配规则，但**出现即扣分**
//!   never_extracted[] 同样的匹配规则，但比对的是**语料快照**而非检索结果
//!   as_of            引用某个 checkpoint，走系统时间回放而非四路召回
//! ```
//!
//! # 为什么 tool_expect / tool_avoid 不落库
//!
//! 落了就是自问自答：题目声明「应当抽出来这句」，评测把这句写进库，
//! 然后宣布检索能召回它。它们唯一的用途是给 gold 与 never_extracted
//! 当**锚点** —— 工具经验题的 gold 按定义匹配不到任何 `SeedFact`
//! （匹配得到就说明这道题不用抽取器也能答），但也不能因此就允许它
//! 凭空发明。完整的准入规程见 `evals/README.md`。
//!
//! # gold 为什么按子串匹配而不是按 fact id
//!
//! 事实的 id 是落库时现生成的 ULID，题集写不出来。而 llm 模式下连
//! statement 的措辞都由模型决定。子串匹配（「这几个关键词都得在同一条
//! 事实里」）是唯一在两种模式下都成立的标识方式。

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// 题型。分组出数字的依据 —— 总分会掩盖问题，
/// 「中文语义类 Recall@5 只有 0.3」才是可行动的信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    /// 中文语义：换个说法问同一件事，考验分词与语义相近
    ChineseSemantic,
    /// 专有名词精确匹配：型号、端口、标识符、人名 —— BM25 的主场
    ExactTerm,
    /// 时间回放：「三个月前我以为什么」，走系统时间
    TemporalReplay,
    /// 关系推理：从一个实体跳到它关联的事实 —— 图遍历的主场
    Relational,
    /// 跨域：在 coding 里问 office 的事（或反之）
    CrossDomain,
    /// **工具经验**：答案只存在于工具轨迹里，对话原文一个字都没说。
    ///
    /// 与上面五类的根本差别：前五类考的是**检索**（事实已经在库里，问能不能
    /// 排到前面），这一类考的是**抽取**（那条事实压根还不存在，问抽取器
    /// 能不能从「命令 + 路径 + 成败」里把它造出来）。
    ///
    /// 因此它是这套评测里唯一一个 `mode=seed` 下**必然全挂**的题型 ——
    /// seed 喂的是手写的 `SeedFact`，而工具轨迹的题刻意不写任何 `facts`。
    /// 挂法是 `Status::GoldMissing`（gold 没落库），不是「排太后」。
    /// 见 `docs/memory-content.md` §二②。
    ToolExperience,
    /// **应当召回不到**：库里根本没有答案。
    ///
    /// 这一类必须有。只测召回率会奖励「什么都召回」的退化策略：
    /// 把召回宽度调到无穷大，Recall@k 永远是 1。
    Unanswerable,
}

impl QuestionKind {
    /// 报告里的中文标签。
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ChineseSemantic => "中文语义",
            Self::ExactTerm => "专名精确",
            Self::TemporalReplay => "时间回放",
            Self::Relational => "关系推理",
            Self::CrossDomain => "跨域检索",
            Self::ToolExperience => "工具经验",
            Self::Unanswerable => "应召不到",
        }
    }

    #[must_use]
    pub fn all() -> [Self; 7] {
        [
            Self::ChineseSemantic,
            Self::ExactTerm,
            Self::TemporalReplay,
            Self::Relational,
            Self::CrossDomain,
            Self::ToolExperience,
            Self::Unanswerable,
        ]
    }
}

/// 查询语种。中英双语是刚需：Cortex 的语料天然中英混排
/// （中文叙述里嵌着 `RustFS` / `pg_advisory_xact_lock` 这类标识符）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lang {
    Zh,
    En,
    /// 中英混排 —— 中文提问里带英文标识符
    Mixed,
}

impl Lang {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Zh => "中文",
            Self::En => "英文",
            Self::Mixed => "中英混排",
        }
    }

    #[must_use]
    pub fn all() -> [Self; 3] {
        [Self::Zh, Self::En, Self::Mixed]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suite {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub scenarios: Vec<Scenario>,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub id: String,
    /// coding / office / personal —— 落到 episode 与 fact 的 `domain` 列
    pub domain: String,
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    /// 对话原文。落成 episode；llm 模式下同时是抽取器的输入
    pub text: String,
    /// 事件发生时间 `YYYY-MM-DD`
    pub occurred_at: String,
    /// 记下本轮提交后的系统时间，供 `as_of` 引用
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<String>,
    /// seed 模式下手写的候选事实
    #[serde(default)]
    pub facts: Vec<SeedFact>,
    /// 本轮的工具轨迹，按数组顺序落成 `episode_tool_calls`（`ordinal` 就是下标）。
    ///
    /// 它是**语料**不是**候选事实**：无论 seed 还是 llm 模式都照样落库。
    /// 抽取器现在还不读这张表（`docs/memory-content.md` §二② 那条缺口），
    /// 所以这段 fixture 目前只是「摆在那里等着被读」—— 这正是工具经验题
    /// 现在必然挂掉的原因，也是将来接上之后不用改题集就能量到收益的原因。
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// 一个正确的抽取器**应当**从上面这段轨迹里得出的陈述。
    ///
    /// 它不会被写进库（写了就成了自问自答）。它的唯一用途是给
    /// `tool_experience` 题的 gold 当锚点：完整性检查要求每条 gold
    /// 至少命中一条 `tool_expect`，否则那条 gold 就是凭空发明的。
    #[serde(default)]
    pub tool_expect: Vec<String>,
    /// 从这段轨迹里**绝对不该**被抽出来的东西 —— 通常是命令的原始输出片段。
    ///
    /// 判据见 `docs/memory-content.md` §二③「工具输出原始字节明确不存」。
    /// 同样只作锚点：题目的 `never_extracted` 必须命中其中一条。
    #[serde(default)]
    pub tool_avoid: Vec<String>,
}

/// 一次工具调用的 fixture，与 `cortex_store::NewEpisodeToolCall` 一一对应。
///
/// 只有「命令 + 路径 + 成败 + 一行结论」四样，**没有原始 stdout** ——
/// 形状照抄生产的 `episode_tool_calls`，评测里能表达的东西不能比生产多，
/// 否则测出来的收益是假的。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    /// 这次调用碰的路径；没碰文件的工具留空
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// 这次调用成没成。**「失败→成功」的那个差值就是这一类题要考的知识**
    pub ok: bool,
    /// 一行结论。失败时按生产的做法带上工具输出的首行片段
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeedFact {
    pub subject: String,
    #[serde(default = "default_kind")]
    pub subject_kind: String,
    pub predicate: String,
    /// 字面量宾语。与 `object_entity` 二选一
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<String>,
    /// 实体宾语 —— 这才是知识图谱里的一条边，图遍历召回靠它
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_entity: Option<String>,
    #[serde(default = "default_kind")]
    pub object_entity_kind: String,
    /// 自然语言表述。gold 的子串就是往这里匹配的
    pub statement: String,
    /// 【事件时间】`YYYY-MM-DD`。矛盾消解按它判先后
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_at: Option<String>,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    /// 覆盖场景的 domain（跨域题需要在一个场景里混两个 domain 时用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
}

fn default_kind() -> String {
    "concept".to_owned()
}

fn default_confidence() -> f32 {
    0.9
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Question {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: QuestionKind,
    pub lang: Lang,
    pub query: String,
    /// 当前对话领域，传给检索器做域加权
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// 引用某个 checkpoint —— 走 `retrieve_as_of`（系统时间回放）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
    /// 应当被召回的事实。每个元素是一组子串（AND 关系）
    #[serde(default)]
    pub gold: Vec<Vec<String>>,
    /// **不该**出现在结果里的事实。已被取代的旧值、话题相邻的干扰项
    #[serde(default)]
    pub forbidden: Vec<Vec<String>>,
    /// **不该被抽出来**的东西 —— 与 `forbidden` 是两件事，别混。
    ///
    /// `forbidden` 判的是「检索有没有把它排进前 k」，前提是它**已经在库里**；
    /// `never_extracted` 判的是「它根本就不该在库里」，比对的是**语料快照**
    /// 而不是检索结果。
    ///
    /// 为什么必须分开：抽取 prompt 里「绝对不抽」那一节，改了之后
    /// 检索侧的十八项指标一动不动 —— 因为该不该抽根本不是检索能看见的事。
    /// 这一列是那一节唯一的尺子。
    #[serde(default)]
    pub never_extracted: Vec<Vec<String>>,
    #[serde(default)]
    pub note: String,
}

impl Suite {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("读不到题集文件 {}", path.display()))?;
        let suite: Self = serde_json::from_str(&raw)
            .with_context(|| format!("题集 {} 不是合法的 JSON 或字段对不上", path.display()))?;
        suite.validate()?;
        Ok(suite)
    }

    /// 静态校验。**不需要数据库** —— CI 里可以先跑这一步快速挡住坏题。
    pub fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        let mut checkpoints = HashSet::new();

        for sc in &self.scenarios {
            if !seen.insert(format!("scenario:{}", sc.id)) {
                bail!("场景 id 重复：{}", sc.id);
            }
            if sc.turns.is_empty() {
                bail!("场景 {} 没有任何对话轮次", sc.id);
            }
            for (i, turn) in sc.turns.iter().enumerate() {
                parse_date(&turn.occurred_at).with_context(|| {
                    format!(
                        "场景 {} 第 {} 轮的 occurred_at 不是 YYYY-MM-DD",
                        sc.id,
                        i + 1
                    )
                })?;
                if let Some(cp) = &turn.checkpoint
                    && !checkpoints.insert(cp.clone())
                {
                    bail!("checkpoint 名重复：{cp}");
                }
                for f in &turn.facts {
                    validate_fact(&sc.id, i + 1, f)?;
                }
                validate_tool_trace(&sc.id, i + 1, turn)?;
            }
        }

        if self.questions.is_empty() {
            bail!("题集里一道题都没有");
        }

        for q in &self.questions {
            if !seen.insert(format!("question:{}", q.id)) {
                bail!("题目 id 重复：{}", q.id);
            }
            if q.query.trim().is_empty() {
                bail!("题目 {} 的 query 为空", q.id);
            }
            if let Some(cp) = &q.as_of
                && !checkpoints.contains(cp)
            {
                bail!("题目 {} 引用了不存在的 checkpoint「{cp}」", q.id);
            }
            for group in q
                .gold
                .iter()
                .chain(q.forbidden.iter())
                .chain(q.never_extracted.iter())
            {
                if group.is_empty() || group.iter().any(|s| s.trim().is_empty()) {
                    bail!("题目 {} 的匹配组里有空串 —— 空串匹配一切，等于没写", q.id);
                }
            }
            match q.kind {
                QuestionKind::Unanswerable => {
                    if !q.gold.is_empty() {
                        bail!("题目 {} 标为「应当召回不到」却给了 gold", q.id);
                    }
                    if q.forbidden.is_empty() {
                        bail!(
                            "题目 {} 是「应当召回不到」，必须给 forbidden —— \
                             否则这道题没有任何可判定的内容",
                            q.id
                        );
                    }
                }
                _ => {
                    if q.gold.is_empty() {
                        bail!("题目 {} 没有 gold", q.id);
                    }
                }
            }
            if q.kind == QuestionKind::TemporalReplay && q.as_of.is_none() {
                bail!(
                    "题目 {} 是时间回放题却没有 as_of —— \
                     四路召回走的是当前有效集，回放不到任何东西",
                    q.id
                );
            }
            if q.kind == QuestionKind::ToolExperience && q.as_of.is_some() {
                bail!(
                    "题目 {} 是工具经验题却带了 as_of —— \
                     回放走的是快照内三路，量到的是「当时库里有什么」而不是「抽取器能不能抽出来」",
                    q.id
                );
            }
        }

        // 题型覆盖：少一类就是评测有盲区
        let counts = self.count_by_kind();
        for kind in QuestionKind::all() {
            if counts.get(&kind).copied().unwrap_or(0) == 0 {
                bail!("题型「{}」一道题都没有，评测有盲区", kind.label());
            }
        }

        // 有工具经验题却一条轨迹都没有 = 这一类题目**注定**无解，
        // 而它挂掉的样子（gold 没落库）与「抽取器还没接工具轨迹」一模一样，
        // 从报告里根本分不出是缺 fixture 还是缺实现
        if counts
            .get(&QuestionKind::ToolExperience)
            .copied()
            .unwrap_or(0)
            > 0
            && self.tool_call_count() == 0
        {
            bail!("题集有工具经验题，却没有任何 tool_calls 语料 —— 这类题永远无解");
        }

        Ok(())
    }

    #[must_use]
    pub fn count_by_kind(&self) -> BTreeMap<QuestionKind, usize> {
        let mut out = BTreeMap::new();
        for q in &self.questions {
            *out.entry(q.kind).or_insert(0) += 1;
        }
        out
    }

    #[must_use]
    pub fn count_by_lang(&self) -> BTreeMap<Lang, usize> {
        let mut out = BTreeMap::new();
        for q in &self.questions {
            *out.entry(q.lang).or_insert(0) += 1;
        }
        out
    }

    /// 题集里一共写了多少条铺垫事实（seed 模式下的语料规模）。
    #[must_use]
    pub fn seed_fact_count(&self) -> usize {
        self.scenarios
            .iter()
            .flat_map(|s| &s.turns)
            .map(|t| t.facts.len())
            .sum()
    }

    #[must_use]
    pub fn turn_count(&self) -> usize {
        self.scenarios.iter().map(|s| s.turns.len()).sum()
    }

    /// 题集里一共写了多少次工具调用。
    #[must_use]
    pub fn tool_call_count(&self) -> usize {
        self.turns().map(|t| t.tool_calls.len()).sum()
    }

    /// 全部轮次，按场景顺序。
    pub fn turns(&self) -> impl Iterator<Item = &Turn> {
        self.scenarios.iter().flat_map(|s| &s.turns)
    }
}

/// 工具轨迹的结构校验。长度上限与 `cortex_store` 的 CHECK 对齐 ——
/// 对不上的症状是灌语料灌到一半整个写事务回滚，而报错信息在 Postgres 那头。
fn validate_tool_trace(scenario: &str, turn_idx: usize, turn: &Turn) -> Result<()> {
    for (i, c) in turn.tool_calls.iter().enumerate() {
        let at = format!("场景 {scenario} 第 {turn_idx} 轮第 {} 次调用", i + 1);
        if c.name.trim().is_empty() {
            bail!("{at} 没有工具名");
        }
        if c.name.chars().count() > cortex_store::TOOL_NAME_MAX_CHARS {
            bail!(
                "{at} 的工具名超过 {} 字符，落库会被 CHECK 打回",
                cortex_store::TOOL_NAME_MAX_CHARS
            );
        }
        if c.summary.trim().is_empty() {
            bail!("{at} 没有 summary —— 只剩「跑了个命令」，一点知识都表达不出来");
        }
        if c.summary.chars().count() > cortex_store::TOOL_SUMMARY_MAX_CHARS {
            bail!(
                "{at} 的 summary 超过 {} 字符，落库会被 CHECK 打回",
                cortex_store::TOOL_SUMMARY_MAX_CHARS
            );
        }
        if let Some(p) = &c.path
            && p.chars().count() > cortex_store::TOOL_PATH_MAX_CHARS
        {
            bail!(
                "{at} 的 path 超过 {} 字符，落库会被 CHECK 打回",
                cortex_store::TOOL_PATH_MAX_CHARS
            );
        }
    }

    if turn.tool_calls.is_empty() && !(turn.tool_expect.is_empty() && turn.tool_avoid.is_empty()) {
        bail!(
            "场景 {scenario} 第 {turn_idx} 轮声明了 tool_expect/tool_avoid 却没有 tool_calls —— \
             那这个「应当抽出来」是凭空写的，不是从轨迹里来的"
        );
    }
    for s in turn.tool_expect.iter().chain(turn.tool_avoid.iter()) {
        if s.trim().is_empty() {
            bail!("场景 {scenario} 第 {turn_idx} 轮的 tool_expect/tool_avoid 里有空串");
        }
    }
    Ok(())
}

fn validate_fact(scenario: &str, turn: usize, f: &SeedFact) -> Result<()> {
    match (&f.object, &f.object_entity) {
        (Some(_), None) | (None, Some(_)) => {}
        _ => bail!(
            "场景 {scenario} 第 {turn} 轮的事实「{}」必须且只能给 object 与 object_entity 之一",
            f.statement
        ),
    }
    if f.statement.trim().is_empty() {
        bail!("场景 {scenario} 第 {turn} 轮有事实缺 statement");
    }
    if !(0.0..=1.0).contains(&f.confidence) {
        bail!(
            "场景 {scenario} 第 {turn} 轮的 confidence {} 超出 schema 的 CHECK 范围",
            f.confidence
        );
    }
    if let Some(v) = &f.valid_at {
        parse_date(v)
            .with_context(|| format!("场景 {scenario} 第 {turn} 轮的 valid_at 不是 YYYY-MM-DD"))?;
    }
    Ok(())
}

/// `YYYY-MM-DD` → UTC 零点。
pub fn parse_date(s: &str) -> Result<DateTime<Utc>> {
    let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .with_context(|| format!("「{s}」不是 YYYY-MM-DD"))?;
    d.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc())
        .context("日期转不成时间戳")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Suite {
        let mut questions: Vec<Question> = QuestionKind::all()
            .iter()
            .enumerate()
            .map(|(i, kind)| Question {
                id: format!("q{i}"),
                kind: *kind,
                lang: Lang::Zh,
                query: "问题".into(),
                domain: None,
                as_of: (*kind == QuestionKind::TemporalReplay).then(|| "cp".to_owned()),
                gold: if *kind == QuestionKind::Unanswerable {
                    vec![]
                } else {
                    vec![vec!["甲".into()]]
                },
                forbidden: if *kind == QuestionKind::Unanswerable {
                    vec![vec!["乙".into()]]
                } else {
                    vec![]
                },
                never_extracted: if *kind == QuestionKind::ToolExperience {
                    vec![vec!["原始输出".into()]]
                } else {
                    vec![]
                },
                note: String::new(),
            })
            .collect();
        questions[0].note = "首题".into();

        Suite {
            name: "t".into(),
            version: "1".into(),
            description: String::new(),
            scenarios: vec![Scenario {
                id: "sc".into(),
                domain: "coding".into(),
                turns: vec![Turn {
                    text: "一轮对话".into(),
                    occurred_at: "2026-01-01".into(),
                    checkpoint: Some("cp".into()),
                    facts: vec![SeedFact {
                        subject: "甲".into(),
                        subject_kind: "concept".into(),
                        predicate: "p".into(),
                        object: Some("乙".into()),
                        object_entity: None,
                        object_entity_kind: "concept".into(),
                        statement: "甲的 p 是乙".into(),
                        valid_at: None,
                        confidence: 0.9,
                        domain: None,
                    }],
                    tool_calls: vec![ToolCall {
                        name: "bash".into(),
                        path: Some("Cargo.lock".into()),
                        ok: true,
                        summary: "跑了个命令".into(),
                    }],
                    tool_expect: vec!["应当抽出来的一条".into()],
                    tool_avoid: vec!["原始输出".into()],
                }],
            }],
            questions,
        }
    }

    #[test]
    fn minimal_suite_passes() {
        minimal().validate().expect("最小题集应当合法");
    }

    #[test]
    fn duplicate_question_ids_are_rejected() {
        let mut s = minimal();
        s.questions[1].id = s.questions[0].id.clone();
        let err = s.validate().expect_err("重复 id 应当报错");
        assert!(err.to_string().contains("重复"), "实际错误：{err}");
    }

    #[test]
    fn unanswerable_must_not_carry_gold() {
        let mut s = minimal();
        let i = s
            .questions
            .iter()
            .position(|q| q.kind == QuestionKind::Unanswerable)
            .expect("应有应召不到题");
        s.questions[i].gold = vec![vec!["甲".into()]];
        assert!(
            s.validate().is_err(),
            "「应当召回不到」题带 gold 是自相矛盾的，必须挡住"
        );
    }

    #[test]
    fn missing_kind_coverage_is_rejected() {
        let mut s = minimal();
        s.questions.retain(|q| q.kind != QuestionKind::Unanswerable);
        let err = s.validate().expect_err("缺题型应当报错");
        assert!(
            err.to_string().contains("应召不到"),
            "错误信息要点名缺哪一类：{err}"
        );
    }

    #[test]
    fn temporal_question_without_checkpoint_is_rejected() {
        let mut s = minimal();
        let i = s
            .questions
            .iter()
            .position(|q| q.kind == QuestionKind::TemporalReplay)
            .expect("应有时间回放题");
        s.questions[i].as_of = None;
        assert!(
            s.validate().is_err(),
            "时间回放题没有 as_of 就退化成了普通检索题"
        );
    }

    #[test]
    fn empty_matcher_group_is_rejected() {
        let mut s = minimal();
        s.questions[0].gold = vec![vec!["  ".into()]];
        assert!(s.validate().is_err(), "空串匹配一切，等于这道题恒定满分");
    }

    #[test]
    fn fact_needs_exactly_one_object_form() {
        let mut s = minimal();
        s.scenarios[0].turns[0].facts[0].object_entity = Some("丙".into());
        assert!(
            s.validate().is_err(),
            "字面量宾语与实体宾语同时给，落库时无法判定"
        );
    }

    #[test]
    fn tool_experience_question_without_any_trace_is_rejected() {
        let mut s = minimal();
        s.scenarios[0].turns[0].tool_calls.clear();
        s.scenarios[0].turns[0].tool_expect.clear();
        s.scenarios[0].turns[0].tool_avoid.clear();
        let err = s
            .validate()
            .expect_err("没有轨迹语料时工具经验题应当被挡住");
        assert!(
            err.to_string().contains("tool_calls"),
            "错误要点名缺的是轨迹语料，否则会被误读成「抽取器还没接」：{err}"
        );
    }

    #[test]
    fn tool_expectation_without_a_trace_is_rejected() {
        let mut s = minimal();
        s.scenarios[0].turns[0].tool_calls.clear();
        let err = s
            .validate()
            .expect_err("声明了 tool_expect 却没有轨迹应当报错");
        assert!(
            err.to_string().contains("凭空写的"),
            "「应当抽出来」必须有轨迹作依据：{err}"
        );
    }

    #[test]
    fn tool_call_without_summary_is_rejected() {
        let mut s = minimal();
        s.scenarios[0].turns[0].tool_calls[0].summary = "   ".into();
        assert!(
            s.validate().is_err(),
            "只有命令名没有结论的轨迹，抽取器无从下手，等于没写这段语料"
        );
    }

    #[test]
    fn tool_experience_question_must_not_be_a_replay() {
        let mut s = minimal();
        let i = s
            .questions
            .iter()
            .position(|q| q.kind == QuestionKind::ToolExperience)
            .expect("应有工具经验题");
        s.questions[i].as_of = Some("cp".into());
        assert!(
            s.validate().is_err(),
            "带 as_of 的工具经验题量到的是快照内容，不是抽取能力"
        );
    }

    #[test]
    fn never_extracted_rejects_empty_parts() {
        let mut s = minimal();
        let i = s
            .questions
            .iter()
            .position(|q| q.kind == QuestionKind::ToolExperience)
            .expect("应有工具经验题");
        s.questions[i].never_extracted = vec![vec!["  ".into()]];
        assert!(
            s.validate().is_err(),
            "空串匹配一切 —— 这条「不该抽」会永远判定为泄漏"
        );
    }

    #[test]
    fn parses_dates() {
        assert!(parse_date("2026-08-07").is_ok());
        assert!(parse_date("2026/08/07").is_err());
    }
}

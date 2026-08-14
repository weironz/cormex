//! 工具轨迹 → 候选事实。
//!
//! ```text
//! episode_tool_calls（name / path / ok / ordinal / summary）
//!     │
//!     ├─① 硬信号定位候选   确定性，不问模型
//!     ├─② 剪掉原始输出     「失败：」之后那一截不进事实
//!     └─③ 合成 statement   逐字引用轨迹里已有的字
//! ```
//!
//! # 为什么这一格要另立判据
//!
//! 对话的判据是「下次不知道这条，会不会做错决定？」——它需要模型去权衡。
//! 工具轨迹不一样：**值不值得记，从 `ok` 的真假序列和出现次数上就能算出来**
//! （docs/memory-content.md §二②）。三条硬信号：
//!
//! | 信号 | 怎么算 | 为什么值钱 |
//! |---|---|---|
//! | 失败后成功 | 同一工具连续 `ok=false` 之后出现 `ok=true` | 差异就是知识 |
//! | 同一命令反复出现 | 跨轮次数 ≥ [`TraceRules::habit_threshold`] | 这是这个项目的惯例 |
//! | 用户确认过的高风险操作 | `ok=true` 且摘要里有确认标记 | 它批准过什么 |
//!
//! 「失败后成功」这条来自 Manus 的一手复盘：**保留错误记录不清洗，
//! 失败痕迹是模型改错的依据**。所以那条事实里必须把**试过但没走通的做法**
//! 一起写上，而不是只留最后成功的那一条 —— 只抽成功路径的抽取器答不出
//! 「那条路我们试过了，不通」。
//!
//! # 为什么不把整段轨迹丢给模型自己找
//!
//! 灵活，但三样东西一起丢掉：
//!
//! 1. **可测**。硬信号是纯函数，能在没有数据库也没有模型的情况下断言到位；
//!    交给模型之后，「这段轨迹该不该出事实」就再没有确定性的答案。
//! 2. **可省**。轨迹跟着每一轮走，模型判定就是每轮多一次跨网络调用 ——
//!    而绝大多数轮次的轨迹里一条硬信号都没有，那次调用注定空手而归。
//! 3. **可控**。模型「自由发挥」出来的结论会引入轨迹里根本没有的字，
//!    而这一格的全部价值就在于它逐字来自真实执行过的东西。
//!
//! 代价是**措辞机械**：statement 是拼出来的，不是写出来的。可以接受 ——
//! 检索比对的是 `statement` 的内容而不是它的文采，而这里每一个标识符
//! （命令、参数、环境变量、路径）都逐字保真。
//!
//! # 明确不抽
//!
//! - **单次成功的调用**（读文件、列目录）：没有信息量。这不是价值取向，
//!   是 `summarize()` 的信息论事实 —— 成功时摘要只有「返回 N 行 / M 字符」，
//!   内容压根不在里面。
//! - **命令的原始输出**：失败摘要里「失败：」之后那一截是工具输出的原文首行
//!   （见 `cortex_agent::turn::summarize`），docs/memory-content.md §二③
//!   已定案不存。[`safe_text`] 在这里把它剪掉。
//! - **一路失败到底、没有成功收尾的序列**：还没有结论。
//!   与「尚在讨论中的选项不是事实」同一条道理。
//!
//! # 这一路今天在生产上能产出多少，取决于 `summary` 里有什么
//!
//! 两处上游缺口会直接压低产出，都不在这个 crate 里：
//!
//! 1. **成功的调用不带命令。** `cortex_agent::turn::summarize` 在 `ok` 分支
//!    只输出「返回 N 行 / M 字符」，于是落库的摘要是「shell 返回 12 行」——
//!    [`command_prefix`] 剥出来是空的，惯例那条判据在生产上直接哑火。
//!    正确的修法是给 `episode_tool_calls` 加一列 `command`（新 migration +
//!    改 `cortex-agent`），而不是继续从措辞里抠。
//! 2. **「被用户批准过」没有结构化标记。** 确认回路的结果只活在一轮对话的
//!    内存里（`cortexd::confirm` 明确不落库），`AgentEvent::ToolResult` 也
//!    只带 `name / ok / summary`。这里只能退到 [`CONFIRMATION_MARKERS`]
//!    这个文本启发式。
//!
//! 评测的 fixture 用的是「命令 + 一行中文结论」这种更饱满的摘要，
//! 所以题集上的读数是**上游补齐之后**的样子，不是今天生产的样子。

use chrono::{DateTime, Utc};
use cortex_store::EpisodeToolCall;

use crate::extract::{CandidateFact, EntityRef, ObjectRef};

// ══════════════════════════════════════════════════════════
//  参数
// ══════════════════════════════════════════════════════════

/// 一条命令出现几次才算「惯例」。
///
/// 取 3 而不是 2：两次是巧合，三次才是模式。这个方向是刻意保守的 ——
/// 惯例事实会被反复召回（它按定义就与高频操作相关），误判一条的成本
/// 是每一轮都被它占掉一个注入名额。
pub const DEFAULT_HABIT_THRESHOLD: usize = 3;

/// 一段「失败 → 成功」里最多记几条失败的做法。
///
/// 试了七次才成的序列，前五次通常是同一个错的变体。留最靠近成功的两条：
/// 它们与最终解法的差值最小，也就是「差异即知识」里那个差值最清晰的地方。
pub const DEFAULT_MAX_FAILURES_PER_RUN: usize = 2;

/// 一轮轨迹最多产出多少条候选。
///
/// 判据必须编译成预算（docs/memory-content.md §4.2），否则「重要就存」
/// 在一段几十次调用的轨迹上会一次写进几十条事实，把这一轮的注入窗口吃光。
pub const DEFAULT_MAX_CANDIDATES: usize = 6;

/// 判惯例时回看多少条历史轨迹。
pub const DEFAULT_HISTORY_LIMIT: i64 = 500;

/// 从 statement 里引用一段轨迹文本时的字符上限。
///
/// 不只是省注入预算。**长句本身就是检索噪声源**：`ts_rank` 默认不做长度
/// 归一，词多的文档在任何查询上都更容易蹭到分；散列向量后端上更明显。
/// 实测把这个上限从 160 收到 80，一条一百多字的失败摘要就不再霸占
/// 「买的显卡什么时候到」这种毫不相干的查询的第一名了。
/// 同一个坑上一轮在 `crosslink` 的边上踩过（evals/README.md 的 D 行）。
const QUOTE_MAX_CHARS: usize = 80;

/// 命令前缀的字符上限。超过这个长度的多半不是一条命令，而是一整行日志。
const COMMAND_MAX_CHARS: usize = 120;

/// 轨迹抽取的判据参数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceRules {
    pub habit_threshold: usize,
    pub max_failures_per_run: usize,
    pub max_candidates: usize,
    pub history_limit: i64,
}

impl Default for TraceRules {
    fn default() -> Self {
        Self {
            habit_threshold: DEFAULT_HABIT_THRESHOLD,
            max_failures_per_run: DEFAULT_MAX_FAILURES_PER_RUN,
            max_candidates: DEFAULT_MAX_CANDIDATES,
            history_limit: DEFAULT_HISTORY_LIMIT,
        }
    }
}

// ══════════════════════════════════════════════════════════
//  谓词
// ══════════════════════════════════════════════════════════

/// 「失败几次之后，这个做法才通」。
pub const WORKED_AFTER_FAILURES: &str = "worked_after_failures";
/// 「这条命令在这个位置反复执行」。
pub const RUNS_ROUTINELY_IN: &str = "runs_routinely_in";
/// 「用户显式确认之后执行过这次高风险操作」。
pub const APPROVED_BY_USER: &str = "approved_by_user";

/// 这三个谓词一律**多值**，绝不进 `PredicateRules::functional`。
///
/// 经验是累加的：今天 `--locked` 管用不代表上个月 `--precise` 那次的教训
/// 就此失效。把它们当单值会让新一条经验把旧一条打成 `superseded`，
/// 而失效是 append-only 的不可逆写入。
pub const TRACE_PREDICATES: &[&str] = &[WORKED_AFTER_FAILURES, RUNS_ROUTINELY_IN, APPROVED_BY_USER];

// ══════════════════════════════════════════════════════════
//  文本处理
// ══════════════════════════════════════════════════════════

/// 摘要里「从这里开始是工具输出原文」的分界标记。
///
/// 生产只会产出「失败：」这一种（`cortex_agent::turn::summarize` 里写死的
/// `format!("失败：{}", first_line(&r.content, 120))`）。其余几个是给别的
/// 轨迹生产者留的余地 —— 多剪一点只是少记一句话，少剪一点就是把原始字节
/// 写进 append-only 的库。
const FAILURE_MARKERS: &[&str] = &["失败：", "失败:", "错误：", "错误:", "failed:", "error:"];

/// 摘要里可以安全落进事实的那一截。
///
/// 成功的摘要整条都安全：`summarize()` 在 `ok` 分支只输出行数与字符数，
/// 一个字节的工具输出都不带。失败的摘要则是「命令 … 失败：<输出原文首行>」，
/// 后半截按 docs/memory-content.md §二③ 不能存。
///
/// 找不到标记就整条保留 —— 那说明这条摘要不是生产格式，此时**猜**一个
/// 切点比不切更危险（切错地方会把命令本身也剪掉，而那才是唯一有用的部分）。
#[must_use]
pub fn safe_text(call: &EpisodeToolCall) -> &str {
    if call.ok {
        return call.summary.trim();
    }
    let cut = FAILURE_MARKERS
        .iter()
        .filter_map(|m| find_ignore_ascii_case(&call.summary, m))
        .min();
    let head = cut.map_or(call.summary.as_str(), |i| &call.summary[..i]);
    trim_dangling(head)
}

/// 切在标记处会留下半截连接词（`…重跑 cargo test，仍然` ← 「失败：」被剪掉了）。
/// 把它们一起去掉，剩下的才是一句能独立读懂的话。
fn trim_dangling(text: &str) -> &str {
    const CONNECTIVES: &[&str] = &["仍然", "依然", "还是", "结果", "但是", "但", "却", "并且"];
    let mut s = text.trim();
    loop {
        let cut = s.trim_end_matches(['，', ',', '：', ':', '—', '-', '、', '。', '；', ';']);
        let next = CONNECTIVES
            .iter()
            .find_map(|c| cut.strip_suffix(c))
            .unwrap_or(cut)
            .trim();
        if next == s {
            return s;
        }
        s = next;
    }
}

/// 大小写不敏感的子串位置（只对 ASCII 折叠，中文标记原样比）。
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_ascii() {
        let h = haystack.to_ascii_lowercase();
        let n = needle.to_ascii_lowercase();
        // to_ascii_lowercase 逐字节改写、不改变长度，字节位置可以直接用回原串
        return h.find(&n);
    }
    haystack.find(needle)
}

/// 从摘要里剥出命令行部分。
///
/// 规则只有一条：**取开头连续的 ASCII 可打印字符**。摘要的形状是
/// 「命令 + 中文结论」（`docker compose down --remove-orphans 之后再 just up 通过`），
/// 第一个非 ASCII 字符就是命令结束的地方。
///
/// # 为什么允许自己从 summary 的措辞里抠东西
///
/// migration 20260807000005 明确写过「客户端不必再从 summary 的措辞里正则抠
/// 文件名」，理由是抠错会**静默指向另一个文件**。这里不冲突：
/// 路径有自己的列，我们照用；而**命令根本没有列**，不从摘要里取就等于没有。
/// 抠错的后果也不同 —— 只是 statement 里少半截字，不会把链接指错地方。
///
/// 这仍然是一个 schema 缺口，正确的修法是给 `episode_tool_calls` 加一列
/// `command`，那需要新 migration 与 `cortex-agent` 一起改。
///
/// 开头那个与工具名相同的 token 会被丢掉：生产落库时摘要是
/// `format!("{name} {summary}")`，于是一次成功的 shell 调用长成
/// 「shell 返回 12 行 / 340 字符」—— 剥出来的「shell」不是命令，是工具名。
/// 剥完为空即「这条轨迹没带命令」，硬信号里凡是需要命令的都直接跳过它。
#[must_use]
pub fn command_prefix<'a>(name: &str, text: &'a str) -> Option<&'a str> {
    let end = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii() || c.is_ascii_control())
        .map_or(text.len(), |(i, _)| i);
    let head = text[..end].trim();

    // 只在「工具名后面正好是词边界」时剥掉它 —— 否则 name="ca" 会把
    // `cargo build` 削成 `rgo build`
    let rest = match head.strip_prefix(name) {
        Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => r.trim(),
        _ => head,
    };

    let cmd = rest
        .trim_end_matches(['，', ',', '：', ':', ';', '；'])
        .trim();
    if cmd.is_empty() || cmd.chars().count() > COMMAND_MAX_CHARS {
        return None;
    }
    Some(cmd)
}

/// 摘要里表示「这次是用户点头之后才跑的」的标记。
///
/// **这是一个文本启发式，不是结构化信号 —— 今天没有别的载体。**
/// 确认回路的结果只活在一轮对话的内存里（`cortexd::confirm` 明确不落库），
/// `AgentEvent::ToolResult` 也只带 `name / ok / summary`。要把「被批准过」
/// 变成可判定的列，得在 `cortex-agent` 那一侧把 `Approval` 一路带下来。
pub const CONFIRMATION_MARKERS: &[&str] =
    &["用户确认", "用户已确认", "经用户确认", "user confirmed"];

fn is_user_confirmed(summary: &str) -> bool {
    CONFIRMATION_MARKERS
        .iter()
        .any(|m| find_ignore_ascii_case(summary, m).is_some())
}

/// 这条调用作用在什么上：优先文件路径，其次工具名。
fn scope(call: &EpisodeToolCall) -> &str {
    match call.path.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => p,
        _ => call.name.as_str(),
    }
}

/// 给人读的位置名。`.` 在 statement 里是读不懂的。
fn scope_label(s: &str) -> &str {
    match s {
        "." | "./" => "工作区根目录",
        other => other,
    }
}

/// 一条轨迹事实的主语。
///
/// 优先用命令本身：查询问的是「跑什么才过」，命令名才是那个可被点住的实体。
/// 命令取不到就退到路径，再退到工具名 —— 工具名（`shell` / `bash`）在库里
/// 近乎常量，做主语没有分辨力，所以放在最后。
fn subject_of(call: &EpisodeToolCall) -> EntityRef {
    if let Some(cmd) = command_prefix(&call.name, safe_text(call)) {
        return EntityRef::new(cmd, "command");
    }
    // 路径退化成 `.` / `..` 时不建实体：一个叫「.」的实体谁都消解不了，
    // 它只会把所有在仓库根上跑的东西并成一坨
    match call.path.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() && p != "." && p != "./" && p != ".." => EntityRef::new(p, "file"),
        _ => EntityRef::new(&call.name, "tool"),
    }
}

fn quote(text: &str) -> String {
    if text.chars().count() <= QUOTE_MAX_CHARS {
        return text.to_owned();
    }
    let head: String = text.chars().take(QUOTE_MAX_CHARS).collect();
    format!("{head}…")
}

// ══════════════════════════════════════════════════════════
//  硬信号
// ══════════════════════════════════════════════════════════

/// 一段「连续失败之后成功」。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Recovery<'a> {
    failures: Vec<&'a EpisodeToolCall>,
    success: &'a EpisodeToolCall,
}

/// 在一轮的调用序列里找出全部「失败 → 成功」段。
///
/// 按**工具名**分组而不是按路径：同一件事在三次尝试里往往碰的是不同文件
/// （先改 `Cargo.toml` 再改 `Cargo.lock`），按路径分组会把一段完整的
/// 试错切成互不相干的三截，而知识恰恰在这三截的差值里。
///
/// 一路失败到底、本轮没有成功收尾的，不产出任何东西 —— 那是还没有结论的
/// 过程，不是经验。
fn recoveries<'a>(calls: &'a [EpisodeToolCall]) -> Vec<Recovery<'a>> {
    let mut pending: Vec<(&str, Vec<&'a EpisodeToolCall>)> = Vec::new();
    let mut out = Vec::new();

    for call in calls {
        let slot = pending.iter_mut().find(|(n, _)| *n == call.name.as_str());
        if call.ok {
            if let Some((_, failures)) = slot
                && !failures.is_empty()
            {
                out.push(Recovery {
                    failures: std::mem::take(failures),
                    success: call,
                });
            }
        } else {
            match slot {
                Some((_, failures)) => failures.push(call),
                None => pending.push((call.name.as_str(), vec![call])),
            }
        }
    }
    out
}

/// 惯例分组的键：同一个工具、同一个位置、同一条命令。
///
/// 三样都要：只按工具名分组，生产里的 `shell` 会把所有命令混成一堆；
/// 只按命令分组，「在哪跑的」就丢了，而惯例是与位置绑定的
/// （在仓库根跑 `just ci` 是惯例，在别人的目录里跑不是）。
type HabitKey<'a> = (&'a str, &'a str, &'a str);

fn habit_key<'a>(call: &'a EpisodeToolCall) -> Option<HabitKey<'a>> {
    command_prefix(&call.name, safe_text(call)).map(|cmd| (call.name.as_str(), scope(call), cmd))
}

// ══════════════════════════════════════════════════════════
//  合成候选
// ══════════════════════════════════════════════════════════

/// 从一轮的工具轨迹产出候选事实。
///
/// - `calls`：本轮的调用，按 `ordinal` 升序。
/// - `history`：跨轮的历史调用（**含本轮**），只用来数「这条命令出现过几次」。
/// - `occurred_at`：本轮的发生时间，作为事实的**事件时间**。
///   这不是「用今天的日期充数」—— 命令确实是在这一轮里跑的，
///   这是轨迹少数几个能给出精确事件时间的地方。
///
/// 纯函数：不碰数据库、不碰模型，因此能在没有二者的环境里断言到位。
#[must_use]
pub fn candidates_from_trace(
    calls: &[EpisodeToolCall],
    history: &[EpisodeToolCall],
    domain: Option<&str>,
    occurred_at: DateTime<Utc>,
    rules: &TraceRules,
) -> Vec<CandidateFact> {
    let mut out: Vec<CandidateFact> = Vec::new();
    let mk = |subject: EntityRef, predicate: &str, object: &str, statement: String, conf: f32| {
        CandidateFact {
            subject,
            predicate: predicate.to_owned(),
            object: ObjectRef::Literal(object.to_owned()),
            statement,
            domain: domain.map(str::to_owned),
            valid_at: Some(occurred_at),
            confidence: conf,
        }
    };

    // ── ① 失败后成功 ──
    for rec in recoveries(calls) {
        let scope_of_run = scope(rec.success);
        let label = scope_label(scope_of_run);
        let success_text = safe_text(rec.success);
        let success_cmd = command_prefix(&rec.success.name, success_text);
        let n = rec.failures.len();

        // 试过但没走通的那几条。三道闸门，缺一条写出来就是错的或空的：
        // ① 剥得出命令，否则剩下的只是「它失败了」，没有可复用的内容；
        // ② 成功那条也剥得出命令，否则「差值」的另一端写不出来；
        // ③ 两条命令**不一样** —— 差异即知识，差异为空就没有知识。
        //    同一条命令失败又成功（改完代码重跑）说明变的是别的东西，
        //    此时说「这条路不通」是**错的**。
        let start = rec
            .failures
            .len()
            .saturating_sub(rules.max_failures_per_run);
        let dead_ends: Vec<String> = rec.failures[start..]
            .iter()
            .filter_map(|failed| {
                let text = safe_text(failed);
                let cmd = command_prefix(&failed.name, text)?;
                (Some(cmd) != success_cmd).then(|| format!("「{}」", quote(text)))
            })
            .collect();

        // **一段试错 = 一条事实**，不拆成「成功一条 + 每个死胡同各一条」。
        //
        // 这一条是量出来的，不是写着顺口。同一份题集上试过三种写法
        //（`hash` 后端；「回归面」= 原有 116 题，工具经验那 8 题除外）：
        //
        // | 写法 | 回归面 R@5 | 工具经验 R@∞ |
        // |---|---|---|
        // | 拆开，每条只引命令 | 0.8707 | 0.625 |
        // | 拆开，死胡同那条复述成功摘要 | 0.8621 | 1.000 |
        // | 合成一条（现在这版） | **0.8707** | 0.750 |
        //
        // 拆开的写法必须在每个死胡同里复述一遍成功那条，否则问
        // 「那条路能不能走通」时答案不在手边；而复述就是同一段知识以三份
        // 近乎相同的词面进 BM25，把别的事实挤出注入窗口 —— crosslink 的边
        // 就是这么栽的（evals/README.md 的 D 行）。合成一条同时解决两头：
        // 差值的两端本来就该在一起，而总词面只剩三分之一，回归面一格没掉。
        //
        // 第三行那个 0.750 是 `hash` 桩的读数，不是结论：换成真实语义后端
        // （`fastembed:bge-m3`）同一版是 **1.000**。掉的两道题问的是
        // 「钉版本管不管用」这种与轨迹措辞零词面重合的问法，散列桩上本来
        // 就答不了 —— README 早写过「向量那一路在桩上只是第二条词法通道」。
        //
        // 失败那半句「为什么不行」删不得：**用户是照着症状问的**
        //（「报编码错误」「端口被占」），而症状只写在失败摘要里，
        // 成功摘要里一个字都没有（`summarize()` 成功分支只有行数与字符数）。
        let mut statement = format!(
            "{label}：失败 {n} 次之后，「{}」才通过。",
            quote(success_text)
        );
        if !dead_ends.is_empty() {
            statement.push_str(&format!("试过但不管用的是 {}。", dead_ends.join("、")));
        }
        out.push(mk(
            subject_of(rec.success),
            WORKED_AFTER_FAILURES,
            scope_of_run,
            statement,
            0.85,
        ));
    }

    // ── ② 同一命令反复出现 ──
    //
    // 只为**本轮也做过**的命令下结论：本轮没碰的命令，它的次数与这一轮
    // 没有关系，此刻宣布它是惯例只是把同一条事实在每一轮都重算一遍。
    let mut seen: Vec<HabitKey<'_>> = Vec::new();
    for call in calls {
        let Some(key) = habit_key(call) else { continue };
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);

        let count = history.iter().filter(|h| habit_key(h) == Some(key)).count();
        if count < rules.habit_threshold {
            continue;
        }
        let (_, scope_of_habit, cmd) = key;
        out.push(mk(
            EntityRef::new(cmd, "command"),
            RUNS_ROUTINELY_IN,
            scope_of_habit,
            format!(
                "{}：「{cmd}」反复执行了 {count} 次，是这个项目的惯例。",
                scope_label(scope_of_habit)
            ),
            0.85,
        ));
    }

    // ── ③ 用户显式确认过的高风险操作 ──
    //
    // 这是唯一一条「单次成功也要抽」的信号，因此它与「单次成功的读操作不抽」
    // 不矛盾：判据从来不是「成功就不抽」，是**有没有风险与授权**。
    for call in calls
        .iter()
        .filter(|c| c.ok && is_user_confirmed(&c.summary))
    {
        out.push(mk(
            subject_of(call),
            APPROVED_BY_USER,
            scope(call),
            format!("用户确认后执行过的高风险操作：{}。", quote(safe_text(call))),
            // 硬信号里最硬的一条：有人真的按了那个按钮
            0.95,
        ));
    }

    out.truncate(rules.max_candidates);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 2, 11, 0, 0, 0).unwrap()
    }

    fn call(
        ordinal: i32,
        name: &str,
        path: Option<&str>,
        ok: bool,
        summary: &str,
    ) -> EpisodeToolCall {
        EpisodeToolCall {
            id: format!("call-{ordinal}"),
            episode_id: "ep".into(),
            ordinal,
            name: name.to_owned(),
            path: path.map(str::to_owned),
            summary: summary.to_owned(),
            ok,
            device_id: "test".into(),
            diff: None,
            created_at: at(),
        }
    }

    /// 题集里那段依赖锁的轨迹（evals/suites 的 `sc-tool-lock`）。
    fn lock_trace() -> Vec<EpisodeToolCall> {
        vec![
            call(
                0,
                "bash",
                Some("Cargo.toml"),
                false,
                "cargo test --workspace 失败：error: failed to select a version for sqlx-core……本地解析出的次版本与 CI 不同",
            ),
            call(
                1,
                "bash",
                Some("Cargo.lock"),
                false,
                "cargo update -p sqlx-core --precise 0.9.1 之后重跑 cargo test --workspace，仍然失败：那一步把版本又按回去了",
            ),
            call(
                2,
                "bash",
                Some("Cargo.lock"),
                true,
                "cargo test --workspace --locked 通过：锁住 Cargo.lock 后本地与 CI 解析一致",
            ),
        ]
    }

    fn run(calls: &[EpisodeToolCall]) -> Vec<CandidateFact> {
        candidates_from_trace(calls, calls, Some("coding"), at(), &TraceRules::default())
    }

    // ── 剪掉原始输出 ──────────────────────────────────────

    #[test]
    fn raw_tool_output_never_survives_into_a_statement() {
        let facts = run(&lock_trace());
        for f in &facts {
            assert!(
                !f.statement.contains("failed to select a version"),
                "命令的原始输出不能进事实（memory-content §二③），实际：{}",
                f.statement
            );
        }
        assert!(!facts.is_empty(), "剪掉输出不等于什么都不抽");
    }

    #[test]
    fn safe_text_keeps_success_whole_and_cuts_failure_at_the_marker() {
        let ok = call(
            0,
            "bash",
            None,
            true,
            "cargo test --locked 通过：本地与 CI 一致",
        );
        assert_eq!(
            safe_text(&ok),
            "cargo test --locked 通过：本地与 CI 一致",
            "成功的摘要没有一个字节的工具输出，整条都该留"
        );

        let bad = call(
            1,
            "bash",
            None,
            false,
            "just up 失败：Bind for 0.0.0.0:5442 failed",
        );
        assert_eq!(
            safe_text(&bad),
            "just up",
            "「失败：」之后是输出原文，必须剪掉"
        );
    }

    #[test]
    fn safe_text_keeps_everything_when_there_is_no_marker() {
        // 猜一个切点比不切更危险：切错地方会把命令本身剪掉
        let bad = call(
            0,
            "bash",
            None,
            false,
            "chcp 65001 之后仍不管用，改的是控制台代码页",
        );
        assert_eq!(
            safe_text(&bad),
            "chcp 65001 之后仍不管用，改的是控制台代码页"
        );
    }

    // ── 命令前缀 ──────────────────────────────────────────

    #[test]
    fn command_prefix_stops_at_the_first_non_ascii_char() {
        assert_eq!(
            command_prefix(
                "bash",
                "docker compose down --remove-orphans 之后再 just up 通过"
            ),
            Some("docker compose down --remove-orphans")
        );
        assert_eq!(
            command_prefix(
                "bash",
                "PGOPTIONS=--search_path=x,public sqlx migrate run 通过"
            ),
            Some("PGOPTIONS=--search_path=x,public sqlx migrate run")
        );
    }

    #[test]
    fn command_prefix_drops_the_leading_tool_name() {
        // 生产落库的摘要是 format!("{name} {summary}")，于是一次成功的 shell
        // 调用长成「shell 返回 12 行 / 340 字符」—— 剥出来的只有工具名，
        // 那不是命令。认成命令的下场是「shell 是这个项目的惯例」这种废话事实
        assert_eq!(
            command_prefix("shell", "shell 返回 12 行 / 340 字符"),
            None,
            "只剩工具名就等于这条轨迹没带命令"
        );
        assert_eq!(
            command_prefix("shell", "shell cargo build 通过"),
            Some("cargo build")
        );
    }

    #[test]
    fn command_prefix_is_none_when_the_summary_starts_in_chinese() {
        assert_eq!(
            command_prefix("read", "读取 id.rs 确认标识符的构造方式"),
            None
        );
    }

    // ── 失败后成功 ────────────────────────────────────────

    #[test]
    fn recovery_yields_both_the_fix_and_the_dead_ends() {
        let facts = run(&lock_trace());

        assert!(
            facts.iter().any(|f| f.statement.contains("--locked")),
            "最终生效的那个参数必须留下来，实际：{facts:#?}"
        );
        assert!(
            facts.iter().any(|f| f.statement.contains("cargo update")),
            "被证伪的那条路同样是知识（Manus：保留错误记录不清洗），实际：{facts:#?}"
        );
        assert!(
            facts.iter().any(|f| f.predicate == WORKED_AFTER_FAILURES),
            "应当有一条「失败几次后这么做才通」"
        );
        assert_eq!(
            facts.len(),
            1,
            "一段试错就是一条经验：拆成多条会让同一段知识以多份词面进 BM25，实际 {facts:#?}"
        );
    }

    #[test]
    fn failures_without_a_success_produce_nothing() {
        let calls = vec![
            call(0, "bash", None, false, "cargo test 失败：boom"),
            call(1, "bash", None, false, "cargo test --locked 失败：boom"),
        ];
        assert!(
            run(&calls).is_empty(),
            "一路失败到底是还没有结论的过程，不是经验"
        );
    }

    #[test]
    fn a_lone_successful_read_produces_nothing() {
        let calls = vec![call(
            0,
            "read",
            Some("crates/cortex-core/src/id.rs"),
            true,
            "读取 id.rs 确认标识符的构造方式",
        )];
        assert!(
            run(&calls).is_empty(),
            "单次成功的读操作没有信息量（memory-content §二②「明确不抽」）"
        );
    }

    /// 同一条命令失败又成功 = 变的是别的东西（改完代码重跑），
    /// 此时说「这条路不通」是**错的**：差异为空就没有知识。
    #[test]
    fn the_same_command_failing_then_passing_is_not_a_dead_end() {
        let calls = vec![
            call(
                0,
                "bash",
                Some("."),
                false,
                "just ci 失败：clippy 卡在一处 needless_borrow",
            ),
            call(1, "bash", Some("."), true, "just ci 通过"),
        ];
        let facts = run(&calls);
        assert!(
            !facts[0].statement.contains("试过但不管用"),
            "命令一个字都没变，不存在「这条路不通」这条知识：{}",
            facts[0].statement
        );
    }

    #[test]
    fn recovery_groups_by_tool_not_by_path() {
        // 三次调用碰了三个不同的文件，按路径分组会把一段完整的试错切碎
        let facts = run(&lock_trace());
        assert!(
            facts.iter().any(|f| f.statement.contains("--locked")),
            "同一段试错跨了 Cargo.toml → Cargo.lock，仍应认成一段"
        );
    }

    // ── 惯例 ──────────────────────────────────────────────

    #[test]
    fn a_command_becomes_a_habit_only_at_the_threshold() {
        let this_turn = vec![call(0, "bash", Some("."), true, "just ci 通过")];
        let twice: Vec<_> = (0..2)
            .map(|i| call(i, "bash", Some("."), true, "just ci 通过"))
            .collect();
        assert!(
            run_with_history(&this_turn, &twice).is_empty(),
            "两次是巧合，不该下「这是惯例」的结论"
        );

        let thrice: Vec<_> = (0..3)
            .map(|i| call(i, "bash", Some("."), true, "just ci 通过"))
            .collect();
        let facts = run_with_history(&this_turn, &thrice);
        assert!(
            facts
                .iter()
                .any(|f| f.predicate == RUNS_ROUTINELY_IN && f.statement.contains("just ci")),
            "第三次出现应当把它认成惯例，实际：{facts:#?}"
        );
    }

    #[test]
    fn habit_needs_the_command_to_appear_in_this_very_turn() {
        // 本轮碰都没碰的命令，它的次数与这一轮没有关系
        let other = vec![call(0, "bash", Some("."), true, "cargo build 通过")];
        let history: Vec<_> = (0..5)
            .map(|i| call(i, "bash", Some("."), true, "just ci 通过"))
            .collect();
        let facts = run_with_history(&other, &history);
        assert!(
            !facts.iter().any(|f| f.statement.contains("just ci")),
            "本轮没做的事不该在这一轮被重新宣布一次，实际：{facts:#?}"
        );
    }

    fn run_with_history(
        calls: &[EpisodeToolCall],
        history: &[EpisodeToolCall],
    ) -> Vec<CandidateFact> {
        candidates_from_trace(calls, history, Some("coding"), at(), &TraceRules::default())
    }

    // ── 用户确认过的高风险操作 ────────────────────────────

    #[test]
    fn a_user_confirmed_destructive_call_is_worth_one_fact() {
        let calls = vec![call(
            0,
            "bash",
            Some("cortex_eval_1749000000_4242"),
            true,
            "用户确认后执行 DROP SCHEMA cortex_eval_1749000000_4242 CASCADE，清掉上周崩溃留下的临时 schema",
        )];
        let facts = run(&calls);
        assert_eq!(facts.len(), 1, "实际产出 {} 条：{facts:#?}", facts.len());
        assert_eq!(facts[0].predicate, APPROVED_BY_USER);
        assert!(
            facts[0].statement.contains("DROP SCHEMA"),
            "批准过的到底是什么必须写进去，实际：{}",
            facts[0].statement
        );
    }

    #[test]
    fn a_successful_call_without_a_confirmation_marker_is_not_one() {
        let calls = vec![call(
            0,
            "bash",
            Some("x"),
            true,
            "DROP SCHEMA x CASCADE 通过",
        )];
        assert!(
            run(&calls).is_empty(),
            "没有授权痕迹就只是一次单次成功的调用"
        );
    }

    // ── 预算 ──────────────────────────────────────────────

    #[test]
    fn output_is_capped_by_the_budget() {
        let mut calls = Vec::new();
        for i in 0..20 {
            calls.push(call(
                i * 2,
                "bash",
                Some("x"),
                false,
                &format!("cmd{i} 失败：boom"),
            ));
            calls.push(call(
                i * 2 + 1,
                "bash",
                Some("x"),
                true,
                &format!("cmd{i} --fix 通过"),
            ));
        }
        let facts = run(&calls);
        assert!(
            facts.len() <= DEFAULT_MAX_CANDIDATES,
            "判据必须编译成预算，实际产出 {} 条",
            facts.len()
        );
    }

    #[test]
    fn statements_quote_the_trace_but_stay_bounded() {
        let long = "x".repeat(3000);
        let calls = vec![
            call(0, "bash", Some("p"), false, &format!("cmd 失败：{long}")),
            call(
                1,
                "bash",
                Some("p"),
                true,
                &format!("cmd --fix 通过：{long}"),
            ),
        ];
        for f in run(&calls) {
            assert!(
                f.statement.chars().count() < 600,
                "一条事实会以同样的体积进注入窗口，实际 {} 字符",
                f.statement.chars().count()
            );
        }
    }

    // ── 事实的形状 ────────────────────────────────────────

    #[test]
    fn trace_facts_carry_the_turn_time_as_event_time() {
        let facts = run(&lock_trace());
        assert_eq!(
            facts[0].valid_at,
            Some(at()),
            "命令确实是在这一轮跑的，这是轨迹少数几个有精确事件时间的地方"
        );
    }

    #[test]
    fn trace_facts_are_domain_agnostic_in_wording() {
        // 跨会话复用的前提：statement 里不能出现会话/轮次这类只在当时成立的锚点
        for f in run(&lock_trace()) {
            for banned in ["本轮", "本次会话", "episode", "session"] {
                assert!(
                    !f.statement.contains(banned),
                    "「{banned}」把经验绑死在当时那个会话上：{}",
                    f.statement
                );
            }
        }
    }
}

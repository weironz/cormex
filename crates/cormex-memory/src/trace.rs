//! Tool traces → candidate facts.
//!
//! ```text
//! episode_tool_calls (name / path / ok / ordinal / summary)
//!     │
//!     ├─① locate candidates from hard signals   deterministic; the model is not asked
//!     ├─② cut away the raw output               everything after 「失败：」 stays out of the fact
//!     └─③ synthesize the statement              quoting, word for word, what the trace already contains
//! ```
//!
//! # Why this slot needs its own criterion
//!
//! The criterion for conversation is "if the next turn didn't know this, would it
//! make a wrong decision?" — which requires a model to weigh it up. Tool traces
//! are different: **whether something is worth remembering can be computed from
//! the true/false sequence of `ok` and from occurrence counts**
//! (docs/memory-content.md §2②). Three hard signals:
//!
//! | Signal | How it's computed | Why it's valuable |
//! |---|---|---|
//! | Success after failure | `ok=true` appearing after a run of `ok=false` for the same tool | The difference is the knowledge |
//! | The same command recurring | Cross-turn count ≥ [`TraceRules::habit_threshold`] | This is how this project does things |
//! | A high-risk operation the user confirmed | `ok=true` and a confirmation marker in the summary | What it was authorized to do |
//!
//! "Success after failure" comes from Manus's first-hand retrospective:
//! **keep the error records, don't scrub them; traces of failure are what the
//! model uses to correct itself**. That is why the fact must also record **the
//! approaches that were tried and didn't work**, rather than keeping only the one
//! that finally succeeded — an extractor that captures only the successful path
//! cannot answer "we tried that route, it doesn't work".
//!
//! # Why not just hand the whole trace to the model and let it find things
//!
//! Flexible, but it throws away three things at once:
//!
//! 1. **Testability.** Hard signals are pure functions and can be asserted on
//!    fully without a database or a model; hand it to the model and "should this
//!    trace produce a fact" no longer has a deterministic answer.
//! 2. **Frugality.** Traces come with every turn, so a model judgement means one
//!    extra network round trip per turn — and the overwhelming majority of turns
//!    have no hard signal in their traces at all, so that call is guaranteed to
//!    come back empty-handed.
//! 3. **Control.** Conclusions the model "improvises" introduce words the trace
//!    never contained, and this slot's entire value lies in coming word for word
//!    from something that was actually executed.
//!
//! The cost is **mechanical phrasing**: the statement is assembled, not written.
//! That is acceptable — retrieval matches against the content of `statement`, not
//! its literary quality, and here every identifier (command, flag, environment
//! variable, path) is preserved verbatim.
//!
//! # Explicitly not extracted
//!
//! - **A single successful call** (reading a file, listing a directory): no
//!   information content. This is not a value judgement, it is an
//!   information-theoretic fact about `summarize()` — on success the summary says
//!   only "returned N lines / M characters", and the content isn't in there at
//!   all.
//! - **A command's raw output**: in a failure summary, everything after
//!   「失败：」 is the first line of the tool's raw output (see
//!   `cormex_agent::turn::summarize`), and docs/memory-content.md §2③ already
//!   settled that it is not stored. [`safe_text`] cuts it away here.
//! - **Sequences that fail all the way through with no successful conclusion**:
//!   there is no conclusion yet. Same reasoning as "an option still under
//!   discussion is not a fact".
//!
//! # How much this channel actually produces in production today depends on what
//! is in `summary`
//!
//! Two upstream gaps directly suppress its output, and neither is in this crate:
//!
//! 1. **Successful calls don't carry the command.**
//!    `cormex_agent::turn::summarize` emits only "returned N lines / M
//!    characters" on the `ok` branch, so the persisted summary reads
//!    「shell 返回 12 行」— [`command_prefix`] extracts nothing from it, and the
//!    habit criterion is dead on arrival in production. The correct fix is to add
//!    a `command` column to `episode_tool_calls` (a new migration plus changes to
//!    `cormex-agent`), not to keep picking at the phrasing.
//! 2. **"Approved by the user" has no structured marker.** The result of the
//!    confirmation loop lives only in the memory of a single turn
//!    (`cormexd::confirm` explicitly does not persist it), and
//!    `AgentEvent::ToolResult` carries only `name / ok / summary`. All that is
//!    left here is the text heuristic in [`CONFIRMATION_MARKERS`].
//!
//! The eval fixtures use a fuller kind of summary ("command + one line of Chinese
//! conclusion"), so the readings on the dataset show what this looks like **after
//! the upstream gaps are closed**, not what production looks like today.

use chrono::{DateTime, Utc};
use cormex_store::EpisodeToolCall;

use crate::extract::{CandidateFact, EntityRef, ObjectRef};

// ══════════════════════════════════════════════════════════
//  Parameters
// ══════════════════════════════════════════════════════════

/// How many occurrences a command needs before it counts as a "habit".
///
/// 3 rather than 2: twice is coincidence, three times is a pattern. This
/// direction is deliberately conservative — habit facts get recalled repeatedly
/// (by definition they relate to frequent operations), so the cost of getting one
/// wrong is that it occupies an injection slot on every single turn.
pub const DEFAULT_HABIT_THRESHOLD: usize = 3;

/// How many failed approaches at most get recorded within one "failure →
/// success" run.
///
/// In a sequence that took seven attempts to get right, the first five are
/// usually variants of the same mistake. Keep the two closest to the success:
/// their difference from the final solution is smallest, which is where the delta
/// in "the difference is the knowledge" is at its clearest.
pub const DEFAULT_MAX_FAILURES_PER_RUN: usize = 2;

/// How many candidates one turn's traces may produce at most.
///
/// A criterion must be compiled down into a budget (docs/memory-content.md §4.2),
/// otherwise "store it if it's important", applied to a trace with dozens of
/// calls, writes dozens of facts in one go and eats up that turn's entire
/// injection window.
pub const DEFAULT_MAX_CANDIDATES: usize = 6;

/// How many historical trace entries to look back over when deciding habits.
pub const DEFAULT_HISTORY_LIMIT: i64 = 500;

/// The character limit when quoting a stretch of trace text inside a statement.
///
/// This is not just about saving injection budget. **A long sentence is itself a
/// source of retrieval noise**: `ts_rank` does not normalize for length by
/// default, so a document with more terms picks up score more easily on any
/// query, and it is more pronounced still on the hash vector backend. Measured:
/// tightening this limit from 160 to 80 stopped a hundred-plus-character failure
/// summary from taking first place on a query as unrelated as "when is the
/// graphics card I ordered arriving". The same trap was hit one round earlier on
/// crosslink's edges (row D of evals/README.md).
const QUOTE_MAX_CHARS: usize = 80;

/// The character limit for a command prefix. Anything longer than this is
/// probably not a command but a whole line of log output.
const COMMAND_MAX_CHARS: usize = 120;

/// The parameters governing trace extraction.
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
//  Predicates
// ══════════════════════════════════════════════════════════

/// "This approach only worked after N failures."
pub const WORKED_AFTER_FAILURES: &str = "worked_after_failures";
/// "This command is run repeatedly at this location."
pub const RUNS_ROUTINELY_IN: &str = "runs_routinely_in";
/// "This high-risk operation was performed after the user explicitly confirmed
/// it."
pub const APPROVED_BY_USER: &str = "approved_by_user";

/// These three predicates are **always multi-valued** and must never go into
/// `PredicateRules::functional`.
///
/// Experience accumulates: `--locked` working today does not mean last month's
/// lesson about `--precise` has ceased to hold. Treating them as single-valued
/// would let each new piece of experience mark the previous one `superseded` —
/// and invalidation is an irreversible append-only write.
pub const TRACE_PREDICATES: &[&str] = &[WORKED_AFTER_FAILURES, RUNS_ROUTINELY_IN, APPROVED_BY_USER];

// ══════════════════════════════════════════════════════════
//  Text handling
// ══════════════════════════════════════════════════════════

/// The markers in a summary meaning "the tool's raw output starts here".
///
/// Production only ever emits 「失败：」 (hard-coded in
/// `cormex_agent::turn::summarize` as
/// `format!("失败：{}", first_line(&r.content, 120))`). The others are leeway for
/// other trace producers — cutting a little too much only costs us one sentence,
/// whereas cutting a little too little means writing raw bytes into an
/// append-only database.
const FAILURE_MARKERS: &[&str] = &["失败：", "失败:", "错误：", "错误:", "failed:", "error:"];

/// The portion of a summary that can safely be persisted into a fact.
///
/// A successful summary is safe in its entirety: on the `ok` branch,
/// `summarize()` emits only line and character counts, carrying not one byte of
/// tool output. A failure summary, by contrast, reads
/// 「command … 失败：<first line of raw output>」, and the second half must not be
/// stored, per docs/memory-content.md §2③.
///
/// If no marker is found, the whole thing is kept — that means this summary is
/// not in the production format, and at that point **guessing** a cut point is
/// more dangerous than not cutting at all (cutting in the wrong place would take
/// the command with it, and the command is the only useful part).
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

/// Cutting at the marker leaves a dangling connective behind
/// (`…重跑 cargo test，仍然` ← 「失败：」 was cut off). Removing those too is what
/// leaves a sentence that reads on its own.
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

/// Case-insensitive substring position (folding ASCII only; Chinese markers are
/// compared as-is).
fn find_ignore_ascii_case(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_ascii() {
        let h = haystack.to_ascii_lowercase();
        let n = needle.to_ascii_lowercase();
        // to_ascii_lowercase rewrites byte by byte without changing the length, so
        // the byte position can be used directly against the original string
        return h.find(&n);
    }
    haystack.find(needle)
}

/// Peel the command-line portion out of a summary.
///
/// There is exactly one rule: **take the leading run of printable ASCII
/// characters**. The shape of a summary is "command + a Chinese conclusion"
/// (`docker compose down --remove-orphans 之后再 just up 通过`), so the first
/// non-ASCII character is where the command ends.
///
/// # Why we allow ourselves to pick at the wording of a summary here
///
/// Migration 20260807000005 explicitly stated that "clients must no longer
/// regex file names out of the wording of a summary", on the grounds that picking
/// wrongly would **silently point at a different file**. There is no conflict
/// here: paths have their own column and we use it; whereas **the command has no
/// column at all**, so not taking it from the summary means not having it. The
/// consequences of picking wrongly also differ — we merely lose half a phrase in
/// the statement; no link ends up pointing somewhere wrong.
///
/// This is still a schema gap, and the correct fix is to add a `command` column
/// to `episode_tool_calls`, which requires a new migration alongside changes to
/// `cormex-agent`.
///
/// A leading token identical to the tool name is dropped: what production
/// persists is `format!("{name} {summary}")`, so a successful shell call comes out
/// as 「shell 返回 12 行 / 340 字符」— and the "shell" peeled off the front is not
/// a command, it is the tool name. Peeling down to nothing means "this trace
/// carries no command", and every hard signal that needs a command skips it
/// outright.
#[must_use]
pub fn command_prefix<'a>(name: &str, text: &'a str) -> Option<&'a str> {
    let end = text
        .char_indices()
        .find(|(_, c)| !c.is_ascii() || c.is_ascii_control())
        .map_or(text.len(), |(i, _)| i);
    let head = text[..end].trim();

    // Strip the tool name only when what follows it is exactly a word boundary —
    // otherwise name="ca" would shave `cargo build` down to `rgo build`
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

/// The markers in a summary meaning "this only ran after the user gave the
/// nod".
///
/// **This is a text heuristic, not a structured signal — there is no other
/// carrier today.** The result of the confirmation loop lives only in the memory
/// of a single turn (`cormexd::confirm` explicitly does not persist it), and
/// `AgentEvent::ToolResult` carries only `name / ok / summary`. Turning "was
/// approved" into a column that can actually be tested means threading `Approval`
/// all the way down on the `cormex-agent` side.
pub const CONFIRMATION_MARKERS: &[&str] =
    &["用户确认", "用户已确认", "经用户确认", "user confirmed"];

fn is_user_confirmed(summary: &str) -> bool {
    CONFIRMATION_MARKERS
        .iter()
        .any(|m| find_ignore_ascii_case(summary, m).is_some())
}

/// What this call acted on: the file path if there is one, otherwise the tool
/// name.
fn scope(call: &EpisodeToolCall) -> &str {
    match call.path.as_deref().map(str::trim) {
        Some(p) if !p.is_empty() => p,
        _ => call.name.as_str(),
    }
}

/// A human-readable name for the location. `.` is unreadable inside a statement.
fn scope_label(s: &str) -> &str {
    match s {
        "." | "./" => "工作区根目录",
        other => other,
    }
}

/// The subject of a trace fact.
///
/// The command itself is preferred: queries ask "what do I run to make it pass",
/// and the command name is the entity that can actually be pinned down. If no
/// command can be obtained it falls back to the path, and then to the tool name —
/// tool names (`shell` / `bash`) are near-constant across the database and have no
/// discriminating power as subjects, which is why they come last.
fn subject_of(call: &EpisodeToolCall) -> EntityRef {
    if let Some(cmd) = command_prefix(&call.name, safe_text(call)) {
        return EntityRef::new(cmd, "command");
    }
    // Do not create an entity when the path degenerates to `.` / `..`: nobody can
    // resolve an entity named 「.」, and all it does is lump everything ever run
    // at the repository root into one blob
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
//  Hard signals
// ══════════════════════════════════════════════════════════

/// One stretch of "a run of failures, then a success".
#[derive(Debug, Clone, PartialEq, Eq)]
struct Recovery<'a> {
    failures: Vec<&'a EpisodeToolCall>,
    success: &'a EpisodeToolCall,
}

/// Find every "failure → success" stretch in one turn's call sequence.
///
/// Grouped by **tool name** rather than by path: over three attempts at the same
/// thing the calls often touch different files (edit `Cargo.toml` first, then
/// `Cargo.lock`), and grouping by path would slice one complete
/// trial-and-error run into three unrelated pieces — while the knowledge lives
/// precisely in the differences between those pieces.
///
/// Runs that fail all the way through with no successful conclusion in this turn
/// produce nothing at all — that is a process without a conclusion yet, not
/// experience.
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

/// The grouping key for habits: same tool, same location, same command.
///
/// All three are required: grouping by tool name alone would let production's
/// `shell` blend every command into one heap; grouping by command alone would
/// lose "where it was run", and a habit is bound to its location (running
/// `just ci` at the repository root is a habit; running it inside somebody else's
/// directory is not).
type HabitKey<'a> = (&'a str, &'a str, &'a str);

fn habit_key<'a>(call: &'a EpisodeToolCall) -> Option<HabitKey<'a>> {
    command_prefix(&call.name, safe_text(call)).map(|cmd| (call.name.as_str(), scope(call), cmd))
}

// ══════════════════════════════════════════════════════════
//  Synthesizing candidates
// ══════════════════════════════════════════════════════════

/// Produce candidate facts from one turn's tool traces.
///
/// - `calls`: this turn's calls, ascending by `ordinal`.
/// - `history`: historical calls across turns (**including this one**), used only
///   to count how many times a command has appeared.
/// - `occurred_at`: when this turn happened, used as the fact's **event time**.
///   This is not "passing today's date off as the answer" — the command really did
///   run during this turn, and traces are one of the few places that can supply a
///   precise event time.
///
/// A pure function: it touches neither the database nor a model, so it can be
/// asserted on fully in an environment that has neither.
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

    // ── ① Success after failure ──
    for rec in recoveries(calls) {
        let scope_of_run = scope(rec.success);
        let label = scope_label(scope_of_run);
        let success_text = safe_text(rec.success);
        let success_cmd = command_prefix(&rec.success.name, success_text);
        let n = rec.failures.len();

        // The approaches that were tried and didn't work. Three gates, and dropping
        // any one of them makes what gets written either wrong or empty:
        // ① a command can be peeled out, otherwise all that remains is "it
        //    failed", with nothing reusable in it;
        // ② a command can be peeled out of the successful call too, otherwise the
        //    other end of the "difference" cannot be written down;
        // ③ the two commands **differ** — the difference is the knowledge, and an
        //    empty difference is no knowledge at all. The same command failing and
        //    then succeeding (re-running after editing the code) means what changed
        //    was something else, and saying "that route doesn't work" there is
        //    **wrong**.
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

        // **One trial-and-error run = one fact**, not split into "one for the
        // success plus one per dead end".
        //
        // This was measured, not chosen because it reads nicely. Three phrasings
        // were tried on the same dataset (`hash` backend; "regression surface" =
        // the original 116 questions, excluding the 8 tool-experience ones):
        //
        // | Phrasing | Regression surface R@5 | Tool experience R@∞ |
        // |---|---|---|
        // | Split, each entry quoting only the command | 0.8707 | 0.625 |
        // | Split, with the dead-end entries restating the success summary | 0.8621 | 1.000 |
        // | Merged into one (this version) | **0.8707** | 0.750 |
        //
        // The split phrasing has to restate the successful call inside every dead
        // end, otherwise the answer isn't at hand when someone asks "does that
        // route work"; and restating means the same piece of knowledge enters BM25
        // as three near-identical surface forms, crowding other facts out of the
        // injection window — which is exactly how crosslink's edges came to grief
        // (row D of evals/README.md). Merging into one solves both ends at once:
        // the two ends of the difference belonged together anyway, the total
        // surface form drops to a third, and the regression surface doesn't lose a
        // single cell.
        //
        // The 0.750 on the third row is a reading from the `hash` stub, not a
        // conclusion: on a real semantic backend (`fastembed:bge-m3`) the same
        // version scores **1.000**. The two questions it loses ask things like
        // "does pinning the version help", phrasings with zero surface-form overlap
        // with the trace wording, which the hash stub cannot answer in the first
        // place — the README said long ago that "on the stub, the vector channel is
        // just a second lexical channel".
        //
        // The "why it didn't work" half of the failure must not be deleted:
        // **users ask in terms of the symptom** (「报编码错误」, 「端口被占」),
        // and the symptom is written only in the failure summary — the success
        // summary doesn't contain a word of it (`summarize()`'s success branch has
        // only line and character counts).
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

    // ── ② The same command recurring ──
    //
    // Conclusions are drawn only for commands **this turn also ran**: for a
    // command this turn never touched, its count has nothing to do with this turn,
    // and declaring it a habit right now merely recomputes the same fact on every
    // single turn.
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

    // ── ③ High-risk operations the user explicitly confirmed ──
    //
    // This is the one signal where a single success still gets extracted, which is
    // why it does not contradict "a single successful read is not extracted": the
    // criterion was never "success means don't extract", it is **whether there is
    // risk and authorization**.
    for call in calls
        .iter()
        .filter(|c| c.ok && is_user_confirmed(&c.summary))
    {
        out.push(mk(
            subject_of(call),
            APPROVED_BY_USER,
            scope(call),
            format!("用户确认后执行过的高风险操作：{}。", quote(safe_text(call))),
            // The hardest of the hard signals: somebody actually pressed that
            // button
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

    /// The dependency-lock trace from the dataset (`sc-tool-lock` in
    /// evals/suites).
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

    // ── Cutting away the raw output ────────────────────────

    #[test]
    fn raw_tool_output_never_survives_into_a_statement() {
        let facts = run(&lock_trace());
        for f in &facts {
            assert!(
                !f.statement.contains("failed to select a version"),
                "a command's raw output must not enter a fact (memory-content §2③), actual: {}",
                f.statement
            );
        }
        assert!(
            !facts.is_empty(),
            "cutting away the output is not the same as extracting nothing"
        );
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
            "a success summary carries not one byte of tool output, so the whole thing should be kept"
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
            "everything after 「失败：」 is raw output and must be cut away"
        );
    }

    #[test]
    fn safe_text_keeps_everything_when_there_is_no_marker() {
        // Guessing a cut point is more dangerous than not cutting: cutting in the
        // wrong place takes the command itself with it
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

    // ── Command prefix ─────────────────────────────────────

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
        // What production persists is format!("{name} {summary}"), so a successful
        // shell call comes out as 「shell 返回 12 行 / 340 字符」— all that gets
        // peeled off is the tool name, which is not a command. Accepting it as one
        // yields vacuous facts like "shell is this project's habit"
        assert_eq!(
            command_prefix("shell", "shell 返回 12 行 / 340 字符"),
            None,
            "nothing but the tool name left means this trace carries no command"
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

    // ── Success after failure ──────────────────────────────

    #[test]
    fn recovery_yields_both_the_fix_and_the_dead_ends() {
        let facts = run(&lock_trace());

        assert!(
            facts.iter().any(|f| f.statement.contains("--locked")),
            "the flag that finally worked must be kept, actual: {facts:#?}"
        );
        assert!(
            facts.iter().any(|f| f.statement.contains("cargo update")),
            "the route that was ruled out is knowledge too (Manus: keep the error records, don't scrub them), actual: {facts:#?}"
        );
        assert!(
            facts.iter().any(|f| f.predicate == WORKED_AFTER_FAILURES),
            "there should be one fact saying 'this only worked after N failures'"
        );
        assert_eq!(
            facts.len(),
            1,
            "one trial-and-error run is one piece of experience: splitting it makes the same knowledge enter BM25 as several surface forms, actual {facts:#?}"
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
            "failing all the way through is a process without a conclusion yet, not experience"
        );
    }

    #[test]
    fn a_lone_successful_read_produces_nothing() {
        let calls = vec![call(
            0,
            "read",
            Some("crates/cormex-core/src/id.rs"),
            true,
            "读取 id.rs 确认标识符的构造方式",
        )];
        assert!(
            run(&calls).is_empty(),
            "a single successful read carries no information (memory-content §2②, 'explicitly not extracted')"
        );
    }

    /// The same command failing and then succeeding = what changed was something
    /// else (re-running after editing the code), and saying "that route doesn't
    /// work" there is **wrong**: an empty difference is no knowledge.
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
            "not a single character of the command changed, so there is no 'that route doesn't work' knowledge here: {}",
            facts[0].statement
        );
    }

    #[test]
    fn recovery_groups_by_tool_not_by_path() {
        // The three calls touch three different files, and grouping by path would
        // shred one complete trial-and-error run
        let facts = run(&lock_trace());
        assert!(
            facts.iter().any(|f| f.statement.contains("--locked")),
            "the same run spans Cargo.toml → Cargo.lock and should still be recognized as one"
        );
    }

    // ── Habits ─────────────────────────────────────────────

    #[test]
    fn a_command_becomes_a_habit_only_at_the_threshold() {
        let this_turn = vec![call(0, "bash", Some("."), true, "just ci 通过")];
        let twice: Vec<_> = (0..2)
            .map(|i| call(i, "bash", Some("."), true, "just ci 通过"))
            .collect();
        assert!(
            run_with_history(&this_turn, &twice).is_empty(),
            "twice is coincidence; no 'this is a habit' conclusion should be drawn"
        );

        let thrice: Vec<_> = (0..3)
            .map(|i| call(i, "bash", Some("."), true, "just ci 通过"))
            .collect();
        let facts = run_with_history(&this_turn, &thrice);
        assert!(
            facts
                .iter()
                .any(|f| f.predicate == RUNS_ROUTINELY_IN && f.statement.contains("just ci")),
            "the third occurrence should be recognized as a habit, actual: {facts:#?}"
        );
    }

    #[test]
    fn habit_needs_the_command_to_appear_in_this_very_turn() {
        // For a command this turn never touched, its count has nothing to do with
        // this turn
        let other = vec![call(0, "bash", Some("."), true, "cargo build 通过")];
        let history: Vec<_> = (0..5)
            .map(|i| call(i, "bash", Some("."), true, "just ci 通过"))
            .collect();
        let facts = run_with_history(&other, &history);
        assert!(
            !facts.iter().any(|f| f.statement.contains("just ci")),
            "something this turn did not do should not be re-declared during this turn, actual: {facts:#?}"
        );
    }

    fn run_with_history(
        calls: &[EpisodeToolCall],
        history: &[EpisodeToolCall],
    ) -> Vec<CandidateFact> {
        candidates_from_trace(calls, history, Some("coding"), at(), &TraceRules::default())
    }

    // ── High-risk operations the user confirmed ────────────

    #[test]
    fn a_user_confirmed_destructive_call_is_worth_one_fact() {
        let calls = vec![call(
            0,
            "bash",
            Some("cormex_eval_1749000000_4242"),
            true,
            "用户确认后执行 DROP SCHEMA cormex_eval_1749000000_4242 CASCADE，清掉上周崩溃留下的临时 schema",
        )];
        let facts = run(&calls);
        assert_eq!(
            facts.len(),
            1,
            "actually produced {} entries: {facts:#?}",
            facts.len()
        );
        assert_eq!(facts[0].predicate, APPROVED_BY_USER);
        assert!(
            facts[0].statement.contains("DROP SCHEMA"),
            "what exactly was approved must be written down, actual: {}",
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
            "with no trace of authorization it is just one more single successful call"
        );
    }

    // ── Budget ─────────────────────────────────────────────

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
            "the criterion must be compiled down into a budget, actually produced {} entries",
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
                "a fact enters the injection window at exactly this size, actual {} characters",
                f.statement.chars().count()
            );
        }
    }

    // ── The shape of a fact ────────────────────────────────

    #[test]
    fn trace_facts_carry_the_turn_time_as_event_time() {
        let facts = run(&lock_trace());
        assert_eq!(
            facts[0].valid_at,
            Some(at()),
            "the command really did run during this turn, and traces are one of the few places with a precise event time"
        );
    }

    #[test]
    fn trace_facts_are_domain_agnostic_in_wording() {
        // The precondition for reuse across sessions: a statement must not contain
        // session/turn anchors that only held at the time
        for f in run(&lock_trace()) {
            for banned in ["本轮", "本次会话", "episode", "session"] {
                assert!(
                    !f.statement.contains(banned),
                    "「{banned}」 pins the experience to the session it came from: {}",
                    f.statement
                );
            }
        }
    }
}

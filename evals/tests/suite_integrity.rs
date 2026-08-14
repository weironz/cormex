//! 对仓库内自建题集本身的检查。
//!
//! 题集是评测的地基，地基坏了所有数字都是假的。这些检查**不需要数据库**，
//! 因此可以无条件进 CI —— 它们挡住的是「题目写错了」这一类最容易发生、
//! 又最难从报告里看出来的问题。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use cortex_evals::matcher::Matcher;
use cortex_evals::suite::{Lang, QuestionKind, Suite};

fn suite_path() -> PathBuf {
    // 测试的工作目录是 crate 目录（evals/）
    PathBuf::from("suites/cortex-zh-en-v1.json")
}

fn load() -> Suite {
    Suite::load(&suite_path()).expect("仓库内的题集必须能加载并通过静态校验")
}

/// 题集里全部铺垫事实的 statement —— gold 就是往这些串上匹配的。
fn statements(suite: &Suite) -> Vec<String> {
    suite
        .scenarios
        .iter()
        .flat_map(|s| &s.turns)
        .flat_map(|t| &t.facts)
        .map(|f| f.statement.clone())
        .collect()
}

/// 工具轨迹声明的「一个正确的抽取器应当得出的陈述」。
///
/// 它**不是**铺垫事实，一个字都不会落库。之所以要单独有这个池子：
/// 工具经验题的 gold 按定义匹配不到任何 `SeedFact`（匹配得到就说明
/// 这道题问的是对话里已经说过的事，与工具轨迹无关），
/// 但也不能因此就允许它凭空发明 —— 得有个锚点。
fn tool_expectations(suite: &Suite) -> Vec<String> {
    suite.turns().flat_map(|t| t.tool_expect.clone()).collect()
}

/// 工具轨迹声明的「绝对不该被抽出来的东西」。
fn tool_avoidances(suite: &Suite) -> Vec<String> {
    suite.turns().flat_map(|t| t.tool_avoid.clone()).collect()
}

/// 每一轮的轨迹全文（工具名 + 路径 + 一行结论拼起来）。
fn trace_texts(suite: &Suite) -> Vec<String> {
    suite
        .turns()
        .map(|t| {
            t.tool_calls
                .iter()
                .map(|c| {
                    format!(
                        "{} {} {}",
                        c.name,
                        c.path.clone().unwrap_or_default(),
                        c.summary
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect()
}

#[test]
fn bundled_suite_loads_and_validates() {
    let suite = load();
    assert!(
        suite.questions.len() >= 60,
        "题量少于 60 时分组指标的每一格都只剩个位数，统计上没有意义，实际 {} 题",
        suite.questions.len()
    );
}

#[test]
fn every_gold_matcher_hits_at_least_one_seeded_fact() {
    // seed 模式下 gold 匹配不到任何铺垫事实，这道题就是恒定 0 分的死题 ——
    // 它会把基线数字压低，而且压低的原因看报告根本看不出来
    let suite = load();
    let stmts = statements(&suite);

    let mut orphans = Vec::new();
    for q in &suite.questions {
        // 工具经验题的 gold 按定义匹配不到任何铺垫事实 —— 它要的那条事实
        // 还不存在，得由抽取器从轨迹里造出来。它的锚点检查在
        // `tool_experience_gold_is_anchored_in_a_declared_expectation`
        if q.kind == QuestionKind::ToolExperience {
            continue;
        }
        for group in &q.gold {
            let m = Matcher::new(group);
            if !stmts.iter().any(|s| m.matches(s)) {
                orphans.push(format!("{} → {}", q.id, m.describe()));
            }
        }
    }
    assert!(
        orphans.is_empty(),
        "以下 gold 匹配不到任何铺垫事实（题目写错了，不是检索的问题）：\n{}",
        orphans.join("\n")
    );
}

#[test]
fn every_forbidden_matcher_hits_at_least_one_seeded_fact() {
    // forbidden 匹配不到任何东西 = 这条断言永远为真 = 白写。
    // 「应当召回不到」那一类全靠 forbidden 支撑，形同虚设就等于没测
    let suite = load();
    let stmts = statements(&suite);

    let mut orphans = Vec::new();
    for q in &suite.questions {
        for group in &q.forbidden {
            let m = Matcher::new(group);
            if !stmts.iter().any(|s| m.matches(s)) {
                orphans.push(format!("{} → {}", q.id, m.describe()));
            }
        }
    }
    assert!(
        orphans.is_empty(),
        "以下 forbidden 匹配不到任何铺垫事实，这条断言恒为真：\n{}",
        orphans.join("\n")
    );
}

#[test]
fn gold_and_forbidden_never_overlap() {
    // 同一条事实既是 gold 又是 forbidden，这道题无论如何都判不对
    let suite = load();
    let stmts = statements(&suite);

    for q in &suite.questions {
        for g in &q.gold {
            let gm = Matcher::new(g);
            for f in &q.forbidden {
                let fm = Matcher::new(f);
                let clash = stmts.iter().any(|s| gm.matches(s) && fm.matches(s));
                assert!(
                    !clash,
                    "题目 {} 的 gold「{}」与 forbidden「{}」命中了同一条事实",
                    q.id,
                    gm.describe(),
                    fm.describe()
                );
            }
        }
    }
}

#[test]
fn every_kind_has_enough_questions_to_be_readable() {
    // 分组指标的意义在于「中文语义类 R@5 只有 0.3」这种可行动的结论。
    // 一类只有两三题时，一道题的抖动就是 30 个百分点
    let suite = load();
    for (kind, n) in suite.count_by_kind() {
        assert!(
            n >= 8,
            "题型「{}」只有 {n} 题，单题抖动会淹没信号",
            kind.label()
        );
    }
}

#[test]
fn suite_is_bilingual() {
    // 自建集的存在理由之一就是公开基准全是英文；反过来也不能只有中文
    let suite = load();
    let by_lang = suite.count_by_lang();
    for lang in [Lang::Zh, Lang::En, Lang::Mixed] {
        assert!(
            by_lang.get(&lang).copied().unwrap_or(0) >= 5,
            "语种「{}」题量不足：{:?}",
            lang.label(),
            by_lang
        );
    }
}

#[test]
fn temporal_questions_reference_distinct_checkpoints() {
    // 全部回放题都指向同一个时点的话，测的只是「当前有效集」，不是双时间轴
    let suite = load();
    let used: HashSet<&String> = suite
        .questions
        .iter()
        .filter(|q| q.kind == QuestionKind::TemporalReplay)
        .filter_map(|q| q.as_of.as_ref())
        .collect();
    assert!(
        used.len() >= 3,
        "时间回放题只用了 {} 个 checkpoint，覆盖不到多个认知时点",
        used.len()
    );
}

#[test]
fn superseding_facts_share_subject_and_predicate() {
    // 取代关系靠 (subject, predicate) 撞车 + valid_at 分先后来成立。
    // 谓词不在 `PredicateRules` 的单值表里，或者两条的 valid_at 相同，
    // 取代就不会发生，回放题会静默地全部答成「两条都在」
    let suite = load();
    let mut slots: HashMap<(String, String), Vec<Option<String>>> = HashMap::new();
    for f in suite
        .scenarios
        .iter()
        .flat_map(|s| &s.turns)
        .flat_map(|t| &t.facts)
    {
        slots
            .entry((f.subject.clone(), f.predicate.clone()))
            .or_default()
            .push(f.valid_at.clone());
    }

    // 题集刻意安排的四组取代
    for (subject, predicate) in [
        ("对象存储", "stores_in"),
        ("存储层", "assigned_to"),
        ("v1 发布", "deadline"),
        ("同步协议", "decided_on"),
    ] {
        let key = (subject.to_owned(), predicate.to_owned());
        let dates = slots
            .get(&key)
            .unwrap_or_else(|| panic!("题集里应当有 {subject}/{predicate} 这一组取代"));
        assert_eq!(
            dates.len(),
            2,
            "{subject}/{predicate} 应当正好两条（旧值 + 新值），实际 {}",
            dates.len()
        );
        let mut sorted: Vec<&Option<String>> = dates.iter().collect();
        sorted.sort();
        assert!(
            sorted[0] < sorted[1],
            "{subject}/{predicate} 的两条 valid_at 必须分得出先后，否则只会被打成 flag 而不是取代"
        );
    }
}

#[test]
fn unanswerable_questions_have_tempting_distractors() {
    // 「应当召回不到」题的价值全在干扰项的诱惑力上。
    // 干扰项太少，这一类就退化成「随便问句废话」
    let suite = load();
    for q in &suite.questions {
        if q.kind == QuestionKind::Unanswerable {
            assert!(
                !q.forbidden.is_empty(),
                "题目 {} 没有干扰项，测不出任何东西",
                q.id
            );
        }
    }
}

// ══════════════════════════════════════════════════════════
//  工具经验题的准入规程
//
//  这一类题最容易变成自证：自己写一条「应当抽出来的陈述」，再自己写一条
//  匹配它的 gold，然后宣布评测覆盖了工具轨迹抽取。下面六条是防这个的，
//  规程照 README 里 `relates_to` 那次的做法：先定形状再写字、gold 必须
//  有语料侧的锚点、证伪先行、不许反向筛题。
// ══════════════════════════════════════════════════════════

fn tool_questions(suite: &Suite) -> Vec<&cortex_evals::suite::Question> {
    suite
        .questions
        .iter()
        .filter(|q| q.kind == QuestionKind::ToolExperience)
        .collect()
}

#[test]
fn tool_experience_gold_is_anchored_in_a_declared_expectation() {
    // gold 必须命中某条 `tool_expect`。允许 gold 凭空发明，等于允许
    // 「题目要一个语料里根本没有依据的答案」—— 那样这道题永远挂，
    // 而挂的原因看报告与「抽取器没接轨迹」完全一样，分不出来
    let suite = load();
    let expectations = tool_expectations(&suite);

    let mut orphans = Vec::new();
    for q in tool_questions(&suite) {
        for group in &q.gold {
            let m = Matcher::new(group);
            if !expectations.iter().any(|s| m.matches(s)) {
                orphans.push(format!("{} → {}", q.id, m.describe()));
            }
        }
    }
    assert!(
        orphans.is_empty(),
        "以下工具经验题的 gold 没有对应的 tool_expect，等于凭空发明答案：\n{}",
        orphans.join("\n")
    );
}

#[test]
fn tool_experience_gold_is_absent_from_the_seeded_corpus() {
    // gold 若能命中某条铺垫事实，这道题在 seed 模式下就**已经能答对**了 ——
    // 它量的是「一条手写事实能不能被召回」，而不是「轨迹能不能被抽成事实」。
    // 这是这一类题最隐蔽的失效方式：它会安安静静地过，并且看起来像个好消息
    let suite = load();
    let stmts = statements(&suite);

    for q in tool_questions(&suite) {
        for group in &q.gold {
            let m = Matcher::new(group);
            let hit = stmts.iter().find(|s| m.matches(s));
            assert!(
                hit.is_none(),
                "工具经验题 {} 的 gold「{}」命中了铺垫事实「{}」—— \
                 这道题不用抽取器就能答，量到的不是工具轨迹",
                q.id,
                m.describe(),
                hit.map_or("", String::as_str)
            );
        }
    }
}

#[test]
fn tool_experience_gold_is_absent_from_every_conversation_turn() {
    // 第二条自证通路：结论写在了对话原文里。那样 llm 模式下抽取器读 text
    // 就能得出 gold，轨迹一眼都不用看 —— 这道题就退化成了普通的抽取题。
    // 工具轨迹这一格的全部价值就在于「对话里一个字都没说」
    let suite = load();
    let texts: Vec<&str> = suite.turns().map(|t| t.text.as_str()).collect();

    for q in tool_questions(&suite) {
        for group in &q.gold {
            let m = Matcher::new(group);
            let leak = texts.iter().find(|t| m.matches(t));
            assert!(
                leak.is_none(),
                "工具经验题 {} 的 gold「{}」能直接从对话原文「{}」读出来 —— \
                 那就不需要工具轨迹了",
                q.id,
                m.describe(),
                leak.copied().unwrap_or("")
            );
        }
    }
}

#[test]
fn tool_experience_gold_is_copyable_from_the_trace() {
    // 反过来的约束：gold 的每个片段都必须**逐字**出现在某一轮的轨迹里。
    // 抽取器只能看见 name / path / summary 三样东西，要一个它无从得知的
    // 词，这道题就永远挂 —— 而那是题目的错，不是实现的错。
    //
    // 这条同时解释了为什么工具经验题的 gold 全压在命令、参数、环境变量、
    // 路径这类标识符上：statement 的措辞由模型决定，只有标识符是稳的
    let suite = load();
    let traces = trace_texts(&suite);

    for q in tool_questions(&suite) {
        for group in &q.gold {
            for part in group {
                let needle = part.to_lowercase();
                assert!(
                    traces.iter().any(|t| t.to_lowercase().contains(&needle)),
                    "工具经验题 {} 的 gold 片段「{part}」在任何一段轨迹里都没有逐字出现 —— \
                     抽取器无从得知这个词",
                    q.id
                );
            }
        }
    }
}

#[test]
fn tool_experience_query_never_leaks_its_gold() {
    // 查询里直接出现 gold 的标识符，等于把答案写进了题面。
    // 规程第 1 步「先定形状再写字」的落点
    let suite = load();
    for q in tool_questions(&suite) {
        let query = q.query.to_lowercase();
        for group in &q.gold {
            for part in group {
                assert!(
                    !query.contains(&part.to_lowercase()),
                    "工具经验题 {} 的查询里出现了 gold 片段「{part}」—— 答案写在题面上了",
                    q.id
                );
            }
        }
    }
}

#[test]
fn tool_traces_contain_failure_then_success_sequences() {
    // 「差异即知识」这条判据要成立，语料里必须真有「先失败后成功」的序列。
    // 全是单次成功的轨迹只能考「这条命令跑过」，那和普通召回没有区别
    let suite = load();
    let with_diff = suite
        .turns()
        .filter(|t| t.tool_calls.iter().any(|c| !c.ok) && t.tool_calls.last().is_some_and(|c| c.ok))
        .count();
    assert!(
        with_diff >= 4,
        "只有 {with_diff} 轮轨迹是「失败→成功」的序列。\
         差异即知识是这一类题的核心判据，语料不给差异就无从考起"
    );
}

#[test]
fn never_extracted_is_anchored_and_not_self_contradictory() {
    // 三条一起查，因为它们只有合在一起才挡得住「这条断言恒为真」：
    //   ① 必须命中某条 `tool_avoid`     —— 否则是凭空写的
    //   ② 不得命中任何铺垫事实           —— 否则 seed 模式下恒定判为泄漏
    //   ③ 不得命中任何 `tool_expect`     —— 否则「必须抽」与「绝对不抽」自相矛盾
    let suite = load();
    let avoidances = tool_avoidances(&suite);
    let stmts = statements(&suite);
    let expectations = tool_expectations(&suite);

    let mut any = 0usize;
    for q in &suite.questions {
        for group in &q.never_extracted {
            any += 1;
            let m = Matcher::new(group);
            assert!(
                avoidances.iter().any(|s| m.matches(s)),
                "题目 {} 的 never_extracted「{}」没有对应的 tool_avoid —— 凭空写的断言",
                q.id,
                m.describe()
            );
            let clash = stmts.iter().find(|s| m.matches(s));
            assert!(
                clash.is_none(),
                "题目 {} 的 never_extracted「{}」命中了铺垫事实「{}」—— 这条断言恒定失败",
                q.id,
                m.describe(),
                clash.map_or("", String::as_str)
            );
            let contradiction = expectations.iter().find(|s| m.matches(s));
            assert!(
                contradiction.is_none(),
                "题目 {} 的 never_extracted「{}」命中了 tool_expect「{}」—— \
                 同一条东西既要求抽出来又要求别抽",
                q.id,
                m.describe(),
                contradiction.map_or("", String::as_str)
            );
        }
    }
    assert!(
        any >= 3,
        "只写了 {any} 条「不该被抽出来」的断言。抽取 prompt 里「绝对不抽」那一节\
         没有别的落点，这一列薄了那一节就等于没测"
    );
}

#[test]
fn tool_avoidance_quotes_the_trace_verbatim() {
    // `tool_avoid` 是「抽取器可能会误抽的那段东西」，必须真的出现在轨迹里。
    // 写一段轨迹里没有的话当 avoid，等于给自己发了一张永远通过的许可证
    let suite = load();
    for turn in suite.turns() {
        let trace = turn
            .tool_calls
            .iter()
            .map(|c| c.summary.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            .to_lowercase();
        for a in &turn.tool_avoid {
            assert!(
                trace.contains(&a.to_lowercase()),
                "tool_avoid「{a}」在本轮的任何一条 summary 里都没有逐字出现，\
                 抽取器根本不可能误抽出它"
            );
        }
    }
}

#[test]
fn question_ids_are_sorted_and_dense() {
    // 报告与基线 JSON 要能直接 diff，id 乱序会让 diff 满屏噪声
    let suite = load();
    let ids: Vec<&str> = suite.questions.iter().map(|q| q.id.as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "题目 id 应当按字典序排列，便于比对报告");
}

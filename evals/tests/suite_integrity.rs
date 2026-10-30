//! Checks on the in-repository suite itself.
//!
//! The suite is the eval's foundation, and if the foundation is broken every
//! number is fake. These checks **need no database** and can therefore go into
//! CI unconditionally — what they stop is "the question is written wrong", the
//! most likely class of problem and the hardest to spot from a report.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use cormex_evals::matcher::Matcher;
use cormex_evals::suite::{Lang, QuestionKind, Suite};

fn suite_path() -> PathBuf {
    // A test's working directory is the crate directory (evals/)
    PathBuf::from("suites/cormex-zh-en-v1.json")
}

fn load() -> Suite {
    Suite::load(&suite_path())
        .expect("the in-repository suite must load and pass static validation")
}

/// The statements of every seeded fact in the suite — these are the strings
/// gold matches against.
fn statements(suite: &Suite) -> Vec<String> {
    suite
        .scenarios
        .iter()
        .flat_map(|s| &s.turns)
        .flat_map(|t| &t.facts)
        .map(|f| f.statement.clone())
        .collect()
}

/// The "statements a correct extractor should derive" declared by the tool
/// traces.
///
/// They are **not** seeded facts and not one word of them reaches the store.
/// The reason this pool exists separately: a tool-experience question's gold
/// matches no `SeedFact` by definition (if it did, the question would be asking
/// about something the conversation already said, with nothing to do with the
/// tool trace), but that cannot become license to invent one out of thin air —
/// there has to be an anchor.
fn tool_expectations(suite: &Suite) -> Vec<String> {
    suite.turns().flat_map(|t| t.tool_expect.clone()).collect()
}

/// The "things that must never be extracted" declared by the tool traces.
fn tool_avoidances(suite: &Suite) -> Vec<String> {
    suite.turns().flat_map(|t| t.tool_avoid.clone()).collect()
}

/// The full text of each turn's trace (tool name + path + one-line conclusion,
/// joined).
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
        "with fewer than 60 questions every cell of the grouped metrics falls \
         into single digits and means nothing statistically; got {}",
        suite.questions.len()
    );
}

#[test]
fn every_gold_matcher_hits_at_least_one_seeded_fact() {
    // In seed mode, a gold that matches no seeded fact makes the question a
    // dead one scoring a constant 0 — it drags the baseline numbers down, and
    // the reason it does is invisible in the report
    let suite = load();
    let stmts = statements(&suite);

    let mut orphans = Vec::new();
    for q in &suite.questions {
        // A tool-experience question's gold matches no seeded fact by
        // definition — the fact it wants does not exist yet and has to be
        // constructed by the extractor from the trace. Its anchor check lives
        // in `tool_experience_gold_is_anchored_in_a_declared_expectation`
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
        "the following golds match no seeded fact (the question is written \
         wrong; this is not a retrieval problem):\n{}",
        orphans.join("\n")
    );
}

#[test]
fn every_forbidden_matcher_hits_at_least_one_seeded_fact() {
    // A forbidden that matches nothing = the assertion is always true = wasted.
    // The whole "should retrieve nothing" kind rests on forbidden, and a
    // meaningless one is the same as not testing at all
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
        "the following forbidden entries match no seeded fact, so the assertion \
         is always true:\n{}",
        orphans.join("\n")
    );
}

#[test]
fn gold_and_forbidden_never_overlap() {
    // One fact being both gold and forbidden makes the question impossible to
    // judge correctly under any outcome
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
                    "question {}'s gold \"{}\" and forbidden \"{}\" hit the same fact",
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
    // The point of grouped metrics is an actionable conclusion like "the
    // chinese-semantic kind has R@5 of only 0.3". With two or three questions
    // in a kind, one question's jitter is 30 percentage points
    let suite = load();
    for (kind, n) in suite.count_by_kind() {
        assert!(
            n >= 8,
            "the \"{}\" kind has only {n} questions; single-question jitter \
             would drown the signal",
            kind.label()
        );
    }
}

#[test]
fn suite_is_bilingual() {
    // One reason the home-grown suite exists is that the public benchmarks are
    // entirely English; the converse — being entirely Chinese — is equally
    // unacceptable
    let suite = load();
    let by_lang = suite.count_by_lang();
    for lang in [Lang::Zh, Lang::En, Lang::Mixed] {
        assert!(
            by_lang.get(&lang).copied().unwrap_or(0) >= 5,
            "not enough questions in the \"{}\" language: {:?}",
            lang.label(),
            by_lang
        );
    }
}

#[test]
fn temporal_questions_reference_distinct_checkpoints() {
    // If every replay question pointed at the same moment, what is being tested
    // is only "the currently active set", not the bitemporal axis
    let suite = load();
    let used: HashSet<&String> = suite
        .questions
        .iter()
        .filter(|q| q.kind == QuestionKind::TemporalReplay)
        .filter_map(|q| q.as_of.as_ref())
        .collect();
    assert!(
        used.len() >= 3,
        "the time-replay questions use only {} checkpoints, which cannot cover \
         several moments of knowledge",
        used.len()
    );
}

#[test]
fn superseding_facts_share_subject_and_predicate() {
    // A supersession holds by (subject, predicate) colliding plus valid_at
    // ordering them. If the predicate is not in `PredicateRules`' single-valued
    // table, or the two rows share a valid_at, supersession never happens and
    // every replay question silently answers "both are present"
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

    // The four supersessions the suite deliberately arranges
    for (subject, predicate) in [
        ("对象存储", "stores_in"),
        ("存储层", "assigned_to"),
        ("v1 发布", "deadline"),
        ("同步协议", "decided_on"),
    ] {
        let key = (subject.to_owned(), predicate.to_owned());
        let dates = slots.get(&key).unwrap_or_else(|| {
            panic!("the suite should contain the {subject}/{predicate} supersession")
        });
        assert_eq!(
            dates.len(),
            2,
            "{subject}/{predicate} should have exactly two entries (old value + \
             new value), got {}",
            dates.len()
        );
        let mut sorted: Vec<&Option<String>> = dates.iter().collect();
        sorted.sort();
        assert!(
            sorted[0] < sorted[1],
            "{subject}/{predicate}'s two valid_at values must be orderable, \
             otherwise it is only marked flag rather than superseded"
        );
    }
}

#[test]
fn unanswerable_questions_have_tempting_distractors() {
    // A "should retrieve nothing" question's entire value lies in how tempting
    // its distractors are. Too few distractors and the kind degenerates into
    // "ask any old nonsense"
    let suite = load();
    for q in &suite.questions {
        if q.kind == QuestionKind::Unanswerable {
            assert!(
                !q.forbidden.is_empty(),
                "question {} has no distractors and measures nothing",
                q.id
            );
        }
    }
}

// ══════════════════════════════════════════════════════════
//  Admission rules for tool-experience questions
//
//  This kind is the easiest to turn into self-proof: write a "statement that
//  should be extracted" yourself, then write a gold matching it yourself, and
//  declare that the eval covers tool-trace extraction. The six checks below
//  guard against that, following the same procedure as the `relates_to` round
//  in the README: fix the shape before writing the words, require a
//  corpus-side anchor for every gold, attempt falsification first, and never
//  select questions backwards from the results.
// ══════════════════════════════════════════════════════════

fn tool_questions(suite: &Suite) -> Vec<&cormex_evals::suite::Question> {
    suite
        .questions
        .iter()
        .filter(|q| q.kind == QuestionKind::ToolExperience)
        .collect()
}

#[test]
fn tool_experience_gold_is_anchored_in_a_declared_expectation() {
    // A gold must match some `tool_expect`. Allowing gold to be invented out of
    // thin air is allowing "the question demands an answer with no basis in the
    // corpus at all" — such a question fails forever, and in the report the
    // reason looks identical to "the extractor is not wired to traces",
    // indistinguishable
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
        "the following tool-experience golds have no corresponding tool_expect, \
         which amounts to inventing an answer out of thin air:\n{}",
        orphans.join("\n")
    );
}

#[test]
fn tool_experience_gold_is_absent_from_the_seeded_corpus() {
    // If a gold can match a seeded fact, the question is **already answerable**
    // in seed mode — it measures "can a handwritten fact be retrieved" rather
    // than "can a trace be extracted into a fact". This is the kind's most
    // insidious failure mode: it passes quietly and looks like good news
    let suite = load();
    let stmts = statements(&suite);

    for q in tool_questions(&suite) {
        for group in &q.gold {
            let m = Matcher::new(group);
            let hit = stmts.iter().find(|s| m.matches(s));
            assert!(
                hit.is_none(),
                "tool-experience question {}'s gold \"{}\" hit the seeded fact \
                 \"{}\" — this question is answerable without the extractor, so \
                 what it measures is not the tool trace",
                q.id,
                m.describe(),
                hit.map_or("", String::as_str)
            );
        }
    }
}

#[test]
fn tool_experience_gold_is_absent_from_every_conversation_turn() {
    // The second self-proof route: the conclusion is written into the
    // conversation text. Then in llm mode the extractor derives the gold by
    // reading text without glancing at the trace once — and the question
    // degenerates into an ordinary extraction question. The entire value of the
    // tool-trace cell rests on "the conversation says not one word of it"
    let suite = load();
    let texts: Vec<&str> = suite.turns().map(|t| t.text.as_str()).collect();

    for q in tool_questions(&suite) {
        for group in &q.gold {
            let m = Matcher::new(group);
            let leak = texts.iter().find(|t| m.matches(t));
            assert!(
                leak.is_none(),
                "tool-experience question {}'s gold \"{}\" can be read straight \
                 out of the conversation text \"{}\" — no tool trace is needed \
                 at all",
                q.id,
                m.describe(),
                leak.copied().unwrap_or("")
            );
        }
    }
}

#[test]
fn tool_experience_gold_is_copyable_from_the_trace() {
    // The constraint in the other direction: every fragment of a gold must
    // appear **verbatim** in some turn's trace. The extractor can only see
    // name / path / summary, so demanding a word it has no way of knowing makes
    // the question fail forever — and that is the question's fault, not the
    // implementation's.
    //
    // This also explains why tool-experience golds all rest on identifiers like
    // commands, arguments, environment variables, and paths: a statement's
    // wording is decided by the model, and only identifiers are stable
    let suite = load();
    let traces = trace_texts(&suite);

    for q in tool_questions(&suite) {
        for group in &q.gold {
            for part in group {
                let needle = part.to_lowercase();
                assert!(
                    traces.iter().any(|t| t.to_lowercase().contains(&needle)),
                    "tool-experience question {}'s gold fragment \"{part}\" does \
                     not appear verbatim in any trace — the extractor has no way \
                     of knowing that word",
                    q.id
                );
            }
        }
    }
}

#[test]
fn tool_experience_query_never_leaks_its_gold() {
    // A gold's identifier appearing directly in the query means the answer was
    // written into the question. This is where step 1 of the procedure, "fix
    // the shape before writing the words", lands
    let suite = load();
    for q in tool_questions(&suite) {
        let query = q.query.to_lowercase();
        for group in &q.gold {
            for part in group {
                assert!(
                    !query.contains(&part.to_lowercase()),
                    "tool-experience question {}'s query contains the gold \
                     fragment \"{part}\" — the answer is written into the \
                     question",
                    q.id
                );
            }
        }
    }
}

#[test]
fn tool_traces_contain_failure_then_success_sequences() {
    // For the criterion "the difference is the knowledge" to hold, the corpus
    // must really contain "failed first, then succeeded" sequences. Traces of
    // nothing but single successes can only test "this command has been run",
    // which is no different from ordinary retrieval
    let suite = load();
    let with_diff = suite
        .turns()
        .filter(|t| t.tool_calls.iter().any(|c| !c.ok) && t.tool_calls.last().is_some_and(|c| c.ok))
        .count();
    assert!(
        with_diff >= 4,
        "only {with_diff} turns' traces are failure→success sequences. \"The \
         difference is the knowledge\" is this kind's core criterion, and a \
         corpus with no difference gives it nothing to test"
    );
}

#[test]
fn never_extracted_is_anchored_and_not_self_contradictory() {
    // All three are checked together, because only together do they stop "this
    // assertion is always true":
    //   ① it must match some `tool_avoid`   — otherwise it was written out of thin air
    //   ② it must match no seeded fact      — otherwise seed mode judges it a leak every time
    //   ③ it must match no `tool_expect`    — otherwise "must extract" and "never extract" contradict each other
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
                "question {}'s never_extracted \"{}\" has no corresponding \
                 tool_avoid — an assertion written out of thin air",
                q.id,
                m.describe()
            );
            let clash = stmts.iter().find(|s| m.matches(s));
            assert!(
                clash.is_none(),
                "question {}'s never_extracted \"{}\" hit the seeded fact \
                 \"{}\" — this assertion fails every time",
                q.id,
                m.describe(),
                clash.map_or("", String::as_str)
            );
            let contradiction = expectations.iter().find(|s| m.matches(s));
            assert!(
                contradiction.is_none(),
                "question {}'s never_extracted \"{}\" hit the tool_expect \
                 \"{}\" — the same thing is required to be extracted and \
                 required not to be",
                q.id,
                m.describe(),
                contradiction.map_or("", String::as_str)
            );
        }
    }
    assert!(
        any >= 3,
        "only {any} \"should never be extracted\" assertions are written. The \
         \"never extract this\" section of the extraction prompt has nowhere \
         else to land, so a thin column here means that section is untested"
    );
}

#[test]
fn tool_avoidance_quotes_the_trace_verbatim() {
    // `tool_avoid` is "the passage the extractor might wrongly extract", so it
    // must really appear in the trace. Writing an avoid entry that is nowhere
    // in the trace is issuing yourself a permanent pass
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
                "tool_avoid \"{a}\" does not appear verbatim in any of this \
                 turn's summaries, so the extractor could never wrongly extract \
                 it in the first place"
            );
        }
    }
}

#[test]
fn question_ids_are_sorted_and_dense() {
    // Reports and baseline JSON have to be diffable directly, and out-of-order
    // ids fill the diff with noise
    let suite = load();
    let ids: Vec<&str> = suite.questions.iter().map(|q| q.id.as_str()).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(
        ids, sorted,
        "question ids should be in lexicographic order, so reports can be \
         compared"
    );
}

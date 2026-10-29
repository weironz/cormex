# cormex-evals —— retrieval quality evaluation

Before this crate existed, the k of RRF, the width of each retrieval leg, the domain
weighting coefficients and the budget truncation **could only be tuned by feel**. It does
exactly one thing: give every "optimization" a number to be checked against.

The question set is a **home-grown** private Chinese-English bilingual one. The public
benchmarks (LoCoMo / LongMemEval) are all English, and not one of them covers what makes
Cormex different: Chinese BM25 built on jieba + tsvector, bitemporal replay, cross-domain
retrieval, and "ought to retrieve nothing". Without building our own there is no way to
show the selling points hold up.

---

## How to run it

```bash
# Validate the question set only, no database -- CI's first gate, takes seconds
cargo run -p cormex-evals -- validate

# One run, producing the baseline figures (needs Postgres: run just up first)
cargo run -p cormex-evals -- run

# Write out two reports
cargo run -p cormex-evals -- run --json baseline.json --md baseline.md

# Run only a few questions, to work on a bad case
cargo run -p cormex-evals -- run --only q05 --keep-schema

# A/B: turn domain weighting off and see how much it matters
cargo run -p cormex-evals -- run --ignore-domain

# CI regression gate: exit non-zero on falling below the threshold
cargo run -p cormex-evals -- run --min-recall5 0.75
```

`DATABASE_URL` is read from the `.env` at the repository root. `--mode llm` really calls the
extraction model (needs `DEEPSEEK_API_KEY`), folding extraction quality into the
measurement as well.

**The vector backend follows the same selection logic as production**
(`CORMEX_EMBED_BACKEND`, default `fast`). An early version hard-coded `HashEmbedder` here,
and the price was that the whole first baseline rested on the premise that "the vector leg
is merely a second lexical channel" -- swapping in a real semantic model overturned the
per-leg conclusions wholesale, and a round of tuning had been wasted. On a machine with no
network, set `=hash` explicitly.

### A/B and parameter sweeps

Every knob on the retrieval side has a switch of its own, there to answer "what justifies
this default":

| knob | flag |
|---|---|
| abstain threshold | `--abstain-below 0.02` |
| semantic floor | `--semantic-floor 0.45` / `--no-semantic-floor` |
| shape of time recency | `--recency channel\|decay\|off` (+ `--recency-strength/-half-life`) |
| the episodes leg | `--episodes` / `--no-episodes` |
| single-leg strong-hit bonus | `--peak-bonus 0.04` |
| ordering of time replay | `--replay-chronological` (falls back to the whole-picture version from before the rework) |
| cross-domain link scan | `--cross-link` (off by default) + `--cross-link-judge prefilter\|cheap` |
| knobs of the cross-domain bridge | `--cross-link-min/max-distance` · `--cross-link-degree-cap` · `--cross-link-indexed` |
| domain-penalty exemption for bridges | `--no-bridge-exemption` |

With none of these flags given, **what runs is the production default**, so `run` always
produces the baseline.

Bulk sweeps go through `evals/sweep.py` (one group per invocation, flattening the key
figures into a single table):

```bash
python evals/sweep.py recency          # A/B of the recency shape
python evals/sweep.py episodes         # A/B of the episodes leg
python evals/sweep.py floor:0.4,0.45,0.5,0.55   # sweep a range ad hoc
```

It is deliberately not written in Rust: what `cormex-evals` produces is the report of
**one** run, and pushing "run it N times and compare" inside would saddle the CI path with
a pile of knobs it has no use for.

### It does not pollute the development database

Every run creates a **temporary schema** (`cormex_eval_<timestamp>_<pid>`), runs the
migrations inside it, and ends with `DROP SCHEMA ... CASCADE`. The same approach as
`crates/cormex-store/tests/common/mod.rs`. Schemas left behind by a crash are swept up by
the next run according to age (30 minutes) -- not swept indiscriminately, which would
delete the one a sibling process is using.

`--keep-schema` preserves the scene for manual investigation (the schema name is printed at
the end of the report).

---

## Two modes

| `--mode` | where candidate facts come from | what it is for |
|---|---|---|
| `seed` (default) | candidates hand-written in the question set, through `Extractor::write_candidates` | the corpus is fixed, a hundred runs give the same result, **fit for CI** |
| `llm` | really calls the cheap model, through `Extractor::ingest` | measures extraction quality too, but drifts with the model, unfit as a regression gate |

Both modes **share** entity resolution, contradiction resolution, jieba tokenization,
embedding and persistence through `write_txn` -- the only difference is where the candidate
facts come from. What is being evaluated is the whole chain, not four SQL queries.

### Why there is no third mode (record / replay)

The idea of "record the model output of one real extraction, then replay it
deterministically" was discussed, and **rejected**.

It does not solve the problem it means to solve. The reason the regression gate cannot see
a change to the extraction prompt is that seed mode bypasses the model; and **a tape
freezes the model's output**, so after the prompt changes what plays back is still that old
tape and the gate stays just as flat. Seeing the effect of a prompt change requires
re-recording, and re-recording is a real model call -- the non-determinism comes right back.

The half it does buy is already there anyway: what a tape goes through beyond seed is one
pure function, `parse_candidates` (fault-tolerant parsing of dirty JSON); everything after
it -- resolution, embedding, persistence -- goes through the **same** `write_candidates`
that seed uses. Building a whole tape-management apparatus for one pure function is worse
than writing that function a unit test.

And the price does not stop there: a tape's key is a function of "episode text + prompt +
model version", so changing a single character anywhere in the question set invalidates it
-- and the way it fails is a **green light** (what gets replayed is an old corpus nobody
uses any more). One more deterministic source that lies silently is worse than one missing
gate on prompts.

**What is really missing is not a third mode but a third axis**: assertions on the
extraction side. Every metric of the Recall family has "the facts already in the database"
as its denominator, so they are **structurally** incapable of answering "should this have
been extracted at all". Hence two more columns, computed in both modes:

| reading on the extraction side | where it lands | what it decides |
|---|---|---|
| `gold missing` | last column of the report table / `by_kind[*].gold_missing` | what should have been extracted was not (where the "must be extracted" clause lands) |
| `never_extracted` leakage | the "extraction side" section of the report / `questions[*].leaked` | what should never have been extracted was (where the "must never be extracted" clause lands) |

In seed mode these two columns are trivially true for **conversation** corpora (whatever
seed writes is exactly what the database holds) and carry no information; but for **tool
traces** they are a real criterion -- seed never writes any fact for a tool trace. The
tool-experience questions are therefore the one cell in seed mode that reflects the state
of the extraction side.

---

## The session-retrieval target (P7-2)

```bash
cargo run -p cormex-evals -- validate --sessions
cargo run -p cormex-evals -- run --target sessions
```

A **separate suite and a separate target**, because it measures something else entirely:
`cormex-zh-en-v1` measures the memory side (facts, four recall legs, RRF), while this
measures the archive's two indexes — did `search_conversation` / `search_archive` find the
session somebody was thinking of. They share no table, no index and no ranking, so a number
from one says nothing about the other, and folding them into one report would let a good
memory score hide a broken archive.

Three classes of question, and all three are required:

| class | what it proves |
|---|---|
| only the archive finds it | tool output is indexed and reachable |
| only the conversation finds it | the conversation index is not just the archive |
| nothing should be found | the search is not returning everything |

The third is not a supplement. Recall alone rewards the degenerate strategy of returning
every session, and the `expect: []` questions are the only thing that punishes it — which is
why validation **refuses a suite that has none**.

The scoring differs by class on purpose: a positive question asks "did the right session
come back" (containment — the index is a ranking, and a query sharing a token with three
other sessions legitimately returns four), while a negative one asks "did anything come
back at all" (emptiness).

The corpus is **written, not captured**: hand-written sessions in the shape real ones take.
Real transcripts cannot go in a public repository, and a fixture anonymized until it says
nothing measures nothing. What has to be real is the shape — a tool result whose words
appear nowhere in the conversation is exactly what D4's two indexes exist to separate, and
`the_archive_only_questions_are_really_archive_only` asserts that property rather than
assuming it.

## LongMemEval-S retrieval recall (P7-3)

```bash
# The dataset is 277 MB and MIT-licensed but not ours; it is not in this repo.
curl -L -o longmemeval_s_cleaned.json \
  https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json

cargo run -p cormex-evals -- run --target longmemeval \
  --dataset longmemeval_s_cleaned.json --json lme.json --md lme.md
```

**What this measures, stated plainly: retrieval recall (R@k), not
question-answering accuracy.** For each question, its haystack is loaded into a
fresh schema, the question is used as a search query, and the top-k sessions are
checked for the one the dataset says holds the answer.

That number is **not comparable** to the QA accuracies other systems publish for
LongMemEval, and the report prints that warning above its own table. A system
can score 96% recall and 60% QA accuracy on the same data; the survey in
`docs/research/` records exactly how the two get conflated in public
comparisons, and the rule this project took from it is: say which one, never put
them in one table.

One schema per question, because each question carries its **own** haystack: a
shared schema would let question A's answer session be retrieved for question B.

### Baseline (2026-10-30, `CORMEX_EMBED_BACKEND=hash`)

All 500 questions, BM25 only — the hash embedder has no semantic ability, so
the vector leg contributes nothing:

```
  by type
    knowledge-update           n=78   R@1 74.4%  R@5 98.7%  R@10 98.7%
    multi-session              n=133  R@1 56.4%  R@5 86.5%  R@10 94.0%
    single-session-assistant   n=56   R@1 50.0%  R@5 80.4%  R@10 83.9%
    single-session-preference  n=30   R@1 10.0%  R@5 33.3%  R@10 56.7%
    single-session-user        n=70   R@1 58.6%  R@5 81.4%  R@10 90.0%
    temporal-reasoning         n=133  R@1 52.6%  R@5 89.5%  R@10 96.2%

  overall
    R@1 55.0%  R@5 84.6%  R@10 91.4%
```

**This is a floor, not a claim about quality.** With a real embedder the vector
leg joins the ranking and the numbers move; this run exists so a later one can
be compared against something. The weak cell is `single-session-preference`
(R@5 33.3%): those questions ask about a stated preference, and the session
that holds it uses different words — exactly what a semantic leg is for.

### Two things the dataset does that the loader has to handle

- **Duplicate session ids.** A handful of questions list the same
  `haystack_session_ids` entry twice, with identical content at two dates. The
  archive's unique key is `(agent, native_session_id)`, so the second
  registration returns the same session and its events collide. The loader keeps
  the first — the content is identical, so retrieval cannot differ.
- **Non-string fields.** Some `answer` values are numbers and some turn
  `content` values are lists of parts. Both are read as `Value` and flattened;
  deserializing them as strings fails on a 277 MB file with a line number and no
  hint that one question is the problem.

## Question set format

The question set is a single JSON file (`suites/cormex-zh-en-v1.json`).
Not YAML, because the workspace has no YAML crate at all and pulling in a dependency for
one file does not pay (and `serde_yaml` is no longer maintained).

```jsonc
{
  "name": "...", "version": "...",
  "scenarios": [{
    "id": "sc-storage",
    "domain": "coding",              // lands in the domain column of both episode and fact
    "turns": [{
      "text": "the conversation text...",  // becomes an episode; in llm mode it is the extractor's input
      "occurred_at": "2026-03-02",
      "checkpoint": "ckpt_minio",    // optional: record the transaction time after this turn commits
      "facts": [{
        "subject": "object storage", "subject_kind": "concept",
        "predicate": "stores_in",
        "object_entity": "MinIO",    // either this or "object" (a literal), not both
        "object_entity_kind": "tool",
        "statement": "The first version of Cormex object storage used MinIO.",
        "valid_at": "2026-03-02",    // [Valid time] contradiction resolution decides order by it
        "confidence": 0.9
      }]
    }]
  }],
  "questions": [{
    "id": "q001",
    "type": "chinese_semantic",      // see the table below
    "lang": "zh",                    // zh | en | mixed
    "query": "what did we end up using for object storage?",
    "domain": "coding",              // handed to the retriever for domain weighting
    "as_of": "ckpt_minio",           // optional: replay by transaction time instead of everyday retrieval
    "gold":      [["object storage", "RustFS"]],
    "forbidden": [["first version used MinIO"]],
    "note": "why this question exists"
  }]
}
```

### Fixtures for tool traces

A turn takes three more optional fields, meant for questions of the `tool_experience` kind:

```jsonc
"turns": [{
  "text": "Get the workspace tests passing first, the rest can wait.",  // not one word of the conclusion may appear here
  "occurred_at": "2026-02-11",
  "tool_calls": [                                  // → episode_tool_calls, ordinal is just the index
    { "name": "bash", "path": "Cargo.toml", "ok": false,
      "summary": "cargo test --workspace failed: error: failed to select a version for sqlx-core..." },
    { "name": "bash", "path": "Cargo.lock", "ok": false,
      "summary": "re-ran after cargo update -p sqlx-core --precise 0.9.1, still failing..." },
    { "name": "bash", "path": "Cargo.lock", "ok": true,
      "summary": "cargo test --workspace --locked passes: pinning Cargo.lock makes local and CI resolve alike" }
  ],
  "tool_expect": ["the statement a correct extractor ought to draw from this trace"],
  "tool_avoid":  ["error: failed to select a version for sqlx-core"]
}]
```

**The shape is copied from production's `episode_tool_calls`**: nothing but the command,
the path, success or failure, and a one-line conclusion -- **no raw stdout**. The evaluation
must not be able to express more than production does, or the benefit it measures is fake
(for why the raw bytes are not stored, see `docs/memory-content.md` §2, item ③).

`tool_calls` are **corpus**, not candidate facts: both seed and llm mode persist them just
the same, through the **same `write_txn`** the episode goes through. After a run there is an
assertion that "as many rows exist in the database as the question set declared" -- a trace
swallowed on the write path looks exactly like "the extractor has not been wired to traces
yet" in the report, and without the assertion the two can never be told apart.

`tool_expect` / `tool_avoid` **never reach the database by a single character**; writing
them in would be asking and answering your own question. Their sole purpose is to serve as
anchors for a question's `gold` / `never_extracted` (the procedure is below).

The question side gains one matching field:

```jsonc
{ "id": "qt01", "type": "tool_experience", "lang": "zh", "domain": "coding",
  "query": "that time it passed locally but not in CI -- what was added in the end to line the two up?",
  "gold": [["--locked"]],
  "never_extracted": [["failed to select a version"]] }
```

`never_extracted` and `forbidden` are **not the same thing**, do not conflate them:

- `forbidden` judges "did retrieval rank it into the top k", on the premise that
  **it is already in the database**
- `never_extracted` judges "it should not be in the database at all", and is compared
  against **the corpus snapshot**

The comparison is against all facts, not the valid ones: invalidation only adds a tombstone,
the row is still there and so is the vector (the Ghost Vectors of
`docs/memory-content.md` §8.5). Extracted and then invalidated is not the same as never
extracted.

### Why gold / forbidden are written as substrings

A fact's id is a ULID generated at the moment it is stored, which cannot be written into the
question set; and in `llm` mode even the wording of the statement is decided by the model.
**"All of these keywords have to appear in the `statement` of one and the same fact"** is
the only way of identifying a fact that holds in both modes.

- one `gold` element = one group of substrings, a hit requires **all** of them (AND)
- several `gold` elements = several facts that ought to be retrieved, and Recall is their ratio
- `forbidden` follows the same rule, but **appearing costs points**: superseded old values,
  distractors from a neighbouring topic
- matching is case-insensitive, and matches `statement` only (predicate / object take no
  part -- the looser gold is, the more inflated the score)

### Question types

| `type` | meaning |
|---|---|
| `chinese_semantic` | asking the same thing in different words (semantic rephrasing; the vast majority are Chinese questions) |
| `exact_term` | model numbers, ports, identifiers, personal names, email addresses -- BM25's home ground |
| `temporal_replay` | "what did I believe three months ago", must carry `as_of` |
| `relational` | hopping from one entity to the facts related to it -- graph traversal's home ground |
| `cross_domain` | asking about office matters in a coding context (or the reverse) |
| `tool_experience` | **the answer exists only in the tool trace**, the conversation text says not a word of it -- see below |
| `unanswerable` | **there is no answer in the database at all**; `gold` must be empty and `forbidden` must not be |

`tool_experience` is not on the same scale as the five above it: **the first five test
retrieval** (the fact is already in the database, the question is whether it gets ranked up),
while **this one tests extraction** (the fact does not exist at all, the question is whether
the extractor can construct it out of "command + path + success or failure"). It therefore
**necessarily fails outright** under `mode=seed`, and the way it fails is `GoldMissing`
rather than "ranked too low". That is not bad news; it is the zero point of this ruler.

The `unanswerable` type is not a supplement but a necessity: **measuring recall alone
rewards the degenerate strategy of "retrieve everything"** -- turn the retrieval width up to
infinity and Recall@k is identically 1.

---

## How to add a question

1. Add to `scenarios` if new corpus is needed, otherwise just append to `questions`.
   If a new fact's `predicate` is meant to **supersede** an old value, it has to be one of
   the single-valued predicates in `cormex_memory::extract::DEFAULT_FUNCTIONAL`
   (`decided_on` / `assigned_to` / `deadline` / `stores_in` ...), and the two `valid_at`
   values have to be distinguishable in order -- otherwise it is merely marked `flag` and
   both coexist.
2. Pass static validation with `cargo run -p cormex-evals -- validate`.
3. Pass the question-set integrity checks with `cargo test -p cormex-evals`. They stop:
   - gold / forbidden matching no seeded fact at all (a dead question / a trivially true assertion)
   - gold and forbidden hitting the same fact (a question that can never be judged correctly)
   - the count for some question type dropping below 8 (single-question jitter drowning the signal)
   - the seven admission rules for tool-experience questions (see "How to add a tool-experience question")
4. Look at the single-question result with
   `cargo run -p cormex-evals -- run --only <the new question id>`.

Keep question ids in lexicographic order, so that reports and baseline JSON can be diffed
directly.

### How to add a question that "needs a cross-domain bridge" (q112-q116 came about this way)

Questions of this kind test whether the `relates_to` edges written by
`cormex-memory::crosslink` really do let graph traversal get from one domain to the other.
**Writing your own question and grading it yourself is circular reasoning**, so there is a
fixed procedure:

1. **Settle the shape before writing any words.** The query names the entity on **this**
   side of the bridge (which is how a user would ask anyway), but **not one word of it may
   overlap with gold's statement** -- with an overlap, BM25 or the vector leg would reach it
   on their own, and the question would not be testing the bridge.
2. **gold must be a fact that already exists in the corpus**, and never the statement of the
   `relates_to` edge itself. Both entity names appear inside the edge's statement, so using
   it as gold means letting the question prove itself.
3. **Falsification first**: run once without `--cross-link`, and the question **must fail**.
   If it does not, the existing four legs can already answer it and the question is void.
   This step is an **admission** condition.
4. **No filtering in reverse**: questions that still fail with the bridge on **stay in the
   question set**. Keeping only the ones that "pass once the bridge is on" is writing the
   answer backwards into the question -- that is where circular reasoning would really happen.
5. **Look at the regression surface separately**: new questions drag down the denominator of
   the `cross-domain` cell. Judging whether the bridge has side effects means looking at the
   figures for the group of **the original 111 questions**, not at the total.

The evidence for step 3 is preserved in the `A no bridge` row of the "Baseline for
cross-domain links" table below: R@∞ for the five questions is only 0.400, meaning three of
them **retrieve nothing at all**.

### How to add a "tool experience" question (qt01-qt08 came about this way)

This kind of question is **the easiest to turn into self-proof**: write a "statement that
ought to be extracted" yourself, then write a gold that matches it yourself, then declare
that the evaluation covers tool-trace extraction. The procedure follows the one above, but
because the fact gold points at **does not exist yet**, the anchor has to be driven in
somewhere else:

1. **gold must hit some `tool_expect`**; inventing an answer out of nothing is not allowed.
   (`tool_experience_gold_is_anchored_in_a_declared_expectation`)
2. **gold must not hit any seeded fact.** If it does, the question can already be answered
   in seed mode -- it is measuring "can a hand-written fact be retrieved", not "can a trace
   be turned into a fact".
   **This is the most insidious failure mode of this kind of question: it passes quietly,
   and looks like good news.**
   (`tool_experience_gold_is_absent_from_the_seeded_corpus`)
3. **gold must not be readable out of the `text` of any turn.** Write the conclusion into
   the conversation text and in llm mode the extractor only has to read text, never glancing
   at the trace -- and the entire value of this cell lies in "the conversation says not a
   word of it". (`..._is_absent_from_every_conversation_turn`)
4. **Every fragment of gold has to appear verbatim in some trace.** The extractor sees only
   three things -- name / path / summary -- so demanding a word it has no way of knowing
   makes the question fail forever, and that is **the question's fault, not the
   implementation's**. Corollary: gold is always pinned to identifiers such as commands,
   arguments, environment variables and paths -- the wording of a statement is decided by
   the model, and only identifiers are stable.
   (`tool_experience_gold_is_copyable_from_the_trace`)
5. **No fragment of gold may appear in the query** (as in step 1 for the cross-domain bridge).
6. **`never_extracted` is boxed in from three sides**: it must hit some `tool_avoid`
   (otherwise it was invented), must not hit any seeded fact (otherwise it is judged a leak
   unconditionally), and must not hit any `tool_expect` (otherwise "must be extracted" and
   "must never be extracted" contradict each other). And `tool_avoid` itself must
   **quote the trace verbatim**, or it is a licence to pass forever.
7. **Falsification first**: as things stand, with the extractor not yet wired to tool traces,
   these questions **must all fail**. Passing without any switch being flipped means they are
   measuring something else -- see rules 2 and 3.

The evidence for step 7 (2026-08-07, `mode=seed`, the `hash` backend, `--only qt`):

```
Tool traces: 17 rows of `episode_tool_calls` persisted

| type            | count | scored | R@5 | ... | gold missing |
| tool_experience |     8 |      0 | 0.000 | ... |            8 |

## Question problems (not counted in the retrieval metrics)
- `qt01` [tool_experience] GoldMissing  gold: --locked
- `qt02` [tool_experience] GoldMissing  gold: cargo update
- `qt03` [tool_experience] GoldMissing  gold: PGOPTIONS
- `qt04` [tool_experience] GoldMissing  gold: PYTHONIOENCODING
- `qt05` [tool_experience] GoldMissing  gold: just ci
- `qt06` [tool_experience] GoldMissing  gold: DROP SCHEMA
- `qt07` [tool_experience] GoldMissing  gold: --remove-orphans
- `qt08` [tool_experience] GoldMissing  gold: --locked
```

The "17 rows persisted" line has to be read together with the rest: it proves the reason
they fail is that **nobody reads that table**, and not that the corpus never got in. The two
are indistinguishable in the metrics.

**The positive control is on the static side**: rules 1 and 4 together amount to "if the
extractor did produce the statements in `tool_expect`, these 8 questions would hit by
construction". No further switch for "pretend the extractor is already wired up" was built
-- that would be leaving a cheat mode inside the evaluation, and what it could prove is
already proved by rule 1.

**The eight questions each test one angle** (each of them one row of the criteria table in
`docs/memory-content.md` §2, item ②):

| question | angle | gold |
|---|---|---|
| qt01 | difference is knowledge: three calls differing by one argument | `--locked` |
| qt02 | **a trace of failure is itself knowledge** -- it asks about the road already falsified | `cargo update` |
| qt03 | difference is knowledge: the difference is an environment variable | `PGOPTIONS` |
| qt04 | as above, the cross-platform variety | `PYTHONIOENCODING` |
| qt05 | the same command recurring = this is the convention of this project (the evidence is scattered across three traces) | `just ci` |
| qt06 | a high-risk operation the user explicitly confirmed | `DROP SCHEMA` |
| qt07 | English question × Chinese trace -- forcing gold to be pinned to language-independent identifiers | `--remove-orphans` |
| qt08 | **reuse across sessions**: the same experience, asked with office vocabulary and `domain=office` (domain weighting against it) | `--locked` |

qt02 and qt06 are a pair of counterexample guards: qt02 stops an extractor that "only
extracts the successful path", and qt06 together with qt05's `never_extracted` (that one
successful read operation should produce no fact) shows that **the criterion is not "do not
extract on success", it is "was there risk, and was there authorization"**.

qt08's gold being the same as qt01's is deliberate: the evaluation has no session dimension,
so "across sessions" is operationalized here as "a different vocabulary + a different
domain", and the two questions measure the reachability of one and the same piece of
knowledge through different entrances.

---

## How to read the metrics

| metric | meaning | how to use it |
|---|---|---|
| `R@1 / R@5 / R@10` | how many golds were retrieved within the top k (macro average, each question weighted equally) | R@5 corresponds to the actual injection window and is the primary metric |
| `R@∞` | recall with no rank limit | **the gap from R@5 is the debt owed by ranking**: a large gap means fix the fusion, a small one means fix retrieval |
| `MRR` | the reciprocal rank of the first gold hit | sensitive to the very top of the ranking |
| `mean hit rank` | the average rank at which gold is hit | together with R@∞ it locates "retrieved, but ranked too low" |
| `mis-retrieval@5` | among questions that have `forbidden`, the proportion whose top 5 contains a distractor | **lower is better**. This is the lock against degenerate strategies |
| `mean injected count` | how many memories each question ends up injecting | if the "ought to retrieve nothing" row is not 0, the retriever **cannot abstain** |
| `gold missing` | gold never made it into the database at all | this is a problem of **extraction or of the question**, not of retrieval, and is already excluded from Recall |

For the first five types `gold missing` is an **alarm** (the question is wrong or extraction
is broken); for `tool experience` it is a **reading**: the day that cell goes from 8 to 0 is
the day the extractor gets wired to tool traces. One column, two readings, depending on the
question type.

### Per-leg contribution

"Coverage" = the number of questions where this leg caught a gold in its own raw retrieval
list.
"**Exclusive**" = the number of questions only this leg caught -- **that, and not coverage,
is the basis for whether a leg deserves to stay**. High coverage with zero exclusives means
the leg can be entirely replaced by the others.

"Distractors sent into top 5" attributes noise to a specific channel; without it a
mis-retrieval can only be blamed on "the fusion" as a whole.

**Per-leg diagnostics always report all five legs**, regardless of whether the fusion side
has them enabled -- "what this leg would have caught if it were switched off" is precisely
the number you most want to see during an A/B, and switching it off along with the flag
leaves nothing but a column of zeros.

Time-replay questions go through the three legs **inside the snapshot** (bm25 / vector /
reverse chronological over one and the same as-of set), which query a different body of data
than everyday retrieval; mixed into one table neither set of figures can be read, so they
are still excluded from the per-leg denominator. The quality of their ordering is measured
by the main metrics.

### Grouping is not optional

A total score hides problems. The first baseline's "overall R@5 = 0.78" looked passable, but
it was the average of "exact term 0.92 + Chinese semantic 0.69 + temporal replay 0.42" --
what actually needed fixing was the latter two. Every metric is reported once per question
type and once per language.

---

## Suggested CI setup

```yaml
# The first gate: takes seconds, needs no services at all
- run: cargo run -p cormex-evals -- validate
- run: cargo test -p cormex-evals

# The second gate: needs a Postgres service
- run: cargo run -p cormex-evals --
         run --mode seed
             --json evals-report.json
             --min-recall5 0.75
- uses: actions/upload-artifact@v4
  with: { name: evals-report, path: evals-report.json }
```

`--min-recall5` is an overall threshold. The stricter approach is to store the previous
`evals-report.json` as a baseline and compare `by_kind[*].recall["5"]` type by type --
comparing the total alone lets the gate through when one category has collapsed while the
total barely dipped. `Report::gate_metrics()` already flattens the figures worth comparing
into a flat map like `kind.chinese_semantic.recall@5`, ready to diff as it is.

**Do not put `--mode llm` in the regression gate.** Its results drift with the model version
and would become a random red light. Its place is "run it by hand periodically, to look at
extraction quality".

**`--cross-link --cross-link-judge cheap` likewise does not belong in the regression gate**
-- how many pairs it accepts each night is decided by the model on the spot (four runs over
one and the same corpus accepted 3/4/6/7 pairs respectively).
To run cross-domain linking in CI, the only option is `--cross-link-judge prefilter`, and
that setting measures the **prefilter** rather than the **judgement** -- do not mix the two
up when reading the numbers.

> **After adding questions or changing the fusion, `scripts/evals-baseline.*.json` has to be
> blessed again** (`just evals-bless`). After adding q112-q116, the default configuration's
> `kind.cross_domain.recall@5` drops from 0.900 to 0.667 -- those new questions **were
> supposed to fail** under the default configuration, so it is not a regression, but the
> regression gate cannot tell the two apart and only a human blessing can.

---

## Baseline figures

Reproducible by running it yourself (`cargo run -p cormex-evals -- run`; with no flags at all
that is the production default configuration). 2026-08-07, `mode=seed`,
`embedder=fastembed:gpahal/bge-m3-onnx-int8`, 73 seeded facts / 17 tool traces /
124 questions:

| type | count | scored | R@1 | R@5 | R@10 | R@∞ | MRR | mean hit rank | mis-retr.@5 | mean injected | gold missing |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| all | 124 | 102 | 0.640 | 0.877 | 0.931 | 0.935 | 0.771 | 2.0 | 0.286 | 12.2 | 8 |
| chinese_semantic | 33 | 33 | 0.727 | 0.909 | 0.970 | 0.970 | 0.809 | 1.8 | — | 14.0 | 0 |
| exact_term | 27 | 27 | 0.741 | 0.926 | 0.926 | 0.926 | 0.824 | 1.4 | 0.500 | 9.0 | 0 |
| temporal_replay | 12 | 12 | 0.667 | 0.833 | 0.917 | 0.944 | 0.744 | 2.8 | — | 27.2 | 0 |
| relational | 15 | 15 | 0.483 | 0.967 | 1.000 | 1.000 | 0.867 | 2.0 | — | 8.9 | 0 |
| cross_domain | 15 | 15 | 0.400 | 0.667 | 0.800 | 0.800 | 0.518 | 2.9 | — | 13.1 | 0 |
| **tool experience** | 8 | **0** | — | — | — | — | — | — | — | 0.0 | **8** |
| unanswerable | 14 | 0 | — | — | — | — | — | — | **0.500** | 10.4 | 0 |

Per leg (denominator 90): `vector` coverage 89 / exclusive 2 / R@5 0.911 · `bm25` 81/1/0.733 ·
`episode` 76/0/0.700 (not in the fusion by default) · `graph` 42/0/0.444 ·
`recency` 35/0/0.111 (not in the fusion by default).

Extraction side: 5 questions declare 5 assertions about "what should never be extracted",
and **0** leaked -- in seed mode this is trivially true (seed never writes any fact for a
tool trace), and its value as a zero point only begins once the extractor is wired to traces,
or once `--mode llm` is used.

> **After the tool-experience questions were added, all eighteen retrieval-side metrics did
> not move by a single digit.**
> That is neither a coincidence nor good news: questions marked `GoldMissing` are excluded
> from Recall (per the line above in this README, "extraction's fault should not be charged
> to retrieval"), so these eight questions **occupy not one cell** in the regression gate.
> The regression gate cannot measure the extraction side today; that is a known and
> deliberate boundary of the current design -- the signal is in the `gold missing` column of
> the report and in the "extraction side" section, not in the gate.
> Once the extractor is wired to traces, `kind.tool_experience.recall@5` will show up as a
> **new metric** (`scripts/evals-gate.py` will print "+ not in the baseline"), and only after
> one blessing does it start being guarded.

> **This table is directly comparable with the previous version (116 questions) on the
> retrieval side** (identical digit for digit).
> All that is new is one "tool experience" row and two cells of the "all" row:
> `mean injected` 13.0 → 12.2 and `gold missing` 0 → 8, both coming from the eight new
> questions that are doomed to fail.
>
> It is still not comparable with the earlier 111-question version: ① the five cross-domain
> bridge questions q112-q116 were supposed to fail under the default configuration;
> ② the tie-breaking order in `fusion::rrf` was fixed (see below).

### This evaluation used to be unable to tell apart changes smaller than 0.02

`fusion::rrf` used to collect the results in the order of `HashMap::into_values()` and then
sort them by score, and the sort is stable -- so **the relative order of tied entries equalled
the hash bucket order**, which varies from process to process. Ties are the normal case in
RRF, so three runs over the same corpus with the same parameters gave:

```
R@1 = 0.630 / 0.640 / 0.650      MRR = 0.766 / 0.771 / 0.778
```

Which is to say that every conclusion in this README within ±0.02 had no evidence behind it
at the time.
Ties are now broken by fact id, and the noise has narrowed sharply -- **but it has not gone
to zero, so do not read this as "the ruler has stopped shaking".**

Six consecutive runs of the same parameters, measured (hash backend, 116 questions):

| metric | range | note |
|---|---:|---|
| `recall@5` / `recall@10` | **0.0000** | identical digit for digit |
| `recall_any` / `avg_returned` / the various counts | **0.0000** | identical digit for digit |
| `recall@1` | 0.0098 | exactly one question changing its top-1 (1/102) |
| `mrr` | 0.0049 | as above |
| `avg_gold_rank` | 0.0094 | as above |

**The root cause of the residual noise**: the final tie-break is on fact id, and a fact id is
a ULID generated afresh on every ingest -- the top 48 bits are a millisecond timestamp and
the low 80 bits are random. **For two facts landing in the same millisecond, which comes
first differs every time.** The corpus has only 73 facts and most of them fall in different
milliseconds, so only one place is left that can flip.

**What this means for the conclusions**:

- **The per-type `recall@5` the regression gate uses is deterministic**, so the gate itself
  is trustworthy.
- **The third decimal place of `recall@1` and `MRR` is noise.** Judging a change smaller than
  0.01 by them amounts to reading the width of the ruler's tick marks. To judge at that
  magnitude, ingest has to be made deterministic first (a reproducible id-generation path for
  the evaluation), or the corpus has to grow until the weight of a single question comes down.

**Before tuning parameters, establish how coarse the ruler is** -- not "the ruler does not
shake", but "we know how much it shakes".

**Four things you have to know while reading this table:**

1. **`exact term`'s R@∞ is 0.926 and not 1.000; this is a known and deliberately paid price.**
   The semantic floor (abstaining altogether once the cosine distance is > 0.50) catches two
   extremely short proper-noun queries by mistake (`who is Zhao Min`, `jieba-rs`) -- the
   vector of a two-or-three-character query is diffuse to begin with.
   What it buys is a mis-retrieval rate of 0.393 → 0.286. The full curve is at
   `retrieval::BGE_M3_SEMANTIC_FLOOR`.
2. **The 0.500 mis-retrieval on the `unanswerable` row is the largest weakness of this
   retrieval stack today**, and also the hardest to fix: the distractors are things that
   really are semantically nearest, such as `meeting room B302` against `B301`, or
   `Redis expiry policy` against `pgvector`. No **score-based** gate can separate them --
   that column does not budge as the abstain threshold sweeps from 0 to 0.030. See
   `retrieval::abstain_below`.
3. **`R@∞` being 1.000 almost across the board is an artefact of corpus size**: all 69 valid
   facts fit inside the 6000-token injection budget, so budget truncation has never taken
   effect. That column only carries information once the corpus is in the thousands.
   By the same token, **the generally low exclusive counts per leg are not enough grounds for
   cutting a leg** -- every leg's retrieval width covers most of the database, so exclusives
   cannot accumulate in the first place.
4. **The `Decay` shape of recency is undecidable on this question set**: every fact is written
   within seconds of one ingest, `created_at` has no spread, the decay coefficient is the same
   for every row, and the ranking metrics are identical to `Off` digit for digit. Deciding it
   requires the question set to grow a new dimension first: "corpus spanning several weeks".

---

## Baseline for cross-domain links (`relates_to`)

`--cross-link` runs one pass of `cormex-memory::crosslink`'s nightly scan after the corpus is
loaded: vector nearest neighbours prefilter the cross-domain candidate pairs, the judge
decides whether to link them, and two directed `relates_to` edges are written.
**Off by default** -- it writes extra rows into the database, and should not disturb the
baseline unless explicitly asked for.

The judge has two settings: `prefilter` (accept everything the prefilter passed;
deterministic, fit for CI) and `cheap` (really calls the cheap model, **drifts with the
model**: four runs over one and the same corpus accepted 3/4/6/7 pairs).

### The five questions that need a bridge (q112-q116)

| configuration | edges (pairs/rows) | R@5 | R@10 | R@∞ | MRR | gold ranks |
|---|---|---:|---:|---:|---:|---|
| **A** no bridge | — | 0.200 | 0.400 | 0.400 | 0.069 | ×, 4, 6, ×, × |
| **B** bridge · accept all prefiltered | 24/34 | 0.200 | 0.600 | 1.000 | 0.148 | 13, 11, 2, 7, 7 |
| **C** bridge · judged by the cheap model | 3/6 | **0.600** | **1.000** | 1.000 | **0.237** | 8, 4, 1, 3, 7 |
| **D** bridge · prefilter + edges in the index | 24/34 | 0.200 | 0.400 | 1.000 | 0.110 | 15, 17, 4, 6, 10 |
| **B′** bridge · prefilter · exemption off | 24/34 | 0.200 | 0.400 | 0.800 | 0.089 | ×, 8, 4, 11, 18 |

### The original 111 questions (the regression surface -- side effects can only be judged on this group)

| configuration | R@1 | R@5 | R@10 | MRR | mis-retr.@5 | mean injected |
|---|---:|---:|---:|---:|---:|---:|
| **A** no bridge | 0.673 | 0.912 | 0.959 | 0.807 | 0.286 | 12.9 |
| **B** accept all prefiltered | 0.688 | 0.918 | 0.948 | 0.818 | **0.357** | 15.1 |
| **C** judged by the cheap model | 0.683 | 0.912 | 0.948 | 0.814 | **0.250** | 13.1 |
| **D** edges in the index | 0.657 | 0.897 | 0.943 | 0.792 | 0.321 | 14.2 |
| **B′** exemption off | 0.673 | 0.918 | 0.948 | 0.808 | 0.321 | 13.9 |

**Four conclusions can be read out of these two tables:**

1. **The bridge really does turn "cannot be retrieved" into "can be retrieved".** R@∞ jumps
   from 0.400 to 1.000 between A and B: of the five questions, three originally **caught not
   a single gold**, and with the bridge all of them are reachable.
   This is the only change of an order of magnitude in this piece of work.
2. **Spend the money on the judgement, not on the prefilter.** B and C differ eightfold in
   edge count, and C is better across the board on the new questions (R@5 0.200→0.600)
   **and** the mis-retrieval of the original 111 questions actually falls from 0.286 to 0.250.
   B's mis-retrieval rises to 0.357 -- that is the direct reading of noisy edges infecting
   graph traversal, and it pushed the `exact term` cell's mis-retrieval from 0.500 up to 1.000.
   The prefilter on its own (cross-domain + not yet connected + a distance band) is **not
   enough**.
3. **Edges must not go into the BM25 / vector index** (row D). An edge's statement crams two
   entity names and a one-line summary into a single sentence, which is exactly the shape
   `ts_rank` loves most, so it dominates the board on any query that mentions either end:
   R@5 on the original 111 questions drops 0.015, MRR drops 0.026, and the new questions get
   worse rather than better.
   `crosslink::Linker::indexed` defaults to false, and this row is the reason.
4. **The domain-penalty exemption is worth having** (row B′). Turn it off and the new
   questions' R@10 falls back from 0.600 to 0.400, R@∞ drops to 0.800, and gold's average
   rank retreats across the board. A bridge that is built and then pressed back down by domain
   weighting might as well not have been built. For the scope of the exemption see
   `retrieval::bridged_entities`.

**Still unsolved**: in C the gold ranks of the five questions are 8/4/1/3/7 -- only one leg
(graph traversal) catches them, so the fusion score is naturally low. The bridge got them
into the injection window (13 memories injected on average) but not into the front of the
top 5. Getting them further up requires the graph traversal leg's contribution to be worth
more than a single `1/(60+rank)`, and that is a question for the fusion side, outside the
scope of this change.

### Knobs that have been settled

Behind every default there is a sweep curve; before changing one, run the curve again --
they interact with each other (the abstain threshold was recalibrated against the
distribution that the strong-hit bonus had lifted).

| knob | value | where the reasoning is written |
|---|---|---|
| time recency | **off** (neither a retrieval leg nor a decay) | `retrieval::RecencyMode` |
| the episodes leg | **off** (the implementation and the questions are kept) | `retrieval::Retriever::episode_channel` |
| single-leg strong-hit bonus | 0.04 | `fusion::apply_peak_bonus` + `Retriever::new` |
| abstain threshold | 0.030 | `retrieval::Retriever::abstain_below` |
| semantic floor | 0.50 (only for a real semantic backend) | `retrieval::BGE_M3_SEMANTIC_FLOOR` |
| time replay | three legs ranked inside the snapshot | `retrieval::Retriever::retrieve_as_of_ranked` |
| cross-domain link scan | **off by default**; when on, the judge is `cheap` | `crosslink::Linker` |
| whether edges go into the index | **they do not** | `crosslink::Linker::indexed` |
| distance band / degree cap for edges | 0.12-0.45 / 4 | `crosslink::DEFAULT_MIN_DISTANCE` and friends |
| domain-penalty exemption for bridges | **on** | `retrieval::bridged_entities` |

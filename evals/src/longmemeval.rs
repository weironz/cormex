//! LongMemEval-S retrieval recall (P7-3).
//!
//! # What this measures, stated plainly
//!
//! **Retrieval recall (R@k), not question-answering accuracy.** For each
//! question, the haystack is loaded into a fresh schema, the question is used as
//! a search query, and the top-k sessions are checked for the session the
//! dataset says holds the answer.
//!
//! That number is **not comparable** to the QA accuracies other systems publish
//! for LongMemEval. A system can score 96% recall and 60% QA accuracy on the
//! same data: recall says "the right session came back", QA says "the model
//! then answered correctly from it". The survey in `docs/research/` records the
//! exact way this gets conflated in public comparisons, and the rule this
//! project took from it is: **say which one, and never put them in one table**.
//!
//! # Why the dataset is not in the repository
//!
//! It is 277 MB and MIT-licensed but not ours. `--dataset` points at a copy;
//! the README says where to get it. Committing it would make every clone pay
//! for a benchmark most contributors never run.
//!
//! # Why one schema per question
//!
//! Each question carries its **own** haystack of ~50 sessions, and they differ
//! between questions. Loading them all into one schema would let question A's
//! answer session be retrieved for question B, which inflates the score and
//! measures nothing.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context as _, Result};
use chrono::{DateTime, NaiveDateTime, Utc};
use cormex_core::Id;
use cormex_memory::tokenize;
use cormex_store::{
    ChunkEncoding, FileRole, Framing, NewArchiveChunk, NewCaptureChunk, NewCaptureFile,
    NewProjection, NewTranscriptEvent, ProjectionStatus, Store, TranscriptEventKind,
};
use serde::Deserialize;

/// One entry of `longmemeval_s_cleaned.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Question {
    pub question_id: String,
    pub question_type: String,
    pub question: String,
    /// The dataset's own answer. **Never used here** — this target measures
    /// retrieval, and reading the answer would invite scoring it.
    ///
    /// A `Value` rather than a `String`: some answers are numbers
    /// (`"answer": 3`), and deserializing them as strings fails on a 277 MB
    /// file with a line number and no hint that one question's answer is the
    /// problem.
    #[serde(default)]
    pub answer: serde_json::Value,
    /// The session ids that hold the answer.
    #[serde(default)]
    pub answer_session_ids: Vec<String>,
    #[serde(default)]
    pub haystack_dates: Vec<String>,
    #[serde(default)]
    pub haystack_session_ids: Vec<String>,
    #[serde(default)]
    pub haystack_sessions: Vec<Vec<Turn>>,
}

/// One turn of a haystack session.
///
/// `content` is usually a string but is **not always one**: some turns carry a
/// list of content parts (the dataset includes a few multimodal exchanges).
/// Deserializing it as `String` fails on those, and the failure names a line
/// number in a 277 MB file with no hint that one turn in one session is the
/// problem. So it is read as a `Value` and flattened.
#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub role: String,
    #[serde(deserialize_with = "string_or_parts")]
    pub content: String,
}

/// A string, or a list of content parts flattened to text.
fn string_or_parts<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|p| {
                p.get("text")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
                    // A part with no text (an image) contributes nothing to
                    // retrieval, and inventing a placeholder would put words in
                    // the haystack that the dataset does not contain
                    .or_else(|| p.as_str().map(str::to_owned))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    })
}

/// What one question scored.
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuestionScore {
    pub id: String,
    pub kind: String,
    /// The rank of the first correct session, 1-based. `None` = not in the
    /// top-k at all.
    pub first_hit_rank: Option<usize>,
    /// Whether any answer session was in the top-k.
    pub hit: bool,
    pub sessions: usize,
    pub turns: usize,
}

/// The run's result.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LongMemEvalReport {
    pub dataset: String,
    pub questions: usize,
    /// Which k values were scored.
    pub ks: Vec<usize>,
    /// `k` → recall. **Recall over sessions**, not QA accuracy.
    pub recall: Vec<(usize, f64)>,
    /// The same, per question type.
    pub by_type: Vec<TypeRecall>,
    pub scores: Vec<QuestionScore>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TypeRecall {
    pub kind: String,
    pub questions: usize,
    /// `k` → recall.
    pub recall: Vec<(usize, f64)>,
}

impl LongMemEvalReport {
    /// The human-readable report, with the 口径 stated at the top.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "LongMemEval-S retrieval recall — {} questions from {}\n\n\
             ⚠️  This is **retrieval recall (R@k)**, not question-answering accuracy.\n\
             \x20   It says \"the session holding the answer came back in the top k\".\n\
             \x20   It does NOT say \"the model then answered correctly\". The two are\n\
             \x20   not comparable and must not be put in one table.\n\n",
            self.questions, self.dataset
        );
        out.push_str("  by type\n");
        for t in &self.by_type {
            let line: Vec<String> = t
                .recall
                .iter()
                .map(|(k, r)| format!("R@{k} {:.1}%", 100.0 * r))
                .collect();
            out.push_str(&format!(
                "    {:<26} n={:<4} {}\n",
                t.kind,
                t.questions,
                line.join("  ")
            ));
        }
        out.push_str("\n  overall\n    ");
        let line: Vec<String> = self
            .recall
            .iter()
            .map(|(k, r)| format!("R@{k} {:.1}%", 100.0 * r))
            .collect();
        out.push_str(&line.join("  "));
        out.push('\n');
        out
    }
}

/// Load the dataset.
///
/// # Errors
/// The file is missing or is not the expected JSON.
pub fn load(path: &Path) -> Result<Vec<Question>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| {
        format!(
            "{} is not longmemeval_s_cleaned.json (the array-of-questions shape)",
            path.display()
        )
    })
}

/// Run the evaluation over `questions`, scoring recall at each of `ks`.
///
/// # Errors
/// The database cannot be reached, or a write failed.
pub async fn run(
    database_url: &str,
    dataset_name: &str,
    questions: &[Question],
    ks: &[usize],
) -> Result<LongMemEvalReport> {
    let mut scores = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        tracing::info!(
            "{}/{}: {} ({})",
            i + 1,
            questions.len(),
            q.question_id,
            q.question_type
        );
        let db = crate::harness::EvalDb::create(database_url).await?;
        let outcome = score_one(&db.store, q, ks).await;
        db.cleanup(false).await;
        scores.push(outcome?);
    }

    let recall: Vec<(usize, f64)> = ks
        .iter()
        .map(|k| {
            let hits = scores
                .iter()
                .filter(|s| s.first_hit_rank.is_some_and(|r| r <= *k))
                .count();
            (*k, ratio(hits, scores.len()))
        })
        .collect();

    let mut kinds: Vec<String> = scores.iter().map(|s| s.kind.clone()).collect();
    kinds.sort();
    kinds.dedup();
    let by_type = kinds
        .into_iter()
        .map(|kind| {
            let group: Vec<&QuestionScore> = scores.iter().filter(|s| s.kind == kind).collect();
            let recall = ks
                .iter()
                .map(|k| {
                    let hits = group
                        .iter()
                        .filter(|s| s.first_hit_rank.is_some_and(|r| r <= *k))
                        .count();
                    (*k, ratio(hits, group.len()))
                })
                .collect();
            TypeRecall {
                kind,
                questions: group.len(),
                recall,
            }
        })
        .collect();

    Ok(LongMemEvalReport {
        dataset: dataset_name.to_owned(),
        questions: scores.len(),
        ks: ks.to_vec(),
        recall,
        by_type,
        scores,
    })
}

fn ratio(hits: usize, total: usize) -> f64 {
    if total == 0 {
        return 0.0;
    }
    f64::from(u32::try_from(hits).unwrap_or(u32::MAX))
        / f64::from(u32::try_from(total).unwrap_or(u32::MAX))
}

/// Seed one question's haystack, search, and score.
async fn score_one(store: &Store, q: &Question, ks: &[usize]) -> Result<QuestionScore> {
    let max_k = ks.iter().copied().max().unwrap_or(5);
    let mut turns = 0;

    // The dataset's session id → the archive's session id, so a hit can be
    // matched back to `answer_session_ids`
    let mut by_native: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    // **Duplicate session ids are real in this dataset.** A handful of
    // questions list the same `haystack_session_ids` entry twice with identical
    // content at two different dates. The archive's unique key is
    // `(agent, native_session_id)`, so the second registration returns the same
    // session and its events collide on `(session, parser_version, seq)`.
    //
    // Keeping the first occurrence is right rather than merely convenient: the
    // content is identical, so the retrieval result cannot differ, and the
    // answer ids still match the session that is loaded.
    let mut seen: HashSet<String> = HashSet::new();

    for (i, session) in q.haystack_sessions.iter().enumerate() {
        let native = q
            .haystack_session_ids
            .get(i)
            .cloned()
            .unwrap_or_else(|| format!("s{i}"));
        if !seen.insert(native.clone()) {
            tracing::debug!(
                question = %q.question_id,
                session = %native,
                "the dataset lists this session twice; keeping the first"
            );
            continue;
        }
        let occurred_at = q
            .haystack_dates
            .get(i)
            .and_then(|d| parse_dataset_time(d))
            .unwrap_or_else(Utc::now);

        let file = NewCaptureFile {
            id: Id::new(),
            native_session_id: native.clone(),
            parent_native_session_id: None,
            device_id: Id::derived(
                DateTime::from_timestamp(0, 0).expect("the epoch is valid"),
                "longmemeval-device",
            )
            .to_string(),
            agent: "longmemeval".into(),
            logical_path: format!("longmemeval/{native}"),
            role: FileRole::Transcript,
            framing: Framing::Append,
            cwd: None,
            project_key: None,
        };
        let registered = store
            .archive_txn(async |t| t.register_capture_file(&file).await)
            .await
            .context("registering a haystack session failed")?;
        let session_id = registered.session.id.clone();
        let generation_id = registered.generation.id.clone();
        by_native.insert(session_id.clone(), native.clone());

        let body = session
            .iter()
            .map(|t| t.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let sha = format!("{:064x}", fnv1a(body.as_bytes()));
        store
            .archive_txn(async |t| {
                t.append_chunk(&NewCaptureChunk {
                    id: Id::new(),
                    generation_id: generation_id.parse().expect("a ULID"),
                    byte_offset: 0,
                    byte_length: i64::try_from(body.len()).unwrap_or(0),
                    content_sha256: sha.clone(),
                    storage_key: format!("eval/capture/{}/{}/{sha}", &sha[0..2], &sha[2..4]),
                    encoding: ChunkEncoding::Identity,
                    observed_at: occurred_at,
                })
                .await
            })
            .await
            .context("appending a haystack chunk failed")?;

        let mut events = Vec::with_capacity(session.len());
        for (seq, turn) in session.iter().enumerate() {
            let kind = match turn.role.as_str() {
                "user" => TranscriptEventKind::UserMessage,
                "assistant" => TranscriptEventKind::AgentMessage,
                _ => TranscriptEventKind::System,
            };
            events.push(NewTranscriptEvent {
                id: Id::derived(occurred_at, &format!("longmemeval|{native}|{seq}")),
                session_id: session_id.parse().expect("a ULID"),
                parser_version: "longmemeval@1".into(),
                seq: i64::try_from(seq).unwrap_or(0),
                kind,
                occurred_at: Some(occurred_at),
                content: serde_json::json!([{ "type": "text", "text": turn.content }]),
                text: Some(turn.content.clone()),
                tsv_source: Some(tokenize::to_tsvector_input(&turn.content)),
                tool_name: None,
                tool_call_id: None,
                src_generation_id: generation_id.parse().expect("a ULID"),
                src_offset: 0,
                src_length: i64::try_from(turn.content.len()).unwrap_or(0),
            });
            turns += 1;
        }
        let projection = NewProjection {
            id: Id::new(),
            session_id: session_id.parse().expect("a ULID"),
            parser_version: "longmemeval@1".into(),
            through: serde_json::json!({}),
            next_seq: i64::try_from(events.len()).unwrap_or(0),
            status: ProjectionStatus::Ok,
            error: None,
        };
        store
            .archive_txn(async |t| t.insert_transcript_events(&events, &projection).await)
            .await
            .context("writing a haystack session's events failed")?;

        // The archive index too, so `scope=archive` would work: a tool result
        // here is any turn, since the dataset has no tool notion
        let chunks: Vec<NewArchiveChunk> = session
            .iter()
            .enumerate()
            .filter(|(_, t)| !t.content.trim().is_empty())
            .map(|(seq, t)| NewArchiveChunk {
                id: Id::new(),
                session_id: session_id.parse().expect("a ULID"),
                event_id: Id::derived(occurred_at, &format!("longmemeval|{native}|{seq}")),
                part: 0,
                tsv_source: tokenize::to_tsvector_input(&t.content),
                text: t.content.clone(),
            })
            .collect();
        store
            .archive_txn(async |t| t.insert_archive_chunks(&chunks).await)
            .await
            .context("writing a haystack session's index chunks failed")?;
    }

    // The question, as the search query. **The conversation index**, which is
    // what a person would search — not the archive index, which is tool output.
    let tsquery = tokenize::to_tsquery_input(&q.question);
    let hits = store
        .search_conversation(
            &tsquery,
            None,
            None,
            None,
            None,
            i64::try_from(max_k).unwrap_or(10),
        )
        .await
        .context("the search failed")?;

    let answer: HashSet<String> = q.answer_session_ids.iter().cloned().collect();
    let first_hit_rank = hits
        .iter()
        .enumerate()
        .find(|(_, h)| {
            by_native
                .get(&h.session_id)
                .is_some_and(|native| answer.contains(native))
        })
        .map(|(i, _)| i + 1);

    Ok(QuestionScore {
        id: q.question_id.clone(),
        kind: q.question_type.clone(),
        first_hit_rank,
        hit: first_hit_rank.is_some_and(|r| r <= max_k),
        sessions: q.haystack_sessions.len(),
        turns,
    })
}

/// `2023/05/20 (Sat) 02:21` → a timestamp.
///
/// The dataset's own format, which is neither RFC 3339 nor ISO 8601. Parsing it
/// rather than defaulting to `now` matters: the event-time axis is what makes
/// "what did I know in March" answerable, and a wrong timestamp would silently
/// make every session look like it happened at import time.
fn parse_dataset_time(raw: &str) -> Option<DateTime<Utc>> {
    // Drop the weekday in parentheses
    let cleaned: String = raw
        .split_whitespace()
        .filter(|part| !part.starts_with('('))
        .collect::<Vec<_>>()
        .join(" ");
    NaiveDateTime::parse_from_str(&cleaned, "%Y/%m/%d %H:%M")
        .ok()
        .map(|dt| dt.and_utc())
}

/// A deterministic 64-bit hash, formatted to fill the `sha256` domain.
///
/// Not a cryptographic digest and never used as one: these bytes are never
/// deduplicated against anything.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The dataset's timestamp format parses.
    #[test]
    fn the_dataset_timestamp_parses() {
        let t = parse_dataset_time("2023/05/20 (Sat) 02:21").expect("parse");
        assert_eq!(t.format("%Y-%m-%d %H:%M").to_string(), "2023-05-20 02:21");
    }

    /// An unparseable timestamp is `None`, not "now".
    ///
    /// Defaulting to now would make every session look like it happened at
    /// import time, and the event-time axis is exactly what the archive exists
    /// to keep honest.
    #[test]
    fn an_unparseable_timestamp_is_none() {
        assert!(parse_dataset_time("not a date").is_none());
    }

    /// The report says what it measures, at the top.
    ///
    /// This is the card's whole point: the number must not be mistaken for QA
    /// accuracy, and a report that only prints "R@5 87%" invites exactly that.
    #[test]
    fn the_report_states_its_metric() {
        let report = LongMemEvalReport {
            dataset: "test".into(),
            questions: 2,
            ks: vec![5],
            recall: vec![(5, 0.5)],
            by_type: vec![TypeRecall {
                kind: "single-session-user".into(),
                questions: 2,
                recall: vec![(5, 0.5)],
            }],
            scores: Vec::new(),
        };
        let text = report.render();
        assert!(
            text.contains("not question-answering accuracy"),
            "the report does not say what it measures: {text}"
        );
        assert!(text.contains("must not be put in one table"), "{text}");
        assert!(text.contains("R@5 50.0%"), "{text}");
    }

    /// A question whose answer session is not retrieved scores 0, not a panic.
    #[test]
    fn a_miss_is_zero() {
        assert_eq!(
            ratio(0, 3),
            0.0,
            "a miss must be 0, and an empty group must not divide by zero"
        );
        assert_eq!(ratio(0, 0), 0.0);
    }
}

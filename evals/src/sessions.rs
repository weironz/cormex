//! The **session retrieval** eval suite: captured conversations, not facts.
//!
//! Design: docs/plan/session-archive-plan.md P7-2.
//!
//! # Why this is a separate suite and a separate target
//!
//! `cormex-zh-en-v1` measures the memory side: facts, four recall legs, RRF.
//! This measures the archive side: did `search_conversation` /
//! `search_archive` find the session a person was thinking of. They share no
//! tables, no index, and no ranking — a number from one says nothing about the
//! other, and folding them into one report would let a good memory score hide a
//! broken archive.
//!
//! # The three classes of question, and why all three are required
//!
//! | class | what it proves |
//! |---|---|
//! | only the archive finds it | tool output is indexed and reachable |
//! | only the conversation finds it | the conversation index is not just the archive |
//! | nothing should be found | the search is not returning everything |
//!
//! The third is not a supplement. Recall alone rewards the degenerate strategy
//! of returning every session, and the `expect: []` questions are the only
//! thing that punishes it.
//!
//! # The corpus is written, not captured
//!
//! These are hand-written sessions in the shape real ones take (a decision, a
//! build error, a `docker compose ps` line). Real transcripts cannot go in a
//! public repository, and a fixture that is anonymized until it says nothing
//! measures nothing. What has to be real is the **shape**: a tool result whose
//! text appears nowhere in the conversation is exactly what D4's two indexes
//! exist to separate.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use cormex_core::Id;
use cormex_memory::tokenize;
use cormex_store::{
    ArchiveTxn, ChunkEncoding, FileRole, Framing, NewCaptureChunk, NewCaptureFile, NewProjection,
    NewTranscriptEvent, ProjectionStatus, Store, TranscriptEventKind,
};
use serde::{Deserialize, Serialize};

/// One captured session in the corpus.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionFixture {
    pub id: String,
    pub agent: String,
    #[serde(default)]
    pub project: Option<String>,
    pub events: Vec<EventFixture>,
}

/// One event, as the suite writes it.
#[derive(Debug, Clone, Deserialize)]
pub struct EventFixture {
    /// `user_message` / `agent_message` / `tool_call` / `tool_result` / `plan`.
    pub kind: String,
    /// RFC 3339.
    pub occurred_at: String,
    pub text: String,
    #[serde(default)]
    pub tool_name: Option<String>,
}

/// One question.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionQuestion {
    pub id: String,
    /// `conversation` or `archive`.
    pub scope: String,
    pub query: String,
    /// The session ids that should come back. Empty = nothing should.
    #[serde(default)]
    pub expect: Vec<String>,
    #[serde(default)]
    pub note: String,
}

/// The whole suite.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionSuite {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub sessions: Vec<SessionFixture>,
    pub questions: Vec<SessionQuestion>,
}

/// One question's result.
#[derive(Debug, Clone, Serialize)]
pub struct SessionQuestionResult {
    pub id: String,
    pub scope: String,
    pub query: String,
    /// Did the search return the right set?
    pub hit: bool,
    /// What came back, in order.
    pub got: Vec<String>,
    pub expect: Vec<String>,
    pub note: String,
}

/// The run's result.
#[derive(Debug, Clone, Serialize)]
pub struct SessionReport {
    pub suite: String,
    pub sessions: usize,
    pub events: usize,
    /// Per scope: how many questions hit out of how many.
    pub by_scope: Vec<ScopeScore>,
    pub questions: Vec<SessionQuestionResult>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScopeScore {
    pub scope: String,
    pub hit: usize,
    pub total: usize,
}

impl SessionReport {
    /// `hit / total` over every question.
    #[must_use]
    pub fn accuracy(&self) -> f64 {
        let total: usize = self.by_scope.iter().map(|s| s.total).sum();
        let hit: usize = self.by_scope.iter().map(|s| s.hit).sum();
        if total == 0 {
            return 0.0;
        }
        f64::from(u32::try_from(hit).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(total).unwrap_or(u32::MAX))
    }

    /// The human-readable report.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "session retrieval eval — {} ({} sessions, {} events)\n\n",
            self.suite, self.sessions, self.events
        );
        for s in &self.by_scope {
            out.push_str(&format!(
                "  {:<14} {:>3}/{:<3}  {:.0}%\n",
                s.scope,
                s.hit,
                s.total,
                if s.total == 0 {
                    0.0
                } else {
                    100.0 * f64::from(u32::try_from(s.hit).unwrap_or(0))
                        / f64::from(u32::try_from(s.total).unwrap_or(1))
                }
            ));
        }
        out.push_str(&format!("\n  overall {:.1}%\n\n", 100.0 * self.accuracy()));

        for q in &self.questions {
            let mark = if q.hit { "ok  " } else { "MISS" };
            out.push_str(&format!("  {mark} {} [{}] {}\n", q.id, q.scope, q.query));
            if !q.hit {
                out.push_str(&format!(
                    "        expected {:?}, got {:?}\n",
                    q.expect, q.got
                ));
            }
        }
        out
    }
}

impl SessionSuite {
    /// # Errors
    /// The file cannot be read, or its fields do not line up.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read the session suite {}", path.display()))?;
        serde_json::from_str(&raw).with_context(|| {
            format!(
                "session suite {} is not valid JSON, or its fields do not line up",
                path.display()
            )
        })
    }

    /// Static checks, no database needed.
    ///
    /// # Errors
    /// A duplicate id, an unknown scope, an expectation naming a session that
    /// is not in the corpus, or a suite with no negative question.
    pub fn validate(&self) -> Result<()> {
        let mut seen = std::collections::HashSet::new();
        for s in &self.sessions {
            if !seen.insert(format!("session:{}", s.id)) {
                bail!("duplicate session id: {}", s.id);
            }
            if s.events.is_empty() {
                bail!("session {} has no events", s.id);
            }
            for e in &s.events {
                parse_time(&e.occurred_at)
                    .with_context(|| format!("session {} has an unreadable occurred_at", s.id))?;
                if !KNOWN_KINDS.contains(&e.kind.as_str()) {
                    bail!(
                        "session {} has an event of unknown kind {:?}; the projector would \
                         reject it",
                        s.id,
                        e.kind
                    );
                }
            }
        }

        let mut qids = std::collections::HashSet::new();
        let mut negatives = 0;
        for q in &self.questions {
            if !qids.insert(format!("question:{}", q.id)) {
                bail!("duplicate question id: {}", q.id);
            }
            if q.scope != "conversation" && q.scope != "archive" {
                bail!(
                    "question {} has scope {:?}; it must be conversation or archive",
                    q.id,
                    q.scope
                );
            }
            if q.query.trim().is_empty() {
                bail!("question {} has an empty query", q.id);
            }
            for want in &q.expect {
                if !seen.contains(&format!("session:{want}")) {
                    bail!(
                        "question {} expects session {want}, which is not in the corpus -- the \
                         question can never be answered",
                        q.id
                    );
                }
            }
            if q.expect.is_empty() {
                negatives += 1;
            }
        }

        // **At least one negative.** Recall alone rewards returning everything;
        // the `expect: []` questions are the only thing that punishes it
        if negatives == 0 {
            bail!(
                "the suite has no question with an empty `expect`; without one, a search that \
                 returns every session scores 100%"
            );
        }
        Ok(())
    }

    /// How many events the corpus holds.
    #[must_use]
    pub fn event_count(&self) -> usize {
        self.sessions.iter().map(|s| s.events.len()).sum()
    }
}

/// A deterministic 64-bit hash, formatted to fill the `sha256` domain.
///
/// Not a cryptographic digest and never used as one — see the call site.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

const KNOWN_KINDS: &[&str] = &[
    "user_message",
    "agent_message",
    "agent_thought",
    "tool_call",
    "tool_result",
    "plan",
    "system",
    "meta",
];

fn parse_time(raw: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(raw)
        .with_context(|| format!("{raw:?} is not RFC 3339"))?
        .with_timezone(&Utc))
}

fn kind_of(raw: &str) -> Result<TranscriptEventKind> {
    Ok(match raw {
        "user_message" => TranscriptEventKind::UserMessage,
        "agent_message" => TranscriptEventKind::AgentMessage,
        "agent_thought" => TranscriptEventKind::AgentThought,
        "tool_call" => TranscriptEventKind::ToolCall,
        "tool_result" => TranscriptEventKind::ToolResult,
        "plan" => TranscriptEventKind::Plan,
        "system" => TranscriptEventKind::System,
        "meta" => TranscriptEventKind::Meta,
        other => bail!("unknown event kind {other:?}"),
    })
}

/// Write the corpus into `store` as archive-layer rows.
///
/// # Why the archive tables directly, rather than through the capture API
///
/// The capture API takes **bytes** and parses them with a host-specific
/// parser. What this eval measures is the **index**, not the parser: writing
/// `transcript_events` directly is the same data the projector would produce,
/// without a fixture that has to be valid Claude Code JSONL. The parser has its
/// own tests (`cormex-transcript`), and a broken parser would fail there, not
/// here with a confusing "the search found nothing".
///
/// The archive chunks are written the same way `index_tick` writes them, so
/// `scope=archive` sees exactly what production would.
///
/// # Errors
/// A write failed.
pub async fn seed_sessions(store: &Store, suite: &SessionSuite) -> Result<usize> {
    let mut written = 0;
    for session in &suite.sessions {
        let file = NewCaptureFile {
            id: Id::new(),
            native_session_id: session.id.clone(),
            parent_native_session_id: None,
            // A `ulid` domain, not free text: `capture_files.device_id` points at
            // `cormex_auth.device_keys.id` (cross-schema, so no FK), and the
            // domain CHECK rejects anything that is not a ULID
            device_id: Id::derived(
                chrono::DateTime::from_timestamp(0, 0).expect("the epoch is valid"),
                "eval-sessions-device",
            )
            .to_string(),
            agent: session.agent.clone(),
            logical_path: format!("eval/{}", session.id),
            role: FileRole::Transcript,
            framing: Framing::Append,
            cwd: None,
            project_key: session.project.clone(),
        };
        let registered = store
            .archive_txn(async |t| t.register_capture_file(&file).await)
            .await
            .context("registering the eval session's file failed")?;
        let session_id = registered.session.id.clone();
        let generation_id = registered.generation.id.clone();

        // One chunk carrying the whole session, so `src_generation_id` has
        // something to point at. The bytes are never read back by this eval --
        // it searches the index -- but the FK is NOT NULL
        let body = session
            .events
            .iter()
            .map(|e| e.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // A **valid** `sha256` domain value, but not a real digest: these bytes
        // are never deduplicated against anything, and pulling `sha2` in for a
        // value nothing reads would be a dependency for a placeholder. The
        // domain's CHECK still applies, so it has to be 64 lowercase hex
        let sha = format!("{:064x}", fnv1a(session.id.as_bytes()));
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
                    observed_at: Utc::now(),
                })
                .await
            })
            .await
            .context("appending the eval session's chunk failed")?;

        let mut events = Vec::with_capacity(session.events.len());
        let mut chunks = Vec::new();
        for (seq, e) in session.events.iter().enumerate() {
            let kind = kind_of(&e.kind)?;
            let occurred_at = parse_time(&e.occurred_at)?;
            let event_id = Id::derived(occurred_at, &format!("eval-session|{}|{seq}", session.id));
            events.push(NewTranscriptEvent {
                id: event_id,
                session_id: session_id.parse().expect("a ULID"),
                parser_version: "eval@1".into(),
                seq: i64::try_from(seq).unwrap_or(0),
                kind,
                occurred_at: Some(occurred_at),
                content: serde_json::json!([{ "type": "text", "text": e.text }]),
                text: Some(e.text.clone()),
                tsv_source: Some(tokenize::to_tsvector_input(&e.text)),
                tool_name: e.tool_name.clone(),
                tool_call_id: None,
                src_generation_id: generation_id.parse().expect("a ULID"),
                src_offset: 0,
                src_length: i64::try_from(e.text.len()).unwrap_or(0),
            });

            // The archive index holds **tool output only**, matching
            // `index_tick`: indexing the messages here too would put every
            // sentence in two indexes, and `scope=archive` would then return
            // the conversation with tool output mixed in
            if matches!(
                kind,
                TranscriptEventKind::ToolCall | TranscriptEventKind::ToolResult
            ) && !e.text.trim().is_empty()
            {
                chunks.push(cormex_store::NewArchiveChunk {
                    id: Id::new(),
                    session_id: session_id.parse().expect("a ULID"),
                    event_id,
                    part: 0,
                    tsv_source: tokenize::to_tsvector_input(&e.text),
                    text: e.text.clone(),
                });
            }
            written += 1;
        }

        let projection = NewProjection {
            id: Id::new(),
            session_id: session_id.parse().expect("a ULID"),
            parser_version: "eval@1".into(),
            through: serde_json::json!({}),
            next_seq: i64::try_from(events.len()).unwrap_or(0),
            status: ProjectionStatus::Ok,
            error: None,
        };
        store
            .archive_txn(async |t| t.insert_transcript_events(&events, &projection).await)
            .await
            .context("writing the eval session's events failed")?;
        // **The archive index is the only thing `scope=archive` reads.** If
        // this write were skipped the archive questions would fail and the
        // conversation ones would not, which is the shape a real regression
        // takes -- and it is what the fail-before check exercises
        store
            .archive_txn(async |t| t.insert_archive_chunks(&chunks).await)
            .await
            .context("writing the eval session's archive chunks failed")?;
    }
    Ok(written)
}

/// Run the questions against the seeded store.
///
/// # Errors
/// A search failed.
pub async fn run_questions(store: &Store, suite: &SessionSuite) -> Result<SessionReport> {
    // The fixture's session id is the one a person would use; the database's is
    // derived. The map is what turns a hit back into the id the question names
    let mut by_native: HashMap<String, String> = HashMap::new();
    for session in &suite.sessions {
        let id = cormex_store::archive::session_id_for(&session.agent, &session.id);
        by_native.insert(id.to_string(), session.id.clone());
    }

    let mut results = Vec::with_capacity(suite.questions.len());
    for q in &suite.questions {
        let tsquery = tokenize::to_tsquery_input(&q.query);
        // A **bounded** limit: the negative questions are the ones that punish
        // returning everything, and a limit of 1000 would make every one of
        // them fail for the wrong reason
        let got: Vec<String> = if q.scope == "archive" {
            store
                .search_archive(&tsquery, None, None, None, None, 10)
                .await?
                .into_iter()
                .filter_map(|h| by_native.get(&h.session_id).cloned())
                .collect()
        } else {
            store
                .search_conversation(&tsquery, None, None, None, None, 10)
                .await?
                .into_iter()
                .filter_map(|h| by_native.get(&h.session_id).cloned())
                .collect()
        };

        // **Two different criteria, and they must be different.**
        //
        // A positive question asks "did the right session come back", which is
        // recall over the returned list: the index is a ranking, and a query
        // sharing a token with three other sessions legitimately returns four.
        // Requiring set equality would make every ordinary question fail.
        //
        // A negative question asks "did anything come back at all", and there
        // the answer has to be nothing. That is the only thing that punishes a
        // search which returns everything.
        let hit = if q.expect.is_empty() {
            got.is_empty()
        } else {
            q.expect.iter().all(|want| got.contains(want))
        };

        results.push(SessionQuestionResult {
            id: q.id.clone(),
            scope: q.scope.clone(),
            query: q.query.clone(),
            hit,
            got,
            expect: q.expect.clone(),
            note: q.note.clone(),
        });
    }

    let mut by_scope: Vec<ScopeScore> = Vec::new();
    for scope in ["conversation", "archive"] {
        let group: Vec<&SessionQuestionResult> =
            results.iter().filter(|r| r.scope == scope).collect();
        if group.is_empty() {
            continue;
        }
        by_scope.push(ScopeScore {
            scope: scope.to_owned(),
            hit: group.iter().filter(|r| r.hit).count(),
            total: group.len(),
        });
    }

    Ok(SessionReport {
        suite: format!("{} {}", suite.name, suite.version),
        sessions: suite.sessions.len(),
        events: suite.event_count(),
        by_scope,
        questions: results,
    })
}

/// Ensure the archive transaction type is reachable from here.
///
/// A compile-time check rather than a comment: `seed_sessions` writes through
/// `archive_txn` (the archive layer takes no sync lock, D3), and if that ever
/// stopped being the entry point this would fail to build.
const _: fn() = || {
    let _: Option<ArchiveTxn> = None;
};

#[cfg(test)]
mod tests {
    use super::*;

    fn suite() -> SessionSuite {
        SessionSuite::load(std::path::Path::new("suites/sessions-zh-en-v1.json"))
            .expect("the checked-in suite should load")
    }

    /// The checked-in suite passes its own static checks.
    #[test]
    fn the_checked_in_suite_is_valid() {
        suite().validate().expect("the suite should validate");
    }

    /// A suite with no negative question is refused.
    ///
    /// Without one, a search that returns every session scores 100%.
    #[test]
    fn a_suite_without_a_negative_question_is_refused() {
        let mut s = suite();
        s.questions.retain(|q| !q.expect.is_empty());
        let err = s.validate().expect_err("should be refused");
        assert!(
            err.to_string().contains("empty `expect`"),
            "the error should name the missing negative question: {err}"
        );
    }

    /// An expectation naming a session that is not in the corpus is refused.
    #[test]
    fn an_expectation_naming_a_missing_session_is_refused() {
        let mut s = suite();
        s.questions[0].expect = vec!["s-does-not-exist".into()];
        let err = s.validate().expect_err("should be refused");
        assert!(
            err.to_string().contains("not in the corpus"),
            "the error should say the session is missing: {err}"
        );
    }

    /// An unknown scope is refused.
    #[test]
    fn an_unknown_scope_is_refused() {
        let mut s = suite();
        s.questions[0].scope = "everything".into();
        assert!(s.validate().is_err());
    }

    /// The suite covers all three classes the card requires.
    ///
    /// Checked rather than assumed: a suite that quietly lost its
    /// "only the archive finds it" questions would still validate, and the
    /// number it produced would then be measuring something else.
    #[test]
    fn the_suite_covers_all_three_classes() {
        let s = suite();
        let has_archive_only = s
            .questions
            .iter()
            .any(|q| q.scope == "archive" && !q.expect.is_empty());
        let has_conversation = s
            .questions
            .iter()
            .any(|q| q.scope == "conversation" && !q.expect.is_empty());
        let has_negative = s.questions.iter().any(|q| q.expect.is_empty());
        assert!(has_archive_only, "no archive-only question");
        assert!(has_conversation, "no conversation question");
        assert!(has_negative, "no negative question");
    }

    /// The corpus has a tool result whose text appears in no conversation
    /// message.
    ///
    /// That is what makes "only the archive finds it" a real test rather than a
    /// question the conversation index could answer too.
    #[test]
    fn the_archive_only_questions_are_really_archive_only() {
        let s = suite();
        for q in s
            .questions
            .iter()
            .filter(|q| q.scope == "archive" && !q.expect.is_empty())
        {
            let words: Vec<&str> = q.query.split_whitespace().collect();
            for session in &s.sessions {
                let conversation: String = session
                    .events
                    .iter()
                    .filter(|e| e.kind == "user_message" || e.kind == "agent_message")
                    .map(|e| e.text.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                for w in &words {
                    assert!(
                        !conversation.contains(w),
                        "question {} is labelled archive-only, but {w:?} appears in session \
                         {}'s conversation -- the conversation index could answer it too, so \
                         the question does not test what it claims",
                        q.id,
                        session.id
                    );
                }
            }
        }
    }

    /// The whole thing against a real database: seed, search, score.
    ///
    /// Ignored by default because it needs Postgres; `just test-db` runs it.
    /// The suite's own questions are the fixture, so this is the check that the
    /// generator and the search agree on what a session id is.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_suite_runs_against_a_real_database() {
        let _ = dotenvy::dotenv();
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipped: DATABASE_URL is not set");
            return;
        };
        let db = crate::harness::EvalDb::create(&url)
            .await
            .expect("the eval database should come up");
        let suite = suite();
        let seeded = seed_sessions(&db.store, &suite).await.expect("seed");
        assert_eq!(
            seeded,
            suite.event_count(),
            "the corpus should hold every event the suite declares"
        );

        let report = run_questions(&db.store, &suite).await.expect("run");
        db.cleanup(false).await;

        assert_eq!(
            report.accuracy(),
            1.0,
            "the checked-in suite should be fully answerable by the archive's own indexes; a \
             miss here means either the generator and the search disagree, or a question is \
             wrong. Report:\n{}",
            report.render()
        );
    }

    /// Without the archive index, the archive questions fail and the
    /// conversation ones still pass.
    ///
    /// This is the fail-before check for the whole target: it proves the
    /// archive questions are actually measuring the archive index rather than
    /// passing for some other reason.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_archive_questions_measure_the_archive_index() {
        let _ = dotenvy::dotenv();
        let Ok(url) = std::env::var("DATABASE_URL") else {
            eprintln!("skipped: DATABASE_URL is not set");
            return;
        };
        let db = crate::harness::EvalDb::create(&url)
            .await
            .expect("the eval database should come up");
        let suite = suite();
        seed_sessions(&db.store, &suite).await.expect("seed");

        // Drop the archive index, as a broken `index_tick` would leave it
        let pool = sqlx::postgres::PgPool::connect(&format!(
            "{url}?options=-csearch_path%3D{}%2Cpublic",
            db.schema()
        ))
        .await
        .expect("connect");
        sqlx::query("DELETE FROM archive_chunks")
            .execute(&pool)
            .await
            .expect("drop the index");
        pool.close().await;

        let report = run_questions(&db.store, &suite).await.expect("run");
        db.cleanup(false).await;

        let archive: Vec<_> = report
            .questions
            .iter()
            .filter(|r| r.scope == "archive" && !r.expect.is_empty())
            .collect();
        assert!(
            archive.iter().all(|r| !r.hit),
            "an archive question passed with the archive index empty -- it is not measuring \
             what it claims: {archive:?}"
        );
        let conversation: Vec<_> = report
            .questions
            .iter()
            .filter(|r| r.scope == "conversation" && !r.expect.is_empty())
            .collect();
        assert!(
            conversation.iter().all(|r| r.hit),
            "dropping the archive index broke the conversation questions too -- the two \
             indexes are not independent: {conversation:?}"
        );
    }
}

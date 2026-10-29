//! Session search: the two indexes, and the project exclusion.
//!
//! Everything here needs a real database — the properties under test are
//! `to_tsquery`, `ts_rank` and a GIN index.

mod common;

use chrono::Utc;
use cormex_core::Id;
use cormex_store::{
    ChunkEncoding, FileRole, Framing, NewArchiveChunk, NewCaptureChunk, NewCaptureFile,
    NewProjection, NewTranscriptEvent, ProjectionStatus, Store, TranscriptEventKind,
};

/// Seed a session with one message and one tool result, returning
/// `(session_id, generation_id)`.
async fn seed(
    s: &Store,
    agent: &str,
    project: Option<&str>,
    message: &str,
    tool_output: &str,
) -> (String, String) {
    let new = NewCaptureFile {
        id: Id::new(),
        native_session_id: format!("native-{}", Id::new()),
        parent_native_session_id: None,
        device_id: Id::new().to_string(),
        agent: agent.into(),
        logical_path: "projects/-p/s1.jsonl".into(),
        role: FileRole::Transcript,
        framing: Framing::Append,
        cwd: None,
        project_key: project.map(str::to_owned),
    };
    let registered = s
        .archive_txn(async |t| t.register_capture_file(&new).await)
        .await
        .expect("register");
    let (session, generation) = (registered.session.id, registered.generation.id);

    s.archive_txn(async |t| {
        t.append_chunk(&NewCaptureChunk {
            id: Id::new(),
            generation_id: generation.parse().expect("a ULID"),
            byte_offset: 0,
            byte_length: 10,
            content_sha256: "a".repeat(64),
            storage_key: format!("public/capture/aa/aa/{}.zst", "a".repeat(64)),
            encoding: ChunkEncoding::Zstd,
            observed_at: Utc::now(),
        })
        .await
    })
    .await
    .expect("append");

    let events = vec![
        NewTranscriptEvent {
            id: Id::new(),
            session_id: session.parse().expect("a ULID"),
            parser_version: "claude_code@1".into(),
            seq: 0,
            kind: TranscriptEventKind::UserMessage,
            occurred_at: Some(Utc::now()),
            content: serde_json::json!([]),
            text: Some(message.into()),
            tsv_source: Some(simple_tsv(message)),
            tool_name: None,
            tool_call_id: None,
            src_generation_id: generation.parse().expect("a ULID"),
            src_offset: 0,
            src_length: 10,
        },
        NewTranscriptEvent {
            id: Id::new(),
            session_id: session.parse().expect("a ULID"),
            parser_version: "claude_code@1".into(),
            seq: 1,
            kind: TranscriptEventKind::ToolResult,
            occurred_at: Some(Utc::now()),
            content: serde_json::json!([]),
            text: Some(tool_output.into()),
            tsv_source: Some(simple_tsv(tool_output)),
            tool_name: Some("Bash".into()),
            tool_call_id: None,
            src_generation_id: generation.parse().expect("a ULID"),
            src_offset: 10,
            src_length: 10,
        },
    ];
    let event_ids: Vec<Id> = events.iter().map(|e| e.id).collect();
    s.archive_txn(async |t| {
        t.insert_transcript_events(
            &events,
            &NewProjection {
                id: Id::new(),
                session_id: session.parse().expect("a ULID"),
                parser_version: "claude_code@1".into(),
                through: serde_json::json!({}),
                next_seq: 2,
                status: ProjectionStatus::Ok,
                error: None,
            },
        )
        .await
    })
    .await
    .expect("events");

    // Index the tool result, as the indexer would
    s.archive_txn(async |t| {
        t.insert_archive_chunks(&[NewArchiveChunk {
            id: Id::new(),
            session_id: session.parse().expect("a ULID"),
            event_id: event_ids[1],
            part: 0,
            text: tool_output.into(),
            tsv_source: simple_tsv(tool_output),
        }])
        .await
    })
    .await
    .expect("chunk");

    (session, generation)
}

/// The production tokenizer, from a **dev-dependency**.
///
/// The Chinese cases need jieba: `to_tsvector('simple', …)` does not segment
/// Chinese at all — a whole sentence becomes one lexeme, so `对象存储` never
/// matches `对象存储用的是 RustFS`. Using anything other than the production
/// tokenizer here would make the test pass against a query the real system
/// never produces.
fn simple_tsv(text: &str) -> String {
    cormex_memory::tokenize::to_tsvector_input(text)
}

fn tsq(q: &str) -> String {
    cormex_memory::tokenize::to_tsquery_input(q)
}

#[tokio::test]
async fn a_chinese_query_finds_the_conversation() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    seed(
        s,
        "claude_code",
        None,
        "对象存储用的是 RustFS",
        "build succeeded",
    )
    .await;

    let hits = s
        .search_conversation(&tsq("对象存储"), None, None, None, None, 20)
        .await
        .expect("search");
    db.cleanup().await;
    assert_eq!(hits.len(), 1, "the Chinese query should find the message");
    assert!(hits[0].text.as_deref().unwrap_or("").contains("RustFS"));
}

#[tokio::test]
async fn an_english_query_finds_the_conversation() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    seed(
        s,
        "claude_code",
        None,
        "we use RustFS for storage",
        "build succeeded",
    )
    .await;

    let hits = s
        .search_conversation(&tsq("RustFS"), None, None, None, None, 20)
        .await
        .expect("search");
    db.cleanup().await;
    assert_eq!(hits.len(), 1);
}

/// **Tool output is in the archive index and not in the conversation index.**
///
/// That is D4: a build log would otherwise outrank the sentence somebody
/// actually typed.
#[tokio::test]
async fn the_archive_finds_tool_output_and_the_conversation_does_not() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    seed(
        s,
        "claude_code",
        None,
        "run the tests please",
        "test result: ok. 42 passed; 0 failed",
    )
    .await;

    let conversation = s
        .search_conversation(&tsq("passed"), None, None, None, None, 20)
        .await
        .expect("conversation search");
    assert!(
        conversation.is_empty(),
        "the conversation index found a word that only appears in tool output -- that is the \
         noise D4 keeps out of the default search"
    );

    let archive = s
        .search_archive(&tsq("passed"), None, None, None, None, 20)
        .await
        .expect("archive search");
    db.cleanup().await;
    assert_eq!(
        archive.len(),
        1,
        "the archive index should find the tool output"
    );
    assert!(archive[0].text.contains("42 passed"));
}

/// A project excluded from the archive index is not found there.
#[tokio::test]
async fn an_excluded_project_is_not_in_the_archive_index() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    seed(
        s,
        "claude_code",
        Some("github.com/u/secret"),
        "hello",
        "the secret word is pineapple",
    )
    .await;

    let before = s
        .search_archive(&tsq("pineapple"), None, None, None, None, 20)
        .await
        .expect("search");
    assert_eq!(
        before.len(),
        1,
        "the premise: it is findable before exclusion"
    );

    s.set_archive_index("github.com/u/secret", true)
        .await
        .expect("exclude");
    assert!(
        s.archive_index_excluded("github.com/u/secret")
            .await
            .expect("query"),
        "the exclusion should be recorded"
    );

    let after = s
        .search_archive(&tsq("pineapple"), None, None, None, None, 20)
        .await
        .expect("search");
    db.cleanup().await;
    assert!(
        after.is_empty(),
        "a project excluded from the archive index was still found"
    );
}

/// Including it again brings it back, **without re-indexing**.
///
/// That is why the exclusion is applied at query time as well as at write time:
/// turning it back on must not require a re-scan.
#[tokio::test]
async fn including_a_project_again_brings_it_back() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    seed(
        s,
        "claude_code",
        Some("github.com/u/p"),
        "hello",
        "the word is pineapple",
    )
    .await;

    s.set_archive_index("github.com/u/p", true)
        .await
        .expect("exclude");
    s.set_archive_index("github.com/u/p", false)
        .await
        .expect("include");

    let hits = s
        .search_archive(&tsq("pineapple"), None, None, None, None, 20)
        .await
        .expect("search");
    db.cleanup().await;
    assert_eq!(
        hits.len(),
        1,
        "including the project again should bring it back without re-indexing"
    );
}

/// A project with no event is **included**, not excluded.
///
/// Excluding by default would mean the index silently stops covering new
/// projects.
#[tokio::test]
async fn a_project_with_no_event_is_included() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let excluded = s
        .archive_index_excluded("github.com/u/never-seen")
        .await
        .expect("query");
    db.cleanup().await;
    assert!(!excluded);
}

/// Results are aggregated per session: one long session does not bury the rest.
#[tokio::test]
async fn results_are_one_per_session() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    seed(s, "claude_code", None, "pineapple one", "pineapple two").await;

    let hits = s
        .search_conversation(&tsq("pineapple"), None, None, None, None, 20)
        .await
        .expect("search");
    db.cleanup().await;
    assert_eq!(
        hits.len(),
        1,
        "one session matched twice and should appear once; otherwise a long session buries the \
         other sessions that matched"
    );
}

#[tokio::test]
async fn an_empty_query_returns_nothing() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    seed(s, "claude_code", None, "hello", "world").await;

    assert!(
        s.search_conversation("", None, None, None, None, 20)
            .await
            .expect("search")
            .is_empty()
    );
    assert!(
        s.search_archive("   ", None, None, None, None, 20)
            .await
            .expect("search")
            .is_empty()
    );
    db.cleanup().await;
}

/// The agent filter applies.
#[tokio::test]
async fn the_agent_filter_applies() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    seed(s, "claude_code", None, "pineapple", "x").await;

    let matched = s
        .search_conversation(&tsq("pineapple"), Some("claude_code"), None, None, None, 20)
        .await
        .expect("search");
    let unmatched = s
        .search_conversation(&tsq("pineapple"), Some("codex"), None, None, None, 20)
        .await
        .expect("search");
    db.cleanup().await;
    assert_eq!(matched.len(), 1);
    assert!(unmatched.is_empty());
}

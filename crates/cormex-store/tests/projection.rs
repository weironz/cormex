//! The P layer: insertion, pagination, the `tsv`, and the unique constraint.
//!
//! Everything here needs a real database — the properties under test are
//! `to_tsvector`, a GIN index and a unique constraint, and faking any of them
//! fakes away the thing being tested.

mod common;

use chrono::Utc;
use cormex_core::Id;
use cormex_store::{
    ChunkEncoding, FileRole, Framing, NewAccessLog, NewArchiveChunk, NewCaptureChunk,
    NewCaptureFile, NewCaptureMask, NewMaskProgress, NewProjection, NewTranscriptEvent,
    ProjectionStatus, Store, TranscriptEventKind,
};

/// Register a session and a file, returning `(session_id, file_id,
/// generation_id)`.
async fn setup(s: &Store) -> (String, String, String) {
    let new = NewCaptureFile {
        id: Id::new(),
        native_session_id: "native-1".into(),
        parent_native_session_id: None,
        device_id: Id::new().to_string(),
        agent: "claude_code".into(),
        logical_path: "projects/-p/s1.jsonl".into(),
        role: FileRole::Transcript,
        framing: Framing::Append,
        cwd: None,
        project_key: None,
    };
    let registered = s
        .archive_txn(async |t| t.register_capture_file(&new).await)
        .await
        .expect("register");
    (
        registered.session.id,
        registered.file.id,
        registered.generation.id,
    )
}

fn event(
    session_id: &str,
    generation_id: &str,
    seq: i64,
    kind: TranscriptEventKind,
    text: &str,
) -> NewTranscriptEvent {
    NewTranscriptEvent {
        // Derived, so a re-projection is idempotent
        id: Id::derived(
            Utc::now(),
            &format!("test|claude_code@1|{generation_id}|{seq}"),
        ),
        session_id: session_id.parse().expect("a ULID"),
        parser_version: "claude_code@1".into(),
        seq,
        kind,
        occurred_at: Some(Utc::now()),
        content: serde_json::json!([{ "type": "text", "text": text }]),
        text: Some(text.into()),
        tsv_source: Some(text.into()),
        tool_name: None,
        tool_call_id: None,
        src_generation_id: generation_id.parse().expect("a ULID"),
        src_offset: seq * 100,
        src_length: 100,
    }
}

fn progress(session_id: &str, next_seq: i64) -> NewProjection {
    NewProjection {
        id: Id::new(),
        session_id: session_id.parse().expect("a ULID"),
        parser_version: "claude_code@1".into(),
        through: serde_json::json!({}),
        next_seq,
        status: ProjectionStatus::Ok,
        error: None,
    }
}

#[tokio::test]
async fn events_insert_and_paginate_in_seq_order() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (session, _, generation) = setup(s).await;

    let events: Vec<NewTranscriptEvent> = (0..5)
        .map(|i| {
            event(
                &session,
                &generation,
                i,
                TranscriptEventKind::UserMessage,
                &format!("message {i}"),
            )
        })
        .collect();
    s.archive_txn(async |t| {
        t.insert_transcript_events(&events, &progress(&session, 5))
            .await
    })
    .await
    .expect("insert");

    let page = s
        .transcript_page(&session, "claude_code@1", -1, 3)
        .await
        .expect("page");
    assert_eq!(page.len(), 3);
    assert_eq!(
        page.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![0, 1, 2],
        "the page must be in seq order"
    );

    let next = s
        .transcript_page(&session, "claude_code@1", 2, 10)
        .await
        .expect("page");
    assert_eq!(
        next.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![3, 4],
        "after_seq is exclusive: passing the last seq of the previous page must not repeat it"
    );
    db.cleanup().await;
}

/// The `tsv` is written with the main row, so full-text search finds it
/// immediately.
#[tokio::test]
async fn the_tsv_is_written_with_the_main_row() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (session, _, generation) = setup(s).await;

    let e = event(
        &session,
        &generation,
        0,
        TranscriptEventKind::UserMessage,
        "对象存储用的是 RustFS",
    );
    s.archive_txn(async |t| {
        t.insert_transcript_events(&[e], &progress(&session, 1))
            .await
    })
    .await
    .expect("insert");

    let hits: Vec<String> = db
        .query_raw(format!(
            "SELECT id FROM transcript_events
              WHERE session_id = '{session}'
                AND tsv @@ to_tsquery('simple', 'RustFS')"
        ))
        .await;
    db.cleanup().await;
    assert_eq!(
        hits.len(),
        1,
        "the row is not findable by full text. If the tsv were written in a separate statement, \
         the symptom would be exactly this: the row exists, search cannot see it"
    );
}

/// Inserting the same `(session, parser_version, seq)` twice is refused.
///
/// The projector relies on the derived id plus `ON CONFLICT DO NOTHING` for
/// idempotence; this pins the constraint that makes that safe.
#[tokio::test]
async fn a_duplicate_seq_is_refused_by_the_constraint() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (session, _, generation) = setup(s).await;

    let first = event(
        &session,
        &generation,
        0,
        TranscriptEventKind::UserMessage,
        "one",
    );
    s.archive_txn(async |t| {
        t.insert_transcript_events(&[first], &progress(&session, 1))
            .await
    })
    .await
    .expect("insert");

    // A *different* id at the same seq: the derived id would make this a no-op,
    // so this uses a fresh one to exercise the unique constraint itself
    let mut second = event(
        &session,
        &generation,
        0,
        TranscriptEventKind::UserMessage,
        "two",
    );
    second.id = Id::new();
    let result = s
        .archive_txn(async |t| {
            t.insert_transcript_events(&[second], &progress(&session, 1))
                .await
        })
        .await;
    db.cleanup().await;

    assert!(
        result.is_err(),
        "a duplicate (session_id, parser_version, seq) was accepted. The projector's idempotence \
         rests on the derived id making it a no-op; without this constraint a bug in the id \
         derivation would silently duplicate events"
    );
}

/// A re-projection with the same derived id is a no-op.
#[tokio::test]
async fn a_reprojection_with_the_same_id_is_idempotent() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (session, _, generation) = setup(s).await;

    let e = event(
        &session,
        &generation,
        0,
        TranscriptEventKind::UserMessage,
        "one",
    );
    let clone = e.clone();
    s.archive_txn(async |t| {
        t.insert_transcript_events(&[e], &progress(&session, 1))
            .await
    })
    .await
    .expect("insert");
    s.archive_txn(async |t| {
        t.insert_transcript_events(&[clone], &progress(&session, 1))
            .await
    })
    .await
    .expect("the second insert should be a no-op, not an error");

    let count: Vec<String> = db
        .query_raw(format!(
            "SELECT count(*)::text FROM transcript_events WHERE session_id = '{session}'"
        ))
        .await;
    db.cleanup().await;
    assert_eq!(count.first().map(String::as_str), Some("1"));
}

#[tokio::test]
async fn the_latest_projection_is_the_newest_row() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (session, _, _) = setup(s).await;

    assert!(
        s.latest_projection(&session, "claude_code@1")
            .await
            .expect("query")
            .is_none(),
        "a session that has never been projected has no projection"
    );

    s.archive_txn(async |t| {
        t.insert_transcript_events(&[], &progress(&session, 5))
            .await
    })
    .await
    .expect("insert");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    s.archive_txn(async |t| {
        t.insert_transcript_events(&[], &progress(&session, 9))
            .await
    })
    .await
    .expect("insert");

    let latest = s
        .latest_projection(&session, "claude_code@1")
        .await
        .expect("query")
        .expect("a projection");
    db.cleanup().await;
    assert_eq!(latest.next_seq, 9, "the newest row should win");
}

#[tokio::test]
async fn masks_and_progress_round_trip() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (_, _, generation) = setup(s).await;

    let masks = vec![
        NewCaptureMask {
            id: Id::new(),
            generation_id: generation.parse().expect("a ULID"),
            byte_offset: 10,
            byte_length: 20,
            rule: "openai_key".into(),
            scanner: "redaction@1".into(),
        },
        NewCaptureMask {
            id: Id::new(),
            generation_id: generation.parse().expect("a ULID"),
            byte_offset: 100,
            byte_length: 5,
            rule: "aws_key".into(),
            scanner: "redaction@1".into(),
        },
    ];
    let progress_row = NewMaskProgress {
        id: Id::new(),
        generation_id: generation.parse().expect("a ULID"),
        scanner: "redaction@1".into(),
        through_offset: 200,
    };
    s.archive_txn(async |t| t.insert_masks(&masks, &progress_row).await)
        .await
        .expect("insert masks");

    assert_eq!(
        s.mask_progress(&generation, "redaction@1")
            .await
            .expect("progress"),
        200
    );

    // A range overlapping only the first mask
    let hits = s.masks_in_range(&generation, 0, 50).await.expect("query");
    db.cleanup().await;
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rule, "openai_key");
}

/// The masker's progress starts at 0, not at "unknown".
#[tokio::test]
async fn a_never_scanned_generation_is_at_zero() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (_, _, generation) = setup(s).await;
    let progress = s
        .mask_progress(&generation, "redaction@1")
        .await
        .expect("progress");
    db.cleanup().await;
    assert_eq!(
        progress, 0,
        "a generation that has never been scanned starts at the beginning"
    );
}

/// Archive chunks are searchable, and the `tsv` is `NOT NULL`.
#[tokio::test]
async fn archive_chunks_are_searchable() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (session, _, generation) = setup(s).await;

    let e = event(
        &session,
        &generation,
        0,
        TranscriptEventKind::ToolResult,
        "cargo test",
    );
    let event_id = e.id;
    s.archive_txn(async |t| {
        t.insert_transcript_events(&[e], &progress(&session, 1))
            .await
    })
    .await
    .expect("insert");

    let chunk = NewArchiveChunk {
        id: Id::new(),
        session_id: session.parse().expect("a ULID"),
        event_id,
        part: 0,
        text: "the output of cargo test was 42 passed".into(),
        tsv_source: "the output of cargo test was 42 passed".into(),
    };
    s.archive_txn(async |t| t.insert_archive_chunks(&[chunk]).await)
        .await
        .expect("insert chunk");

    let hits: Vec<String> = db
        .query_raw(
            "SELECT id FROM archive_chunks WHERE tsv @@ to_tsquery('simple', 'cargo & test')",
        )
        .await;
    db.cleanup().await;
    assert_eq!(hits.len(), 1, "the archive chunk is not searchable");
}

/// Every archive write stays out of `sync_log`.
#[tokio::test]
async fn the_p_layer_never_enters_sync_log() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (session, _, generation) = setup(s).await;

    let e = event(
        &session,
        &generation,
        0,
        TranscriptEventKind::UserMessage,
        "x",
    );
    let event_id = e.id;
    s.archive_txn(async |t| {
        t.insert_transcript_events(&[e], &progress(&session, 1))
            .await
    })
    .await
    .expect("insert");
    s.archive_txn(async |t| {
        t.insert_archive_chunks(&[NewArchiveChunk {
            id: Id::new(),
            session_id: session.parse().expect("a ULID"),
            event_id,
            part: 0,
            text: "x".into(),
            tsv_source: "x".into(),
        }])
        .await
    })
    .await
    .expect("chunks");
    s.archive_txn(async |t| {
        t.insert_access_log(&NewAccessLog {
            id: Id::new(),
            principal: "user:01U".into(),
            generation_id: generation.parse().expect("a ULID"),
            byte_offset: 0,
            byte_length: 10,
            unmasked: false,
        })
        .await
    })
    .await
    .expect("log");

    let rows: Vec<String> = db
        .query_raw(
            "SELECT count(*)::text FROM sync_log WHERE table_name = ANY(ARRAY[\
             'transcript_events','transcript_projections','archive_chunks','archive_access_log'])",
        )
        .await;
    db.cleanup().await;
    assert_eq!(
        rows.first().map(String::as_str),
        Some("0"),
        "a P-layer write reached sync_log; the projection is server-only"
    );
}

/// The access log records whether the read bypassed the mask.
#[tokio::test]
async fn the_access_log_records_unmasked_reads() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (_, _, generation) = setup(s).await;
    let gid: Id = generation.parse().expect("a ULID");

    for unmasked in [false, true] {
        s.archive_txn(async |t| {
            t.insert_access_log(&NewAccessLog {
                id: Id::new(),
                principal: "user:01U".into(),
                generation_id: gid,
                byte_offset: 0,
                byte_length: 10,
                unmasked,
            })
            .await
        })
        .await
        .expect("log");
    }

    let rows: Vec<String> = db
        .query_raw(format!(
            "SELECT unmasked::text FROM archive_access_log WHERE generation_id = '{generation}' \
             ORDER BY created_at"
        ))
        .await;
    db.cleanup().await;
    assert_eq!(
        rows,
        vec!["false", "true"],
        "the log must distinguish a masked read from an unmasked one: that distinction is the \
         only thing that makes an audit of raw:read meaningful"
    );
}

/// A chunk of a generation is needed for the mask tests to be realistic.
#[tokio::test]
async fn a_chunk_can_be_appended_before_masking() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (_, _, generation) = setup(s).await;
    let outcome = s
        .archive_txn(async |t| {
            t.append_chunk(&NewCaptureChunk {
                id: Id::new(),
                generation_id: generation.parse().expect("a ULID"),
                byte_offset: 0,
                byte_length: 100,
                content_sha256: "a".repeat(64),
                storage_key: format!("public/capture/aa/aa/{}.zst", "a".repeat(64)),
                encoding: ChunkEncoding::Zstd,
                observed_at: Utc::now(),
            })
            .await
        })
        .await
        .expect("append");
    db.cleanup().await;
    assert!(matches!(
        outcome,
        cormex_store::AppendOutcome::Appended { end_offset: 100 }
    ));
}

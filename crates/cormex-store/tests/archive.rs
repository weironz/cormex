//! The archive write path: contiguity, idempotency, and the two things that
//! must **not** happen (a hole in a generation, a row in `sync_log`).
//!
//! Everything here needs a real database. The properties under test are
//! `pg_advisory_xact_lock`, `UNIQUE (generation_id, byte_offset)` and a
//! `SELECT max(...)` — faking any of them fakes away the thing being tested.
//!
//! # Why the concurrency test matters more than the rest
//!
//! A hole in a generation raises no error at write time. It surfaces when
//! somebody restores the file and gets a shorter one, or when the projector
//! reads a chunk that starts after the previous one ended and silently skips
//! the gap. By then nothing in the database says which bytes are missing —
//! that is exactly what the contiguity invariant exists to prevent, and it is
//! only observable under concurrent writers.

mod common;

use chrono::Utc;
use cormex_core::Id;
use cormex_store::{
    AppendOutcome, ChunkEncoding, FileEventOp, FileRole, Framing, GenerationReason,
    NewCaptureChunk, NewCaptureFile, NewCaptureFileEvent, Store, table,
};

/// A device id. `capture_files.device_id` is a `ulid` column (it points at
/// `cormex_auth.device_keys.id`), so a readable name like `"device-1"` is
/// rejected by the domain's CHECK — which is the schema doing its job.
fn device() -> String {
    Id::new().to_string()
}

/// Register one file and return `(file_id, generation_id)`.
async fn register(s: &Store, device: &str, path: &str) -> (String, String) {
    let new = NewCaptureFile {
        id: Id::new(),
        native_session_id: "native-session-1".to_owned(),
        parent_native_session_id: None,
        device_id: device.to_owned(),
        agent: "claude_code".to_owned(),
        logical_path: path.to_owned(),
        role: FileRole::Transcript,
        framing: Framing::Append,
        cwd: Some("/home/user/project-a".to_owned()),
        project_key: None,
    };
    s.archive_txn(async |t| t.register_capture_file(&new).await)
        .await
        .expect("registering a file should succeed")
        .into_ids()
}

/// A small extension so tests can destructure the registration result without
/// repeating the two field names.
trait IntoIds {
    fn into_ids(self) -> (String, String);
}

impl IntoIds for cormex_store::RegisteredFile {
    fn into_ids(self) -> (String, String) {
        (self.file.id, self.generation.id)
    }
}

fn chunk(generation_id: &str, offset: i64, len: i64, sha: &str) -> NewCaptureChunk {
    NewCaptureChunk {
        id: Id::new(),
        generation_id: generation_id.parse().expect("a valid ULID"),
        byte_offset: offset,
        byte_length: len,
        content_sha256: sha.to_owned(),
        storage_key: format!("public/capture/aa/bb/{sha}.zst"),
        encoding: ChunkEncoding::Zstd,
        observed_at: Utc::now(),
    }
}

/// `sha` of a fixed, obviously fake block. Not a real hash of anything: these
/// tests are about offsets and lengths, and a real SHA-256 here would only make
/// the failure messages harder to read.
fn fake_sha(n: u8) -> String {
    format!("{:064x}", n)
}

#[tokio::test]
async fn register_is_idempotent() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;

    let device = device();
    let (file_a, generation_a) = register(s, &device, "projects/a/s1.jsonl").await;
    let (file_b, generation_b) = register(s, &device, "projects/a/s1.jsonl").await;

    let generations: Vec<String> = db
        .query_raw(format!(
            "SELECT id FROM capture_generations WHERE file_id = '{file_a}' ORDER BY ordinal"
        ))
        .await;
    db.cleanup().await;

    assert_eq!(
        file_a, file_b,
        "registering the same logical file twice produced two rows — the \
         (device_id, agent, logical_path) unique key is not doing its job, and every sync would \
         create another copy of the same file"
    );
    assert_eq!(
        generation_a, generation_b,
        "the second registration opened a new generation — appending to a file would then \
         silently fork its history every time the client re-registered"
    );
    assert_eq!(
        generations.len(),
        1,
        "a freshly registered file must have exactly one generation (ordinal 0), got {}",
        generations.len()
    );
}

#[tokio::test]
async fn append_is_contiguous() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (_, generation) = register(s, &device(), "projects/a/s1.jsonl").await;

    let first = s
        .archive_txn(async |t| {
            t.append_chunk(&chunk(&generation, 0, 100, &fake_sha(1)))
                .await
        })
        .await
        .expect("the first append should succeed");
    assert_eq!(
        first,
        AppendOutcome::Appended { end_offset: 100 },
        "the first chunk at offset 0 with length 100 must leave the generation ending at 100"
    );

    let second = s
        .archive_txn(async |t| {
            t.append_chunk(&chunk(&generation, 100, 100, &fake_sha(2)))
                .await
        })
        .await
        .expect("the second append should succeed");
    assert_eq!(second, AppendOutcome::Appended { end_offset: 200 });

    // Skipping ahead is refused, and the refusal says where to resume
    let skipped = s
        .archive_txn(async |t| {
            t.append_chunk(&chunk(&generation, 300, 100, &fake_sha(3)))
                .await
        })
        .await
        .expect("a mismatched append is a result, not an error");
    assert_eq!(
        skipped,
        AppendOutcome::OffsetMismatch { expected: 200 },
        "an append at 300 when the generation ends at 200 must be refused with expected=200. \
         Accepting it would leave a 100-byte hole that nothing reports -- the restore would just \
         come back short"
    );

    let end = s
        .generation_end(&generation)
        .await
        .expect("reading the end should succeed");
    db.cleanup().await;
    assert_eq!(end, 200, "a refused append must not have changed the end");
}

#[tokio::test]
async fn append_replay_is_idempotent() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (_, generation) = register(s, &device(), "projects/a/s1.jsonl").await;

    let c = chunk(&generation, 0, 100, &fake_sha(1));
    s.archive_txn(async |t| t.append_chunk(&c).await)
        .await
        .expect("the first append should succeed");

    // Same offset, same hash, same length: a replay
    let replay = s
        .archive_txn(async |t| {
            t.append_chunk(&chunk(&generation, 0, 100, &fake_sha(1)))
                .await
        })
        .await
        .expect("a replay is a result, not an error");
    assert_eq!(
        replay,
        AppendOutcome::AlreadyPresent { end_offset: 100 },
        "a client that died between 'the server accepted it' and 'I recorded it' retries on the \
         next run; answering anything other than AlreadyPresent turns a success into a failure"
    );

    // Same offset, different bytes: the client's idea of the file has diverged
    let diverged = s
        .archive_txn(async |t| {
            t.append_chunk(&chunk(&generation, 0, 100, &fake_sha(9)))
                .await
        })
        .await
        .expect("a mismatch is a result, not an error");
    assert_eq!(
        diverged,
        AppendOutcome::OffsetMismatch { expected: 100 },
        "the same offset with different content is **not** a replay -- answering AlreadyPresent \
         would silently keep the wrong bytes"
    );

    let count: Vec<String> = db
        .query_raw(format!(
            "SELECT count(*)::text FROM capture_chunks WHERE generation_id = '{generation}'"
        ))
        .await;
    db.cleanup().await;
    assert_eq!(
        count.first().map(String::as_str),
        Some("1"),
        "the replays must not have inserted extra rows"
    );
}

/// Eight writers, each doing "read the end, append there", fifty times.
///
/// Without `pg_advisory_xact_lock(4273, hashtext(generation_id))` this is the
/// classic read-then-write race: two writers read the same end, both insert at
/// it, and the unique constraint rejects one — so the test would see errors.
/// With the lock, every append either lands or is told where to resume, and the
/// final generation has no hole and no overlap.
#[tokio::test]
async fn concurrent_appends_stay_contiguous() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (_, generation) = register(s, &device(), "projects/a/s1.jsonl").await;

    const WRITERS: usize = 8;
    const PER_WRITER: usize = 50;
    const BLOCK: i64 = 64;

    let mut tasks = Vec::new();
    for w in 0..WRITERS {
        let store = s.clone();
        let generation = generation.clone();
        tasks.push(tokio::spawn(async move {
            let mut written = 0;
            while written < PER_WRITER {
                let outcome = store
                    .archive_txn(async |t| {
                        let end = t.generation_end(&generation).await?;
                        // A distinct hash per (writer, block) so that two
                        // writers never look like a replay of each other
                        let sha = format!("{:064x}", w * 1000 + written);
                        t.append_chunk(&chunk(&generation, end, BLOCK, &sha)).await
                    })
                    .await
                    .expect("an append inside the lock must not error");
                match outcome {
                    AppendOutcome::Appended { .. } => written += 1,
                    // Somebody else got there first; try again from the new end
                    AppendOutcome::OffsetMismatch { .. } => {}
                    AppendOutcome::AlreadyPresent { .. } => {
                        panic!(
                            "two writers produced the same (offset, sha), which the distinct \
                                hashes rule out -- the test's own bookkeeping is wrong"
                        )
                    }
                }
            }
        }));
    }
    for t in tasks {
        t.await.expect("a writer task panicked");
    }

    let total: Vec<String> = db
        .query_raw(format!(
            "SELECT count(*)::text FROM capture_chunks WHERE generation_id = '{generation}'"
        ))
        .await;
    let end = s.generation_end(&generation).await.expect("read the end");
    let gaps: Vec<String> = db
        .query_raw(format!(
            "SELECT count(*)::text FROM (
                 SELECT byte_offset,
                        lag(byte_offset + byte_length) OVER (ORDER BY byte_offset) AS prev_end
                   FROM capture_chunks WHERE generation_id = '{generation}'
             ) x WHERE prev_end IS NOT NULL AND prev_end <> byte_offset"
        ))
        .await;
    db.cleanup().await;

    let expected = i64::try_from(WRITERS * PER_WRITER).expect("fits in i64");
    assert_eq!(
        total.first().map(String::as_str),
        Some(expected.to_string().as_str()),
        "expected {expected} chunks"
    );
    assert_eq!(
        end,
        expected * BLOCK,
        "the generation should end at {expected} * {BLOCK} bytes"
    );
    assert_eq!(
        gaps.first().map(String::as_str),
        Some("0"),
        "the chunks are not contiguous: some chunk does not start where the previous one ended. \
         A hole here raises no error at write time -- it surfaces when somebody restores the file \
         and gets a shorter one, with nothing in the database saying which bytes are missing"
    );
}

/// **Not one archive write may reach `sync_log`.**
///
/// The archive tables are server-only (D3). A `sync_log` row for them would
/// make `sync_payload` need a branch, and the R layer would start flowing to
/// devices — with no error anywhere, just a lot of bytes on the wire and every
/// device holding a copy of everybody's conversations.
#[tokio::test]
async fn archive_rows_never_enter_sync_log() {
    let Some(db) = common::setup().await else {
        return;
    };
    let s = &db.store;
    let (file_id, generation) = register(s, &device(), "projects/a/s1.jsonl").await;

    s.archive_txn(async |t| {
        t.append_chunk(&chunk(&generation, 0, 100, &fake_sha(1)))
            .await?;
        t.insert_file_event(&NewCaptureFileEvent {
            id: Id::new(),
            file_id: file_id.parse().expect("a valid ULID"),
            op: FileEventOp::Missing,
            detail: Some("the host deleted it".to_owned()),
        })
        .await?;
        t.open_generation(&file_id, GenerationReason::Rewrite).await
    })
    .await
    .expect("every archive write should succeed");

    let rows: Vec<String> = db
        .query_raw(
            "SELECT count(*)::text FROM sync_log WHERE table_name = ANY(ARRAY[\
             'capture_sessions','capture_files','capture_generations','capture_chunks',\
             'capture_file_events','capture_masks','capture_mask_progress'])",
        )
        .await;
    db.cleanup().await;

    assert_eq!(
        rows.first().map(String::as_str),
        Some("0"),
        "an archive write appended to sync_log. The R layer is server-only -- syncing it would \
         push every captured conversation to every device"
    );
}

/// `table::ALL` is defined as "every table name that can appear in
/// `sync_log.table_name`", and the archive tables never do.
///
/// The failure this guards against is somebody adding a capture table to `ALL`
/// "for completeness": `sync_payload` would then need a branch for it, and the
/// compiler would not say a word.
#[test]
fn the_archive_tables_are_disjoint_from_the_synced_ones() {
    let overlap: Vec<&&str> = table::ARCHIVE
        .iter()
        .filter(|t| table::ALL.contains(t))
        .collect();
    assert!(
        overlap.is_empty(),
        "these tables are in both table::ALL and table::ARCHIVE: {overlap:?}. ALL means \
         \"can appear in sync_log.table_name\"; the archive tables never do (D3)"
    );
    assert!(
        !table::ARCHIVE.is_empty(),
        "table::ARCHIVE is empty -- the disjointness assertion above is then vacuous"
    );
}

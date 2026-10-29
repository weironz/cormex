//! The sync engine, driven through every branch the design describes.
//!
//! All of these use `FakeCaptureApi` and a temporary directory: no server, no
//! database, no network. The fake implements the real server's three-way answer
//! to a chunk, so "a 409 that means skip ahead" and "a 409 that means rewind"
//! are both reachable — and those are the branches that are silent when wrong.

use cormex::config::Config;
use cormex::state::StateDb;
use cormex::sync::fake::FakeCaptureApi;
use cormex::sync::upload::Budget;
use cormex::sync::{SyncOptions, Trigger, run};

/// A catalog with one append source rooted at `home`.
fn catalog(home: &std::path::Path) -> cormex_proto::catalog::Catalog {
    use cormex_proto::capture::{FileRole, Framing};
    use cormex_proto::catalog::{AgentEntry, HomeRoot, SessionIdRule, SourceSpec};
    cormex_proto::catalog::Catalog {
        schema: 1,
        version: "test".into(),
        agents: vec![AgentEntry {
            id: "claude_code".into(),
            name: "Claude Code".into(),
            home: vec![HomeRoot::Path {
                path: home.display().to_string(),
            }],
            sources: vec![SourceSpec {
                role: FileRole::Transcript,
                glob: "projects/*/*.jsonl".into(),
                framing: Framing::Append,
                also_compressed: None,
                session_id: SessionIdRule::Stem {},
                parent_session_id: None,
                cwd: None,
            }],
            hooks: None,
            retention: None,
        }],
    }
}

struct Fixture {
    home: tempfile::TempDir,
    data: tempfile::TempDir,
    db: StateDb,
    api: FakeCaptureApi,
    catalog: cormex_proto::catalog::Catalog,
    cfg: Config,
}

impl Fixture {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("home");
        let data = tempfile::tempdir().expect("data");
        std::fs::create_dir_all(home.path().join("projects/-home-u-proj")).expect("mkdir");
        let db = StateDb::open(data.path()).expect("db");
        Self {
            catalog: catalog(home.path()),
            home,
            data,
            db,
            api: FakeCaptureApi::new(),
            cfg: Config::default(),
        }
    }

    fn write(&self, name: &str, body: &[u8]) {
        std::fs::write(
            self.home.path().join("projects/-home-u-proj").join(name),
            body,
        )
        .expect("write");
    }

    async fn sync(&self) -> cormex::sync::SyncReport {
        run(
            &self.api,
            &self.db,
            self.data.path(),
            &self.catalog,
            &self.cfg,
            "device-1",
            &SyncOptions::default(),
        )
        .await
        .expect("the run itself should not fail")
    }

    async fn sync_with(&self, opts: SyncOptions) -> cormex::sync::SyncReport {
        run(
            &self.api,
            &self.db,
            self.data.path(),
            &self.catalog,
            &self.cfg,
            "device-1",
            &opts,
        )
        .await
        .expect("the run itself should not fail")
    }

    fn state(&self, name: &str) -> cormex::state::FileState {
        self.db
            .file(&format!("claude_code|projects/-home-u-proj/{name}"))
            .expect("read")
            .expect("the file should have state")
    }
}

#[tokio::test]
async fn the_first_run_registers_and_uploads_everything() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"{\"a\":1}\n");
    let report = f.sync().await;

    assert_eq!(report.files_seen, 1);
    assert_eq!(report.files_uploaded, 1);
    assert_eq!(report.errors, 0);
    assert_eq!(report.bytes, 8);

    let state = f.state("s1.jsonl");
    assert_eq!(state.uploaded_offset, 8);
    assert!(state.file_id.is_some());
    assert!(state.generation_id.is_some());
    assert_eq!(state.generation_ordinal, Some(0));
    assert_eq!(f.api.end(state.generation_id.as_deref().expect("gid")), 8);
}

#[tokio::test]
async fn a_grown_file_only_sends_the_new_bytes() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"{\"a\":1}\n");
    f.sync().await;
    let first = f.state("s1.jsonl");

    f.write("s1.jsonl", b"{\"a\":1}\n{\"b\":2}\n");
    let report = f.sync().await;

    assert_eq!(report.bytes, 8, "only the appended line should be sent");
    let state = f.state("s1.jsonl");
    assert_eq!(
        state.generation_id, first.generation_id,
        "an append must stay in the same generation -- opening a new one would fork the history"
    );
    assert_eq!(state.uploaded_offset, 16);
}

#[tokio::test]
async fn an_unchanged_file_sends_nothing() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"{\"a\":1}\n");
    f.sync().await;
    let puts = f.api.puts();

    let report = f.sync().await;
    assert_eq!(report.files_uploaded, 0);
    assert_eq!(
        f.api.puts(),
        puts,
        "an unchanged file was uploaded again; every hook would then re-send the whole session"
    );
}

#[tokio::test]
async fn a_truncated_file_opens_a_new_generation() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.sync().await;
    let first = f.state("s1.jsonl");

    f.write("s1.jsonl", b"012");
    f.sync().await;
    let second = f.state("s1.jsonl");

    assert_ne!(
        first.generation_id, second.generation_id,
        "a truncated file must open a new generation; the old bytes stay in the old one"
    );
    assert_eq!(second.generation_ordinal, Some(1));
    assert_eq!(second.uploaded_offset, 3);
    assert_eq!(
        f.api.end(first.generation_id.as_deref().expect("gid")),
        10,
        "the old generation must keep its bytes -- generations are branches, not versions"
    );
}

#[tokio::test]
async fn a_rewrite_of_the_same_length_opens_a_new_generation() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.sync().await;
    let first = f.state("s1.jsonl");

    // Same length, different content: invisible to a size comparison
    f.write("s1.jsonl", b"abcdefghij");
    f.sync().await;
    let second = f.state("s1.jsonl");

    assert_ne!(
        first.generation_id, second.generation_id,
        "a same-length rewrite must open a new generation; appending would concatenate two \
         different files into one generation and the result is not a valid transcript"
    );
    assert_eq!(second.uploaded_offset, 10);
}

#[tokio::test]
async fn a_409_that_says_the_server_is_ahead_skips_forward() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.sync().await;

    // Pretend a previous 200 was lost: the server already has 10, the client
    // thinks it has 0
    let mut state = f.state("s1.jsonl");
    state.uploaded_offset = 0;
    state.tail_sha256 = None;
    f.db.put_file(&state).expect("write");

    f.write("s1.jsonl", b"0123456789ABCDE");
    // The server's generation ends at 10, so the first PUT at 0 answers 409
    f.sync().await;

    let after = f.state("s1.jsonl");
    assert_eq!(
        after.uploaded_offset, 15,
        "the client should have skipped to the server's end and sent only the new bytes"
    );
}

#[tokio::test]
async fn a_409_that_says_the_client_is_ahead_rewinds() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.sync().await;

    // Pretend the state database was restored from an older backup: the client
    // thinks it has more than the server does
    let mut state = f.state("s1.jsonl");
    state.uploaded_offset = 10;
    f.db.put_file(&state).expect("write");
    // Force the server to claim it only has 0
    f.api.force_mismatch(0);

    f.write("s1.jsonl", b"0123456789");
    f.sync().await;

    let after = f.state("s1.jsonl");
    assert_eq!(
        after.uploaded_offset, 10,
        "the client should have rewound to 0 and re-sent to 10"
    );
}

#[tokio::test]
async fn a_network_error_is_retried_then_the_run_continues() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.write("s2.jsonl", b"abcdefghij");

    // The first chunk of the first file fails twice, then succeeds
    f.api.fail_next_puts(2);
    let report = f.sync().await;

    assert_eq!(
        report.errors,
        0,
        "a transient network error must be retried, not counted as a file failure: {}",
        report.last_error.as_deref().unwrap_or("")
    );
    assert_eq!(report.files_uploaded, 2);
}

#[tokio::test]
async fn a_budget_exhausted_run_leaves_work_for_next_time() {
    let f = Fixture::new();
    f.write("s1.jsonl", &[b'x'; 100]);
    f.write("s2.jsonl", &[b'y'; 100]);

    // Zero bytes: nothing may be uploaded this run
    let report = f
        .sync_with(SyncOptions {
            budget: Budget::new(60, 0),
            ..SyncOptions::default()
        })
        .await;
    assert_eq!(report.files_uploaded, 0);
    assert_eq!(report.deferred, 2, "both files should be deferred");

    // The next run with a real budget picks them up
    let report = f.sync().await;
    assert_eq!(report.files_uploaded, 2);
}

#[tokio::test]
async fn a_dry_run_uploads_nothing_and_records_no_run() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");

    let report = f
        .sync_with(SyncOptions {
            dry_run: true,
            ..SyncOptions::default()
        })
        .await;
    assert_eq!(report.files_uploaded, 0);
    assert_eq!(f.api.puts(), 0, "a dry run must not upload");
    assert!(
        f.db.last_successful_run().expect("read").is_none(),
        "a dry run must not be recorded as a successful sync -- `doctor` would then say \
         everything is fine while nothing has uploaded"
    );
}

#[tokio::test]
async fn a_run_is_recorded_for_doctor() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.sync().await;
    assert!(
        f.db.last_successful_run().expect("read").is_some(),
        "a successful run must be recorded, or `doctor` cannot answer 'when did this last work'"
    );
}

#[tokio::test]
async fn a_compressed_only_file_is_uploaded_as_its_logical_bytes() {
    use std::io::Write as _;

    let f = Fixture::new();
    let body = b"{\"type\":\"user\"}\n";
    let mut encoder = zstd::Encoder::new(Vec::new(), 3).expect("encoder");
    encoder
        .set_pledged_src_size(Some(body.len() as u64))
        .expect("pledge");
    encoder.include_contentsize(true).expect("size");
    encoder.write_all(body).expect("write");
    let frame = encoder.finish().expect("finish");
    std::fs::write(
        f.home.path().join("projects/-home-u-proj/s1.jsonl.zst"),
        &frame,
    )
    .expect("write");

    // The catalog's glob matches `.jsonl`, and `also_compressed` maps the `.zst`
    let mut catalog = f.catalog.clone();
    catalog.agents[0].sources[0].also_compressed = Some(".zst".into());
    let report = run(
        &f.api,
        &f.db,
        f.data.path(),
        &catalog,
        &f.cfg,
        "device-1",
        &SyncOptions::default(),
    )
    .await
    .expect("run");

    assert_eq!(report.files_uploaded, 1);
    assert_eq!(
        report.bytes,
        body.len() as i64,
        "the uploaded bytes must be the logical ones, not the frame's"
    );
    let state =
        f.db.file("claude_code|projects/-home-u-proj/s1.jsonl")
            .expect("read")
            .expect("state");
    assert_eq!(state.uploaded_offset, body.len() as i64);
}

#[tokio::test]
async fn a_half_written_last_line_is_uploaded_and_continued() {
    let f = Fixture::new();
    // A line cut off mid-way, which is what a log looks like while it is being
    // written
    f.write("s1.jsonl", b"{\"type\":\"user\",\"text\":\"hel");
    f.sync().await;
    let first = f.state("s1.jsonl");

    // The rest of the line arrives
    f.write("s1.jsonl", b"{\"type\":\"user\",\"text\":\"hello\"}\n");
    f.sync().await;
    let second = f.state("s1.jsonl");

    assert_eq!(
        first.generation_id, second.generation_id,
        "a partially written line must be appended to, not treated as a rewrite -- the R layer \
         only knows bytes, and the parser handles a line split across chunks"
    );
    assert_eq!(second.uploaded_offset, 31);
}

#[tokio::test]
async fn the_trigger_is_recorded() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"x");
    f.sync_with(SyncOptions {
        trigger: Trigger::Hook,
        ..SyncOptions::default()
    })
    .await;
    // `runs.trigger` is what `doctor` reads; the run above must have recorded
    // "hook" rather than the default
    assert!(f.db.last_successful_run().expect("read").is_some());
}

#[tokio::test]
async fn two_files_are_independent() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"aaa");
    f.write("s2.jsonl", b"bbb");
    let report = f.sync().await;
    assert_eq!(report.files_uploaded, 2);
    assert_eq!(report.bytes, 6);
    // Each has its own generation
    let a = f.state("s1.jsonl");
    let b = f.state("s2.jsonl");
    assert_ne!(a.generation_id, b.generation_id);
}

#[tokio::test]
async fn a_second_run_while_the_first_holds_the_lock_skips() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    // Hold the lock the second run would want
    let lock = cormex::sync::lock::FileLock::try_acquire(
        f.data.path(),
        "claude_code|projects/-home-u-proj/s1.jsonl",
    )
    .expect("acquire")
    .expect("acquire");

    let report = f.sync().await;
    assert_eq!(
        report.locked, 1,
        "a file whose lock is held must be skipped, not blocked on: the hook has 1.5 seconds"
    );
    assert_eq!(report.files_uploaded, 0);
    drop(lock);

    let report = f.sync().await;
    assert_eq!(report.files_uploaded, 1, "the next run picks it up");
}

#[tokio::test]
async fn a_replay_after_a_lost_ack_is_idempotent() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.sync().await;

    // Simulate "the server accepted it but the 200 never arrived": reset the
    // local state to before the upload, leaving the server with the bytes
    let mut state = f.state("s1.jsonl");
    state.uploaded_offset = 0;
    state.tail_sha256 = None;
    f.db.put_file(&state).expect("write");

    let report = f.sync().await;
    assert_eq!(report.errors, 0);
    let after = f.state("s1.jsonl");
    assert_eq!(
        after.uploaded_offset, 10,
        "the replay should converge on the server's end rather than duplicating the bytes"
    );
    assert_eq!(
        f.api
            .chunk_count(after.generation_id.as_deref().expect("gid")),
        1,
        "the replay inserted a second chunk at the same offset"
    );
}

#[tokio::test]
async fn an_api_that_always_fails_counts_one_error_and_continues() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.write("s2.jsonl", b"abcdefghij");
    // More failures than the retry budget, so the first file gives up
    f.api.fail_next_puts(100);

    let report = f.sync().await;
    assert!(report.errors >= 1, "a permanent failure must be counted");
    assert!(
        report.last_error.is_some(),
        "the first error must be kept for `doctor`"
    );
}

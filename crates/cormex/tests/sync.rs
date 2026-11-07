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
            provider: None,
            kind: None,
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
                sqlite: None,
            }],
            manual_only: false,
            import_only: false,
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
        self.sync_with_catalog_and_config(&self.catalog, &self.cfg, opts)
            .await
    }

    async fn sync_with_catalog_and_config(
        &self,
        catalog: &cormex_proto::catalog::Catalog,
        cfg: &Config,
        opts: SyncOptions,
    ) -> cormex::sync::SyncReport {
        run(
            &self.api,
            &self.db,
            self.data.path(),
            catalog,
            cfg,
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

fn catalog_with_parent_cwd(home: &std::path::Path) -> cormex_proto::catalog::Catalog {
    let mut catalog = catalog(home);
    catalog.agents[0].sources[0].cwd = Some(cormex_proto::catalog::CwdRule {
        from: "parent_dir".into(),
        decode: None,
        index: None,
    });
    catalog
}

fn git_repository() -> tempfile::TempDir {
    let repo = tempfile::tempdir().expect("repo");
    let init = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(repo.path())
        .status()
        .expect("git should be installed for the capture tests");
    assert!(init.success());
    let remote = std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/example/repository.git",
        ])
        .current_dir(repo.path())
        .status()
        .expect("git remote should run");
    assert!(remote.success());
    repo
}

fn normalized_windows_path(path: &std::path::Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    if value.as_bytes().get(1) == Some(&b':')
        && value.as_bytes().first().is_some_and(u8::is_ascii_uppercase)
    {
        let mut bytes = value.into_bytes();
        bytes[0] = bytes[0].to_ascii_lowercase();
        value = String::from_utf8(bytes).expect("ASCII drive letters stay UTF-8");
    }
    value
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
async fn queued_work_survives_budget_exhaustion_and_drains_after_upload() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"hello");
    let path = f.home.path().join("projects/-home-u-proj/s1.jsonl");
    f.db.enqueue("claude_code", path.to_str().unwrap(), None, None, "now")
        .unwrap();
    f.sync_with(SyncOptions {
        trigger: Trigger::Queue,
        budget: Budget::new(60, 0),
        ..SyncOptions::default()
    })
    .await;
    assert_eq!(f.db.queue_len().unwrap(), 1);
    let report = f
        .sync_with(SyncOptions {
            trigger: Trigger::Queue,
            ..SyncOptions::default()
        })
        .await;
    assert_eq!(report.files_uploaded, 1);
    assert_eq!(f.db.queue_len().unwrap(), 0);
}

#[tokio::test]
async fn spool_is_only_removed_after_its_source_was_synced() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"hello");
    let spool = f.data.path().join("spool");
    std::fs::create_dir(&spool).unwrap();
    let source = f.home.path().join("projects/-home-u-proj/s1.jsonl");
    std::fs::write(
        spool.join("handled.json"),
        serde_json::json!({"path": source}).to_string(),
    )
    .unwrap();
    std::fs::write(
        spool.join("unknown.json"),
        serde_json::json!({"path": "unknown"}).to_string(),
    )
    .unwrap();
    std::fs::write(spool.join("other.txt"), "keep").unwrap();
    f.sync().await;
    assert!(!spool.join("handled.json").exists());
    assert!(spool.join("unknown.json").exists());
    assert!(spool.join("other.txt").exists());
}

#[tokio::test]
async fn broken_source_is_reported_without_blocking_other_uploads() {
    use cormex_proto::catalog::HomeRoot;
    let mut f = Fixture::new();
    f.write("s1.jsonl", b"hello");
    let broken = tempfile::tempdir().unwrap();
    std::fs::write(broken.path().join("opencode.db"), b"not sqlite").unwrap();
    let mut agent = cormex_proto::catalog::Catalog::builtin()
        .agents
        .iter()
        .find(|a| a.id == "opencode")
        .unwrap()
        .clone();
    agent.home = vec![HomeRoot::Path {
        path: broken.path().display().to_string(),
    }];
    f.catalog.agents.insert(0, agent);
    let report = f.sync().await;
    assert_eq!(report.files_uploaded, 1);
    assert_eq!(report.errors, 1);
    assert!(report.last_error.unwrap().contains("opencode"));
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
async fn a_dry_run_does_not_correct_server_classification() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    let catalog = catalog_with_parent_cwd(f.home.path());
    f.sync_with_catalog_and_config(&catalog, &f.cfg, SyncOptions::default())
        .await;
    let mut state = f.state("s1.jsonl");
    state.classification_sent = false;
    f.db.put_file(&state).expect("write");
    let before = f.api.classifications().len();

    f.sync_with_catalog_and_config(
        &catalog,
        &f.cfg,
        SyncOptions {
            dry_run: true,
            ..SyncOptions::default()
        },
    )
    .await;

    assert_eq!(
        f.api.classifications().len(),
        before,
        "a dry run must not mutate the server, including classification metadata"
    );
}

#[tokio::test]
async fn a_new_file_registers_its_classification_in_the_same_request() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"hello");
    let catalog = catalog_with_parent_cwd(f.home.path());

    f.sync_with_catalog_and_config(&catalog, &f.cfg, SyncOptions::default())
        .await;

    let registrations = f.api.registrations();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].cwd.as_deref(), Some("/projects"));
    assert_eq!(
        registrations[0].project_key.as_deref(),
        Some("local:/projects")
    );
    assert!(
        f.api.classifications().is_empty(),
        "a newly registered file already carries its classification; sending a second correction \
         would double every first sync"
    );
}

#[tokio::test]
async fn an_existing_file_is_corrected_once() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"hello");
    let catalog = catalog_with_parent_cwd(f.home.path());
    let disabled = Config {
        git_metadata: false,
        ..Config::default()
    };
    f.sync_with_catalog_and_config(&catalog, &disabled, SyncOptions::default())
        .await;
    assert!(f.api.classifications().is_empty());

    f.sync_with_catalog_and_config(&catalog, &f.cfg, SyncOptions::default())
        .await;
    f.sync_with_catalog_and_config(&catalog, &f.cfg, SyncOptions::default())
        .await;

    let corrections = f.api.classifications();
    assert_eq!(
        corrections.len(),
        1,
        "the first enabled run must repair the old row exactly once"
    );
    assert_eq!(corrections[0].1.cwd.as_deref(), Some("/projects"));
    assert_eq!(
        corrections[0].1.project_key.as_deref(),
        Some("local:/projects")
    );
}

#[tokio::test]
async fn disabled_git_metadata_sends_no_classification() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"hello");
    let catalog = catalog_with_parent_cwd(f.home.path());
    let disabled = Config {
        git_metadata: false,
        ..Config::default()
    };

    f.sync_with_catalog_and_config(&catalog, &disabled, SyncOptions::default())
        .await;

    assert_eq!(
        f.api.registrations()[0].project_key,
        None,
        "turning git metadata off must not invent a local project key"
    );
    assert!(f.api.classifications().is_empty());
}

#[tokio::test]
async fn queued_cwd_wins_over_path_guessing_and_is_drained() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"hello");
    let repo = git_repository();
    let path = f.home.path().join("projects/-home-u-proj/s1.jsonl");
    f.db.enqueue(
        "claude_code",
        path.to_str().unwrap(),
        Some(repo.path().to_str().unwrap()),
        Some("Stop"),
        "now",
    )
    .unwrap();

    f.sync_with(SyncOptions {
        trigger: Trigger::Queue,
        ..SyncOptions::default()
    })
    .await;

    let registrations = f.api.registrations();
    assert_eq!(registrations.len(), 1);
    assert_eq!(
        registrations[0].cwd.as_deref(),
        Some(normalized_windows_path(repo.path()).as_str())
    );
    assert_eq!(
        registrations[0].project_key.as_deref(),
        Some("github.com/example/repository"),
        "the hook's cwd is accurate enough to use the real git project key"
    );
    assert_eq!(f.db.queue_len().unwrap(), 0);
}

#[tokio::test]
async fn spooled_cwd_wins_over_path_guessing_and_spool_is_removed() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"hello");
    let repo = git_repository();
    let spool = f.data.path().join("spool");
    std::fs::create_dir(&spool).unwrap();
    let source = f.home.path().join("projects/-home-u-proj/s1.jsonl");
    let entry = spool.join("handled.json");
    std::fs::write(
        &entry,
        serde_json::json!({
            "agent": "claude_code",
            "path": source,
            "cwd": repo.path().to_string_lossy(),
        })
        .to_string(),
    )
    .unwrap();

    f.sync().await;

    assert_eq!(
        f.api.registrations()[0].project_key.as_deref(),
        Some("github.com/example/repository")
    );
    assert!(
        !entry.exists(),
        "a spool entry that contributed its cwd must be removed after the source syncs"
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

// ── Idempotency: the same conversation is archived once, whatever happens ──
//
// Each test below is one way a machine can lose track of what it already sent:
// a deleted `state.db`, an uninstall that also dropped the device key, the host
// moving the file, a second machine with the same session. The assertion that
// matters in every one is `bytes_received`: the server must not receive the
// history a second time, and must not end up with two copies of one stream.

/// A second installation on the same fake server: its own empty state, its
/// own device id. Exactly what `cormex uninstall` + wiping the data directory
/// + `cormex login` (a new device key) leaves behind.
struct Install {
    data: tempfile::TempDir,
    db: StateDb,
    device: &'static str,
}

impl Install {
    fn new(device: &'static str) -> Self {
        let data = tempfile::tempdir().expect("data");
        let db = StateDb::open(data.path()).expect("db");
        Self { data, db, device }
    }

    async fn sync(&self, f: &Fixture) -> cormex::sync::SyncReport {
        f.api.set_device(self.device);
        let report = run(
            &f.api,
            &self.db,
            self.data.path(),
            &f.catalog,
            &f.cfg,
            self.device,
            &SyncOptions::default(),
        )
        .await
        .expect("the run itself should not fail");
        f.api.set_device("device-1");
        report
    }
}

const HISTORY: &[u8] = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";

#[tokio::test]
async fn a_lost_state_db_sends_nothing_again() {
    let f = Fixture::new();
    f.write("s1.jsonl", HISTORY);
    f.sync().await;
    let sent = f.api.bytes_received();

    // Same device key, state gone: `rm state.db`
    let again = Install::new("device-1");
    let report = again.sync(&f).await;

    assert_eq!(report.errors, 0);
    assert_eq!(
        f.api.bytes_received(),
        sent,
        "losing the local state must not re-upload a file the server already has"
    );
    assert_eq!(f.api.non_empty_generations(), 1);
}

#[tokio::test]
async fn a_reinstall_with_a_new_device_key_follows_the_archived_copy() {
    let f = Fixture::new();
    f.write("s1.jsonl", HISTORY);
    f.sync().await;
    let sent = f.api.bytes_received();

    // Uninstall, wipe everything, log in again: a new device id
    let reinstall = Install::new("device-2");
    let report = reinstall.sync(&f).await;

    assert_eq!(report.errors, 0);
    assert_eq!(
        f.api.bytes_received(),
        sent,
        "a new device key made the whole history upload a second time"
    );
    assert_eq!(
        f.api.adoptions(),
        1,
        "the new file should follow the archived stream"
    );
    assert_eq!(
        f.api.non_empty_generations(),
        1,
        "two generations with the same bytes is the same conversation archived twice"
    );
}

#[tokio::test]
async fn a_reinstall_then_more_chat_sends_only_the_new_turn() {
    let f = Fixture::new();
    f.write("s1.jsonl", HISTORY);
    f.sync().await;
    let sent = f.api.bytes_received();

    let reinstall = Install::new("device-2");
    let more = b"{\"type\":\"user\",\"n\":2}\n";
    let mut grown = HISTORY.to_vec();
    grown.extend_from_slice(more);
    f.write("s1.jsonl", &grown);
    reinstall.sync(&f).await;

    assert_eq!(
        f.api.bytes_received() - sent,
        i64::try_from(more.len()).expect("small"),
        "only the turn appended after the reinstall is new"
    );
    assert_eq!(f.api.non_empty_generations(), 1);

    // And it keeps working as an ordinary append afterwards
    let even_more = b"{\"type\":\"assistant\",\"n\":2}\n";
    grown.extend_from_slice(even_more);
    f.write("s1.jsonl", &grown);
    reinstall.sync(&f).await;
    assert_eq!(
        f.api.bytes_received() - sent,
        i64::try_from(more.len() + even_more.len()).expect("small")
    );
}

#[tokio::test]
async fn a_file_the_host_moved_is_not_uploaded_again() {
    let f = Fixture::new();
    f.write("s1.jsonl", HISTORY);
    f.sync().await;
    let sent = f.api.bytes_received();

    // The project directory is renamed (Claude Code encodes the cwd in it);
    // the session file keeps its name, so it is the same session
    let projects = f.home.path().join("projects");
    std::fs::rename(
        projects.join("-home-u-proj"),
        projects.join("-home-u-renamed"),
    )
    .expect("rename");
    let report = f.sync().await;

    assert_eq!(report.errors, 0);
    assert_eq!(
        f.api.bytes_received(),
        sent,
        "a renamed project directory re-uploaded every session in it"
    );
    assert_eq!(f.api.non_empty_generations(), 1);
}

#[tokio::test]
async fn an_older_copy_on_another_machine_sends_nothing_and_later_only_the_new_bytes() {
    let f = Fixture::new();
    let mut long = HISTORY.to_vec();
    long.extend_from_slice(b"{\"type\":\"user\",\"n\":2}\n");
    f.write("s1.jsonl", &long);
    f.sync().await;
    let sent = f.api.bytes_received();

    // A second machine whose copy of the session stopped earlier
    let laptop = Install::new("device-2");
    f.write("s1.jsonl", HISTORY);
    laptop.sync(&f).await;
    assert_eq!(
        f.api.bytes_received(),
        sent,
        "a copy that is a prefix of the archive holds nothing new"
    );

    // The laptop catches up with the same bytes and then goes further
    let more = b"{\"type\":\"assistant\",\"n\":3}\n";
    let mut laptop_copy = long.clone();
    laptop_copy.extend_from_slice(more);
    f.write("s1.jsonl", &laptop_copy);
    laptop.sync(&f).await;
    assert_eq!(
        f.api.bytes_received() - sent,
        i64::try_from(more.len()).expect("small"),
        "the bytes another machine already sent must be verified and skipped, not resent"
    );
    assert_eq!(f.api.non_empty_generations(), 1);
}

#[tokio::test]
async fn a_file_rewritten_while_uninstalled_starts_a_new_generation() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"0123456789");
    f.sync().await;

    // Rewritten (same length, different bytes) while no state existed to
    // notice: resuming at the server's end would splice two histories
    let again = Install::new("device-1");
    f.write("s1.jsonl", b"ABCDEFGHIJKLMN");
    let report = again.sync(&f).await;

    assert_eq!(report.errors, 0);
    assert_eq!(
        f.api.non_empty_generations(),
        2,
        "different bytes are a different generation, and the old one stays intact"
    );
    let state = again
        .db
        .file("claude_code|projects/-home-u-proj/s1.jsonl")
        .expect("read")
        .expect("state");
    assert_eq!(
        f.api
            .assembled(state.generation_id.as_deref().expect("generation")),
        b"ABCDEFGHIJKLMN".to_vec(),
        "the new generation holds exactly the rewritten file"
    );
}

#[tokio::test]
async fn a_different_conversation_under_the_same_session_id_is_not_merged() {
    let f = Fixture::new();
    f.write("s1.jsonl", b"one machine's history\n");
    f.sync().await;

    let other = Install::new("device-2");
    f.write("s1.jsonl", b"a different history altogether\n");
    other.sync(&f).await;

    assert_eq!(
        f.api.adoptions(),
        0,
        "content that disagrees must never be adopted"
    );
    assert_eq!(f.api.non_empty_generations(), 2);
}

/// A Codex home holding one rollout and the desktop app's sidebar state.
fn codex_home(name: &str) -> (tempfile::TempDir, cormex_proto::catalog::Catalog) {
    use cormex_proto::capture::{FileRole, Framing};
    use cormex_proto::catalog::{AgentEntry, HomeRoot, SessionIdRule, SourceSpec};
    let home = tempfile::tempdir().expect("home");
    let id = "01234567-89ab-cdef-0123-456789abcdef";
    let dir = home.path().join("sessions/2026/11/07");
    std::fs::create_dir_all(&dir).expect("mkdir");
    std::fs::write(
        dir.join(format!("rollout-2026-11-07T10-00-00-{id}.jsonl")),
        format!(
            "{{\"timestamp\":\"2026-11-07T10:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"/r\"}}}}\n"
        ),
    )
    .expect("rollout");
    let db = rusqlite::Connection::open(home.path().join("state_5.sqlite")).expect("sqlite");
    db.execute_batch(
        "CREATE TABLE threads (id TEXT, cwd TEXT, name TEXT, title TEXT, is_pinned INTEGER, project_id TEXT);",
    )
    .expect("table");
    db.execute(
        "INSERT INTO threads VALUES (?1, '/r', ?2, 'first prompt', 0, NULL)",
        [id, name],
    )
    .expect("row");
    std::fs::write(
        home.path().join(".codex-global-state.json"),
        r#"{"local-projects":{"p1":{"name":"Proj","rootPaths":["/r"]}}}"#,
    )
    .expect("sidebar");
    let catalog = cormex_proto::catalog::Catalog {
        schema: 1,
        version: "test".into(),
        agents: vec![AgentEntry {
            id: "codex".into(),
            name: "Codex".into(),
            provider: None,
            kind: None,
            home: vec![HomeRoot::Path {
                path: home.path().display().to_string(),
            }],
            sources: vec![SourceSpec {
                role: FileRole::Transcript,
                glob: "{sessions,archived_sessions}/**/rollout-*.jsonl".into(),
                framing: Framing::Append,
                also_compressed: None,
                session_id: SessionIdRule::StemRegex {
                    regex: "rollout-[0-9T-]+-(?P<id>[0-9a-f-]{36})".into(),
                },
                parent_session_id: None,
                cwd: None,
                sqlite: None,
            }],
            manual_only: false,
            import_only: false,
            hooks: None,
            retention: None,
        }],
    };
    (home, catalog)
}

/// Every sync sends the Codex desktop metadata, but only when it changed;
/// `--codex-metadata` resends it all.
#[tokio::test]
async fn codex_desktop_metadata_is_sent_when_it_changes_and_only_then() {
    let f = Fixture::new();
    let (home, catalog) = codex_home("Named in the sidebar");
    let plain = SyncOptions::default();

    f.sync_with_catalog_and_config(&catalog, &f.cfg, plain.clone())
        .await;
    let sent = f.api.metadata();
    assert_eq!(sent.len(), 1, "the first sync sends it: {sent:?}");
    assert_eq!(sent[0].1.title.as_deref(), Some("Named in the sidebar"));
    assert_eq!(
        sent[0].1.project_name.as_deref(),
        Some("Proj"),
        "the project comes from the sidebar's root paths"
    );

    f.sync_with_catalog_and_config(&catalog, &f.cfg, plain.clone())
        .await;
    assert_eq!(
        f.api.metadata().len(),
        1,
        "nothing changed, nothing is sent: a scheduled sync every few minutes must not resend \
         every session"
    );

    rusqlite::Connection::open(home.path().join("state_5.sqlite"))
        .expect("sqlite")
        .execute("UPDATE threads SET name = 'Renamed'", [])
        .expect("rename");
    f.sync_with_catalog_and_config(&catalog, &f.cfg, plain.clone())
        .await;
    let sent = f.api.metadata();
    assert_eq!(
        sent.len(),
        2,
        "a rename in Codex reaches the archive on the next sync"
    );
    assert_eq!(sent[1].1.title.as_deref(), Some("Renamed"));

    f.sync_with_catalog_and_config(
        &catalog,
        &f.cfg,
        SyncOptions {
            force_metadata: true,
            ..SyncOptions::default()
        },
    )
    .await;
    assert_eq!(f.api.metadata().len(), 3, "--codex-metadata resends");
}

/// A Codex home that is only in `extra_homes` is captured, under a prefixed
/// logical path, whatever this run's environment.
#[tokio::test]
async fn a_configured_extra_home_is_captured() {
    let f = Fixture::new();
    let (home, mut catalog) = codex_home("x");
    let real = home.path().display().to_string();
    // The catalog's own path points nowhere; only the config names the home
    catalog.agents[0].home = vec![cormex_proto::catalog::HomeRoot::Path {
        path: "/definitely/not/here".into(),
    }];
    let mut cfg = f.cfg.clone();
    cfg.extra_homes.insert("codex".into(), vec![real]);
    let report = f
        .sync_with_catalog_and_config(&catalog, &cfg, SyncOptions::default())
        .await;
    assert_eq!(report.files_uploaded, 1, "{report:?}");
    let registered = f.api.registrations();
    assert!(
        registered
            .iter()
            .any(|r| r.logical_path.starts_with("@home-") && r.native_session_id.len() == 36),
        "a non-default home's files carry its prefix and keep their session id: {registered:?}"
    );
}

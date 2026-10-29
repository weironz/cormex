//! The capture path, end to end: CLI sync engine → real server → real database
//! and object store → bytes back out.
//!
//! Design: docs/design/session-archive-architecture.md §5.3, and P1-12's
//! acceptance: "reassembled from the server's chunks and decompressed, the
//! sha256 matches the original file, one by one".
//!
//! # Why this test exists on top of the unit tests
//!
//! Every piece is tested separately: the decision function, the reader, the
//! upload loop, the server's contiguity rules, the archive transaction. What
//! none of them can check is that the pieces **agree** — that the hash the
//! client computes is the hash the server verifies, that the offset the client
//! sends is the offset the server stores, that the bytes the server keeps
//! reassemble into the file the client read. Those are exactly the joins where
//! a mismatch is silent, because each side is internally consistent.
//!
//! # Two "devices"
//!
//! Two temporary homes with the same session content, registered under two
//! device keys. The server must keep them as two files under two devices, and
//! each must reassemble correctly — which is the property that would break if
//! the device id were dropped from the file key.

use cormex::config::Config;
use cormex::state::StateDb;
use cormex::sync::api::HttpCaptureApi;
use cormex::sync::upload::Budget;
use cormex::sync::{SyncOptions, Trigger, run};
use cormex_proto::capture::{FileRole, Framing};
use cormex_proto::catalog::{AgentEntry, Catalog, HomeRoot, SessionIdRule, SourceSpec};

/// A catalog rooted at `home`, with one append transcript source.
fn catalog(home: &std::path::Path) -> Catalog {
    Catalog {
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
                sqlite: None,
            }],
            hooks: None,
            retention: None,
        }],
    }
}

/// Everything the test needs, with its own database and object store.
struct Harness {
    st: cormexd::state::AppState,
    db: cormexd::test_support::FreshDb,
    /// Kept alive for the test's lifetime: dropping it deletes the object
    /// store's directory, and the server still has rows pointing into it
    _objects: tempfile::TempDir,
    base_url: String,
    _server: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn start() -> Option<Self> {
        let base = match std::env::var("DATABASE_URL") {
            Ok(u) if !u.is_empty() => u,
            _ => {
                eprintln!("skipped: DATABASE_URL is not set");
                return None;
            }
        };
        let db = cormexd::test_support::FreshDb::create(&base).await;
        let (st, _rt, objects) = cormexd::test_support::live_state_with_blobs(&db.url).await;

        // A real listener: the CLI's HTTP client talks to it over TCP, so the
        // wire format is exercised rather than short-circuited
        let app = cormexd::routes::router(st.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        Some(Self {
            st,
            db,
            _objects: objects,
            base_url: format!("http://{addr}"),
            _server: server,
        })
    }

    /// Mint a device key for a user, returning the plaintext.
    async fn device_key(&self, username: &str) -> String {
        let user = self
            .st
            .create_account(username, "correct horse battery")
            .await
            .expect("create the account");
        let token = self
            .st
            .access_book()
            .issue_for_test(&user, std::time::Duration::from_secs(600));
        let client = cormex::ApiClient::new(&self.base_url, &token).expect("client");
        let issued: cormex_proto::devices::DeviceKeyIssued = client
            .post_json(
                "/devices",
                &cormex_proto::devices::NewDeviceRequest {
                    label: username.into(),
                    scopes: vec!["capture:write".into()],
                    platform: None,
                },
            )
            .await
            .expect("mint a device key");
        issued.key
    }

    /// Reassemble a generation's logical bytes from the server's chunks.
    ///
    /// This is the acceptance criterion: read every chunk's object, decompress
    /// it, concatenate in offset order. Nothing here consults the client's
    /// state — the point is that the server's copy is complete.
    async fn reassemble(&self, generation_id: &str) -> Vec<u8> {
        let store = self.store_for_generation(generation_id).await;
        let chunks = store
            .chunks_overlapping(generation_id, 0, i64::MAX / 2)
            .await
            .expect("read the chunks");
        assert!(
            !chunks.is_empty(),
            "the generation has no chunks; nothing was uploaded"
        );

        let mut out = Vec::new();
        let mut expected_offset = 0;
        for c in &chunks {
            assert_eq!(
                c.byte_offset, expected_offset,
                "the chunks are not contiguous: expected an offset of {expected_offset}, got {}",
                c.byte_offset
            );
            // **Through the server's own reader**, not the filesystem: with
            // `CORMEX_SECRET_KEY` set the object is ciphertext under a per-file
            // data key, and reading the file directly would decode random
            // bytes. What this test checks is that the server's copy
            // reassembles, so the server's reader is the right one to use
            let stored = self
                .st
                .get_capture_chunk(&store, c)
                .await
                .unwrap_or_else(|e| panic!("could not read {}: {e}", c.storage_key));
            let logical = zstd::decode_all(&stored[..]).expect("the object is zstd");
            assert_eq!(
                logical.len() as i64,
                c.byte_length,
                "the object's length disagrees with the row"
            );
            assert_eq!(
                sha256_hex(&logical),
                c.content_sha256,
                "the object's bytes do not hash to the row's content_sha256"
            );
            out.extend_from_slice(&logical);
            expected_offset += c.byte_length;
        }
        out
    }

    /// The newest generation of a file, by its logical path.
    ///
    /// Resolved through the **device's own tenant**: two devices belonging to
    /// two different accounts live in two different schemas, so querying
    /// `public` would only ever see the first account's files.
    async fn generation_of(&self, device_id: &str, path: &str) -> String {
        let store = self.store_for_device(device_id).await;
        let file = store
            .capture_file(device_id, "claude_code", path)
            .await
            .expect("query")
            .unwrap_or_else(|| panic!("no capture file for {path} on {device_id}"));
        let (generation, _) = store
            .latest_generation(&file.id)
            .await
            .expect("query")
            .expect("a generation");
        generation.id
    }

    /// The tenant store a generation lives in, found through its file and
    /// session.
    async fn store_for_generation(
        &self,
        generation_id: &str,
    ) -> std::sync::Arc<cormex_store::Store> {
        // Every tenant is tried in turn: a generation id is globally unique,
        // and the schema it lives in is not recorded anywhere the test can
        // reach without one. Two tenants is the shape of this test, so this is
        // a loop over two, not a scan.
        let accounts = self.st.accounts().expect("accounts");
        let schemas: Vec<String> =
            sqlx::query_scalar("SELECT schema_name FROM cormex_auth.users ORDER BY id")
                .fetch_all(&accounts.pool)
                .await
                .expect("list the users");
        for schema in schemas {
            let name = cormex_store::SchemaName::new(&schema).expect("a legal schema name");
            let store = accounts.tenants.get(&name).await.expect("the tenant pool");
            if store
                .generation_by_id(generation_id)
                .await
                .expect("query")
                .is_some()
            {
                return std::sync::Arc::new((*store).clone());
            }
        }
        panic!("no tenant holds generation {generation_id}");
    }

    /// The tenant store a device's files live in.
    async fn store_for_device(&self, device_id: &str) -> std::sync::Arc<cormex_store::Store> {
        // The device id → user id mapping is in `cormex_auth`; the tenant
        // resolution then goes through the same path a request would
        let accounts = self.st.accounts().expect("accounts");
        let user: String =
            sqlx::query_scalar("SELECT user_id FROM cormex_auth.device_keys WHERE id = $1")
                .bind(device_id)
                .fetch_one(&accounts.pool)
                .await
                .expect("the device should exist");
        let tenant = self
            .st
            .tenant_of(&cormexd::principal::Principal::User { user_id: user })
            .await
            .expect("resolve the tenant");
        std::sync::Arc::new(tenant.store().expect("a live tenant").clone())
    }

    /// The device id behind a plaintext key.
    async fn device_id(&self, key: &str) -> String {
        let client = cormex::ApiClient::new(&self.base_url, key).expect("client");
        let me: cormex_proto::auth::WhoAmI = client.get_json("/auth/me").await.expect("whoami");
        me.device_id.expect("a device key has a device id")
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    format!("{:x}", sha2::Sha256::digest(bytes))
}

/// One device's home, with a transcript file.
fn home_with(body: &[u8]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("home");
    std::fs::create_dir_all(dir.path().join("projects/-home-u-proj")).expect("mkdir");
    std::fs::write(dir.path().join("projects/-home-u-proj/s1.jsonl"), body).expect("write");
    dir
}

/// Sync one home against the server.
async fn sync(
    home: &std::path::Path,
    data: &std::path::Path,
    key: &str,
    base_url: &str,
) -> cormex::sync::SyncReport {
    let client = cormex::ApiClient::new(base_url, key).expect("client");
    let api = HttpCaptureApi::new(client);
    let db = StateDb::open(data).expect("db");
    let cfg = Config::default();
    run(
        &api,
        &db,
        data,
        &catalog(home),
        &cfg,
        "test-device",
        &SyncOptions {
            trigger: Trigger::Manual,
            only: None,
            budget: Budget::unlimited(),
            dry_run: false,
        },
    )
    .await
    .expect("the run itself should not fail")
}

#[tokio::test]
#[ignore = "needs a database; run with just test-db"]
async fn two_devices_capture_and_every_byte_round_trips() {
    let Some(h) = Harness::start().await else {
        return;
    };

    // Two homes with the same content, under two device keys
    let body_a = b"{\"type\":\"user\",\"text\":\"hello from A\"}\n";
    let body_b = b"{\"type\":\"user\",\"text\":\"hello from B\"}\n";
    let home_a = home_with(body_a);
    let home_b = home_with(body_b);
    let data_a = tempfile::tempdir().expect("data");
    let data_b = tempfile::tempdir().expect("data");

    let key_a = h.device_key("alpha").await;
    let key_b = h.device_key("bravo").await;

    let report = sync(home_a.path(), data_a.path(), &key_a, &h.base_url).await;
    assert_eq!(report.errors, 0, "{:?}", report.last_error);
    assert_eq!(report.files_uploaded, 1);

    let report = sync(home_b.path(), data_b.path(), &key_b, &h.base_url).await;
    assert_eq!(report.errors, 0, "{:?}", report.last_error);

    // **Each device's bytes reassemble to its own file, exactly.**
    let device_a = h.device_id(&key_a).await;
    let device_b = h.device_id(&key_b).await;
    assert_ne!(device_a, device_b, "the two keys must be different devices");

    let path = "projects/-home-u-proj/s1.jsonl";
    let gen_a = h.generation_of(&device_a, path).await;
    let gen_b = h.generation_of(&device_b, path).await;
    assert_ne!(
        gen_a, gen_b,
        "two devices' files share a generation -- the device id is missing from the file key"
    );

    assert_eq!(
        sha256_hex(&h.reassemble(&gen_a).await),
        sha256_hex(body_a),
        "device A's bytes did not round trip"
    );
    assert_eq!(
        sha256_hex(&h.reassemble(&gen_b).await),
        sha256_hex(body_b),
        "device B's bytes did not round trip"
    );

    // ── Append, then sync again ──
    let appended_a = b"{\"type\":\"user\",\"text\":\"hello from A\"}\n{\"type\":\"assistant\",\"text\":\"hi\"}\n";
    std::fs::write(home_a.path().join(path), appended_a).expect("write");
    let report = sync(home_a.path(), data_a.path(), &key_a, &h.base_url).await;
    assert_eq!(report.errors, 0, "{:?}", report.last_error);

    let gen_after = h.generation_of(&device_a, path).await;
    assert_eq!(
        gen_after, gen_a,
        "an append opened a new generation; appends stay in the same one"
    );
    assert_eq!(
        sha256_hex(&h.reassemble(&gen_a).await),
        sha256_hex(appended_a),
        "the appended bytes did not round trip"
    );

    // ── Rewrite the same length, then sync again ──
    let rewritten = b"{\"type\":\"user\",\"text\":\"HELLO FROM A\"}\n{\"type\":\"assistant\",\"text\":\"hi\"}\n";
    assert_eq!(rewritten.len(), appended_a.len(), "the test's own premise");
    std::fs::write(home_a.path().join(path), rewritten).expect("write");
    let report = sync(home_a.path(), data_a.path(), &key_a, &h.base_url).await;
    assert_eq!(report.errors, 0, "{:?}", report.last_error);

    let gen_rewritten = h.generation_of(&device_a, path).await;
    assert_ne!(
        gen_rewritten, gen_a,
        "a same-length rewrite did not open a new generation"
    );
    assert_eq!(
        sha256_hex(&h.reassemble(&gen_rewritten).await),
        sha256_hex(rewritten),
        "the rewritten bytes did not round trip"
    );
    // **The old generation is intact.** Generations are branches, not versions
    assert_eq!(
        sha256_hex(&h.reassemble(&gen_a).await),
        sha256_hex(appended_a),
        "opening a new generation damaged the old one"
    );

    h.db.drop().await;
}

#[tokio::test]
#[ignore = "needs a database; run with just test-db"]
async fn a_compressed_source_round_trips_as_its_logical_bytes() {
    use std::io::Write as _;

    let Some(h) = Harness::start().await else {
        return;
    };

    let body = b"{\"type\":\"user\",\"text\":\"compressed\"}\n";
    let home = tempfile::tempdir().expect("home");
    std::fs::create_dir_all(home.path().join("projects/-p")).expect("mkdir");
    // A `.zst` written the way a host writes one for a finished file: with a
    // content size, so its logical length is knowable without decompressing
    let mut encoder = zstd::Encoder::new(Vec::new(), 3).expect("encoder");
    encoder
        .set_pledged_src_size(Some(body.len() as u64))
        .expect("pledge");
    encoder.include_contentsize(true).expect("size");
    encoder.write_all(body).expect("write");
    let frame = encoder.finish().expect("finish");
    std::fs::write(home.path().join("projects/-p/s.jsonl.zst"), &frame).expect("write");

    let mut cat = catalog(home.path());
    cat.agents[0].sources[0].also_compressed = Some(".zst".into());

    let key = h.device_key("compressed").await;
    let client = cormex::ApiClient::new(&h.base_url, &key).expect("client");
    let api = HttpCaptureApi::new(client);
    let data = tempfile::tempdir().expect("data");
    let db = StateDb::open(data.path()).expect("db");
    let report = run(
        &api,
        &db,
        data.path(),
        &cat,
        &Config::default(),
        "test-device",
        &SyncOptions {
            budget: Budget::unlimited(),
            ..SyncOptions::default()
        },
    )
    .await
    .expect("run");

    assert_eq!(report.errors, 0, "{:?}", report.last_error);
    assert_eq!(
        report.bytes,
        body.len() as i64,
        "the uploaded bytes must be the logical ones, not the frame's"
    );

    let device = h.device_id(&key).await;
    let generation = h.generation_of(&device, "projects/-p/s.jsonl").await;
    assert_eq!(
        sha256_hex(&h.reassemble(&generation).await),
        sha256_hex(body),
        "a compressed source must round trip as its logical bytes"
    );

    h.db.drop().await;
}

#[tokio::test]
#[ignore = "needs a database; run with just test-db"]
async fn a_second_sync_of_an_unchanged_file_sends_nothing() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let body = b"{\"type\":\"user\",\"text\":\"stable\"}\n";
    let home = home_with(body);
    let data = tempfile::tempdir().expect("data");
    let key = h.device_key("idempotent").await;

    let first = sync(home.path(), data.path(), &key, &h.base_url).await;
    assert_eq!(first.files_uploaded, 1);

    let second = sync(home.path(), data.path(), &key, &h.base_url).await;
    assert_eq!(
        second.files_uploaded, 0,
        "an unchanged file was uploaded again; every scheduled run would re-send the whole \
         history"
    );
    assert_eq!(second.bytes, 0);

    h.db.drop().await;
}

#[tokio::test]
#[ignore = "needs a database; run with just test-db"]
async fn a_device_cannot_write_into_another_devices_generation() {
    let Some(h) = Harness::start().await else {
        return;
    };
    let home_a = home_with(b"{\"a\":1}\n");
    let data_a = tempfile::tempdir().expect("data");
    let key_a = h.device_key("owner").await;
    sync(home_a.path(), data_a.path(), &key_a, &h.base_url).await;

    let device_a = h.device_id(&key_a).await;
    let gen_a = h
        .generation_of(&device_a, "projects/-home-u-proj/s1.jsonl")
        .await;

    // A second device tries to append into the first one's generation
    let key_b = h.device_key("intruder").await;
    let client = cormex::ApiClient::new(&h.base_url, &key_b).expect("client");
    let (status, _) = client
        .put_bytes(
            &format!("/capture/generations/{gen_a}/chunks/0"),
            &[
                (
                    cormex_proto::capture::HEADER_CONTENT_SHA256,
                    sha256_hex(b"intruder"),
                ),
                (
                    cormex_proto::capture::HEADER_OBSERVED_AT,
                    "2026-10-29T00:00:00Z".into(),
                ),
            ],
            b"intruder".to_vec(),
        )
        .await
        .expect("the request should complete");

    assert!(
        status == reqwest::StatusCode::FORBIDDEN || status == reqwest::StatusCode::NOT_FOUND,
        "a second device appended into the first device's generation (got {status}). 403 is the \
         handler's ownership check; 404 is the tenant lookup not finding the generation at all. \
         Either is correct -- what must not happen is a 200"
    );

    h.db.drop().await;
}

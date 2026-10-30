//! The backup/restore drill (P6-3), executed rather than described.
//!
//! `docs/operations.md` says what the drill is: `pg_dump` → restore into an
//! empty database → compare row counts → **replay the deletions**. A
//! description is not a drill. The steps have to be run against a real
//! database before anybody can say the backup is restorable, and the failure
//! modes are the kind that only appear when you actually run them.
//!
//! # What this covers, and what it cannot
//!
//! Covered: the database half, end to end — create, migrate, seed with an
//! **erased** episode and its tombstone, dump, restore into an empty database,
//! compare counts table by table, and assert the erased content did not come
//! back.
//!
//! Not covered: the object store. RustFS's volume is a directory and copying it
//! is `tar`; what can go wrong there is a path mistake, not a logic one, and it
//! cannot be reproduced without the production volume's layout.
//! `docs/operations.md` keeps that step manual, with the check that catches it
//! (`find … -name '*.zst*' | wc -l` against `capture_chunks`).
//!
//! # Why the erased-content assertion is the important one
//!
//! A restore brings back whatever was in the dump. If the redact cascade ran
//! **before** the dump, the content is already gone and the restore is clean.
//! If it ran **after**, the dump holds the secret and the restore resurrects
//! it — which is exactly why the operations doc says to replay the deletions
//! after restoring. This test pins the first case, because that is the one a
//! correct deployment produces.
//!
//! # Why `#[ignore]`
//!
//! It calls `pg_dump` and `pg_restore` as subprocesses and creates databases.
//! `just test-db` runs it with `--include-ignored`; a development machine
//! without those binaries skips it rather than failing.

use std::process::Command;

use sqlx::postgres::PgPoolOptions;

fn database_url() -> Option<String> {
    let _ = dotenvy::dotenv();
    std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty())
}

/// Point a connection string at a different database on the same server.
fn with_database(url: &str, database: &str) -> String {
    let (base, _) = url
        .rsplit_once('/')
        .expect("DATABASE_URL should end in /<database>");
    format!("{base}/{database}")
}

fn unique(prefix: &str) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock should not predate 1970")
        .as_millis();
    format!("{prefix}_{}_{}", std::process::id(), millis)
}

/// The major version of a `pg_dump`-style binary on `PATH`, if there is one.
///
/// The output is `pg_dump (PostgreSQL) 17.11 (Debian …)`, so the version is the
/// first dotted number in the string. `None` means the binary is absent or its
/// output does not look like a version, and both are treated the same way: do
/// not use it.
fn major_version(tool: &str) -> Option<u32> {
    let out = Command::new(tool).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .find_map(|w| w.split('.').next()?.parse::<u32>().ok())
}

/// The server's major version, from the database itself.
///
/// Asking the server rather than assuming: the CI service and a development
/// container are not the same image, and the whole point of the check is to
/// compare against what is actually running.
async fn server_major(base_url: &str) -> u32 {
    let Ok(pool) = PgPoolOptions::new()
        .max_connections(1)
        .connect(base_url)
        .await
    else {
        // Unreachable server: the drill will fail at its own connection step
        // with a better message than this one could give
        return 0;
    };
    let version: Option<String> = sqlx::query_scalar("SHOW server_version")
        .fetch_one(&pool)
        .await
        .ok();
    pool.close().await;
    version
        .and_then(|v| v.split('.').next()?.parse::<u32>().ok())
        .unwrap_or(0)
}

/// Where `pg_dump` / `pg_restore` come from.
///
/// # Why the container is tried as well as the host
///
/// On a development machine Postgres runs in Docker, and the client binaries
/// are **inside that container** — `pg_dump` on the host is often not installed
/// at all. Trying only the host would skip the drill on exactly the machine
/// that has a database to run it against, which is the machine where it is
/// most useful.
///
/// `CORMEX_PG_CONTAINER` overrides the container name; the default matches
/// `docker-compose.yml`.
///
/// # Why the host binaries are version-checked before being used
///
/// `pg_dump` **refuses to dump a server newer than itself**:
///
/// ```text
/// pg_dump: error: aborting because of server version mismatch
/// ```
///
/// A host that merely *has* `pg_dump` is therefore not enough. GitHub's
/// `ubuntu-latest` ships one, the CI service is `pgvector/pgvector:pg17`, and
/// this test failed in CI on every branch for that reason alone — while passing
/// on a development machine, where the host has no `pg_dump` at all and the
/// container path is taken.
///
/// So the host is used only when its major version is at least the server's.
/// Otherwise the container is tried, and if that is unavailable too the drill
/// **skips** rather than reporting a failure it cannot fix.
enum PgTools {
    Host,
    Container(String),
}

impl PgTools {
    /// Pick a pair of binaries that can dump the server at `server_major`.
    fn detect(server_major: u32) -> Option<Self> {
        let host_ok = ["pg_dump", "pg_restore"].iter().all(|tool| {
            Command::new(tool)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        });
        if host_ok
            && let Some(v) = major_version("pg_dump")
            && v >= server_major
        {
            return Some(Self::Host);
        }

        let name =
            std::env::var("CORMEX_PG_CONTAINER").unwrap_or_else(|_| "cormex-postgres".into());
        let in_container = Command::new("docker")
            .args(["exec", &name, "pg_dump", "--version"])
            .output()
            .is_ok_and(|o| o.status.success());
        if in_container {
            return Some(Self::Container(name));
        }

        // Neither side can dump this server. Say which side and why, so the
        // skip is diagnosable rather than mysterious
        let host = major_version("pg_dump");
        eprintln!(
            "skipped: no pg_dump can dump a {server_major} server \
             (host: {host:?}, container `{name}`: unavailable). \
             A pg_dump older than the server aborts with `server version mismatch`"
        );
        None
    }

    /// Run one tool, writing its stdout to `stdout_path` when given.
    ///
    /// # Errors
    /// The process could not be started.
    fn run(
        &self,
        tool: &str,
        args: &[&str],
        stdout_path: Option<&std::path::Path>,
    ) -> std::io::Result<std::process::Output> {
        let mut cmd = match self {
            Self::Host => Command::new(tool),
            Self::Container(name) => {
                let mut c = Command::new("docker");
                c.args(["exec", "-i", name, tool]);
                c
            }
        };
        cmd.args(args);
        match stdout_path {
            Some(path) => {
                let file = std::fs::File::create(path)?;
                cmd.stdout(std::process::Stdio::from(file));
                cmd.stderr(std::process::Stdio::piped());
                cmd.output()
            }
            None => cmd.output(),
        }
    }
}

#[tokio::test]
#[ignore = "needs a database and pg_dump/pg_restore; run with just test-db"]
async fn the_backup_restores_and_the_erased_content_stays_erased() {
    let Some(base) = database_url() else {
        eprintln!("skipped: DATABASE_URL is not set");
        return;
    };
    let Some(tools) = PgTools::detect(server_major(&base).await) else {
        return;
    };

    let src = unique("cormex_drill_src");
    let dst = unique("cormex_drill_dst");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&base)
        .await
        .expect("the admin connection should work");

    for name in [&src, &dst] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await
        .ok();
    }
    for name in [&src, &dst] {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .expect("the test role should be allowed to create a database");
    }

    let src_url = with_database(&base, &src);
    let dst_url = with_database(&base, &dst);

    // ── 1. migrate and seed the source ──
    let seeded = async {
        cormex_store::Store::migrate_global(&src_url).await?;
        let store = cormex_store::Store::connect(&src_url).await?;
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .connect(&src_url)
            .await?;

        // An episode whose content is **already** the redact placeholder, its
        // tombstone, and a fact blanked the same way. This is the state a
        // correct deployment is in after a redaction, and the state a restore
        // must reproduce — not undo
        sqlx::raw_sql(
            "INSERT INTO capture_sessions (id, agent, native_session_id)
             VALUES ('01ARZ3NDEKTSV4RRFFQ69G5FAV', 'claude_code', 'drill-session');

             INSERT INTO episodes (id, session_id, role, content, text, tsv, device_id, occurred_at)
             VALUES ('01ARZ3NDEKTSV4RRFFQ69G5FAW', 'drill-session', 'user',
                     '{\"redacted\":true}'::jsonb, NULL, NULL, 'drill', now());

             INSERT INTO entities (id, kind, name, embedding_model, device_id)
             VALUES ('01ARZ3NDEKTSV4RRFFQ69G5FAX', 'concept', 'RustFS', 'drill', 'drill');

             INSERT INTO facts (id, subject_id, predicate, object_text, statement,
                                embedding_model, source_episode_id, source_channel,
                                trust_tier, extracted_by, device_id)
             VALUES ('01ARZ3NDEKTSV4RRFFQ69G5FAY', '01ARZ3NDEKTSV4RRFFQ69G5FAX', 'uses',
                     '[redacted]', '[redacted]', 'drill', '01ARZ3NDEKTSV4RRFFQ69G5FAW',
                     'conversation', 2, 'drill', 'drill');

             INSERT INTO redactions (id, target_kind, target_id, mode, reason, actor, device_id)
             VALUES ('01ARZ3NDEKTSV4RRFFQ69G5FAZ', 'episode',
                     '01ARZ3NDEKTSV4RRFFQ69G5FAW', 'purge', 'drill', 'user', 'drill');",
        )
        .execute(&pool)
        .await?;

        store.close().await;
        pool.close().await;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    if let Err(e) = seeded {
        for name in [&src, &dst] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
            )))
            .execute(&admin)
            .await
            .ok();
        }
        panic!("seeding the drill database failed: {e}");
    }

    // ── 2. pg_dump ──
    let dump_path = std::env::temp_dir().join(format!("{src}.dump"));
    // **`--file -` inside the container, captured on the host.** A container
    // cannot write to the host's temp directory, so the dump comes out on
    // stdout and the host writes the file
    let dump = tools
        .run(
            "pg_dump",
            &["--format=custom", "--dbname", &src_url],
            Some(&dump_path),
        )
        .expect("pg_dump should run");
    assert!(
        dump.status.success(),
        "pg_dump failed: {}",
        String::from_utf8_lossy(&dump.stderr)
    );
    let dump_size = std::fs::metadata(&dump_path).map(|m| m.len()).unwrap_or(0);
    assert!(dump_size > 0, "pg_dump produced an empty file");

    // ── 3. restore into the empty database ──
    // The restore reads the dump from **stdin**, for the same reason
    let mut restore_cmd = match &tools {
        PgTools::Host => Command::new("pg_restore"),
        PgTools::Container(name) => {
            let mut c = Command::new("docker");
            c.args(["exec", "-i", name, "pg_restore"]);
            c
        }
    };
    let restore = restore_cmd
        .args(["--no-owner", "--no-acl", "--dbname", &dst_url])
        .stdin(std::process::Stdio::from(
            std::fs::File::open(&dump_path).expect("the dump should exist"),
        ))
        .output()
        .expect("pg_restore should run");
    assert!(
        restore.status.success(),
        "pg_restore failed: {}",
        String::from_utf8_lossy(&restore.stderr)
    );

    // ── 4. compare row counts ──
    let counts = async |url: &str| -> Vec<(String, i64)> {
        let pool = PgPoolOptions::new()
            .max_connections(1)
            .connect(url)
            .await
            .expect("connect");
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT 'capture_sessions', count(*) FROM capture_sessions
             UNION ALL SELECT 'episodes', count(*) FROM episodes
             UNION ALL SELECT 'entities', count(*) FROM entities
             UNION ALL SELECT 'facts', count(*) FROM facts
             UNION ALL SELECT 'redactions', count(*) FROM redactions
             ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .expect("count");
        pool.close().await;
        rows
    };
    let before = counts(&src_url).await;
    let after = counts(&dst_url).await;
    assert_eq!(
        before, after,
        "the restored database's row counts differ from the source's"
    );

    // ── 5. the erased content stayed erased ──
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&dst_url)
        .await
        .expect("connect");
    let (text, content): (Option<String>, serde_json::Value) = sqlx::query_as(
        "SELECT text, content FROM episodes WHERE id = '01ARZ3NDEKTSV4RRFFQ69G5FAW'",
    )
    .fetch_one(&pool)
    .await
    .expect("the restored episode");
    assert!(
        text.is_none(),
        "the restored episode carries its text: a restore that resurrects erased content is \
         the worst outcome this drill exists to catch"
    );
    assert_eq!(
        content,
        serde_json::json!({"redacted": true}),
        "the restored episode's content is not the placeholder"
    );

    let tombstones: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM redactions WHERE target_id = '01ARZ3NDEKTSV4RRFFQ69G5FAW'",
    )
    .fetch_one(&pool)
    .await
    .expect("the tombstone");
    assert_eq!(
        tombstones, 1,
        "the tombstone did not survive the restore -- without it, nothing can tell that this \
         episode was erased, and the next restore would have no way to know either"
    );
    pool.close().await;

    // ── clean up ──
    let _ = std::fs::remove_file(&dump_path);
    for name in [&src, &dst] {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await;
    }
    admin.close().await;
}

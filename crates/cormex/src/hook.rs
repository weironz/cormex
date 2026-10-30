//! `cormex hook <agent>` — what the host calls, and which must never fail it.
//!
//! Design: docs/design/session-archive-architecture.md §5.5.
//!
//! # The three budgets, and why each exists
//!
//! | What | Budget | Why |
//! |---|---|---|
//! | reading stdin | 200 ms, 1 MiB | a host that writes nothing must not hang the hook |
//! | writing the queue | 100 ms busy timeout | a sync holding the database must not block the host |
//! | the whole process | ~50 ms typical | `SessionEnd` gives 1.5 seconds |
//!
//! # It exits 0 always
//!
//! Not "on success" — **always**. A hook is in the host's critical path: a
//! non-zero exit can make Claude Code treat the turn as failed. The hook's job
//! is to note that a file changed; if it cannot, the next scheduled sync will
//! find the file anyway, because discovery is not driven by the queue. So every
//! failure here is logged and swallowed.
//!
//! # It does not upload
//!
//! It writes one row and spawns `cormex sync --queue` detached. Uploading in the
//! hook would put the whole sync inside the host's 1.5 seconds.

use std::io::Read as _;
use std::path::Path;
use std::time::Duration;

use crate::config::Config;
use crate::state::StateDb;

/// How much of stdin is read.
pub const MAX_STDIN: usize = 1 << 20;

/// How long reading stdin may take.
pub const STDIN_TIMEOUT: Duration = Duration::from_millis(200);

/// How long the queue write may wait for the database.
pub const QUEUE_BUSY_TIMEOUT: Duration = Duration::from_millis(100);

/// What the host told us.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookInput {
    /// The session's transcript.
    pub transcript_path: Option<String>,
    /// A sub-agent's transcript, when the event is `SubagentStop`.
    pub agent_transcript_path: Option<String>,
    /// The working directory, which is more accurate than decoding it from the
    /// path.
    pub cwd: Option<String>,
    /// The event name (`Stop`, `SessionEnd`, …).
    pub event: Option<String>,
}

/// Parse the host's stdin JSON.
///
/// **Never fails**: a host that sends something unexpected still gets a hook
/// that exits 0. What it returns is whatever fields could be recognised, which
/// may be all `None`.
#[must_use]
pub fn parse_input(raw: &[u8]) -> HookInput {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return HookInput::default();
    };
    let s = |key: &str| {
        value
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    // Claude Code puts the event name in `hook_event_name`; Codex uses `event`.
    // Both are read rather than picking one, because the difference is not
    // worth a per-agent branch in the parser
    HookInput {
        transcript_path: s("transcript_path"),
        agent_transcript_path: s("agent_transcript_path"),
        cwd: s("cwd"),
        event: s("hook_event_name").or_else(|| s("event")),
    }
}

/// Read stdin with a timeout and a size cap.
///
/// # Why a thread rather than `tokio::time::timeout`
///
/// stdin is a blocking file descriptor; wrapping it in a timeout needs either a
/// dedicated thread or a non-blocking read with a poll loop. A thread is the
/// simpler of the two, and it is **abandoned** on timeout — the process is about
/// to exit anyway, so the leaked thread costs nothing.
#[must_use]
pub fn read_stdin(timeout: Duration) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = std::io::stdin()
            .take(MAX_STDIN as u64)
            .read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    rx.recv_timeout(timeout).unwrap_or_default()
}

/// Run the hook. **Always returns `()`** — the caller exits 0 unconditionally.
///
/// # What it does, in order
///
/// 1. Parse stdin (never fails)
/// 2. Write a queue row, or a spool file when the database is locked
/// 3. Spawn `cormex sync --queue` detached
///
/// The spool file exists because the queue write has a 100 ms budget: a sync
/// holding the database must not block the host. A row that lands in the spool
/// is picked up by the next `sync --queue`, which drains both.
pub fn run_hook(agent: &str, config_dir: &Path, data_dir: &Path, stdin: &[u8]) {
    let input = parse_input(stdin);

    let cfg = Config::load(config_dir).unwrap_or_default();
    let _ = cfg;

    let at = chrono::Utc::now().to_rfc3339();
    let mut queued = false;

    for path in [
        input.transcript_path.as_deref(),
        input.agent_transcript_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if path.trim().is_empty() {
            continue;
        }
        match StateDb::open_quick(data_dir, QUEUE_BUSY_TIMEOUT) {
            Ok(db) => match db.enqueue(
                agent,
                path,
                input.cwd.as_deref(),
                input.event.as_deref(),
                &at,
            ) {
                Ok(_) => queued = true,
                Err(e) => {
                    tracing::debug!(error = %e, "could not enqueue; spooling instead");
                    spool(data_dir, agent, path, input.cwd.as_deref(), &at);
                }
            },
            Err(e) => {
                tracing::debug!(error = %e, "could not open the state database; spooling");
                spool(data_dir, agent, path, input.cwd.as_deref(), &at);
            }
        }
    }

    if queued || !spool_dir(data_dir).exists() {
        spawn_sync(data_dir);
    }
}

/// Write one spool file, when the database could not be written.
fn spool(data_dir: &Path, agent: &str, path: &str, cwd: Option<&str>, at: &str) {
    let dir = spool_dir(data_dir);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let name = format!("{}.json", cortex_like_id());
    let body = serde_json::json!({
        "agent": agent,
        "path": path,
        "cwd": cwd,
        "enqueued_at": at,
    });
    let _ = std::fs::write(dir.join(name), body.to_string());
}

/// Where spool files go.
#[must_use]
pub fn spool_dir(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("spool")
}

/// A filename-safe unique id.
///
/// A timestamp plus the process id: two hooks firing in the same millisecond
/// are different processes, so the pair is unique without a random source.
fn cortex_like_id() -> String {
    format!(
        "{}-{}",
        chrono::Utc::now().timestamp_millis(),
        std::process::id()
    )
}

/// Start `cormex sync --queue` detached.
///
/// # Why detached, and why the standard streams are null
///
/// The hook is about to exit. A child that inherited the host's stdout would
/// keep that pipe open, and the host — which is waiting for the pipe to close
/// to consider the hook finished — would wait for the sync. `setsid` on Unix
/// also takes it out of the host's process group, so a Ctrl-C in the terminal
/// does not kill a sync halfway through a chunk.
fn spawn_sync(data_dir: &Path) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("sync")
        .arg("--queue")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .env("CORMEX_DATA_DIR", data_dir);

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: `setsid` is async-signal-safe and touches no memory the
        // parent owns; it is the documented way to detach a child
        unsafe {
            cmd.pre_exec(|| {
                libc_setsid();
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let _ = cmd.spawn();
}

#[cfg(unix)]
unsafe fn libc_setsid() {
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    // SAFETY: no arguments, no return value used
    unsafe {
        setsid();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_claude_code_payload_parses() {
        let raw = br#"{"hook_event_name":"Stop","transcript_path":"/p/s.jsonl","cwd":"/p"}"#;
        let input = parse_input(raw);
        assert_eq!(input.transcript_path.as_deref(), Some("/p/s.jsonl"));
        assert_eq!(input.cwd.as_deref(), Some("/p"));
        assert_eq!(input.event.as_deref(), Some("Stop"));
    }

    #[test]
    fn a_subagent_stop_carries_both_paths() {
        let raw = br#"{"hook_event_name":"SubagentStop","transcript_path":"/p/s.jsonl",
                       "agent_transcript_path":"/p/sub.jsonl"}"#;
        let input = parse_input(raw);
        assert_eq!(input.transcript_path.as_deref(), Some("/p/s.jsonl"));
        assert_eq!(
            input.agent_transcript_path.as_deref(),
            Some("/p/sub.jsonl"),
            "a sub-agent's own transcript must be queued too, or its session is never captured"
        );
    }

    /// Bad JSON is not a failure.
    ///
    /// The hook's contract is "exit 0 whatever happens"; a parse error that
    /// propagated would break the host's turn.
    #[test]
    fn bad_json_yields_an_empty_input_rather_than_an_error() {
        assert_eq!(parse_input(b"not json"), HookInput::default());
        assert_eq!(parse_input(b""), HookInput::default());
        assert_eq!(parse_input(b"{}"), HookInput::default());
    }

    /// A payload with the wrong types yields `None` for those fields, not a
    /// panic.
    #[test]
    fn wrong_types_are_ignored() {
        let input = parse_input(br#"{"transcript_path": 42, "cwd": null}"#);
        assert_eq!(input.transcript_path, None);
        assert_eq!(input.cwd, None);
    }

    /// The hook writes a queue row when it can.
    #[test]
    fn a_hook_writes_one_queue_row_per_path() {
        let cfg = tempfile::tempdir().expect("cfg");
        let data = tempfile::tempdir().expect("data");
        let raw = br#"{"hook_event_name":"SubagentStop","transcript_path":"/p/s.jsonl",
                       "agent_transcript_path":"/p/sub.jsonl","cwd":"/p"}"#;
        run_hook("claude_code", cfg.path(), data.path(), raw);

        let db = StateDb::open(data.path()).expect("db");
        let queue = db.queue().expect("queue");
        assert_eq!(
            queue.len(),
            2,
            "both transcripts should be queued, got {queue:?}"
        );
        assert_eq!(queue[0].agent, "claude_code");
        assert_eq!(queue[0].cwd.as_deref(), Some("/p"));
    }

    /// An empty stdin writes nothing, and still returns.
    #[test]
    fn an_empty_payload_writes_nothing() {
        let cfg = tempfile::tempdir().expect("cfg");
        let data = tempfile::tempdir().expect("data");
        run_hook("claude_code", cfg.path(), data.path(), b"");
        let db = StateDb::open(data.path()).expect("db");
        assert_eq!(db.queue_len().expect("len"), 0);
    }

    /// Reading stdin respects the timeout rather than hanging.
    ///
    /// The test cannot easily give the process an empty stdin, so it asserts
    /// the function returns at all within a generous bound — the point is that
    /// it does not block forever on a descriptor nobody writes to.
    #[test]
    fn reading_stdin_returns_within_the_timeout() {
        let start = std::time::Instant::now();
        let _ = read_stdin(Duration::from_millis(50));
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "read_stdin blocked for {:?}; a host that writes nothing would then hang the hook",
            start.elapsed()
        );
    }
}

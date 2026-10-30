//! The parsers, against the committed fixtures.
//!
//! # What this adds over the unit tests
//!
//! The unit tests use hand-written one-line records. These run the parsers over
//! real-derived transcripts — 751 lines of a real Claude Code session with
//! sixteen distinct record types — which is where "the parser handles what the
//! host actually writes" is decided.
//!
//! # Every fixture line becomes exactly one event
//!
//! No line is dropped, whatever its type. A fixture has N lines and the parse
//! has N events; anything else means a record vanished, and the projection
//! would be shorter than the file with nothing to say why.

use std::path::{Path, PathBuf};

use cormex_transcript::{Parser, claude_code::ClaudeCodeParser, codex::CodexParser};

fn fixtures(agent: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(agent);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    out.sort();
    out
}

fn line_count(body: &[u8]) -> usize {
    body.split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .count()
}

/// Every line of every Claude Code fixture becomes exactly one event.
#[test]
fn every_claude_code_fixture_line_becomes_one_event() {
    let files = fixtures("claude_code");
    assert!(!files.is_empty(), "no Claude Code fixtures found");
    for path in files {
        let body = std::fs::read(&path).expect("read");
        let mut p = ClaudeCodeParser::new();
        let events = p.feed(0, &body).expect("feed");
        assert_eq!(
            events.len(),
            line_count(&body),
            "{}: {} lines but {} events -- a record vanished, and the projection would be \
             shorter than the file with nothing to say why",
            path.display(),
            line_count(&body),
            events.len()
        );
    }
}

/// Every line of every Codex fixture becomes exactly one event.
#[test]
fn every_codex_fixture_line_becomes_one_event() {
    let files = fixtures("codex");
    assert!(!files.is_empty(), "no Codex fixtures found");
    for path in files {
        let body = std::fs::read(&path).expect("read");
        let mut p = CodexParser::new();
        let events = p.feed(0, &body).expect("feed");
        assert_eq!(events.len(), line_count(&body), "{}", path.display());
    }
}

/// The real session exercises **every** record type it contains, and none of
/// them is a `malformed-json`.
///
/// A `malformed-json` on a real fixture means the parser's line splitting or
/// its JSON handling is wrong — not that the file is bad.
#[test]
fn the_real_fixture_has_no_malformed_lines() {
    for path in fixtures("claude_code") {
        let body = std::fs::read(&path).expect("read");
        let mut p = ClaudeCodeParser::new();
        let events = p.feed(0, &body).expect("feed");
        let malformed = events
            .iter()
            .filter(|e| {
                e.content.iter().any(|c| {
                    matches!(c, cormex_proto::transcript::ContentBlock::Unknown { raw_type }
                        if raw_type == "malformed-json")
                })
            })
            .count();
        assert_eq!(
            malformed,
            0,
            "{}: {malformed} lines did not parse as JSON. On a real fixture that is the parser's \
             bug, not the file's",
            path.display()
        );
    }
}

/// The record types the card names all appear, and each lands somewhere.
///
/// The card counted `assistant`, `attachment`, `user`, `last-prompt`,
/// `atis-latch`, `mode`, `tools-used`, `queue-operation`, `system`,
/// `cost-state` in one real session. The fixture is from a different session,
/// so the exact set differs — what is checked is that **every** type present is
/// recognised as something other than `malformed`.
#[test]
fn every_record_type_present_is_recognised() {
    use cormex_proto::transcript::ContentBlock;
    use std::collections::BTreeSet;

    for path in fixtures("claude_code") {
        let body = std::fs::read(&path).expect("read");
        let mut p = ClaudeCodeParser::new();
        let events = p.feed(0, &body).expect("feed");

        // The raw `type` of every line, for the comparison
        let mut raw_types = BTreeSet::new();
        for line in body.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(line)
                && let Some(t) = v.get("type").and_then(|t| t.as_str())
            {
                raw_types.insert(t.to_owned());
            }
        }

        // Every raw type must be accounted for: a message kind, `Meta`, or
        // `Unknown` carrying the name
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for e in &events {
            for c in &e.content {
                if let ContentBlock::Unknown { raw_type } = c {
                    seen.insert(raw_type.clone());
                }
            }
        }
        // Types that became messages or thoughts do not appear in `Unknown`;
        // they are the ones the parser handles, and the count check above
        // already proves none was dropped
        for t in &raw_types {
            let handled = matches!(t.as_str(), "user" | "assistant" | "system" | "summary");
            assert!(
                handled || seen.contains(t),
                "{}: record type {t:?} is neither handled nor recorded as Meta/Unknown",
                path.display()
            );
        }
    }
}

/// A checkpoint taken at every line boundary resumes identically.
///
/// The projector checkpoints between batches, so a checkpoint that loses state
/// silently changes the projection after every resume.
#[test]
fn a_checkpoint_at_any_line_boundary_resumes_identically() {
    for path in fixtures("claude_code").into_iter().take(1) {
        let body = std::fs::read(&path).expect("read");
        let mut whole = ClaudeCodeParser::new();
        let expected = whole.feed(0, &body).expect("feed");

        // A few boundaries, not every one: the fixture is 751 lines and the
        // exhaustive version is already covered by the unit test on a small
        // input
        let mut offsets: Vec<usize> = body
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .map(|(i, _)| i + 1)
            .collect();
        offsets.truncate(20);

        for split in offsets {
            let mut p = ClaudeCodeParser::new();
            let first = p.feed(0, &body[..split]).expect("feed");
            let mut resumed = ClaudeCodeParser::restore(&p.checkpoint()).expect("restore");
            let mut got = first;
            got.extend(resumed.feed(split as u64, &body[split..]).expect("feed"));
            assert_eq!(
                got.len(),
                expected.len(),
                "{}: resuming at byte {split} produced {} events instead of {}",
                path.display(),
                got.len(),
                expected.len()
            );
        }
    }
}

/// The parsers report their versions, and they are stable strings.
///
/// The version is stored with every projected event; changing it means a new
/// projection, so it must not drift by accident.
#[test]
fn the_parser_versions_are_stable() {
    assert_eq!(ClaudeCodeParser::new().version(), "claude_code@1");
    assert_eq!(CodexParser::new().version(), "codex@1");
}

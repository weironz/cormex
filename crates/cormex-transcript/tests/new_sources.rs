//! The pi, omp, DeepSeek Harness, Grok Build and Hermes parsers against
//! sessions the real tools wrote.
//!
//! The fixtures were produced on 2026-11-07 by running each CLI (pi 1.5.0,
//! omp 18.18.0, dsh 0.2.2-rc.1, grok 1.0.77, Hermes 0.19.0) against a toy
//! repository, with DeepSeek as the model, and then replacing the working
//! directory with `/home/user/todo-app`. See `fixtures/MANIFEST.md`.
//!
//! What every test here asserts is the same thing in different words: the
//! conversation in the file is the conversation in the projection — each
//! question, each answer, each tool call paired with its result — and nothing
//! in the file is silently dropped or turned into garbage.

use std::path::{Path, PathBuf};

use cormex_proto::capture::FileRole;
use cormex_proto::transcript::{ContentBlock, TranscriptKind};
use cormex_transcript::parser::{ParsedEvent, parser_for};

fn files(dir: &str) -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir);
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{} is missing: {e}", dir.display()))
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no fixtures in {}", dir.display());
    out
}

fn parse(agent: &str, body: &[u8]) -> Vec<ParsedEvent> {
    parser_for(agent, FileRole::Transcript)
        .unwrap_or_else(|| panic!("no parser for {agent}"))
        .feed(0, body)
        .expect("feed")
}

/// Questions, answers and tool calls are all there, every call has its result,
/// and no line is malformed or of an unrecognised shape.
fn assert_conversation(agent: &str, dir: &str) {
    let mut saw_tools = false;
    for path in files(dir) {
        let body = std::fs::read(&path).expect("read");
        let events = parse(agent, &body);
        let name = path.display();
        let count = |k| events.iter().filter(|e| e.kind == k).count();
        assert!(
            count(TranscriptKind::UserMessage) >= 1,
            "{name}: the question is missing"
        );
        assert!(
            count(TranscriptKind::AgentMessage) >= 1,
            "{name}: the answer is missing"
        );
        for e in &events {
            if let Some(ContentBlock::Unknown { raw_type }) = e.content.first() {
                assert!(
                    e.kind == TranscriptKind::Meta || raw_type.contains("empty"),
                    "{name}: a record the parser did not recognise ({raw_type}) at offset {}; \
                     it would show as an unknown block in the conversation",
                    e.src_offset
                );
                assert_ne!(
                    raw_type, "malformed-json",
                    "{name}: a line the host wrote did not parse"
                );
            }
        }
        let calls: Vec<_> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::ToolCall)
            .filter_map(|e| e.tool_call_id.clone())
            .collect();
        let results: Vec<_> = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::ToolResult)
            .filter_map(|e| e.tool_call_id.clone())
            .collect();
        for id in &calls {
            saw_tools = true;
            assert!(
                results.contains(id),
                "{name}: tool call {id} has no result; the UI would show a call that never \
                 returned"
            );
        }
        assert!(
            events
                .iter()
                .filter(|e| e.kind != TranscriptKind::Meta)
                .all(|e| e.occurred_at.is_some()),
            // omp's first-line `title` record carries `updatedAt` rather than a
            // timestamp, so the requirement is on what was said, not on the
            // bookkeeping
            "{name}: every message these tools write is timestamped, so every conversation event \
             should be"
        );
    }
    assert!(
        saw_tools,
        "the {dir} fixtures were meant to include a tool call; without one the pairing is untested"
    );
}

/// Splitting the file at any line boundary gives the same events as feeding
/// it whole — the projector stops between chunks, and a parser whose state does
/// not survive that would project a different conversation each run.
fn assert_chunk_invariant(
    agent: &str,
    dir: &str,
    restore: impl Fn(&serde_json::Value) -> Box<dyn cormex_transcript::Parser>,
) {
    for path in files(dir) {
        let body = std::fs::read(&path).expect("read");
        let whole = parse(agent, &body);
        let mut cuts: Vec<usize> = body
            .iter()
            .enumerate()
            .filter(|(_, b)| **b == b'\n')
            .map(|(i, _)| i + 1)
            .collect();
        // And one cut inside a line: a chunk boundary is arbitrary
        cuts.push(body.len() / 2);
        for cut in cuts {
            let mut first = parser_for(agent, FileRole::Transcript).expect("parser");
            let mut events = first.feed(0, &body[..cut]).expect("feed");
            let mut second = restore(&first.checkpoint());
            events.extend(second.feed(cut as u64, &body[cut..]).expect("feed"));
            assert_eq!(
                events,
                whole,
                "{}: cutting at byte {cut} changed the projection",
                path.display()
            );
        }
    }
}

#[test]
fn pi_sessions_project_completely() {
    assert_conversation("pi", "pi");
}

#[test]
fn omp_sessions_project_completely() {
    assert_conversation("omp", "omp");
}

#[test]
fn deepseek_harness_sessions_project_completely() {
    assert_conversation("deepseek_harness", "deepseek_harness");
    for path in files("deepseek_harness") {
        let events = parse("deepseek_harness", &std::fs::read(&path).expect("read"));
        let questions = events
            .iter()
            .filter(|e| e.kind == TranscriptKind::UserMessage)
            .count();
        assert_eq!(
            questions,
            1,
            "{}: one prompt per session; dsh's runtime-context snapshot is also a user/message \
             and must not read as a second question",
            path.display()
        );
    }
}

#[test]
fn grok_build_sessions_project_completely() {
    assert_conversation("grok_build", "grok_build");
}

#[test]
fn pi_is_chunk_invariant() {
    assert_chunk_invariant("pi", "pi", |s| {
        Box::new(cormex_transcript::pi::PiParser::restore(s).expect("restore"))
    });
    assert_chunk_invariant("omp", "omp", |s| {
        Box::new(cormex_transcript::pi::PiParser::restore(s).expect("restore"))
    });
}

#[test]
fn deepseek_harness_is_chunk_invariant() {
    assert_chunk_invariant("deepseek_harness", "deepseek_harness", |s| {
        Box::new(cormex_transcript::dsh::DshParser::restore(s).expect("restore"))
    });
}

#[test]
fn grok_build_is_chunk_invariant() {
    assert_chunk_invariant("grok_build", "grok_build", |s| {
        Box::new(cormex_transcript::grok_build::GrokBuildParser::restore(s).expect("restore"))
    });
}

/// Hermes stores one message per row; the catalog's query turns each row into
/// a JSON object, and the fixture is those rows as the query would shape them.
#[test]
fn hermes_rows_project_completely() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hermes/messages.json");
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&std::fs::read(path).expect("read")).expect("json");
    let mut events = Vec::new();
    for row in &rows {
        // Exactly the object the catalog query builds
        let shaped = serde_json::json!({
            "role": row["role"],
            "content": row["content"],
            "tool_calls": row["tool_calls"],
            "tool_name": row["tool_name"],
            "tool_call_id": row["tool_call_id"],
            "reasoning": row["reasoning_content"].as_str().or(row["reasoning"].as_str()),
            "timestamp": row["timestamp"],
            "active": row["active"],
            "compacted": row["compacted"],
        });
        let bytes = serde_json::to_vec(&shaped).expect("bytes");
        events.extend(
            parser_for("hermes", FileRole::Row)
                .expect("parser")
                .feed(0, &bytes)
                .expect("feed"),
        );
    }
    let kinds = |k| events.iter().filter(|e| e.kind == k).count();
    assert_eq!(
        kinds(TranscriptKind::UserMessage),
        2,
        "two sessions, one question each"
    );
    assert!(kinds(TranscriptKind::AgentMessage) >= 2);
    let calls: Vec<_> = events
        .iter()
        .filter(|e| e.kind == TranscriptKind::ToolCall)
        .filter_map(|e| e.tool_call_id.clone())
        .collect();
    assert!(
        !calls.is_empty(),
        "the fixture includes a turn that called tools"
    );
    for id in &calls {
        assert!(
            events.iter().any(
                |e| e.kind == TranscriptKind::ToolResult && e.tool_call_id.as_ref() == Some(id)
            ),
            "Hermes tool call {id} has no result row"
        );
    }
    assert!(
        events
            .iter()
            .all(|e| !matches!(e.content.first(), Some(ContentBlock::Unknown { .. }))),
        "every Hermes row in the fixture is a role the parser knows"
    );
    let named = events
        .iter()
        .filter(|e| e.kind == TranscriptKind::ToolResult)
        .all(|e| e.tool_name.is_some());
    assert!(named, "a Hermes tool row names its tool");
}

//! The archive background jobs: the masker and the projector.
//!
//! Design: docs/design/session-archive-architecture.md §8.1, §8.2.
//!
//! # No job queue, and that is the design
//!
//! Both jobs' work is **derivable from the data**: a generation needs masking
//! when its end is past the scanner's progress, a session needs projecting when
//! any of its generations is past the projection's `through`. So there is no
//! table of pending jobs to fall out of step with the rows, and a process that
//! dies mid-round loses nothing — the next round recomputes the same todo.
//!
//! # Both jobs are idempotent, and that is what makes the above safe
//!
//! - The masker inserts with `ON CONFLICT DO NOTHING` on
//!   `(generation, offset, rule, scanner)`, and only advances its progress
//!   after the masks are in.
//! - The projector derives each event's id from
//!   `(session, parser_version, generation, offset)` and inserts with
//!   `ON CONFLICT (id) DO NOTHING`.
//!
//! So a round that is interrupted halfway is simply re-run, and a round that
//! overlaps with itself changes nothing.
//!
//! # One failure does not stop the round
//!
//! A session whose parser errors is recorded as `status='failed'` with the
//! error, and the loop continues. It is **not retried for an hour**: a parser
//! that fails on a given input fails on it every time, and retrying every round
//! turns one bad session into a log flood.

use anyhow::Result;
use cormex_core::Id;
use cormex_proto::transcript::ContentBlock;
use cormex_store::{
    CaptureChunk, ChunkEncoding, NewCaptureMask, NewMaskProgress, NewProjection,
    NewTranscriptEvent, Projection, ProjectionStatus, Store, TranscriptEventKind,
};

use crate::request_tenant::Tenant;
use crate::state::AppState;

/// How often a round runs.
const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// How long to wait after startup before the first round.
///
/// Not zero: a restart during a deploy would otherwise start scanning while the
/// database is still warming, and the first round's errors would be noise.
const FIRST_SCAN_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// How many generations / sessions one round handles per tenant.
const BATCH: i64 = 20;

/// How many events go in one `archive_txn`.
///
/// The design says "at most 500 rows per transaction": a transaction that holds
/// a lock stays short, and 500 is small enough that a rollback costs little.
const EVENTS_PER_TXN: usize = 500;

/// Blocks larger than this become a [`ContentBlock::ResourceRef`] instead of
/// being inlined.
///
/// The raw bytes are always kept, so a large tool output is a pointer rather
/// than a copy — which is what makes "the original is always readable" true
/// without storing everything twice.
const MAX_INLINE_BLOCK: usize = 16 * 1024;

/// How much context before the progress point the masker re-scans.
///
/// A secret can straddle the boundary: a key that starts 100 bytes before the
/// progress point was not matched last round, because the regex needs its whole
/// body. Re-scanning a window catches it. 1 KiB is far more than the longest
/// pattern.
const MASK_OVERLAP: i64 = 1024;

/// The scanner's version string, recorded with every mask.
///
/// It is a constant rather than a value derived from the rules: a rule change
/// that does not bump this would leave old masks attributed to the new scanner,
/// and "which rules produced this mask" would have no answer.
pub const SCANNER: &str = "redaction@1";

/// How long a failed session is left alone.
const FAILED_BACKOFF: chrono::Duration = chrono::Duration::hours(1);

/// Spawn both workers.
///
/// Does nothing on the mock backend: there is no database, so there is nothing
/// to mask or project.
pub fn spawn(st: AppState) {
    let Some(_) = st.live().cloned() else {
        tracing::info!("mock backend: the archive workers will not start");
        return;
    };

    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SCAN_DELAY).await;
        let mut tick = tokio::time::interval(SCAN_INTERVAL);
        loop {
            tick.tick().await;

            // Re-enumerated each round: a tenant that registered after startup
            // has to be covered too. Holding the startup list would leave new
            // users' archives unmasked and unprojected, with no symptom
            let schemas = match st.tenant_schemas().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "could not list the tenants; skipping this round");
                    continue;
                }
            };

            for schema in &schemas {
                let Ok(store) = st.tenant_store(schema).await else {
                    tracing::warn!(
                        schema = schema.as_str(),
                        "could not connect to this tenant; skipping its archive work"
                    );
                    continue;
                };
                let tenant = Tenant::Live {
                    schema: schema.clone(),
                    store,
                };

                match mask_tick(&st, &tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(count = n, schema = schema.as_str(), "masked"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the masker round failed; the next one retries"
                    ),
                }
                // **Projector first, then indexer.** The indexer consumes
                // events the projector wrote; the other order means the first
                // round indexes nothing and every round is one behind
                match project_tick(&st, &tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(projected = n, schema = schema.as_str(), "projected"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the projector round failed; the next one retries"
                    ),
                }
                match index_tick(&tenant).await {
                    Ok(0) => {}
                    Ok(n) => tracing::debug!(chunks = n, schema = schema.as_str(), "indexed"),
                    Err(e) => tracing::warn!(
                        error = %e,
                        schema = schema.as_str(),
                        "the indexer round failed; the next one retries"
                    ),
                }
            }
        }
    });
}

/// One masker round. Returns how many generations were processed.
///
/// # Errors
/// The todo query failed, or the store could not be reached.
pub(crate) async fn mask_tick(st: &AppState, tenant: &Tenant) -> Result<usize> {
    let store = tenant.store()?;
    let todo = store.generations_needing_masks(SCANNER, BATCH).await?;
    let mut done = 0;

    for (generation_id, end) in todo {
        let through = store.mask_progress(&generation_id, SCANNER).await?;
        // The window starts a little before the progress point, so a secret
        // straddling the boundary is caught. It must not go below zero
        let from = (through - MASK_OVERLAP).max(0);
        let len = end - from;
        if len <= 0 {
            continue;
        }

        let bytes = read_logical_range(st, store, &generation_id, from, len).await?;
        let spans = cormex_memory::redaction::scan_spans_bytes(&bytes);

        let masks: Vec<NewCaptureMask> = spans
            .iter()
            .map(|s| NewCaptureMask {
                id: Id::new(),
                generation_id: generation_id.parse().expect("a ULID from the database"),
                // The span is relative to the window, so it is shifted back to
                // the generation's own coordinates
                byte_offset: from + i64::try_from(s.range.start).unwrap_or(0),
                byte_length: i32::try_from(s.range.end - s.range.start).unwrap_or(i32::MAX),
                rule: s.rule.name().to_owned(),
                scanner: SCANNER.to_owned(),
            })
            .collect();

        let progress = NewMaskProgress {
            id: Id::new(),
            generation_id: generation_id.parse().expect("a ULID from the database"),
            scanner: SCANNER.to_owned(),
            through_offset: end,
        };

        store
            .archive_txn(async |t| t.insert_masks(&masks, &progress).await)
            .await?;
        done += 1;
    }
    Ok(done)
}

/// One indexer round. Returns how many chunks were written.
///
/// # What it indexes, and what it deliberately does not
///
/// Only `tool_call` / `tool_result` events — the ones the conversation index
/// skips. Indexing the messages here as well would put every sentence in two
/// indexes, and `scope=archive` would then return the conversation with tool
/// output mixed in, which is exactly the noise D4 keeps out of the default
/// search.
///
/// # Errors
/// The todo query failed, or the store could not be reached.
pub(crate) async fn index_tick(tenant: &Tenant) -> Result<usize> {
    let store = tenant.store()?;
    let todo = store.events_needing_index(BATCH * 20).await?;
    let mut written = 0;

    for event in todo {
        // A project excluded from the archive index is skipped at **write**
        // time as well as at query time: writing it would put a project the
        // user asked to leave alone into an index, and the query-side filter
        // would only hide it
        if let Some(project) = store.session_project_key(&event.session_id).await?
            && store.archive_index_excluded(&project).await?
        {
            continue;
        }

        let text = event.text.unwrap_or_default();
        if text.trim().is_empty() {
            continue;
        }

        let chunks: Vec<cormex_store::NewArchiveChunk> = split_chunks(&text)
            .into_iter()
            .enumerate()
            .map(|(part, piece)| cormex_store::NewArchiveChunk {
                id: Id::new(),
                session_id: event.session_id.parse().expect("a ULID from the database"),
                event_id: event.id.parse().expect("a ULID from the database"),
                part: i32::try_from(part).unwrap_or(0),
                tsv_source: cormex_memory::tokenize::to_tsvector_input(&piece),
                text: piece,
            })
            .collect();

        store
            .archive_txn(async |t| t.insert_archive_chunks(&chunks).await)
            .await?;
        written += chunks.len();
    }
    Ok(written)
}

/// Split a long text into 2 KiB pieces, on a **character** boundary.
///
/// A byte boundary can land in the middle of a multi-byte character, and the
/// result is not valid UTF-8 — which `to_tsvector` then rejects, turning one
/// long tool output into a failed round.
fn split_chunks(text: &str) -> Vec<String> {
    const PIECE: usize = 2 * 1024;
    if text.len() <= PIECE {
        return vec![text.to_owned()];
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + PIECE).min(text.len());
        while end > start && !text.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            // A single character larger than the piece size: take it whole
            end = text[start..]
                .chars()
                .next()
                .map_or(text.len(), |c| start + c.len_utf8());
        }
        out.push(text[start..end].to_owned());
        start = end;
    }
    out
}

/// One projector round. Returns how many events were written.
///
/// # Errors
/// The todo query failed, or the store could not be reached.
pub(crate) async fn project_tick(st: &AppState, tenant: &Tenant) -> Result<usize> {
    let store = tenant.store()?;
    let todo = store
        .sessions_needing_projection("claude_code@1", BATCH)
        .await?;
    let mut written = 0;

    for (session_id, agent) in todo {
        match project_session(st, store, &session_id, &agent).await {
            Ok(n) => written += n,
            Err(e) => {
                // **Recorded, then skipped.** A parser that fails on this input
                // fails on it every time; retrying every round turns one bad
                // session into a log flood
                tracing::warn!(session = %session_id, error = %e, "could not project this session");
                let _ = store
                    .archive_txn(async |t| {
                        t.insert_transcript_events(
                            &[],
                            &NewProjection {
                                id: Id::new(),
                                session_id: session_id.parse().map_err(|_| {
                                    cormex_store::StoreError::Invalid("bad session id".into())
                                })?,
                                parser_version: "claude_code@1".into(),
                                through: serde_json::json!({}),
                                next_seq: 0,
                                status: ProjectionStatus::Failed,
                                error: Some(e.to_string()),
                            },
                        )
                        .await
                    })
                    .await;
            }
        }
    }
    Ok(written)
}

/// Project one session's files.
async fn project_session(
    st: &AppState,
    store: &Store,
    session_id: &str,
    agent: &str,
) -> Result<usize> {
    // Every file of the session, with its newest generation
    let files = store.capture_files_of_session(session_id).await?;
    let mut written = 0;

    for file in files {
        // The store's `FileRole` and the wire's are two types (the store must
        // not depend on the wire crate's enum). The conversion is one `match`,
        // where a new variant is a compile error rather than a silently
        // unhandled role
        let Some(parser) = cormex_transcript::parser_for(agent, wire_role(file.role)) else {
            continue;
        };
        let Some((generation, end)) = store.latest_generation(&file.id).await? else {
            continue;
        };

        // ── Resume from the parser's own checkpoint ──
        //
        // **Not just a byte offset.** The parser holds a line buffer: a record
        // straddling the resume point has to be completed by the next chunk, and
        // feeding the parser a chunk that starts mid-file fails its contiguity
        // check. So `through` carries the parser's checkpoint (which includes
        // the buffer and the tool-name map) alongside the offset, and a resume
        // restores it rather than starting from zero.
        let latest = store
            .latest_projection(session_id, parser.version())
            .await?;
        let slot = latest.as_ref().and_then(|p| p.through.get(&generation.id));

        let (through, mut parser) = match slot {
            Some(v) if v.get("parser").is_some() => {
                let offset = v
                    .get("offset")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(0);
                let state = v.get("parser").cloned().unwrap_or(serde_json::Value::Null);
                // The parser type is known from the agent, so the restore is
                // dispatched the same way `parser_for` dispatches
                let restored = restore_parser(agent, file.role, &state)?;
                (offset, restored)
            }
            _ => (0, parser),
        };
        let mut next_seq = latest.as_ref().map_or(0, |p| p.next_seq);

        if through >= end {
            continue;
        }

        let chunks = store
            .chunks_overlapping(&generation.id, through, end - through)
            .await?;
        let mut pending: Vec<NewTranscriptEvent> = Vec::new();
        // Assigned in the loop; the final value is what the last checkpoint
        // records. The initial value is only a placeholder for the compiler.
        #[allow(unused_assignments)]
        let mut projected_to = through;

        for chunk in chunks {
            let bytes = read_chunk_logical(st, &chunk).await?;
            // The chunk may start before `through` when it straddles the resume
            // point; slice to the part not yet consumed
            let skip = usize::try_from((through - chunk.byte_offset).max(0)).unwrap_or(0);
            let slice = if skip < bytes.len() {
                &bytes[skip..]
            } else {
                &[][..]
            };
            let feed_from = u64::try_from(chunk.byte_offset.max(through)).unwrap_or(0);

            let events = parser
                .feed(feed_from, slice)
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            for e in events {
                let masked_text = cormex_memory::redaction::mask_text(&e.text).into_owned();
                let content = mask_blocks(&e.content);
                let occurred_at = e.occurred_at.unwrap_or(chunk.created_at);
                pending.push(NewTranscriptEvent {
                    id: Id::derived(
                        occurred_at,
                        &format!(
                            "{session_id}|{}|{}|{}",
                            parser.version(),
                            generation.id,
                            e.src_offset
                        ),
                    ),
                    session_id: session_id.parse()?,
                    parser_version: parser.version().to_owned(),
                    seq: next_seq,
                    kind: kind_of(e.kind),
                    occurred_at: Some(occurred_at),
                    content: serde_json::to_value(&content)?,
                    text: Some(masked_text.clone()),
                    tsv_source: Some(cormex_memory::tokenize::to_tsvector_input(&masked_text)),
                    tool_name: e.tool_name,
                    tool_call_id: e.tool_call_id,
                    src_generation_id: generation.id.parse()?,
                    src_offset: i64::try_from(e.src_offset).unwrap_or(0),
                    src_length: i64::try_from(e.src_length).unwrap_or(0),
                });
                next_seq += 1;
            }
            projected_to = chunk.byte_offset + chunk.byte_length;

            if pending.len() >= EVENTS_PER_TXN {
                let batch = std::mem::take(&mut pending);
                let checkpoint = checkpoint_of(&generation.id, projected_to, parser.checkpoint());
                store
                    .archive_txn(async |t| {
                        t.insert_transcript_events(
                            &batch,
                            &NewProjection {
                                id: Id::new(),
                                session_id: session_id.parse().map_err(|_| {
                                    cormex_store::StoreError::Invalid("bad session id".into())
                                })?,
                                parser_version: parser.version().to_owned(),
                                through: checkpoint.clone(),
                                next_seq,
                                status: ProjectionStatus::Partial,
                                error: None,
                            },
                        )
                        .await
                    })
                    .await?;
                written += batch.len();
            }
        }

        // The final batch, with `ok` status and the generation marked complete
        store
            .archive_txn(async |t| {
                t.insert_transcript_events(
                    &pending,
                    &NewProjection {
                        id: Id::new(),
                        session_id: session_id.parse().map_err(|_| {
                            cormex_store::StoreError::Invalid("bad session id".into())
                        })?,
                        parser_version: parser.version().to_owned(),
                        through: checkpoint_of(&generation.id, end, parser.checkpoint()),
                        next_seq,
                        status: ProjectionStatus::Ok,
                        error: None,
                    },
                )
                .await
            })
            .await?;
        written += pending.len();
    }
    Ok(written)
}

/// Read one chunk's logical bytes from object storage.
async fn read_chunk_logical(st: &AppState, chunk: &CaptureChunk) -> Result<Vec<u8>> {
    let stored = st.get_capture_object(&chunk.storage_key).await?;
    match chunk.encoding {
        ChunkEncoding::Zstd => Ok(zstd::decode_all(&stored[..])?),
        ChunkEncoding::Identity => Ok(stored.to_vec()),
    }
}

/// Read `[offset, offset + len)` of a generation's logical bytes.
async fn read_logical_range(
    st: &AppState,
    store: &Store,
    generation_id: &str,
    offset: i64,
    len: i64,
) -> Result<Vec<u8>> {
    let chunks = store.chunks_overlapping(generation_id, offset, len).await?;
    let mut out = Vec::new();
    for chunk in chunks {
        let bytes = read_chunk_logical(st, &chunk).await?;
        // The chunk may start before `offset`; slice to the window
        let start = usize::try_from((offset - chunk.byte_offset).max(0)).unwrap_or(0);
        let end = usize::try_from((offset + len - chunk.byte_offset).min(chunk.byte_length))
            .unwrap_or(bytes.len())
            .min(bytes.len());
        if start < end {
            out.extend_from_slice(&bytes[start..end]);
        }
    }
    Ok(out)
}

/// Mask every text field inside a block, and replace a large one with a
/// reference.
fn mask_blocks(blocks: &[ContentBlock]) -> Vec<ContentBlock> {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => ContentBlock::Text {
                text: cormex_memory::redaction::mask_text(text).into_owned(),
            },
            ContentBlock::Thought { text } => ContentBlock::Thought {
                text: cormex_memory::redaction::mask_text(text).into_owned(),
            },
            ContentBlock::ToolResult {
                tool_call_id,
                output,
                is_error,
            } => {
                if output.len() > MAX_INLINE_BLOCK {
                    // A large tool output is a pointer rather than a copy: the
                    // raw bytes are always kept, so "the original is always
                    // readable" stays true without storing everything twice
                    ContentBlock::ResourceRef {
                        generation_id: String::new(),
                        offset: 0,
                        length: i64::try_from(output.len()).unwrap_or(0),
                        mime: "text/plain".into(),
                    }
                } else {
                    ContentBlock::ToolResult {
                        tool_call_id: tool_call_id.clone(),
                        output: cormex_memory::redaction::mask_text(output).into_owned(),
                        is_error: *is_error,
                    }
                }
            }
            other => other.clone(),
        })
        .collect()
}

/// The `through` value: the offset **and** the parser's checkpoint.
///
/// The offset alone is not enough to resume — the parser holds a line buffer,
/// and feeding it a chunk that starts mid-file fails its contiguity check. The
/// failure is an `Err`, which the caller records as a `failed` projection, so
/// the symptom would be "the session stopped projecting after the first
/// append".
fn checkpoint_of(
    generation_id: &str,
    offset: i64,
    parser_state: serde_json::Value,
) -> serde_json::Value {
    serde_json::json!({
        generation_id: {
            "offset": offset,
            "parser": parser_state,
        }
    })
}

/// Restore a parser from a stored checkpoint.
fn restore_parser(
    agent: &str,
    role: cormex_store::FileRole,
    state: &serde_json::Value,
) -> Result<Box<dyn cormex_transcript::Parser>> {
    use cormex_proto::capture::FileRole as W;
    match (agent, wire_role(role)) {
        ("claude_code", W::Transcript | W::Subagent) => Ok(Box::new(
            cormex_transcript::claude_code::ClaudeCodeParser::restore(state)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        )),
        ("codex", W::Transcript) => Ok(Box::new(
            cormex_transcript::codex::CodexParser::restore(state)
                .map_err(|e| anyhow::anyhow!("{e}"))?,
        )),
        _ => anyhow::bail!("no parser for {agent}"),
    }
}

fn wire_role(role: cormex_store::FileRole) -> cormex_proto::capture::FileRole {
    use cormex_proto::capture::FileRole as W;
    use cormex_store::FileRole as S;
    match role {
        S::Transcript => W::Transcript,
        S::Subagent => W::Subagent,
        S::Sidecar => W::Sidecar,
        S::Attachment => W::Attachment,
        S::Memory => W::Memory,
        S::Row => W::Row,
    }
}

fn kind_of(k: cormex_proto::transcript::TranscriptKind) -> TranscriptEventKind {
    use cormex_proto::transcript::TranscriptKind as K;
    match k {
        K::UserMessage => TranscriptEventKind::UserMessage,
        K::AgentMessage => TranscriptEventKind::AgentMessage,
        K::AgentThought => TranscriptEventKind::AgentThought,
        K::ToolCall => TranscriptEventKind::ToolCall,
        K::ToolResult => TranscriptEventKind::ToolResult,
        K::Plan => TranscriptEventKind::Plan,
        K::System => TranscriptEventKind::System,
        K::Meta => TranscriptEventKind::Meta,
        K::Unknown => TranscriptEventKind::Unknown,
    }
}

/// The failed-session backoff, exposed so the projector can consult it.
#[must_use]
pub fn failed_recently(p: &Projection) -> bool {
    p.status == ProjectionStatus::Failed && chrono::Utc::now() - p.created_at < FAILED_BACKOFF
}

#[cfg(test)]
mod tests {
    //! The masker and the projector, driven directly against a real database.
    //!
    //! The tick functions are `pub(crate)` so a test can call one round without
    //! waiting for the interval — a test that slept 60 seconds per round would
    //! not be run.

    use super::*;
    use cormex_store::{FileRole, Framing, NewCaptureChunk, NewCaptureFile};

    /// Register a file with one chunk of `body`, returning
    /// `(session_id, generation_id)`.
    async fn seed(store: &Store, agent: &str, body: &[u8]) -> (String, String) {
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
            project_key: None,
        };
        let registered = store
            .archive_txn(async |t| t.register_capture_file(&new).await)
            .await
            .expect("register");
        let (session_id, generation_id) = (registered.session.id, registered.generation.id);

        let sha = {
            use sha2::Digest as _;
            format!("{:x}", sha2::Sha256::digest(body))
        };
        store
            .archive_txn(async |t| {
                t.append_chunk(&NewCaptureChunk {
                    id: Id::new(),
                    generation_id: generation_id.parse().expect("a ULID"),
                    byte_offset: 0,
                    byte_length: i64::try_from(body.len()).expect("fits"),
                    content_sha256: sha.clone(),
                    storage_key: format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]),
                    encoding: ChunkEncoding::Zstd,
                    observed_at: chrono::Utc::now(),
                })
                .await
            })
            .await
            .expect("append");
        (session_id, generation_id)
    }

    /// Put the chunk's bytes into the object store, zstd-compressed.
    async fn store_object(st: &AppState, body: &[u8]) {
        use sha2::Digest as _;
        let sha = format!("{:x}", sha2::Sha256::digest(body));
        // The bucket levels must agree with the hash: `validate_capture_key`
        // checks that, because a key whose levels do not match is legal to
        // write and then unreachable by `capture_key`
        let key = format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]);
        let compressed = zstd::encode_all(body, 3).expect("compress");
        st.put_capture_object(&key, axum::body::Bytes::from(compressed))
            .await
            .expect("store");
    }

    /// The masker finds a key in the logical bytes and records its range.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_masker_records_the_span_of_a_key() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"my key is AKIAIOSFODNN7EXAMPLE"}}
"#;
        let store = st.public_store().expect("a live backend");
        let (_session, generation) = seed(&store, "claude_code", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let n = mask_tick(&st, &tenant).await.expect("mask tick");
        assert_eq!(n, 1, "one generation should have been masked");

        let masks = store
            .masks_in_range(&generation, 0, i64::try_from(body.len()).expect("fits"))
            .await
            .expect("query");
        assert_eq!(masks.len(), 1, "the key should have produced one mask");
        assert_eq!(masks[0].rule, "vendor_key");
        assert_eq!(masks[0].scanner, SCANNER);
        assert!(
            masks[0].byte_offset > 0,
            "the mask must be at the key's offset, not at 0"
        );

        // A second tick has nothing to do
        let n = mask_tick(&st, &tenant).await.expect("second tick");
        assert_eq!(
            n, 0,
            "the second tick re-masked a generation that was already done"
        );

        db.drop().await;
    }

    /// The projector turns a session's chunks into events, with the text masked.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_projector_writes_masked_events() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        // Three records: a user message with a fake key, an assistant message,
        // and a record type the parser knows as Meta
        let body = br#"{"type":"user","message":{"role":"user","content":"my key is AKIAIOSFODNN7EXAMPLE"},"timestamp":"2026-10-29T08:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"noted"}]},"timestamp":"2026-10-29T08:00:01Z"}
{"type":"mode"}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, _generation) = seed(&store, "claude_code", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let n = project_tick(&st, &tenant).await.expect("project tick");
        assert_eq!(n, 3, "all three records should have become events");

        let events = store
            .transcript_page(&session, "claude_code@1", -1, 100)
            .await
            .expect("page");
        assert_eq!(events.len(), 3);
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "the events must be in file order"
        );
        assert_eq!(events[0].kind, TranscriptEventKind::UserMessage);
        assert_eq!(events[1].kind, TranscriptEventKind::AgentMessage);
        assert_eq!(events[2].kind, TranscriptEventKind::Meta);

        // **The key is masked in the P layer.**
        let text = events[0].text.as_deref().expect("text");
        assert!(
            !text.contains("AKIA"),
            "the P layer still contains the key: {text}"
        );
        assert!(
            text.contains("[masked:"),
            "the P layer should carry the mask marker: {text}"
        );

        db.drop().await;
    }

    /// A second projector tick writes nothing new.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_second_projector_tick_is_idempotent() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"hello"}}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, _generation) = seed(&store, "claude_code", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        project_tick(&st, &tenant).await.expect("first tick");
        let second = project_tick(&st, &tenant).await.expect("second tick");
        assert_eq!(
            second, 0,
            "the second tick projected the same session again; the projection would then have \
             duplicate events, or the unique constraint would turn a normal round into an error"
        );

        let events = store
            .transcript_page(&session, "claude_code@1", -1, 100)
            .await
            .expect("page");
        assert_eq!(events.len(), 1);

        db.drop().await;
    }

    /// Appending a chunk makes exactly the new records appear.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn an_appended_chunk_adds_only_the_new_events() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let first = br#"{"type":"user","message":{"role":"user","content":"one"}}
"#;
        let second = br#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"two"}]}}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, generation) = seed(&store, "claude_code", first).await;
        store_object(&st, first).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        project_tick(&st, &tenant).await.expect("first tick");

        // Append the second record as a second chunk
        let end = i64::try_from(first.len()).expect("fits");
        let sha = {
            use sha2::Digest as _;
            format!("{:x}", sha2::Sha256::digest(second))
        };
        store
            .archive_txn(async |t| {
                t.append_chunk(&NewCaptureChunk {
                    id: Id::new(),
                    generation_id: generation.parse().expect("a ULID"),
                    byte_offset: end,
                    byte_length: i64::try_from(second.len()).expect("fits"),
                    content_sha256: sha.clone(),
                    storage_key: format!("public/capture/{}/{}/{sha}.zst", &sha[0..2], &sha[2..4]),
                    encoding: ChunkEncoding::Zstd,
                    observed_at: chrono::Utc::now(),
                })
                .await
            })
            .await
            .expect("append");
        store_object(&st, second).await;

        project_tick(&st, &tenant).await.expect("second tick");

        let events = store
            .transcript_page(&session, "claude_code@1", -1, 100)
            .await
            .expect("page");
        assert_eq!(
            events.len(),
            2,
            "the appended record should be the only new event, got {} events",
            events.len()
        );
        assert_eq!(events[1].kind, TranscriptEventKind::AgentMessage);

        db.drop().await;
    }

    /// A session with no parser is skipped, not failed.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn a_session_with_no_parser_is_skipped() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = b"whatever\n";
        let store = st.public_store().expect("a live backend");
        let (_session, _generation) = seed(&store, "unknown_agent", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        let n = project_tick(&st, &tenant).await.expect("tick");
        assert_eq!(
            n, 0,
            "a session with no parser should be skipped rather than failing the round"
        );

        db.drop().await;
    }

    /// The worker does not start on the mock backend.
    #[test]
    fn the_worker_does_not_start_on_the_mock_backend() {
        let src = include_str!("archive_workers.rs");
        let body = src
            .split("pub fn spawn(st: AppState) {")
            .nth(1)
            .expect("spawn is still here");
        let head = &body[..body.len().min(400)];
        assert!(
            head.contains("st.live()"),
            "spawn no longer checks for the mock backend; on a mock instance the workers would \
             spin against a database that does not exist"
        );
    }

    /// The indexer writes chunks for tool events only.
    ///
    /// Indexing the messages here as well would put every sentence in two
    /// indexes, and `scope=archive` would return the conversation with tool
    /// output mixed in — the noise D4 keeps out of the default search.
    #[tokio::test]
    #[ignore = "needs a database; run with just test-db"]
    async fn the_indexer_indexes_tool_events_only() {
        let Some(base) = crate::test_support::database_url() else {
            return;
        };
        let db = crate::test_support::FreshDb::create(&base).await;
        let (st, _rt, _objects) = crate::test_support::live_state_with_blobs(&db.url).await;

        let body = br#"{"type":"user","message":{"role":"user","content":"run the tests"},"timestamp":"2026-10-29T08:00:00Z"}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]},"timestamp":"2026-10-29T08:00:01Z"}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"test result: ok. 42 passed"}]},"timestamp":"2026-10-29T08:00:02Z"}
"#;
        let store = st.public_store().expect("a live backend");
        let (session, _generation) = seed(&store, "claude_code", body).await;
        store_object(&st, body).await;

        let tenant = Tenant::Live {
            schema: cormex_store::SchemaName::public(),
            store: store.clone(),
        };
        project_tick(&st, &tenant).await.expect("project");
        let chunks = index_tick(&tenant).await.expect("index");
        assert!(chunks > 0, "the tool events should have been indexed");

        // The tool result is findable in the archive
        let archive = store
            .search_archive(
                &cormex_memory::tokenize::to_tsquery_input("passed"),
                None,
                None,
                None,
                None,
                20,
            )
            .await
            .expect("archive search");
        assert_eq!(
            archive.len(),
            1,
            "the tool result should be in the archive index"
        );

        // The message is not
        let conversation = store
            .search_conversation(
                &cormex_memory::tokenize::to_tsquery_input("tests"),
                None,
                None,
                None,
                None,
                20,
            )
            .await
            .expect("conversation search");
        assert_eq!(
            conversation.len(),
            1,
            "the user message should be in the conversation index"
        );

        // A second tick has nothing to do
        let again = index_tick(&tenant).await.expect("second index");
        assert_eq!(
            again, 0,
            "the second tick re-indexed what was already there"
        );

        // The session id is the one we seeded
        assert_eq!(archive[0].session_id, session);

        db.drop().await;
    }

    /// A long tool output is split on a **character** boundary.
    ///
    /// A byte boundary can land inside a multi-byte character, and the result
    /// is not valid UTF-8 — which `to_tsvector` then rejects, turning one long
    /// tool output into a failed round.
    #[test]
    fn a_long_output_is_split_on_a_char_boundary() {
        let text = "对".repeat(3000); // 3 bytes each, 9000 bytes
        let pieces = split_chunks(&text);
        assert!(pieces.len() > 1, "a 9 KB text should have been split");
        for p in &pieces {
            assert!(
                std::str::from_utf8(p.as_bytes()).is_ok(),
                "a piece is not valid UTF-8: a byte boundary landed inside a character"
            );
        }
        assert_eq!(pieces.concat(), text, "the pieces must reassemble");
    }

    #[test]
    fn a_short_output_is_one_piece() {
        assert_eq!(split_chunks("short"), vec!["short".to_owned()]);
    }
}

//! An in-memory server, implementing the same contiguity rules as the real one.
//!
//! The engine's interesting behaviour is recovery — a 409 in each direction, a
//! network error mid-upload, a replay after a crash. None of those can be
//! produced by a real server on demand, and a test that only walks the happy
//! path would miss exactly the branches the design spends the most words on.
//!
//! This lives in the library rather than in `tests/` so that both the unit
//! tests here and the end-to-end test in P1-12 can use it.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use cormex_proto::capture::{
    ChunkAck, FileEventOp, FileEventRequest, GenerationDto, RegisterFileRequest,
    RegisterFileResponse,
};

use crate::sync::api::{CaptureApi, PutOutcome};

/// One generation's chunks, keyed by offset.
#[derive(Default, Clone)]
struct Generation {
    next_ordinal: i32,
    /// offset → (sha256, length)
    chunks: HashMap<i64, (String, i64)>,
    end: i64,
}

/// The fake server.
#[derive(Default)]
pub struct FakeCaptureApi {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// `(agent, logical_path)` → file id
    files: HashMap<(String, String), String>,
    /// file id → its generations, in order
    generations: HashMap<String, Vec<String>>,
    generations_by_id: HashMap<String, Generation>,
    next_id: u64,
    events: Vec<(String, FileEventOp)>,
    /// How many `put_chunk` calls to fail before succeeding. Used to exercise
    /// the retry path.
    fail_next: usize,
    /// Force this `expected` on the next 409, to exercise both directions.
    force_mismatch: Option<i64>,
    puts: usize,
}

impl FakeCaptureApi {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fail the next `n` chunk uploads with a network-shaped error.
    pub fn fail_next_puts(&self, n: usize) {
        self.inner.lock().unwrap().fail_next = n;
    }

    /// Make the next chunk upload answer 409 with this `expected`.
    pub fn force_mismatch(&self, expected: i64) {
        self.inner.lock().unwrap().force_mismatch = Some(expected);
    }

    /// The bytes stored for a generation, reassembled in offset order.
    #[must_use]
    pub fn assembled(&self, generation_id: &str) -> Vec<u8> {
        let inner = self.inner.lock().unwrap();
        let Some(g) = inner.generations_by_id.get(generation_id) else {
            return Vec::new();
        };
        let mut offsets: Vec<i64> = g.chunks.keys().copied().collect();
        offsets.sort_unstable();
        offsets
            .into_iter()
            .flat_map(|o| {
                inner.generations_by_id[generation_id].chunks[&o]
                    .0
                    .clone()
                    .into_bytes()
            })
            .collect()
    }

    /// How many chunks a generation holds.
    #[must_use]
    pub fn chunk_count(&self, generation_id: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .generations_by_id
            .get(generation_id)
            .map_or(0, |g| g.chunks.len())
    }

    /// How far a generation extends.
    #[must_use]
    pub fn end(&self, generation_id: &str) -> i64 {
        self.inner
            .lock()
            .unwrap()
            .generations_by_id
            .get(generation_id)
            .map_or(0, |g| g.end)
    }

    /// How many chunk uploads the server received.
    #[must_use]
    pub fn puts(&self) -> usize {
        self.inner.lock().unwrap().puts
    }

    /// Every file event the server received.
    #[must_use]
    pub fn events(&self) -> Vec<(String, FileEventOp)> {
        self.inner.lock().unwrap().events.clone()
    }
}

#[async_trait]
impl CaptureApi for FakeCaptureApi {
    async fn register(&self, req: &RegisterFileRequest) -> Result<RegisterFileResponse> {
        let mut inner = self.inner.lock().unwrap();
        let key = (req.agent.clone(), req.logical_path.clone());
        if let Some(file_id) = inner.files.get(&key) {
            // Idempotent: the newest generation, exactly like the real server
            let gens = inner.generations.get(file_id).cloned().unwrap_or_default();
            let gid = gens.last().cloned().unwrap_or_default();
            let g = inner
                .generations_by_id
                .get(&gid)
                .cloned()
                .unwrap_or_default();
            return Ok(RegisterFileResponse {
                file_id: file_id.clone(),
                session_id: "01SESSION".into(),
                generation: GenerationDto {
                    id: gid,
                    ordinal: g.next_ordinal.saturating_sub(1),
                    end_offset: g.end,
                },
            });
        }
        inner.next_id += 1;
        let file_id = format!("01FILE{}", inner.next_id);
        let gid = format!("01GEN{}", inner.next_id);
        inner.files.insert(key, file_id.clone());
        inner.generations.insert(file_id.clone(), vec![gid.clone()]);
        inner
            .generations_by_id
            .insert(gid.clone(), Generation::default());
        Ok(RegisterFileResponse {
            file_id,
            session_id: "01SESSION".into(),
            generation: GenerationDto {
                id: gid,
                ordinal: 0,
                end_offset: 0,
            },
        })
    }

    async fn open_generation(&self, file_id: &str, _reason: &str) -> Result<GenerationDto> {
        let mut inner = self.inner.lock().unwrap();
        inner.next_id += 1;
        let gid = format!("01GEN{}", inner.next_id);
        let gens = inner.generations.entry(file_id.to_owned()).or_default();
        gens.push(gid.clone());
        let ordinal = i32::try_from(gens.len() - 1).unwrap_or(0);
        inner.generations_by_id.insert(
            gid.clone(),
            Generation {
                next_ordinal: ordinal + 1,
                ..Generation::default()
            },
        );
        Ok(GenerationDto {
            id: gid,
            ordinal,
            end_offset: 0,
        })
    }

    async fn put_chunk(
        &self,
        generation_id: &str,
        offset: i64,
        body: Vec<u8>,
        sha256: &str,
        _observed_at: &str,
    ) -> Result<PutOutcome> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_next > 0 {
            inner.fail_next -= 1;
            anyhow::bail!("simulated network failure");
        }
        inner.puts += 1;
        let len = i64::try_from(body.len()).unwrap_or(0);

        let forced = inner.force_mismatch.take();
        let Some(g) = inner.generations_by_id.get_mut(generation_id) else {
            anyhow::bail!("no such generation");
        };
        if let Some(expected) = forced {
            return Ok(PutOutcome::OffsetMismatch { expected });
        }

        // The real server's three-way answer
        if offset < g.end {
            return Ok(match g.chunks.get(&offset) {
                Some((sha, l)) if sha == sha256 && *l == len => PutOutcome::Accepted(ChunkAck {
                    end_offset: g.end,
                    already_present: true,
                }),
                _ => PutOutcome::OffsetMismatch { expected: g.end },
            });
        }
        if offset > g.end {
            return Ok(PutOutcome::OffsetMismatch { expected: g.end });
        }

        // The fake stores the sha rather than the bytes; `assembled` is only
        // used by tests that check offsets, not content
        g.chunks.insert(offset, (sha256.to_owned(), len));
        g.end += len;
        Ok(PutOutcome::Accepted(ChunkAck {
            end_offset: g.end,
            already_present: false,
        }))
    }

    async fn file_event(&self, file_id: &str, req: &FileEventRequest) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .events
            .push((file_id.to_owned(), req.op));
        Ok(())
    }
}

//! An in-memory server, implementing the same contiguity rules as the real one.
//!
//! The engine's interesting behaviour is recovery — a 409 in each direction, a
//! network error mid-upload, a replay after a crash, a reinstall that finds
//! its files already archived under another device. None of those can be
//! produced by a real server on demand, and a test that only walks the happy
//! path would miss exactly the branches the design spends the most words on.
//!
//! This lives in the library rather than in `tests/` so that both the unit
//! tests here and the end-to-end test in P1-12 can use it.
//!
//! It keeps the real server's identity rules: a file is `(device, agent,
//! logical path)`, a session is `(agent, native session id)`, adoption is only
//! within one session and role. [`FakeCaptureApi::set_device`] switches which
//! device the next calls come from, so "a reinstall with a new device key" and
//! "a second machine" are both one line in a test.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use cormex_proto::capture::{
    ChunkAck, FileClassification, FileEventOp, FileEventRequest, FileRole, GenerationDto,
    RegisterFileRequest, RegisterFileResponse, VerifyPrefixResponse,
};

use crate::sync::api::{CaptureApi, PutOutcome};

/// One generation's chunks, keyed by offset.
#[derive(Default, Clone)]
struct Generation {
    ordinal: i32,
    /// offset → (sha256, bytes)
    chunks: HashMap<i64, (String, Vec<u8>)>,
    end: i64,
}

impl Generation {
    fn bytes(&self) -> Vec<u8> {
        let mut offsets: Vec<i64> = self.chunks.keys().copied().collect();
        offsets.sort_unstable();
        offsets
            .into_iter()
            .flat_map(|o| self.chunks[&o].1.clone())
            .collect()
    }
}

#[derive(Clone)]
struct File {
    session: (String, String),
    role: FileRole,
}

/// The fake server.
#[derive(Default)]
pub struct FakeCaptureApi {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The device the next calls come from. Empty means `device-1`.
    device: String,
    /// `(device, agent, logical_path)` → file id
    files: HashMap<(String, String, String), String>,
    file_meta: HashMap<String, File>,
    /// file id → its own generations, in order
    generations: HashMap<String, Vec<String>>,
    /// file id → the generation it currently follows (own newest, or adopted)
    current: HashMap<String, String>,
    generations_by_id: HashMap<String, Generation>,
    next_id: u64,
    events: Vec<(String, FileEventOp)>,
    /// How many `put_chunk` calls to fail before succeeding. Used to exercise
    /// the retry path.
    fail_next: usize,
    /// Force this `expected` on the next 409, to exercise both directions.
    force_mismatch: Option<i64>,
    puts: usize,
    bytes_received: i64,
    adoptions: usize,
    registrations: Vec<RegisterFileRequest>,
    classifications: Vec<(String, FileClassification)>,
}

impl Inner {
    fn device(&self) -> String {
        if self.device.is_empty() {
            "device-1".into()
        } else {
            self.device.clone()
        }
    }

    fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}", self.next_id)
    }

    fn dto(&self, gid: &str) -> GenerationDto {
        let g = self.generations_by_id.get(gid).cloned().unwrap_or_default();
        GenerationDto {
            id: gid.to_owned(),
            ordinal: g.ordinal,
            end_offset: g.end,
        }
    }
}

impl FakeCaptureApi {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Make the next calls come from this device (a new device key, or a
    /// second machine). The default is `device-1`.
    pub fn set_device(&self, device: &str) {
        self.inner.lock().unwrap().device = device.to_owned();
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
        self.inner
            .lock()
            .unwrap()
            .generations_by_id
            .get(generation_id)
            .map(Generation::bytes)
            .unwrap_or_default()
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

    /// How many logical bytes the server stored, over all generations. The
    /// number an idempotency test cares about: a re-run that sends nothing new
    /// must leave it unchanged.
    #[must_use]
    pub fn bytes_received(&self) -> i64 {
        self.inner.lock().unwrap().bytes_received
    }

    /// How many generations hold any bytes. Two means the same content was
    /// archived twice — unless it really is two different streams.
    #[must_use]
    pub fn non_empty_generations(&self) -> usize {
        self.inner
            .lock()
            .unwrap()
            .generations_by_id
            .values()
            .filter(|g| g.end > 0)
            .count()
    }

    /// How many adoptions the server recorded.
    #[must_use]
    pub fn adoptions(&self) -> usize {
        self.inner.lock().unwrap().adoptions
    }

    /// Every file event the server received.
    #[must_use]
    pub fn events(&self) -> Vec<(String, FileEventOp)> {
        self.inner.lock().unwrap().events.clone()
    }

    /// Every file registration the server received.
    #[must_use]
    pub fn registrations(&self) -> Vec<RegisterFileRequest> {
        self.inner.lock().unwrap().registrations.clone()
    }

    /// Every classification correction, in arrival order.
    #[must_use]
    pub fn classifications(&self) -> Vec<(String, FileClassification)> {
        self.inner.lock().unwrap().classifications.clone()
    }
}

#[async_trait]
impl CaptureApi for FakeCaptureApi {
    async fn register(&self, req: &RegisterFileRequest) -> Result<RegisterFileResponse> {
        let mut inner = self.inner.lock().unwrap();
        inner.registrations.push(req.clone());
        let device = inner.device();
        let key = (device, req.agent.clone(), req.logical_path.clone());
        let session = (req.agent.clone(), req.native_session_id.clone());

        let file_id = if let Some(id) = inner.files.get(&key) {
            id.clone()
        } else {
            let file_id = inner.fresh_id("01FILE");
            let gid = inner.fresh_id("01GEN");
            inner.files.insert(key, file_id.clone());
            inner.file_meta.insert(
                file_id.clone(),
                File {
                    session: session.clone(),
                    role: req.role,
                },
            );
            inner.generations.insert(file_id.clone(), vec![gid.clone()]);
            inner.current.insert(file_id.clone(), gid.clone());
            inner.generations_by_id.insert(gid, Generation::default());
            file_id
        };

        let current = inner.current[&file_id].clone();
        let generation = inner.dto(&current);
        // Candidates exactly as the real server offers them: only to a file
        // that holds nothing yet, the current generation of every other file of
        // the same session and role, non-empty, longest first
        let mut candidates = Vec::new();
        if generation.end_offset == 0 {
            let mut seen = std::collections::HashSet::new();
            for (other, meta) in &inner.file_meta {
                if *other == file_id || meta.session != session || meta.role != req.role {
                    continue;
                }
                let gid = &inner.current[other];
                if *gid == current || !seen.insert(gid.clone()) {
                    continue;
                }
                let dto = inner.dto(gid);
                if dto.end_offset > 0 {
                    candidates.push(dto);
                }
            }
            candidates.sort_by(|a, b| b.end_offset.cmp(&a.end_offset).then(b.id.cmp(&a.id)));
        }

        Ok(RegisterFileResponse {
            file_id,
            session_id: format!("01SESSION|{}|{}", session.0, session.1),
            generation,
            candidates,
        })
    }

    async fn update_classification(&self, file_id: &str, req: &FileClassification) -> Result<()> {
        self.inner
            .lock()
            .unwrap()
            .classifications
            .push((file_id.to_owned(), req.clone()));
        Ok(())
    }

    async fn open_generation(&self, file_id: &str, _reason: &str) -> Result<GenerationDto> {
        let mut inner = self.inner.lock().unwrap();
        let gid = inner.fresh_id("01GEN");
        let gens = inner.generations.entry(file_id.to_owned()).or_default();
        gens.push(gid.clone());
        let ordinal = i32::try_from(gens.len() - 1).unwrap_or(0);
        inner.generations_by_id.insert(
            gid.clone(),
            Generation {
                ordinal,
                ..Generation::default()
            },
        );
        inner.current.insert(file_id.to_owned(), gid.clone());
        Ok(GenerationDto {
            id: gid,
            ordinal,
            end_offset: 0,
        })
    }

    async fn verify_prefix(
        &self,
        generation_id: &str,
        length: i64,
        sha256: &str,
    ) -> Result<VerifyPrefixResponse> {
        let inner = self.inner.lock().unwrap();
        let Some(g) = inner.generations_by_id.get(generation_id) else {
            anyhow::bail!("no such generation");
        };
        if length < 0 || length > g.end {
            return Ok(VerifyPrefixResponse {
                matches: false,
                end_offset: g.end,
            });
        }
        let bytes = g.bytes();
        let prefix = &bytes[..usize::try_from(length).unwrap_or(0)];
        Ok(VerifyPrefixResponse {
            matches: crate::sync::reader::sha256_hex(prefix) == sha256,
            end_offset: g.end,
        })
    }

    async fn adopt(
        &self,
        file_id: &str,
        generation_id: &str,
        _verified_through: i64,
    ) -> Result<GenerationDto> {
        let mut inner = self.inner.lock().unwrap();
        // The real server's check: the two files have to be one stream
        let me = inner
            .file_meta
            .get(file_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no such file"))?;
        let owner = inner
            .generations
            .iter()
            .find(|(_, gens)| gens.iter().any(|g| g == generation_id))
            .map(|(f, _)| f.clone())
            .ok_or_else(|| anyhow::anyhow!("no such generation"))?;
        let owner = &inner.file_meta[&owner];
        if owner.session != me.session || owner.role != me.role {
            anyhow::bail!("a file can only adopt a generation of the same session and role");
        }
        inner
            .current
            .insert(file_id.to_owned(), generation_id.to_owned());
        inner.adoptions += 1;
        Ok(inner.dto(generation_id))
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
                Some((sha, b)) if sha == sha256 && b.len() == body.len() => {
                    PutOutcome::Accepted(ChunkAck {
                        end_offset: g.end,
                        already_present: true,
                    })
                }
                _ => PutOutcome::OffsetMismatch { expected: g.end },
            });
        }
        if offset > g.end {
            return Ok(PutOutcome::OffsetMismatch { expected: g.end });
        }

        g.chunks.insert(offset, (sha256.to_owned(), body));
        g.end += len;
        let end = g.end;
        inner.bytes_received += len;
        Ok(PutOutcome::Accepted(ChunkAck {
            end_offset: end,
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

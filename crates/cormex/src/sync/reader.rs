//! Reading a logical file: plaintext, or transparently decompressed.
//!
//! Design: docs/design/session-archive-architecture.md §5.3 step 3.
//!
//! # "Logical bytes" is the unit everywhere
//!
//! A Codex rollout may exist as `rollout-x.jsonl.zst`, and the server stores
//! the **uncompressed** bytes under a key derived from their hash. So this
//! reader presents one interface — length, range, tail hash — over either
//! representation, and nothing above it knows which it got.
//!
//! # Why the tail hash is a separate method
//!
//! It is the only read the "did this file change" decision needs, and it is
//! bounded to 4 KiB. Reading the whole file to answer it would make every hook
//! invocation proportional to the session's length — on a long conversation
//! that is tens of megabytes per hook, and the host gives the hook 1.5 seconds.

use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};

/// How many bytes before the upload point the tail hash covers.
pub const TAIL_WINDOW: u64 = 4096;

/// A logical file: plaintext, or a zstd frame that is decompressed on the fly.
#[derive(Debug)]
pub struct LogicalReader {
    path: PathBuf,
    compressed: bool,
    /// The bytes, when the source is not a file (a SQLite row, P4-0).
    ///
    /// A row has no path to seek in: it was read out of a database that the
    /// host may rewrite at any moment, so the bytes are taken once and the
    /// reader works on that copy. Holding them is bounded by the row's size,
    /// and a row is a conversation bubble rather than a file.
    memory: Option<Vec<u8>>,
    /// The logical length, in uncompressed bytes.
    ///
    /// For a plaintext file this is the file's size. For a compressed one it is
    /// the frame's declared content size, or a streaming count when omitted.
    len: u64,
    /// For a compressed file: how many of its bytes are **complete** zstd
    /// frames. Every read decodes only this prefix.
    ///
    /// A host that appends a frame per write (DeepSeek Harness) can be caught
    /// mid-frame. Decoding that partial frame fails, and counting it would
    /// report bytes that cannot be read yet; stopping at the last complete
    /// frame reads the file as it was a moment earlier, and the next sync picks
    /// up the rest.
    frames_end: u64,
}

impl LogicalReader {
    /// Open a file.
    ///
    /// # Errors
    /// The file cannot be opened or decompressed.
    pub fn open(path: &Path, compressed: bool) -> Result<Self> {
        let (len, frames_end) = if compressed {
            let head = std::fs::read(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            let (frames_end, declared) = complete_frames(&head);
            let len = match declared {
                Some(size) => size,
                // A streaming writer may omit the content size. Count the
                // decoded bytes of the complete frames without keeping them
                None => {
                    let complete = std::io::Cursor::new(&head[..frames_end]);
                    let mut decoder = zstd::Decoder::new(complete)?;
                    std::io::copy(&mut decoder, &mut std::io::sink()).with_context(|| {
                        format!("could not count decoded bytes of {}", path.display())
                    })?
                }
            };
            (len, u64::try_from(frames_end).unwrap_or(u64::MAX))
        } else {
            let len = std::fs::metadata(path)
                .with_context(|| format!("could not stat {}", path.display()))?
                .len();
            (len, 0)
        };
        Ok(Self {
            path: path.to_path_buf(),
            compressed,
            memory: None,
            len,
            frames_end,
        })
    }

    /// A reader over bytes that are already in memory (a SQLite row).
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Self {
            // No path: nothing can be re-read, and the `memory` field is what
            // every read consults first
            path: PathBuf::new(),
            compressed: false,
            memory: Some(bytes),
            len,
            frames_end: 0,
        }
    }

    /// The logical length.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether the logical length is zero.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Read `[offset, offset + len)` of the **logical** bytes.
    ///
    /// # Errors
    /// The file cannot be read, or the range is outside it.
    pub fn read_range(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if offset >= self.len {
            return Ok(Vec::new());
        }
        let want = usize::try_from((self.len - offset).min(len as u64)).unwrap_or(len);

        if let Some(bytes) = &self.memory {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let end = start.saturating_add(want).min(bytes.len());
            return Ok(bytes[start..end].to_vec());
        }

        if !self.compressed {
            let mut file = std::fs::File::open(&self.path)
                .with_context(|| format!("could not open {}", self.path.display()))?;
            file.seek(SeekFrom::Start(offset))
                .context("could not seek")?;
            let mut buf = vec![0u8; want];
            let n = file.read(&mut buf).context("could not read")?;
            buf.truncate(n);
            return Ok(buf);
        }

        // A compressed file has no seekable index, so the only way to reach an
        // offset is to decompress up to it. That is why the client prefers the
        // plaintext sibling whenever one exists, and why a `.zst`-only file is
        // read in one pass rather than in chunks.
        let mut decoder = self.decoder()?;
        let mut sink = std::io::sink();
        std::io::copy(&mut (&mut decoder).take(offset), &mut sink)
            .context("could not skip to the offset")?;
        // `read_to_end` rather than one `read`: a single read returns at a frame
        // boundary, which for a file of appended frames is after the first line
        let mut buf = Vec::with_capacity(want);
        (&mut decoder)
            .take(want as u64)
            .read_to_end(&mut buf)
            .context("could not read")?;
        Ok(buf)
    }

    /// The whole file, decompressed.
    ///
    /// # Errors
    /// The file cannot be read.
    pub fn read_all(&self) -> Result<Vec<u8>> {
        self.read_range(0, usize::try_from(self.len).unwrap_or(usize::MAX))
    }

    /// The sha256 of the 4 KiB ending at `offset`.
    ///
    /// `None` when `offset` is 0: there is nothing before the start, and a hash
    /// of zero bytes is not a useful comparison.
    ///
    /// # Errors
    /// The file cannot be read.
    pub fn tail_hash(&self, offset: u64) -> Result<Option<String>> {
        if offset == 0 {
            return Ok(None);
        }
        let start = offset.saturating_sub(TAIL_WINDOW);
        let bytes = self.read_range(start, usize::try_from(offset - start).unwrap_or(0))?;
        Ok(Some(sha256_hex(&bytes)))
    }

    /// The sha256 of the first `len` logical bytes (all of them if the file is
    /// shorter).
    ///
    /// What the client sends to `/capture/generations/{id}/verify` before it
    /// trusts any offset the server gives it. Streams in one pass — a
    /// compressed file is decompressed once, not once per block.
    ///
    /// # Errors
    /// The file cannot be read.
    pub fn prefix_hash(&self, len: u64) -> Result<String> {
        let len = len.min(self.len);
        let mut hasher = Sha256::new();
        if let Some(bytes) = &self.memory {
            let end = usize::try_from(len).unwrap_or(bytes.len()).min(bytes.len());
            hasher.update(&bytes[..end]);
            return Ok(format!("{:x}", hasher.finalize()));
        }
        let mut source: Box<dyn std::io::Read> = if self.compressed {
            Box::new(self.decoder()?.take(len))
        } else {
            Box::new(
                std::fs::File::open(&self.path)
                    .with_context(|| format!("could not open {}", self.path.display()))?
                    .take(len),
            )
        };
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = source.read(&mut buf).context("could not read")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    /// The sha256 of the whole file.
    ///
    /// # Errors
    /// The file cannot be read.
    pub fn whole_hash(&self) -> Result<String> {
        if let Some(bytes) = &self.memory {
            return Ok(sha256_hex(bytes));
        }
        if self.compressed {
            // Stream it: a large rollout decompressed into memory just to hash
            // it is the one place this client would allocate without bound
            let mut decoder = self.decoder()?;
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                let n = decoder.read(&mut buf).context("could not read")?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            return Ok(format!("{:x}", hasher.finalize()));
        }
        let mut file = std::fs::File::open(&self.path)
            .with_context(|| format!("could not open {}", self.path.display()))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).context("could not read")?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    fn decoder(
        &self,
    ) -> Result<zstd::Decoder<'static, std::io::BufReader<std::io::Take<std::fs::File>>>> {
        let file = std::fs::File::open(&self.path)
            .with_context(|| format!("could not open {}", self.path.display()))?;
        zstd::Decoder::new(file.take(self.frames_end))
            .with_context(|| format!("{} is not a zstd frame", self.path.display()))
    }
}

/// The length of the complete zstd frames at the start of `bytes`, and the sum
/// of their declared content sizes when **every** one declares it.
///
/// The old reader asked only the first frame for its size, which is the whole
/// file for Codex's single-frame `.jsonl.zst` and a small fraction of it for a
/// log that appends a frame per write: everything after the first frame was
/// never captured.
fn complete_frames(bytes: &[u8]) -> (usize, Option<u64>) {
    let mut end = 0;
    let mut declared = Some(0_u64);
    while end < bytes.len() {
        let Ok(size) = zstd::zstd_safe::find_frame_compressed_size(&bytes[end..]) else {
            break;
        };
        if size == 0 || end + size > bytes.len() {
            break;
        }
        declared = match (
            declared,
            zstd::zstd_safe::get_frame_content_size(&bytes[end..end + size]),
        ) {
            (Some(total), Ok(Some(n))) => Some(total + n),
            _ => None,
        };
        end += size;
    }
    (end, declared)
}

/// Lowercase hex SHA-256.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log that appends one zstd frame per write (DeepSeek Harness) is read
    /// whole, not just its first frame.
    #[test]
    fn a_file_of_appended_frames_is_read_to_the_end() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("session.v4.jsonl.zstd");
        let lines = [
            b"{\"seq\":0}\n".as_slice(),
            b"{\"seq\":1}\n",
            b"{\"seq\":2}\n",
        ];
        let mut file = Vec::new();
        for line in lines {
            file.extend(zstd::encode_all(line, 3).expect("compress"));
        }
        std::fs::write(&path, &file).expect("write");

        let r = LogicalReader::open(&path, true).expect("open");
        let whole: Vec<u8> = lines.concat();
        assert_eq!(
            r.len(),
            whole.len() as u64,
            "the logical length is every frame's content, not the first frame's"
        );
        assert_eq!(r.read_all().expect("read"), whole);
        assert_eq!(
            r.prefix_hash(r.len()).expect("hash"),
            sha256_hex(&whole),
            "the prefix hash the server verifies covers the same bytes"
        );
    }

    /// A frame the host is still writing is left for the next sync.
    #[test]
    fn a_frame_cut_off_mid_write_is_not_counted_yet() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("session.v4.jsonl.zstd");
        let done = zstd::encode_all(b"{\"seq\":0}\n".as_slice(), 3).expect("compress");
        let partial = zstd::encode_all(b"{\"seq\":1}\n".as_slice(), 3).expect("compress");
        let mut file = done.clone();
        file.extend(&partial[..partial.len() / 2]);
        std::fs::write(&path, &file).expect("write");

        let r = LogicalReader::open(&path, true).expect("open");
        assert_eq!(
            r.read_all().expect("read"),
            b"{\"seq\":0}\n",
            "only the complete frame is readable; counting the partial one would report bytes \
             that fail to decode, and the upload would stop with an error"
        );
    }

    #[test]
    fn a_plaintext_file_reads_ranges_and_hashes() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("s.jsonl");
        let body = b"0123456789abcdefghij";
        std::fs::write(&path, body).expect("write");

        let r = LogicalReader::open(&path, false).expect("open");
        assert_eq!(r.len(), 20);
        assert_eq!(r.read_range(0, 5).expect("read"), b"01234");
        assert_eq!(r.read_range(15, 100).expect("read"), b"fghij");
        assert_eq!(
            r.read_range(20, 10).expect("read"),
            b"",
            "a range starting exactly at the end is empty, not an error"
        );
        assert_eq!(r.whole_hash().expect("hash"), sha256_hex(body));
    }

    #[test]
    fn a_tail_hash_covers_the_last_four_kib() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("s.jsonl");
        let body = vec![b'x'; 10_000];
        std::fs::write(&path, &body).expect("write");

        let r = LogicalReader::open(&path, false).expect("open");
        assert_eq!(
            r.tail_hash(0).expect("hash"),
            None,
            "there is nothing before offset 0, and a hash of zero bytes is not a useful \
             comparison"
        );
        assert_eq!(
            r.tail_hash(10_000).expect("hash"),
            Some(sha256_hex(&body[10_000 - 4096..])),
            "the tail hash covers the 4 KiB ending at the offset"
        );
        // An offset smaller than the window covers everything before it
        assert_eq!(
            r.tail_hash(100).expect("hash"),
            Some(sha256_hex(&body[..100]))
        );
    }

    /// A compressed file presents the **logical** bytes.
    ///
    /// This is the whole point of the reader: the server stores uncompressed
    /// bytes under a hash of them, so a `.zst` and its `.jsonl` must be
    /// indistinguishable above this layer.
    #[test]
    fn a_compressed_file_reads_as_plaintext() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("s.jsonl.zst");
        let body = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
        // **With a content size**, which is what a host that writes a `.zst`
        // for some finished files produces. The test below covers streaming
        // frames that require a decoding pass to determine logical length.
        // `include_contentsize` only takes effect when the size is pledged
        let mut encoder = zstd::Encoder::new(Vec::new(), 3).expect("encoder");
        encoder
            .set_pledged_src_size(Some(body.len() as u64))
            .expect("pledge");
        encoder.include_contentsize(true).expect("size");
        std::io::Write::write_all(&mut encoder, body).expect("write");
        let frame = encoder.finish().expect("finish");
        std::fs::write(&path, &frame).expect("write");

        let r = LogicalReader::open(&path, true).expect("open");
        assert_eq!(
            r.len(),
            body.len() as u64,
            "the length must be the logical one, not the frame's"
        );
        assert_eq!(r.read_all().expect("read"), body);
        assert_eq!(r.read_range(10, 5).expect("read"), &body[10..15]);
        assert_eq!(
            r.whole_hash().expect("hash"),
            sha256_hex(body),
            "the whole-file hash must be of the logical bytes, or the same file as .jsonl and \
             as .jsonl.zst would hash differently"
        );
        assert_eq!(
            r.tail_hash(body.len() as u64).expect("hash"),
            Some(sha256_hex(body)),
            "the tail of a short compressed file is the whole file"
        );
    }

    /// A frame without a content size is counted and read as logical bytes.
    ///
    /// Without it, "how long is this file" can only be answered by decompressing
    /// all of it — on every hook invocation.
    #[test]
    fn a_frame_without_a_content_size_is_readable() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("s.jsonl.zst");
        // Streaming compression omits the content size
        let mut encoder = zstd::Encoder::new(Vec::new(), 3).expect("encoder");
        encoder.include_checksum(false).expect("checksum");
        // `include_contentsize(false)` is what a streaming writer produces
        encoder.include_contentsize(false).expect("size");
        std::io::Write::write_all(&mut encoder, b"hello").expect("write");
        let frame = encoder.finish().expect("finish");
        std::fs::write(&path, &frame).expect("write");

        let reader = LogicalReader::open(&path, true).expect("streamed frame");
        assert_eq!(reader.len(), 5);
        assert_eq!(reader.read_all().unwrap(), b"hello");
    }
}

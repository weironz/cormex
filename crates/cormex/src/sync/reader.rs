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
    /// The logical length, in uncompressed bytes.
    ///
    /// For a plaintext file this is the file's size. For a compressed one it is
    /// the frame's declared content size — which is why a zstd frame written
    /// without a content size is refused: without it, "how long is this file"
    /// can only be answered by decompressing all of it.
    len: u64,
}

impl LogicalReader {
    /// Open a file.
    ///
    /// # Errors
    /// The file cannot be opened, or a compressed one does not carry its
    /// content size.
    pub fn open(path: &Path, compressed: bool) -> Result<Self> {
        let len = if compressed {
            // The frame header carries the content size, so reading it is a few
            // bytes rather than a full decompression. A frame without one
            // cannot answer "how long is this" at all, and decompressing to
            // find out would read the whole file on every hook invocation.
            let head = std::fs::read(path)
                .with_context(|| format!("could not read {}", path.display()))?;
            zstd::zstd_safe::get_frame_content_size(&head)
                .map_err(|e| {
                    anyhow::anyhow!("{} is not a readable zstd frame: {e}", path.display())
                })?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "{} is a zstd frame without a content size, so its logical length is \
                         unknown without decompressing all of it",
                        path.display()
                    )
                })?
        } else {
            std::fs::metadata(path)
                .with_context(|| format!("could not stat {}", path.display()))?
                .len()
        };
        Ok(Self {
            path: path.to_path_buf(),
            compressed,
            len,
        })
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
        let mut buf = vec![0u8; want];
        let n = decoder.read(&mut buf).context("could not read")?;
        buf.truncate(n);
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

    /// The sha256 of the whole file.
    ///
    /// # Errors
    /// The file cannot be read.
    pub fn whole_hash(&self) -> Result<String> {
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

    fn decoder(&self) -> Result<zstd::Decoder<'static, std::io::BufReader<std::fs::File>>> {
        let file = std::fs::File::open(&self.path)
            .with_context(|| format!("could not open {}", self.path.display()))?;
        zstd::Decoder::new(file)
            .with_context(|| format!("{} is not a zstd frame", self.path.display()))
    }
}

/// Lowercase hex SHA-256.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // for a finished file produces. `encode_all` omits it, and a frame
        // without one cannot report its logical length -- see the test below.
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

    /// A frame without a content size is refused.
    ///
    /// Without it, "how long is this file" can only be answered by decompressing
    /// all of it — on every hook invocation.
    #[test]
    fn a_frame_without_a_content_size_is_refused() {
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

        let err = LogicalReader::open(&path, true)
            .expect_err("a frame without a content size cannot report its length");
        assert!(
            err.to_string().contains("content size"),
            "the error should say why: {err}"
        );
    }
}

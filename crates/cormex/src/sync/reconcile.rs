//! Deciding, without trusting any local state, how a file on disk relates to a
//! generation the server already holds.
//!
//! # Why this exists
//!
//! Every idempotency decision the client makes when it has no memory of its own
//! — `state.db` deleted, the CLI reinstalled with a new device key, the host
//! moving the file, a second machine — comes down to one question: *are the
//! bytes the server has the bytes on this disk?* The earlier answer was an
//! offset: the server says "I end at 12 345", the client resumes there. An
//! offset says nothing about the bytes before it, so a file rewritten while the
//! CLI was uninstalled was spliced onto the old history, and a copy under a new
//! device or path was uploaded and projected a second time.
//!
//! The answer now is a prefix hash, checked by the server
//! (`POST /capture/generations/{id}/verify`). It is exact for any length, which
//! a chunk manifest is not: a shorter local copy usually ends inside a server
//! chunk, and that chunk's hash says nothing about part of it.

use anyhow::Result;

use crate::sync::api::CaptureApi;
use crate::sync::reader::LogicalReader;

/// How the local file relates to a generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The generation is a prefix of the local file (or they are equal):
    /// continue appending at the generation's end.
    Extends { end: i64 },
    /// The local file is a strict prefix of the generation — an older copy.
    /// Nothing on this disk is new; send nothing.
    Behind,
    /// The two disagree somewhere inside the shorter one. Not the same stream:
    /// following this generation would splice two different histories.
    Diverged,
}

/// Compare the local file with `generation_id`, which the server says ends at
/// `end`.
///
/// One request, and one pass over at most `min(local, end)` bytes of the file.
///
/// # Errors
/// The file cannot be read, or the verify request fails.
pub async fn compare(
    api: &dyn CaptureApi,
    reader: &LogicalReader,
    generation_id: &str,
    end: i64,
) -> Result<Verdict> {
    let local = i64::try_from(reader.len()).unwrap_or(i64::MAX);
    if end <= 0 {
        // An empty generation is a prefix of everything
        return Ok(Verdict::Extends { end: 0 });
    }
    let n = local.min(end);
    if n == 0 {
        return Ok(Verdict::Behind);
    }
    let sha = reader.prefix_hash(u64::try_from(n).unwrap_or(0))?;
    let resp = api.verify_prefix(generation_id, n, &sha).await?;
    if !resp.matches {
        return Ok(Verdict::Diverged);
    }
    Ok(if local >= end {
        Verdict::Extends { end }
    } else {
        Verdict::Behind
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sync::fake::FakeCaptureApi;
    use cormex_proto::capture::{FileRole, Framing, RegisterFileRequest};

    async fn generation_holding(api: &FakeCaptureApi, bytes: &[u8]) -> String {
        let reg = api
            .register(&RegisterFileRequest {
                agent: "claude_code".into(),
                native_session_id: "s".into(),
                parent_native_session_id: None,
                logical_path: "projects/p/s.jsonl".into(),
                role: FileRole::Transcript,
                framing: Framing::Append,
                cwd: None,
                project_key: None,
            })
            .await
            .expect("register");
        let sha = crate::sync::reader::sha256_hex(bytes);
        api.put_chunk(
            &reg.generation.id,
            0,
            bytes.to_vec(),
            &sha,
            "2026-11-07T00:00:00Z",
        )
        .await
        .expect("put");
        reg.generation.id
    }

    #[tokio::test]
    async fn a_longer_local_file_with_the_same_prefix_extends() {
        let api = FakeCaptureApi::new();
        let g = generation_holding(&api, b"0123456789").await;
        let reader = LogicalReader::from_bytes(b"0123456789ABC".to_vec());
        assert_eq!(
            compare(&api, &reader, &g, 10).await.expect("compare"),
            Verdict::Extends { end: 10 },
            "the server's ten bytes are this file's first ten: resume at 10"
        );
    }

    #[tokio::test]
    async fn an_identical_file_extends_by_nothing() {
        let api = FakeCaptureApi::new();
        let g = generation_holding(&api, b"0123456789").await;
        let reader = LogicalReader::from_bytes(b"0123456789".to_vec());
        assert_eq!(
            compare(&api, &reader, &g, 10).await.expect("compare"),
            Verdict::Extends { end: 10 }
        );
    }

    #[tokio::test]
    async fn a_shorter_copy_is_behind() {
        let api = FakeCaptureApi::new();
        let g = generation_holding(&api, b"0123456789").await;
        let reader = LogicalReader::from_bytes(b"01234".to_vec());
        assert_eq!(
            compare(&api, &reader, &g, 10).await.expect("compare"),
            Verdict::Behind,
            "an older copy of the same stream holds nothing new and must send nothing"
        );
    }

    #[tokio::test]
    async fn a_file_rewritten_while_away_has_diverged() {
        let api = FakeCaptureApi::new();
        let g = generation_holding(&api, b"0123456789").await;
        // Same length, different bytes: exactly what an offset alone cannot see
        let reader = LogicalReader::from_bytes(b"01234XXXXXmore".to_vec());
        assert_eq!(
            compare(&api, &reader, &g, 10).await.expect("compare"),
            Verdict::Diverged,
            "resuming at 10 here would splice a rewritten file onto the old history"
        );
    }
}

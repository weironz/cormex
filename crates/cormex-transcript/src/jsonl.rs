//! Line splitting across `feed` calls.
//!
//! # Why the buffer lives here rather than in each parser
//!
//! A chunk boundary lands wherever 4 MiB happens to end, which is almost never
//! a line boundary. Every parser therefore needs "the last line was incomplete,
//! hold it until the next chunk" — and writing that twice is how one of them
//! ends up dropping the tail of a chunk, which loses a record with no error.
//!
//! # Offsets are the raw layer's, not the parser's
//!
//! Every event carries `src_offset` and `src_length`: where in the **logical
//! file** it came from. That is what `GET /archive/raw/{generation}?offset=`
//! uses to show the original record, and what makes "every fact points at its
//! source" true at the byte level.

/// Splits a byte stream into lines, carrying an incomplete tail across calls.
#[derive(Debug, Default)]
pub struct LineBuffer {
    /// Bytes of the line that has not ended yet, and where it started.
    partial: Vec<u8>,
    partial_start: u64,
    /// The next byte's offset in the logical file.
    next_offset: u64,
}

/// One complete line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// Where the line starts in the logical file.
    pub offset: u64,
    /// The line's bytes, **without** the trailing newline.
    pub bytes: Vec<u8>,
}

impl LineBuffer {
    /// A buffer starting at logical offset 0.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed the next contiguous chunk.
    ///
    /// `offset` is where this chunk starts; the caller must feed contiguous
    /// bytes. A gap would make every subsequent `src_offset` wrong, and the
    /// symptom is "the raw view shows the wrong record" — so a mismatch is
    /// reported rather than absorbed.
    ///
    /// # Errors
    /// The chunk does not start where the buffer expects.
    pub fn feed(&mut self, offset: u64, bytes: &[u8]) -> Result<Vec<Line>, OffsetGap> {
        if offset != self.next_offset {
            return Err(OffsetGap {
                expected: self.next_offset,
                got: offset,
            });
        }
        self.next_offset += bytes.len() as u64;

        let mut out = Vec::new();
        let mut start = 0usize;
        for (i, b) in bytes.iter().enumerate() {
            if *b != b'\n' {
                continue;
            }
            let mut line = std::mem::take(&mut self.partial);
            let line_start = if line.is_empty() {
                offset + start as u64
            } else {
                self.partial_start
            };
            line.extend_from_slice(&bytes[start..i]);
            // A trailing `\r` is stripped: the hosts write LF, and a file that
            // has been through a Windows editor has CRLF. Leaving the `\r` in
            // makes every JSON parse fail with "trailing characters", which
            // reads as "the file is corrupt".
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            out.push(Line {
                offset: line_start,
                bytes: line,
            });
            start = i + 1;
            self.partial_start = offset + start as u64;
        }

        if start < bytes.len() {
            if self.partial.is_empty() {
                self.partial_start = offset + start as u64;
            }
            self.partial.extend_from_slice(&bytes[start..]);
        }
        Ok(out)
    }

    /// The incomplete tail, if any.
    ///
    /// The caller decides what to do with it: a session file being written may
    /// end mid-line, and that line is **not** an error — the next sync appends
    /// the rest.
    #[must_use]
    pub fn pending(&self) -> Option<Line> {
        if self.partial.is_empty() {
            None
        } else {
            Some(Line {
                offset: self.partial_start,
                bytes: self.partial.clone(),
            })
        }
    }

    /// The offset the next `feed` must start at.
    #[must_use]
    pub const fn next_offset(&self) -> u64 {
        self.next_offset
    }

    /// Restore from a checkpoint.
    #[must_use]
    pub fn from_state(next_offset: u64, partial_start: u64, partial: Vec<u8>) -> Self {
        Self {
            partial,
            partial_start,
            next_offset,
        }
    }

    /// The state to put in a checkpoint.
    #[must_use]
    pub fn state(&self) -> (u64, u64, Vec<u8>) {
        (self.next_offset, self.partial_start, self.partial.clone())
    }
}

/// A chunk arrived out of order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetGap {
    pub expected: u64,
    pub got: u64,
}

impl std::fmt::Display for OffsetGap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the chunk starts at {} but the buffer expects {}; feeding it would make every \
             subsequent src_offset wrong, and the raw view would show the wrong record",
            self.got, self.expected
        )
    }
}

impl std::error::Error for OffsetGap {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_lines_are_split() {
        let mut b = LineBuffer::new();
        let lines = b.feed(0, b"one\ntwo\n").expect("feed");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].bytes, b"one");
        assert_eq!(lines[0].offset, 0);
        assert_eq!(lines[1].offset, 4);
    }

    /// A line split across two chunks comes out whole.
    ///
    /// This is the case a 4 MiB chunk boundary produces constantly, and the
    /// failure — dropping the tail of a chunk — loses a record with no error.
    #[test]
    fn a_line_split_across_chunks_comes_out_whole() {
        let mut b = LineBuffer::new();
        let first = b.feed(0, b"{\"a\":").expect("feed");
        assert!(first.is_empty(), "an incomplete line is not a line yet");
        let second = b.feed(5, b"1}\n").expect("feed");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].bytes, b"{\"a\":1}");
        assert_eq!(
            second[0].offset, 0,
            "the line starts where it started, not where the second chunk began"
        );
    }

    /// Splitting at **every** position gives the same lines.
    ///
    /// The chunk-invariance property, checked exhaustively on a small input: a
    /// parser that is correct for one split and wrong for another is a parser
    /// whose bug appears only when a session happens to cross a 4 MiB boundary
    /// at that byte.
    #[test]
    fn every_split_position_gives_the_same_lines() {
        let body = b"{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n";
        let mut whole = LineBuffer::new();
        let expected = whole.feed(0, body).expect("feed");

        for split in 0..=body.len() {
            let mut b = LineBuffer::new();
            let mut got = b.feed(0, &body[..split]).expect("feed");
            got.extend(b.feed(split as u64, &body[split..]).expect("feed"));
            assert_eq!(
                got, expected,
                "splitting at byte {split} produced different lines"
            );
        }
    }

    #[test]
    fn an_incomplete_tail_is_reported_not_emitted() {
        let mut b = LineBuffer::new();
        let lines = b.feed(0, b"complete\nincomplete").expect("feed");
        assert_eq!(lines.len(), 1);
        let pending = b.pending().expect("a pending line");
        assert_eq!(pending.bytes, b"incomplete");
        assert_eq!(pending.offset, 9);
    }

    #[test]
    fn a_crlf_line_drops_the_carriage_return() {
        let mut b = LineBuffer::new();
        let lines = b.feed(0, b"{\"a\":1}\r\n").expect("feed");
        assert_eq!(
            lines[0].bytes, b"{\"a\":1}",
            "a trailing \\r makes every JSON parse fail with 'trailing characters', which reads \
             as 'the file is corrupt'"
        );
    }

    #[test]
    fn a_gap_is_reported() {
        let mut b = LineBuffer::new();
        b.feed(0, b"abc\n").expect("feed");
        let err = b.feed(10, b"def\n").expect_err("a gap must be reported");
        assert_eq!(err.expected, 4);
        assert_eq!(err.got, 10);
    }

    /// The checkpoint round-trips the buffer's whole state.
    #[test]
    fn the_checkpoint_restores_the_buffer() {
        let mut b = LineBuffer::new();
        b.feed(0, b"one\npartial").expect("feed");
        let (next, start, partial) = b.state();

        let mut restored = LineBuffer::from_state(next, start, partial);
        let lines = restored.feed(next, b" rest\n").expect("feed");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].bytes, b"partial rest");
        assert_eq!(lines[0].offset, 4);
    }
}

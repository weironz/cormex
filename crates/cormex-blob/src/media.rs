//! Media metadata probing: recognizing the MIME type, and an image's width and
//! height, from a byte stream.
//!
//! # Why this belongs at this layer
//!
//! The `Content-Type` a client reports is untrustworthy (browsers guess from
//! the extension, mobile clients routinely send `application/octet-stream`
//! outright), and `blobs.mime` determines how everything downstream handles the
//! content: the transcription pipeline routes on it to ASR / vision / OCR, and
//! the front end picks a player by it. The cost of guessing wrong is that this
//! content becomes a **permanent retrieval black hole** — nobody ever goes back
//! and rescans.
//!
//! Hence: the bytes decide, and the client's claim is only a fallback.
//!
//! # Read the header only; never decode
//!
//! Every probe looks at no more than the first few dozen bytes. For a 200 MB
//! video, not decoding takes a few hundred nanoseconds, whereas decoding it
//! would take a few hundred megabytes of memory — which is the premise on which
//! probing can sit on the upload hot path at all.

use serde::{Deserialize, Serialize};

/// Media metadata probed from a byte stream. Every field is optional: if it
/// cannot be determined, it cannot be determined, and no default is ever
/// invented (a `0x0` width and height would make the front end's layout divide
/// by zero).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaMeta {
    /// The MIME decided by the leading bytes. `None` when it cannot be
    /// determined, in which case the caller falls back to the client's claim.
    pub mime: Option<String>,
    /// Image dimensions in pixels. Video dimensions would require demuxing the
    /// container, which falls in the "heavy dependency" category; see below.
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The duration of audio or video. **Currently always `None`**; see the
    /// documentation of [`probe`] for why.
    pub duration_ms: Option<u64>,
}

impl MediaMeta {
    /// Produce a value that can be written straight into `blobs.mime`: the
    /// leading bytes first, the client's claim as backstop.
    ///
    /// When both are missing it gives `application/octet-stream` — which is
    /// what `blobs.mime NOT NULL` requires, and also HTTP's standard answer for
    /// "no idea what this is".
    #[must_use]
    pub fn resolve_mime(&self, declared: Option<&str>) -> String {
        self.mime
            .clone()
            .or_else(|| declared.map(str::to_owned).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_MIME.to_owned())
    }

    /// Whether this is an image. The transcription pipeline routes on it to
    /// vision/OCR.
    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
    }

    /// Whether this is audio or video. Routes to ASR, and is also the only
    /// category future duration probing would ever have to handle.
    #[must_use]
    pub fn is_audio_video(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.starts_with("video/") || m.starts_with("audio/"))
    }
}

/// The MIME for "no idea what this is".
pub const DEFAULT_MIME: &str = "application/octet-stream";

/// Probe a stretch of bytes for media metadata.
///
/// # Why duration was not implemented
///
/// Getting the duration means demuxing the container: MP4 requires walking the
/// box tree to read `mvhd.duration/timescale`, Matroska requires reading
/// `Segment/Info/Duration`, and MP3 has no container at all so the frames have
/// to be scanned and summed — and for a VBR MP3 anything short of scanning the
/// whole file is wrong. Doing this properly for every format leaves two routes:
///
/// - Bind ffmpeg: which drags in a C dependency and a GPL/LGPL licensing
///   discussion, and burdens cross-compilation for all six targets.
/// - Adopt `symphonia`: pure Rust, but carrying an entire demuxer stack for the
///   sake of one number.
///
/// And **duration currently has no consumer** — the player reads it out of the
/// stream itself, and retrieval uses `blob_transcripts.span_*_ms` (produced by
/// ASR, which carries a timeline anyway). So the slot `duration_ms:
/// Option<u64>` is left here, to be filled from the transcription result when
/// the ASR pipeline lands — at which point it is free.
///
/// Until then this field is invariably `None`; do **not** mistake that for "the
/// probe failed".
#[must_use]
pub fn probe(bytes: &[u8]) -> MediaMeta {
    let mime = infer::get(bytes).map(|t| t.mime_type().to_owned());

    // Only call imagesize on bytes that really are an image: on non-images it
    // performs a pointless round of header matching.
    let (width, height) = match mime.as_deref() {
        Some(m) if m.starts_with("image/") => match imagesize::blob_size(bytes) {
            Ok(size) => (
                u32::try_from(size.width).ok(),
                u32::try_from(size.height).ok(),
            ),
            // Recognizable as an image yet with unreadable dimensions is most
            // likely a truncated upload. Not escalated to an error: the content
            // itself is stored regardless, and missing dimensions only cost the
            // front end one layout hint.
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    mime = m,
                    "recognized as an image but the dimensions could not be read; \
                     treating them as unknown"
                );
                (None, None)
            }
        },
        _ => (None, None),
    };

    MediaMeta {
        mime,
        width,
        height,
        duration_ms: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest legal PNG: 1×1, fully transparent. Real bytes rather than a
    /// faked header, so that both the infer and the imagesize paths are
    /// exercised.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// The smallest GIF87a: 3×5, with the dimensions written at bytes 7..10 —
    /// used to verify that width and height are not hard-coded.
    const GIF_3X5: &[u8] = &[
        0x47, 0x49, 0x46, 0x38, 0x37, 0x61, 0x03, 0x00, 0x05, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00,
        0x00, 0xFF, 0xFF, 0xFF, 0x2C, 0x00, 0x00, 0x00, 0x00, 0x03, 0x00, 0x05, 0x00, 0x00, 0x02,
        0x02, 0x44, 0x01, 0x00, 0x3B,
    ];

    #[test]
    fn png_yields_mime_and_dimensions() {
        let meta = probe(PNG_1X1);
        assert_eq!(
            meta.mime.as_deref(),
            Some("image/png"),
            "the PNG magic number was not recognized, so the transcription pipeline would throw \
             the image away as binary"
        );
        assert_eq!(
            (meta.width, meta.height),
            (Some(1), Some(1)),
            "the dimensions of a 1×1 PNG were read wrong"
        );
        assert!(
            meta.is_image(),
            "a PNG should be judged an image so it routes to vision/OCR"
        );
        assert!(
            !meta.is_audio_video(),
            "a PNG should not be judged audio or video"
        );
    }

    #[test]
    fn dimensions_are_read_not_guessed() {
        let meta = probe(GIF_3X5);
        assert_eq!(
            meta.mime.as_deref(),
            Some("image/gif"),
            "the GIF magic number was not recognized"
        );
        assert_eq!(
            (meta.width, meta.height),
            (Some(3), Some(5)),
            "width and height should really be read from the file header (3×5 is not square, so \
             hard-coded values give themselves away here)"
        );
    }

    #[test]
    fn unknown_bytes_yield_no_guesses() {
        let meta = probe(b"just some plain text, no magic bytes at all");
        assert_eq!(
            meta.mime, None,
            "unrecognized has to mean None; never invent a MIME"
        );
        assert_eq!(
            (meta.width, meta.height),
            (None, None),
            "non-images must be left empty; inventing a 0×0 makes the front end's layout divide \
             by zero"
        );
    }

    #[test]
    fn sniffed_mime_beats_client_declaration() {
        let meta = probe(PNG_1X1);
        assert_eq!(
            meta.resolve_mime(Some("application/octet-stream")),
            "image/png",
            "the client's claim is untrustworthy — when the leading bytes say PNG, the bytes win"
        );

        let unknown = probe(b"...");
        assert_eq!(
            unknown.resolve_mime(Some("text/markdown")),
            "text/markdown",
            "only when the leading bytes say nothing does it fall back to the client's claim"
        );
        assert_eq!(
            unknown.resolve_mime(None),
            DEFAULT_MIME,
            "with both missing it must still yield something writable into blobs.mime NOT NULL"
        );
        assert_eq!(
            unknown.resolve_mime(Some("")),
            DEFAULT_MIME,
            "a claim of the empty string is the same as no claim, and must not be written into \
             blobs.mime verbatim"
        );
    }

    #[test]
    fn duration_is_deliberately_absent() {
        // Not a defect but a trade-off: see the documentation of probe. This
        // assertion will fail once duration probing is actually implemented,
        // which is precisely the moment to go and update the docs and the
        // callers.
        assert_eq!(
            probe(PNG_1X1).duration_ms,
            None,
            "duration probing is not implemented yet (it comes free with the ASR pipeline), so \
             for now it must invariably be None"
        );
    }

    #[test]
    fn empty_input_does_not_panic() {
        let meta = probe(&[]);
        assert_eq!(
            meta,
            MediaMeta::default(),
            "empty input should yield entirely empty metadata rather than a panic"
        );
    }
}

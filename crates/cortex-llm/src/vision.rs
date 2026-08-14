//! Image content blocks: let a [`Message`] carry images, and determine whether
//! the model can actually see them before anything goes out.
//!
//! # Why this layer is so thin
//!
//! The vendors' image formats differ — Anthropic wants
//! `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}`,
//! while the OpenAI-compatible family wants
//! `{"type":"image_url","image_url":{"url":"data:…"}}`. But that difference
//! **is already solved in the vendored goose**:
//! `goose_provider_types::images::convert_image` dispatches on `ImageFormat`,
//! the anthropic engine passes `ImageFormat::Anthropic`, the openai / ollama
//! engines pass `ImageFormat::OpenAi`, and the declarative configuration's
//! `engine` field decides which engine is used.
//!
//! In other words: **get the image into a `Message` and the format conversion
//! is free**. So this module does only the three things goose does not:
//!
//! 1. Build an image from **bytes** (goose's [`Message::with_image`] only takes
//!    a base64 string, while the upload pipeline holds a `&[u8]`);
//! 2. MIME normalization and a size ceiling — the vendors' vision APIs accept
//!    only a handful of formats, and sending the wrong one surfaces as a vague
//!    400, so it is better to say so locally;
//! 3. **Capability determination** — goose's `ModelInfo` has no vision field,
//!    and DeepSeek currently has no vision capability at all. Without this step
//!    a user's image is silently lost in the form of "the model says it did not
//!    see an image", which is the hardest class of problem to track down.

use base64::Engine as _;
use goose_providers::conversation::message::{Message, MessageContent};

use crate::error::{LlmError, Result};

/// The intersection of image MIME types every vendor's vision API accepts.
///
/// The intersection rather than the union: formats in the union (BMP, TIFF,
/// AVIF, …) are supported by only a few providers, and letting one through this
/// layer merely defers the error to a vague 400 from the remote end.
pub const SUPPORTED_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// The byte ceiling for a single image (**raw bytes**, not post-base64).
///
/// 5 MiB is Anthropic's hard limit for one image and the strictest among the
/// vendors; base64 inflates by another 4/3, so capping on raw bytes satisfies
/// everyone at once.
///
/// Nothing is auto-downscaled when over the limit: decoding and re-encoding
/// would drag in an image-processing dependency, and the right move is to
/// generate a thumbnail **at upload time** (when a decode has to happen anyway)
/// rather than patching it up as the request goes out.
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Normalize a MIME type into one of the canonical values in
/// [`SUPPORTED_IMAGE_MIMES`].
///
/// Handles three spellings encountered daily in the real world: with parameters
/// (`image/jpeg; charset=binary`), inconsistent case (`IMAGE/PNG`), and
/// `image/jpg`, which has never been valid yet is used everywhere.
///
/// # Errors
///
/// Still not on the supported list after normalization.
pub fn normalize_image_mime(mime: &str) -> Result<&'static str> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // image/jpg is not a registered MIME type, but browsers and mobile clients
    // send it exactly like that
    let bare = if bare == "image/jpg" {
        "image/jpeg"
    } else {
        bare.as_str()
    };
    SUPPORTED_IMAGE_MIMES
        .iter()
        .find(|m| **m == bare)
        .copied()
        .ok_or_else(|| LlmError::UnsupportedImageMime {
            mime: mime.to_string(),
            supported: SUPPORTED_IMAGE_MIMES.join(", "),
        })
}

/// Encode image bytes into a `data:` URL.
///
/// For callers that assemble a payload themselves (e.g. handing an image to a
/// non-goose HTTP endpoint); the normal [`Message`] path does **not** need it —
/// goose assembles it itself.
///
/// # Errors
///
/// The MIME type is unsupported, or the byte count exceeds
/// [`MAX_IMAGE_BYTES`].
pub fn to_data_url(bytes: &[u8], mime: &str) -> Result<String> {
    let mime = normalize_image_mime(mime)?;
    check_size(bytes.len())?;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::prelude::BASE64_STANDARD.encode(bytes)
    ))
}

/// Parse `data:image/png;base64,…` back into a canonical MIME type and the
/// base64 payload.
///
/// Only the `;base64` form is accepted: `data:` URLs also permit
/// percent-encoded raw bytes, but no provider sends images that way, and
/// supporting it would just be inviting parsing bugs.
///
/// # Errors
///
/// Not a `data:` URL, not base64-encoded, an unsupported MIME type, or over the
/// size ceiling once decoded.
pub fn parse_data_url(url: &str) -> Result<(&'static str, String)> {
    let rest = url
        .strip_prefix("data:")
        .ok_or_else(|| LlmError::InvalidImage("not a data: URL".into()))?;
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| LlmError::InvalidImage("data: URL has no comma-separated payload".into()))?;
    let meta_lower = meta.to_ascii_lowercase();
    if !meta_lower.ends_with(";base64") {
        return Err(LlmError::InvalidImage(
            "only the ;base64 form of data: URL is supported".into(),
        ));
    }
    let mime = normalize_image_mime(&meta_lower[..meta_lower.len() - ";base64".len()])?;

    // Decode once purely to verify the payload is valid and within the size
    // limit — sending bad base64 out gets back a bare "invalid image" from the
    // provider, which is far harder to trace than saying so right here
    let decoded = base64::prelude::BASE64_STANDARD
        .decode(payload.trim())
        .map_err(|e| LlmError::InvalidImage(format!("base64 decode failed: {e}")))?;
    check_size(decoded.len())?;

    Ok((mime, payload.trim().to_string()))
}

fn check_size(len: usize) -> Result<()> {
    if len > MAX_IMAGE_BYTES {
        return Err(LlmError::ImageTooLarge {
            bytes: len,
            limit: MAX_IMAGE_BYTES,
        });
    }
    if len == 0 {
        return Err(LlmError::InvalidImage("the image has zero bytes".into()));
    }
    Ok(())
}

/// Give [`Message`] a convenient "attach an image from bytes" construction.
///
/// goose's own [`Message::with_image`] takes a base64 string and validates
/// nothing; the methods here all return a [`Result`], keeping "wrong format /
/// image too large / broken base64" out of the request.
///
/// ```no_run
/// # fn demo(png: &[u8]) -> cortex_llm::Result<()> {
/// use cortex_llm::{Message, MessageImageExt};
///
/// let msg = Message::user()
///     .with_text("What is the error in this screenshot?")
///     .with_image_bytes(png, "image/png")?;
/// assert_eq!(msg.image_count(), 1);
/// # Ok(())
/// # }
/// ```
pub trait MessageImageExt: Sized {
    /// Append an image (raw bytes; base64 is applied internally).
    ///
    /// # Errors
    ///
    /// Unsupported MIME type, empty bytes, or over [`MAX_IMAGE_BYTES`].
    fn with_image_bytes(self, bytes: &[u8], mime: &str) -> Result<Self>;

    /// Append an image (a payload that is already base64).
    ///
    /// # Errors
    ///
    /// Unsupported MIME type, or invalid base64 / over the limit once decoded.
    fn with_image_base64(self, data: &str, mime: &str) -> Result<Self>;

    /// Append an image (`data:image/png;base64,…`).
    ///
    /// # Errors
    ///
    /// See [`parse_data_url`].
    fn with_image_data_url(self, data_url: &str) -> Result<Self>;

    /// How many images this message carries.
    fn image_count(&self) -> usize;

    /// Whether this message carries an image.
    fn has_image(&self) -> bool {
        self.image_count() > 0
    }
}

impl MessageImageExt for Message {
    fn with_image_bytes(self, bytes: &[u8], mime: &str) -> Result<Self> {
        let mime = normalize_image_mime(mime)?;
        check_size(bytes.len())?;
        let data = base64::prelude::BASE64_STANDARD.encode(bytes);
        Ok(self.with_image(data, mime))
    }

    fn with_image_base64(self, data: &str, mime: &str) -> Result<Self> {
        let mime = normalize_image_mime(mime)?;
        let decoded = base64::prelude::BASE64_STANDARD
            .decode(data.trim())
            .map_err(|e| LlmError::InvalidImage(format!("base64 decode failed: {e}")))?;
        check_size(decoded.len())?;
        Ok(self.with_image(data.trim().to_string(), mime))
    }

    fn with_image_data_url(self, data_url: &str) -> Result<Self> {
        let (mime, data) = parse_data_url(data_url)?;
        Ok(self.with_image(data, mime))
    }

    fn image_count(&self) -> usize {
        self.content
            .iter()
            .filter(|c| matches!(c, MessageContent::Image(_)))
            .count()
    }
}

/// How many images a whole conversation carries in total.
#[must_use]
pub fn image_count(messages: &[Message]) -> usize {
    messages.iter().map(MessageImageExt::image_count).sum()
}

// ──────────────────── capability determination ────────────────────

/// Whether a given model can see images.
///
/// Three states rather than a bool, because **"unknown" and "no" must be kept
/// apart**: goose's own catalog holds nearly 40 providers, and we have neither
/// the means nor the obligation to research the vision capability of every one
/// of their models; treating unknown as "no" would have this layer reject, for
/// no reason, providers that can in fact see images.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionSupport {
    /// The definition JSON explicitly declares `"vision": true`.
    Supported,
    /// The definition JSON explicitly declares `"vision": false`. Requests with
    /// images get blocked by this layer.
    Unsupported,
    /// The definition says nothing. It may be a provider from goose's catalog,
    /// or one like Ollama whose model list is decided dynamically by the
    /// server. **Let it through** and leave the remote end to complain.
    Unknown,
}

impl VisionSupport {
    /// Whether messages carrying images may be sent.
    ///
    /// [`Self::Unknown`] is let through — see the type's docs for why.
    #[must_use]
    pub fn allows_images(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest valid PNG: 1×1, fully transparent.
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn mime_is_normalized_not_merely_matched() {
        assert_eq!(normalize_image_mime("IMAGE/PNG").unwrap(), "image/png");
        assert_eq!(
            normalize_image_mime("image/jpeg; charset=binary").unwrap(),
            "image/jpeg"
        );
        assert_eq!(
            normalize_image_mime("image/jpg").unwrap(),
            "image/jpeg",
            "image/jpg is not a valid MIME type, but browsers and mobile \
             clients send exactly that, so it must be normalized"
        );
        assert_eq!(normalize_image_mime(" image/webp ").unwrap(), "image/webp");
    }

    #[test]
    fn unsupported_mime_names_the_alternatives() {
        let err = normalize_image_mime("image/bmp")
            .expect_err("BMP is not in the intersection and should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("image/png"),
            "the error must list the usable formats, otherwise the caller has \
             to go read the source; got: {msg}"
        );
        assert!(
            normalize_image_mime("application/pdf").is_err(),
            "a non-image MIME type must be rejected, otherwise it goes out as \
             an image and comes back as a vague 400"
        );
    }

    #[test]
    fn bytes_become_one_image_block() {
        let msg = Message::user()
            .with_text("这是什么")
            .with_image_bytes(PNG_1X1, "image/png")
            .expect("a valid PNG should attach");
        assert_eq!(msg.image_count(), 1);
        assert!(msg.has_image());
        // The text block must still be there — the image must not displace the
        // question
        assert!(msg.as_concat_text().contains("这是什么"));
    }

    #[test]
    fn image_data_is_base64_of_the_original_bytes() {
        let msg = Message::user()
            .with_image_bytes(PNG_1X1, "image/png")
            .unwrap();
        let MessageContent::Image(img) = &msg.content[0] else {
            panic!("the first content block should be an Image");
        };
        let decoded = base64::prelude::BASE64_STANDARD.decode(&img.data).unwrap();
        assert_eq!(
            decoded, PNG_1X1,
            "the encoding must be lossless: one byte more or one byte less and \
             the remote end rejects the image"
        );
        assert_eq!(img.mime_type, "image/png");
    }

    #[test]
    fn oversize_and_empty_are_rejected_before_the_wire() {
        let huge = vec![0u8; MAX_IMAGE_BYTES + 1];
        assert!(
            matches!(
                Message::user().with_image_bytes(&huge, "image/png"),
                Err(LlmError::ImageTooLarge { .. })
            ),
            "going over the limit must be caught locally: sending it merely \
             wastes a round of upstream bandwidth and comes back a 400"
        );
        assert!(
            Message::user().with_image_bytes(&[], "image/png").is_err(),
            "empty bytes are not an image"
        );
    }

    #[test]
    fn data_url_round_trips() {
        let url = to_data_url(PNG_1X1, "image/png").unwrap();
        assert!(url.starts_with("data:image/png;base64,"));

        let msg = Message::user().with_image_data_url(&url).unwrap();
        assert_eq!(msg.image_count(), 1);

        let (mime, data) = parse_data_url(&url).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(
            base64::prelude::BASE64_STANDARD.decode(data).unwrap(),
            PNG_1X1
        );
    }

    #[test]
    fn broken_data_urls_are_rejected_with_a_reason() {
        for (bad, why) in [
            ("https://example.com/a.png", "not a data: URL"),
            ("data:image/png;base64", "missing comma"),
            ("data:image/png,rawbytes", "not the base64 form"),
            ("data:image/png;base64,!!!!", "invalid base64"),
            ("data:text/plain;base64,aGk=", "MIME is not an image"),
        ] {
            assert!(
                parse_data_url(bad).is_err(),
                "a data URL that is {why} should be rejected: {bad}"
            );
        }
    }

    #[test]
    fn base64_payload_is_validated_not_trusted() {
        assert!(
            Message::user()
                .with_image_base64("not base64 at all!!", "image/png")
                .is_err(),
            "broken base64 must fail on the spot, otherwise the error shows up \
             far away as the remote end's invalid image"
        );
        let ok = base64::prelude::BASE64_STANDARD.encode(PNG_1X1);
        assert!(Message::user().with_image_base64(&ok, "image/png").is_ok());
    }

    #[test]
    fn counts_images_across_a_conversation() {
        let msgs = [
            Message::user().with_text("只有文字"),
            Message::user()
                .with_image_bytes(PNG_1X1, "image/png")
                .unwrap()
                .with_image_bytes(PNG_1X1, "image/jpeg")
                .unwrap(),
        ];
        assert_eq!(image_count(&msgs), 2);
        assert_eq!(image_count(&msgs[..1]), 0);
    }

    #[test]
    fn unknown_capability_is_permissive() {
        // This is a deliberate tradeoff: we do not research each of the nearly
        // 40 providers in goose's catalog, and rejecting everything unknown
        // would wrongly hit the ones that really can see images
        assert!(VisionSupport::Unknown.allows_images());
        assert!(VisionSupport::Supported.allows_images());
        assert!(!VisionSupport::Unsupported.allows_images());
    }
}

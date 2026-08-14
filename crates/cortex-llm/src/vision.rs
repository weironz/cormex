//! 图像内容块：让 [`Message`] 带图，并在发出去之前判断模型是否真能看图。
//!
//! # 这一层为什么这么薄
//!
//! 各家的图像格式并不一样 —— Anthropic 要
//! `{"type":"image","source":{"type":"base64","media_type":…,"data":…}}`，
//! OpenAI 兼容那一系要 `{"type":"image_url","image_url":{"url":"data:…"}}`。
//! 但这个差异**在取件的 goose 里已经解决了**：
//! `goose_provider_types::images::convert_image` 按 `ImageFormat` 分派，
//! anthropic 引擎传 `ImageFormat::Anthropic`，openai / ollama 引擎传
//! `ImageFormat::OpenAi`，而声明式配置的 `engine` 字段决定走哪个引擎。
//!
//! 也就是说：**只要把图塞进 `Message`，格式转换就是免费的**。
//! 本模块因此只做 goose 没做的三件事：
//!
//! 1. 从**字节**造图（goose 的 [`Message::with_image`] 只收 base64 字符串，
//!    而上传管线拿到的是 `&[u8]`）；
//! 2. MIME 归一化与体积上限 —— 各家 vision API 只认几种格式，
//!    传错的表现是一个含糊的 400，不如在本地就说清楚；
//! 3. **能力判定** —— goose 的 `ModelInfo` 没有 vision 字段，
//!    而 DeepSeek 当前完全没有 vision 能力。没有这一步，用户传的图会
//!    以「模型说它没看到图片」的形式静默丢失，这是最难查的一类问题。

use base64::Engine as _;
use goose_providers::conversation::message::{Message, MessageContent};

use crate::error::{LlmError, Result};

/// 各家 vision API 都认的图像 MIME 交集。
///
/// 取交集而非并集：并集里的格式（BMP、TIFF、AVIF…）只有个别供应商支持，
/// 让它通过这一层只会把错误推迟到一个含糊的远端 400。
pub const SUPPORTED_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// 单张图的字节上限（**原始字节**，不是 base64 之后）。
///
/// 5 MiB 是 Anthropic 对单张图的硬上限，也是各家里最严的一档；
/// base64 会再涨 4/3，所以按原始字节卡住就同时满足了所有人。
///
/// 超限不做自动缩放：解码 + 重编码要拖进图像处理依赖，而正确的做法是
/// 在**上传时**就生成缩略图（那时本来就要解一次），不是在发请求时补救。
pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// 把 MIME 归一化成 [`SUPPORTED_IMAGE_MIMES`] 里的规范值。
///
/// 处理三种现实里天天遇到的写法：带参数（`image/jpeg; charset=binary`）、
/// 大小写不一致（`IMAGE/PNG`）、以及 `image/jpg` 这个从来不合法却到处都在用的拼法。
///
/// # Errors
///
/// 归一化之后仍不在支持列表里。
pub fn normalize_image_mime(mime: &str) -> Result<&'static str> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // image/jpg 不是注册过的 MIME，但浏览器与移动端就是这么发的
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

/// 把图像字节编成 `data:` URL。
///
/// 供需要自己拼 payload 的调用方（例如把图交给非 goose 的 HTTP 端点）使用；
/// 走 [`Message`] 的正常路径**不需要**它 —— goose 会自己拼。
///
/// # Errors
///
/// MIME 不支持或字节数超过 [`MAX_IMAGE_BYTES`]。
pub fn to_data_url(bytes: &[u8], mime: &str) -> Result<String> {
    let mime = normalize_image_mime(mime)?;
    check_size(bytes.len())?;
    Ok(format!(
        "data:{mime};base64,{}",
        base64::prelude::BASE64_STANDARD.encode(bytes)
    ))
}

/// 解析 `data:image/png;base64,…`，还原出规范 MIME 与 base64 载荷。
///
/// 只接受 `;base64` 的形式：`data:` URL 也允许 percent-encoding 的裸字节，
/// 但没有任何供应商会那样发图，支持它只是给自己找解析 bug。
///
/// # Errors
///
/// 不是 `data:` URL、不是 base64 编码、MIME 不支持，或解码后超过体积上限。
pub fn parse_data_url(url: &str) -> Result<(&'static str, String)> {
    let rest = url
        .strip_prefix("data:")
        .ok_or_else(|| LlmError::InvalidImage("不是 data: URL".into()))?;
    let (meta, payload) = rest
        .split_once(',')
        .ok_or_else(|| LlmError::InvalidImage("data: URL 缺少逗号分隔的载荷".into()))?;
    let meta_lower = meta.to_ascii_lowercase();
    if !meta_lower.ends_with(";base64") {
        return Err(LlmError::InvalidImage(
            "只支持 ;base64 形式的 data: URL".into(),
        ));
    }
    let mime = normalize_image_mime(&meta_lower[..meta_lower.len() - ";base64".len()])?;

    // 解一次只为验证载荷合法且体积没超 —— 把一段坏 base64 发出去，
    // 换回来的是供应商一句「invalid image」，比在这里说清楚难查得多
    let decoded = base64::prelude::BASE64_STANDARD
        .decode(payload.trim())
        .map_err(|e| LlmError::InvalidImage(format!("base64 解码失败：{e}")))?;
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
        return Err(LlmError::InvalidImage("图像字节为空".into()));
    }
    Ok(())
}

/// 给 [`Message`] 补上「从字节带图」的便捷构造。
///
/// goose 自带的 [`Message::with_image`] 收的是 base64 字符串且不校验任何东西；
/// 这里的几个方法都返回 [`Result`]，把「格式不对 / 图太大 / base64 是坏的」
/// 挡在发请求之前。
///
/// ```no_run
/// # fn demo(png: &[u8]) -> cortex_llm::Result<()> {
/// use cortex_llm::{Message, MessageImageExt};
///
/// let msg = Message::user()
///     .with_text("这张截图里的报错是什么？")
///     .with_image_bytes(png, "image/png")?;
/// assert_eq!(msg.image_count(), 1);
/// # Ok(())
/// # }
/// ```
pub trait MessageImageExt: Sized {
    /// 追加一张图（原始字节，内部做 base64）。
    ///
    /// # Errors
    ///
    /// MIME 不支持、字节为空，或超过 [`MAX_IMAGE_BYTES`]。
    fn with_image_bytes(self, bytes: &[u8], mime: &str) -> Result<Self>;

    /// 追加一张图（已经是 base64 的载荷）。
    ///
    /// # Errors
    ///
    /// MIME 不支持，或 base64 非法 / 解码后超限。
    fn with_image_base64(self, data: &str, mime: &str) -> Result<Self>;

    /// 追加一张图（`data:image/png;base64,…`）。
    ///
    /// # Errors
    ///
    /// 见 [`parse_data_url`]。
    fn with_image_data_url(self, data_url: &str) -> Result<Self>;

    /// 这条消息里有几张图。
    fn image_count(&self) -> usize;

    /// 这条消息是否带图。
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
            .map_err(|e| LlmError::InvalidImage(format!("base64 解码失败：{e}")))?;
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

/// 一次对话里总共带了几张图。
#[must_use]
pub fn image_count(messages: &[Message]) -> usize {
    messages.iter().map(MessageImageExt::image_count).sum()
}

// ───────────────────────────── 能力判定 ─────────────────────────────

/// 某个模型能不能看图。
///
/// 三态而非 bool，因为**「不知道」和「不行」必须区分开**：
/// goose 自带目录里有近 40 家供应商，我们没有能力也没有义务去逐一考据
/// 它们每个模型的 vision 能力；把未知一律当成「不行」会让那些其实能看图的
/// 供应商在本层被无理由拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisionSupport {
    /// 定义 JSON 里显式声明 `"vision": true`。
    Supported,
    /// 定义 JSON 里显式声明 `"vision": false`。带图请求会被本层拦下。
    Unsupported,
    /// 定义里没说。可能是 goose 目录里的供应商，也可能是 Ollama 这类
    /// 模型列表由服务端动态决定的。**按放行处理**，让远端自己报错。
    Unknown,
}

impl VisionSupport {
    /// 是否允许把带图的消息发出去。
    ///
    /// [`Self::Unknown`] 放行 —— 理由见类型文档。
    #[must_use]
    pub fn allows_images(self) -> bool {
        !matches!(self, Self::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小合法 PNG：1×1 全透明。
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
            "image/jpg 不是合法 MIME，但浏览器与移动端就是这么发的，必须归一化"
        );
        assert_eq!(normalize_image_mime(" image/webp ").unwrap(), "image/webp");
    }

    #[test]
    fn unsupported_mime_names_the_alternatives() {
        let err = normalize_image_mime("image/bmp").expect_err("BMP 不在交集里，应被拒绝");
        let msg = err.to_string();
        assert!(
            msg.contains("image/png"),
            "报错必须列出可用格式，否则调用方只能去翻源码；实际：{msg}"
        );
        assert!(
            normalize_image_mime("application/pdf").is_err(),
            "非图片 MIME 必须拒绝，否则会被当成图发出去换回一个含糊的 400"
        );
    }

    #[test]
    fn bytes_become_one_image_block() {
        let msg = Message::user()
            .with_text("这是什么")
            .with_image_bytes(PNG_1X1, "image/png")
            .expect("合法 PNG 应能挂上");
        assert_eq!(msg.image_count(), 1);
        assert!(msg.has_image());
        // 文本块必须还在 —— 图不能把提问顶掉
        assert!(msg.as_concat_text().contains("这是什么"));
    }

    #[test]
    fn image_data_is_base64_of_the_original_bytes() {
        let msg = Message::user()
            .with_image_bytes(PNG_1X1, "image/png")
            .unwrap();
        let MessageContent::Image(img) = &msg.content[0] else {
            panic!("第一个内容块应该是 Image");
        };
        let decoded = base64::prelude::BASE64_STANDARD.decode(&img.data).unwrap();
        assert_eq!(
            decoded, PNG_1X1,
            "编码必须无损：多一个字节少一个字节都会让远端拒图"
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
            "超限必须在本地拦下：发出去只会浪费一次上行带宽再换回 400"
        );
        assert!(
            Message::user().with_image_bytes(&[], "image/png").is_err(),
            "空字节不是图片"
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
            ("https://example.com/a.png", "不是 data: URL"),
            ("data:image/png;base64", "缺少逗号"),
            ("data:image/png,rawbytes", "不是 base64 形式"),
            ("data:image/png;base64,!!!!", "base64 非法"),
            ("data:text/plain;base64,aGk=", "MIME 不是图片"),
        ] {
            assert!(
                parse_data_url(bad).is_err(),
                "{why} 的 data URL 应被拒绝：{bad}"
            );
        }
    }

    #[test]
    fn base64_payload_is_validated_not_trusted() {
        assert!(
            Message::user()
                .with_image_base64("not base64 at all!!", "image/png")
                .is_err(),
            "坏 base64 必须当场报错，否则错误会以远端的 invalid image 出现在很远的地方"
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
        // 这条是刻意的取舍：goose 目录里近 40 家供应商我们不逐一考据，
        // 未知一律拒绝会误伤真的能看图的那些
        assert!(VisionSupport::Unknown.allows_images());
        assert!(VisionSupport::Supported.allows_images());
        assert!(!VisionSupport::Unsupported.allows_images());
    }
}

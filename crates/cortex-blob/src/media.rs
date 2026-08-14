//! 媒体元数据探测：从字节流认出 MIME、图片宽高。
//!
//! # 为什么要在这一层做
//!
//! 客户端上报的 `Content-Type` 不可信（浏览器按扩展名猜，移动端常直接给
//! `application/octet-stream`），而 `blobs.mime` 决定了下游怎么处理这份内容：
//! 转录 pipeline 靠它分流到 ASR / vision / OCR，前端靠它决定用哪个播放器。
//! 猜错的代价是这份内容**永久成为检索黑洞** —— 没人会回头重扫。
//!
//! 所以：以字节头为准，客户端声明只作后备。
//!
//! # 只读文件头，不解码
//!
//! 全部探测都只看前几十字节。对一段 200 MB 的视频不解码就是几百纳秒，
//! 而解码它需要几百 MB 内存 —— 这是把探测放在上传热路径上的前提。

use serde::{Deserialize, Serialize};

/// 从字节流探测出的媒体元数据。字段全部可选：探不出来就是探不出来，
/// 绝不编一个默认值（`0x0` 的宽高会让前端布局算出除零）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaMeta {
    /// 由字节头判定的 MIME。探不出时为 `None`，调用方回落到客户端声明。
    pub mime: Option<String>,
    /// 图片像素宽高。视频尺寸需要解容器，属于「重依赖」那一档，见下。
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// 音视频时长。**当前恒为 `None`**，理由见 [`probe`] 的文档。
    pub duration_ms: Option<u64>,
}

impl MediaMeta {
    /// 取一个可直接写进 `blobs.mime` 的值：字节头优先，客户端声明兜底。
    ///
    /// 二者都缺时给 `application/octet-stream` —— 这是 `blobs.mime NOT NULL`
    /// 的要求，也是 HTTP 对「不知道是什么」的标准答案。
    #[must_use]
    pub fn resolve_mime(&self, declared: Option<&str>) -> String {
        self.mime
            .clone()
            .or_else(|| declared.map(str::to_owned).filter(|s| !s.is_empty()))
            .unwrap_or_else(|| DEFAULT_MIME.to_owned())
    }

    /// 是否是图片。转录 pipeline 靠它分流到 vision/OCR。
    #[must_use]
    pub fn is_image(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
    }

    /// 是否是音视频。分流到 ASR，也是将来时长探测唯一需要处理的一类。
    #[must_use]
    pub fn is_audio_video(&self) -> bool {
        self.mime
            .as_deref()
            .is_some_and(|m| m.starts_with("video/") || m.starts_with("audio/"))
    }
}

/// 不知道是什么时的 MIME。
pub const DEFAULT_MIME: &str = "application/octet-stream";

/// 探测一段字节的媒体元数据。
///
/// # 时长为什么没做
///
/// 拿到时长要解容器：MP4 得走 box 树读 `mvhd.duration/timescale`，
/// Matroska 得读 `Segment/Info/Duration`，MP3 无容器只能扫帧算，
/// 而 VBR 的 MP3 不扫完全文件就是错的。真做全格式只有两条路：
///
/// - 绑 ffmpeg：拖进 C 依赖与 GPL/LGPL 的许可证讨论，六端交叉编译全部受累
/// - 上 `symphonia`：纯 Rust，但为了一个数字要背整套解复用器
///
/// 而**时长现在没有消费方** —— 播放器自己会从流里读，检索用的是
/// `blob_transcripts.span_*_ms`（由 ASR 产出，本来就带时间轴）。
/// 所以留 `duration_ms: Option<u64>` 这个位置，等 ASR pipeline 落地时
/// 顺手从转录结果里拿——那时它是免费的。
///
/// 在此之前此字段恒为 `None`，**不要**误以为「探测失败」。
#[must_use]
pub fn probe(bytes: &[u8]) -> MediaMeta {
    let mime = infer::get(bytes).map(|t| t.mime_type().to_owned());

    // 只对确实是图片的字节调 imagesize：它对非图片会做一轮无谓的头部匹配
    let (width, height) = match mime.as_deref() {
        Some(m) if m.starts_with("image/") => match imagesize::blob_size(bytes) {
            Ok(size) => (
                u32::try_from(size.width).ok(),
                u32::try_from(size.height).ok(),
            ),
            // 认得出是图片却读不出尺寸，多半是被截断的上传。
            // 不升级为错误：内容本身照存不误，尺寸缺失只是前端少一个布局提示。
            Err(e) => {
                tracing::debug!(error = %e, mime = m, "认出是图片但读不出尺寸，按未知处理");
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

    /// 最小合法 PNG：1×1 全透明。用真实字节而不是造假的头，
    /// 才能同时验到 infer 与 imagesize 两条路径。
    const PNG_1X1: &[u8] = &[
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F,
        0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49,
        0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
    ];

    /// 最小 GIF87a：3×5，尺寸写在第 7..10 字节，用来验「宽高不是写死的」。
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
            "PNG 魔数没认出来，转录 pipeline 会把图片当二进制丢掉"
        );
        assert_eq!(
            (meta.width, meta.height),
            (Some(1), Some(1)),
            "1×1 PNG 的尺寸读错"
        );
        assert!(meta.is_image(), "PNG 应被判定为图片以便分流到 vision/OCR");
        assert!(!meta.is_audio_video(), "PNG 不该被判定为音视频");
    }

    #[test]
    fn dimensions_are_read_not_guessed() {
        let meta = probe(GIF_3X5);
        assert_eq!(meta.mime.as_deref(), Some("image/gif"), "GIF 魔数没认出来");
        assert_eq!(
            (meta.width, meta.height),
            (Some(3), Some(5)),
            "宽高应真从文件头读出（3×5 非正方，写死值会在这里露馅）"
        );
    }

    #[test]
    fn unknown_bytes_yield_no_guesses() {
        let meta = probe(b"just some plain text, no magic bytes at all");
        assert_eq!(meta.mime, None, "认不出就该是 None，绝不编造 MIME");
        assert_eq!(
            (meta.width, meta.height),
            (None, None),
            "非图片必须留空，编一个 0×0 会让前端布局除零"
        );
    }

    #[test]
    fn sniffed_mime_beats_client_declaration() {
        let meta = probe(PNG_1X1);
        assert_eq!(
            meta.resolve_mime(Some("application/octet-stream")),
            "image/png",
            "客户端声明不可信 —— 字节头认出是 PNG 时必须以字节头为准"
        );

        let unknown = probe(b"...");
        assert_eq!(
            unknown.resolve_mime(Some("text/markdown")),
            "text/markdown",
            "字节头认不出时才回落到客户端声明"
        );
        assert_eq!(
            unknown.resolve_mime(None),
            DEFAULT_MIME,
            "两者都缺时须给出可写进 blobs.mime NOT NULL 的值"
        );
        assert_eq!(
            unknown.resolve_mime(Some("")),
            DEFAULT_MIME,
            "空字符串的声明等同于没有声明，不能原样写进 blobs.mime"
        );
    }

    #[test]
    fn duration_is_deliberately_absent() {
        // 这不是缺陷是取舍：见 probe 的文档。此断言在真做时长探测时会失败，
        // 那正是提醒去更新文档与调用方的时刻。
        assert_eq!(
            probe(PNG_1X1).duration_ms,
            None,
            "时长探测尚未实现（等 ASR pipeline 顺手产出），当前必须恒为 None"
        );
    }

    #[test]
    fn empty_input_does_not_panic() {
        let meta = probe(&[]);
        assert_eq!(
            meta,
            MediaMeta::default(),
            "空输入应得到全空元数据而不是 panic"
        );
    }
}

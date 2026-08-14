//! Cortex 对象存储：内容寻址的 blob 读写。
//!
//! 「全模态归档」的地基 —— 图片、音频、视频、文档都以 SHA-256 为 key 落在这里，
//! `blobs` / `episode_blobs` / `blob_transcripts` 三张表只存指针与派生文本。
//!
//! # 内容寻址带来的三个「天然」
//!
//! key 由内容决定，于是：
//!
//! - **天然去重**：同一张图被十个 episode 引用，磁盘上只有一份 ——
//!   但**只在同一租户之内**。key 带租户前缀（见 [`hash::storage_key`]），
//!   跨租户的去重是被刻意放弃的，换掉的是一整类跨 schema 的 GC bug
//! - **天然幂等**：上传中断后整段重放即可，不需要断点续传协议 ——
//!   这正是 `docs/memory.md` §九「blob 三步固定顺序、中断按队列重放」成立的前提
//! - **天然不可变**：一个 key 的字节永远是同一份，「旧版本」这个概念
//!   根本不存在，所以 RustFS 不支持对象版本控制并无妨
//!
//! # 两个后端
//!
//! | | 用途 |
//! |---|---|
//! | [`S3BlobStore`] | 生产。RustFS / MinIO / R2 / AWS S3 通用 |
//! | [`LocalFsBlobStore`] | 测试，以及「第一版不起 RustFS 也能跑通」 |
//!
//! 二者都封在 [`BlobStore`] 后面。RustFS 尚处 alpha（社区已有 disk-full
//! 元数据损坏不可恢复的实录），这层抽象就是那条退路。

#![forbid(unsafe_code)]

mod error;
mod local;
mod s3;

pub mod hash;
pub mod media;

use std::{ops::Range, time::Duration};

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use url::Url;

pub use error::{BlobError, Result};
pub use hash::TenantPrefix;
pub use local::LocalFsBlobStore;
pub use media::{MediaMeta, probe as probe_media};
pub use s3::{MULTIPART_PART_SIZE, MULTIPART_THRESHOLD, S3BlobStore, S3Options};

/// presigned URL 的有效期上限。SigV4 的硬限制，不是我们定的。
pub const MAX_PRESIGN_TTL: Duration = Duration::from_secs(7 * 24 * 3600);

/// 一次写入的结果。字段刻意与 `cortex_store::NewBlob` 对齐，
/// 但**不**依赖那个 crate —— 依赖方向是 `core ← {llm, store}`，
/// blob 层横在 store 旁边，反向依赖会成环。转换由 cortexd 在边界处做。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    /// SHA-256 十六进制小写，与 migration 的 `sha256` domain 同形。
    pub hash: String,
    pub mime: String,
    pub size_bytes: i64,
    /// 对象 key。存进 `blobs.storage_key`，由 [`hash::storage_key`] 从
    /// 「租户前缀 + 哈希」派生；两个后端共用同一套布局，因此换后端不必回写这一列。
    pub storage_key: String,
    /// 本次写入是否命中了已有对象（即：没有真的上传字节）。
    ///
    /// 只用于观测（去重率、省下多少带宽）。调用方**不该**据此决定要不要写
    /// `blobs` 行 —— 那一行的幂等由 `ON CONFLICT (hash) DO NOTHING` 保证，
    /// 依赖这个布尔值会在并发下漏行。
    pub deduplicated: bool,
}

/// 内容寻址的对象存储。
///
/// # 实现者须知
///
/// 所有以 `hash` 为入参的方法**必须**先过 [`hash::validate_hash`]。
/// 那不只是格式检查，是路径遍历的唯一栅栏（详见该函数文档）。
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// 内容寻址写入，返回 SHA-256。已存在则跳过上传（天然去重）。
    ///
    /// `declared_mime` 是客户端的声明，仅作后备：能从字节头认出类型时
    /// 一律以字节头为准（浏览器按扩展名猜、移动端常给 `octet-stream`，都不可信）。
    ///
    /// # 并发写同一份内容
    ///
    /// 两个请求同时上传同一张图时，双方都会看到「不存在」并各自上传。
    /// **这是良性的**：内容寻址下两次写入的目标 key 相同、字节相同，
    /// 谁后到都不改变结果。所以此处刻意不加分布式锁 ——
    /// 为一次可忽略的重复上传引入锁，是拿可用性换带宽，赔本。
    ///
    /// 真正要当心的是**非原子写**的后端：写到一半被另一个读者看见就是坏数据。
    /// [`LocalFsBlobStore`] 为此走「临时文件 + rename」，S3 的 PUT 本身即原子。
    async fn put(&self, bytes: Bytes, declared_mime: Option<&str>) -> Result<BlobRef>;

    /// 按哈希取回完整内容。
    async fn get(&self, hash: &str) -> Result<Bytes>;

    /// 取回字节区间 `[start, end)`（左闭右开，与 Rust 惯例一致；
    /// HTTP Range 的右闭端由实现负责换算）。
    ///
    /// 音视频拖动播放要靠它：播放器只要 31:40 那一段，
    /// 没有 range 就得把整个视频拉下来。
    async fn get_range(&self, hash: &str, range: Range<u64>) -> Result<Bytes>;

    /// 签发直传 URL，客户端 `PUT` 字节到该 URL，不经 cortexd 中转。
    ///
    /// 手机传视频时这能省一半带宽（否则同一份字节要先上行到 cortexd，
    /// 再由它上行到对象存储）。
    ///
    /// 调用方必须**先算好哈希**再来要 URL —— 内容寻址不允许「先传后定 key」。
    async fn presign_put(&self, hash: &str, ttl: Duration) -> Result<Url>;

    /// 签发直下 URL，同理省一半带宽。
    async fn presign_get(&self, hash: &str, ttl: Duration) -> Result<Url>;

    /// 对象是否存在。
    async fn exists(&self, hash: &str) -> Result<bool>;

    /// 删除对象。**purge 专用**（`docs/memory.md` §十一）。
    ///
    /// # 调用方的义务
    ///
    /// 内容寻址意味着一份字节可能被多个 episode 引用。
    /// purge 前必须先确认 `episode_blobs` 里再无其他引用 —— 否则删掉的是
    /// 别人的附件。这个检查**只能在调用方做**：blob 层看不见 Postgres，
    /// 硬要它去查就把依赖方向掰反了。
    ///
    /// 删不存在的对象视为成功（幂等），级联清除任务据此可重跑。
    async fn delete(&self, hash: &str) -> Result<()>;
}

/// 校验 presign 的 ttl 落在 SigV4 允许的区间内。
///
/// 越界不静默截断：签出一个比调用方以为的短的 URL，
/// 会变成「偶尔过期」的间歇性故障，比直接报错难查一个数量级。
pub(crate) fn validate_ttl(ttl: Duration) -> Result<()> {
    if ttl.is_zero() {
        return Err(BlobError::Unsupported("presign ttl 不能为 0"));
    }
    if ttl > MAX_PRESIGN_TTL {
        return Err(BlobError::Backend(format!(
            "presign ttl {}s 超过 SigV4 上限 {}s（7 天）",
            ttl.as_secs(),
            MAX_PRESIGN_TTL.as_secs()
        )));
    }
    Ok(())
}

/// 校验 range 形制。空区间与首尾颠倒都要在发请求前拦下 ——
/// S3 对 `bytes=5-4` 会返回 416，而那时错误已经绕了一圈网络。
pub(crate) fn validate_range(range: &Range<u64>) -> Result<()> {
    if range.start >= range.end {
        return Err(BlobError::InvalidRange(format!(
            "空或颠倒的区间 [{}, {})",
            range.start, range.end
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttl_bounds_are_enforced() {
        assert!(
            validate_ttl(Duration::from_secs(300)).is_ok(),
            "5 分钟应是合法 ttl"
        );
        assert!(
            validate_ttl(MAX_PRESIGN_TTL).is_ok(),
            "恰好 7 天应在允许范围内"
        );
        assert!(
            validate_ttl(Duration::ZERO).is_err(),
            "0 ttl 必须拒 —— 签出立刻过期的 URL 是纯粹的浪费"
        );
        assert!(
            validate_ttl(MAX_PRESIGN_TTL + Duration::from_secs(1)).is_err(),
            "超过 7 天必须报错而非静默截断，否则会变成偶发过期的间歇性故障"
        );
    }

    #[test]
    fn degenerate_ranges_are_rejected() {
        assert!(validate_range(&(0..1)).is_ok(), "单字节区间应合法");
        assert!(
            validate_range(&(5..5)).is_err(),
            "空区间必须在本地拦下，否则要绕一圈网络才拿到 416"
        );
        // 用结构体字面量绕开 clippy 的 reversed_empty_ranges —— 这里要的
        // 恰恰是一个颠倒的区间，它正是被测的输入
        assert!(
            validate_range(&Range { start: 9, end: 4 }).is_err(),
            "首尾颠倒的区间必须拒"
        );
    }
}

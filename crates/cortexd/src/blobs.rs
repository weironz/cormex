//! 对象存储的接线：后端选择、presign 策略、HTTP Range 换算。
//!
//! `cortex-blob` 提供的是「内容寻址的字节读写」，本模块把它接到 HTTP 与
//! `blobs` 表之间的那道缝上 —— 也就是 docs/memory.md §九 那三步固定顺序里
//! 的第一步与第二步之间：
//!
//! ```text
//! ① RustFS 上传  ──►  ② blobs 行（同事务写 sync_log）  ──►  ③ episode + episode_blobs
//! ```
//!
//! 顺序反过来会留下「数据库说有、对象存储里没有」的悬空引用，而那是**无法
//! 自愈**的：客户端会永远拉到一条取不回内容的记录。反之（对象在、行还没写）
//! 只是一个孤儿对象，重放一次就补上了，服务端周期 GC 也能清掉。
//! 内容寻址让整段重放天然幂等，所以这个方向的取舍是免费的。

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use cortex_blob::{BlobStore, LocalFsBlobStore, S3BlobStore, S3Options};
use cortex_core::{Config, CortexError, Result};

/// presigned URL 的有效期。
///
/// 15 分钟：够手机在弱网下把一段视频传完，又短到即使 URL 被日志或截图带出去
/// 也很快作废。做成常量而非配置项 —— 这个数字没有需要按部署调的理由，
/// 而多一个旋钮就多一处能被调错的地方。
pub const PRESIGN_TTL: Duration = Duration::from_secs(15 * 60);

/// 服务端中转上传（`POST /blobs`）的体积上限。
///
/// 32 MiB 之上应当走 presign 直传：中转意味着同一份字节要先上行到 cortexd、
/// 再由它上行到对象存储，手机传视频时这是白白多付一倍的上行带宽。
/// 门槛不设得更低，是因为直传要多两次往返，小文件上并不划算。
pub const DIRECT_UPLOAD_LIMIT: usize = 32 * 1024 * 1024;

/// 嗅探 MIME 时读取的字节数。
///
/// 所有常见格式的魔数都在头 32 字节内，取 4 KiB 是为了给 PNG 的 IHDR、
/// MP4 的 ftyp 这类「要多看几个盒子」的探测留余量。走 range 读而不是整取，
/// 是因为直传路径上服务端根本没见过这份字节 —— 为了认一个 MIME 把整段视频
/// 拉下来，等于把刚省下的带宽再花回去。
const SNIFF_BYTES: u64 = 4096;

/// 本地回落后端的默认根目录。
const LOCAL_ROOT_ENV: &str = "CORTEX_BLOB_DIR";
const LOCAL_ROOT_DEFAULT: &str = ".cortex/blobs";

/// 对象存储句柄 + 它是哪一路后端。
#[derive(Clone)]
pub struct MediaStore {
    /// `None` 表示两路后端都没起来。此时 blob 端点一律报错而不是假装成功 ——
    /// 上传成功却没有字节落地，是「自证清白的坏数据」。
    inner: Option<Arc<dyn BlobStore>>,
    backend: &'static str,
}

impl MediaStore {
    /// 优先真实对象存储，连不上就回落本地文件系统并**明确告警**。
    ///
    /// 与数据库那套降级策略同源：开发机上少起一个 alpha 阶段的进程不该让
    /// 整个服务起不来，但降级必须在日志里说清楚，否则「东西传上去了、
    /// 换台机器就没了」会变成一桩悬案。
    pub async fn connect(config: &Config) -> Self {
        match Self::try_s3(&config.s3).await {
            Ok(s3) => {
                tracing::info!(
                    endpoint = %config.s3.endpoint,
                    bucket = %config.s3.bucket,
                    "对象存储已接入 S3 兼容后端"
                );
                return Self {
                    inner: Some(Arc::new(s3)),
                    backend: "s3",
                };
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    endpoint = %config.s3.endpoint,
                    "对象存储不可用，回落本地文件系统 —— 上传的媒体只存在于本机，\
                     不会随对象存储备份，也签不出 presigned URL"
                );
            }
        }

        let root = std::env::var(LOCAL_ROOT_ENV).unwrap_or_else(|_| LOCAL_ROOT_DEFAULT.to_string());
        match LocalFsBlobStore::new(&root, Self::current_tenant()).await {
            Ok(local) => {
                tracing::warn!(root, "对象存储已回落到本地文件系统");
                Self {
                    inner: Some(Arc::new(local)),
                    backend: "local_fs",
                }
            }
            Err(e) => {
                // 连本地目录都建不出来（只读文件系统、权限不足）。此时不能装作
                // 有对象存储：宁可让 /blobs 明确报错，也不能收下字节再丢掉。
                tracing::error!(error = %e, root, "本地回落也建不起来，媒体功能不可用");
                Self {
                    inner: None,
                    backend: "unavailable",
                }
            }
        }
    }

    /// 过渡期的租户前缀。
    ///
    /// 现在整个部署只有 1 号用户，而他的 schema 名就是 `public`
    /// （见 `migrations-global/20260810000002` 的文件头：存量数据不搬家）。
    /// 所以对象 key 的前缀也取 `public` —— **两者必须是同一个字符串**，
    /// 不然「这个用户的对象」与「这个用户的行」会指向两个不同的命名空间。
    ///
    /// 多用户接线之后这里换成从请求里取出的租户，而不是加一个 env。
    fn current_tenant() -> cortex_blob::TenantPrefix {
        cortex_blob::TenantPrefix::new("public").expect("public 是合法的租户前缀")
    }

    async fn try_s3(cfg: &cortex_core::config::S3Config) -> cortex_blob::Result<S3BlobStore> {
        let store = S3BlobStore::new(&S3Options::from_config(cfg, Self::current_tenant()))?;
        // ensure_bucket 是第一次真实网络调用，兼作连通性探针 ——
        // 构造本身不访问网络，光构造成功证明不了对端活着
        store.ensure_bucket().await?;
        Ok(store)
    }

    /// 一个**什么都不接**的媒体存储，专供 crate 内的 HTTP 测试。
    ///
    /// 存在的理由是 [`Self::connect`] 会真的去连 S3（`ensure_bucket` 是一次
    /// 网络往返），连不上还会在进程工作目录下**建一个本地回落目录**。
    /// 一条只想断言「没带凭据要拿 401」的测试不该为此等一次网络超时，
    /// 更不该在仓库里留下文件。
    ///
    /// 用 `inner: None` 而不是接一个 tempdir：媒体端点在这种状态下会明确报错，
    /// 而那正好让「测试里不小心真的走到了 blob 逻辑」变成一个响亮的失败，
    /// 而不是一个悄悄成功的假象。
    #[cfg(test)]
    pub(crate) const fn unavailable_for_tests() -> Self {
        Self {
            inner: None,
            backend: "unavailable",
        }
    }

    /// 报给 `/health` 的后端标识。`local_fs` 出现在生产日志里就是一条告警。
    #[must_use]
    pub const fn backend(&self) -> &'static str {
        self.backend
    }

    /// 这一路后端能不能签 presigned URL。
    ///
    /// 只有 S3 兼容后端可以。判断放在**发请求之前**，是为了让客户端拿到
    /// 501「本部署不支持直传」而不是 500 —— 前者说「别再试了，改走中转」，
    /// 后者说「服务端出故障了，等会儿重试」。移动端的重传逻辑会认真对待
    /// 这个区别，把它们混为一谈就是让客户端在一条永远走不通的路上重试。
    #[must_use]
    pub fn supports_presign(&self) -> bool {
        self.backend == "s3"
    }

    fn store(&self) -> Result<&Arc<dyn BlobStore>> {
        self.inner.as_ref().ok_or_else(|| {
            CortexError::Store("对象存储不可用：S3 连不上且本地回落目录建不出来".into())
        })
    }

    /// 服务端中转上传。返回内容哈希与嗅探出的 MIME。
    pub async fn put(
        &self,
        bytes: axum::body::Bytes,
        declared_mime: Option<&str>,
    ) -> Result<cortex_blob::BlobRef> {
        Ok(self.store()?.put(bytes, declared_mime).await?)
    }

    pub async fn get(&self, hash: &str) -> Result<axum::body::Bytes> {
        Ok(self.store()?.get(hash).await?)
    }

    pub async fn get_range(&self, hash: &str, range: Range<u64>) -> Result<axum::body::Bytes> {
        Ok(self.store()?.get_range(hash, range).await?)
    }

    pub async fn exists(&self, hash: &str) -> Result<bool> {
        Ok(self.store()?.exists(hash).await?)
    }

    pub async fn presign_put(&self, hash: &str) -> Result<String> {
        Ok(self.store()?.presign_put(hash, PRESIGN_TTL).await?.into())
    }

    pub async fn presign_get(&self, hash: &str) -> Result<String> {
        Ok(self.store()?.presign_get(hash, PRESIGN_TTL).await?.into())
    }

    /// 直传路径上的 MIME 判定：只拉对象头几 KB 来嗅探。
    ///
    /// **不信客户端声明的 MIME**，与 [`BlobStore::put`] 的规矩一致 ——
    /// 浏览器按扩展名猜、移动端常给 `application/octet-stream`，
    /// 一旦把 octet-stream 写进 `blobs.mime`，这份内容对将来的转录 pipeline
    /// 就是个黑洞（不知道该送 ASR 还是 OCR），而那时它已经沉在库底了。
    pub async fn sniff_mime(&self, hash: &str, declared: Option<&str>) -> Result<String> {
        let head = match self.get_range(hash, 0..SNIFF_BYTES).await {
            Ok(bytes) => bytes,
            // 对象比 SNIFF_BYTES 还短时某些后端会回 416；退回整取（这种对象本来就小）
            Err(CortexError::Invalid(_)) => self.get(hash).await?,
            Err(e) => return Err(e),
        };
        Ok(cortex_blob::probe_media(&head).resolve_mime(declared))
    }
}

// ─────────────────────── HTTP Range 换算 ────────────────────────

/// 解析出的字节区间，左闭右开（与 Rust 惯例一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeSpec {
    /// 整个对象
    Full,
    /// `[start, end)`，已按对象实际大小夹紧
    Partial(Range<u64>),
    /// 语法合法但完全落在对象之外 —— 必须回 416，不能回 200
    Unsatisfiable,
}

/// 解析 `Range` 头。
///
/// # 为什么自己写而不是找个库
///
/// 需要的只是 `bytes=` 这一种单区间形式 —— 播放器拖动进度条发的就是它。
/// 多区间（`bytes=0-99,200-299`）要求 multipart/byteranges 响应体，
/// 没有播放器指望服务端支持它，实现了也是死代码。
///
/// 三种形式都得认，缺一个都会有播放器踩空：
///
/// | 形式 | 谁在用 |
/// |---|---|
/// | `bytes=500-999` | 明确要一段 |
/// | `bytes=500-` | 从某处播到底 —— **拖动进度条最常见的形态** |
/// | `bytes=-500` | 要末尾 N 字节，MP4 的 moov 在文件尾时靠它 |
///
/// `header` 为 `None`（没带 Range）返回 [`RangeSpec::Full`]；
/// 语法不合法也返回 `Full` —— RFC 9110 明确要求「无法理解的 Range 头当作
/// 没带」，回 416 反而不合规。
#[must_use]
pub fn parse_range(header: Option<&str>, size: u64) -> RangeSpec {
    let Some(raw) = header else {
        return RangeSpec::Full;
    };
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return RangeSpec::Full;
    };
    // 多区间：只认第一段而不是拒绝整个请求 —— 少给字节客户端会再要一次，
    // 报错则会让播放直接停住
    let spec = spec.split(',').next().unwrap_or("").trim();

    let Some((start, end)) = spec.split_once('-') else {
        return RangeSpec::Full;
    };
    let (start, end) = (start.trim(), end.trim());

    // 空对象没有任何可满足的区间
    if size == 0 {
        return RangeSpec::Unsatisfiable;
    }

    match (start.is_empty(), end.is_empty()) {
        // `bytes=-N`：末尾 N 字节。N 比对象还大时给整个对象（RFC 9110 如此规定）
        (true, false) => match end.parse::<u64>() {
            Ok(0) => RangeSpec::Unsatisfiable,
            Ok(n) => RangeSpec::Partial(size.saturating_sub(n)..size),
            Err(_) => RangeSpec::Full,
        },
        // `bytes=N-`：从 N 播到底
        (false, true) => match start.parse::<u64>() {
            Ok(s) if s < size => RangeSpec::Partial(s..size),
            Ok(_) => RangeSpec::Unsatisfiable,
            Err(_) => RangeSpec::Full,
        },
        // `bytes=A-B`：HTTP 的 B 是**闭端**，Rust 的 Range 是开端，+1 在这里做。
        // 差一位错最容易发生在这一行，症状是「视频每段少最后一帧」——
        // 没人会把它归因到存储层
        (false, false) => match (start.parse::<u64>(), end.parse::<u64>()) {
            (Ok(s), Ok(e)) if s <= e && s < size => RangeSpec::Partial(s..(e + 1).min(size)),
            (Ok(s), Ok(e)) if s <= e => RangeSpec::Unsatisfiable,
            _ => RangeSpec::Full,
        },
        (true, true) => RangeSpec::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_or_unparsable_range_means_the_whole_object() {
        assert_eq!(parse_range(None, 100), RangeSpec::Full);
        // RFC 9110：读不懂的 Range 头必须当作没带，而不是回 416
        for bad in ["items=0-1", "bytes=abc-def", "bytes", "bytes=", "garbage"] {
            assert_eq!(
                parse_range(Some(bad), 100),
                RangeSpec::Full,
                "读不懂的 Range 头 {bad:?} 应当被忽略，回 416 不合规"
            );
        }
    }

    #[test]
    fn closed_end_becomes_exclusive() {
        // HTTP 的 0-99 是 100 个字节；照抄成 Rust 的 0..99 会少一个
        assert_eq!(
            parse_range(Some("bytes=0-99"), 1000),
            RangeSpec::Partial(0..100),
            "HTTP Range 右端是闭的，换算时必须 +1"
        );
        assert_eq!(
            parse_range(Some("bytes=5-5"), 1000),
            RangeSpec::Partial(5..6),
            "单字节区间 5-5 应当取到 1 个字节"
        );
    }

    #[test]
    fn open_ended_range_plays_to_the_end() {
        // 拖动进度条发的就是这个形态
        assert_eq!(
            parse_range(Some("bytes=500-"), 1000),
            RangeSpec::Partial(500..1000)
        );
        // 右端越界应当夹到对象末尾，而不是报错 —— 播放器常报一个比文件大的数
        assert_eq!(
            parse_range(Some("bytes=900-99999"), 1000),
            RangeSpec::Partial(900..1000),
            "右端越界应夹紧到对象末尾"
        );
    }

    #[test]
    fn suffix_range_takes_the_tail() {
        assert_eq!(
            parse_range(Some("bytes=-500"), 1000),
            RangeSpec::Partial(500..1000)
        );
        // 要的比对象还长时给整个对象（RFC 9110）
        assert_eq!(
            parse_range(Some("bytes=-9999"), 1000),
            RangeSpec::Partial(0..1000),
            "后缀长度超过对象大小时应当给整个对象"
        );
        assert_eq!(
            parse_range(Some("bytes=-0"), 1000),
            RangeSpec::Unsatisfiable,
            "要 0 个末尾字节是无法满足的"
        );
    }

    #[test]
    fn out_of_bounds_start_is_unsatisfiable() {
        // 起点越界必须回 416；回 200 会让播放器以为自己拿到了那一段，
        // 然后按错位的字节去解码
        assert_eq!(
            parse_range(Some("bytes=1000-"), 1000),
            RangeSpec::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some("bytes=2000-3000"), 1000),
            RangeSpec::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some("bytes=0-0"), 0),
            RangeSpec::Unsatisfiable,
            "空对象上没有任何可满足的区间"
        );
    }

    #[test]
    fn multi_range_falls_back_to_the_first_span() {
        // multipart/byteranges 不实现：只取第一段，客户端会再来要剩下的
        assert_eq!(
            parse_range(Some("bytes=0-99,200-299"), 1000),
            RangeSpec::Partial(0..100)
        );
    }

    #[test]
    fn reversed_range_is_rejected_not_silently_swapped() {
        // 首尾颠倒的区间若被悄悄交换，客户端拿到的字节和它以为的不是一回事
        assert_eq!(parse_range(Some("bytes=99-0"), 1000), RangeSpec::Full);
    }
}

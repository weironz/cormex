//! S3 兼容后端：RustFS / MinIO / Cloudflare R2 / AWS S3 通用。
//!
//! 只用标准 S3 API，不碰任何一家的私有扩展 —— RustFS 处于 alpha
//! （社区已有 disk-full 元数据损坏不可恢复的实录），换掉它的那天
//! 应该只是改一行 endpoint。

use std::{ops::Range, time::Duration};

use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::{
    Client,
    config::{BehaviorVersion, Region},
    error::{ProvideErrorMetadata, SdkError},
    operation::{get_object::GetObjectError, head_object::HeadObjectError},
    presigning::PresigningConfig,
    primitives::ByteStream,
    types::{CompletedMultipartUpload, CompletedPart},
};
use bytes::Bytes;
use url::Url;

use crate::{
    BlobError, BlobRef, BlobStore, Result, hash, hash::TenantPrefix, media, validate_range,
    validate_ttl,
};

/// 走 multipart 的体积门槛。
///
/// 取 16 MiB 的理由：multipart 至少要三次往返（create / upload / complete），
/// 单次 `PutObject` 只要一次。小对象上 multipart 是纯亏。
/// 门槛之上才值得，换来的是**按分片重试**——一段 500 MB 的视频
/// 传到 480 MB 时网络抖一下，单次 PUT 要从头再来，multipart 只重传 8 MiB。
pub const MULTIPART_THRESHOLD: usize = 16 * 1024 * 1024;

/// 分片大小。8 MiB 与 AWS CLI 的 `multipart_chunksize` 默认值一致 ——
/// 不是拍脑袋，是一个被大量生产流量验证过的数字。
///
/// 下限由 S3 规定：除最后一片外每片不得小于 5 MiB。取 8 MiB 留了余量，
/// 同时把 10000 片的上限换算成单对象 ~78 GiB，远超任何要进记忆的媒体。
pub const MULTIPART_PART_SIZE: usize = 8 * 1024 * 1024;

// S3 的硬性约束在编译期兜住 —— 调这两个常数时若破了规矩，
// 不该等到某段视频传到一半才被服务端拒绝。
const _: () = {
    assert!(
        MULTIPART_PART_SIZE >= 5 * 1024 * 1024,
        "S3 规定除末片外每片不得小于 5 MiB，小于此值 CompleteMultipartUpload 会被拒"
    );
    assert!(
        MULTIPART_THRESHOLD >= MULTIPART_PART_SIZE,
        "门槛低于分片大小会产出只有一片的 multipart —— 三次往返做一次 PUT 的事"
    );
    assert!(
        MULTIPART_PART_SIZE * 10_000 > 64 * 1024 * 1024 * 1024,
        "分片数上限是 10000；此分片大小换算出的单对象容量不足 64 GiB，长视频会传不上去"
    );
};

/// 建 [`S3BlobStore`] 所需的参数。
#[derive(Debug, Clone)]
pub struct S3Options {
    /// 形如 `http://localhost:9010`。留空表示走 AWS 官方 endpoint。
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// path-style 寻址（`host/bucket/key`）而非 virtual-host
    /// （`bucket.host/key`）。
    ///
    /// **对 RustFS / MinIO 必须为 `true`**，理由见 [`S3BlobStore::new`]。
    pub force_path_style: bool,
    /// 本实例服务的租户，会成为 object key 的首段。
    ///
    /// 与 endpoint / bucket 并列放进 `S3Options`，而不是给 [`S3BlobStore::new`]
    /// 单开一个参数：这几样东西的生命周期是一样的（一个 store 实例一份，
    /// 建完就不再变），拆成两处只会让「建 store 需要哪些东西」这件事有两个答案。
    pub tenant: TenantPrefix,
}

impl S3Options {
    /// 由 [`cortex_core::config::S3Config`] 与租户构造。默认开 path-style ——
    /// 自建对象存储是本项目的默认部署形态，而它们清一色只认 path-style。
    ///
    /// 租户是独立参数而非取自 `S3Config`：endpoint / bucket / 密钥是**整个进程
    /// 一份**的部署配置，租户却是**每用户一份**的运行期身份。塞进同一个配置
    /// 结构体，就等于暗示它也能从环境变量里读一个全局值出来——而那恰好是
    /// 多租户下最不该发生的事。
    #[must_use]
    pub fn from_config(cfg: &cortex_core::config::S3Config, tenant: TenantPrefix) -> Self {
        Self {
            endpoint: cfg.endpoint.clone(),
            bucket: cfg.bucket.clone(),
            region: cfg.region.clone(),
            access_key: cfg.access_key.clone(),
            secret_key: cfg.secret_key.clone(),
            force_path_style: true,
            tenant,
        }
    }
}

/// S3 兼容的对象存储后端。
#[derive(Clone)]
pub struct S3BlobStore {
    client: Client,
    bucket: String,
    tenant: TenantPrefix,
}

/// 手写而非 derive：`Client` 的 `Debug` 会把整份 SDK 配置
/// （全部 partition 的 endpoint 正则、拦截器链、重试策略）铺开成几十 KB。
/// 那东西一旦进了 `tracing` 的错误上下文或某条断言消息，日志就没法看了。
impl std::fmt::Debug for S3BlobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3BlobStore")
            .field("bucket", &self.bucket)
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl S3BlobStore {
    /// 建客户端。不做任何网络调用 —— 桶的存在性由
    /// [`ensure_bucket`](Self::ensure_bucket) 单独负责，
    /// 好让「构造」保持同步、无副作用、可在配置解析阶段完成。
    ///
    /// # `force_path_style` 为什么是死规定
    ///
    /// 关掉它，SDK 会把桶名拼成子域名，去请求 `cortex-blobs.localhost:9010`
    /// 而不是 `localhost:9010/cortex-blobs`。RustFS **只实现了 path-style**，
    /// 于是必然失败 —— 但失败形态取决于本机 DNS 怎么处理那个子域名，
    /// 两种都实测见过：
    ///
    /// - 子域名解析不了：`dispatch failure: io error: failed to lookup
    ///   address information`
    /// - 子域名被解析到 127.0.0.1（`*.localhost` 在不少系统上如此，
    ///   本机即是）：连接建立后 RustFS 认不得这个 Host，直接断开，
    ///   报 `connection closed before message completed`
    ///
    /// 两条错误信息里**都不含桶名、也不含「addressing style」字样**，
    /// 看上去纯粹像网络不通，足以把人往防火墙、容器网络、endpoint 拼写上
    /// 引一整天。所以不给关的机会：`from_config` 恒为 `true`。
    /// （AWS 官方 endpoint 两种寻址都支持，开着也不影响。）
    ///
    /// 效果本身由 `path_style_flag_decides_where_the_bucket_name_goes` 钉住。
    ///
    /// # Errors
    /// endpoint 不是合法 URL 时返回 [`BlobError::Backend`]。
    pub fn new(opts: &S3Options) -> Result<Self> {
        let creds = Credentials::new(
            &opts.access_key,
            &opts.secret_key,
            None,
            None,
            "cortex-static",
        );

        let mut builder = aws_sdk_s3::config::Builder::new()
            // 不锁行为版本：锁了以后每次升 SDK 都要手工对一遍变更清单，
            // 而我们只用最基础的六个 API，行为变更面几乎为零。
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(opts.region.clone()))
            .credentials_provider(creds)
            .force_path_style(opts.force_path_style);

        if !opts.endpoint.is_empty() {
            // 提前校验一次：endpoint 拼错若留到第一次请求才炸，
            // 错误会混在网络失败里难以分辨。
            //
            // 光靠 `Url::parse` 成功是不够的 —— 它会把 `localhost:9010`
            // 解析成「scheme = localhost，path = 9010」并欣然返回 Ok，
            // 于是最常见的那个错（漏写 http://）恰恰是它漏掉的一个。
            // 所以显式要求 http/https 且带 host。
            let url = Url::parse(&opts.endpoint).map_err(|e| {
                BlobError::Backend(format!("S3 endpoint {:?} 非法：{e}", opts.endpoint))
            })?;
            if !matches!(url.scheme(), "http" | "https") || !url.has_host() {
                return Err(BlobError::Backend(format!(
                    "S3 endpoint {:?} 缺少 http:// 或 https:// 前缀",
                    opts.endpoint
                )));
            }
            builder = builder.endpoint_url(&opts.endpoint);
        }

        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket: opts.bucket.clone(),
            tenant: opts.tenant.clone(),
        })
    }

    #[must_use]
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// 本实例服务的租户。对象都落在 `<tenant>/` 这一前缀之下。
    #[must_use]
    pub fn tenant(&self) -> &TenantPrefix {
        &self.tenant
    }

    fn key_of(&self, hash: &str) -> Result<String> {
        hash::storage_key(&self.tenant, hash)
    }

    /// 桶不存在则创建。幂等，可在每次启动时无条件调用。
    ///
    /// 不用 `HeadBucket` 先探再建：那是 TOCTOU，两个实例同时启动会撞。
    /// 直接建，然后把「已存在」这两个错误码当成功 —— 这才是幂等的正确写法。
    ///
    /// # Errors
    /// 建桶失败且并非「已存在」时返回 [`BlobError::Backend`]。
    pub async fn ensure_bucket(&self) -> Result<()> {
        match self
            .client
            .create_bucket()
            .bucket(&self.bucket)
            .send()
            .await
        {
            Ok(_) => {
                tracing::info!(bucket = %self.bucket, "已创建对象存储桶");
                Ok(())
            }
            Err(e) => {
                let code = e.code().unwrap_or_default();
                if matches!(code, "BucketAlreadyOwnedByYou" | "BucketAlreadyExists") {
                    Ok(())
                } else {
                    Err(backend_err("CreateBucket", e))
                }
            }
        }
    }

    fn presign_cfg(ttl: Duration) -> Result<PresigningConfig> {
        validate_ttl(ttl)?;
        PresigningConfig::expires_in(ttl)
            .map_err(|e| BlobError::Backend(format!("构造 presigning 配置失败：{e}")))
    }

    /// 大对象走 multipart。失败时**必须** abort，否则已上传的分片会
    /// 永久占着桶的空间且不在 `ListObjects` 里可见 —— 是最难发现的那种泄漏。
    async fn put_multipart(&self, key: &str, bytes: &Bytes, mime: &str) -> Result<()> {
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .content_type(mime)
            .send()
            .await
            .map_err(|e| backend_err("CreateMultipartUpload", e))?;

        let upload_id = created
            .upload_id()
            .ok_or_else(|| BlobError::Backend("CreateMultipartUpload 未返回 upload_id".into()))?;

        match self.upload_parts(key, upload_id, bytes).await {
            Ok(parts) => {
                let completed = CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build();
                self.client
                    .complete_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .multipart_upload(completed)
                    .send()
                    .await
                    .map_err(|e| backend_err("CompleteMultipartUpload", e))?;
                Ok(())
            }
            Err(e) => {
                // abort 本身也可能失败（网络已断），但那时再报它只会盖住真正的病因。
                // 记日志而不是替换错误。
                if let Err(abort_err) = self
                    .client
                    .abort_multipart_upload()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .send()
                    .await
                {
                    tracing::warn!(
                        key, %abort_err,
                        "multipart 失败后 abort 也失败，桶内可能残留分片，需靠生命周期规则回收"
                    );
                }
                Err(e)
            }
        }
    }

    async fn upload_parts(
        &self,
        key: &str,
        upload_id: &str,
        bytes: &Bytes,
    ) -> Result<Vec<CompletedPart>> {
        let mut parts = Vec::with_capacity(bytes.len().div_ceil(MULTIPART_PART_SIZE));

        // 顺序上传而非并发：字节已经全在内存里，瓶颈是上行带宽，
        // 并发分片并不会让它变宽，只会让内存里同时多出几份切片的引用。
        // 真要提速应该先把 put 改成流式接口，那是另一件事。
        for (i, chunk) in bytes.chunks(MULTIPART_PART_SIZE).enumerate() {
            // S3 的分片编号从 1 开始，不是 0
            let part_number =
                i32::try_from(i + 1).map_err(|_| BlobError::Backend("分片数超出 i32".into()))?;

            let out = self
                .client
                .upload_part()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(part_number)
                .body(ByteStream::from(Bytes::copy_from_slice(chunk)))
                .send()
                .await
                .map_err(|e| backend_err("UploadPart", e))?;

            parts.push(
                CompletedPart::builder()
                    .part_number(part_number)
                    .set_e_tag(out.e_tag().map(str::to_owned))
                    .build(),
            );
        }
        Ok(parts)
    }
}

#[async_trait]
impl BlobStore for S3BlobStore {
    async fn put(&self, bytes: Bytes, declared_mime: Option<&str>) -> Result<BlobRef> {
        let digest = hash::sha256_hex(&bytes);
        let storage_key = self.key_of(&digest)?;

        let meta = media::probe(&bytes);
        let mime = meta.resolve_mime(declared_mime);
        let blob = BlobRef {
            mime: mime.clone(),
            size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            hash: digest,
            storage_key: storage_key.clone(),
            deduplicated: true,
        };

        // 先探后传：命中就省掉整段上行带宽。
        //
        // 这里有一个众所周知的竞态 —— 两个请求同时探到「不存在」，
        // 于是都传一遍。**故意不管**：内容寻址下二者的 key 与字节完全相同，
        // S3 的 PutObject 又是原子的，谁后到结果都一样。
        // 为省一次重复上传去引入分布式锁，是拿可用性换带宽，赔本。
        //
        // 唯一的实际影响是 `deduplicated` 这个观测字段偶尔偏低，
        // 而它本就注明了只作观测用。
        if self.exists(&blob.hash).await? {
            return Ok(blob);
        }

        if bytes.len() >= MULTIPART_THRESHOLD {
            self.put_multipart(&storage_key, &bytes, &mime).await?;
        } else {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .key(&storage_key)
                .content_type(&mime)
                .body(ByteStream::from(bytes))
                .send()
                .await
                .map_err(|e| backend_err("PutObject", e))?;
        }

        Ok(BlobRef {
            deduplicated: false,
            ..blob
        })
    }

    async fn get(&self, hash: &str) -> Result<Bytes> {
        let key = self.key_of(hash)?;
        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| map_get_err(hash, e))?;

        collect(out.body).await
    }

    async fn get_range(&self, hash: &str, range: Range<u64>) -> Result<Bytes> {
        validate_range(&range)?;
        let key = self.key_of(hash)?;

        // HTTP Range 的两端都是闭的，Rust 的 Range 是左闭右开 —— 差一位错在这里
        // 最容易发生，而症状是「视频每段少最后一帧」这种没人会归因到存储层的问题。
        let header = format!("bytes={}-{}", range.start, range.end - 1);

        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .range(header)
            .send()
            .await
            .map_err(|e| map_get_err(hash, e))?;

        collect(out.body).await
    }

    async fn presign_put(&self, hash: &str, ttl: Duration) -> Result<Url> {
        let key = self.key_of(hash)?;
        let cfg = Self::presign_cfg(ttl)?;

        // 刻意不设 content_type / content_length：presign 会把请求头一并签进
        // SignedHeaders，客户端届时发的头只要有一个字节对不上，签名就验不过
        // （报 SignatureDoesNotMatch，而真因是「你多设了一个头」）。
        // MIME 由服务端在写 `blobs` 行时定，本就不必让客户端参与。
        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(&key)
            .presigned(cfg)
            .await
            .map_err(|e| backend_err("presign PutObject", e))?;

        parse_presigned(req.uri())
    }

    async fn presign_get(&self, hash: &str, ttl: Duration) -> Result<Url> {
        let key = self.key_of(hash)?;
        let cfg = Self::presign_cfg(ttl)?;

        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(&key)
            .presigned(cfg)
            .await
            .map_err(|e| backend_err("presign GetObject", e))?;

        parse_presigned(req.uri())
    }

    async fn exists(&self, hash: &str) -> Result<bool> {
        let key = self.key_of(hash)?;
        match self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(SdkError::ServiceError(e)) if matches!(e.err(), HeadObjectError::NotFound(_)) => {
                Ok(false)
            }
            // 桶还没建出来时 HeadObject 报 NoSuchBucket，语义上等同于「对象不存在」。
            // 不特判会让首次启动的 put 直接失败在探测这一步。
            Err(e) if is_no_such_bucket(&e) => Ok(false),
            Err(e) => Err(backend_err("HeadObject", e)),
        }
    }

    async fn delete(&self, hash: &str) -> Result<()> {
        let key = self.key_of(hash)?;
        // S3 的 DeleteObject 对不存在的 key 也返回 204，天然幂等 ——
        // 正合 purge 级联清除任务「可重跑」的要求。
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| backend_err("DeleteObject", e))?;
        Ok(())
    }
}

/// 把 SDK 错误压成一行可读的字符串。
///
/// 直接 `{e}` 只会得到 "service error" 这种毫无信息量的顶层描述 ——
/// 真正的原因埋在 source 链的第二三层。这里显式取 `code`/`message`，
/// 并在连不上（根本没有服务端响应）时一路下钻到最深一层，
/// 省下的是每次排查时手工展开错误链的功夫。
fn backend_err<E>(op: &str, e: SdkError<E>) -> BlobError
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    let code = e.code().unwrap_or("-");
    let msg = e.message().map(str::to_owned).unwrap_or_else(|| {
        let mut src: Option<&(dyn std::error::Error + 'static)> = std::error::Error::source(&e);
        let mut deepest = e.to_string();
        while let Some(s) = src {
            deepest = s.to_string();
            src = s.source();
        }
        deepest
    });
    BlobError::Backend(format!("{op} 失败[{code}]：{msg}"))
}

fn map_get_err(hash: &str, e: SdkError<GetObjectError>) -> BlobError {
    match http_status(&e) {
        // 416：range 落在对象之外。归到 InvalidRange 而不是 Backend，
        // 才能在 HTTP 层映射成 400 而非 500。
        Some(416) => BlobError::InvalidRange(format!("请求的区间完全落在 {hash} 之外")),
        // 走状态码而不是只认 `GetObjectError::NoSuchKey`：RustFS 等兼容实现
        // 对缺失对象回 404 却不一定带上那个结构体，只认类型化错误会把 404 错报成 500。
        Some(404) => BlobError::NotFound(hash.to_owned()),
        _ if matches!(&e, SdkError::ServiceError(se) if matches!(se.err(), GetObjectError::NoSuchKey(_))) => {
            BlobError::NotFound(hash.to_owned())
        }
        _ => backend_err("GetObject", e),
    }
}

/// 取服务端返回的 HTTP 状态码。连不上时无响应可取，返回 `None`。
fn http_status<E>(e: &SdkError<E>) -> Option<u16> {
    match e {
        SdkError::ServiceError(se) => Some(se.raw().status().as_u16()),
        SdkError::ResponseError(re) => Some(re.raw().status().as_u16()),
        _ => None,
    }
}

fn is_no_such_bucket<E: ProvideErrorMetadata>(e: &SdkError<E>) -> bool {
    e.code() == Some("NoSuchBucket")
}

fn parse_presigned(uri: &str) -> Result<Url> {
    Url::parse(uri).map_err(|e| BlobError::Backend(format!("presigned URL {uri:?} 不可解析：{e}")))
}

async fn collect(body: ByteStream) -> Result<Bytes> {
    body.collect()
        .await
        .map(aws_smithy_types::byte_stream::AggregatedBytes::into_bytes)
        .map_err(|e| BlobError::Backend(format!("读取对象内容失败：{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_TENANT: &str = "public";

    fn opts() -> S3Options {
        S3Options {
            endpoint: "http://localhost:9010".into(),
            bucket: "cortex-blobs".into(),
            region: "us-east-1".into(),
            access_key: "k".into(),
            secret_key: "s".into(),
            force_path_style: true,
            tenant: TenantPrefix::new(TEST_TENANT).expect("测试租户前缀本身应合法"),
        }
    }

    #[test]
    fn construction_makes_no_network_call() {
        // 指向一个必然不通的地址：若构造里藏了网络调用，这里会挂住或报错
        let mut o = opts();
        o.endpoint = "http://127.0.0.1:1".into();
        assert!(
            S3BlobStore::new(&o).is_ok(),
            "构造不该访问网络 —— 它要能在配置解析阶段完成"
        );
    }

    #[test]
    fn malformed_endpoint_fails_fast() {
        // "localhost:9010" 是最常见的那个错（漏写 http://），而它恰恰能被
        // Url::parse 解析成功（scheme = localhost）。只靠 parse 会放它过去。
        for bad in ["localhost:9010", "9010", "ftp://localhost:9010", "http://"] {
            let mut o = opts();
            o.endpoint = bad.into();
            assert!(
                matches!(S3BlobStore::new(&o), Err(BlobError::Backend(_))),
                "endpoint {bad:?} 应在构造期被拒 —— 留到首次请求才炸会混进网络失败里难以分辨"
            );
        }
    }

    #[test]
    fn empty_endpoint_means_aws() {
        let mut o = opts();
        o.endpoint = String::new();
        assert!(
            S3BlobStore::new(&o).is_ok(),
            "空 endpoint 表示走 AWS 官方地址，不该被当成非法输入"
        );
    }

    #[tokio::test]
    async fn traversal_hash_never_reaches_the_network() {
        // endpoint 指向必然不通的端口：只要请求真的发出去就会是 Backend 错误。
        // 断言拿到的是 InvalidHash，即证明校验发生在任何网络动作之前。
        let mut o = opts();
        o.endpoint = "http://127.0.0.1:1".into();
        let s = S3BlobStore::new(&o).unwrap();
        let ttl = Duration::from_secs(60);

        for evil in ["../../../etc/passwd", "..", "a/../b", "NOTAHASH"] {
            assert!(
                matches!(s.get(evil).await, Err(BlobError::InvalidHash(_))),
                "get 的哈希校验没有先于网络调用：{evil:?}"
            );
            assert!(
                matches!(s.exists(evil).await, Err(BlobError::InvalidHash(_))),
                "exists 的哈希校验没有先于网络调用：{evil:?}"
            );
            assert!(
                matches!(s.delete(evil).await, Err(BlobError::InvalidHash(_))),
                "delete 的哈希校验没有先于网络调用：{evil:?}"
            );
            assert!(
                matches!(
                    s.presign_get(evil, ttl).await,
                    Err(BlobError::InvalidHash(_))
                ),
                "presign_get 会把 key 直接签进 URL，校验必须在最前面：{evil:?}"
            );
            assert!(
                matches!(
                    s.presign_put(evil, ttl).await,
                    Err(BlobError::InvalidHash(_))
                ),
                "presign_put 会把 key 直接签进 URL，校验必须在最前面：{evil:?}"
            );
        }
    }

    /// `force_path_style` 的效果是什么 —— 签名不走网络，可以在单测里直接看。
    ///
    /// 这条测试的价值在于：把「关掉它会怎样」变成一行可读的断言。
    /// 否则那个后果（DNS 解析 `cortex-blobs.localhost`）只有在真跑起来
    /// 并读懂一条不含桶名的 io error 时才会显形。
    #[tokio::test]
    async fn path_style_flag_decides_where_the_bucket_name_goes() {
        let h = hash::sha256_hex(b"probe");
        let ttl = Duration::from_secs(60);

        let on = S3BlobStore::new(&opts()).unwrap();
        let url = on.presign_get(&h, ttl).await.unwrap();
        assert_eq!(
            url.host_str(),
            Some("localhost"),
            "开启 path-style 后 host 应保持为 endpoint 原样"
        );
        assert!(
            url.path().starts_with("/cortex-blobs/public/blobs/"),
            "开启 path-style 后路径应是「桶名 / 租户 / blobs」，实际是 {}",
            url.path()
        );

        let mut o = opts();
        o.force_path_style = false;
        let off = S3BlobStore::new(&o).unwrap();
        let url = off.presign_get(&h, ttl).await.unwrap();
        assert_eq!(
            url.host_str(),
            Some("cortex-blobs.localhost"),
            "关掉 path-style 后桶名会被拼成子域名 —— 这正是 RustFS 上 DNS 解析失败的成因，\
             报出来的错误里既没有桶名也没有 addressing style 字样，极难归因"
        );
    }

    /// 租户前缀必须真的进到**签名后的 URL** 里。
    ///
    /// 单独验这一处，是因为 presign 是唯一一条不经 `put`/`get` 就能把 key 送出去的
    /// 路径：若哪天它绕开了 `key_of`，客户端就会拿着一个不带租户的 URL 直传，
    /// 而那份字节会落在别人的前缀下 —— 服务端这边全程无错可报。
    #[tokio::test]
    async fn tenant_prefix_reaches_the_signed_url() {
        let h = hash::sha256_hex(b"tenant in url");
        let ttl = Duration::from_secs(60);

        let mut other = opts();
        other.tenant = TenantPrefix::new("cortex_u2").unwrap();

        let a = S3BlobStore::new(&opts()).unwrap();
        let b = S3BlobStore::new(&other).unwrap();

        let ua = a.presign_get(&h, ttl).await.unwrap();
        let ub = b.presign_get(&h, ttl).await.unwrap();

        assert!(
            ua.path().contains("/public/blobs/"),
            "签出的 URL 里没有租户前缀，客户端直传会落到无租户的 key 上：{}",
            ua.path()
        );
        assert_ne!(
            ua.path(),
            ub.path(),
            "两个租户对同一份内容签出了同一个 key —— 直传会互相覆盖"
        );
    }

    #[test]
    fn part_boundaries_cover_the_last_partial_chunk() {
        // multipart 最易错的一处是「末片被漏掉」：分片数按整除算就会丢掉尾巴，
        // 而症状是取回的视频比原文件短一点点 —— 播放器多半还能放，无人察觉。
        for (size, want_parts) in [
            (MULTIPART_PART_SIZE, 1),
            (MULTIPART_PART_SIZE + 1, 2),
            (MULTIPART_THRESHOLD, 2),
            (MULTIPART_PART_SIZE * 3 - 1, 3),
        ] {
            assert_eq!(
                size.div_ceil(MULTIPART_PART_SIZE),
                want_parts,
                "{size} 字节应切成 {want_parts} 片；按整除算会丢掉不足一片的尾巴"
            );
        }
    }
}

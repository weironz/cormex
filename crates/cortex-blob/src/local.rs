//! 本地文件系统后端。
//!
//! 存在的两个理由：
//!
//! 1. **测试**：单测里跑真实的读写往返，不需要起容器
//! 2. **让「第一版可以不起 RustFS」成立**：单机自用时，
//!    多一个 alpha 阶段的对象存储进程就是多一个故障源
//!
//! 与 [`S3BlobStore`](crate::S3BlobStore) 共用 [`hash::storage_key`] 的布局，
//! 所以目录结构与桶内 key 一一对应 —— 将来迁移直接 `aws s3 sync` 即可，
//! `blobs.storage_key` 那一列一个字都不用改。

use std::{
    io::ErrorKind,
    ops::Range,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use url::Url;

use crate::{
    BlobError, BlobRef, BlobStore, Result, hash, hash::TenantPrefix, media, validate_range,
    validate_ttl,
};

/// 按哈希前缀分目录的本地存储。一个实例服务一个租户。
#[derive(Debug, Clone)]
pub struct LocalFsBlobStore {
    root: PathBuf,
    tenant: TenantPrefix,
}

impl LocalFsBlobStore {
    /// 以 `root` 为根、`tenant` 为租户建库。目录不存在则创建。
    ///
    /// # 为什么租户绑在构造期，而不是每个方法多带一个参数
    ///
    /// 「传错租户」在运行期是彻底静默的：写进去的对象取不回来，取回来的是
    /// 别人的，两种都不报错。而这个 store 的实际调用方是 cortexd 的**每用户
    /// 实例注册表**——租户在那里查一次就定下来了，之后每一次读写都用同一个。
    /// 把它做成构造参数，等于把「每次调用都可能传错」压缩成「创建实例时错一次」，
    /// 而那一处恰好是唯一能拿到用户身份、也唯一有条件校验的地方。
    ///
    /// 参数类型是 [`TenantPrefix`] 而非 `&str`：校验因此发生在**更外面**
    /// （构造 `TenantPrefix` 那一刻），这里拿到的已是「被证明安全」的值。
    ///
    /// # Errors
    /// 根目录建不出来时返回 [`BlobError::Io`]。
    pub async fn new(root: impl Into<PathBuf>, tenant: TenantPrefix) -> Result<Self> {
        let root = root.into();
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self { root, tenant })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 本实例服务的租户。对象都落在 `root/<tenant>/` 之下。
    #[must_use]
    pub fn tenant(&self) -> &TenantPrefix {
        &self.tenant
    }

    fn key_of(&self, hash: &str) -> Result<String> {
        hash::storage_key(&self.tenant, hash)
    }

    /// 哈希 → 绝对路径。
    ///
    /// 安全性全靠 [`hash::storage_key`] 那一侧的两道校验：租户前缀在构造
    /// [`TenantPrefix`] 时已被限定为 Postgres 标识符字符集，哈希在此处逐次校验，
    /// 于是 key 里只可能有小写字母数字下划线与我们自己插的 `/`，
    /// `root.join(key)` 绝无可能逃出 root。
    fn path_of(&self, hash: &str) -> Result<PathBuf> {
        Ok(self.root.join(self.key_of(hash)?))
    }
}

#[async_trait]
impl BlobStore for LocalFsBlobStore {
    async fn put(&self, bytes: Bytes, declared_mime: Option<&str>) -> Result<BlobRef> {
        let digest = hash::sha256_hex(&bytes);
        let storage_key = self.key_of(&digest)?;
        let path = self.root.join(&storage_key);

        let meta = media::probe(&bytes);
        let blob = BlobRef {
            mime: meta.resolve_mime(declared_mime),
            size_bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
            hash: digest,
            storage_key,
            deduplicated: true,
        };

        if tokio::fs::metadata(&path).await.is_ok() {
            return Ok(blob);
        }

        let dir = path.parent().expect("storage_key 保证了至少两级前缀目录");
        tokio::fs::create_dir_all(dir).await?;

        // 先写临时文件再 rename：rename 在同一文件系统内是原子的，
        // 于是「路径存在」与「内容完整」永远同时成立。
        //
        // 直接 create + write 会开一个窗口，窗口里另一个读者看到的是半截文件 ——
        // 而内容寻址下这半截文件的 key 仍然宣称自己是完整内容的哈希，
        // 是最恶劣的那种坏数据：自证清白的坏数据。
        //
        // 临时文件名带进程 id 与内容哈希，保证两个并发写者不会写进同一个临时文件。
        let tmp = dir.join(format!(".tmp-{}-{}", std::process::id(), &blob.hash[..16]));
        {
            let mut f = tokio::fs::File::create(&tmp).await?;
            f.write_all(&bytes).await?;
            // 不 fsync：这是本地开发/测试后端，真正的持久性承诺由 RustFS
            // 的 durability_mode=strict 提供。在此每写一个 blob 都 fsync
            // 会让测试慢一个数量级，换来的耐久性无人消费。
            f.flush().await?;
        }

        match tokio::fs::rename(&tmp, &path).await {
            Ok(()) => {}
            Err(e) => {
                // 竞态：另一个写者刚把同样的内容 rename 到位。
                // 内容寻址下它写的字节与我们的逐字节相同，直接认账即可。
                let _ = tokio::fs::remove_file(&tmp).await;
                if tokio::fs::metadata(&path).await.is_err() {
                    return Err(BlobError::Io(e));
                }
            }
        }

        Ok(BlobRef {
            deduplicated: false,
            ..blob
        })
    }

    async fn get(&self, hash: &str) -> Result<Bytes> {
        let path = self.path_of(hash)?;
        match tokio::fs::read(&path).await {
            Ok(v) => Ok(Bytes::from(v)),
            Err(e) if e.kind() == ErrorKind::NotFound => Err(BlobError::NotFound(hash.to_owned())),
            Err(e) => Err(BlobError::Io(e)),
        }
    }

    async fn get_range(&self, hash: &str, range: Range<u64>) -> Result<Bytes> {
        validate_range(&range)?;
        let path = self.path_of(hash)?;

        let mut f = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(BlobError::NotFound(hash.to_owned()));
            }
            Err(e) => return Err(BlobError::Io(e)),
        };

        f.seek(std::io::SeekFrom::Start(range.start)).await?;

        // 起点越界时 read 会直接读到 0 字节，与「文件恰好在此结束」无法区分，
        // 但结果都是空 —— 交给下面的空判统一处理。
        let want = usize::try_from(range.end - range.start).unwrap_or(usize::MAX);
        let mut buf = Vec::with_capacity(want.min(1 << 20));
        f.take(range.end - range.start)
            .read_to_end(&mut buf)
            .await?;

        if buf.is_empty() {
            return Err(BlobError::InvalidRange(format!(
                "区间 [{}, {}) 完全落在 {hash} 之外",
                range.start, range.end
            )));
        }
        Ok(Bytes::from(buf))
    }

    /// 本地后端**签不出** presigned URL：没有签名密钥，也没有一个能被
    /// 浏览器直接 PUT 的 HTTP 端点。
    ///
    /// 返回 [`BlobError::Unsupported`] 而不是伪造一个 `file://` ——
    /// 后者在桌面端会「看起来能用」（同机同用户），一上移动端或 Web 就崩，
    /// 而那时错误已经离现场很远了。调用方拿到 `Unsupported` 应显式降级
    /// 为经服务端中转。
    async fn presign_put(&self, hash: &str, ttl: Duration) -> Result<Url> {
        hash::validate_hash(hash)?;
        validate_ttl(ttl)?;
        Err(BlobError::Unsupported(
            "LocalFsBlobStore 无法签发 presigned URL，请改走服务端中转或换用 S3BlobStore",
        ))
    }

    async fn presign_get(&self, hash: &str, ttl: Duration) -> Result<Url> {
        hash::validate_hash(hash)?;
        validate_ttl(ttl)?;
        Err(BlobError::Unsupported(
            "LocalFsBlobStore 无法签发 presigned URL，请改走服务端中转或换用 S3BlobStore",
        ))
    }

    async fn exists(&self, hash: &str) -> Result<bool> {
        Ok(tokio::fs::metadata(self.path_of(hash)?).await.is_ok())
    }

    async fn delete(&self, hash: &str) -> Result<()> {
        match tokio::fs::remove_file(self.path_of(hash)?).await {
            Ok(()) => Ok(()),
            // 幂等：purge 的级联清除任务必须可重跑
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(BlobError::Io(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试统一用 `public` —— roadmap R9 定的 1 号用户 schema 名就是它。
    const TEST_TENANT: &str = "public";

    fn tenant(s: &str) -> TenantPrefix {
        TenantPrefix::new(s).unwrap_or_else(|e| panic!("{s:?} 本该是合法租户前缀，却被拒了：{e}"))
    }

    async fn store() -> (LocalFsBlobStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let s = LocalFsBlobStore::new(dir.path(), tenant(TEST_TENANT))
            .await
            .expect("建 LocalFsBlobStore 失败");
        (s, dir)
    }

    #[tokio::test]
    async fn round_trip_preserves_bytes() {
        let (s, _d) = store().await;
        let payload = Bytes::from_static(b"cortex blob round trip \xE4\xB8\xAD\xE6\x96\x87");

        let r = s.put(payload.clone(), None).await.expect("写入失败");
        assert_eq!(
            r.hash,
            hash::sha256_hex(&payload),
            "返回的哈希与内容不符，内容寻址就断了"
        );
        assert_eq!(
            r.size_bytes,
            payload.len() as i64,
            "size_bytes 与实际字节数不符"
        );
        assert!(!r.deduplicated, "首次写入不该被标记为去重命中");

        let got = s.get(&r.hash).await.expect("按哈希取回失败");
        assert_eq!(got, payload, "取回的字节与写入的不一致");
    }

    #[tokio::test]
    async fn objects_land_under_the_shared_key_layout() {
        // 目录结构必须与 S3 桶内 key 逐段对应，将来 `aws s3 sync` 才能直接迁移，
        // `blobs.storage_key` 那一列也才不用回写
        let (s, dir) = store().await;
        let r = s.put(Bytes::from_static(b"layout"), None).await.unwrap();

        assert_eq!(s.root(), dir.path(), "root() 应返回建库时给的目录");
        let on_disk = dir.path().join(&r.storage_key);
        assert!(
            tokio::fs::metadata(&on_disk).await.is_ok(),
            "文件应恰好落在 root/storage_key 上，实际找不到：{}",
            on_disk.display()
        );
        assert!(
            r.storage_key.starts_with(&format!(
                "{TEST_TENANT}/blobs/{}/{}/",
                &r.hash[0..2],
                &r.hash[2..4]
            )),
            "storage_key 应是「租户 / blobs / 哈希两级前缀」，实际是 {}",
            r.storage_key
        );
    }

    #[tokio::test]
    async fn two_tenants_sharing_a_root_stay_separate() {
        // 同一台机器上两个用户的 store 会共用一个 root。跨租户去重已被刻意放弃，
        // 所以同一份字节必须在磁盘上留下两份、互不可见 —— 否则 A 的 purge
        // 会删掉 B 还在引用的对象，而 A 的 schema 里根本看不到 B 的引用。
        let dir = tempfile::tempdir().expect("建临时目录失败");
        let a = LocalFsBlobStore::new(dir.path(), tenant("public"))
            .await
            .unwrap();
        let b = LocalFsBlobStore::new(dir.path(), tenant("cortex_u2"))
            .await
            .unwrap();
        let payload = Bytes::from_static(b"one file, two owners");

        let ra = a.put(payload.clone(), None).await.expect("A 写入失败");
        let rb = b.put(payload.clone(), None).await.expect("B 写入失败");

        assert_eq!(ra.hash, rb.hash, "内容寻址下哈希本身仍应与租户无关");
        assert_ne!(
            ra.storage_key, rb.storage_key,
            "两个租户的 storage_key 必须不同，否则 A 的 purge 会删掉 B 的字节"
        );
        assert!(
            !rb.deduplicated,
            "B 的写入不该命中 A 已经写下的对象 —— 跨租户去重正是这次要去掉的东西"
        );

        // 互不可见这一半更要紧：A 删干净之后 B 必须完好
        a.delete(&ra.hash).await.expect("A 删除失败");
        assert!(
            !a.exists(&ra.hash).await.unwrap(),
            "A 删除后自己这份不该还在"
        );
        assert_eq!(
            b.get(&rb.hash).await.expect("A 的删除波及了 B 的对象"),
            payload,
            "A 的 purge 不得影响 B 的字节"
        );
    }

    #[tokio::test]
    async fn blob_ref_survives_a_json_round_trip() {
        // cortexd 要把它作为 HTTP 响应体发出去，字段名与类型是对外契约
        let (s, _d) = store().await;
        let r = s.put(Bytes::from_static(b"serde"), None).await.unwrap();

        let json = serde_json::to_string(&r).expect("BlobRef 序列化失败");
        let back: BlobRef = serde_json::from_str(&json).expect("BlobRef 反序列化失败");
        assert_eq!(back, r, "JSON 往返后 BlobRef 发生了变化：{json}");
    }

    #[tokio::test]
    async fn second_put_is_deduplicated() {
        let (s, _d) = store().await;
        let payload = Bytes::from_static(b"same content twice");

        let a = s.put(payload.clone(), None).await.unwrap();
        let b = s.put(payload.clone(), None).await.unwrap();

        assert_eq!(a.hash, b.hash, "同内容两次写入必须得到同一个 key");
        assert_eq!(a.storage_key, b.storage_key, "storage_key 也必须一致");
        assert!(!a.deduplicated, "第一次写入不是去重命中");
        assert!(
            b.deduplicated,
            "第二次写入应命中已有对象，不该真的再写一遍磁盘"
        );
    }

    #[tokio::test]
    async fn concurrent_put_of_same_content_stays_intact() {
        let (s, _d) = store().await;
        // 够大才能让「写一半被看见」有机会发生 —— 小文件一次 write 就完了
        let payload = Bytes::from(vec![0x5Au8; 3 << 20]);
        let expected = hash::sha256_hex(&payload);

        let s = std::sync::Arc::new(s);
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let s = s.clone();
            let p = payload.clone();
            tasks.push(tokio::spawn(async move { s.put(p, None).await }));
        }
        for t in tasks {
            let r = t.await.expect("任务 panic").expect("并发写入报错");
            assert_eq!(r.hash, expected, "并发写入返回了不同的哈希");
        }

        let got = s.get(&expected).await.expect("并发写入后取不回");
        assert_eq!(
            hash::sha256_hex(&got),
            expected,
            "并发写入产生了半截文件 —— 说明 rename 的原子性没兜住"
        );
    }

    #[tokio::test]
    async fn range_read_returns_the_slice() {
        let (s, _d) = store().await;
        let payload = Bytes::from_static(b"0123456789abcdef");
        let r = s.put(payload.clone(), None).await.unwrap();

        let mid = s.get_range(&r.hash, 4..10).await.expect("range 读失败");
        assert_eq!(&mid[..], b"456789", "range 是左闭右开，取到的切片不对");

        let tail = s
            .get_range(&r.hash, 14..999)
            .await
            .expect("越界的右端应被截断而非报错");
        assert_eq!(&tail[..], b"ef", "右端越界时应返回到文件末尾为止的字节");

        assert!(s.get_range(&r.hash, 5..5).await.is_err(), "空区间必须被拒");
        assert!(
            s.get_range(&r.hash, 99..120).await.is_err(),
            "起点完全越界时应报错，而不是悄悄返回空内容"
        );
    }

    #[tokio::test]
    async fn missing_blob_is_not_found_not_io_error() {
        let (s, _d) = store().await;
        let absent = hash::sha256_hex(b"never written");

        assert!(!s.exists(&absent).await.unwrap(), "没写过的内容不该存在");
        let e = s.get(&absent).await.expect_err("取不存在的对象应报错");
        assert!(
            matches!(e, BlobError::NotFound(_)),
            "缺失应归类为 NotFound（HTTP 404），错报成 IO 错误会变成 500：{e:?}"
        );
    }

    #[tokio::test]
    async fn delete_is_idempotent() {
        let (s, _d) = store().await;
        let r = s
            .put(Bytes::from_static(b"to be purged"), None)
            .await
            .unwrap();

        s.delete(&r.hash).await.expect("首次删除失败");
        assert!(!s.exists(&r.hash).await.unwrap(), "删除后不该还存在");
        s.delete(&r.hash)
            .await
            .expect("重复删除必须成功 —— purge 的级联清除任务要求可重跑");
    }

    #[tokio::test]
    async fn traversal_hash_never_touches_the_filesystem() {
        let (s, dir) = store().await;
        // 目标文件在 root 之外。若校验失守，下面任何一个调用都会碰到它。
        let outside = dir.path().parent().unwrap().join("cortex-traversal-canary");
        tokio::fs::write(&outside, b"secret").await.unwrap();

        for evil in [
            "../cortex-traversal-canary",
            "../../etc/passwd",
            "..",
            "a/../b",
        ] {
            assert!(
                matches!(s.get(evil).await, Err(BlobError::InvalidHash(_))),
                "get 未拦下路径遍历载荷 {evil:?}，这是任意文件读"
            );
            assert!(
                matches!(s.exists(evil).await, Err(BlobError::InvalidHash(_))),
                "exists 未拦下路径遍历载荷 {evil:?}"
            );
            assert!(
                matches!(s.delete(evil).await, Err(BlobError::InvalidHash(_))),
                "delete 未拦下路径遍历载荷 {evil:?}，这是任意文件删除"
            );
            assert!(
                matches!(
                    s.get_range(evil, 0..4).await,
                    Err(BlobError::InvalidHash(_))
                ),
                "get_range 未拦下路径遍历载荷 {evil:?}"
            );
        }

        assert!(
            tokio::fs::metadata(&outside).await.is_ok(),
            "root 之外的文件被删掉了 —— 路径遍历防线已被击穿"
        );
        let _ = tokio::fs::remove_file(&outside).await;
    }

    #[tokio::test]
    async fn presign_is_explicitly_unsupported() {
        let (s, _d) = store().await;
        let h = hash::sha256_hex(b"x");
        let ttl = Duration::from_secs(60);

        assert!(
            matches!(s.presign_put(&h, ttl).await, Err(BlobError::Unsupported(_))),
            "本地后端应明确报 Unsupported 供调用方降级，而不是伪造 file:// URL"
        );
        assert!(matches!(
            s.presign_get(&h, ttl).await,
            Err(BlobError::Unsupported(_))
        ));
        // 非法入参仍要先于「不支持」被拒，否则调用方永远发现不了自己传错了
        assert!(matches!(
            s.presign_get("nope", ttl).await,
            Err(BlobError::InvalidHash(_))
        ));
    }

    #[tokio::test]
    async fn mime_is_sniffed_not_trusted() {
        let (s, _d) = store().await;
        let png = Bytes::from_static(&[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89,
        ]);

        let r = s.put(png, Some("application/octet-stream")).await.unwrap();
        assert_eq!(
            r.mime, "image/png",
            "字节头认出是 PNG 时必须覆盖客户端的 octet-stream 声明，否则这份内容会成为检索黑洞"
        );

        let r = s
            .put(Bytes::from_static(b"# hi"), Some("text/markdown"))
            .await
            .unwrap();
        assert_eq!(r.mime, "text/markdown", "认不出类型时应回落到客户端声明");

        let r = s.put(Bytes::from_static(b"?!"), None).await.unwrap();
        assert_eq!(
            r.mime,
            media::DEFAULT_MIME,
            "两者都没有时须给出可写进 blobs.mime NOT NULL 的兜底值"
        );
    }
}

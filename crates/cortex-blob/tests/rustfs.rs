//! 对**真实 RustFS** 跑的集成测试。
//!
//! 单测能证明的只有逻辑自洽；path-style 寻址、presigned URL 的签名是否被
//! 对端接受、multipart 的分片能否被拼回原样 —— 这些只有真发出去才知道。
//! 本文件里的每一条都对应一个「不实测就会在生产上第一次发现」的问题。
//!
//! # 跑不起来时 skip 而不是 fail
//!
//! 没起 RustFS 的开发机上，这些测试全部跳过并打印原因。
//! 但「静默跳过」本身是个陷阱：CI 里若 RustFS 没起来，绿灯会是假的。
//! 所以留了 `CORTEX_REQUIRE_RUSTFS=1` —— 置上以后跳过即失败。

use std::time::Duration;

use bytes::Bytes;
use cortex_blob::{
    BlobError, BlobStore, MULTIPART_THRESHOLD, S3BlobStore, S3Options, TenantPrefix,
    hash::{self, sha256_hex},
};

/// 真实的 1×1 PNG。用真图片而非随便几个字节，MIME 与尺寸探测才算被验到。
const PNG_1X1: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

/// 拿到一个可用的 store，或说明为什么用不了。
///
/// 返回 `None` 即「本机没有 RustFS」，调用方据此跳过。
async fn store() -> Option<S3BlobStore> {
    let _ = dotenvy::dotenv();

    let opts = S3Options {
        endpoint: std::env::var("S3_ENDPOINT").unwrap_or_else(|_| "http://localhost:9010".into()),
        bucket: std::env::var("S3_BUCKET").unwrap_or_else(|_| "cortex-blobs".into()),
        region: std::env::var("S3_REGION").unwrap_or_else(|_| "us-east-1".into()),
        access_key: std::env::var("RUSTFS_ACCESS_KEY").unwrap_or_else(|_| "cortexadmin".into()),
        secret_key: std::env::var("RUSTFS_SECRET_KEY").unwrap_or_else(|_| "cortex_dev_only".into()),
        force_path_style: true,
        // 用固定租户而不是每次随机：随机前缀会在桶里留下一堆再也没人清理的空前缀，
        // 而这些测试本就靠 unique_payload 做实例间隔离，不需要再靠租户隔离一次。
        tenant: TenantPrefix::new("cortex_test").expect("测试租户前缀本身应合法"),
    };

    let s = S3BlobStore::new(&opts).expect("S3Options 本身不合法，这与 RustFS 是否在跑无关");

    // ensure_bucket 是第一次真实网络调用，顺便当连通性探针
    match s.ensure_bucket().await {
        Ok(()) => Some(s),
        Err(e) => {
            assert!(
                std::env::var("CORTEX_REQUIRE_RUSTFS").is_err(),
                "CORTEX_REQUIRE_RUSTFS 已置位但连不上 RustFS（{}）：{e}",
                opts.endpoint
            );
            eprintln!("跳过：连不上 RustFS（{}）：{e}", opts.endpoint);
            None
        }
    }
}

/// 每个测试用独立内容，避免并行跑时互相干扰（内容寻址下同内容即同对象）。
fn unique_payload(tag: &str, len: usize) -> Bytes {
    let seed = format!(
        "{tag}-{:?}-{}",
        std::time::SystemTime::now(),
        std::process::id()
    );
    let mut v = seed.into_bytes();
    v.resize(len.max(1), 0);
    // 填成非全零：全零内容会被某些存储做稀疏优化，掩盖真实的读写路径
    for (i, b) in v.iter_mut().enumerate() {
        if *b == 0 {
            *b = (i % 251) as u8;
        }
    }
    Bytes::from(v)
}

macro_rules! skip_unless_rustfs {
    () => {
        match store().await {
            Some(s) => s,
            None => return,
        }
    };
}

#[tokio::test]
async fn image_round_trips_through_rustfs() {
    let s = skip_unless_rustfs!();
    let png = Bytes::from_static(PNG_1X1);

    let r = s
        .put(png.clone(), Some("application/octet-stream"))
        .await
        .expect("上传 PNG 失败");

    assert_eq!(r.hash, sha256_hex(&png), "返回的哈希与内容不符");
    assert_eq!(
        r.mime, "image/png",
        "字节头认出是 PNG 时必须覆盖客户端的 octet-stream 声明"
    );
    assert_eq!(
        r.storage_key,
        hash::storage_key(s.tenant(), &r.hash).unwrap(),
        "storage_key 必须由「租户前缀 + 哈希」派生 —— 它要写进 blobs.storage_key 并对两个后端通用"
    );

    let got = s.get(&r.hash).await.expect("按哈希取回失败");
    assert_eq!(got, png, "取回的字节与上传的不一致");

    s.delete(&r.hash).await.expect("清理失败");
}

/// path-style 的实际效果：URL 必须是 `host/bucket/key`，
/// 而不是 `bucket.host/key`（后者在 RustFS 上会 DNS 失败）。
///
/// 用 presigned URL 来验，是因为它是**唯一能直接看到 SDK 拼出的地址**的地方。
#[tokio::test]
async fn path_style_addressing_is_actually_used() {
    let s = skip_unless_rustfs!();
    let h = sha256_hex(b"path style probe");

    let url = s
        .presign_get(&h, Duration::from_secs(60))
        .await
        .expect("签发 presigned URL 失败");

    let host = url.host_str().expect("presigned URL 没有 host");
    assert!(
        !host.starts_with(&format!("{}.", s.bucket())),
        "桶名被拼进了子域名（{host}）—— force_path_style 没生效，RustFS 上会在 DNS 解析阶段失败"
    );
    assert!(
        url.path().starts_with(&format!("/{}/", s.bucket())),
        "path-style 下桶名应在路径首段，实际路径是 {}",
        url.path()
    );
}

#[tokio::test]
async fn range_read_returns_exact_slice() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("range", 4096);
    let r = s.put(payload.clone(), None).await.expect("上传失败");

    let mid = s.get_range(&r.hash, 100..200).await.expect("range 读失败");
    assert_eq!(
        &mid[..],
        &payload[100..200],
        "range 取到的切片与原内容对不上 —— HTTP Range 是闭区间，差一位错最常出在这里"
    );
    assert_eq!(mid.len(), 100, "左闭右开的 100..200 应恰好是 100 字节");

    let tail = s
        .get_range(&r.hash, 4090..4096)
        .await
        .expect("读末尾区间失败");
    assert_eq!(&tail[..], &payload[4090..4096], "末尾区间读错");

    // 完全越界应报 InvalidRange（HTTP 400），不能错报成 500
    let e = s
        .get_range(&r.hash, 999_999..1_000_000)
        .await
        .expect_err("完全越界的区间应报错");
    assert!(
        matches!(e, BlobError::InvalidRange(_)),
        "越界应归类为 InvalidRange 才能映射成 400，实际是：{e:?}"
    );

    s.delete(&r.hash).await.expect("清理失败");
}

/// presigned GET 必须能被一个**毫不知情的 HTTP 客户端**直接取用 ——
/// 这正是手机端省下一半带宽的前提。用 reqwest 裸请求验证。
#[tokio::test]
async fn presigned_get_is_usable_by_a_plain_http_client() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("presign-get", 2048);
    let r = s.put(payload.clone(), None).await.expect("上传失败");

    let url = s
        .presign_get(&r.hash, Duration::from_secs(300))
        .await
        .expect("签发 presigned GET 失败");

    let resp = reqwest::get(url.as_str())
        .await
        .expect("presigned GET 请求发不出去");
    assert!(
        resp.status().is_success(),
        "presigned GET 被拒（{}）—— 签名或寻址方式不被 RustFS 接受",
        resp.status()
    );

    let body = resp.bytes().await.expect("读 presigned GET 响应体失败");
    assert_eq!(body, payload, "经 presigned URL 取回的字节与上传的不一致");

    s.delete(&r.hash).await.expect("清理失败");
}

/// presigned PUT 是客户端直传的那一半。
///
/// 特别要验的是：**签名时不预设任何请求头**。若签进了 content-type，
/// 客户端发的头只要差一个字节就会 SignatureDoesNotMatch，
/// 而那个报错完全指不向真因。
#[tokio::test]
async fn presigned_put_accepts_a_plain_upload() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("presign-put", 1024);
    let h = sha256_hex(&payload);

    assert!(
        !s.exists(&h).await.expect("exists 探测失败"),
        "测试内容应当尚未存在，否则下面的直传验不到东西"
    );

    let url = s
        .presign_put(&h, Duration::from_secs(300))
        .await
        .expect("签发 presigned PUT 失败");

    let resp = reqwest::Client::new()
        .put(url.as_str())
        .body(payload.clone())
        .send()
        .await
        .expect("presigned PUT 请求发不出去");
    assert!(
        resp.status().is_success(),
        "presigned PUT 被拒（{}）—— 若是 SignatureDoesNotMatch，多半是签名时预设了请求头",
        resp.status()
    );

    // 客户端直传的字节，服务端要能按同一个哈希取回来 —— 这条闭环成立，
    // 「不经 cortexd 中转」才真正可用
    let got = s.get(&h).await.expect("直传后服务端取不回");
    assert_eq!(got, payload, "直传的字节与取回的不一致");

    s.delete(&h).await.expect("清理失败");
}

#[tokio::test]
async fn duplicate_upload_is_deduplicated() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("dedup", 8192);

    let a = s.put(payload.clone(), None).await.expect("首次上传失败");
    assert!(!a.deduplicated, "首次上传不该被判为去重命中");

    let b = s.put(payload.clone(), None).await.expect("重复上传失败");
    assert_eq!(a.hash, b.hash, "同内容两次上传必须得到同一个 key");
    assert_eq!(a.storage_key, b.storage_key, "storage_key 也必须一致");
    assert!(
        b.deduplicated,
        "第二次上传应命中已有对象 —— 去重失效意味着每次转发同一张图都要重付一次带宽"
    );

    s.delete(&a.hash).await.expect("清理失败");
}

/// 并发上传同一份内容：**不要求**只上传一次（那需要分布式锁，不划算），
/// 只要求最终对象完整且哈希正确 —— 即竞态是良性的。
#[tokio::test]
async fn concurrent_upload_of_same_content_is_benign() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("concurrent", 64 * 1024);
    let expected = sha256_hex(&payload);

    let s = std::sync::Arc::new(s);
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let s = s.clone();
        let p = payload.clone();
        tasks.push(tokio::spawn(async move { s.put(p, None).await }));
    }
    for t in tasks {
        let r = t.await.expect("任务 panic").expect("并发上传报错");
        assert_eq!(r.hash, expected, "并发上传返回了不同的哈希");
    }

    let got = s.get(&expected).await.expect("并发上传后取不回");
    assert_eq!(
        sha256_hex(&got),
        expected,
        "并发上传后对象内容与哈希不符 —— 说明有一路写坏了对象"
    );

    s.delete(&expected).await.expect("清理失败");
}

/// 超过门槛的对象走 multipart。分片能否被 RustFS 拼回原样，
/// 是那种「小文件全过、上线传视频才炸」的问题，必须实测。
#[tokio::test]
async fn multipart_upload_reassembles_exactly() {
    let s = skip_unless_rustfs!();
    // 刻意取 门槛 + 1 MiB：既跨过门槛，又保证最后一片不足整片，
    // 从而覆盖「末片被漏掉」这个 multipart 的经典错误
    let payload = unique_payload("multipart", MULTIPART_THRESHOLD + 1024 * 1024);
    let expected = sha256_hex(&payload);

    let r = s
        .put(payload.clone(), None)
        .await
        .expect("multipart 上传失败");
    assert_eq!(r.hash, expected, "multipart 上传返回的哈希不对");
    assert_eq!(
        r.size_bytes,
        payload.len() as i64,
        "size_bytes 与实际字节数不符"
    );

    let got = s.get(&r.hash).await.expect("取回 multipart 对象失败");
    assert_eq!(
        got.len(),
        payload.len(),
        "取回的长度与上传的不同 —— 多半是末片丢了"
    );
    assert_eq!(
        sha256_hex(&got),
        expected,
        "取回内容的哈希与上传时不符，分片被拼错了"
    );

    // 大对象上的 range 读：视频拖动播放走的正是这条路
    let slice = s
        .get_range(&r.hash, 8 * 1024 * 1024..8 * 1024 * 1024 + 256)
        .await
        .expect("在分片边界附近 range 读失败");
    assert_eq!(
        &slice[..],
        &payload[8 * 1024 * 1024..8 * 1024 * 1024 + 256],
        "跨分片边界的 range 读取到了错误的字节"
    );

    s.delete(&r.hash).await.expect("清理失败");
}

#[tokio::test]
async fn missing_object_maps_to_not_found() {
    let s = skip_unless_rustfs!();
    let absent = sha256_hex(&unique_payload("never-uploaded", 32));

    assert!(
        !s.exists(&absent).await.expect("exists 探测失败"),
        "没传过的内容不该存在"
    );

    let e = s.get(&absent).await.expect_err("取不存在的对象应报错");
    assert!(
        matches!(e, BlobError::NotFound(_)),
        "缺失必须归类为 NotFound（HTTP 404）；错报成 Backend 会在接口上变成 500：{e:?}"
    );
}

#[tokio::test]
async fn delete_is_idempotent_on_rustfs() {
    let s = skip_unless_rustfs!();
    let payload = unique_payload("delete", 256);
    let r = s.put(payload, None).await.expect("上传失败");

    s.delete(&r.hash).await.expect("首次删除失败");
    assert!(
        !s.exists(&r.hash).await.expect("exists 探测失败"),
        "删除后不该还存在"
    );
    s.delete(&r.hash)
        .await
        .expect("重复删除必须成功 —— purge 的级联清除任务要求可重跑");
}

#[tokio::test]
async fn ensure_bucket_is_idempotent() {
    let s = skip_unless_rustfs!();
    // store() 里已经建过一次，这里再建两次都应成功
    s.ensure_bucket().await.expect("重复建桶应幂等");
    s.ensure_bucket().await.expect("重复建桶应幂等");
}

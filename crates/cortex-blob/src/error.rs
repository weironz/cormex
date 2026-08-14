//! blob 层错误。
//!
//! 与 `CortexError::Store` 分开的理由：对象存储的失败模式与关系库完全不同
//! （网络抖动可重试、404 是正常控制流、非法哈希是安全事件），
//! 合并成一个 `Store(String)` 就再也分不出来了。边界处才收敛。

use thiserror::Error;

pub type Result<T, E = BlobError> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum BlobError {
    /// 哈希不是 64 位小写十六进制。**这条同时是路径遍历的栅栏**：
    /// 哈希是唯一进入存储路径的外部输入，`..` / `/` / `\` 都不是十六进制字符，
    /// 只要在此处拦死，下游拼路径就永远安全。
    #[error("非法 blob 哈希：{0}")]
    InvalidHash(String),

    /// 租户前缀不是合法的 Postgres 未加引号标识符。
    ///
    /// 与 [`InvalidHash`](Self::InvalidHash) 并列而不是合并成一个变体：
    /// 二者的**来源**完全不同——坏哈希多半是客户端传错了，坏租户前缀只可能
    /// 是服务端自己拼错了 schema 名（前缀从不来自请求体）。日志里混成一条，
    /// 「用户传了脏数据」与「我们的租户注册表出了问题」就再也分不开。
    #[error("非法租户前缀：{0}")]
    InvalidTenant(String),

    #[error("blob 不存在：{0}")]
    NotFound(String),

    /// 请求的字节区间越界或首尾颠倒。
    #[error("非法 range：{0}")]
    InvalidRange(String),

    /// 后端不支持该能力（如 `LocalFsBlobStore` 无法签发 presigned URL）。
    /// 单列一个变体，是为了让调用方能**显式降级**（改走服务端中转）
    /// 而不是把它当成一次普通失败去重试。
    #[error("后端不支持：{0}")]
    Unsupported(&'static str),

    #[error("对象存储错误：{0}")]
    Backend(String),

    #[error("IO 错误：{0}")]
    Io(#[from] std::io::Error),
}

// 这里刻意**没有** `is_retryable()`。
//
// 写过一版，形如 `matches!(self, Backend(_) | Io(_))`，然后发现它是个陷阱：
// `Backend` 同时装着「连接被拒」（该重试）与「SignatureDoesNotMatch /
// AccessDenied / NoSuchBucket」（重试一万次也一样）。离线写队列若信了它，
// 会在一个配错的密钥上无限重放，而日志里看到的只是「一直在重试」。
//
// 要给出可信的判断，得把 HTTP 状态码与 S3 错误码留在错误里，那是另一次
// 结构改动。在此之前，宁可让调用方自己按变体决策 —— 半真的谓词比没有更坏。
//
// 另注：跨网络的瞬时失败，aws-sdk-s3 自己已按 Standard 策略重试三次。

impl From<BlobError> for cortex_core::CortexError {
    fn from(e: BlobError) -> Self {
        match e {
            BlobError::NotFound(hash) => Self::NotFound {
                kind: "blob",
                id: hash,
            },
            BlobError::InvalidHash(_) | BlobError::InvalidRange(_) => Self::Invalid(e.to_string()),
            // InvalidTenant 刻意**不**归到 Invalid：它落在 `other` 里变成 500。
            // 租户前缀不来自请求体，它是服务端自己从租户注册表取出来的，
            // 回 400 等于让客户端去改一个它根本没发过的字段。
            other => Self::Store(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 错误分类最终会变成 HTTP 状态码。分错的代价很具体：
    /// 客户端把 404 当 500 就会去重试一个永远不会出现的对象，
    /// 把非法输入当 500 则会掩盖掉调用方自己的 bug。
    #[test]
    fn errors_map_to_the_right_http_status() {
        let cases = [
            (BlobError::NotFound("abc".into()), 404),
            (BlobError::InvalidHash("../x".into()), 400),
            (BlobError::InvalidRange("9..4".into()), 400),
            // 500 而非 400：租户前缀是服务端自己拼的，客户端无从修正
            (BlobError::InvalidTenant("../x".into()), 500),
            (BlobError::Backend("rustfs down".into()), 500),
            (BlobError::Unsupported("presign"), 500),
        ];
        for (e, want) in cases {
            let desc = e.to_string();
            let got = cortex_core::CortexError::from(e).http_status();
            assert_eq!(got, want, "{desc} 应映射为 HTTP {want}，实际是 {got}");
        }
    }

    #[test]
    fn not_found_carries_the_hash_through_the_conversion() {
        let e = cortex_core::CortexError::from(BlobError::NotFound("deadbeef".into()));
        let msg = e.to_string();
        assert!(
            msg.contains("deadbeef") && msg.contains("blob"),
            "转换后必须还看得出是哪个 blob 缺失，否则 404 日志无法定位：{msg}"
        );
    }
}

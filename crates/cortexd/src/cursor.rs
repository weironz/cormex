//! 会话消息分页游标的编解码。
//!
//! 游标落在 `(occurred_at, id)` 两列上，理由见
//! [`cortex_store::EpisodeCursor`]：`occurred_at` 来自客户端时钟，同一微秒
//! 落两行是常态，单列游标在同刻的那几行上要么跳过、要么重复。
//!
//! # 为什么是明文而不是 base64
//!
//! base64 只能造成「不透明」的错觉：客户端要构造一个假游标，解一次 base64
//! 就够了。真正的防线是**服务端校验**（见 [`decode`]）—— 游标里的两个字段
//! 都要能通过形制检查才会被送进 SQL。既然安全性不靠编码，那就选可读的那个：
//! 排查一个「翻页翻不动」的工单时，日志里能直接看出客户端拿的是哪一刻。

use chrono::{DateTime, Utc};
use cortex_core::{CortexError, Result};
use cortex_store::EpisodeCursor;

/// 两段之间的分隔符。RFC3339 与 ULID 都不含它，因此可以从右往左切一刀。
const SEP: char = '|';

/// ULID 的字符数。
const ULID_LEN: usize = 26;

/// 编码成 `<RFC3339>|<ULID>`。
#[must_use]
pub fn encode(occurred_at: DateTime<Utc>, id: &str) -> String {
    format!("{}{SEP}{id}", occurred_at.to_rfc3339())
}

/// 解析客户端回传的游标。
///
/// # 为什么要校验 ULID 的形制
///
/// `episodes.id` 的类型是 `ulid` 域（带正则 CHECK）。往它上面比一个畸形字符串，
/// Postgres 会报域约束错误，而那一路错误在 HTTP 上是 500 ——
/// 客户端看到的是「服务端挂了」，真相是「你的游标是坏的」。在这里拦下来
/// 换成 400，报错里直接说清是游标的问题。
///
/// # Errors
///
/// 缺分隔符、时间戳不是合法 RFC3339、或 id 不是 26 位 Crockford base32。
pub fn decode(raw: &str) -> Result<EpisodeCursor> {
    // 从**右**往左切：RFC3339 本身不含 '|'，但从左切会在将来给游标加第三段时
    // 悄悄改变语义，而从右切只需要认最后一段
    let (time_part, id) = raw
        .rsplit_once(SEP)
        .ok_or_else(|| CortexError::Invalid(format!("游标格式不对，缺少 {SEP:?}：{raw:?}")))?;

    let occurred_at = DateTime::parse_from_rfc3339(time_part)
        .map_err(|e| CortexError::Invalid(format!("游标里的时间不是合法 RFC3339：{e}")))?
        .with_timezone(&Utc);

    if id.len() != ULID_LEN || !id.bytes().all(is_crockford_upper) {
        return Err(CortexError::Invalid(format!(
            "游标里的 id 不是 26 位大写 Crockford base32 ULID：{id:?}"
        )));
    }

    Ok(EpisodeCursor {
        occurred_at,
        id: id.to_owned(),
    })
}

/// Crockford base32 大写字母表：去掉了 I / L / O / U。
/// 与 `migrations/20260807000001_init.sql` 里 `ulid` 域的正则一致。
const fn is_crockford_upper(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'A'..=b'H' | b'J' | b'K' | b'M' | b'N' | b'P'..=b'T' | b'V'..=b'Z')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ID: &str = "01ARZ3NDEKTSV4RRFFQ69G5FAV";

    #[test]
    fn roundtrips_through_encode_decode() {
        let t: DateTime<Utc> = "2026-08-07T11:22:33.456789Z".parse().expect("合法时间");
        let got = decode(&encode(t, SAMPLE_ID)).expect("自己编出来的游标必须能解回来");
        assert_eq!(got.id, SAMPLE_ID);
        assert_eq!(
            got.occurred_at, t,
            "微秒必须无损往返 —— 丢精度会让同一秒内的消息在翻页时重复或漏掉"
        );
    }

    #[test]
    fn a_malformed_id_is_rejected_before_it_reaches_sql() {
        // episodes.id 是带 CHECK 的 ulid 域，畸形值撞上去是 500；
        // 在这里拦下来才能给出一句说得清的 400
        for bad in [
            "2026-08-07T00:00:00Z|short",
            // I / L / O / U 不在 Crockford 字母表里
            "2026-08-07T00:00:00Z|01ARZ3NDEKTSV4RRFFQ69G5FAI",
            "2026-08-07T00:00:00Z|01arz3ndektsv4rrffq69g5fav",
            "2026-08-07T00:00:00Z|01ARZ3NDEKTSV4RRFFQ69G5FA'",
        ] {
            assert!(
                decode(bad).is_err(),
                "畸形游标 {bad:?} 必须在进 SQL 之前被拒"
            );
        }
    }

    #[test]
    fn a_missing_or_broken_timestamp_is_rejected() {
        assert!(decode(SAMPLE_ID).is_err(), "没有分隔符的游标必须拒");
        assert!(
            decode(&format!("昨天|{SAMPLE_ID}")).is_err(),
            "时间部分不是 RFC3339 必须拒"
        );
    }

    #[test]
    fn the_separator_is_taken_from_the_right() {
        // 从左切的话，将来给游标加第三段会悄悄改变这里的语义。
        // 现在就把方向钉死，免得那次改动的破坏被推迟到运行时才发现
        let raw = format!("2026-08-07T00:00:00Z{SEP}{SAMPLE_ID}");
        assert_eq!(decode(&raw).expect("应能解析").id, SAMPLE_ID);
    }
}

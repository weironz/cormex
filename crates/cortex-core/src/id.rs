//! 标识符。
//!
//! 全系统统一使用 ULID：26 字符，毫秒时间戳 + 随机位。
//! 客户端可离线生成，无需中心发号器，且天然按时间有序 ——
//! 这是 append-only 数据模型下多端无冲突写入的前提。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// 一个 ULID 标识符。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Id(ulid::Ulid);

impl Id {
    /// 基于当前时间生成一个新 ID。
    #[must_use]
    pub fn new() -> Self {
        Self(ulid::Ulid::generate())
    }

    /// 由「时刻 + 种子」**确定地**派生一个 ID。同样的输入永远得到同样的结果。
    ///
    /// # 它是为导入准备的
    ///
    /// 把三年的 ChatGPT / Claude 历史灌进来，这件事必然会被中断：关电脑、
    /// 断网、Ctrl-C、进程被杀。而 `POST /episodes` 已经按 id 判重了 ——
    /// 只要重跑能算出**同一批 id**，断点续传就是免费的，不需要任何进度文件。
    ///
    /// 用 [`Self::new`] 则做不到：它每次都是新的随机数，重跑一次就是把
    /// 整份历史再导一遍。
    ///
    /// # 时间段取的是**消息的原始时刻**，不是现在
    ///
    /// ULID 的高 48 位是毫秒时间戳，而系统里到处依赖这个顺序：episodes 按它
    /// 排序、recency 那一路召回按它算新鲜度。用 `Id::new()` 的话，三年前的
    /// 对话会全部带上今天的时间戳 —— 于是「最近聊过什么」永远是那三年的历史。
    ///
    /// # 种子怎么给
    ///
    /// 调用方拼一个**在本用途内唯一**的串，例如
    /// `"chatgpt:{conversation_id}:{message_id}"`。前缀是必要的：
    /// 不带来源的话，两个平台里恰好同号的消息会撞成同一个 id。
    ///
    /// # 越界的时刻会被夹住，而不是绕回去
    ///
    /// 1970 年之前（负数）与公元 10889 年之后都超出 48 位能表示的范围。
    /// 直接转换会**静默绕回**成一个看起来正常的时间。这里显式夹到边界：
    /// 顺序会不对，但 id 仍然确定、仍然唯一，而且不会伪装成另一个年份。
    #[must_use]
    pub fn derived(at: chrono::DateTime<chrono::Utc>, seed: &str) -> Self {
        use sha2::{Digest as _, Sha256};

        /// ULID 时间段是 48 位毫秒。
        const MAX_TIME_MS: i64 = (1 << 48) - 1;

        let ms = at.timestamp_millis().clamp(0, MAX_TIME_MS) as u64;

        // 域分隔前缀：这个函数以后可能被别的用途调用，而两个用途用同一个
        // 种子串算出同一个 id，是那种要到数据对不上时才发现的事
        let mut h = Sha256::new();
        h.update(b"cortex/id/derived/v1\0");
        h.update(seed.as_bytes());
        let digest = h.finalize();

        // 取前 16 字节当 u128，`from_parts` 自己会掩到 80 位
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&digest[..16]);
        Self(ulid::Ulid::from_parts(ms, u128::from_be_bytes(buf)))
    }

    /// 该 ID 编码的生成时刻。
    #[must_use]
    pub fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(self.0.datetime())
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for Id {
    type Err = ulid::DecodeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(ulid::Ulid::from_str(s)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_roundtrips_through_string() {
        let id = Id::new();
        let parsed: Id = id
            .to_string()
            .parse()
            .expect("生成的 ID 应当可以被解析回来");
        assert_eq!(id, parsed);
    }

    #[test]
    fn ids_sort_in_generation_order() {
        let first = Id::new();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let second = Id::new();
        assert!(first < second, "ULID 应当按生成时间有序");
    }

    #[test]
    fn id_renders_as_26_chars() {
        assert_eq!(Id::new().to_string().len(), 26);
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        s.parse().expect("测试用的时间常量")
    }

    /// **同样的输入永远得到同样的 id。** 导入的断点续传全靠这一条。
    ///
    /// 它比看起来脆：只要有人把哈希换成 `DefaultHasher`（那是最顺手的选择），
    /// 这条测试在**同一次运行里**仍然是绿的 —— 因为 `DefaultHasher` 的
    /// 随机化是按进程的。真正的失败要等到换一个 Rust 版本编译之后，
    /// 表现是「重跑导入把三年历史又灌了一遍」。
    ///
    /// 所以这里还钉死了一个**字面量**：算法一旦变了，它当场就变红。
    #[test]
    fn the_same_seed_always_yields_the_same_id() {
        let t = at("2023-04-15T08:30:00Z");
        let a = Id::derived(t, "chatgpt:conv-1:msg-7");
        let b = Id::derived(t, "chatgpt:conv-1:msg-7");
        assert_eq!(a, b, "同输入必须同输出，否则重跑导入会整份重来");

        assert_eq!(
            a.to_string(),
            "01GY20J4T0X1CZNXCWNNQX3E72",
            "派生算法变了。这不是一个可以顺手改的实现细节 —— 已经导入过的\
             用户重跑一次会得到一整份重复的历史"
        );
    }

    /// 不同的种子必须给出不同的 id，而且**只差一个字符也算不同**。
    #[test]
    fn different_seeds_do_not_collide() {
        let t = at("2023-04-15T08:30:00Z");
        let ids = [
            Id::derived(t, "chatgpt:conv-1:msg-7"),
            Id::derived(t, "chatgpt:conv-1:msg-8"),
            Id::derived(t, "chatgpt:conv-2:msg-7"),
            // 同一条消息号，不同平台。不带来源前缀的话这两个会撞
            Id::derived(t, "claude:conv-1:msg-7"),
        ];
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "种子不同却撞了 id");
    }

    /// 时间段必须是**消息的原始时刻**，而且顺序要对。
    ///
    /// 用 `Id::new()` 的话三年前的对话会全带上今天的时间戳 ——
    /// 于是「最近聊过什么」这一路召回永远返回那三年的历史。
    #[test]
    fn the_time_part_is_the_original_moment() {
        let old = at("2023-04-15T08:30:00Z");
        let recent = at("2026-08-09T12:00:00Z");

        let a = Id::derived(old, "x");
        assert_eq!(
            a.timestamp().timestamp_millis(),
            old.timestamp_millis(),
            "时间段没保住原始时刻"
        );

        assert!(
            a < Id::derived(recent, "x"),
            "ULID 必须按原始时间有序 —— episodes 排序与 recency 召回都靠它"
        );
    }

    /// 越界的时刻夹到边界，**不绕回**。
    ///
    /// 直接 `as u64` 的话，1970 年之前的负数会变成一个巨大的正数，
    /// 再被掩到 48 位 —— 得到的是一个看起来完全正常、却指向另一个年份的
    /// 时间戳。导出文件里出现 `created_at: 0` 或者一个错误的负数并不稀奇。
    #[test]
    fn out_of_range_moments_clamp_instead_of_wrapping() {
        let ancient = at("1900-01-01T00:00:00Z");
        let id = Id::derived(ancient, "x");
        assert_eq!(
            id.timestamp().timestamp_millis(),
            0,
            "1970 之前应当夹到纪元，而不是绕成某个未来时刻"
        );
        // 仍然确定、仍然唯一
        assert_eq!(id, Id::derived(ancient, "x"));
        assert_ne!(id, Id::derived(ancient, "y"));
    }
}

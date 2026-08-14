//! 客户端 ↔ 服务端那条线上的全部内容。
//!
//! # 为什么是一个独立的 crate
//!
//! agent 循环搬到本地之后，有**两个**进程在讲这套协议：cortexd 对 Web
//! 客户端讲，本地 agent 对 GUI / CLI 讲（并把不认识的路径原样反代给 cortexd）。
//!
//! 两个独立发版的二进制各自维护一份 DTO 定义，漂移只是时间问题，而
//! 漂移的表现是**字段静默丢失**：`serde` 默认忽略不认识的字段，
//! 少一个 `path` 只会让 UI 少画一行，不会有任何红灯。放进一个共享 crate 之后，
//! 这类不一致在**编译期**就没了 —— 两侧引用的是同一个类型。
//!
//! # 为什么 [`confirm`] 里只剩一个类型
//!
//! `GET /confirmations` 的响应体就是 [`confirm::PendingInfo`]，而
//! [`dto::ChatEvent::Confirm`] 的字段口径必须与它逐字对齐 —— 把两者分到不同
//! crate，等于把一个契约的两半交给两个地方维护。所以它留在这里。
//!
//! 那本**簿子**（`ConfirmRegistry` 与它的 oneshot、TTL、`Drop` 清理）曾经也
//! 在这里，靠的是同一句话。但那句话覆盖不了它：簿子不是契约，它是运行时。
//! 而它把整个 `cortex-agent` 拖进了这个线协议 crate，于是**只想用记忆那一层
//! 的人也得连 agent 一起编** —— 记忆那一层要独立开源，这条依赖是拦路的。
//!
//! 簿子现在在 `cortex-local`：它是唯一的宿主（cortexd 不跑 agent，容器里那个
//! 不问确认）。风险等级的线上写法则是 `cortex_agent::Risk::as_wire`，
//! 住在枚举自己旁边 —— 任何拿得到 `Risk` 的宿主都拿得到它。

pub mod auth;
pub mod confirm;
pub mod delegate;
pub mod dto;
pub mod episodes;
pub mod import;
pub mod llm;

/// 本端讲的线协议版本。**契约不兼容地变了才 +1。**
///
/// # 为什么不能拿发布版本号（`cortex_core::VERSION`）来比
///
/// 0.1.3 和 0.1.4 之间绝大多数改动与线协议无关。拿它比，等于每发一个
/// 补丁版本就宣布一次不兼容 —— 于是这个检查很快会被当成噪声关掉，
/// 而它唯一有用的那一次也就跟着没了。
///
/// # 什么算「不兼容」
///
/// 只有**去掉字段、改字段含义、改字段类型、去掉端点**才算。
/// 加字段不算：`serde` 忽略不认识的键，而新字段一律带 `#[serde(default)]`
/// （见 [`dto::FactDto::source_channel`] 那几个）。
///
/// 加了字段就 +1 的话，一次纯增量的改动会把所有老客户端挡在门外，
/// 而它们本来完全能用。
pub const PROTOCOL_VERSION: u32 = 1;

/// 本端还能对话的**最老**对端协议版本。
///
/// 与 [`PROTOCOL_VERSION`] 分开，是为了让「向后兼容一两个版本」这件事
/// 能被表达出来。删掉对某个老版本的支持时抬这个数 —— 而不是等到
/// 出问题时才发现两侧对不上。
pub const MIN_PEER_PROTOCOL: u32 = 1;

/// 一个版本必须能和自己对话。
///
/// 看着像废话，挡的却是一个真会发生的手滑：抬 [`PROTOCOL_VERSION`] 时
/// 顺手把 [`MIN_PEER_PROTOCOL`] 抬过了头，于是新版本连自己都拒绝。
///
/// 这在开发环境里**测不出来** —— 那时两侧是同一次编译产出的，
/// 而两个常量都错得一样。所以放在编译期：构建当场就红，
/// 而不是等到某台机器上两个进程握手时才发现。
const _: () = assert!(
    MIN_PEER_PROTOCOL <= PROTOCOL_VERSION,
    "MIN_PEER_PROTOCOL 超过了 PROTOCOL_VERSION —— 这个版本会拒绝和自己对话"
);

/// 两端的协议能不能对话。
///
/// # 两个方向都要查
///
/// 只查一头是查不全的：
///
/// - `peer_protocol < MIN_PEER_PROTOCOL` —— **对端太老**，它不会说我要的话。
/// - `PROTOCOL_VERSION < peer_min_peer` —— **我太老**，对端已经不再支持我。
///   这一条只有对端知道，所以它必须把自己的下限也报出来。
///
/// 少查第二条的后果是：服务端升级并停止支持老协议之后，老客户端仍然
/// 兴高采烈地连上去，然后在某个具体请求上以一种看不出所以然的方式失败。
///
/// # Errors
/// 不兼容时返回一句**给人看**的话：带上两边的数字，以及该升哪一边。
pub fn check_peer(peer_protocol: u32, peer_min_peer: u32, peer_label: &str) -> Result<(), String> {
    if peer_protocol < MIN_PEER_PROTOCOL {
        return Err(format!(
            "{peer_label}讲的是协议 v{peer_protocol}，而本端最低只支持 v{MIN_PEER_PROTOCOL}。\
             请把{peer_label}升级到更新的版本。"
        ));
    }
    if PROTOCOL_VERSION < peer_min_peer {
        return Err(format!(
            "本端讲的是协议 v{PROTOCOL_VERSION}，而{peer_label}最低要求 v{peer_min_peer}。\
             请把本机这一侧升级到更新的版本。"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个方向都要真的挡住，而且要说清该升哪一边。
    ///
    /// 只挡一头是最容易写出来的版本，也是没用的那个：服务端单方面
    /// 停止支持老协议时，老客户端会照样连上去。
    #[test]
    fn both_directions_are_checked_and_say_which_side_to_upgrade() {
        assert!(check_peer(PROTOCOL_VERSION, MIN_PEER_PROTOCOL, "远端").is_ok());

        let too_old = check_peer(MIN_PEER_PROTOCOL - 1, 0, "远端").expect_err("对端太老必须挡住");
        assert!(
            too_old.contains("升级到"),
            "报错要说清该升哪一边，实际：{too_old}"
        );

        let we_are_old = check_peer(PROTOCOL_VERSION, PROTOCOL_VERSION + 1, "远端")
            .expect_err("对端已不再支持本端时必须挡住 —— 这条只有对端知道");
        assert!(
            we_are_old.contains("本机"),
            "这个方向该让用户升**本机**，实际：{we_are_old}"
        );
    }

    /// **此刻正跑在生产上的 0.1.3 必须被判为兼容。**
    ///
    /// 下面这段 JSON 不是编的，是从一个真的 0.1.3 守护进程的 `/health`
    /// 上抓下来的原文 —— 它没有 `protocol` 也没有 `min_peer_protocol`，
    /// 因为这个检查那时还不存在。
    ///
    /// 缺省值取 0 是这里最自然、也最致命的写法：检查上线当天，
    /// 所有桌面端都会因为「远端协议 v0 太老」而拒绝启动 ——
    /// 而它们其实完全能用，什么契约都没变。
    ///
    /// 字段缺席的正确含义是「早于本检查的守护进程」，也就是 v1。
    #[test]
    fn a_daemon_that_predates_this_check_is_still_compatible() {
        let real_0_1_3 = r#"{"status":"ok","version":"0.1.3","database":"not_wired",
                             "blob_backend":"s3","auth":"token"}"#;
        let peer: crate::dto::PeerProtocol =
            serde_json::from_str(real_0_1_3).expect("老 daemon 的 /health 必须还能解析");

        assert!(
            check_peer(peer.protocol, peer.min_peer_protocol, "远端").is_ok(),
            "0.1.3 被判成不兼容 —— 这个检查一上线就会把所有桌面端挡在门外。\
             解析出来的是 protocol={} min_peer={}",
            peer.protocol,
            peer.min_peer_protocol
        );
    }

    /// `/health` 上别的字段怎么变，都不该动摇协议检查。
    ///
    /// 只取两个数字而不是整个 `Health`，就是为了这一条：数据库状态、
    /// 对象存储后端、向量化健康度都在同一个响应里，而它们比协议版本
    /// 变得频繁得多。用整个 `Health` 去解，那些字段的任何变动都能让
    /// 协议检查本身解析失败 —— 而它失败的表现，恰恰是「不兼容时没人拦」。
    #[test]
    fn unrelated_health_fields_cannot_break_the_check() {
        let future = r#"{"status":"degraded","version":"9.9.9","protocol":1,
                         "min_peer_protocol":1,"database":{"pool":"nested object"},
                         "something_invented_later":[1,2,3]}"#;
        let peer: crate::dto::PeerProtocol =
            serde_json::from_str(future).expect("不相干的字段不该影响协议检查");
        assert_eq!((peer.protocol, peer.min_peer_protocol), (1, 1));
    }

    /// 同一次构建的两个进程必须能握上手。
    ///
    /// 「两个常量的大小关系」由 crate 顶部那条 `const _: () = assert!(…)`
    /// 在编译期保证；这里走的是**真正的握手函数**，因为大小关系对了
    /// 不等于 `check_peer` 用对了它们 —— 比较写反、参数传反，都能在
    /// 常量正确的前提下让同版本的两个进程互相拒绝。
    #[test]
    fn a_build_is_always_compatible_with_itself() {
        assert!(
            check_peer(PROTOCOL_VERSION, MIN_PEER_PROTOCOL, "对端").is_ok(),
            "同一次构建的两个进程握不上手 —— check_peer 里的比较方向反了"
        );
    }
}

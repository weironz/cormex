//! 这台机器上放本地状态的地方。
//!
//! # 为什么在 cortex-core 而不在本地 agent 里
//!
//! 它原本长在 `cortex-local::config`，因为只有 agent 用得着。现在 CLI 也要用：
//! 它得去这个目录里找**桌面端已经起好的那个 agent** 的地址握手文件，
//! 否则同一台机器上会跑两个 agent，抢同一份 `workspaces.json` 与 `outbox.mark`
//! —— 而那两个文件只有进程内 `Mutex`，没有文件锁。
//!
//! 各写一份是本轮已经抓了三次的形状（`risk_str` 压平、`Health` 三份、
//! 提示词自相矛盾）：两边今天算出同一个路径，明天有人改了其中一个，
//! 症状是「CLI 找不到桌面端的 agent」，而没有任何东西会报错。

use std::path::PathBuf;

use crate::{CortexError, Result};

/// 本地状态目录：outbox、工作区绑定、地址握手文件都放这里。
///
/// Windows 用 `%LOCALAPPDATA%` 而不是 `%APPDATA%`：后者会被漫游配置文件同步到
/// 域内其他机器上，而这里面全是**只对这台机器成立**的东西 ——
/// `D:\codes\myproject` 同步到同事的笔记本上没有任何意义，
/// 而一个跨机器同步的 outbox 会让同一条 episode 被两台机器各重放一次。
///
/// # Errors
/// 目录建不出来（权限、磁盘满）时返回错误。**不回落到临时目录** ——
/// 那会让 outbox 在重启后消失，也就是「断网期间的对话默默没了」，
/// 而那正是这个队列存在的理由。
pub fn state_dir() -> Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
    };
    let base = base.ok_or_else(|| {
        CortexError::Config(
            "找不到本地状态目录：Windows 上需要 %LOCALAPPDATA%，\
             其余平台需要 $XDG_DATA_HOME 或 $HOME"
                .into(),
        )
    })?;
    let dir = base.join("cortex");
    std::fs::create_dir_all(&dir)
        .map_err(|e| CortexError::Config(format!("建不出本地状态目录 {}：{e}", dir.display())))?;
    Ok(dir)
}

/// 地址握手文件的扩展名。
///
/// 本地 agent 绑 `127.0.0.1:0`（内核挑端口）之后把真实地址写进这样一个文件，
/// **拉起它的那一方**读回来后随即删掉。**猜一个固定端口的失败方式是最坏的
/// 那种**：连到一个碰巧在听的别的东西上 —— 本轮实测撞到过，8090 上是另一个
/// 应用的 web 客户端，它对 `/health` 回 200 加一段 HTML。
pub const ADDR_FILE_EXT: &str = "addr";

/// 存活指针文件的扩展名。见 [`live_file`]。
pub const LIVE_FILE_EXT: &str = "live";

/// 「这台机器上有个 agent 在 X 端口」的指针文件。
///
/// # 与 [`ADDR_FILE_EXT`] 是两件事
///
/// 地址握手文件是**一次性**的：拉起它的那一方读到就删，因为它只回答
/// 「我刚拉起的那个绑到哪了」。留着它反而制造陈旧 —— 那正是当初决定删掉的
/// 理由。
///
/// 存活指针回答的是另一个问题：**「现在有没有一个能用的 agent」**，
/// 而问的人可能根本不是拉起它的那个进程。桌面端起了一个 agent，之后用户在
/// 终端敲 `cortex chat` —— CLI 需要找到它，否则同机会有两个 agent 实例，
/// 抢同一份 `workspaces.json` 与 `outbox.mark`（那两个文件只有进程内
/// `Mutex`，没有文件锁）。
///
/// # 陈旧怎么办 —— 靠探活，不靠删文件
///
/// 进程被 kill、断电、崩溃时这个文件会留下来。读的一方**必须**对拿到的地址
/// 做一次真探活（打 `/health` 并确认 `role == "local-agent"`），探不通就跳过。
/// 当初 addr 文件选择「读完即删」是因为**那时没有探活**；现在有了，
/// 陈旧就不再是需要用文件生命周期去规避的问题。
///
/// # 文件名带 pid
///
/// 同机允许多个 agent 并存（不同账号、开发时手工起的），一个固定文件名会让
/// 它们互相覆盖，而覆盖的后果是「指向一个已经死了的实例」。带 pid 就各写各
/// 的，读的一方扫目录、逐个探活、用第一个活的。
#[must_use]
pub fn live_file(dir: &std::path::Path, pid: u32) -> PathBuf {
    dir.join(format!("agent-{pid}.{LIVE_FILE_EXT}"))
}

/// 存活指针的内容。
///
/// # 为什么不只写一个地址
///
/// 复用一个 agent 等于**把自己的请求交给它的身份**：它拿自己的 token 去连
/// 自己的 remote。地址相同不代表身份相同 —— 一个指向别的 cortexd、或者登着
/// 另一个账号的 agent，会让 CLI 静默地读写**别人的**记忆，而这件事没有任何
/// 症状。本轮已经因为「remote 没传下去」吃过一次同类的亏。
///
/// 所以复用前要比对两样：远端地址，以及**凭据指纹**。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct LivePointer {
    /// 实际监听地址，形如 `127.0.0.1:54321`
    pub addr: String,
    /// 它连的那个 cortexd
    pub remote: String,
    /// 凭据指纹。见 [`token_fingerprint`]。无凭据时是 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_fp: Option<String>,
}

/// 凭据的短指纹 —— 只用来判断「是不是同一把」。
///
/// SHA-256 取前 16 个十六进制字符（64 位）。**不是**给认证用的，
/// 所以截断没有安全含义；它要防的是把 token 本身写进一个明文文件里 ——
/// 那个文件躺在状态目录，同机任意进程都读得到。
///
/// 64 位对「同一把 token 算出同一个值」这件事绰绰有余，而反推不出原文。
#[must_use]
pub fn token_fingerprint(token: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let d = Sha256::digest(token.as_bytes());
    format!("{d:x}").chars().take(16).collect()
}

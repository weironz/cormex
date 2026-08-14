//! 沙箱令牌 —— 发给容器里那个 `cortex-local` 的凭据。
//!
//! # 为什么不能把用户的登录 token 塞进容器
//!
//! 容器里跑的是**不可信代码**：模型的输出、用户装的依赖、被 prompt 注入的
//! 工具调用，全在里面。拿到用户的 bearer token 等于拿到那个人的全部记忆、
//! 全部会话、全部附件。
//!
//! OpenHands 在这一点上走的是另一条路（把 LLM api key 直接发进容器，容器
//! 自己连供应商）。我们不 —— key 留在 cortexd，容器经 `/llm/stream` 代理，
//! 顺带天然有了配额与计费的位置。
//!
//! # 这个令牌的作用域**不只是一张路由白名单**
//!
//! 这是本设计里最不显然的一处。四家云 agent（Codex / Claude web / Devin /
//! Manus）都可以只做路由级隔离，因为**它们的沙箱没有可写的长期记忆**：
//! 沙箱能碰的只有一个 clone 出来的 git 工作副本，坏了丢掉重来。
//!
//! 我们不同。沙箱要调 `POST /episodes`（写对话）与 `GET /memory/search`
//! （查记忆），这两条是**穿透容器边界的持久通道**：
//!
//! - `/episodes` 的 `role` 若由请求体自由决定，被注入的 agent 可以伪装成
//!   「用户亲口说的」写进抽取管线 —— 那条 fact 会在**未来所有会话、所有
//!   设备**上被召回并注入 system prompt。容器早就销毁了，记忆还在。
//! - `/memory/search` 若按租户检索，沙箱能把该用户**其他会话**里的敏感记忆
//!   捞出来，再经出网通道送走。
//!
//! 所以「爆炸半径已被容器限住」这句话对两家成立，对我们不成立。作用域必须
//! 下沉到**语义**：见 [`SandboxScope`]。
//!
//! # 令牌与 session 绑定，不只与 owner 绑定
//!
//! 一个用户可能有多个会话。沙箱只该看得见自己那一个 —— 否则「隔离」只隔到
//! 了用户这一层，而同一个用户的两段工作之间同样需要边界（一段在处理公司
//! 合同、另一段在跑一个从网上抄来的脚本）。

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 沙箱令牌的有效期。
///
/// 比票据（60 秒）长得多：这把是长连接用的，容器一活可能几小时。
/// 但仍然有限 —— 容器被回收后令牌应当自然失效，而不是永远留在表里。
/// 每次 [`SandboxTokens::touch`] 会续期，所以活跃的沙箱不会被误杀。
const TTL: Duration = Duration::from_secs(6 * 3600);

/// 沙箱能做什么。**按语义授权，不是按路由**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxScope {
    /// 令牌属于谁。租户隔离仍然由它决定 —— 这一层是既有的。
    pub owner: String,
    /// 绑定的会话。沙箱只能读写这一个会话的东西。
    pub session_id: String,
    /// 这个会话属于哪个项目。`None` = 未分组。
    ///
    /// **它决定容器与卷**（见 [`Self::key`]），不参与 [`Self::allows`] ——
    /// 令牌能干什么仍然只看会话。
    pub project: Option<String>,
}

impl SandboxScope {
    /// 容器与卷按这个键分。
    ///
    /// # 为什么按项目而不是按用户
    ///
    /// 上一版一个用户只有一个 `/workspace`。于是「客户合同」与「从网上抄来的
    /// 脚本」两段工作的文件混在同一个目录里 —— 装依赖互相踩、`ls` 一屏全是
    /// 别的项目的东西，而这两件事用户自己是分开的（他建了两个项目）。
    ///
    /// 按项目分之后，切项目会重建容器（冷启动 913 ms，用户感知不到），
    /// 同一个项目的多个会话共用一个容器。
    ///
    /// # 为什么未分组仍然用裸 owner
    ///
    /// 恒等映射 = **生产上不用做数据迁移**。现有那些卷叫 `cortex-ws-<owner>`，
    /// 它们的会话都还没有项目，所以照旧命中；一旦被移进项目才会拿到新卷，
    /// 而那时用户是明确在「新建一个项目」，空工作区是他预期的。
    ///
    /// # 项目那一段为什么是哈希而不是 id 本身
    ///
    /// **容器名会当 DNS 名用**（cortexd 与沙箱同网段时直连 `http://<容器名>`），
    /// 而 DNS 标签的硬上限是 **63 字节**。两个 ULID 直接拼起来：
    ///
    /// ```text
    /// cortex-sbx- + 26 + --p- + 26 = 67  ✗
    /// ```
    ///
    /// 真机上撞到过，症状很误导：容器 `Up (healthy)`、里面的 agent 日志写着
    /// 「已就绪」，而 cortexd 报「30 秒没应答」—— 因为它连名字都解析不出来。
    ///
    /// 12 位十六进制（48 bit）之后是 11+26+4+12 = **53**，留足余量。
    /// 碰撞概率在「一个用户的项目数」这个量级上可以忽略。
    ///
    /// 代价是容器名读不出项目 id。反查的路子是 cortexd 每轮那条日志
    /// （`本轮走云沙箱 owner=… project=… sandbox=…`）—— 它把三者印在一行。
    /// **不给 runner 加一个 project 入参**：那条 trait 只收作用域与 spec hash
    /// 是刻意的（有测试守着「调用方塞不进第二个挂载」），为了一行日志
    /// 松掉它不划算。
    ///
    /// # 分隔符为什么不用管注入
    ///
    /// owner 由服务端从凭据解析（`current_user`），永远在最前；
    /// project 由客户端给，但它只能改变**自己名下**的后半段 ——
    /// 拼出的键无论如何都以自己的 owner 开头，够不到别人的键。
    /// 哈希顺带把「客户端能塞多长的 id 进来」这件事也钉死了。
    #[must_use]
    pub fn key(&self) -> String {
        match &self.project {
            None => self.owner.clone(),
            Some(p) => {
                use sha2::{Digest as _, Sha256};
                let digest = Sha256::digest(p.as_bytes());
                format!("{}--p-{}", self.owner, hex::encode(&digest[..6]))
            }
        }
    }

    /// 这条路径 + 方法允许吗。
    ///
    /// # 为什么是白名单而不是黑名单
    ///
    /// 与 `WORKSPACE_FREE_TOOLS` 同一个论证：黑名单漏掉一条新路由 = 沙箱
    /// 悄悄多了一样能力（静默、危险）；白名单漏掉一条 = 沙箱少一样能力
    /// （响亮、当场可见）。**失败方向不同，选会往安全那边倒的那个。**
    ///
    /// 清单是代码审计出来的**实际调用**，不是猜的。少一条的后果已经量化过：
    /// 漏掉 `GET /sessions/{id}` 的话，`load_history` 会静默降级为空历史，
    /// 云端会话**逐轮失忆且没有任何报错**。
    #[must_use]
    pub fn allows(&self, method: &axum::http::Method, path: &str) -> bool {
        use axum::http::Method;
        match (method, path) {
            // 每轮一次到多次。LLM key 不进容器的代价与收益都在这条上
            (&Method::POST, "/llm/stream") => true,
            // 写 user / assistant episode，以及 outbox 重放
            (&Method::POST, "/episodes") => true,
            // memory_search 工具
            (&Method::GET, "/memory/search") => true,
            // 每轮拉会话历史。**漏掉这条 = 逐轮失忆且静默**
            (&Method::GET, p) if is_session_path(p) => true,
            // 启动时问「这把凭据属于谁」。漏掉的话状态目录永远落在 _pending
            (&Method::GET, "/auth/me") => true,
            _ => false,
        }
    }
}

/// `/sessions/{id}` 形状（不含 `/sessions` 本身 —— 那是列全部会话）。
fn is_session_path(path: &str) -> bool {
    match path.strip_prefix("/sessions/") {
        // 只认一层：`/sessions/abc` 是，`/sessions/abc/xxx` 不是。
        // 多一层就是另一个端点，而这里的清单是逐条审计出来的
        Some(rest) => !rest.is_empty() && !rest.contains('/'),
        None => false,
    }
}

/// 已签发的沙箱令牌。
///
/// 与 `TicketBook` 同形（内存、随进程走）。cortexd 重启后所有沙箱令牌失效，
/// 而那是**对的**：那些容器也需要被重新接管，让它们带着一把还能用的凭据
/// 继续跑才是问题。
#[derive(Default)]
pub struct SandboxTokens {
    inner: Mutex<HashMap<String, (SandboxScope, Instant)>>,
}

impl SandboxTokens {
    /// 签一把。调用方必须**已经**确认了 owner 的身份。
    pub fn issue(&self, scope: SandboxScope) -> String {
        let mut buf = [0u8; 32];
        getrandom::fill(&mut buf).expect("内核熵源不可用，拒绝签发可预测的令牌");
        let token = hex::encode(buf);
        let mut guard = self.inner.lock().expect("令牌表的锁不该中毒");
        // 顺手清过期的。同 TicketBook：签发是唯一让这个表长大的动作，
        // 在这里清就不可能「只涨不清」
        let now = Instant::now();
        guard.retain(|_, (_, at)| now.duration_since(*at) < TTL);
        guard.insert(token.clone(), (scope, now));
        token
    }

    /// 认这把令牌吗，认的话它的作用域是什么。顺带续期。
    #[must_use]
    pub fn resolve(&self, token: &str) -> Option<SandboxScope> {
        let mut guard = self.inner.lock().ok()?;
        let now = Instant::now();
        let (scope, at) = guard.get_mut(token)?;
        if now.duration_since(*at) >= TTL {
            guard.remove(token);
            return None;
        }
        *at = now;
        Some(scope.clone())
    }

    /// 这个作用域（用户 + 项目）手上那把还有效的令牌（如果有）。
    ///
    /// # 为什么需要「复用」而不是每轮签一把新的
    ///
    /// 容器的**入站**认证用的是它启动时 env 里那把（`cortex-local` 的入站与
    /// 出站共用一个 token，刻意不引入第二个秘密）。每轮换新的话，第二轮反代
    /// 过去就是 401 —— 而那条错误读起来像「沙箱坏了」。
    ///
    /// 真机第一轮通、第二轮 401，就是这个。**令牌的生命周期跟着容器走**，
    /// 不跟着轮次走。
    ///
    /// 按 [`SandboxScope::key`] 而不是按 owner 找：一个用户可以同时有几个
    /// 项目的沙箱活着，拿错那把的后果是**把 A 项目的令牌塞给 B 项目的容器**
    /// —— 而容器起来之后照常应答，只是它认的是另一把，第二轮才 401。
    #[must_use]
    pub fn find_by_key(&self, key: &str) -> Option<String> {
        let guard = self.inner.lock().ok()?;
        let now = Instant::now();
        guard.iter().find_map(|(tok, (scope, at))| {
            (scope.key() == key && now.duration_since(*at) < TTL).then(|| tok.clone())
        })
    }

    /// 把一把已有的令牌改绑到另一个会话上。
    ///
    /// 同一个容器服务这个用户的下一个会话时用。**不是放宽作用域** ——
    /// 改完之后它仍然只对得上一个会话，只是换了一个。
    pub fn rebind(&self, token: &str, session_id: &str) {
        if let Ok(mut g) = self.inner.lock()
            && let Some((scope, at)) = g.get_mut(token)
        {
            scope.session_id = session_id.to_owned();
            *at = Instant::now();
        }
    }

    /// 作废（沙箱停掉时调）。
    pub fn revoke(&self, token: &str) {
        if let Ok(mut g) = self.inner.lock() {
            g.remove(token);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Method;

    fn scope() -> SandboxScope {
        SandboxScope {
            owner: "u1".into(),
            session_id: "s1".into(),
            project: None,
        }
    }

    fn in_project(owner: &str, project: &str) -> SandboxScope {
        SandboxScope {
            owner: owner.into(),
            session_id: "s1".into(),
            project: Some(project.into()),
        }
    }

    /// 白名单**正好**是审计出来的那五条，一条不多。
    ///
    /// 每一条都有代价：多放一条就是沙箱多一样穿透容器边界的能力。
    ///
    /// `/confirmations` 那条从来没进过这张单子，而现在它连**存不存在**都
    /// 不成立了：cortexd 不跑 agent，也就没有那本簿子（见 `routes.rs` 里
    /// 路由清单上那段注释）。这条否定用例因此从「防一个无界递归」变成
    /// 「防有人把一个已经不存在的端点当成还在」—— 两者都该让它红。
    #[test]
    fn the_allowlist_is_exactly_what_the_agent_actually_calls() {
        let s = scope();
        for (m, p) in [
            (Method::POST, "/llm/stream"),
            (Method::POST, "/episodes"),
            (Method::GET, "/memory/search"),
            (Method::GET, "/sessions/01ABC"),
            (Method::GET, "/auth/me"),
        ] {
            assert!(
                s.allows(&m, p),
                "{m} {p} 是每轮都要打的，漏了整个沙箱不可用"
            );
        }

        for (m, p) in [
            // 确认回路：加进来会让那个回环复活
            (Method::POST, "/confirmations"),
            (Method::GET, "/confirmations"),
            // 列全部会话 —— 沙箱只该看得见自己那一个
            (Method::GET, "/sessions"),
            // 改会话（含换工作区）不该由沙箱自己决定
            (Method::PATCH, "/sessions/01ABC"),
            // 项目、附件、同步：都不是 agent 循环需要的
            (Method::GET, "/projects"),
            (Method::POST, "/blobs"),
            (Method::GET, "/sync"),
            (Method::POST, "/auth/ticket"),
            // 写 episode 是 POST；GET 那条是回放别人的对话
            (Method::GET, "/episodes/01ABC"),
            // 多一层路径就是另一个端点
            (Method::GET, "/sessions/01ABC/messages"),
        ] {
            assert!(
                !s.allows(&m, p),
                "{m} {p} 不在审计出来的调用清单里，放行等于给沙箱一样它不需要的能力"
            );
        }
    }

    /// **被攻陷的容器碰不到自己的备份。**
    ///
    /// 这条单独立出来，是因为它守的不是「少给一点权限」这种程度问题，
    /// 而是数据兜底第一层**整层的前提**：快照由宿主驱动、写进容器够不着的
    /// 地方，所以一个能在容器里执行任意命令的攻击者删不掉它。
    ///
    /// 那条性质完全来自这里的**拓扑**（令牌白名单里没有这几条路由），
    /// 不来自任何运行期判断。谁哪天顺手把 `/sandbox/snapshots` 加进
    /// `allows`，整层兜底就一起没了 —— 而那**不会有任何症状**，
    /// 直到有人真的需要恢复的那一天。
    #[test]
    fn the_sandbox_cannot_reach_its_own_backups() {
        let s = scope();
        for (m, p) in [
            (Method::GET, "/sandbox/snapshots"),
            (Method::POST, "/sandbox/snapshots"),
            (Method::POST, "/sandbox/snapshots/01ABC/restore"),
        ] {
            assert!(
                !s.allows(&m, p),
                "{m} {p} 被放行了 —— 数据兜底的第一层就此失效：\
                 一个被攻陷的容器可以列出、覆盖、并（将来 purge 落地后）\
                 删掉自己的全部备份，而这件事没有任何症状"
            );
        }
    }

    #[test]
    fn a_token_resolves_to_its_scope_and_an_unknown_one_does_not() {
        let book = SandboxTokens::default();
        let t = book.issue(scope());
        assert_eq!(book.resolve(&t).as_ref(), Some(&scope()));
        assert!(
            book.resolve("deadbeef").is_none(),
            "没签发过的令牌必须认不出来"
        );
    }

    #[test]
    fn revoking_takes_effect_immediately() {
        let book = SandboxTokens::default();
        let t = book.issue(scope());
        book.revoke(&t);
        assert!(
            book.resolve(&t).is_none(),
            "沙箱停掉后它那把令牌必须当场失效 —— 容器可能还活着（stop 之前的\
             最后一瞬），而它此刻已经不该再写记忆了"
        );
    }

    /// 未分组的键**恒等于 owner** —— 生产上那些卷靠这条不用迁移。
    #[test]
    fn 未分组的作用域键就是裸的_owner() {
        assert_eq!(
            scope().key(),
            "u1",
            "改掉这条等于让生产上每个用户的 cortex-ws-<owner> 卷当场失联：\
             容器会挂一个空卷起来，而用户看到的是「我的文件全没了」"
        );
        assert_ne!(
            in_project("u1", "p1").key(),
            scope().key(),
            "项目里的沙箱必须是另一个卷，否则这次改动等于没做"
        );
        assert_ne!(
            in_project("u1", "p1").key(),
            in_project("u1", "p2").key(),
            "两个项目之间也要分开 —— 这才是用户真正抱怨的那件事"
        );
    }

    /// 容器名会当 **DNS 名**用，所以它必须 ≤ 63 字节。
    ///
    /// 这条是补写的：第一版把两个 ULID 直接拼起来，得到 67 字节的容器名。
    /// 真机上的症状极具误导性 —— 容器 `Up (healthy)`、里面的 agent 日志写着
    /// 「本地 agent 已就绪」，而 cortexd 报「30 秒没应答，可能是崩了」。
    /// 实际是 `getent hosts <容器名>` 直接解析失败。
    ///
    /// 上限写死 63 而不是引用某个常量：这是 DNS 标签的**协议**上限
    /// （RFC 1035），不是我们选的。
    #[test]
    fn 作用域键拼出来的容器名不超过_dns_标签上限() {
        // 最长的现实输入：两个 26 字符的 ULID
        let worst = SandboxScope {
            owner: "01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            session_id: "s".into(),
            project: Some("01BX5ZZKBKACTAV9WEVGEMMVRZ".into()),
        };
        // 与 `sandbox_runner` 的前缀对齐。两处写歪的后果不同：那边加长了
        // 才会真的炸，所以这里取更长的那个（容器名）算
        let name_len = "cortex-sbx-".len() + worst.key().len();
        assert!(
            name_len <= 63,
            "容器名 {name_len} 字节，超过 DNS 标签上限 63。\
             症状不是「建容器失败」，而是容器起来了、健康检查也过了，\
             cortexd 却连它的名字都解析不出来 —— 报出来的是「30 秒没应答」"
        );
    }

    /// 同一个项目 id 每次都要算出**同一个**键。
    #[test]
    fn 项目哈希是确定的() {
        assert_eq!(
            in_project("u1", "p1").key(),
            in_project("u1", "p1").key(),
            "键不稳定的话，每一轮对话都会去找一个不存在的容器 —— \
             于是每轮建一个新的，旧的那些带着文件留在原地"
        );
    }

    /// 项目 id 由客户端给，但它**改不动别人的键**。
    #[test]
    fn 客户端给的项目_id_够不到别人的沙箱() {
        // 一个想撞到 u2 名下去的项目 id
        let evil = in_project("u1", "--p-x/../u2");
        assert!(
            evil.key().starts_with("u1"),
            "键必然以服务端解析出来的 owner 开头（current_user，不来自请求体），\
             所以无论项目 id 长成什么样都只能改动自己名下那一段"
        );
    }

    #[test]
    fn expired_tokens_are_swept_when_a_new_one_is_issued() {
        let book = SandboxTokens::default();
        {
            let mut g = book.inner.lock().unwrap();
            g.insert(
                "old".into(),
                (scope(), Instant::now() - TTL - Duration::from_secs(1)),
            );
        }
        assert_eq!(book.len(), 1);
        let fresh = book.issue(scope());
        assert_eq!(book.len(), 1, "过期令牌应当在签新令牌时被清掉");
        assert!(book.resolve(&fresh).is_some());
    }
}

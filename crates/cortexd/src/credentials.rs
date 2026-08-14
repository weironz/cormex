//! 账号凭据 —— 密码怎么存、refresh token 怎么校验、什么时候要把整条链子作废。
//!
//! 这个模块只有**纯逻辑**：不碰数据库、不碰 HTTP、不做租户隔离。
//! 它回答三个问题，每一个的错误答案都不会在测试里显形，只会在事后显形。
//!
//! # 一、为什么密码不能沿用 [`crate::auth`] 那套 SHA-256
//!
//! `auth.rs` 论证过「预共享 token 用 SHA-256 摘要就够，不需要慢 KDF」，
//! 那段论证是对的，但它**明写了一个前提**：token 是 256 位内核随机数。
//! 慢 KDF 防的是「低熵秘密被离线爆破」，而对高熵随机串来说，把每次尝试
//! 慢一百万倍，爆破时间只是从「宇宙年龄」变成「宇宙年龄」—— 白付一份依赖
//! 和每请求几十毫秒的 CPU。
//!
//! **人选的密码不满足那个前提。** 它的真实熵在二三十位量级，而且分布高度
//! 集中在几百万个已知串上。对这种秘密，单次尝试的成本**就是**唯一的防线：
//! 库被拖走之后，SHA-256 让攻击者一秒试几十亿个，argon2id 让他一秒试几千个。
//! 同一条判据（秘密是否高熵），两侧结论相反。
//!
//! 所以这个文件里会出现一个看起来自相矛盾的组合：
//!
//! | 秘密 | 谁选的 | 熵 | 存法 |
//! |---|---|---|---|
//! | 密码 | 人 | 低 | **argon2id**（慢，抗离线爆破） |
//! | refresh token | 内核 CSPRNG | 256 位 | **SHA-256**（快，够用） |
//!
//! 它不是不一致，是同一条判据的两侧。谁要是「顺手统一一下」，
//! 无论统一到哪边都是错的：统一到 SHA-256 是把密码库送人，
//! 统一到 argon2 是让每次刷新令牌都烧掉几十毫秒 CPU（而刷新比登录频繁得多）。
//!
//! # 二、refresh token 轮转与 family
//!
//! 每次用 refresh token 换新的 access token，这条 token 就作废并轮转出一个
//! 后继。一条轮转链叫一个 **family**。
//!
//! 这样做的意义全在于：**被轮转过的旧 token 再次出现 = 它泄露了**。
//! 正常客户端拿到后继之后不会再用旧的那条；会用旧的那条的只有两种人 ——
//! 拿到副本的攻击者，或者被攻击者抢先用掉后继的真用户。**分不清是哪一种，
//! 也不需要分清**：两种情形下正确的动作都是把整条 family 作废，让双方
//! 一起重新登录。见 [`judge_refresh`]。

// 本模块是账号体系的**纯逻辑那一半**，调用它的路由与仓储还没落地。
// cortexd 是 bin crate，`pub` 在这里不豁免 dead_code，于是在被接上之前
// 每一个导出项都是一条 `-D warnings` 下的编译错误。
//
// 这与 CLAUDE.md 里「优先让代码真的用起来，而不是加 allow」的要求相抵触，
// 而这里的实情是「马上就会用起来」—— 所以这条 allow 是**脚手架**：
// 路由一接上就该删掉它，删掉之后如果还有东西报 dead_code，
// 那就是真的没人用，该按那条规则处理。
//
// 没用 `#[expect(dead_code)]`（它会在不再需要时自动报警、自己催自己删除，
// 本该是更好的选择）：测试里这些项**全都被用到**，于是 `--all-targets`
// 编译测试目标时这条 expect 是「未兑现」的，反过来触发
// `unfulfilled_lint_expectations`。两个目标各要一种写法，只有 allow 两边都过。

use argon2::{Algorithm, Argon2, Params, Version};
use cortex_core::Id;
use password_hash::{PasswordHash, PasswordHasher as _, PasswordVerifier as _, SaltString};
use sha2::{Digest as _, Sha256};

// ══════════════════════════════════════════════════════════════
//  一、密码
// ══════════════════════════════════════════════════════════════

/// 密码最短长度。
///
/// 12 而不是 8：argon2id 把离线爆破从「每秒几十亿」压到「每秒几千」，
/// 这个倍率对 8 位密码不够 —— 常见的 8 位口令基本都在几亿条的字典里，
/// 每秒几千也就是几天。12 位把字典空间推到需要真正的算力预算。
///
/// **刻意没有任何组合规则**（必须含大写 / 数字 / 符号）。那类规则的实测效果是
/// 把人推向 `Password1!` 这种形状 —— 它满足全部规则，而且就在字典的最前面。
/// 长度是唯一一个「照做就真的多出熵」的要求，所以这里只要长度。
pub const MIN_PASSWORD_LEN: usize = 12;

/// 密码最长长度。
///
/// argon2 的开销与输入长度几乎无关，所以长密码本身不是 DoS —— 加这个上界
/// 是为了让「密码」这个字段的形状有限，不至于因为上游某处漏了 body 限制，
/// 就有一条把 100 MB 塞进哈希函数的路径。1 KiB 对任何 passphrase 都绰绰有余。
pub const MAX_PASSWORD_LEN: usize = 1024;

/// 盐的字节数。16 字节是 PHC 的推荐值，也是 [`SaltString`] 的 `RECOMMENDED_LENGTH`。
const SALT_BYTES: usize = 16;

/// 内存开销，单位 KiB —— 19 MiB。
///
/// # 为什么是 19 MiB，而不是「越大越安全」
///
/// 这是 OWASP 对 argon2id 给出的第一档推荐（m=19 MiB, t=2, p=1）。选它不是
/// 因为它是默认值，是因为**生产机器是 2 核 3.5 GB，上面已经跑着 19 个容器**。
///
/// 内存这一项其实是最便宜的：19 MiB 是**每次哈希瞬时**占用，几个并发登录
/// 也就几十 MB，用完即还。真正会出事的是 CPU —— 见 [`T_COST`]。
///
/// 往上调（比如 64 MiB）在这台机器上买到的抗 GPU 能力有限，却让「几个并发
/// 登录」变成「几百 MB 瞬时峰值」，而这台机器的内存本来就是被 19 个容器分完的。
const M_COST_KIB: u32 = 19 * 1024;

/// 迭代次数。
///
/// 与 `M_COST_KIB` 配套的 OWASP 推荐值。**实测**（release，开发机单核空闲）：
///
/// | 参数 | 单次 |
/// |---|---|
/// | m=19 MiB, t=2, p=1（本模块） | 16.5 ms |
/// | m=19 MiB, t=3, p=1 | 22.7 ms |
/// | m=64 MiB, t=3, p=1（OWASP 另一档） | 89.6 ms |
///
/// 生产那台 2 核、CPU 被 19 个容器分掉的机器上按 2–4 倍算，即 35–70 ms。
///
/// 所以这里其实**有余量**，t 调到 3 也还在可接受范围。没调，是因为多出来的
/// 那 6 ms 换到的抗爆破倍率只有 1.5 倍，而它会等比例放大下面这件事：
///
/// **登录本身就是最好的 DoS 放大器**。一个不需要任何凭据的攻击者，只要往
/// `/login` 灌错误密码，每条请求就白烧掉服务端一份 argon2 —— 在这台机器上
/// 每秒二三十条请求就能吃掉一个核（总共两个）。参数翻倍等于把他的放大倍率
/// 翻倍，而抗爆破只线性变好。
///
/// 真正该防这件事的是登录端点上的限流，不是把 KDF 调软（调软的代价由全体
/// 用户在拖库那天付）。**限流落地之后，这个常量才值得重新讨论。**
///
/// 注意 debug build 下这套参数要 400 ms 以上 —— 那是未优化的 argon2，
/// 不是参数选狠了。别照着开发机上跑测试的手感去调它。
const T_COST: u32 = 2;

/// 并行度。
///
/// **必须是 1，而且这一条比看上去要紧。** `argon2` crate 在没有开 `parallel`
/// feature 时，p>1 的那些 lane 是**顺序**算的 —— 也就是说把 p 调到 2 只会让
/// 单次哈希的墙钟时间翻倍，一点并行收益都没有，纯粹是自己给自己加了一倍
/// 的 DoS 放大倍率。
///
/// 就算真开了并行：这台机器只有 2 核。让一次登录吃掉两个核，意味着一个人
/// 登录期间整台机器（连同那 19 个容器）都在等。
const P_COST: u32 = 1;

/// 本部署的 argon2id 实例。
///
/// 每次调用现构造：`Argon2::new` 只是填几个 u32，没有预计算，
/// 缓存它换不到任何东西，却要引入一个 `OnceLock` 和「参数是什么时候定的」
/// 这个问题。
fn hasher() -> Argon2<'static> {
    // 参数是常量且已在上面逐条论证过合法性（m ≥ 8p、t ≥ 1、p ≥ 1），
    // 构造不出错。真出错说明有人改坏了上面的常量，此时崩在启动路径上
    // 远好过悄悄退化成弱参数
    let params = Params::new(M_COST_KIB, T_COST, P_COST, None)
        .expect("argon2 参数是编译期常量，构造失败说明本模块的常量被改坏了");
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
}

/// 把明文密码哈希成 PHC 字符串（`$argon2id$v=19$m=19456,t=2,p=1$<盐>$<摘要>`）。
///
/// 产出的是**自描述**的 PHC 串：参数和盐都编在里面。所以 [`verify_password`]
/// 不读上面那几个常量，只读这个串 —— 这意味着以后调高参数时，老密码照样能验，
/// 用户下次登录时再按新参数重算即可，不需要一次性迁移，也不需要在库里
/// 存一列「这行是用哪代参数算的」。
pub fn hash_password(plain: &str) -> cortex_core::Result<String> {
    check_password_shape(plain)?;

    // 盐直接取自内核 CSPRNG，而不是走 `SaltString::generate(&mut OsRng)`。
    //
    // 后者要 `password-hash` 打开 `rand_core/getrandom`，于是「盐是不是真随机」
    // 就取决于一条**跨 crate 的 feature 传递**能不能一直传对 —— 而 feature
    // 被谁关掉了这件事不会报错，只会让盐变弱，且没有任何症状。
    // `getrandom` 本来就是本 crate 的直接依赖（见 auth.rs 的同类理由）。
    let mut salt_bytes = [0u8; SALT_BYTES];
    getrandom::fill(&mut salt_bytes).expect("内核熵源不可用，拒绝用可预测的盐哈希密码");
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| cortex_core::CortexError::Other(anyhow::anyhow!("盐编码失败：{e}")))?;

    hasher()
        .hash_password(plain.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| cortex_core::CortexError::Other(anyhow::anyhow!("密码哈希失败：{e}")))
}

/// 校验明文密码。
///
/// # 为什么返回 `bool` 而不是 `Result`
///
/// 这里有三种失败：密码不对、PHC 串是坏的（截断 / 手改过 / 算法列不认识）、
/// 参数解析不了。**调用方对这三种的正确反应完全一样：拒绝登录。**
///
/// 而把它们分开报会造成两件事，都是坏的：
///
/// 1. 差别顺着 HTTP 响应漏出去。「密码错」与「这个账号的哈希串是坏的」
///    是两条不同的信息，后者等于告诉攻击者「这个账号存在，而且它的记录有问题」。
/// 2. 调用点上多一条 `?`，于是一条**损坏的哈希串会变成 500 而不是 401**。
///    500 会被重试、会进告警、会被人当成服务故障去查，而它实际上是一次
///    应当被静静拒绝的登录。
///
/// 所以差别在这里就地咽掉，只往日志里留一行（`debug` 级，不带任何密码材料）。
/// 绝不 panic —— 这个函数的输入有一半来自数据库，另一半来自公网。
#[must_use]
pub fn verify_password(plain: &str, phc: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc) else {
        // 这条不是「密码错了」，是这一行数据坏了。运维需要知道，用户不需要
        tracing::debug!("PHC 串无法解析 —— 该账号的密码哈希已损坏");
        return false;
    };

    // **算法必须由我们说了算，不能由被验的那个串说了算。**
    //
    // `PasswordVerifier` 的默认实现会照着 PHC 串里写的 `$argon2i$` / `$argon2d$`
    // 去重算并放行 —— 也就是说这个串自己指定用哪个算法验自己。本系统只产出
    // argon2id，所以出现别的算法只有两种可能：数据被人写过，或者哪天有人加了
    // 一条「兼容老哈希」的路径而没人复查。两种都该在这里当场停住。
    //
    // argon2i 抗 GPU 弱、argon2d 抗侧信道弱，id 是二者的混合 ——
    // 「验的时候顺着串走」等于把选择权交给攻击面那一侧。
    if argon2::Algorithm::try_from(parsed.algorithm) != Ok(argon2::Algorithm::Argon2id) {
        tracing::debug!(algorithm = %parsed.algorithm, "拒绝非 argon2id 的密码哈希");
        return false;
    }

    // 摘要与参数照样从串里读（那正是 PHC 自描述的意义，也是以后调高
    // M_COST_KIB 还能验老密码的原因）—— 被钉死的只有「用哪个算法」这一项
    hasher().verify_password(plain.as_bytes(), &parsed).is_ok()
}

/// 长度校验。
///
/// 单独一个函数是为了让注册路径能在**不做哈希**的情况下先拒掉 ——
/// 否则一个 4 字符的密码也要先烧掉 100 ms argon2 才被告知太短。
fn check_password_shape(plain: &str) -> cortex_core::Result<()> {
    // 按**字节**数而不是字符数。理由不是省事：熵是按信息量算的，而
    // `plain.chars().count()` 会把一个 emoji 算成 1，于是 12 个 emoji
    // （在真实键盘上是从一张面板里点 12 下）就满足了要求
    let len = plain.len();
    if len < MIN_PASSWORD_LEN {
        return Err(cortex_core::CortexError::Invalid(format!(
            "密码至少需要 {MIN_PASSWORD_LEN} 个字节。\
             这里不要求大小写 / 数字 / 符号的组合 —— 长度是唯一真正管用的那一项。"
        )));
    }
    if len > MAX_PASSWORD_LEN {
        return Err(cortex_core::CortexError::Invalid(format!(
            "密码最长 {MAX_PASSWORD_LEN} 个字节。"
        )));
    }
    Ok(())
}

// ══════════════════════════════════════════════════════════════
//  二、refresh token
// ══════════════════════════════════════════════════════════════

/// refresh token 的熵，单位字节。
///
/// 32 字节 = 256 位，与 `auth.rs::generate` 同一个数量级，理由也一样：
/// 这是整个「不用慢 KDF」论证的**前提**，不是一个可以为了短一点而调的旋钮。
pub const REFRESH_BYTES: usize = 32;

/// 签发一条 refresh token：`(明文, SHA-256 摘要的十六进制)`。
///
/// 明文只在这一刻存在，发给客户端；**落库的只有摘要**。所以数据库被拖走
/// 之后，里面的 refresh token 一条都不能直接拿来换 access token —— 服务端
/// 会先对收到的东西做一次 SHA-256，再按摘要去索引里查（见 [`refresh_digest`]）。
///
/// 摘要用 SHA-256 而不是 argon2：见模块文档第一节那张表。这一条与上面
/// [`hash_password`] 的选择相反，而它们出自同一条判据。
#[must_use]
pub fn generate_refresh() -> (String, String) {
    let mut buf = [0u8; REFRESH_BYTES];
    getrandom::fill(&mut buf).expect("内核熵源不可用，拒绝签发可预测的 refresh token");
    let plain = hex::encode(buf);
    let digest = refresh_digest(&plain);
    (plain, digest)
}

/// 明文 refresh token 的落库形态。
#[must_use]
/// # 为什么这里没有 `verify_refresh` / 常数时间比较
///
/// 一度有过，写得也对，但接线时发现**用不上**：`auth_tokens.token_sha256`
/// 是唯一索引，服务端拿明文算出摘要之后是**按摘要查行**，而不是「取一行
/// 出来再逐字节比」。比较发生在 B 树里，攻击者对着它测时间学不到东西 ——
/// 而「这个 token 存不存在」本来就写在响应码上。
///
/// 删掉而不是留着加 `#[allow(dead_code)]`：一段没人调用的安全代码会让
/// 下一个读它的人以为那条路径被保护着。
pub fn refresh_digest(plain: &str) -> String {
    hex::encode(Sha256::digest(plain.as_bytes()))
}

// ══════════════════════════════════════════════════════════════
//  三、重放判定
// ══════════════════════════════════════════════════════════════

/// 一条 refresh token 在库里的状态 —— [`judge_refresh`] 需要知道的全部。
///
/// 刻意**不含摘要、不含明文、不含用户**：判定与「这条 token 是不是真的」
/// 无关，后者靠按摘要查行完成，且必须先于判定发生。把两件事塞进
/// 一个函数，就会出现「摘要对不上也去作废 family」这种可以被任何人白嫖的
/// 拒绝服务 —— 攻击者只要瞎猜一个 token，就能把别人的会话踢下线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshRecord {
    /// 这条轮转链的 id。作废时要作废的是**它**，不是这一条。
    pub family: Id,
    /// 已经被用过一次，并且轮转出了后继。
    pub rotated: bool,
    /// 整条 family 已经被作废（本条或它的兄弟触发过一次重放检测）。
    pub revoked: bool,
    /// 绝对过期时刻已过。
    ///
    /// 是个**已经算好的布尔**而不是一个时间戳：判定是纯函数，
    /// 让它去读 `Utc::now()` 就等于让它不可测。
    pub expired: bool,
}

/// 为什么被拒 —— 只进日志与指标，**绝不进 HTTP 响应体**。
///
/// 对客户端来说三种拒绝没有区别，都是「重新登录」。分开告诉它，
/// 等于告诉攻击者「你手上这条是过期了，还是被人抢先用掉了」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 整条 family 早就死了。
    FamilyRevoked,
    /// 过了绝对有效期。
    Expired,
}

/// 判定结果。**三态，不能塌成布尔。**
///
/// 把 [`Self::Reject`] 与 [`Self::RevokeFamily`] 合并成一个「拒绝」，
/// 意味着检测到重放之后，攻击者手上那条**还没被轮转过的**后继 token
/// 依然有效 —— 也就是说泄露被检测到了，却什么也没发生。
/// 重放检测的全部价值就在于第三态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshVerdict {
    /// 接受：发新的 access token，把这条标记为已轮转并签出后继。
    Rotate,
    /// 拒绝这一次，family 不动。
    Reject(RejectReason),
    /// 拒绝，并且把整条 family 作废 —— 检测到重放。
    RevokeFamily { family: Id },
}

/// 判定一次 refresh 请求。**纯函数**：调用方已经确认过摘要对得上。
///
/// # 检查顺序是这个函数唯一的内容，且它不是任意的
///
/// ```text
/// revoked  ─► Reject(FamilyRevoked)   ← family 已死，不重复作废
/// rotated  ─► RevokeFamily            ← 重放！必须在 expired 之前判
/// expired  ─► Reject(Expired)
/// 否则     ─► Rotate
/// ```
///
/// ## 为什么 `rotated` 必须排在 `expired` 前面
///
/// 这是最容易写反的一处，而写反之后测试大概率照样绿。
///
/// 直觉会说「都过期了还查什么重放」。但轮转链上**每一次轮转都会签出一条
/// 有新有效期的后继**：一条 2020 年就该过期的旧 token，它的 family 现在
/// 完全可能还活着、还在被真用户用。此时有人拿着这条老掉牙的 token 来换 ——
/// 那正是「他有一份历史副本」的铁证，也正是必须把整条链子掐掉的时刻。
/// 先判 `expired` 就会把这个信号降级成一次平平无奇的「过期了，重新登录吧」，
/// 于是攻击者手上那条**没过期的**后继继续可用。
///
/// ## 为什么 `revoked` 排在最前，而且只是 Reject
///
/// family 已经作废了，再作废一次不会更安全，只会往一个 append-only 的库里
/// 多写一条毫无信息量的记录；更糟的是它会把「**新**检测到一次泄露」这个
/// 本该稀有的信号淹没在噪声里 —— 而这个信号是要用来给人报警的。
#[must_use]
pub fn judge_refresh(record: &RefreshRecord) -> RefreshVerdict {
    if record.revoked {
        return RefreshVerdict::Reject(RejectReason::FamilyRevoked);
    }
    if record.rotated {
        return RefreshVerdict::RevokeFamily {
            family: record.family,
        };
    }
    if record.expired {
        return RefreshVerdict::Reject(RejectReason::Expired);
    }
    RefreshVerdict::Rotate
}

#[cfg(test)]
mod tests {
    use super::*;

    // ───────────────────────── 密码 ─────────────────────────

    #[test]
    fn a_password_hash_is_phc_with_the_parameters_we_chose() {
        let phc = hash_password("correct horse battery staple").expect("合法密码应当能哈希");
        assert!(
            phc.starts_with("$argon2id$"),
            "必须是 argon2id（不是 argon2i / argon2d），实际拿到：{phc}"
        );
        assert!(
            phc.contains("v=19"),
            "必须是 argon2 v1.3（0x13）；老版本有已知弱点。实际：{phc}"
        );
        assert!(
            phc.contains(&format!("m={M_COST_KIB},t={T_COST},p={P_COST}")),
            "PHC 串里的参数必须与本模块论证过的常量一致，否则「以后能平滑调参」\
             这个性质不成立。实际：{phc}"
        );
    }

    #[test]
    fn the_same_password_never_hashes_to_the_same_string() {
        let a = hash_password("correct horse battery staple").expect("合法密码应当能哈希");
        let b = hash_password("correct horse battery staple").expect("合法密码应当能哈希");
        assert_ne!(
            a, b,
            "两次哈希同一个密码得到了相同的串 —— 说明盐不是随机的，\
             于是拖库之后可以直接按摘要分组找出「谁和谁用了同一个密码」"
        );
        // 但两条都能验回同一个密码
        assert!(verify_password("correct horse battery staple", &a));
        assert!(verify_password("correct horse battery staple", &b));
    }

    #[test]
    fn the_plaintext_password_never_appears_in_the_hash() {
        let phc = hash_password("hunter2-hunter2-hunter2").expect("合法密码应当能哈希");
        assert!(!phc.contains("hunter2"), "明文密码出现在了 PHC 串里：{phc}");
    }

    #[test]
    fn only_the_right_password_verifies() {
        let phc = hash_password("correct horse battery staple").expect("合法密码应当能哈希");
        assert!(
            verify_password("correct horse battery staple", &phc),
            "正确的密码必须通过"
        );
        assert!(
            !verify_password("correct horse battery stapl", &phc),
            "少一个字符的密码不能通过"
        );
        assert!(!verify_password("", &phc), "空密码不能通过");
        assert!(
            !verify_password("Correct horse battery staple", &phc),
            "只差大小写的密码不能通过 —— 校验必须是大小写敏感的"
        );
    }

    /// 坏掉的 PHC 串一律 `false`，一条都不许 panic。
    ///
    /// 这个函数的第二个参数来自数据库，第一个来自公网。任何一条 panic
    /// 都是一个「往登录接口发个特定请求就能把 worker 打挂」的洞。
    #[test]
    fn a_corrupt_hash_is_a_quiet_false_never_a_panic() {
        let good = hash_password("correct horse battery staple").expect("合法密码应当能哈希");
        let corrupt = [
            ("", "空串"),
            ("not a phc string at all", "根本不是 PHC 形状"),
            ("$argon2id$", "只有算法段"),
            ("$argon2id$v=19$m=19456,t=2,p=1$", "缺盐与摘要"),
            ("$bcrypt$v=19$m=1,t=1,p=1$c2FsdA$aGFzaA", "不认识的算法"),
            (
                "$argon2id$v=19$m=19456,t=2,p=1$!!!!$!!!!",
                "盐与摘要不是 b64",
            ),
            (&good[..good.len() - 5], "被截断的合法串"),
        ];
        for (phc, what) in corrupt {
            assert!(
                !verify_password("correct horse battery staple", phc),
                "损坏的 PHC 串（{what}）必须判否而不是通过：{phc:?}"
            );
        }
    }

    /// 用 argon2i / argon2d 算出来的串不能被当成 argon2id 放行。
    ///
    /// `PasswordVerifier` 的默认实现会照着串里写的算法去验，而我们只该接受
    /// argon2id —— argon2i 抗 GPU 弱，argon2d 抗侧信道弱。
    #[test]
    fn only_argon2id_is_accepted() {
        let salt = SaltString::encode_b64(&[7u8; SALT_BYTES]).expect("固定盐应当能编码");
        let params = Params::new(M_COST_KIB, T_COST, P_COST, None).expect("参数合法");
        let weak = Argon2::new(Algorithm::Argon2i, Version::V0x13, params)
            .hash_password(b"correct horse battery staple", &salt)
            .expect("argon2i 哈希应当能算出来")
            .to_string();
        assert!(weak.starts_with("$argon2i$"), "构造的样本应当是 argon2i");
        assert!(
            !verify_password("correct horse battery staple", &weak),
            "argon2i 的哈希串竟然被放行了 —— 算法降级攻击的入口"
        );
    }

    #[test]
    fn a_too_short_password_is_rejected_before_any_hashing() {
        let err = hash_password(&"a".repeat(MIN_PASSWORD_LEN - 1))
            .expect_err("比下限短一个字节的密码必须被拒");
        assert!(
            matches!(err, cortex_core::CortexError::Invalid(_)),
            "太短应当是「非法输入」而不是别的类别，实际：{err:?}"
        );
        assert!(
            hash_password(&"a".repeat(MIN_PASSWORD_LEN)).is_ok(),
            "刚好达到下限的密码必须被接受 —— 边界不能差一"
        );
        assert!(
            hash_password(&"a".repeat(MAX_PASSWORD_LEN + 1)).is_err(),
            "超过上限的密码必须被拒"
        );
    }

    /// 12 个 emoji 不算 12 个字节意义上的「长」，但按字节数它远超下限。
    /// 这条锁住的是「按字节而不是按 char 计长」这个选择本身。
    #[test]
    fn length_is_measured_in_bytes() {
        // 3 个汉字 = 9 字节 < 12，按 char 数（3）和按字节数（9）都该拒
        assert!(hash_password("密码短").is_err(), "9 字节的密码必须被拒");
        // 4 个汉字 = 12 字节：按字节够了，按 char 数只有 4
        assert!(
            hash_password("密码够长了").is_ok(),
            "15 字节的密码按字节计长应当通过"
        );
    }

    // ─────────────────────── refresh token ───────────────────────

    #[test]
    fn a_refresh_token_is_high_entropy_and_only_its_digest_is_stored() {
        let (plain, digest) = generate_refresh();
        assert_eq!(
            plain.len(),
            REFRESH_BYTES * 2,
            "明文应当是 {REFRESH_BYTES} 字节的十六进制"
        );
        assert_ne!(plain, digest, "落库的必须是摘要，不能是明文");
        assert_eq!(
            digest,
            refresh_digest(&plain),
            "摘要必须真的是明文的 SHA-256"
        );
        let (other, _) = generate_refresh();
        assert_ne!(plain, other, "两次签发不能撞上 —— 熵源坏了才会这样");
    }

    // ─────────────────────── 重放判定 ───────────────────────

    fn fresh() -> RefreshRecord {
        RefreshRecord {
            family: Id::new(),
            rotated: false,
            revoked: false,
            expired: false,
        }
    }

    #[test]
    fn a_fresh_token_rotates() {
        assert_eq!(
            judge_refresh(&fresh()),
            RefreshVerdict::Rotate,
            "没轮转过、没作废、没过期的 token 必须被接受并轮转"
        );
    }

    #[test]
    fn a_replayed_token_takes_down_the_whole_family() {
        let rec = RefreshRecord {
            rotated: true,
            ..fresh()
        };
        assert_eq!(
            judge_refresh(&rec),
            RefreshVerdict::RevokeFamily { family: rec.family },
            "一条已经轮转过的 token 再次出现 = 它泄露了。此时只「拒绝」是不够的：\
             攻击者手上那条没轮转过的后继依然有效，检测到了泄露却什么也没发生"
        );
    }

    /// 判定必须落在**这一条**的 family 上。
    ///
    /// 返回一个新造的 / 错的 family 就等于把别人的会话踢掉，而把真正泄露的
    /// 那条留着 —— 两个方向的错都发生了。
    #[test]
    fn the_revoked_family_is_the_one_from_the_record() {
        let mine = Id::new();
        let someone_else = Id::new();
        let rec = RefreshRecord {
            family: mine,
            rotated: true,
            ..fresh()
        };
        let RefreshVerdict::RevokeFamily { family } = judge_refresh(&rec) else {
            panic!("重放必须判 RevokeFamily");
        };
        assert_eq!(family, mine, "要作废的必须是这条 token 自己的 family");
        assert_ne!(family, someone_else, "绝不能作废到别人的 family 上");
    }

    /// **这条测试守的是检查顺序**，是本模块最容易写反的一处。
    ///
    /// 一条又过期又被轮转过的 token 出现，说明有人手上有历史副本；
    /// 而这条链上**现在**那条后继完全可能还没过期、还在被真用户用。
    /// 先判 `expired` 会把这个信号降级成一次平平无奇的「过期了」，
    /// 于是攻击者手上那条没过期的后继继续可用。
    #[test]
    fn an_expired_token_that_was_also_rotated_still_takes_down_the_family() {
        let rec = RefreshRecord {
            rotated: true,
            expired: true,
            ..fresh()
        };
        assert_eq!(
            judge_refresh(&rec),
            RefreshVerdict::RevokeFamily { family: rec.family },
            "又过期又被轮转过 —— 过期不能掩盖重放。轮转链上每次轮转都会签出\
             一条有新有效期的后继，所以「旧的过期了」完全不意味着「这条链死了」"
        );
    }

    #[test]
    fn a_merely_expired_token_is_rejected_without_touching_the_family() {
        let rec = RefreshRecord {
            expired: true,
            ..fresh()
        };
        assert_eq!(
            judge_refresh(&rec),
            RefreshVerdict::Reject(RejectReason::Expired),
            "只是过期（没被轮转过）说明用户单纯离开太久，不是泄露信号；\
             作废整条 family 只会让正常的「太久没用」变成一次莫名其妙的全端登出"
        );
    }

    /// 已死的 family 只 `Reject`，不再作废一次。
    ///
    /// 重复作废在 append-only 的库里就是永久噪声，而且会把「**新**检测到一次
    /// 泄露」这个本该稀有、本该报警的信号淹掉。
    #[test]
    fn an_already_revoked_family_is_not_revoked_again() {
        let rec = RefreshRecord {
            revoked: true,
            rotated: true,
            expired: true,
            ..fresh()
        };
        assert_eq!(
            judge_refresh(&rec),
            RefreshVerdict::Reject(RejectReason::FamilyRevoked),
            "family 已经死了，再作废一次不会更安全，只会往 append-only 的库里\
             写噪声，并把新的泄露告警淹没"
        );
    }

    /// 三态不能塌成布尔 —— 这条把「合并 Reject 与 RevokeFamily」直接钉死。
    #[test]
    fn reject_and_revoke_family_are_not_the_same_verdict() {
        let replayed = RefreshRecord {
            rotated: true,
            ..fresh()
        };
        let expired = RefreshRecord {
            expired: true,
            ..fresh()
        };
        assert_ne!(
            judge_refresh(&replayed),
            judge_refresh(&expired),
            "「拒绝」与「拒绝并作废整个 family」必须是两个不同的结果。\
             合并成一个等于：检测到泄露之后，攻击者还能继续用这条链上的其他 token"
        );
        assert!(
            matches!(
                judge_refresh(&replayed),
                RefreshVerdict::RevokeFamily { .. }
            ),
            "重放必须落到第三态上"
        );
    }
}

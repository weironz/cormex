//! 写入侧负过滤 —— 凭据在**进抽取之前**就被挡掉。
//!
//! 对应 [`docs/memory-content.md`] 差异清单 #4。此前这一层只有 extract prompt
//! 里的一句话（「凭据与密钥的字面值……不抽」），而那是**软约束**：
//! 模型看得见原文，且原文已经完整过了一遍网络。
//!
//! # 三条写死的规矩，逐条对应文档
//!
//! ## 一、阻断的是派生层，不是 L0
//!
//! `episodes` 是唯一真相来源，**不能有洞** —— 而密钥最常出现的地方恰恰
//! 就是对话正文。所以这里挡的是「进抽取、算 embedding、进检索索引」，
//! **不是「不写 episodes」**。原文照常落盘。
//!
//! 真要抹掉已落盘的原文，那是 `redact`，而它必须显式触发、二次确认、
//! 留墓碑（`docs/memory.md` §十一）。自动的写前销毁不在那条例外里。
//!
//! ## 二、必须排在 LLM 调用之前
//!
//! 这是 Mem0 的反面教训：它的过滤在抽取**层**，等于原文先完整发出去一趟
//! 再筛。所以这个函数的调用点是 [`crate::extract::Extractor::extract`]
//! 的第一行，而不是 `write_candidates`。
//!
//! ## 三、只做 A 级 —— 零歧义的那几类
//!
//! 邮箱、URL、泛型高熵串一概不管。文档里有实测数字：SecretBench 的
//! "Other" 类 **99.29% 是误报**，数据库/服务器 URL 的真阳性率只有 **1.6%**。
//! 把它们硬阻断，等于让正常的技术讨论大面积抽不出记忆 ——
//! 而那种损失是静默的，没有人会来报告「我的记忆少了」。
//!
//! # 为什么是**整轮阻断**，不是把密钥涂黑后继续抽
//!
//! 涂黑更「不浪费」，但它把正确性押在正则的完整性上：模式少匹配一位，
//! 留在文本里的就是密钥的一部分，而它会照常进模型、进 embedding、
//! 进索引 —— 一个**看起来生效了**的过滤器。
//!
//! 整轮阻断的失败方向是相反的：最坏情况是少抽了一轮的事实，
//! 而这件事有日志、可解释、可事后补抽。A 级本来就是「宁可不要」的那一档。

use std::sync::LazyLock;

use regex::Regex;

/// 命中了哪条规则。
///
/// **刻意只有规则名，不带匹配到的内容。** 这个类型的唯一消费者是日志，
/// 而把匹配片段打进日志，就是把密钥从「只在对话正文里」搬到「还在
/// 容器日志里」—— 后者会被整段贴进工单、CI 输出和聊天窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    /// PEM 私钥块
    PrivateKey,
    /// 带厂商前缀的 key（AWS / GitHub / Slack / Stripe / OpenAI / Google …）
    VendorKey,
    /// `Authorization: Bearer …`
    BearerHeader,
    /// JWT（三段式，payload 常含身份信息）
    Jwt,
    /// `.env` 形式的赋值，且键名点名了这是凭据
    EnvAssignment,
}

impl Rule {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::PrivateKey => "private_key",
            Self::VendorKey => "vendor_key",
            Self::BearerHeader => "bearer_header",
            Self::Jwt => "jwt",
            Self::EnvAssignment => "env_assignment",
        }
    }
}

/// PEM 私钥块。正则零歧义，信噪比最高的一条。
static PRIVATE_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-----BEGIN [A-Z ]*PRIVATE KEY-----").expect("常量正则"));

/// 带厂商前缀的 key。SecretBench 上这一类的命中率 87.7%。
///
/// 每一条都要求前缀 + 一段够长的字符集：只匹配前缀会让「我们用的是 AWS」
/// 这种句子被误杀，而前缀本身对判据零贡献。
static VENDOR_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"AKIA[0-9A-Z]{16}",               // AWS access key id
        r"|gh[pousr]_[A-Za-z0-9]{20,}",    // GitHub PAT / OAuth / server / refresh
        r"|xox[abposr]-[A-Za-z0-9-]{10,}", // Slack
        r"|sk_live_[A-Za-z0-9]{16,}",      // Stripe（live，test 的不管）
        r"|sk-[A-Za-z0-9_-]{20,}",         // OpenAI / Anthropic 家族
        r"|AIza[0-9A-Za-z_-]{35}",         // Google API key
        r"|glpat-[A-Za-z0-9_-]{16,}",      // GitLab PAT
    ))
    .expect("常量正则")
});

/// `Authorization: Bearer …`。
static BEARER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)authorization\s*:\s*bearer\s+[A-Za-z0-9._~+/=-]{8,}").expect("常量正则")
});

/// JWT 三段式。payload 段几乎总是 `eyJ`（`{"` 的 base64）。
static JWT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"eyJ[A-Za-z0-9_-]{8,}\.eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}").expect("常量正则")
});

/// `.env` 形式赋值。捕获组 1 = 键名，组 2 = 值。
///
/// 编码 agent 最高频的误入路径，本仓库自己就有过先例。
static ENV_ASSIGN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?im)^[ \t]*(?:export[ \t]+)?([A-Za-z][A-Za-z0-9_]*)[ \t]*=[ \t]*['\x22]?([^\s'\x22]+)",
    )
    .expect("常量正则")
});

/// 键名里出现这些**完整段**（按 `_` 切）就认定是凭据。
///
/// 要求「完整段」而不是子串，挡的是一类真实误报：`KEY` 作为子串会命中
/// `MONKEY`、`KEYBOARD`、`KEYWORDS`；而 `TOKENS`（复数）与 `TOKEN` 是
/// 不同的段 —— 于是 `MAX_TOKENS=100000` 不会被当成凭据。
const SECRET_SEGMENTS: &[&str] = &[
    "SECRET",
    "SECRETS",
    "PASSWORD",
    "PASSWD",
    "PWD",
    "TOKEN",
    "APIKEY",
    "CREDENTIAL",
    "CREDENTIALS",
];

/// 键名里出现这两段相邻时也算（`API_KEY` / `PRIVATE_KEY` / `ACCESS_KEY`）。
///
/// 单独一个 `KEY` 段不够：`SORT_KEY`、`PRIMARY_KEY`、`CACHE_KEY` 在技术讨论里
/// 遍地都是，而它们后面跟的是列名，不是凭据。
const KEY_QUALIFIERS: &[&str] = &["API", "PRIVATE", "ACCESS", "SECRET", "AUTH", "SIGNING"];

/// 扫一遍文本，返回**第一条**命中的规则。
///
/// 不返回全部命中：调用方要做的决定只有「抽不抽」，而多列几条规则名
/// 只会让日志更长。第一条足够定位。
#[must_use]
pub fn scan(text: &str) -> Option<Rule> {
    if PRIVATE_KEY.is_match(text) {
        return Some(Rule::PrivateKey);
    }
    if VENDOR_KEY.is_match(text) {
        return Some(Rule::VendorKey);
    }
    if BEARER.is_match(text) {
        return Some(Rule::BearerHeader);
    }
    if JWT.is_match(text) {
        return Some(Rule::Jwt);
    }
    for c in ENV_ASSIGN.captures_iter(text) {
        let (Some(key), Some(value)) = (c.get(1), c.get(2)) else {
            continue;
        };
        if is_secret_key(key.as_str()) && looks_assigned(value.as_str()) {
            return Some(Rule::EnvAssignment);
        }
    }
    None
}

/// 这个键名是不是在说「我后面跟的是凭据」。
fn is_secret_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    let segs: Vec<&str> = upper.split('_').filter(|s| !s.is_empty()).collect();
    if segs.iter().any(|s| SECRET_SEGMENTS.contains(s)) {
        return true;
    }
    // `..._KEY` 只在前面那一段够具体时才算
    segs.windows(2)
        .any(|w| w[1] == "KEY" && KEY_QUALIFIERS.contains(&w[0]))
}

/// 值看起来像**被赋了一个真东西**，而不是一个数字或占位符。
///
/// 至少一个字母：`TOKEN_LIMIT=4096`、`MAX_RETRIES=3` 这类配置项即便键名
/// 撞上了也不该阻断整轮抽取。密码是字母数字混排的，纯数字的那个不是密码。
///
/// 占位符单独排除：文档与示例里 `PASSWORD=<your-password>` 这种写法极常见，
/// 把它当凭据会让「怎么配置」这类对话永远抽不出记忆。
fn looks_assigned(value: &str) -> bool {
    if !value.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    let v = value.trim_matches(['<', '>', '{', '}', '$', '(', ')']);
    let lower = v.to_ascii_lowercase();
    const PLACEHOLDERS: &[&str] = &[
        "your",
        "yourpassword",
        "xxx",
        "xxxx",
        "changeme",
        "placeholder",
        "redacted",
        "example",
        "todo",
        "fixme",
        "null",
        "none",
        "empty",
    ];
    if PLACEHOLDERS
        .iter()
        .any(|p| lower == *p || lower.starts_with(p))
    {
        return false;
    }
    // `***`、`…` 之类涂黑后的残留
    !v.chars().all(|c| c == '*' || c == '.' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 把一个合成密钥拼起来。**源码里绝不留一段连续的、像钥匙的文本。**
    ///
    /// 这些串全是编的，但**形状是真的** —— 真到会被扫描器当成真钥匙。
    /// 本仓库就撞过一次：GitHub 的推送保护把下面那个假 Stripe key 拦下来，
    /// 整个 push 被拒。gitleaks、trufflehog 之类同理。
    ///
    /// 所以前缀与主体分开写，拼接发生在运行时。测试要的只是形状，
    /// 而形状在拼完之后才出现。
    fn synthetic(prefix: &str, body: &str) -> String {
        format!("{prefix}{body}")
    }

    /// 每一类 A 级凭据都要被挡住。
    #[test]
    fn every_a_level_credential_is_blocked() {
        let vendor_keys = [
            synthetic("AKIA", "IOSFODNN7EXAMPLE"),
            synthetic("ghp", "_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"),
            synthetic("xoxb", "-1234567890-abcdefghijkl"),
            synthetic("sk", "_live_aBcDeFgHiJkLmNoPqRsTuVwX"),
            synthetic("sk", "-proj-aBcDeFgHiJkLmNoPqRsTuVwXyZ01"),
            synthetic("AIza", "SyA1234567890abcdefghijklmnopqrstuv"),
        ];
        for k in &vendor_keys {
            assert_eq!(
                scan(&format!("我的 key 是 {k}")),
                Some(Rule::VendorKey),
                "没挡住厂商前缀 key —— 这段会原样发给抽取模型"
            );
        }

        let bearer = synthetic("Authorization: Bearer ", "abcdef1234567890");
        assert_eq!(
            scan(&format!("curl -H '{bearer}' https://x")),
            Some(Rule::BearerHeader)
        );

        let cases: &[(&str, Rule)] = &[
            (
                "帮我看看这个\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow…\n",
                Rule::PrivateKey,
            ),
            (
                "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA…",
                Rule::PrivateKey,
            ),
            (
                "token 是 eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
                Rule::Jwt,
            ),
            ("CORTEXD_TOKEN=s3cr3tvalue123", Rule::EnvAssignment),
            ("export DB_PASSWORD=hunter2", Rule::EnvAssignment),
            ("CORTEX_EMBED_API_KEY=abc123def456", Rule::EnvAssignment),
        ];
        for (text, want) in cases {
            assert_eq!(
                scan(text),
                Some(*want),
                "没挡住：{text}\n—— 这段会原样发给抽取模型，再进 embedding 与索引"
            );
        }
    }

    /// **正常的技术讨论一条都不能被误杀。**
    ///
    /// 这条比上面那条更重要。漏掉一个凭据是可见的（真出事时查得到），
    /// 而误杀是**静默**的：用户只会觉得「这段聊完好像没记住什么」，
    /// 不会有任何人来报告。文档里那两个数字说的就是这件事 ——
    /// SecretBench "Other" 类 99.29% 是误报，DB/服务器 URL 真阳性率只有 1.6%。
    #[test]
    fn ordinary_technical_talk_is_never_killed() {
        let benign = [
            "我们用 AWS 的 S3，bucket 叫 cortex-blobs",
            "GitHub Actions 里配一个 secrets.GITHUB_TOKEN 就行", // 说的是引用，不是字面值
            "数据库连到 postgres://localhost:5442/cortex",
            "MAX_TOKENS=100000 这个上限要调高",
            "TOKEN_LIMIT=4096",
            "RUST_LOG=cortexd=info,warn",
            "CORTEX_BACKEND=mock",
            "PRIMARY_KEY 用 ULID，SORT_KEY 用时间戳",
            "缓存的 CACHE_KEY 是 session_id 拼上日期",
            "把 PASSWORD=<your-password> 换成你自己的",
            "DB_PASSWORD=***",
            "这个 commit 的 sha 是 a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0",
            "文件哈希 sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "他的邮箱是 zhang@example.com，电话 13800138000",
            "我们决定生产用 Postgres 17 作为数据库",
        ];
        for text in benign {
            assert_eq!(
                scan(text),
                None,
                "误杀了正常内容：{text}\n—— 这一轮的记忆会静默地抽不出来，而没有人会来报告"
            );
        }
    }

    /// 键名的分段判定：要求完整段，不是子串。
    #[test]
    fn key_names_are_matched_by_whole_segments() {
        assert!(is_secret_key("CORTEXD_TOKEN"));
        assert!(is_secret_key("token"));
        assert!(is_secret_key("AWS_SECRET_ACCESS_KEY"));
        assert!(is_secret_key("MY_API_KEY"));
        assert!(is_secret_key("SSH_PRIVATE_KEY"));

        assert!(!is_secret_key("MAX_TOKENS"), "复数是另一个段，不是 TOKEN");
        assert!(!is_secret_key("MONKEY"), "KEY 作为子串会命中它");
        assert!(!is_secret_key("KEYBOARD_LAYOUT"));
        assert!(!is_secret_key("PRIMARY_KEY"), "列名，不是凭据");
        assert!(!is_secret_key("SORT_KEY"));
        assert!(!is_secret_key("CACHE_KEY"));
    }

    /// 值的判定：纯数字与占位符都不算。
    #[test]
    fn a_value_must_look_like_it_was_actually_set() {
        assert!(looks_assigned("hunter2"));
        assert!(looks_assigned("s3cr3t"));

        assert!(!looks_assigned("100000"), "纯数字不是密码");
        assert!(!looks_assigned("<your-password>"), "文档占位符");
        assert!(!looks_assigned("changeme"));
        assert!(!looks_assigned("***"), "已经被涂黑过的残留");
        assert!(!looks_assigned("..."));
    }

    /// 规则名进日志，**匹配内容绝不进**。
    ///
    /// 这条守的是一个很容易在「顺手把上下文也打出来方便排查」时被破坏的
    /// 约束。真那么做了，密钥就从「只在对话正文里」变成「还在容器日志里」，
    /// 而容器日志会被整段贴进工单和聊天窗口。
    #[test]
    fn the_rule_carries_a_name_and_nothing_else() {
        let hit = scan("AKIAIOSFODNN7EXAMPLE").expect("应当命中");
        assert_eq!(hit.name(), "vendor_key");
        // Rule 是个无字段枚举 —— 它在类型上就带不了内容。
        // 下面这行断言的是「Debug 输出里也没有」，因为日志经常用 {:?}
        assert!(
            !format!("{hit:?}").contains("AKIA"),
            "Debug 输出泄露了匹配内容"
        );
    }
}

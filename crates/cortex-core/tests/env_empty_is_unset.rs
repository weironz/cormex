//! 环境变量设成**空串**时，必须走默认值，不能把空串当成用户的选择。
//!
//! 单独一个文件、单独一个测试函数：`env::set_var` 改的是进程全局状态，
//! 和别的测试同二进制并行跑会互相踩。（`crates/cortex-agent/tests/
//! sandbox_degraded.rs` 是同一个理由。）
//!
//! # 这条测试挡的是什么
//!
//! `env::var(k)` 对「设成空串」返回 `Ok("")` 而不是 `Err`。所以
//! `env::var(k).unwrap_or(default)` 里的默认值**永远不会生效**——
//! 只要那个变量被设过，哪怕设成空。
//!
//! 这个 bug 在本地开发时不可见：`.env` 的写法是「不写这一行」。
//! 它只在容器里显形 —— docker compose 的
//! `CORTEX_LLM_MODEL: ${CORTEX_LLM_MODEL:-}` 在 `.env` 缺这一项时
//! 会把变量**设成空串**。第一次上生产就是这么炸的：进程正常启动、
//! `/health` 返回 ok、日志一切正常，直到发出第一条真实对话才收到
//! DeepSeek 的 `... but you passed .`。

use cortex_core::Config;

/// 一条用例：环境变量名、从 [`Config`] 里把它取出来的取值器、期望的默认值。
type Getter = for<'a> fn(&'a Config) -> &'a str;
type Case = (&'static str, Getter, &'static str);

/// 这些 key 的共同点：空串都不是合法取值。
/// 真有「空是合法值」的配置项时，它不该走 `optional`。
const CASES: &[Case] = &[
    ("CORTEXD_BIND", |c| &c.bind, "127.0.0.1:8080"),
    ("S3_BUCKET", |c| &c.s3.bucket, "cortex-blobs"),
    ("S3_REGION", |c| &c.s3.region, "us-east-1"),
    ("CORTEX_LLM_MODEL", |c| &c.llm.model, "deepseek-v4-pro"),
    (
        "CORTEX_LLM_CHEAP_MODEL",
        |c| &c.llm.cheap_model,
        "deepseek-v4-flash",
    ),
    ("CORTEX_DEVICE_ID", |c| &c.device_id, "dev-local"),
];

#[test]
fn empty_env_falls_back_to_default() {
    // SAFETY: 本文件只有这一个测试函数，没有别的线程读写这些变量。
    unsafe {
        std::env::set_var("DATABASE_URL", "postgres://u:p@127.0.0.1:5432/x");
        std::env::set_var("CORTEX_LLM_PROVIDER", "deepseek");
        for (key, _, _) in CASES {
            std::env::set_var(key, "");
        }
        // 空白也算空：`FOO=  ` 与 `FOO=` 在 compose 里是一回事
        std::env::set_var("CORTEX_DEVICE_ID", "   ");
    }

    let cfg = Config::from_env().expect("全部设成空串时也该能加载（走默认值）");

    for (key, get, want) in CASES {
        assert_eq!(
            get(&cfg),
            *want,
            "{key} 被设成空串时应回落到默认值 {want}，实际拿到 {:?} —— \
             env::var 对空串返回 Ok(\"\")，默认值被顶掉了",
            get(&cfg)
        );
    }

    // ── required 那一侧：空串应当报「缺少」，而不是放一个空串过去 ──
    // 否则 DATABASE_URL="" 会一路走到 sqlx 才炸，错误信息面目全非
    unsafe { std::env::set_var("DATABASE_URL", "") };
    let err = Config::from_env().expect_err("DATABASE_URL 为空串时应当报错");
    assert!(
        err.to_string().contains("DATABASE_URL"),
        "空的 DATABASE_URL 应当报「缺少环境变量 DATABASE_URL」，实际是：{err}"
    );
}

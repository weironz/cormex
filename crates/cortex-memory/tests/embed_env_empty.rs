//! `CORTEX_EMBED_*` 设成**空串**时必须按「没设」处理。
//!
//! 单独一个文件、单独一个测试函数：`env::set_var` 改的是进程全局状态，
//! 与别的测试同二进制并行跑会互相踩。（`cortex-core/tests/env_empty_is_unset.rs`
//! 与 `cortex-agent/tests/sandbox_degraded.rs` 是同一个理由。）
//!
//! # 这条测试挡的是什么 —— 一个差点上了生产的故障
//!
//! `deploy/docker-compose.yml` 里写的是
//! `CORTEX_EMBED_BATCH: ${CORTEX_EMBED_BATCH:-}`。`.env` 里没这一项时，
//! compose 把它**设成空串**而不是不设，而 `env::var` 对空串返回 `Ok("")`。
//!
//! `FastEmbedderConfig::from_env` 原来是裸 `if let Ok(v) = env::var(...)`，
//! 于是 `"".parse::<usize>()` 失败 → 返回配置错误 → **cortexd 拒绝启动**，
//! 而运维那侧根本没配过这个变量。
//!
//! 同一个 crate 里 `embed_api::from_env` **有**这个处理、`embed_local` 没有 ——
//! 两份实现对同一个坑给出了两种答案，而只有一份是对的。
//!
//! 这是这个坑第二次出现（第一次是 `CORTEX_LLM_MODEL`，写在 0.1.1 的
//! CHANGELOG 里）。所以这条测试守的不是一个函数，是一类错误。

#![cfg(feature = "local-embed")]

use cortex_memory::embed_local::FastEmbedderConfig;

/// 空串必须等价于「没设」，对三个变量都成立。
///
/// 与「不设」的结果逐字段比对，而不是只断言 `is_ok()` ——
/// 后者在「空串被当成合法取值并悄悄改了某个字段」时照样绿。
#[test]
fn empty_strings_are_treated_as_unset() {
    // 先取一份基准：三个变量都不设时的样子
    for k in [
        "CORTEX_EMBED_MODEL",
        "CORTEX_MODEL_CACHE",
        "CORTEX_EMBED_BATCH",
    ] {
        unsafe { std::env::remove_var(k) };
    }
    let baseline = FastEmbedderConfig::from_env().expect("三个变量都不设时必须能构造");

    // 再把三个都设成空串 —— 这正是 compose 的 `${VAR:-}` 干的事
    for k in [
        "CORTEX_EMBED_MODEL",
        "CORTEX_MODEL_CACHE",
        "CORTEX_EMBED_BATCH",
    ] {
        unsafe { std::env::set_var(k, "") };
    }
    let with_empties = FastEmbedderConfig::from_env().unwrap_or_else(|e| {
        panic!(
            "空串把配置顶坏了，cortexd 会拒绝启动 —— 而运维根本没配过这些变量。\
             错误：{e}"
        )
    });

    assert_eq!(
        with_empties.backfill_batch, baseline.backfill_batch,
        "CORTEX_EMBED_BATCH 空串没有按「没设」处理"
    );
    assert_eq!(
        with_empties.cache_dir, baseline.cache_dir,
        "CORTEX_MODEL_CACHE 空串没有按「没设」处理 —— \
         它决定去哪儿找 560MB 权重，指错了会去 Hugging Face 现下"
    );
    assert_eq!(
        format!("{:?}", with_empties.model),
        format!("{:?}", baseline.model),
        "CORTEX_EMBED_MODEL 空串没有按「没设」处理"
    );

    // 纯空白也算空 —— `.env` 里一行 `CORTEX_EMBED_BATCH= ` 是真实存在的写法
    unsafe { std::env::set_var("CORTEX_EMBED_BATCH", "   ") };
    let with_blank = FastEmbedderConfig::from_env().expect("纯空白也必须按「没设」处理");
    assert_eq!(with_blank.backfill_batch, baseline.backfill_batch);

    // 真给了值时仍然要生效 —— 别把「宽容空串」做成「忽略这个变量」
    unsafe { std::env::set_var("CORTEX_EMBED_BATCH", "7") };
    let with_value = FastEmbedderConfig::from_env().expect("给了合法值应当成功");
    assert_eq!(
        with_value.backfill_batch, 7,
        "宽容空串不等于忽略这个变量 —— 真设了值必须生效"
    );

    // 非法值仍然要**响亮失败**。宽容空串不该顺带把打错的值也放过去：
    // `CORTEX_EMBED_BATCH=64条` 悄悄退回默认值，比报错难查得多
    unsafe { std::env::set_var("CORTEX_EMBED_BATCH", "abc") };
    let err = FastEmbedderConfig::from_env().expect_err("非法取值必须报错，不能悄悄回落到默认值");
    assert!(
        err.to_string().contains("CORTEX_EMBED_BATCH"),
        "报错必须点名是哪个变量，实际：{err}"
    );

    for k in [
        "CORTEX_EMBED_MODEL",
        "CORTEX_MODEL_CACHE",
        "CORTEX_EMBED_BATCH",
    ] {
        unsafe { std::env::remove_var(k) };
    }
}

//! 负过滤必须排在 **LLM 调用之前**，而不只是「存在」。
//!
//! `redaction::scan` 自己的单测（`src/redaction.rs`）只证明它认得出凭据。
//! 它认得出，但**挂错了位置**的话，一切照常绿：原文仍然会完整地发出去
//! 一趟，只是抽出来的事实被丢掉而已。而那正是 `memory-content.md` §七
//! 原则二点名的 Mem0 的做法 —— 「过滤在抽取层，原文已经完整过了一遍网络」。
//!
//! # 怎么证明「之前」
//!
//! 用一个**假 key** 造的 `Extractor`：
//!
//! - 含凭据的文本 → 必须**立刻**返回 `Ok(空)`。没有网络往返，
//!   所以假 key 根本不会被用到。
//! - 正常文本 → 必须**尝试出网**（拿到 401，或者在无网环境里连接失败）。
//!
//! 两者的差别只能由「过滤器在网络调用之前就短路了」解释。
//! 把 `scan` 那段挪到 `write_candidates`，第一条断言当场变红。
//!
//! 不连数据库：`extract()` 只需要 LLM，走 `ingest` 反而会把这条测试
//! 绑死在有没有 Postgres 上。

use std::sync::Arc;
use std::time::Duration;

use cortex_core::Id;
use cortex_core::config::LlmConfig;
use cortex_llm::LlmClient;
use cortex_memory::embed::HashEmbedder;
use cortex_memory::extract::{ExtractContext, Extractor};

fn offline_extractor() -> Extractor {
    let cfg = LlmConfig {
        provider: "deepseek".to_string(),
        model: "deepseek-v4-pro".to_string(),
        cheap_model: "deepseek-v4-flash".to_string(),
        base_url: None,
    };
    // 刻意是个假 key：这条测试的全部意义就是「被挡住的那条路根本不会用到它」
    let llm = LlmClient::from_config(&cfg, "not-a-real-key").expect("应能离线构造");
    Extractor::new(llm, Arc::new(HashEmbedder::new()), "test-device")
}

fn ctx() -> ExtractContext {
    ExtractContext::new(Id::new(), chrono::Utc::now())
}

#[tokio::test]
async fn a_credential_short_circuits_before_any_network_call() {
    let extractor = offline_extractor();

    // 一秒是很宽的上界：这条路上除了几个正则什么都没有。
    // 真发出去一趟的话，光 DNS + TLS 就不止这个数
    let out = tokio::time::timeout(
        Duration::from_secs(1),
        extractor.extract(
            "用户：这个 key 还能用吗 AKIAIOSFODNN7EXAMPLE\n助手：我看看。",
            &ctx(),
        ),
    )
    .await
    .expect("含凭据时不该有任何网络往返 —— 超时说明它已经发出去了");

    assert_eq!(
        out.expect("过滤命中是正常路径，不是错误").len(),
        0,
        "含凭据的一轮必须不产出任何候选事实"
    );
}

#[tokio::test]
async fn ordinary_text_really_does_reach_the_provider() {
    let extractor = offline_extractor();

    // 反面对照。没有这一条，上面那条测试在「extract 永远返回空」的实现下
    // 也会是绿的 —— 而那种实现等于把整个抽取管线关掉了
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        extractor.extract(
            "用户：我们生产库定了用 Postgres 17。\n助手：记下了。",
            &ctx(),
        ),
    )
    .await;

    match out {
        // 假 key → 供应商 401；无网环境 → 连接失败。两者都证明它**试图**出网
        Ok(Err(_)) => {}
        Ok(Ok(candidates)) => panic!(
            "假 key 竟然抽出了 {} 条候选 —— 说明正常文本这条路也被短路了，\
             负过滤误伤了它",
            candidates.len()
        ),
        // 超时同样说明它在等一个网络响应，也就是确实走到了调用那一步
        Err(_) => {}
    }
}

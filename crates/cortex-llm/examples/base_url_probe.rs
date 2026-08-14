//! 证明 `CORTEX_LLM_BASE_URL` 真的把请求打到了别的地方。
//!
//! 不是单元测试：要的就是一次**真的 HTTP 出站**，而那正是单元测试造不出来的
//! 那一段 —— 一个只检查结构体字段的测试，在 goose 忽略了这个字段时照样是绿的。
//!
//! 用法：`cargo run -p cortex-llm --example base_url_probe -- http://127.0.0.1:9099`
use cortex_core::config::LlmConfig;
use cortex_llm::{LlmClient, Message};
use futures::StreamExt as _;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let base = std::env::args()
        .nth(1)
        .expect("用法：base_url_probe <base_url>");
    let cfg = LlmConfig {
        provider: "openai".into(),
        model: "my-local-model".into(),
        cheap_model: "my-local-model".into(),
        base_url: Some(base.clone()),
    };
    let client = LlmClient::from_config(&cfg, "sk-not-checked")?;
    let model = client.model().clone();
    match client
        .stream_with(
            &model,
            "你是助手",
            &[Message::user().with_text("你好")],
            &[],
        )
        .await
    {
        Ok(mut s) => {
            while let Some(item) = s.next().await {
                match item {
                    Ok((m, _)) => println!("收到：{m:?}"),
                    Err(e) => println!("流内错误：{e}"),
                }
            }
        }
        Err(e) => println!("调用失败（但请求可能已经打出去了）：{e}"),
    }
    Ok(())
}

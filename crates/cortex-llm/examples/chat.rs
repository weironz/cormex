//! 供应商层冒烟测试：向配置好的模型问一句话，把流式增量逐块打印出来。
//!
//! 跑之前把密钥写进项目根的 `.env`（该文件已被 .gitignore 排除）：
//!
//! ```text
//! DEEPSEEK_API_KEY=sk-...
//! CORTEX_LLM_PROVIDER=deepseek
//! CORTEX_LLM_CHEAP_MODEL=deepseek-v4-flash
//! ```
//!
//! 然后：`cargo run -p cortex-llm --example chat`
//!
//! 每块增量之间打一个 `|`，肉眼就能确认是真流式而不是收完再吐。

use std::io::Write;

use cortex_llm::{LlmClient, Message};
use futures::StreamExt;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 从项目根往上找 .env；example 的工作目录就是 workspace 根，直接找得到。
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("提示：未加载 .env（{e}），改用进程环境变量");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cortex_llm=debug,warn".into()),
        )
        .init();

    let client = LlmClient::from_env()?;
    // 冒烟测试用廉价模型，别烧主模型的额度。
    let model = client.cheap_model();
    println!(
        "供应商 {} / 模型 {}（上下文上限 {}）",
        client.provider_id(),
        model.model_name,
        model.context_limit()
    );
    println!("─────────────────────────────────────────");

    let messages = [Message::user().with_text("用一句话介绍你自己")];
    let mut stream = client
        .stream_text(model, "你是 Cortex 的助手，回答简洁。", &messages)
        .await?;

    let mut chunks = 0usize;
    while let Some(chunk) = stream.next().await {
        print!("{}|", chunk?);
        // 不 flush 的话终端会攒到换行才显示，看不出流式效果。
        std::io::stdout().flush()?;
        chunks += 1;
    }

    println!("\n─────────────────────────────────────────");
    println!("共 {chunks} 个增量块");
    anyhow::ensure!(chunks > 1, "只收到 {chunks} 块，流式没生效");
    Ok(())
}

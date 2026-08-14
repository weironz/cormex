//! 多模态冒烟测试：把一张图真的发出去，看两件事。
//!
//! 1. **本地闸门**：不支持 vision 的模型（DeepSeek）在发请求之前就被
//!    [`LlmClient::ensure_can_see`] 拦下，错误里点名是哪个模型、怎么改；
//! 2. **线上行为**：绕过闸门直接走 goose 的 `Provider`，证明消息**构造是对的**
//!    （goose 按 engine 转成了各家的图像格式），并把远端的真实回复或
//!    真实错误原样打出来。
//!
//! 第 2 步刻意用 `client.provider()` 而不是 `client.complete()` —— 后者会被
//! 我们自己的闸门挡住，那样就永远看不到远端到底怎么回应。这条路只属于本
//! example，产品代码不该这么写。
//!
//! ```bash
//! # 默认用 .env 里配置的供应商（deepseek，无 vision → 演示错误路径）
//! cargo run -p cortex-llm --example vision
//!
//! # 指定一家能看图的（本机 Ollama 不需要密钥）
//! CORTEX_LLM_PROVIDER=ollama CORTEX_LLM_MODEL=gemma4:latest \
//!   cargo run -p cortex-llm --example vision -- 路径/图.png
//! ```

use std::path::PathBuf;

use cortex_llm::{LlmClient, Message, MessageImageExt, VisionSupport};

/// 仓库里自带的测试图：一张造出来的「控制台截图」，含中文标题、
/// 三个指标卡、七根带数值的柱子和一行页脚 —— 专门用来检验描述里
/// 有没有真的把文字与数字读出来。
const DEFAULT_IMAGE: &str = "crates/cortex-memory/tests/fixtures/console_screenshot.png";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("提示：未加载 .env（{e}），改用进程环境变量");
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cortex_llm=debug,warn".into()),
        )
        .init();

    let path = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from(DEFAULT_IMAGE), PathBuf::from);
    let bytes =
        std::fs::read(&path).map_err(|e| anyhow::anyhow!("读不到图片 {}：{e}", path.display()))?;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        other => anyhow::bail!("不认识的扩展名 {other:?}"),
    };

    let client = LlmClient::from_env()?;
    let model = client.model().clone();
    println!(
        "供应商 {} / 模型 {} / vision 声明 {:?}",
        client.provider_id(),
        model.model_name,
        client.vision_support(&model)
    );
    println!("图片 {} （{} 字节，{mime}）", path.display(), bytes.len());
    println!("─────────────────────────────────────────");

    let messages = [Message::user()
        .with_text("这张图里有什么？把你看到的文字逐字念出来。")
        .with_image_bytes(&bytes, mime)?];
    println!(
        "消息构造完成：{} 个图像块",
        cortex_llm::image_count(&messages)
    );

    // ① 本地闸门
    match client.ensure_can_see(&model, &messages) {
        Ok(()) => println!("① 本地闸门：放行"),
        Err(e) => println!("① 本地闸门：拦下 —— {e}"),
    }

    // ② 真的发出去。绕过闸门只为看清远端行为，见文件头说明。
    println!("② 直发供应商（绕过闸门）……");
    match client
        .provider()
        .complete(&model, "你是媒体转录器，回答简洁。", &messages, &[])
        .await
    {
        Ok((reply, usage)) => {
            println!("─────────────────────────────────────────");
            println!("{}", reply.as_concat_text());
            println!("─────────────────────────────────────────");
            println!(
                "token：入 {:?} 出 {:?}",
                usage.usage.input_tokens, usage.usage.output_tokens
            );
            anyhow::ensure!(
                client.vision_support(&model) != VisionSupport::Unsupported,
                "声明不支持 vision 的模型却成功返回了 —— 定义 JSON 该更新了"
            );
        }
        // 供应商拒图也是有效结果：它证明请求成功组装并送达，
        // 差别只在对面认不认。错误必须可读，这正是要看的东西。
        Err(e) => println!("远端拒绝：{e}"),
    }
    Ok(())
}

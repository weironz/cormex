//! 供应商目录与构造。
//!
//! 一条路走到黑：**所有**供应商都由一份声明式 JSON 描述，交给 goose 的
//! [`goose_providers::declarative::from_json`] 装配。goose 把 JSON 里的
//! `engine` 映射到三套已经调好的引擎实现（openai / anthropic / ollama），
//! 各家的 prompt caching、thinking 不透明块、格式转换、重试都在那一层完成 ——
//! 这里一行适配代码都不用写，加供应商 = 加一个 JSON 文件。
//!
//! 定义按两级查找：
//! 1. 本 crate 内置的 `src/definitions/*.json`；
//! 2. goose 自带目录（`fixed_provider_config_entries`，约 38 家）。
//!
//! 内置那份优先，理由是 goose 发版节奏与我们无关：alpha.1 自带的
//! `deepseek.json` 只列了 `deepseek-chat` / `deepseek-reasoner`，
//! 而 Cortex 用的是 `deepseek-v4-pro` / `deepseek-v4-flash`。
//! anthropic / openai / ollama 在 goose 里是「一等引擎」而非声明式条目，
//! 目录里查不到，也由内置定义补齐。
//!
//! 内置 JSON 的字段布局取件自 goose（Apache-2.0，
//! <https://github.com/aaif-goose/goose>，
//! `crates/goose-providers/src/declarative/definitions/deepseek.json`），
//! 模型列表按 Cortex 实际使用改写。

use std::convert::Infallible;

use goose_providers::base::Provider;
use goose_providers::declarative::{
    DeclarativeProviderConfig, KeyResolver, deserialize_provider_config,
    fixed_provider_config_entries, from_json,
};
use goose_providers::model::ModelConfig;

use crate::error::{LlmError, Result};
use crate::vision::VisionSupport;

/// 本 crate 内置的供应商定义。名字即配置里的 `provider` 取值。
const BUILTIN: &[(&str, &str)] = &[
    ("deepseek", include_str!("definitions/deepseek.json")),
    ("anthropic", include_str!("definitions/anthropic.json")),
    ("openai", include_str!("definitions/openai.json")),
    ("ollama", include_str!("definitions/ollama.json")),
];

/// 把固定密钥喂给 goose，而不是让它自己去读环境变量。
///
/// goose 默认用 `EnvKeyResolver`（`std::env::var(api_key_env)`）。我们把密钥
/// 从上层传进来，好处是：调用方可以从 `.env`、密钥托管、请求头等任意来源取件，
/// 供应商层不关心；测试也不必污染进程环境。
struct StaticKey(String);

impl KeyResolver for StaticKey {
    type Error = Infallible;

    fn resolve_key(&self, _key: &str) -> std::result::Result<String, Self::Error> {
        Ok(self.0.clone())
    }
}

/// 查一个供应商的定义 JSON。内置优先，其次 goose 自带目录。
pub(crate) fn definition(name: &str) -> Option<&'static str> {
    if let Some((_, json)) = BUILTIN.iter().find(|(id, _)| *id == name) {
        return Some(json);
    }
    let file = format!("{name}.json");
    fixed_provider_config_entries()
        .into_iter()
        .find(|(id, _)| *id == file)
        .map(|(_, json)| json)
}

/// 全部可用的供应商标识，已排序去重。用于报错时提示，以及 CLI 补全。
#[must_use]
pub fn available() -> Vec<String> {
    let mut all: Vec<String> = BUILTIN.iter().map(|(id, _)| (*id).to_string()).collect();
    all.extend(
        fixed_provider_config_entries()
            .into_iter()
            .filter_map(|(file, _)| file.strip_suffix(".json").map(str::to_string)),
    );
    all.sort_unstable();
    all.dedup();
    all
}

/// 某供应商约定的密钥环境变量名，如 deepseek → `DEEPSEEK_API_KEY`。
///
/// 返回空串表示该供应商免鉴权（Ollama）。名字来自定义 JSON 的 `api_key_env`，
/// 不在代码里硬编码 —— 加供应商时不必回来改这里。
pub fn api_key_env(name: &str) -> Result<String> {
    Ok(config_of(name)?.api_key_env)
}

/// 定义里声明的默认模型：`fast_model` 用作廉价模型，`models` 首项用作主模型。
pub(crate) fn default_models(name: &str) -> Result<(Option<String>, Option<String>)> {
    let config = config_of(name)?;
    let main = config.models.first().map(|info| info.name.clone());
    let cheap = config.fast_model.clone().or_else(|| main.clone());
    Ok((main, cheap))
}

fn config_of(name: &str) -> Result<DeclarativeProviderConfig> {
    let json = definition(name).ok_or_else(|| LlmError::UnknownProvider {
        name: name.to_string(),
        available: available().join(", "),
    })?;
    deserialize_provider_config(json).map_err(|source| LlmError::Build {
        name: name.to_string(),
        source,
    })
}

/// 按名字构造一个 goose `Provider`。
///
/// `api_key` 空串表示免鉴权（Ollama 这类本地服务），goose 会退回 `NoAuth`。
pub fn build(name: &str, api_key: &str) -> Result<Box<dyn Provider>> {
    build_with(name, api_key, None)
}

/// 同上，但可以把定义里那个 `base_url` 换掉。
///
/// # 为什么这一项必须能在运行期改
///
/// 供应商定义是 `include_str!` **编译进二进制**的，`base_url` 也在里面。
/// 没有这个覆盖，想把 `openai` 这个引擎指向任何别的地方 —— 自建的
/// vLLM / llama.cpp / LM Studio、公司内网网关、one-api / LiteLLM 这类中转、
/// 或者某个更便宜的兼容服务 —— 都只能改代码重编。
///
/// 而「OpenAI 兼容」几乎是这个行业的事实标准：给一个 URL 覆盖，
/// 比我们逐个把供应商加进内置清单可用得多，也免得把「支持哪些供应商」
/// 变成我们要维护的东西。模型名本来就已经能配（`CORTEX_LLM_MODEL`），
/// 而未知模型名不会被拒（退回 canonical 上下文上限），所以补上这一项之后
/// 「指向任意兼容端点」就齐了。
///
/// # 为什么是改 JSON 而不是改结构体
///
/// goose 的公开入口是 `from_json`，它自己在内部反序列化。拿到
/// `DeclarativeProviderConfig` 再塞回去需要它导出一个 `from_config`，
/// 而那是上游的 API。改一个字段的 JSON 是完全等价的，且不动上游。
///
/// # Errors
/// 供应商名不认识，或者 goose 建不起来。
pub fn build_with(name: &str, api_key: &str, base_url: Option<&str>) -> Result<Box<dyn Provider>> {
    let json = definition(name).ok_or_else(|| LlmError::UnknownProvider {
        name: name.to_string(),
        available: available().join(", "),
    })?;

    let json: std::borrow::Cow<'_, str> = match base_url.map(str::trim).filter(|u| !u.is_empty()) {
        None => std::borrow::Cow::Borrowed(json),
        Some(url) => {
            let mut v: serde_json::Value =
                serde_json::from_str(json).map_err(|e| LlmError::Build {
                    name: name.to_string(),
                    source: anyhow::anyhow!("内置的 {name} 定义不是合法 JSON：{e}"),
                })?;
            // 末尾的 `/` 会让 goose 拼出 `https://x//v1/...`。有的网关不在乎，
            // 有的直接 404 —— 而那条 404 看起来像「模型不存在」
            let url = url.trim_end_matches('/');
            v["base_url"] = serde_json::Value::String(url.to_owned());
            std::borrow::Cow::Owned(v.to_string())
        }
    };

    // TLS 配置传 None —— 走系统信任根即可，我们不需要客户端证书或自定义 CA。
    from_json(&json, None, StaticKey(api_key.to_string())).map_err(|source| LlmError::Build {
        name: name.to_string(),
        source,
    })
}

/// 为某个模型组装 [`ModelConfig`]，并把定义里声明的上下文上限填进去。
///
/// 显式填上限是有意为之：goose 自己只会去查它内置的 canonical 模型表，
/// 查不到就留空，运行时再靠 `GOOSE_CONTEXT_LIMIT` 环境变量兜底。
/// Cortex 不打算让用户去设 `GOOSE_*`，所以上限一律由本层的 JSON 定义说了算；
/// 定义里没列到的模型才回落到 goose 的 canonical 表。
pub fn model_config(provider: &str, model: &str) -> Result<ModelConfig> {
    let config = config_of(provider)?;
    let limit = config
        .models
        .iter()
        .find(|info| info.name == model)
        .map(|info| info.context_limit);

    Ok(ModelConfig::new(model)
        .with_canonical_limits(provider)
        .with_context_limit(limit))
}

// ───────────────────────────── vision 能力 ─────────────────────────────

/// 只为读 `vision` 字段而存在的一份极简定义视图。
///
/// # 为什么要再解析一遍同一份 JSON
///
/// goose 的 `DeclarativeProviderConfig` / `ModelInfo` **没有 vision 字段**
/// （alpha.1 里 `ModelInfo` 只有 name / context_limit / 价格 / cache_control /
/// reasoning）。serde 默认忽略未知字段，所以我们可以把 `"vision": true`
/// 写进同一份 JSON 而不影响 goose 解析 —— 但也就意味着 goose 那条路
/// 拿不到这个值，只能自己再走一遍。
///
/// 代价是一次极小的 JSON 解析；收益是**加供应商仍然只需改一个 JSON 文件**，
/// 不必回来改 Rust 代码。这与本模块「声明式，一条路走到黑」的取舍一致。
#[derive(serde::Deserialize)]
struct VisionView {
    #[serde(default)]
    models: Vec<VisionModel>,
}

#[derive(serde::Deserialize)]
struct VisionModel {
    name: String,
    /// 缺省即 [`VisionSupport::Unknown`]：没声明不等于不支持。
    #[serde(default)]
    vision: Option<bool>,
}

/// 查某个 `provider/model` 能不能看图。
///
/// 判据只有定义 JSON 里的 `vision` 字段。查不到供应商、查不到模型、
/// 或模型没声明该字段，一律 [`VisionSupport::Unknown`]（放行，让远端报错）。
///
/// Ollama 这类 `models: []` + `dynamic_models: true` 的供应商永远落在
/// `Unknown` —— 本地装了哪些模型只有那台机器自己知道，在这里编一个答案
/// 只会在用户装了 vision 模型时错误地拦下他。
#[must_use]
pub fn vision_support(provider: &str, model: &str) -> VisionSupport {
    let Some(json) = definition(provider) else {
        return VisionSupport::Unknown;
    };
    let Ok(view) = serde_json::from_str::<VisionView>(json) else {
        return VisionSupport::Unknown;
    };
    match view
        .models
        .iter()
        .find(|m| m.name == model)
        .and_then(|m| m.vision)
    {
        Some(true) => VisionSupport::Supported,
        Some(false) => VisionSupport::Unsupported,
        None => VisionSupport::Unknown,
    }
}

#[cfg(test)]
mod base_url_tests {
    use super::*;

    /// 覆盖之后，交给 goose 的那份 JSON 里 `base_url` 必须真的换了。
    ///
    /// 这条只验「改对了 JSON」。**它不足以证明这个功能是好的** ——
    /// goose 完全可能忽略这个字段，而那样这条测试照样绿。真正的证明是
    /// `examples/base_url_probe.rs`：它对着一个本机假端点发一次**真的 HTTP**，
    /// 并把响应打出来。写下来是因为这个区别值得记住。
    fn rewritten(name: &str, url: &str) -> serde_json::Value {
        let json = definition(name).expect("内置定义");
        let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
        v["base_url"] = serde_json::Value::String(url.trim_end_matches('/').to_owned());
        v
    }

    /// 免鉴权的供应商，`api_key_env` 必须是**空串**。
    ///
    /// 调用方据此跳过「读环境变量」那一步。cortexd 一度没有这一支，
    /// 于是 `std::env::var("")` 必然失败，报「缺少 ollama 的 API key
    /// 环境变量」—— 一条读起来像配置漏了、实际上无论怎么配都过不去的错误，
    /// 而它让 cortexd 用本地模型时**根本起不来**。
    ///
    /// 同一个判断在 `LlmClient::from_config` 与 `Live::new` 两处，
    /// 当时只写对了一处。这条钉住这个约定本身。
    #[test]
    fn a_keyless_provider_declares_an_empty_key_env() {
        assert_eq!(
            api_key_env("ollama").expect("内置定义"),
            "",
            "免鉴权的供应商必须回空串。回一个假的变量名会让调用方去读它，             而那个变量永远不会被设"
        );
        assert!(
            !api_key_env("deepseek").expect("内置定义").is_empty(),
            "要鉴权的供应商必须给出变量名，否则调用方会以为它免鉴权、             用空 key 去调，然后拿一条 401"
        );
    }

    #[test]
    fn an_override_replaces_the_compiled_in_url() {
        let v = rewritten("openai", "http://127.0.0.1:9099");
        assert_eq!(v["base_url"], "http://127.0.0.1:9099");
        assert_eq!(
            v["engine"], "openai",
            "只该换 URL —— 引擎、模型列表、鉴权方式都得原样留着"
        );
    }

    /// 末尾的 `/` 要削掉。
    ///
    /// 留着的话拼出来是 `http://x//v1/chat/completions`。有的网关不在乎，
    /// 有的直接 404 —— 而那条 404 读起来像「模型不存在」，
    /// 没有人会想到是自己多打了一个斜杠。
    #[test]
    fn a_trailing_slash_is_trimmed() {
        assert_eq!(
            rewritten("openai", "http://gw.local/v1/")["base_url"],
            "http://gw.local/v1"
        );
    }

    /// 不给覆盖时，用的还是编译进去的那个。
    #[test]
    fn no_override_means_the_builtin_url() {
        let json = definition("deepseek").expect("内置定义");
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(
            v["base_url"], "https://api.deepseek.com",
            "没给覆盖就不该动它 —— 绝大多数部署走的是这条路"
        );
    }

    /// 空串与纯空白按「没给」处理。
    ///
    /// compose 的 `${VAR:-}` 会把没配的变量**设成空串**，而那在这个仓库里
    /// 已经炸过三次（见 `cortex_core::config` 的 `non_empty`）。
    /// 这里把同一个坑堵死：空串绝不能变成 `base_url: ""`。
    #[test]
    fn an_empty_override_is_ignored() {
        for empty in ["", "   ", "	"] {
            assert!(
                Some(str::trim(empty)).filter(|u| !u.is_empty()).is_none(),
                "空串 {empty:?} 被当成了合法 URL —— 那会让 base_url 变成空，                 而请求会打到一个拼不出来的地址上"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_definitions_all_parse() {
        for (name, _) in BUILTIN {
            config_of(name).unwrap_or_else(|e| panic!("内置定义 {name} 解析失败：{e}"));
        }
    }

    #[test]
    fn builtin_shadows_goose_catalog() {
        // goose 自带目录里也有 deepseek，但内置那份才是权威。
        let config = config_of("deepseek").expect("deepseek 定义应存在");
        assert!(
            config.models.iter().any(|m| m.name == "deepseek-v4-flash"),
            "应取到内置定义（含 deepseek-v4-flash），实际：{:?}",
            config.models.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn goose_catalog_is_reachable() {
        // 没有内置定义的供应商应能从 goose 目录取到。
        assert!(available().iter().any(|p| p == "groq"));
        assert!(config_of("groq").is_ok());
    }

    #[test]
    fn unknown_provider_lists_alternatives() {
        let err = config_of("no-such-provider").expect_err("应报未知供应商");
        assert!(matches!(err, LlmError::UnknownProvider { .. }));
        assert!(err.to_string().contains("deepseek"));
    }

    #[test]
    fn context_limit_comes_from_definition() {
        let config = model_config("deepseek", "deepseek-v4-flash").expect("应构造成功");
        assert_eq!(config.context_limit(), 128_000);
    }

    #[test]
    fn vision_is_declared_per_model_not_guessed() {
        // 这四条是「多模态到底能不能用」的唯一事实来源。DeepSeek 那条尤其重要：
        // 它是当前默认供应商，而它一个 vision 模型都没有。
        assert_eq!(
            vision_support("deepseek", "deepseek-v4-pro"),
            VisionSupport::Unsupported
        );
        assert_eq!(
            vision_support("anthropic", "claude-opus-5"),
            VisionSupport::Supported
        );
        assert_eq!(
            vision_support("openai", "gpt-5.5"),
            VisionSupport::Supported
        );
        // 定义里没有的模型名 → 未知，不是「不支持」
        assert_eq!(
            vision_support("deepseek", "deepseek-v9-omni"),
            VisionSupport::Unknown
        );
    }

    #[test]
    fn dynamic_and_foreign_providers_stay_unknown() {
        // Ollama 装了什么只有那台机器知道；goose 目录里的供应商我们没考据过。
        // 两者都必须是 Unknown（放行），否则会误伤真能看图的模型。
        assert_eq!(
            vision_support("ollama", "gemma4:latest"),
            VisionSupport::Unknown
        );
        assert_eq!(vision_support("groq", "whatever"), VisionSupport::Unknown);
        assert_eq!(
            vision_support("no-such-provider", "x"),
            VisionSupport::Unknown,
            "供应商都不存在时也不能崩，判定函数在热路径上"
        );
    }

    #[test]
    fn vision_field_does_not_break_goose_parsing() {
        // 我们往 goose 的 JSON 里塞了它不认识的 `vision` 字段。
        // serde 默认忽略未知字段——但这是**约定而非契约**，
        // goose 哪天加上 deny_unknown_fields 就会在这里炸，而不是在生产上。
        for (name, _) in BUILTIN {
            let config = config_of(name).unwrap_or_else(|e| panic!("{name} 解析失败：{e}"));
            assert!(!config.models.is_empty() || name == &"ollama");
        }
    }

    #[test]
    fn provider_builds_without_touching_env() {
        // 关键性质：不设任何 DEEPSEEK_API_KEY 也能构造出来。
        let provider = build("deepseek", "test-key").expect("应构造成功");
        assert_eq!(provider.get_name(), "deepseek");
    }
}

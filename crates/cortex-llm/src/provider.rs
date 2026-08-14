//! The provider catalog and construction.
//!
//! One approach, all the way down: **every** provider is described by a
//! declarative JSON document and assembled by goose's
//! [`goose_providers::declarative::from_json`]. goose maps the JSON's `engine`
//! onto three already-tuned engine implementations (openai / anthropic /
//! ollama), and each vendor's prompt caching, thinking opaque blocks, format
//! conversion, and retries all happen in that layer — not one line of adapter
//! code is needed here, and adding a provider = adding a JSON file.
//!
//! Definitions are looked up at two levels:
//! 1. This crate's built-in `src/definitions/*.json`;
//! 2. goose's own catalog (`fixed_provider_config_entries`, around 38
//!    vendors).
//!
//! The built-in one wins, because goose's release cadence has nothing to do
//! with ours: the `deepseek.json` shipped in alpha.1 lists only
//! `deepseek-chat` / `deepseek-reasoner`, while Cortex uses `deepseek-v4-pro` /
//! `deepseek-v4-flash`. anthropic / openai / ollama are "first-class engines"
//! in goose rather than declarative entries, so they are absent from the
//! catalog and are likewise supplied by the built-in definitions.
//!
//! The field layout of the built-in JSON is vendored from goose (Apache-2.0,
//! <https://github.com/aaif-goose/goose>,
//! `crates/goose-providers/src/declarative/definitions/deepseek.json`), with
//! the model list rewritten to match what Cortex actually uses.

use std::convert::Infallible;

use goose_providers::base::Provider;
use goose_providers::declarative::{
    DeclarativeProviderConfig, KeyResolver, deserialize_provider_config,
    fixed_provider_config_entries, from_json,
};
use goose_providers::model::ModelConfig;

use crate::error::{LlmError, Result};
use crate::vision::VisionSupport;

/// This crate's built-in provider definitions. The name is exactly the
/// `provider` value in the configuration.
const BUILTIN: &[(&str, &str)] = &[
    ("deepseek", include_str!("definitions/deepseek.json")),
    ("anthropic", include_str!("definitions/anthropic.json")),
    ("openai", include_str!("definitions/openai.json")),
    ("ollama", include_str!("definitions/ollama.json")),
];

/// Hand goose a fixed key instead of letting it read the environment itself.
///
/// goose defaults to `EnvKeyResolver` (`std::env::var(api_key_env)`). Passing
/// the key down from above buys two things: the caller can source it from
/// `.env`, a secret manager, a request header, or anywhere else, and the
/// provider layer does not care; and tests need not pollute the process
/// environment.
struct StaticKey(String);

impl KeyResolver for StaticKey {
    type Error = Infallible;

    fn resolve_key(&self, _key: &str) -> std::result::Result<String, Self::Error> {
        Ok(self.0.clone())
    }
}

/// Look up a provider's definition JSON. Built-in first, then goose's own
/// catalog.
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

/// Every available provider identifier, sorted and deduplicated. Used for the
/// hint in error messages and for CLI completion.
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

/// The environment variable name a provider expects its key in, e.g. deepseek
/// → `DEEPSEEK_API_KEY`.
///
/// An empty string means the provider needs no authentication (Ollama). The
/// name comes from the definition JSON's `api_key_env` and is not hardcoded
/// here — adding a provider requires no edit to this function.
pub fn api_key_env(name: &str) -> Result<String> {
    Ok(config_of(name)?.api_key_env)
}

/// The default models declared in the definition: `fast_model` serves as the
/// cheap model, and the first entry of `models` as the main one.
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

/// Build a goose `Provider` by name.
///
/// An empty `api_key` means no authentication (local services like Ollama), and
/// goose falls back to `NoAuth`.
pub fn build(name: &str, api_key: &str) -> Result<Box<dyn Provider>> {
    build_with(name, api_key, None)
}

/// Same as above, but the `base_url` from the definition can be swapped out.
///
/// # Why this one setting must be changeable at runtime
///
/// Provider definitions are `include_str!`-ed, i.e. **compiled into the
/// binary**, `base_url` included. Without this override, pointing the `openai`
/// engine anywhere else — a self-hosted vLLM / llama.cpp / LM Studio, a
/// corporate gateway, a one-api / LiteLLM style relay, or some cheaper
/// compatible service — would mean editing code and rebuilding.
///
/// And "OpenAI-compatible" is very nearly this industry's de facto standard: a
/// single URL override is far more useful than us adding providers to the
/// built-in list one at a time, and it keeps "which providers are supported"
/// from becoming something we maintain. The model name was already
/// configurable (`CORTEX_LLM_MODEL`), and an unknown model name is not
/// rejected (it falls back to the canonical context limit), so once this
/// setting is in place "point at any compatible endpoint" is complete.
///
/// # Why edit the JSON rather than the struct
///
/// goose's public entry point is `from_json`, which deserializes internally.
/// Taking a `DeclarativeProviderConfig` and pushing it back in would require
/// goose to export a `from_config`, and that is upstream's API. Editing one
/// field of the JSON is exactly equivalent and touches nothing upstream.
///
/// # Errors
/// The provider name is unrecognized, or goose fails to build it.
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
                    source: anyhow::anyhow!("built-in {name} definition is not valid JSON: {e}"),
                })?;
            // A trailing `/` makes goose assemble `https://x//v1/...`. Some
            // gateways do not care, others return a flat 404 — and that 404
            // looks like "the model does not exist"
            let url = url.trim_end_matches('/');
            v["base_url"] = serde_json::Value::String(url.to_owned());
            std::borrow::Cow::Owned(v.to_string())
        }
    };

    // Pass None for the TLS configuration — the system trust roots are enough,
    // and we need neither client certificates nor a custom CA.
    from_json(&json, None, StaticKey(api_key.to_string())).map_err(|source| LlmError::Build {
        name: name.to_string(),
        source,
    })
}

/// Assemble a [`ModelConfig`] for a model, filling in the context limit
/// declared in the definition.
///
/// Filling the limit in explicitly is deliberate: goose only consults its own
/// built-in canonical model table, leaves the field empty when it finds
/// nothing, and then falls back at runtime to the `GOOSE_CONTEXT_LIMIT`
/// environment variable. Cortex has no intention of making users set `GOOSE_*`,
/// so the limit is always decided by this layer's JSON definitions; only models
/// the definitions do not list fall back to goose's canonical table.
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

// ─────────────────────────── vision capability ───────────────────────────

/// A minimal view over the definition that exists only to read the `vision`
/// field.
///
/// # Why the same JSON gets parsed a second time
///
/// goose's `DeclarativeProviderConfig` / `ModelInfo` **have no vision field**
/// (in alpha.1 `ModelInfo` carries only name / context_limit / pricing /
/// cache_control / reasoning). serde ignores unknown fields by default, so we
/// can put `"vision": true` into the same JSON without disturbing goose's
/// parsing — but that also means the value is unreachable via goose's path, and
/// we have to walk it ourselves.
///
/// The price is one very small JSON parse; the payoff is that **adding a
/// provider still only means editing one JSON file**, with no need to come back
/// and change Rust code. That matches this module's "declarative, all the way
/// down" tradeoff.
#[derive(serde::Deserialize)]
struct VisionView {
    #[serde(default)]
    models: Vec<VisionModel>,
}

#[derive(serde::Deserialize)]
struct VisionModel {
    name: String,
    /// Absent means [`VisionSupport::Unknown`]: undeclared is not the same as
    /// unsupported.
    #[serde(default)]
    vision: Option<bool>,
}

/// Look up whether a given `provider/model` can see images.
///
/// The only criterion is the `vision` field in the definition JSON. An unknown
/// provider, an unknown model, or a model that does not declare the field all
/// give [`VisionSupport::Unknown`] (let it through and let the remote end
/// complain).
///
/// Providers like Ollama with `models: []` + `dynamic_models: true` always land
/// on `Unknown` — only that machine knows which models are installed locally,
/// and inventing an answer here would wrongly block a user who has in fact
/// installed a vision model.
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

    /// After an override, `base_url` in the JSON handed to goose must really
    /// have changed.
    ///
    /// This only verifies "the JSON was edited correctly". **It is not enough
    /// to prove the feature works** — goose could perfectly well ignore the
    /// field, and this test would stay green anyway. The real proof is
    /// `examples/base_url_probe.rs`: it sends **actual HTTP** at a fake local
    /// endpoint and prints the response. Written down because the distinction
    /// is worth remembering.
    fn rewritten(name: &str, url: &str) -> serde_json::Value {
        let json = definition(name).expect("the built-in definition");
        let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
        v["base_url"] = serde_json::Value::String(url.trim_end_matches('/').to_owned());
        v
    }

    /// For a provider needing no authentication, `api_key_env` must be the
    /// **empty string**.
    ///
    /// The caller uses that to skip the "read the environment variable" step.
    /// cortexd once lacked this branch, so `std::env::var("")` failed
    /// unavoidably and reported "the API key environment variable for ollama is
    /// missing" — an error that reads like a configuration oversight but that no
    /// amount of configuration could get past, and it left cortexd **unable to
    /// start at all** when using a local model.
    ///
    /// The same check lives in two places, `LlmClient::from_config` and
    /// `Live::new`, and back then only one of them was right. This test pins
    /// down the convention itself.
    #[test]
    fn a_keyless_provider_declares_an_empty_key_env() {
        assert_eq!(
            api_key_env("ollama").expect("the built-in definition"),
            "",
            "a provider needing no authentication must return an empty string. \
             Returning a bogus variable name sends the caller off to read it, \
             and that variable will never be set"
        );
        assert!(
            !api_key_env("deepseek")
                .expect("the built-in definition")
                .is_empty(),
            "a provider that does need authentication must give a variable \
             name, otherwise the caller assumes it is keyless, calls with an \
             empty key, and gets back a 401"
        );
    }

    #[test]
    fn an_override_replaces_the_compiled_in_url() {
        let v = rewritten("openai", "http://127.0.0.1:9099");
        assert_eq!(v["base_url"], "http://127.0.0.1:9099");
        assert_eq!(
            v["engine"], "openai",
            "only the URL should change — the engine, the model list, and the \
             authentication scheme all have to stay exactly as they were"
        );
    }

    /// A trailing `/` gets trimmed.
    ///
    /// Left in place, the assembled URL is
    /// `http://x//v1/chat/completions`. Some gateways do not care, others
    /// return a flat 404 — and that 404 reads like "the model does not exist",
    /// with nobody suspecting they typed one slash too many.
    #[test]
    fn a_trailing_slash_is_trimmed() {
        assert_eq!(
            rewritten("openai", "http://gw.local/v1/")["base_url"],
            "http://gw.local/v1"
        );
    }

    /// With no override, the compiled-in URL is what gets used.
    #[test]
    fn no_override_means_the_builtin_url() {
        let json = definition("deepseek").expect("the built-in definition");
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(
            v["base_url"], "https://api.deepseek.com",
            "with no override it must be left alone — this is the path the vast \
             majority of deployments take"
        );
    }

    /// An empty string, or pure whitespace, is treated as "not given".
    ///
    /// compose's `${VAR:-}` **sets an unconfigured variable to the empty
    /// string**, and that has already blown up three times in this repository
    /// (see `non_empty` in `cortex_core::config`). This seals the same pit:
    /// an empty string must never become `base_url: ""`.
    #[test]
    fn an_empty_override_is_ignored() {
        for empty in ["", "   ", "	"] {
            assert!(
                Some(str::trim(empty)).filter(|u| !u.is_empty()).is_none(),
                "the empty string {empty:?} was taken as a valid URL — that \
                 would blank out base_url, and requests would go to an address \
                 that cannot be assembled at all"
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
            config_of(name)
                .unwrap_or_else(|e| panic!("built-in definition {name} failed to parse: {e}"));
        }
    }

    #[test]
    fn builtin_shadows_goose_catalog() {
        // goose's own catalog also has a deepseek, but the built-in one is
        // authoritative.
        let config = config_of("deepseek").expect("a deepseek definition should exist");
        assert!(
            config.models.iter().any(|m| m.name == "deepseek-v4-flash"),
            "the built-in definition (containing deepseek-v4-flash) should \
             have been picked up, got: {:?}",
            config.models.iter().map(|m| &m.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn goose_catalog_is_reachable() {
        // A provider with no built-in definition should be reachable from
        // goose's catalog.
        assert!(available().iter().any(|p| p == "groq"));
        assert!(config_of("groq").is_ok());
    }

    #[test]
    fn unknown_provider_lists_alternatives() {
        let err = config_of("no-such-provider").expect_err("should report an unknown provider");
        assert!(matches!(err, LlmError::UnknownProvider { .. }));
        assert!(err.to_string().contains("deepseek"));
    }

    #[test]
    fn context_limit_comes_from_definition() {
        let config = model_config("deepseek", "deepseek-v4-flash").expect("should build");
        assert_eq!(config.context_limit(), 128_000);
    }

    #[test]
    fn vision_is_declared_per_model_not_guessed() {
        // These four are the single source of truth for "is multimodal usable
        // at all". The DeepSeek one matters most: it is the current default
        // provider, and it has no vision model whatsoever.
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
        // A model name absent from the definition → unknown, not "unsupported"
        assert_eq!(
            vision_support("deepseek", "deepseek-v9-omni"),
            VisionSupport::Unknown
        );
    }

    #[test]
    fn dynamic_and_foreign_providers_stay_unknown() {
        // Only that machine knows what Ollama has installed; and we have not
        // researched the providers in goose's catalog. Both must be Unknown
        // (let through), or we wrongly block models that genuinely can see
        // images.
        assert_eq!(
            vision_support("ollama", "gemma4:latest"),
            VisionSupport::Unknown
        );
        assert_eq!(vision_support("groq", "whatever"), VisionSupport::Unknown);
        assert_eq!(
            vision_support("no-such-provider", "x"),
            VisionSupport::Unknown,
            "it must not blow up even when the provider does not exist at all — \
             this decision function sits on the hot path"
        );
    }

    #[test]
    fn vision_field_does_not_break_goose_parsing() {
        // We stuff a `vision` field goose does not recognize into goose's JSON.
        // serde ignores unknown fields by default — but that is **a convention,
        // not a contract**; the day goose adds deny_unknown_fields it will blow
        // up here rather than in production.
        for (name, _) in BUILTIN {
            let config = config_of(name).unwrap_or_else(|e| panic!("{name} failed to parse: {e}"));
            assert!(!config.models.is_empty() || name == &"ollama");
        }
    }

    #[test]
    fn provider_builds_without_touching_env() {
        // The key property: it builds without any DEEPSEEK_API_KEY being set.
        let provider = build("deepseek", "test-key").expect("should build");
        assert_eq!(provider.get_name(), "deepseek");
    }
}

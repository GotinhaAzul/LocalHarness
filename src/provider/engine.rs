//! Engine fingerprint and per-engine wire capabilities.
//!
//! The OpenAI-compatible API is a family, not one server. Sampler names,
//! structured-output dialects, and the ability to combine tools with a strict
//! response format differ by engine. Detection is metadata-only and best
//! effort; `[model] engine` pins it when a gateway hides the real server.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::probe;
use super::{Provider, endpoint_urls};

/// How long one metadata probe may take while resolving the engine. The
/// probes run concurrently, so the cost is one timeout, not one per engine.
const DETECT_TIMEOUT: Duration = Duration::from_millis(900);

/// The inference engine behind an OpenAI-compatible endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    LlamaCpp,
    Vllm,
    Sglang,
    Ollama,
    LmStudio,
    KoboldCpp,
    TabbyApi,
    Mlx,
    #[default]
    Generic,
}

impl Engine {
    /// Stable label used in config, diagnostics, and request bodies.
    pub fn label(self) -> &'static str {
        match self {
            Engine::LlamaCpp => "llama.cpp",
            Engine::Vllm => "vllm",
            Engine::Sglang => "sglang",
            Engine::Ollama => "ollama",
            Engine::LmStudio => "lmstudio",
            Engine::KoboldCpp => "koboldcpp",
            Engine::TabbyApi => "tabbyapi",
            Engine::Mlx => "mlx",
            Engine::Generic => "generic",
        }
    }

    /// Parse a configured engine name. `auto` and the empty string mean
    /// detect from server metadata.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "auto" => None,
            "llama.cpp" | "llama-cpp" | "llamacpp" | "llama" | "llama-server" => {
                Some(Engine::LlamaCpp)
            }
            "vllm" | "v-llm" => Some(Engine::Vllm),
            "sglang" | "sg-lang" => Some(Engine::Sglang),
            "ollama" => Some(Engine::Ollama),
            "lmstudio" | "lm-studio" | "lm_studio" => Some(Engine::LmStudio),
            "koboldcpp" | "kobold" => Some(Engine::KoboldCpp),
            "tabbyapi" | "tabby" => Some(Engine::TabbyApi),
            "mlx" | "mlx-lm" | "mlx_lm" => Some(Engine::Mlx),
            "generic" | "openai" | "unknown" => Some(Engine::Generic),
            _ => None,
        }
    }

    /// Resolve the engine from `[model] engine`, falling back to metadata
    /// detection when the value is empty or `auto`.
    pub async fn resolve(provider: &dyn Provider, configured: &str) -> Self {
        if let Some(engine) = Engine::parse(configured) {
            return engine;
        }
        Engine::detect(provider).await
    }

    /// Fingerprint the engine from metadata endpoints. Order matches
    /// `tot doctor`; detection stops at the first engine that answers. Every
    /// probe is best-effort and bounded by [`DETECT_TIMEOUT`].
    pub async fn detect(provider: &dyn Provider) -> Self {
        let model = provider.model().trim().to_string();
        let props = tokio::time::timeout(DETECT_TIMEOUT, probe::llama_props(provider)).await;
        if matches!(props, Ok(Ok(_))) {
            return Engine::LlamaCpp;
        }
        if !model.is_empty() {
            let ollama =
                tokio::time::timeout(DETECT_TIMEOUT, probe::ollama_show(provider, &model)).await;
            if matches!(ollama, Ok(Ok(_))) {
                return Engine::Ollama;
            }
        }

        let urls = endpoint_urls(provider.endpoint());
        let models_url = format!("{}/models", urls.api);
        let health_url = format!("{}/health", urls.root);
        let server_info_url = format!("{}/server_info", urls.root);
        let lmstudio_url = format!("{}/api/v1/models", urls.root);
        let kobold_url = format!("{}/api/extra/version", urls.root);
        let tabby_url = format!("{}/model", urls.api);
        let (models, health, sglang, lmstudio, kobold, tabby) = tokio::join!(
            probe::get_json_with_timeout(provider, &models_url, DETECT_TIMEOUT),
            probe::get_ok_with_timeout(provider, &health_url, DETECT_TIMEOUT),
            probe::get_json_with_timeout(provider, &server_info_url, DETECT_TIMEOUT),
            probe::get_json_with_timeout(provider, &lmstudio_url, DETECT_TIMEOUT),
            probe::get_json_with_timeout(provider, &kobold_url, DETECT_TIMEOUT),
            probe::get_json_with_timeout(provider, &tabby_url, DETECT_TIMEOUT),
        );

        if health
            && models.as_ref().ok().is_some_and(|value| {
                value["data"].as_array().is_some_and(|items| {
                    items
                        .iter()
                        .any(|item| item["max_model_len"].as_u64().is_some_and(|n| n > 0))
                })
            })
        {
            return Engine::Vllm;
        }
        if sglang.as_ref().ok().is_some_and(|value| {
            value.get("max_context_length").is_some()
                || value.get("context_length").is_some()
                || value.get("version").is_some()
        }) {
            return Engine::Sglang;
        }
        if lmstudio.as_ref().ok().is_some_and(|value| {
            let entries = value["data"]
                .as_array()
                .or_else(|| value["models"].as_array())
                .map(Vec::as_slice)
                .unwrap_or_default();
            entries.iter().any(|entry| {
                entry.get("max_context_length").is_some()
                    || entry.get("loaded_context_length").is_some()
            })
        }) {
            return Engine::LmStudio;
        }
        if kobold.as_ref().ok().is_some_and(|value| {
            value["result"]
                .as_str()
                .is_some_and(|result| result.to_ascii_lowercase().contains("kobold"))
        }) {
            return Engine::KoboldCpp;
        }
        if tabby
            .as_ref()
            .ok()
            .is_some_and(|value| value["parameters"].is_object())
        {
            return Engine::TabbyApi;
        }
        // MLX-LM identifies itself only through the model id it serves.
        if model.to_ascii_lowercase().contains("mlx") || entries_name_mlx(models.as_ref().ok()) {
            return Engine::Mlx;
        }
        Engine::Generic
    }

    /// Fields the engine maps with a known wire name. `None` means the field
    /// is not sent (and `tot doctor` warns). `repeat_penalty` is renamed per
    /// engine; vLLM and SGLang call it `repetition_penalty`.
    pub fn sampler_key(self, field: super::sampler::SamplerField) -> Option<&'static str> {
        use super::sampler::SamplerField as F;
        match (self, field) {
            (
                Engine::LlamaCpp
                | Engine::Vllm
                | Engine::Sglang
                | Engine::Ollama
                | Engine::LmStudio
                | Engine::KoboldCpp
                | Engine::TabbyApi
                | Engine::Mlx
                | Engine::Generic,
                F::Temperature,
            ) => Some("temperature"),
            (
                Engine::LlamaCpp
                | Engine::Vllm
                | Engine::Sglang
                | Engine::Ollama
                | Engine::LmStudio
                | Engine::KoboldCpp
                | Engine::TabbyApi
                | Engine::Mlx
                | Engine::Generic,
                F::TopP,
            ) => Some("top_p"),
            (
                Engine::LlamaCpp
                | Engine::Vllm
                | Engine::Sglang
                | Engine::LmStudio
                | Engine::KoboldCpp
                | Engine::TabbyApi,
                F::MinP,
            ) => Some("min_p"),
            (Engine::Vllm | Engine::Sglang | Engine::TabbyApi, F::RepeatPenalty) => {
                Some("repetition_penalty")
            }
            (
                Engine::LlamaCpp | Engine::Ollama | Engine::LmStudio | Engine::KoboldCpp,
                F::RepeatPenalty,
            ) => Some("repeat_penalty"),
            (Engine::LlamaCpp | Engine::KoboldCpp, F::DryMultiplier) => Some("dry_multiplier"),
            (Engine::LlamaCpp | Engine::KoboldCpp, F::DryBase) => Some("dry_base"),
            (Engine::LlamaCpp | Engine::KoboldCpp, F::XtcProbability) => Some("xtc_probability"),
            (
                Engine::LlamaCpp
                | Engine::Vllm
                | Engine::Sglang
                | Engine::Ollama
                | Engine::LmStudio
                | Engine::KoboldCpp
                | Engine::TabbyApi
                | Engine::Mlx,
                F::Seed,
            ) => Some("seed"),
            _ => None,
        }
    }

    /// Structured-output dialect this engine understands. `None` means no
    /// grammar backend: the harness falls back to the repair loop.
    pub fn structured_output(self) -> StructuredOutput {
        match self {
            Engine::LlamaCpp => StructuredOutput::Grammar,
            Engine::Vllm | Engine::Sglang => StructuredOutput::JsonSchema,
            Engine::KoboldCpp => StructuredOutput::KoboldGrammar,
            Engine::Ollama | Engine::LmStudio => StructuredOutput::JsonSchema,
            Engine::TabbyApi | Engine::Mlx => StructuredOutput::None,
            Engine::Generic => StructuredOutput::Unknown,
        }
    }

    /// Whether a strict response format can ride on the same request as
    /// `tools[]`. llama.cpp rejects a custom grammar with tools (issue #30377
    /// documents the reasoning loop), and the other engines that only proxy
    /// `response_format` do not document both together.
    pub fn supports_tools_with_strict_output(self) -> bool {
        matches!(self, Engine::Vllm | Engine::Sglang | Engine::KoboldCpp)
    }
}

/// Structured-output capability recorded in the `tot doctor` manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredOutput {
    /// JSON Schema (`response_format` / `structured_outputs`).
    JsonSchema,
    /// GBNF grammar.
    Grammar,
    /// KoboldCpp's `/api/extra/json_to_grammar` endpoint.
    KoboldGrammar,
    /// No grammar backend.
    None,
    /// The server did not identify itself.
    Unknown,
}

impl StructuredOutput {
    pub fn label(self) -> &'static str {
        match self {
            StructuredOutput::JsonSchema => "json_schema",
            StructuredOutput::Grammar | StructuredOutput::KoboldGrammar => "grammar",
            StructuredOutput::None => "none",
            StructuredOutput::Unknown => "unknown",
        }
    }

    pub fn usable(self) -> bool {
        !matches!(self, StructuredOutput::None | StructuredOutput::Unknown)
    }
}

fn entries_name_mlx(value: Option<&serde_json::Value>) -> bool {
    value
        .and_then(|value| value["data"].as_array())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item["id"]
                    .as_str()
                    .is_some_and(|id| id.to_ascii_lowercase().contains("mlx"))
                    || item["owned_by"]
                        .as_str()
                        .is_some_and(|owner| owner.eq_ignore_ascii_case("mlx"))
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_names_parse_with_aliases_and_auto() {
        assert_eq!(Engine::parse("auto"), None);
        assert_eq!(Engine::parse(""), None);
        assert_eq!(Engine::parse("llama.cpp"), Some(Engine::LlamaCpp));
        assert_eq!(Engine::parse("llama-server"), Some(Engine::LlamaCpp));
        assert_eq!(Engine::parse("vLLM"), Some(Engine::Vllm));
        assert_eq!(Engine::parse("sglang"), Some(Engine::Sglang));
        assert_eq!(Engine::parse("lm-studio"), Some(Engine::LmStudio));
        assert_eq!(Engine::parse("mlx-lm"), Some(Engine::Mlx));
        assert_eq!(Engine::parse("unknown-name"), None);
    }

    #[test]
    fn sampler_and_structured_capabilities_match_the_documented_engines() {
        use super::super::sampler::SamplerField;
        assert_eq!(
            Engine::LlamaCpp.sampler_key(SamplerField::RepeatPenalty),
            Some("repeat_penalty")
        );
        assert_eq!(
            Engine::Vllm.sampler_key(SamplerField::RepeatPenalty),
            Some("repetition_penalty")
        );
        assert_eq!(Engine::Vllm.sampler_key(SamplerField::MinP), Some("min_p"));
        assert_eq!(Engine::Ollama.sampler_key(SamplerField::MinP), None);
        assert_eq!(Engine::Mlx.sampler_key(SamplerField::DryMultiplier), None);
        assert_eq!(Engine::Mlx.structured_output(), StructuredOutput::None);
        assert_eq!(Engine::LlamaCpp.structured_output().label(), "grammar");
        assert_eq!(Engine::Vllm.structured_output().label(), "json_schema");
        assert!(!Engine::LlamaCpp.supports_tools_with_strict_output());
        assert!(Engine::Vllm.supports_tools_with_strict_output());
    }
}

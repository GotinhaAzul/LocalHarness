//! Optional capability probes. None of these are required; failures degrade.

use std::time::Duration;

use serde_json::json;

use super::Provider;
use super::types::{ErrorClass, ProviderError};
use super::{EndpointUrls, endpoint_urls};

#[derive(Debug, Clone, Default)]
pub struct Capabilities {
    pub models: bool,
    pub props: bool,
    pub tokenize: bool,
    pub ollama_show: bool,
    pub usage: bool,
    pub reasoning: bool,
    pub native_tools: bool,
    pub constrained_tools: bool,
}

#[derive(Debug, Clone, Default)]
pub struct LlamaProps {
    pub n_ctx: Option<u32>,
    pub model_path: Option<String>,
    pub chat_template: Option<String>,
    pub slots: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct OllamaModel {
    /// `num_ctx` from the Modelfile parameters: what Ollama actually serves.
    pub num_ctx: Option<u32>,
    /// `<arch>.context_length`: the trained maximum, usually larger than the
    /// window Ollama allocates.
    pub context_length: Option<u32>,
    pub family: Option<String>,
}

fn client_and_urls(
    provider: &dyn Provider,
) -> Result<(&reqwest::Client, EndpointUrls), ProviderError> {
    let client = provider.client().ok_or_else(|| {
        ProviderError::new(
            ErrorClass::BadRequest,
            "this provider does not expose HTTP probes",
        )
    })?;
    Ok((client, endpoint_urls(provider.endpoint())))
}

fn request_with_timeout(
    provider: &dyn Provider,
    url: &str,
    timeout: Duration,
) -> Result<reqwest::RequestBuilder, ProviderError> {
    let (client, _) = client_and_urls(provider)?;
    let mut req = client.get(url).timeout(timeout);
    if !provider.api_key().is_empty() {
        req = req.bearer_auth(provider.api_key());
    }
    Ok(req)
}

fn request(provider: &dyn Provider, url: &str) -> Result<reqwest::RequestBuilder, ProviderError> {
    request_with_timeout(provider, url, Duration::from_secs(10))
}

/// `GET /v1/models`, returning model ids.
pub async fn list_models(provider: &dyn Provider) -> Result<Vec<String>, ProviderError> {
    list_models_with_timeout(provider, Duration::from_secs(10)).await
}

/// `GET /v1/models` with an explicit request timeout.
pub async fn list_models_with_timeout(
    provider: &dyn Provider,
    timeout: Duration,
) -> Result<Vec<String>, ProviderError> {
    let value = models_json(provider, timeout).await?;
    Ok(model_ids(&value))
}

/// Raw `GET /v1/models` body. Some servers (vLLM, OpenRouter, LiteLLM, LM
/// Studio) put per-model context sizes in the entries.
async fn models_json(
    provider: &dyn Provider,
    timeout: Duration,
) -> Result<serde_json::Value, ProviderError> {
    let (_, urls) = client_and_urls(provider)?;
    let url = format!("{}/models", urls.api);
    let response = request_with_timeout(provider, &url, timeout)?
        .send()
        .await
        .map_err(|err| {
            ProviderError::new(ErrorClass::Connection, format!("/v1/models failed: {err}"))
        })?;
    if !response.status().is_success() {
        return Err(ProviderError::new(
            ErrorClass::BadRequest,
            format!("/v1/models returned {}", response.status()),
        ));
    }
    response.json().await.map_err(|err| {
        ProviderError::new(ErrorClass::Parse, format!("/v1/models is not JSON: {err}"))
    })
}

fn model_ids(value: &serde_json::Value) -> Vec<String> {
    value["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Context size advertised for `model` in a `/v1/models` body. Loaded/runtime
/// sizes win over maximums. llama.cpp's `meta.n_ctx_train` is deliberately
/// ignored: it is the trained length, not what the server allocated.
fn window_from_models(value: &serde_json::Value, model: &str) -> Option<u32> {
    let entry = value["data"]
        .as_array()?
        .iter()
        .find(|item| item["id"].as_str() == Some(model))?;
    const KEYS: &[&str] = &[
        "loaded_context_length", // LM Studio
        "max_model_len",         // vLLM
        "context_length",        // OpenRouter, LiteLLM, many gateways
        "context_window",
        "max_context_length", // LM Studio
        "max_input_tokens",
    ];
    KEYS.iter()
        .find_map(|key| positive(&entry[*key]))
        .or_else(|| positive(&entry["top_provider"]["context_length"]))
}

fn positive(value: &serde_json::Value) -> Option<u32> {
    value
        .as_u64()
        .filter(|n| *n > 0)
        .map(|n| n.min(u32::MAX as u64) as u32)
}

/// `GET /props` on llama.cpp.
pub async fn llama_props(provider: &dyn Provider) -> Result<LlamaProps, ProviderError> {
    let (_, urls) = client_and_urls(provider)?;
    let url = format!("{}/props", urls.root);
    let response = request(provider, &url)?.send().await.map_err(|err| {
        ProviderError::new(ErrorClass::Connection, format!("/props failed: {err}"))
    })?;
    if !response.status().is_success() {
        return Err(ProviderError::new(
            ErrorClass::BadRequest,
            format!("/props returned {}", response.status()),
        ));
    }
    let value: serde_json::Value = response.json().await.map_err(|err| {
        ProviderError::new(ErrorClass::Parse, format!("/props is not JSON: {err}"))
    })?;
    let n_ctx = value["default_generation_settings"]["n_ctx"]
        .as_u64()
        .or_else(|| value["n_ctx"].as_u64())
        .map(|n| n as u32);
    let chat_template = value["chat_template"].as_str().map(str::to_string);
    let slots = value["total_slots"]
        .as_u64()
        .or_else(|| value["slots"].as_u64())
        .map(|n| n as u32);
    let model_path = value["model_path"].as_str().map(str::to_string);
    Ok(LlamaProps {
        n_ctx,
        model_path,
        chat_template,
        slots,
    })
}

/// `POST /tokenize` on llama.cpp and vLLM. Returns token ids.
pub async fn tokenize(provider: &dyn Provider, text: &str) -> Result<Vec<u32>, ProviderError> {
    let (client, urls) = client_and_urls(provider)?;
    let url = format!("{}/tokenize", urls.root);
    let mut req = client
        .post(&url)
        .json(&json!({"content": text}))
        .timeout(Duration::from_secs(15));
    if !provider.api_key().is_empty() {
        req = req.bearer_auth(provider.api_key());
    }
    let response = req.send().await.map_err(|err| {
        ProviderError::new(ErrorClass::Connection, format!("/tokenize failed: {err}"))
    })?;
    if !response.status().is_success() {
        return Err(ProviderError::new(
            ErrorClass::BadRequest,
            format!("/tokenize returned {}", response.status()),
        ));
    }
    let value: serde_json::Value = response.json().await.map_err(|err| {
        ProviderError::new(ErrorClass::Parse, format!("/tokenize is not JSON: {err}"))
    })?;
    if let Some(tokens) = value["tokens"].as_array() {
        return Ok(tokens
            .iter()
            .filter_map(|t| t.as_u64().map(|n| n as u32))
            .collect());
    }
    if let Some(count) = value["count"].as_u64() {
        return Ok(vec![0; count as usize]);
    }
    Err(ProviderError::new(
        ErrorClass::Parse,
        "/tokenize returned no token array",
    ))
}

/// Exact token count when the engine supports `/tokenize`.
pub async fn token_count(provider: &dyn Provider, text: &str) -> Option<usize> {
    match tokenize(provider, text).await {
        Ok(tokens) => Some(tokens.len()),
        Err(_) => None,
    }
}

/// `GET /api/show` on Ollama.
pub async fn ollama_show(
    provider: &dyn Provider,
    model: &str,
) -> Result<OllamaModel, ProviderError> {
    let (client, urls) = client_and_urls(provider)?;
    let url = format!("{}/api/show", urls.root);
    let mut req = client
        .post(&url)
        .json(&json!({"name": model}))
        .timeout(Duration::from_secs(10));
    if !provider.api_key().is_empty() {
        req = req.bearer_auth(provider.api_key());
    }
    let response = req.send().await.map_err(|err| {
        ProviderError::new(ErrorClass::Connection, format!("/api/show failed: {err}"))
    })?;
    if !response.status().is_success() {
        return Err(ProviderError::new(
            ErrorClass::BadRequest,
            format!("/api/show returned {}", response.status()),
        ));
    }
    let value: serde_json::Value = response.json().await.map_err(|err| {
        ProviderError::new(ErrorClass::Parse, format!("/api/show is not JSON: {err}"))
    })?;
    let context_length = value["model_info"]
        .as_object()
        .and_then(|info| {
            info.iter()
                .find(|(key, _)| key.ends_with(".context_length"))
                .and_then(|(_, value)| value.as_u64())
        })
        .map(|n| n as u32);
    let num_ctx = value["parameters"].as_str().and_then(|params| {
        params.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next() == Some("num_ctx"))
                .then(|| parts.next()?.parse::<u32>().ok())
                .flatten()
        })
    });
    let family = value["details"]["family"].as_str().map(str::to_string);
    Ok(OllamaModel {
        num_ctx,
        context_length,
        family,
    })
}

/// Probe what the endpoint supports, with a short timeout per probe.
pub async fn detect(provider: &dyn Provider) -> Capabilities {
    let mut caps = Capabilities::default();
    let mut joined = Vec::new();
    if let Ok(models) = list_models(provider).await {
        caps.models = true;
        joined.push(models.join(", "));
    }
    if let Ok(props) = llama_props(provider).await {
        caps.props = true;
        caps.native_tools = props.chat_template.is_some();
    }
    if token_count(provider, "hello").await.is_some() {
        caps.tokenize = true;
    }
    if ollama_show(provider, provider.model()).await.is_ok() {
        caps.ollama_show = true;
    }
    caps.usage = true;
    caps
}

/// Per-slot `n_ctx` from llama.cpp `GET /slots`: the real per-request limit
/// when the server runs several parallel slots.
pub async fn slots_window(provider: &dyn Provider) -> Option<u32> {
    let (_, urls) = client_and_urls(provider).ok()?;
    let url = format!("{}/slots", urls.root);
    let response = request_with_timeout(provider, &url, Duration::from_secs(3))
        .ok()?
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value: serde_json::Value = response.json().await.ok()?;
    value
        .as_array()?
        .iter()
        .find_map(|slot| positive(&slot["n_ctx"]))
}

/// The window of whatever model the server has loaded right now. Only
/// meaningful right after that model answered: behind a gateway, `/slots`
/// and `/props` describe the loaded backend, not a requested one.
pub async fn served_window(provider: &dyn Provider) -> Option<u32> {
    if let Some(n) = slots_window(provider).await {
        return Some(n);
    }
    llama_props(provider)
        .await
        .ok()
        .and_then(|props| props.n_ctx)
        .filter(|n| *n > 0)
}

/// The context size in a llama.cpp overflow error, e.g. `"n_ctx":32768` or
/// `exceeds the available context size (32768 tokens)`.
pub fn window_from_overflow(message: &str) -> Option<u32> {
    let json = regex::Regex::new(r#""n_ctx"\s*:\s*(\d+)"#).ok()?;
    let prose = regex::Regex::new(r"context size \((\d+) tokens\)").ok()?;
    json.captures(message)
        .or_else(|| prose.captures(message))
        .and_then(|caps| caps[1].parse::<u32>().ok())
        .filter(|n| *n > 0)
}

/// `GET /api/ps` on Ollama: the context the loaded runner for `model` uses.
pub async fn ollama_loaded_context(provider: &dyn Provider, model: &str) -> Option<u32> {
    let (_, urls) = client_and_urls(provider).ok()?;
    let url = format!("{}/api/ps", urls.root);
    let response = request(provider, &url).ok()?.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let value: serde_json::Value = response.json().await.ok()?;
    value["models"]
        .as_array()?
        .iter()
        .find(|item| item["name"].as_str() == Some(model) || item["model"].as_str() == Some(model))
        .and_then(|item| positive(&item["context_length"]))
}

/// Where a probed context window came from, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowProbe {
    /// The model's entry in `/v1/models`.
    ModelList,
    /// llama.cpp `/props` `n_ctx`.
    LlamaProps,
    /// Ollama's loaded runner or `num_ctx` parameter.
    Ollama,
    /// Ollama's trained maximum; the served window may be smaller.
    OllamaTrained,
    /// Learned from the server during a turn: llama.cpp `/slots` or `/props`
    /// read right after the model answered (a gateway then has it loaded),
    /// or the `n_ctx` in an overflow error.
    Served,
    /// Remembered from earlier in this process for the same model.
    Remembered,
}

impl WindowProbe {
    pub fn label(self) -> &'static str {
        match self {
            WindowProbe::ModelList => "/v1/models",
            WindowProbe::LlamaProps => "llama.cpp /props",
            WindowProbe::Ollama => "ollama",
            WindowProbe::OllamaTrained => "ollama trained max",
            WindowProbe::Served => "detected from the server",
            WindowProbe::Remembered => "detected earlier",
        }
    }
}

/// True when llama.cpp `/props` describes `model`. A gateway or a llama.cpp
/// router in front of several models answers `/props` for whichever model is
/// loaded, so with several models listed the path must name the selected one.
fn props_match(props: &LlamaProps, model: &str, listed: &[String]) -> bool {
    if listed.len() <= 1 {
        return true;
    }
    let Some(path) = &props.model_path else {
        return false;
    };
    let stem = std::path::Path::new(path)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or(path)
        .to_ascii_lowercase();
    let model = model.to_ascii_lowercase();
    let model_tail = model.rsplit('/').next().unwrap_or(&model);
    !stem.is_empty() && (stem == model || stem.contains(model_tail) || model_tail.contains(&stem))
}

/// Best-effort context window for the provider's selected model.
pub async fn probe_context_window(provider: &dyn Provider) -> Option<(u32, WindowProbe)> {
    let model = provider.model().trim().to_string();
    if model.is_empty() {
        return None;
    }
    let listing = models_json(provider, Duration::from_secs(10)).await.ok();
    let listed = listing.as_ref().map(model_ids).unwrap_or_default();
    if let Some(window) = listing
        .as_ref()
        .and_then(|value| window_from_models(value, &model))
    {
        return Some((window, WindowProbe::ModelList));
    }
    if let Ok(props) = llama_props(provider).await
        && let Some(n) = props.n_ctx.filter(|n| *n > 0)
        && props_match(&props, &model, &listed)
    {
        return Some((n, WindowProbe::LlamaProps));
    }
    // `/slots` names no model, so it is only unambiguous for one model.
    if listed.len() <= 1
        && let Some(n) = slots_window(provider).await
    {
        return Some((n, WindowProbe::LlamaProps));
    }
    if let Some(n) = ollama_loaded_context(provider, &model).await {
        return Some((n, WindowProbe::Ollama));
    }
    if let Ok(info) = ollama_show(provider, &model).await {
        if let Some(n) = info.num_ctx {
            return Some((n, WindowProbe::Ollama));
        }
        if let Some(n) = info.context_length {
            return Some((n, WindowProbe::OllamaTrained));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_list_window_matches_the_selected_entry() {
        let body = json!({"data": [
            {"id": "a", "max_model_len": 8192},
            {"id": "b", "context_length": 200000, "max_model_len": 0},
            {"id": "c", "meta": {"n_ctx_train": 131072}},
            {"id": "d", "loaded_context_length": 16384, "max_context_length": 131072},
        ]});
        assert_eq!(window_from_models(&body, "a"), Some(8192));
        assert_eq!(window_from_models(&body, "b"), Some(200000));
        assert_eq!(window_from_models(&body, "c"), None);
        assert_eq!(window_from_models(&body, "d"), Some(16384));
        assert_eq!(window_from_models(&body, "zzz"), None);
    }

    #[test]
    fn props_only_trusted_for_the_selected_model_behind_a_gateway() {
        let props = LlamaProps {
            n_ctx: Some(4096),
            model_path: Some("/models/Qwen3-8B-Q4_K_M.gguf".into()),
            ..LlamaProps::default()
        };
        let one = vec!["anything".to_string()];
        let many = vec!["qwen3-8b".to_string(), "llama-70b".to_string()];
        assert!(props_match(&props, "anything", &one));
        assert!(props_match(&props, "qwen3-8b", &many));
        assert!(!props_match(&props, "llama-70b", &many));
        let bare = LlamaProps {
            n_ctx: Some(4096),
            ..LlamaProps::default()
        };
        assert!(!props_match(&bare, "llama-70b", &many));
    }

    #[test]
    fn overflow_errors_reveal_the_window() {
        let body = r#"Server said: {"error":{"code":400,"message":"request (40000 tokens) exceeds the available context size (32768 tokens), try increasing it","type":"exceed_context_size_error","n_prompt_tokens":40000,"n_ctx":32768}}"#;
        assert_eq!(window_from_overflow(body), Some(32768));
        assert_eq!(
            window_from_overflow("exceeds the available context size (8192 tokens)"),
            Some(8192)
        );
        assert_eq!(window_from_overflow("context length exceeded"), None);
    }

    #[test]
    fn capabilities_default_to_false() {
        let caps = Capabilities::default();
        assert!(!caps.models);
        assert!(!caps.tokenize);
    }
}

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
    pub chat_template: Option<String>,
    pub slots: Option<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct OllamaModel {
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
    let value: serde_json::Value = response.json().await.map_err(|err| {
        ProviderError::new(ErrorClass::Parse, format!("/v1/models is not JSON: {err}"))
    })?;
    let models = value["data"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item["id"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(models)
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
    Ok(LlamaProps {
        n_ctx,
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
        .map(|n| n as u32)
        .or_else(|| value["parameters_num_ctx"].as_u64().map(|n| n as u32));
    let family = value["details"]["family"].as_str().map(str::to_string);
    Ok(OllamaModel {
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

/// Best-effort context window from probes.
pub async fn probe_context_window(provider: &dyn Provider) -> Option<u32> {
    if let Ok(props) = llama_props(provider).await
        && let Some(n) = props.n_ctx
    {
        return Some(n);
    }
    if let Ok(model) = ollama_show(provider, provider.model()).await {
        return model.context_length;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_default_to_false() {
        let caps = Capabilities::default();
        assert!(!caps.models);
        assert!(!caps.tokenize);
    }
}

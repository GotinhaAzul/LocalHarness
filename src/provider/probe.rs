//! Optional capability probes. None of these are required; failures degrade.

use std::time::Duration;

use serde::{Deserialize, Serialize};
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
    /// Server build string when the build reports one.
    pub build_info: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct OllamaModel {
    /// `num_ctx` from the Modelfile parameters: what Ollama actually serves.
    pub num_ctx: Option<u32>,
    /// `<arch>.context_length`: the trained maximum, usually larger than the
    /// window Ollama allocates.
    pub context_length: Option<u32>,
    pub family: Option<String>,
    /// `capabilities` from `/api/show` (for example `completion`, `tools`).
    pub capabilities: Vec<String>,
}

/// The three context sizes an engine can report, resolved into one budget.
///
/// * `trained` — the model's trained maximum from metadata.
/// * `allocated` — what the server reserved (`--ctx-size`, `num_ctx`,
///   `max_model_len`, ...).
/// * `per_slot` — what one request may use when the server splits the
///   allocation across parallel slots.
///
/// `effective` is the smallest present value: the only number prompts can
/// safely be budgeted against. `0` means no value was reported; callers then
/// fall back to configuration or a default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowTruth {
    pub trained: Option<u32>,
    pub allocated: Option<u32>,
    pub per_slot: Option<u32>,
    pub effective: u32,
    pub trained_source: Option<String>,
    pub allocated_source: Option<String>,
    pub per_slot_source: Option<String>,
}

impl WindowTruth {
    /// Resolve the three values into an effective window.
    pub fn resolve(trained: Option<u32>, allocated: Option<u32>, per_slot: Option<u32>) -> Self {
        Self {
            trained,
            allocated,
            per_slot,
            effective: effective_window(trained, allocated, per_slot).unwrap_or(0),
            trained_source: None,
            allocated_source: None,
            per_slot_source: None,
        }
    }

    /// True when the engine reported at least one usable context size.
    pub fn is_known(&self) -> bool {
        self.effective > 0
    }

    /// Sources keyed by field name, for storing in a session header.
    pub fn source_map(&self) -> std::collections::BTreeMap<String, String> {
        let mut map = std::collections::BTreeMap::new();
        if let Some(source) = &self.trained_source {
            map.insert("trained".into(), source.clone());
        }
        if let Some(source) = &self.allocated_source {
            map.insert("allocated".into(), source.clone());
        }
        if let Some(source) = &self.per_slot_source {
            map.insert("per_slot".into(), source.clone());
        }
        map
    }
}

/// The smallest positive context size among the values an engine reported.
/// `None` when nothing was reported: never fabricate a default here.
pub fn effective_window(
    trained: Option<u32>,
    allocated: Option<u32>,
    per_slot: Option<u32>,
) -> Option<u32> {
    trained
        .into_iter()
        .chain(allocated)
        .chain(per_slot)
        .filter(|n| *n > 0)
        .min()
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

/// `GET` a JSON metadata endpoint. Shared with `lh doctor`.
pub(crate) async fn get_json_with_timeout(
    provider: &dyn Provider,
    url: &str,
    timeout: Duration,
) -> Result<serde_json::Value, ProviderError> {
    let response = request_with_timeout(provider, url, timeout)?
        .send()
        .await
        .map_err(|err| {
            ProviderError::new(ErrorClass::Connection, format!("GET {url} failed: {err}"))
        })?;
    if !response.status().is_success() {
        return Err(ProviderError::new(
            ErrorClass::BadRequest,
            format!("GET {url} returned {}", response.status()),
        ));
    }
    response.json().await.map_err(|err| {
        ProviderError::new(ErrorClass::Parse, format!("GET {url} is not JSON: {err}"))
    })
}

/// `GET` a metadata endpoint and report only whether it answered successfully.
/// Used for bodyless health checks (`/health`).
pub(crate) async fn get_ok_with_timeout(
    provider: &dyn Provider,
    url: &str,
    timeout: Duration,
) -> bool {
    let Ok(request) = request_with_timeout(provider, url, timeout) else {
        return false;
    };
    request
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
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
///
/// Kept as the single-number view for diagnostics; [`probe_context_truth`]
/// reports the trained, allocated, and per-slot values separately.
#[cfg_attr(not(test), allow(dead_code))]
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

/// One reported context size and where it came from.
#[derive(Debug, Clone, Copy)]
struct Part {
    value: u32,
    source: &'static str,
}

/// Trained and allocated sizes from the selected entry in a `/v1/models`
/// body. `max_context_length` is LM Studio's trained maximum; the loaded and
/// runtime keys are what the server actually allocated.
fn truth_from_models(value: &serde_json::Value, model: &str) -> (Option<Part>, Option<Part>) {
    let Some(entry) = value["data"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["id"].as_str() == Some(model)))
    else {
        return (None, None);
    };
    let trained = positive(&entry["meta"]["n_ctx_train"])
        .map(|value| Part {
            value,
            source: "/v1/models meta.n_ctx_train",
        })
        .or_else(|| {
            positive(&entry["max_context_length"]).map(|value| Part {
                value,
                source: "/v1/models max_context_length",
            })
        });
    let allocated = [
        ("loaded_context_length", "/v1/models loaded_context_length"),
        ("max_model_len", "vllm /v1/models max_model_len"),
        ("context_length", "/v1/models context_length"),
        ("context_window", "/v1/models context_window"),
    ]
    .into_iter()
    .find_map(|(key, source)| positive(&entry[key]).map(|value| Part { value, source }))
    .or_else(|| {
        positive(&entry["top_provider"]["context_length"]).map(|value| Part {
            value,
            source: "/v1/models top_provider.context_length",
        })
    })
    .or_else(|| {
        positive(&entry["max_input_tokens"]).map(|value| Part {
            value,
            source: "/v1/models max_input_tokens",
        })
    });
    (trained, allocated)
}

/// Accumulates the first value reported for each of the three context sizes.
/// Earlier probes win, so engine-specific endpoints can refine a generic
/// `/v1/models` answer without overwriting it.
#[derive(Default)]
struct TruthBuilder {
    trained: Option<Part>,
    allocated: Option<Part>,
    per_slot: Option<Part>,
}

impl TruthBuilder {
    fn trained(&mut self, value: Option<Part>) {
        if self.trained.is_none() {
            self.trained = value;
        }
    }

    fn allocated(&mut self, value: Option<Part>) {
        if self.allocated.is_none() {
            self.allocated = value;
        }
    }

    fn per_slot(&mut self, value: Option<Part>) {
        if self.per_slot.is_none() {
            self.per_slot = value;
        }
    }

    fn finish(self) -> WindowTruth {
        WindowTruth {
            trained: self.trained.map(|part| part.value),
            allocated: self.allocated.map(|part| part.value),
            per_slot: self.per_slot.map(|part| part.value),
            effective: effective_window(
                self.trained.map(|part| part.value),
                self.allocated.map(|part| part.value),
                self.per_slot.map(|part| part.value),
            )
            .unwrap_or(0),
            trained_source: self.trained.map(|part| part.source.to_string()),
            allocated_source: self.allocated.map(|part| part.source.to_string()),
            per_slot_source: self.per_slot.map(|part| part.source.to_string()),
        }
    }
}

/// The selected model's entry in an LM Studio `/api/v1/models` body. The body
/// keys the model list as `data` (newer) or `models` (older).
fn lmstudio_entry<'a>(value: &'a serde_json::Value, model: &str) -> Option<&'a serde_json::Value> {
    let items = value["data"]
        .as_array()
        .or_else(|| value["models"].as_array())?;
    items
        .iter()
        .find(|item| item["id"].as_str() == Some(model))
        .or_else(|| items.first())
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
    let build_info = value["build_info"]
        .as_str()
        .or_else(|| value["version"].as_str())
        .map(str::to_string);
    Ok(LlamaProps {
        n_ctx,
        model_path,
        chat_template,
        slots,
        build_info,
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
    let num_ctx = value["parameters"]
        .as_str()
        .and_then(|params| {
            params.lines().find_map(|line| {
                let mut parts = line.split_whitespace();
                (parts.next() == Some("num_ctx"))
                    .then(|| parts.next()?.parse::<u32>().ok())
                    .flatten()
            })
        })
        .or_else(|| {
            value["parameters"]["num_ctx"]
                .as_u64()
                .map(|n| n.min(u32::MAX as u64) as u32)
        })
        .or_else(|| {
            value["parameters"]["num_ctx"]
                .as_str()
                .and_then(|n| n.parse::<u32>().ok())
        });
    let family = value["details"]["family"].as_str().map(str::to_string);
    let capabilities = value["capabilities"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(OllamaModel {
        num_ctx,
        context_length,
        family,
        capabilities,
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
    /// llama.cpp `/slots` `n_ctx` for one slot.
    Slots,
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
            WindowProbe::Slots => "llama.cpp /slots",
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

/// Every context size the endpoint reports, with the source of each value.
///
/// Probes are metadata-only and best-effort: llama.cpp `/props` and `/slots`,
/// `/v1/models` entries, Ollama `/api/ps` and `/api/show`, SGLang
/// `/server_info`, LM Studio `/api/v1/models`, and TabbyAPI `/v1/model`.
/// `None` means nothing usable was reported; no default is fabricated here
/// (Ollama's documented 4096-token default is the one exception, marked with
/// the `"ollama default"` source so callers can warn).
pub async fn probe_context_truth(provider: &dyn Provider) -> Option<WindowTruth> {
    let model = provider.model().trim().to_string();
    if model.is_empty() {
        return None;
    }
    let urls = endpoint_urls(provider.endpoint());
    let mut truth = TruthBuilder::default();

    // `/v1/models` is the only probe every OpenAI-compatible server has.
    let listing = models_json(provider, Duration::from_secs(10)).await.ok();
    let listed = listing.as_ref().map(model_ids).unwrap_or_default();
    if let Some(value) = listing.as_ref() {
        let (trained, allocated) = truth_from_models(value, &model);
        truth.trained(trained);
        truth.allocated(allocated);
    }

    // llama.cpp: `n_ctx` is per slot when the server runs parallel slots.
    // `/slots` is the direct per-request value; prefer it over `/props`.
    let mut llama_props_ctx: Option<(u32, Option<u32>)> = None;
    if let Ok(props) = llama_props(provider).await
        && props_match(&props, &model, &listed)
        && let Some(n) = props.n_ctx.filter(|n| *n > 0)
    {
        llama_props_ctx = Some((n, props.slots.filter(|slots| *slots > 1)));
    }

    // `/slots` names no model, so it is only unambiguous for one model.
    let slots_n = if listed.len() <= 1 {
        slots_window(provider).await
    } else {
        None
    };
    if let Some(n) = slots_n {
        truth.per_slot(Some(Part {
            value: n,
            source: "llama.cpp /slots n_ctx",
        }));
        match llama_props_ctx.and_then(|(_, slots)| slots) {
            Some(slots) => truth.allocated(Some(Part {
                value: n.saturating_mul(slots),
                source: "llama.cpp /slots n_ctx × slots",
            })),
            None if truth.allocated.is_none() => truth.allocated(Some(Part {
                value: n,
                source: "llama.cpp /slots n_ctx",
            })),
            None => {}
        }
    } else if let Some((n, slots)) = llama_props_ctx {
        match slots {
            Some(slots) => {
                truth.per_slot(Some(Part {
                    value: n,
                    source: "llama.cpp /props n_ctx (per slot)",
                }));
                truth.allocated(Some(Part {
                    value: n.saturating_mul(slots),
                    source: "llama.cpp /props n_ctx × slots",
                }));
            }
            None => truth.allocated(Some(Part {
                value: n,
                source: "llama.cpp /props n_ctx",
            })),
        }
    }

    // Ollama: the loaded runner's context beats the Modelfile parameter.
    if truth.allocated.is_none()
        && let Some(n) = ollama_loaded_context(provider, &model).await
    {
        truth.allocated(Some(Part {
            value: n,
            source: "ollama /api/ps context_length",
        }));
    }
    if let Ok(info) = ollama_show(provider, &model).await {
        truth.allocated(info.num_ctx.map(|value| Part {
            value,
            source: "ollama /api/show num_ctx",
        }));
        truth.trained(info.context_length.map(|value| Part {
            value,
            source: "ollama /api/show model_info",
        }));
        if truth.allocated.is_none() {
            truth.allocated(Some(Part {
                value: 4096,
                source: "ollama default",
            }));
        }
    }

    // SGLang reports the server-wide context on `/server_info`.
    if truth.allocated.is_none()
        && let Ok(value) = get_json_with_timeout(
            provider,
            &format!("{}/server_info", urls.root),
            Duration::from_secs(3),
        )
        .await
        && let Some(n) =
            positive(&value["max_context_length"]).or_else(|| positive(&value["context_length"]))
    {
        truth.allocated(Some(Part {
            value: n,
            source: "sglang /server_info max_context_length",
        }));
    }

    // LM Studio separates the loaded window from the model maximum.
    if (truth.allocated.is_none() || truth.trained.is_none())
        && let Ok(value) = get_json_with_timeout(
            provider,
            &format!("{}/api/v1/models", urls.root),
            Duration::from_secs(3),
        )
        .await
        && let Some(entry) = lmstudio_entry(&value, &model)
    {
        truth.allocated(positive(&entry["loaded_context_length"]).map(|value| Part {
            value,
            source: "lmstudio /api/v1/models loaded_context_length",
        }));
        truth.trained(positive(&entry["max_context_length"]).map(|value| Part {
            value,
            source: "lmstudio /api/v1/models max_context_length",
        }));
    }

    // TabbyAPI exposes the configured sequence length on `/v1/model`.
    if truth.allocated.is_none()
        && let Ok(value) = get_json_with_timeout(
            provider,
            &format!("{}/model", urls.api),
            Duration::from_secs(3),
        )
        .await
        && let Some(n) = positive(&value["parameters"]["max_seq_len"])
            .or_else(|| positive(&value["max_seq_len"]))
    {
        truth.allocated(Some(Part {
            value: n,
            source: "tabbyapi /v1/model max_seq_len",
        }));
    }

    let truth = truth.finish();
    truth.is_known().then_some(truth)
}

/// Pick the display source that produced the effective window.
fn window_probe_for(truth: &WindowTruth) -> Option<WindowProbe> {
    let effective = truth.effective;
    if effective == 0 {
        return None;
    }
    if truth.per_slot == Some(effective) {
        return Some(match truth.per_slot_source.as_deref() {
            Some(source) if source.starts_with("llama.cpp /slots") => WindowProbe::Slots,
            _ => WindowProbe::LlamaProps,
        });
    }
    if truth.allocated == Some(effective) {
        return Some(match truth.allocated_source.as_deref() {
            Some(source) if source.contains("/v1/models") => WindowProbe::ModelList,
            Some(source) if source.starts_with("llama.cpp") => WindowProbe::LlamaProps,
            Some(source) if source.starts_with("ollama") => WindowProbe::Ollama,
            _ => WindowProbe::Served,
        });
    }
    if truth.trained == Some(effective) {
        return match truth.trained_source.as_deref() {
            Some(source) if source.contains("meta.n_ctx_train") => None,
            Some(source) if source.contains("/v1/models") => Some(WindowProbe::ModelList),
            Some(source) if source.starts_with("ollama") => Some(WindowProbe::OllamaTrained),
            // A trained maximum with no allocated value is a guess; do not
            // present it as a measured window.
            _ => None,
        };
    }
    None
}

/// Best-effort context window for the provider's selected model.
///
/// Kept for callers that only need one number; [`probe_context_truth`] also
/// returns the trained, allocated, and per-slot values and their sources.
pub async fn probe_context_window(provider: &dyn Provider) -> Option<(u32, WindowProbe)> {
    let truth = probe_context_truth(provider).await?;
    let probe = window_probe_for(&truth)?;
    Some((truth.effective, probe))
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

    #[test]
    fn effective_window_is_the_smallest_reported_value() {
        // llama.cpp --parallel 2 --ctx-size 8192: /props n_ctx is 4096 per slot.
        let llama = WindowTruth::resolve(Some(131_072), Some(8192), Some(4096));
        assert_eq!(llama.effective, 4096);
        assert_eq!(
            effective_window(Some(131_072), Some(8192), Some(4096)),
            Some(4096)
        );

        // Ollama: the trained maximum is much larger than the served window.
        let ollama = WindowTruth::resolve(Some(131_072), Some(4096), None);
        assert_eq!(ollama.effective, 4096);

        // vLLM: max_model_len is the allocation.
        let vllm = WindowTruth::resolve(None, Some(32_768), None);
        assert_eq!(vllm.effective, 32_768);

        // Nothing reported: no fabricated default.
        assert_eq!(effective_window(None, None, None), None);
        assert_eq!(WindowTruth::resolve(None, None, None).effective, 0);
        assert!(!WindowTruth::default().is_known());
    }

    #[test]
    fn model_entry_separates_trained_from_allocated() {
        let body = json!({"data": [
            {"id": "llama", "meta": {"n_ctx_train": 131072}},
            {"id": "lmstudio", "loaded_context_length": 8192, "max_context_length": 131072},
            {"id": "vllm", "max_model_len": 32768},
        ]});
        let (trained, allocated) = truth_from_models(&body, "llama");
        assert_eq!(trained.map(|part| part.value), Some(131_072));
        assert_eq!(
            trained.map(|part| part.source),
            Some("/v1/models meta.n_ctx_train")
        );
        assert!(allocated.is_none());

        let (trained, allocated) = truth_from_models(&body, "lmstudio");
        assert_eq!(trained.map(|part| part.value), Some(131_072));
        assert_eq!(allocated.map(|part| part.value), Some(8192));
        assert_eq!(
            allocated.map(|part| part.source),
            Some("/v1/models loaded_context_length")
        );

        let (trained, allocated) = truth_from_models(&body, "vllm");
        assert!(trained.is_none());
        assert_eq!(allocated.map(|part| part.value), Some(32_768));
        assert_eq!(
            allocated.map(|part| part.source),
            Some("vllm /v1/models max_model_len")
        );
    }

    #[test]
    fn window_probe_tracks_the_limiting_value() {
        let llama = WindowTruth {
            trained: Some(131_072),
            allocated: Some(8192),
            per_slot: Some(4096),
            effective: 4096,
            trained_source: Some("/v1/models meta.n_ctx_train".into()),
            allocated_source: Some("llama.cpp /props n_ctx × slots".into()),
            per_slot_source: Some("llama.cpp /slots n_ctx".into()),
        };
        assert_eq!(window_probe_for(&llama), Some(WindowProbe::Slots));

        let ollama_trained = WindowTruth::resolve(Some(131_072), None, None);
        let ollama_trained = WindowTruth {
            trained_source: Some("ollama /api/show model_info".into()),
            ..ollama_trained
        };
        assert_eq!(
            window_probe_for(&ollama_trained),
            Some(WindowProbe::OllamaTrained)
        );

        let llama_trained = WindowTruth {
            trained: Some(131_072),
            effective: 131_072,
            trained_source: Some("/v1/models meta.n_ctx_train".into()),
            ..WindowTruth::default()
        };
        assert_eq!(window_probe_for(&llama_trained), None);

        let lmstudio_max = WindowTruth {
            trained: Some(131_072),
            effective: 131_072,
            trained_source: Some("/v1/models max_context_length".into()),
            ..WindowTruth::default()
        };
        assert_eq!(
            window_probe_for(&lmstudio_max),
            Some(WindowProbe::ModelList)
        );
    }

    #[test]
    fn source_map_names_present_values() {
        let truth = WindowTruth {
            trained: Some(131_072),
            allocated: Some(8192),
            per_slot: Some(4096),
            effective: 4096,
            trained_source: Some("/v1/models meta.n_ctx_train".into()),
            allocated_source: Some("llama.cpp /props n_ctx × slots".into()),
            per_slot_source: Some("llama.cpp /props n_ctx (per slot)".into()),
        };
        let map = truth.source_map();
        assert_eq!(map.len(), 3);
        assert_eq!(map["per_slot"], "llama.cpp /props n_ctx (per slot)");
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    fn provider_for(endpoint: String) -> crate::provider::OpenAiCompatProvider {
        let config = crate::config::ModelConfig {
            endpoint,
            ..Default::default()
        };
        crate::provider::OpenAiCompatProvider::new(&config, "qwen3").unwrap()
    }

    #[tokio::test]
    async fn llama_parallel_slots_budget_against_the_per_slot_window() {
        use axum::routing::get;
        let app = axum::Router::new()
            .route(
                "/props",
                get(|| async {
                    axum::Json(json!({
                        "default_generation_settings": {"n_ctx": 4096},
                        "total_slots": 2,
                        "model_path": "/models/qwen3.gguf",
                        "chat_template": "tool-aware template"
                    }))
                }),
            )
            .route(
                "/v1/models",
                get(|| async {
                    axum::Json(json!({
                        "object": "list",
                        "data": [{"id": "qwen3", "meta": {"n_ctx_train": 131072}}]
                    }))
                }),
            );
        let provider = provider_for(serve(app).await);
        let truth = probe_context_truth(&provider).await.expect("truth");
        assert_eq!(truth.trained, Some(131_072));
        assert_eq!(truth.allocated, Some(8192));
        assert_eq!(truth.per_slot, Some(4096));
        assert_eq!(truth.effective, 4096);
        assert_eq!(window_probe_for(&truth), Some(WindowProbe::LlamaProps));
    }

    #[tokio::test]
    async fn ollama_without_a_served_window_reports_its_default() {
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/api/show",
            post(|| async {
                axum::Json(json!({
                    "parameters": "",
                    "model_info": {"qwen3.context_length": 131072},
                    "details": {"family": "qwen3"}
                }))
            }),
        );
        let provider = provider_for(serve(app).await);
        let truth = probe_context_truth(&provider).await.expect("truth");
        assert_eq!(truth.trained, Some(131_072));
        assert_eq!(truth.allocated, Some(4096));
        assert_eq!(truth.allocated_source.as_deref(), Some("ollama default"));
        assert_eq!(truth.effective, 4096);
    }
}

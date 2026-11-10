//! `lh doctor`: a stable capability manifest for the configured engine.
//!
//! The manifest is assembled from metadata endpoints only. It never sends a
//! chat completion and never loads a model. Unknown engines fall back to
//! conservative `generic` defaults and list the probes that failed.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::provider::probe::{self, LlamaProps, OllamaModel, ToolProbe, WindowTruth};
use crate::provider::{Provider, endpoint_urls};

/// Timeout for one metadata probe. Long enough for a local engine to answer,
/// short enough that an unknown endpoint does not stall `lh doctor`.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// What the configured engine can do, as reported by its own metadata.
///
/// The JSON shape is stable: fields are always present; unknown optional
/// capabilities are `null`, never omitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoctorManifest {
    /// `llama.cpp`, `ollama`, `vllm`, `sglang`, `lmstudio`, `koboldcpp`,
    /// `tabbyapi`, or `generic`.
    pub engine: String,
    pub build: Option<String>,
    pub model: String,
    pub context: WindowTruth,
    /// `native`, `repair`, `none`, or `unknown`.
    pub tool_dialect: String,
    pub tool_parser: Option<String>,
    /// Last forced-tool conformance probe for this model/build. `null` when
    /// the TUI/headless runtime has not run one or `lh doctor` has no cached
    /// result. `lh doctor` itself never sends a chat completion.
    #[serde(default)]
    pub tool_probe: Option<ToolProbe>,
    pub reasoning_field: Option<String>,
    /// `json_schema`, `grammar`, `none`, or `unknown`.
    pub structured_output: String,
    pub cache_mode: Option<String>,
    pub slots: Option<u32>,
    pub tokenizer_route: Option<String>,
    pub embeddings: bool,
    pub rerank: bool,
    /// Metadata probes that answered.
    pub probes_ok: Vec<String>,
    /// Metadata probes that failed or did not identify an engine.
    pub probes_failed: Vec<String>,
    pub warnings: Vec<String>,
    pub fixes: Vec<String>,
}

impl DoctorManifest {
    /// Fingerprint the engine and describe its capabilities.
    ///
    /// Probes are tried in this documented order and stop at the first match:
    /// llama.cpp `/props`, Ollama `/api/show`, vLLM `/v1/models` with
    /// `max_model_len` plus `/health`, SGLang `/server_info`, LM Studio
    /// `/api/v1/models`, KoboldCpp `/api/extra/version`, TabbyAPI `/v1/model`,
    /// then `generic`. `/tokenize` is probed last to record whether exact
    /// token counts are available.
    pub async fn probe(provider: &dyn Provider, configured_window: u32) -> Self {
        let model = provider.model().trim().to_string();
        let urls = endpoint_urls(provider.endpoint());

        let mut probes_ok: Vec<String> = Vec::new();
        let mut probes_failed: Vec<String> = Vec::new();
        let mut engine = "generic".to_string();
        let mut build = None;
        let mut slots = None;
        let mut tool_dialect = "unknown".to_string();
        let mut tool_parser = None;
        let reasoning_field = None;
        let structured_output = "unknown".to_string();
        let mut cache_mode = None;
        let mut tokenizer_route = None;
        let mut embeddings = false;
        let mut rerank = false;

        let mut detected = false;
        let mut props: Option<LlamaProps> = None;
        let mut ollama: Option<OllamaModel> = None;
        let mut engine_json: Option<serde_json::Value> = None;

        // 1. llama.cpp answers `/props` with its loaded model and slots.
        match probe::llama_props(provider).await {
            Ok(value) => {
                probes_ok.push("llama.cpp /props".into());
                props = Some(value);
                engine = "llama.cpp".into();
                detected = true;
            }
            Err(_) => probes_failed.push("llama.cpp /props".into()),
        }

        // 2. Ollama's `/api/show` is the only metadata endpoint it always has.
        if !detected {
            match probe::ollama_show(provider, &model).await {
                Ok(value) => {
                    probes_ok.push("ollama /api/show".into());
                    ollama = Some(value);
                    engine = "ollama".into();
                    detected = true;
                }
                Err(_) => probes_failed.push("ollama /api/show".into()),
            }
        }

        // 3. vLLM: `max_model_len` in the selected model entry, plus `/health`.
        if !detected {
            match probe::get_json_with_timeout(
                provider,
                &format!("{}/models", urls.api),
                PROBE_TIMEOUT,
            )
            .await
            {
                Ok(value) => {
                    let has_max_len = selected_entry(&value, &model)
                        .is_some_and(|entry| positive(&entry["max_model_len"]).is_some());
                    if has_max_len
                        && probe::get_ok_with_timeout(
                            provider,
                            &format!("{}/health", urls.root),
                            PROBE_TIMEOUT,
                        )
                        .await
                    {
                        probes_ok.push("vllm /v1/models max_model_len".into());
                        probes_ok.push("vllm /health".into());
                        engine = "vllm".into();
                        detected = true;
                        engine_json = Some(value);
                    } else if has_max_len {
                        probes_failed.push("vllm /health".into());
                    } else {
                        probes_failed.push("vllm /v1/models max_model_len".into());
                    }
                }
                Err(_) => probes_failed.push("vllm /v1/models max_model_len".into()),
            }
        }

        // 4. SGLang's `/server_info`.
        if !detected {
            match probe::get_json_with_timeout(
                provider,
                &format!("{}/server_info", urls.root),
                PROBE_TIMEOUT,
            )
            .await
            {
                Ok(value)
                    if value.get("max_context_length").is_some()
                        || value.get("context_length").is_some()
                        || value.get("version").is_some() =>
                {
                    probes_ok.push("sglang /server_info".into());
                    engine = "sglang".into();
                    detected = true;
                    engine_json = Some(value);
                }
                _ => probes_failed.push("sglang /server_info".into()),
            }
        }

        // 5. LM Studio's REST model list separates loaded and maximum windows.
        if !detected {
            match probe::get_json_with_timeout(
                provider,
                &format!("{}/api/v1/models", urls.root),
                PROBE_TIMEOUT,
            )
            .await
            {
                Ok(value) if lmstudio_signature(&value, &model) => {
                    probes_ok.push("lmstudio /api/v1/models".into());
                    engine = "lmstudio".into();
                    detected = true;
                    engine_json = Some(value);
                }
                _ => probes_failed.push("lmstudio /api/v1/models".into()),
            }
        }

        // 6. KoboldCpp's version endpoint names the engine.
        if !detected {
            match probe::get_json_with_timeout(
                provider,
                &format!("{}/api/extra/version", urls.root),
                PROBE_TIMEOUT,
            )
            .await
            {
                Ok(value)
                    if value["result"]
                        .as_str()
                        .is_some_and(|result| result.to_ascii_lowercase().contains("kobold")) =>
                {
                    probes_ok.push("koboldcpp /api/extra/version".into());
                    engine = "koboldcpp".into();
                    detected = true;
                    engine_json = Some(value);
                }
                _ => probes_failed.push("koboldcpp /api/extra/version".into()),
            }
        }

        // 7. TabbyAPI's `/v1/model` carries its parameters.
        if !detected {
            match probe::get_json_with_timeout(
                provider,
                &format!("{}/model", urls.api),
                PROBE_TIMEOUT,
            )
            .await
            {
                Ok(value) if value["parameters"].is_object() => {
                    probes_ok.push("tabbyapi /v1/model".into());
                    engine = "tabbyapi".into();
                    detected = true;
                    engine_json = Some(value);
                }
                _ => probes_failed.push("tabbyapi /v1/model".into()),
            }
        }

        // Fill engine-specific fields from what the engine actually reported.
        match engine.as_str() {
            "llama.cpp" => {
                if let Some(props) = &props {
                    build = props.build_info.clone();
                    slots = props.slots;
                    if props
                        .chat_template
                        .as_deref()
                        .is_some_and(probe::template_supports_tools)
                    {
                        tool_dialect = "native".into();
                    }
                }
            }
            "ollama" => {
                if let Some(info) = &ollama {
                    if info
                        .capabilities
                        .iter()
                        .any(|capability| capability == "tools")
                    {
                        tool_dialect = "native".into();
                    } else if !info.capabilities.is_empty() {
                        tool_dialect = "none".into();
                    }
                }
            }
            "sglang" => {
                if let Some(value) = &engine_json {
                    build = value["version"].as_str().map(str::to_string);
                    embeddings = value["is_embedding"].as_bool().unwrap_or(false);
                    rerank = value["model_type"]
                        .as_str()
                        .is_some_and(|kind| kind.to_ascii_lowercase().contains("rerank"))
                        || value["is_generation"].as_bool() == Some(false);
                }
            }
            "lmstudio" => {
                if let Some(entry) = engine_json
                    .as_ref()
                    .and_then(|value| selected_entry(value, &model))
                {
                    embeddings = entry["type"]
                        .as_str()
                        .is_some_and(|kind| kind.to_ascii_lowercase().contains("embed"));
                }
            }
            "koboldcpp" => {
                if let Some(value) = &engine_json {
                    build = value["version"].as_str().map(str::to_string);
                    cache_mode = value["cache_mode"].as_str().map(str::to_string);
                }
            }
            "tabbyapi" => {
                if let Some(value) = &engine_json {
                    cache_mode = value["parameters"]["cache_mode"]
                        .as_str()
                        .map(str::to_string);
                    tool_parser = value["parameters"]["tool_format"]
                        .as_str()
                        .map(str::to_string);
                }
            }
            _ => {}
        }
        if tool_parser.is_none()
            && let Some(value) = &engine_json
        {
            tool_parser = value["tool_call_parser"]
                .as_str()
                .or_else(|| value["parameters"]["tool_call_parser"].as_str())
                .map(str::to_string);
        }

        // Metadata only: `/tokenize` proves exact token counts without
        // generating anything.
        if probe::token_count(provider, "hello").await.is_some() {
            probes_ok.push("POST /tokenize".into());
            tokenizer_route = Some("/tokenize".to_string());
        } else {
            probes_failed.push("POST /tokenize".into());
        }

        let context = probe::probe_context_truth(provider)
            .await
            .unwrap_or_default();
        let (mut warnings, mut fixes) = context_notices(&engine, configured_window, &context);
        fixes.extend(engine_fixes(
            &engine,
            suggested_window(configured_window, &context),
        ));
        if tool_dialect == "unknown" {
            warnings.push(
                "tool-calling support could not be confirmed from server metadata; see fixes for the engine's native tool flag"
                    .into(),
            );
        }
        if !detected {
            warnings.push(
                "engine not recognized: only the OpenAI-compatible API is assumed and capability fields are conservative"
                    .into(),
            );
        }

        Self {
            engine,
            build,
            model,
            context,
            tool_dialect,
            tool_parser,
            tool_probe: None,
            reasoning_field,
            structured_output,
            cache_mode,
            slots,
            tokenizer_route,
            embeddings,
            rerank,
            probes_ok,
            probes_failed,
            warnings,
            fixes,
        }
    }

    /// Stable pretty JSON for `lh doctor --json`.
    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    /// Human-readable form for `lh doctor`.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("engine: {}\n", self.engine));
        if let Some(build) = &self.build {
            out.push_str(&format!("build: {build}\n"));
        }
        let model = if self.model.is_empty() {
            "(none)"
        } else {
            self.model.as_str()
        };
        out.push_str(&format!("model: {model}\n"));
        out.push_str(&format!(
            "context: trained {} · allocated {} · per slot {} · effective {}\n",
            number(self.context.trained),
            number(self.context.allocated),
            number(self.context.per_slot),
            number(Some(self.context.effective)),
        ));
        if let Some(source) = &self.context.trained_source {
            out.push_str(&format!("  trained from: {source}\n"));
        }
        if let Some(source) = &self.context.allocated_source {
            out.push_str(&format!("  allocated from: {source}\n"));
        }
        if let Some(source) = &self.context.per_slot_source {
            out.push_str(&format!("  per slot from: {source}\n"));
        }
        out.push_str(&format!("tools: {}", self.tool_dialect));
        if let Some(parser) = &self.tool_parser {
            out.push_str(&format!(" (parser: {parser})"));
        }
        out.push('\n');
        match &self.tool_probe {
            Some(probe) => out.push_str(&format!(
                "tool probe: {} · mode {} · model {} · checked {}\n",
                probe.outcome.code(),
                probe.mode,
                probe.model,
                probe.checked
            )),
            None => out.push_str("tool probe: not run (no cached conformance result)\n"),
        }
        out.push_str(&format!(
            "reasoning field: {}\n",
            self.reasoning_field.as_deref().unwrap_or("unknown")
        ));
        out.push_str(&format!("structured output: {}\n", self.structured_output));
        out.push_str(&format!(
            "cache: {}\n",
            self.cache_mode.as_deref().unwrap_or("unknown")
        ));
        out.push_str(&format!("slots: {}\n", number(self.slots)));
        out.push_str(&format!(
            "tokenizer: {}\n",
            self.tokenizer_route
                .as_deref()
                .unwrap_or("unknown (estimated)")
        ));
        out.push_str(&format!("embeddings: {}\n", yes_no(self.embeddings)));
        out.push_str(&format!("rerank: {}\n", yes_no(self.rerank)));
        out.push_str(&format!("probes ok: {}\n", join_or_none(&self.probes_ok)));
        out.push_str(&format!(
            "probes failed: {}\n",
            join_or_none(&self.probes_failed)
        ));
        if self.warnings.is_empty() {
            out.push_str("warnings: none\n");
        } else {
            out.push_str("warnings:\n");
            for warning in &self.warnings {
                out.push_str(&format!("  - {warning}\n"));
            }
        }
        if self.fixes.is_empty() {
            out.push_str("fixes: none\n");
        } else {
            out.push_str("fixes:\n");
            for fix in &self.fixes {
                out.push_str(&format!("  - {fix}\n"));
            }
        }
        out
    }
}

/// Cache key for a manifest: API base, model, and build string. Probes depend
/// on all three; an engine upgrade invalidates the entry through `build`.
pub fn cache_key(provider: &dyn Provider, manifest: &DoctorManifest) -> String {
    format!(
        "{}|{}|{}",
        endpoint_urls(provider.endpoint()).api,
        provider.model().trim(),
        manifest.build.as_deref().unwrap_or("")
    )
}

/// Read a cached manifest. A missing or corrupt file is a cache miss.
pub fn load_cached(path: &Path, key: &str) -> Option<DoctorManifest> {
    let bytes = std::fs::read(path).ok()?;
    let cache: BTreeMap<String, DoctorManifest> = serde_json::from_slice(&bytes).ok()?;
    cache.get(key).cloned()
}

/// Store a manifest in the JSON map at `path`, creating parent directories.
pub fn store_cached(path: &Path, key: &str, manifest: &DoctorManifest) -> std::io::Result<()> {
    let mut cache: BTreeMap<String, DoctorManifest> = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    cache.insert(key.to_string(), manifest.clone());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&cache).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// Context warnings and the clamp fix for a configured window. Pure so the
/// wording and the named flag are testable without a server.
fn context_notices(
    engine: &str,
    configured_window: u32,
    context: &WindowTruth,
) -> (Vec<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut fixes = Vec::new();
    if context.effective == 0 {
        if configured_window > 0 {
            warnings.push(format!(
                "the server did not report a context window; the harness will use the configured {configured_window} and may be clamped"
            ));
        } else {
            warnings.push(
                "the server did not report a context window; set [model] context_window or a model profile"
                    .into(),
            );
        }
    } else if configured_window > 0 && configured_window != context.effective {
        if configured_window > context.effective {
            warnings.push(format!(
                "configured context_window {configured_window} exceeds the server's effective window {}; prompts will be clamped to the server limit",
                context.effective
            ));
            fixes.push(clamp_fix(engine, configured_window));
        } else {
            warnings.push(format!(
                "configured context_window {configured_window} is smaller than the server's effective window {}; the harness will under-use the server",
                context.effective
            ));
            fixes.push(format!(
                "raise the configured context_window to {} (the server has it allocated)",
                context.effective
            ));
        }
    }
    if context.allocated_source.as_deref() == Some("ollama default") {
        warnings.push(
            "Ollama is serving its default 4096-token context; long prompts are truncated from the front"
                .into(),
        );
    }
    (warnings, fixes)
}

/// The window to name in engine fix-up commands: what the profile asks for,
/// never smaller than what the server already has.
fn suggested_window(configured_window: u32, context: &WindowTruth) -> u32 {
    let suggested = if context.allocated_source.as_deref() == Some("ollama default") {
        configured_window.max(32_768)
    } else {
        configured_window.max(context.effective)
    };
    if suggested == 0 { 32_768 } else { suggested }
}

/// The exact server flag that raises the limit, for the mismatch fix line.
fn clamp_fix(engine: &str, configured_window: u32) -> String {
    match engine {
        "llama.cpp" => format!(
            "llama.cpp --ctx-size {configured_window} (or raise the profile's context_window to the server limit)"
        ),
        "ollama" => format!("OLLAMA_CONTEXT_LENGTH={configured_window} ollama serve"),
        "vllm" => format!("vllm --max-model-len {configured_window}"),
        "sglang" => format!("sglang --context-length {configured_window}"),
        "lmstudio" => {
            format!("LM Studio: set the loaded model's Context Length to {configured_window}")
        }
        "koboldcpp" => format!("koboldcpp --contextsize {configured_window}"),
        "tabbyapi" => format!("tabbyapi max_seq_len: {configured_window} in config.yml"),
        _ => format!("raise the server context limit to {configured_window} tokens"),
    }
}

/// Engine-specific commands worth printing even without a mismatch.
fn engine_fixes(engine: &str, window: u32) -> Vec<String> {
    match engine {
        "llama.cpp" => vec![format!(
            "llama.cpp: llama-server -m <model> --jinja --ctx-size {window} --port 8080"
        )],
        "ollama" => vec![
            format!("ollama: OLLAMA_CONTEXT_LENGTH={window} ollama serve"),
            format!("ollama: or add `PARAMETER num_ctx {window}` to a Modelfile"),
        ],
        "vllm" => vec![format!(
            "vllm: vllm serve <model> --max-model-len {window} --enable-auto-tool-choice --tool-call-parser <name>"
        )],
        "sglang" => vec![format!(
            "sglang: python -m sglang.launch_server --context-length {window} --tool-call-parser <name>"
        )],
        "lmstudio" => vec![format!(
            "lmstudio: set the loaded model's Context Length to {window} in the server UI"
        )],
        "koboldcpp" => vec![format!("koboldcpp: --contextsize {window}")],
        "tabbyapi" => vec![format!(
            "tabbyapi: set `max_seq_len: {window}` in config.yml"
        )],
        _ => vec![format!(
            "generic: raise the server's context limit to {window} tokens"
        )],
    }
}

fn selected_entry<'a>(value: &'a serde_json::Value, model: &str) -> Option<&'a serde_json::Value> {
    let items = model_entries(value);
    items
        .iter()
        .find(|item| item["id"].as_str() == Some(model))
        .or_else(|| items.first())
}

fn model_entries(value: &serde_json::Value) -> &[serde_json::Value] {
    value["data"]
        .as_array()
        .or_else(|| value["models"].as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn lmstudio_signature(value: &serde_json::Value, model: &str) -> bool {
    selected_entry(value, model).is_some_and(|entry| {
        positive(&entry["loaded_context_length"]).is_some()
            || positive(&entry["max_context_length"]).is_some()
    })
}

fn positive(value: &serde_json::Value) -> Option<u32> {
    value
        .as_u64()
        .filter(|n| *n > 0)
        .map(|n| n.min(u32::MAX as u64) as u32)
}

fn number(value: Option<u32>) -> String {
    match value {
        Some(value) if value > 0 => value.to_string(),
        _ => "unknown".into(),
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn join_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "(none)".into()
    } else {
        values.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use tokio::sync::mpsc::UnboundedSender;
    use tokio_util::sync::CancellationToken;

    use crate::provider::{ChatRequest, StreamItem};

    fn sample_context() -> WindowTruth {
        WindowTruth {
            trained: Some(131_072),
            allocated: Some(8192),
            per_slot: Some(4096),
            effective: 4096,
            trained_source: Some("/v1/models meta.n_ctx_train".into()),
            allocated_source: Some("llama.cpp /props n_ctx × slots".into()),
            per_slot_source: Some("llama.cpp /props n_ctx (per slot)".into()),
        }
    }

    fn sample_manifest() -> DoctorManifest {
        DoctorManifest {
            engine: "llama.cpp".into(),
            build: Some("b6040".into()),
            model: "qwen3".into(),
            context: sample_context(),
            tool_dialect: "native".into(),
            tool_parser: None,
            tool_probe: None,
            reasoning_field: None,
            structured_output: "unknown".into(),
            cache_mode: None,
            slots: Some(2),
            tokenizer_route: Some("/tokenize".into()),
            embeddings: false,
            rerank: false,
            probes_ok: vec!["llama.cpp /props".into(), "POST /tokenize".into()],
            probes_failed: vec![],
            warnings: vec![],
            fixes: vec![
                "llama.cpp: llama-server -m <model> --jinja --ctx-size 65536 --port 8080".into(),
            ],
        }
    }

    #[test]
    fn text_manifest_surfaces_the_tool_probe() {
        let mut manifest = sample_manifest();
        assert!(manifest.to_text().contains("tool probe: not run"));
        manifest.tool_probe = Some(ToolProbe {
            outcome: probe::ToolProbeOutcome::ProseFallback,
            model: "qwen3".into(),
            build: Some("b6040".into()),
            mode: "repair".into(),
            message: "the model answered in prose".into(),
            checked: "2026-01-01T00:00:00Z".into(),
        });
        let text = manifest.to_text();
        assert!(
            text.contains("tool probe: prose_fallback · mode repair"),
            "{text}"
        );
    }

    #[test]
    fn json_manifest_has_every_documented_field() {
        let value: serde_json::Value =
            serde_json::from_str(&sample_manifest().to_json_pretty()).unwrap();
        for key in [
            "engine",
            "build",
            "model",
            "context",
            "tool_dialect",
            "tool_parser",
            "tool_probe",
            "reasoning_field",
            "structured_output",
            "cache_mode",
            "slots",
            "tokenizer_route",
            "embeddings",
            "rerank",
            "probes_ok",
            "probes_failed",
            "warnings",
            "fixes",
        ] {
            assert!(value.get(key).is_some(), "missing {key}");
        }
        assert_eq!(value["context"]["trained"], 131_072);
        assert_eq!(value["context"]["effective"], 4096);
        assert_eq!(
            value["context"]["per_slot_source"],
            "llama.cpp /props n_ctx (per slot)"
        );
        assert_eq!(value["tool_dialect"], "native");
    }

    #[test]
    fn text_manifest_shows_the_effective_window_and_fixes() {
        let text = sample_manifest().to_text();
        assert!(text.contains("engine: llama.cpp"));
        assert!(text.contains("effective 4096"));
        assert!(text.contains("per slot from: llama.cpp /props n_ctx (per slot)"));
        assert!(text.contains("fixes:"));
        assert!(text.contains("--jinja --ctx-size 65536"));
    }

    #[test]
    fn mismatch_warns_about_clamping_and_names_the_flag() {
        let (warnings, fixes) = context_notices("llama.cpp", 65_536, &sample_context());
        assert!(warnings.iter().any(|warning| warning.contains("clamped")));
        assert!(fixes.iter().any(|fix| fix.contains("--ctx-size 65536")));

        let (warnings, fixes) = context_notices("ollama", 32_768, &sample_context());
        assert!(warnings.iter().any(|warning| warning.contains("clamped")));
        assert!(
            fixes
                .iter()
                .any(|fix| fix.contains("OLLAMA_CONTEXT_LENGTH=32768"))
        );
    }

    #[test]
    fn matching_window_does_not_warn() {
        let (warnings, fixes) = context_notices("llama.cpp", 4096, &sample_context());
        assert!(warnings.is_empty());
        assert!(fixes.is_empty());
    }

    #[test]
    fn ollama_default_is_called_out() {
        let context = WindowTruth {
            trained: Some(131_072),
            allocated: Some(4096),
            per_slot: None,
            effective: 4096,
            trained_source: Some("ollama /api/show model_info".into()),
            allocated_source: Some("ollama default".into()),
            per_slot_source: None,
        };
        let (warnings, _) = context_notices("ollama", 32_768, &context);
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("default 4096"))
        );
        assert_eq!(suggested_window(0, &context), 32_768);
    }

    #[test]
    fn cache_round_trips_a_manifest() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("doctor-cache.json");
        let manifest = sample_manifest();
        let provider = NoProbe;
        let key = cache_key(&provider, &manifest);

        assert!(load_cached(&path, &key).is_none());
        store_cached(&path, &key, &manifest).unwrap();
        let loaded = load_cached(&path, &key).expect("cached manifest");
        assert_eq!(loaded.engine, "llama.cpp");
        assert_eq!(loaded.context, manifest.context);
        assert!(load_cached(&path, "other key").is_none());
    }

    struct NoProbe;

    #[async_trait]
    impl Provider for NoProbe {
        async fn chat_stream(
            &self,
            _req: ChatRequest,
            _tx: UnboundedSender<StreamItem>,
            _cancel: CancellationToken,
        ) {
        }

        fn endpoint(&self) -> &str {
            "http://127.0.0.1:9"
        }

        fn model(&self) -> &str {
            "test-model"
        }
    }

    #[tokio::test]
    async fn unknown_engine_falls_back_to_generic_with_failed_probes() {
        let manifest = DoctorManifest::probe(&NoProbe, 8192).await;
        assert_eq!(manifest.engine, "generic");
        assert_eq!(manifest.context.effective, 0);
        assert!(
            manifest
                .probes_failed
                .iter()
                .any(|probe| probe == "llama.cpp /props")
        );
        assert!(
            manifest
                .probes_failed
                .iter()
                .any(|probe| probe == "tabbyapi /v1/model")
        );
        assert!(
            manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("engine not recognized"))
        );
        assert!(
            manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("configured 8192"))
        );
    }

    #[tokio::test]
    async fn ollama_manifest_reports_capabilities_and_mismatch() {
        use axum::routing::{get, post};

        let app = axum::Router::new()
            .route(
                "/api/show",
                post(|| async {
                    axum::Json(serde_json::json!({
                        "parameters": "num_ctx 8192",
                        "model_info": {"qwen3.context_length": 131072},
                        "capabilities": ["completion", "tools"],
                        "details": {"family": "qwen3"}
                    }))
                }),
            )
            .route(
                "/api/ps",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "models": [{"name": "qwen3", "context_length": 8192}]
                    }))
                }),
            )
            .route(
                "/v1/models",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "object": "list",
                        "data": [{"id": "qwen3", "object": "model", "owned_by": "ollama"}]
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let config = crate::config::ModelConfig {
            endpoint: format!("http://{address}"),
            ..Default::default()
        };
        let provider = crate::provider::OpenAiCompatProvider::new(&config, "qwen3").unwrap();
        let manifest = DoctorManifest::probe(&provider, 32_768).await;

        assert_eq!(manifest.engine, "ollama");
        assert_eq!(manifest.tool_dialect, "native");
        assert_eq!(manifest.context.trained, Some(131_072));
        assert_eq!(manifest.context.allocated, Some(8192));
        assert_eq!(manifest.context.effective, 8192);
        assert!(
            manifest
                .probes_ok
                .iter()
                .any(|probe| probe == "ollama /api/show")
        );
        assert!(
            manifest
                .warnings
                .iter()
                .any(|warning| warning.contains("clamped"))
        );
        assert!(
            manifest
                .fixes
                .iter()
                .any(|fix| fix.contains("OLLAMA_CONTEXT_LENGTH=32768"))
        );
    }
}

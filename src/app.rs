//! Build the runtime core: resolve the model, open or create a session, load
//! external tools, and compute the active tool list and system prompt.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio_util::sync::CancellationToken;

use crate::agent::{Core, RunLimits, WindowSource, prompt};
use crate::config::Config;
use crate::context::{StaticPrompt, TokenBudget, ToolSpecCost};
use crate::external;
use crate::paths::Paths;
use crate::provider::{OpenAiCompatProvider, Provider, probe};
use crate::security::SecurityState;
use crate::session::{
    APP_VERSION, CustomData, EntryKind, Session, SessionContextWindow, SessionData,
};
use crate::skills::SkillIndex;
use crate::tools::{BgRegistry, ReadTracker, SessionHandle, SessionHandle as Handle, ToolRegistry};

#[derive(Debug, Clone, Default)]
pub struct RuntimeOptions {
    pub path_jail: bool,
    pub engagement: Option<String>,
    pub no_session: bool,
    pub resume: Option<PathBuf>,
    pub continue_last: bool,
}

pub struct Runtime {
    pub core: Arc<Core>,
    pub session: SessionHandle,
}

const START_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const START_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const START_RETRY_INTERVAL: Duration = Duration::from_millis(250);

/// Explicit wire format used when the conformance probe did not find native
/// tool calls. `recover_output` already recognizes `<tool_call>` blocks.
const REPAIR_PROTOCOL: &str = "\n\n## Tool calls\n\
This server does not return structured tool calls. When you need a tool, write exactly one call as:\n\
<tool_call>{\"name\":\"tool_name\",\"arguments\":{...}}</tool_call>\n\
Use a bare block, not a fenced code block. Give the final answer in prose when no tool is needed.";

/// Run the configured startup command once when the endpoint is unreachable,
/// then wait for it to come up. Empty `model.start_command` disables this.
async fn ensure_endpoint(
    provider: &OpenAiCompatProvider,
    config: &crate::config::ModelConfig,
) -> Result<()> {
    let command = config.start_command.trim();
    if command.is_empty() {
        return Ok(());
    }
    if probe::list_models_with_timeout(provider, START_PROBE_TIMEOUT)
        .await
        .is_ok()
    {
        return Ok(());
    }
    tracing::info!(command = %command, "model endpoint is down; running model.start_command");
    let mut process = tokio::process::Command::new("sh");
    process
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(START_COMMAND_TIMEOUT, process.output())
        .await
        .with_context(|| {
            format!(
                "model.start_command timed out after {}s",
                START_COMMAND_TIMEOUT.as_secs()
            )
        })?
        .context("cannot run model.start_command")?;
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        // A failed command did not start the endpoint; waiting out the retry
        // window only delays the diagnostic by another 30 seconds.
        tracing::warn!(
            status = %output.status,
            stderr = %stderr,
            "model.start_command exited non-zero"
        );
        if stderr.is_empty() {
            bail!("model.start_command failed ({})", output.status);
        }
        bail!("model.start_command failed ({}): {stderr}", output.status);
    }
    let deadline = tokio::time::Instant::now() + START_COMMAND_TIMEOUT;
    loop {
        if probe::list_models_with_timeout(provider, START_PROBE_TIMEOUT)
            .await
            .is_ok()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            if stderr.is_empty() {
                bail!(
                    "model.start_command did not bring {} up within {}s",
                    config.endpoint,
                    START_COMMAND_TIMEOUT.as_secs()
                );
            }
            bail!(
                "model.start_command did not bring {} up within {}s: {stderr}",
                config.endpoint,
                START_COMMAND_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(START_RETRY_INTERVAL).await;
    }
}

/// Resolve the model, open the session, and assemble the core.
pub async fn create_runtime(
    mut config: Config,
    paths: Paths,
    options: RuntimeOptions,
) -> Result<Runtime> {
    if options.no_session && (options.resume.is_some() || options.continue_last) {
        bail!("--no-session cannot be combined with --continue or --resume");
    }

    if let Ok(endpoint) = std::env::var("LH_ENDPOINT")
        && !endpoint.trim().is_empty()
    {
        config.model.endpoint = endpoint;
    }

    let mut model = config.model.model.trim().to_string();
    let start_command = config.model.start_command.trim().to_string();
    // With a model already selected, endpoint bring-up runs in the background
    // so the TUI opens at once; the first turn waits for the gate. With no
    // model it still runs first: auto-selection needs a reachable endpoint.
    let deferred_start = !model.is_empty() && !start_command.is_empty();
    let mut startup_warnings = Vec::new();
    if !deferred_start {
        let probe_provider = OpenAiCompatProvider::new(&config.model, model.clone())?;
        if let Err(error) = ensure_endpoint(&probe_provider, &config.model).await {
            tracing::warn!(%error, "model endpoint is not available");
            startup_warnings.push(format!("model endpoint is not available: {error}"));
        }
        if model.is_empty() {
            // The TUI must open without a model so the user can configure one
            // with /login. Auto-select only when the endpoint is reachable and
            // names one model; every other outcome becomes a warning.
            let api = crate::provider::endpoint_urls(&config.model.endpoint).api;
            let probed =
                tokio::time::timeout(Duration::from_secs(3), probe::list_models(&probe_provider))
                    .await;
            match probed {
                Ok(Ok(models)) => match models.len() {
                    0 => startup_warnings.push(format!(
                        "not logged in: {api}/models reports no models; load a model, then run /login"
                    )),
                    1 => model = models[0].clone(),
                    _ => startup_warnings.push(format!(
                        "not logged in: {api}/models exposes multiple models ({}); run /login <endpoint> <model>",
                        models.join(", ")
                    )),
                },
                Ok(Err(error)) => startup_warnings.push(format!(
                    "not logged in: no model is set and {api}/models failed: {error}. Start the inference server, then run /login"
                )),
                Err(_) => startup_warnings.push(format!(
                    "not logged in: no model is set and {api}/models did not answer within 3s. Start the inference server, then run /login"
                )),
            }
        }
    }
    config.model.model = model.clone();
    let probe_provider = OpenAiCompatProvider::new(&config.model, model.clone())?;

    let (_, mut profile) = config.model_profile();
    let window_cache: Arc<Mutex<std::collections::HashMap<String, u32>>> = Default::default();
    let resolved = resolve_window_truth(&config, &probe_provider, &window_cache).await;
    let context_window = resolved.window;
    let window_source = resolved.source;
    let window_truth = resolved.truth;
    let configured_window = resolved.configured;
    let provider: Arc<dyn Provider> =
        Arc::new(OpenAiCompatProvider::new(&config.model, model.clone())?);
    warn_if_remote(&config.model.endpoint);

    let security = Arc::new(Mutex::new(SecurityState::load(
        &paths,
        options.engagement.clone(),
    )?));

    if !options.no_session {
        paths.ensure_dirs()?;
    }
    let session_path = if let Some(path) = &options.resume {
        Some(path.clone())
    } else if options.continue_last {
        Session::latest_path(&paths)?
    } else {
        None
    };
    let mut session = match session_path {
        Some(path) => {
            let mut session = Session::open(&path)
                .with_context(|| format!("cannot resume {}", path.display()))?;
            if let Some(header) = session.header().cloned() {
                if header.endpoint != config.model.endpoint {
                    let warning = format!(
                        "resumed session used endpoint {}; now using {}",
                        header.endpoint, config.model.endpoint
                    );
                    tracing::warn!("{warning}");
                    session.warnings.push(warning);
                }
                if header.model != model {
                    let warning = format!(
                        "resumed session used model {}; now using {model}",
                        header.model
                    );
                    tracing::warn!("{warning}");
                    session.warnings.push(warning);
                }
            }
            session
        }
        None => Session::create(
            &paths,
            SessionData {
                cwd: paths.cwd.display().to_string(),
                endpoint: config.model.endpoint.clone(),
                model: model.clone(),
                profile: config.profile_name().to_string(),
                app: APP_VERSION.to_string(),
                context: session_context(window_truth.as_ref()),
            },
            options.no_session,
        )?,
    };
    session.warnings.extend(startup_warnings);
    let window_notice = window_notice(window_truth.as_ref(), configured_window);
    if let Some(notice) = &window_notice {
        session.warnings.push(notice.clone());
    }

    // Build the OS sandbox before any turn. A requested sandbox that cannot
    // start is a hard error; it never silently falls back to unsandboxed
    // execution. `--no-sandbox` is the explicit escape hatch.
    let sandbox = if config.sandbox == crate::config::SandboxMode::Off {
        None
    } else {
        let sandbox = crate::sandbox::Sandbox::new(config.sandbox, paths.cwd.clone())
            .map_err(anyhow::Error::msg)?;
        Some(Arc::new(sandbox))
    };
    if config.sandbox_explicit {
        let backend = sandbox
            .as_ref()
            .map(|sandbox| sandbox.label())
            .unwrap_or("off");
        if let Err(error) = session.append(EntryKind::Custom(
            CustomData::new("sandbox")
                .with("mode", config.sandbox.label())
                .with("backend", backend),
        )) {
            tracing::warn!(%error, "cannot record the sandbox mode in the session");
        }
        if config.sandbox == crate::config::SandboxMode::Off {
            session.warnings.push(
                "sandbox disabled explicitly with --no-sandbox; bash can read ~/.ssh, write anywhere, and use the network"
                    .into(),
            );
        }
    }
    if let Some(context) = session_context(window_truth.as_ref()) {
        let payload = serde_json::to_value(&context)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default();
        if let Err(error) = session.append(EntryKind::Custom(CustomData {
            kind: "context-window".into(),
            payload,
        })) {
            tracing::warn!(%error, "cannot record the context window in the session");
        }
    }

    let registry = ToolRegistry::builtin();
    let external_tools = external::load_tools(&paths, config.trust).unwrap_or_else(|err| {
        tracing::warn!(%err, "external tools were not loaded");
        Vec::new()
    });
    let tools = registry
        .select(config.profile(), &external_tools)
        .map_err(anyhow::Error::msg)?;
    let mut tool_specs = ToolRegistry::specs(&tools);
    // With a background `model.start_command`, the server may not be up yet;
    // probing would stall the TUI for the request timeout. The probe runs on
    // the next rebuild (model switch/profile reload) instead.
    let tool_probe = if deferred_start {
        None
    } else {
        resolve_tool_probe(provider.as_ref(), &paths, &mut profile, &mut tool_specs).await
    };
    if let Some(probe) = &tool_probe {
        if let Some(warning) = probe.warning() {
            session.warnings.push(warning.clone());
        }
        let payload = serde_json::json!({
            "outcome": probe.outcome.code(),
            "mode": probe.mode,
            "model": probe.model,
            "build": probe.build,
            "message": probe.message,
            "checked": probe.checked,
        });
        if let Some(object) = payload.as_object() {
            let mut data = CustomData::new("tool-probe");
            for (key, value) in object {
                data.payload.insert(key.clone(), value.clone());
            }
            if let Err(error) = session.append(EntryKind::Custom(data)) {
                tracing::warn!(%error, "cannot record the tool probe in the session");
            }
        }
    }
    let skills =
        SkillIndex::load(&paths, config.trust).advertised_for(&crate::tools::active_packs(&tools));
    let repo_brief = registry
        .repo()
        .brief(&paths.cwd, crate::tools::repo_map::BRIEF_BUDGET);
    let system_prompt =
        prompt::build_system_prompt(&config, &paths, &skills, &tools, Some(&repo_brief));
    // A server without native tool calls still lists the tools in the system
    // prompt; name the exact wire format the repair layer can execute.
    let mut system_prompt = system_prompt;
    if profile.tool_mode == crate::config::ToolMode::Repair {
        system_prompt.push_str(REPAIR_PROTOCOL);
    }
    let mut budget = TokenBudget::new(context_window, config.model.max_output_tokens);
    let sampled = tokio::time::timeout(
        Duration::from_secs(2),
        probe::token_count(&probe_provider, &system_prompt),
    )
    .await
    .ok()
    .flatten();
    let tokenize = sampled.is_some();
    if let Some(tokens) = sampled
        && tokens > 0
    {
        budget.calibrate(
            system_prompt.chars().count(),
            tokens.min(u32::MAX as usize) as u32,
        );
    }
    let tool_specs_json = if tool_specs.is_empty() {
        String::new()
    } else {
        crate::provider::tools_json(&tool_specs)
    };
    let mut tool_specs_cost = ToolSpecCost::from_json(&tool_specs_json, &budget);
    if tokenize && !tool_specs_json.is_empty() {
        tool_specs_cost =
            measure_tool_specs(&probe_provider, &tool_specs_json, tool_specs_cost).await;
    }
    // Measure the static prompt with the calibrated ratio, not the initial
    // 3.5-chars-per-token guess.
    let static_prompt = measure_static_prompt(
        &budget,
        &system_prompt,
        tool_specs_cost,
        Some(repo_brief.as_str()),
    );
    if static_prompt.total > budget.usable_prompt() {
        session.warnings.push(format!(
            "static prompt is {} tokens but only {} fit the {} token window; turns will be refused until the profile or context is reduced",
            static_prompt.total,
            budget.usable_prompt(),
            budget.context_window
        ));
    } else if u64::from(static_prompt.total) * 2 > u64::from(budget.usable_prompt()) {
        session.warnings.push(format!(
            "static prompt uses {:.0}% of the usable prompt ({} of {} tokens)",
            static_prompt.percent_of(budget.usable_prompt()),
            static_prompt.total,
            budget.usable_prompt()
        ));
    }
    let limits = RunLimits::from(&config.budgets);
    for warning in &session.warnings {
        tracing::warn!("{warning}");
    }
    let artifacts = session.artifacts.clone();
    let reads = Arc::new(Mutex::new(ReadTracker::default()));
    let bg = Arc::new(Mutex::new(BgRegistry::new()));
    let session_handle: Handle = Arc::new(Mutex::new(session));

    // The gateway may be down. Start it in the background so the TUI does not
    // wait for the command and its retry window; turns wait on the gate.
    let startup = if deferred_start {
        let provider = OpenAiCompatProvider::new(&config.model, model.clone())?;
        let model_config = config.model.clone();
        let session = session_handle.clone();
        let (handle, gate) = crate::agent::StartupGate::channel();
        tokio::spawn(async move {
            let result = ensure_endpoint(&provider, &model_config)
                .await
                .map_err(|error| format!("{error:#}"));
            match &result {
                Ok(()) => tracing::info!("model endpoint is ready"),
                Err(error) => {
                    tracing::warn!(%error, "model endpoint is not available");
                    if let Ok(mut session) = session.lock() {
                        session
                            .warnings
                            .push(format!("model endpoint is not available: {error}"));
                        let _ = session.fsync();
                    }
                }
            }
            handle.finish(result);
        });
        Some(gate)
    } else {
        None
    };

    let core = Core {
        config,
        paths,
        provider,
        registry,
        tools,
        tool_specs,
        tool_specs_json,
        tool_specs_cost,
        session: session_handle.clone(),
        artifacts,
        reads,
        bg,
        security,
        sandbox: sandbox.clone(),
        budget,
        static_prompt,
        tokenize,
        limits,
        system_prompt,
        repo_brief,
        model_profile: profile,
        tool_probe,
        cancel: CancellationToken::new(),
        tool_timeout: Duration::from_secs(660),
        path_jail: options.path_jail,
        server_window: Default::default(),
        window_source,
        window_truth,
        window_notice,
        window_notice_sent: std::sync::Once::new(),
        detected_window: Default::default(),
        window_cache,
        startup,
    };

    Ok(Runtime {
        core: Arc::new(core),
        session: session_handle,
    })
}

/// Rebuild the core between turns after a config change (`/model`,
/// `/profile`, `/theme`), reusing the open session and run state.
pub async fn rebuild_core(old: &Arc<Core>, mut config: Config) -> Result<Arc<Core>> {
    let model = if config.model.model.trim().is_empty() {
        old.provider.model().to_string()
    } else {
        config.model.model.trim().to_string()
    };
    config.model.model = model.clone();
    let provider: Arc<dyn Provider> = Arc::new(OpenAiCompatProvider::new(&config.model, model)?);
    // Same endpoint and model: keep the detected window and any truncation
    // limit seen from the server instead of probing again.
    let same_target = old.provider.model() == provider.model()
        && old.config.model.endpoint == config.model.endpoint;
    let resolved = resolve_window_truth(&config, provider.as_ref(), &old.window_cache).await;
    let context_window = resolved.window;
    let window_source = resolved.source;
    let window_truth = resolved.truth;
    let window_notice = window_notice(window_truth.as_ref(), resolved.configured);
    let (server_window, detected_window) = if same_target {
        (old.server_window.clone(), old.detected_window.clone())
    } else {
        Default::default()
    };
    let registry = ToolRegistry::builtin();
    let external_tools = external::load_tools(&old.paths, config.trust).unwrap_or_else(|err| {
        tracing::warn!(%err, "external tools were not loaded");
        Vec::new()
    });
    let tools = registry
        .select(config.profile(), &external_tools)
        .map_err(anyhow::Error::msg)?;
    let mut tool_specs = ToolRegistry::specs(&tools);
    let skills = SkillIndex::load(&old.paths, config.trust)
        .advertised_for(&crate::tools::active_packs(&tools));
    let repo_brief = registry
        .repo()
        .brief(&old.paths.cwd, crate::tools::repo_map::BRIEF_BUDGET);
    let mut system_prompt =
        prompt::build_system_prompt(&config, &old.paths, &skills, &tools, Some(&repo_brief));
    let (_, mut model_profile) = config.model_profile();
    // Same target: keep the conformance result unless it never ran. A model
    // switch probes again so the mode tracks the newly loaded server.
    let tool_probe = if same_target && old.tool_probe.is_some() {
        old.tool_probe.clone()
    } else {
        resolve_tool_probe(
            provider.as_ref(),
            &old.paths,
            &mut model_profile,
            &mut tool_specs,
        )
        .await
    };
    if let Some(probe) = &tool_probe {
        apply_tool_probe(probe, &mut model_profile, &mut tool_specs);
    }
    if model_profile.tool_mode == crate::config::ToolMode::Repair {
        system_prompt.push_str(REPAIR_PROTOCOL);
    }
    let mut budget = TokenBudget::new(context_window, config.model.max_output_tokens);
    if same_target {
        // The tokenizer calibration only holds for the same model.
        budget.chars_per_token = old.budget.chars_per_token;
    }
    let sampled = tokio::time::timeout(
        Duration::from_secs(2),
        probe::token_count(provider.as_ref(), &system_prompt),
    )
    .await
    .ok()
    .flatten();
    let tokenize = sampled.is_some();
    if let Some(tokens) = sampled
        && tokens > 0
    {
        budget.calibrate(
            system_prompt.chars().count(),
            tokens.min(u32::MAX as usize) as u32,
        );
    }
    let tool_specs_json = if tool_specs.is_empty() {
        String::new()
    } else {
        crate::provider::tools_json(&tool_specs)
    };
    let mut tool_specs_cost = ToolSpecCost::from_json(&tool_specs_json, &budget);
    if tokenize && !tool_specs_json.is_empty() {
        tool_specs_cost =
            measure_tool_specs(provider.as_ref(), &tool_specs_json, tool_specs_cost).await;
    }
    let static_prompt = measure_static_prompt(
        &budget,
        &system_prompt,
        tool_specs_cost,
        Some(repo_brief.as_str()),
    );
    let limits = RunLimits::from(&config.budgets);
    Ok(Arc::new(Core {
        config,
        paths: old.paths.clone(),
        provider,
        registry,
        tools,
        tool_specs,
        tool_specs_json,
        tool_specs_cost,
        session: old.session.clone(),
        artifacts: old.artifacts.clone(),
        reads: old.reads.clone(),
        bg: old.bg.clone(),
        security: old.security.clone(),
        sandbox: old.sandbox.clone(),
        budget,
        static_prompt,
        tokenize,
        limits,
        system_prompt,
        repo_brief,
        model_profile,
        tool_probe,
        cancel: old.cancel.clone(),
        tool_timeout: old.tool_timeout,
        path_jail: old.path_jail,
        server_window,
        window_source,
        window_truth,
        window_notice,
        window_notice_sent: std::sync::Once::new(),
        detected_window,
        window_cache: old.window_cache.clone(),
        // Endpoint bring-up is not retried on `/model` or `/login`; only a
        // still-running bring-up for the same target carries over.
        startup: if same_target {
            old.startup.clone()
        } else {
            None
        },
    }))
}

/// Warning to show once before the first turn: a clamped profile or an
/// Ollama server still at its small default context.
pub(crate) fn window_notice(
    truth: Option<&probe::WindowTruth>,
    configured: Option<u32>,
) -> Option<String> {
    let truth = truth.filter(|truth| truth.is_known())?;
    if let Some(configured) = configured
        && configured > truth.effective
    {
        return Some(format!(
            "the model profile asks for {configured} tokens but the server allocated {} ({}); the window is clamped to {}. {}",
            truth.effective,
            truth
                .allocated_source
                .as_deref()
                .unwrap_or("no allocated value"),
            truth.effective,
            context_fix(truth),
        ));
    }
    if truth.allocated_source.as_deref() == Some("ollama default") {
        return Some(format!(
            "Ollama is running at its default context of {} tokens; before the first turn set OLLAMA_CONTEXT_LENGTH=32768 (or add PARAMETER num_ctx 32768 to the Modelfile), then restart Ollama.",
            truth.effective
        ));
    }
    None
}

/// Session-header form of a probed window. Absent when nothing was known, so
/// a guessed default is never recorded as measured.
fn session_context(truth: Option<&probe::WindowTruth>) -> Option<SessionContextWindow> {
    let truth = truth.filter(|truth| truth.is_known())?;
    let sources = truth.source_map();
    Some(SessionContextWindow {
        trained: truth.trained,
        allocated: truth.allocated,
        per_slot: truth.per_slot,
        effective: truth.effective,
        sources: (!sources.is_empty()).then_some(sources),
    })
}

/// Exact tool-schema tokens when the engine answers `/tokenize`, otherwise the
/// fixed JSON-density estimate. Called once when the core is built (and once
/// after a deferred startup learns the tokenizer).
pub(crate) async fn measure_tool_specs(
    provider: &dyn Provider,
    tool_specs_json: &str,
    estimated: ToolSpecCost,
) -> ToolSpecCost {
    if tool_specs_json.is_empty() {
        return estimated;
    }
    let measured = tokio::time::timeout(
        Duration::from_secs(2),
        probe::token_count(provider, tool_specs_json),
    )
    .await
    .ok()
    .flatten()
    // A token count above the byte count is impossible for a real tokenizer;
    // treat it as a broken probe instead of inflating the budget.
    .filter(|tokens| *tokens > 0 && *tokens <= tool_specs_json.len())
    .map(|tokens| tokens.min(u32::MAX as usize) as u32);
    measured.map_or(estimated, |tokens| estimated.with_tokens(tokens))
}

/// Static prompt cost at build time. The repository brief is part of the
/// system prompt, so it is measured once and subtracted from the system
/// total; the message can then name the three contributors separately. The
/// tools term is the fixed cost computed from the request's serialized array.
pub(crate) fn measure_static_prompt(
    budget: &TokenBudget,
    system_prompt: &str,
    tools: ToolSpecCost,
    repo_brief: Option<&str>,
) -> StaticPrompt {
    let repo = repo_brief
        .map(|brief| budget.estimate_text(brief))
        .unwrap_or(0);
    let system = budget.estimate_text(system_prompt).saturating_sub(repo);
    StaticPrompt::new(system, tools.tokens, repo)
}

struct ResolvedWindow {
    configured: Option<u32>,
    truth: Option<probe::WindowTruth>,
    window: u32,
    source: WindowSource,
}

/// Resolve the context window from all three advertised values. The effective
/// value is `min(trained, allocated, per_slot)`; a configured value is clamped
/// to it rather than sent unchecked. A probe failure keeps the configured
/// value, then a cached value, then the default.
async fn resolve_window_truth(
    config: &Config,
    provider: &dyn Provider,
    cache: &Mutex<std::collections::HashMap<String, u32>>,
) -> ResolvedWindow {
    let (_, profile) = config.model_profile();
    let configured = profile.context_window.or(config.model.context_window);
    let fallback = || ResolvedWindow {
        configured,
        truth: None,
        window: configured.unwrap_or_else(|| config.context_window()),
        source: if configured.is_some() {
            WindowSource::Configured
        } else {
            WindowSource::Default
        },
    };
    if provider.model().trim().is_empty() {
        return fallback();
    }
    let key = Core::window_key(&config.model.endpoint, provider.model());
    let mut truth =
        tokio::time::timeout(Duration::from_secs(3), probe::probe_context_truth(provider))
            .await
            .ok()
            .flatten();
    let mut remembered = false;
    if truth.as_ref().is_none_or(|truth| !truth.is_known())
        && let Some(window) = cache.lock().ok().and_then(|cache| cache.get(&key).copied())
    {
        truth = Some(probe::WindowTruth::resolve(None, Some(window), None));
        remembered = true;
    }
    let probed = truth
        .as_ref()
        .map(|truth| truth.effective)
        .filter(|window| *window > 0);
    if let Some(window) = probed
        && let Ok(mut cache) = cache.lock()
    {
        cache.insert(key, window);
    }
    let window = match (configured, probed) {
        (Some(configured), Some(probed)) => configured.min(probed),
        (Some(configured), None) => configured,
        (None, Some(probed)) => probed,
        (None, None) => config.context_window(),
    };
    let source = if remembered {
        WindowSource::Probed(probe::WindowProbe::Remembered)
    } else if let Some(truth) = &truth
        && truth.is_known()
    {
        // A trained maximum alone is not what the server allocated.
        if truth.allocated.is_some() || truth.per_slot.is_some() {
            WindowSource::Probed(probe::WindowProbe::Served)
        } else {
            WindowSource::Probed(probe::WindowProbe::OllamaTrained)
        }
    } else if configured.is_some() {
        WindowSource::Configured
    } else {
        WindowSource::Default
    };
    ResolvedWindow {
        configured,
        truth,
        window,
        source,
    }
}

/// Exact server-side fix for a too-small context, based on which probe
/// reported the allocated value.
fn context_fix(truth: &probe::WindowTruth) -> String {
    let source = truth.allocated_source.as_deref().unwrap_or("");
    if source.contains("ollama") {
        format!(
            "Set OLLAMA_CONTEXT_LENGTH={} (or add PARAMETER num_ctx {} to the Modelfile), then restart Ollama.",
            truth.effective.max(32768),
            truth.effective.max(32768)
        )
    } else if source.contains("llama") {
        format!(
            "Restart llama.cpp with --ctx-size {} (or -c {}).",
            truth.effective.max(32768),
            truth.effective.max(32768)
        )
    } else if source.contains("vllm") {
        format!(
            "Restart vLLM with --max-model-len {}.",
            truth.effective.max(32768)
        )
    } else {
        "Raise the server's context size, or lower [model] max_output_tokens.".to_string()
    }
}

/// Run the forced-tool conformance probe once per model/build and apply its
/// result to the effective profile. A cached `native_ok` for the same model
/// and build skips the request; everything else is re-probed because a server
/// restart often changes the tool configuration.
async fn resolve_tool_probe(
    provider: &dyn Provider,
    paths: &Paths,
    profile: &mut crate::config::ModelProfile,
    tool_specs: &mut Vec<crate::provider::ToolSpec>,
) -> Option<probe::ToolProbe> {
    if std::env::var_os("LH_SKIP_TOOL_PROBE").is_some() {
        return None;
    }
    if provider.model().trim().is_empty() {
        return None;
    }
    let metadata = probe::tool_probe_metadata(provider).await;
    let key = probe::tool_probe_key(provider, metadata.build.as_deref());
    let cache_path = paths.data_dir.join("tool-probe-cache.json");
    if let Some(cached) = probe::load_tool_probe(&cache_path, &key)
        && cached.native()
        && cached.model == provider.model()
        && cached.build.as_deref() == metadata.build.as_deref()
    {
        apply_tool_probe(&cached, profile, tool_specs);
        return Some(cached);
    }
    let probed = tokio::time::timeout(
        Duration::from_secs(20),
        probe::probe_tool_conformance(provider, &metadata),
    )
    .await;
    match probed {
        Ok(Ok(result)) => {
            if let Err(error) = probe::store_tool_probe(&cache_path, &key, &result) {
                tracing::warn!(%error, "cannot write the tool probe cache");
            }
            apply_tool_probe(&result, profile, tool_specs);
            Some(result)
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "tool-call conformance probe did not run");
            None
        }
        Err(_) => {
            tracing::warn!("tool-call conformance probe timed out; assuming configured tool mode");
            None
        }
    }
}

/// A non-native probe result switches the effective mode to the text/JSON
/// repair protocol. A server that rejects tool definitions also stops
/// receiving them, so the next request is not rejected the same way.
fn apply_tool_probe(
    probe: &probe::ToolProbe,
    profile: &mut crate::config::ModelProfile,
    tool_specs: &mut Vec<crate::provider::ToolSpec>,
) {
    if probe.native() {
        return;
    }
    profile.tool_mode = crate::config::ToolMode::Repair;
    if probe.outcome == probe::ToolProbeOutcome::Rejected400 {
        tool_specs.clear();
    }
}

fn warn_if_remote(endpoint: &str) {
    let host = endpoint
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(endpoint)
        .split('/')
        .next()
        .unwrap_or("");
    let host = host.split(':').next().unwrap_or(host);
    let local = matches!(
        host,
        "localhost" | "127.0.0.1" | "::1" | "[::1]" | "0.0.0.0"
    ) || host.starts_with("127.");
    if !local {
        tracing::warn!(%endpoint, "non-local inference endpoint configured");
        eprintln!(
            "warning: inference endpoint {endpoint} is not local; prompts and tool output leave this machine"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A LocalAI-style gateway: `/v1/models` without context sizes, no
    /// `/props`, and llama.cpp `/slots` for whichever backend is loaded.
    async fn gateway(n_ctx: u64) -> String {
        use axum::routing::{get, post};
        let models = || async {
            axum::Json(serde_json::json!({"object": "list", "data": [
                {"id": "spark26", "object": "model", "owned_by": "localai"},
                {"id": "gemma", "object": "model", "owned_by": "localai"},
            ]}))
        };
        let slots =
            move || async move { axum::Json(serde_json::json!([{"id": 0, "n_ctx": n_ctx}])) };
        let chat = || async {
            let body = concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            );
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                body,
            )
        };
        let app = axum::Router::new()
            .route("/v1/models", get(models))
            .route("/slots", get(slots))
            .route("/v1/chat/completions", post(chat));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn gateway_window_is_detected_after_the_first_reply_and_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            cwd,
            project_id: "test".into(),
            sessions_dir_override: None,
        };
        let mut config = Config::builtin();
        config.model.endpoint = gateway(65_536).await;
        config.model.model = "gemma".into();
        let runtime = create_runtime(
            config,
            paths,
            RuntimeOptions {
                no_session: true,
                ..RuntimeOptions::default()
            },
        )
        .await
        .unwrap();

        // Several models behind the gateway: /slots is ambiguous at startup.
        let core = runtime.core.clone();
        assert_eq!(core.window_source, WindowSource::Default);
        assert!(!core.effective_window().1.is_reliable());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = crate::agent::spawn_turn(
            core.clone(),
            "hi".into(),
            tx,
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(outcome.error.is_none(), "{:?}", outcome.error);
        let mut detected = None;
        while let Ok(event) = rx.try_recv() {
            if let crate::agent::AgentEvent::WindowDetected { window, .. } = event {
                detected = Some(window);
            }
        }
        assert_eq!(detected, Some(65_536));
        let (window, source) = core.effective_window();
        assert_eq!(window, 65_536);
        assert!(source.is_reliable());

        // Switching away and back reuses the detected window.
        let mut other = core.config.clone();
        other.model.model = "spark26".into();
        let spark = rebuild_core(&core, other).await.unwrap();
        assert_eq!(
            spark.effective_window().0,
            crate::config::DEFAULT_CONTEXT_WINDOW
        );
        let mut back = spark.config.clone();
        back.model.model = "gemma".into();
        let gemma = rebuild_core(&spark, back).await.unwrap();
        assert_eq!(gemma.effective_window().0, 65_536);
        assert_eq!(
            gemma.window_source,
            WindowSource::Probed(probe::WindowProbe::Remembered)
        );
    }

    #[tokio::test]
    async fn rejected_tool_probe_switches_to_repair_and_is_recorded() {
        use axum::http::StatusCode;
        use axum::routing::post;
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(|| async {
                (
                    StatusCode::BAD_REQUEST,
                    "tools are not supported; enable the tool parser",
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut config = Config::builtin();
        config.model.endpoint = format!("http://{address}");
        config.model.model = "spark26".into();
        config.model.context_window = Some(8192);
        let runtime = create_runtime(
            config,
            paths,
            RuntimeOptions {
                no_session: true,
                ..RuntimeOptions::default()
            },
        )
        .await
        .unwrap();
        let probe = runtime.core.tool_probe.as_ref().expect("probe ran");
        assert_eq!(
            probe.outcome,
            probe::ToolProbeOutcome::Rejected400,
            "{:?}",
            probe.outcome
        );
        assert_eq!(
            runtime.core.model_profile.tool_mode,
            crate::config::ToolMode::Repair
        );
        assert!(
            runtime.core.tool_specs.is_empty(),
            "a 400 must stop sending tool definitions"
        );
        assert!(
            runtime
                .core
                .system_prompt
                .contains("<tool_call>{\"name\":\"tool_name\""),
            "the repair protocol must name the wire format"
        );
        let session = runtime.session.lock().unwrap();
        let recorded = session
            .latest_custom_on_path("tool-probe")
            .expect("tool probe is in the session header");
        match &recorded.kind {
            EntryKind::Custom(custom) => {
                assert_eq!(custom.payload["outcome"], "rejected_400");
                assert_eq!(custom.payload["mode"], "repair");
            }
            _ => panic!("wrong entry type"),
        }
        assert!(
            session
                .warnings
                .iter()
                .any(|warning| warning.contains("tool-call probe")),
            "missing warning: {:?}",
            session.warnings
        );
    }

    #[tokio::test]
    async fn native_tool_probe_is_skipped_on_the_next_start_for_the_same_model() {
        use axum::routing::post;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    axum::Json(serde_json::json!({
                        "choices": [{
                            "message": {
                                "role": "assistant",
                                "content": null,
                                "tool_calls": [{
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {
                                        "name": "probe_echo",
                                        "arguments": "{\"value\":\"ok\"}"
                                    }
                                }]
                            }
                        }]
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let endpoint = format!("http://{address}");
        let config = crate::config::ModelConfig {
            endpoint,
            ..Default::default()
        };
        let provider = OpenAiCompatProvider::new(&config, "spark26").unwrap();
        let mut profile = crate::config::ModelProfile::default();
        let mut specs = vec![crate::provider::ToolSpec {
            name: "read".into(),
            description: "read".into(),
            parameters: serde_json::json!({"type":"object"}),
        }];
        let first = resolve_tool_probe(&provider, &paths, &mut profile, &mut specs)
            .await
            .expect("first probe");
        assert!(first.native());
        let second = resolve_tool_probe(&provider, &paths, &mut profile, &mut specs)
            .await
            .expect("cached probe");
        assert!(second.native());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "a known-good probe must not run again for the same model/build"
        );
    }

    #[tokio::test]
    async fn starts_without_a_model_when_the_endpoint_is_down() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(cwd.join("src")).unwrap();
        std::fs::write(cwd.join("src/main.rs"), "fn main() {}\n").unwrap();
        let paths = Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            cwd,
            project_id: "test".into(),
            sessions_dir_override: None,
        };
        let mut config = Config::builtin();
        config.model.endpoint = endpoint;
        config.model.model.clear();

        let runtime = create_runtime(
            config,
            paths,
            RuntimeOptions {
                no_session: true,
                ..RuntimeOptions::default()
            },
        )
        .await
        .expect("the TUI must open without a reachable model");

        assert!(!runtime.core.logged_in());
        assert!(
            runtime.core.system_prompt.contains("## Repository map"),
            "prompt lacks repository map"
        );
        assert!(
            runtime.core.system_prompt.contains("src/main.rs"),
            "prompt lacks the source file"
        );
        assert!(
            runtime.core.system_prompt.contains("fn main"),
            "prompt lacks the symbol"
        );
        let session = runtime.session.lock().unwrap();
        assert!(
            session
                .warnings
                .iter()
                .any(|warning| warning.contains("not logged in")),
            "missing login warning: {:?}",
            session.warnings
        );
    }

    fn unreachable_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        endpoint
    }

    #[tokio::test]
    async fn explicit_no_sandbox_is_recorded_in_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut config = Config::builtin();
        config.model.endpoint = unreachable_endpoint();
        config.model.model = "spark26".into();
        config.model.context_window = Some(8192);
        config.sandbox = crate::config::SandboxMode::Off;
        config.sandbox_explicit = true;

        let runtime = create_runtime(config, paths, RuntimeOptions::default())
            .await
            .unwrap();
        let session = runtime.session.lock().unwrap();
        let entry = session
            .latest_custom_on_path("sandbox")
            .expect("sandbox mode is recorded");
        match &entry.kind {
            EntryKind::Custom(custom) => {
                assert_eq!(custom.payload["mode"], "off");
                assert_eq!(custom.payload["backend"], "off");
            }
            _ => panic!("wrong entry type"),
        }
        assert!(
            session
                .warnings
                .iter()
                .any(|warning| warning.contains("--no-sandbox")),
            "missing warning: {:?}",
            session.warnings
        );
    }

    #[tokio::test]
    async fn failed_start_command_is_reported_without_the_retry_window() {
        let mut config = Config::builtin();
        config.model.endpoint = unreachable_endpoint();
        config.model.model = "spark26".into();
        config.model.start_command = "exit 7".into();
        let provider =
            OpenAiCompatProvider::new(&config.model, config.model.model.clone()).unwrap();

        let started = std::time::Instant::now();
        let error = ensure_endpoint(&provider, &config.model)
            .await
            .expect_err("a failed command must not wait for the endpoint");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "waited out the retry window: {error:#}"
        );
        assert!(
            format!("{error:#}").contains("model.start_command failed"),
            "{error:#}"
        );
    }

    fn test_paths(dir: &std::path::Path) -> Paths {
        let cwd = dir.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        Paths {
            config_dir: dir.join("config"),
            data_dir: dir.join("data"),
            cwd,
            project_id: "test".into(),
            sessions_dir_override: None,
        }
    }

    #[tokio::test]
    async fn start_command_does_not_hold_the_runtime_open() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut config = Config::builtin();
        config.model.endpoint = unreachable_endpoint();
        config.model.model = "spark26".into();
        config.model.start_command = "sleep 30".into();
        config.model.context_window = Some(131_072);

        let started = std::time::Instant::now();
        let runtime = create_runtime(
            config,
            paths,
            RuntimeOptions {
                no_session: true,
                ..RuntimeOptions::default()
            },
        )
        .await
        .unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "create_runtime waited for model.start_command"
        );
        let gate = runtime
            .core
            .startup
            .as_ref()
            .expect("start_command should be backgrounded");
        assert!(
            gate.pending(),
            "start_command finished before the runtime was returned"
        );
    }

    #[tokio::test]
    async fn background_start_failure_becomes_a_session_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = test_paths(dir.path());
        let mut config = Config::builtin();
        config.model.endpoint = unreachable_endpoint();
        config.model.model = "spark26".into();
        config.model.start_command = "exit 7".into();
        config.model.context_window = Some(131_072);

        let runtime = create_runtime(
            config,
            paths,
            RuntimeOptions {
                no_session: true,
                ..RuntimeOptions::default()
            },
        )
        .await
        .unwrap();
        let gate = runtime.core.startup.as_ref().unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            gate.ready(&CancellationToken::new()),
        )
        .await
        .expect("bring-up did not finish")
        .expect("bring-up was cancelled");
        assert!(result.is_err());
        let session = runtime.session.lock().unwrap();
        assert!(
            session
                .warnings
                .iter()
                .any(|warning| warning.contains("model.start_command failed")),
            "missing warning: {:?}",
            session.warnings
        );
    }

    #[tokio::test]
    async fn startup_gate_releases_waiters_on_result_and_cancel() {
        let (handle, gate) = crate::agent::StartupGate::channel();
        assert!(gate.pending());
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(gate.ready(&cancelled).await.is_none());

        handle.finish(Err("endpoint down".into()));
        assert!(!gate.pending());
        assert_eq!(
            gate.ready(&CancellationToken::new()).await,
            Some(Err("endpoint down".to_string()))
        );
    }
}

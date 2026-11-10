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
use crate::context::TokenBudget;
use crate::external;
use crate::paths::Paths;
use crate::provider::{OpenAiCompatProvider, Provider, probe};
use crate::security::SecurityState;
use crate::session::{APP_VERSION, Session, SessionData};
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

    let (_, profile) = config.model_profile();
    let window_cache: Arc<Mutex<std::collections::HashMap<String, u32>>> = Default::default();
    let (context_window, window_source) =
        resolve_window(&config, &probe_provider, None, &window_cache).await;
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
            },
            options.no_session,
        )?,
    };
    session.warnings.extend(startup_warnings);
    for warning in &session.warnings {
        tracing::warn!("{warning}");
    }

    let registry = ToolRegistry::builtin();
    let external_tools = external::load_tools(&paths, config.trust).unwrap_or_else(|err| {
        tracing::warn!(%err, "external tools were not loaded");
        Vec::new()
    });
    let tools = registry
        .select(config.profile(), &external_tools)
        .map_err(anyhow::Error::msg)?;
    let tool_specs = ToolRegistry::specs(&tools);
    let skills = SkillIndex::load(&paths, config.trust);
    let repo_brief = registry
        .repo()
        .brief(&paths.cwd, crate::tools::repo_map::BRIEF_BUDGET);
    let system_prompt =
        prompt::build_system_prompt(&config, &paths, &skills, &tools, Some(&repo_brief));
    let mut budget = TokenBudget::new(context_window, config.model.max_output_tokens);
    let sampled = tokio::time::timeout(
        Duration::from_secs(2),
        probe::token_count(&probe_provider, &system_prompt),
    )
    .await
    .ok()
    .flatten();
    if let Some(tokens) = sampled
        && tokens > 0
    {
        budget.calibrate(
            system_prompt.chars().count(),
            tokens.min(u32::MAX as usize) as u32,
        );
    }
    let limits = RunLimits::from(&config.budgets);
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
        session: session_handle.clone(),
        artifacts,
        reads,
        bg,
        security,
        budget,
        limits,
        system_prompt,
        model_profile: profile,
        cancel: CancellationToken::new(),
        tool_timeout: Duration::from_secs(660),
        path_jail: options.path_jail,
        server_window: Default::default(),
        window_source,
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
    let reuse = same_target.then_some((old.budget.context_window, old.window_source));
    let (context_window, window_source) =
        resolve_window(&config, provider.as_ref(), reuse, &old.window_cache).await;
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
    let tool_specs = ToolRegistry::specs(&tools);
    let skills = SkillIndex::load(&old.paths, config.trust);
    let repo_brief = registry
        .repo()
        .brief(&old.paths.cwd, crate::tools::repo_map::BRIEF_BUDGET);
    let system_prompt =
        prompt::build_system_prompt(&config, &old.paths, &skills, &tools, Some(&repo_brief));
    let (_, model_profile) = config.model_profile();
    let mut budget = TokenBudget::new(context_window, config.model.max_output_tokens);
    if same_target {
        // The tokenizer calibration only holds for the same model.
        budget.chars_per_token = old.budget.chars_per_token;
    }
    let limits = RunLimits::from(&config.budgets);
    Ok(Arc::new(Core {
        config,
        paths: old.paths.clone(),
        provider,
        registry,
        tools,
        tool_specs,
        session: old.session.clone(),
        artifacts: old.artifacts.clone(),
        reads: old.reads.clone(),
        bg: old.bg.clone(),
        security: old.security.clone(),
        budget,
        limits,
        system_prompt,
        model_profile,
        cancel: old.cancel.clone(),
        tool_timeout: old.tool_timeout,
        path_jail: old.path_jail,
        server_window,
        window_source,
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

/// Context window for `provider`'s model: an explicit config or model-profile
/// value, else a probe of the endpoint, else the default. `reuse` carries a
/// previous probe for the same endpoint and model.
async fn resolve_window(
    config: &Config,
    provider: &dyn Provider,
    reuse: Option<(u32, WindowSource)>,
    cache: &Mutex<std::collections::HashMap<String, u32>>,
) -> (u32, WindowSource) {
    let (_, profile) = config.model_profile();
    if profile.context_window.is_some() || config.model.context_window.is_some() {
        return (config.context_window(), WindowSource::Configured);
    }
    if let Some((window, source @ WindowSource::Probed(_))) = reuse {
        return (window, source);
    }
    if provider.model().trim().is_empty() {
        return (config.context_window(), WindowSource::Default);
    }
    let key = Core::window_key(&config.model.endpoint, provider.model());
    if let Some(window) = cache.lock().ok().and_then(|cache| cache.get(&key).copied()) {
        return (window, WindowSource::Probed(probe::WindowProbe::Remembered));
    }
    let probed = tokio::time::timeout(
        Duration::from_secs(3),
        probe::probe_context_window(provider),
    )
    .await
    .ok()
    .flatten();
    match probed {
        Some((window, how)) => (window, WindowSource::Probed(how)),
        None => (config.context_window(), WindowSource::Default),
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

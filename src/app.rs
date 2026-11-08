//! Build the runtime core: resolve the model, open or create a session, load
//! external tools, and compute the active tool list and system prompt.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use tokio_util::sync::CancellationToken;

use crate::agent::{Core, RunLimits, prompt};
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
        tracing::warn!(
            status = %output.status,
            stderr = %stderr,
            "model.start_command exited non-zero"
        );
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
    let probe_provider = OpenAiCompatProvider::new(&config.model, model.clone())?;
    let mut startup_warnings = Vec::new();
    if let Err(error) = ensure_endpoint(&probe_provider, &config.model).await {
        tracing::warn!(%error, "model endpoint is not available");
        startup_warnings.push(format!("model endpoint is not available: {error}"));
    }
    if model.is_empty() {
        // The TUI must open without a model so the user can configure one with
        // /login. Auto-select only when the endpoint is reachable and names one
        // model; every other outcome becomes a warning, not a startup error.
        let api = crate::provider::endpoint_urls(&config.model.endpoint).api;
        let probed =
            tokio::time::timeout(Duration::from_secs(3), probe::list_models(&probe_provider)).await;
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
    config.model.model = model.clone();
    let probe_provider = OpenAiCompatProvider::new(&config.model, model.clone())?;

    // Optional context probe only when no explicit context was configured.
    let (_, profile) = config.model_profile();
    if !model.is_empty()
        && profile.context_window.is_none()
        && config.model.context_window.is_none()
    {
        let probed = tokio::time::timeout(
            Duration::from_secs(3),
            probe::probe_context_window(&probe_provider),
        )
        .await
        .ok()
        .flatten();
        if let Some(window) = probed {
            config.model.context_window = Some(window);
        }
    }
    let context_window = config.context_window();
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
    let budget = TokenBudget::new(config.context_window(), config.model.max_output_tokens);
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
        server_window: Default::default(),
    }))
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
}

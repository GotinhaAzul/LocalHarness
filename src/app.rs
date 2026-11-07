//! Build the runtime core: resolve the model, open or create a session, load
//! external tools, and compute the active tool list and system prompt.

use std::path::PathBuf;
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

/// Resolve the model, open the session, and assemble the core.
pub async fn create_runtime(
    mut config: Config,
    paths: Paths,
    options: RuntimeOptions,
) -> Result<Runtime> {
    if options.no_session && (options.resume.is_some() || options.continue_last) {
        bail!("--no-session cannot be combined with --continue or --resume");
    }

    if let Ok(endpoint) = std::env::var("LH_ENDPOINT") {
        if !endpoint.trim().is_empty() {
            config.model.endpoint = endpoint;
        }
    }

    let mut model = config.model.model.trim().to_string();
    let probe_provider = OpenAiCompatProvider::new(&config.model, model.clone())?;
    if model.is_empty() {
        let models = probe::list_models(&probe_provider)
            .await
            .with_context(|| {
                format!(
                    "model is not set and GET {}/v1/models failed. Start the inference server, set --model, or set LH_MODEL.",
                    config.model.endpoint.trim_end_matches('/')
                )
            })?;
        match models.len() {
            0 => bail!(
                "endpoint {} reports no models; load a model or set --model",
                config.model.endpoint
            ),
            1 => model = models[0].clone(),
            _ => bail!(
                "endpoint has multiple models ({}); choose one with --model",
                models.join(", ")
            ),
        }
    }
    config.model.model = model.clone();

    // Optional context probe only when no explicit context was configured.
    let (_, profile) = config.model_profile();
    if profile.context_window.is_none() && config.model.context_window.is_none() {
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
    let provider: Arc<dyn Provider> = Arc::new(OpenAiCompatProvider::new(&config.model, model.clone())?);

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
    let session = match session_path {
        Some(path) => {
            let session = Session::open(&path)
                .with_context(|| format!("cannot resume {}", path.display()))?;
            if let Some(header) = session.header() {
                if header.endpoint != config.model.endpoint {
                    tracing::warn!(
                        old = %header.endpoint,
                        new = %config.model.endpoint,
                        "resumed session used a different endpoint"
                    );
                }
                if header.model != model {
                    tracing::warn!(old = %header.model, new = %model, "resumed session used a different model");
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
    let system_prompt = prompt::build_system_prompt(&config, &paths, &skills, &tools);
    let budget = TokenBudget::new(context_window, config.model.max_output_tokens);
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
    };

    Ok(Runtime {
        core: Arc::new(core),
        session: session_handle,
    })
}

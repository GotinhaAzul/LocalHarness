//! CLI parsing, logging, headless mode, and TUI launch.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, TurnStatus, run_turn};
use crate::app::{RuntimeOptions, create_runtime};
use crate::config::{ColorMode, Config};
use crate::paths::Paths;
use crate::ui::{self, UiOptions};

#[derive(Debug, Parser)]
#[command(
    name = "lh",
    version,
    about = "Local-first agent harness for coding and security work"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Run one turn without the TUI and stream the answer to stdout.
    #[arg(long, short = 'p', value_name = "PROMPT")]
    pub print: Option<String>,

    /// With --print, emit newline-delimited JSON events.
    #[arg(long, requires = "print")]
    pub json: bool,

    /// Endpoint base URL, for example http://127.0.0.1:8080.
    #[arg(long, global = true, value_name = "URL")]
    pub endpoint: Option<String>,

    /// Model id; auto-selected when the endpoint exposes exactly one model.
    #[arg(long, global = true, value_name = "MODEL")]
    pub model: Option<String>,

    /// Model profile name from config, or "auto".
    #[arg(long = "model-profile", global = true, value_name = "NAME")]
    pub model_profile: Option<String>,

    /// Tool profile (full, coding, security, minimal, or a custom profile).
    #[arg(long, global = true, value_name = "NAME")]
    pub profile: Option<String>,

    /// Theme name.
    #[arg(long, global = true, value_name = "NAME")]
    pub theme: Option<String>,

    /// Bearer token for the endpoint.
    #[arg(long, global = true, value_name = "KEY")]
    pub api_key: Option<String>,

    /// Trust project-local .harness config, profiles, skills, workflows, and tools.
    #[arg(long, global = true)]
    pub trust: bool,

    /// Do not write a session or artifacts.
    #[arg(long, global = true)]
    pub no_session: bool,

    /// Resume the most recent session for this project.
    #[arg(long = "continue", conflicts_with = "resume")]
    pub continue_last: bool,

    /// Resume a session by id or path.
    #[arg(long, value_name = "ID|PATH")]
    pub resume: Option<String>,

    /// Restrict file tools to the working directory (bash is not restricted).
    #[arg(long, global = true)]
    pub path_jail: bool,

    /// Engagement name for scope and findings.
    #[arg(long, global = true, value_name = "NAME")]
    pub engagement: Option<String>,

    /// Disable color output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Keep artifacts and skip retention cleanup.
    #[arg(long, conflicts_with = "clean_artifacts")]
    pub keep_artifacts: bool,

    /// Delete this session's artifacts before starting.
    #[arg(long)]
    pub clean_artifacts: bool,

    /// Enable debug-level diagnostic logs.
    #[arg(long, global = true)]
    pub debug: bool,

    /// Run bash and bg inside an OS sandbox. PROFILE is read-only or
    /// workspace-write (default when the flag is present). Fails closed when
    /// no sandbox backend is available.
    #[arg(long, global = true, value_name = "PROFILE", num_args = 0..=1, default_missing_value = "workspace-write")]
    pub sandbox: Option<String>,

    /// Explicitly run bash and bg without a sandbox. Required to restore the
    /// pre-sandbox behavior; recorded in the session.
    #[arg(long, global = true, conflicts_with = "sandbox")]
    pub no_sandbox: bool,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Print the engine capability manifest and profile mismatches.
    Doctor {
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
        /// Print the exact server command or config change for each mismatch.
        #[arg(long)]
        fix: bool,
    },
}

pub async fn run(cli: Cli) -> Result<i32> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let mut paths = Paths::discover(&cwd)?;

    let trust = cli.trust || std::env::var("LH_TRUST").is_ok();
    let project_config = if trust {
        Some(paths.project_dir().join("config.toml"))
    } else {
        None
    };
    let mut config = Config::load(&paths.config_file(), project_config.as_deref())?;
    if trust {
        let verify_path = paths.project_dir().join("verify.toml");
        if verify_path.exists() {
            config.verify = crate::config::VerifyConfig::load(&verify_path)?;
        }
    }

    let project_profiles = paths.project_dir().join("profiles");
    config.load_profile_dirs(
        &paths.global_profiles_dir(),
        if trust { Some(&project_profiles) } else { None },
    )?;
    if !config.session.dir.as_os_str().is_empty() {
        paths.sessions_dir_override = Some(config.session.dir.clone());
    }

    if let Some(endpoint) = cli.endpoint {
        config.model.endpoint = endpoint;
    }
    if let Some(model) = cli.model {
        config.model.model = model;
    }
    if let Some(api_key) = cli.api_key {
        config.model.api_key = api_key;
    }
    if let Some(profile) = cli.profile {
        config.profile_name = profile;
    }
    if let Some(profile) = cli.model_profile {
        config.model_profile_override = Some(profile);
    }
    if let Some(theme) = cli.theme {
        config.ui.theme = theme;
    }
    if let Some(profile) = &cli.sandbox {
        config.sandbox = crate::config::SandboxMode::parse(profile).ok_or_else(|| {
            anyhow::anyhow!("unknown sandbox profile {profile:?}; use read-only or workspace-write")
        })?;
        config.sandbox_explicit = true;
    }
    if cli.no_sandbox {
        config.sandbox = crate::config::SandboxMode::Off;
        config.sandbox_explicit = true;
    }
    config.trust = trust;
    config.session.no_session |= cli.no_session;
    if cli.keep_artifacts {
        config.session.keep_artifacts = true;
    }
    if cli.clean_artifacts {
        config.session.clean_artifacts = true;
    }
    let no_color = cli.no_color || std::env::var_os("NO_COLOR").is_some();
    if no_color {
        config.ui.color_mode = ColorMode::Ansi16;
    }
    if cli.debug {
        init_logging(&paths, true);
    } else {
        init_logging(&paths, false);
    }

    if let Some(Command::Doctor { json, fix }) = &cli.command {
        return run_doctor(&config, &paths, *json, *fix).await;
    }

    let resume = match (&cli.resume, cli.continue_last) {
        (Some(id_or_path), _) => {
            let direct = PathBuf::from(id_or_path);
            if direct.is_file() {
                Some(direct)
            } else {
                let candidate = paths
                    .project_sessions_dir()
                    .join(format!("{id_or_path}.jsonl"));
                if candidate.is_file() {
                    Some(candidate)
                } else {
                    bail!(
                        "no session {id_or_path:?} in {}; run `lh --continue` for the latest session",
                        paths.project_sessions_dir().display()
                    );
                }
            }
        }
        (None, true) => None,
        (None, false) => None,
    };

    let options = RuntimeOptions {
        path_jail: cli.path_jail,
        engagement: cli.engagement.clone(),
        no_session: config.session.no_session,
        resume,
        continue_last: cli.continue_last,
    };

    let runtime = create_runtime(config.clone(), paths.clone(), options).await?;

    if cli.print.is_some() && !runtime.core.logged_in() {
        bail!(
            "no model is selected. Start the inference server and run `/login` in the TUI, or pass --model (or set LH_MODEL)."
        );
    }

    if config.session.clean_artifacts {
        if let Err(err) = runtime.core.artifacts.clean_all() {
            tracing::warn!(%err, "cannot clean artifacts");
        }
    } else if !config.session.keep_artifacts && !config.session.no_session {
        let _ = runtime
            .core
            .artifacts
            .cleanup_older_than(config.session.artifact_retention_days);
        let _ = crate::session::artifact::cleanup_root(
            &paths.artifacts_root(),
            config.session.artifact_retention_days,
        );
    }

    match cli.print.clone() {
        Some(prompt) => run_headless(runtime, prompt, cli.json).await,
        None => {
            let ui_options = UiOptions {
                no_color: no_color || config.ui.color_mode == ColorMode::Ansi16,
                trust,
                skills: crate::skills::SkillIndex::load(&paths, trust),
            };
            ui::run(runtime, ui_options).await?;
            Ok(0)
        }
    }
}

/// `lh doctor`: fingerprint the engine and print a capability manifest. Only
/// metadata probes are used; no chat completion and no model load.
async fn run_doctor(config: &Config, paths: &Paths, json: bool, fix: bool) -> Result<i32> {
    let mut model = config.model.model.trim().to_string();
    let probe_provider = crate::provider::OpenAiCompatProvider::new(&config.model, model.clone())?;
    if model.is_empty() {
        // Auto-select only when the endpoint names exactly one model; a
        // doctor run must not load or choose anything.
        if let Ok(models) = crate::provider::probe::list_models(&probe_provider).await
            && models.len() == 1
        {
            model = models[0].clone();
        }
    }
    let provider = crate::provider::OpenAiCompatProvider::new(&config.model, model)?;
    let (_, profile) = config.model_profile();
    let configured = profile
        .context_window
        .or(config.model.context_window)
        .unwrap_or(0);
    let manifest = crate::doctor::DoctorManifest::probe(&provider, configured).await;
    if json {
        println!("{}", manifest.to_json_pretty());
    } else {
        print!("{}", manifest.to_text());
    }
    if fix && !json {
        for line in &manifest.fixes {
            println!("fix: {line}");
        }
    }
    let key = crate::doctor::cache_key(&provider, &manifest);
    let path = paths.data_dir.join("doctor-cache.json");
    if let Err(error) = crate::doctor::store_cached(&path, &key, &manifest) {
        tracing::warn!(%error, "cannot write the doctor cache");
    }
    Ok(0)
}

async fn run_headless(runtime: crate::app::Runtime, prompt: String, json: bool) -> Result<i32> {
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let core = runtime.core.clone();
    let cancel_turn = cancel.clone();
    let handle = tokio::spawn(async move { run_turn(core, prompt, tx, cancel_turn).await });

    let mut first_signal = false;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                if first_signal {
                    break;
                }
                first_signal = true;
                cancel.cancel();
                if json {
                    println!("{}", json!({"type":"cancelled"}));
                } else {
                    eprintln!("cancelling...");
                }
            }
            event = rx.recv() => {
                match event {
                    Some(event) => {
                        if render_event(event, json)?.is_some() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    let outcome = handle.await.context("agent task panicked")?;
    let status = outcome.status;
    let _ = std::io::stdout().flush();
    Ok(match status {
        TurnStatus::Done => 0,
        TurnStatus::Cancelled => 130,
        TurnStatus::Budget | TurnStatus::Stalled | TurnStatus::ContextTruncated => 2,
        TurnStatus::Error | TurnStatus::Blocked => 1,
    })
}

/// Render one event in plain or JSON mode. Returns the final status when done.
fn render_event(event: AgentEvent, json: bool) -> Result<Option<TurnStatus>> {
    if json {
        let value = match &event {
            AgentEvent::TurnStarted { model, profile } => {
                json!({"type":"started","model":model,"profile":profile})
            }
            AgentEvent::State(state) => json!({"type":"state","state":format!("{state:?}")}),
            AgentEvent::TextDelta(text) => json!({"type":"text","text":text}),
            AgentEvent::ReasoningDelta(text) => json!({"type":"reasoning","text":text}),
            AgentEvent::ToolStarted {
                call_id,
                name,
                summary,
            } => json!({"type":"tool_started","call_id":call_id,"name":name,"summary":summary}),
            AgentEvent::ToolFinished {
                call_id,
                name,
                is_error,
                duration_ms,
                preview,
                artifact,
            } => {
                json!({"type":"tool_finished","call_id":call_id,"name":name,"is_error":is_error,"duration_ms":duration_ms,"preview":preview,"artifact":artifact})
            }
            AgentEvent::Usage(usage) => {
                json!({"type":"usage","prompt_tokens":usage.prompt_tokens,"completion_tokens":usage.completion_tokens})
            }
            AgentEvent::Context { used, window } => {
                json!({"type":"context","used":used,"window":window})
            }
            AgentEvent::WindowDetected { window, source } => {
                json!({"type":"window_detected","window":window,"source":source})
            }
            AgentEvent::Compacted {
                summary,
                tokens_before,
            } => json!({"type":"compacted","summary":summary,"tokens_before":tokens_before}),
            AgentEvent::Notice(message) => json!({"type":"notice","message":message}),
            AgentEvent::Guard { code, message } => {
                json!({"type":"guard","code":code,"message":message})
            }
            AgentEvent::Error {
                code,
                message,
                recoverable,
            } => json!({"type":"error","code":code,"message":message,"recoverable":recoverable}),
            AgentEvent::TurnDone(outcome) => {
                json!({"type":"done","status":format!("{:?}", outcome.status)})
            }
        };
        println!("{}", serde_json::to_string(&value)?);
        if let AgentEvent::TurnDone(outcome) = event {
            return Ok(Some(outcome.status));
        }
        return Ok(None);
    }

    match event {
        AgentEvent::TextDelta(text) => {
            print!("{text}");
            std::io::stdout().flush()?;
        }
        AgentEvent::ReasoningDelta(_) => {}
        AgentEvent::ToolStarted { name, summary, .. } => {
            eprintln!("[tool] {name} {summary}");
        }
        AgentEvent::ToolFinished {
            name,
            is_error,
            preview,
            ..
        } => {
            if is_error {
                eprintln!("[error] {name}: {preview}");
            } else {
                eprintln!("[ok] {name}");
            }
        }
        AgentEvent::Notice(message) => eprintln!("[notice] {message}"),
        AgentEvent::Guard { code, message } => eprintln!("[{code}] {message}"),
        AgentEvent::Error { code, message, .. } => eprintln!("[{code}] {message}"),
        AgentEvent::Usage(usage) => {
            eprintln!(
                "[usage] prompt {} completion {}",
                usage.prompt_tokens, usage.completion_tokens
            );
        }
        AgentEvent::Compacted { tokens_before, .. } => {
            eprintln!("[compact] ~{tokens_before} tokens before compaction");
        }
        AgentEvent::TurnDone(outcome) => {
            println!();
            return Ok(Some(outcome.status));
        }
        _ => {}
    }
    Ok(None)
}

fn init_logging(paths: &Paths, debug: bool) {
    use tracing_subscriber::filter::EnvFilter;
    if std::env::var_os("LH_DEBUG_CONTENT").is_some() {
        eprintln!(
            "warning: LH_DEBUG_CONTENT is set. Prompts and tool output may appear in diagnostic logs; unset it unless debugging."
        );
    }
    let filter = if debug {
        EnvFilter::new("localharness=debug,warn")
    } else {
        EnvFilter::new("localharness=warn,error")
    };
    let _ = std::fs::create_dir_all(&paths.data_dir);
    let file_appender = tracing_appender::rolling::never(&paths.data_dir, "localharness.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(non_blocking)
        .with_ansi(false)
        .finish();
    if tracing::subscriber::set_global_default(subscriber).is_ok() {
        std::mem::forget(guard);
    }
}

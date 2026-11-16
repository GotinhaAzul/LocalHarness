//! CLI parsing, logging, headless mode, and TUI launch.

use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, TurnStatus, run_turn};
use crate::app::{RuntimeOptions, create_runtime};
use crate::config::{ColorMode, Config};
use crate::paths::Paths;
use crate::session::Session;
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

    /// JSON Schema file that constrains the final answer. The schema is
    /// lowered to the engine's grammar dialect and every reply is validated.
    #[arg(long = "json-schema", global = true, value_name = "FILE")]
    pub json_schema: Option<PathBuf>,

    /// Tool profile (coding, full, security, minimal, or a custom profile).
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

    /// Disable secret redaction in sessions, artifacts, evidence, and reports.
    /// The choice is recorded in the session header.
    #[arg(long, global = true)]
    pub no_redact: bool,

    /// Do not load AGENTS.md files into the system prompt.
    #[arg(long, global = true)]
    pub no_agents_md: bool,

    /// Add tools to the active profile for this run, e.g. `--tools +fetch`.
    /// Opt-in tools such as `fetch` are enabled only this way or by naming
    /// them in a profile's `tools`.
    #[arg(long, global = true, value_name = "+TOOL,...", value_delimiter = ',')]
    pub tools: Vec<String>,

    /// Resume the most recent session for this project.
    #[arg(long = "continue", conflicts_with = "resume")]
    pub continue_last: bool,

    /// Resume a session by id or path.
    #[arg(long, value_name = "ID|PATH")]
    pub resume: Option<String>,

    /// Restore harness-written files and the conversation to
    /// <SESSION>:<ENTRY>. SESSION may be "last". Refuses when a file changed
    /// outside the harness; add --force to back it up first.
    #[arg(long, value_name = "SESSION:ENTRY", conflicts_with = "print")]
    pub rewind: Option<String>,

    /// With --rewind, back up externally changed files before overwriting.
    #[arg(long, requires = "rewind")]
    pub force: bool,

    /// Restrict file tools to the working directory (bash is not restricted).
    #[arg(long, global = true)]
    pub path_jail: bool,

    /// Engagement name for scope and findings.
    #[arg(long, global = true, value_name = "NAME")]
    pub engagement: Option<String>,

    /// Disable color output.
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Allow sending images to an endpoint that is not loopback or on a
    /// private network (same as `[vision] allow_remote = true`).
    #[arg(long, global = true)]
    pub allow_remote_images: bool,

    /// Send images with their EXIF/XMP metadata instead of stripping it.
    #[arg(long, global = true)]
    pub keep_exif: bool,

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

    /// Permission mode for unmatched calls: ask, accept-edits, plan, or
    /// dont-ask. Overrides `[permissions] mode`; rules still apply.
    #[arg(long, global = true, value_name = "MODE", conflicts_with_all = ["yolo", "plan"])]
    pub mode: Option<String>,

    /// Run every tool call without asking; only deny rules still apply.
    /// Never the default; recorded in the session.
    #[arg(long, global = true, conflicts_with = "plan")]
    pub yolo: bool,

    /// Start in plan mode: read-only tools only, and the turn must produce a
    /// todo plan. With --print, runs one read-only turn, prints the plan, and
    /// exits.
    #[arg(long, global = true)]
    pub plan: bool,
}

#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Show or set up the verify steps for this project.
    Verify {
        /// Detect steps from the project's manifest files and save them for
        /// this project (in the user data directory). Nothing is run.
        #[arg(long)]
        init: bool,
    },
    /// Print the engine capability manifest and profile mismatches.
    Doctor {
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
        /// Print the exact server command or config change for each mismatch.
        #[arg(long)]
        fix: bool,
    },
    /// Inspect and exercise configured MCP servers.
    Mcp {
        #[command(subcommand)]
        action: McpCommand,
    },
    /// List workflows, or run one with deterministic steps.
    Workflow {
        #[command(subcommand)]
        action: WorkflowCommand,
    },
    /// List lifecycle hooks or run one with a sample payload.
    Hooks {
        #[command(subcommand)]
        action: HooksCommand,
    },
    /// Serve the Agent Client Protocol v1 on stdio for editor integrations.
    Acp,
    /// Serve a local HTTP API (sessions, turns, SSE events, cancel) for
    /// scripts and automation. See docs/serve.md.
    Serve {
        /// Address to listen on. Port 0 picks a free port.
        #[arg(long, value_name = "IP:PORT", default_value = "127.0.0.1:0")]
        bind: String,
        /// Permit a non-loopback --bind address (prints a warning).
        #[arg(long)]
        allow_non_loopback: bool,
        /// Where to write the bearer token (mode 0600). Defaults to
        /// <data dir>/serve/token-<port>.
        #[arg(long, value_name = "PATH")]
        token_file: Option<PathBuf>,
        /// Let a session request permission_mode "yolo" (every call the
        /// harness gates allow runs without approval).
        #[arg(long)]
        allow_yolo: bool,
    },
    /// Print shell completions to stdout.
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
    /// Print the roff man page to stdout.
    Man,
    /// Install, update, remove, list, lint, and evaluation-test skills.
    Skill {
        #[command(subcommand)]
        action: SkillCommand,
    },
    /// Inspect context assembly inputs: the hierarchical AGENTS.md manifest.
    Context {
        /// Print the manifest with each loaded file, its bytes, and SHA-256.
        #[arg(long, required_unless_present = "json")]
        show: bool,
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum SkillCommand {
    /// Install a skill from a local directory or a git URL with an optional #ref.
    Add {
        /// Directory, or `git-url#ref`. The ref is a branch, tag, or commit.
        source: String,
        /// Replace an existing copy; the old copy moves to the trash directory.
        #[arg(long)]
        force: bool,
    },
    /// Update one skill, or every locked skill.
    Update {
        /// Skill name; omit to update every locked skill.
        name: Option<String>,
        /// Replace a locally edited copy; the old copy moves to the trash directory.
        #[arg(long)]
        force: bool,
        /// Fail when the lockfile and installed files disagree; change nothing.
        #[arg(long)]
        frozen: bool,
    },
    /// Move an installed skill to the trash directory and drop its lock entry.
    Remove {
        /// Skill name as shown by `lh skill list`.
        name: String,
    },
    /// List installed skills with source, pinned ref, and digest.
    List {
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
        /// Fail when the lockfile and installed files disagree.
        #[arg(long)]
        frozen: bool,
    },
    /// Validate skill names, descriptions, bodies, references, and duplicates.
    Lint {
        /// Lint one skill by name.
        name: Option<String>,
        /// Lint bundled, global, and trusted project skills.
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Measure trigger precision and recall over prompt fixtures.
    Eval {
        /// Directory of .toml or .json fixtures.
        #[arg(long, value_name = "DIR")]
        fixtures: PathBuf,
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum HooksCommand {
    /// List configured hooks.
    List {
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Run one hook with a sample payload for its event.
    Test {
        /// Hook name from [hooks.<name>].
        name: String,
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum WorkflowCommand {
    /// List bundled and installed workflows.
    List {
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Run a workflow: prompt steps, approval gates, conditions, retries, and
    /// declared artifacts.
    Run {
        /// Workflow name (bundled, global, or trusted project).
        name: String,
        /// Continue the last run at its first incomplete step.
        #[arg(long)]
        resume: bool,
        /// Emit newline-delimited JSON events.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, clap::Subcommand)]
pub enum McpCommand {
    /// Connect to every enabled server and list its tools.
    List {
        /// Emit one stable JSON object.
        #[arg(long)]
        json: bool,
    },
    /// Call one tool directly, bypassing the model.
    Call {
        /// Server name from [mcp.<name>].
        server: String,
        /// Remote tool name as the server advertises it.
        tool: String,
        /// Arguments as a JSON object.
        #[arg(default_value = "{}")]
        args: String,
    },
}

pub async fn run(cli: Cli) -> Result<i32> {
    // Packaging commands only print generated text. Handle them before config
    // discovery so a broken config or a missing working directory cannot stop
    // a completion or man-page build.
    match &cli.command {
        Some(Command::Completions { shell }) => return run_completions(*shell),
        Some(Command::Man) => return run_man_page(),
        _ => {}
    }

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
    // Steps the user accepted for this project (`lh verify --init`, the TUI
    // offer). They live in the user's data directory, not the repository.
    if config.verify.steps.is_empty()
        && let Some(steps) = crate::verify_setup::accepted_steps(&paths)
    {
        config.verify.steps = steps;
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
    if let Some(schema) = cli.json_schema {
        config.model.response_schema = schema.display().to_string();
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
    if let Some(mode) = &cli.mode {
        config.permissions.mode = match crate::policy::PermissionMode::parse(mode) {
            Some(crate::policy::PermissionMode::Yolo) => {
                bail!("--mode yolo is not accepted; pass --yolo explicitly")
            }
            Some(mode) => mode,
            None => {
                bail!("unknown permission mode {mode:?}; use ask, accept-edits, plan, or dont-ask")
            }
        };
    }
    if cli.plan {
        config.permissions.mode = crate::policy::PermissionMode::Plan;
    }
    if cli.yolo {
        config.permissions.mode = crate::policy::PermissionMode::Yolo;
    }
    config.trust = trust;
    config.session.no_session |= cli.no_session;
    if cli.no_redact {
        config.session.redact = false;
    }
    if cli.no_agents_md {
        config.prompt.agents_md = false;
    }
    if !cli.tools.is_empty() {
        let name = config.profile_name().to_string();
        if let Some(profile) = config.profiles.get_mut(&name) {
            for tool in &cli.tools {
                let tool = tool.trim().trim_start_matches('+');
                if !tool.is_empty() && !profile.tools.iter().any(|t| t == tool) {
                    profile.tools.push(tool.to_string());
                }
            }
        }
    }
    if cli.keep_artifacts {
        config.session.keep_artifacts = true;
    }
    if cli.allow_remote_images {
        config.vision.allow_remote = true;
    }
    if cli.keep_exif {
        config.vision.keep_exif = true;
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

    if let Some(Command::Skill { action }) = &cli.command {
        return run_skill_command(&paths, trust, action);
    }

    if matches!(&cli.command, Some(Command::Acp)) {
        if cli.print.is_some() || cli.json || cli.continue_last || cli.resume.is_some() {
            bail!(
                "`lh acp` is a stdio server; --print, --json, --continue, and --resume do not apply"
            );
        }
        let options = crate::acp::AcpOptions {
            engagement: cli.engagement.clone(),
            // ACP sessions must be resumable; an ephemeral session cannot
            // be loaded later, so the capability is not advertised.
            no_session: config.session.no_session,
            no_redact: !config.session.redact,
        };
        return crate::acp::run(config, paths, options).await;
    }
    if let Some(Command::Serve {
        bind,
        allow_non_loopback,
        token_file,
        allow_yolo,
    }) = &cli.command
    {
        if cli.print.is_some() || cli.json || cli.continue_last || cli.resume.is_some() {
            bail!(
                "`lh serve` is a server; --print, --json, --continue, and --resume do not apply (resume over HTTP with {{\"resume\": \"<id>\"}})"
            );
        }
        let options = crate::serve::ServeOptions {
            bind: bind.clone(),
            allow_non_loopback: *allow_non_loopback,
            token_file: token_file.clone(),
            allow_yolo: *allow_yolo,
            engagement: cli.engagement.clone(),
            no_session: config.session.no_session,
            no_redact: !config.session.redact,
        };
        return crate::serve::run(config, paths, options).await;
    }
    if let Some(Command::Context { json, .. }) = &cli.command {
        return run_context(&config, &paths, *json);
    }
    if let Some(Command::Verify { init }) = &cli.command {
        return run_verify(&config, &paths, *init);
    }
    if let Some(Command::Doctor { json, fix }) = &cli.command {
        return run_doctor(&config, &paths, *json, *fix).await;
    }
    if let Some(Command::Mcp { action }) = &cli.command {
        return run_mcp(&config, &paths, action).await;
    }
    if let Some(Command::Workflow { action }) = &cli.command {
        return run_workflow_command(
            &config,
            &paths,
            action,
            cli.path_jail,
            cli.engagement.clone(),
        )
        .await;
    }
    if let Some(spec) = &cli.rewind {
        return run_rewind(&paths, spec, cli.force);
    }

    if let Some(Command::Hooks { action }) = &cli.command {
        return run_hooks(&config, &paths, action).await;
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
        no_redact: !config.session.redact,
        resume,
        continue_last: cli.continue_last,
        approver: None,
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

    // No verify steps: say so once, with what was detected. Headless never
    // prompts; the TUI offers the steps once per project.
    if config.verify.steps.is_empty() {
        let detected = crate::tools::project::detect_verify_steps(&paths.cwd);
        if cli.print.is_some() {
            eprintln!(
                "[notice] {}",
                crate::verify_setup::unconfigured_notice(
                    &detected,
                    "Save them with `lh verify --init`."
                )
            );
        } else if !detected.steps.is_empty()
            && crate::verify_setup::take_offer(&paths)
            && let Ok(mut session) = runtime.core.session.lock()
        {
            session
                .warnings
                .push(crate::verify_setup::unconfigured_notice(
                    &detected,
                    "Type /verify accept to save them for this project.",
                ));
        }
    }

    // Every core rebuild shares this manager, so one shutdown stops every
    // MCP server this process started.
    let mcp = runtime.core.mcp.clone();
    let outcome = match cli.print.clone() {
        Some(prompt) => run_headless(runtime, prompt, cli.json).await,
        None => {
            let ui_options = UiOptions {
                no_color: no_color || config.ui.color_mode == ColorMode::Ansi16,
                trust,
                skills: crate::skills::SkillIndex::load(&paths, trust)
                    .advertised_for(&crate::tools::active_packs(&runtime.core.tools)),
            };
            ui::run(runtime, ui_options).await.map(|()| 0)
        }
    };
    mcp.shutdown().await;
    outcome
}

/// `lh verify [--init]`: show the verify steps, or detect and save them.
fn run_verify(config: &Config, paths: &Paths, init: bool) -> Result<i32> {
    if !config.verify.steps.is_empty() && !init {
        println!("verify: {} step(s) configured", config.verify.steps.len());
        for step in &config.verify.steps {
            println!("  {}: {}", step.name, step.command.join(" "));
        }
        return Ok(0);
    }
    if !init {
        let detected = crate::tools::project::detect_verify_steps(&paths.cwd);
        println!(
            "{}",
            crate::verify_setup::unconfigured_notice(
                &detected,
                "Save them with `lh verify --init`."
            )
        );
        return Ok(0);
    }
    let (detected, path) = crate::verify_setup::accept_detected(paths)
        .with_context(|| "cannot save the verify steps")?;
    if detected.steps.is_empty() {
        println!(
            "no verify steps detected in {}; looked for Cargo.toml, package.json, pyproject.toml/setup.py/pytest.ini, and go.mod. Add [[steps]] to a [verify] config section instead.",
            paths.cwd.display()
        );
        return Ok(1);
    }
    println!(
        "saved {} verify step(s) for {} in {}:",
        detected.steps.len(),
        paths.cwd.display(),
        path.display()
    );
    for step in &detected.steps {
        println!("  {}: {}", step.name, step.command);
    }
    Ok(0)
}

/// `lh completions <shell>`: write a completion script for packaging.
fn run_completions(shell: clap_complete::Shell) -> Result<i32> {
    let mut out = std::io::stdout();
    clap_complete::generate(shell, &mut Cli::command(), "lh", &mut out);
    out.flush().context("cannot write shell completions")?;
    Ok(0)
}

/// `lh man`: write the roff man page used by release tarballs and packages.
fn run_man_page() -> Result<i32> {
    let command = Cli::command().name("lh");
    let man = clap_mangen::Man::new(command);
    let mut out = std::io::stdout();
    man.render(&mut out).context("cannot write the man page")?;
    out.flush().context("cannot write the man page")?;
    Ok(0)
}

/// `lh context`: print the AGENTS.md chain and its manifest. This reads local
/// files only; it does not open a session, start a turn, or contact a model.
fn run_context(config: &Config, paths: &Paths, json: bool) -> Result<i32> {
    let agents = crate::agent::agents_md::AgentsMd::load(&paths.cwd, config.prompt.agents_md);
    let skills = crate::skills::SkillIndex::load(paths, config.trust);
    let index = skills.index_text_with(config.skills.index);
    let clipped = skills.clipped_names();
    if json {
        let mut manifest = agents.manifest_json();
        if let Some(object) = manifest.as_object_mut() {
            object.insert(
                "skill_index".into(),
                serde_json::json!({
                    "mode": config.skills.index,
                    "skills": skills.skills.len(),
                    "chars": index.chars().count(),
                    "clipped": clipped,
                }),
            );
        }
        println!("{}", serde_json::to_string_pretty(&manifest)?);
    } else {
        print!("{}", agents.manifest_text());
        println!(
            "skill index: {} skills, {} chars",
            skills.skills.len(),
            index.chars().count()
        );
        if !clipped.is_empty() {
            println!(
                "  clipped (description over {} chars; shorten it): {}",
                crate::skills::INDEX_ENTRY_CAP,
                clipped.join(", ")
            );
        }
        for warning in &agents.warnings {
            eprintln!("warning: {warning}");
        }
    }
    Ok(0)
}

/// The engine a diagnostic should judge capabilities against. Runtime
/// resolution honors `[model] engine` even when a gateway hides the server
/// metadata, so the configured pin wins over the probe result.
fn diagnostic_engine(configured: &str, detected: &str) -> crate::provider::Engine {
    crate::provider::Engine::parse(configured)
        .or_else(|| crate::provider::Engine::parse(detected))
        .unwrap_or(crate::provider::Engine::Generic)
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
    let (profile_name, profile) = config.model_profile();
    let configured = profile
        .context_window
        .or(config.model.context_window)
        .unwrap_or(0);
    let samplers = crate::provider::SamplerSettings::resolve(&config.model, &profile);
    let mut manifest = crate::doctor::DoctorManifest::probe(&provider, configured).await;
    let engine = diagnostic_engine(&config.model.engine, &manifest.engine);
    manifest.warn_unsupported_samplers(&profile_name, engine, &samplers);
    // `lh doctor` never sends a chat completion. Show the last conformance
    // result the runtime stored for this model/build, if any.
    let probe_path = paths.data_dir.join("tool-probe-cache.json");
    let probe_key = crate::provider::probe::tool_probe_key(&provider, manifest.build.as_deref());
    if let Some(cached) = crate::provider::probe::load_tool_probe(&probe_path, &probe_key) {
        manifest.tool_dialect = cached.mode.clone();
        manifest.tool_probe = Some(cached);
    }
    manifest.verify = Some(crate::doctor::VerifyStatus {
        configured: config.verify.steps.len(),
        detected: if config.verify.steps.is_empty() {
            crate::tools::project::detect_verify_steps(&paths.cwd)
                .steps
                .iter()
                .map(|step| format!("{}: {}", step.name, step.command))
                .collect()
        } else {
            Vec::new()
        },
    });
    if let Ok(tools) = crate::tools::ToolRegistry::builtin().select(config.profile(), &[]) {
        let (full_chars, short_chars) = crate::tools::schema_sizes(&tools);
        manifest.tool_schemas = Some(crate::doctor::SchemaSizes {
            full_chars,
            short_chars,
            setting: match profile.tool_schemas() {
                crate::config::ToolSchemas::Full => "full",
                crate::config::ToolSchemas::Short => "short",
                crate::config::ToolSchemas::Auto => "auto",
            }
            .into(),
        });
    }
    let usage_key = crate::provider::probe::tool_probe_key(&provider, None);
    if let Some(message) = crate::doctor::load_usage_mismatch(
        &paths.data_dir.join(crate::doctor::USAGE_MISMATCH_CACHE),
        &usage_key,
    ) {
        manifest.note_usage_mismatch(message);
    }
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

/// `lh mcp`: connect to configured servers without starting a model turn.
async fn run_mcp(config: &Config, paths: &Paths, action: &McpCommand) -> Result<i32> {
    match action {
        McpCommand::List { json } => {
            let probes = crate::mcp::McpManager::probe_all(config, &paths.cwd).await;
            let disabled: Vec<&String> = config
                .mcp
                .iter()
                .filter(|(_, server)| !server.enabled)
                .map(|(name, _)| name)
                .collect();
            let failed = probes.iter().filter(|(_, _, r)| r.is_err()).count();
            if *json {
                let servers: Vec<serde_json::Value> = probes
                    .iter()
                    .map(|(name, server, result)| {
                        let transport = if server.url.trim().is_empty() {
                            "stdio"
                        } else {
                            "http"
                        };
                        match result {
                            Ok((info, tools)) => json!({
                                "name": name,
                                "transport": transport,
                                "ok": true,
                                "server": info.name,
                                "version": info.version,
                                "protocol": info.protocol,
                                "tools": tools.iter().map(|tool| json!({
                                    "name": tool.name,
                                    "exposed_as": crate::mcp::exposed_name(name, &tool.name),
                                    "description": tool.description,
                                    "read_only": tool.read_only,
                                })).collect::<Vec<_>>(),
                            }),
                            Err(error) => json!({
                                "name": name,
                                "transport": transport,
                                "ok": false,
                                "error": error.to_string(),
                            }),
                        }
                    })
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "servers": servers,
                        "disabled": disabled,
                    }))?
                );
            } else if config.mcp.is_empty() {
                println!(
                    "no MCP servers configured; add an [mcp.<name>] table to {}",
                    paths.config_file().display()
                );
            } else {
                for (name, server, result) in &probes {
                    let transport = if server.url.trim().is_empty() {
                        "stdio"
                    } else {
                        "http"
                    };
                    match result {
                        Ok((info, tools)) => {
                            println!(
                                "{name} ({transport}): ok · {} {} · protocol {} · {} tool{}",
                                info.name,
                                info.version,
                                info.protocol,
                                tools.len(),
                                if tools.len() == 1 { "" } else { "s" }
                            );
                            for tool in tools {
                                let hidden = (!server.tools.is_empty()
                                    && !server.tools.contains(&tool.name))
                                    || server.exclude_tools.contains(&tool.name);
                                let first_line =
                                    tool.description.lines().next().unwrap_or("").trim();
                                println!(
                                    "  {}{}{}",
                                    crate::mcp::exposed_name(name, &tool.name),
                                    if hidden { " (filtered out)" } else { "" },
                                    if first_line.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" — {first_line}")
                                    }
                                );
                            }
                        }
                        Err(error) => println!("{name} ({transport}): FAILED · {error}"),
                    }
                }
                for name in disabled {
                    println!("{name}: disabled");
                }
            }
            Ok(if failed > 0 { 1 } else { 0 })
        }
        McpCommand::Call { server, tool, args } => {
            let Some(server_config) = config.mcp.get(server) else {
                bail!(
                    "no [mcp.{server}] in the config; configured servers: {}",
                    config.mcp.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            };
            if !server_config.enabled {
                bail!(
                    "MCP server `{server}` is disabled (enabled = false in [mcp.{server}]); set enabled = true to call it"
                );
            }
            let args: serde_json::Value =
                serde_json::from_str(args).context("arguments must be a JSON object")?;
            if !args.is_object() {
                bail!("arguments must be a JSON object, for example '{{\"query\": \"x\"}}'");
            }
            let client = crate::mcp::McpClient::new(server, server_config.clone(), &paths.cwd);
            let cancel = CancellationToken::new();
            let outcome = client.call_tool(tool, args, &cancel).await;
            client.close().await;
            match outcome {
                Ok(result) => {
                    let converted = crate::mcp::convert_result(server, tool, &result);
                    println!("{}", converted.content);
                    Ok(if converted.is_error { 1 } else { 0 })
                }
                Err(error) => {
                    eprintln!("MCP call failed: {error}");
                    Ok(1)
                }
            }
        }
    }
}

/// `lh skill`: the package manager, linter, and trigger eval.
fn run_skill_command(paths: &Paths, trust: bool, action: &SkillCommand) -> Result<i32> {
    use crate::skills::{eval, lint, manager};

    match action {
        SkillCommand::Add { source, force } => {
            let report = manager::add(paths, source, *force).map_err(anyhow::Error::msg)?;
            for warning in &report.warnings {
                eprintln!("warning: {warning}");
            }
            println!(
                "{} skill `{}` at {} ({} file{}, digest {})",
                match report.action {
                    manager::Action::Installed => "installed",
                    manager::Action::Updated => "updated",
                    manager::Action::UpToDate => "unchanged",
                    manager::Action::FrozenVerified => "verified",
                },
                report.name,
                report.path.display(),
                report.files.len(),
                if report.files.len() == 1 { "" } else { "s" },
                &report.digest[..12.min(report.digest.len())]
            );
            if let Some(commit) = &report.resolved_commit {
                println!("commit: {commit}");
            }
            if let Some(trash) = &report.trash {
                println!("previous copy moved to: {}", trash.display());
            }
            println!("lockfile: {}", paths.skills_lock_file().display());
            Ok(0)
        }
        SkillCommand::Update {
            name,
            force,
            frozen,
        } => {
            let outcome = manager::update(paths, name.as_deref(), *force, *frozen)
                .map_err(anyhow::Error::msg)?;
            if outcome.frozen {
                let lock = manager::LockFile::load(&paths.skills_lock_file())
                    .map_err(anyhow::Error::msg)?;
                println!(
                    "skills.lock agrees with {} installed skill{}",
                    lock.skills.len(),
                    if lock.skills.len() == 1 { "" } else { "s" }
                );
                return Ok(0);
            }
            for report in &outcome.reports {
                for warning in &report.warnings {
                    eprintln!("warning: {warning}");
                }
                println!(
                    "{} `{}` ({})",
                    match report.action {
                        manager::Action::Installed => "installed",
                        manager::Action::Updated => "updated",
                        manager::Action::UpToDate => "up to date",
                        manager::Action::FrozenVerified => "verified",
                    },
                    report.name,
                    if let Some(commit) = &report.resolved_commit {
                        commit[..12.min(commit.len())].to_string()
                    } else {
                        report.source.clone()
                    }
                );
                if let Some(trash) = &report.trash {
                    println!("  previous copy moved to: {}", trash.display());
                }
            }
            Ok(0)
        }
        SkillCommand::Remove { name } => {
            let report = manager::remove(paths, name).map_err(anyhow::Error::msg)?;
            println!("removed skill `{}`", report.name);
            if let Some(trash) = &report.trash {
                println!("moved to: {}", trash.display());
            }
            if report.lock_removed {
                println!("lockfile: entry removed");
            }
            Ok(0)
        }
        SkillCommand::List { json, frozen } => {
            let rows = manager::list(paths).map_err(anyhow::Error::msg)?;
            let mut drift = Vec::new();
            if *frozen {
                let lock = manager::LockFile::load(&paths.skills_lock_file())
                    .map_err(anyhow::Error::msg)?;
                drift = manager::verify_lock(paths, &lock, None);
            }
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "lockfile": paths.skills_lock_file().display().to_string(),
                        "frozen": frozen,
                        "drift": drift.iter().map(|drift| json!({
                            "skill": drift.name,
                            "file": drift.file,
                            "reason": drift.reason,
                        })).collect::<Vec<_>>(),
                        "skills": rows.iter().map(|row| json!({
                            "name": row.name,
                            "source": row.source,
                            "source_kind": row.source_kind,
                            "ref": row.requested_ref,
                            "requested_ref": row.requested_ref,
                            "pinned_ref": row.resolved_commit,
                            "resolved_commit": row.resolved_commit,
                            "digest": row.digest,
                            "installed_at": row.installed_at,
                            "path": row.path.display().to_string(),
                            "installed": row.installed,
                            "modified": row.modified,
                            "files": row.files,
                        })).collect::<Vec<_>>(),
                    }))?
                );
            } else if rows.is_empty() {
                println!(
                    "no skills installed; `lh skill add <dir|git-url#ref>` installs one into {}",
                    paths.skills_dir().display()
                );
            } else {
                for row in &rows {
                    let mut line = row.name.to_string();
                    if let Some(source) = &row.source {
                        line.push_str(&format!(" · {source}"));
                    } else {
                        line.push_str(" · installed by hand");
                    }
                    if let Some(reference) = &row.requested_ref {
                        line.push_str(&format!(" · ref {reference}"));
                    }
                    if let Some(commit) = &row.resolved_commit {
                        line.push_str(&format!(" · {}", &commit[..12.min(commit.len())]));
                    }
                    if let Some(digest) = &row.digest {
                        line.push_str(&format!(" · sha256 {}", &digest[..12.min(digest.len())]));
                    }
                    if row.modified {
                        line.push_str(" · MODIFIED");
                    }
                    if !row.installed {
                        line.push_str(" · MISSING");
                    }
                    println!("{line}");
                }
            }
            if !drift.is_empty() {
                eprintln!("{}", manager::format_drift(&drift));
                return Ok(1);
            }
            Ok(0)
        }
        SkillCommand::Lint { name, all, json } => {
            let target = match name {
                Some(name) => lint::Target::Name(name.clone()),
                None if *all => lint::Target::All,
                None => lint::Target::Installed,
            };
            let report = lint::run(paths, trust, &target).map_err(anyhow::Error::msg)?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "clean": report.is_clean(),
                        "errors": report.errors(),
                        "warnings": report.warnings(),
                        "skills": report.skills.iter().map(|skill| json!({
                            "name": skill.name,
                            "path": skill.path.as_ref().map(|path| path.display().to_string()),
                            "origin": skill.origin.label(),
                            "errors": skill.errors(),
                            "warnings": skill.warnings(),
                            "diagnostics": skill.diagnostics.iter().map(|diagnostic| diagnostic.to_json()).collect::<Vec<_>>(),
                        })).collect::<Vec<_>>(),
                        "duplicates": report.duplicates.iter().map(|diagnostic| diagnostic.to_json()).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                for skill in &report.skills {
                    if skill.diagnostics.is_empty() {
                        println!("{} ({}): clean", skill.name, skill.origin.label());
                        continue;
                    }
                    println!(
                        "{} ({}): {} error{}, {} warning{}",
                        skill.name,
                        skill.origin.label(),
                        skill.errors(),
                        if skill.errors() == 1 { "" } else { "s" },
                        skill.warnings(),
                        if skill.warnings() == 1 { "" } else { "s" },
                    );
                    for diagnostic in &skill.diagnostics {
                        println!(
                            "  {} [{}] {}: {}",
                            diagnostic.severity,
                            diagnostic.field,
                            diagnostic.code,
                            diagnostic.message
                        );
                    }
                }
                if !report.duplicates.is_empty() {
                    println!("\nduplicate names:");
                    for diagnostic in &report.duplicates {
                        println!(
                            "  {} [{}] {}: {}",
                            diagnostic.severity,
                            diagnostic.field,
                            diagnostic.code,
                            diagnostic.message
                        );
                    }
                }
                println!(
                    "\n{} skill{}, {} error{}, {} warning{}",
                    report.skills.len(),
                    if report.skills.len() == 1 { "" } else { "s" },
                    report.errors(),
                    if report.errors() == 1 { "" } else { "s" },
                    report.warnings(),
                    if report.warnings() == 1 { "" } else { "s" },
                );
            }
            Ok(if report.is_clean() { 0 } else { 1 })
        }
        SkillCommand::Eval { fixtures, json } => {
            let report = eval::run(paths, trust, fixtures).map_err(anyhow::Error::msg)?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&report.to_json())?);
            } else {
                println!(
                    "skill eval ({} provider): {} fixture{}, {} failure{}",
                    report.provider,
                    report.fixtures.len(),
                    if report.fixtures.len() == 1 { "" } else { "s" },
                    report.failures,
                    if report.failures == 1 { "" } else { "s" },
                );
                for fixture in &report.fixtures {
                    println!(
                        "  {} {}: {}",
                        if fixture.passed { "PASS" } else { "FAIL" },
                        fixture.name,
                        fixture.failure.as_deref().unwrap_or("expected outcome")
                    );
                }
                for metrics in &report.skills {
                    println!(
                        "  {}: precision {}, recall {} (tp {} fp {} fn {} tn {})",
                        metrics.skill,
                        metrics
                            .precision
                            .map(|value| format!("{value:.2}"))
                            .unwrap_or_else(|| "n/a".into()),
                        metrics
                            .recall
                            .map(|value| format!("{value:.2}"))
                            .unwrap_or_else(|| "n/a".into()),
                        metrics.true_positives,
                        metrics.false_positives,
                        metrics.false_negatives,
                        metrics.true_negatives,
                    );
                }
                println!(
                    "average loaded tokens: {:.0}; total loaded tokens: {}; false-load cost: {}",
                    report.average_loaded_tokens,
                    report.total_loaded_tokens,
                    report.false_load_cost
                );
            }
            Ok(if report.passed { 0 } else { 1 })
        }
    }
}

/// `lh hooks`: list or exercise configured lifecycle hooks without a turn.
async fn run_hooks(config: &Config, paths: &Paths, action: &HooksCommand) -> Result<i32> {
    match action {
        HooksCommand::List { json } => {
            let hooks: Vec<serde_json::Value> = config
                .hooks
                .entries
                .iter()
                .map(|(name, hook)| {
                    json!({
                        "name": name,
                        "event": hook.event.as_str(),
                        "command": hook.command,
                        "on_error": hook.on_error().label(),
                        "timeout_secs": config.hooks.timeout(hook).as_secs(),
                    })
                })
                .collect();
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"hooks": hooks}))?
                );
            } else if hooks.is_empty() {
                println!(
                    "no hooks configured; add a [hooks.<name>] table to {} (see docs/hooks.md)",
                    paths.config_file().display()
                );
            } else {
                for row in &hooks {
                    println!(
                        "{}: {} · {} · on_error={} · {}s",
                        row["name"].as_str().unwrap_or("?"),
                        row["event"].as_str().unwrap_or("?"),
                        row["command"]
                            .as_array()
                            .map(|parts| parts
                                .iter()
                                .filter_map(|part| part.as_str())
                                .collect::<Vec<_>>()
                                .join(" "))
                            .unwrap_or_default(),
                        row["on_error"].as_str().unwrap_or("?"),
                        row["timeout_secs"].as_u64().unwrap_or(0),
                    );
                }
            }
            Ok(0)
        }
        HooksCommand::Test { name, json } => {
            if !config.hooks.entries.contains_key(name) {
                bail!(
                    "no hook named {name:?} in {}; run `lh hooks list` to see configured hooks and docs/hooks.md for the format",
                    paths.config_file().display()
                );
            }
            let env = crate::hooks::HookEnv {
                config: &config.hooks,
                cwd: &paths.cwd,
                session: None,
                artifacts: None,
                identity: crate::hooks::HookIdentity {
                    session_id: "hooks-test".into(),
                    model: config.model.model.clone(),
                    profile: config.profile_name().to_string(),
                },
            };
            let (invocation, outcome) = crate::hooks::test_named(&env, name)
                .await
                .map_err(anyhow::Error::msg)?;
            if *json {
                let mut value = json!({
                    "hook": invocation.hook,
                    "event": invocation.event.as_str(),
                    "outcome": invocation.outcome.label(),
                    "duration_ms": invocation.duration_ms,
                    "exit_code": invocation.exit_code,
                    "reason": invocation.reason,
                    "artifact": invocation.artifact,
                    "warnings": outcome.warnings.clone(),
                    "contexts": outcome.contexts.iter().map(|context| context.text.clone()).collect::<Vec<_>>(),
                });
                if let Some(block) = &outcome.blocked {
                    value["blocked"] = json!({"hook": block.hook, "reason": block.reason});
                }
                println!("{}", serde_json::to_string_pretty(&value)?);
            } else {
                println!(
                    "hook {} ({}): {} in {} ms",
                    invocation.hook,
                    invocation.event.as_str(),
                    invocation.outcome.label(),
                    invocation.duration_ms
                );
                if let Some(block) = &outcome.blocked {
                    println!("blocked: {}", block.reason);
                }
                for warning in &outcome.warnings {
                    println!("warning: {warning}");
                }
                for context in &outcome.contexts {
                    println!("context: {} chars", context.text.chars().count());
                }
                if let Some(artifact) = &invocation.artifact {
                    println!("output artifact: {artifact}");
                }
            }
            let failed = !matches!(invocation.outcome, crate::hooks::InvocationOutcome::Ok);
            Ok(if failed || outcome.is_blocked() { 1 } else { 0 })
        }
    }
}

/// `lh workflow`: list playbooks or run one through the deterministic engine.
async fn run_workflow_command(
    config: &Config,
    paths: &Paths,
    action: &WorkflowCommand,
    path_jail: bool,
    engagement: Option<String>,
) -> Result<i32> {
    match action {
        WorkflowCommand::List { json } => {
            let names = crate::workflows::available_names(paths, config.trust);
            let mut rows = Vec::new();
            for name in &names {
                match crate::workflows::resolve(paths, config.trust, name) {
                    Ok(spec) => rows.push(json!({
                        "name": name,
                        "description": spec.description,
                        "steps": spec.steps.len(),
                        "artifacts": spec.artifacts.len(),
                        "bundled": crate::workflows::get(name).is_some(),
                    })),
                    Err(error) => rows.push(json!({"name": name, "error": error})),
                }
            }
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"workflows": rows}))?
                );
            } else if rows.is_empty() {
                println!(
                    "no workflows; add one to {}",
                    paths.workflows_dir().display()
                );
            } else {
                for row in &rows {
                    let name = row["name"].as_str().unwrap_or("?");
                    if let Some(error) = row["error"].as_str() {
                        println!("{name}: INVALID · {error}");
                        continue;
                    }
                    let description = row["description"].as_str().unwrap_or("");
                    let steps = row["steps"].as_u64().unwrap_or(0);
                    let artifacts = row["artifacts"].as_u64().unwrap_or(0);
                    println!(
                        "{name}{} · {}{}{}",
                        if row["bundled"].as_bool().unwrap_or(false) {
                            " (bundled)"
                        } else {
                            ""
                        },
                        if steps == 0 {
                            "prompt playbook".to_string()
                        } else {
                            format!("{steps} step{}", if steps == 1 { "" } else { "s" })
                        },
                        if artifacts > 0 {
                            format!(
                                " · {artifacts} artifact{}",
                                if artifacts == 1 { "" } else { "s" }
                            )
                        } else {
                            String::new()
                        },
                        if description.is_empty() {
                            String::new()
                        } else {
                            format!(" · {description}")
                        }
                    );
                }
            }
            Ok(0)
        }
        WorkflowCommand::Run { name, resume, json } => {
            let mut spec =
                crate::workflows::resolve(paths, config.trust, name).map_err(anyhow::Error::msg)?;
            if spec.steps.is_empty() {
                // A prompt-only playbook is one prompt step. This keeps the
                // bundled workflows and `lh workflow run coding` honest.
                spec.steps.push(crate::workflows::engine::StepSpec {
                    id: "workflow".into(),
                    run: crate::workflows::engine::StepRun::Prompt,
                    prompt: spec.prompt.clone(),
                    message: None,
                    when: None,
                    require: crate::workflows::engine::Requirement::None,
                    retry: 0,
                    artifacts: Vec::new(),
                });
                spec.prompt.clear();
            }
            let mut run_config = config.clone();
            spec.apply_to_config(&mut run_config)
                .map_err(anyhow::Error::msg)?;
            let skills = crate::skills::SkillIndex::load(paths, run_config.trust);
            let preamble = spec.skill_preamble(&skills).map_err(anyhow::Error::msg)?;
            if !preamble.is_empty()
                && let Some(step) = spec.steps.iter_mut().find(|step| !step.is_approval())
            {
                step.prompt = format!("{preamble}{}", step.prompt);
            }

            let journal_path = paths
                .project_sessions_dir()
                .join("workflows")
                .join(format!("{}.json", crate::text::slug(name, '-')));
            let resume_journal = if *resume {
                if !journal_path.is_file() {
                    bail!(
                        "no saved run for {name:?} at {}; run it without --resume to start one",
                        journal_path.display()
                    );
                }
                Some(
                    crate::workflows::engine::Journal::load(&journal_path)
                        .map_err(|error| anyhow::anyhow!("cannot resume {name:?}: {error}"))?,
                )
            } else {
                None
            };
            let recorded_session_missing = resume_journal
                .as_ref()
                .and_then(|journal| journal.session_file.as_deref())
                .is_some_and(|path| !std::path::Path::new(path).is_file());
            if recorded_session_missing {
                eprintln!(
                    "[workflow] the recorded session file is gone; continuing with a fresh session"
                );
            }
            let resume_session = resume_journal
                .as_ref()
                .and_then(|journal| journal.session_file.as_ref())
                .map(PathBuf::from)
                .filter(|path| path.is_file());
            if run_config.session.no_session && *resume {
                bail!("--no-session cannot be combined with --resume");
            }
            let redact = run_config.session.redact;
            let persist = !run_config.session.no_session;
            let options = RuntimeOptions {
                path_jail,
                engagement,
                no_session: run_config.session.no_session,
                no_redact: !run_config.session.redact,
                resume: resume_session,
                continue_last: false,
                approver: None,
            };
            let runtime = create_runtime(run_config, paths.clone(), options).await?;
            // A resumed run only needs a model for a step that will actually
            // execute: a completed or skipped unchanged step does not.
            let needs_model = spec.steps.iter().any(|step| {
                if step.is_approval() {
                    return false;
                }
                if !*resume {
                    return true;
                }
                let Some(record) = resume_journal
                    .as_ref()
                    .and_then(|journal| journal.step(&step.id))
                else {
                    return true;
                };
                let unchanged =
                    record.fingerprint == crate::workflows::engine::step_fingerprint(step);
                !(unchanged
                    && matches!(
                        record.status,
                        crate::workflows::engine::StepState::Completed
                            | crate::workflows::engine::StepState::Skipped
                    ))
            });
            if needs_model && !runtime.core.logged_in() {
                bail!(
                    "no model is selected. Start the inference server and pass --model (or set LH_MODEL), or run `/login` in the TUI."
                );
            }
            let (session_id, session_file) = match runtime.core.session.lock() {
                Ok(session) => (Some(session.id.clone()), session.path.clone()),
                Err(poison) => {
                    let session = poison.into_inner();
                    (Some(session.id.clone()), session.path.clone())
                }
            };
            let cancel = CancellationToken::new();
            let signal = cancel.clone();
            tokio::spawn(async move {
                let _ = tokio::signal::ctrl_c().await;
                signal.cancel();
            });
            let mut executor = crate::workflows::runner::AgentExecutor::new(
                runtime.core.clone(),
                spec.name.clone(),
                cancel,
                *json,
            );
            let run_options = crate::workflows::engine::RunOptions {
                journal_path: journal_path.clone(),
                cwd: paths.cwd.clone(),
                resume: *resume,
                session_id,
                session_file,
                persist,
                redact,
            };
            let mut on_event = |event: &crate::workflows::engine::Event| {
                print_workflow_event(event, *json);
            };
            let report =
                crate::workflows::engine::execute(&spec, run_options, &mut executor, &mut on_event)
                    .await;
            runtime.core.mcp.shutdown().await;
            match &report.failure {
                None => Ok(0),
                Some(failure) if failure.code == "workflow_cancelled" => Ok(130),
                Some(_) => Ok(1),
            }
        }
    }
}

fn print_workflow_event(event: &crate::workflows::engine::Event, json: bool) {
    use crate::workflows::engine::Event;
    if json {
        if let Ok(line) = serde_json::to_string(event) {
            println!("{line}");
        }
        return;
    }
    match event {
        Event::WorkflowStarted {
            workflow,
            steps,
            resumed,
        } => println!(
            "\n[workflow] {workflow} · {steps} step{}{}",
            if *steps == 1 { "" } else { "s" },
            if *resumed { " · resuming" } else { "" }
        ),
        Event::StepStarted { step, attempt } => {
            eprintln!("[workflow] step {step} (attempt {attempt})")
        }
        Event::StepSucceeded { step, attempts } => eprintln!(
            "[workflow] step {step} complete after {attempts} attempt{}",
            if *attempts == 1 { "" } else { "s" }
        ),
        Event::StepSkipped { step, reason } => {
            eprintln!("[workflow] step {step} skipped: {reason}")
        }
        Event::StepRetry {
            step,
            attempt,
            reason,
        } => eprintln!(
            "[workflow] step {step} attempt {attempt} did not satisfy the step: {reason}; retrying"
        ),
        Event::ApprovalRequested { step, .. } => {
            eprintln!("[workflow] approval gate: {step}")
        }
        Event::ApprovalRecorded { step, decision } => {
            eprintln!("[workflow] approval {step}: {decision:?}")
        }
        Event::ArtifactChecked { artifact, ok } => eprintln!(
            "[workflow] artifact {artifact}: {}",
            if *ok { "ok" } else { "missing or empty" }
        ),
        Event::WorkflowFinished { status } => eprintln!("[workflow] finished: {status:?}"),
        Event::Failure {
            code,
            step,
            message,
        } => match step {
            Some(step) => eprintln!("\n[workflow] {code} ({step}): {message}"),
            None => eprintln!("\n[workflow] {code}: {message}"),
        },
    }
}

/// `lh --rewind <session>:<entry>`: restore harness-written files and move the
/// conversation leaf, without starting a model turn.
fn run_rewind(paths: &Paths, spec: &str, force: bool) -> Result<i32> {
    let (session_part, entry) = spec.rsplit_once(':').ok_or_else(|| {
        anyhow::anyhow!("--rewind expects <session>:<entry>, for example `lh --rewind last:e12`")
    })?;
    let entry = entry.trim();
    if entry.is_empty() {
        bail!("--rewind needs an entry id after the colon; run /tree in the TUI to see ids");
    }
    let session_part = session_part.trim();
    let session_path = match session_part {
        "" | "last" => Session::latest_path(paths)?.ok_or_else(|| {
            anyhow::anyhow!(
                "no sessions in {}; pass <session>:<entry> from another project",
                paths.project_sessions_dir().display()
            )
        })?,
        value => {
            let direct = PathBuf::from(value);
            if direct.is_file() {
                direct
            } else {
                let candidate = paths.project_sessions_dir().join(format!("{value}.jsonl"));
                if candidate.is_file() {
                    candidate
                } else {
                    bail!(
                        "no session {value:?} in {}; use `lh --rewind last:{entry}` for the latest",
                        paths.project_sessions_dir().display()
                    );
                }
            }
        }
    };
    let mut session = Session::open(&session_path)
        .with_context(|| format!("cannot open {}", session_path.display()))?;
    let plan = crate::session::rewind::plan(&session, entry)?;
    let report = crate::session::rewind::apply(&mut session, &plan, force)?;
    session.fsync()?;
    println!(
        "rewound session {} to {} (seq {}): {}",
        session.id,
        report.target,
        plan.target_seq,
        report.summary()
    );
    Ok(0)
}

async fn run_headless(runtime: crate::app::Runtime, prompt: String, json: bool) -> Result<i32> {
    let cancel = CancellationToken::new();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let core = runtime.core.clone();
    let cancel_turn = cancel.clone();
    let handle = tokio::spawn(async move { run_turn(core, prompt, tx, cancel_turn).await });

    let mut first_signal = false;
    let mut text = HeadlessText::new();
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                if first_signal {
                    break;
                }
                first_signal = true;
                cancel.cancel();
                if json {
                    println!("{}", json!({"type": crate::events::CANCELLED}));
                } else {
                    eprintln!("cancelling...");
                }
            }
            event = rx.recv() => {
                match event {
                    Some(event) => {
                        if render_event(event, json, &mut text)?.is_some() {
                            break;
                        }
                    }
                    None => break,
                }
            }
        }
    }
    text.flush()?;
    let outcome = handle.await.context("agent task panicked")?;
    let status = outcome.status;
    if runtime.core.permissions.mode() == crate::policy::PermissionMode::Plan
        && status == TurnStatus::Done
    {
        let plan = runtime
            .session
            .lock()
            .ok()
            .and_then(|session| crate::tools::todo::latest_plan(&session));
        if let Some((_, tasks)) = plan {
            if json {
                println!("{}", json!({"type":"plan","tasks":tasks}));
            } else {
                println!("\nPlan (plan mode; nothing was changed):");
                for (index, task) in tasks.iter().enumerate() {
                    println!("{}. {task}", index + 1);
                }
            }
        }
    }
    let _ = std::io::stdout().flush();
    Ok(match status {
        TurnStatus::Done => 0,
        TurnStatus::Cancelled => 130,
        TurnStatus::Budget | TurnStatus::Stalled | TurnStatus::ContextTruncated => 2,
        TurnStatus::Error | TurnStatus::Blocked => 1,
    })
}

/// Plain `lh -p` answer text. On a terminal it streams. Piped, it is held
/// until the model call finishes, so a reply that fails partway and is
/// retried does not leave its partial text in front of the real answer.
struct HeadlessText {
    stream: bool,
    pending: String,
}

impl HeadlessText {
    fn new() -> Self {
        use std::io::IsTerminal;
        Self {
            stream: std::io::stdout().is_terminal(),
            pending: String::new(),
        }
    }

    fn push(&mut self, text: &str) -> Result<()> {
        if self.stream {
            print!("{text}");
            std::io::stdout().flush()?;
        } else {
            self.pending.push_str(text);
        }
        Ok(())
    }

    fn discard(&mut self, chars: usize) {
        if self.stream {
            eprintln!("\n[retry] the {chars} characters above were a failed partial reply");
        } else {
            let keep = self.pending.chars().count().saturating_sub(chars);
            self.pending = self.pending.chars().take(keep).collect();
        }
    }

    fn flush(&mut self) -> Result<()> {
        if !self.pending.is_empty() {
            print!("{}", std::mem::take(&mut self.pending));
            std::io::stdout().flush()?;
        }
        Ok(())
    }
}

/// Render one event in plain or JSON mode. Returns the final status when done.
fn render_event(
    event: AgentEvent,
    json: bool,
    text: &mut HeadlessText,
) -> Result<Option<TurnStatus>> {
    if json {
        let value = crate::events::event_json(&event);
        println!("{}", serde_json::to_string(&value)?);
        if let AgentEvent::TurnDone(outcome) = event {
            return Ok(Some(outcome.status));
        }
        return Ok(None);
    }

    match &event {
        AgentEvent::TextDelta(_) | AgentEvent::ReasoningDelta(_) | AgentEvent::Discard { .. } => {}
        // Any other event ends the model call's text.
        _ => text.flush()?,
    }
    match event {
        AgentEvent::TextDelta(delta) => text.push(&delta)?,
        AgentEvent::Discard { chars } => text.discard(chars),
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
        AgentEvent::Provenance {
            origin, warning, ..
        } => {
            if let Some(warning) = warning {
                eprintln!("[provenance] {origin}: {warning}");
            } else {
                eprintln!("[provenance] {origin}");
            }
        }
        AgentEvent::Guard { code, message } => eprintln!("[{code}] {message}"),
        AgentEvent::Error { code, message, .. } => eprintln!("[{code}] {message}"),
        AgentEvent::Usage(usage) => {
            let cache = match (usage.cached_tokens, usage.cache_percent()) {
                (Some(cached), Some(percent)) => {
                    format!(" cache {cached}/{} ({percent}%)", usage.prompt_tokens)
                }
                _ => String::new(),
            };
            eprintln!(
                "[usage] prompt {} completion {}{cache}",
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

#[cfg(test)]
mod tests {
    #[test]
    fn piped_answers_drop_a_discarded_partial_reply() {
        let mut text = super::HeadlessText {
            stream: false,
            pending: String::new(),
        };
        text.push("half an ans").unwrap();
        text.discard("half an ans".len());
        text.push("the whole answer").unwrap();
        assert_eq!(text.pending, "the whole answer");
    }
    use super::*;

    #[test]
    fn provenance_event_json_exposes_the_tag_and_warning() {
        let event = AgentEvent::Provenance {
            call_id: "call_0001".into(),
            origin: "tool:read".into(),
            warning: Some("[provenance] untrusted content".into()),
            approval_required: true,
        };
        let value = crate::events::event_json(&event);
        assert_eq!(value["type"], "provenance");
        assert_eq!(value["call_id"], "call_0001");
        assert_eq!(value["origin"], "tool:read");
        assert_eq!(value["warning"], "[provenance] untrusted content");
        assert_eq!(value["approval_required"], true);
    }

    #[test]
    fn no_redact_is_an_explicit_global_flag() {
        let cli = Cli::try_parse_from(["lh", "--no-redact"]).unwrap();
        assert!(cli.no_redact);
        assert!(!Cli::try_parse_from(["lh"]).unwrap().no_redact);
    }

    #[test]
    fn doctor_prefers_the_configured_engine_over_detection() {
        use crate::provider::Engine;
        assert_eq!(diagnostic_engine("vllm", "generic"), Engine::Vllm);
        assert_eq!(diagnostic_engine("", "llama.cpp"), Engine::LlamaCpp);
        assert_eq!(diagnostic_engine("auto", "ollama"), Engine::Ollama);
        assert_eq!(diagnostic_engine("", "unknown"), Engine::Generic);
    }

    #[test]
    fn rewind_takes_a_session_and_entry_and_force_needs_it() {
        let cli = Cli::try_parse_from(["lh", "--rewind", "last:e12"]).unwrap();
        assert_eq!(cli.rewind.as_deref(), Some("last:e12"));
        assert!(!cli.force);
        let cli = Cli::try_parse_from(["lh", "--rewind", "abc:e3", "--force"]).unwrap();
        assert!(cli.force);
        assert!(
            Cli::try_parse_from(["lh", "--force"]).is_err(),
            "--force only means something with --rewind"
        );
        assert!(
            Cli::try_parse_from(["lh", "--rewind", "last:e3", "--print", "hi"]).is_err(),
            "--rewind and --print are separate modes"
        );
    }

    #[test]
    fn run_rewind_restores_files_and_reports_the_target() {
        use crate::session::{CustomData, EntryKind, SessionData, UserData};

        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        std::fs::write(cwd.join("a.txt"), "after\n").unwrap();
        let mut paths = Paths::discover(&cwd).unwrap();
        paths.config_dir = dir.path().join("config");
        paths.data_dir = dir.path().join("data");
        paths.sessions_dir_override = Some(dir.path().join("sessions"));
        paths.ensure_dirs().unwrap();
        let mut session = Session::create(
            &paths,
            SessionData {
                cwd: cwd.display().to_string(),
                endpoint: "test".into(),
                model: "test".into(),
                profile: "test".into(),
                app: "test".into(),
                redact: true,
                context: None,
                permissions: None,
            },
            false,
        )
        .unwrap();
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "edit it".into(),
            }))
            .unwrap();
        let target = session
            .append(EntryKind::Assistant(crate::session::AssistantData {
                content: Some("done".into()),
                ..Default::default()
            }))
            .unwrap();
        let snapshot = session.artifacts.write("before\n").unwrap();
        session
            .append(EntryKind::Custom(
                CustomData::new("fs-change")
                    .with("path", "a.txt")
                    .with("snapshot", snapshot.id)
                    .with("after_sha256", crate::tools::undo::content_hash(b"after\n")),
            ))
            .unwrap();
        session.fsync().unwrap();
        drop(session);

        assert_eq!(
            run_rewind(&paths, &format!("last:{}", target.id), false).unwrap(),
            0
        );
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "before\n"
        );
        let reopened = Session::open(&Session::latest_path(&paths).unwrap().unwrap()).unwrap();
        assert_eq!(
            reopened.active_leaf_id().as_deref(),
            Some(reopened.tree.last().unwrap().id.as_str())
        );
        assert!(reopened.latest_custom_on_path("rewind").is_some());
    }
}

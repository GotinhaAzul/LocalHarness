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
    /// Serve the Agent Client Protocol v1 on stdio for editor integrations.
    Acp,
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
    if cli.no_redact {
        config.session.redact = false;
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
    let mut manifest = manifest;
    // `lh doctor` never sends a chat completion. Show the last conformance
    // result the runtime stored for this model/build, if any.
    let probe_path = paths.data_dir.join("tool-probe-cache.json");
    let probe_key = crate::provider::probe::tool_probe_key(&provider, manifest.build.as_deref());
    if let Some(cached) = crate::provider::probe::load_tool_probe(&probe_path, &probe_key) {
        manifest.tool_dialect = cached.mode.clone();
        manifest.tool_probe = Some(cached);
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
            AgentEvent::TurnStarted {
                model,
                profile,
                tool_mode,
            } => {
                json!({"type":"started","model":model,"profile":profile,"tool_mode":tool_mode})
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
                json!({"type":"usage","prompt_tokens":usage.prompt_tokens,"completion_tokens":usage.completion_tokens,"reasoning_tokens":usage.reasoning_tokens})
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_redact_is_an_explicit_global_flag() {
        let cli = Cli::try_parse_from(["lh", "--no-redact"]).unwrap();
        assert!(cli.no_redact);
        assert!(!Cli::try_parse_from(["lh"]).unwrap().no_redact);
    }
}

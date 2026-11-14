//! Interactive TUI event loop, slash commands, and terminal lifecycle.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result, anyhow, bail};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::agent::{AgentEvent, AgentState, Core, TurnOutcome, TurnStatus};
use crate::app::{self, Runtime, RuntimeOptions};
use crate::config::Config;
use crate::context;
use crate::git::{self, GitStatus};
use crate::provider::{OpenAiCompatProvider, probe};
use crate::session::{CompactionData, Entry, EntryKind, UserData};
use crate::skills::SkillIndex;
use crate::tools::todo::{PlanStatus, plan_snapshot};
use crate::ui::UiOptions;
use crate::ui::complete::{self, Completion};
use crate::ui::input::InputState;
use crate::ui::render::{self, ActivityData, Transcript};
use crate::ui::theme::{Role, Theme};
use crate::ui::tree::TreeView;
use crate::ui::widgets::{
    FooterData, HeaderData, ModalData, PickAction, PickItem, PickerModal, SessionItem,
    SessionsModal, ToolItem, ToolsModal, clip_end,
};
use crate::workflows::{self, WorkflowSpec};

/// Slash commands with one-line descriptions for the completion popup.
pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "list commands and key bindings"),
    ("quit", "exit LocalHarness"),
    ("model", "pick a model from the endpoint or gateway"),
    ("login", "configure the endpoint, model, and API key"),
    ("profile", "pick the tool profile"),
    ("theme", "pick a theme (live preview) or save it"),
    ("tree", "browse and checkout the session tree"),
    ("fork", "checkout the latest assistant step"),
    ("new", "start a fresh session"),
    ("resume", "resume a previous session"),
    ("sessions", "list and switch sessions"),
    ("compact", "append a deterministic compaction"),
    ("tools", "show the active tools"),
    ("mcp", "show MCP server status"),
    ("skill", "pick a skill and append its instructions"),
    ("workflow", "pick a workflow to start on a new branch"),
    ("sec", "engagement summary, scope, or report"),
    ("trust", "show the trust state"),
    ("export", "write the branch as Markdown"),
    ("clear", "clear the transcript view"),
    ("debug", "toggle the debug panel"),
];

/// Argument syntax per command, for `/help <command>`.
fn command_usage(name: &str) -> &'static str {
    match name {
        "help" => "/help [command]",
        "model" => "/model [name]   (no name: pick from a list)",
        "login" => "/login [endpoint] [model] [api-key|-]   (no arguments: show status)",
        "profile" => "/profile [name]   (no name: pick from a list)",
        "theme" => "/theme [name|save]   (no name: pick with live preview)",
        "resume" => "/resume [id|last]   (no id: pick from a list)",
        "skill" => "/skill [name]   (no name: pick from a list)",
        "workflow" => "/workflow [name]   (no name: pick from a list)",
        "sec" => {
            "/sec [summary|report|scope add|remove <target>|scope clear]   (no argument: pick)"
        }
        "trust" => "/trust [confirm]",
        "export" => "/export [path]",
        _ => "",
    }
}

const HELP: &str = "\
Commands: /help [command] /quit /login [endpoint] [model] [api-key] /model [name]
/profile [name] /theme [name|save] /tree /fork /new /resume [id|last] /sessions
/compact /tools /mcp
/skill [name] /workflow [name] /sec [...] /trust [confirm] /export [path]
/clear /debug
A command that chooses from a list opens a picker when run without an
argument: type to filter, arrows to move, Enter to pick.

Keys: Enter send · Shift+Enter / Ctrl+J newline · Esc interrupt or dismiss
Tab complete commands, arguments, and paths · Shift+Tab previous choice
@path in a prompt attaches that file for the model
Ctrl+R show thinking · Ctrl+O expand tool output · Ctrl+T toggle the plan
Scroll: mouse wheel · PgUp/PgDn half page · Shift+Up/Down one line
Home/End top and bottom (prompt empty) · Ctrl+Home/End anytime · Ctrl+C quit
Hold Shift while dragging to select text ([ui] mouse = false disables
mouse capture).";

/// Footer hints while idle.
const HINTS: &str = "enter send · ⇧enter newline · / commands · tab complete · @file attach · ctrl+r thinking · ctrl+o output";

/// Shortest gap between two frames while output streams (~60 fps). Input
/// events draw immediately.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Results of work the TUI runs off the event loop so it never freezes.
enum BgEvent {
    /// A `/v1/models` listing for `endpoint`.
    Models {
        endpoint: String,
        result: std::result::Result<Vec<String>, String>,
    },
    /// A core rebuilt for a model or profile switch.
    Rebuilt {
        switch: Switch,
        result: std::result::Result<Arc<Core>, String>,
    },
}

/// A config switch running in the background.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Switch {
    Model(String),
    Profile(String),
}

impl Switch {
    fn label(&self) -> String {
        match self {
            Switch::Model(name) => format!("Switching to {name}…"),
            Switch::Profile(name) => format!("Loading profile {name}…"),
        }
    }
}

/// Lines per mouse-wheel notch.
const WHEEL_LINES: usize = 3;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// What the model stream is producing right now, for the activity line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamPhase {
    Waiting,
    Thinking,
    Writing,
}

struct RunningTool {
    call_id: String,
    name: String,
    summary: String,
    started: Instant,
}

const LOGIN_HINT: &str = "\
No model is selected, so turns are disabled.
Start an OpenAI-compatible server, then:
  /login http://127.0.0.1:8080            auto-select when it exposes one model
  /login http://127.0.0.1:8080 <model>    choose a model
  /login <endpoint> <model> <api-key>     remote endpoint with a bearer token

/model lists the models the current endpoint exposes.";

static TERMINAL_ACTIVE: AtomicBool = AtomicBool::new(false);

fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

fn restore_terminal() {
    if TERMINAL_ACTIVE.swap(false, Ordering::SeqCst) {
        let _ = disable_raw_mode();
        let mut out = std::io::stdout();
        let _ = execute!(
            out,
            DisableMouseCapture,
            LeaveAlternateScreen,
            DisableBracketedPaste
        );
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter(mouse: bool) -> Result<Self> {
        install_panic_hook();
        enable_raw_mode().context("cannot enable raw mode")?;
        let mut out = std::io::stdout();
        let entered = if mouse {
            execute!(
                out,
                EnterAlternateScreen,
                EnableBracketedPaste,
                EnableMouseCapture
            )
        } else {
            execute!(out, EnterAlternateScreen, EnableBracketedPaste)
        };
        if let Err(error) = entered {
            let _ = disable_raw_mode();
            return Err(error).context("cannot enter the alternate screen");
        }
        TERMINAL_ACTIVE.store(true, Ordering::SeqCst);
        Ok(TerminalGuard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

#[derive(Debug, Deserialize, Default)]
struct WorkflowFile {
    name: Option<String>,
    description: Option<String>,
    profile: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    tools: Vec<String>,
}

pub async fn run(runtime: Runtime, options: UiOptions) -> Result<()> {
    let mut app = App::new(runtime, options)?;
    let _guard = TerminalGuard::enter(app.config.ui.mouse)?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend).context("cannot initialize the terminal")?;
    // Clear directly: `Terminal::clear` queries the cursor position, and a
    // terminal that never answers (some SSH and mobile clients, piped PTYs)
    // stalls startup for two seconds. A failed clear must not abort.
    let _ = execute!(std::io::stdout(), Clear(ClearType::All));

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(true) => match event::read() {
                    Ok(ev) => {
                        if event_tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                Ok(false) => {}
                Err(_) => break,
            }
        }
    });

    let (git_tx, mut git_rx) = mpsc::unbounded_channel::<GitStatus>();
    let git_cwd = app.cwd_path();
    tokio::spawn(git::watch(git_cwd, git_tx));

    // Warm the model list so `/model <Tab>` and the picker are instant.
    app.spawn_models_refresh();
    let mut bg_rx = app
        .bg_rx
        .take()
        .context("background channel already taken")?;

    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    // Frames are drawn on demand: immediately after input, at most every
    // FRAME_INTERVAL while output streams, and not at all while idle.
    let mut needs_draw = true;
    let mut urgent = true;
    let mut last_draw = Instant::now();
    while !app.quit {
        if needs_draw && (urgent || last_draw.elapsed() >= FRAME_INTERVAL) {
            app.draw(&mut terminal)?;
            last_draw = Instant::now();
            needs_draw = false;
            urgent = false;
        }
        let frame_due =
            needs_draw.then(|| tokio::time::Instant::from_std(last_draw + FRAME_INTERVAL));

        let mut agent_rx = app.agent_rx.take();
        tokio::select! {
            // Keys first: typing must never wait behind a burst of tokens.
            biased;
            maybe = event_rx.recv() => {
                match maybe {
                    Some(ev) => {
                        app.handle_event(ev).await;
                        // Apply everything already queued (a paste, key
                        // repeat) before drawing once.
                        while !app.quit
                            && let Ok(ev) = event_rx.try_recv()
                        {
                            app.handle_event(ev).await;
                        }
                    }
                    None => app.quit = true,
                }
                needs_draw = true;
                urgent = true;
            }
            maybe = async {
                match &mut agent_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending::<Option<AgentEvent>>().await,
                }
            } => {
                match maybe {
                    Some(event) => app.handle_agent_event(event),
                    None => app.finish_turn(None),
                }
                // Coalesce the deltas that arrived meanwhile into one frame.
                let mut drained = 0;
                while app.turn_active && drained < 4096 {
                    let Some(rx) = agent_rx.as_mut() else { break };
                    match rx.try_recv() {
                        Ok(event) => app.handle_agent_event(event),
                        Err(_) => break,
                    }
                    drained += 1;
                }
                if !app.turn_active {
                    agent_rx = None;
                }
                needs_draw = true;
            }
            Some(event) = bg_rx.recv() => {
                app.handle_bg(event).await;
                needs_draw = true;
                urgent = true;
            }
            _ = ticker.tick() => {
                if app.on_tick() {
                    needs_draw = true;
                }
            }
            _ = async {
                match frame_due {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending::<()>().await,
                }
            } => {}
            maybe = git_rx.recv() => {
                if let Some(status) = maybe {
                    app.apply_git(status);
                    needs_draw = true;
                }
            }
            _ = async {
                match sigint.as_mut() {
                    Some(signal) => { signal.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                app.quit = true;
            }
            _ = async {
                match sigterm.as_mut() {
                    Some(signal) => { signal.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                app.quit = true;
            }
        }

        if app.agent_rx.is_none() {
            app.agent_rx = agent_rx;
        }
    }

    app.shutdown(&mut terminal).await;
    let fatal = app.fatal.take();
    drop(_guard);
    restore_terminal();
    if let Some(message) = fatal {
        bail!(message);
    }
    Ok(())
}

struct App {
    runtime: Option<Runtime>,
    options: UiOptions,
    config: Config,
    theme: Theme,
    no_color: bool,
    header: HeaderData,
    transcript: Transcript,
    input: InputState,
    modal: Option<ModalData>,
    agent_rx: Option<UnboundedReceiver<AgentEvent>>,
    agent_task: Option<JoinHandle<TurnOutcome>>,
    turn_cancel: tokio_util::sync::CancellationToken,
    turn_active: bool,
    turn_state: Option<AgentState>,
    cancelling: bool,
    quit: bool,
    fatal: Option<String>,
    notice: Option<(String, Role, Instant)>,
    /// How many session warnings the transcript already shows.
    warnings_seen: usize,
    token_events: VecDeque<(Instant, u64)>,
    tok_s: f32,
    /// The last model call's usage, with server and harness timings.
    last_usage: Option<crate::provider::Usage>,
    used_tokens: u32,
    completion_tokens: u32,
    window: u32,
    debug: bool,
    floor_seq: u64,
    spinner: usize,
    last_width: usize,
    turn_started: Option<Instant>,
    stream_phase: StreamPhase,
    running_tools: Vec<RunningTool>,
    /// The agent's `todo` plan on the active branch.
    plan: Vec<(String, PlanStatus)>,
    show_plan: bool,
    /// What slash-command arguments can complete to.
    completion_sources: complete::Sources,
    /// The last completion, keyed by the text before the cursor and whether
    /// the popup was opened, so frames do not recompute (or re-list
    /// directories) while nothing changed.
    completion_cache: RefCell<Option<(String, bool, Option<Completion>)>>,
    /// Background work reports here; the event loop takes the receiver.
    bg_tx: UnboundedSender<BgEvent>,
    bg_rx: Option<UnboundedReceiver<BgEvent>>,
    /// A model or profile switch in flight.
    switching: Option<Switch>,
    /// The theme to restore when a `/theme` picker previewing others is
    /// dismissed.
    theme_before_preview: Option<String>,
}

impl App {
    fn new(runtime: Runtime, options: UiOptions) -> Result<Self> {
        let config = runtime.core.config.clone();
        let paths = runtime.core.paths.clone();
        let theme = Theme::load(
            &config.ui.theme,
            &paths.themes_dir(),
            config.ui.color_mode,
            options.no_color,
        );
        let session_id = match runtime.session.lock() {
            Ok(session) => session.id.clone(),
            Err(poison) => poison.into_inner().id.clone(),
        };
        let warnings = match runtime.session.lock() {
            Ok(session) => session.warnings.clone(),
            Err(poison) => poison.into_inner().warnings.clone(),
        };
        let warnings_seen = warnings.len();
        let used = estimate_used(&runtime.core);
        let window = runtime.core.effective_window().0;
        let trust = config.trust || options.trust;
        let header = HeaderData {
            cwd: paths.cwd.display().to_string(),
            git: GitStatus::none(),
            profile: config.profile_name().to_string(),
            model: runtime.core.provider.model().to_string(),
            tool_mode: runtime.core.tool_mode_label().unwrap_or_default(),
            logged_in: runtime.core.logged_in(),
            used,
            window,
            completion: 0,
            tok_s: 0.0,
            cache: None,
            session: short_session_id(&session_id),
            trust,
            window_guess: !runtime.core.effective_window().1.is_reliable(),
        };
        let (bg_tx, bg_rx) = mpsc::unbounded_channel();
        let mut app = App {
            runtime: Some(runtime),
            options,
            config,
            theme,
            no_color: false,
            header,
            transcript: Transcript::new(false),
            input: InputState::new(),
            modal: None,
            agent_rx: None,
            agent_task: None,
            turn_cancel: tokio_util::sync::CancellationToken::new(),
            turn_active: false,
            turn_state: None,
            cancelling: false,
            quit: false,
            fatal: None,
            notice: None,
            warnings_seen,
            token_events: VecDeque::new(),
            tok_s: 0.0,
            last_usage: None,
            used_tokens: used,
            completion_tokens: 0,
            window,
            debug: false,
            floor_seq: 0,
            spinner: 0,
            last_width: 0,
            turn_started: None,
            stream_phase: StreamPhase::Waiting,
            running_tools: Vec::new(),
            plan: Vec::new(),
            show_plan: true,
            completion_sources: complete::Sources::default(),
            completion_cache: RefCell::new(None),
            bg_tx,
            bg_rx: Some(bg_rx),
            switching: None,
            theme_before_preview: None,
        };
        app.no_color = app.options.no_color;
        app.transcript = Transcript::new(app.config.ui.show_reasoning);
        app.refresh_session_blocks();
        app.refresh_completion_sources(true);
        app.attach_prompt_history();
        for warning in warnings {
            app.transcript.push_note("session warning", warning, false);
        }
        if !app.header.trust {
            app.set_notice(
                "untrusted: project config, skills, workflows, and tools are off · restart with --trust",
                Role::Warning,
            );
        }
        if !app.header.logged_in {
            app.transcript.push_note("login", LOGIN_HINT, false);
            app.set_notice("no model selected; run /login", Role::Warning);
        }
        Ok(app)
    }

    fn core(&self) -> Option<&Arc<Core>> {
        self.runtime.as_ref().map(|runtime| &runtime.core)
    }

    /// Persist Up-arrow history per project, next to its sessions. Skipped
    /// with `--no-session`, which promises not to write session data.
    fn attach_prompt_history(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let sessions_enabled = match runtime.session.lock() {
            Ok(session) => session.path.is_some(),
            Err(poison) => poison.into_inner().path.is_some(),
        };
        if sessions_enabled {
            let path = runtime
                .core
                .paths
                .project_sessions_dir()
                .join("prompt_history.jsonl");
            self.input.attach_history_file(path);
        }
    }

    /// Re-read what slash-command arguments complete to. Session previews
    /// read every session file, so they refresh only when asked.
    fn refresh_completion_sources(&mut self, sessions: bool) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let paths = runtime.core.paths.clone();
        let trust = self.config.trust || self.options.trust;
        let sources = &mut self.completion_sources;
        sources.cwd = paths.cwd.clone();
        sources.home = dirs::home_dir();
        sources.themes = available_theme_names(&paths);
        sources.profiles = self
            .config
            .profiles
            .iter()
            .map(|(name, data)| (name.clone(), data.description.clone().unwrap_or_default()))
            .collect();
        sources.skills = skill_choices(&paths, trust, &runtime.core.tools);
        sources.workflows = available_workflows(&paths, trust)
            .into_iter()
            .map(|name| {
                let description = workflow_description(&name);
                (name, description)
            })
            .collect();
        sources.endpoint = runtime.core.config.model.endpoint.clone();
        if sessions {
            sources.sessions = scan_sessions(&paths)
                .into_iter()
                .map(|item| (item.id, item.preview))
                .collect();
        }
        self.completion_cache.replace(None);
    }

    /// List the endpoint's models off the event loop; the result lands in
    /// [`App::handle_bg`].
    fn spawn_models_refresh(&self) {
        let Some(core) = self.core() else {
            return;
        };
        let config = core.config.model.clone();
        let tx = self.bg_tx.clone();
        tokio::spawn(async move {
            let result = probe_endpoint_models(&config)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(BgEvent::Models {
                endpoint: config.endpoint,
                result,
            });
        });
    }

    async fn handle_bg(&mut self, event: BgEvent) {
        match event {
            BgEvent::Models { endpoint, result } => {
                let current = self
                    .core()
                    .map(|core| core.config.model.endpoint.clone())
                    .unwrap_or_default();
                let Ok(models) = result else {
                    return;
                };
                if endpoint != current {
                    return;
                }
                self.completion_sources.models = models.clone();
                self.completion_cache.replace(None);
                if let Some(ModalData::Picker(view)) = &mut self.modal
                    && view.command == "model"
                {
                    view.replace_items(model_items(&models));
                }
            }
            BgEvent::Rebuilt { switch, result } => {
                self.switching = None;
                let core = match result {
                    Ok(core) => core,
                    Err(error) => {
                        let what = match &switch {
                            Switch::Model(_) => "model change",
                            Switch::Profile(_) => "profile change",
                        };
                        self.set_notice(format!("{what} failed: {error}"), Role::Error);
                        return;
                    }
                };
                self.apply_core(core);
                self.refresh_completion_sources(false);
                match switch {
                    Switch::Model(name) => {
                        let window = self
                            .core()
                            .map(|core| {
                                format!(
                                    " · context {} ({})",
                                    crate::ui::widgets::format_tokens(core.effective_window().0),
                                    core.effective_window().1.label()
                                )
                            })
                            .unwrap_or_default();
                        self.set_notice(format!("model: {name}{window}"), Role::Success);
                    }
                    Switch::Profile(name) => {
                        self.set_notice(format!("profile: {name}"), Role::Success);
                    }
                }
            }
        }
    }

    /// Rebuild the core for `config` in the background so the TUI keeps
    /// drawing and taking keys while the server loads a model or probes.
    fn spawn_rebuild(&mut self, switch: Switch, config: Config) {
        let Some(old) = self.core().cloned() else {
            return;
        };
        let tx = self.bg_tx.clone();
        self.set_notice(switch.label(), Role::Muted);
        self.switching = Some(switch.clone());
        tokio::spawn(async move {
            let result = app::rebuild_core(&old, config)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(BgEvent::Rebuilt { switch, result });
        });
    }

    /// Wait for background switches to land (tests drive the app without
    /// the event loop).
    #[cfg(test)]
    async fn settle(&mut self) {
        while self.switching.is_some() {
            let Some(rx) = self.bg_rx.as_mut() else {
                break;
            };
            let Some(event) = rx.recv().await else {
                break;
            };
            self.handle_bg(event).await;
        }
    }

    /// The completion for the prompt as it stands, or `None` when there is
    /// nothing useful to show.
    fn current_completion(&self) -> Option<Completion> {
        let before = self.input.before_cursor();
        let popup = self.input.popup_open();
        if let Some((text, open, cached)) = self.completion_cache.borrow().as_ref()
            && *text == before
            && *open == popup
        {
            return cached.clone();
        }
        let completion = complete::complete(&before, COMMANDS, &self.completion_sources, popup)
            .filter(|completion| !completion.is_settled());
        self.completion_cache
            .replace(Some((before, popup, completion.clone())));
        completion
    }

    fn accept_candidate(&mut self, completion: &Completion, index: usize) {
        let Some(candidate) = completion.candidates.get(index) else {
            return;
        };
        let command = candidate.value.trim_start_matches('/');
        let insertion = completion.insertion(candidate, complete::takes_args(command));
        self.input
            .replace_before_cursor(completion.start, &insertion);
    }

    /// Tab: complete the token at the cursor. A unique match is accepted; an
    /// ambiguous one first extends to the shared prefix, then opens (or, if
    /// already showing, accepts from) the popup.
    fn tab_complete(&mut self) -> bool {
        let visible = self.current_completion().is_some();
        if !self.input.popup_open() {
            self.input.open_popup();
        }
        let Some(completion) = self.current_completion() else {
            // `/help` + Tab: a settled command that takes arguments gets its
            // separating space.
            let before = self.input.before_cursor();
            if let Some(settled) =
                complete::complete(&before, COMMANDS, &self.completion_sources, true)
                && settled.is_settled()
            {
                self.accept_candidate(&settled, 0);
                return true;
            }
            self.input.reset_completion();
            return false;
        };
        if completion.candidates.len() == 1 {
            self.accept_candidate(&completion, 0);
            return true;
        }
        if let Some(prefix) = completion.common_extension() {
            self.input.replace_before_cursor(completion.start, &prefix);
            self.input.open_popup();
            return true;
        }
        if visible {
            let index = self
                .input
                .completion_index
                .min(completion.candidates.len() - 1);
            self.accept_candidate(&completion, index);
        }
        true
    }

    fn cwd_path(&self) -> PathBuf {
        self.core()
            .map(|core| core.paths.cwd.clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn set_notice(&mut self, text: impl Into<String>, role: Role) {
        self.notice = Some((text.into(), role, Instant::now()));
    }

    /// Surface session warnings appended after launch (for example, a failed
    /// background `model.start_command`).
    fn sync_session_warnings(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let warnings = match runtime.session.lock() {
            Ok(session) => session.warnings.clone(),
            Err(poison) => poison.into_inner().warnings.clone(),
        };
        if warnings.len() <= self.warnings_seen {
            return;
        }
        for warning in &warnings[self.warnings_seen..] {
            self.transcript
                .push_note("session warning", warning.clone(), false);
            self.set_notice(warning.clone(), Role::Warning);
        }
        self.warnings_seen = warnings.len();
    }

    fn refresh_session_blocks(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let blocks = {
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let _ = session.fsync();
            self.plan = plan_snapshot(&session);
            render::blocks_from_session_after(&session, self.floor_seq)
        };
        self.transcript.set_session(blocks);
        self.used_tokens = estimate_used(&runtime.core);
        self.header.used = self.used_tokens;
        self.header.window = self.window;
        self.header.completion = self.completion_tokens;
        self.sync_banner();
    }

    /// Refresh the welcome card and the context-window confidence marker.
    fn sync_banner(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let core = &runtime.core;
        let (window_size, source) = core.effective_window();
        self.header.window_guess = !source.is_reliable();
        let model = if core.logged_in() {
            (core.provider.model().to_string(), false)
        } else {
            ("not logged in · run /login".to_string(), true)
        };
        let window = format!(
            "{} · {}",
            crate::ui::widgets::format_tokens(window_size),
            source.label()
        );
        let session = match runtime.session.lock() {
            Ok(session) => session.id.clone(),
            Err(poison) => poison.into_inner().id.clone(),
        };
        let rows = vec![
            ("model".to_string(), model.0, model.1),
            ("context".to_string(), window, !source.is_reliable()),
            (
                "cwd".to_string(),
                crate::ui::widgets::display_cwd(&core.paths.cwd.display().to_string()),
                false,
            ),
            ("session".to_string(), session, false),
        ];
        self.transcript.set_banner(Some(render::Block {
            kind: render::BlockKind::Banner {
                title: format!("LocalHarness {}", env!("CARGO_PKG_VERSION")),
                rows,
                hint: "/help commands · /model switch model · /resume past sessions".into(),
            },
            ts: None,
        }));
    }

    fn update_from_core(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let config = runtime.core.config.clone();
        self.config = config;
        self.header.model = runtime.core.provider.model().to_string();
        self.header.logged_in = runtime.core.logged_in();
        self.header.profile = self.config.profile_name().to_string();
        self.header.tool_mode = runtime.core.tool_mode_label().unwrap_or_default();
        self.header.trust = self.config.trust || self.options.trust;
        self.window = runtime.core.effective_window().0;
        self.header.window = self.window;
        self.header.cwd = runtime.core.paths.cwd.display().to_string();
        if let Ok(session) = runtime.session.lock() {
            self.header.session = short_session_id(&session.id);
        }
        self.used_tokens = estimate_used(&runtime.core);
        self.header.used = self.used_tokens;
        self.sync_banner();
    }

    fn apply_git(&mut self, status: GitStatus) {
        self.header.git = status;
    }

    fn draw(&mut self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        let size = terminal.size().context("cannot read the terminal size")?;
        self.last_width = size.width as usize;
        let debug_lines = if self.debug {
            self.debug_lines()
        } else {
            Vec::new()
        };
        let activity = self.activity();
        let plan = self.visible_plan();
        let busy = self.turn_active;
        self.input.set_placeholder(if busy {
            "the agent is working · esc to interrupt"
        } else if self.switching.is_some() {
            "switching · type ahead, Enter sends when ready"
        } else {
            "ask anything · @file attaches · / for commands · tab completes"
        });
        let text_width = size.width.saturating_sub(render::PROMPT_WIDTH + 1) as usize;
        let rects = render::split_with(
            Rect::new(0, 0, size.width, size.height),
            render::LayoutSpec {
                input_lines: self.input.visual_rows(text_width),
                debug_lines: debug_lines.len(),
                activity: activity.is_some(),
                plan_rows: render::plan_rows(&plan),
            },
        );
        self.transcript
            .set_viewport(rects.transcript.height as usize);
        let show_reasoning = self.config.ui.show_reasoning;
        self.transcript
            .prepare(self.last_width, &self.theme, show_reasoning);
        self.input.apply_theme(&self.theme);
        self.input
            .fit_viewport(text_width as u16, rects.input.height.saturating_sub(2));

        let completion = self.current_completion();
        let completion_title = completion
            .as_ref()
            .map(|completion| completion.kind.title())
            .unwrap_or("");
        let completions: Vec<(String, String)> = completion
            .map(|completion| {
                completion
                    .candidates
                    .into_iter()
                    .map(|candidate| (candidate.label, candidate.description))
                    .collect()
            })
            .unwrap_or_default();
        let completion_index = self
            .input
            .completion_index
            .min(completions.len().saturating_sub(1));
        let footer = self.footer();
        let header = self.header.clone();
        let modal = self.modal.as_ref();
        let transcript = &self.transcript;
        let input = &self.input.textarea;

        terminal
            .draw(|frame| {
                let scene = render::Scene {
                    header: header.clone(),
                    transcript,
                    input,
                    debug_lines: debug_lines.clone(),
                    footer: footer.clone(),
                    completions: completions.clone(),
                    completion_index,
                    completion_title,
                    modal,
                    activity: activity.clone(),
                    plan: plan.clone(),
                    busy,
                };
                render::draw(frame, &scene, &self.theme);
            })
            .context("cannot draw the TUI")?;
        Ok(())
    }

    fn footer(&self) -> FooterData {
        // Login and trust state live in the info bar; the footer is hints or
        // a transient notice only.
        let (status, status_role) = (String::new(), Role::Muted);
        let hints = if !self.transcript.follow {
            "End or Ctrl+End to return to the bottom · wheel/pgup/pgdn scroll".to_string()
        } else if self.turn_active {
            "esc interrupt · ctrl+o tool output · wheel/pgup scroll".to_string()
        } else {
            HINTS.to_string()
        };
        let (notice, notice_role) = match &self.notice {
            Some((text, role, _)) => (Some(text.clone()), *role),
            None => (None, Role::Warning),
        };
        FooterData {
            hints,
            status,
            status_role,
            notice,
            notice_role,
        }
    }

    /// The live status line: what the agent is doing right now.
    fn activity(&self) -> Option<ActivityData> {
        let spinner = SPINNER[self.spinner % SPINNER.len()];
        if !self.turn_active {
            return self.switching.as_ref().map(|switch| ActivityData {
                spinner,
                label: switch.label(),
                detail: String::new(),
                meta: "the prompt stays editable".into(),
                role: Role::Accent,
            });
        }
        let elapsed = self
            .turn_started
            .map(|at| at.elapsed().as_secs())
            .unwrap_or(0);
        let mut meta = vec![render::elapsed_label(elapsed)];
        if self.completion_tokens > 0 {
            meta.push(format!(
                "↓ {} tokens",
                crate::ui::widgets::format_tokens(self.completion_tokens)
            ));
        }
        if self.cancelling {
            meta.push("ctrl+c to force quit".into());
            return Some(ActivityData {
                spinner,
                label: "Interrupting…".into(),
                detail: String::new(),
                meta: meta.join(" · "),
                role: Role::Warning,
            });
        }
        meta.push("esc to interrupt".into());
        let (label, detail) = match (self.turn_state, self.running_tools.as_slice()) {
            (_, [tool]) => (
                format!("Running {}", tool.name),
                format!(
                    "· {} ({})",
                    tool.summary,
                    render::elapsed_label(tool.started.elapsed().as_secs())
                ),
            ),
            (_, [_, ..]) => (
                format!("Running {} tools", self.running_tools.len()),
                format!(
                    "· {}",
                    self.running_tools
                        .iter()
                        .map(|tool| tool.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            (Some(AgentState::StreamModel), []) => match self.stream_phase {
                StreamPhase::Waiting => ("Waiting for the model…".into(), String::new()),
                StreamPhase::Thinking => ("Thinking…".into(), String::new()),
                StreamPhase::Writing => ("Writing…".into(), String::new()),
            },
            (Some(AgentState::Compact), []) => ("Compacting context…".into(), String::new()),
            (Some(AgentState::ToolBatch), []) => ("Running tools…".into(), String::new()),
            (Some(AgentState::PostTool), []) => ("Reading tool results…".into(), String::new()),
            (Some(AgentState::SettleGates), []) => ("Verifying…".into(), String::new()),
            _ => ("Preparing context…".into(), String::new()),
        };
        Some(ActivityData {
            spinner,
            label,
            detail,
            meta: meta.join(" · "),
            role: Role::Accent,
        })
    }

    /// The plan shows while there is open work, or during a turn.
    fn visible_plan(&self) -> Vec<(String, PlanStatus)> {
        let open = self
            .plan
            .iter()
            .any(|(_, status)| *status != PlanStatus::Completed);
        if self.show_plan && (open || self.turn_active) {
            self.plan.clone()
        } else {
            Vec::new()
        }
    }

    fn refresh_plan(&mut self) {
        if let Some(runtime) = &self.runtime {
            let session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            self.plan = plan_snapshot(&session);
        }
    }

    fn debug_lines(&self) -> Vec<String> {
        let Some(runtime) = &self.runtime else {
            return Vec::new();
        };
        let (session_path, session_id) = match runtime.session.lock() {
            Ok(session) => (
                session
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "(ephemeral)".into()),
                session.id.clone(),
            ),
            Err(poison) => {
                let session = poison.into_inner();
                (
                    session
                        .path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "(ephemeral)".into()),
                    session.id.clone(),
                )
            }
        };
        vec![
            format!(
                "debug · turn={} state={} spinner={}",
                if self.turn_active { "active" } else { "idle" },
                state_label(self.turn_state),
                self.spinner,
            ),
            format!(
                "tokens {}/{} · {:.2} chars/token · completion {}",
                self.used_tokens,
                self.window,
                runtime.core.budget.chars_per_token,
                self.completion_tokens,
            ),
            format!(
                "last call · {}",
                self.last_usage
                    .as_ref()
                    .map(|usage| usage.perf_line())
                    .filter(|line| !line.is_empty())
                    .unwrap_or_else(|| "no timings yet".into()),
            ),
            format!(
                "provider {} · model {} · window {} ({})",
                self.config.model.endpoint,
                runtime.core.provider.model(),
                runtime.core.effective_window().0,
                runtime.core.effective_window().1.label(),
            ),
            format!(
                "profile {} · trust {} · theme {}",
                self.config.profile_name(),
                self.header.trust,
                self.theme.name,
            ),
            format!("session {session_id} · {session_path}"),
            format!(
                "git {} · tools {}",
                self.header.git.label(),
                runtime.core.tools.len(),
            ),
        ]
    }

    /// Advance timers. Returns whether anything visible changed, so an idle
    /// TUI does not redraw ten times a second.
    fn on_tick(&mut self) -> bool {
        let animating = self.turn_active || self.switching.is_some();
        if animating {
            self.spinner = (self.spinner + 1) % SPINNER.len();
        }
        let warnings = self.warnings_seen;
        self.sync_session_warnings();
        let mut changed = animating || warnings != self.warnings_seen;
        if let Some((_, _, at)) = &self.notice
            && at.elapsed() > Duration::from_secs(8)
        {
            self.notice = None;
            changed = true;
        }
        let before = (self.tok_s, self.header.completion);
        let now = Instant::now();
        while let Some((at, _)) = self.token_events.front() {
            if now.duration_since(*at) > Duration::from_secs(3) {
                self.token_events.pop_front();
            } else {
                break;
            }
        }
        let total: u64 = self.token_events.iter().map(|(_, tokens)| *tokens).sum();
        // Live estimate while tokens flow; afterwards the last measured rate
        // instead of decaying to zero.
        self.tok_s = if total > 0 {
            total as f32 / 3.0
        } else {
            self.last_usage
                .as_ref()
                .and_then(|usage| usage.tokens_per_sec)
                .unwrap_or(0.0)
        };
        self.header.tok_s = self.tok_s;
        self.header.completion = self.completion_tokens;
        changed || before != (self.tok_s, self.header.completion) || self.transcript.is_dirty()
    }

    fn record_tokens(&mut self, tokens: u64) {
        self.token_events.push_back((Instant::now(), tokens));
    }

    fn start_turn(&mut self, text: String) {
        if self.turn_active {
            self.set_notice(
                "a turn is running; press Esc to cancel first",
                Role::Warning,
            );
            return;
        }
        if text.trim().is_empty() {
            return;
        }
        if let Some(switch) = &self.switching {
            let label = switch.label();
            self.set_notice(
                format!("{label} send again when it finishes"),
                Role::Warning,
            );
            return;
        }
        let Some(runtime) = &self.runtime else {
            return;
        };
        if !runtime.core.logged_in() {
            self.transcript.push_note("login", LOGIN_HINT, false);
            self.set_notice("no model selected; run /login", Role::Warning);
            return;
        }
        let core = runtime.core.clone();
        self.transcript.begin_turn(&text);
        self.turn_active = true;
        self.cancelling = false;
        self.turn_state = Some(AgentState::AssembleContext);
        self.turn_started = Some(Instant::now());
        self.stream_phase = StreamPhase::Waiting;
        self.running_tools.clear();
        self.completion_tokens = 0;
        self.tok_s = 0.0;
        self.token_events.clear();
        self.notice = None;
        let (tx, rx) = mpsc::unbounded_channel();
        self.agent_rx = Some(rx);
        let token = core.cancel.child_token();
        self.turn_cancel = token.clone();
        self.agent_task = Some(crate::agent::spawn_turn(core, text, tx, token));
    }

    fn cancel_turn(&mut self) {
        if !self.turn_active {
            return;
        }
        self.cancelling = true;
        self.turn_cancel.cancel();
    }

    fn finish_turn(&mut self, outcome: Option<TurnOutcome>) {
        self.turn_active = false;
        self.cancelling = false;
        self.turn_state = None;
        self.turn_started = None;
        self.running_tools.clear();
        self.agent_rx = None;
        self.agent_task = None;
        self.transcript.finish_turn();
        // Also syncs the session file.
        self.refresh_session_blocks();
        match outcome {
            Some(outcome) => {
                if let Some((code, message)) = outcome.error {
                    self.transcript
                        .push_note("error", format!("{code}: {message}"), true);
                    self.set_notice(format!("{code}: {message}"), Role::Error);
                }
                match outcome.status {
                    TurnStatus::Done => {}
                    TurnStatus::Cancelled => {
                        self.set_notice("turn cancelled", Role::Warning);
                    }
                    TurnStatus::Budget => {
                        self.set_notice(
                            "turn stopped: checkpoint reached with no progress",
                            Role::Warning,
                        );
                    }
                    TurnStatus::Stalled => {
                        self.set_notice("turn stopped: stalled", Role::Warning);
                    }
                    TurnStatus::ContextTruncated => {
                        self.set_notice(
                            "turn stopped: the prompt was truncated; /compact or /new",
                            Role::Error,
                        );
                    }
                    TurnStatus::Blocked => {
                        self.set_notice("turn blocked by a guard", Role::Error);
                    }
                    TurnStatus::Error => {
                        if self.notice.is_none() {
                            self.set_notice("turn failed", Role::Error);
                        }
                    }
                }
            }
            None => {
                self.transcript
                    .push_note("error", "agent task ended unexpectedly", true);
                self.set_notice("agent task ended unexpectedly", Role::Error);
            }
        }
    }

    fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TurnStarted {
                model,
                profile,
                tool_mode,
            } => {
                self.header.logged_in = !model.trim().is_empty();
                self.header.model = model;
                self.header.profile = profile;
                self.header.tool_mode = tool_mode;
                self.turn_state = Some(AgentState::AssembleContext);
            }
            AgentEvent::State(state) => {
                if state == AgentState::StreamModel && self.turn_state != Some(state) {
                    self.stream_phase = StreamPhase::Waiting;
                }
                self.turn_state = Some(state);
            }
            AgentEvent::TextDelta(text) => {
                let tokens = (text.chars().count() as u64).div_ceil(4).max(1);
                self.record_tokens(tokens);
                self.completion_tokens = self.completion_tokens.saturating_add(tokens as u32);
                self.stream_phase = StreamPhase::Writing;
                self.transcript.text_delta(&text);
            }
            AgentEvent::ReasoningDelta(text) => {
                let tokens = (text.chars().count() as u64).div_ceil(4).max(1);
                self.record_tokens(tokens);
                if self.stream_phase == StreamPhase::Waiting {
                    self.stream_phase = StreamPhase::Thinking;
                }
                self.transcript.reasoning_delta(&text);
            }
            AgentEvent::ToolStarted {
                call_id,
                name,
                summary,
            } => {
                self.turn_state = Some(AgentState::ToolBatch);
                self.transcript.tool_started(&call_id, &name, &summary);
                self.running_tools.push(RunningTool {
                    call_id,
                    name,
                    summary,
                    started: Instant::now(),
                });
            }
            AgentEvent::ToolFinished {
                call_id,
                name,
                is_error,
                duration_ms,
                preview,
                artifact,
            } => {
                self.transcript.tool_finished(
                    &call_id,
                    &name,
                    is_error,
                    duration_ms,
                    &preview,
                    artifact,
                );
                self.running_tools.retain(|tool| tool.call_id != call_id);
                if name == "todo" {
                    self.refresh_plan();
                }
            }
            AgentEvent::Usage(usage) => {
                if usage.prompt_tokens > 0 {
                    self.used_tokens = usage.prompt_tokens;
                    self.header.used = usage.prompt_tokens;
                }
                self.completion_tokens = usage.completion_tokens;
                self.header.completion = usage.completion_tokens;
                if usage.cache_percent().is_some() {
                    self.header.cache = usage.cache_percent();
                }
                self.last_usage = Some(usage);
            }
            AgentEvent::Context { used, window } => {
                self.used_tokens = used;
                self.window = window;
                self.header.used = used;
                self.header.window = window;
            }
            AgentEvent::WindowDetected { window, source } => {
                self.window = window;
                self.header.window = window;
                self.header.window_guess = false;
                self.sync_banner();
                self.set_notice(
                    format!(
                        "context window: {} ({source})",
                        crate::ui::widgets::format_tokens(window)
                    ),
                    Role::Success,
                );
            }
            AgentEvent::Compacted {
                summary,
                tokens_before,
            } => {
                let preview = clip_end(
                    &summary.split_whitespace().collect::<Vec<_>>().join(" "),
                    160,
                );
                self.transcript.turn_note(
                    "compacted",
                    &format!("{tokens_before} tokens before · {preview}"),
                    false,
                );
            }
            AgentEvent::Notice(message) => {
                self.transcript.turn_note("notice", &message, false);
                self.set_notice(message, Role::Muted);
            }
            AgentEvent::Guard { code, message } => {
                self.transcript.turn_note(&code, &message, true);
            }
            AgentEvent::Error {
                code,
                message,
                recoverable,
            } => {
                self.transcript.turn_error(&code, &message);
                self.set_notice(
                    format!(
                        "{code}: {message}{}",
                        if recoverable { "" } else { " (fatal)" }
                    ),
                    Role::Error,
                );
            }
            AgentEvent::TurnDone(outcome) => {
                self.finish_turn(Some(outcome));
            }
        }
    }

    async fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key).await,
            Event::Paste(text) => self.input.insert_text(&text),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Resize(_, _) => {}
            _ => {}
        }
    }

    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.modal.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.transcript.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.transcript.scroll_down(WHEEL_LINES),
            _ => {}
        }
    }

    async fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            if self.turn_active {
                if self.cancelling {
                    self.quit = true;
                } else {
                    self.cancel_turn();
                }
            } else {
                self.quit = true;
            }
            return;
        }
        if ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) {
            if self.input.is_empty() && !self.turn_active {
                self.quit = true;
            }
            return;
        }

        if self.modal.is_some() {
            self.handle_modal_key(key).await;
            return;
        }

        if ctrl && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
            self.config.ui.show_reasoning = !self.config.ui.show_reasoning;
            self.transcript
                .set_show_reasoning(self.config.ui.show_reasoning);
            let state = if self.config.ui.show_reasoning {
                "reasoning expanded"
            } else {
                "reasoning collapsed"
            };
            self.set_notice(state, Role::Muted);
            return;
        }

        if ctrl && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O')) {
            let expand = !self.transcript.expand_tools();
            self.transcript.set_expand_tools(expand);
            self.set_notice(
                if expand {
                    "tool output expanded"
                } else {
                    "tool output collapsed"
                },
                Role::Muted,
            );
            return;
        }
        if ctrl && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T')) {
            self.show_plan = !self.show_plan;
            self.set_notice(
                if self.show_plan {
                    "plan shown"
                } else {
                    "plan hidden"
                },
                Role::Muted,
            );
            return;
        }

        match key.code {
            KeyCode::Esc => {
                if self.turn_active {
                    self.cancel_turn();
                } else if self.input.text().trim_start().starts_with('/')
                    && !self.input.text().contains('\n')
                {
                    // Abandon a half-typed command (and its popup).
                    self.input.set_text("");
                } else if self.input.popup_open() {
                    self.input.reset_completion();
                }
                return;
            }
            KeyCode::PageUp => {
                self.transcript.scroll_up(self.transcript.page());
                return;
            }
            KeyCode::PageDown => {
                self.transcript.scroll_down(self.transcript.page());
                return;
            }
            KeyCode::Up if shift => {
                self.transcript.scroll_up(1);
                return;
            }
            KeyCode::Down if shift => {
                self.transcript.scroll_down(1);
                return;
            }
            // Home/End edit the prompt when it has text; Ctrl always scrolls.
            KeyCode::Home if ctrl || self.input.is_empty() => {
                self.transcript.scroll_home();
                return;
            }
            KeyCode::End if ctrl || self.input.is_empty() => {
                self.transcript.scroll_end();
                return;
            }
            _ => {}
        }

        if ctrl && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('J')) {
            self.input.insert_newline();
            return;
        }

        match key.code {
            KeyCode::Enter => {
                if shift || alt {
                    self.input.insert_newline();
                    return;
                }
                // A partial command or argument runs as its highlighted
                // completion: `/the dr` + Enter runs `/theme dracula`.
                if let Some(completion) = self.current_completion()
                    && completion.kind != complete::Kind::Path
                    && !completion.token.is_empty()
                    && self.input.cursor_at_end()
                    && !completion
                        .candidates
                        .iter()
                        .any(|candidate| candidate.value == completion.token)
                {
                    let index = self
                        .input
                        .completion_index
                        .min(completion.candidates.len() - 1);
                    let value = completion.candidates[index].value.clone();
                    self.input.replace_before_cursor(completion.start, &value);
                }
                // While a switch loads, prose stays in the prompt (commands
                // still run) so nothing typed ahead is lost.
                let command = self.input.text().trim_start().starts_with('/');
                let waiting = self.switching.is_some() && !command;
                let idle = !self.turn_active && !waiting;
                match self.input.submit(idle) {
                    Some(text) => {
                        let trimmed = text.trim().to_string();
                        if trimmed.starts_with('/') {
                            self.run_command(&trimmed).await;
                        } else if !trimmed.is_empty() {
                            self.start_turn(text);
                        }
                    }
                    None if waiting => {
                        let label = self
                            .switching
                            .as_ref()
                            .map(Switch::label)
                            .unwrap_or_default();
                        self.set_notice(
                            format!("{label} press Enter again when it finishes"),
                            Role::Warning,
                        );
                    }
                    None if !idle => {
                        self.set_notice(
                            "a turn is running; press Esc to cancel before sending",
                            Role::Warning,
                        );
                    }
                    None => {}
                }
                return;
            }
            KeyCode::Up | KeyCode::Down => {
                if let Some(completion) = self.current_completion() {
                    let delta = if matches!(key.code, KeyCode::Up) {
                        -1
                    } else {
                        1
                    };
                    self.input
                        .cycle_completion(delta, completion.candidates.len());
                    return;
                }
                if self.input.is_empty() {
                    let moved = if matches!(key.code, KeyCode::Up) {
                        self.input.history_prev()
                    } else {
                        self.input.history_next()
                    };
                    if moved {
                        return;
                    }
                }
                self.input.handle_key(key);
                return;
            }
            KeyCode::Tab => {
                // A tab character never belongs in a slash command.
                if !self.tab_complete() && !self.input.text().trim_start().starts_with('/') {
                    self.input.handle_key(key);
                }
                return;
            }
            KeyCode::BackTab => {
                if let Some(completion) = self.current_completion() {
                    self.input.cycle_completion(-1, completion.candidates.len());
                }
                return;
            }
            _ => {}
        }

        self.input.handle_key(key);
    }

    async fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        let mut close = false;
        let mut target: Option<String> = None;
        let mut focus_input = false;
        let mut switch: Option<PathBuf> = None;
        let mut picked: Option<PickAction> = None;
        let mut preview_theme: Option<String> = None;

        match &mut modal {
            ModalData::Tree(view) => match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Up => view.move_by(-1),
                KeyCode::Down => view.move_by(1),
                KeyCode::PageUp => view.page(-1, 15),
                KeyCode::PageDown => view.page(1, 15),
                KeyCode::Home => view.home(),
                KeyCode::End => view.end(),
                KeyCode::Char('k') => view.move_by(-1),
                KeyCode::Char('j') => view.move_by(1),
                KeyCode::Char('f') | KeyCode::Char('n') => {
                    if let Some(row) = view.selected_row() {
                        target = Some(row.id.clone());
                        focus_input = true;
                    }
                }
                KeyCode::Enter => {
                    if let Some(row) = view.selected_row() {
                        target = Some(row.id.clone());
                    }
                }
                _ => {}
            },
            ModalData::Sessions(view) => match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Up | KeyCode::Char('k') => {
                    view.selected = view.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.selected = (view.selected + 1).min(view.items.len().saturating_sub(1));
                }
                KeyCode::PageUp => {
                    view.selected = view.selected.saturating_sub(15);
                }
                KeyCode::PageDown => {
                    view.selected = (view.selected + 15).min(view.items.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(item) = view.items.get(view.selected) {
                        switch = Some(item.path.clone());
                    }
                }
                _ => {}
            },
            ModalData::Tools(view) => match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Up | KeyCode::Char('k') => {
                    view.selected = view.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.selected = (view.selected + 1).min(view.items.len().saturating_sub(1));
                }
                KeyCode::PageUp => {
                    view.selected = view.selected.saturating_sub(15);
                }
                KeyCode::PageDown => {
                    view.selected = (view.selected + 15).min(view.items.len().saturating_sub(1));
                }
                _ => {}
            },
            // Letters feed the filter, so navigation is arrows, Tab, and
            // PgUp/PgDn; Ctrl+P/N and Ctrl+K/J also move.
            ModalData::Picker(view) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => {
                        // Esc clears a filter first, then closes.
                        if view.filter.is_empty() {
                            close = true;
                        } else {
                            view.filter.clear();
                            view.selected = 0;
                            view.scroll = 0;
                        }
                    }
                    KeyCode::Up | KeyCode::BackTab => view.move_by(-1),
                    KeyCode::Down | KeyCode::Tab => view.move_by(1),
                    KeyCode::Char('p' | 'k') if ctrl => view.move_by(-1),
                    KeyCode::Char('n' | 'j') if ctrl => view.move_by(1),
                    KeyCode::PageUp => view.move_by(-15),
                    KeyCode::PageDown => view.move_by(15),
                    KeyCode::Home => view.move_by(isize::MIN / 2),
                    KeyCode::End => view.move_by(isize::MAX / 2),
                    KeyCode::Backspace => view.pop_filter(),
                    KeyCode::Enter => picked = view.selected_item().map(|item| item.action.clone()),
                    KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                        view.push_filter(ch);
                    }
                    _ => {}
                }
                // The theme picker previews the highlighted theme live.
                if view.command == "theme" && !close && picked.is_none() {
                    preview_theme = view
                        .selected_item()
                        .filter(|item| item.label != "save")
                        .map(|item| item.label.clone());
                }
            }
        }

        if let Some(target) = target {
            match self.checkout_target(&target, "tree") {
                Ok(()) => {
                    close = true;
                    self.set_notice(
                        if focus_input {
                            "branch point selected; type a message"
                        } else {
                            "checked out"
                        },
                        Role::Success,
                    );
                }
                Err(error) => self.set_notice(error, Role::Error),
            }
        }

        if let Some(path) = switch {
            self.modal = None;
            self.switch_session(path).await;
            return;
        }

        if let Some(action) = picked {
            self.modal = None;
            match action {
                PickAction::Run(command) => {
                    // A confirmed theme keeps the preview's base for `save`.
                    self.theme_before_preview = None;
                    self.run_command(&command).await;
                }
                PickAction::Fill(text) => {
                    self.restore_previewed_theme();
                    self.input.set_text(&text);
                }
            }
            return;
        }

        if let Some(name) = preview_theme
            && name != self.theme.name
        {
            self.apply_theme_named(&name);
        }

        if close {
            self.modal = None;
            self.restore_previewed_theme();
        } else {
            modal.ensure_visible(15);
            self.modal = Some(modal);
        }
    }

    async fn run_command(&mut self, text: &str) {
        let body = text.trim().trim_start_matches('/');
        let (name, rest) = match body.find(char::is_whitespace) {
            Some(position) => (&body[..position], body[position..].trim()),
            None => (body, ""),
        };
        let always = matches!(name, "help" | "quit" | "debug" | "trust");
        if self.turn_active && !always {
            self.set_notice(
                "cancel the running turn first (Esc) before using this command",
                Role::Warning,
            );
            return;
        }

        if self.switching.is_some() && matches!(name, "model" | "login" | "profile") {
            self.set_notice(
                "a switch is still loading; try again when it finishes",
                Role::Warning,
            );
            return;
        }

        match name {
            "help" => self.command_help(rest),
            "quit" => self.quit = true,
            "model" => self.command_model(rest).await,
            "login" => self.command_login(rest).await,
            "profile" => self.command_profile(rest).await,
            "theme" => self.command_theme(rest),
            "tree" => self.command_tree(),
            "fork" => self.command_fork(),
            "new" => self.command_new().await,
            "resume" => self.command_resume(rest).await,
            "sessions" => self.command_sessions(),
            "compact" => self.command_compact(),
            "tools" => self.command_tools(),
            "mcp" => self.command_mcp(),
            "skill" => self.command_skill(rest),
            "workflow" => self.command_workflow(rest).await,
            "sec" => self.command_sec(rest),
            "trust" => self.command_trust(rest).await,
            "export" => self.command_export(rest),
            "clear" => self.command_clear(),
            "debug" => {
                self.debug = !self.debug;
                self.set_notice(
                    if self.debug {
                        "debug panel on"
                    } else {
                        "debug panel off"
                    },
                    Role::Muted,
                );
            }
            other => self.unknown_command(other),
        }
        self.refresh_completion_sources(matches!(name, "new" | "resume" | "sessions"));
    }

    fn command_help(&mut self, topic: &str) {
        let topic = topic.trim().trim_start_matches('/');
        if topic.is_empty() {
            self.transcript.push_note("help", HELP, false);
            return;
        }
        match COMMANDS.iter().find(|(name, _)| *name == topic) {
            Some((name, description)) => {
                let usage = match command_usage(name) {
                    "" => format!("/{name}"),
                    usage => usage.to_string(),
                };
                self.transcript.push_note(
                    "help",
                    format!("/{name} — {description}\nusage: {usage}"),
                    false,
                );
            }
            None => self.unknown_command(topic),
        }
    }

    /// Open a picker modal, or say there is nothing to pick.
    fn open_picker(&mut self, picker: PickerModal, empty: &str) {
        if picker.items.is_empty() {
            self.set_notice(empty.to_string(), Role::Muted);
            return;
        }
        let mut modal = ModalData::Picker(picker);
        modal.ensure_visible(15);
        self.modal = Some(modal);
    }

    fn unknown_command(&mut self, name: &str) {
        let available: Vec<String> = COMMANDS
            .iter()
            .map(|(command, _)| format!("/{command}"))
            .collect();
        self.transcript.push_note(
            "unknown command",
            format!(
                "/{name} is not a command.\nAvailable: {}",
                available.join(" ")
            ),
            true,
        );
        self.set_notice(format!("unknown command /{name}; try /help"), Role::Error);
    }

    async fn command_model(&mut self, name: &str) {
        let name = name.trim();
        if !name.is_empty() {
            if name.split_whitespace().count() != 1 {
                self.set_notice("usage: /model <name>", Role::Error);
                return;
            }
            self.switch_model(name).await;
            return;
        }
        let Some(core) = self.core().cloned() else {
            return;
        };
        let current = if core.logged_in() {
            core.provider.model().to_string()
        } else {
            String::new()
        };
        let endpoint = core.config.model.endpoint.clone();
        // A warm list (probed at startup or by the last picker) opens the
        // picker at once; a refresh lands in the open picker.
        if !self.completion_sources.models.is_empty() {
            let picker = PickerModal::new(
                "model",
                "Models",
                model_items(&self.completion_sources.models),
                current,
            )
            .with_subtitle(endpoint);
            self.open_picker(picker, "the endpoint reports no models");
            self.spawn_models_refresh();
            return;
        }
        match probe_endpoint_models(&core.config.model).await {
            Ok(models) if !models.is_empty() => {
                self.completion_sources.models = models.clone();
                self.completion_cache.replace(None);
                let picker = PickerModal::new("model", "Models", model_items(&models), current)
                    .with_subtitle(endpoint);
                self.open_picker(picker, "the endpoint reports no models");
            }
            result => {
                let mut lines = vec![
                    format!("endpoint: {endpoint}"),
                    format!(
                        "model: {}",
                        if current.is_empty() {
                            "(not set)"
                        } else {
                            current.as_str()
                        }
                    ),
                ];
                match result {
                    Ok(_) => {
                        lines.push("available models: (the endpoint reports none)".into());
                        lines.push("load a model, then run /model again".into());
                    }
                    Err(error) => {
                        lines.push(format!("cannot list models: {error}"));
                        lines.push(
                            "start the server, or run /login <endpoint> [model] [api-key]".into(),
                        );
                    }
                }
                self.transcript.push_note("models", lines.join("\n"), false);
            }
        }
    }

    /// Switch the active model for this session; the saved config is
    /// untouched. The rebuild (which may wait for the server to load the
    /// model) runs in the background.
    async fn switch_model(&mut self, name: &str) {
        if self
            .core()
            .is_some_and(|core| core.logged_in() && core.provider.model() == name)
        {
            self.set_notice(format!("model: {name} (unchanged)"), Role::Muted);
            return;
        }
        let mut config = self.config.clone();
        config.model.model = name.to_string();
        self.spawn_rebuild(Switch::Model(name.to_string()), config);
    }

    async fn command_login(&mut self, rest: &str) {
        let Some(core) = self.core().cloned() else {
            return;
        };
        let mut parts = rest.split_whitespace();
        let Some(endpoint) = parts.next() else {
            self.transcript
                .push_note("login", login_status(&core), false);
            return;
        };
        let model = parts.next();
        let api_key = parts.next();
        if parts.next().is_some() {
            self.set_notice("usage: /login <endpoint> [model] [api-key]", Role::Error);
            return;
        }
        let endpoint = normalize_endpoint(endpoint);
        let mut candidate = self.config.clone();
        let endpoint_changed = crate::provider::endpoint_urls(&candidate.model.endpoint).api
            != crate::provider::endpoint_urls(&endpoint).api;
        candidate.model.endpoint = endpoint.clone();
        if endpoint_changed && model.is_none() {
            candidate.model.model.clear();
        }
        if endpoint_changed && api_key.is_none() {
            candidate.model.api_key.clear();
        }
        if let Some(model) = model {
            candidate.model.model = model.to_string();
        }
        if let Some(api_key) = api_key {
            candidate.model.api_key = if api_key == "-" {
                String::new()
            } else {
                api_key.to_string()
            };
        }

        let models = match probe_endpoint_models(&candidate.model).await {
            Ok(models) => {
                self.completion_sources.models = models.clone();
                models
            }
            Err(error) => {
                self.transcript.push_note(
                    "login",
                    format!(
                        "cannot log in to {endpoint}: {error}\n\nStart the inference server and retry, or check the endpoint URL."
                    ),
                    true,
                );
                self.set_notice(format!("login failed: {error}"), Role::Error);
                return;
            }
        };

        if candidate.model.model.trim().is_empty() {
            match models.as_slice() {
                [only] => candidate.model.model = only.clone(),
                [] => {
                    self.transcript.push_note(
                        "login",
                        format!(
                            "{endpoint} reports no models.\nLoad a model, or pass its id: /login <endpoint> <model>"
                        ),
                        true,
                    );
                    self.set_notice("login failed: endpoint reports no models", Role::Error);
                    return;
                }
                _ => {
                    self.transcript.push_note(
                        "login",
                        format!(
                            "{endpoint} exposes multiple models:\n{}\n\nChoose one: /login <endpoint> <model>",
                            models.join("\n")
                        ),
                        true,
                    );
                    self.set_notice("login failed: choose a model", Role::Error);
                    return;
                }
            }
        } else if !models.is_empty()
            && !models
                .iter()
                .any(|available| available == candidate.model.model.trim())
        {
            self.transcript.push_note(
                "login",
                format!(
                    "{endpoint} has no model {:?}. Available:\n{}\n\nRetry with: /login <endpoint> <model>",
                    candidate.model.model,
                    models.join("\n")
                ),
                true,
            );
            self.set_notice("login failed: model not found", Role::Error);
            return;
        }

        let previous = self.config.clone();
        if let Err(error) = self.rebuild_core_with(candidate).await {
            self.config = previous;
            self.set_notice(format!("login failed: {error}"), Role::Error);
            return;
        }
        let model = self.config.model.model.clone();
        let key_state = if self.config.model.api_key.is_empty() {
            "not set"
        } else {
            "set"
        };
        match self.persist_connection() {
            Ok(path) => {
                self.transcript.push_note(
                    "login",
                    format!(
                        "endpoint: {endpoint}\nmodel: {model}\napi key: {key_state}\nsaved: {}",
                        path.display()
                    ),
                    false,
                );
                self.set_notice(format!("logged in · {model} · saved"), Role::Success);
            }
            Err(error) => {
                self.transcript.push_note(
                    "login",
                    format!("logged in as {model}, but settings were not saved: {error}"),
                    true,
                );
                self.set_notice(
                    format!("logged in · settings not saved: {error}"),
                    Role::Warning,
                );
            }
        }
    }

    fn persist_connection(&self) -> Result<PathBuf> {
        let core = self.core().context("no runtime")?;
        let path = core.paths.config_file();
        core.config.model.save_connection(&path)?;
        Ok(path)
    }

    async fn command_profile(&mut self, name: &str) {
        if name.is_empty() {
            let items = self
                .config
                .profiles
                .iter()
                .map(|(profile, data)| {
                    PickItem::run(
                        "profile",
                        profile.clone(),
                        data.description.clone().unwrap_or_default(),
                    )
                })
                .collect();
            let picker = PickerModal::new(
                "profile",
                "Profiles",
                items,
                self.config.profile_name().to_string(),
            );
            self.open_picker(picker, "no profiles are configured");
            return;
        }
        if !self.config.profiles.contains_key(name) {
            let available: Vec<String> = self.config.profiles.keys().cloned().collect();
            let message = format!(
                "unknown profile {name:?}; available: {}",
                available.join(", ")
            );
            self.set_notice(message, Role::Error);
            return;
        }
        if self.config.profile_name() == name {
            self.set_notice(format!("profile: {name} (unchanged)"), Role::Muted);
            return;
        }
        let mut config = self.config.clone();
        config.profile_name = name.to_string();
        self.spawn_rebuild(Switch::Profile(name.to_string()), config);
    }

    async fn rebuild_core_with(&mut self, config: Config) -> Result<()> {
        let Some(old) = self.core().cloned() else {
            bail!("no runtime");
        };
        let core = app::rebuild_core(&old, config).await?;
        self.apply_core(core);
        Ok(())
    }

    /// Install a rebuilt core and refresh everything derived from it.
    fn apply_core(&mut self, core: Arc<Core>) {
        if let Some(runtime) = &mut self.runtime {
            runtime.core = core.clone();
        }
        self.config = core.config.clone();
        self.header.model = core.provider.model().to_string();
        self.header.logged_in = core.logged_in();
        self.header.profile = self.config.profile_name().to_string();
        self.header.tool_mode = core.tool_mode_label().unwrap_or_default();
        self.header.trust = self.config.trust || self.options.trust;
        self.window = core.effective_window().0;
        self.header.window = self.window;
        self.used_tokens = estimate_used(&core);
        self.header.used = self.used_tokens;
        self.sync_banner();
    }

    fn apply_theme_named(&mut self, name: &str) {
        let Some(paths) = self.core().map(|core| core.paths.clone()) else {
            return;
        };
        self.theme = Theme::load(
            name,
            &paths.themes_dir(),
            self.config.ui.color_mode,
            self.no_color,
        );
        self.config.ui.theme = name.to_string();
        self.input.apply_theme(&self.theme);
    }

    /// Undo a `/theme` picker's live preview when it closes without a pick.
    fn restore_previewed_theme(&mut self) {
        if let Some(name) = self.theme_before_preview.take()
            && name != self.theme.name
        {
            self.apply_theme_named(&name);
        }
    }

    fn command_theme(&mut self, argument: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let paths = runtime.core.paths.clone();
        match argument {
            "" => {
                let current = self.theme.name.clone();
                let mut items = vec![PickItem::run(
                    "theme",
                    "save",
                    format!("write {current} to the config"),
                )];
                items.extend(
                    available_theme_names(&paths)
                        .into_iter()
                        .map(|name| PickItem::run("theme", name, "")),
                );
                self.theme_before_preview = Some(current.clone());
                let picker = PickerModal::new("theme", "Themes · live preview", items, current);
                self.open_picker(picker, "no themes found");
            }
            "save" => {
                let path = paths.config_file();
                match save_theme(&path, &self.theme.name) {
                    Ok(()) => {
                        self.set_notice(
                            format!("theme saved to {}", path.display()),
                            Role::Success,
                        );
                    }
                    Err(error) => {
                        self.set_notice(format!("cannot save theme: {error}"), Role::Error);
                    }
                }
            }
            name => {
                let names = available_theme_names(&paths);
                if !names.iter().any(|candidate| candidate == name) {
                    self.set_notice(
                        format!("unknown theme {name:?}; available: {}", names.join(", ")),
                        Role::Error,
                    );
                    return;
                }
                self.apply_theme_named(name);
                self.set_notice(format!("theme: {name}"), Role::Success);
            }
        }
    }

    fn command_tree(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let view = match runtime.session.lock() {
            Ok(session) => TreeView::new(&session),
            Err(poison) => TreeView::new(&poison.into_inner()),
        };
        if view.is_empty() {
            self.set_notice("the session tree is empty", Role::Muted);
            return;
        }
        self.modal = Some(ModalData::Tree(view));
    }

    fn command_fork(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let target = match runtime.session.lock() {
            Ok(session) => session
                .branch_entries()
                .into_iter()
                .rev()
                .find(|entry| entry.is_assistant())
                .map(|entry| entry.id.clone()),
            Err(poison) => poison
                .into_inner()
                .branch_entries()
                .into_iter()
                .rev()
                .find(|entry| entry.is_assistant())
                .map(|entry| entry.id.clone()),
        };
        let Some(target) = target else {
            self.set_notice("no assistant step to fork from", Role::Warning);
            return;
        };
        match self.checkout_target(&target, "fork") {
            Ok(()) => {
                self.set_notice("forked from the latest assistant step", Role::Success);
            }
            Err(error) => self.set_notice(error, Role::Error),
        }
    }

    async fn command_new(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let empty = match runtime.session.lock() {
            Ok(session) => session.tree.entries().len() <= 1,
            Err(poison) => poison.into_inner().tree.entries().len() <= 1,
        };
        if empty {
            self.set_notice("already in a fresh session", Role::Muted);
            return;
        }
        self.open_session(None).await;
    }

    async fn command_resume(&mut self, argument: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let current = match runtime.session.lock() {
            Ok(session) => session.path.clone(),
            Err(poison) => poison.into_inner().path.clone(),
        };
        let Some(current) = current else {
            self.set_notice("sessions are disabled (--no-session)", Role::Warning);
            return;
        };
        let items: Vec<SessionItem> = scan_sessions(&runtime.core.paths)
            .into_iter()
            .filter(|item| item.path != current)
            .collect();
        if items.is_empty() {
            self.set_notice("no other sessions to resume for this project", Role::Muted);
            return;
        }
        let argument = argument.trim();
        if argument.is_empty() {
            self.modal = Some(ModalData::Sessions(SessionsModal {
                items,
                selected: 0,
                scroll: 0,
            }));
            return;
        }
        let path = if argument == "last" {
            Ok(items[0].path.clone())
        } else {
            match_session(&items, argument)
        };
        match path {
            Ok(path) => self.switch_session(path).await,
            Err(message) => {
                self.transcript.push_note("resume", message, true);
                self.set_notice(format!("no session matches {argument:?}"), Role::Error);
            }
        }
    }

    fn command_sessions(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let ephemeral = match runtime.session.lock() {
            Ok(session) => session.path.is_none(),
            Err(poison) => poison.into_inner().path.is_none(),
        };
        if ephemeral {
            self.set_notice("sessions are disabled (--no-session)", Role::Warning);
            return;
        }
        let items = scan_sessions(&runtime.core.paths);
        if items.is_empty() {
            self.set_notice("no sessions for this project", Role::Muted);
            return;
        }
        self.modal = Some(ModalData::Sessions(SessionsModal {
            items,
            selected: 0,
            scroll: 0,
        }));
    }

    fn command_compact(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let findings = runtime.core.findings_summary();
        let result = {
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let budget = context::compact::summary_budget(&runtime.core.budget);
            let summary = context::build_summary(&session, findings.as_ref(), budget);
            let result = session
                .append(EntryKind::Compaction(CompactionData {
                    summary: summary.summary.clone(),
                    tokens_before: summary.tokens_before,
                    reason: "manual".into(),
                    thread_id: None,
                    keep_from: None,
                }))
                .map(|_| summary.tokens_before);
            let _ = session.fsync();
            result
        };
        match result {
            Ok(tokens) => {
                self.refresh_session_blocks();
                self.set_notice(format!("compacted ({tokens} tokens before)"), Role::Success);
            }
            Err(error) => self.set_notice(format!("compaction failed: {error}"), Role::Error),
        }
    }

    fn command_tools(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let items: Vec<ToolItem> = runtime
            .core
            .tools
            .iter()
            .map(|tool| ToolItem {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
            })
            .collect();
        if items.is_empty() {
            self.set_notice("no tools are active in this profile", Role::Warning);
            return;
        }
        self.modal = Some(ModalData::Tools(ToolsModal {
            items,
            selected: 0,
            scroll: 0,
        }));
    }

    fn command_mcp(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let statuses = runtime.core.mcp.status();
        if statuses.is_empty() {
            self.transcript.push_note(
                "mcp",
                "no MCP servers configured; add an [mcp.<name>] table to config.toml (see docs/mcp.md)",
                false,
            );
            return;
        }
        let lines: Vec<String> = statuses.iter().map(|status| status.summary()).collect();
        let failed = statuses
            .iter()
            .any(|status| matches!(status.state, crate::mcp::ServerState::Failed(_)));
        self.transcript.push_note("mcp", lines.join("\n"), failed);
    }

    fn command_skill(&mut self, name: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let skills = SkillIndex::load(&runtime.core.paths, self.config.trust);
        let advertised = skills.advertised_for(&crate::tools::active_packs(&runtime.core.tools));
        if name.is_empty() {
            let items = skill_choices(&runtime.core.paths, self.config.trust, &runtime.core.tools)
                .into_iter()
                .map(|(name, description)| PickItem::run("skill", name, description))
                .collect();
            let picker = PickerModal::new("skill", "Skills · appends instructions", items, "");
            self.open_picker(picker, "no skills are available in this profile");
            return;
        }
        let Some(skill) = skills.get(name) else {
            self.set_notice(
                format!(
                    "unknown skill {name:?}; available: {}",
                    advertised.names().join(", ")
                ),
                Role::Error,
            );
            return;
        };
        let content = format!(
            "{}\n\n{}",
            crate::tools::skill::skill_marker(&skill.name),
            skill.full_text()
        );
        let result = {
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let result = session
                .append(EntryKind::User(UserData {
                    attachments: Vec::new(),
                    content,
                }))
                .map(|entry| entry.id);
            let _ = session.fsync();
            result
        };
        match result {
            Ok(entry) => {
                self.refresh_session_blocks();
                self.set_notice(format!("skill {name} added as {entry}"), Role::Success);
            }
            Err(error) => self.set_notice(format!("cannot add skill: {error}"), Role::Error),
        }
    }

    async fn command_workflow(&mut self, name: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let paths = runtime.core.paths.clone();
        let trust = self.config.trust;
        if name.is_empty() {
            let items = available_workflows(&paths, trust)
                .into_iter()
                .map(|name| {
                    let description = workflow_description(&name);
                    PickItem::run("workflow", name, description)
                })
                .collect();
            let picker = PickerModal::new("workflow", "Workflows · starts a branch", items, "");
            let empty = format!("no workflows in {}", paths.workflows_dir().display());
            self.open_picker(picker, &empty);
            return;
        }
        let global = paths.workflows_dir().join(format!("{name}.toml"));
        let project = paths
            .project_dir()
            .join("workflows")
            .join(format!("{name}.toml"));
        let workflow: WorkflowSpec = if global.is_file() {
            match load_workflow_file(&global) {
                Ok(workflow) => workflow,
                Err(message) => {
                    self.set_notice(message, Role::Error);
                    return;
                }
            }
        } else if project.is_file() {
            if !trust {
                self.set_notice(
                    "project workflows require --trust; restart with --trust to load them",
                    Role::Error,
                );
                return;
            }
            match load_workflow_file(&project) {
                Ok(workflow) => workflow,
                Err(message) => {
                    self.set_notice(message, Role::Error);
                    return;
                }
            }
        } else if let Some(bundled) = workflows::get(name) {
            bundled.clone()
        } else {
            let names = available_workflows(&paths, trust);
            self.set_notice(
                format!(
                    "unknown workflow {name:?}; available: {}",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                ),
                Role::Error,
            );
            return;
        };
        let skills = SkillIndex::load(&paths, trust);
        for skill in &workflow.skills {
            if skills.get(skill).is_none() {
                self.set_notice(
                    format!(
                        "workflow {name} uses unknown skill {skill:?}; available: {}",
                        skills.names().join(", ")
                    ),
                    Role::Error,
                );
                return;
            }
        }
        let mut config = self.config.clone();
        if let Some(profile) = &workflow.profile {
            if !config.profiles.contains_key(profile) {
                let available: Vec<&str> = config.profiles.keys().map(String::as_str).collect();
                self.set_notice(
                    format!(
                        "workflow {name} uses unknown profile {profile:?}; available: {}",
                        available.join(", ")
                    ),
                    Role::Error,
                );
                return;
            }
            config.profile_name = profile.clone();
        }
        if !workflow.tools.is_empty() {
            let base = config.profile().clone();
            let derived_name = format!("workflow-{}", sanitize_name(name));
            let mut derived = base;
            // A workflow's tool list is an allowlist: drop the base profile's
            // packs so unlisted tools in those packs are not silently enabled.
            derived.packs.clear();
            derived.tools = workflow.tools.clone();
            derived.description = workflow
                .description
                .clone()
                .or_else(|| Some(workflow.name.clone()));
            config.profiles.insert(derived_name.clone(), derived);
            config.profile_name = derived_name;
        }
        let previous = self.config.clone();
        if let Err(error) = self.rebuild_core_with(config).await {
            self.config = previous;
            self.set_notice(format!("workflow profile failed: {error}"), Role::Error);
            return;
        }
        let result = {
            let Some(runtime) = &self.runtime else {
                return;
            };
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let mut result = Ok(());
            if let Some(leaf) = session.active_leaf_id() {
                match session.tree.resolve_checkout(&leaf) {
                    Ok(target) => {
                        if let Err(error) = session.checkout(&target, &format!("workflow {name}")) {
                            result = Err(error);
                        }
                    }
                    Err(error) => result = Err(anyhow!("{error}")),
                }
            }
            if result.is_ok()
                && let Err(error) = session.append(EntryKind::User(UserData {
                    attachments: Vec::new(),
                    content: workflow.prompt.clone(),
                }))
            {
                result = Err(error);
            }
            if result.is_ok() {
                for skill_name in &workflow.skills {
                    if let Some(skill) = skills.get(skill_name) {
                        let content = format!(
                            "{}\n\n{}",
                            crate::tools::skill::skill_marker(&skill.name),
                            skill.full_text()
                        );
                        if let Err(error) = session.append(EntryKind::User(UserData {
                            attachments: Vec::new(),
                            content,
                        })) {
                            result = Err(error);
                            break;
                        }
                    }
                }
            }
            let _ = session.fsync();
            result
        };
        match result {
            Ok(()) => {
                self.refresh_session_blocks();
                self.set_notice(format!("workflow {name} started"), Role::Success);
            }
            Err(error) => self.set_notice(format!("workflow failed: {error}"), Role::Error),
        }
    }

    fn command_sec(&mut self, rest: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let paths = runtime.core.paths.clone();
        let security = runtime.core.security.clone();
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.is_empty() {
            let fill = |label: &str, description: &str, text: &str| PickItem {
                label: label.into(),
                description: description.into(),
                action: PickAction::Fill(text.into()),
            };
            let picker = PickerModal::new(
                "sec",
                "Security",
                vec![
                    PickItem::run("sec", "summary", "show the engagement summary"),
                    PickItem::run("sec", "report", "write the engagement report"),
                    fill(
                        "scope add",
                        "add a target to scope (type it next)",
                        "/sec scope add ",
                    ),
                    fill(
                        "scope remove",
                        "remove a target from scope (type it next)",
                        "/sec scope remove ",
                    ),
                    PickItem {
                        label: "scope clear".into(),
                        description: "clear the scope".into(),
                        action: PickAction::Run("/sec scope clear".into()),
                    },
                ],
                "",
            );
            self.open_picker(picker, "");
            return;
        }
        if parts == ["summary"] {
            let summary = match security.lock() {
                Ok(state) => state.summary(),
                Err(poison) => poison.into_inner().summary(),
            };
            self.transcript.push_note("security", summary, false);
            return;
        }
        match parts[0] {
            "scope" => {
                let Some(action) = parts.get(1).copied() else {
                    self.command_sec_usage();
                    return;
                };
                match action {
                    "add" => {
                        let target = parts[2..].join(" ");
                        if target.is_empty() {
                            self.set_notice("usage: /sec scope add <target>", Role::Warning);
                            return;
                        }
                        let result = match security.lock() {
                            Ok(mut state) => state.add_scope(&paths, &target),
                            Err(poison) => poison.into_inner().add_scope(&paths, &target),
                        };
                        match result {
                            Ok(()) => {
                                self.set_notice(format!("scope added: {target}"), Role::Success);
                            }
                            Err(error) => {
                                self.set_notice(format!("scope add failed: {error}"), Role::Error);
                            }
                        }
                    }
                    "remove" => {
                        let target = parts[2..].join(" ");
                        if target.is_empty() {
                            self.set_notice("usage: /sec scope remove <target>", Role::Warning);
                            return;
                        }
                        let result = match security.lock() {
                            Ok(mut state) => state.remove_scope(&paths, &target),
                            Err(poison) => poison.into_inner().remove_scope(&paths, &target),
                        };
                        match result {
                            Ok(true) => {
                                self.set_notice(format!("scope removed: {target}"), Role::Success);
                            }
                            Ok(false) => {
                                self.set_notice(
                                    format!("scope did not contain {target}"),
                                    Role::Warning,
                                );
                            }
                            Err(error) => {
                                self.set_notice(
                                    format!("scope remove failed: {error}"),
                                    Role::Error,
                                );
                            }
                        }
                    }
                    "clear" => {
                        let result = match security.lock() {
                            Ok(mut state) => state.clear_scope(&paths),
                            Err(poison) => poison.into_inner().clear_scope(&paths),
                        };
                        match result {
                            Ok(()) => {
                                self.set_notice("scope cleared", Role::Success);
                            }
                            Err(error) => {
                                self.set_notice(
                                    format!("scope clear failed: {error}"),
                                    Role::Error,
                                );
                            }
                        }
                    }
                    other => {
                        self.set_notice(
                            format!("unknown scope action {other:?}; use add, remove, or clear"),
                            Role::Error,
                        );
                    }
                }
            }
            "report" => {
                let rendered = {
                    let mut state = match security.lock() {
                        Ok(state) => state,
                        Err(poison) => poison.into_inner(),
                    };
                    let name = state.engagement_name.clone();
                    let redact = state.redact;
                    match state.ensure_engagement(&paths) {
                        Ok(engagement) => Some((
                            name,
                            crate::security::render_report_with(engagement, redact),
                        )),
                        Err(error) => {
                            self.set_notice(
                                format!("cannot open engagement: {error}"),
                                Role::Error,
                            );
                            None
                        }
                    }
                };
                if let Some((name, markdown)) = rendered {
                    let path = paths.engagement_dir(&name).join("report.md");
                    let result = (|| -> std::io::Result<()> {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
                        std::fs::write(&path, markdown)?;
                        let _ =
                            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                        Ok(())
                    })();
                    match result {
                        Ok(()) => {
                            self.set_notice(
                                format!("report written to {}", path.display()),
                                Role::Success,
                            );
                        }
                        Err(error) => {
                            self.set_notice(format!("cannot write report: {error}"), Role::Error);
                        }
                    }
                }
            }
            _ => self.command_sec_usage(),
        }
    }

    fn command_sec_usage(&mut self) {
        self.transcript.push_note(
            "security",
            "usage:\n/sec  (pick from a list)\n/sec summary\n/sec scope add|remove <target>\n/sec scope clear\n/sec report",
            false,
        );
    }

    async fn command_trust(&mut self, rest: &str) {
        let trust = self.config.trust || self.options.trust;
        if !trust && rest.trim() == "confirm" {
            match self.accept_trust().await {
                Ok(()) => {
                    self.set_notice(
                        "trusted for this session: project config is loaded",
                        Role::Success,
                    );
                }
                Err(error) => {
                    self.set_notice(format!("cannot trust project: {error}"), Role::Error);
                }
            }
            return;
        }
        let text = if trust {
            "trusted: project .harness config, profiles, skills, workflows, and tools are loaded"
                .to_string()
        } else {
            format!(
                "untrusted: project .harness config, profiles, skills, workflows, and tools are ignored.\nType /trust confirm to load them for this session, or restart with --trust (LH_TRUST=1). The global config is always trusted.\nProject directory: {}",
                self.config_paths_project()
            )
        };
        self.transcript.push_note("trust", text, false);
        self.set_notice(
            if trust {
                "trusted"
            } else {
                "untrusted; /trust confirm loads project config"
            },
            if trust { Role::Success } else { Role::Warning },
        );
    }

    fn config_paths_project(&self) -> String {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.core.paths.project_dir().display().to_string())
            .unwrap_or_else(|| ".harness".into())
    }

    async fn accept_trust(&mut self) -> Result<()> {
        let Some(old) = self.core().cloned() else {
            bail!("no runtime");
        };
        let paths = old.paths.clone();
        let global = paths.config_file();
        let project = paths.project_dir().join("config.toml");
        let mut fresh = Config::load(&global, Some(&project))?;
        fresh.load_profile_dirs(
            &paths.global_profiles_dir(),
            Some(&paths.project_dir().join("profiles")),
        )?;
        let verify_path = paths.project_dir().join("verify.toml");
        if verify_path.exists() {
            fresh.verify = crate::config::VerifyConfig::load(&verify_path)?;
        }
        // Keep the runtime selections the user already made on the command line
        // or in this session.
        fresh.model.endpoint = self.config.model.endpoint.clone();
        fresh.model.model = self.config.model.model.clone();
        fresh.model.api_key = self.config.model.api_key.clone();
        fresh.model.context_window = self.config.model.context_window;
        fresh.profile_name = self.config.profile_name.clone();
        fresh.model_profile_override = self.config.model_profile_override.clone();
        fresh.ui.theme = self.config.ui.theme.clone();
        fresh.session = self.config.session.clone();
        fresh.trust = true;
        let core = app::rebuild_core(&old, fresh.clone()).await?;
        if let Some(runtime) = &mut self.runtime {
            runtime.core = core.clone();
        }
        self.config = core.config.clone();
        self.options.trust = true;
        self.options.skills =
            SkillIndex::load(&paths, true).advertised_for(&crate::tools::active_packs(&core.tools));
        self.header.trust = true;
        self.header.profile = self.config.profile_name().to_string();
        self.header.tool_mode = core.tool_mode_label().unwrap_or_default();
        self.window = core.effective_window().0;
        self.header.window = self.window;
        self.used_tokens = estimate_used(&core);
        self.header.used = self.used_tokens;
        Ok(())
    }

    fn command_export(&mut self, argument: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let (markdown, id) = match runtime.session.lock() {
            Ok(session) => (session.export_markdown(), session.id.clone()),
            Err(poison) => {
                let session = poison.into_inner();
                (session.export_markdown(), session.id.clone())
            }
        };
        let cwd = runtime.core.paths.cwd.clone();
        let path = if argument.is_empty() {
            cwd.join(format!("localharness-{id}.md"))
        } else {
            let requested = PathBuf::from(argument);
            if requested.is_absolute() {
                requested
            } else {
                cwd.join(requested)
            }
        };
        match std::fs::write(&path, markdown) {
            Ok(()) => {
                self.transcript
                    .push_note("export", format!("wrote {}", path.display()), false);
                self.set_notice(format!("exported {}", path.display()), Role::Success);
            }
            Err(error) => {
                self.set_notice(
                    format!("cannot write {}: {error}", path.display()),
                    Role::Error,
                );
            }
        }
    }

    fn command_clear(&mut self) {
        if let Some(runtime) = &self.runtime {
            let floor = match runtime.session.lock() {
                Ok(session) => session
                    .tree
                    .entries()
                    .last()
                    .map(|entry| entry.seq)
                    .unwrap_or(0),
                Err(poison) => poison
                    .into_inner()
                    .tree
                    .entries()
                    .last()
                    .map(|entry| entry.seq)
                    .unwrap_or(0),
            };
            self.floor_seq = floor;
        }
        self.transcript.clear_view();
        self.set_notice("transcript cleared", Role::Muted);
    }

    fn checkout_target(&mut self, target: &str, reason: &str) -> Result<(), String> {
        let result = {
            let Some(runtime) = &self.runtime else {
                return Err("no runtime".into());
            };
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let resolved = session
                .tree
                .resolve_checkout(target)
                .map_err(|error| error.to_string())?;
            session
                .checkout(&resolved, reason)
                .map_err(|error| error.to_string())?;
            let _ = session.fsync();
            Ok(())
        };
        if result.is_ok() {
            self.refresh_session_blocks();
        }
        result
    }

    async fn switch_session(&mut self, path: PathBuf) {
        self.open_session(Some(path)).await;
    }

    /// Replace the runtime with one on `target`, or on a fresh session when
    /// `target` is `None`. On failure the previous session is reopened.
    async fn open_session(&mut self, target: Option<PathBuf>) {
        if self.turn_active {
            self.set_notice(
                "cancel the running turn before switching sessions",
                Role::Warning,
            );
            return;
        }
        let Some(runtime) = &self.runtime else {
            return;
        };
        let config = self.config.clone();
        let paths = runtime.core.paths.clone();
        let path_jail = runtime.core.path_jail;
        let engagement = match runtime.core.security.lock() {
            Ok(security) => Some(security.engagement_name.clone()),
            Err(poison) => Some(poison.into_inner().engagement_name.clone()),
        };
        let old_resume = match runtime.session.lock() {
            Ok(session) => session.path.clone(),
            Err(poison) => poison.into_inner().path.clone(),
        };
        // An ephemeral (--no-session) run can start a new ephemeral session,
        // but cannot resume one from disk.
        let ephemeral = old_resume.is_none();
        if ephemeral && target.is_some() {
            self.set_notice("sessions are disabled (--no-session)", Role::Warning);
            return;
        }
        if let Some(runtime) = &self.runtime
            && let Ok(mut session) = runtime.session.lock()
        {
            let _ = session.fsync();
        }
        let old = self.runtime.take();
        drop(old);

        let options = RuntimeOptions {
            path_jail,
            engagement: engagement.clone(),
            no_session: ephemeral,
            no_redact: !config.session.redact,
            resume: target.clone(),
            continue_last: false,
        };
        match app::create_runtime(config.clone(), paths.clone(), options).await {
            Ok(runtime) => {
                self.runtime = Some(runtime);
                self.after_session_switch();
                let notice = match &target {
                    Some(path) => format!("session: {}", path.display()),
                    None => format!("new session {}", self.header.session),
                };
                self.set_notice(notice, Role::Success);
            }
            Err(error) => {
                self.set_notice(
                    format!("cannot switch session: {error}; reopening the previous session"),
                    Role::Error,
                );
                let options = RuntimeOptions {
                    path_jail,
                    engagement,
                    no_session: ephemeral,
                    no_redact: !config.session.redact,
                    resume: old_resume,
                    continue_last: false,
                };
                match app::create_runtime(config, paths, options).await {
                    Ok(runtime) => {
                        self.runtime = Some(runtime);
                        self.after_session_switch();
                    }
                    Err(recover_error) => {
                        self.runtime = None;
                        self.quit = true;
                        self.fatal = Some(format!(
                            "cannot switch session ({error}) and cannot reopen the previous session ({recover_error})"
                        ));
                    }
                }
            }
        }
    }

    fn after_session_switch(&mut self) {
        self.config = self
            .runtime
            .as_ref()
            .map(|runtime| runtime.core.config.clone())
            .unwrap_or_else(|| self.config.clone());
        self.floor_seq = 0;
        self.transcript = Transcript::new(self.config.ui.show_reasoning);
        self.update_from_core();
        self.refresh_session_blocks();
        let warnings = self
            .runtime
            .as_ref()
            .and_then(|runtime| {
                runtime
                    .session
                    .lock()
                    .ok()
                    .map(|session| session.warnings.clone())
            })
            .unwrap_or_default();
        for warning in warnings {
            self.transcript.push_note("session warning", warning, false);
        }
    }

    async fn shutdown(&mut self, _terminal: &mut Terminal<CrosstermBackend<Stdout>>) {
        if self.turn_active {
            self.turn_cancel.cancel();
            if let Some(core) = self.core() {
                core.cancel.cancel();
            }
            if let Some(mut handle) = self.agent_task.take()
                && tokio::time::timeout(Duration::from_secs(2), &mut handle)
                    .await
                    .is_err()
            {
                handle.abort();
            }
        }
        if let Some(runtime) = &self.runtime {
            let bg = runtime.core.bg.clone();
            let mut registry = {
                let mut guard = match bg.lock() {
                    Ok(registry) => registry,
                    Err(poison) => poison.into_inner(),
                };
                std::mem::take(&mut *guard)
            };
            registry.shutdown().await;
        }
        if let Some(runtime) = &self.runtime
            && let Ok(mut session) = runtime.session.lock()
        {
            let _ = session.fsync();
        }
    }
}

const LOGIN_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

fn normalize_endpoint(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("http://{trimmed}")
    }
}

async fn probe_endpoint_models(config: &crate::config::ModelConfig) -> Result<Vec<String>> {
    let provider = OpenAiCompatProvider::new(config, config.model.clone())?;
    tokio::time::timeout(LOGIN_PROBE_TIMEOUT, probe::list_models(&provider))
        .await
        .map_err(|_| {
            anyhow!(
                "listing models timed out after {}s",
                LOGIN_PROBE_TIMEOUT.as_secs()
            )
        })?
        .map_err(anyhow::Error::from)
}

fn login_status(core: &Core) -> String {
    let config = &core.config;
    format!(
        "status: {}\nendpoint: {}\nmodel: {}\napi key: {}\n\nusage:\n  /login <endpoint> [model] [api-key]\n  /login http://127.0.0.1:8080             auto-select one model\n  /login http://127.0.0.1:8080 <model>     choose a model\n  /login <endpoint> <model> -             clear the API key\n\nendpoint, model, and key are saved to {}.",
        if core.logged_in() {
            "logged in"
        } else {
            "not logged in"
        },
        config.model.endpoint,
        if core.logged_in() {
            core.provider.model()
        } else {
            "(not set)"
        },
        if config.model.api_key.is_empty() {
            "not set"
        } else {
            "set"
        },
        core.paths.config_file().display()
    )
}

fn estimate_used(core: &Arc<Core>) -> u32 {
    let session = match core.session.lock() {
        Ok(session) => session,
        Err(poison) => poison.into_inner(),
    };
    context::assemble(&core.system_prompt, &session, None, &core.budget).estimated_tokens
}

fn state_label(state: Option<AgentState>) -> &'static str {
    match state {
        Some(AgentState::AssembleContext) => "assembling context",
        Some(AgentState::Compact) => "compacting",
        Some(AgentState::StreamModel) => "streaming",
        Some(AgentState::ToolBatch) => "running tools",
        Some(AgentState::PostTool) => "post-tool",
        Some(AgentState::SettleGates) => "settling",
        Some(AgentState::Done) => "done",
        Some(AgentState::Cancelled) => "cancelled",
        Some(AgentState::Error) => "error",
        None => "idle",
    }
}

fn short_session_id(id: &str) -> String {
    if id == "ephemeral" {
        return id.to_string();
    }
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= 8 {
        return id.to_string();
    }
    chars[chars.len() - 8..].iter().collect()
}

fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

fn save_theme(path: &Path, theme: &str) -> Result<()> {
    let mut value = if path.exists() {
        toml::from_str::<toml::Value>(&std::fs::read_to_string(path)?)
            .context("existing config is not valid TOML")?
    } else {
        toml::Value::Table(Default::default())
    };
    let root = value
        .as_table_mut()
        .ok_or_else(|| anyhow!("config root is not a table"))?;
    let ui = root
        .entry("ui".to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let ui_table = ui
        .as_table_mut()
        .ok_or_else(|| anyhow!("[ui] is not a table"))?;
    ui_table.insert("theme".to_string(), toml::Value::String(theme.to_string()));
    let raw = toml::to_string_pretty(&value).context("cannot serialize config")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, raw)?;
    std::fs::rename(&temporary, path)?;
    Ok(())
}

fn available_theme_names(paths: &crate::paths::Paths) -> Vec<String> {
    let mut names: Vec<String> = Theme::names().iter().map(|name| name.to_string()).collect();
    if let Ok(entries) = std::fs::read_dir(paths.themes_dir()) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && !names.iter().any(|name| name == stem)
            {
                names.push(stem.to_string());
            }
        }
    }
    names.sort();
    names
}

/// Load an installable workflow TOML and its prompt, resolved relative to the
/// file. Errors are the prescriptive messages `/workflow` has always shown.
fn load_workflow_file(path: &Path) -> std::result::Result<WorkflowSpec, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let workflow: WorkflowFile = toml::from_str(&raw)
        .map_err(|error| format!("invalid workflow {}: {error}", path.display()))?;
    let Some(prompt_ref) = workflow.prompt.as_deref() else {
        return Err(format!("workflow {} has no prompt file", path.display()));
    };
    let prompt_path = if Path::new(prompt_ref).is_absolute() {
        PathBuf::from(prompt_ref)
    } else {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(prompt_ref)
    };
    let prompt = std::fs::read_to_string(&prompt_path).map_err(|error| {
        format!(
            "cannot read workflow prompt {}: {error}",
            prompt_path.display()
        )
    })?;
    let name = workflow.name.unwrap_or_else(|| {
        path.file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .unwrap_or_else(|| "workflow".to_string())
    });
    Ok(WorkflowSpec {
        name,
        description: workflow.description,
        profile: workflow.profile,
        prompt,
        skills: workflow.skills,
        tools: workflow.tools,
    })
}

/// Picker rows for the endpoint's models.
fn model_items(models: &[String]) -> Vec<PickItem> {
    models
        .iter()
        .map(|model| PickItem::run("model", model.clone(), ""))
        .collect()
}

/// Skills the active tools advertise, with their descriptions.
fn skill_choices(
    paths: &crate::paths::Paths,
    trust: bool,
    tools: &[Arc<dyn crate::tools::Tool>],
) -> Vec<(String, String)> {
    let index = SkillIndex::load(paths, trust);
    let advertised = index.advertised_for(&crate::tools::active_packs(tools));
    advertised
        .names()
        .into_iter()
        .map(|name| {
            let description = advertised
                .get(name)
                .map(|skill| skill.description.clone())
                .unwrap_or_default();
            (name.to_string(), description)
        })
        .collect()
}

/// One-line description of a bundled workflow (file workflows are only read
/// when run).
fn workflow_description(name: &str) -> String {
    workflows::get(name)
        .and_then(|workflow| workflow.description.clone())
        .unwrap_or_default()
}

fn available_workflows(paths: &crate::paths::Paths, trust: bool) -> Vec<String> {
    let mut names = Vec::new();
    let mut scan = |dir: &Path| {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|ext| ext.to_str()) == Some("toml")
                    && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                    && !names.iter().any(|name| name == stem)
                {
                    names.push(stem.to_string());
                }
            }
        }
    };
    scan(&paths.workflows_dir());
    if trust {
        scan(&paths.project_dir().join("workflows"));
    }
    for name in workflows::names() {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_string());
        }
    }
    names.sort();
    names
}

fn scan_sessions(paths: &crate::paths::Paths) -> Vec<SessionItem> {
    let directory = paths.project_sessions_dir();
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return Vec::new();
    };
    let mut found: Vec<(SystemTime, SessionItem)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let preview = first_user_preview(&path);
        let when = chrono::DateTime::<chrono::Local>::from(modified)
            .format("%Y-%m-%d %H:%M")
            .to_string();
        found.push((
            modified,
            SessionItem {
                id: id.to_string(),
                preview,
                when,
                path,
            },
        ));
    }
    found.sort_by_key(|item| std::cmp::Reverse(item.0));
    found.into_iter().map(|(_, item)| item).collect()
}

/// Resolve `/resume <id>`: an exact id, else a unique id prefix or suffix (the
/// header shows the last 8 characters).
fn match_session(items: &[SessionItem], needle: &str) -> Result<PathBuf, String> {
    if let Some(item) = items.iter().find(|item| item.id == needle) {
        return Ok(item.path.clone());
    }
    let matches: Vec<&SessionItem> = items
        .iter()
        .filter(|item| item.id.starts_with(needle) || item.id.ends_with(needle))
        .collect();
    match matches.as_slice() {
        [only] => Ok(only.path.clone()),
        [] => Err(format!(
            "no session id matches {needle:?}.\nRun /resume to pick from a list, or /resume last."
        )),
        many => Err(format!(
            "{needle:?} matches {} sessions:\n{}\n\nUse a longer id.",
            many.len(),
            many.iter()
                .map(|item| format!("  {}  {}  {}", item.id, item.when, item.preview))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

fn first_user_preview(path: &Path) -> String {
    let Ok(file) = std::fs::File::open(path) else {
        return "(unreadable)".into();
    };
    for line in BufReader::new(file).lines().take(500).flatten() {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Entry>(&line)
            && let EntryKind::User(data) = entry.kind
        {
            let text: String = data
                .content
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            return clip_end(&text, 80);
        }
    }
    "(no messages)".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui_textarea::TextArea;

    #[test]
    fn bundled_workflows_are_discoverable() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = crate::paths::Paths::discover(dir.path()).unwrap();
        paths.config_dir = dir.path().join("config");
        let names = available_workflows(&paths, false);
        for expected in [
            "api-security",
            "coding",
            "general-assistance",
            "native-code-review",
            "security-blue",
            "security-red",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "missing {expected}: {names:?}"
            );
        }
    }

    fn header(model: &str, session: &str) -> HeaderData {
        HeaderData {
            cwd: "/tmp/project".into(),
            git: GitStatus::none(),
            profile: "full".into(),
            model: model.into(),
            tool_mode: String::new(),
            logged_in: true,
            used: 12400,
            window: 64000,
            completion: 0,
            tok_s: 24.0,
            cache: Some(91),
            session: session.into(),
            trust: true,
            window_guess: false,
        }
    }

    fn draw_scene(
        width: u16,
        height: u16,
        header: HeaderData,
        theme: Theme,
    ) -> Terminal<TestBackend> {
        let mut transcript = Transcript::new(false);
        transcript.set_viewport(height as usize);
        transcript.prepare(width as usize, &theme, false);
        let input = TextArea::new(vec![String::new()]);
        let footer = FooterData {
            hints: "Enter send".into(),
            status: String::new(),
            status_role: Role::Muted,
            notice: None,
            notice_role: Role::Warning,
        };
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| {
                let scene = render::Scene {
                    header: header.clone(),
                    transcript: &transcript,
                    input: &input,
                    debug_lines: Vec::new(),
                    footer: footer.clone(),
                    completions: Vec::new(),
                    completion_index: 0,
                    completion_title: "",
                    modal: None,
                    activity: None,
                    plan: Vec::new(),
                    busy: false,
                };
                render::draw(frame, &scene, &theme);
            })
            .unwrap();
        terminal
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        let buffer = terminal.backend().buffer();
        let area = *buffer.area();
        let mut out = String::new();
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    out.push_str(cell.symbol());
                }
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn header_at_120_contains_cwd_model_and_context() {
        let terminal = draw_scene(
            120,
            40,
            header("qwen3.5-35b-a3b", "abcdef12"),
            Theme::bundled("default"),
        );
        let text = buffer_text(&terminal);
        assert!(text.contains("/tmp/project"), "cwd missing:\n{text}");
        assert!(text.contains("qwen3.5-35b-a3b"), "model missing:\n{text}");
        assert!(text.contains("12.4k/64k"), "context missing:\n{text}");
        assert!(text.contains("tok/s"));
        assert!(text.contains("▰"), "context gauge missing:\n{text}");
    }

    #[test]
    fn narrow_header_drops_throughput_session_and_profile_but_keeps_required() {
        let terminal = draw_scene(
            60,
            20,
            header("qwen3.5-35b-a3b", "abcdef123456"),
            Theme::bundled("default"),
        );
        let text = buffer_text(&terminal);
        assert!(text.contains("/tmp/project"), "cwd missing:\n{text}");
        assert!(text.contains("qwen3.5-35b-a3b"), "model missing:\n{text}");
        assert!(text.contains("12.4k/64k"), "context missing:\n{text}");
        assert!(
            !text.contains("tok/s"),
            "throughput should be dropped:\n{text}"
        );
        assert!(
            !text.contains("abcdef123456"),
            "session id should be dropped:\n{text}"
        );
        assert!(!text.contains("full"), "profile should be dropped:\n{text}");
    }

    #[test]
    fn theme_switch_changes_header_cell_style() {
        let before = draw_scene(
            120,
            20,
            header("model", "session"),
            Theme::bundled("default"),
        );
        let after = draw_scene(120, 20, header("model", "session"), Theme::bundled("light"));
        // The info bar is the second-to-last row; column 1 starts the cwd.
        let before_cell = before.backend().buffer().cell((1, 18)).unwrap();
        let after_cell = after.backend().buffer().cell((1, 18)).unwrap();
        assert_eq!(before_cell.symbol(), "/");
        assert_eq!(after_cell.symbol(), "/");
        assert_ne!(
            before_cell.style().fg,
            after_cell.style().fg,
            "theme switch should recolor the header"
        );
    }

    #[test]
    fn header_shows_not_logged_in_without_a_model() {
        let mut header = header("", "session");
        header.logged_in = false;
        let terminal = draw_scene(120, 20, header, Theme::bundled("default"));
        let text = buffer_text(&terminal);
        assert!(text.contains("not logged in"), "header:\n{text}");
    }

    async fn model_list(
        axum::extract::State(models): axum::extract::State<Arc<Vec<String>>>,
    ) -> axum::response::Response {
        let data: Vec<serde_json::Value> = models
            .iter()
            .map(|id| serde_json::json!({"id": id, "object": "model"}))
            .collect();
        let body = serde_json::json!({"object": "list", "data": data}).to_string();
        axum::response::Response::builder()
            .status(axum::http::StatusCode::OK)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .unwrap()
    }

    async fn model_endpoint(models: &[&str]) -> String {
        let state: Arc<Vec<String>> = Arc::new(models.iter().map(|m| m.to_string()).collect());
        let app = axum::Router::new()
            .route("/v1/models", axum::routing::get(model_list))
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    /// A `/v1/models` endpoint whose entries advertise `max_model_len`, like vLLM.
    async fn sized_model_endpoint(models: &[(&str, u64)]) -> String {
        let data: Vec<serde_json::Value> = models
            .iter()
            .map(|(id, ctx)| serde_json::json!({"id": id, "object": "model", "max_model_len": ctx}))
            .collect();
        let body = Arc::new(serde_json::json!({"object": "list", "data": data}).to_string());
        let app = axum::Router::new().route(
            "/v1/models",
            axum::routing::get(move || {
                let body = body.clone();
                async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body.as_ref().clone(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn switching_models_reprobes_the_context_window() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let paths = crate::paths::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            cwd,
            project_id: "test".into(),
            sessions_dir_override: None,
        };
        let base = sized_model_endpoint(&[("small", 8192), ("big", 131072)]).await;
        let mut config = Config::builtin();
        config.model.endpoint = base;
        config.model.model = "small".into();
        let runtime = app::create_runtime(
            config,
            paths,
            RuntimeOptions {
                no_session: true,
                ..RuntimeOptions::default()
            },
        )
        .await
        .unwrap();
        let mut app = App::new(
            runtime,
            UiOptions {
                no_color: true,
                trust: false,
                skills: SkillIndex::bundled(),
            },
        )
        .unwrap();
        assert_eq!(app.header.window, 8192);
        // The session header records all four context values.
        let header = app
            .core()
            .unwrap()
            .session
            .lock()
            .unwrap()
            .header()
            .cloned()
            .expect("session header");
        let context = header.context.expect("context recorded");
        assert_eq!(context.allocated, Some(8192));
        assert_eq!(context.effective, 8192);
        // The probe result is not written back as if the user configured it.
        assert_eq!(app.config.model.context_window, None);

        app.switch_model("big").await;
        // The rebuild runs in the background; the TUI stays live meanwhile.
        assert!(app.switching.is_some());
        assert!(app.activity().is_some_and(|a| a.label.contains("big")));
        app.settle().await;
        assert!(app.switching.is_none());
        assert_eq!(app.header.model, "big");
        assert_eq!(app.header.window, 131072);
        assert_eq!(app.core().unwrap().budget.context_window, 131072);

        // An explicit value is clamped to what the server allocated.
        let mut pinned = app.config.clone();
        pinned.model.context_window = Some(20_000);
        pinned.model.model = "small".into();
        app.rebuild_core_with(pinned).await.unwrap();
        assert_eq!(app.header.window, 8192);
        assert_eq!(app.core().unwrap().budget.context_window, 8192);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn picker_filters_ranks_and_clamps_selection() {
        let models: Vec<String> = vec![
            "openai/gpt-x".into(),
            "anthropic/claude".into(),
            "local/qwen3".into(),
        ];
        let mut view = PickerModal::new("model", "Models", model_items(&models), "local/qwen3")
            .with_subtitle("http://gw");
        assert_eq!(view.selected_label().as_deref(), Some("local/qwen3"));
        view.push_filter('A');
        view.push_filter('n');
        let labels: Vec<&str> = view.filtered().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["anthropic/claude"]);
        view.move_by(5);
        assert_eq!(
            view.selected_item().map(|item| item.action.clone()),
            Some(PickAction::Run("/model anthropic/claude".into()))
        );
        view.push_filter('z');
        assert!(view.selected_item().is_none());
        view.pop_filter();
        view.pop_filter();
        view.pop_filter();
        assert_eq!(view.filtered().len(), 3);

        // Label prefixes rank above substrings and description matches.
        let mut view = PickerModal::new(
            "x",
            "X",
            vec![
                PickItem::run("x", "dark-blue", ""),
                PickItem::run("x", "plain", "a dark look"),
                PickItem::run("x", "darker", ""),
            ],
            "",
        );
        view.push_filter('d');
        view.push_filter('a');
        view.push_filter('r');
        let labels: Vec<&str> = view.filtered().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["dark-blue", "darker", "plain"]);

        // A refresh keeps the highlighted row.
        view.move_by(1);
        view.replace_items(vec![
            PickItem::run("x", "darker", ""),
            PickItem::run("x", "dark-blue", ""),
        ]);
        assert_eq!(view.selected_label().as_deref(), Some("darker"));
    }

    #[test]
    fn match_session_by_prefix_suffix_and_ambiguity() {
        let item = |id: &str| SessionItem {
            id: id.into(),
            preview: String::new(),
            when: String::new(),
            path: PathBuf::from(format!("{id}.jsonl")),
        };
        let items = vec![item("20261108-aaaa1111"), item("20261107-bbbb2222")];
        assert_eq!(
            match_session(&items, "aaaa1111").unwrap(),
            PathBuf::from("20261108-aaaa1111.jsonl")
        );
        assert_eq!(
            match_session(&items, "20261107").unwrap(),
            PathBuf::from("20261107-bbbb2222.jsonl")
        );
        assert!(
            match_session(&items, "2026")
                .unwrap_err()
                .contains("matches 2 sessions")
        );
        assert!(match_session(&items, "zzz").is_err());
    }

    #[tokio::test]
    async fn tab_completes_commands_arguments_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(cwd.join("src/ui")).unwrap();
        std::fs::write(cwd.join("src/ui/input.rs"), "").unwrap();
        std::fs::write(cwd.join("src/main.rs"), "").unwrap();
        let paths = crate::paths::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            cwd: cwd.clone(),
            project_id: "test".into(),
            sessions_dir_override: None,
        };
        let base = model_endpoint(&["m"]).await;
        let mut config = Config::builtin();
        config.model.endpoint = base;
        config.model.model = "m".into();
        let runtime = app::create_runtime(config, paths.clone(), RuntimeOptions::default())
            .await
            .unwrap();
        let options = || UiOptions {
            no_color: true,
            trust: false,
            skills: SkillIndex::bundled(),
        };
        let mut app = App::new(runtime, options()).unwrap();
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
        let back_tab = KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT);

        // A unique command completes with a separating space.
        app.input.set_text("/them");
        app.handle_key(tab).await;
        assert_eq!(app.input.text(), "/theme ");

        // Arguments complete from the theme list.
        app.input.set_text("/theme gru");
        app.handle_key(tab).await;
        assert_eq!(app.input.text(), "/theme gruvbox-dark ");
        // No match inside a command: Tab inserts nothing.
        app.input.set_text("/theme zzz");
        app.handle_key(tab).await;
        assert_eq!(app.input.text(), "/theme zzz");

        // Ambiguous commands: Shift+Tab moves the selection back, Tab accepts.
        app.input.set_text("/tr");
        app.handle_key(back_tab).await;
        app.handle_key(tab).await;
        assert_eq!(app.input.text(), "/trust ");

        // Enter on a partial argument runs the highlighted completion.
        app.input.set_text("/theme high");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await;
        assert_eq!(app.theme.name, "high-contrast");

        // Paths: the first Tab extends to the shared prefix, the popup lists
        // the rest, and a unique file is accepted.
        app.input.set_text("explain @sr");
        app.handle_key(tab).await;
        assert_eq!(app.input.text(), "explain @src/");
        assert!(app.current_completion().is_none());
        app.handle_key(tab).await;
        let completion = app.current_completion().expect("path popup");
        assert_eq!(completion.candidates[0].value, "src/ui/");
        app.input.set_text("explain @src/m");
        app.handle_key(tab).await;
        assert_eq!(app.input.text(), "explain @src/main.rs ");

        // Plain prose: Tab falls through to the textarea.
        app.input.set_text("hello");
        app.handle_key(tab).await;
        assert!(app.input.text().starts_with("hello"));

        // Prompt history survives a restart for the same project.
        app.input.set_text("remember me");
        app.input.submit(true).unwrap();
        drop(app);
        let runtime = app::create_runtime(
            {
                let mut config = Config::builtin();
                config.model.endpoint = model_endpoint(&["m"]).await;
                config.model.model = "m".into();
                config
            },
            paths,
            RuntimeOptions::default(),
        )
        .await
        .unwrap();
        let mut app = App::new(runtime, options()).unwrap();
        assert!(app.input.history_prev());
        assert_eq!(app.input.text(), "remember me");
    }

    async fn test_app() -> (tempfile::TempDir, App) {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let paths = crate::paths::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            cwd,
            project_id: "test".into(),
            sessions_dir_override: None,
        };
        let mut config = Config::builtin();
        config.model.endpoint = model_endpoint(&["m"]).await;
        config.model.model = "m".into();
        let runtime = app::create_runtime(
            config,
            paths,
            RuntimeOptions {
                no_session: true,
                ..RuntimeOptions::default()
            },
        )
        .await
        .unwrap();
        let app = App::new(
            runtime,
            UiOptions {
                no_color: true,
                trust: false,
                skills: SkillIndex::bundled(),
            },
        )
        .unwrap();
        (dir, app)
    }

    #[tokio::test]
    async fn list_commands_open_pickers_and_pick_without_typing() {
        let (_dir, mut app) = test_app().await;
        let enter = key(KeyCode::Enter);

        // /theme previews as the highlight moves; Esc restores.
        let original = app.theme.name.clone();
        app.input.set_text("/theme");
        app.handle_key(enter).await;
        assert!(matches!(&app.modal, Some(ModalData::Picker(v)) if v.command == "theme"));
        app.handle_modal_key(key(KeyCode::Down)).await;
        assert_ne!(app.theme.name, original, "moving previews the theme");
        app.handle_modal_key(key(KeyCode::Esc)).await;
        assert!(app.modal.is_none());
        assert_eq!(app.theme.name, original, "Esc restores the theme");

        // Filtering and Enter applies the pick.
        app.command_theme("");
        for ch in "gruvbox-d".chars() {
            app.handle_modal_key(key(KeyCode::Char(ch))).await;
        }
        app.handle_modal_key(enter).await;
        assert!(app.modal.is_none());
        assert_eq!(app.theme.name, "gruvbox-dark");

        // /profile lists profiles with the current one preselected.
        app.command_profile("").await;
        match &app.modal {
            Some(ModalData::Picker(view)) => {
                assert!(view.items.len() > 1);
                assert_eq!(
                    view.selected_label().as_deref(),
                    Some(app.config.profile_name())
                );
            }
            _ => panic!("expected the profile picker"),
        }
        app.modal = None;

        // /sec: summary runs directly; scope add fills the prompt.
        app.command_sec("");
        for ch in "scope add".chars() {
            app.handle_modal_key(key(KeyCode::Char(ch))).await;
        }
        app.handle_modal_key(enter).await;
        assert!(app.modal.is_none());
        assert_eq!(app.input.text(), "/sec scope add ");
        app.input.set_text("");

        // /skill lists the skills the profile's packs advertise.
        let mut full = app.config.clone();
        full.profile_name = "full".into();
        app.rebuild_core_with(full).await.unwrap();
        app.command_skill("");
        match &app.modal {
            Some(ModalData::Picker(view)) => {
                assert!(!view.items.is_empty());
                assert!(view.items.iter().all(|item| item.action
                    == PickAction::Run(format!("/skill {}", item.label))
                    && !item.description.is_empty()));
            }
            _ => panic!("expected the skill picker"),
        }
        app.modal = None;

        // /help <command> explains one command.
        app.command_help("theme");
        app.transcript.prepare(120, &app.theme, false);
        let notes = format!("{:?}", app.transcript.lines());
        assert!(notes.contains("live preview"), "{notes}");

        // Esc abandons a half-typed command.
        app.input.set_text("/the");
        app.handle_key(key(KeyCode::Esc)).await;
        assert!(app.input.is_empty());

        // Idle ticks do not ask for frames; a running switch animates.
        app.notice = None;
        app.transcript.prepare(80, &app.theme, false);
        assert!(!app.on_tick());
    }

    #[tokio::test]
    async fn new_and_resume_switch_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let paths = crate::paths::Paths {
            config_dir: dir.path().join("config"),
            data_dir: dir.path().join("data"),
            cwd,
            project_id: "test".into(),
            sessions_dir_override: None,
        };
        let base = model_endpoint(&["m"]).await;
        let mut config = Config::builtin();
        config.model.endpoint = base;
        config.model.model = "m".into();
        let runtime = app::create_runtime(config, paths, RuntimeOptions::default())
            .await
            .unwrap();
        let mut app = App::new(
            runtime,
            UiOptions {
                no_color: true,
                trust: false,
                skills: SkillIndex::bundled(),
            },
        )
        .unwrap();
        let session_path = |app: &App| {
            app.runtime
                .as_ref()
                .unwrap()
                .session
                .lock()
                .unwrap()
                .path
                .clone()
                .unwrap()
        };
        let first = session_path(&app);

        // /new on an untouched session does not litter another file.
        app.command_new().await;
        assert_eq!(session_path(&app), first);

        app.runtime
            .as_ref()
            .unwrap()
            .session
            .lock()
            .unwrap()
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "first session message".into(),
            }))
            .unwrap();
        app.command_new().await;
        let second = session_path(&app);
        assert_ne!(second, first);

        // /resume lists only the other sessions; /resume last reopens the first.
        app.command_resume("").await;
        match &app.modal {
            Some(ModalData::Sessions(view)) => {
                assert_eq!(view.items.len(), 1);
                assert_eq!(view.items[0].path, first);
                assert_eq!(view.items[0].preview, "first session message");
            }
            _ => panic!("expected the sessions picker"),
        }
        app.modal = None;
        app.command_resume("last").await;
        assert_eq!(session_path(&app), first);
    }

    #[tokio::test]
    async fn login_configures_a_model_and_model_command_lists_choices() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let config_dir = dir.path().join("config");
        let config_path = config_dir.join("config.toml");
        let paths = crate::paths::Paths {
            config_dir,
            data_dir: dir.path().join("data"),
            cwd,
            project_id: "test".into(),
            sessions_dir_override: None,
        };

        // Reserve a port and close it so the initial endpoint is unreachable.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let down = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let mut config = Config::builtin();
        config.model.endpoint = down;
        config.model.model.clear();
        let runtime = app::create_runtime(
            config,
            paths,
            RuntimeOptions {
                no_session: true,
                ..RuntimeOptions::default()
            },
        )
        .await
        .unwrap();
        let mut app = App::new(
            runtime,
            UiOptions {
                no_color: true,
                trust: false,
                skills: SkillIndex::bundled(),
            },
        )
        .unwrap();
        assert!(!app.header.logged_in);

        app.start_turn("hello".into());
        assert!(!app.turn_active, "turns must be blocked before login");

        // /login validates the endpoint, selects the named model, and saves.
        let base = model_endpoint(&["alpha", "beta"]).await;
        app.command_login(&format!("{base} beta")).await;
        assert!(app.header.logged_in);
        assert_eq!(app.header.model, "beta");
        assert_eq!(app.config.model.endpoint, base);

        // /model with no argument opens a picker over the endpoint's models,
        // with the current model preselected.
        app.command_model("").await;
        match &app.modal {
            Some(ModalData::Picker(view)) => {
                let labels: Vec<&str> = view.items.iter().map(|i| i.label.as_str()).collect();
                assert_eq!(labels, vec!["alpha", "beta"]);
                assert_eq!(view.selected_label().as_deref(), Some("beta"));
            }
            _ => panic!("expected the models picker"),
        }
        // Typing filters; Enter switches the session's model.
        for ch in "alp".chars() {
            app.handle_modal_key(key(KeyCode::Char(ch))).await;
        }
        app.handle_modal_key(key(KeyCode::Enter)).await;
        assert!(app.modal.is_none());
        app.settle().await;
        assert_eq!(app.header.model, "alpha");
        // The picker switch lasts for the session; the saved config keeps beta.
        let saved = std::fs::read_to_string(&config_path).unwrap();
        assert!(saved.contains("beta"), "config:\n{saved}");

        // A login without a model auto-selects the endpoint's only model and
        // replaces the previous endpoint's selection.
        let single = model_endpoint(&["only-model"]).await;
        app.command_login(&single).await;
        assert_eq!(app.header.model, "only-model");
        let saved = std::fs::read_to_string(&config_path).unwrap();
        let parsed: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(parsed["model"]["endpoint"].as_str(), Some(single.as_str()));
        assert_eq!(parsed["model"]["model"].as_str(), Some("only-model"));
    }
}

//! Interactive TUI event loop, slash commands, and terminal lifecycle.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, BufWriter, Stdout};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context as _, Result, anyhow, bail};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
    LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;
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
mod commands;
mod keys;
mod support;
mod terminal;
#[cfg(test)]
mod tests;

use support::*;
use terminal::*;

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
Ctrl+L redraw the screen (if it ever looks garbled)
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

pub async fn run(runtime: Runtime, options: UiOptions) -> Result<()> {
    let mut app = App::new(runtime, options)?;
    let _guard = TerminalGuard::enter(app.config.ui.mouse)?;
    let backend = CrosstermBackend::new(BufWriter::with_capacity(OUTPUT_BUFFER, std::io::stdout()));
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
            // A closed channel disables the branch instead of spinning.
            Some(status) = git_rx.recv() => {
                if app.apply_git(status) {
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
    /// Clear the screen and repaint every cell on the next frame.
    repaint: bool,
    /// The last frame as drawn, for tests that check the screen.
    #[cfg(test)]
    last_frame: Option<ratatui::buffer::Buffer>,
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
            repaint: false,
            #[cfg(test)]
            last_frame: None,
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

    /// Returns whether the header changed (the watcher reports every few
    /// seconds whether or not anything moved).
    fn apply_git(&mut self, status: GitStatus) -> bool {
        if self.header.git == status {
            return false;
        }
        self.header.git = status;
        true
    }

    fn draw<B>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B: ratatui::backend::Backend + std::io::Write,
        B::Error: Send + Sync + 'static,
    {
        // Terminals that support synchronized output (DEC 2026) show the
        // whole frame at once instead of painting it as bytes arrive; others
        // ignore the markers. The opening marker is flushed right away so
        // nothing stays buffered if rendering panics (the restore path ends
        // the update before leaving the alternate screen).
        let _ = execute!(terminal.backend_mut(), BeginSynchronizedUpdate);
        let drawn = self.draw_frame(terminal);
        let _ = execute!(terminal.backend_mut(), EndSynchronizedUpdate);
        drawn
    }

    fn draw_frame<B>(&mut self, terminal: &mut Terminal<B>) -> Result<()>
    where
        B: ratatui::backend::Backend + std::io::Write,
        B::Error: Send + Sync + 'static,
    {
        // Pick up a size change now, so the layout below matches the frame.
        terminal
            .autoresize()
            .context("cannot read the terminal size")?;
        let size = terminal.get_frame().area();
        if std::mem::take(&mut self.repaint) {
            // Forget what ratatui believes is on screen and clear it, so the
            // next frame rewrites every cell. Recovers from anything that
            // drew behind our back. (`Terminal::clear` would query the
            // cursor position, which stalls on terminals that never answer.)
            terminal
                .resize(Rect::new(0, 0, size.width, size.height))
                .context("cannot repaint the terminal")?;
        }
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

        let _completed = terminal
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
        #[cfg(test)]
        {
            self.last_frame = Some(_completed.buffer.clone());
        }
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
                // An exact error (for example `budget_static`) is the notice;
                // the generic status text below must not replace it.
                let has_error = outcome.error.is_some();
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
                    TurnStatus::Budget if has_error => {}
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

    async fn shutdown(&mut self, _terminal: &mut Tty) {
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

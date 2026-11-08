//! Interactive TUI event loop, slash commands, and terminal lifecycle.

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
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::Rect;
use serde::Deserialize;
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::task::JoinHandle;

use crate::agent::{AgentEvent, AgentState, Core, TurnOutcome, TurnStatus};
use crate::app::{self, Runtime, RuntimeOptions};
use crate::config::Config;
use crate::context;
use crate::git::{self, GitStatus};
use crate::session::{CompactionData, Entry, EntryKind, UserData};
use crate::skills::SkillIndex;
use crate::ui::UiOptions;
use crate::ui::input::InputState;
use crate::ui::render::{self, Transcript};
use crate::ui::theme::{Role, Theme};
use crate::ui::tree::TreeView;
use crate::ui::widgets::{
    FooterData, HeaderData, ModalData, SessionItem, SessionsModal, ToolItem, ToolsModal, clip_end,
};

/// Slash commands with one-line descriptions for the completion popup.
pub const COMMANDS: &[(&str, &str)] = &[
    ("help", "list commands and key bindings"),
    ("quit", "exit LocalHarness"),
    ("model", "set the model id and rebuild the core"),
    ("profile", "switch the tool profile"),
    ("theme", "list, apply, or save a theme"),
    ("tree", "browse and checkout the session tree"),
    ("fork", "checkout the latest assistant step"),
    ("sessions", "list and switch sessions"),
    ("compact", "append a deterministic compaction"),
    ("tools", "show the active tools"),
    ("skill", "append a skill's instructions"),
    ("workflow", "start a workflow on a new branch"),
    ("sec", "engagement scope, summary, or report"),
    ("trust", "show the trust state"),
    ("export", "write the branch as Markdown"),
    ("clear", "clear the transcript view"),
    ("debug", "toggle the debug panel"),
];

const HELP: &str = "\
Commands: /help /quit /model <name> /profile <name> /theme [name|save] /tree /fork
/sessions /compact /tools /skill <name> /workflow <name> /sec ... /trust [confirm]
/export [path] /clear /debug

Keys: Enter send · Ctrl+J newline · Esc cancel · Ctrl+R reasoning
PgUp/PgDn scroll · Home/End scroll when the prompt is empty · Ctrl+C quit";

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
        let _ = execute!(out, LeaveAlternateScreen, DisableBracketedPaste);
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        install_panic_hook();
        enable_raw_mode().context("cannot enable raw mode")?;
        let mut out = std::io::stdout();
        if let Err(error) = execute!(out, EnterAlternateScreen, EnableBracketedPaste) {
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
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend).context("cannot initialize the terminal")?;
    // Some terminals (and piped PTYs) cannot answer a cursor-position query;
    // a failed clear must not abort the session.
    let _ = terminal.clear();

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

    let mut ticker = tokio::time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok();
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();

    while !app.quit {
        app.draw(&mut terminal)?;

        let mut agent_rx = app.agent_rx.take();
        tokio::select! {
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
                if !app.turn_active {
                    agent_rx = None;
                }
            }
            maybe = event_rx.recv() => {
                match maybe {
                    Some(ev) => app.handle_event(ev).await,
                    None => app.quit = true,
                }
            }
            _ = ticker.tick() => app.on_tick(),
            maybe = git_rx.recv() => {
                if let Some(status) = maybe {
                    app.apply_git(status);
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
    token_events: VecDeque<(Instant, u64)>,
    tok_s: f32,
    used_tokens: u32,
    completion_tokens: u32,
    window: u32,
    debug: bool,
    floor_seq: u64,
    spinner: usize,
    last_width: usize,
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
        let used = estimate_used(&runtime.core);
        let window = runtime.core.budget.context_window;
        let trust = config.trust || options.trust;
        let header = HeaderData {
            cwd: paths.cwd.display().to_string(),
            git: GitStatus::none(),
            profile: config.profile_name().to_string(),
            model: runtime.core.provider.model().to_string(),
            used,
            window,
            completion: 0,
            tok_s: 0.0,
            session: short_session_id(&session_id),
            trust,
        };
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
            token_events: VecDeque::new(),
            tok_s: 0.0,
            used_tokens: used,
            completion_tokens: 0,
            window,
            debug: false,
            floor_seq: 0,
            spinner: 0,
            last_width: 0,
        };
        app.no_color = app.options.no_color;
        app.transcript = Transcript::new(app.config.ui.show_reasoning);
        app.refresh_session_blocks();
        for warning in warnings {
            app.transcript.push_note("session warning", warning, false);
        }
        if !app.header.trust {
            app.set_notice(
                "untrusted: project .harness config, skills, workflows, and tools are ignored (restart with --trust)",
                Role::Warning,
            );
        }
        Ok(app)
    }

    fn core(&self) -> Option<&Arc<Core>> {
        self.runtime.as_ref().map(|runtime| &runtime.core)
    }

    fn cwd_path(&self) -> PathBuf {
        self.core()
            .map(|core| core.paths.cwd.clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn set_notice(&mut self, text: impl Into<String>, role: Role) {
        self.notice = Some((text.into(), role, Instant::now()));
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
            render::blocks_from_session_after(&session, self.floor_seq)
        };
        self.transcript.set_session(blocks);
        self.used_tokens = estimate_used(&runtime.core);
        self.header.used = self.used_tokens;
        self.header.window = self.window;
        self.header.completion = self.completion_tokens;
    }

    fn update_from_core(&mut self) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let config = runtime.core.config.clone();
        self.config = config;
        self.header.model = runtime.core.provider.model().to_string();
        self.header.profile = self.config.profile_name().to_string();
        self.header.trust = self.config.trust || self.options.trust;
        self.window = runtime.core.budget.context_window;
        self.header.window = self.window;
        self.header.cwd = runtime.core.paths.cwd.display().to_string();
        if let Ok(session) = runtime.session.lock() {
            self.header.session = short_session_id(&session.id);
        }
        self.used_tokens = estimate_used(&runtime.core);
        self.header.used = self.used_tokens;
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
        let rects = render::split(
            Rect::new(0, 0, size.width, size.height),
            self.input.textarea.lines().len(),
            debug_lines.len(),
        );
        self.transcript
            .set_viewport(rects.transcript.height as usize);
        let show_reasoning = self.config.ui.show_reasoning;
        self.transcript
            .prepare(self.last_width, &self.theme, show_reasoning, false);
        self.input.apply_theme(&self.theme);

        let completions = self.input.completions(COMMANDS);
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
                    modal,
                };
                render::draw(frame, &scene, &self.theme);
            })
            .context("cannot draw the TUI")?;
        Ok(())
    }

    fn footer(&self) -> FooterData {
        let (status, status_role) = if self.cancelling {
            ("cancelling".to_string(), Role::Warning)
        } else if self.turn_active {
            (
                format!("streaming · {}", state_label(self.turn_state)),
                Role::Accent,
            )
        } else if self.header.trust {
            ("trusted".to_string(), Role::Success)
        } else {
            ("untrusted".to_string(), Role::Warning)
        };
        let hints = if self.transcript.new_output {
            "↓ new output · End to follow".to_string()
        } else {
            "Enter send · Ctrl+J newline · Esc cancel · PgUp/PgDn scroll · Ctrl+R reasoning · Ctrl+C quit"
                .to_string()
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
                "provider {} · model {}",
                self.config.model.endpoint,
                runtime.core.provider.model(),
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

    fn on_tick(&mut self) {
        self.spinner = (self.spinner + 1) % 4;
        if let Some((_, _, at)) = &self.notice
            && at.elapsed() > Duration::from_secs(8)
        {
            self.notice = None;
        }
        let now = Instant::now();
        while let Some((at, _)) = self.token_events.front() {
            if now.duration_since(*at) > Duration::from_secs(3) {
                self.token_events.pop_front();
            } else {
                break;
            }
        }
        let total: u64 = self.token_events.iter().map(|(_, tokens)| *tokens).sum();
        self.tok_s = total as f32 / 3.0;
        self.header.tok_s = self.tok_s;
        self.header.completion = self.completion_tokens;
        if self.transcript.is_dirty() && self.last_width > 0 {
            let show_reasoning = self.config.ui.show_reasoning;
            self.transcript
                .prepare(self.last_width, &self.theme, show_reasoning, false);
        }
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
        let Some(runtime) = &self.runtime else {
            return;
        };
        let core = runtime.core.clone();
        self.transcript.begin_turn(&text);
        self.turn_active = true;
        self.cancelling = false;
        self.turn_state = Some(AgentState::AssembleContext);
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
        self.set_notice("cancelling…", Role::Warning);
    }

    fn finish_turn(&mut self, outcome: Option<TurnOutcome>) {
        self.turn_active = false;
        self.cancelling = false;
        self.turn_state = None;
        self.agent_rx = None;
        self.agent_task = None;
        self.transcript.finish_turn();
        self.refresh_session_blocks();
        if let Some(runtime) = &self.runtime
            && let Ok(mut session) = runtime.session.lock()
        {
            let _ = session.fsync();
        }
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
                        self.set_notice("turn stopped: budget exhausted", Role::Warning);
                    }
                    TurnStatus::Stalled => {
                        self.set_notice("turn stopped: stalled", Role::Warning);
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
            AgentEvent::TurnStarted { model, profile } => {
                self.header.model = model;
                self.header.profile = profile;
                self.turn_state = Some(AgentState::AssembleContext);
            }
            AgentEvent::State(state) => {
                self.turn_state = Some(state);
            }
            AgentEvent::TextDelta(text) => {
                let tokens = (text.chars().count() as u64).div_ceil(4).max(1);
                self.record_tokens(tokens);
                self.completion_tokens = self.completion_tokens.saturating_add(tokens as u32);
                self.transcript.text_delta(&text);
            }
            AgentEvent::ReasoningDelta(text) => {
                let tokens = (text.chars().count() as u64).div_ceil(4).max(1);
                self.record_tokens(tokens);
                self.transcript.reasoning_delta(&text);
            }
            AgentEvent::ToolStarted {
                call_id,
                name,
                summary,
            } => {
                self.turn_state = Some(AgentState::ToolBatch);
                self.transcript.tool_started(&call_id, &name, &summary);
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
            }
            AgentEvent::Usage(usage) => {
                if usage.prompt_tokens > 0 {
                    self.used_tokens = usage.prompt_tokens;
                    self.header.used = usage.prompt_tokens;
                }
                self.completion_tokens = usage.completion_tokens;
                self.header.completion = usage.completion_tokens;
            }
            AgentEvent::Context { used, window } => {
                self.used_tokens = used;
                self.window = window;
                self.header.used = used;
                self.header.window = window;
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
            Event::Resize(_, _) => {}
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

        match key.code {
            KeyCode::Esc => {
                if self.turn_active {
                    self.cancel_turn();
                }
                return;
            }
            KeyCode::PageUp => {
                self.transcript.scroll_up(8);
                return;
            }
            KeyCode::PageDown => {
                self.transcript.scroll_down(8);
                return;
            }
            KeyCode::Home => {
                self.transcript.scroll_home();
                return;
            }
            KeyCode::End => {
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
                if let Some(name) = self.input.partial_command(COMMANDS) {
                    self.input.accept_completion(name);
                }
                let idle = !self.turn_active;
                match self.input.submit(idle) {
                    Some(text) => {
                        let trimmed = text.trim().to_string();
                        if trimmed.starts_with('/') {
                            self.run_command(&trimmed).await;
                        } else if !trimmed.is_empty() {
                            self.start_turn(text);
                        }
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
                let completions = self.input.completions(COMMANDS);
                if !completions.is_empty() {
                    let delta = if matches!(key.code, KeyCode::Up) {
                        -1
                    } else {
                        1
                    };
                    self.input.cycle_completion(delta, completions.len());
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
                let completions = self.input.completions(COMMANDS);
                if !completions.is_empty() {
                    let index = self
                        .input
                        .completion_index
                        .min(completions.len().saturating_sub(1));
                    let name = completions[index].0;
                    self.input.accept_completion(name);
                    return;
                }
                self.input.handle_key(key);
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

        if close {
            self.modal = None;
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

        match name {
            "help" => self.transcript.push_note("help", HELP, false),
            "quit" => self.quit = true,
            "model" => self.command_model(rest).await,
            "profile" => self.command_profile(rest).await,
            "theme" => self.command_theme(rest),
            "tree" => self.command_tree(),
            "fork" => self.command_fork(),
            "sessions" => self.command_sessions(),
            "compact" => self.command_compact(),
            "tools" => self.command_tools(),
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
        if name.is_empty() {
            self.transcript.push_note(
                "model",
                format!(
                    "model: {}\nendpoint: {}",
                    self.config.model.model, self.config.model.endpoint
                ),
                false,
            );
            return;
        }
        let previous = self.config.clone();
        self.config.model.model = name.to_string();
        let config = self.config.clone();
        if let Err(error) = self.rebuild_core_with(config).await {
            self.config = previous;
            self.set_notice(format!("model change failed: {error}"), Role::Error);
            return;
        }
        self.set_notice(format!("model: {name}"), Role::Success);
    }

    async fn command_profile(&mut self, name: &str) {
        if name.is_empty() {
            let mut lines = Vec::new();
            for (profile, data) in &self.config.profiles {
                lines.push(format!(
                    "{profile}: {}",
                    data.description.as_deref().unwrap_or("")
                ));
            }
            self.transcript
                .push_note("profiles", lines.join("\n"), false);
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
        let previous = self.config.clone();
        self.config.profile_name = name.to_string();
        let config = self.config.clone();
        if let Err(error) = self.rebuild_core_with(config).await {
            self.config = previous;
            self.set_notice(format!("profile change failed: {error}"), Role::Error);
            return;
        }
        self.set_notice(format!("profile: {name}"), Role::Success);
    }

    async fn rebuild_core_with(&mut self, config: Config) -> Result<()> {
        let Some(old) = self.core().cloned() else {
            bail!("no runtime");
        };
        let core = app::rebuild_core(&old, config).await?;
        if let Some(runtime) = &mut self.runtime {
            runtime.core = core.clone();
        }
        self.config = core.config.clone();
        self.header.model = core.provider.model().to_string();
        self.header.profile = self.config.profile_name().to_string();
        self.header.trust = self.config.trust || self.options.trust;
        self.window = core.budget.context_window;
        self.header.window = self.window;
        self.used_tokens = estimate_used(&core);
        self.header.used = self.used_tokens;
        Ok(())
    }

    fn command_theme(&mut self, argument: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let paths = runtime.core.paths.clone();
        match argument {
            "" => {
                let names = available_theme_names(&paths);
                self.transcript.push_note(
                    "themes",
                    format!(
                        "current: {}\navailable: {}\nuse /theme <name> or /theme save",
                        self.theme.name,
                        names.join(", ")
                    ),
                    false,
                );
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
                self.theme = Theme::load(
                    name,
                    &paths.themes_dir(),
                    self.config.ui.color_mode,
                    self.no_color,
                );
                self.config.ui.theme = name.to_string();
                self.input.apply_theme(&self.theme);
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
            let summary = context::build_summary(&session, findings.as_ref(), 4500);
            let result = session
                .append(EntryKind::Compaction(CompactionData {
                    summary: summary.summary.clone(),
                    tokens_before: summary.tokens_before,
                    reason: "manual".into(),
                    thread_id: None,
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

    fn command_skill(&mut self, name: &str) {
        let Some(runtime) = &self.runtime else {
            return;
        };
        let skills = SkillIndex::load(&runtime.core.paths, self.config.trust);
        if name.is_empty() {
            let names: Vec<&str> = skills.names();
            self.transcript
                .push_note("skills", format!("available: {}", names.join(", ")), false);
            return;
        }
        let Some(skill) = skills.get(name) else {
            self.set_notice(
                format!(
                    "unknown skill {name:?}; available: {}",
                    skills.names().join(", ")
                ),
                Role::Error,
            );
            return;
        };
        let content = format!("[skill: {}]\n\n{}", skill.name, skill.full_text());
        let result = {
            let mut session = match runtime.session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            let result = session
                .append(EntryKind::User(UserData { content }))
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
            let names = available_workflows(&paths, trust);
            self.transcript.push_note(
                "workflows",
                if names.is_empty() {
                    format!("no workflows in {}", paths.workflows_dir().display())
                } else {
                    format!("available: {}", names.join(", "))
                },
                false,
            );
            return;
        }
        let global = paths.workflows_dir().join(format!("{name}.toml"));
        let project = paths
            .project_dir()
            .join("workflows")
            .join(format!("{name}.toml"));
        let path = if global.is_file() {
            global
        } else if project.is_file() {
            if !trust {
                self.set_notice(
                    "project workflows require --trust; restart with --trust to load them",
                    Role::Error,
                );
                return;
            }
            project
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
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) => {
                self.set_notice(
                    format!("cannot read {}: {error}", path.display()),
                    Role::Error,
                );
                return;
            }
        };
        let workflow: WorkflowFile = match toml::from_str(&raw) {
            Ok(workflow) => workflow,
            Err(error) => {
                self.set_notice(
                    format!("invalid workflow {}: {error}", path.display()),
                    Role::Error,
                );
                return;
            }
        };
        let Some(prompt_ref) = workflow.prompt.as_deref() else {
            self.set_notice(
                format!("workflow {} has no prompt file", path.display()),
                Role::Error,
            );
            return;
        };
        let prompt_path = if Path::new(prompt_ref).is_absolute() {
            PathBuf::from(prompt_ref)
        } else {
            path.parent()
                .unwrap_or_else(|| Path::new("."))
                .join(prompt_ref)
        };
        let prompt = match std::fs::read_to_string(&prompt_path) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.set_notice(
                    format!(
                        "cannot read workflow prompt {}: {error}",
                        prompt_path.display()
                    ),
                    Role::Error,
                );
                return;
            }
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
            derived.tools = workflow.tools.clone();
            derived.description = workflow.description.clone().or(workflow.name.clone());
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
                && let Err(error) = session.append(EntryKind::User(UserData { content: prompt }))
            {
                result = Err(error);
            }
            if result.is_ok() {
                for skill_name in &workflow.skills {
                    if let Some(skill) = skills.get(skill_name) {
                        let content = format!("[skill: {}]\n\n{}", skill.name, skill.full_text());
                        if let Err(error) = session.append(EntryKind::User(UserData { content })) {
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
                    match state.ensure_engagement(&paths) {
                        Ok(engagement) => Some((name, crate::security::render_report(engagement))),
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
            "usage:\n/sec\n/sec scope add|remove <target>\n/sec scope clear\n/sec report",
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
        self.options.skills = SkillIndex::load(&paths, true);
        self.header.trust = true;
        self.header.profile = self.config.profile_name().to_string();
        self.window = core.budget.context_window;
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
        let Some(old_resume) = old_resume else {
            self.set_notice("sessions are disabled (--no-session)", Role::Warning);
            return;
        };
        let old = self.runtime.take();
        drop(old);

        let options = RuntimeOptions {
            path_jail,
            engagement: engagement.clone(),
            no_session: false,
            resume: Some(path.clone()),
            continue_last: false,
        };
        match app::create_runtime(config.clone(), paths.clone(), options).await {
            Ok(runtime) => {
                self.runtime = Some(runtime);
                self.after_session_switch();
                self.set_notice(format!("session: {}", path.display()), Role::Success);
            }
            Err(error) => {
                self.set_notice(
                    format!("cannot switch session: {error}; reopening the previous session"),
                    Role::Error,
                );
                let options = RuntimeOptions {
                    path_jail,
                    engagement,
                    no_session: false,
                    resume: Some(old_resume),
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

    fn header(model: &str, session: &str) -> HeaderData {
        HeaderData {
            cwd: "/tmp/project".into(),
            git: GitStatus::none(),
            profile: "full".into(),
            model: model.into(),
            used: 12400,
            window: 64000,
            completion: 0,
            tok_s: 24.0,
            session: session.into(),
            trust: true,
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
        transcript.prepare(width as usize, &theme, false, true);
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
                    modal: None,
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
            Theme::bundled("omarchy"),
        );
        let text = buffer_text(&terminal);
        assert!(text.contains("/tmp/project"), "cwd missing:\n{text}");
        assert!(text.contains("qwen3.5-35b-a3b"), "model missing:\n{text}");
        assert!(text.contains("12.4k/64k"), "context missing:\n{text}");
        assert!(text.contains("tok/s"));
        assert!(text.contains("trusted"));
    }

    #[test]
    fn narrow_header_drops_throughput_session_and_profile_but_keeps_required() {
        let terminal = draw_scene(
            60,
            20,
            header("qwen3.5-35b-a3b", "abcdef123456"),
            Theme::bundled("omarchy"),
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
            Theme::bundled("omarchy"),
        );
        let after = draw_scene(120, 20, header("model", "session"), Theme::bundled("light"));
        let before_cell = before.backend().buffer().cell((0, 0)).unwrap();
        let after_cell = after.backend().buffer().cell((0, 0)).unwrap();
        assert_eq!(before_cell.symbol(), "L");
        assert_eq!(after_cell.symbol(), "L");
        assert_ne!(
            before_cell.style().fg,
            after_cell.style().fg,
            "theme switch should recolor the header"
        );
    }
}

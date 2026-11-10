//! Agent loop: context assembly, model streaming, tool batch execution, gates.

pub mod gates;
pub mod limits;
pub mod mentions;
pub mod output;
pub mod prompt;
pub mod repair;
pub mod run;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc::UnboundedSender, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, ModelProfile};
use crate::context::budget::TokenBudget;
use crate::context::compact::FindingSummary;
use crate::paths::Paths;
use crate::provider::{Provider, ToolSpec, Usage};
use crate::security::SecurityState;
use crate::tools::{BgRegistry, ReadTracker, SessionHandle, Tool, ToolContext, ToolRegistry};

pub use gates::{SettleDecision, SettleKind, TurnFacts, analyze_turn, settle};
pub use limits::{RunLimits, RunProgress, StopReason};
pub use run::{TurnOutcome, TurnStatus, run_turn};

/// Everything a turn needs. Built between turns so `/model`, `/profile`, and
/// theme changes take effect without restarting the process.
pub struct Core {
    pub config: Config,
    pub paths: Paths,
    pub provider: Arc<dyn Provider>,
    pub registry: ToolRegistry,
    pub tools: Vec<Arc<dyn Tool>>,
    pub tool_specs: Vec<ToolSpec>,
    pub session: SessionHandle,
    pub artifacts: Arc<crate::session::ArtifactStore>,
    pub reads: Arc<Mutex<ReadTracker>>,
    pub bg: Arc<Mutex<BgRegistry>>,
    pub security: Arc<Mutex<SecurityState>>,
    /// OS sandbox for `bash` and `bg`, shared with every tool context.
    pub sandbox: Option<Arc<crate::sandbox::Sandbox>>,
    pub budget: TokenBudget,
    /// Static prompt cost measured at build time: system prompt, tool
    /// schemas, and repository brief. Checked before every turn.
    pub static_prompt: crate::context::StaticPrompt,
    /// Whether the engine answered `/tokenize`, so the rendered prompt can be
    /// measured exactly before sending.
    pub tokenize: bool,
    pub limits: RunLimits,
    pub system_prompt: String,
    /// Repository brief included in `system_prompt`, kept separately so the
    /// static prompt can be re-measured after tokenizer calibration.
    pub repo_brief: String,
    pub model_profile: ModelProfile,
    pub cancel: CancellationToken,
    pub tool_timeout: Duration,
    pub path_jail: bool,
    /// Prompt limit observed from a server that silently truncates (0 when
    /// none was seen). Shared across turns; reset when the core is rebuilt.
    pub server_window: Arc<std::sync::atomic::AtomicU32>,
    /// Where `budget.context_window` came from.
    pub window_source: WindowSource,
    /// Trained, allocated, per-slot, and effective context values with their
    /// sources, when the endpoint reported them.
    pub window_truth: Option<crate::provider::probe::WindowTruth>,
    /// Warning about the window to show once before the first turn.
    pub window_notice: Option<String>,
    /// Ensures `window_notice` is sent only once per core.
    pub window_notice_sent: std::sync::Once,
    /// Window learned from the server during this core's turns (0 = none),
    /// overriding a guessed `budget.context_window`.
    pub detected_window: Arc<std::sync::atomic::AtomicU32>,
    /// Windows learned per `endpoint|model`, shared across core rebuilds so
    /// switching models back and forth keeps what was detected.
    pub window_cache: Arc<Mutex<std::collections::HashMap<String, u32>>>,
    /// Endpoint bring-up (`model.start_command`) still running in the
    /// background. Turns wait for it so the first request does not race the
    /// server. `None` when nothing was started or it finished before launch.
    pub startup: Option<StartupGate>,
}

/// One-time endpoint bring-up that outlives the launch path. The TUI opens
/// immediately; a turn waits for the gate instead of failing against a server
/// that is still starting.
#[derive(Clone)]
pub struct StartupGate {
    state: watch::Receiver<Option<Result<(), String>>>,
}

impl StartupGate {
    /// Create the gate and the handle its background task finishes with.
    pub fn channel() -> (StartupHandle, Self) {
        let (state, watch) = watch::channel(None);
        (StartupHandle { state }, Self { state: watch })
    }

    /// Whether the background task has not reported yet.
    pub fn pending(&self) -> bool {
        self.state.borrow().is_none()
    }

    /// Wait for the background task. `None` means `cancel` fired first. A
    /// dropped task (panic or shutdown) counts as finished.
    pub async fn ready(&self, cancel: &CancellationToken) -> Option<Result<(), String>> {
        let mut state = self.state.clone();
        let waited = async {
            loop {
                if let Some(result) = state.borrow().clone() {
                    return result;
                }
                if state.changed().await.is_err() {
                    return Ok(());
                }
            }
        };
        tokio::select! {
            _ = cancel.cancelled() => None,
            result = waited => Some(result),
        }
    }
}

/// Sends the outcome of the background bring-up exactly once.
pub struct StartupHandle {
    state: watch::Sender<Option<Result<(), String>>>,
}

impl StartupHandle {
    pub fn finish(self, result: Result<(), String>) {
        let _ = self.state.send(Some(result));
    }
}

/// Origin of the context window, shown in the UI so a fallback guess is never
/// mistaken for a measured value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSource {
    /// `context_window` in the config or the active model profile.
    Configured,
    /// Detected from the endpoint for the selected model.
    Probed(crate::provider::probe::WindowProbe),
    /// Nothing configured or detected: `DEFAULT_CONTEXT_WINDOW`.
    Default,
}

impl WindowSource {
    pub fn label(self) -> &'static str {
        match self {
            WindowSource::Configured => "configured",
            WindowSource::Probed(probe) => probe.label(),
            WindowSource::Default => "default (not detected)",
        }
    }

    /// False when the value is a guess (default or a trained maximum).
    pub fn is_reliable(self) -> bool {
        !matches!(
            self,
            WindowSource::Default
                | WindowSource::Probed(crate::provider::probe::WindowProbe::OllamaTrained)
        )
    }
}

impl Core {
    /// A model is selected. The harness can start without one; `/login` sets it.
    pub fn logged_in(&self) -> bool {
        !self.provider.model().trim().is_empty()
    }

    /// The window in force and where it came from, including a window
    /// detected from the server after this core was built.
    pub fn effective_window(&self) -> (u32, WindowSource) {
        let detected = self
            .detected_window
            .load(std::sync::atomic::Ordering::Relaxed);
        if detected > 0 {
            (
                detected,
                WindowSource::Probed(crate::provider::probe::WindowProbe::Served),
            )
        } else {
            (self.budget.context_window, self.window_source)
        }
    }

    /// Key for [`Core::window_cache`].
    pub fn window_key(endpoint: &str, model: &str) -> String {
        format!(
            "{}|{}",
            crate::provider::endpoint_urls(endpoint).api,
            model.trim()
        )
    }

    /// Record a window learned from the server for the current model.
    pub fn remember_window(&self, window: u32) {
        self.detected_window
            .store(window, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut cache) = self.window_cache.lock() {
            cache.insert(
                Self::window_key(&self.config.model.endpoint, self.provider.model()),
                window,
            );
        }
    }

    pub fn tool_context(&self) -> ToolContext {
        self.tool_context_with(self.cancel.clone())
    }

    /// Tool context bound to a specific turn's cancellation token. Each turn
    /// gets a child of `Core::cancel`, so cancelling one turn does not poison
    /// later turns.
    pub fn tool_context_with(&self, cancel: CancellationToken) -> ToolContext {
        ToolContext {
            cwd: self.paths.cwd.clone(),
            path_jail: self.path_jail,
            session: self.session.clone(),
            artifacts: self.artifacts.clone(),
            cancel,
            trust: self.config.trust,
            verify: self.config.verify.clone(),
            profile: self.config.profile_name().to_string(),
            reads: self.reads.clone(),
            bg: self.bg.clone(),
            security: self.security.clone(),
            paths: self.paths.clone(),
            lsp: self.registry.lsp(),
            sandbox: self.sandbox.clone(),
        }
    }

    pub fn findings_summary(&self) -> Option<FindingSummary> {
        let security = self.security.lock().ok()?;
        let engagement = security.engagement.as_ref()?;
        let (open, high, other) = engagement.counts();
        Some(FindingSummary { open, high, other })
    }
}

/// UI-facing events for one turn.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    TurnStarted {
        model: String,
        profile: String,
    },
    State(AgentState),
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted {
        call_id: String,
        name: String,
        summary: String,
    },
    ToolFinished {
        call_id: String,
        name: String,
        is_error: bool,
        duration_ms: u64,
        preview: String,
        artifact: Option<String>,
    },
    Usage(Usage),
    Context {
        used: u32,
        window: u32,
    },
    /// The real context window was learned from the server mid-session.
    WindowDetected {
        window: u32,
        source: String,
    },
    Compacted {
        summary: String,
        tokens_before: u64,
    },
    Notice(String),
    Guard {
        code: String,
        message: String,
    },
    Error {
        code: String,
        message: String,
        recoverable: bool,
    },
    TurnDone(TurnOutcome),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    AssembleContext,
    Compact,
    StreamModel,
    ToolBatch,
    PostTool,
    SettleGates,
    Done,
    Cancelled,
    Error,
}

/// Spawn a turn as a background task. The UI owns session and config mutation;
/// the task only appends session entries.
pub fn spawn_turn(
    core: Arc<Core>,
    user_text: String,
    events: UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> JoinHandle<TurnOutcome> {
    tokio::spawn(async move { run::run_turn(core, user_text, events, cancel).await })
}

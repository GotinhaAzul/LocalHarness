//! Agent loop: context assembly, model streaming, tool batch execution, gates.

pub mod gates;
pub mod limits;
pub mod prompt;
pub mod repair;
pub mod run;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
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
    pub budget: TokenBudget,
    pub limits: RunLimits,
    pub system_prompt: String,
    pub model_profile: ModelProfile,
    pub cancel: CancellationToken,
    pub tool_timeout: Duration,
    pub path_jail: bool,
}

impl Core {
    pub fn tool_context(&self) -> ToolContext {
        ToolContext {
            cwd: self.paths.cwd.clone(),
            path_jail: self.path_jail,
            session: self.session.clone(),
            artifacts: self.artifacts.clone(),
            cancel: self.cancel.clone(),
            trust: self.config.trust,
            verify: self.config.verify.clone(),
            profile: self.config.profile_name().to_string(),
            reads: self.reads.clone(),
            bg: self.bg.clone(),
            security: self.security.clone(),
            paths: self.paths.clone(),
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
    TurnStarted { model: String, profile: String },
    State(AgentState),
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted { call_id: String, name: String, summary: String },
    ToolFinished {
        call_id: String,
        name: String,
        is_error: bool,
        duration_ms: u64,
        preview: String,
        artifact: Option<String>,
    },
    Usage(Usage),
    Context { used: u32, window: u32 },
    Compacted { summary: String, tokens_before: u64 },
    Notice(String),
    Guard { code: String, message: String },
    Error { code: String, message: String, recoverable: bool },
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

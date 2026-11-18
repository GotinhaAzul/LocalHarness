//! Hosting harness sessions for a long-lived frontend: `tot acp` (stdio) and
//! `tot serve` (local HTTP).
//!
//! Both frontends own many sessions at once, each with its own `Core`, and
//! drive turns from requests. This module holds what they share so there is
//! one loop driver, not two:
//!
//! - [`Host`] builds a runtime for a declared root with the process's
//!   config/data roots and session store, and withholds project trust and
//!   verify steps from roots other than the process working directory.
//! - [`HostedSession`] enforces one active turn per session and carries its
//!   cancellation token.
//! - [`start_turn`] runs [`run_turn`] and hands back the event stream plus a
//!   join that always yields a [`TurnOutcome`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::mpsc::{self, UnboundedReceiver};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, Core, TurnOutcome, TurnStatus, run_turn};
use crate::app::{RuntimeOptions, create_runtime};
use crate::config::Config;
use crate::paths::Paths;
use crate::tools::ToolApprover;

/// Process-level settings every hosted session inherits.
#[derive(Clone)]
pub struct Host {
    pub config: Config,
    /// Paths for the process working directory: config/data roots, the
    /// `[session] dir` override, and the trust boundary.
    pub process_paths: Paths,
    pub engagement: Option<String>,
    pub no_session: bool,
    pub no_redact: bool,
}

/// A runtime opened by [`Host::open`].
pub struct Opened {
    pub core: Arc<Core>,
    /// The harness session id (`"ephemeral"` with `--no-session`).
    pub harness_id: String,
    /// Whether project trust applied to this root.
    pub trusted: bool,
}

impl Host {
    /// Project trust and verify steps belong to the process working
    /// directory. A session that declares another root gets neither, so a
    /// client cannot extend the project's trust to an arbitrary directory.
    pub fn session_config(&self, root: &Path) -> Config {
        let mut config = self.config.clone();
        if root != self.process_paths.cwd {
            config.trust = false;
            config.verify = crate::config::VerifyConfig::default();
        }
        config
    }

    /// Paths for a session root, keeping the process's config/data roots and
    /// `[session] dir` override so every frontend sees the same sessions.
    pub fn session_paths(&self, root: &Path) -> Result<Paths> {
        let mut paths =
            Paths::discover(root).with_context(|| format!("cannot prepare {root:?}"))?;
        paths.config_dir = self.process_paths.config_dir.clone();
        paths.data_dir = self.process_paths.data_dir.clone();
        paths.sessions_dir_override = self.process_paths.sessions_dir_override.clone();
        Ok(paths)
    }

    /// Open a runtime rooted at `root` with the path jail on. `resume` names
    /// a session file to reopen; `None` starts a new session.
    pub async fn open(
        &self,
        root: &Path,
        resume: Option<PathBuf>,
        approver: Option<Arc<dyn ToolApprover>>,
    ) -> Result<Opened> {
        self.open_in_mode(root, resume, approver, None).await
    }

    /// [`Host::open`] with the session's permission mode pinned before the
    /// runtime is built. `tot serve` selects the mode per HTTP session; setting
    /// it here makes the policy engine the only pre-dispatch decision point,
    /// so `[permissions]` allow and deny rules apply over HTTP too.
    pub async fn open_in_mode(
        &self,
        root: &Path,
        resume: Option<PathBuf>,
        approver: Option<Arc<dyn ToolApprover>>,
        mode: Option<crate::policy::PermissionMode>,
    ) -> Result<Opened> {
        let paths = self.session_paths(root)?;
        let options = RuntimeOptions {
            path_jail: true,
            engagement: self.engagement.clone(),
            no_session: self.no_session && resume.is_none(),
            no_redact: self.no_redact,
            resume,
            continue_last: false,
            approver,
        };
        let mut config = self.session_config(root);
        if let Some(mode) = mode {
            config.permissions.mode = mode;
        }
        let trusted = config.trust;
        let runtime = create_runtime(config, paths, options).await?;
        let harness_id = runtime
            .core
            .session
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .id
            .clone();
        Ok(Opened {
            core: runtime.core,
            harness_id,
            trusted,
        })
    }
}

/// A second turn was started while one is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

/// One live session: a `Core` plus the single-turn lock.
pub struct HostedSession {
    pub id: String,
    pub root: PathBuf,
    pub core: Arc<Core>,
    active: Mutex<Option<CancellationToken>>,
    idle: watch::Sender<bool>,
}

impl HostedSession {
    pub fn new(id: String, root: PathBuf, core: Arc<Core>) -> Self {
        Self {
            id,
            root,
            core,
            active: Mutex::new(None),
            idle: watch::Sender::new(true),
        }
    }

    fn active(&self) -> std::sync::MutexGuard<'_, Option<CancellationToken>> {
        self.active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Cancel the running turn, if any. Returns whether one was running.
    pub fn cancel(&self) -> bool {
        match self.active().as_ref() {
            Some(token) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// The running turn's token, or a fresh one when idle.
    pub fn token(&self) -> CancellationToken {
        self.active().clone().unwrap_or_default()
    }

    pub fn busy(&self) -> bool {
        self.active().is_some()
    }

    /// Claim the session for one turn. The check and the claim happen under
    /// one lock, so two concurrent callers cannot both start a turn.
    pub fn begin(&self) -> Result<CancellationToken, Busy> {
        let mut active = self.active();
        if active.is_some() {
            return Err(Busy);
        }
        let token = CancellationToken::new();
        *active = Some(token.clone());
        self.idle.send_replace(false);
        Ok(token)
    }

    /// Release the session after the turn has settled and its entries are
    /// written.
    pub fn finish(&self) {
        *self.active() = None;
        self.idle.send_replace(true);
    }

    /// Wait until no turn is running, up to `timeout`. Returns whether the
    /// session is idle.
    pub async fn wait_idle(&self, timeout: Duration) -> bool {
        let mut rx = self.idle.subscribe();
        tokio::time::timeout(timeout, rx.wait_for(|idle| *idle))
            .await
            .is_ok_and(|result| result.is_ok())
    }
}

/// A running turn: its events, then its outcome.
pub struct Turn {
    pub events: UnboundedReceiver<AgentEvent>,
    handle: JoinHandle<TurnOutcome>,
}

/// Start one turn on `core`. The caller drains [`Turn::events`] and then
/// calls [`Turn::outcome`].
pub fn start_turn(core: Arc<Core>, text: String, cancel: CancellationToken) -> Turn {
    let (tx, events) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move { run_turn(core, text, tx, cancel).await });
    Turn { events, handle }
}

impl Turn {
    /// The turn's outcome. `seen` is the outcome carried by a `TurnDone`
    /// event, when the caller saw one; a panicked turn becomes an error.
    pub async fn outcome(self, seen: Option<TurnOutcome>) -> TurnOutcome {
        match (seen, self.handle.await) {
            (Some(outcome), _) => outcome,
            (None, Ok(outcome)) => outcome,
            (None, Err(error)) => TurnOutcome {
                status: TurnStatus::Error,
                error: Some(("turn_panicked".into(), error.to_string())),
                gate_errors: Default::default(),
            },
        }
    }
}

/// Built-in tools that cannot touch the filesystem or run a process. MCP and
/// external tools always need approval, even when their manifest claims
/// read-only: a tool does not get to decide its own permission level.
pub fn needs_permission(name: &str) -> bool {
    !matches!(
        name,
        "read"
            | "grep"
            | "glob"
            | "repo_map"
            | "recall"
            | "review"
            | "lsp"
            | "skill"
            | "todo"
            | "thread"
    )
}

/// Canonicalize a client-declared root. It must be an absolute, existing
/// directory; symlinks resolve before the jail is applied.
pub fn declared_root(cwd: &str) -> Result<PathBuf, String> {
    let path = Path::new(cwd);
    if !path.is_absolute() {
        return Err(format!("cwd {cwd:?} must be an absolute path"));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cwd {cwd:?} cannot be resolved: {error}"))?;
    if !canonical.is_dir() {
        return Err(format!("cwd {cwd:?} is not a directory"));
    }
    Ok(canonical)
}

/// A best-effort check that a session id cannot be used as a path.
pub fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_builtin_read_tools_skip_permission() {
        for name in [
            "read", "grep", "glob", "repo_map", "recall", "review", "lsp", "skill", "todo",
            "thread",
        ] {
            assert!(!needs_permission(name), "{name} should be free");
        }
        for name in ["write", "edit", "patch", "bash", "bg", "verify", "test"] {
            assert!(needs_permission(name), "{name} must ask");
        }
        // MCP/external names are not self-certifying.
        for name in ["files__read_file", "mcp_read", "readOnlyThing"] {
            assert!(needs_permission(name), "{name} must ask");
        }
    }

    #[test]
    fn session_ids_cannot_be_paths() {
        assert!(valid_session_id("abc-123_x"));
        assert!(!valid_session_id("../etc/passwd"));
        assert!(!valid_session_id(""));
    }
}

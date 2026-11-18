//! Run checkpoints, wall clock, and stall detection.
//!
//! Tool-call, model-call, and wall-clock limits are checkpoints, not hard
//! stops. When one is reached and the run produced new information, a file
//! change, or a passing verification recently, the allowance is renewed and
//! the turn continues. Only a checkpoint with no recent progress — or the
//! exact-repetition guards — ends the turn. This lets a real long task run as
//! long as it keeps making progress without letting a looping model do so.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::config::BudgetConfig;

#[derive(Debug, Clone)]
pub struct RunLimits {
    /// Checkpoint interval, not a hard cap.
    pub max_tool_calls: u32,
    /// Checkpoint interval, not a hard cap.
    pub max_model_calls: u32,
    /// Checkpoint interval, not a hard cap.
    pub max_wall: Duration,
    pub stall_calls: u32,
    /// Failing or repeated calls of one tool family allowed in a turn. Zero
    /// disables the guard.
    pub max_same_tool_family: u32,
    /// Successful calls of one family that change nothing allowed in a turn.
    /// Zero disables the guard.
    pub max_same_tool_family_success: u32,
    pub verify_attempts: u32,
}

impl From<&BudgetConfig> for RunLimits {
    fn from(config: &BudgetConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls.max(1),
            max_model_calls: config.max_model_calls.max(1),
            max_wall: Duration::from_secs(u64::from(config.max_wall_minutes.max(1)) * 60),
            stall_calls: config.stall_calls.max(1),
            max_same_tool_family: config.max_same_tool_family,
            max_same_tool_family_success: config.max_same_tool_family_success,
            verify_attempts: config.verify_attempts,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    ToolBudget,
    ModelBudget,
    WallClock,
    Stalled,
    Cancelled,
}

impl StopReason {
    /// Code recorded in the session and the headless JSON stream.
    pub fn code(&self) -> &'static str {
        match self {
            StopReason::ToolBudget => "budget_tools",
            StopReason::ModelBudget => "budget_models",
            StopReason::WallClock => "budget_wall",
            StopReason::Stalled => "stalled",
            StopReason::Cancelled => "cancelled",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            StopReason::ToolBudget => {
                "the tool-call checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::ModelBudget => {
                "the model-call checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::WallClock => {
                "the wall-clock checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::Stalled => {
                "the last calls produced no new information or file change; stop and report the last known state, or change approach in a new turn"
            }
            StopReason::Cancelled => "cancelled by the user",
        }
    }
}

/// Result of asking a checkpoint whether the turn may continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCheck {
    /// Not at a checkpoint yet.
    Ok,
    /// The checkpoint was reached while the run was still productive; the
    /// allowance was renewed and the turn continues.
    Renewed(StopReason),
    /// The checkpoint was reached with no recent progress. This is the only
    /// budget outcome that ends a turn.
    Exhausted(StopReason),
}

#[derive(Debug)]
pub struct RunProgress {
    pub limits: RunLimits,
    /// Wall clock for the whole turn, used for reporting only.
    pub started_at: Instant,
    /// Cumulative counters for reporting and messages.
    pub tool_calls: u32,
    pub model_calls: u32,
    /// Current checkpoint allowance.
    epoch_tool_calls: u32,
    epoch_model_calls: u32,
    epoch_started_at: Instant,
    /// How many times a productive checkpoint renewed the allowance.
    pub renewals: u32,
    /// Cumulative tool-call index of the last result that produced usable new
    /// information, a file change, or a passing verification.
    last_progress_call: Option<u32>,
    last_progress_at: Option<Instant>,
    /// Result digests already seen this turn. A repeated result is not new
    /// information even when the call arguments differ.
    seen_results: HashSet<u64>,
    pub stall: StallTracker,
    pub settle_continuations: u32,
    pub verify_attempts: u32,
    pub review_nudged: bool,
    pub context_retry_used: bool,
    pub last_stop: Option<StopReason>,
    /// Harness nudges spent on broken output this turn.
    pub output_recoveries: u32,
    /// Whether the user was told that tool calls are arriving as text.
    pub text_calls_noticed: bool,
    /// Whether the exact tokenizer check already forced a compaction. Reset
    /// after every successful model reply so a later context fill can check
    /// again.
    pub presend_compaction_used: bool,
    /// Whether the advance notice for the todo gate was attached this turn.
    pub todo_notice_sent: bool,
}

impl RunProgress {
    pub fn new(limits: RunLimits) -> Self {
        let stall_calls = limits.stall_calls;
        let family_limit = limits.max_same_tool_family;
        let success_limit = limits.max_same_tool_family_success;
        let now = Instant::now();
        Self {
            limits,
            started_at: now,
            tool_calls: 0,
            model_calls: 0,
            epoch_tool_calls: 0,
            epoch_model_calls: 0,
            epoch_started_at: now,
            renewals: 0,
            last_progress_call: None,
            last_progress_at: None,
            seen_results: HashSet::new(),
            stall: StallTracker::new(stall_calls, family_limit, success_limit),
            settle_continuations: 0,
            verify_attempts: 0,
            review_nudged: false,
            todo_notice_sent: false,
            context_retry_used: false,
            last_stop: None,
            output_recoveries: 0,
            text_calls_noticed: false,
            presend_compaction_used: false,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Check the model-call and wall-clock checkpoint before a model call.
    pub fn model_checkpoint(&mut self) -> BudgetCheck {
        let model_hit = self.epoch_model_calls >= self.limits.max_model_calls;
        let wall_hit = self.epoch_started_at.elapsed() >= self.limits.max_wall;
        if !model_hit && !wall_hit {
            return BudgetCheck::Ok;
        }
        if model_hit {
            if self.recent_call_progress() {
                return self.renew(StopReason::ModelBudget);
            }
            return BudgetCheck::Exhausted(StopReason::ModelBudget);
        }
        if self.recent_time_progress() {
            self.renew(StopReason::WallClock)
        } else {
            BudgetCheck::Exhausted(StopReason::WallClock)
        }
    }

    pub fn model_call_started(&mut self) {
        self.model_calls += 1;
        self.epoch_model_calls += 1;
    }

    /// Check the tool-call checkpoint before a batch of `n` calls. A batch is
    /// allowed to cross the checkpoint: the allowance is renewed first when
    /// the run is productive.
    pub fn tool_checkpoint(&mut self, n: u32) -> BudgetCheck {
        if self.epoch_tool_calls.saturating_add(n) <= self.limits.max_tool_calls {
            return BudgetCheck::Ok;
        }
        if self.recent_call_progress() {
            self.renew(StopReason::ToolBudget)
        } else {
            BudgetCheck::Exhausted(StopReason::ToolBudget)
        }
    }

    /// Record a tool result. `outcome.result` is the content shown to the
    /// model; a result content that was already seen this turn is a repeat, not
    /// new information, even when the call arguments differ. A failed call
    /// never counts as new information. `cwd` is the workspace the call ran in.
    pub fn record_tool_result(&mut self, cwd: &Path, outcome: ToolOutcome<'_>) {
        self.tool_calls += 1;
        self.epoch_tool_calls += 1;
        self.stall.observe(cwd, outcome);
        let novel = !outcome.is_error && self.seen_results.insert(result_digest(outcome.result));
        if outcome.changed_file || outcome.verified_ok || novel {
            self.last_progress_call = Some(self.tool_calls);
            self.last_progress_at = Some(Instant::now());
        }
    }

    pub fn stalled(&self) -> bool {
        self.stall.stalled()
    }

    /// Why the turn is being stopped as `stalled`, naming the repeating family
    /// when that guard is the one that fired.
    pub fn stall_message(&self) -> String {
        self.stall
            .family_reason()
            .unwrap_or_else(|| StopReason::Stalled.message().to_string())
    }

    /// Re-check a recorded family trip once a batch is over. A call later in
    /// the same batch may have changed the workspace (a `sed -i`, a script
    /// writing a file); the run was making progress, so it is not a stall.
    pub fn recheck_workspace(&mut self, cwd: &Path) {
        self.stall.recheck(cwd);
    }

    /// A successful reply breaks a context-error episode: the next compaction
    /// or context retry gets its own allowance.
    pub fn recovered_context(&mut self) {
        self.context_retry_used = false;
        self.presend_compaction_used = false;
    }

    /// At most two settle continuations per turn, shared by verify and todo.
    pub fn can_continue_settle(&self) -> bool {
        self.settle_continuations < 2
    }

    /// Message for a checkpoint that ended the turn.
    pub fn checkpoint_message(&self, reason: StopReason) -> String {
        let window = self.limits.stall_calls;
        let no_progress = format!(
            "no new result, file change, or passing verification in the last {window} call(s)"
        );
        match reason {
            StopReason::ToolBudget => format!(
                "tool-call checkpoint: {} calls in this allowance, {no_progress}. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.epoch_tool_calls
            ),
            StopReason::ModelBudget => format!(
                "model-call checkpoint: {} model calls in this allowance, {no_progress}. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.epoch_model_calls
            ),
            StopReason::WallClock => format!(
                "wall-clock checkpoint: {} minutes in this allowance, no new result, file change, or passing verification. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.limits.max_wall.as_secs() / 60
            ),
            StopReason::Stalled => {
                "the last calls produced no new information or file change; stop and report the last known state, or change approach in a new turn".into()
            }
            StopReason::Cancelled => "cancelled by the user".into(),
        }
    }

    /// User-facing notice when a productive checkpoint renews the allowance.
    pub fn renewal_message(&self, trigger: StopReason) -> String {
        format!(
            "budget checkpoint renewed ({}): {} tool calls, {} model calls, {} min used; the run is still producing new results or file changes, so the turn continues.",
            trigger.code(),
            self.tool_calls,
            self.model_calls,
            self.elapsed().as_secs() / 60
        )
    }

    fn recent_call_progress(&self) -> bool {
        self.last_progress_call
            .is_some_and(|at| self.tool_calls.saturating_sub(at) < self.limits.stall_calls)
    }

    fn recent_time_progress(&self) -> bool {
        let window = (self.limits.max_wall / 2).max(Duration::from_secs(60));
        self.last_progress_at
            .is_some_and(|at| at.elapsed() < window)
    }

    fn renew(&mut self, trigger: StopReason) -> BudgetCheck {
        self.epoch_tool_calls = 0;
        self.epoch_model_calls = 0;
        self.epoch_started_at = Instant::now();
        self.renewals += 1;
        // A renewal must be earned again by new work in the next allowance.
        self.last_progress_call = None;
        self.last_progress_at = None;
        BudgetCheck::Renewed(trigger)
    }
}

fn result_digest(result: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    result.hash(&mut hasher);
    hasher.finish()
}

/// One tool result as the stall guard sees it.
#[derive(Debug, Clone, Copy)]
pub struct ToolOutcome<'a> {
    /// The call and its result, for the consecutive-identical strike counter.
    pub fingerprint: &'a str,
    /// Canonical family of the call: `bash` by program and operation, other
    /// tools by name and key arguments.
    pub family: &'a str,
    /// The family plus the arguments that make this exact call distinct.
    pub signature: &'a str,
    /// Result content shown to the model.
    pub result: &'a str,
    pub is_error: bool,
    pub changed_file: bool,
    pub verified_ok: bool,
    /// The call only inspects the workspace, the repository, or the network:
    /// a read-only tool, a read-only action of a mixed tool, or a `bash`
    /// command with no write in it. Never a repeat or an idle success.
    pub read_only: bool,
}

/// Tools whose every call only reads. The list is the registry's read-only
/// half (`Tool::read_only`), named here because the guard sees a tool name,
/// not a handle: `read`, `grep`, `glob`, `recall`, `repo_map`, `lsp`,
/// `evidence`, `fetch`, `skill`, `review`, `iocs`, `surface`, `attack_path`.
/// `symbol` is `read` with `symbol`, and `project`/`hunk` are helpers, not
/// tools.
const READ_ONLY_TOOLS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "recall",
    "repo_map",
    "lsp",
    "evidence",
    "fetch",
    "skill",
    "review",
    "iocs",
    "surface",
    "attack_path",
];

/// Tools whose calls read only for some actions. The rest of each schema is
/// there to change something: `bg start`/`stop` runs and kills a process,
/// `todo plan`/`start`/`done` rewrite the plan, `thread split`/`note`/`close`
/// rewrite the thread, `finding add`/`update`/`report`/`verify` writes the
/// ledger, `detect run` creates findings, and `pr draft`/`comment` creates or
/// writes a pull request.
const READ_ONLY_ACTIONS: &[(&str, &[&str])] = &[
    // `status` lists every process when no id is given; `list` is the same
    // read under its clearer name.
    ("bg", &["logs", "status", "list"]),
    ("todo", &["list"]),
    ("thread", &["status"]),
    ("finding", &["list"]),
    ("detect", &["validate"]),
    ("pr", &["checks", "review"]),
];

/// `git` tool actions that only read. `branch` and `worktree` read in their
/// `op = "list"` form.
const GIT_READ_ACTIONS: &[&str] = &["status", "diff", "log", "blame", "commit_message"];

/// Whether a call only inspects. `bash` is classified by its command, `git`
/// and the mixed tools by their action, and the rest by the tool's own
/// declaration. A tool this does not know (an MCP or external tool) is not
/// read-only here; its own `Tool::read_only` is consulted by the caller.
pub fn call_read_only(name: &str, arguments: &str) -> bool {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    match name {
        "bash" => args
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(crate::policy::command::reads_only),
        "git" => {
            let op = args.get("op").and_then(Value::as_str).unwrap_or("");
            GIT_READ_ACTIONS.contains(&action)
                || (matches!(action, "branch" | "worktree") && op == "list")
        }
        _ => match READ_ONLY_ACTIONS.iter().find(|(tool, _)| *tool == name) {
            Some((_, actions)) => actions.contains(&action),
            None => READ_ONLY_TOOLS.contains(&name),
        },
    }
}

/// Detects a turn that repeats itself. One counter watches consecutive
/// identical fingerprints; three cumulative counters watch a tool family
/// across the whole turn: failing calls, repeated calls (successful or not),
/// and successful calls that changed nothing and passed no verification.
///
/// Only a file change, a passing verification, or a workspace that changed
/// since the counter started clears a family counter. A different call in
/// between, or a success of the same family, does not. Read-only calls are
/// watched by the consecutive-identical counter only.
#[derive(Debug)]
pub struct StallTracker {
    needed: u32,
    strikes: u32,
    last: Option<String>,
    /// Failing and repeated calls per family. Zero disables.
    family_limit: u32,
    /// Successful calls that changed nothing per family. Zero disables.
    success_limit: u32,
    failures: HashMap<String, Counter>,
    repeats: HashMap<String, Counter>,
    idle_successes: HashMap<String, Counter>,
    trip: Option<Trip>,
    /// Workspace identity, read lazily and reused by every counter.
    workspace: WorkspaceMemo,
    /// How the workspace is identified. A field so a test can count reads
    /// instead of timing them.
    fingerprint: Fingerprint,
}

impl StallTracker {
    pub fn new(needed: u32, family_limit: u32, success_limit: u32) -> Self {
        Self {
            needed,
            strikes: 0,
            last: None,
            family_limit,
            success_limit,
            failures: HashMap::new(),
            repeats: HashMap::new(),
            idle_successes: HashMap::new(),
            trip: None,
            workspace: WorkspaceMemo::default(),
            fingerprint: workspace_state,
        }
    }

    pub fn observe(&mut self, cwd: &Path, outcome: ToolOutcome<'_>) {
        self.workspace.begin_call();
        if outcome.changed_file || outcome.verified_ok {
            self.strikes = 0;
            self.last = Some(outcome.fingerprint.to_string());
            // Real progress clears every counter and the memo: the workspace
            // is read again when the next counter starts.
            self.clear();
            self.workspace.invalidate();
            return;
        }
        if self.last.as_deref() == Some(outcome.fingerprint) {
            self.strikes += 1;
        } else {
            self.strikes = 1;
            self.last = Some(outcome.fingerprint.to_string());
        }

        // A counter that already tripped ended the guard's work for the turn.
        // Further calls are not counted and take no fingerprint; only a
        // reported file change (above) or the end-of-batch re-check clears it.
        if self.trip.is_some() {
            return;
        }
        if outcome.is_error {
            if let Some(count) = self.bump(cwd, Counted::Failure, outcome.family) {
                self.set_trip(TripKind::Failed, outcome.family, count);
            }
            return;
        }
        // Reading a file, polling a job, or fetching a URL is inspection, not
        // an attempt to do the work: a `bg logs` poll that shows a build
        // advancing is new information every time, and a chain of distinct
        // `curl`, `git show`, or `gh pr view` calls is exploration. Only the
        // consecutive-identical counter watches read loops, and it needs the
        // same result twice. Failing reads still count above: a broken read
        // retried forever is a loop.
        if outcome.read_only {
            return;
        }
        if let Some(count) = self.bump(cwd, Counted::Repeat, outcome.signature) {
            self.set_trip(TripKind::Repeated, outcome.signature, count);
        }
        let coarse = coarse_family(outcome.family);
        if let Some(count) = self.bump(cwd, Counted::Success, &coarse) {
            self.set_trip(TripKind::Unproductive, &coarse, count);
        }
    }

    fn bump(&mut self, cwd: &Path, counted: Counted, key: &str) -> Option<u32> {
        let limit = counted.limit(self.family_limit, self.success_limit);
        if limit == 0 {
            return None;
        }
        let counters = match counted {
            Counted::Failure => &mut self.failures,
            Counted::Repeat => &mut self.repeats,
            Counted::Success => &mut self.idle_successes,
        };
        bump(
            counters,
            key,
            limit,
            &mut self.workspace,
            cwd,
            self.fingerprint,
        )
    }

    fn set_trip(&mut self, kind: TripKind, family: &str, count: u32) {
        if self.trip.is_some() {
            return;
        }
        let workspace = self.workspace.state().map(str::to_string);
        self.trip = Some(Trip {
            kind,
            family: family.to_string(),
            count,
            workspace,
        });
    }

    fn clear(&mut self) {
        self.failures.clear();
        self.repeats.clear();
        self.idle_successes.clear();
        self.trip = None;
    }

    /// Re-check a recorded trip once its batch is over: a call in the same
    /// batch may have changed the workspace, which makes the repetition real
    /// work rather than a loop.
    pub fn recheck(&mut self, cwd: &Path) {
        let Some(trip) = self.trip.take() else {
            return;
        };
        self.workspace.begin_call();
        let now = self.workspace.fresh(cwd, self.fingerprint);
        let changed = matches!(
            (trip.workspace.as_deref(), now.as_deref()),
            (Some(before), Some(after)) if before != after
        );
        if !changed {
            self.trip = Some(trip);
            return;
        }
        self.failures.clear();
        self.repeats.clear();
        self.idle_successes.clear();
    }

    pub fn stalled(&self) -> bool {
        self.strikes >= self.needed || self.trip.is_some()
    }

    pub fn strikes(&self) -> u32 {
        self.strikes
    }

    /// Stop message for the family guard, once it has fired.
    pub fn family_reason(&self) -> Option<String> {
        let trip = self.trip.as_ref()?;
        Some(match trip.kind {
            TripKind::Repeated => format!(
                "the same call was repeated {} times this turn without changing a file or passing a verification ({}); stop repeating it and report the last known state, or change approach in a new turn",
                trip.count, trip.family
            ),
            TripKind::Failed => format!(
                "the same tool family ({}) ran {} failing calls this turn without a file change or a passing verification; stop repeating it and report the last known state, or change approach in a new turn",
                trip.family, trip.count
            ),
            TripKind::Unproductive => format!(
                "{} successful calls of {} changed no file and passed no verification this turn; stop repeating it and report the last known state, or change approach in a new turn",
                trip.count, trip.family
            ),
        })
    }
}

#[derive(Debug, Clone, Copy)]
enum TripKind {
    Repeated,
    Failed,
    Unproductive,
}

#[derive(Debug)]
struct Trip {
    kind: TripKind,
    family: String,
    count: u32,
    /// Workspace state when the trip fired. If the batch that follows changed
    /// it, the turn is not stalled.
    workspace: Option<String>,
}

/// A cumulative count for one key, plus the workspace state when the count
/// started so real work in between can clear it.
#[derive(Debug, Default)]
struct Counter {
    count: u32,
    baseline: Option<String>,
}

/// Which counter a call feeds.
#[derive(Debug, Clone, Copy)]
enum Counted {
    Failure,
    Repeat,
    Success,
}

impl Counted {
    fn limit(self, family_limit: u32, success_limit: u32) -> u32 {
        match self {
            Counted::Failure | Counted::Repeat => family_limit,
            Counted::Success => success_limit,
        }
    }
}

/// How the workspace is identified. A function pointer rather than a closure
/// so a test can replace it with a counter.
type Fingerprint = fn(&Path) -> Option<String>;

/// Count one call for `key`. Returns the count once it reaches `limit`, unless
/// the workspace changed since the counter started: then the calls did real
/// work and the counter restarts instead of stopping the turn.
fn bump(
    counters: &mut HashMap<String, Counter>,
    key: &str,
    limit: u32,
    workspace: &mut WorkspaceMemo,
    cwd: &Path,
    fingerprint: Fingerprint,
) -> Option<u32> {
    if limit == 0 {
        return None;
    }
    let counter = counters.entry(key.to_string()).or_default();
    if counter.count == 0 {
        counter.baseline = workspace.baseline(cwd, fingerprint);
    }
    counter.count += 1;
    if counter.count < limit {
        return None;
    }
    let changed = match (
        counter.baseline.as_deref(),
        workspace.fresh(cwd, fingerprint).as_deref(),
    ) {
        (Some(before), Some(now)) => before != now,
        // Without a readable workspace the guard keeps its stop: the call did
        // not report a file change either.
        _ => false,
    };
    if !changed {
        return Some(counter.count);
    }
    counter.count = 0;
    counter.baseline = None;
    None
}

/// The workspace identity, read lazily and memoized across calls.
///
/// Reading it costs a `git status` plus a stat per listed path, or a bounded
/// walk outside a repository, so it is taken once and reused: the first
/// counter that needs a baseline reads it, every later counter reuses it, and
/// only a reported file change clears it. A trip decision takes one fresh
/// reading — a `sed -i` through `bash` changes files without the harness
/// seeing it — and that reading becomes the new baseline. Two counters that
/// trip on the same call share the reading.
#[derive(Debug, Default)]
struct WorkspaceMemo {
    cwd: Option<PathBuf>,
    state: Option<String>,
    /// Whether `state` is current for `cwd`.
    valid: bool,
    /// Whether this call already took a fresh reading for a trip decision.
    fresh_taken: bool,
}

impl WorkspaceMemo {
    fn begin_call(&mut self) {
        self.fresh_taken = false;
    }

    /// The cached identity, read now only if the memo is empty or invalid.
    fn baseline(&mut self, cwd: &Path, fingerprint: Fingerprint) -> Option<String> {
        if !self.valid || self.cwd.as_deref() != Some(cwd) {
            self.read(cwd, fingerprint);
        }
        self.state.clone()
    }

    /// The identity as it is now, for a trip decision. One reading per call.
    fn fresh(&mut self, cwd: &Path, fingerprint: Fingerprint) -> Option<String> {
        if !self.fresh_taken || self.cwd.as_deref() != Some(cwd) {
            self.read(cwd, fingerprint);
        }
        self.state.clone()
    }

    fn read(&mut self, cwd: &Path, fingerprint: Fingerprint) {
        self.state = fingerprint(cwd);
        self.cwd = Some(cwd.to_path_buf());
        self.valid = true;
        self.fresh_taken = true;
    }

    fn state(&self) -> Option<&str> {
        self.state.as_deref()
    }

    fn invalidate(&mut self) {
        self.valid = false;
    }
}

/// Identity of the working tree, used to tell real work from a loop: `git
/// status` plus the size and mtime of every changed or untracked path in a
/// repository (nothing is written to the object store), otherwise a bounded
/// walk of paths, sizes, and modification times. `None` when it cannot be
/// read.
pub fn workspace_state(cwd: &Path) -> Option<String> {
    if let Some(state) = crate::tools::git::working_state(cwd) {
        return Some(state);
    }
    if !cwd.is_dir() {
        return None;
    }
    Some(metadata_digest(cwd))
}

/// Bounded walk for a directory that is not a git repository. Build output and
/// dependency trees are skipped, like the repository map does, so a build does
/// not read as a file change.
fn metadata_digest(root: &Path) -> String {
    const MAX_ENTRIES: usize = 2_000;
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(true)
        .parents(false)
        .follow_links(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git"
                    | ".hg"
                    | ".svn"
                    | "target"
                    | "node_modules"
                    | "dist"
                    | "build"
                    | ".venv"
                    | "venv"
                    | "__pycache__"
                    | ".next"
                    | ".cache"
                    | "vendor"
            )
        });
    let mut entries: Vec<(String, u64, u128)> = Vec::new();
    for entry in builder.build().flatten() {
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = entry.path().strip_prefix(root).unwrap_or(entry.path());
        entries.push((path.to_string_lossy().into_owned(), meta.len(), modified));
    }
    entries.sort();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    entries.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Canonical family for a tool call, so near-identical retries count as the
/// same operation: `bash` calls by program (and subcommand for tools that take
/// one), other tools by name and key arguments. Body arguments (`content`,
/// `old`, `new`, ...) do not create a new family.
pub fn tool_family(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    if name == "bash" {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        return format!("bash {}", bash_family(command));
    }
    let args = canonical_args(&args, false);
    if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {args}")
    }
}

/// The family plus the arguments that make one call distinct, so a repeat can
/// be told from a near-identical retry. Here-doc bodies, temporary paths, and
/// bare numbers are hidden for `bash`; other tools keep their arguments, so
/// two writes with different content are two calls.
pub fn tool_signature(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let family = tool_family(name, arguments);
    let detail = if name == "bash" {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        normalize_text(&strip_heredocs(command))
    } else {
        canonical_args(&args, true)
    };
    if detail.is_empty() {
        family
    } else {
        format!("{family} :: {detail}")
    }
}

/// Programs whose second bare word is a subcommand worth keeping (`cargo
/// test`, `git status`). For everything else the program alone is the family,
/// so `tesseract a.png`, `tesseract b.png --psm 6` and the like stay together.
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "cargo",
    "git",
    "go",
    "dotnet",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "uv",
    "pip",
    "pip3",
    "rustup",
    "kubectl",
    "docker",
    "podman",
    "systemctl",
    "mise",
    "nix",
    "gh",
];

const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "time", "timeout", "command", "nohup", "exec", "nice", "stdbuf",
];

/// Flags that take a value, so the word after them is not the operation:
/// `git -C repo status`, `kubectl -n ns get pods`, `nice -n 5 cargo test`.
const VALUE_FLAGS: &[&str] = &[
    "-C",
    "-c",
    "-n",
    "-u",
    "-b",
    "-R",
    "-s",
    "--context",
    "--namespace",
    "--target",
    "--profile",
];

/// The family without its subcommand, for the unproductive-success counter:
/// `bash cargo test` and `bash cargo build` are both `bash cargo`, and every
/// read of the same tool collapses to the tool name.
fn coarse_family(family: &str) -> String {
    if let Some(rest) = family.strip_prefix("bash ") {
        let program = rest.split(' ').next().unwrap_or(rest);
        return format!("bash {program}");
    }
    family.split(' ').next().unwrap_or(family).to_string()
}

/// Argument keys that carry code or file bodies. Their contents change between
/// retries, so they say nothing about the operation.
const BODY_KEYS: &[&str] = &[
    "content",
    "old",
    "new",
    "replacement",
    "replace",
    "patch",
    "diff",
    "code",
    "script",
    "text",
    "body",
    "source",
];

fn bash_family(command: &str) -> String {
    let tokens = shell_words(&strip_heredocs(command));
    let mut index = 0;
    let program = loop {
        let Some(token) = tokens.get(index) else {
            return "shell".into();
        };
        if is_assignment(token) {
            index += 1;
            continue;
        }
        if matches!(token.as_str(), "cd" | "pushd" | "popd") {
            index = skip_flags(&tokens, index + 1);
            if tokens.get(index).is_some() {
                index += 1;
            }
            continue;
        }
        if WRAPPERS.contains(&token.as_str()) {
            index = skip_flags(&tokens, index + 1);
            // `timeout 30 cargo test`: the duration is a bare word.
            if token == "timeout" && tokens.get(index).is_some() {
                index += 1;
            }
            continue;
        }
        break token.as_str();
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    if !SUBCOMMAND_PROGRAMS.contains(&program) {
        return program.to_string();
    }
    let operation = tokens
        .get(skip_flags(&tokens, index + 1))
        .filter(|token| is_operation(token))
        .map(String::as_str);
    match operation {
        Some(operation) => format!("{program} {operation}"),
        None => program.to_string(),
    }
}

/// Skip flags starting at `index`, consuming the value of a flag that takes
/// one. Returns the index of the first word that is not a flag or a value.
fn skip_flags(tokens: &[String], mut index: usize) -> usize {
    while let Some(token) = tokens.get(index) {
        if !token.starts_with('-') {
            break;
        }
        let takes_value = VALUE_FLAGS.contains(&token.as_str());
        index += 1;
        if takes_value {
            index += 1;
        }
    }
    index
}

fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_operation(token: &str) -> bool {
    !token.is_empty()
        && !token.starts_with('-')
        && !token.contains(['/', '=', '$', '*', '?'])
        && !token.chars().all(|ch| ch.is_ascii_digit())
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':'))
}

/// Remove here-document bodies, which are code and differ between retries.
fn strip_heredocs(command: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut delimiter: Option<String> = None;
    for line in command.lines() {
        if let Some(wanted) = &delimiter {
            if line.trim() == wanted {
                delimiter = None;
            }
            continue;
        }
        kept.push(line);
        delimiter = heredoc_delimiter(line);
    }
    if kept.is_empty() {
        command.to_string()
    } else {
        kept.join("\n")
    }
}

fn heredoc_delimiter(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) => {
                if byte == open {
                    quote = None;
                }
                index += 1;
                continue;
            }
            None => match byte {
                b'\'' | b'"' => {
                    quote = Some(byte);
                    index += 1;
                    continue;
                }
                b'<' if bytes[index + 1] == b'<' => {
                    let mut start = index + 2;
                    if bytes.get(start) == Some(&b'-') {
                        start += 1;
                    }
                    let rest = line[start..].trim_start();
                    let delimiter: String = rest
                        .trim_start_matches(['\'', '"'])
                        .chars()
                        .take_while(|ch| {
                            !ch.is_whitespace() && !matches!(ch, '\'' | '"' | ';' | '|')
                        })
                        .collect();
                    return (!delimiter.is_empty()).then_some(delimiter);
                }
                _ => {}
            },
        }
        index += 1;
    }
    None
}

/// Split a shell command into words, dropping quotes and separators. Enough to
/// name the program and subcommand; not a shell parser.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(open) => {
                if ch == open {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                ch if ch.is_whitespace() || matches!(ch, ';' | '|' | '&' | '<' | '>') => {
                    if !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                    }
                }
                ch => current.push(ch),
            },
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn canonical_args(args: &Value, keep_bodies: bool) -> String {
    let Value::Object(map) = args else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in map {
        if value.is_null() {
            continue;
        }
        let value = if !keep_bodies && BODY_KEYS.contains(&key.as_str()) {
            "<body>".to_string()
        } else {
            canonical_value(value)
        };
        parts.push(format!("{key}={value}"));
    }
    parts.sort();
    parts.join(",")
}

fn canonical_value(value: &Value) -> String {
    match value {
        Value::String(text) => collapse(text),
        Value::Array(items) => {
            let mut items: Vec<String> = items.iter().map(canonical_value).collect();
            items.sort();
            format!("[{}]", items.join(","))
        }
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Null => String::new(),
        other => collapse(&other.to_string()),
    }
}

/// Collapse whitespace so formatting differences do not create a new call.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapse whitespace and hide temporary-directory names and bare numbers,
/// which change between retries even when the operation is the same. Used for
/// bash commands only: a file path in another tool's arguments is that tool's
/// target, not noise.
fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&normalize_word(word));
    }
    out
}

fn normalize_word(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|ch| ch.is_ascii_digit()) {
        return "<n>".to_string();
    }
    normalize_temp_path(word)
}

/// Temporary directories whose names change on every call.
const TEMP_ROOTS: &[&str] = &["/tmp/", "/var/tmp/", "/private/tmp/"];

fn normalize_temp_path(word: &str) -> String {
    for root in TEMP_ROOTS {
        if let Some(rest) = word.strip_prefix(root)
            && let Some((_, tail)) = rest.split_once('/')
        {
            return format!("<tmp>/{tail}");
        }
    }
    word.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> RunLimits {
        RunLimits {
            max_tool_calls: 10,
            max_model_calls: 4,
            max_wall: Duration::from_secs(600),
            stall_calls: 3,
            max_same_tool_family: 8,
            max_same_tool_family_success: 24,
            verify_attempts: 2,
        }
    }

    fn disabled() -> RunLimits {
        RunLimits {
            stall_calls: 1_000,
            max_same_tool_family: 0,
            max_same_tool_family_success: 0,
            ..limits()
        }
    }

    /// A fresh, empty workspace: its state never changes during a test.
    fn empty_workspace() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// Record a call whose family and signature are hand-built. Read-only
    /// classification is a property of the call, not of the family string, so
    /// the read-only cases below go through [`call`] instead.
    #[allow(clippy::too_many_arguments)]
    fn record(
        progress: &mut RunProgress,
        cwd: &Path,
        family: &str,
        signature: &str,
        result: &str,
        is_error: bool,
        changed: bool,
        verified: bool,
    ) {
        record_reading(
            progress, cwd, family, signature, result, is_error, changed, verified, false,
        );
    }

    /// Record a real call: the family, signature, and read-only flag come from
    /// the same functions the agent loop uses.
    fn call(
        progress: &mut RunProgress,
        cwd: &Path,
        name: &str,
        arguments: &serde_json::Value,
        result: &str,
        is_error: bool,
        changed: bool,
    ) {
        let raw = arguments.to_string();
        let family = tool_family(name, &raw);
        let signature = tool_signature(name, &raw);
        let read_only = call_read_only(name, &raw);
        record_reading(
            progress, cwd, &family, &signature, result, is_error, changed, false, read_only,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn record_reading(
        progress: &mut RunProgress,
        cwd: &Path,
        family: &str,
        signature: &str,
        result: &str,
        is_error: bool,
        changed: bool,
        verified: bool,
        read_only: bool,
    ) {
        let fingerprint = format!("{signature} | {result}");
        progress.record_tool_result(
            cwd,
            ToolOutcome {
                fingerprint: &fingerprint,
                family,
                signature,
                result,
                is_error,
                changed_file: changed,
                verified_ok: verified,
                read_only,
            },
        );
    }

    fn args(command: &str) -> String {
        serde_json::json!({"command": command}).to_string()
    }

    #[test]
    fn productive_model_checkpoint_renews() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for _ in 0..4 {
            progress.model_call_started();
        }
        record(
            &mut progress,
            dir.path(),
            "bash cat",
            "bash cat :: cat notes.txt",
            "new facts",
            false,
            false,
            false,
        );
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Renewed(StopReason::ModelBudget)
        );
        assert_eq!(progress.renewals, 1);
        assert_eq!(progress.model_calls, 4, "cumulative remains for reporting");
        assert_eq!(progress.epoch_model_calls, 0);
        assert_eq!(progress.model_checkpoint(), BudgetCheck::Ok);
    }

    #[test]
    fn checkpoint_without_progress_stops() {
        let mut progress = RunProgress::new(limits());
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
        assert_eq!(progress.renewals, 0);
    }

    #[test]
    fn repeated_results_are_not_progress() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..6 {
            record(
                &mut progress,
                dir.path(),
                "bash cat",
                &format!("bash cat :: cat notes-{index}.txt"),
                "same",
                false,
                false,
                false,
            );
        }
        // Four model calls after five repeats of the same result.
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
    }

    #[test]
    fn stale_progress_does_not_renew() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        record(
            &mut progress,
            dir.path(),
            "bash cat",
            "bash cat :: cat notes.txt",
            "useful",
            false,
            false,
            false,
        );
        for index in 0..6 {
            record(
                &mut progress,
                dir.path(),
                &format!("bash probe-{index}"),
                &format!("bash probe-{index} :: probe {index}"),
                &format!("noise-{index}"),
                true,
                false,
                false,
            );
        }
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
    }

    #[test]
    fn tool_checkpoint_renews_for_a_productive_batch() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for calls in 0..9 {
            record(
                &mut progress,
                dir.path(),
                "bash cat",
                &format!("bash cat :: cat notes-{calls}.txt"),
                &format!("result {calls}"),
                false,
                false,
                false,
            );
        }
        assert_eq!(
            progress.tool_checkpoint(2),
            BudgetCheck::Renewed(StopReason::ToolBudget)
        );
        assert_eq!(progress.epoch_tool_calls, 0);
        // The batch itself may cross the new checkpoint.
        assert_eq!(
            progress.tool_checkpoint(20),
            BudgetCheck::Exhausted(StopReason::ToolBudget)
        );
    }

    #[test]
    fn file_change_is_progress_even_when_the_result_repeats() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        record(
            &mut progress,
            dir.path(),
            "bash cat",
            "bash cat :: cat notes.txt",
            "ok",
            false,
            true,
            false,
        );
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Renewed(StopReason::ModelBudget)
        );
    }

    #[test]
    fn stall_needs_consecutive_identical_fingerprints() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for _ in 0..3 {
            record(
                &mut progress,
                dir.path(),
                "bash cat",
                "bash cat :: cat notes.txt",
                "r",
                false,
                false,
                false,
            );
        }
        assert!(progress.stalled());
        record(
            &mut progress,
            dir.path(),
            "bash cat",
            "bash cat :: cat other.txt",
            "r",
            false,
            false,
            false,
        );
        assert!(!progress.stalled());
    }

    #[test]
    fn file_change_resets_stall() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for _ in 0..2 {
            record(
                &mut progress,
                dir.path(),
                "bash cat",
                "bash cat :: cat notes.txt",
                "r",
                false,
                false,
                false,
            );
        }
        record(
            &mut progress,
            dir.path(),
            "bash cat",
            "bash cat :: cat notes.txt",
            "r",
            false,
            true,
            false,
        );
        assert!(!progress.stalled());
    }

    #[test]
    fn failed_results_are_never_progress() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..6 {
            record(
                &mut progress,
                dir.path(),
                &format!("family-{index}"),
                &format!("family-{index} :: call {index}"),
                &format!("distinct failure {index}"),
                true,
                false,
                false,
            );
        }
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
    }

    #[test]
    fn failing_calls_of_one_family_trip_the_family_guard() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..8 {
            record(
                &mut progress,
                dir.path(),
                "bash tesseract",
                &format!("bash tesseract :: tesseract receipt-{index}.png"),
                &format!("[exit 1] cannot read receipt-{index}.png"),
                true,
                false,
                false,
            );
            if index < 7 {
                assert!(!progress.stalled(), "call {} is below the limit", index + 1);
            }
        }
        assert!(progress.stalled());
        assert!(progress.stall_message().contains("bash tesseract"));
    }

    #[test]
    fn family_repeats_count_across_interleaved_calls() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..7 {
            record(
                &mut progress,
                dir.path(),
                "bash tesseract",
                &format!("bash tesseract :: tesseract receipt-{index}.png"),
                "failed",
                true,
                false,
                false,
            );
            record(
                &mut progress,
                dir.path(),
                "bash cat",
                &format!("bash cat :: cat note-{index}.txt"),
                &format!("content {index}"),
                false,
                false,
                false,
            );
        }
        assert!(
            !progress.stalled(),
            "an interleaved call must not clear the family count"
        );
        record(
            &mut progress,
            dir.path(),
            "bash tesseract",
            "bash tesseract :: tesseract receipt-8.png",
            "failed",
            true,
            false,
            false,
        );
        assert!(progress.stalled());
    }

    #[test]
    fn a_same_family_success_does_not_clear_its_failures() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..8 {
            record(
                &mut progress,
                dir.path(),
                "bash tesseract",
                &format!("bash tesseract :: tesseract page-{index}.png"),
                "failed",
                true,
                false,
                false,
            );
            // An exit-0 call that changed nothing is not progress either.
            record(
                &mut progress,
                dir.path(),
                "bash tesseract",
                &format!("bash tesseract :: tesseract page-{index}.png out-{index}.txt"),
                &format!("garbage {index}"),
                false,
                false,
                false,
            );
        }
        assert!(progress.stalled());
        assert!(progress.stall_message().contains("failing calls"));
    }

    #[test]
    fn identical_successful_calls_trip_the_repeat_guard() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..8 {
            record(
                &mut progress,
                dir.path(),
                "bash python3",
                "bash python3 :: python3 probe.py",
                &format!("slightly different garbage {index}"),
                false,
                false,
                false,
            );
            if index < 7 {
                assert!(!progress.stalled(), "call {} is below the limit", index + 1);
            }
        }
        assert!(progress.stalled());
        assert!(progress.stall_message().contains("same call was repeated"));
    }

    #[test]
    fn unproductive_successes_trip_at_their_own_limit() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..24 {
            record(
                &mut progress,
                dir.path(),
                "bash python3",
                &format!("bash python3 :: probe {index}"),
                &format!("garbage {index}"),
                false,
                false,
                false,
            );
            if index < 23 {
                assert!(!progress.stalled(), "call {} is below the limit", index + 1);
            }
        }
        assert!(progress.stalled());
        assert!(progress.stall_message().contains("24 successful calls"));
    }

    #[test]
    fn read_only_successes_never_trip_the_success_budget() {
        let dir = empty_workspace();
        for index in 0..40 {
            std::fs::write(dir.path().join(format!("note-{index}.txt")), "x").unwrap();
        }
        let mut progress = RunProgress::new(limits());
        for index in 0..40 {
            call(
                &mut progress,
                dir.path(),
                "bash",
                &serde_json::json!({"command": format!("cat note-{index}.txt")}),
                &format!("line {index}"),
                false,
                false,
            );
            call(
                &mut progress,
                dir.path(),
                "read",
                &serde_json::json!({"path": format!("note-{index}.txt")}),
                &format!("line {index}"),
                false,
                false,
            );
        }
        assert!(!progress.stalled());
    }

    #[test]
    fn read_only_classification_follows_the_registry() {
        let reads = |name: &str, args: serde_json::Value| call_read_only(name, &args.to_string());
        // Whole tools that only read.
        for tool in READ_ONLY_TOOLS {
            assert!(reads(tool, serde_json::json!({})), "{tool} only reads");
        }
        // Mixed tools: only the reading actions are exempt.
        for (tool, args) in [
            ("bg", serde_json::json!({"action": "logs", "id": "bg-1"})),
            ("bg", serde_json::json!({"action": "status"})),
            ("bg", serde_json::json!({"action": "list"})),
            ("todo", serde_json::json!({"action": "list"})),
            ("thread", serde_json::json!({"action": "status"})),
            ("finding", serde_json::json!({"action": "list"})),
            ("detect", serde_json::json!({"action": "validate"})),
            ("pr", serde_json::json!({"action": "checks"})),
            ("pr", serde_json::json!({"action": "review"})),
            ("git", serde_json::json!({"action": "status"})),
            ("git", serde_json::json!({"action": "log"})),
            ("git", serde_json::json!({"action": "branch", "op": "list"})),
        ] {
            assert!(reads(tool, args.clone()), "{tool} {args} reads");
        }
        for (tool, args) in [
            (
                "bg",
                serde_json::json!({"action": "start", "command": "make"}),
            ),
            ("bg", serde_json::json!({"action": "stop", "id": "bg-1"})),
            (
                "todo",
                serde_json::json!({"action": "plan", "tasks": ["a"]}),
            ),
            ("thread", serde_json::json!({"action": "note", "note": "x"})),
            (
                "finding",
                serde_json::json!({"action": "add", "title": "x"}),
            ),
            ("finding", serde_json::json!({"action": "report"})),
            (
                "detect",
                serde_json::json!({"action": "run", "target": "x"}),
            ),
            ("pr", serde_json::json!({"action": "draft"})),
            ("pr", serde_json::json!({"action": "comment", "body": "x"})),
            ("git", serde_json::json!({"action": "commit"})),
            (
                "git",
                serde_json::json!({"action": "branch", "op": "create"}),
            ),
            ("write", serde_json::json!({"path": "a", "content": "x"})),
            ("verify", serde_json::json!({})),
            ("bash", serde_json::json!({"command": "cargo test"})),
            (
                "bash",
                serde_json::json!({"command": "curl -X POST http://x/y"}),
            ),
        ] {
            assert!(!reads(tool, args.clone()), "{tool} {args} changes things");
        }
    }

    /// A job being polled with new output is a build advancing, not a loop:
    /// `bg` polls are read-only, and the coarse family no longer lumps them in
    /// with the `bg start` that produced the work.
    #[test]
    fn background_polls_with_new_output_are_not_a_stop() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..30 {
            call(
                &mut progress,
                dir.path(),
                "bg",
                &serde_json::json!({"action": "logs", "id": "bg-1", "lines": 40}),
                &format!("[{index}/600] Compiling crate_{index}"),
                false,
                false,
            );
            assert!(!progress.stalled(), "poll {} stopped the turn", index + 1);
        }
    }

    /// The success budget counts work that changed nothing; a turn that starts
    /// jobs and polls them is one unit of work, not 24 idle calls.
    #[test]
    fn mixed_background_actions_do_not_trip_the_success_budget() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..24 {
            let (name, args, result) = match index % 3 {
                0 => (
                    "bg",
                    serde_json::json!({"action": "start", "command": format!("job-{index}")}),
                    format!("started bg-{}", index / 3),
                ),
                1 => (
                    "bg",
                    serde_json::json!({"action": "status", "id": format!("bg-{}", index / 3)}),
                    format!("bg-{} running", index / 3),
                ),
                _ => (
                    "bg",
                    serde_json::json!({"action": "logs", "id": format!("bg-{}", index / 3)}),
                    format!("line {index}"),
                ),
            };
            call(
                &mut progress,
                dir.path(),
                name,
                &args,
                &result,
                false,
                false,
            );
        }
        assert!(!progress.stalled());
    }

    /// A long thread of distinct GETs is exploration, not an idle loop.
    #[test]
    fn distinct_curl_reads_are_not_idle_successes() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..40 {
            call(
                &mut progress,
                dir.path(),
                "bash",
                &serde_json::json!({"command": format!("curl -s https://docs.example/{index}.html")}),
                &format!("<html>page {index}</html>"),
                false,
                false,
            );
            assert!(!progress.stalled(), "GET {} stopped the turn", index + 1);
        }
    }

    /// A `curl` that uploads, posts, or writes a file is work, so it still
    /// counts against the family that changed nothing.
    #[test]
    fn a_curl_that_writes_still_counts_against_the_success_budget() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..24 {
            call(
                &mut progress,
                dir.path(),
                "bash",
                &serde_json::json!({"command": format!("curl -X POST -d item={index} https://api.example/items")}),
                &format!("created {index}"),
                false,
                false,
            );
            if index < 23 {
                assert!(!progress.stalled(), "call {} is below the limit", index + 1);
            }
        }
        assert!(progress.stalled());
        assert!(
            progress
                .stall_message()
                .contains("24 successful calls of bash curl"),
            "{}",
            progress.stall_message()
        );
    }

    /// Reading history is inspection: forty distinct `git show` calls are not
    /// forty idle successes.
    #[test]
    fn git_history_reads_are_not_idle_successes() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..25 {
            call(
                &mut progress,
                dir.path(),
                "bash",
                &serde_json::json!({"command": format!("git show HEAD~{index}:src/main.rs")}),
                &format!("commit {index} content"),
                false,
                false,
            );
            assert!(!progress.stalled(), "show {} stopped the turn", index + 1);
        }
    }

    /// A read of a pull request is inspection too.
    #[test]
    fn github_reads_are_not_idle_successes() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..30 {
            call(
                &mut progress,
                dir.path(),
                "bash",
                &serde_json::json!({"command": format!("gh pr view {index} --json title,state")}),
                &format!("PR {index}"),
                false,
                false,
            );
        }
        assert!(!progress.stalled());
    }

    #[test]
    fn fingerprint_reads_are_memoized_across_counters_and_calls() {
        static COMPUTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counted(cwd: &Path) -> Option<String> {
            COMPUTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            workspace_state(cwd)
        }
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        progress.stall.fingerprint = counted;
        // Distinct families, a few calls each: every counter needs a baseline
        // for the same unchanged workspace, so all of them share one reading.
        for family in 0..10 {
            for index in 0..3 {
                call(
                    &mut progress,
                    dir.path(),
                    "bash",
                    &serde_json::json!({"command": format!("probe-{family} {index}")}),
                    &format!("probe {family}/{index}"),
                    false,
                    false,
                );
            }
        }
        assert!(!progress.stalled());
        assert_eq!(
            COMPUTED.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one workspace reading serves every counter"
        );
    }

    #[test]
    fn fingerprint_reads_are_bounded_at_a_trip_decision() {
        static COMPUTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counted(cwd: &Path) -> Option<String> {
            COMPUTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            workspace_state(cwd)
        }
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        progress.stall.fingerprint = counted;
        for index in 0..30 {
            call(
                &mut progress,
                dir.path(),
                "bash",
                &serde_json::json!({"command": format!("python3 probe-{index}.py")}),
                &format!("garbage {index}"),
                false,
                false,
            );
        }
        assert!(progress.stalled(), "24 idle successes stop the turn");
        progress.recheck_workspace(dir.path());
        // One reading for the baseline, one to decide the trip, and one at the
        // batch re-check; the calls after the trip take none.
        assert!(
            COMPUTED.load(std::sync::atomic::Ordering::SeqCst) <= 3,
            "expected at most 3 workspace readings, got {}",
            COMPUTED.load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[test]
    fn a_reported_change_invalidates_the_memoized_fingerprint() {
        static COMPUTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        fn counted(cwd: &Path) -> Option<String> {
            COMPUTED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            workspace_state(cwd)
        }
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        progress.stall.fingerprint = counted;
        call(
            &mut progress,
            dir.path(),
            "bash",
            &serde_json::json!({"command": "python3 a.py"}),
            "one",
            false,
            false,
        );
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        call(
            &mut progress,
            dir.path(),
            "bash",
            &serde_json::json!({"command": "python3 b.py"}),
            "two",
            false,
            true,
        );
        call(
            &mut progress,
            dir.path(),
            "bash",
            &serde_json::json!({"command": "python3 c.py"}),
            "three",
            false,
            false,
        );
        assert_eq!(
            COMPUTED.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "the reported change forces a new reading on the next call"
        );
    }

    #[test]
    fn failing_reads_of_different_files_are_different_families() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..20 {
            let family = tool_family(
                "read",
                &serde_json::json!({"path": format!("missing-{index}.txt")}).to_string(),
            );
            record(
                &mut progress,
                dir.path(),
                &family,
                &family,
                "missing",
                true,
                false,
                false,
            );
        }
        assert!(!progress.stalled());
    }

    #[test]
    fn progress_in_the_same_batch_clears_a_family_trip() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..8 {
            record(
                &mut progress,
                dir.path(),
                "bash tesseract",
                &format!("bash tesseract :: tesseract page-{index}.png"),
                "failed",
                true,
                false,
                false,
            );
        }
        assert!(progress.stalled());
        record(
            &mut progress,
            dir.path(),
            "edit path=receipt.txt",
            "edit path=receipt.txt",
            "edited",
            false,
            true,
            false,
        );
        assert!(
            !progress.stalled(),
            "a real file change in the same batch is progress, not a stall"
        );
    }

    #[test]
    fn a_workspace_change_at_trip_time_restarts_the_counter() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..8 {
            record(
                &mut progress,
                dir.path(),
                "bash sed",
                &format!("bash sed :: sed -i s/{index}/x/ receipt.txt"),
                "failed",
                true,
                false,
                false,
            );
            if index == 3 {
                // A bash `sed -i` changes the file without the harness seeing
                // it; the trip is checked against the workspace the counter
                // started in, so real work restarts it instead of stopping.
                std::fs::write(dir.path().join("receipt.txt"), "fixed\n").unwrap();
            }
        }
        assert!(!progress.stalled());
    }

    #[test]
    fn recheck_clears_a_trip_after_a_later_workspace_change() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(limits());
        for index in 0..8 {
            record(
                &mut progress,
                dir.path(),
                "bash sed",
                &format!("bash sed :: sed -i s/{index}/x/ receipt.txt"),
                "no such file",
                true,
                false,
                false,
            );
        }
        assert!(progress.stalled(), "the trip fired during the batch");
        std::fs::write(dir.path().join("receipt.txt"), "fixed\n").unwrap();
        progress.recheck_workspace(dir.path());
        assert!(
            !progress.stalled(),
            "the workspace changed after the trip, so the batch made progress"
        );
    }

    #[test]
    fn zero_disables_both_family_budgets() {
        let dir = empty_workspace();
        let mut progress = RunProgress::new(disabled());
        for index in 0..40 {
            record(
                &mut progress,
                dir.path(),
                "bash python3",
                "bash python3 :: python3 probe.py",
                &format!("garbage {index}"),
                index % 2 == 0,
                false,
                false,
            );
        }
        assert!(!progress.stalled());
    }

    #[test]
    fn bash_families_ignore_paths_numbers_and_code_bodies() {
        assert_eq!(
            tool_family(
                "bash",
                &args("tesseract /tmp/a/receipt-1.png out-1.txt --psm 6")
            ),
            "bash tesseract"
        );
        assert_eq!(
            tool_family("bash", &args("tesseract /tmp/b/receipt-2.png out-2.txt")),
            "bash tesseract"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 -c \"from PIL import Image; print(1)\"")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 -c \"from PIL import Image; print(2); print(3)\"")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args(
                    "cd /tmp/scratch-7 && python3 - <<'EOF'\nfrom PIL import Image\nprint(1)\nEOF"
                )
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 << 'EOF'\nimport zlib\nprint(2)\nEOF")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family("bash", &args("cargo test --locked")),
            "bash cargo test"
        );
        assert_eq!(tool_family("bash", &args("git status")), "bash git status");
        assert_ne!(
            tool_family("bash", &args("git status")),
            tool_family("bash", &args("git diff"))
        );
        assert_eq!(tool_family("bash", &args("")), "bash shell");
    }

    #[test]
    fn bash_families_skip_flag_values() {
        let family = |command: &str| tool_family("bash", &args(command));
        assert_eq!(family("git -C repo status"), "bash git status");
        assert_eq!(family("git -c user.name=x status"), "bash git status");
        assert_eq!(family("sudo -u deploy cargo test"), "bash cargo test");
        assert_eq!(family("nice -n 5 cargo test"), "bash cargo test");
        assert_eq!(family("kubectl -n ns get pods"), "bash kubectl get");
        assert_eq!(family("timeout 30 cargo test"), "bash cargo test");
        assert_eq!(family("timeout -s KILL 30 cargo test"), "bash cargo test");
        // A here-doc without a space after `<<` is still a here-doc.
        assert_eq!(family("python3 <<\"EOF\"\nprint(1)\nEOF"), "bash python3");
        assert_eq!(family("cat<<'EOF'\nhello\nEOF"), "bash cat");
    }

    #[test]
    fn bash_signatures_hide_temp_paths_numbers_and_heredoc_bodies() {
        let signature = |command: &str| tool_signature("bash", &args(command));
        assert_eq!(
            signature("tesseract /tmp/a/receipt.png out.txt --psm 6"),
            signature("tesseract /tmp/b/receipt.png out.txt --psm 3")
        );
        assert_eq!(
            signature("python3 <<'EOF'\nprint(1)\nEOF"),
            signature("python3 <<'EOF'\nprint(2); print(3)\nEOF")
        );
        assert_ne!(
            signature("tesseract receipt-a.png"),
            signature("tesseract receipt-b.png")
        );
        assert_ne!(
            signature("cat note-1.txt"),
            signature("cat note-2.txt"),
            "digits inside a name are part of the target, not a counter"
        );
    }

    #[test]
    fn other_tool_families_keep_targets_and_drop_bodies() {
        let family = |value: serde_json::Value| tool_family("tests", &value.to_string());
        assert_ne!(
            family(serde_json::json!({"path": "a.rs"})),
            family(serde_json::json!({"path": "b.rs"}))
        );
        assert_eq!(
            family(serde_json::json!({"paths": ["b.rs", "a.rs"]})),
            family(serde_json::json!({"paths": ["a.rs", "b.rs"]}))
        );
        assert_eq!(
            tool_family(
                "edit",
                &serde_json::json!({"path": "a.rs", "old": "one", "new": "two"}).to_string()
            ),
            tool_family(
                "edit",
                &serde_json::json!({"path": "a.rs", "old": "three", "new": "four"}).to_string()
            )
        );
        assert_ne!(
            tool_family(
                "write",
                &serde_json::json!({"path": "/tmp/scratch-1/x.txt", "content": "a"}).to_string()
            ),
            tool_family(
                "write",
                &serde_json::json!({"path": "/tmp/scratch-2/x.txt", "content": "b"}).to_string()
            ),
            "temporary-path normalization is for bash families only"
        );
        assert_ne!(
            tool_signature(
                "write",
                &serde_json::json!({"path": "a.txt", "content": "one"}).to_string()
            ),
            tool_signature(
                "write",
                &serde_json::json!({"path": "a.txt", "content": "two"}).to_string()
            ),
            "different content is a different call, not a repeat"
        );
    }
}

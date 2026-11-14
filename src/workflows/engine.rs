//! Deterministic workflow steps: conditions, bounded retries, approval gates,
//! declared artifacts, and resume.
//!
//! The engine is the only place a step's outcome is decided. Conditions read a
//! fixed fact set, never code; steps cannot select tools or a profile; and an
//! approval is recorded whether it is accepted or denied.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::session::{EntryKind, Session};

/// A retry count above this is a configuration mistake, not persistence.
pub const MAX_RETRIES: u32 = 5;
/// Steps in one workflow.
pub const MAX_STEPS: usize = 32;
/// Declared artifacts in one workflow (top level plus one step).
pub const MAX_ARTIFACTS: usize = 16;

/// Why a step runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepRun {
    /// One agent turn with the step prompt.
    Prompt,
    /// Block until the user accepts or denies.
    Approval,
}

/// What a prompt step must prove before it counts as complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requirement {
    None,
    /// The step called `name` (and the exit code matched when given).
    Tool {
        name: String,
        exit: Option<i32>,
    },
    /// The step's own verify result matched.
    Verify {
        passed: bool,
    },
    /// The named artifact exists and is non-empty.
    Artifact(String),
}

/// One step in a workflow. Tool policy is not a field: a step cannot change
/// it, only the workflow-level profile and allowlist can.
#[derive(Debug, Clone)]
pub struct StepSpec {
    pub id: String,
    pub run: StepRun,
    /// Prompt text for `Prompt` steps.
    pub prompt: String,
    /// Question for `Approval` steps.
    pub message: Option<String>,
    pub when: Option<Condition>,
    pub require: Requirement,
    /// Extra attempts after the first.
    pub retry: u32,
    /// Artifacts that must exist and be non-empty after this step.
    pub artifacts: Vec<String>,
}

impl StepSpec {
    pub fn is_approval(&self) -> bool {
        self.run == StepRun::Approval
    }
}

/// A condition over the fixed fact set. The grammar is
/// `<fact> <op> <literal>` clauses joined by `and`; anything else is a
/// configuration error, so a typo never silently skips a step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    clauses: Vec<Clause>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Clause {
    Numeric {
        fact: NumericFact,
        op: CompareOp,
        value: i64,
    },
    Verify {
        op: CompareOp,
        value: VerifyState,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumericFact {
    ChangedFiles,
    OpenTasks,
    LastExit,
    StepsCompleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompareOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

impl Condition {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() {
            return Err("condition is empty; use e.g. `changed_files > 0`".into());
        }
        let mut clauses = Vec::new();
        for part in raw.split(" and ") {
            let part = part.trim();
            let tokens: Vec<&str> = part.split_whitespace().collect();
            if tokens.len() != 3 {
                return Err(format!(
                    "condition {part:?} is not `<fact> <op> <value>`; facts: changed_files, open_tasks, last_exit, steps_completed, last_verify"
                ));
            }
            let (fact, op, value) = (tokens[0], tokens[1], tokens[2]);
            let op = match op {
                "==" => CompareOp::Eq,
                "!=" => CompareOp::Ne,
                ">" => CompareOp::Gt,
                ">=" => CompareOp::Ge,
                "<" => CompareOp::Lt,
                "<=" => CompareOp::Le,
                other => {
                    return Err(format!(
                        "condition operator {other:?} is not supported; use ==, !=, >, >=, <, <="
                    ));
                }
            };
            if fact == "last_verify" {
                if !matches!(op, CompareOp::Eq | CompareOp::Ne) {
                    return Err("last_verify compares with == or != only (pass, fail, none)".into());
                }
                let value = match value {
                    "pass" => VerifyState::Pass,
                    "fail" => VerifyState::Fail,
                    "none" => VerifyState::None,
                    other => {
                        return Err(format!(
                            "last_verify value {other:?} is not pass, fail, or none"
                        ));
                    }
                };
                clauses.push(Clause::Verify { op, value });
                continue;
            }
            let fact = match fact {
                "changed_files" => NumericFact::ChangedFiles,
                "open_tasks" => NumericFact::OpenTasks,
                "last_exit" => NumericFact::LastExit,
                "steps_completed" => NumericFact::StepsCompleted,
                other => {
                    return Err(format!(
                        "unknown condition fact {other:?}; facts: changed_files, open_tasks, last_exit, steps_completed, last_verify"
                    ));
                }
            };
            let value: i64 = value
                .parse()
                .map_err(|_| format!("{value:?} is not a number"))?;
            if fact == NumericFact::LastExit && value < 0 {
                return Err("last_exit compares against a non-negative exit code".into());
            }
            clauses.push(Clause::Numeric { fact, op, value });
        }
        Ok(Self { clauses })
    }

    pub fn evaluate(&self, facts: &Facts) -> bool {
        self.clauses.iter().all(|clause| match clause {
            Clause::Numeric { fact, op, value } => {
                let actual = match fact {
                    NumericFact::ChangedFiles => facts.changed_files as i64,
                    NumericFact::OpenTasks => facts.open_tasks as i64,
                    NumericFact::LastExit => facts.last_exit.map(i64::from).unwrap_or(-1),
                    NumericFact::StepsCompleted => facts.steps_completed as i64,
                };
                compare(*op, actual, *value)
            }
            Clause::Verify { op, value } => match op {
                CompareOp::Eq => facts.last_verify == *value,
                CompareOp::Ne => facts.last_verify != *value,
                _ => false,
            },
        })
    }
}

fn compare(op: CompareOp, actual: i64, expected: i64) -> bool {
    match op {
        CompareOp::Eq => actual == expected,
        CompareOp::Ne => actual != expected,
        CompareOp::Gt => actual > expected,
        CompareOp::Ge => actual >= expected,
        CompareOp::Lt => actual < expected,
        CompareOp::Le => actual <= expected,
    }
}

/// Last verify result in the run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerifyState {
    Pass,
    Fail,
    #[default]
    None,
}

/// Fixed facts a condition may read. They come from session entries only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub changed_files: u64,
    pub last_verify: VerifyState,
    pub open_tasks: u64,
    pub last_exit: Option<i32>,
    pub steps_completed: u64,
}

impl Facts {
    /// Facts from the whole active branch: unique changed files, the last
    /// verify result, incomplete todo tasks, and the last bash exit code.
    pub fn from_session(session: &Session) -> Self {
        let mut files = HashSet::new();
        let mut calls: HashMap<String, String> = HashMap::new();
        let mut facts = Facts::default();
        for entry in session.branch_entries() {
            match &entry.kind {
                EntryKind::Custom(custom) => match custom.kind.as_str() {
                    "fs-change" => {
                        if let Some(path) = custom.payload.get("path").and_then(|v| v.as_str()) {
                            files.insert(path.to_string());
                        }
                    }
                    "verify-result" => {
                        facts.last_verify =
                            match custom.payload.get("passed").and_then(|v| v.as_bool()) {
                                Some(true) => VerifyState::Pass,
                                Some(false) => VerifyState::Fail,
                                None => facts.last_verify,
                            };
                    }
                    "todo-state" => {
                        facts.open_tasks = custom
                            .payload
                            .get("tasks")
                            .and_then(|v| v.as_array())
                            .map(|tasks| {
                                tasks
                                    .iter()
                                    .filter(|task| {
                                        task.get("status").and_then(|s| s.as_str())
                                            != Some("completed")
                                    })
                                    .count() as u64
                            })
                            .unwrap_or(0);
                    }
                    _ => {}
                },
                EntryKind::Assistant(assistant) => {
                    if let Some(tool_calls) = &assistant.tool_calls {
                        for call in tool_calls {
                            calls.insert(call.id.clone(), call.name.clone());
                        }
                    }
                }
                EntryKind::ToolResult(result)
                    if calls
                        .get(&result.call_id)
                        .is_some_and(|name| name == "bash") =>
                {
                    facts.last_exit = result
                        .details
                        .as_ref()
                        .and_then(|details| details.get("exit_code"))
                        .and_then(|code| code.as_i64())
                        .and_then(|code| i32::try_from(code).ok());
                }
                _ => {}
            }
        }
        facts.changed_files = files.len() as u64;
        facts
    }

    /// The last verify result after branch index `start`.
    pub fn verify_since(session: &Session, start: usize) -> VerifyState {
        let branch = session.branch_entries();
        for entry in branch.iter().skip(start).rev() {
            if let EntryKind::Custom(custom) = &entry.kind
                && custom.kind == "verify-result"
            {
                return match custom.payload.get("passed").and_then(|v| v.as_bool()) {
                    Some(true) => VerifyState::Pass,
                    Some(false) => VerifyState::Fail,
                    None => VerifyState::None,
                };
            }
        }
        VerifyState::None
    }
}

/// A tool result observed since a step began.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolObservation {
    pub name: String,
    pub is_error: bool,
    pub exit: Option<i32>,
}

/// Tool results after branch index `start`, in call order.
pub fn observations_since(session: &Session, start: usize) -> Vec<ToolObservation> {
    let branch = session.branch_entries();
    let mut calls: HashMap<String, String> = HashMap::new();
    for entry in &branch {
        if let EntryKind::Assistant(assistant) = &entry.kind
            && let Some(tool_calls) = &assistant.tool_calls
        {
            for call in tool_calls {
                calls.insert(call.id.clone(), call.name.clone());
            }
        }
    }
    let mut found = Vec::new();
    for entry in branch.iter().skip(start) {
        if let EntryKind::ToolResult(result) = &entry.kind
            && let Some(name) = calls.get(&result.call_id)
        {
            found.push(ToolObservation {
                name: name.clone(),
                is_error: result.is_error,
                exit: result
                    .details
                    .as_ref()
                    .and_then(|details| details.get("exit_code"))
                    .and_then(|code| code.as_i64())
                    .and_then(|code| i32::try_from(code).ok()),
            });
        }
    }
    found
}

/// The result of one prompt attempt.
#[derive(Debug, Clone)]
pub struct StepReport {
    pub status: StepStatus,
    /// Facts after the turn, cumulative for the workflow.
    pub facts: Facts,
    /// Tool results produced during this attempt.
    pub tools: Vec<ToolObservation>,
    /// Verify result produced during this attempt.
    pub verify: VerifyState,
    /// One line for the journal, e.g. the last assistant text.
    pub detail: Option<String>,
}

impl StepReport {
    pub fn succeeded(facts: Facts) -> Self {
        Self {
            status: StepStatus::Succeeded,
            facts,
            tools: Vec::new(),
            verify: VerifyState::None,
            detail: None,
        }
    }

    pub fn failed(code: &str, message: impl Into<String>) -> Self {
        Self {
            status: StepStatus::Failed {
                code: code.into(),
                message: message.into(),
            },
            facts: Facts::default(),
            tools: Vec::new(),
            verify: VerifyState::None,
            detail: None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum StepStatus {
    Succeeded,
    Failed { code: String, message: String },
    Cancelled,
}

/// What the user chose at an approval gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    Accepted,
    Denied,
    Cancelled,
}

/// The engine's only dependency on the outside world. In production this is
/// the agent loop; in tests it is a scripted executor.
#[async_trait]
pub trait StepExecutor {
    /// Run one prompt attempt. `previous` is the failure that caused a retry.
    async fn prompt(&mut self, step: &StepSpec, attempt: u32, previous: Option<&str>)
    -> StepReport;

    /// Ask the user. Must block until a decision or cancellation.
    async fn approve(&mut self, step: &StepSpec) -> Approval;
}

/// Where the run is stored, and whether to continue an earlier one.
#[derive(Debug, Clone)]
pub struct RunOptions {
    pub journal_path: PathBuf,
    /// Directory artifacts are resolved against.
    pub cwd: PathBuf,
    pub resume: bool,
    /// Session created for this run, stored for resume.
    pub session_id: Option<String>,
    pub session_file: Option<PathBuf>,
    /// Write the journal to disk. `--no-session` keeps it in memory.
    pub persist: bool,
    /// Redact secret-shaped values before they reach the journal.
    pub redact: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    Pending,
    Completed,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepRecord {
    pub id: String,
    pub status: StepState,
    pub attempts: u32,
    /// Hash of the step definition. A changed step is not treated as
    /// completed on resume.
    #[serde(default)]
    pub fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub step: String,
    pub decision: Approval,
    pub at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Durable state for one workflow run. A restart resumes from this file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub workflow: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    #[serde(default)]
    pub facts: Facts,
    #[serde(default)]
    pub steps: Vec<StepRecord>,
    #[serde(default)]
    pub approvals: Vec<ApprovalRecord>,
    pub status: RunState,
    pub started_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

impl Journal {
    fn new(workflow: &str) -> Self {
        let now = now();
        Self {
            workflow: workflow.to_string(),
            session_id: None,
            session_file: None,
            facts: Facts::default(),
            steps: Vec::new(),
            approvals: Vec::new(),
            status: RunState::Running,
            started_at: now.clone(),
            updated_at: now,
            failure: None,
        }
    }

    pub fn step(&self, id: &str) -> Option<&StepRecord> {
        self.steps.iter().find(|step| step.id == id)
    }

    fn step_mut(&mut self, id: &str) -> Option<&mut StepRecord> {
        self.steps.iter_mut().find(|step| step.id == id)
    }

    fn ensure_step(&mut self, id: &str) -> &mut StepRecord {
        if self.step(id).is_none() {
            self.steps.push(StepRecord {
                id: id.to_string(),
                status: StepState::Pending,
                attempts: 0,
                fingerprint: String::new(),
                detail: None,
                started_at: now(),
                finished_at: None,
            });
        }
        self.step_mut(id).expect("step was just inserted")
    }

    pub fn load(path: &Path) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path)
            .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        serde_json::from_str(&raw)
            .map_err(|error| format!("cannot parse {}: {error}", path.display()))
    }

    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        use std::io::Write;
        self.updated_at = now();
        let Some(parent) = path.parent() else {
            return Err(format!("{} has no parent directory", path.display()));
        };
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(self)
            .map_err(|error| format!("cannot encode workflow state: {error}"))?;
        let mut file = {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            options
                .open(&tmp)
                .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?
        };
        file.write_all(&body)
            .map_err(|error| format!("cannot write {}: {error}", tmp.display()))?;
        drop(file);
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("cannot replace {}: {error}", path.display()));
        }
        Ok(())
    }
}

/// A typed workflow failure. The code is stable and names the step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowFailure {
    pub code: &'static str,
    pub step: Option<String>,
    pub message: String,
}

impl WorkflowFailure {
    pub fn code(&self) -> &'static str {
        self.code
    }

    pub fn step(&self) -> Option<&str> {
        self.step.as_deref()
    }
}

impl std::fmt::Display for WorkflowFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.step {
            Some(step) => write!(f, "{} ({step}): {}", self.code, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

/// Emitted for the CLI and for tests. Serializes as a tagged JSON object.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    WorkflowStarted {
        workflow: String,
        steps: usize,
        resumed: bool,
    },
    StepStarted {
        step: String,
        attempt: u32,
    },
    StepSucceeded {
        step: String,
        attempts: u32,
    },
    StepSkipped {
        step: String,
        reason: String,
    },
    StepRetry {
        step: String,
        attempt: u32,
        reason: String,
    },
    ApprovalRequested {
        step: String,
        message: String,
    },
    ApprovalRecorded {
        step: String,
        decision: Approval,
    },
    ArtifactChecked {
        artifact: String,
        ok: bool,
    },
    WorkflowFinished {
        status: RunState,
    },
    Failure {
        code: String,
        step: Option<String>,
        message: String,
    },
}

pub struct RunReport {
    pub journal: Journal,
    pub events: Vec<Event>,
    pub failure: Option<WorkflowFailure>,
}

/// Run every step in order, persisting after each change. Completed and
/// skipped steps are not replayed when `resume` is set.
pub async fn execute(
    spec: &crate::workflows::WorkflowSpec,
    options: RunOptions,
    executor: &mut dyn StepExecutor,
    on_event: &mut dyn FnMut(&Event),
) -> RunReport {
    let redactor = crate::security::redact::Redactor::new(options.redact);
    let mut events = Vec::new();

    let mut journal = if options.resume && options.journal_path.is_file() {
        match Journal::load(&options.journal_path) {
            Ok(journal) if journal.workflow == spec.name => journal,
            Ok(journal) => {
                let message = format!(
                    "{} belongs to workflow {:?}, not {:?}",
                    options.journal_path.display(),
                    journal.workflow,
                    spec.name
                );
                // Never overwrite another workflow's resume state.
                let failure = WorkflowFailure {
                    code: "workflow_state_mismatch",
                    step: None,
                    message,
                };
                let mut events = events;
                push(
                    &mut events,
                    on_event,
                    Event::Failure {
                        code: failure.code.to_string(),
                        step: None,
                        message: redactor.redact_owned(&failure.message),
                    },
                );
                return RunReport {
                    journal: Journal::new(&spec.name),
                    events,
                    failure: Some(failure),
                };
            }
            Err(message) => {
                return fail(
                    Journal::new(&spec.name),
                    events,
                    WorkflowFailure {
                        code: "workflow_state_invalid",
                        step: None,
                        message,
                    },
                    &options,
                    &redactor,
                    on_event,
                );
            }
        }
    } else {
        Journal::new(&spec.name)
    };

    journal.workflow = spec.name.clone();
    journal.status = RunState::Running;
    journal.failure = None;
    if let Some(session_id) = &options.session_id {
        journal.session_id = Some(session_id.clone());
    }
    if let Some(session_file) = &options.session_file {
        journal.session_file = Some(session_file.display().to_string());
    }
    let mut facts = journal.facts.clone();
    facts.steps_completed = count_completed(&journal);
    journal.facts = facts.clone();

    push(
        &mut events,
        on_event,
        Event::WorkflowStarted {
            workflow: spec.name.clone(),
            steps: spec.steps.len(),
            resumed: options.resume,
        },
    );
    if let Err(message) = save_state(&mut journal, &options) {
        return fail(
            journal,
            events,
            state_failure(message),
            &options,
            &redactor,
            on_event,
        );
    }

    for step in &spec.steps {
        let fingerprint = step_fingerprint(step);
        let prior = journal.step(&step.id).cloned();
        let unchanged = prior
            .as_ref()
            .is_some_and(|record| record.fingerprint == fingerprint);
        if options.resume
            && unchanged
            && prior.as_ref().is_some_and(|record| {
                matches!(record.status, StepState::Completed | StepState::Skipped)
            })
        {
            push(
                &mut events,
                on_event,
                Event::StepSkipped {
                    step: step.id.clone(),
                    reason: "completed in an earlier run".into(),
                },
            );
            continue;
        }

        // A changed definition (or a fresh run) starts with a fresh budget.
        {
            let record = journal.ensure_step(&step.id);
            record.fingerprint = fingerprint.clone();
            if !unchanged {
                record.attempts = 0;
                record.status = StepState::Pending;
                record.detail = None;
            }
        }

        if step
            .when
            .as_ref()
            .is_some_and(|condition| !condition.evaluate(&facts))
        {
            let record = journal.ensure_step(&step.id);
            record.status = StepState::Skipped;
            record.detail = Some("condition was false".into());
            record.finished_at = Some(now());
            facts.steps_completed = count_completed(&journal);
            journal.facts = facts.clone();
            if let Err(message) = save_state(&mut journal, &options) {
                return fail(
                    journal,
                    events,
                    state_failure(message),
                    &options,
                    &redactor,
                    on_event,
                );
            }
            push(
                &mut events,
                on_event,
                Event::StepSkipped {
                    step: step.id.clone(),
                    reason: "condition was false".into(),
                },
            );
            continue;
        }

        if step.is_approval() {
            let message = step.message.clone().unwrap_or_default();
            push(
                &mut events,
                on_event,
                Event::ApprovalRequested {
                    step: step.id.clone(),
                    message: message.clone(),
                },
            );
            let decision = executor.approve(step).await;
            journal.approvals.push(ApprovalRecord {
                step: step.id.clone(),
                decision,
                at: now(),
            });
            push(
                &mut events,
                on_event,
                Event::ApprovalRecorded {
                    step: step.id.clone(),
                    decision,
                },
            );
            match decision {
                Approval::Accepted => {
                    if let Some(reason) = missing_artifacts(&step.artifacts, &options.cwd) {
                        let record = journal.ensure_step(&step.id);
                        record.status = StepState::Failed;
                        record.attempts = 1;
                        record.detail = Some("accepted, but an artifact is missing".into());
                        record.finished_at = Some(now());
                        journal.facts = facts.clone();
                        return fail(
                            journal,
                            events,
                            WorkflowFailure {
                                code: "workflow_artifact_missing",
                                step: Some(step.id.clone()),
                                message: reason,
                            },
                            &options,
                            &redactor,
                            on_event,
                        );
                    }
                    for artifact in &step.artifacts {
                        push(
                            &mut events,
                            on_event,
                            Event::ArtifactChecked {
                                artifact: artifact.clone(),
                                ok: true,
                            },
                        );
                    }
                    let record = journal.ensure_step(&step.id);
                    record.status = StepState::Completed;
                    record.attempts = 1;
                    record.detail = Some("accepted".into());
                    record.finished_at = Some(now());
                    facts.steps_completed = count_completed(&journal);
                    journal.facts = facts.clone();
                    if let Err(message) = save_state(&mut journal, &options) {
                        return fail(
                            journal,
                            events,
                            state_failure(message),
                            &options,
                            &redactor,
                            on_event,
                        );
                    }
                }
                Approval::Denied => {
                    let record = journal.ensure_step(&step.id);
                    record.status = StepState::Failed;
                    record.attempts = 1;
                    record.detail = Some("denied".into());
                    record.finished_at = Some(now());
                    journal.facts = facts.clone();
                    return fail(
                        journal,
                        events,
                        WorkflowFailure {
                            code: "workflow_approval_denied",
                            step: Some(step.id.clone()),
                            message: message.clone(),
                        },
                        &options,
                        &redactor,
                        on_event,
                    );
                }
                Approval::Cancelled => {
                    let record = journal.ensure_step(&step.id);
                    record.status = StepState::Pending;
                    record.detail = Some("cancelled".into());
                    return cancel(journal, events, Some(step.id.clone()), &options, on_event);
                }
            }
            continue;
        }

        // A step left Pending by a crash continues its budget. A Failed step
        // is only resumed by a human `--resume`, which starts a fresh budget
        // for this invocation; the journal keeps the cumulative attempt count.
        let mut attempt = if options.resume
            && unchanged
            && prior
                .as_ref()
                .is_some_and(|record| record.status == StepState::Pending)
        {
            prior.as_ref().map(|record| record.attempts).unwrap_or(0)
        } else {
            0
        };
        if attempt > step.retry {
            let record = journal.ensure_step(&step.id);
            record.status = StepState::Failed;
            record.finished_at = Some(now());
            journal.facts = facts.clone();
            return fail(
                journal,
                events,
                WorkflowFailure {
                    code: "workflow_step_failed",
                    step: Some(step.id.clone()),
                    message: format!(
                        "retry budget exhausted after {attempt} attempt{}; change the step or start a new run",
                        if attempt == 1 { "" } else { "s" }
                    ),
                },
                &options,
                &redactor,
                on_event,
            );
        }

        let mut previous: Option<String> = None;
        loop {
            attempt += 1;
            push(
                &mut events,
                on_event,
                Event::StepStarted {
                    step: step.id.clone(),
                    attempt,
                },
            );
            {
                let record = journal.ensure_step(&step.id);
                record.attempts = record.attempts.saturating_add(1);
                record.fingerprint = fingerprint.clone();
            }
            if let Err(message) = save_state(&mut journal, &options) {
                return fail(
                    journal,
                    events,
                    state_failure(message),
                    &options,
                    &redactor,
                    on_event,
                );
            }

            let report = executor.prompt(step, attempt, previous.as_deref()).await;
            facts = report.facts.clone();
            facts.steps_completed = count_completed(&journal);

            let unmet = match &report.status {
                StepStatus::Cancelled => {
                    return cancel(journal, events, Some(step.id.clone()), &options, on_event);
                }
                StepStatus::Failed { code, message } => Some(format!("{code}: {message}")),
                StepStatus::Succeeded => requirement_failure(step, &report, &options.cwd),
            };

            let artifact_failure = if unmet.is_none() {
                missing_artifacts(&step.artifacts, &options.cwd)
            } else {
                None
            };

            match (unmet, artifact_failure) {
                (None, None) => {
                    for artifact in &step.artifacts {
                        push(
                            &mut events,
                            on_event,
                            Event::ArtifactChecked {
                                artifact: artifact.clone(),
                                ok: true,
                            },
                        );
                    }
                    let record = journal.ensure_step(&step.id);
                    record.status = StepState::Completed;
                    record.detail = report
                        .detail
                        .as_deref()
                        .map(|detail| redactor.redact_owned(detail));
                    record.finished_at = Some(now());
                    facts.steps_completed = count_completed(&journal);
                    journal.facts = facts.clone();
                    if let Err(message) = save_state(&mut journal, &options) {
                        return fail(
                            journal,
                            events,
                            state_failure(message),
                            &options,
                            &redactor,
                            on_event,
                        );
                    }
                    push(
                        &mut events,
                        on_event,
                        Event::StepSucceeded {
                            step: step.id.clone(),
                            attempts: attempt,
                        },
                    );
                    break;
                }
                (unmet, artifact) => {
                    let reason = unmet
                        .or(artifact)
                        .unwrap_or_else(|| "step requirement unmet".into());
                    if attempt < step.retry + 1 {
                        push(
                            &mut events,
                            on_event,
                            Event::StepRetry {
                                step: step.id.clone(),
                                attempt,
                                reason: reason.clone(),
                            },
                        );
                        previous = Some(reason);
                        continue;
                    }
                    let record = journal.ensure_step(&step.id);
                    record.status = StepState::Failed;
                    record.detail = Some(redactor.redact_owned(&reason));
                    record.finished_at = Some(now());
                    journal.facts = facts.clone();
                    return fail(
                        journal,
                        events,
                        WorkflowFailure {
                            code: "workflow_step_failed",
                            step: Some(step.id.clone()),
                            message: reason,
                        },
                        &options,
                        &redactor,
                        on_event,
                    );
                }
            }
        }
    }

    // Every artifact that belongs to a completed step must still exist, and
    // top-level artifacts are checked last. A later step cannot delete an
    // earlier step's declared output and still report success.
    let mut required: Vec<String> = spec.artifacts.clone();
    for step in &spec.steps {
        if journal
            .step(&step.id)
            .is_some_and(|record| record.status == StepState::Completed)
        {
            required.extend(step.artifacts.iter().cloned());
        }
    }
    let mut seen = std::collections::HashSet::new();
    required.retain(|artifact| seen.insert(artifact.clone()));
    let missing: Vec<&String> = required
        .iter()
        .filter(|artifact| !artifact_ok(&options.cwd, artifact))
        .collect();
    for artifact in &required {
        push(
            &mut events,
            on_event,
            Event::ArtifactChecked {
                artifact: artifact.clone(),
                ok: artifact_ok(&options.cwd, artifact),
            },
        );
    }
    if !missing.is_empty() {
        let names: Vec<&str> = missing.iter().map(|name| name.as_str()).collect();
        return fail(
            journal,
            events,
            WorkflowFailure {
                code: "workflow_artifact_missing",
                step: None,
                message: format!("declared artifacts missing or empty: {}", names.join(", ")),
            },
            &options,
            &redactor,
            on_event,
        );
    }

    journal.status = RunState::Completed;
    journal.facts = facts;
    if let Err(message) = save_state(&mut journal, &options) {
        return fail(
            journal,
            events,
            state_failure(message),
            &options,
            &redactor,
            on_event,
        );
    }
    push(
        &mut events,
        on_event,
        Event::WorkflowFinished {
            status: RunState::Completed,
        },
    );
    RunReport {
        journal,
        events,
        failure: None,
    }
}

fn count_completed(journal: &Journal) -> u64 {
    journal
        .steps
        .iter()
        .filter(|record| record.status == StepState::Completed)
        .count() as u64
}

/// Hash the fields that decide what a step does. A resumed step whose
/// definition changed is rerun instead of being skipped by id alone.
pub(crate) fn step_fingerprint(step: &StepSpec) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(format!(
        "{:?}|{}|{}|{:?}|{:?}|{}|{:?}",
        step.run,
        step.prompt,
        step.message.as_deref().unwrap_or(""),
        step.when,
        step.require,
        step.retry,
        step.artifacts
    ));
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn save_state(journal: &mut Journal, options: &RunOptions) -> Result<(), String> {
    if options.persist {
        journal.save(&options.journal_path)
    } else {
        Ok(())
    }
}

fn requirement_failure(step: &StepSpec, report: &StepReport, cwd: &Path) -> Option<String> {
    match &step.require {
        Requirement::None => None,
        Requirement::Tool { name, exit } => {
            let Some(observed) = report.tools.iter().rev().find(|tool| &tool.name == name) else {
                return Some(format!("the step did not call tool {name}"));
            };
            if let Some(expected) = exit {
                match observed.exit {
                    Some(actual) if actual == *expected => {}
                    Some(actual) => {
                        return Some(format!("{name} exited {actual}, required {expected}"));
                    }
                    None => {
                        return Some(format!(
                            "{name} did not report an exit code, required {expected}"
                        ));
                    }
                }
            } else if observed.is_error {
                return Some(format!("{name} failed"));
            }
            None
        }
        Requirement::Verify { passed } => {
            let expected = if *passed {
                VerifyState::Pass
            } else {
                VerifyState::Fail
            };
            if report.verify == expected {
                None
            } else {
                Some(format!(
                    "verify was {:?}, required {:?}",
                    report.verify, expected
                ))
            }
        }
        Requirement::Artifact(name) => {
            if artifact_ok(cwd, name) {
                None
            } else {
                Some(format!("artifact {name:?} is missing or empty"))
            }
        }
    }
}

/// True when the artifact exists under `cwd`, is a non-empty file, and still
/// resolves inside `cwd` after symlinks.
pub fn artifact_ok(cwd: &Path, name: &str) -> bool {
    if validate_artifact(name).is_err() {
        return false;
    }
    let Ok(root) = cwd.canonicalize() else {
        return false;
    };
    let Ok(real) = cwd.join(name).canonicalize() else {
        return false;
    };
    real.starts_with(&root)
        && real.is_file()
        && std::fs::metadata(&real)
            .map(|meta| meta.len() > 0)
            .unwrap_or(false)
}

fn missing_artifacts(artifacts: &[String], cwd: &Path) -> Option<String> {
    let missing: Vec<&str> = artifacts
        .iter()
        .filter(|name| !artifact_ok(cwd, name))
        .map(String::as_str)
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(format!(
            "declared artifact{} {} missing or empty",
            if missing.len() == 1 { "" } else { "s" },
            missing.join(", ")
        ))
    }
}

fn state_failure(message: String) -> WorkflowFailure {
    WorkflowFailure {
        code: "workflow_state_failed",
        step: None,
        message,
    }
}

fn push(events: &mut Vec<Event>, on_event: &mut dyn FnMut(&Event), event: Event) {
    on_event(&event);
    events.push(event);
}

fn fail(
    mut journal: Journal,
    mut events: Vec<Event>,
    mut failure: WorkflowFailure,
    options: &RunOptions,
    redactor: &crate::security::redact::Redactor,
    on_event: &mut dyn FnMut(&Event),
) -> RunReport {
    failure.message = redactor.redact_owned(&failure.message);
    journal.status = RunState::Failed;
    journal.failure = Some(failure.to_string());
    let _ = save_state(&mut journal, options);
    push(
        &mut events,
        on_event,
        Event::Failure {
            code: failure.code.to_string(),
            step: failure.step.clone(),
            message: failure.message.clone(),
        },
    );
    RunReport {
        journal,
        events,
        failure: Some(failure),
    }
}

fn cancel(
    mut journal: Journal,
    mut events: Vec<Event>,
    step: Option<String>,
    options: &RunOptions,
    on_event: &mut dyn FnMut(&Event),
) -> RunReport {
    journal.status = RunState::Cancelled;
    journal.failure = Some("workflow cancelled".into());
    let _ = save_state(&mut journal, options);
    push(
        &mut events,
        on_event,
        Event::WorkflowFinished {
            status: RunState::Cancelled,
        },
    );
    RunReport {
        journal,
        events,
        failure: Some(WorkflowFailure {
            code: "workflow_cancelled",
            step,
            message: "the workflow was cancelled".into(),
        }),
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Validate and normalize a step definition shared by file and bundled
/// workflows.
pub fn validate_step(
    id: &str,
    run: StepRun,
    prompt: &str,
    message: Option<&str>,
    when: Option<&str>,
    retry: u32,
    artifacts: &[String],
) -> Result<(Option<Condition>, String, Vec<String>), String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("step has an empty id".into());
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(format!(
            "step id {id:?} must use lowercase letters, digits, - or _"
        ));
    }
    if retry > MAX_RETRIES {
        return Err(format!(
            "step {id:?} asks for {retry} retries; the maximum is {MAX_RETRIES}"
        ));
    }
    match run {
        StepRun::Prompt if prompt.trim().is_empty() => {
            return Err(format!("prompt step {id:?} has no prompt"));
        }
        StepRun::Approval if message.is_none_or(|message| message.trim().is_empty()) => {
            return Err(format!("approval step {id:?} has no message"));
        }
        _ => {}
    }
    let condition = when.map(Condition::parse).transpose()?;
    for artifact in artifacts {
        validate_artifact(artifact)?;
    }
    if artifacts.len() > MAX_ARTIFACTS {
        return Err(format!(
            "step {id:?} declares {} artifacts; the maximum is {MAX_ARTIFACTS}",
            artifacts.len()
        ));
    }
    Ok((condition, prompt.trim().to_string(), artifacts.to_vec()))
}

/// Artifacts are confined to the working directory: a workflow cannot declare
/// `/etc/shadow` or `../outside` and have the engine read it.
pub fn validate_artifact(name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("artifact name is empty".into());
    }
    let path = Path::new(name);
    if path.is_absolute() {
        return Err(format!(
            "artifact {name:?} must be relative to the working directory"
        ));
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => {
                return Err(format!(
                    "artifact {name:?} must not contain . or .. segments"
                ));
            }
        }
    }
    Ok(())
}

/// True when `name` resolves under `cwd`.
pub fn artifact_within(cwd: &Path, name: &str) -> bool {
    validate_artifact(name).is_ok() && cwd.join(name).starts_with(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Session, SessionData};
    use crate::workflows::{StepSpec, WorkflowSpec};

    fn spec(steps: Vec<StepSpec>, artifacts: Vec<String>) -> WorkflowSpec {
        WorkflowSpec {
            name: "release".into(),
            description: None,
            profile: None,
            prompt: String::new(),
            skills: Vec::new(),
            tools: Vec::new(),
            steps,
            artifacts,
        }
    }

    fn prompt_step(id: &str, retry: u32, require: Requirement) -> StepSpec {
        StepSpec {
            id: id.into(),
            run: StepRun::Prompt,
            prompt: format!("do {id}"),
            message: None,
            when: None,
            require,
            retry,
            artifacts: Vec::new(),
        }
    }

    fn approval_step(id: &str) -> StepSpec {
        StepSpec {
            id: id.into(),
            run: StepRun::Approval,
            prompt: String::new(),
            message: Some("push?".into()),
            when: None,
            require: Requirement::None,
            retry: 0,
            artifacts: Vec::new(),
        }
    }

    #[derive(Clone)]
    enum Action {
        /// Remove a file on the Nth prompt call (1-based).
        Remove { path: PathBuf, at: usize },
    }

    struct Fake {
        /// One report per prompt attempt, in order. Exhausted means success.
        reports: Vec<StepReport>,
        calls: Vec<(String, u32, Option<String>)>,
        approvals: Vec<Approval>,
        approval_calls: Vec<String>,
        actions: Vec<Action>,
        prompts: usize,
    }

    impl Fake {
        fn new(reports: Vec<StepReport>, approvals: Vec<Approval>) -> Self {
            Self {
                reports,
                calls: Vec::new(),
                approvals,
                approval_calls: Vec::new(),
                actions: Vec::new(),
                prompts: 0,
            }
        }

        fn with_action(mut self, action: Action) -> Self {
            self.actions.push(action);
            self
        }
    }

    #[async_trait]
    impl StepExecutor for Fake {
        async fn prompt(
            &mut self,
            step: &StepSpec,
            attempt: u32,
            previous: Option<&str>,
        ) -> StepReport {
            self.calls
                .push((step.id.clone(), attempt, previous.map(str::to_string)));
            self.prompts += 1;
            if self.actions.first().is_some_and(
                |action| matches!(action, Action::Remove { at, .. } if *at == self.prompts),
            ) {
                let action = self.actions.remove(0);
                match action {
                    Action::Remove { path, .. } => {
                        let _ = std::fs::remove_file(path);
                    }
                }
            }
            if self.reports.is_empty() {
                StepReport::succeeded(Facts::default())
            } else {
                self.reports.remove(0)
            }
        }

        async fn approve(&mut self, step: &StepSpec) -> Approval {
            self.approval_calls.push(step.id.clone());
            if self.approvals.is_empty() {
                Approval::Accepted
            } else {
                self.approvals.remove(0)
            }
        }
    }

    fn temp_journal() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/run.json");
        (dir, path)
    }

    fn options(path: PathBuf, cwd: &Path, resume: bool) -> RunOptions {
        RunOptions {
            journal_path: path,
            cwd: cwd.to_path_buf(),
            resume,
            session_id: Some("s1".into()),
            session_file: None,
            persist: true,
            redact: false,
        }
    }

    #[tokio::test]
    async fn a_failing_required_step_ends_the_workflow_and_names_it() {
        let (dir, path) = temp_journal();
        let mut fake = Fake::new(
            vec![StepReport {
                status: StepStatus::Succeeded,
                facts: Facts::default(),
                tools: vec![ToolObservation {
                    name: "bash".into(),
                    is_error: true,
                    exit: Some(1),
                }],
                verify: VerifyState::None,
                detail: None,
            }],
            vec![],
        );
        let spec = spec(
            vec![prompt_step(
                "reproduce",
                0,
                Requirement::Tool {
                    name: "bash".into(),
                    exit: Some(0),
                },
            )],
            vec![],
        );
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        let failure = report.failure.unwrap();
        assert_eq!(failure.code, "workflow_step_failed");
        assert_eq!(failure.step.as_deref(), Some("reproduce"));
        assert!(failure.message.contains("bash exited 1"));
        assert_eq!(Journal::load(&path).unwrap().status, RunState::Failed);
    }

    #[tokio::test]
    async fn a_retry_runs_at_most_the_configured_times_and_logs_each_attempt() {
        let (dir, path) = temp_journal();
        let mut fake = Fake::new(
            vec![
                StepReport::failed("verify_failed", "first"),
                StepReport::failed("verify_failed", "second"),
                StepReport::succeeded(Facts::default()),
            ],
            vec![],
        );
        let spec = spec(vec![prompt_step("test", 2, Requirement::None)], vec![]);
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert!(report.failure.is_none(), "{:?}", report.failure);
        assert_eq!(fake.calls.len(), 3);
        assert_eq!(
            fake.calls.iter().map(|(_, a, _)| *a).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(fake.calls[2].2.as_deref().unwrap().contains("second"));
        let retries: Vec<_> = report
            .events
            .iter()
            .filter(|event| matches!(event, Event::StepRetry { .. }))
            .collect();
        assert_eq!(retries.len(), 2);
        assert_eq!(
            Journal::load(&path).unwrap().step("test").unwrap().attempts,
            3
        );

        // One more failure than retries is fatal, with no extra attempt.
        let (dir, path) = temp_journal();
        let mut fake = Fake::new(
            vec![
                StepReport::failed("verify_failed", "a"),
                StepReport::failed("verify_failed", "b"),
                StepReport::failed("verify_failed", "c"),
                StepReport::succeeded(Facts::default()),
            ],
            vec![],
        );
        let report = execute(
            &spec,
            options(path, dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert_eq!(fake.calls.len(), 3);
        assert_eq!(report.failure.unwrap().code, "workflow_step_failed");
    }

    #[tokio::test]
    async fn an_approval_gate_blocks_and_records_accept_or_deny() {
        let (dir, path) = temp_journal();
        let mut fake = Fake::new(vec![], vec![Approval::Accepted]);
        let spec = spec(
            vec![
                prompt_step("work", 0, Requirement::None),
                approval_step("push"),
            ],
            vec![],
        );
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert!(report.failure.is_none());
        assert_eq!(fake.approval_calls, vec!["push"]);
        let journal = Journal::load(&path).unwrap();
        assert_eq!(journal.approvals.len(), 1);
        assert_eq!(journal.approvals[0].decision, Approval::Accepted);
        assert_eq!(journal.step("push").unwrap().status, StepState::Completed);

        let (dir, path) = temp_journal();
        let mut fake = Fake::new(vec![], vec![Approval::Denied]);
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        let failure = report.failure.unwrap();
        assert_eq!(failure.code, "workflow_approval_denied");
        assert_eq!(failure.step.as_deref(), Some("push"));
        let journal = Journal::load(&path).unwrap();
        assert_eq!(journal.approvals[0].decision, Approval::Denied);
        assert_eq!(journal.step("push").unwrap().status, StepState::Failed);
    }

    #[tokio::test]
    async fn a_missing_artifact_fails_before_success() {
        let (dir, path) = temp_journal();
        let mut fake = Fake::new(vec![], vec![]);
        let spec = spec(
            vec![prompt_step("work", 0, Requirement::None)],
            vec!["out/report.md".into()],
        );
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        let failure = report.failure.unwrap();
        assert_eq!(failure.code, "workflow_artifact_missing");
        assert!(failure.message.contains("out/report.md"));
        assert_eq!(Journal::load(&path).unwrap().status, RunState::Failed);

        // An empty file is not an artifact.
        std::fs::create_dir_all(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("out/report.md"), "").unwrap();
        let (_, path) = temp_journal();
        let mut fake = Fake::new(vec![], vec![]);
        let report = execute(
            &spec,
            options(path, dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert_eq!(report.failure.unwrap().code, "workflow_artifact_missing");

        std::fs::write(dir.path().join("out/report.md"), "done\n").unwrap();
        let (_, path) = temp_journal();
        let report = execute(
            &spec,
            options(path, dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert!(report.failure.is_none());
    }

    #[tokio::test]
    async fn resume_starts_at_the_first_incomplete_step() {
        let (dir, path) = temp_journal();
        let spec = spec(
            vec![
                prompt_step("one", 0, Requirement::None),
                prompt_step("two", 0, Requirement::None),
                prompt_step("three", 0, Requirement::None),
            ],
            vec![],
        );
        let mut fake = Fake::new(
            vec![
                StepReport::succeeded(Facts::default()),
                StepReport::failed("turn_failed", "boom"),
            ],
            vec![],
        );
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert_eq!(report.failure.unwrap().code, "workflow_step_failed");
        assert_eq!(
            fake.calls
                .iter()
                .map(|(id, _, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two"]
        );

        let mut fake = Fake::new(vec![StepReport::succeeded(Facts::default())], vec![]);
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), true),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert!(report.failure.is_none(), "{:?}", report.failure);
        assert_eq!(
            fake.calls
                .iter()
                .map(|(id, _, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["two", "three"]
        );
        assert!(report.events.iter().any(|event| matches!(
            event,
            Event::StepSkipped { step, .. } if step == "one"
        )));
    }

    #[test]
    fn conditions_read_only_fixed_facts() {
        let facts = Facts {
            changed_files: 2,
            last_verify: VerifyState::Pass,
            open_tasks: 0,
            last_exit: Some(0),
            steps_completed: 1,
        };
        assert!(
            Condition::parse("changed_files > 0")
                .unwrap()
                .evaluate(&facts)
        );
        assert!(
            !Condition::parse("changed_files == 0")
                .unwrap()
                .evaluate(&facts)
        );
        assert!(
            Condition::parse("last_verify == pass")
                .unwrap()
                .evaluate(&facts)
        );
        assert!(
            Condition::parse("changed_files > 0 and open_tasks == 0")
                .unwrap()
                .evaluate(&facts)
        );
        assert!(Condition::parse("last_exit == 0").unwrap().evaluate(&facts));

        for bad in [
            "changed_files > 0 || true",
            "$(touch /tmp/x)",
            "std::process::Command::new(\"sh\")",
            "changed_files >",
            "unknown_fact > 0",
            "last_verify > pass",
            "changed_files = 1",
        ] {
            assert!(Condition::parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn artifacts_cannot_escape_the_working_directory() {
        let cwd = Path::new("/work/project");
        assert!(!artifact_ok(cwd, "out/report.md"));
        for name in ["/etc/passwd", "../outside", "./x", "a/../../b"] {
            assert!(
                validate_artifact(name).is_err(),
                "{name:?} should be rejected"
            );
        }
        assert!(validate_artifact("out/report.md").is_ok());
        assert!(artifact_within(cwd, "out/report.md"));
        assert!(!artifact_within(cwd, "/etc/passwd"));
    }

    #[test]
    fn facts_come_from_the_session_branch() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = crate::paths::Paths::discover(dir.path()).unwrap();
        paths.config_dir = dir.path().join("config");
        paths.data_dir = dir.path().join("data");
        let mut session = Session::create(
            &paths,
            SessionData {
                cwd: dir.path().display().to_string(),
                endpoint: "http://x".into(),
                model: "m".into(),
                profile: "coding".into(),
                app: "test".into(),
                redact: true,
                context: None,
            },
            true,
        )
        .unwrap();
        session
            .append(EntryKind::Custom(crate::session::CustomData::new(
                "fs-change",
            )))
            .unwrap();
        // The helper above writes no payload; use a real one.
        let mut change = crate::session::CustomData::new("fs-change");
        change
            .payload
            .insert("path".into(), serde_json::json!("src/a.rs"));
        session.append(EntryKind::Custom(change)).unwrap();
        let mut verify = crate::session::CustomData::new("verify-result");
        verify
            .payload
            .insert("passed".into(), serde_json::json!(true));
        session.append(EntryKind::Custom(verify)).unwrap();
        let mut todo = crate::session::CustomData::new("todo-state");
        todo.payload.insert(
            "tasks".into(),
            serde_json::json!([
                {"status": "completed"},
                {"status": "in_progress"}
            ]),
        );
        session.append(EntryKind::Custom(todo)).unwrap();
        let facts = Facts::from_session(&session);
        assert_eq!(facts.changed_files, 1);
        assert_eq!(facts.last_verify, VerifyState::Pass);
        assert_eq!(facts.open_tasks, 1);
        assert_eq!(Facts::verify_since(&session, 0), VerifyState::Pass);
    }

    #[tokio::test]
    async fn a_resume_file_for_another_workflow_is_left_alone() {
        let (dir, path) = temp_journal();
        let mut other = Journal::new("other");
        other.status = RunState::Completed;
        other.save(&path).unwrap();
        let mut fake = Fake::new(vec![], vec![]);
        let spec = spec(vec![prompt_step("one", 0, Requirement::None)], vec![]);
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), true),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert_eq!(report.failure.unwrap().code, "workflow_state_mismatch");
        let untouched = Journal::load(&path).unwrap();
        assert_eq!(untouched.workflow, "other");
        assert_eq!(untouched.status, RunState::Completed);
    }

    #[tokio::test]
    async fn steps_completed_counts_finished_steps_across_prompt_reports() {
        let (dir, path) = temp_journal();
        let mut third = prompt_step("three", 0, Requirement::None);
        third.when = Some(Condition::parse("steps_completed >= 2").unwrap());
        let spec = spec(
            vec![
                prompt_step("one", 0, Requirement::None),
                prompt_step("two", 0, Requirement::None),
                third,
            ],
            vec![],
        );
        let mut fake = Fake::new(vec![], vec![]);
        let report = execute(
            &spec,
            options(path.clone(), dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert!(report.failure.is_none(), "{:?}", report.failure);
        assert_eq!(
            fake.calls
                .iter()
                .map(|(id, _, _)| id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "two", "three"],
            "the third step must see steps_completed >= 2"
        );
        let journal = Journal::load(&path).unwrap();
        assert_eq!(journal.facts.steps_completed, 3);
        assert_eq!(journal.step("three").unwrap().status, StepState::Completed);
    }

    #[tokio::test]
    async fn a_changed_step_is_not_skipped_on_resume() {
        let (dir, path) = temp_journal();
        let first = approval_step("push");
        let workflow = spec(vec![first.clone()], vec![]);
        let mut fake = Fake::new(vec![], vec![Approval::Accepted]);
        let report = execute(
            &workflow,
            options(path.clone(), dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert!(report.failure.is_none());

        // Same id, different question: the gate must be asked again.
        let mut changed = first;
        changed.message = Some("Push to production?".into());
        let workflow = spec(vec![changed], vec![]);
        let mut fake = Fake::new(vec![], vec![Approval::Denied]);
        let report = execute(
            &workflow,
            options(path.clone(), dir.path(), true),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert_eq!(fake.approval_calls, vec!["push"]);
        assert_eq!(report.failure.unwrap().code, "workflow_approval_denied");
        assert_eq!(Journal::load(&path).unwrap().approvals.len(), 2);
    }

    #[tokio::test]
    async fn a_pending_step_from_a_crash_keeps_its_retry_budget() {
        let (dir, path) = temp_journal();
        let step = prompt_step("one", 0, Requirement::None);
        let mut journal = Journal::new("release");
        {
            let record = journal.ensure_step("one");
            record.status = StepState::Pending;
            record.attempts = 1;
            record.fingerprint = step_fingerprint(&step);
        }
        journal.save(&path).unwrap();
        let mut fake = Fake::new(vec![], vec![]);
        let report = execute(
            &spec(vec![step], vec![]),
            options(path, dir.path(), true),
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert!(fake.calls.is_empty(), "the budget is already exhausted");
        let failure = report.failure.unwrap();
        assert_eq!(failure.code, "workflow_step_failed");
        assert!(failure.message.contains("retry budget exhausted"));
    }

    #[tokio::test]
    async fn a_step_artifact_removed_by_a_later_step_fails_the_run() {
        let (dir, path) = temp_journal();
        let output = dir.path().join("out.txt");
        std::fs::write(&output, "report\n").unwrap();
        let mut first = prompt_step("one", 0, Requirement::None);
        first.artifacts = vec!["out.txt".into()];
        let spec = spec(
            vec![first, prompt_step("two", 0, Requirement::None)],
            vec![],
        );
        let mut fake = Fake::new(vec![], vec![]).with_action(Action::Remove {
            path: output,
            at: 2,
        });
        // The first step's artifact check passes; the second prompt attempt
        // removes it before the final check.
        let report = execute(
            &spec,
            options(path, dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        let failure = report.failure.unwrap();
        assert_eq!(failure.code, "workflow_artifact_missing");
        assert!(failure.message.contains("out.txt"));
    }

    #[tokio::test]
    async fn an_approval_step_checks_its_declared_artifact() {
        let (dir, path) = temp_journal();
        let mut gate = approval_step("push");
        gate.artifacts = vec!["out/report.md".into()];
        let mut fake = Fake::new(vec![], vec![Approval::Accepted]);
        let report = execute(
            &spec(vec![gate], vec![]),
            options(path, dir.path(), false),
            &mut fake,
            &mut |_| {},
        )
        .await;
        let failure = report.failure.unwrap();
        assert_eq!(failure.code, "workflow_artifact_missing");
        assert_eq!(failure.step.as_deref(), Some("push"));
    }

    #[tokio::test]
    async fn no_session_keeps_the_journal_in_memory() {
        let (dir, path) = temp_journal();
        let spec = spec(vec![prompt_step("one", 0, Requirement::None)], vec![]);
        let mut fake = Fake::new(vec![], vec![]);
        let mut options = options(path.clone(), dir.path(), false);
        options.persist = false;
        let report = execute(&spec, options, &mut fake, &mut |_| {}).await;
        assert!(report.failure.is_none());
        assert!(!path.exists(), "the journal must not be written");
        assert!(!path.with_extension("json.tmp").exists());
    }

    #[tokio::test]
    async fn journal_details_are_redacted_when_requested() {
        let (dir, path) = temp_journal();
        let mut report = StepReport::succeeded(Facts::default());
        report.detail = Some("remote: ghp_0123456789abcdefghijklmnopqrstuvwxyz".into());
        let mut fake = Fake::new(vec![report], vec![]);
        let mut options = options(path.clone(), dir.path(), false);
        options.redact = true;
        let report = execute(
            &spec(vec![prompt_step("one", 0, Requirement::None)], vec![]),
            options,
            &mut fake,
            &mut |_| {},
        )
        .await;
        assert!(report.failure.is_none());
        let stored = Journal::load(&path).unwrap();
        let detail = stored.step("one").unwrap().detail.clone().unwrap();
        assert!(!detail.contains("ghp_0123456789"), "{detail}");
        assert!(detail.contains("[REDACTED:github"), "{detail}");
    }

    #[test]
    fn an_artifact_symlink_outside_the_root_is_not_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("work");
        std::fs::create_dir_all(root.join("out")).unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "secret\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("out/report.md")).unwrap();
        #[cfg(unix)]
        assert!(!artifact_ok(&root, "out/report.md"));
        // A real file inside the root is still accepted.
        std::fs::write(root.join("out/real.md"), "ok\n").unwrap();
        assert!(artifact_ok(&root, "out/real.md"));
    }

    #[test]
    fn journal_is_valid_json_and_reloads() {
        let (dir, path) = temp_journal();
        let mut journal = Journal::new("release");
        journal.ensure_step("one").status = StepState::Completed;
        journal.save(&path).unwrap();
        let loaded = Journal::load(&path).unwrap();
        assert_eq!(loaded.workflow, "release");
        assert_eq!(loaded.step("one").unwrap().status, StepState::Completed);
        assert!(!path.with_extension("json.tmp").exists());
        drop(dir);
    }
}

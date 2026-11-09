//! Deterministic PoC replay and reproducer minimization.
//!
//! A finding is confirmed only when a declared assertion passes against a
//! reproduced step. `error` and `skipped` are inconclusive: they never suppress
//! a finding. A failed assertion is a rejection, but replay does not change the
//! finding's lifecycle status on its own.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;
use regex::Regex;
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::scope::{Scope, gate_command};
use crate::tools::bash::{collect, spawn_reader, take_bytes, terminate_group};

/// Hard cap on one PoC step graph.
pub const MAX_STEPS: usize = 50;
/// Full captured output per command.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// Excerpt carried in the verification result.
const EXCERPT_BYTES: usize = 8 * 1024;
/// Largest HTTP body read for assertions.
const MAX_BODY_BYTES: usize = 1024 * 1024;
/// Largest header set accepted for one HTTP step.
const MAX_HEADERS_BYTES: usize = 16 * 1024;
/// Confidence floor when steps are present but not verifiable.
pub const POC_PRESENT_FLOOR: f64 = 0.6;
/// Confidence floor when at least one step has a verifiable assertion.
pub const POC_VERIFIABLE_FLOOR: f64 = 0.8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PocStepKind {
    #[default]
    Setup,
    Auth,
    Prerequisite,
    Exploit,
    Verify,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PocStep {
    pub id: String,
    #[serde(default)]
    pub kind: PocStepKind,
    #[serde(default)]
    pub summary: String,
    pub action: PocAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<PocExpect>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PocAction {
    Shell {
        cmd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    Http {
        method: String,
        url: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        headers: BTreeMap<String, String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
    },
    Note {
        text: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum PocExpect {
    ExitZero,
    HttpStatus {
        #[serde(deserialize_with = "one_or_many_statuses")]
        status: Vec<u16>,
    },
    BodyContains {
        text: String,
    },
    BodyMatches {
        pattern: String,
    },
    FileExists {
        path: String,
    },
}

impl PocExpect {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ExitZero => "exit-zero",
            Self::HttpStatus { .. } => "http-status",
            Self::BodyContains { .. } => "body-contains",
            Self::BodyMatches { .. } => "body-matches",
            Self::FileExists { .. } => "file-exists",
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::ExitZero => "exit code 0".into(),
            Self::HttpStatus { status } => format!(
                "HTTP status {}",
                status
                    .iter()
                    .map(u16::to_string)
                    .collect::<Vec<_>>()
                    .join(" or ")
            ),
            Self::BodyContains { text } => format!("body contains {text:?}"),
            Self::BodyMatches { pattern } => format!("body matches /{pattern}/"),
            Self::FileExists { path } => format!("file exists at {path:?}"),
        }
    }
}

fn one_or_many_statuses<'de, D>(deserializer: D) -> Result<Vec<u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(u16),
        Many(Vec<u16>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(value) => vec![value],
        OneOrMany::Many(values) => values,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStatus {
    Reproduced,
    NotReproduced,
    Error,
    Skipped,
}

impl VerifyStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reproduced => "reproduced",
            Self::NotReproduced => "not_reproduced",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyVerdict {
    Confirmed,
    Rejected,
    Inconclusive,
}

impl VerifyVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::Inconclusive => "inconclusive",
        }
    }
}

pub fn verdict_for(status: VerifyStatus) -> VerifyVerdict {
    match status {
        VerifyStatus::Reproduced => VerifyVerdict::Confirmed,
        VerifyStatus::NotReproduced => VerifyVerdict::Rejected,
        VerifyStatus::Error | VerifyStatus::Skipped => VerifyVerdict::Inconclusive,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyCommand {
    pub step_id: String,
    pub argv: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stdout_excerpt: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub stderr_excerpt: String,
    pub duration_ms: u64,
    #[serde(default)]
    pub timed_out: bool,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyAssertion {
    pub step_id: String,
    pub kind: String,
    pub expected: String,
    pub actual: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    pub status: VerifyStatus,
    pub verdict: VerifyVerdict,
    pub finding_id: String,
    pub started: DateTime<Utc>,
    pub finished: DateTime<Utc>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<VerifyCommand>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assertions: Vec<VerifyAssertion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One executed step, ready to become an engagement evidence entry.
#[derive(Debug, Clone)]
pub struct StepRecord {
    pub step_id: String,
    pub command: String,
    pub target: String,
    pub started: DateTime<Utc>,
    pub finished: DateTime<Utc>,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub is_error: bool,
    pub output: String,
    pub summary: String,
}

pub struct ReplayOutcome {
    pub verification: Verification,
    pub records: Vec<StepRecord>,
}

#[derive(Clone)]
pub struct ReplayOptions {
    pub run_dir: PathBuf,
    pub scope: Scope,
    pub timeout: Duration,
    pub cancel: CancellationToken,
    pub target: String,
}

pub struct MinimizeOptions {
    pub base_dir: PathBuf,
    pub scope: Scope,
    pub timeout: Duration,
    pub cancel: CancellationToken,
    pub target: String,
    pub max_oracle_calls: usize,
}

/// Confidence is the model's self-report raised to the evidence floor. The
/// floor never exceeds 1.0 and never invents a value when no signal exists.
pub fn compute_confidence(raw: Option<f64>, steps: &[PocStep]) -> Option<f64> {
    let raw = raw
        .filter(|value| value.is_finite())
        .map(|value| value.clamp(0.0, 1.0));
    let floor = if steps.is_empty() {
        0.0
    } else if steps.iter().any(|step| step.expect.is_some()) {
        POC_VERIFIABLE_FLOOR
    } else {
        POC_PRESENT_FLOOR
    };
    if raw.is_none() && floor == 0.0 {
        return None;
    }
    Some(raw.unwrap_or(0.0).max(floor).min(1.0))
}

pub fn validate_steps(steps: &[PocStep]) -> Result<(), String> {
    if steps.is_empty() {
        return Err("poc_steps is empty; add at least one shell or http step".into());
    }
    if steps.len() > MAX_STEPS {
        return Err(format!(
            "poc_steps has {} steps; the limit is {MAX_STEPS}",
            steps.len()
        ));
    }
    let mut ids = HashSet::new();
    for step in steps {
        if step.id.is_empty()
            || step.id.len() > 64
            || !step
                .id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return Err(format!(
                "step id {:?} must be 1-64 characters of letters, digits, '_' or '-'",
                step.id
            ));
        }
        if !ids.insert(step.id.clone()) {
            return Err(format!("duplicate step id {:?}", step.id));
        }
        match &step.action {
            PocAction::Shell { cmd, cwd } => {
                if cmd.trim().is_empty() {
                    return Err(format!("step {} has an empty shell command", step.id));
                }
                if cmd.len() > 8 * 1024 {
                    return Err(format!("step {} shell command is too long", step.id));
                }
                if let Some(cwd) = cwd {
                    validate_relative(cwd)?;
                }
                if matches!(&step.expect, Some(PocExpect::HttpStatus { .. })) {
                    return Err(format!(
                        "step {} is a shell step; http-status only applies to http steps",
                        step.id
                    ));
                }
            }
            PocAction::Http {
                method,
                url,
                headers,
                body,
            } => {
                let method = method.trim().to_ascii_uppercase();
                if !matches!(
                    method.as_str(),
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
                ) {
                    return Err(format!(
                        "step {} has unsupported HTTP method {method:?}",
                        step.id
                    ));
                }
                let parsed = reqwest::Url::parse(url)
                    .map_err(|err| format!("step {} has invalid url: {err}", step.id))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(format!(
                        "step {} url must use http or https, not {}",
                        step.id,
                        parsed.scheme()
                    ));
                }
                if !parsed.username().is_empty() || parsed.password().is_some() {
                    return Err(format!(
                        "step {} url contains credentials; put them in headers instead",
                        step.id
                    ));
                }
                if headers.len() > 50 {
                    return Err(format!("step {} has more than 50 headers", step.id));
                }
                let mut header_bytes = 0usize;
                for (name, value) in headers {
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                        format!("step {} has invalid header name {name:?}", step.id)
                    })?;
                    reqwest::header::HeaderValue::from_str(value).map_err(|_| {
                        format!("step {} has invalid header value for {name}", step.id)
                    })?;
                    header_bytes += name.len() + value.len();
                }
                if header_bytes > MAX_HEADERS_BYTES {
                    return Err(format!("step {} headers exceed 16 KiB", step.id));
                }
                if body.as_ref().is_some_and(|body| body.len() > 64 * 1024) {
                    return Err(format!("step {} body exceeds 64 KiB", step.id));
                }
                if matches!(&step.expect, Some(PocExpect::ExitZero)) {
                    return Err(format!(
                        "step {} is an http step; exit-zero only applies to shell steps",
                        step.id
                    ));
                }
            }
            PocAction::Note { text } => {
                if text.trim().is_empty() {
                    return Err(format!("step {} has empty note text", step.id));
                }
                if step.expect.is_some() {
                    return Err(format!(
                        "step {} is a note; notes cannot have an assertion",
                        step.id
                    ));
                }
            }
        }
        match &step.expect {
            Some(PocExpect::HttpStatus { status }) => {
                if status.is_empty() || status.iter().any(|code| !(100..=599).contains(code)) {
                    return Err(format!(
                        "step {} http-status must list codes between 100 and 599",
                        step.id
                    ));
                }
            }
            Some(PocExpect::BodyMatches { pattern }) => {
                Regex::new(pattern)
                    .map_err(|err| format!("step {} has invalid regex: {err}", step.id))?;
            }
            Some(PocExpect::FileExists { path }) => validate_relative(path)?,
            Some(PocExpect::BodyContains { text }) if text.is_empty() => {
                return Err(format!("step {} body-contains text is empty", step.id));
            }
            Some(PocExpect::BodyContains { .. }) | Some(PocExpect::ExitZero) | None => {}
        }
    }
    if !steps.iter().any(|step| step.expect.is_some()) {
        return Err(
            "poc_steps has no expect predicate; add at least one assertion (exit-zero, http-status, body-contains, body-matches, or file-exists)"
                .into(),
        );
    }
    Ok(())
}

fn validate_relative(value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err("path is empty".into());
    }
    if value.contains('\0') {
        return Err("path contains NUL".into());
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return Err(format!(
            "{value:?} is absolute; use a path inside the PoC run directory"
        ));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_) | Component::CurDir) {
            return Err(format!(
                "{value:?} escapes the PoC run directory; use a relative path without '..'"
            ));
        }
    }
    Ok(())
}

fn run_path(run_dir: &Path, value: &str) -> Result<PathBuf, String> {
    validate_relative(value)?;
    Ok(run_dir.join(value))
}

fn assertion_for(
    step: &PocStep,
    exit_code: Option<i32>,
    http_status: Option<u16>,
    body: Option<&str>,
    stdout: &str,
    run_dir: &Path,
    error: Option<&str>,
) -> Option<VerifyAssertion> {
    let expect = step.expect.as_ref()?;
    if let Some(error) = error {
        return Some(VerifyAssertion {
            step_id: step.id.clone(),
            kind: expect.kind().into(),
            expected: expect.describe(),
            actual: format!("error: {error}"),
            passed: false,
        });
    }
    let (passed, actual) = match expect {
        PocExpect::ExitZero => {
            let passed = exit_code == Some(0);
            (passed, format!("exit code {}", display_exit(exit_code)))
        }
        PocExpect::HttpStatus { status } => {
            let passed = http_status.is_some_and(|code| status.contains(&code));
            (
                passed,
                http_status
                    .map(|code| format!("HTTP {code}"))
                    .unwrap_or_else(|| "no HTTP response".into()),
            )
        }
        PocExpect::BodyContains { text } => {
            let haystack = body.unwrap_or(stdout);
            let passed = haystack.contains(text);
            (
                passed,
                if passed {
                    format!("body contains {text:?}")
                } else {
                    format!("body does not contain {text:?}")
                },
            )
        }
        PocExpect::BodyMatches { pattern } => {
            let haystack = body.unwrap_or(stdout);
            let passed = Regex::new(pattern).is_ok_and(|regex| regex.is_match(haystack));
            (
                passed,
                if passed {
                    format!("body matches /{pattern}/")
                } else {
                    format!("body does not match /{pattern}/")
                },
            )
        }
        PocExpect::FileExists { path } => match run_path(run_dir, path) {
            Ok(path) => {
                let passed = std::fs::symlink_metadata(&path)
                    .map(|meta| meta.file_type().is_file())
                    .unwrap_or(false);
                (
                    passed,
                    if passed {
                        format!("{path:?} exists")
                    } else {
                        format!("{path:?} is missing")
                    },
                )
            }
            Err(err) => (false, err),
        },
    };
    Some(VerifyAssertion {
        step_id: step.id.clone(),
        kind: expect.kind().into(),
        expected: expect.describe(),
        actual,
        passed,
    })
}

fn display_exit(exit_code: Option<i32>) -> String {
    exit_code
        .map(|code| code.to_string())
        .unwrap_or_else(|| "-".into())
}

struct StepExecution {
    command: Option<VerifyCommand>,
    record: Option<StepRecord>,
    assertion: Option<VerifyAssertion>,
    error: Option<String>,
}

pub async fn execute_steps(steps: &[PocStep], opts: &ReplayOptions) -> ReplayOutcome {
    let started = Utc::now();
    let mut commands = Vec::new();
    let mut assertions = Vec::new();
    let mut records = Vec::new();
    let mut first_error: Option<String> = None;

    if let Err(err) = std::fs::create_dir_all(&opts.run_dir) {
        return failed_outcome(
            started,
            format!(
                "cannot create PoC run directory {}: {err}",
                opts.run_dir.display()
            ),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&opts.run_dir, std::fs::Permissions::from_mode(0o700));
    }

    for step in steps {
        if opts.cancel.is_cancelled() {
            first_error = Some(format!(
                "cancelled before step {}; the remaining steps did not run",
                step.id
            ));
            break;
        }
        let execution = execute_step(step, opts).await;
        if let Some(command) = execution.command {
            commands.push(command);
        }
        if let Some(record) = execution.record {
            records.push(record);
        }
        if let Some(assertion) = execution.assertion {
            assertions.push(assertion);
        }
        if first_error.is_none()
            && let Some(error) = execution.error
        {
            first_error = Some(error);
        }
    }

    let status = if first_error.is_some() {
        VerifyStatus::Error
    } else if assertions.is_empty() {
        VerifyStatus::Skipped
    } else if assertions.iter().all(|assertion| assertion.passed) {
        VerifyStatus::Reproduced
    } else {
        VerifyStatus::NotReproduced
    };

    let passed = assertions
        .iter()
        .filter(|assertion| assertion.passed)
        .count();
    let summary = match status {
        VerifyStatus::Reproduced => format!(
            "reproduced: {passed}/{} assertions passed across {} command(s)",
            assertions.len(),
            commands.len()
        ),
        VerifyStatus::NotReproduced => {
            let failure = assertions.iter().find(|assertion| !assertion.passed);
            match failure {
                Some(assertion) => format!(
                    "not reproduced: {passed}/{} assertions passed; first failure at {} ({}): expected {}, got {}",
                    assertions.len(),
                    assertion.step_id,
                    assertion.kind,
                    assertion.expected,
                    assertion.actual
                ),
                None => format!(
                    "not reproduced: {passed}/{} assertions passed",
                    assertions.len()
                ),
            }
        }
        VerifyStatus::Error => format!(
            "error: {}",
            first_error
                .as_deref()
                .unwrap_or("the PoC runner failed before reaching a verdict")
        ),
        VerifyStatus::Skipped => "skipped: no assertions were declared".into(),
    };

    let finished = Utc::now();
    ReplayOutcome {
        verification: Verification {
            status,
            verdict: verdict_for(status),
            finding_id: String::new(),
            started,
            finished,
            duration_ms: (finished - started).num_milliseconds().max(0) as u64,
            commands,
            assertions,
            evidence: Vec::new(),
            summary,
            error: first_error,
        },
        records,
    }
}

fn failed_outcome(started: DateTime<Utc>, error: String) -> ReplayOutcome {
    let finished = Utc::now();
    ReplayOutcome {
        verification: Verification {
            status: VerifyStatus::Error,
            verdict: VerifyVerdict::Inconclusive,
            finding_id: String::new(),
            started,
            finished,
            duration_ms: (finished - started).num_milliseconds().max(0) as u64,
            commands: Vec::new(),
            assertions: Vec::new(),
            evidence: Vec::new(),
            summary: format!("error: {error}"),
            error: Some(error),
        },
        records: Vec::new(),
    }
}

async fn execute_step(step: &PocStep, opts: &ReplayOptions) -> StepExecution {
    match &step.action {
        PocAction::Shell { cmd, cwd } => execute_shell(step, cmd, cwd.as_deref(), opts).await,
        PocAction::Http {
            method,
            url,
            headers,
            body,
        } => execute_http(step, method, url, headers, body.as_deref(), opts).await,
        PocAction::Note { .. } => StepExecution {
            command: None,
            record: None,
            assertion: None,
            error: None,
        },
    }
}

async fn execute_shell(
    step: &PocStep,
    command: &str,
    cwd: Option<&str>,
    opts: &ReplayOptions,
) -> StepExecution {
    let started = Utc::now();
    if let Err(message) = gate_command(command, &opts.scope) {
        return shell_failure(
            step,
            command,
            started,
            format!("scope gate: {message}"),
            opts,
        );
    }
    let cwd = match cwd {
        Some(raw) => match run_path(&opts.run_dir, raw) {
            Ok(path) => path,
            Err(error) => return shell_failure(step, command, started, error, opts),
        },
        None => opts.run_dir.clone(),
    };
    if let Err(err) = std::fs::create_dir_all(&cwd) {
        return shell_failure(
            step,
            command,
            started,
            format!("cannot create step directory {}: {err}", cwd.display()),
            opts,
        );
    }

    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd.env_clear();
    for key in [
        "PATH", "HOME", "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL", "SHELL", "USER", "LOGNAME",
    ] {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
    cmd.env("LH_POC", "1");
    {
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().process_group(0);
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            return shell_failure(
                step,
                command,
                started,
                format!("cannot start bash: {err}"),
                opts,
            );
        }
    };

    let captured = Arc::new(AtomicUsize::new(0));
    let stdout_task = spawn_reader(child.stdout.take(), captured.clone());
    let stderr_task = spawn_reader(child.stderr.take(), captured);

    enum Outcome {
        Exited(Option<i32>),
        TimedOut,
        Cancelled,
    }
    let sleep = tokio::time::sleep(opts.timeout);
    tokio::pin!(sleep);
    let cancel = opts.cancel.cancelled();
    tokio::pin!(cancel);
    let outcome = tokio::select! {
        status = child.wait() => Outcome::Exited(status.ok().and_then(|status| status.code())),
        _ = &mut sleep => Outcome::TimedOut,
        _ = &mut cancel => Outcome::Cancelled,
    };

    let (exit_code, timed_out, cancelled, error) = match outcome {
        Outcome::Exited(code) => (code, false, false, None),
        Outcome::TimedOut => {
            let status = terminate_group(&mut child).await;
            (
                status.and_then(|status| status.code()),
                true,
                false,
                Some(format!(
                    "step {} exceeded {}s and was terminated",
                    step.id,
                    opts.timeout.as_secs()
                )),
            )
        }
        Outcome::Cancelled => {
            let status = terminate_group(&mut child).await;
            (
                status.and_then(|status| status.code()),
                false,
                true,
                Some(format!("step {} was cancelled", step.id)),
            )
        }
    };

    let stdout = collect(stdout_task).await;
    let stderr = collect(stderr_task).await;
    let finished = Utc::now();
    let duration_ms = (finished - started).num_milliseconds().max(0) as u64;
    let stdout_text = String::from_utf8_lossy(&stdout);
    let stderr_text = String::from_utf8_lossy(&stderr);

    let mut output = format!("$ {command}\n");
    append_stream(&mut output, "stdout", stdout_text.as_ref());
    append_stream(&mut output, "stderr", stderr_text.as_ref());
    output.push_str(&format!("exit: {}\n", display_exit(exit_code)));
    let (output, _) = take_bytes(&output, MAX_OUTPUT_BYTES);
    let (stdout_excerpt, _) = take_bytes(stdout_text.as_ref(), EXCERPT_BYTES);
    let (stderr_excerpt, _) = take_bytes(stderr_text.as_ref(), EXCERPT_BYTES);

    let assertion = assertion_for(
        step,
        exit_code,
        None,
        None,
        stdout_text.as_ref(),
        &opts.run_dir,
        error.as_deref(),
    );
    let summary = assertion
        .as_ref()
        .map(|assertion| {
            format!(
                "{}: {} ({})",
                step.id,
                if assertion.passed { "pass" } else { "fail" },
                assertion.kind
            )
        })
        .unwrap_or_else(|| format!("{}: no assertion", step.id));
    StepExecution {
        command: Some(VerifyCommand {
            step_id: step.id.clone(),
            argv: vec!["bash".into(), "-c".into(), command.into()],
            exit_code,
            stdout_excerpt,
            stderr_excerpt,
            duration_ms,
            timed_out,
            cancelled,
            error: error.clone(),
        }),
        record: Some(StepRecord {
            step_id: step.id.clone(),
            command: command.into(),
            target: opts.target.clone(),
            started,
            finished,
            duration_ms,
            exit_code,
            timed_out,
            cancelled,
            is_error: error.is_some(),
            output,
            summary,
        }),
        assertion,
        error,
    }
}

fn shell_failure(
    step: &PocStep,
    command: &str,
    started: DateTime<Utc>,
    error: String,
    opts: &ReplayOptions,
) -> StepExecution {
    let finished = Utc::now();
    let duration_ms = (finished - started).num_milliseconds().max(0) as u64;
    let assertion = assertion_for(
        step,
        None,
        None,
        None,
        "",
        &opts.run_dir,
        Some(error.as_str()),
    );
    StepExecution {
        command: Some(VerifyCommand {
            step_id: step.id.clone(),
            argv: vec!["bash".into(), "-c".into(), command.into()],
            exit_code: None,
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
            duration_ms,
            timed_out: false,
            cancelled: false,
            error: Some(error.clone()),
        }),
        record: Some(StepRecord {
            step_id: step.id.clone(),
            command: command.into(),
            target: opts.target.clone(),
            started,
            finished,
            duration_ms,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            is_error: true,
            output: format!("$ {command}\nerror: {error}\n"),
            summary: format!("{}: error", step.id),
        }),
        assertion,
        error: Some(error),
    }
}

async fn execute_http(
    step: &PocStep,
    method: &str,
    url: &str,
    headers: &BTreeMap<String, String>,
    body: Option<&str>,
    opts: &ReplayOptions,
) -> StepExecution {
    let started = Utc::now();
    if opts.scope.is_empty() || !opts.scope.allows(url) {
        return http_failure(
            step,
            method,
            url,
            started,
            format!(
                "HTTP step {} targets {url}, which is not in the engagement scope; add it with /sec scope add or use an in-scope target",
                step.id
            ),
            opts,
        );
    }
    let parsed = match reqwest::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(err) => {
            return http_failure(
                step,
                method,
                url,
                started,
                format!("invalid url: {err}"),
                opts,
            );
        }
    };
    let method = match reqwest::Method::from_bytes(method.trim().to_ascii_uppercase().as_bytes()) {
        Ok(method) => method,
        Err(err) => {
            return http_failure(
                step,
                method,
                url,
                started,
                format!("invalid method: {err}"),
                opts,
            );
        }
    };
    let client = match reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(opts.timeout)
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return http_failure(
                step,
                method.as_str(),
                url,
                started,
                format!("cannot build HTTP client: {err}"),
                opts,
            );
        }
    };
    let mut request = client.request(method.clone(), parsed.clone());
    for (name, value) in headers {
        let name = match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
            Ok(name) => name,
            Err(_) => {
                return http_failure(
                    step,
                    method.as_str(),
                    url,
                    started,
                    format!("invalid header name {name:?}"),
                    opts,
                );
            }
        };
        let value = match reqwest::header::HeaderValue::from_str(value) {
            Ok(value) => value,
            Err(_) => {
                return http_failure(
                    step,
                    method.as_str(),
                    url,
                    started,
                    format!("invalid header value for {name}"),
                    opts,
                );
            }
        };
        request = request.header(name, value);
    }
    if let Some(body) = body {
        request = request.body(body.to_string());
    }

    let send = request.send();
    tokio::pin!(send);
    let cancel = opts.cancel.cancelled();
    tokio::pin!(cancel);
    let response = tokio::select! {
        result = &mut send => result,
        _ = &mut cancel => {
            return http_failure(
                step,
                method.as_str(),
                url,
                started,
                format!("step {} was cancelled before a response", step.id),
                opts,
            );
        }
    };
    let mut response = match response {
        Ok(response) => response,
        Err(err) => {
            return http_failure(
                step,
                method.as_str(),
                url,
                started,
                format!("request failed: {err}"),
                opts,
            );
        }
    };

    let status = response.status().as_u16();
    let mut response_headers = String::new();
    for (name, value) in response.headers().iter().take(16) {
        response_headers.push_str(&format!(
            "< {name}: {}\n",
            value.to_str().unwrap_or("<non-utf8>")
        ));
    }
    let mut body_bytes = Vec::new();
    let mut body_error = None;
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body_bytes.len() >= MAX_BODY_BYTES {
                    break;
                }
                let take = chunk.len().min(MAX_BODY_BYTES - body_bytes.len());
                body_bytes.extend_from_slice(&chunk[..take]);
                if body_bytes.len() >= MAX_BODY_BYTES {
                    break;
                }
            }
            Ok(None) => break,
            Err(err) => {
                body_error = Some(format!("cannot read response body: {err}"));
                break;
            }
        }
    }
    let body_text = String::from_utf8_lossy(&body_bytes);
    let finished = Utc::now();
    let duration_ms = (finished - started).num_milliseconds().max(0) as u64;
    let error = body_error;
    let (body_excerpt, _) = take_bytes(body_text.as_ref(), EXCERPT_BYTES);
    let mut output = format!("> {} {}\n", method.as_str(), parsed);
    for (name, value) in headers {
        let value = if name.eq_ignore_ascii_case("authorization") {
            "<redacted>"
        } else {
            value.as_str()
        };
        output.push_str(&format!("> {name}: {value}\n"));
    }
    if let Some(body) = body {
        output.push_str(&format!("> body: {body}\n"));
    }
    output.push_str(&format!("< HTTP {status}\n{response_headers}"));
    append_stream(&mut output, "body", body_text.as_ref());
    if let Some(error) = &error {
        output.push_str(&format!("error: {error}\n"));
    }
    let (output, _) = take_bytes(&output, MAX_OUTPUT_BYTES);
    let assertion = assertion_for(
        step,
        None,
        Some(status),
        Some(body_text.as_ref()),
        "",
        &opts.run_dir,
        error.as_deref(),
    );
    let summary = assertion
        .as_ref()
        .map(|assertion| {
            format!(
                "{}: {} ({})",
                step.id,
                if assertion.passed { "pass" } else { "fail" },
                assertion.kind
            )
        })
        .unwrap_or_else(|| format!("{}: no assertion", step.id));
    StepExecution {
        command: Some(VerifyCommand {
            step_id: step.id.clone(),
            argv: vec![method.as_str().into(), parsed.to_string()],
            exit_code: None,
            stdout_excerpt: body_excerpt,
            stderr_excerpt: String::new(),
            duration_ms,
            timed_out: false,
            cancelled: false,
            error: error.clone(),
        }),
        record: Some(StepRecord {
            step_id: step.id.clone(),
            command: format!("{} {}", method.as_str(), parsed),
            target: opts.target.clone(),
            started,
            finished,
            duration_ms,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            is_error: error.is_some(),
            output,
            summary,
        }),
        assertion,
        error,
    }
}

fn http_failure(
    step: &PocStep,
    method: &str,
    url: &str,
    started: DateTime<Utc>,
    error: String,
    opts: &ReplayOptions,
) -> StepExecution {
    let finished = Utc::now();
    let duration_ms = (finished - started).num_milliseconds().max(0) as u64;
    let assertion = assertion_for(
        step,
        None,
        None,
        None,
        "",
        &opts.run_dir,
        Some(error.as_str()),
    );
    StepExecution {
        command: Some(VerifyCommand {
            step_id: step.id.clone(),
            argv: vec![method.into(), url.into()],
            exit_code: None,
            stdout_excerpt: String::new(),
            stderr_excerpt: String::new(),
            duration_ms,
            timed_out: false,
            cancelled: false,
            error: Some(error.clone()),
        }),
        record: Some(StepRecord {
            step_id: step.id.clone(),
            command: format!("{method} {url}"),
            target: opts.target.clone(),
            started,
            finished,
            duration_ms,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            is_error: true,
            output: format!("> {method} {url}\nerror: {error}\n"),
            summary: format!("{}: error", step.id),
        }),
        assertion,
        error: Some(error),
    }
}

fn append_stream(output: &mut String, label: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    output.push_str(label);
    output.push_str(":\n");
    output.push_str(text);
    if !output.ends_with('\n') {
        output.push('\n');
    }
}

/// Delta-debugging over step indices. The test future returns true when the
/// subset still reproduces. Stops at a 1-minimal set.
pub async fn ddmin<F>(unit_count: usize, mut test: F) -> Vec<usize>
where
    F: FnMut(Vec<usize>) -> BoxFuture<'static, bool>,
{
    let mut kept: Vec<usize> = (0..unit_count).collect();
    let mut granularity = 2usize;
    while kept.len() >= 2 {
        let chunks = partition(&kept, granularity.min(kept.len()));
        let mut reduced = false;
        if chunks.len() > 2 {
            for chunk in &chunks {
                if test(chunk.clone()).await {
                    kept = chunk.clone();
                    granularity = 2;
                    reduced = true;
                    break;
                }
            }
        }
        if !reduced {
            for index in 0..chunks.len() {
                let candidate: Vec<usize> = chunks
                    .iter()
                    .enumerate()
                    .filter(|(chunk_index, _)| *chunk_index != index)
                    .flat_map(|(_, chunk)| chunk.iter().copied())
                    .collect();
                if !candidate.is_empty() && test(candidate.clone()).await {
                    kept = candidate;
                    granularity = granularity.saturating_sub(1).max(2);
                    reduced = true;
                    break;
                }
            }
        }
        if !reduced {
            if granularity >= kept.len() {
                break;
            }
            granularity = (granularity * 2).min(kept.len());
        }
    }
    kept
}

/// Minimize a step graph. Only removes steps; ordering is preserved. The
/// caller must still replay the result and persist evidence for the final run.
pub async fn minimize_steps(
    steps: &[PocStep],
    opts: &MinimizeOptions,
) -> (Vec<PocStep>, usize, bool) {
    if steps.len() <= 1 || opts.cancel.is_cancelled() {
        return (steps.to_vec(), 0, true);
    }
    // A step that carries the exploit or a non-exit-zero assertion is an
    // anchor. Without this, ddmin can "minimize" to an unrelated `exit-zero`
    // setup step that happens to pass on its own.
    let anchors: Vec<usize> = steps
        .iter()
        .enumerate()
        .filter(|(_, step)| is_anchor(step))
        .map(|(index, _)| index)
        .collect();
    let removable: Vec<usize> = (0..steps.len())
        .filter(|index| !anchors.contains(index))
        .collect();
    if anchors.is_empty() || removable.is_empty() {
        return (steps.to_vec(), 0, true);
    }

    let budget = opts.max_oracle_calls.max(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let trials = Arc::new(AtomicUsize::new(0));
    let exhausted = Arc::new(AtomicBool::new(false));
    let all_steps = Arc::new(steps.to_vec());
    let closure_steps = all_steps.clone();
    let closure_anchors = anchors.clone();
    let closure_removable = removable.clone();
    let calls_tracker = calls.clone();
    let exhausted_tracker = exhausted.clone();
    let base = opts.base_dir.clone();
    let scope = opts.scope.clone();
    let timeout = opts.timeout;
    let cancel = opts.cancel.clone();
    let target = opts.target.clone();

    let kept = ddmin(removable.len(), move |keep| {
        let steps = closure_steps.clone();
        let anchors = closure_anchors.clone();
        let removable = closure_removable.clone();
        let calls = calls_tracker.clone();
        let trials = trials.clone();
        let exhausted = exhausted_tracker.clone();
        let base = base.clone();
        let scope = scope.clone();
        let cancel = cancel.clone();
        let target = target.clone();
        let trial = trials.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if calls.fetch_add(1, Ordering::SeqCst) >= budget {
                exhausted.store(true, Ordering::SeqCst);
                return false;
            }
            let mut indices = anchors;
            indices.extend(
                keep.iter()
                    .filter_map(|&relative| removable.get(relative).copied()),
            );
            indices.sort_unstable();
            let subset: Vec<PocStep> = indices
                .iter()
                .filter_map(|&index| steps.get(index).cloned())
                .collect();
            if subset.is_empty() {
                return false;
            }
            let run_dir = base.join(format!("trial-{trial}"));
            let outcome = execute_steps(
                &subset,
                &ReplayOptions {
                    run_dir: run_dir.clone(),
                    scope,
                    timeout,
                    cancel,
                    target,
                },
            )
            .await;
            let _ = std::fs::remove_dir_all(&run_dir);
            outcome.verification.status == VerifyStatus::Reproduced
        })
    })
    .await;

    let mut indices = anchors;
    indices.extend(
        kept.iter()
            .filter_map(|&relative| removable.get(relative).copied()),
    );
    indices.sort_unstable();
    let minimized: Vec<PocStep> = indices
        .iter()
        .filter_map(|&index| all_steps.get(index).cloned())
        .collect();
    (
        minimized,
        calls.load(Ordering::SeqCst),
        !exhausted.load(Ordering::SeqCst),
    )
}

fn is_anchor(step: &PocStep) -> bool {
    matches!(step.kind, PocStepKind::Exploit | PocStepKind::Verify)
        || step
            .expect
            .as_ref()
            .is_some_and(|expect| !matches!(expect, PocExpect::ExitZero))
}

fn partition(items: &[usize], parts: usize) -> Vec<Vec<usize>> {
    let size = items.len().div_ceil(parts.max(1)).max(1);
    items.chunks(size).map(|chunk| chunk.to_vec()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(id: &str, expect: Option<PocExpect>) -> PocStep {
        PocStep {
            id: id.into(),
            kind: PocStepKind::Exploit,
            summary: String::new(),
            action: PocAction::Shell {
                cmd: "true".into(),
                cwd: None,
            },
            expect,
        }
    }

    #[test]
    fn confidence_floor() {
        assert_eq!(compute_confidence(None, &[]), None);
        assert_eq!(compute_confidence(Some(0.3), &[]), Some(0.3));
        let bare = vec![step("a", None)];
        assert_eq!(compute_confidence(Some(0.2), &bare), Some(0.6));
        let asserted = vec![step("a", Some(PocExpect::ExitZero))];
        assert_eq!(compute_confidence(Some(0.2), &asserted), Some(0.8));
        assert_eq!(compute_confidence(Some(0.95), &asserted), Some(0.95));
        assert_eq!(compute_confidence(None, &asserted), Some(0.8));
    }

    #[test]
    fn poc_steps_parse() {
        let steps: Vec<PocStep> = serde_json::from_value(json!([{
            "id": "one",
            "kind": "exploit",
            "summary": "write a marker",
            "action": {"type": "shell", "cmd": "printf ok"},
            "expect": {"type": "exit-zero"}
        }, {
            "id": "two",
            "action": {"type": "http", "method": "GET", "url": "http://127.0.0.1:1/"},
            "expect": {"type": "http-status", "status": 200}
        }, {
            "id": "three",
            "action": {"type": "http", "method": "GET", "url": "http://127.0.0.1:1/"},
            "expect": {"type": "http-status", "status": [200, 204]}
        }]))
        .unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0].kind, PocStepKind::Exploit);
        validate_steps(&steps).unwrap();
        match &steps[1].expect {
            Some(PocExpect::HttpStatus { status }) => assert_eq!(status, &[200]),
            other => panic!("unexpected expect: {other:?}"),
        }
        match &steps[2].expect {
            Some(PocExpect::HttpStatus { status }) => assert_eq!(status, &[200, 204]),
            other => panic!("unexpected expect: {other:?}"),
        }
    }

    #[test]
    fn validation_rejects_unsafe_shapes() {
        let no_expect = vec![step("a", None)];
        assert!(
            validate_steps(&no_expect)
                .unwrap_err()
                .contains("no expect")
        );

        let absolute = vec![PocStep {
            id: "a".into(),
            kind: PocStepKind::Exploit,
            summary: String::new(),
            action: PocAction::Shell {
                cmd: "true".into(),
                cwd: Some("/tmp".into()),
            },
            expect: Some(PocExpect::ExitZero),
        }];
        assert!(validate_steps(&absolute).unwrap_err().contains("absolute"));

        let escape = vec![PocStep {
            id: "a".into(),
            kind: PocStepKind::Exploit,
            summary: String::new(),
            action: PocAction::Shell {
                cmd: "true".into(),
                cwd: None,
            },
            expect: Some(PocExpect::FileExists {
                path: "../secret".into(),
            }),
        }];
        assert!(validate_steps(&escape).unwrap_err().contains("escapes"));

        let mismatch = vec![PocStep {
            id: "a".into(),
            kind: PocStepKind::Exploit,
            summary: String::new(),
            action: PocAction::Shell {
                cmd: "true".into(),
                cwd: None,
            },
            expect: Some(PocExpect::HttpStatus { status: vec![200] }),
        }];
        assert!(
            validate_steps(&mismatch)
                .unwrap_err()
                .contains("shell step")
        );
    }

    #[tokio::test]
    async fn ddmin_finds_a_minimal_subset() {
        let kept = ddmin(5, |keep| {
            Box::pin(async move { keep.contains(&1) && keep.contains(&3) })
        })
        .await;
        assert_eq!(kept, vec![1, 3]);
    }

    #[tokio::test]
    async fn shell_replay_reproduces_and_rejects() {
        let dir = tempfile::tempdir().unwrap();
        let opts = ReplayOptions {
            run_dir: dir.path().join("run"),
            scope: Scope::default(),
            timeout: Duration::from_secs(5),
            cancel: CancellationToken::new(),
            target: "local".into(),
        };
        let ok = vec![
            step("write", Some(PocExpect::ExitZero)),
            PocStep {
                id: "probe".into(),
                kind: PocStepKind::Verify,
                summary: String::new(),
                action: PocAction::Shell {
                    cmd: "printf 'ZERO-INJ-OK'".into(),
                    cwd: None,
                },
                expect: Some(PocExpect::BodyContains {
                    text: "ZERO-INJ-OK".into(),
                }),
            },
        ];
        let outcome = execute_steps(&ok, &opts).await;
        assert_eq!(outcome.verification.status, VerifyStatus::Reproduced);
        assert_eq!(outcome.verification.verdict, VerifyVerdict::Confirmed);
        assert_eq!(outcome.records.len(), 2);

        let bad = vec![PocStep {
            id: "fail".into(),
            kind: PocStepKind::Verify,
            summary: String::new(),
            action: PocAction::Shell {
                cmd: "exit 1".into(),
                cwd: None,
            },
            expect: Some(PocExpect::ExitZero),
        }];
        let outcome = execute_steps(&bad, &opts).await;
        assert_eq!(outcome.verification.status, VerifyStatus::NotReproduced);
        assert_eq!(outcome.verification.verdict, VerifyVerdict::Rejected);
    }

    #[tokio::test]
    async fn http_requires_scope_and_checks_assertion() {
        let dir = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/ok", axum::routing::get(|| async { "ZERO-INJ-OK" }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("http://{addr}/ok");
        let http_step = PocStep {
            id: "http".into(),
            kind: PocStepKind::Verify,
            summary: String::new(),
            action: PocAction::Http {
                method: "GET".into(),
                url: url.clone(),
                headers: BTreeMap::new(),
                body: None,
            },
            expect: Some(PocExpect::BodyContains {
                text: "ZERO-INJ-OK".into(),
            }),
        };

        let no_scope = ReplayOptions {
            run_dir: dir.path().join("no-scope"),
            scope: Scope::default(),
            timeout: Duration::from_secs(5),
            cancel: CancellationToken::new(),
            target: url.clone(),
        };
        let outcome = execute_steps(std::slice::from_ref(&http_step), &no_scope).await;
        assert_eq!(outcome.verification.status, VerifyStatus::Error);
        assert!(outcome.verification.summary.contains("scope"));

        let mut scope = Scope::default();
        scope.add("127.0.0.1").unwrap();
        let scoped = ReplayOptions {
            run_dir: dir.path().join("scoped"),
            scope,
            timeout: Duration::from_secs(5),
            cancel: CancellationToken::new(),
            target: url,
        };
        let outcome = execute_steps(std::slice::from_ref(&http_step), &scoped).await;
        assert_eq!(outcome.verification.status, VerifyStatus::Reproduced);
    }

    #[tokio::test]
    async fn minimization_keeps_reproducing_steps() {
        let dir = tempfile::tempdir().unwrap();
        let steps = vec![
            PocStep {
                id: "setup".into(),
                kind: PocStepKind::Setup,
                summary: String::new(),
                action: PocAction::Shell {
                    cmd: "printf setup".into(),
                    cwd: None,
                },
                expect: Some(PocExpect::ExitZero),
            },
            PocStep {
                id: "probe".into(),
                kind: PocStepKind::Exploit,
                summary: String::new(),
                action: PocAction::Shell {
                    cmd: "printf 'ZERO-INJ-OK'".into(),
                    cwd: None,
                },
                expect: Some(PocExpect::BodyContains {
                    text: "ZERO-INJ-OK".into(),
                }),
            },
            PocStep {
                id: "noise".into(),
                kind: PocStepKind::Setup,
                summary: String::new(),
                action: PocAction::Shell {
                    cmd: "true".into(),
                    cwd: None,
                },
                expect: Some(PocExpect::ExitZero),
            },
        ];
        let opts = MinimizeOptions {
            base_dir: dir.path().join("min"),
            scope: Scope::default(),
            timeout: Duration::from_secs(5),
            cancel: CancellationToken::new(),
            target: "local".into(),
            max_oracle_calls: 20,
        };
        let (minimized, calls, one_minimal) = minimize_steps(&steps, &opts).await;
        assert!(calls > 0);
        assert!(one_minimal);
        assert!(minimized.iter().any(|step| step.id == "probe"));
        assert!(minimized.len() < steps.len());
    }
}

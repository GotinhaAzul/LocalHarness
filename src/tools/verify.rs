//! `verify` tool: run configured verification steps sequentially, without a
//! shell, and record a compact `verify-result` custom entry.

use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::task::JoinHandle;

use crate::config::VerifyStep;
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult, optional_str};

/// Combined stdout+stderr cap for one verify call.
const OUTPUT_CAP_BYTES: usize = 1024 * 1024;
/// Failure excerpt shown in the tool result.
const FAILURE_EXCERPT_CHARS: usize = 1400;
/// Characters the attached failing locations may take.
const MAX_LOCATION_CHARS: usize = 1600;
/// Room kept for the lines after the locations (rerun, detection note).
const LOCATION_RESERVE_CHARS: usize = 300;
/// A step may not run longer than this, whatever its config says.
const MAX_STEP_SECS: u64 = 600;
/// How long to wait for pipe readers after the child exits.
const READER_GRACE_SECS: u64 = 2;

pub struct VerifyTool;

impl VerifyTool {}

struct StepRun {
    name: String,
    exit_code: Option<i32>,
    timed_out: bool,
    duration_ms: u64,
    output: String,
}

enum StepOutcome {
    Ran(StepRun),
    SpawnFailed(String),
    Cancelled,
}

fn no_verify_error(ctx: &ToolContext) -> ToolResult {
    // Name the command this project actually uses, so the error is a next
    // action instead of `cargo test` in a Python repository.
    let detected = crate::tools::project::detect_verify_steps(&ctx.cwd);
    let step = detected
        .steps
        .iter()
        .find(|step| step.name == "test")
        .or_else(|| detected.steps.last());
    let suggestion = match step {
        Some(step) => format!(
            "Detected from {}: {}. Run it through bash now, for example {{\"command\":\"{}\"}}, or save the detected steps with `tot verify --init` and call verify again.",
            detected.markers.join(", "),
            step.command,
            step.command
        ),
        None => "Add [[steps]] entries to .harness/verify.toml (trusted projects) or a [verify] section to your config, for example:\n[[steps]]\nname = \"test\"\ncommand = [\"cargo\", \"test\"]".into(),
    };
    ToolResult::err(
        "NO_VERIFY",
        format!("no verification steps are configured, so verify has nothing to run. {suggestion}"),
    )
}

fn select_steps<'a>(
    ctx: &'a ToolContext,
    filter: Option<&str>,
) -> Result<Vec<&'a VerifyStep>, ToolResult> {
    let steps: Vec<&VerifyStep> = ctx.verify.steps.iter().collect();
    let Some(filter) = filter.map(str::trim).filter(|f| !f.is_empty()) else {
        return Ok(steps);
    };
    if let Some(step) = steps.iter().find(|step| step.name == filter) {
        return Ok(vec![*step]);
    }
    let lower = filter.to_lowercase();
    let matched: Vec<&VerifyStep> = steps
        .iter()
        .copied()
        .filter(|step| step.name.to_lowercase().contains(&lower))
        .collect();
    if matched.is_empty() {
        let names: Vec<&str> = steps.iter().map(|step| step.name.as_str()).collect();
        return Err(ToolResult::err(
            "VERIFY_STEP_NOT_FOUND",
            format!(
                "no verify step matches {filter:?}; configured steps: {}. Call verify without \"step\" to run them all.",
                names.join(", ")
            ),
        ));
    }
    Ok(matched)
}

#[async_trait]
impl Tool for VerifyTool {
    fn preview_tail(&self) -> bool {
        true
    }

    fn name(&self) -> &'static str {
        "verify"
    }

    fn description(&self) -> &'static str {
        "Run configured verification steps."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "step": {"type": "string", "description": "Run only this configured step (e.g. \"test\"); omit to run all."}
            }),
            &[],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        if ctx.verify.steps.is_empty() {
            return no_verify_error(ctx);
        }
        let filter = optional_str(&args, "step");
        let steps = match select_steps(ctx, filter.as_deref()) {
            Ok(steps) => steps,
            Err(err) => return err,
        };
        if ctx.is_cancelled() {
            return ToolResult::err("cancelled", "verify cancelled before its first step");
        }

        // The content the steps run against, so a PR can tell whether its
        // commits are what passed (edits through the shell leave no trace in
        // the session).
        let cwd = ctx.cwd.clone();
        let tree = tokio::task::spawn_blocking(move || crate::tools::git::content_tree(&cwd))
            .await
            .ok()
            .flatten();
        let mut runs: Vec<StepRun> = Vec::new();
        let mut failed: Option<(String, String, Vec<String>)> = None;
        for step in steps {
            if step.command.is_empty() {
                return ToolResult::err(
                    "VERIFY_CONFIG",
                    format!(
                        "verify step {:?} has an empty command; add a program and arguments to [[steps]] in .harness/verify.toml",
                        step.name
                    ),
                );
            }
            match run_step(ctx, step).await {
                StepOutcome::Ran(run) => {
                    let passed = !run.timed_out && run.exit_code == Some(0);
                    if !passed {
                        failed = Some((run.name.clone(), run.output.clone(), step.command.clone()));
                    }
                    runs.push(run);
                    if !passed {
                        break;
                    }
                }
                StepOutcome::SpawnFailed(message) => {
                    return ToolResult::err(
                        "VERIFY_SPAWN",
                        format!(
                            "verify step {:?} could not start: {message}. Check that {:?} exists on PATH and that the command can run in {}. Then retry verify.",
                            step.name,
                            step.command.first().map(String::as_str).unwrap_or(""),
                            ctx.cwd.display()
                        ),
                    );
                }
                StepOutcome::Cancelled => {
                    return ToolResult::err(
                        "cancelled",
                        format!("verify cancelled while running {:?}", step.name),
                    );
                }
            }
        }

        let mut content = String::new();
        for run in &runs {
            let seconds = run.duration_ms as f64 / 1000.0;
            if run.timed_out {
                content.push_str(&format!("FAIL {} timeout after {seconds:.1}s\n", run.name));
            } else if run.exit_code == Some(0) {
                content.push_str(&format!("PASS {} ({seconds:.1}s)\n", run.name));
            } else {
                let code = run
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "signal".into());
                content.push_str(&format!("FAIL {} exit {code} ({seconds:.1}s)\n", run.name));
            }
        }

        if let Some((name, output, command)) = &failed {
            let excerpt = failure_excerpt(output);
            content.push_str(&format!(
                "\n--- {name} output (last {FAILURE_EXCERPT_CHARS} chars) ---\n"
            ));
            content.push_str(&excerpt);
            if !excerpt.ends_with('\n') {
                content.push('\n');
            }
            content.push_str("---\n");
            // The spots the output names, already read for the model.
            let locations = crate::tools::locations::find(output, &ctx.cwd);
            let budget = ctx
                .caps
                .tool_budget()
                .saturating_sub(content.len() + LOCATION_RESERVE_CHARS)
                .min(MAX_LOCATION_CHARS);
            let attached = crate::tools::locations::attach(ctx, &locations, budget);
            if !attached.is_empty() {
                content.push_str("\n--- failing locations (read) ---\n");
                content.push_str(&attached);
                content.push_str("---\n");
            }
            content.push_str(&format!("rerun: {}\n", format_command(command)));
        } else {
            content.push_str(&format!("all {} steps passed\n", runs.len()));
        }
        if ctx.verify.is_detected() {
            content.push_str(&format!(
                "(steps detected from {}; not saved)\n",
                ctx.verify.detected_from.join(", ")
            ));
        }

        let steps: Vec<Value> = runs
            .iter()
            .map(|run| {
                json!({
                    "name": run.name,
                    "passed": !run.timed_out && run.exit_code == Some(0),
                    "exit_code": run.exit_code,
                    "duration_ms": run.duration_ms,
                })
            })
            .collect();
        let passed = failed.is_none();
        let failed_step = failed.as_ref().map(|(name, _, _)| name.clone());
        let details = json!({
            "session": [{
                "kind": "verify-result",
                "passed": passed,
                "steps": steps,
                "failed_step": failed_step,
                "tree": tree,
            }]
        });
        // A failing verify still ran successfully as a tool; keep it a success
        // result so the verify-result entry is persisted for the settle gate.
        ToolResult::ok(content).with_details(details)
    }
}

async fn run_step(ctx: &ToolContext, step: &VerifyStep) -> StepOutcome {
    let mut command = crate::tools::procenv::command(&step.command[0]);
    command
        .args(&step.command[1..])
        .current_dir(&ctx.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => return StepOutcome::SpawnFailed(err.to_string()),
    };

    let Some(stdout) = child.stdout.take() else {
        return StepOutcome::SpawnFailed("stdout pipe was not captured".into());
    };
    let Some(stderr) = child.stderr.take() else {
        return StepOutcome::SpawnFailed("stderr pipe was not captured".into());
    };
    let budget = Arc::new(AtomicUsize::new(0));
    let stdout_task = tokio::spawn(drain(stdout, budget.clone()));
    let stderr_task = tokio::spawn(drain(stderr, budget));

    let started = Instant::now();
    let timeout = Duration::from_secs(step.timeout_secs.clamp(1, MAX_STEP_SECS));
    let mut timed_out = false;
    let mut cancelled = false;
    let mut exit_code = None;

    let waited = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => None,
        result = tokio::time::timeout(timeout, child.wait()) => Some(result),
    };
    match waited {
        None => {
            cancelled = true;
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
        Some(Ok(Ok(status))) => exit_code = status.code(),
        Some(Ok(Err(err))) => {
            let _ = child.kill().await;
            return StepOutcome::SpawnFailed(format!("could not wait for the process: {err}"));
        }
        Some(Err(_)) => {
            timed_out = true;
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    let (stdout, stderr) = collect_readers(stdout_task, stderr_task).await;
    if cancelled {
        return StepOutcome::Cancelled;
    }
    let duration_ms = started.elapsed().as_millis() as u64;
    let mut output = String::from_utf8_lossy(&stdout).into_owned();
    if !stderr.is_empty() {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str(&String::from_utf8_lossy(&stderr));
    }
    StepOutcome::Ran(StepRun {
        name: step.name.clone(),
        exit_code,
        timed_out,
        duration_ms,
        output: crate::sanitize::terminal_output(&output).into_owned(),
    })
}

/// Drain a pipe completely while keeping at most the shared byte budget.
async fn drain<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    budget: Arc<AtomicUsize>,
) -> Vec<u8> {
    let mut kept = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => {
                let before = budget.fetch_add(n, Ordering::Relaxed);
                if before < OUTPUT_CAP_BYTES {
                    let take = n.min(OUTPUT_CAP_BYTES - before);
                    kept.extend_from_slice(&buffer[..take]);
                }
            }
            Err(_) => break,
        }
    }
    kept
}

async fn collect_readers(
    stdout: JoinHandle<Vec<u8>>,
    stderr: JoinHandle<Vec<u8>>,
) -> (Vec<u8>, Vec<u8>) {
    let joined = async {
        let (out, err) = tokio::join!(stdout, stderr);
        (out.unwrap_or_default(), err.unwrap_or_default())
    };
    // A grandchild may still hold the pipe; do not wait forever.
    tokio::time::timeout(Duration::from_secs(READER_GRACE_SECS), joined)
        .await
        .unwrap_or_default()
}

fn failure_excerpt(output: &str) -> String {
    let trimmed = output.trim_end_matches(['\n', '\r']);
    if trimmed.trim().is_empty() {
        return "(no output)".into();
    }
    let count = trimmed.chars().count();
    if count <= FAILURE_EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let skip = count - FAILURE_EXCERPT_CHARS;
    let tail: String = trimmed.chars().skip(skip).collect();
    format!("…{}", tail.trim_start())
}

fn format_command(command: &[String]) -> String {
    command
        .iter()
        .map(|part| {
            if part.is_empty() || part.chars().any(char::is_whitespace) {
                format!("{part:?}")
            } else {
                part.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VerifyConfig;
    use crate::paths::Paths;
    use crate::security::SecurityState;
    use crate::session::{ArtifactStore, Session, SessionData};
    use crate::tools::{BgRegistry, ReadTracker};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn test_ctx(steps: Vec<VerifyStep>) -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        let paths = Paths {
            config_dir: cwd.join(".config"),
            data_dir: cwd.join(".data"),
            cwd: cwd.clone(),
            project_id: "test".into(),
            sessions_dir_override: None,
            home_dir: None,
        };
        let header = SessionData {
            cwd: cwd.display().to_string(),
            endpoint: "test".into(),
            model: "test".into(),
            profile: "test".into(),
            app: "test".into(),
            redact: true,
            context: None,
            permissions: None,
        };
        let session = Session::create(&paths, header, true).expect("session");
        let ctx = ToolContext {
            cwd,
            path_jail: true,
            session: Arc::new(Mutex::new(session)),
            artifacts: Arc::new(ArtifactStore::ephemeral()),
            cancel: CancellationToken::new(),
            trust: true,
            verify: VerifyConfig {
                steps,
                ..Default::default()
            },
            profile: "test".into(),
            edit_mode: Default::default(),
            packs: Vec::new(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::new())),
            security: Arc::new(Mutex::new(SecurityState::default())),
            paths,
            lsp: Arc::new(crate::tools::lsp::LspService::new()),
            sandbox: None,
            vision: Default::default(),
            caps: Default::default(),
        };
        (dir, ctx)
    }

    fn step(name: &str, script: &str) -> VerifyStep {
        VerifyStep {
            name: name.into(),
            command: vec!["sh".into(), "-c".into(), script.into()],
            timeout_secs: 10,
        }
    }

    #[tokio::test]
    async fn runs_a_passing_step() {
        let (_dir, ctx) = test_ctx(vec![step("ok", "echo fine; exit 0")]);
        let result = VerifyTool.call(&ctx, json!({})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("PASS ok"), "{}", result.content);
        let entry = &result.details.as_ref().expect("details")["session"][0];
        assert_eq!(entry["kind"], "verify-result");
        assert_eq!(entry["passed"], true);
        assert_eq!(entry["steps"][0]["name"], "ok");
        assert_eq!(entry["steps"][0]["passed"], true);
        assert_eq!(entry["steps"][0]["exit_code"], 0);
    }

    #[tokio::test]
    async fn stops_at_the_first_failing_step() {
        let (_dir, ctx) = test_ctx(vec![
            step("ok", "exit 0"),
            step("bad", "echo boom >&2; exit 7"),
            step("never", "exit 0"),
        ]);
        let result = VerifyTool.call(&ctx, json!({})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("PASS ok"), "{}", result.content);
        assert!(
            result.content.contains("FAIL bad exit 7"),
            "{}",
            result.content
        );
        assert!(result.content.contains("boom"), "{}", result.content);
        assert!(!result.content.contains("PASS never"), "{}", result.content);
        let entry = &result.details.as_ref().expect("details")["session"][0];
        assert_eq!(entry["passed"], false);
        assert_eq!(entry["failed_step"], "bad");
        assert_eq!(entry["steps"].as_array().expect("steps").len(), 2);
    }

    #[tokio::test]
    async fn a_failure_attaches_the_location_it_names_and_records_the_read() {
        let (dir, ctx) = test_ctx(vec![step(
            "check",
            "printf 'error[E0308]: mismatched types\\n  --> src/lib.rs:5:3\\n'; exit 101",
        )]);
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let body: String = (1..=12).map(|n| format!("fn f{n}() {{}}\n")).collect();
        std::fs::write(dir.path().join("src/lib.rs"), body).unwrap();

        let result = VerifyTool.call(&ctx, json!({})).await;
        assert!(
            result.content.contains("--- failing locations (read) ---"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("src/lib.rs:5\n"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains(">    5  fn f5() {}"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("     2  fn f2() {}"),
            "{}",
            result.content
        );
        assert!(!result.content.contains("fn f9()"), "{}", result.content);

        // The attached lines count as read, so an edit there needs no re-read.
        let path = ctx.resolve("src/lib.rs").unwrap();
        let reads = crate::tools::write::lock_reads(&ctx);
        assert!(reads.is_current(&path));
        assert!(!reads.is_fully_read(&path), "only the window was shown");
    }

    #[tokio::test]
    async fn a_failure_naming_no_project_file_attaches_nothing() {
        let (_dir, ctx) = test_ctx(vec![step("t", "echo 'at /usr/lib/x.rs:3:1'; exit 1")]);
        let result = VerifyTool.call(&ctx, json!({})).await;
        assert!(
            !result.content.contains("failing locations"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn detected_steps_run_and_say_where_they_came_from() {
        let (_dir, mut ctx) = test_ctx(vec![step("test", "exit 0")]);
        ctx.verify.detected_from = vec!["Cargo.toml".into()];
        let result = VerifyTool.call(&ctx, json!({})).await;
        assert!(result.content.contains("PASS test"), "{}", result.content);
        assert!(
            result
                .content
                .contains("(steps detected from Cargo.toml; not saved)"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn step_filter_runs_only_the_named_step() {
        let (_dir, ctx) = test_ctx(vec![step("unit", "exit 0"), step("lint", "exit 0")]);
        let result = VerifyTool.call(&ctx, json!({"step":"lint"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("PASS lint"), "{}", result.content);
        assert!(!result.content.contains("PASS unit"), "{}", result.content);
    }

    #[tokio::test]
    async fn no_steps_is_prescriptive() {
        let (_dir, ctx) = test_ctx(Vec::new());
        let result = VerifyTool.call(&ctx, json!({})).await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[NO_VERIFY]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("verify.toml"), "{}", result.content);
    }

    #[tokio::test]
    async fn no_steps_suggests_the_detected_project_command() {
        // Go detection is deterministic: the Python path depends on whether
        // pytest is importable on the machine, which differs in CI.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/demo\n").unwrap();
        let ctx = crate::tools::write::test_context(dir.path());
        let result = VerifyTool.call(&ctx, json!({})).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("go.mod"),
            "must name the marker it detected: {}",
            result.content
        );
        assert!(
            result.content.contains("bash") && result.content.contains("go test ./..."),
            "must offer a runnable detected command: {}",
            result.content
        );
        assert!(
            !result.content.contains("cargo test"),
            "must not suggest cargo in a Go project: {}",
            result.content
        );
    }
}

//! `bash` tool.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::sandbox::Sandbox;
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_str, optional_u64,
    required_str,
};

/// Default wall-clock limit for one command.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard ceiling; larger values are clamped.
const MAX_TIMEOUT_SECS: u64 = 600;
/// Combined bytes captured from stdout and stderr.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// Combined bytes returned to the model.
const MAX_PREVIEW_BYTES: usize = 64 * 1024;
/// How long a terminated process group gets after SIGTERM.
const TERM_GRACE: Duration = Duration::from_secs(2);

pub struct BashTool;

/// Build a non-interactive shell command in its own process group.
///
/// Shared with `bg` so both tools agree on shell semantics and group
/// ownership: a timeout or stop can signal the whole tree. When `sandbox` is
/// present the command is wrapped by the OS sandbox; a failure is returned to
/// the caller and must never fall back to an unsandboxed run.
pub(crate) fn shell_command(
    command: &str,
    cwd: &Path,
    sandbox: Option<&Sandbox>,
) -> Result<tokio::process::Command, String> {
    if let Some(sandbox) = sandbox {
        return sandbox.wrap(command, cwd);
    }
    use std::os::unix::process::CommandExt;
    let mut cmd = tokio::process::Command::new("bash");
    cmd.arg("-lc")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.as_std_mut().process_group(0);
    Ok(cmd)
}

/// Error returned when the sandbox cannot build a command. Fail closed: the
/// model gets an actionable message instead of an unsandboxed run.
pub(crate) fn sandbox_error(message: &str) -> ToolResult {
    let hint = if message.contains("--no-sandbox") {
        String::new()
    } else {
        "\n\nNo command was run. Rerun with --no-sandbox only if unsandboxed shell access is acceptable.".to_string()
    };
    ToolResult::err("SANDBOX_UNAVAILABLE", format!("{message}{hint}"))
}

/// Signal a process group. Guarded so a missing pid can never hit our own
/// process group (`killpg(0, ...)` would do exactly that).
pub(crate) fn kill_group(pid: u32, signal: Signal) {
    if pid <= 1 {
        return;
    }
    let _ = killpg(Pid::from_raw(pid as i32), signal);
}

/// SIGTERM the child's process group, wait up to two seconds, then SIGKILL.
///
/// Returns the exit status when the child could be reaped. The group is
/// SIGKILLed even after a clean exit so orphaned grandchildren cannot keep
/// running with the pipes open.
pub(crate) async fn terminate_group(
    child: &mut tokio::process::Child,
) -> Option<std::process::ExitStatus> {
    let pid = child.id();
    if let Some(pid) = pid {
        kill_group(pid, Signal::SIGTERM);
    }
    let status = match tokio::time::timeout(TERM_GRACE, child.wait()).await {
        Ok(result) => result.ok(),
        Err(_) => {
            if let Some(pid) = pid {
                kill_group(pid, Signal::SIGKILL);
            }
            let _ = child.start_kill();
            match tokio::time::timeout(TERM_GRACE, child.wait()).await {
                Ok(result) => result.ok(),
                Err(_) => None,
            }
        }
    };
    if let Some(pid) = pid {
        kill_group(pid, Signal::SIGKILL);
    }
    status
}

impl BashTool {
    async fn run(&self, ctx: &ToolContext, args: &Value) -> ToolResult {
        let command = match required_str(args, "command") {
            Ok(command) => command,
            Err(err) => return err,
        };
        let timeout_secs = optional_u64(args, "timeout")
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        let cwd = match optional_str(args, "cwd") {
            Some(raw) => match ctx.resolve(&raw) {
                Ok(path) => path,
                Err(err) => return err,
            },
            None => ctx.cwd.clone(),
        };

        // Scope gate first: nothing is spawned for an out-of-scope command.
        let gate = match ctx.security.lock() {
            Ok(security) => security.gate(&command),
            Err(_) => {
                return ToolResult::err(
                    "internal",
                    "security state lock is poisoned; restart the session",
                );
            }
        };
        if let Err(message) = gate {
            return ToolResult::err("SEC_SCOPE", message);
        }

        if ctx.is_cancelled() {
            return cancelled_result();
        }

        let mut cmd = match shell_command(&command, &cwd, ctx.sandbox.as_deref()) {
            Ok(cmd) => cmd,
            Err(message) => return sandbox_error(&message),
        };
        cmd.kill_on_drop(true);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(err) => {
                let hint = if ctx.sandbox.is_some() {
                    "the sandbox backend failed to start the command; fix the backend or rerun with --no-sandbox"
                } else {
                    "check that bash is installed and the directory exists"
                };
                return ToolResult::err(
                    "SPAWN_FAILED",
                    format!(
                        "could not start bash in {}: {err}; {hint}, then retry with {{'command': '...'}}",
                        cwd.display()
                    ),
                );
            }
        };

        let captured = Arc::new(AtomicUsize::new(0));
        let stdout_task = spawn_reader(child.stdout.take(), captured.clone());
        let stderr_task = spawn_reader(child.stderr.take(), captured.clone());

        enum Outcome {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }

        let sleep = tokio::time::sleep(Duration::from_secs(timeout_secs));
        tokio::pin!(sleep);
        let cancel = ctx.cancel.cancelled();
        tokio::pin!(cancel);

        let outcome = tokio::select! {
            status = child.wait() => Outcome::Exited(status),
            _ = &mut sleep => Outcome::TimedOut,
            _ = &mut cancel => Outcome::Cancelled,
        };

        let (status, timed_out, cancelled) = match outcome {
            Outcome::Exited(status) => (status.ok(), false, false),
            Outcome::TimedOut => (terminate_group(&mut child).await, true, false),
            Outcome::Cancelled => (terminate_group(&mut child).await, false, true),
        };

        let stdout = collect(stdout_task).await;
        let stderr = collect(stderr_task).await;
        let exit_code = status.and_then(|status| status.code());

        let body = render_preview(&stdout, &stderr);
        let details = json!({
            "exit_code": exit_code,
            "timed_out": timed_out,
            "cancelled": cancelled,
        });

        let mut content = String::new();
        let is_error = if timed_out {
            content.push_str(&format!(
                "[timeout] command exceeded {timeout_secs}s and was terminated (SIGTERM, then SIGKILL after 2s); reduce the work, raise timeout up to {}s, or use bg for long-running commands.\n\n",
                MAX_TIMEOUT_SECS
            ));
            true
        } else if cancelled {
            content.push_str(
                "[cancelled] command was cancelled and its process group was terminated; re-run it when ready.\n\n",
            );
            true
        } else if exit_code == Some(0) {
            false
        } else {
            match exit_code {
                Some(code) => content.push_str(&format!(
                    "[exit {code}] command failed; inspect the output below, then adjust the command or code and retry.\n\n"
                )),
                None => content.push_str(
                    "[exit -1] command ended without an exit code (killed by a signal); inspect the output and retry the command.\n\n",
                ),
            }
            true
        };
        content.push_str(&body);

        ToolResult {
            content,
            is_error,
            details: Some(details),
        }
    }
}

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "bash"
    }

    fn description(&self) -> &'static str {
        "Run a non-interactive shell command with a timeout (default 120s, max 600s)."
    }

    fn evidence_enabled(&self) -> bool {
        true
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "command": {"type": "string", "description": "Shell command run with bash -lc"},
                "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECS, "description": "Seconds (default 120). Use bg for servers and watchers."},
                "cwd": {"type": "string", "description": "Working directory, relative to the session cwd"}
            }),
            &["command"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        self.run(ctx, &args).await
    }
}

fn cancelled_result() -> ToolResult {
    ToolResult {
        content: "[cancelled] command was not started because the turn was cancelled; re-run it when ready."
            .to_string(),
        is_error: true,
        details: Some(json!({
            "exit_code": null,
            "timed_out": false,
            "cancelled": true,
        })),
    }
}

pub(crate) fn spawn_reader<R>(reader: Option<R>, captured: Arc<AtomicUsize>) -> JoinHandle<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let Some(mut reader) = reader else {
            return Vec::new();
        };
        use tokio::io::AsyncReadExt;
        let mut out = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let previous = captured.fetch_add(n, Ordering::SeqCst);
                    if previous < MAX_CAPTURE_BYTES {
                        let take = n.min(MAX_CAPTURE_BYTES - previous);
                        out.extend_from_slice(&buf[..take]);
                    }
                }
            }
        }
        out
    })
}

/// Await a reader, but never block on a pipe held open by a stray grandchild.
pub(crate) async fn collect(mut handle: JoinHandle<Vec<u8>>) -> Vec<u8> {
    tokio::select! {
        result = &mut handle => result.unwrap_or_default(),
        _ = tokio::time::sleep(TERM_GRACE) => {
            handle.abort();
            Vec::new()
        }
    }
}

fn render_preview(stdout: &[u8], stderr: &[u8]) -> String {
    let mut body = String::new();
    let stdout = String::from_utf8_lossy(stdout);
    if !stdout.is_empty() {
        body.push_str("stdout:\n");
        body.push_str(&stdout);
        if !body.ends_with('\n') {
            body.push('\n');
        }
    }
    let stderr = String::from_utf8_lossy(stderr);
    if !stderr.is_empty() {
        body.push_str("stderr:\n");
        body.push_str(&stderr);
        if !body.ends_with('\n') {
            body.push('\n');
        }
    }
    if body.is_empty() {
        body.push_str("(no output)\n");
    }
    let (mut body, truncated) = take_bytes(&body, MAX_PREVIEW_BYTES);
    if truncated {
        body.push_str("[output truncated to 64 KB]\n");
    }
    body
}

/// Take at most `max` bytes without splitting a UTF-8 code point.
pub(crate) fn take_bytes(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
pub(crate) fn test_context(dir: &Path) -> ToolContext {
    use std::sync::Mutex;

    use tokio_util::sync::CancellationToken;

    use crate::config::VerifyConfig;
    use crate::paths::Paths;
    use crate::security::SecurityState;
    use crate::session::{APP_VERSION, ArtifactStore, Session, SessionData};
    use crate::tools::{BgRegistry, ReadTracker, SessionHandle};

    let mut paths = Paths::discover(dir).expect("discover paths");
    paths.data_dir = dir.join("data");
    paths.config_dir = dir.join("config");
    let session = Session::create(
        &paths,
        SessionData {
            cwd: dir.display().to_string(),
            endpoint: "http://127.0.0.1:0".into(),
            model: "test".into(),
            profile: "full".into(),
            app: APP_VERSION.into(),
            redact: true,
            context: None,
        },
        true,
    )
    .expect("ephemeral session");
    let artifacts =
        Arc::new(ArtifactStore::new("test", Some(dir.join("artifacts"))).expect("artifacts"));
    let handle: SessionHandle = Arc::new(Mutex::new(session));
    ToolContext {
        cwd: dir.to_path_buf(),
        path_jail: false,
        session: handle,
        artifacts,
        cancel: CancellationToken::new(),
        trust: false,
        verify: VerifyConfig::default(),
        profile: "full".into(),
        packs: vec!["core".into(), "coding".into(), "security".into()],
        reads: Arc::new(Mutex::new(ReadTracker::default())),
        bg: Arc::new(Mutex::new(BgRegistry::new())),
        security: Arc::new(Mutex::new(SecurityState::empty())),
        paths,
        lsp: Arc::new(crate::tools::lsp::LspService::new()),
        sandbox: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sandbox_blocks_home_and_git_writes_and_allows_workspace_writes() {
        use crate::config::SandboxMode;
        use crate::sandbox::Sandbox;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(workspace.join(".git")).unwrap();
        std::fs::write(workspace.join(".git/config"), "[core]\n").unwrap();
        let sandbox = match Sandbox::new(SandboxMode::WorkspaceWrite, workspace.clone()) {
            Ok(sandbox) => sandbox,
            Err(_) => return, // no backend on this host; fail-closed is unit-tested
        };
        let mut ctx = test_context(dir.path());
        ctx.sandbox = Some(Arc::new(sandbox));

        let ssh = BashTool
            .call(&ctx, json!({"command": "cat ~/.ssh/id_ed25519"}))
            .await;
        assert!(ssh.is_error, "home read must fail: {}", ssh.content);

        let inside = BashTool
            .call(
                &ctx,
                json!({"command": "echo ok > inside.txt && cat inside.txt"}),
            )
            .await;
        assert!(!inside.is_error, "{}", inside.content);
        assert!(workspace.join("inside.txt").is_file());

        let home = BashTool
            .call(&ctx, json!({"command": "echo no > $HOME/outside.txt"}))
            .await;
        assert!(home.is_error, "$HOME write must fail: {}", home.content);

        let git = BashTool
            .call(&ctx, json!({"command": "echo no > .git/config"}))
            .await;
        assert!(git.is_error, ".git write must fail: {}", git.content);

        if std::process::Command::new("curl")
            .arg("--version")
            .output()
            .is_ok()
        {
            let net = BashTool
                .call(
                    &ctx,
                    json!({"command": "curl -sS --max-time 2 https://example.com"}),
                )
                .await;
            assert!(net.is_error, "network must be closed: {}", net.content);
        }
    }

    #[tokio::test]
    async fn read_only_sandbox_blocks_workspace_writes() {
        use crate::config::SandboxMode;
        use crate::sandbox::Sandbox;

        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().canonicalize().unwrap();
        let sandbox = match Sandbox::new(SandboxMode::ReadOnly, workspace.clone()) {
            Ok(sandbox) => sandbox,
            Err(_) => return,
        };
        let mut ctx = test_context(dir.path());
        ctx.sandbox = Some(Arc::new(sandbox));

        let denied = BashTool
            .call(&ctx, json!({"command": "echo no > inside.txt"}))
            .await;
        assert!(denied.is_error, "{}", denied.content);

        let temp = BashTool
            .call(&ctx, json!({"command": "echo ok > /tmp/lh-sandbox-test"}))
            .await;
        assert!(
            !temp.is_error,
            "private /tmp must be writable: {}",
            temp.content
        );
    }

    #[tokio::test]
    async fn timeout_terminates_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let started = std::time::Instant::now();
        let result = BashTool
            .call(&ctx, json!({"command": "sleep 5", "timeout": 1}))
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout took {:?}",
            started.elapsed()
        );
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[timeout]"),
            "{}",
            result.content
        );
        assert_eq!(result.details.unwrap()["timed_out"], json!(true));
    }

    #[tokio::test]
    async fn non_zero_exit_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let result = BashTool.call(&ctx, json!({"command": "exit 3"})).await;
        assert!(result.is_error);
        assert!(result.content.starts_with("[exit 3]"), "{}", result.content);
    }

    #[tokio::test]
    async fn stdout_and_stderr_are_labeled() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let result = BashTool
            .call(&ctx, json!({"command": "echo out; echo err >&2"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("stdout:\nout"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("stderr:\nerr"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn cwd_is_resolved_against_session() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        let ctx = test_context(dir.path());
        let result = BashTool
            .call(&ctx, json!({"command": "pwd", "cwd": "sub"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("/sub"), "{}", result.content);
    }

    #[tokio::test]
    async fn scope_gate_blocks_out_of_scope_tool() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let result = BashTool
            .call(&ctx, json!({"command": "nmap 192.0.2.1"}))
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[SEC_SCOPE]"),
            "{}",
            result.content
        );
    }
}

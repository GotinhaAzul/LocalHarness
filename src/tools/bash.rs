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
use crate::text::head_and_tail;
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_str, optional_u64,
    required_str,
};

/// Default wall-clock limit for one command.
const DEFAULT_TIMEOUT_SECS: u64 = 120;
/// Hard ceiling; larger values are clamped.
const MAX_TIMEOUT_SECS: u64 = 600;
/// Combined bytes captured from the start of stdout and stderr.
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
/// Bytes each stream also keeps from its end once the capture is full, so
/// a long build still shows how it finished.
const TAIL_CAPTURE_BYTES: usize = 256 * 1024;
/// Combined bytes returned to the model (start and end; the middle is
/// dropped first).
const MAX_PREVIEW_BYTES: usize = 64 * 1024;
/// Of `MAX_PREVIEW_BYTES`, the part kept from the start.
const PREVIEW_HEAD_BYTES: usize = 16 * 1024;
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
    let mut cmd = crate::tools::procenv::command("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
        if let Some(message) = crate::tools::git::destructive_git(&command) {
            return ToolResult::err("GIT_DESTRUCTIVE", message);
        }

        // A command that deletes or overwrites is snapshotted first, so
        // `undo` and `/rewind` can bring the files back even though the
        // change happens outside the harness's own write path.
        let snapshots = crate::tools::undo::bash_snapshots(ctx, &command);

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
        let mut details = json!({
            "exit_code": exit_code,
            "timed_out": timed_out,
            "cancelled": cancelled,
        });
        let mut snapshot_note = String::new();
        if !snapshots.is_empty() {
            let entries = crate::tools::undo::bash_change_entries(&snapshots);
            if !entries.is_empty() {
                let names: Vec<&str> = entries
                    .iter()
                    .take(5)
                    .filter_map(|entry| entry.get("path").and_then(Value::as_str))
                    .collect();
                let more = entries.len().saturating_sub(names.len());
                let more = if more > 0 {
                    format!(", and {more} more")
                } else {
                    String::new()
                };
                let limit = if snapshots.len() >= crate::tools::undo::MAX_BASH_SNAPSHOTS {
                    " (snapshot limit reached; some files were not captured)"
                } else {
                    ""
                };
                snapshot_note = format!(
                    "[snapshot] saved {} file(s) before this command: {}{more}{limit}. If it deleted or overwrote something you need, undo restores it and /rewind brings it back.\n",
                    entries.len(),
                    names.join(", ")
                );
            }
            details["session"] = json!(entries);
        }

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
        content.push_str(&snapshot_note);
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
    fn preview_tail(&self) -> bool {
        true
    }

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
        // What did not fit the head: the last TAIL_CAPTURE_BYTES of it, and
        // how much was dropped in between.
        let mut tail = std::collections::VecDeque::<u8>::new();
        let mut dropped = 0usize;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let previous = captured.fetch_add(n, Ordering::SeqCst);
                    let take = n.min(MAX_CAPTURE_BYTES.saturating_sub(previous));
                    out.extend_from_slice(&buf[..take]);
                    tail.extend(&buf[take..n]);
                    if tail.len() > TAIL_CAPTURE_BYTES {
                        let excess = tail.len() - TAIL_CAPTURE_BYTES;
                        tail.drain(..excess);
                        dropped += excess;
                    }
                }
            }
        }
        if dropped > 0 {
            out.extend_from_slice(
                format!("\n[... {dropped} bytes of output omitted ...]\n").as_bytes(),
            );
        }
        out.extend(tail);
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
    // Color codes and redrawn progress lines are noise for the model.
    let stdout = String::from_utf8_lossy(stdout);
    let stdout = crate::sanitize::terminal_output(&stdout);
    if !stdout.is_empty() {
        body.push_str("stdout:\n");
        body.push_str(&stdout);
        if !body.ends_with('\n') {
            body.push('\n');
        }
    }
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = crate::sanitize::terminal_output(&stderr);
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
    head_and_tail(
        &body,
        PREVIEW_HEAD_BYTES,
        MAX_PREVIEW_BYTES - PREVIEW_HEAD_BYTES,
    )
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
            permissions: None,
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
        edit_mode: Default::default(),
        packs: vec!["core".into(), "coding".into(), "security".into()],
        reads: Arc::new(Mutex::new(ReadTracker::default())),
        bg: Arc::new(Mutex::new(BgRegistry::new())),
        security: Arc::new(Mutex::new(SecurityState::empty())),
        paths,
        lsp: Arc::new(crate::tools::lsp::LspService::new()),
        sandbox: None,
        vision: Default::default(),
        caps: Default::default(),
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
            .call(&ctx, json!({"command": "echo ok > /tmp/tot-sandbox-test"}))
            .await;
        assert!(
            !temp.is_error,
            "private /tmp must be writable: {}",
            temp.content
        );
    }

    #[tokio::test]
    async fn huge_output_keeps_how_the_command_ended() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        // ~2 MB of noise, then the line that matters.
        let command = "for i in $(seq 1 40000); do echo \"   Compiling crate-$i v0.1.0 (padding padding)\"; done; echo 'error: THE-REAL-FAILURE' >&2; exit 1";
        let result = BashTool.call(&ctx, json!({"command": command})).await;
        assert!(result.is_error);
        assert!(
            result.content.contains("THE-REAL-FAILURE"),
            "the end of stderr is kept"
        );
        assert!(
            result.content.contains("Compiling crate-40000 "),
            "the end of stdout is kept"
        );
        assert!(
            result.content.contains("Compiling crate-1 "),
            "the start is kept"
        );
        assert!(result.content.contains("bytes of output omitted"));
        assert!(
            result.content.len() <= MAX_PREVIEW_BYTES + 512,
            "{}",
            result.content.len()
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

    /// Record the `details.session` entries the agent loop would persist.
    fn record_session(details: &Value, ctx: &crate::tools::ToolContext) {
        let entries = details["session"].as_array().cloned().unwrap_or_default();
        let mut session = ctx.session.lock().unwrap();
        for entry in entries {
            let Some(kind) = entry.get("kind").and_then(Value::as_str) else {
                continue;
            };
            let mut payload = entry.as_object().cloned().unwrap_or_default();
            payload.remove("kind");
            session
                .append(crate::session::EntryKind::Custom(
                    crate::session::CustomData {
                        kind: kind.to_string(),
                        payload,
                    },
                ))
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_bash_delete_is_snapshotted_and_undo_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let notes = dir.path().join("notes.txt.bak");
        std::fs::write(&notes, "customer contract renewal terms (only copy!)\n").unwrap();
        let ctx = test_context(dir.path());

        let result = BashTool
            .call(&ctx, json!({"command": "rm notes.txt.bak"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(!notes.exists());
        assert!(
            result.content.contains("[snapshot] saved 1 file(s)"),
            "{}",
            result.content
        );
        let details = result.details.clone().unwrap();
        record_session(&details, &ctx);

        let undone = crate::tools::undo::UndoTool
            .call(&ctx, json!({"path": "notes.txt.bak"}))
            .await;
        assert!(!undone.is_error, "{}", undone.content);
        assert_eq!(
            std::fs::read_to_string(&notes).unwrap(),
            "customer contract renewal terms (only copy!)\n"
        );
        assert!(
            undone.content.contains("bash command"),
            "{}",
            undone.content
        );
    }

    #[tokio::test]
    async fn a_failing_delete_still_records_the_files_it_removed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("keep.txt"), "only copy\n").unwrap();
        std::fs::create_dir_all(dir.path().join("blocked")).unwrap();
        let ctx = test_context(dir.path());

        // `rm keep.txt blocked` removes the file, then fails on the directory.
        let result = BashTool
            .call(&ctx, json!({"command": "rm keep.txt blocked"}))
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(!dir.path().join("keep.txt").exists());
        let details = result.details.clone().unwrap();
        let entries = details["session"].as_array().cloned().unwrap_or_default();
        assert_eq!(entries.len(), 1, "{details}");
        assert_eq!(entries[0]["path"], "keep.txt");
        record_session(&details, &ctx);

        let undone = crate::tools::undo::UndoTool
            .call(&ctx, json!({"path": "keep.txt"}))
            .await;
        assert!(!undone.is_error, "{}", undone.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keep.txt")).unwrap(),
            "only copy\n"
        );
    }

    #[tokio::test]
    async fn redirection_forms_are_parsed_and_created_files_are_undoable() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        for (command, created) in [
            ("echo one > out.txt", "out.txt"),
            ("echo two >err.txt 2>/dev/null", "err.txt"),
            ("printf three>attached.txt", "attached.txt"),
            ("echo four &>both.txt", "both.txt"),
        ] {
            let result = BashTool.call(&ctx, json!({"command": command})).await;
            assert!(!result.is_error, "{command}: {}", result.content);
            assert!(
                dir.path().join(created).is_file(),
                "{command} must create {created}"
            );
            let details = result.details.clone().unwrap();
            let entries = details["session"].as_array().cloned().unwrap_or_default();
            assert!(
                entries.iter().any(
                    |entry| entry["path"].as_str() == Some(created) && entry["created"] == true
                ),
                "{command}: {details}"
            );
            record_session(&details, &ctx);
            let undone = crate::tools::undo::UndoTool
                .call(&ctx, json!({"path": created}))
                .await;
            assert!(!undone.is_error, "{}", undone.content);
            assert!(
                !dir.path().join(created).exists(),
                "undo must remove a file the bash command created"
            );
        }
    }

    #[tokio::test]
    async fn an_unchanged_redirection_target_gets_no_undo_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("same.txt");
        std::fs::write(&path, "same\n").unwrap();
        let ctx = test_context(dir.path());
        let result = BashTool
            .call(&ctx, json!({"command": "echo same > same.txt"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        let details = result.details.unwrap();
        assert!(
            details["session"]
                .as_array()
                .is_none_or(|entries| entries.is_empty()),
            "an identical overwrite is not a change: {details}"
        );
        assert!(
            !result.content.contains("[snapshot] saved"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn shell_redirection_is_snapshotted_too() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("app.log");
        std::fs::write(&log, "old line\n").unwrap();
        let ctx = test_context(dir.path());
        let result = BashTool
            .call(&ctx, json!({"command": "echo new > app.log"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        let details = result.details.clone().unwrap();
        assert_eq!(details["session"].as_array().unwrap().len(), 1, "{details}");
        record_session(&details, &ctx);
        std::fs::write(dir.path().join("app.log"), "changed again\n").unwrap();
        // The recorded after-hash does not match, so undo refuses to discard
        // the newer content.
        let undone = crate::tools::undo::UndoTool
            .call(&ctx, json!({"path": "app.log"}))
            .await;
        assert!(
            undone.content.starts_with("[UNDO_CONFLICT]"),
            "{}",
            undone.content
        );
    }
}

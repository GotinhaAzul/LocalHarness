//! `bg` tool: supervised background processes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nix::sys::signal::Signal;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::tools::bash::{kill_group, sandbox_error, shell_command, terminate_group};
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_str, optional_u64,
    required_str,
};

/// Maximum live (not yet exited) processes per session.
const MAX_LIVE: usize = 4;
/// Log bytes kept per process; later output is drained but discarded.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_LOG_LINES: u64 = 120;
const MAX_LOG_LINES: u64 = 200;
const MAX_LOG_CHARS: usize = 4000;
const MAX_COMMAND_CHARS: usize = 80;
/// How much of the log tail is read from disk before line selection.
const LOG_READ_TAIL: u64 = 256 * 1024;
const TERM_GRACE: Duration = Duration::from_secs(2);

/// Live background processes, owned by the harness and shared with the tool.
#[derive(Debug, Default)]
pub struct BgRegistry {
    pub processes: std::collections::HashMap<String, BgProcess>,
    pub next_id: u32,
}

#[derive(Debug)]
pub struct BgProcess {
    pub id: String,
    pub command: String,
    pub child: tokio::process::Child,
    pub log_path: PathBuf,
    pub started: Instant,
    pub pid: u32,
    pub exit_code: Option<i32>,
    pub exited: bool,
}

impl BgRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record exits without blocking, so status and the live cap stay accurate.
    pub fn reap_all(&mut self) {
        for process in self.processes.values_mut() {
            reap(process);
        }
    }

    pub fn live_count(&self) -> usize {
        self.processes
            .values()
            .filter(|process| !process.exited)
            .count()
    }

    /// Kill every process group. Used on session shutdown.
    pub async fn shutdown(&mut self) {
        let drained: Vec<BgProcess> = self.processes.drain().map(|(_, process)| process).collect();
        for process in &drained {
            if !process.exited {
                kill_group(process.pid, Signal::SIGTERM);
            }
        }
        let deadline = Instant::now() + TERM_GRACE;
        for mut process in drained {
            if process.exited {
                // Descendants may still be alive in the process group.
                kill_group(process.pid, Signal::SIGKILL);
                continue;
            }
            loop {
                match process.child.try_wait() {
                    Ok(Some(status)) => {
                        process.exited = true;
                        process.exit_code = status.code();
                        break;
                    }
                    Ok(None) => {
                        if Instant::now() >= deadline {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(_) => {
                        process.exited = true;
                        break;
                    }
                }
            }
            if !process.exited {
                kill_group(process.pid, Signal::SIGKILL);
                let _ = process.child.start_kill();
                let _ = tokio::time::timeout(TERM_GRACE, process.child.wait()).await;
            }
            // Clean up any descendants that outlived the leader.
            kill_group(process.pid, Signal::SIGKILL);
        }
    }
}

fn reap(process: &mut BgProcess) {
    if process.exited {
        return;
    }
    match process.child.try_wait() {
        Ok(Some(status)) => {
            process.exited = true;
            process.exit_code = status.code();
        }
        Ok(None) => {}
        Err(_) => {
            process.exited = true;
        }
    }
}

pub struct BgTool;

impl BgTool {
    async fn start(&self, ctx: &ToolContext, args: &Value) -> ToolResult {
        let command = match required_str(args, "command") {
            Ok(command) => command,
            Err(err) => return err,
        };

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

        let (id, log_path) = {
            let mut registry = match ctx.bg.lock() {
                Ok(registry) => registry,
                Err(_) => return poisoned(),
            };
            registry.reap_all();
            if registry.live_count() >= MAX_LIVE {
                return limit_error();
            }
            registry.next_id += 1;
            let id = format!("bg-{}", registry.next_id);
            let directory = match log_dir(ctx) {
                Ok(directory) => directory,
                Err(err) => return log_error(err),
            };
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0);
            let log_path = directory.join(format!("{id}-{stamp:x}.log"));
            (id, log_path)
        };

        let stdout_file = match open_log(&log_path) {
            Ok(file) => file,
            Err(err) => return log_error(err),
        };
        let stderr_file = match open_log(&log_path) {
            Ok(file) => file,
            Err(err) => return log_error(err),
        };

        let mut cmd = match shell_command(&command, &ctx.cwd, ctx.sandbox.as_deref()) {
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
                    "check that bash is installed"
                };
                return ToolResult::err(
                    "SPAWN_FAILED",
                    format!("could not start bash: {err}; {hint}, then retry"),
                );
            }
        };
        let pid = child.id().unwrap_or(0);

        let used = Arc::new(AtomicU64::new(0));
        if let Some(stdout) = child.stdout.take() {
            tokio::spawn(pump(
                stdout,
                tokio::fs::File::from_std(stdout_file),
                used.clone(),
            ));
        }
        if let Some(stderr) = child.stderr.take() {
            tokio::spawn(pump(stderr, tokio::fs::File::from_std(stderr_file), used));
        }

        let started = Instant::now();
        {
            let mut registry = match ctx.bg.lock() {
                Ok(registry) => registry,
                Err(_) => return poisoned(),
            };
            registry.reap_all();
            registry.processes.insert(
                id.clone(),
                BgProcess {
                    id: id.clone(),
                    command: command.clone(),
                    child,
                    log_path: log_path.clone(),
                    started,
                    pid,
                    exit_code: None,
                    exited: false,
                },
            );
        }

        ToolResult::ok(format!(
            "started {id} (pid {pid}); log {}\npoll with bg {{\"action\":\"status\",\"id\":\"{id}\"}} then bg {{\"action\":\"logs\",\"id\":\"{id}\",\"lines\":120}}",
            log_path.display()
        ))
        .with_details(json!({
            "id": id,
            "pid": pid,
            "running": true,
            "exit_code": null,
            "log": log_path.display().to_string(),
        }))
    }

    fn status(&self, ctx: &ToolContext, args: &Value) -> ToolResult {
        let wanted = optional_str(args, "id");
        let mut registry = match ctx.bg.lock() {
            Ok(registry) => registry,
            Err(_) => return poisoned(),
        };
        registry.reap_all();

        if let Some(id) = wanted {
            let Some(process) = registry.processes.get(&id) else {
                return unknown_id(&id);
            };
            let (line, details) = describe(process);
            return ToolResult::ok(line).with_details(details);
        }

        if registry.processes.is_empty() {
            return ToolResult::ok(
                "no background processes; start one with bg {\"action\":\"start\",\"command\":\"...\"}",
            )
            .with_details(json!({"processes": [], "max": MAX_LIVE}));
        }

        let mut ids: Vec<&String> = registry.processes.keys().collect();
        ids.sort();
        let mut lines = Vec::new();
        let mut details = Vec::new();
        for id in ids {
            let Some(process) = registry.processes.get(id) else {
                continue;
            };
            let (line, described) = describe(process);
            lines.push(line);
            details.push(described);
        }
        let running = registry.live_count();
        ToolResult::ok(lines.join("\n")).with_details(json!({
            "processes": details,
            "running": running,
            "max": MAX_LIVE,
        }))
    }

    fn logs(&self, ctx: &ToolContext, args: &Value) -> ToolResult {
        let id = match required_str(args, "id") {
            Ok(id) => id,
            Err(_) => {
                return ToolResult::err(
                    "invalid_args",
                    "bg logs needs \"id\"; use bg {\"action\":\"status\"} to list background processes",
                );
            }
        };
        let lines = optional_u64(args, "lines")
            .unwrap_or(DEFAULT_LOG_LINES)
            .clamp(1, MAX_LOG_LINES) as usize;

        let (log_path, running, exit_code) = {
            let mut registry = match ctx.bg.lock() {
                Ok(registry) => registry,
                Err(_) => return poisoned(),
            };
            registry.reap_all();
            let Some(process) = registry.processes.get(&id) else {
                return unknown_id(&id);
            };
            (process.log_path.clone(), !process.exited, process.exit_code)
        };

        let raw = match read_log(&log_path) {
            Ok(raw) => raw,
            Err(err) => {
                return ToolResult::err(
                    "log_unreadable",
                    format!(
                        "could not read {}: {err}; the process may have been stopped and its log removed",
                        log_path.display()
                    ),
                );
            }
        };
        let body = tail_chars(&last_lines(&raw, lines), MAX_LOG_CHARS);
        let body = if body.is_empty() {
            "(log is empty; the process may not have written output yet)".to_string()
        } else {
            body
        };
        ToolResult::ok(format!(
            "{id} log (last {lines} lines, {})\n\n{body}",
            log_path.display()
        ))
        .with_details(json!({
            "id": id,
            "path": log_path.display().to_string(),
            "running": running,
            "exit_code": exit_code,
        }))
    }

    async fn stop(&self, ctx: &ToolContext, args: &Value) -> ToolResult {
        let id = match required_str(args, "id") {
            Ok(id) => id,
            Err(_) => {
                return ToolResult::err(
                    "invalid_args",
                    "bg stop needs \"id\"; use bg {\"action\":\"status\"} to list background processes",
                );
            }
        };

        let removed = {
            let mut registry = match ctx.bg.lock() {
                Ok(registry) => registry,
                Err(_) => return poisoned(),
            };
            registry.reap_all();
            registry.processes.remove(&id)
        };
        let Some(mut process) = removed else {
            return unknown_id(&id);
        };

        let was_exited = process.exited;
        let exit_code = if was_exited {
            kill_group(process.pid, Signal::SIGKILL);
            process.exit_code
        } else {
            let status = terminate_group(&mut process.child).await;
            let code = status.and_then(|status| status.code());
            kill_group(process.pid, Signal::SIGKILL);
            code
        };

        let content = if was_exited {
            match process.exit_code {
                Some(code) => format!(
                    "{id} had already exited with code {code}; process group {} cleaned up",
                    process.pid
                ),
                None => format!(
                    "{id} had already exited after a signal; process group {} cleaned up",
                    process.pid
                ),
            }
        } else {
            format!(
                "stopped {id} (pid {}); log {}",
                process.pid,
                process.log_path.display()
            )
        };
        ToolResult::ok(content).with_details(json!({
            "id": id,
            "pid": process.pid,
            "running": false,
            "exit_code": exit_code,
        }))
    }
}

#[async_trait]
impl Tool for BgTool {
    fn name(&self) -> &'static str {
        "bg"
    }

    fn description(&self) -> &'static str {
        "Start and supervise background processes: start, status, logs, stop (max 4 live)."
    }

    fn evidence_enabled(&self) -> bool {
        true
    }

    fn evidence_command(&self, args: &Value) -> Option<String> {
        if let Some(command) = args.get("command").and_then(Value::as_str) {
            return Some(command.to_string());
        }
        let action = args
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("status");
        let id = args.get("id").and_then(Value::as_str).unwrap_or("");
        Some(format!("bg {action} {id}").trim().to_string())
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "action": {"type": "string", "enum": ["start", "status", "logs", "stop"]},
                "command": {"type": "string", "description": "start: shell command to run in the background."},
                "id": {"type": "string", "description": "logs/stop: process id from start, e.g. bg-1; status lists all when omitted."},
                "lines": {"type": "integer", "minimum": 1, "maximum": MAX_LOG_LINES, "description": "logs: trailing lines to show."}
            }),
            &["action"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        match args.get("action").and_then(Value::as_str) {
            Some("start") => self.start(ctx, &args).await,
            Some("status") => self.status(ctx, &args),
            Some("logs") => self.logs(ctx, &args),
            Some("stop") => self.stop(ctx, &args).await,
            Some(other) => ToolResult::err(
                "invalid_args",
                format!("unknown action {other:?}; use start, status, logs, or stop"),
            ),
            None => ToolResult::err(
                "invalid_args",
                "missing required argument \"action\"; use start, status, logs, or stop",
            ),
        }
    }
}

fn describe(process: &BgProcess) -> (String, Value) {
    let elapsed = process.started.elapsed().as_secs_f32();
    let state = if process.exited {
        match process.exit_code {
            Some(code) => format!("exited code={code}"),
            None => "exited (signal)".to_string(),
        }
    } else {
        "running".to_string()
    };
    (
        format!(
            "{} {state} pid={} elapsed={elapsed:.1}s command={}",
            process.id,
            process.pid,
            one_line(&process.command, MAX_COMMAND_CHARS)
        ),
        json!({
            "id": process.id,
            "pid": process.pid,
            "running": !process.exited,
            "exit_code": process.exit_code,
        }),
    )
}

fn one_line(text: &str, max_chars: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let mut short: String = flat.chars().take(max_chars).collect();
    short.push('…');
    short
}

fn tail_chars(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("[earlier output truncated]\n{}", &text[start..])
}

fn last_lines(text: &str, count: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(count);
    lines[start..].join("\n")
}

fn read_log(path: &Path) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(LOG_READ_TAIL);
    file.seek(SeekFrom::Start(start))?;
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    let mut text = String::from_utf8_lossy(&buffer).to_string();
    if start > 0
        && let Some(newline) = text.find('\n')
    {
        text = text[newline + 1..].to_string();
    }
    Ok(text)
}

async fn pump<R>(mut reader: R, mut file: tokio::fs::File, used: Arc<AtomicU64>)
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut buffer = vec![0u8; 32 * 1024];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let previous = used.fetch_add(n as u64, Ordering::SeqCst);
                if previous < MAX_LOG_BYTES {
                    let take = (n as u64).min(MAX_LOG_BYTES - previous) as usize;
                    if take > 0 {
                        let _ = file.write_all(&buffer[..take]).await;
                    }
                }
            }
        }
    }
    let _ = file.flush().await;
}

fn log_dir(ctx: &ToolContext) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let directory = match ctx.artifacts.root() {
        Some(root) => root.to_path_buf(),
        None => {
            std::env::temp_dir().join(format!("lh-bg-{}", sanitize(ctx.artifacts.session_id())))
        }
    };
    std::fs::create_dir_all(&directory)?;
    let _ = std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700));
    Ok(directory)
}

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn sanitize(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "session".into()
    } else {
        cleaned
    }
}

fn poisoned() -> ToolResult {
    ToolResult::err(
        "internal",
        "background registry lock is poisoned; restart the session",
    )
}

fn limit_error() -> ToolResult {
    ToolResult::err(
        "bg_limit",
        format!(
            "{} background processes are already running; stop one with bg {{\"action\":\"stop\",\"id\":\"bg-1\"}} or wait for one to exit",
            MAX_LIVE
        ),
    )
}

fn unknown_id(id: &str) -> ToolResult {
    ToolResult::err(
        "unknown_bg",
        format!("no background process {id:?}; use bg {{\"action\":\"status\"}} to list ids"),
    )
}

fn log_error(err: std::io::Error) -> ToolResult {
    ToolResult::err(
        "log_failed",
        format!(
            "could not create the background log: {err}; check permissions on the artifacts directory"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::bash::test_context;

    fn id_from(result: &ToolResult) -> String {
        result.details.as_ref().expect("details")["id"]
            .as_str()
            .expect("id")
            .to_string()
    }

    #[tokio::test]
    async fn start_status_logs_stop_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let started = BgTool
            .call(&ctx, json!({"action": "start", "command": "echo hello-bg"}))
            .await;
        assert!(!started.is_error, "{}", started.content);
        let id = id_from(&started);

        let mut logs = String::new();
        for _ in 0..40 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let result = BgTool
                .call(&ctx, json!({"action": "logs", "id": id, "lines": 50}))
                .await;
            assert!(!result.is_error, "{}", result.content);
            logs = result.content;
            if logs.contains("hello-bg") {
                break;
            }
        }
        assert!(logs.contains("hello-bg"), "logs were {logs:?}");

        let status = BgTool
            .call(&ctx, json!({"action": "status", "id": id}))
            .await;
        assert!(!status.is_error, "{}", status.content);
        assert!(status.content.contains(&id), "{}", status.content);

        let stopped = BgTool.call(&ctx, json!({"action": "stop", "id": id})).await;
        assert!(!stopped.is_error, "{}", stopped.content);
        assert_eq!(stopped.details.as_ref().unwrap()["running"], json!(false));
    }

    #[tokio::test]
    async fn status_lists_all_and_unknown_ids_are_recovery_errors() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let started = BgTool
            .call(&ctx, json!({"action": "start", "command": "sleep 30"}))
            .await;
        assert!(!started.is_error, "{}", started.content);
        let id = id_from(&started);

        let all = BgTool.call(&ctx, json!({"action": "status"})).await;
        assert!(!all.is_error, "{}", all.content);
        assert!(all.content.contains(&id), "{}", all.content);

        let missing = BgTool
            .call(&ctx, json!({"action": "status", "id": "bg-99"}))
            .await;
        assert!(missing.is_error);
        assert!(
            missing.content.starts_with("[unknown_bg]"),
            "{}",
            missing.content
        );

        let _ = BgTool.call(&ctx, json!({"action": "stop", "id": id})).await;
    }

    #[tokio::test]
    async fn live_process_cap_is_four() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let mut ids = Vec::new();
        for _ in 0..MAX_LIVE {
            let result = BgTool
                .call(&ctx, json!({"action": "start", "command": "sleep 30"}))
                .await;
            assert!(!result.is_error, "{}", result.content);
            ids.push(id_from(&result));
        }
        let fifth = BgTool
            .call(&ctx, json!({"action": "start", "command": "sleep 30"}))
            .await;
        assert!(fifth.is_error);
        assert!(fifth.content.starts_with("[bg_limit]"), "{}", fifth.content);

        for id in ids {
            let _ = BgTool.call(&ctx, json!({"action": "stop", "id": id})).await;
        }
    }

    #[tokio::test]
    async fn scope_gate_blocks_background_tool() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        let result = BgTool
            .call(
                &ctx,
                json!({"action": "start", "command": "nmap 192.0.2.1"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[SEC_SCOPE]"),
            "{}",
            result.content
        );
        let status = BgTool.call(&ctx, json!({"action": "status"})).await;
        assert!(
            status.content.contains("no background processes"),
            "{}",
            status.content
        );
    }
}

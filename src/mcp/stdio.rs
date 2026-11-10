//! stdio transport: newline-delimited JSON-RPC over a child process's
//! stdin/stdout. Stderr is kept as a bounded tail for error messages.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::transport::{
    Incoming, McpError, Transport, answer_server_request, cancelled_notification, classify,
    notification_message, request_message,
};

/// One message larger than this is discarded instead of buffered.
pub const MESSAGE_CAP: usize = 16 * 1024 * 1024;
/// Bytes of stderr kept for diagnostics.
const STDERR_TAIL: usize = 8 * 1024;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

pub struct StdioTransport {
    name: String,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    child: Mutex<Option<Child>>,
    pid: Option<u32>,
    pending: Pending,
    next_id: AtomicU64,
    alive: Arc<AtomicBool>,
    stderr: Arc<Mutex<VecDeque<u8>>>,
}

pub struct StdioSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
}

impl StdioTransport {
    pub fn spawn(spec: StdioSpec) -> Result<Self, McpError> {
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .envs(&spec.env)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Own process group so closing the server also stops its children
        // (`npx` and `uvx` launchers fork the real server).
        #[cfg(unix)]
        command.process_group(0);
        let mut child = command.spawn().map_err(|error| {
            McpError::Other(format!(
                "cannot start `{}`: {error}; install it or fix `command` in [mcp.{}]",
                spec.command, spec.name
            ))
        })?;
        let pid = child.id();
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr_pipe = child.stderr.take().expect("stderr is piped");

        let transport = Self {
            name: spec.name.clone(),
            stdin: Arc::new(tokio::sync::Mutex::new(stdin)),
            child: Mutex::new(Some(child)),
            pid,
            pending: Arc::default(),
            next_id: AtomicU64::new(1),
            alive: Arc::new(AtomicBool::new(true)),
            stderr: Arc::default(),
        };

        tokio::spawn(read_stderr(
            stderr_pipe,
            transport.stderr.clone(),
            spec.name.clone(),
        ));
        tokio::spawn(read_stdout(
            stdout,
            transport.pending.clone(),
            transport.alive.clone(),
            transport.stdin.clone(),
            spec.name,
        ));
        Ok(transport)
    }

    async fn write(&self, message: &Value) -> Result<(), McpError> {
        let mut line = serde_json::to_vec(message)
            .map_err(|error| McpError::Other(format!("cannot encode a message: {error}")))?;
        line.push(b'\n');
        let mut stdin = self.stdin.lock().await;
        let result = async {
            stdin.write_all(&line).await?;
            stdin.flush().await
        }
        .await;
        result.map_err(|error| {
            self.alive.store(false, Ordering::SeqCst);
            McpError::Closed(format!(
                "MCP server `{}` stopped reading its stdin ({error}){}",
                self.name,
                self.stderr_suffix()
            ))
        })
    }

    fn stderr_suffix(&self) -> String {
        let tail = self.diagnostics();
        if tail.is_empty() {
            String::new()
        } else {
            format!("; stderr: {tail}")
        }
    }

    fn forget(&self, id: u64) {
        lock(&self.pending).remove(&id);
    }

    fn terminate(&self) {
        self.alive.store(false, Ordering::SeqCst);
        #[cfg(unix)]
        if let Some(pid) = self.pid
            && let Ok(pid) = i32::try_from(pid)
        {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, McpError> {
        if !self.is_alive() {
            return Err(McpError::Closed(format!(
                "MCP server `{}` is not running{}",
                self.name,
                self.stderr_suffix()
            )));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if let Err(error) = self.write(&request_message(id, method, &params)).await {
            self.forget(id);
            return Err(error);
        }
        tokio::select! {
            outcome = rx => match outcome {
                Ok(outcome) => outcome,
                Err(_) => {
                    // Give the stderr reader a moment to drain the crash message.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    // Not `Closed`: the request was delivered and may have had
                    // effects, so it must not be retried automatically. The next
                    // call reconnects because the transport is no longer alive.
                    Err(McpError::Other(format!(
                        "MCP server `{}` exited before answering `{method}`{}",
                        self.name,
                        self.stderr_suffix()
                    )))
                }
            },
            _ = tokio::time::sleep(timeout) => {
                self.forget(id);
                let (method, params) = cancelled_notification(id, "timed out");
                let _ = self.notify(method, params).await;
                Err(McpError::Timeout(timeout))
            }
            _ = cancel.cancelled() => {
                self.forget(id);
                let (method, params) = cancelled_notification(id, "cancelled by the user");
                let _ = self.notify(method, params).await;
                Err(McpError::Cancelled)
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.write(&notification_message(method, &params)).await
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn diagnostics(&self) -> String {
        let tail = lock(&self.stderr);
        let bytes: Vec<u8> = tail.iter().copied().collect();
        let text = String::from_utf8_lossy(&bytes);
        let text = text.trim();
        // Keep the last few lines: the newest are the useful ones.
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(6);
        lines[start..].join(" | ")
    }

    async fn close(&self) {
        // Closing stdin is the spec's polite shutdown; then signal the group.
        {
            let mut stdin = self.stdin.lock().await;
            let _ = stdin.shutdown().await;
        }
        let child = lock(&self.child).take();
        if let Some(mut child) = child {
            let exited = tokio::time::timeout(Duration::from_millis(500), child.wait()).await;
            if exited.is_err() {
                self.terminate();
                if tokio::time::timeout(Duration::from_secs(2), child.wait())
                    .await
                    .is_err()
                {
                    let _ = child.kill().await;
                }
            }
        }
        self.alive.store(false, Ordering::SeqCst);
    }
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        if lock(&self.child).is_some() {
            self.terminate();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

async fn read_stdout(
    stdout: impl AsyncRead + Unpin,
    pending: Pending,
    alive: Arc<AtomicBool>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    name: String,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match read_line_capped(&mut reader, &mut line, MESSAGE_CAP).await {
            Ok(None) => break,
            Ok(Some(false)) => {
                tracing::warn!(server = %name, cap = MESSAGE_CAP, "MCP message over the cap was discarded");
                continue;
            }
            Ok(Some(true)) => {}
            Err(error) => {
                tracing::warn!(server = %name, %error, "MCP stdout read failed");
                break;
            }
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(text) else {
            // Servers that log to stdout break the protocol; skip the line.
            tracing::debug!(server = %name, line = %text, "non-JSON line on MCP stdout");
            continue;
        };
        match classify(message) {
            Incoming::Response { id, outcome } => {
                if let Some(tx) = lock(&pending).remove(&id) {
                    let _ = tx.send(outcome);
                }
            }
            Incoming::Request { id, method } => {
                let answer = answer_server_request(id, &method);
                if let Ok(mut bytes) = serde_json::to_vec(&answer) {
                    bytes.push(b'\n');
                    let mut stdin = stdin.lock().await;
                    let _ = stdin.write_all(&bytes).await;
                    let _ = stdin.flush().await;
                }
            }
            Incoming::Other => {}
        }
    }
    alive.store(false, Ordering::SeqCst);
    // Dropping the senders wakes every waiter with "exited before answering".
    lock(&pending).clear();
}

/// Read one `\n`-terminated line into `buf`. Returns `None` at EOF,
/// `Some(false)` when the line exceeded `cap` and was discarded.
async fn read_line_capped(
    reader: &mut (impl AsyncBufReadExt + Unpin),
    buf: &mut Vec<u8>,
    cap: usize,
) -> std::io::Result<Option<bool>> {
    let mut overflow = false;
    let mut read_any = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(if read_any { Some(!overflow) } else { None });
        }
        read_any = true;
        let (chunk, done) = match available.iter().position(|b| *b == b'\n') {
            Some(pos) => (&available[..=pos], true),
            None => (available, false),
        };
        let used = chunk.len();
        if !overflow {
            if buf.len() + used > cap {
                overflow = true;
                buf.clear();
            } else {
                buf.extend_from_slice(chunk);
            }
        }
        reader.consume(used);
        if done {
            return Ok(Some(!overflow));
        }
    }
}

async fn read_stderr(stderr: impl AsyncRead + Unpin, tail: Arc<Mutex<VecDeque<u8>>>, name: String) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(server = %name, "mcp stderr: {line}");
        let mut tail = lock(&tail);
        tail.extend(line.as_bytes());
        tail.push_back(b'\n');
        while tail.len() > STDERR_TAIL {
            tail.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capped_reader_discards_oversized_lines() {
        let data = b"short\nthis line is too long\nok\nlast".to_vec();
        let mut reader = BufReader::with_capacity(4, &data[..]);
        let mut buf = Vec::new();
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).await.unwrap(),
            Some(true)
        );
        assert_eq!(buf, b"short\n");
        buf.clear();
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).await.unwrap(),
            Some(false)
        );
        buf.clear();
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).await.unwrap(),
            Some(true)
        );
        assert_eq!(buf, b"ok\n");
        buf.clear();
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).await.unwrap(),
            Some(true)
        );
        assert_eq!(buf, b"last");
        buf.clear();
        assert_eq!(
            read_line_capped(&mut reader, &mut buf, 10).await.unwrap(),
            None
        );
    }
}

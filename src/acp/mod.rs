//! Agent Client Protocol v1 over stdio.
//!
//! Editors (Zed, Neovim, Emacs) launch `lh acp` and speak newline-delimited
//! JSON-RPC 2.0. This server exposes `initialize`, `session/new`,
//! `session/load`, `session/prompt`, and `session/cancel`, streams
//! `session/update`, and asks the client for permission before every
//! non-read-only tool call.
//!
//! Security stance:
//! - A session's filesystem root is the `cwd` the client declares. File tools
//!   run with the path jail on, so `..` and absolute paths outside the root
//!   are denied even when the client asks for them.
//! - Client approval is necessary but never sufficient: harness gates run
//!   first, and a permission response cannot widen the root, scope, sandbox,
//!   tool list, or a refused read-before-edit.
//! - Terminal access is not advertised. `terminal/*` requests get a typed
//!   `unsupported_capability` error.
//! - Every string sent to the client is stripped of ANSI/OSC controls.

mod session;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::config::Config;
use crate::paths::Paths;

use session::AcpSession;

pub const PROTOCOL_VERSION: u64 = 1;

/// Process-level options carried from the CLI.
#[derive(Debug, Clone, Default)]
pub struct AcpOptions {
    pub engagement: Option<String>,
    pub no_session: bool,
    pub no_redact: bool,
}

pub(crate) struct Server {
    config: Config,
    /// Canonical process working directory. A session rooted elsewhere runs
    /// without project trust or that project's verify steps.
    process_root: std::path::PathBuf,
    options: AcpOptions,
    out: UnboundedSender<String>,
    pub(crate) sessions: Mutex<HashMap<String, Arc<AcpSession>>>,
    /// Replies to reverse requests we sent (permission prompts).
    pub(crate) pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    next_request: AtomicU64,
}

impl Server {
    pub(crate) fn send(&self, message: Value) {
        let line = message.to_string();
        let _ = self.out.send(line);
    }

    pub(crate) fn response(&self, id: Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    pub(crate) fn error(&self, id: Value, error: RpcError) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": error.code,
                "message": error.message,
                "data": error.data,
            }
        }));
    }

    /// A notification, which never carries an id.
    pub(crate) fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    pub(crate) fn send_request(&self, id: String, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
    }

    pub(crate) fn next_request_id(&self) -> String {
        format!("lh-{}", self.next_request.fetch_add(1, Ordering::SeqCst))
    }

    pub(crate) fn session(&self, id: &str) -> Option<Arc<AcpSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(id)
            .cloned()
    }

    /// Undo a pending permission request when a turn stops waiting for it.
    pub(crate) fn drop_pending(&self, id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(id);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Value,
}

impl RpcError {
    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: json!({"code": "invalid_params"}),
        }
    }

    pub fn unknown_session(id: &str) -> Self {
        Self {
            code: -32602,
            message: format!("unknown session {id:?}; create one with session/new"),
            data: json!({"code": "unknown_session", "sessionId": id}),
        }
    }

    pub fn unsupported(capability: &str, detail: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: detail.into(),
            data: json!({"code": "unsupported_capability", "capability": capability}),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: message.into(),
            data: json!({"code": "internal_error"}),
        }
    }
}

/// Serve ACP on stdin/stdout until the client closes the stream.
pub async fn run(config: Config, paths: Paths, options: AcpOptions) -> Result<i32> {
    let (out, out_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(writer(out_rx));
    let server = Arc::new(Server {
        config,
        process_root: paths.cwd.clone(),
        options,
        out,
        sessions: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        next_request: AtomicU64::new(1),
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "ACP stdin failed");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                server.error(
                    Value::Null,
                    RpcError {
                        code: -32700,
                        message: format!("invalid JSON: {error}"),
                        data: json!({"code": "parse_error"}),
                    },
                );
                continue;
            }
        };
        server.handle(message).await;
    }

    server.shutdown().await;
    drop(server);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    Ok(0)
}

async fn writer(mut rx: UnboundedReceiver<String>) {
    let mut stdout = tokio::io::stdout();
    while let Some(line) = rx.recv().await {
        if stdout.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if stdout.write_all(b"\n").await.is_err() {
            break;
        }
        if stdout.flush().await.is_err() {
            break;
        }
    }
}

impl Server {
    async fn handle(self: &Arc<Self>, message: Value) {
        let Some(object) = message.as_object() else {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": Value::Null,
                "error": {"code": -32600, "message": "a JSON-RPC message must be an object", "data": {"code": "invalid_request"}}
            }));
            return;
        };
        let id = object.get("id").cloned();
        let method = object.get("method").and_then(|method| method.as_str());

        if let Some(method) = method {
            let params = object.get("params").cloned().unwrap_or(Value::Null);
            match (method, id) {
                ("initialize", Some(id)) => {
                    self.response(id, initialize_result(&params, !self.options.no_session));
                }
                ("session/new", Some(id)) => match self.new_session(params).await {
                    Ok(result) => self.response(id, result),
                    Err(error) => self.error(id, error),
                },
                ("session/load", Some(id)) => match self.load_session(params).await {
                    Ok(result) => self.response(id, result),
                    Err(error) => self.error(id, error),
                },
                ("session/prompt", Some(id)) => match self.start_prompt(id.clone(), params) {
                    Ok(()) => {}
                    Err(error) => self.error(id, error),
                },
                ("session/cancel", Some(id)) => {
                    self.cancel(params);
                    self.response(id, json!({}));
                }
                ("session/cancel", None) => self.cancel(params),
                ("authenticate", Some(id)) => {
                    // LocalHarness has no auth methods; the endpoint is already
                    // configured by the user who launched the process.
                    self.response(id, json!({}));
                }
                (
                    "terminal/create"
                    | "terminal/output"
                    | "terminal/release"
                    | "terminal/wait_for_exit"
                    | "terminal/kill",
                    Some(id),
                ) => {
                    self.error(
                        id,
                        RpcError::unsupported(
                            "terminal",
                            "terminal access is disabled in this ACP server; run shell work through the harness tools",
                        ),
                    );
                }
                ("fs/read_text_file" | "fs/write_text_file", Some(id)) => {
                    self.error(
                        id,
                        RpcError::unsupported(
                            "fs",
                            "the ACP server does not call client filesystem methods",
                        ),
                    );
                }
                (method, Some(id)) => {
                    self.error(
                        id,
                        RpcError {
                            code: -32601,
                            message: format!("method {method:?} is not supported"),
                            data: json!({"code": "method_not_found", "method": method}),
                        },
                    );
                }
                (method, None) => {
                    if method == "$/cancel_request" {
                        // The client cancels one request by id; prompt turns
                        // are cancelled through session/cancel.
                        self.cancel(params);
                    }
                }
            }
            return;
        }

        // A message without a method is a response to a reverse request, or
        // an invalid message.
        if let Some(id) = id {
            let key = id_key(&id);
            let waiter = self
                .pending
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&key);
            if let Some(waiter) = waiter {
                let result = object
                    .get("result")
                    .cloned()
                    .unwrap_or_else(|| json!({"outcome": {"outcome": "cancelled"}}));
                let _ = waiter.send(result);
            }
            return;
        }

        self.send(json!({
            "jsonrpc": "2.0",
            "id": Value::Null,
            "error": {"code": -32600, "message": "expected a method or a response id", "data": {"code": "invalid_request"}}
        }));
    }

    fn cancel(&self, params: Value) {
        let Some(session_id) = params
            .get("sessionId")
            .and_then(|id| id.as_str())
            .map(str::to_string)
        else {
            return;
        };
        if let Some(session) = self.session(&session_id) {
            session.cancel();
        }
    }

    async fn shutdown(&self) {
        let sessions: Vec<Arc<AcpSession>> = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .cloned()
            .collect();
        for session in &sessions {
            session.cancel();
            session.core.mcp.shutdown().await;
        }
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }
}

fn id_key(id: &Value) -> String {
    match id {
        // Our own reverse-request ids are strings; a client response echoes
        // them unchanged.
        Value::String(text) => text.clone(),
        other => format!("n:{other}"),
    }
}

fn initialize_result(params: &Value, load_session: bool) -> Value {
    let _client_protocol = params
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .unwrap_or(PROTOCOL_VERSION);
    // This server implements v1 only. If the client asks for a newer or older
    // version, returning 1 tells it exactly what it is talking to.
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": load_session,
            "promptCapabilities": {
                "image": false,
                "audio": false,
                "embeddedContext": true,
            },
            "mcpCapabilities": {
                "http": false,
                "sse": false,
            },
            "sessionCapabilities": {},
        },
        "agentInfo": {
            "name": "localharness",
            "title": "LocalHarness",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "authMethods": [],
    })
}

/// Canonicalize a client-declared root. It must be an absolute, existing
/// directory; symlinks resolve before the jail is applied.
pub(crate) fn declared_root(cwd: &str) -> Result<std::path::PathBuf, RpcError> {
    let path = std::path::Path::new(cwd);
    if !path.is_absolute() {
        return Err(RpcError::invalid_params(format!(
            "cwd {cwd:?} must be an absolute path"
        )));
    }
    let canonical = path.canonicalize().map_err(|error| {
        RpcError::invalid_params(format!("cwd {cwd:?} cannot be resolved: {error}"))
    })?;
    if !canonical.is_dir() {
        return Err(RpcError::invalid_params(format!(
            "cwd {cwd:?} is not a directory"
        )));
    }
    Ok(canonical)
}

/// Reject roots this iteration cannot honor instead of pretending to allow
/// them. One session per root is the supported shape.
pub(crate) fn check_additional_directories(params: &Value) -> Result<(), RpcError> {
    if params
        .get("additionalDirectories")
        .and_then(Value::as_array)
        .is_some_and(|dirs| !dirs.is_empty())
    {
        return Err(RpcError::unsupported(
            "additionalDirectories",
            "additional workspace roots are not supported in this version; start one ACP session per root",
        ));
    }
    Ok(())
}

/// A best-effort check that a session id cannot be used as a path.
pub(crate) fn valid_session_id(id: &str) -> bool {
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
    fn initialize_negotiates_down_but_never_up() {
        let result = initialize_result(&json!({"protocolVersion": 1}), true);
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            initialize_result(&json!({"protocolVersion": 1}), false)["agentCapabilities"]["loadSession"],
            false
        );
        assert_eq!(result["agentInfo"]["name"], "localharness");

        let future = initialize_result(&json!({"protocolVersion": 7}), true);
        assert_eq!(future["protocolVersion"], 1);
        let old = initialize_result(&json!({"protocolVersion": 0}), true);
        assert_eq!(old["protocolVersion"], 1);
    }

    #[test]
    fn ids_keep_numbers_and_strings_apart() {
        assert_eq!(id_key(&json!(1)), "n:1");
        assert_eq!(id_key(&json!("1")), "1");
        assert_eq!(id_key(&json!("lh-7")), "lh-7");
        assert!(valid_session_id("abc-123_x"));
        assert!(!valid_session_id("../etc/passwd"));
        assert!(!valid_session_id(""));
    }

    #[test]
    fn additional_roots_are_refused_not_ignored() {
        assert!(check_additional_directories(&json!({"additionalDirectories": ["/tmp"]})).is_err());
        assert!(check_additional_directories(&json!({"additionalDirectories": []})).is_ok());
        assert!(check_additional_directories(&json!({})).is_ok());
    }
}

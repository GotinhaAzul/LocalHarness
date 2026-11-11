//! `lsp` tool: outline, definition, references, diagnostics, and hover over a
//! minimal stdio JSON-RPC language-server client.
//!
//! One client is kept per `(server, root)` pair. Clients are created lazily,
//! disposed after ten minutes of inactivity, and killed when replaced.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::LspGate;
use crate::tools::write::{atomic_write, file_mode, record_own_write};
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, object_schema, optional_str,
    optional_u64, required_str,
};

const INIT_TIMEOUT: Duration = Duration::from_secs(15);
const EDIT_INIT_TIMEOUT: Duration = Duration::from_secs(8);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DIAG_WINDOW: Duration = Duration::from_millis(2500);
const EDIT_DIAG_WINDOW: Duration = Duration::from_millis(1500);
const DIAG_GRACE: Duration = Duration::from_millis(250);
const IDLE_DISPOSE: Duration = Duration::from_secs(10 * 60);
/// The diagnostic gate is a hot path: cap its waits so a warm check stays
/// well under the 300 ms p95 target. A late batch is skipped, never guessed.
const GATE_SNAPSHOT_WINDOW: Duration = Duration::from_millis(200);
const GATE_CHECK_WINDOW: Duration = Duration::from_millis(250);

const MAX_OUTLINE: usize = 200;
const MAX_DEFINITIONS: usize = 5;
const MAX_REFERENCES: usize = 20;
const MAX_DIAGNOSTICS: usize = 40;
const MAX_EDIT_DIAGNOSTICS: usize = 8;
const MAX_EDIT_DIAGNOSTIC_CHARS: usize = 1200;
const MAX_HOVER_LINES: usize = 25;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

const ACTIONS: &[&str] = &[
    "outline",
    "definition",
    "references",
    "diagnostics",
    "hover",
];

pub struct LspService {
    clients: Mutex<HashMap<String, Arc<AsyncMutex<LspClient>>>>,
    /// Roots where automatic diagnostics failed to start. Avoid paying the
    /// startup timeout on every edit; an explicit `lsp` call clears the mark.
    auto_skipped: Mutex<HashSet<String>>,
    /// How post-mutation diagnostics behave; shared by `write`, `edit`, and
    /// `patch` through one service.
    gate: Mutex<LspGate>,
    #[cfg(test)]
    binary_overrides: Mutex<HashMap<&'static str, PathBuf>>,
}

impl LspService {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            auto_skipped: Mutex::new(HashSet::new()),
            gate: Mutex::new(LspGate::Warn),
            #[cfg(test)]
            binary_overrides: Mutex::new(HashMap::new()),
        }
    }

    pub fn gate(&self) -> LspGate {
        *self
            .gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn set_gate(&self, gate: LspGate) {
        *self
            .gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = gate;
    }

    fn auto_skipped(&self, key: &str) -> bool {
        self.auto_skipped
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains(key)
    }

    fn skip_auto(&self, key: &str) {
        self.auto_skipped
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(key.to_string());
    }

    fn allow_auto(&self, key: &str) {
        self.auto_skipped
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(key);
    }

    #[cfg(test)]
    pub(crate) fn override_binary(&self, name: &'static str, path: PathBuf) {
        self.binary_overrides
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(name, path);
    }

    fn resolve_binary(&self, name: &'static str) -> Option<PathBuf> {
        #[cfg(test)]
        if let Some(path) = self
            .binary_overrides
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(name)
            .cloned()
        {
            return Some(path);
        }
        find_binary(name)
    }

    /// Drop clients that have been idle for too long. Dropping a client kills
    /// its child process.
    fn dispose_idle(&self) {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        clients.retain(|_, client| match client.try_lock() {
            Ok(guard) => guard.last_used.elapsed() < IDLE_DISPOSE,
            Err(_) => true, // In use right now.
        });
    }

    fn client_for_key(&self, key: &str) -> Option<Arc<AsyncMutex<LspClient>>> {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        let client = clients.get(key)?;
        let dead = match client.try_lock() {
            Ok(guard) => guard.dead,
            Err(_) => false,
        };
        if dead {
            clients.remove(key);
            None
        } else {
            Some(client.clone())
        }
    }

    fn remove(&self, key: &str) {
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        clients.remove(key);
    }

    fn fail(&self, key: &str, server: &str, action: &str, failure: RpcFailure) -> ToolResult {
        self.remove(key);
        rpc_failure_result(server, action, failure)
    }

    async fn connect(
        &self,
        spec: ServerSpec,
        binary: &Path,
        root: &Path,
        key: &str,
        init_timeout: Duration,
    ) -> Result<Arc<AsyncMutex<LspClient>>, ToolResult> {
        let mut command = Command::new(binary);
        command
            .args(spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| {
            ToolResult::err(
                "LSP_UNAVAILABLE",
                format!(
                    "failed to start {}: {error}; install it: {}",
                    spec.binary, spec.install
                ),
            )
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            ToolResult::err(
                "LSP_UNAVAILABLE",
                format!("{} started without a stdin pipe", spec.binary),
            )
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ToolResult::err(
                "LSP_UNAVAILABLE",
                format!("{} started without a stdout pipe", spec.binary),
            )
        })?;

        let mut client = LspClient {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 0,
            language_id: spec.language_id.to_string(),
            last_used: Instant::now(),
            diagnostics: HashMap::new(),
            diag_counts: HashMap::new(),
            doc_versions: HashMap::new(),
            dead: false,
        };

        let folder_name = root
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "workspace".to_string());
        let initialize = json!({
            "processId": Value::Null,
            "clientInfo": {"name": "localharness", "version": env!("CARGO_PKG_VERSION")},
            "rootUri": file_url(root),
            "workspaceFolders": [{"uri": file_url(root), "name": folder_name}],
            "capabilities": {
                "workspace": {"workspaceFolders": true},
                "textDocument": {
                    "synchronization": {"didSave": false, "dynamicRegistration": false},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "definition": {"linkSupport": true},
                    "references": {},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "publishDiagnostics": {"relatedInformation": false}
                }
            },
            "trace": "off"
        });

        match client.request("initialize", initialize, init_timeout).await {
            Ok(_) => {}
            Err(RpcFailure::Timeout) => {
                return Err(ToolResult::err(
                    "LSP_INDEXING",
                    format!(
                        "{} has not finished starting for {}; retry the same call in a few seconds",
                        spec.binary,
                        root.display()
                    ),
                ));
            }
            Err(RpcFailure::Closed) => {
                return Err(ToolResult::err(
                    "LSP_ERROR",
                    format!(
                        "{} exited during initialize; run it directly to see the error",
                        spec.binary
                    ),
                ));
            }
            Err(RpcFailure::Error(message)) => {
                return Err(ToolResult::err(
                    "LSP_ERROR",
                    format!("{} initialize failed: {message}", spec.binary),
                ));
            }
        }

        if let Err(failure) = client.notify("initialized", json!({})).await {
            return Err(rpc_failure_result(
                spec.binary,
                "initialized notification",
                failure,
            ));
        }

        let client = Arc::new(AsyncMutex::new(client));
        let mut clients = self.clients.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = clients.get(key)
            && let Ok(guard) = existing.try_lock()
            && !guard.dead
        {
            return Ok(existing.clone());
        }
        clients.insert(key.to_string(), client.clone());
        Ok(client)
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolResult> {
        let action = required_str(&args, "action")?;
        if !ACTIONS.contains(&action.as_str()) {
            return Err(ToolResult::err(
                "invalid_args",
                format!(
                    "unknown action {action:?}; use one of: {}",
                    ACTIONS.join(", ")
                ),
            ));
        }

        let path = ctx.resolve(&required_str(&args, "path")?)?;
        let shown = display_path(&ctx.cwd, &path);
        let meta = std::fs::metadata(&path).map_err(|error| {
            ToolResult::err(
                "invalid_args",
                format!("cannot read {shown}: {error}; pass an existing file path"),
            )
        })?;
        if !meta.is_file() {
            return Err(ToolResult::err(
                "invalid_args",
                format!("{shown} is not a file; pass a source file path"),
            ));
        }
        if meta.len() > MAX_FILE_BYTES {
            return Err(ToolResult::err(
                "file_too_large",
                format!(
                    "{shown} is {} bytes; lsp works on files up to {} bytes",
                    meta.len(),
                    MAX_FILE_BYTES
                ),
            ));
        }
        let bytes = std::fs::read(&path).map_err(|error| {
            ToolResult::err("invalid_args", format!("cannot read {shown}: {error}"))
        })?;
        if is_binary(&bytes) {
            return Err(ToolResult::err(
                "binary_file",
                format!("{shown} looks binary; lsp only works on text files"),
            ));
        }
        let text = String::from_utf8_lossy(&bytes).to_string();

        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let spec = match server_spec_for_ext(&extension) {
            Some(spec) => spec,
            None => {
                let kind = if extension.is_empty() {
                    "extensionless files".to_string()
                } else {
                    format!(".{extension} files")
                };
                return Err(ToolResult::err(
                    "LSP_UNAVAILABLE",
                    format!(
                        "no language server is configured for {kind}; supported: rs, py, ts, tsx, js, jsx, go, c, cpp"
                    ),
                ));
            }
        };

        let binary = self.resolve_binary(spec.binary).ok_or_else(|| {
            ToolResult::err(
                "LSP_UNAVAILABLE",
                format!(
                    "{} is not on PATH; install it: {}",
                    spec.binary, spec.install
                ),
            )
        })?;
        let root = project_root(&path, root_markers(&extension), &ctx.cwd);
        let key = format!("{}|{}", spec.binary, root.display());
        self.dispose_idle();

        let symbol = optional_str(&args, "symbol");
        let explicit_column = optional_u64(&args, "column");
        let mut line = optional_u64(&args, "line").map(|value| value.max(1));
        let mut column = explicit_column.map(|value| value.max(1)).unwrap_or(1);
        if action != "outline" && action != "diagnostics" && line.is_none() {
            match symbol.as_deref() {
                Some(symbol) => match locate_symbol(&text, symbol) {
                    Some((found_line, found_column)) => {
                        line = Some(found_line);
                        if explicit_column.is_none() {
                            column = found_column;
                        }
                    }
                    None => {
                        return Err(ToolResult::err(
                            "invalid_args",
                            format!(
                                "symbol {symbol:?} was not found in {shown}; pass \"line\" and \"column\" instead"
                            ),
                        ));
                    }
                },
                None => {
                    return Err(ToolResult::err(
                        "invalid_args",
                        format!(
                            "{action} needs \"line\" (1-based) or \"symbol\"; for example {{\"action\":\"{action}\",\"path\":\"{shown}\",\"line\":10}}"
                        ),
                    ));
                }
            }
        }
        let line = line.unwrap_or(1);

        let client = match self.client_for_key(&key) {
            Some(client) => client,
            None => {
                self.connect(spec, &binary, &root, &key, INIT_TIMEOUT)
                    .await?
            }
        };
        self.allow_auto(&key);

        let mut guard = client.lock().await;
        guard.last_used = Instant::now();

        let uri = file_url(&path);
        let seen = guard.diag_seq(&uri);
        let (synced_uri, synced) = guard
            .sync_document(&path, &text)
            .await
            .map_err(|failure| self.fail(&key, spec.binary, "document sync", failure))?;
        let position = json!({
            "line": line.saturating_sub(1),
            "character": column.saturating_sub(1),
        });
        let text_document = json!({"uri": synced_uri.as_str()});

        let content = match action.as_str() {
            "outline" => {
                let result = guard
                    .request(
                        "textDocument/documentSymbol",
                        json!({"textDocument": text_document}),
                        REQUEST_TIMEOUT,
                    )
                    .await
                    .map_err(|failure| self.fail(&key, spec.binary, "documentSymbol", failure))?;
                let symbols = result.as_array().cloned().unwrap_or_default();
                if symbols.is_empty() {
                    format!("{shown}: no symbols reported (the server may still be indexing)")
                } else {
                    let lines = render_outline(&symbols);
                    format!("{shown}: outline\n{}", lines.join("\n"))
                }
            }
            "definition" => {
                let result = guard
                    .request(
                        "textDocument/definition",
                        json!({"textDocument": text_document, "position": position}),
                        REQUEST_TIMEOUT,
                    )
                    .await
                    .map_err(|failure| self.fail(&key, spec.binary, "definition", failure))?;
                let locations = locations_from(&result);
                if locations.is_empty() {
                    format!("{shown}:{line}:{column}: no definition found")
                } else {
                    let mut out = format!("{shown}:{line}:{column}: definition");
                    for (uri, found_line, found_column) in locations.iter().take(MAX_DEFINITIONS) {
                        out.push_str("\n  ");
                        out.push_str(&render_location(&ctx.cwd, uri, *found_line, *found_column));
                    }
                    if locations.len() > MAX_DEFINITIONS {
                        out.push_str(&format!(
                            "\n  … {} more locations",
                            locations.len() - MAX_DEFINITIONS
                        ));
                    }
                    out
                }
            }
            "references" => {
                let result = guard
                    .request(
                        "textDocument/references",
                        json!({
                            "textDocument": text_document,
                            "position": position,
                            "context": {"includeDeclaration": true}
                        }),
                        REQUEST_TIMEOUT,
                    )
                    .await
                    .map_err(|failure| self.fail(&key, spec.binary, "references", failure))?;
                let locations = locations_from(&result);
                if locations.is_empty() {
                    format!("{shown}:{line}:{column}: no references found")
                } else {
                    let mut out = format!(
                        "{shown}:{line}:{column}: {} reference{}",
                        locations.len(),
                        if locations.len() == 1 { "" } else { "s" }
                    );
                    for (uri, found_line, found_column) in locations.iter().take(MAX_REFERENCES) {
                        out.push_str("\n  ");
                        out.push_str(&render_location(&ctx.cwd, uri, *found_line, *found_column));
                    }
                    if locations.len() > MAX_REFERENCES {
                        out.push_str(&format!(
                            "\n  … {} more references",
                            locations.len() - MAX_REFERENCES
                        ));
                    }
                    out
                }
            }
            "hover" => {
                let result = guard
                    .request(
                        "textDocument/hover",
                        json!({"textDocument": text_document, "position": position}),
                        REQUEST_TIMEOUT,
                    )
                    .await
                    .map_err(|failure| self.fail(&key, spec.binary, "hover", failure))?;
                format!("{shown}:{line}:{column}: hover\n{}", render_hover(&result))
            }
            _ => {
                if synced {
                    guard
                        .wait_diagnostics(&uri, seen, DIAG_WINDOW)
                        .await
                        .map_err(|failure| self.fail(&key, spec.binary, "diagnostics", failure))?;
                }
                let diagnostics = guard.diagnostics_for(&uri);
                if diagnostics.is_empty() {
                    let note = if guard.diag_seq(&uri) == 0 {
                        " (no publishDiagnostics within 2.5 s; the server may still be indexing, retry)"
                    } else {
                        ""
                    };
                    format!("{shown}: no diagnostics{note}")
                } else {
                    format!(
                        "{shown}: {} diagnost{} reported\n{}",
                        diagnostics.len(),
                        if diagnostics.len() == 1 { "ic" } else { "ics" },
                        render_diagnostics(&diagnostics)
                    )
                }
            }
        };

        Ok(ToolResult::ok(content).with_details(json!({
            "action": action,
            "server": spec.binary,
            "path": shown,
        })))
    }

    /// Best-effort diagnostics for a file just changed by a mutation tool.
    ///
    /// Returns `None` when no server is installed, the server is still starting,
    /// or no fresh batch arrives. A clean batch returns a short "none" line.
    /// This method never reports an error: a missing language server must not
    /// turn a successful edit into a failed tool call.
    pub async fn diagnostics_after_edit(&self, ctx: &ToolContext, path: &Path) -> Option<String> {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())?
            .to_ascii_lowercase();
        let spec = server_spec_for_ext(&extension)?;
        let binary = self.resolve_binary(spec.binary)?;
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        if bytes.iter().take(4096).any(|byte| *byte == 0) {
            return None;
        }
        let text = String::from_utf8_lossy(&bytes).to_string();
        let root = project_root_marker(path, root_markers(&extension), &ctx.cwd)?;
        let key = format!("{}|{}", spec.binary, root.display());
        if self.auto_skipped(&key) {
            return None;
        }
        self.dispose_idle();
        let client = match self.client_for_key(&key) {
            Some(client) => client,
            None => match self
                .connect(spec, &binary, &root, &key, EDIT_INIT_TIMEOUT)
                .await
            {
                Ok(client) => client,
                Err(_) => {
                    self.skip_auto(&key);
                    return None;
                }
            },
        };

        let mut guard = client.lock().await;
        guard.last_used = Instant::now();
        let uri = file_url(path);
        let seen = guard.diag_seq(&uri);
        let (_, synced) = guard.sync_document(path, &text).await.ok()?;
        if synced {
            match guard.pump_diagnostics(&uri, seen, EDIT_DIAG_WINDOW).await {
                Ok(true) => {}
                _ => return None,
            }
        } else if guard.diag_seq(&uri) == 0 {
            return None;
        }
        Some(render_edit_diagnostics(
            spec.binary,
            &guard.diagnostics_for(&uri),
        ))
    }

    /// Capture the diagnostics and bytes a mutation is about to replace.
    ///
    /// The gate never starts a language server: `None` when the file is not in
    /// a real project, the server is cold, or the gate is off. Blocking on a
    /// cold server would add seconds to the first edit.
    pub async fn snapshot_before_edit(&self, ctx: &ToolContext, path: &Path) -> EditSnapshot {
        let gate = self.gate();
        if gate == LspGate::Off {
            return EditSnapshot::default();
        }
        let Some(extension) = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
        else {
            return EditSnapshot::default();
        };
        let Some(spec) = server_spec_for_ext(&extension) else {
            return EditSnapshot::default();
        };
        if self.resolve_binary(spec.binary).is_none() {
            return EditSnapshot::default();
        }
        let Some(root) = project_root_marker(path, root_markers(&extension), &ctx.cwd) else {
            return EditSnapshot::default();
        };
        let key = format!("{}|{}", spec.binary, root.display());
        if self.auto_skipped(&key) {
            return EditSnapshot::default();
        }
        self.dispose_idle();
        let Some(client) = self.client_for_key(&key) else {
            return EditSnapshot::default();
        };

        let before = match std::fs::read(path) {
            Ok(bytes) if bytes.len() as u64 <= MAX_FILE_BYTES => Some(bytes),
            Ok(_) => return EditSnapshot::default(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return EditSnapshot::default(),
        };
        let text = before
            .as_ref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .map(str::to_string);

        let mut guard = client.lock().await;
        guard.last_used = Instant::now();
        let uri = file_url(path);
        let seen = guard.diag_seq(&uri);
        let synced = match &text {
            Some(text) => match guard.sync_document(path, text).await {
                Ok((_, synced)) => synced,
                Err(_) => return EditSnapshot::default(),
            },
            None => false,
        };
        if synced {
            let _ = guard
                .pump_diagnostics(&uri, seen, GATE_SNAPSHOT_WINDOW)
                .await;
        } else if guard.diag_seq(&uri) == 0 {
            // A warm client that has never published for this file has no
            // baseline to diff against; do not guess that every error is new.
            return EditSnapshot::default();
        }

        let errors = error_keys(&guard.diagnostics_for(&uri));
        EditSnapshot {
            baseline: Some(DiagnosticBaseline {
                key,
                server: spec.binary,
                errors,
            }),
            before,
        }
    }

    /// Compare a mutation's diagnostics against its snapshot.
    ///
    /// Only new severity-error diagnostics count. Under `block`, the file is
    /// restored and the caller returns `EDIT_REGRESSION`; under `warn`, the
    /// new errors are appended to the result. Returns `None` for a skipped,
    /// clean, or late check.
    pub async fn check_edit(
        &self,
        ctx: &ToolContext,
        path: &Path,
        snapshot: &EditSnapshot,
    ) -> Option<EditVerdict> {
        let gate = self.gate();
        let baseline = snapshot.baseline.as_ref()?;
        if gate == LspGate::Off {
            return None;
        }
        self.dispose_idle();
        let client = self.client_for_key(&baseline.key)?;
        let bytes = std::fs::read(path).ok()?;
        if bytes.len() as u64 > MAX_FILE_BYTES || bytes.iter().take(4096).any(|byte| *byte == 0) {
            return None;
        }
        let text = String::from_utf8_lossy(&bytes).to_string();

        let mut guard = client.lock().await;
        guard.last_used = Instant::now();
        let uri = file_url(path);
        let seen = guard.diag_seq(&uri);
        guard.sync_document(path, &text).await.ok()?;
        let _ = guard.pump_diagnostics(&uri, seen, GATE_CHECK_WINDOW).await;
        let new_errors: Vec<Value> = guard
            .diagnostics_for(&uri)
            .into_iter()
            .filter(|diagnostic| {
                is_error(diagnostic) && !baseline.errors.contains(&error_key(diagnostic))
            })
            .collect();
        if new_errors.is_empty() {
            return None;
        }
        let rendered = render_edit_diagnostics(baseline.server, &new_errors);
        if gate == LspGate::Warn {
            return Some(EditVerdict {
                text: rendered,
                regression: None,
            });
        }

        let restored = restore_file(path, snapshot.before.as_deref());
        if restored {
            record_own_write(ctx, path);
        }
        let display = display_path(&ctx.cwd, path);
        let count = new_errors.len();
        let more = if restored {
            format!(
                "The change was rolled back; {display} is back to the pre-edit content. Fix hint: re-read {display} and retry with a smaller edit that keeps the file type-correct."
            )
        } else {
            format!(
                "The change could not be rolled back automatically. Fix hint: inspect {display} and restore the intended content before continuing."
            )
        };
        Some(EditVerdict {
            text: format!(
                "the edit introduced {count} new language-server error{}:\n{rendered}\n{more}",
                if count == 1 { "" } else { "s" }
            ),
            regression: Some(EditRegression {
                rolled_back: restored,
                diagnostics: new_errors,
                server: baseline.server,
            }),
        })
    }
}

/// Pre-mutation bytes and diagnostics. Default means the gate was skipped.
#[derive(Debug, Clone, Default)]
pub struct EditSnapshot {
    baseline: Option<DiagnosticBaseline>,
    /// `None` means the file did not exist before the mutation.
    before: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct DiagnosticBaseline {
    key: String,
    server: &'static str,
    errors: HashSet<String>,
}

/// Outcome of a post-mutation diagnostic check.
#[derive(Debug, Clone)]
pub struct EditVerdict {
    pub text: String,
    pub regression: Option<EditRegression>,
}

/// A new error that must fail the edit under `block`.
#[derive(Debug, Clone)]
pub struct EditRegression {
    pub rolled_back: bool,
    pub diagnostics: Vec<Value>,
    pub server: &'static str,
}

impl EditRegression {
    /// Tool-result details for `EDIT_REGRESSION`.
    pub fn details(&self) -> Value {
        json!({
            "code": "EDIT_REGRESSION",
            "server": self.server,
            "rolled_back": self.rolled_back,
            "new_errors": self.diagnostics,
        })
    }
}

fn is_error(diagnostic: &Value) -> bool {
    diagnostic.get("severity").and_then(Value::as_u64) == Some(1)
}

/// Error identity for a baseline diff. Lines move on unrelated edits, so the
/// code and message identify an error rather than its coordinates.
fn error_key(diagnostic: &Value) -> String {
    let code = diagnostic.get("code").and_then(|code| {
        code.as_str()
            .map(str::to_string)
            .or_else(|| code.as_u64().map(|number| number.to_string()))
    });
    let message = diagnostic
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!("{}|{message}", code.unwrap_or_default())
}

fn error_keys(diagnostics: &[Value]) -> HashSet<String> {
    diagnostics
        .iter()
        .filter(|diagnostic| is_error(diagnostic))
        .map(error_key)
        .collect()
}

fn restore_file(path: &Path, before: Option<&[u8]>) -> bool {
    match before {
        Some(bytes) => {
            let mode = file_mode(path);
            atomic_write(path, bytes, mode).is_ok()
        }
        None => match std::fs::remove_file(path) {
            Ok(()) => true,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        },
    }
}

impl Default for LspService {
    fn default() -> Self {
        Self::new()
    }
}

/// `lsp` tool adapter over the shared service.
pub struct LspTool {
    service: Arc<LspService>,
}

impl LspTool {
    pub fn new() -> Self {
        Self::with_service(Arc::new(LspService::new()))
    }

    pub fn with_service(service: Arc<LspService>) -> Self {
        Self { service }
    }
}

impl Default for LspTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &'static str {
        "lsp"
    }

    fn description(&self) -> &'static str {
        "Language-server outline, definitions, references, diagnostics, hover."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "action": {"type": "string", "enum": ACTIONS},
                "path": {"type": "string", "description": "Source file to query."},
                "line": {"type": "integer", "minimum": 1, "description": "1-based line."},
                "column": {"type": "integer", "minimum": 1, "description": "1-based column."},
                "symbol": {"type": "string", "description": "Find the position by symbol name instead of line."}
            }),
            &["action", "path"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Coding
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        match self.service.run(ctx, args).await {
            Ok(result) => result,
            Err(error) => error,
        }
    }
}

// ---------------------------------------------------------------------------
// Server discovery
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct ServerSpec {
    binary: &'static str,
    args: &'static [&'static str],
    language_id: &'static str,
    install: &'static str,
}

fn server_spec_for_ext(extension: &str) -> Option<ServerSpec> {
    let spec = match extension {
        "rs" => ServerSpec {
            binary: "rust-analyzer",
            args: &[],
            language_id: "rust",
            install: "rustup component add rust-analyzer",
        },
        "py" | "pyi" => ServerSpec {
            binary: "pyright-langserver",
            args: &["--stdio"],
            language_id: "python",
            install: "npm install -g pyright (or pip install pyright)",
        },
        "ts" => ServerSpec {
            binary: "typescript-language-server",
            args: &["--stdio"],
            language_id: "typescript",
            install: "npm install -g typescript-language-server typescript",
        },
        "tsx" => ServerSpec {
            binary: "typescript-language-server",
            args: &["--stdio"],
            language_id: "typescriptreact",
            install: "npm install -g typescript-language-server typescript",
        },
        "js" | "mjs" | "cjs" => ServerSpec {
            binary: "typescript-language-server",
            args: &["--stdio"],
            language_id: "javascript",
            install: "npm install -g typescript-language-server typescript",
        },
        "jsx" => ServerSpec {
            binary: "typescript-language-server",
            args: &["--stdio"],
            language_id: "javascriptreact",
            install: "npm install -g typescript-language-server typescript",
        },
        "go" => ServerSpec {
            binary: "gopls",
            args: &[],
            language_id: "go",
            install: "go install golang.org/x/tools/gopls@latest",
        },
        "c" | "h" => ServerSpec {
            binary: "clangd",
            args: &[],
            language_id: "c",
            install: "install clangd (clang-tools package)",
        },
        "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => ServerSpec {
            binary: "clangd",
            args: &[],
            language_id: "cpp",
            install: "install clangd (clang-tools package)",
        },
        _ => return None,
    };
    Some(spec)
}

fn root_markers(extension: &str) -> &'static [&'static str] {
    match extension {
        "rs" => &["Cargo.toml"],
        "py" | "pyi" => &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
            "Pipfile",
            "tox.ini",
        ],
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" => {
            &["package.json", "tsconfig.json", "jsconfig.json"]
        }
        "go" => &["go.mod"],
        _ => &[
            "compile_commands.json",
            "CMakeLists.txt",
            ".clangd",
            "compile_flags.txt",
        ],
    }
}

fn project_root(file: &Path, markers: &[&str], fallback: &Path) -> PathBuf {
    project_root_marker(file, markers, fallback).unwrap_or_else(|| fallback.to_path_buf())
}

/// Like [`project_root`], but returns `None` when no marker is found. Automatic
/// post-edit diagnostics use this to avoid starting a server for a loose file
/// in a directory that is not a project.
fn project_root_marker(file: &Path, markers: &[&str], fallback: &Path) -> Option<PathBuf> {
    let bound = if file.starts_with(fallback) {
        fallback.to_path_buf()
    } else {
        PathBuf::from("/")
    };
    let mut dir = file.parent()?.to_path_buf();
    loop {
        if markers.iter().any(|marker| dir.join(marker).exists()) {
            return Some(dir);
        }
        if dir == bound || dir.parent().is_none() {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

fn find_binary(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable(&path).then_some(path);
    }
    let search = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&search) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|byte| *byte == 0)
}

// ---------------------------------------------------------------------------
// URIs and positions
// ---------------------------------------------------------------------------

fn file_url(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::from("file://");
    if !text.starts_with('/') {
        out.push('/');
    }
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    Some(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn locate_symbol(text: &str, symbol: &str) -> Option<(u64, u64)> {
    for (index, line) in text.lines().enumerate() {
        if let Some(column) = line.find(symbol) {
            return Some((index as u64 + 1, column as u64 + 1));
        }
    }
    None
}

fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn symbol_kind(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => "symbol",
    }
}

fn symbol_line(symbol: &Value) -> Option<u64> {
    let range = symbol
        .get("selectionRange")
        .or_else(|| symbol.get("range"))
        .or_else(|| symbol.pointer("/location/range"))?;
    range
        .pointer("/start/line")
        .and_then(Value::as_u64)
        .map(|line| line + 1)
}

fn render_outline(symbols: &[Value]) -> Vec<String> {
    let mut lines = Vec::new();
    render_symbols(symbols, 0, &mut lines);
    lines
}

fn render_symbols(symbols: &[Value], depth: usize, lines: &mut Vec<String>) {
    for symbol in symbols {
        if lines.len() >= MAX_OUTLINE {
            return;
        }
        let name = symbol
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("<unnamed>");
        let kind = symbol_kind(symbol.get("kind").and_then(Value::as_u64).unwrap_or(0));
        let line = symbol_line(symbol)
            .map(|line| format!("line {line}"))
            .unwrap_or_else(|| "line ?".to_string());
        lines.push(format!("{}{} ({kind}, {line})", "  ".repeat(depth), name));
        if let Some(children) = symbol.get("children").and_then(Value::as_array) {
            render_symbols(children, depth + 1, lines);
        }
    }
}

fn locations_from(result: &Value) -> Vec<(String, u64, u64)> {
    let mut locations = Vec::new();
    match result {
        Value::Array(items) => {
            for item in items {
                push_location(item, &mut locations);
            }
        }
        Value::Object(_) => push_location(result, &mut locations),
        _ => {}
    }
    locations
}

fn push_location(item: &Value, locations: &mut Vec<(String, u64, u64)>) {
    let uri = item
        .get("uri")
        .or_else(|| item.get("targetUri"))
        .and_then(Value::as_str);
    let range = item
        .get("range")
        .or_else(|| item.get("targetSelectionRange"))
        .or_else(|| item.get("targetRange"));
    if let (Some(uri), Some(range)) = (uri, range) {
        let line = range
            .pointer("/start/line")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        let column = range
            .pointer("/start/character")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            + 1;
        locations.push((uri.to_string(), line, column));
    }
}

fn render_location(cwd: &Path, uri: &str, line: u64, column: u64) -> String {
    match uri_to_path(uri) {
        Some(path) => format!("{}:{}:{}", display_path(cwd, &path), line, column),
        None => format!("{uri}:{line}:{column}"),
    }
}

fn render_hover(result: &Value) -> String {
    let contents = result.get("contents").unwrap_or(result);
    let mut parts = Vec::new();
    collect_markup(contents, &mut parts);
    let text = parts
        .into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() {
        return "(no hover information)".to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut out = lines
        .iter()
        .take(MAX_HOVER_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    if lines.len() > MAX_HOVER_LINES {
        out.push_str(&format!("\n… {} more lines", lines.len() - MAX_HOVER_LINES));
    }
    out
}

fn collect_markup(value: &Value, parts: &mut Vec<String>) {
    match value {
        Value::String(text) => parts.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                collect_markup(item, parts);
            }
        }
        Value::Object(map) => {
            if let Some(text) = map.get("value").and_then(Value::as_str) {
                parts.push(text.to_string());
            }
        }
        _ => {}
    }
}

fn render_diagnostics(diagnostics: &[Value]) -> String {
    let mut out = String::new();
    for diagnostic in diagnostics.iter().take(MAX_DIAGNOSTICS) {
        let line = diagnostic
            .pointer("/range/start/line")
            .and_then(Value::as_u64)
            .map(|line| line + 1)
            .unwrap_or(0);
        let column = diagnostic
            .pointer("/range/start/character")
            .and_then(Value::as_u64)
            .map(|column| column + 1)
            .unwrap_or(0);
        let severity = match diagnostic.get("severity").and_then(Value::as_u64) {
            Some(1) => "error",
            Some(2) => "warning",
            Some(3) => "info",
            Some(4) => "hint",
            _ => "diagnostic",
        };
        let code = diagnostic.get("code").and_then(|code| {
            code.as_str()
                .map(str::to_string)
                .or_else(|| code.as_u64().map(|number| number.to_string()))
        });
        let code = code.map(|code| format!(" [{code}]")).unwrap_or_default();
        let message = diagnostic
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        let message = truncate_chars(message, 200);
        out.push_str(&format!("{line}:{column} {severity}{code} {message}\n"));
    }
    if diagnostics.len() > MAX_DIAGNOSTICS {
        out.push_str(&format!(
            "… {} more diagnostics\n",
            diagnostics.len() - MAX_DIAGNOSTICS
        ));
    }
    out
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// Errors and warnings only, newest batch, compact enough to append to an edit.
fn render_edit_diagnostics(server: &str, diagnostics: &[Value]) -> String {
    let mut relevant: Vec<&Value> = diagnostics
        .iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.get("severity").and_then(Value::as_u64),
                Some(1 | 2)
            )
        })
        .collect();
    relevant.sort_by_key(|diagnostic| {
        diagnostic
            .get("severity")
            .and_then(Value::as_u64)
            .unwrap_or(9)
    });
    if relevant.is_empty() {
        return format!("diagnostics ({server}): none");
    }
    let mut out = format!("diagnostics ({server}):\n");
    for diagnostic in relevant.iter().take(MAX_EDIT_DIAGNOSTICS) {
        let line = diagnostic
            .pointer("/range/start/line")
            .and_then(Value::as_u64)
            .map(|line| line + 1)
            .unwrap_or(0);
        let column = diagnostic
            .pointer("/range/start/character")
            .and_then(Value::as_u64)
            .map(|column| column + 1)
            .unwrap_or(0);
        let severity = if diagnostic.get("severity").and_then(Value::as_u64) == Some(2) {
            "warning"
        } else {
            "error"
        };
        let code = diagnostic.get("code").and_then(|code| {
            code.as_str()
                .map(str::to_string)
                .or_else(|| code.as_u64().map(|number| number.to_string()))
        });
        let code = code.map(|code| format!(" [{code}]")).unwrap_or_default();
        let message = diagnostic
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .lines()
            .next()
            .unwrap_or("");
        out.push_str(&format!(
            "  {line}:{column} {severity}{code} {}\n",
            truncate_chars(message, 160)
        ));
    }
    if relevant.len() > MAX_EDIT_DIAGNOSTICS {
        out.push_str(&format!(
            "  … {} more diagnostics\n",
            relevant.len() - MAX_EDIT_DIAGNOSTICS
        ));
    }
    truncate_chars(&out, MAX_EDIT_DIAGNOSTIC_CHARS)
}

// ---------------------------------------------------------------------------
// JSON-RPC client
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum RpcFailure {
    Timeout,
    Closed,
    Error(String),
}

fn rpc_failure_result(server: &str, action: &str, failure: RpcFailure) -> ToolResult {
    match failure {
        RpcFailure::Timeout => ToolResult::err(
            "LSP_TIMEOUT",
            format!(
                "{server} did not answer {action} in time; retry, and if it persists run {server} directly to check the project"
            ),
        ),
        RpcFailure::Closed => ToolResult::err(
            "LSP_ERROR",
            format!("{server} closed the connection during {action}; retry the call"),
        ),
        RpcFailure::Error(message) => {
            ToolResult::err("LSP_ERROR", format!("{server} {action}: {message}"))
        }
    }
}

struct LspClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    language_id: String,
    last_used: Instant,
    diagnostics: HashMap<String, Vec<Value>>,
    diag_counts: HashMap<String, u64>,
    doc_versions: HashMap<String, (i64, u64)>,
    dead: bool,
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl LspClient {
    async fn write_message(&mut self, message: &Value) -> Result<(), RpcFailure> {
        let body = serde_json::to_vec(message)
            .map_err(|error| RpcFailure::Error(format!("cannot encode request: {error}")))?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        if let Err(error) = self.stdin.write_all(header.as_bytes()).await {
            self.dead = true;
            return Err(RpcFailure::Error(format!("write failed: {error}")));
        }
        if let Err(error) = self.stdin.write_all(&body).await {
            self.dead = true;
            return Err(RpcFailure::Error(format!("write failed: {error}")));
        }
        if let Err(error) = self.stdin.flush().await {
            self.dead = true;
            return Err(RpcFailure::Error(format!("flush failed: {error}")));
        }
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), RpcFailure> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcFailure> {
        self.next_id += 1;
        let id = self.next_id;
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        let deadline = Instant::now() + timeout;
        loop {
            let response = self.pump(deadline).await?;
            let response_id = response.get("id").and_then(Value::as_i64);
            if response_id != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                let code = error.get("code").and_then(Value::as_i64).unwrap_or(-1);
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(RpcFailure::Error(format!("{message} (code {code})")));
            }
            return Ok(response.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Read and dispatch messages until a JSON-RPC response arrives. Server
    /// notifications are stored (diagnostics) or ignored; server requests get
    /// minimal replies.
    async fn pump(&mut self, deadline: Instant) -> Result<Value, RpcFailure> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RpcFailure::Timeout);
            }
            let message =
                match tokio::time::timeout(remaining, read_message(&mut self.stdout)).await {
                    Ok(Ok(Some(message))) => message,
                    Ok(Ok(None)) => {
                        self.dead = true;
                        return Err(RpcFailure::Closed);
                    }
                    Ok(Err(error)) => {
                        self.dead = true;
                        return Err(RpcFailure::Error(format!("read failed: {error}")));
                    }
                    Err(_) => return Err(RpcFailure::Timeout),
                };

            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if method == "textDocument/publishDiagnostics" {
                    if let Some(params) = message.get("params") {
                        self.store_diagnostics(params);
                    }
                } else if message.get("id").is_some() {
                    let id = message.get("id").cloned().unwrap_or(Value::Null);
                    let reply = server_request_reply(method, &message, id);
                    // A failed reply is not fatal; the next read will surface it.
                    let _ = self.write_message(&reply).await;
                }
                continue;
            }

            if message.get("id").is_some()
                && (message.get("result").is_some() || message.get("error").is_some())
            {
                return Ok(message);
            }
            // Anything else (stray notifications) is ignored.
        }
    }

    fn diag_seq(&self, uri: &str) -> u64 {
        self.diag_counts.get(uri).copied().unwrap_or(0)
    }

    fn diagnostics_for(&self, uri: &str) -> Vec<Value> {
        self.diagnostics.get(uri).cloned().unwrap_or_default()
    }

    fn store_diagnostics(&mut self, params: &Value) {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let diagnostics = params
            .get("diagnostics")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let count = self.diag_counts.get(&uri).copied().unwrap_or(0) + 1;
        self.diag_counts.insert(uri.clone(), count);
        self.diagnostics.insert(uri, diagnostics);
    }

    /// Read until a fresh diagnostics batch for `uri` arrives, a server request
    /// needs a reply, or `timeout` expires. Unlike `pump`, this returns as soon
    /// as the notification arrives instead of waiting for a JSON-RPC response.
    async fn pump_diagnostics(
        &mut self,
        uri: &str,
        seen: u64,
        timeout: Duration,
    ) -> Result<bool, RpcFailure> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            let message =
                match tokio::time::timeout(remaining, read_message(&mut self.stdout)).await {
                    Ok(Ok(Some(message))) => message,
                    Ok(Ok(None)) => {
                        self.dead = true;
                        return Err(RpcFailure::Closed);
                    }
                    Ok(Err(error)) => {
                        self.dead = true;
                        return Err(RpcFailure::Error(format!("read failed: {error}")));
                    }
                    Err(_) => return Ok(false),
                };

            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if method == "textDocument/publishDiagnostics" {
                    if let Some(params) = message.get("params") {
                        self.store_diagnostics(params);
                    }
                    if self.diag_seq(uri) > seen {
                        return Ok(true);
                    }
                } else if message.get("id").is_some() {
                    let id = message.get("id").cloned().unwrap_or(Value::Null);
                    let reply = server_request_reply(method, &message, id);
                    let _ = self.write_message(&reply).await;
                }
                continue;
            }
            // Responses are ignored here; `wait_diagnostics` runs between
            // requests, so there should be nothing to hand back.
        }
    }

    /// Send `didOpen` the first time and `didChange` when the text changed.
    /// Returns the document URI and whether a notification was sent.
    async fn sync_document(
        &mut self,
        path: &Path,
        text: &str,
    ) -> Result<(String, bool), RpcFailure> {
        let uri = file_url(path);
        let hash = fnv1a(text);
        let existing = self.doc_versions.get(&uri).copied();
        if let Some((_, known)) = existing
            && known == hash
        {
            return Ok((uri, false));
        }
        let version = match existing {
            Some((version, _)) => version + 1,
            None => 1,
        };
        if existing.is_some() {
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri.as_str(), "version": version},
                    "contentChanges": [{"text": text}],
                }),
            )
            .await?;
        } else {
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument": {
                        "uri": uri.as_str(),
                        "languageId": self.language_id.as_str(),
                        "version": version,
                        "text": text,
                    },
                }),
            )
            .await?;
        }
        self.doc_versions.insert(uri.clone(), (version, hash));
        Ok((uri, true))
    }

    /// Wait for a fresh `publishDiagnostics` after `seen`, up to `timeout`.
    /// Absence of diagnostics is not an error.
    async fn wait_diagnostics(
        &mut self,
        uri: &str,
        seen: u64,
        timeout: Duration,
    ) -> Result<(), RpcFailure> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.diag_seq(uri) > seen {
                // Collect a possible second batch (unused imports often follow).
                let _ = self
                    .pump_diagnostics(uri, self.diag_seq(uri), DIAG_GRACE)
                    .await;
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            match self.pump_diagnostics(uri, seen, remaining).await {
                Ok(true) => continue,
                Ok(false) => return Ok(()),
                Err(failure) => return Err(failure),
            }
        }
    }
}

fn server_request_reply(method: &str, message: &Value, id: Value) -> Value {
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "workspace/configuration" => {
            let count = params
                .get("items")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(1);
            Value::Array(vec![Value::Null; count])
        }
        "workspace/workspaceFolders" => Value::Array(Vec::new()),
        "window/workDoneProgress/create"
        | "client/registerCapability"
        | "client/unregisterCapability"
        | "window/showMessageRequest"
        | "window/showDocument" => Value::Null,
        _ => Value::Null,
    };
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

async fn read_message(reader: &mut BufReader<ChildStdout>) -> io::Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }
    let length = match content_length {
        Some(length) if length <= 16 * 1024 * 1024 => length,
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LSP message exceeds 16 MB",
            ));
        }
        None => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "missing Content-Length header",
            ));
        }
    };
    let mut buffer = vec![0u8; length];
    reader.read_exact(&mut buffer).await?;
    serde_json::from_slice(&buffer)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_select_servers() {
        assert_eq!(server_spec_for_ext("rs").unwrap().binary, "rust-analyzer");
        assert_eq!(
            server_spec_for_ext("py").unwrap().binary,
            "pyright-langserver"
        );
        assert_eq!(
            server_spec_for_ext("tsx").unwrap().language_id,
            "typescriptreact"
        );
        assert_eq!(server_spec_for_ext("js").unwrap().args, &["--stdio"][..]);
        assert_eq!(server_spec_for_ext("go").unwrap().binary, "gopls");
        assert_eq!(server_spec_for_ext("hpp").unwrap().language_id, "cpp");
        assert!(server_spec_for_ext("txt").is_none());
    }

    #[test]
    fn file_url_escapes_special_characters() {
        assert_eq!(
            file_url(Path::new("/tmp/a b/c.rs")),
            "file:///tmp/a%20b/c.rs"
        );
        assert_eq!(
            file_url(Path::new("/tmp/ok-file_1.rs")),
            "file:///tmp/ok-file_1.rs"
        );
    }

    #[test]
    fn uri_paths_round_trip() {
        let path = Path::new("/tmp/a b/c.rs");
        let uri = file_url(path);
        assert_eq!(uri_to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn symbol_positions_are_one_based() {
        let text = "fn main() {\n    helper();\n}\n";
        assert_eq!(locate_symbol(text, "helper"), Some((2, 5)));
        assert_eq!(locate_symbol(text, "missing"), None);
    }

    #[test]
    fn outline_renders_kinds_and_lines() {
        let symbols = vec![json!({
            "name": "main",
            "kind": 12,
            "selectionRange": {"start": {"line": 11, "character": 3}},
            "children": [{
                "name": "print",
                "kind": 6,
                "selectionRange": {"start": {"line": 13, "character": 4}}
            }]
        })];
        let lines = render_outline(&symbols);
        assert_eq!(lines[0], "main (function, line 12)");
        assert_eq!(lines[1], "  print (method, line 14)");
    }

    #[test]
    fn hover_renders_markup_and_caps_lines() {
        let hover = json!({"contents": {"kind": "markdown", "value": "```rust\nfn f()\n```"}});
        assert_eq!(render_hover(&hover), "```rust\nfn f()\n```");
        let long = "x\n".repeat(40);
        let lines = render_hover(&json!(long));
        assert!(lines.ends_with("more lines"));
        assert!(lines.lines().count() <= MAX_HOVER_LINES + 1);
    }

    #[test]
    fn diagnostics_render_severity_and_cap() {
        let diagnostics: Vec<Value> = (0..45)
            .map(|index| {
                json!({
                    "range": {"start": {"line": index, "character": 2}},
                    "severity": 1,
                    "message": "mismatched types\nsecond line"
                })
            })
            .collect();
        let rendered = render_diagnostics(&diagnostics);
        assert!(rendered.starts_with("1:3 error mismatched types\n"));
        assert_eq!(rendered.matches("error").count(), MAX_DIAGNOSTICS);
        assert!(rendered.contains("5 more diagnostics"));
    }

    #[test]
    fn edit_diagnostics_keep_errors_and_warnings_only() {
        let diagnostics = vec![
            json!({
                "range": {"start": {"line": 1, "character": 2}},
                "severity": 1,
                "code": "E0308",
                "message": "mismatched types\nlong explanation"
            }),
            json!({
                "range": {"start": {"line": 4, "character": 0}},
                "severity": 3,
                "message": "info only"
            }),
            json!({
                "range": {"start": {"line": 6, "character": 1}},
                "severity": 2,
                "code": 42,
                "message": "unused variable"
            }),
        ];
        let rendered = render_edit_diagnostics("rust-analyzer", &diagnostics);
        assert!(rendered.contains("2:3 error [E0308] mismatched types"));
        assert!(rendered.contains("7:2 warning [42] unused variable"));
        assert!(!rendered.contains("info only"));
        assert!(rendered.chars().count() <= MAX_EDIT_DIAGNOSTIC_CHARS);
    }

    #[test]
    fn edit_diagnostics_report_a_clean_batch() {
        assert_eq!(
            render_edit_diagnostics("rust-analyzer", &[]),
            "diagnostics (rust-analyzer): none"
        );
    }

    #[test]
    fn project_root_marker_requires_a_real_project() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("loose.rs");
        std::fs::write(&file, "").expect("file");
        assert_eq!(
            project_root_marker(&file, &["Cargo.toml"], dir.path()),
            None
        );
        std::fs::write(dir.path().join("Cargo.toml"), "").expect("marker");
        assert_eq!(
            project_root_marker(&file, &["Cargo.toml"], dir.path()),
            Some(dir.path().to_path_buf())
        );
    }

    #[test]
    fn locations_accept_plain_and_link_results() {
        let plain = json!([{
            "uri": "file:///tmp/a.rs",
            "range": {"start": {"line": 3, "character": 0}}
        }]);
        assert_eq!(
            locations_from(&plain),
            vec![("file:///tmp/a.rs".to_string(), 4, 1)]
        );
        let link = json!([{
            "targetUri": "file:///tmp/b.rs",
            "targetSelectionRange": {"start": {"line": 0, "character": 9}}
        }]);
        assert_eq!(
            locations_from(&link),
            vec![("file:///tmp/b.rs".to_string(), 1, 10)]
        );
    }

    #[test]
    fn project_root_walks_up_and_respects_bound() {
        let dir = tempfile::tempdir().expect("temp dir");
        let nested = dir.path().join("crate/src");
        std::fs::create_dir_all(&nested).expect("nested dir");
        std::fs::write(dir.path().join("crate/Cargo.toml"), "").expect("marker");
        let file = nested.join("lib.rs");
        std::fs::write(&file, "").expect("file");
        assert_eq!(
            project_root(&file, &["Cargo.toml"], dir.path()),
            dir.path().join("crate")
        );
    }

    fn fake_language_server(dir: &std::path::Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let script = dir.join("fake-lsp");
        std::fs::write(
            &script,
            r#"#!/bin/bash
send() {
  local body="$1"
  printf 'Content-Length: %s\r\n\r\n%s' "${#body}" "$body"
}
while true; do
  len=""
  while IFS= read -r line; do
    line="${line%$'\r'}"
    [ -z "$line" ] && break
    case "$line" in
      *ength:*) len="${line#*:}"; len="${len# }" ;;
    esac
  done
  [ -z "$len" ] && exit 0
  IFS= read -r -N "$len" body || exit 0
  case "$body" in
    *'"method":"initialize"'*)
      id=1
      [[ "$body" =~ \"id\":([0-9]+) ]] && id="${BASH_REMATCH[1]}"
      send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{}}}"
      ;;
    *'"method":"textDocument/didOpen"'*|*'"method":"textDocument/didChange"'*)
      uri=""
      [[ "$body" =~ \"uri\":\"([^\"]+)\" ]] && uri="${BASH_REMATCH[1]}"
      if [[ "$body" == *LH_BAD* ]]; then
        send "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":\"$uri\",\"diagnostics\":[{\"range\":{\"start\":{\"line\":0,\"character\":0}},\"severity\":1,\"code\":\"E0308\",\"message\":\"mismatched types\"}]}}"
      else
        send "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":\"$uri\",\"diagnostics\":[]}}"
      fi
      ;;
    *)
      if [[ "$body" =~ \"id\":([0-9]+) ]]; then
        send "{\"jsonrpc\":\"2.0\",\"id\":${BASH_REMATCH[1]},\"result\":null}"
      fi
      ;;
  esac
done
"#,
        )
        .expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
            .expect("executable");
        script
    }

    fn fake_service(server: &'static str, dir: &std::path::Path) -> Arc<LspService> {
        let service = Arc::new(LspService::new());
        service.override_binary(server, fake_language_server(dir));
        service
    }

    fn rust_project(dir: &std::path::Path) {
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"gate-test\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::create_dir_all(dir.join("src")).expect("src");
    }

    fn python_project(dir: &std::path::Path) {
        std::fs::write(
            dir.join("pyproject.toml"),
            "[project]\nname = \"gate-test\"\nversion = \"0.1.0\"\n",
        )
        .expect("manifest");
    }

    async fn warm(ctx: &ToolContext, path: &str) {
        let result = LspTool::with_service(ctx.lsp.clone())
            .call(ctx, json!({"action": "diagnostics", "path": path}))
            .await;
        assert!(!result.is_error, "warm-up failed: {}", result.content);
    }

    #[tokio::test]
    async fn warn_appends_only_a_new_error_after_a_warm_edit() {
        let dir = tempfile::tempdir().expect("temp dir");
        rust_project(dir.path());
        let file = dir.path().join("src/lib.rs");
        std::fs::write(&file, "pub fn value() -> u32 { 1 }\n").expect("source");
        let mut ctx = crate::tools::write::test_context(dir.path());
        ctx.lsp = fake_service("rust-analyzer", dir.path());
        ctx.lsp.set_gate(crate::config::LspGate::Warn);
        warm(&ctx, "src/lib.rs").await;
        let _ = crate::tools::read::ReadTool
            .call(&ctx, json!({"path": "src/lib.rs"}))
            .await;

        let result = crate::tools::edit::EditTool
            .call(
                &ctx,
                json!({"path": "src/lib.rs", "old": "1", "new": "LH_BAD"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("diagnostics (rust-analyzer):"),
            "diagnostics missing from write result: {}",
            result.content
        );
        assert!(
            result.content.contains("mismatched types"),
            "diagnostic message missing: {}",
            result.content
        );
        assert!(
            std::fs::read_to_string(&file).unwrap().contains("LH_BAD"),
            "warn must keep the change"
        );
    }

    #[tokio::test]
    async fn block_rolls_back_a_rust_type_error() {
        use crate::config::LspGate;

        let dir = tempfile::tempdir().expect("temp dir");
        rust_project(dir.path());
        let file = dir.path().join("src/lib.rs");
        let original = "pub fn value() -> u32 { 1 }\n";
        std::fs::write(&file, original).expect("source");
        let mut ctx = crate::tools::write::test_context(dir.path());
        ctx.lsp = fake_service("rust-analyzer", dir.path());
        ctx.lsp.set_gate(LspGate::Block);
        warm(&ctx, "src/lib.rs").await;
        let _ = crate::tools::read::ReadTool
            .call(&ctx, json!({"path": "src/lib.rs"}))
            .await;

        let result = crate::tools::edit::EditTool
            .call(
                &ctx,
                json!({"path": "src/lib.rs", "old": "1", "new": "LH_BAD"}),
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.starts_with("[EDIT_REGRESSION]"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("mismatched types"),
            "{}",
            result.content
        );
        assert!(result.content.contains("rolled back"), "{}", result.content);
        assert!(result.content.contains("Fix hint:"), "{}", result.content);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
        let details = result.details.expect("details");
        assert_eq!(details["rolled_back"].as_bool(), Some(true));
        assert_eq!(details["new_errors"][0]["code"].as_str(), Some("E0308"));

        // The restored file is stamped as read, so a corrected retry works.
        let retry = crate::tools::edit::EditTool
            .call(&ctx, json!({"path": "src/lib.rs", "old": "1", "new": "2"}))
            .await;
        assert!(!retry.is_error, "{}", retry.content);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "pub fn value() -> u32 { 2 }\n"
        );
    }

    #[tokio::test]
    async fn block_rolls_back_a_python_type_error() {
        use crate::config::LspGate;

        let dir = tempfile::tempdir().expect("temp dir");
        python_project(dir.path());
        let file = dir.path().join("app.py");
        let original = "value: int = 1\n";
        std::fs::write(&file, original).expect("source");
        let mut ctx = crate::tools::write::test_context(dir.path());
        ctx.lsp = fake_service("pyright-langserver", dir.path());
        ctx.lsp.set_gate(LspGate::Block);
        warm(&ctx, "app.py").await;
        let _ = crate::tools::read::ReadTool
            .call(&ctx, json!({"path": "app.py"}))
            .await;

        let result = crate::tools::edit::EditTool
            .call(
                &ctx,
                json!({"path": "app.py", "old": "1", "new": "\"LH_BAD\""}),
            )
            .await;
        assert!(result.is_error, "{}", result.content);
        assert!(
            result.content.starts_with("[EDIT_REGRESSION]"),
            "{}",
            result.content
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), original);
    }

    #[tokio::test]
    async fn clean_edit_adds_no_diagnostic_text() {
        let dir = tempfile::tempdir().expect("temp dir");
        rust_project(dir.path());
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn value() -> u32 { 1 }\n",
        )
        .expect("source");
        let mut ctx = crate::tools::write::test_context(dir.path());
        ctx.lsp = fake_service("rust-analyzer", dir.path());
        warm(&ctx, "src/lib.rs").await;
        let _ = crate::tools::read::ReadTool
            .call(&ctx, json!({"path": "src/lib.rs"}))
            .await;

        let result = crate::tools::edit::EditTool
            .call(&ctx, json!({"path": "src/lib.rs", "old": "1", "new": "2"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            !result.content.contains("diagnostics"),
            "a clean edit must stay clean: {}",
            result.content
        );
        assert!(
            result
                .details
                .as_ref()
                .unwrap()
                .get("diagnostics")
                .is_none()
        );
    }

    #[tokio::test]
    async fn pre_existing_errors_are_not_repeated_or_blocking() {
        use crate::config::LspGate;

        let dir = tempfile::tempdir().expect("temp dir");
        rust_project(dir.path());
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn value() -> u32 { LH_BAD }\n",
        )
        .expect("source");
        let mut ctx = crate::tools::write::test_context(dir.path());
        ctx.lsp = fake_service("rust-analyzer", dir.path());
        ctx.lsp.set_gate(LspGate::Block);
        warm(&ctx, "src/lib.rs").await;
        let _ = crate::tools::read::ReadTool
            .call(&ctx, json!({"path": "src/lib.rs"}))
            .await;

        let result = crate::tools::edit::EditTool
            .call(
                &ctx,
                json!({"path": "src/lib.rs", "old": "value", "new": "answer"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            !result.content.contains("diagnostics"),
            "pre-existing errors must not be repeated: {}",
            result.content
        );
        assert!(
            std::fs::read_to_string(dir.path().join("src/lib.rs"))
                .unwrap()
                .contains("answer")
        );
    }

    #[tokio::test]
    async fn cold_and_non_project_files_are_unaffected() {
        use crate::config::LspGate;

        // Cold server in a real project: no snapshot, no block.
        let dir = tempfile::tempdir().expect("temp dir");
        rust_project(dir.path());
        let mut ctx = crate::tools::write::test_context(dir.path());
        ctx.lsp = fake_service("rust-analyzer", dir.path());
        ctx.lsp.set_gate(LspGate::Block);
        let result = crate::tools::write::WriteTool
            .call(
                &ctx,
                json!({"path": "src/cold.rs", "content": "pub fn bad() -> u32 { LH_BAD }\n"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            !result.content.contains("diagnostics"),
            "{}",
            result.content
        );

        // Loose file without a project marker cannot be gated, even warm.
        let loose = tempfile::tempdir().expect("loose dir");
        std::fs::write(
            loose.path().join("loose.rs"),
            "pub fn value() -> u32 { 1 }\n",
        )
        .expect("source");
        let mut ctx = crate::tools::write::test_context(loose.path());
        ctx.lsp = fake_service("rust-analyzer", loose.path());
        ctx.lsp.set_gate(LspGate::Block);
        warm(&ctx, "loose.rs").await;
        let _ = crate::tools::read::ReadTool
            .call(&ctx, json!({"path": "loose.rs"}))
            .await;
        let result = crate::tools::edit::EditTool
            .call(
                &ctx,
                json!({"path": "loose.rs", "old": "1", "new": "LH_BAD"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            std::fs::read_to_string(loose.path().join("loose.rs"))
                .unwrap()
                .contains("LH_BAD")
        );
    }

    #[tokio::test]
    async fn warm_gate_stays_under_the_latency_target() {
        use std::time::Instant;

        let dir = tempfile::tempdir().expect("temp dir");
        rust_project(dir.path());
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn value() -> u32 { 0 }\n",
        )
        .expect("source");
        let mut ctx = crate::tools::write::test_context(dir.path());
        ctx.lsp = fake_service("rust-analyzer", dir.path());
        warm(&ctx, "src/lib.rs").await;
        let _ = crate::tools::read::ReadTool
            .call(&ctx, json!({"path": "src/lib.rs"}))
            .await;

        let mut latencies = Vec::new();
        for value in 1..=5 {
            let started = Instant::now();
            let result = crate::tools::edit::EditTool
                .call(
                    &ctx,
                    json!({
                        "path": "src/lib.rs",
                        "old": format!("pub fn value() -> u32 {{ {} }}\n", value - 1),
                        "new": format!("pub fn value() -> u32 {{ {value} }}\n"),
                    }),
                )
                .await;
            latencies.push(started.elapsed());
            assert!(!result.is_error, "{}", result.content);
        }
        latencies.sort();
        let p95 = latencies[(latencies.len() * 95 / 100).min(latencies.len() - 1)];
        assert!(
            p95 < std::time::Duration::from_millis(300),
            "warm gate p95 was {p95:?}"
        );
    }

    #[tokio::test]
    #[ignore = "requires a working rust-analyzer on PATH"]
    async fn real_rust_analyzer_reports_an_edit_error() {
        let Some(binary) = find_binary("rust-analyzer") else {
            eprintln!("skipping: rust-analyzer is not on PATH");
            return;
        };
        let usable = std::process::Command::new(binary)
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success());
        if !usable {
            eprintln!("skipping: rust-analyzer is installed but not runnable");
            return;
        }
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"diag-smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("manifest");
        std::fs::create_dir_all(dir.path().join("src")).expect("src");
        let file = dir.path().join("src/lib.rs");
        std::fs::write(&file, "pub fn bad() -> u32 { \"no\" }\n").expect("source");
        let ctx = crate::tools::write::test_context(dir.path());
        let service = LspService::new();
        let diagnostics = service
            .diagnostics_after_edit(&ctx, &file)
            .await
            .expect("rust-analyzer should publish diagnostics");
        assert!(diagnostics.contains("error"), "{diagnostics}");
        assert!(
            diagnostics.contains("mismatched") || diagnostics.contains("expected"),
            "{diagnostics}"
        );
    }
}

//! `lsp` tool: outline, definition, references, diagnostics, and hover over a
//! minimal stdio JSON-RPC language-server client.
//!
//! One client is kept per `(server, root)` pair. Clients are created lazily,
//! disposed after ten minutes of inactivity, and killed when replaced.

use std::collections::HashMap;
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

use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, object_schema, optional_str,
    optional_u64, required_str,
};

const INIT_TIMEOUT: Duration = Duration::from_secs(15);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DIAG_WINDOW: Duration = Duration::from_millis(2500);
const DIAG_GRACE: Duration = Duration::from_millis(250);
const IDLE_DISPOSE: Duration = Duration::from_secs(10 * 60);

const MAX_OUTLINE: usize = 200;
const MAX_DEFINITIONS: usize = 5;
const MAX_REFERENCES: usize = 20;
const MAX_DIAGNOSTICS: usize = 40;
const MAX_HOVER_LINES: usize = 25;
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

const ACTIONS: &[&str] = &[
    "outline",
    "definition",
    "references",
    "diagnostics",
    "hover",
];

pub struct LspTool {
    clients: Mutex<HashMap<String, Arc<AsyncMutex<LspClient>>>>,
}

impl LspTool {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
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

        match client.request("initialize", initialize, INIT_TIMEOUT).await {
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

        let binary = find_binary(spec.binary).ok_or_else(|| {
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
            None => self.connect(spec, &binary, &root, &key).await?,
        };

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
        match self.run(ctx, args).await {
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
    let bound = if file.starts_with(fallback) {
        fallback.to_path_buf()
    } else {
        PathBuf::from("/")
    };
    let mut dir = match file.parent() {
        Some(parent) => parent.to_path_buf(),
        None => return fallback.to_path_buf(),
    };
    loop {
        if markers.iter().any(|marker| dir.join(marker).exists()) {
            return dir;
        }
        if dir == bound || dir.parent().is_none() {
            break;
        }
        if !dir.pop() {
            break;
        }
    }
    fallback.to_path_buf()
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
                let grace_deadline = Instant::now() + DIAG_GRACE;
                let _ = self.pump(grace_deadline).await;
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(());
            }
            match self.pump(deadline).await {
                Ok(_) => continue,
                Err(RpcFailure::Timeout) => return Ok(()),
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
}

//! MCP client: handshake, tool listing, and tool calls over a transport.
//! A dead connection is re-established once on the next call.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::expand::{expand, expand_map};
use super::http::HttpTransport;
use super::stdio::{StdioSpec, StdioTransport};
use super::transport::{McpError, Transport};
use crate::config::McpServerConfig;

/// Newest protocol revision this client speaks. Older revisions the server
/// picks are accepted: the tool surface used here is unchanged since
/// 2024-11-05.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// Upper bound on `tools/list` pages, so a buggy cursor cannot loop forever.
const MAX_PAGES: usize = 50;

/// A tool as advertised by `tools/list`.
#[derive(Debug, Clone)]
pub struct RemoteTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub read_only: bool,
}

/// What the server said about itself during the handshake.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol: String,
    pub instructions: Option<String>,
}

pub struct McpClient {
    pub name: String,
    config: McpServerConfig,
    project_dir: PathBuf,
    connection: tokio::sync::Mutex<Option<Connection>>,
}

struct Connection {
    transport: Arc<dyn Transport>,
    info: ServerInfo,
}

impl McpClient {
    pub fn new(name: &str, config: McpServerConfig, project_dir: &Path) -> Self {
        Self {
            name: name.to_string(),
            config,
            project_dir: project_dir.to_path_buf(),
            connection: tokio::sync::Mutex::new(None),
        }
    }

    pub fn config(&self) -> &McpServerConfig {
        &self.config
    }

    pub fn transport_label(&self) -> &'static str {
        if self.config.url.trim().is_empty() {
            "stdio"
        } else {
            "http"
        }
    }

    /// Connect (if needed) and return the server's handshake info.
    pub async fn connect(&self) -> Result<ServerInfo, McpError> {
        let mut guard = self.connection.lock().await;
        if let Some(connection) = guard.as_ref()
            && connection.transport.is_alive()
        {
            return Ok(connection.info.clone());
        }
        if let Some(old) = guard.take() {
            old.transport.close().await;
        }
        let connection = self.open().await?;
        let info = connection.info.clone();
        *guard = Some(connection);
        Ok(info)
    }

    async fn open(&self) -> Result<Connection, McpError> {
        let transport: Arc<dyn Transport> = if self.config.url.trim().is_empty() {
            let cwd = if self.config.cwd.as_os_str().is_empty() {
                self.project_dir.clone()
            } else if self.config.cwd.is_absolute() {
                self.config.cwd.clone()
            } else {
                self.project_dir.join(&self.config.cwd)
            };
            let args = self
                .config
                .args
                .iter()
                .map(|arg| expand(arg))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| self.config_error(error))?;
            Arc::new(StdioTransport::spawn(StdioSpec {
                name: self.name.clone(),
                command: expand(&self.config.command).map_err(|e| self.config_error(e))?,
                args,
                env: expand_map(&self.config.env).map_err(|e| self.config_error(e))?,
                cwd,
            })?)
        } else {
            let url = expand(&self.config.url).map_err(|e| self.config_error(e))?;
            let headers = expand_map(&self.config.headers).map_err(|e| self.config_error(e))?;
            Arc::new(HttpTransport::new(&self.name, &url, &headers)?)
        };

        let deadline = self.config.startup_timeout();
        let cancel = CancellationToken::new();
        let result = transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "localharness",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
                deadline,
                &cancel,
            )
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                let diagnostics = transport.diagnostics();
                transport.close().await;
                return Err(with_diagnostics(error, &diagnostics));
            }
        };
        let protocol = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !SUPPORTED_VERSIONS.contains(&protocol.as_str()) {
            transport.close().await;
            return Err(McpError::Other(format!(
                "MCP server `{}` speaks protocol {protocol:?}; this client supports {}",
                self.name,
                SUPPORTED_VERSIONS.join(", ")
            )));
        }
        transport.set_protocol_version(&protocol);
        transport
            .notify("notifications/initialized", Value::Null)
            .await?;
        let server = result.get("serverInfo");
        let info = ServerInfo {
            name: server
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(&self.name)
                .to_string(),
            version: server
                .and_then(|s| s.get("version"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            protocol,
            instructions: result
                .get("instructions")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_string),
        };
        Ok(Connection { transport, info })
    }

    fn config_error(&self, error: String) -> McpError {
        McpError::Other(format!("[mcp.{}] {error}", self.name))
    }

    /// One request with a single reconnect when the connection is gone.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, McpError> {
        let mut attempt = 0;
        loop {
            self.connect().await?;
            let transport = {
                let guard = self.connection.lock().await;
                match guard.as_ref() {
                    Some(connection) => connection.transport.clone(),
                    None => return Err(McpError::Closed("not connected".into())),
                }
            };
            match transport
                .request(method, params.clone(), timeout, cancel)
                .await
            {
                Err(error) if error.is_closed() && attempt == 0 => {
                    tracing::warn!(server = %self.name, %error, "MCP connection lost; reconnecting");
                    attempt += 1;
                }
                Err(error) => return Err(with_diagnostics(error, &transport.diagnostics())),
                ok => return ok,
            }
        }
    }

    /// Every tool the server advertises, following pagination.
    pub async fn list_tools(&self) -> Result<Vec<RemoteTool>, McpError> {
        let cancel = CancellationToken::new();
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({"cursor": cursor}),
                None => Value::Null,
            };
            let page = self
                .request("tools/list", params, self.config.startup_timeout(), &cancel)
                .await?;
            for tool in page
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                tools.push(RemoteTool {
                    name: name.to_string(),
                    description: tool
                        .get("description")
                        .and_then(Value::as_str)
                        .or_else(|| tool.get("title").and_then(Value::as_str))
                        .unwrap_or_default()
                        .to_string(),
                    input_schema: tool
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object"})),
                    read_only: tool
                        .pointer("/annotations/readOnlyHint")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                });
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(tools)
    }

    /// `tools/call`; returns the raw `CallToolResult`.
    pub async fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value, McpError> {
        self.request(
            "tools/call",
            json!({"name": tool, "arguments": arguments}),
            self.config.timeout(),
            cancel,
        )
        .await
    }

    pub async fn info(&self) -> Option<ServerInfo> {
        self.connection
            .lock()
            .await
            .as_ref()
            .map(|connection| connection.info.clone())
    }

    pub async fn close(&self) {
        if let Some(connection) = self.connection.lock().await.take() {
            connection.transport.close().await;
        }
    }
}

fn with_diagnostics(error: McpError, diagnostics: &str) -> McpError {
    if diagnostics.is_empty() {
        return error;
    }
    match error {
        McpError::Other(message) => McpError::Other(format!("{message}; stderr: {diagnostics}")),
        McpError::Timeout(_) | McpError::Rpc { .. } | McpError::Cancelled => error,
        McpError::Closed(message) if message.contains("stderr:") => McpError::Closed(message),
        McpError::Closed(message) => McpError::Closed(format!("{message}; stderr: {diagnostics}")),
    }
}

//! Model Context Protocol client support.
//!
//! Servers are configured under `[mcp.<name>]` (stdio `command` or Streamable
//! HTTP `url`). Each remote tool becomes a harness tool named
//! `<server>__<tool>` with the same contract as built-ins: prescriptive
//! errors, capped output, cancellation, and evidence records. Connections
//! live in an [`McpManager`] that survives core rebuilds (`/profile`,
//! `/model`), so servers are not restarted on every config change.

pub mod client;
pub mod expand;
pub mod http;
pub mod stdio;
pub mod transport;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::config::{Config, McpServerConfig};
use crate::external::{cap_content, cap_details, intern, validate_args};
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

pub use client::{McpClient, RemoteTool, ServerInfo};
pub use transport::McpError;

/// Separator between server and tool in exposed names.
pub const SEPARATOR: &str = "__";
/// OpenAI-compatible function names are limited to 64 characters.
const NAME_LIMIT: usize = 64;
/// Tool descriptions are capped: every character is paid on every turn.
const DESCRIPTION_LIMIT: usize = 600;

/// Connection state of one configured server, for `/mcp` and diagnostics.
#[derive(Debug, Clone)]
pub struct ServerStatus {
    pub name: String,
    pub transport: &'static str,
    pub state: ServerState,
}

#[derive(Debug, Clone)]
pub enum ServerState {
    Connected {
        server: String,
        version: String,
        protocol: String,
        /// Exposed tool names.
        tools: Vec<String>,
        /// Whether the active profile carries this server's tools.
        attached: bool,
    },
    Failed(String),
    Disabled,
    /// Configured, but the active profile does not use it, so it was not started.
    Idle,
}

impl ServerStatus {
    pub fn summary(&self) -> String {
        match &self.state {
            ServerState::Connected {
                server,
                version,
                protocol,
                tools,
                attached,
            } => format!(
                "{} ({}): connected to {server}{}{} · protocol {protocol} · {} tool{}{}",
                self.name,
                self.transport,
                if version.is_empty() { "" } else { " " },
                version,
                tools.len(),
                if tools.len() == 1 { "" } else { "s" },
                if *attached {
                    ""
                } else {
                    " · not attached to this profile"
                },
            ),
            ServerState::Failed(error) => {
                format!("{} ({}): failed: {error}", self.name, self.transport)
            }
            ServerState::Disabled => format!("{} ({}): disabled", self.name, self.transport),
            ServerState::Idle => format!(
                "{} ({}): not used by this profile; add \"mcp:{}\" to the profile's tools",
                self.name, self.transport, self.name
            ),
        }
    }
}

/// Tools contributed by MCP servers for one profile.
#[derive(Default)]
pub struct McpResolution {
    /// Servers whose tools join the profile without being named.
    pub auto: Vec<Arc<dyn Tool>>,
    /// Every connected server's exposed tools, for `mcp:<server>` and
    /// explicit tool names in a profile.
    pub by_server: BTreeMap<String, Vec<Arc<dyn Tool>>>,
    pub warnings: Vec<String>,
}

impl McpResolution {
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn Tool>> {
        self.by_server.values().flatten()
    }
}

/// Shared MCP connections. Cheap to share; rebuilt cores reuse it.
#[derive(Default)]
pub struct McpManager {
    clients: tokio::sync::Mutex<BTreeMap<String, Arc<McpClient>>>,
    status: Mutex<Vec<ServerStatus>>,
}

impl McpManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Last known state of every configured server.
    pub fn status(&self) -> Vec<ServerStatus> {
        match self.status.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// Connect the servers the active profile uses and build their tools.
    /// `reserved` holds built-in and external tool names, which win any clash.
    /// Failures become warnings: a broken server never blocks startup.
    pub async fn resolve(
        &self,
        config: &Config,
        project_dir: &Path,
        reserved: &[&str],
    ) -> McpResolution {
        let profile_name = config.profile_name().to_string();
        let referenced = referenced_servers(config);
        let mut resolution = McpResolution::default();
        let mut statuses = Vec::new();

        let mut wanted: Vec<(String, Arc<McpClient>)> = Vec::new();
        {
            let mut clients = self.clients.lock().await;
            // Close servers whose configuration changed or disappeared.
            let stale: Vec<String> = clients
                .iter()
                .filter(|(name, client)| config.mcp.get(*name) != Some(client.config()))
                .map(|(name, _)| name.clone())
                .collect();
            for name in stale {
                if let Some(client) = clients.remove(&name) {
                    client.close().await;
                }
            }
            for (name, server) in &config.mcp {
                let transport = transport_label(server);
                if !server.enabled {
                    statuses.push(ServerStatus {
                        name: name.clone(),
                        transport,
                        state: ServerState::Disabled,
                    });
                    continue;
                }
                if !server.attaches_to(&profile_name) && !referenced.contains(name) {
                    statuses.push(ServerStatus {
                        name: name.clone(),
                        transport,
                        state: ServerState::Idle,
                    });
                    continue;
                }
                let client = clients
                    .entry(name.clone())
                    .or_insert_with(|| Arc::new(McpClient::new(name, server.clone(), project_dir)))
                    .clone();
                wanted.push((name.clone(), client));
            }
        }

        let listings = futures_util::future::join_all(wanted.iter().map(|(_, client)| {
            let client = client.clone();
            async move {
                let deadline = client.config().startup_timeout();
                match tokio::time::timeout(deadline, async {
                    let info = client.connect().await?;
                    let tools = client.list_tools().await?;
                    Ok::<_, McpError>((info, tools))
                })
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(McpError::Timeout(deadline)),
                }
            }
        }))
        .await;

        let mut taken: Vec<String> = reserved.iter().map(|name| name.to_string()).collect();
        for ((name, client), listing) in wanted.into_iter().zip(listings) {
            let server = client.config().clone();
            let transport = transport_label(&server);
            let (info, remote) = match listing {
                Ok(listing) => listing,
                Err(error) => {
                    let message = startup_error(&name, &error);
                    resolution.warnings.push(message.clone());
                    statuses.push(ServerStatus {
                        name,
                        transport,
                        state: ServerState::Failed(error.to_string()),
                    });
                    continue;
                }
            };
            let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
            for tool in remote {
                if !server.tools.is_empty() && !server.tools.contains(&tool.name) {
                    continue;
                }
                if server.exclude_tools.contains(&tool.name) {
                    continue;
                }
                let exposed = exposed_name(&name, &tool.name);
                if taken.contains(&exposed) {
                    resolution.warnings.push(format!(
                        "MCP tool `{}` from `{name}` was skipped: the name `{exposed}` is already taken",
                        tool.name
                    ));
                    continue;
                }
                taken.push(exposed.clone());
                tools.push(Arc::new(McpTool::new(
                    client.clone(),
                    &name,
                    &exposed,
                    tool,
                )));
            }
            if server.attaches_to(&profile_name) {
                resolution.auto.extend(tools.iter().cloned());
            }
            statuses.push(ServerStatus {
                name: name.clone(),
                transport,
                state: ServerState::Connected {
                    server: info.name,
                    version: info.version,
                    protocol: info.protocol,
                    tools: tools.iter().map(|tool| tool.name().to_string()).collect(),
                    attached: server.attaches_to(&profile_name),
                },
            });
            resolution.by_server.insert(name, tools);
        }

        statuses.sort_by(|a, b| a.name.cmp(&b.name));
        match self.status.lock() {
            Ok(mut guard) => *guard = statuses,
            Err(poisoned) => *poisoned.into_inner() = statuses,
        }
        resolution
    }

    /// Connect every enabled server regardless of profile; used by `lh mcp`.
    pub async fn probe_all(
        config: &Config,
        project_dir: &Path,
    ) -> Vec<(
        String,
        McpServerConfig,
        Result<(ServerInfo, Vec<RemoteTool>), McpError>,
    )> {
        let probes = config
            .mcp
            .iter()
            .filter(|(_, s)| s.enabled)
            .map(|(name, server)| {
                let client = McpClient::new(name, server.clone(), project_dir);
                let name = name.clone();
                let server = server.clone();
                async move {
                    let deadline = server.startup_timeout();
                    let result = match tokio::time::timeout(deadline, async {
                        let info = client.connect().await?;
                        let tools = client.list_tools().await?;
                        Ok::<_, McpError>((info, tools))
                    })
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(McpError::Timeout(deadline)),
                    };
                    client.close().await;
                    (name, server, result)
                }
            });
        futures_util::future::join_all(probes).await
    }

    /// Stop every server.
    pub async fn shutdown(&self) {
        let clients: Vec<Arc<McpClient>> = std::mem::take(&mut *self.clients.lock().await)
            .into_values()
            .collect();
        futures_util::future::join_all(clients.iter().map(|client| client.close())).await;
    }
}

fn transport_label(server: &McpServerConfig) -> &'static str {
    if server.url.trim().is_empty() {
        "stdio"
    } else {
        "http"
    }
}

fn startup_error(name: &str, error: &McpError) -> String {
    let hint = match error {
        McpError::Timeout(_) => {
            format!("raise startup_timeout_secs in [mcp.{name}] if the server is slow to start")
        }
        _ => format!("run `lh mcp list` to diagnose, or set enabled = false in [mcp.{name}]"),
    };
    format!("MCP server `{name}` is unavailable: {error}; {hint}")
}

/// Servers a profile names via `mcp:<server>` or `<server>__<tool>`.
pub fn referenced_servers(config: &Config) -> Vec<String> {
    let mut servers = Vec::new();
    for entry in &config.profile().tools {
        let server = if let Some(server) = entry.strip_prefix("mcp:") {
            Some(server.to_string())
        } else {
            config
                .mcp
                .keys()
                .find(|server| entry.starts_with(&format!("{}{SEPARATOR}", sanitize(server))))
                .cloned()
        };
        if let Some(server) = server
            && !servers.contains(&server)
        {
            servers.push(server);
        }
    }
    servers
}

/// Characters allowed in function names by OpenAI-compatible servers.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `<server>__<tool>`, sanitized and kept within 64 characters. Long names are
/// shortened with a stable hash suffix so they stay unique.
pub fn exposed_name(server: &str, tool: &str) -> String {
    let full = format!("{}{SEPARATOR}{}", sanitize(server), sanitize(tool));
    if full.len() <= NAME_LIMIT {
        return full;
    }
    let digest = Sha256::digest(format!("{server}\0{tool}").as_bytes());
    let suffix: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{}_{suffix}", &full[..NAME_LIMIT - suffix.len() - 1])
}

/// Make a server's `inputSchema` acceptable to chat-completions tool specs:
/// always an object schema with `properties`, without `$schema`.
pub fn normalize_schema(schema: &Value) -> Value {
    let mut map = match schema {
        Value::Object(map) => map.clone(),
        _ => Map::new(),
    };
    map.remove("$schema");
    map.insert("type".into(), Value::String("object".into()));
    if !map.get("properties").is_some_and(Value::is_object) {
        map.insert("properties".into(), json!({}));
    }
    Value::Object(map)
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// One remote tool exposed through the harness `Tool` contract.
pub struct McpTool {
    client: Arc<McpClient>,
    server: String,
    remote: String,
    name: &'static str,
    description: &'static str,
    schema: Value,
    read_only: bool,
}

impl McpTool {
    pub fn new(client: Arc<McpClient>, server: &str, exposed: &str, tool: RemoteTool) -> Self {
        let description = if tool.description.trim().is_empty() {
            format!("`{}` from MCP server `{server}`", tool.name)
        } else {
            tool.description.clone()
        };
        Self {
            client,
            server: server.to_string(),
            remote: tool.name,
            name: intern(exposed),
            description: intern(&truncate_chars(&description, DESCRIPTION_LIMIT)),
            schema: normalize_schema(&tool.input_schema),
            read_only: tool.read_only,
        }
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    fn error_result(&self, error: McpError) -> ToolResult {
        let server = &self.server;
        match error {
            McpError::Rpc { code, message } => ToolResult::err(
                "MCP_ERROR",
                format!(
                    "MCP server `{server}` rejected `{}` (code {code}): {message}; check the arguments against the tool schema",
                    self.remote
                ),
            ),
            McpError::Timeout(after) => ToolResult::err(
                "MCP_TIMEOUT",
                format!(
                    "`{}` on MCP server `{server}` did not answer within {after:?}; retry, narrow the request, or raise timeout_secs in [mcp.{server}]",
                    self.remote
                ),
            ),
            McpError::Cancelled => ToolResult::err(
                "cancelled",
                format!("`{}` was cancelled; retry the call when ready", self.name),
            ),
            McpError::Closed(message) | McpError::Other(message) => ToolResult::err(
                "MCP_UNAVAILABLE",
                format!(
                    "{message}; the tool cannot run until MCP server `{server}` is reachable. Continue without it or ask the user to check `lh mcp list`"
                ),
            ),
        }
    }
}

/// Turn a `CallToolResult` into a harness result. Text is kept; binary
/// content is summarized; `structuredContent` is shown when there is no text.
pub fn convert_result(server: &str, tool: &str, result: &Value) -> ToolResult {
    let mut parts: Vec<String> = Vec::new();
    for item in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        let text = match kind {
            "text" => item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            "image" | "audio" => {
                let mime = item
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown type");
                let size = item
                    .get("data")
                    .and_then(Value::as_str)
                    .map(|data| data.len() / 4 * 3)
                    .unwrap_or(0);
                format!("[{kind} omitted: {mime}, about {size} bytes]")
            }
            "resource_link" => format!(
                "[resource link: {} {}]",
                item.get("name").and_then(Value::as_str).unwrap_or(""),
                item.get("uri").and_then(Value::as_str).unwrap_or("")
            ),
            "resource" => {
                let resource = item.get("resource").cloned().unwrap_or(Value::Null);
                match resource.get("text").and_then(Value::as_str) {
                    Some(text) => text.to_string(),
                    None => format!(
                        "[binary resource omitted: {} ({})]",
                        resource.get("uri").and_then(Value::as_str).unwrap_or(""),
                        resource
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown type")
                    ),
                }
            }
            other => format!("[unsupported content type {other:?} omitted]"),
        };
        if !text.is_empty() {
            parts.push(text);
        }
    }

    let structured = result.get("structuredContent").filter(|v| !v.is_null());
    if parts.is_empty()
        && let Some(structured) = structured
    {
        parts.push(serde_json::to_string_pretty(structured).unwrap_or_default());
    }

    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut content = parts.join("\n\n");
    if content.trim().is_empty() {
        content = format!("`{tool}` on MCP server `{server}` returned no content");
    }
    if is_error {
        content = format!(
            "[MCP_TOOL_ERROR] {content}\n\nThe tool reported a failure; fix the arguments or try another approach."
        );
    }
    let mut details = json!({"server": server, "tool": tool});
    if let Some(structured) = structured {
        details["structured"] = cap_details(structured.clone());
    }
    if is_error {
        details["code"] = json!("MCP_TOOL_ERROR");
    }
    ToolResult {
        content: cap_content(content),
        is_error,
        details: Some(details),
    }
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &'static str {
        self.name
    }

    fn description(&self) -> &'static str {
        self.description
    }

    fn schema(&self) -> Value {
        self.schema.clone()
    }

    fn pack(&self) -> ToolPack {
        // Attached explicitly after pack selection, like external tools.
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn evidence_enabled(&self) -> bool {
        true
    }

    fn evidence_command(&self, args: &Value) -> Option<String> {
        let args = serde_json::to_string(args).unwrap_or_else(|_| "{}".into());
        Some(format!("mcp {}/{} {args}", self.server, self.remote))
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        if ctx.is_cancelled() {
            return ToolResult::err(
                "cancelled",
                format!(
                    "`{}` was not started because the turn was cancelled; retry when ready",
                    self.name
                ),
            );
        }
        let args = match args {
            Value::Object(object) => object,
            Value::Null => Map::new(),
            _ => {
                return ToolResult::err(
                    "MCP_ARGS",
                    format!(
                        "arguments for `{}` must be a JSON object matching its schema",
                        self.name
                    ),
                );
            }
        };
        if let Err(message) = validate_args(self.name, &args, &self.schema) {
            return ToolResult::err("MCP_ARGS", message);
        }
        match self
            .client
            .call_tool(&self.remote, Value::Object(args), &ctx.cancel)
            .await
        {
            Ok(result) => convert_result(&self.server, &self.remote, &result),
            Err(error) => self.error_result(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mcp_tools_follow_the_tool_contract() {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .map(|out| !out.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/mcp_stdio_server.py");
        let mut config = Config::builtin();
        config.profile_name = "minimal".into();
        config.mcp.insert(
            "fx".into(),
            McpServerConfig {
                command: "python3".into(),
                args: vec![fixture.display().to_string()],
                tools: vec!["echo".into(), "fail".into()],
                ..Default::default()
            },
        );
        let manager = McpManager::new();
        let resolution = manager.resolve(&config, dir.path(), &["read"]).await;
        assert!(resolution.warnings.is_empty(), "{:?}", resolution.warnings);
        let names: Vec<&str> = resolution.auto.iter().map(|t| t.name()).collect();
        assert_eq!(names, ["fx__echo", "fx__fail"]);
        let echo = resolution.auto[0].clone();
        assert!(echo.read_only());
        assert!(echo.schema().get("$schema").is_none());

        let ctx = crate::tools::write::test_context(dir.path());
        let ok = echo.call(&ctx, json!({"text": "hello"})).await;
        assert!(!ok.is_error, "{}", ok.content);
        assert_eq!(ok.content, "echo: hello");

        let missing = echo.call(&ctx, json!({})).await;
        assert!(missing.is_error);
        assert!(
            missing
                .content
                .starts_with("[MCP_ARGS] missing required argument `text`")
        );

        let failed = resolution.auto[1].call(&ctx, json!({})).await;
        assert!(failed.is_error);
        assert!(failed.content.contains("it broke"));

        assert_eq!(
            echo.evidence_command(&json!({"text": "x"})).unwrap(),
            r#"mcp fx/echo {"text":"x"}"#
        );
        manager.shutdown().await;
    }

    #[test]
    fn exposed_names_are_sanitized_and_bounded() {
        assert_eq!(
            exposed_name("github", "create_issue"),
            "github__create_issue"
        );
        assert_eq!(exposed_name("fs", "read.file/v2"), "fs__read_file_v2");
        let long = exposed_name("server", &"x".repeat(100));
        assert_eq!(long.len(), 64);
        assert_ne!(long, exposed_name("server", &"x".repeat(101)));
    }

    #[test]
    fn schemas_become_object_schemas() {
        let schema = normalize_schema(&json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": {"q": {"type": "string"}},
            "required": ["q"]
        }));
        assert!(schema.get("$schema").is_none());
        assert_eq!(schema["required"], json!(["q"]));
        assert_eq!(normalize_schema(&json!(null))["properties"], json!({}));
    }

    #[test]
    fn results_keep_text_and_summarize_binary() {
        let result = convert_result(
            "s",
            "t",
            &json!({
                "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "image", "mimeType": "image/png", "data": "AAAA"},
                    {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}}
                ]
            }),
        );
        assert!(!result.is_error);
        assert!(result.content.starts_with("hello"));
        assert!(result.content.contains("[image omitted: image/png"));
        assert!(result.content.ends_with("body"));
    }

    #[test]
    fn structured_only_results_and_tool_errors() {
        let result = convert_result("s", "t", &json!({"structuredContent": {"n": 1}}));
        assert!(result.content.contains("\"n\": 1"));
        assert_eq!(result.details.unwrap()["structured"], json!({"n": 1}));

        let error = convert_result(
            "s",
            "t",
            &json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
        );
        assert!(error.is_error);
        assert!(error.content.starts_with("[MCP_TOOL_ERROR] boom"));
    }

    #[test]
    fn profiles_reference_servers_by_prefix_or_mcp_entry() {
        let mut config = Config::builtin();
        config.mcp.insert(
            "github".into(),
            McpServerConfig {
                command: "x".into(),
                ..Default::default()
            },
        );
        config.mcp.insert(
            "docs".into(),
            McpServerConfig {
                command: "y".into(),
                ..Default::default()
            },
        );
        config.profile_name = "minimal".into();
        config
            .profiles
            .get_mut("minimal")
            .unwrap()
            .tools
            .extend(["mcp:docs".to_string(), "github__create_issue".to_string()]);
        assert_eq!(referenced_servers(&config), vec!["docs", "github"]);
    }
}

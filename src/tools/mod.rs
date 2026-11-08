//! Built-in tool surface: 17 static tools in three packs, plus the registry.

pub mod bash;
pub mod bg;
pub mod edit;
pub mod finding;
pub mod glob;
pub mod grep;
pub mod iocs;
pub mod lsp;
pub mod patch;
pub mod read;
pub mod recall;
pub mod review;
pub mod test;
pub mod thread;
pub mod todo;
pub mod verify;
pub mod write;

pub use bg::BgRegistry;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::{ProfileConfig, VerifyConfig};
use crate::paths::Paths;
use crate::provider::ToolSpec;
use crate::security::SecurityState;
use crate::session::{ArtifactStore, Session};

pub type SessionHandle = Arc<Mutex<Session>>;

/// Which pack a tool belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPack {
    Core,
    Coding,
    Security,
}

#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    pub content: String,
    pub is_error: bool,
    pub details: Option<Value>,
}

impl ToolResult {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            details: None,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn err(code: &str, message: impl Into<String>) -> Self {
        Self {
            content: format!("[{code}] {}", message.into()),
            is_error: true,
            details: Some(json!({"code": code})),
        }
    }
}

/// Everything a tool may touch. Cloning is cheap; fields are shared.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    pub path_jail: bool,
    pub session: SessionHandle,
    pub artifacts: Arc<ArtifactStore>,
    pub cancel: CancellationToken,
    pub trust: bool,
    pub verify: VerifyConfig,
    pub profile: String,
    pub reads: Arc<Mutex<ReadTracker>>,
    pub bg: Arc<Mutex<bg::BgRegistry>>,
    pub security: Arc<Mutex<SecurityState>>,
    pub paths: Paths,
}

impl ToolContext {
    /// Resolve a path against the working directory and enforce the jail.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, ToolResult> {
        if path.trim().is_empty() {
            return Err(ToolResult::err(
                "invalid_args",
                "path is empty; pass the file path, for example {\"path\":\"src/main.rs\"}",
            ));
        }
        let joined = if PathBuf::from(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.cwd.join(path)
        };
        let normalized = normalize(&joined);
        if self.path_jail && !normalized.starts_with(&self.cwd) {
            return Err(ToolResult::err(
                "path_jail",
                format!(
                    "{} is outside the working directory; --path-jail is active. Use a path under {} or restart without --path-jail.",
                    normalized.display(),
                    self.cwd.display()
                ),
            ));
        }
        Ok(normalized)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }
}

/// Lexically normalize `.` and `..` without touching the filesystem.
pub fn normalize(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Stat stamps captured by `read`, checked by `edit`/`write` guards.
#[derive(Debug, Default, Clone)]
pub struct ReadTracker {
    stamps: HashMap<PathBuf, FileStamp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime: SystemTime,
    pub len: u64,
}

impl FileStamp {
    /// Stat a path, returning `None` when it does not exist.
    pub fn stamp_for(path: &std::path::Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            mtime: meta.modified().ok()?,
            len: meta.len(),
        })
    }
}

impl ReadTracker {
    pub fn record(&mut self, path: &std::path::Path, stamp: FileStamp) {
        self.stamps.insert(path.to_path_buf(), stamp);
    }

    pub fn stamp_for(path: &std::path::Path) -> Option<FileStamp> {
        let meta = std::fs::metadata(path).ok()?;
        Some(FileStamp {
            mtime: meta.modified().ok()?,
            len: meta.len(),
        })
    }

    pub fn is_current(&self, path: &std::path::Path) -> bool {
        match (self.stamps.get(path), Self::stamp_for(path)) {
            (Some(known), Some(actual)) => *known == actual,
            _ => false,
        }
    }

    pub fn clear(&mut self) {
        self.stamps.clear();
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &'static str;
    fn description(&self) -> &'static str;
    /// JSON Schema for the arguments object.
    fn schema(&self) -> Value;
    fn pack(&self) -> ToolPack;
    /// Read-only tools never need the read-before-edit stamp.
    fn read_only(&self) -> bool {
        false
    }
    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult;
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name() == tool.name()) {
            *slot = tool;
        } else {
            self.tools.push(tool);
        }
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    pub fn all(&self) -> &[Arc<dyn Tool>] {
        &self.tools
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// All built-in tools in canonical order.
    pub fn builtin() -> Self {
        let mut registry = Self::new();
        for tool in [
            Arc::new(read::ReadTool) as Arc<dyn Tool>,
            Arc::new(write::WriteTool),
            Arc::new(edit::EditTool),
            Arc::new(patch::PatchTool),
            Arc::new(bash::BashTool),
            Arc::new(grep::GrepTool),
            Arc::new(glob::GlobTool),
            Arc::new(todo::TodoTool),
            Arc::new(verify::VerifyTool),
            Arc::new(recall::RecallTool),
            Arc::new(thread::ThreadTool),
            Arc::new(bg::BgTool),
            Arc::new(lsp::LspTool::new()),
            Arc::new(test::TestTool),
            Arc::new(review::ReviewTool),
            Arc::new(finding::FindingTool),
            Arc::new(iocs::IocsTool),
        ] {
            registry.register(tool);
        }
        registry
    }

    /// Resolve packs plus explicit tool names. Unknown names are errors that
    /// name the available tools.
    pub fn select(
        &self,
        profile: &ProfileConfig,
        external: &[Arc<dyn Tool>],
    ) -> Result<Vec<Arc<dyn Tool>>, String> {
        let mut packs = Vec::new();
        for pack in &profile.packs {
            let pack = match pack.as_str() {
                "core" => ToolPack::Core,
                "coding" => ToolPack::Coding,
                "security" => ToolPack::Security,
                other => {
                    return Err(format!(
                        "unknown pack {other:?}; packs are core, coding, security"
                    ));
                }
            };
            packs.push(pack);
        }
        let mut selected: Vec<Arc<dyn Tool>> = Vec::new();
        for pack in packs {
            for tool in &self.tools {
                if tool.pack() == pack && !selected.iter().any(|t| t.name() == tool.name()) {
                    selected.push(tool.clone());
                }
            }
        }
        for name in &profile.tools {
            match self.get(name) {
                Some(tool) => {
                    if !selected.iter().any(|t| t.name() == tool.name()) {
                        selected.push(tool);
                    }
                }
                None => {
                    return Err(format!(
                        "unknown tool {name:?}; active built-ins: {}",
                        self.names().join(", ")
                    ));
                }
            }
        }
        for tool in external {
            if !selected.iter().any(|t| t.name() == tool.name()) {
                selected.push(tool.clone());
            }
        }
        Ok(selected)
    }

    pub fn specs(tools: &[Arc<dyn Tool>]) -> Vec<ToolSpec> {
        tools
            .iter()
            .map(|tool| ToolSpec {
                name: tool.name().to_string(),
                description: tool.description().to_string(),
                parameters: tool.schema(),
            })
            .collect()
    }
}

/// JSON Schema helper for a flat object.
pub fn object_schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// Read a required string argument.
pub fn required_str(args: &Value, key: &str) -> Result<String, ToolResult> {
    match args.get(key).and_then(|v| v.as_str()) {
        Some(value) if !value.is_empty() => Ok(value.to_string()),
        Some(_) => Err(ToolResult::err(
            "invalid_args",
            format!("\"{key}\" is empty; pass a non-empty string"),
        )),
        None => Err(ToolResult::err(
            "invalid_args",
            format!("missing required argument \"{key}\""),
        )),
    }
}

pub fn optional_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

pub fn optional_u64(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(|v| v.as_u64())
}

pub fn optional_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(|v| v.as_bool())
}

/// Format a path relative to the working directory for display.
pub fn display_path(cwd: &std::path::Path, path: &std::path::Path) -> String {
    path.strip_prefix(cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

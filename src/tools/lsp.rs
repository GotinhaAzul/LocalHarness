//! `lsp` tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

pub struct LspTool;

impl LspTool {
    pub fn new() -> Self { Self }

    pub fn default() -> Self { Self }
}

impl Default for LspTool { fn default() -> Self { Self::new() } }

#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &'static str {
        "lsp"
    }

    fn description(&self) -> &'static str {
        "Language-server outline, definitions, references, diagnostics, hover."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"action":{"type":"string","enum":["outline","definition","references","diagnostics","hover"]},"path":{"type":"string"},"line":{"type":"integer"},"column":{"type":"integer"},"symbol":{"type":"string"}},"required":["action","path"],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Coding
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _ctx: &ToolContext, _args: Value) -> ToolResult {
        ToolResult::err("not_implemented", "lsp is not implemented yet")
    }
}

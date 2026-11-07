//! `glob` tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

pub struct GlobTool;

impl GlobTool {
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &'static str {
        "glob"
    }

    fn description(&self) -> &'static str {
        "Find files by glob pattern."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _ctx: &ToolContext, _args: Value) -> ToolResult {
        ToolResult::err("not_implemented", "glob is not implemented yet")
    }
}

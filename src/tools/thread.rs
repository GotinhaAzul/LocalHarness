//! `thread` tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

pub struct ThreadTool;

impl ThreadTool {
}

#[async_trait]
impl Tool for ThreadTool {
    fn name(&self) -> &'static str {
        "thread"
    }

    fn description(&self) -> &'static str {
        "Split, note, close, or inspect the active thread."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"action":{"type":"string","enum":["split","note","close","status"]},"title":{"type":"string"},"objective":{"type":"string"},"note":{"type":"string"}},"required":["action"],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, _ctx: &ToolContext, _args: Value) -> ToolResult {
        ToolResult::err("not_implemented", "thread is not implemented yet")
    }
}

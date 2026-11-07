//! `review` tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

pub struct ReviewTool;

impl ReviewTool {
}

#[async_trait]
impl Tool for ReviewTool {
    fn name(&self) -> &'static str {
        "review"
    }

    fn description(&self) -> &'static str {
        "Review working-tree changes and risks."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"staged":{"type":"boolean"},"lines":{"type":"integer"}},"required":[],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Coding
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _ctx: &ToolContext, _args: Value) -> ToolResult {
        ToolResult::err("not_implemented", "review is not implemented yet")
    }
}

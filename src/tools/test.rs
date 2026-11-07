//! `test` tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

pub struct TestTool;

impl TestTool {
}

#[async_trait]
impl Tool for TestTool {
    fn name(&self) -> &'static str {
        "test"
    }

    fn description(&self) -> &'static str {
        "Run one test file or one test; first failure only."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"path":{"type":"string"},"test":{"type":"string"},"timeout":{"type":"integer"}},"required":[],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Coding
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, _ctx: &ToolContext, _args: Value) -> ToolResult {
        ToolResult::err("not_implemented", "test is not implemented yet")
    }
}

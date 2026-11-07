//! `iocs` tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

pub struct IocsTool;

impl IocsTool {
}

#[async_trait]
impl Tool for IocsTool {
    fn name(&self) -> &'static str {
        "iocs"
    }

    fn description(&self) -> &'static str {
        "Extract and defang indicators of compromise."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"text":{"type":"string"},"path":{"type":"string"},"defang":{"type":"boolean"}},"required":[],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Security
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, _ctx: &ToolContext, _args: Value) -> ToolResult {
        ToolResult::err("not_implemented", "iocs is not implemented yet")
    }
}

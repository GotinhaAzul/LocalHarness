//! `finding` tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

pub struct FindingTool;

impl FindingTool {
}

#[async_trait]
impl Tool for FindingTool {
    fn name(&self) -> &'static str {
        "finding"
    }

    fn description(&self) -> &'static str {
        "Engagement findings ledger: add, update, list, report."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"action":{"type":"string","enum":["add","update","list","report"]},"id":{"type":"string"},"title":{"type":"string"},"severity":{"type":"string","enum":["critical","high","medium","low","info"]},"status":{"type":"string","enum":["open","fixed","accepted","false-positive"]},"target":{"type":"string"},"evidence":{"type":"string"},"impact":{"type":"string"},"remediation":{"type":"string"}},"required":["action"],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Security
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, _ctx: &ToolContext, _args: Value) -> ToolResult {
        ToolResult::err("not_implemented", "finding is not implemented yet")
    }
}

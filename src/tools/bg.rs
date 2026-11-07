//! `bg` tool.

use async_trait::async_trait;
use serde_json::Value;

use crate::tools::{Tool, ToolContext, ToolPack, ToolResult};

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

/// Live background processes, owned by the harness and shared with the tool.
#[derive(Debug, Default)]
pub struct BgRegistry {
    pub processes: HashMap<String, BgProcess>,
    pub next_id: u32,
}

#[derive(Debug)]
pub struct BgProcess {
    pub id: String,
    pub command: String,
    pub child: tokio::process::Child,
    pub log_path: PathBuf,
    pub started: Instant,
}

impl BgRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn shutdown(&mut self) {
        for (_, mut process) in self.processes.drain() {
            let _ = process.child.kill().await;
        }
    }
}

pub struct BgTool;

impl BgTool {
}

#[async_trait]
impl Tool for BgTool {
    fn name(&self) -> &'static str {
        "bg"
    }

    fn description(&self) -> &'static str {
        "Start and supervise background processes."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"action":{"type":"string","enum":["start","status","logs","stop"]},"command":{"type":"string"},"id":{"type":"string"},"lines":{"type":"integer"}},"required":["action"],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, _ctx: &ToolContext, _args: Value) -> ToolResult {
        ToolResult::err("not_implemented", "bg is not implemented yet")
    }
}

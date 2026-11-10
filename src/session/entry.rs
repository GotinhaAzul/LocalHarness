//! Session entry types and the on-disk JSONL schema.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const FORMAT_VERSION: u32 = 1;

/// One line of a session file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub v: u32,
    pub seq: u64,
    pub id: String,
    pub parent: Option<String>,
    pub ts: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: EntryKind,
}

impl Entry {
    pub fn is_checkout(&self) -> bool {
        matches!(self.kind, EntryKind::Checkout(_))
    }

    pub fn is_tool_result(&self) -> bool {
        matches!(self.kind, EntryKind::ToolResult(_))
    }

    pub fn is_compaction(&self) -> bool {
        matches!(self.kind, EntryKind::Compaction(_))
    }

    pub fn is_assistant(&self) -> bool {
        matches!(self.kind, EntryKind::Assistant(_))
    }

    /// True when the assistant entry asks for tools.
    pub fn has_tool_calls(&self) -> bool {
        matches!(&self.kind, EntryKind::Assistant(a) if a.tool_calls.as_ref().is_some_and(|c| !c.is_empty()))
    }

    pub fn assistant(&self) -> Option<&AssistantData> {
        match &self.kind {
            EntryKind::Assistant(a) => Some(a),
            _ => None,
        }
    }

    pub fn tool_result(&self) -> Option<&ToolResultData> {
        match &self.kind {
            EntryKind::ToolResult(t) => Some(t),
            _ => None,
        }
    }

    /// One-line role label for the tree UI.
    pub fn role_char(&self) -> char {
        match &self.kind {
            EntryKind::Session(_) => 'S',
            EntryKind::User(_) => 'U',
            EntryKind::Assistant(_) => 'A',
            EntryKind::ToolResult(_) => 'R',
            EntryKind::Compaction(_) => 'C',
            EntryKind::Checkout(_) => 'K',
            EntryKind::Custom(_) => 'M',
            EntryKind::Error(_) => 'E',
        }
    }

    /// Short human preview, at most `max` characters.
    pub fn preview(&self, max: usize) -> String {
        let text = match &self.kind {
            EntryKind::Session(s) => format!("{} · {}", s.model, s.profile),
            EntryKind::User(u) => u.content.clone(),
            EntryKind::Assistant(a) => match (&a.content, a.tool_calls.as_ref()) {
                (Some(c), _) if !c.trim().is_empty() => c.clone(),
                (_, Some(calls)) if !calls.is_empty() => {
                    let names: Vec<&str> = calls.iter().map(|c| c.name.as_str()).collect();
                    format!("[{}]", names.join(", "))
                }
                _ => "(empty)".into(),
            },
            EntryKind::ToolResult(t) => t.content.clone(),
            EntryKind::Compaction(c) => format!("compaction: {}", c.reason),
            EntryKind::Checkout(c) => format!("checkout {}", c.target),
            EntryKind::Custom(c) => format!("{} {:?}", c.kind, c.payload),
            EntryKind::Error(e) => format!("{}: {}", e.code, e.message),
        };
        let mut out: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if out.chars().count() > max {
            out = out.chars().take(max.saturating_sub(1)).collect::<String>() + "…";
        }
        out
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum EntryKind {
    Session(SessionData),
    User(UserData),
    Assistant(AssistantData),
    ToolResult(ToolResultData),
    Compaction(CompactionData),
    Checkout(CheckoutData),
    Custom(CustomData),
    Error(ErrorData),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionData {
    pub cwd: String,
    pub endpoint: String,
    pub model: String,
    pub profile: String,
    pub app: String,
    /// Context window resolution at session start: trained maximum, server
    /// allocation, per-slot budget, and the effective minimum. Absent in
    /// session files written before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<SessionContextWindow>,
}

/// The four context-window numbers recorded in a session header.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionContextWindow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trained: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allocated: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub per_slot: Option<u32>,
    pub effective: u32,
    /// Where each value came from, keyed `trained`, `allocated`, `per_slot`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UserData {
    pub content: String,
    /// Files the user mentioned with `@path`, snapshotted when the prompt
    /// was sent. The model sees them after `content`; the transcript shows
    /// only what the user typed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// One `@path` file snapshot attached to a user message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Path relative to the working directory, as displayed.
    pub path: String,
    /// Line-numbered text, exactly as the `read` tool would return it.
    pub content: String,
}

impl UserData {
    /// The message the model sees: the prompt followed by any attachments.
    pub fn model_content(&self) -> String {
        if self.attachments.is_empty() {
            return self.content.clone();
        }
        let mut text = self.content.clone();
        text.push_str("\n\n[files attached by the user with @mentions; they count as read]");
        for attachment in &self.attachments {
            text.push_str(&format!(
                "\n\n<file path=\"{}\">\n{}\n</file>",
                attachment.path, attachment.content
            ));
        }
        text
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AssistantData {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<crate::provider::Usage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultData {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionData {
    pub summary: String,
    pub tokens_before: u64,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutData {
    pub target: String,
    pub reason: String,
}

/// Custom entries flatten their payload next to `kind`, e.g.
/// `{"type":"custom","data":{"kind":"fs-change","path":"src/lib.rs"}}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomData {
    pub kind: String,
    #[serde(flatten)]
    pub payload: Map<String, Value>,
}

impl CustomData {
    pub fn new(kind: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            payload: Map::new(),
        }
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.payload.insert(key.to_string(), value.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorData {
    pub code: String,
    pub message: String,
    pub recoverable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_roundtrips_flat_json() {
        let entry = Entry {
            v: FORMAT_VERSION,
            seq: 1,
            id: "e1".into(),
            parent: None,
            ts: Utc::now(),
            kind: EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "Fix the parser bug".into(),
            }),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["type"], "user");
        assert_eq!(json["data"]["content"], "Fix the parser bug");
        let back: Entry = serde_json::from_value(json).unwrap();
        assert!(matches!(back.kind, EntryKind::User(_)));
    }

    #[test]
    fn custom_payload_is_flat() {
        let entry = Entry {
            v: 1,
            seq: 2,
            id: "e2".into(),
            parent: Some("e1".into()),
            ts: Utc::now(),
            kind: EntryKind::Custom(CustomData::new("fs-change").with("path", "src/lib.rs")),
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["data"]["kind"], "fs-change");
        assert_eq!(json["data"]["path"], "src/lib.rs");
        let back: Entry = serde_json::from_value(json).unwrap();
        match back.kind {
            EntryKind::Custom(c) => assert_eq!(c.payload["path"], "src/lib.rs"),
            _ => panic!("wrong type"),
        }
    }

    fn session_entry(context: Option<SessionContextWindow>) -> Entry {
        Entry {
            v: FORMAT_VERSION,
            seq: 0,
            id: "s1".into(),
            parent: None,
            ts: Utc::now(),
            kind: EntryKind::Session(SessionData {
                cwd: "/tmp".into(),
                endpoint: "http://127.0.0.1:8080".into(),
                model: "qwen3".into(),
                profile: "full".into(),
                app: "test".into(),
                context,
            }),
        }
    }

    #[test]
    fn old_session_header_without_context_still_loads() {
        let json = serde_json::json!({
            "cwd": "/tmp",
            "endpoint": "http://127.0.0.1:8080",
            "model": "qwen3",
            "profile": "full",
            "app": "0.1.0"
        });
        let data: SessionData = serde_json::from_value(json).unwrap();
        assert!(data.context.is_none());
        assert_eq!(data.model, "qwen3");
    }

    #[test]
    fn session_context_roundtrips_all_four_values() {
        let mut sources = std::collections::BTreeMap::new();
        sources.insert(
            "trained".to_string(),
            "/v1/models meta.n_ctx_train".to_string(),
        );
        sources.insert(
            "per_slot".to_string(),
            "llama.cpp /props n_ctx (per slot)".to_string(),
        );
        let entry = session_entry(Some(SessionContextWindow {
            trained: Some(131_072),
            allocated: Some(8192),
            per_slot: Some(4096),
            effective: 4096,
            sources: Some(sources),
        }));
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["data"]["context"]["trained"], 131_072);
        assert_eq!(json["data"]["context"]["allocated"], 8192);
        assert_eq!(json["data"]["context"]["per_slot"], 4096);
        assert_eq!(json["data"]["context"]["effective"], 4096);
        assert_eq!(
            json["data"]["context"]["sources"]["per_slot"],
            "llama.cpp /props n_ctx (per slot)"
        );
        let back: Entry = serde_json::from_value(json).unwrap();
        match back.kind {
            EntryKind::Session(data) => {
                assert_eq!(
                    data.context,
                    Some(SessionContextWindow {
                        trained: Some(131_072),
                        allocated: Some(8192),
                        per_slot: Some(4096),
                        effective: 4096,
                        sources: Some(
                            [
                                (
                                    "per_slot".to_string(),
                                    "llama.cpp /props n_ctx (per slot)".to_string()
                                ),
                                (
                                    "trained".to_string(),
                                    "/v1/models meta.n_ctx_train".to_string()
                                ),
                            ]
                            .into_iter()
                            .collect()
                        ),
                    })
                );
            }
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn new_session_header_omits_absent_context() {
        let json = serde_json::to_value(session_entry(None)).unwrap();
        assert!(json["data"].get("context").is_none());
    }
}

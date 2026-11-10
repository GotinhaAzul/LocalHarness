//! Provider wire types shared by the OpenAI-compatible client, the fake test
//! provider, and the agent loop.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Parsed JSON object. On the wire this is serialized back to a string.
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments object.
    pub parameters: Value,
}

/// The `tools` array exactly as an OpenAI-compatible request carries it.
/// Shared by the request body and the one-time startup tokenizer measurement.
pub fn tools_payload(tools: &[ToolSpec]) -> Value {
    Value::Array(
        tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    }
                })
            })
            .collect(),
    )
}

/// Serialized [`tools_payload`] for `/tokenize` and character accounting.
pub fn tools_json(tools: &[ToolSpec]) -> String {
    serde_json::to_string(&tools_payload(tools)).unwrap_or_default()
}

#[derive(Debug, Clone)]
pub enum ChatMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        content: String,
    },
}

impl ChatMessage {
    pub fn role(&self) -> &'static str {
        match self {
            ChatMessage::System { .. } => "system",
            ChatMessage::User { .. } => "user",
            ChatMessage::Assistant { .. } => "assistant",
            ChatMessage::Tool { .. } => "tool",
        }
    }

    /// Rough character count used before an exact tokenizer is available.
    pub fn char_len(&self) -> usize {
        match self {
            ChatMessage::System { content } | ChatMessage::User { content } => content.len(),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                content.as_ref().map(|c| c.len()).unwrap_or(0)
                    + tool_calls
                        .iter()
                        .map(|c| c.name.len() + c.arguments.to_string().len() + 24)
                        .sum::<usize>()
            }
            ChatMessage::Tool { content, .. } => content.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: Option<String>,
    /// Server-side hint; dispatch still enforces a profile's one-call rule.
    pub parallel_tool_calls: Option<bool>,
    pub temperature: f32,
    pub top_p: f32,
    pub max_tokens: u32,
    pub stop: Vec<String>,
    pub extra_body: Option<Value>,
    pub include_usage: bool,
    pub reasoning_field: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
    /// Prompt tokens served from the server's prefix cache, when reported
    /// (`prompt_tokens_details.cached_tokens`, or llama.cpp `timings.cache_n`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    /// Server time spent on the uncached part of the prompt (llama.cpp).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_ms: Option<u32>,
    /// Time from sending the request to the first streamed token, measured
    /// by the harness, so it is available from every engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_token_ms: Option<u32>,
    /// Generation speed in tokens per second.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_per_sec: Option<f32>,
    /// Engine-native truncation signal (llama.cpp `truncated`, KoboldCpp
    /// context shift). When true, the prompt was cut before evaluation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
    /// Engine-native count of context-shift events for this request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_shift: Option<u32>,
}

impl Usage {
    pub fn new(prompt: u32, completion: u32) -> Self {
        Self {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            ..Self::default()
        }
    }

    /// Fold in a later, possibly partial report: llama.cpp can send
    /// `timings` and `usage` in different chunks. Known values win.
    pub fn merge(&mut self, other: &Usage) {
        if other.prompt_tokens > 0 {
            self.prompt_tokens = other.prompt_tokens;
        }
        if other.completion_tokens > 0 {
            self.completion_tokens = other.completion_tokens;
        }
        if other.total_tokens > 0 {
            self.total_tokens = other.total_tokens;
        }
        self.cached_tokens = other.cached_tokens.or(self.cached_tokens);
        self.prompt_ms = other.prompt_ms.or(self.prompt_ms);
        self.first_token_ms = other.first_token_ms.or(self.first_token_ms);
        self.tokens_per_sec = other.tokens_per_sec.or(self.tokens_per_sec);
        self.truncated = other.truncated.or(self.truncated);
        self.context_shift = other.context_shift.or(self.context_shift);
    }

    /// Share of the prompt reused from the server cache, 0-100.
    pub fn cache_percent(&self) -> Option<u32> {
        let cached = self.cached_tokens?;
        (self.prompt_tokens > 0).then(|| {
            ((u64::from(cached.min(self.prompt_tokens)) * 100) / u64::from(self.prompt_tokens))
                as u32
        })
    }

    /// One compact line for the UI: `cache 94% · prompt 0.4s · first token
    /// 0.6s · 31 tok/s`. Empty when nothing was measured.
    pub fn perf_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(percent) = self.cache_percent() {
            parts.push(format!("cache {percent}%"));
        }
        if let Some(ms) = self.prompt_ms {
            parts.push(format!("prompt {}", seconds(ms)));
        }
        if let Some(ms) = self.first_token_ms {
            parts.push(format!("first token {}", seconds(ms)));
        }
        if let Some(rate) = self.tokens_per_sec {
            parts.push(format!("{rate:.0} tok/s"));
        }
        parts.join(" · ")
    }
}

fn seconds(ms: u32) -> String {
    format!("{:.1}s", f64::from(ms) / 1000.0)
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Delta(String),
    ReasoningDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    },
    Usage(Usage),
    Finish(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    Connection,
    Auth,
    ModelNotFound,
    ContextTooSmall,
    BadRequest,
    Server,
    Cancelled,
    Parse,
    /// The output limit cut a streamed tool call before its JSON closed. The
    /// call is dropped, never dispatched with partial arguments.
    ToolCallTruncated,
}

impl ErrorClass {
    pub fn code(&self) -> &'static str {
        match self {
            ErrorClass::Connection => "connection",
            ErrorClass::Auth => "auth",
            ErrorClass::ModelNotFound => "model_not_found",
            ErrorClass::ContextTooSmall => "context_too_small",
            ErrorClass::BadRequest => "bad_request",
            ErrorClass::Server => "server",
            ErrorClass::Cancelled => "cancelled",
            ErrorClass::Parse => "parse",
            ErrorClass::ToolCallTruncated => "tool_call_truncated",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ProviderError {
    pub class: ErrorClass,
    pub message: String,
    pub status: Option<u16>,
}

impl ProviderError {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            status: None,
        }
    }

    pub fn with_status(class: ErrorClass, status: u16, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            status: Some(status),
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ProviderError {}

pub type StreamItem = Result<StreamEvent, ProviderError>;

/// Redact every occurrence of a configured secret.
pub fn redact(mut message: String, secrets: &[&str]) -> String {
    for secret in secrets {
        if !secret.is_empty() {
            message = message.replace(secret, "[redacted]");
        }
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_usage_reports_merge_and_render() {
        let mut usage = Usage::new(1000, 0);
        let timings = Usage {
            cached_tokens: Some(940),
            prompt_ms: Some(420),
            tokens_per_sec: Some(31.4),
            ..Usage::default()
        };
        usage.merge(&timings);
        usage.merge(&Usage::new(0, 80));
        assert_eq!((usage.prompt_tokens, usage.completion_tokens), (1000, 80));
        usage.first_token_ms = Some(610);
        assert_eq!(
            usage.perf_line(),
            "cache 94% · prompt 0.4s · first token 0.6s · 31 tok/s"
        );
        assert_eq!(Usage::new(10, 2).perf_line(), "");
        // Old session files without the new fields still load.
        let old: Usage =
            serde_json::from_str(r#"{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}"#)
                .unwrap();
        assert_eq!(old.cached_tokens, None);
        assert!(!serde_json::to_string(&old).unwrap().contains("cached"));
    }

    #[test]
    fn usage_total_is_derived_in_constructor() {
        let usage = Usage::new(10, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn secrets_are_redacted() {
        let msg = "Authorization: Bearer sk-secret failed".to_string();
        assert!(!redact(msg, &["sk-secret"]).contains("sk-secret"));
    }
}

//! Provider abstraction. One implementation: OpenAI-compatible HTTP.

pub mod openai;
pub mod probe;
pub mod sse;
pub mod types;

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

pub use openai::OpenAiCompatProvider;
pub use types::{
    ChatMessage, ChatRequest, ErrorClass, ImageData, ProviderError, StreamEvent, StreamItem,
    ToolCall, ToolSpec, Usage, redact, tools_json, tools_payload,
};

/// Server root and OpenAI API base derived from a configured endpoint.
///
/// Accepts both `http://host:8080` and `http://host:8080/v1`; the `/v1` suffix
/// is optional so users can paste the base URL their engine prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointUrls {
    /// Server root: llama.cpp `/props` and `/tokenize`, Ollama `/api/show`.
    pub root: String,
    /// OpenAI API base: `/chat/completions` and `/models` live here.
    pub api: String,
}

pub fn endpoint_urls(endpoint: &str) -> EndpointUrls {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let root = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    let root = root.trim_end_matches('/').to_string();
    EndpointUrls {
        api: format!("{root}/v1"),
        root,
    }
}

/// A chat-completions stream. Events are pushed to `tx`; the stream ends when
/// the provider closes the channel. Cancellation is cooperative.
#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat_stream(
        &self,
        req: ChatRequest,
        tx: UnboundedSender<StreamItem>,
        cancel: CancellationToken,
    );

    fn endpoint(&self) -> &str;
    fn model(&self) -> &str;

    /// Exposed for optional capability probes; fake providers return `None`.
    fn client(&self) -> Option<&reqwest::Client> {
        None
    }

    fn api_key(&self) -> &str {
        ""
    }
}

/// Convenience wrapper for a one-shot request with a fake or real provider.
pub async fn collect_stream(
    provider: &dyn Provider,
    req: ChatRequest,
    cancel: CancellationToken,
) -> Vec<StreamItem> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let fut = provider.chat_stream(req, tx, cancel);
    tokio::pin!(fut);
    let mut events = Vec::new();
    loop {
        tokio::select! {
            _ = fut.as_mut() => {
                while let Ok(item) = rx.try_recv() {
                    events.push(item);
                }
                break;
            }
            item = rx.recv() => {
                match item {
                    Some(item) => events.push(item),
                    None => {
                        fut.as_mut().await;
                        break;
                    }
                }
            }
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_accepts_plain_and_v1_forms() {
        for endpoint in [
            "http://127.0.0.1:8080",
            "http://127.0.0.1:8080/",
            "http://127.0.0.1:8080/v1",
            "http://127.0.0.1:8080/v1/",
        ] {
            let urls = endpoint_urls(endpoint);
            assert_eq!(urls.root, "http://127.0.0.1:8080", "{endpoint}");
            assert_eq!(urls.api, "http://127.0.0.1:8080/v1", "{endpoint}");
        }
    }

    #[test]
    fn endpoint_keeps_a_custom_prefix() {
        let urls = endpoint_urls("http://host:9000/openai/v1");
        assert_eq!(urls.root, "http://host:9000/openai");
        assert_eq!(urls.api, "http://host:9000/openai/v1");
    }
}

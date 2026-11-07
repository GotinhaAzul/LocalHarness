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
    ChatMessage, ChatRequest, ErrorClass, ProviderError, StreamEvent, StreamItem, ToolCall,
    ToolSpec, Usage, redact,
};

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

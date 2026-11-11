//! Streamable HTTP transport (MCP 2025-03-26 and later): each JSON-RPC
//! message is one POST; the server answers with JSON or an SSE stream that
//! carries the response. The `Mcp-Session-Id` header is echoed when issued.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::transport::{
    Incoming, McpError, Transport, answer_server_request, cancelled_notification, classify,
    notification_message, request_message,
};
use crate::provider::sse::{SseFrame, SseParser};

const SESSION_HEADER: &str = "mcp-session-id";
const VERSION_HEADER: &str = "mcp-protocol-version";
/// A response body larger than this is refused.
const BODY_CAP: usize = 16 * 1024 * 1024;
/// Only this much of an HTTP error body is kept for the message.
const ERROR_BODY_CAP: usize = 4 * 1024;
/// Notifications are fire-and-forget; a server that stalls on one must not
/// hold a timed-out or cancelled call open.
const NOTIFICATION_TIMEOUT: Duration = Duration::from_secs(2);

pub struct HttpTransport {
    name: String,
    url: String,
    client: reqwest::Client,
    headers: HeaderMap,
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
    next_id: AtomicU64,
    alive: AtomicBool,
}

impl HttpTransport {
    pub fn new(
        name: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
    ) -> Result<Self, McpError> {
        let parsed = reqwest::Url::parse(url).map_err(|error| {
            McpError::Other(format!("[mcp.{name}] url {url:?} is invalid: {error}"))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(McpError::Other(format!(
                "[mcp.{name}] url must use http or https"
            )));
        }
        let mut map = HeaderMap::new();
        for (key, value) in headers {
            let key = HeaderName::from_bytes(key.as_bytes()).map_err(|error| {
                McpError::Other(format!("[mcp.{name}] header {key:?} is invalid: {error}"))
            })?;
            let mut value = HeaderValue::from_str(value).map_err(|_| {
                McpError::Other(format!("[mcp.{name}] header {key} has an invalid value"))
            })?;
            value.set_sensitive(true);
            map.insert(key, value);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| McpError::Other(format!("cannot build an HTTP client: {error}")))?;
        Ok(Self {
            name: name.to_string(),
            url: url.to_string(),
            client,
            headers: map,
            session: Mutex::new(None),
            version: Mutex::new(None),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
        })
    }

    fn builder(&self, method: reqwest::Method) -> reqwest::RequestBuilder {
        let mut request = self
            .client
            .request(method, &self.url)
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream");
        if let Some(session) = lock(&self.session).clone() {
            request = request.header(SESSION_HEADER, session);
        }
        if let Some(version) = lock(&self.version).clone() {
            request = request.header(VERSION_HEADER, version);
        }
        request
    }

    async fn post(&self, message: &Value) -> Result<reqwest::Response, McpError> {
        let response = self
            .builder(reqwest::Method::POST)
            .header(CONTENT_TYPE, "application/json")
            .json(message)
            .send()
            .await
            .map_err(|error| {
                McpError::Other(format!(
                    "cannot reach MCP server `{}` at {}: {error}",
                    self.name, self.url
                ))
            })?;
        if let Some(session) = response
            .headers()
            .get(SESSION_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            *lock(&self.session) = Some(session.to_string());
        }
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND && lock(&self.session).is_some() {
            // The spec's signal that the session expired: start a new one.
            self.alive.store(false, Ordering::SeqCst);
            return Err(McpError::Closed(format!(
                "MCP server `{}` ended the session",
                self.name
            )));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(McpError::Other(format!(
                "MCP server `{}` refused the request with HTTP {status}; set an `Authorization` entry under [mcp.{}.headers]",
                self.name, self.name
            )));
        }
        if !status.is_success() {
            let (body, _) = collect_capped(response.bytes_stream(), ERROR_BODY_CAP)
                .await
                .map_err(|error| {
                    McpError::Other(format!("MCP server `{}` body failed: {error}", self.name))
                })?;
            let body: String = String::from_utf8_lossy(&body).chars().take(500).collect();
            return Err(McpError::Other(format!(
                "MCP server `{}` returned HTTP {status}: {}",
                self.name,
                body.trim()
            )));
        }
        Ok(response)
    }

    /// Read the response for request `id` from a JSON or SSE body.
    async fn read_response(&self, response: reqwest::Response, id: u64) -> Result<Value, McpError> {
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.starts_with("text/event-stream") {
            let mut parser = SseParser::new();
            let mut stream = response.bytes_stream();
            let mut seen = 0usize;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|error| {
                    McpError::Other(format!("MCP server `{}` stream failed: {error}", self.name))
                })?;
                seen += chunk.len();
                if seen > BODY_CAP {
                    return Err(McpError::Other(format!(
                        "MCP server `{}` sent more than {} MiB for one response",
                        self.name,
                        BODY_CAP / (1024 * 1024)
                    )));
                }
                for frame in parser.push(&chunk) {
                    if let Some(result) = self.handle_frame(frame, id).await {
                        return result;
                    }
                }
            }
            for frame in parser.finish() {
                if let Some(result) = self.handle_frame(frame, id).await {
                    return result;
                }
            }
            return Err(McpError::Other(format!(
                "MCP server `{}` closed the stream without answering",
                self.name
            )));
        }

        let (bytes, over_cap) = collect_capped(response.bytes_stream(), BODY_CAP)
            .await
            .map_err(|error| {
                McpError::Other(format!("MCP server `{}` body failed: {error}", self.name))
            })?;
        if over_cap {
            return Err(McpError::Other(format!(
                "MCP server `{}` response is over the {} MiB size cap",
                self.name,
                BODY_CAP / (1024 * 1024)
            )));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            McpError::Other(format!(
                "MCP server `{}` returned invalid JSON: {error}",
                self.name
            ))
        })?;
        let messages = match value {
            Value::Array(items) => items,
            other => vec![other],
        };
        for message in messages {
            if let Incoming::Response { id: got, outcome } = classify(message)
                && got == id
            {
                return outcome;
            }
        }
        Err(McpError::Other(format!(
            "MCP server `{}` answered without a response for request {id}",
            self.name
        )))
    }

    async fn handle_frame(&self, frame: SseFrame, id: u64) -> Option<Result<Value, McpError>> {
        let SseFrame::Data(data) = frame else {
            return None;
        };
        let message: Value = serde_json::from_str(&data).ok()?;
        match classify(message) {
            Incoming::Response { id: got, outcome } if got == id => Some(outcome),
            Incoming::Request { id, method } => {
                let answer = answer_server_request(id, &method);
                let _ = self.post(&answer).await;
                None
            }
            _ => None,
        }
    }
}

#[async_trait]
impl Transport for HttpTransport {
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let message = request_message(id, method, &params);
        let exchange = async {
            let response = self.post(&message).await?;
            self.read_response(response, id).await
        };
        tokio::select! {
            outcome = exchange => outcome,
            _ = tokio::time::sleep(timeout) => {
                let (method, params) = cancelled_notification(id, "timed out");
                let _ = self.notify(method, params).await;
                Err(McpError::Timeout(timeout))
            }
            _ = cancel.cancelled() => {
                let (method, params) = cancelled_notification(id, "cancelled by the user");
                let _ = self.notify(method, params).await;
                Err(McpError::Cancelled)
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let message = notification_message(method, &params);
        match tokio::time::timeout(NOTIFICATION_TIMEOUT, self.post(&message)).await {
            Ok(result) => result.map(|_| ()),
            Err(_) => Err(McpError::Other(format!(
                "MCP server `{}` did not accept `{method}` within {}s",
                self.name,
                NOTIFICATION_TIMEOUT.as_secs()
            ))),
        }
    }

    fn set_protocol_version(&self, version: &str) {
        *lock(&self.version) = Some(version.to_string());
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    async fn close(&self) {
        self.alive.store(false, Ordering::SeqCst);
        if lock(&self.session).is_some() {
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                self.builder(reqwest::Method::DELETE).send(),
            )
            .await;
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Read a body stream up to `limit` bytes. The bool reports that more arrived,
/// so a large JSON body is rejected while an error message keeps its prefix.
/// Reading stops at the cap instead of buffering the rest of the response.
async fn collect_capped<S, B, E>(mut chunks: S, limit: usize) -> Result<(Vec<u8>, bool), E>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
{
    let mut body = Vec::new();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        let chunk = chunk.as_ref();
        let room = limit.saturating_sub(body.len());
        if chunk.len() > room {
            body.extend_from_slice(&chunk[..room]);
            return Ok((body, true));
        }
        body.extend_from_slice(chunk);
    }
    Ok((body, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capped_body_stops_once_the_limit_is_crossed() {
        let chunks = futures_util::stream::iter(vec![
            Ok::<&[u8], std::io::Error>(b"12345"),
            Ok(b"67890"),
            Ok(b"never read"),
        ]);
        let (body, over) = collect_capped(chunks, 8).await.unwrap();
        assert_eq!(body, b"12345678");
        assert!(over);
    }

    #[tokio::test]
    async fn capped_body_reads_a_complete_response() {
        let chunks = futures_util::stream::iter(vec![
            Ok::<&[u8], std::io::Error>(b"{\"ok\":"),
            Ok(b"true}"),
        ]);
        let (body, over) = collect_capped(chunks, 64).await.unwrap();
        assert_eq!(body, b"{\"ok\":true}");
        assert!(!over);
    }
}

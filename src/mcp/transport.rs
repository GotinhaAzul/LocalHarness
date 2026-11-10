//! Transport abstraction shared by the stdio and Streamable HTTP clients.
//!
//! A transport moves JSON-RPC messages. It does not know MCP methods; the
//! client in `client.rs` performs the handshake and tool calls on top of it.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A failed request, classified so tools can name the next action.
#[derive(Debug, Clone, thiserror::Error)]
pub enum McpError {
    /// The server answered with a JSON-RPC error object.
    #[error("server error {code}: {message}")]
    Rpc { code: i64, message: String },
    #[error("timed out after {0:?}")]
    Timeout(Duration),
    #[error("cancelled")]
    Cancelled,
    /// The connection was gone before the request was delivered (process not
    /// running, stdin closed, session expired), so retrying on a fresh
    /// connection cannot run anything twice. The client does that once.
    #[error("{0}")]
    Closed(String),
    /// Anything else: spawn failure, HTTP status, malformed message.
    #[error("{0}")]
    Other(String),
}

impl McpError {
    pub fn is_closed(&self) -> bool {
        matches!(self, McpError::Closed(_))
    }
}

#[async_trait]
pub trait Transport: Send + Sync {
    /// Send one request and wait for the matching response's `result`.
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Value, McpError>;

    /// Send one notification; no response is expected.
    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError>;

    /// Called after a successful `initialize` with the negotiated version.
    fn set_protocol_version(&self, _version: &str) {}

    /// False once the connection can no longer carry requests.
    fn is_alive(&self) -> bool;

    /// Recent server diagnostics (stdio stderr), for error messages.
    fn diagnostics(&self) -> String {
        String::new()
    }

    /// Release the connection: terminate the process or end the HTTP session.
    async fn close(&self);
}

/// Encode a JSON-RPC request.
pub fn request_message(id: u64, method: &str, params: &Value) -> Value {
    if params.is_null() {
        json!({"jsonrpc": "2.0", "id": id, "method": method})
    } else {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }
}

/// Encode a JSON-RPC notification.
pub fn notification_message(method: &str, params: &Value) -> Value {
    if params.is_null() {
        json!({"jsonrpc": "2.0", "method": method})
    } else {
        json!({"jsonrpc": "2.0", "method": method, "params": params})
    }
}

/// Classify an incoming message.
pub enum Incoming {
    /// A response to one of our requests.
    Response {
        id: u64,
        outcome: Result<Value, McpError>,
    },
    /// A request from the server that needs an answer.
    Request { id: Value, method: String },
    /// A server notification, or a message we cannot use.
    Other,
}

pub fn classify(message: Value) -> Incoming {
    let Value::Object(mut map) = message else {
        return Incoming::Other;
    };
    let method = map
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_string);
    let id = map.remove("id");
    match (method, id) {
        (Some(method), Some(id)) if !id.is_null() => Incoming::Request { id, method },
        (None, Some(id)) => {
            // Our ids are numbers, but tolerate servers that echo them as strings.
            let id = match &id {
                Value::Number(n) => n.as_u64(),
                Value::String(s) => s.parse().ok(),
                _ => None,
            };
            let Some(id) = id else {
                return Incoming::Other;
            };
            let outcome = if let Some(error) = map.remove("error") {
                Err(McpError::Rpc {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                })
            } else {
                Ok(map.remove("result").unwrap_or(Value::Null))
            };
            Incoming::Response { id, outcome }
        }
        _ => Incoming::Other,
    }
}

/// The answer to a server-initiated request. Only `ping` is supported: the
/// client declares no capabilities (roots, sampling, elicitation), so anything
/// else is "method not found".
pub fn answer_server_request(id: Value, method: &str) -> Value {
    if method == "ping" {
        json!({"jsonrpc": "2.0", "id": id, "result": {}})
    } else {
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": format!("method {method} is not supported by this client")}
        })
    }
}

/// The `notifications/cancelled` message for an abandoned request.
pub fn cancelled_notification(id: u64, reason: &str) -> (&'static str, Value) {
    (
        "notifications/cancelled",
        json!({"requestId": id, "reason": reason}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_responses_requests_and_notifications() {
        match classify(json!({"jsonrpc":"2.0","id":3,"result":{"ok":true}})) {
            Incoming::Response { id, outcome } => {
                assert_eq!(id, 3);
                assert_eq!(outcome.unwrap(), json!({"ok": true}));
            }
            _ => panic!("expected a response"),
        }
        match classify(json!({"jsonrpc":"2.0","id":"4","error":{"code":-32602,"message":"bad"}})) {
            Incoming::Response { id, outcome } => {
                assert_eq!(id, 4);
                assert!(matches!(outcome, Err(McpError::Rpc { code: -32602, .. })));
            }
            _ => panic!("expected an error response"),
        }
        assert!(matches!(
            classify(json!({"jsonrpc":"2.0","id":"s1","method":"ping"})),
            Incoming::Request { .. }
        ));
        assert!(matches!(
            classify(json!({"jsonrpc":"2.0","method":"notifications/progress"})),
            Incoming::Other
        ));
    }

    #[test]
    fn only_ping_is_answered_successfully() {
        assert_eq!(answer_server_request(json!(1), "ping")["result"], json!({}));
        assert_eq!(
            answer_server_request(json!(2), "sampling/createMessage")["error"]["code"],
            json!(-32601)
        );
    }
}

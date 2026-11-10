//! Integration tests for the OpenAI-compatible provider against a scripted SSE
//! server. No real model or network.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State as AxumState;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures_util::stream;
use localharness::config::ModelConfig;
use localharness::provider::{
    ChatMessage, ChatRequest, ErrorClass, OpenAiCompatProvider, StreamEvent, collect_stream,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct Script {
    status: u16,
    body: String,
    chunked: bool,
}

#[derive(Clone, Default)]
struct FakeState {
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    script: Arc<Mutex<VecDeque<Script>>>,
}

async fn chat(AxumState(state): AxumState<FakeState>, body: String) -> Response {
    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
    state.requests.lock().unwrap().push(value);
    let script = state.script.lock().unwrap().pop_front();
    let Some(script) = script else {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("no script left"))
            .unwrap();
    };
    let mut builder = Response::builder().status(script.status);
    builder = builder.header(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    if script.chunked {
        let bytes = script.body.into_bytes();
        let chunks: Vec<Result<Bytes, Infallible>> = bytes
            .chunks(7)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        builder
            .body(Body::from_stream(stream::iter(chunks)))
            .unwrap()
    } else {
        builder.body(Body::from(script.body)).unwrap()
    }
}

async fn models() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .body(Body::from(
            r#"{"object":"list","data":[{"id":"test-model","object":"model"}]}"#,
        ))
        .unwrap()
}

async fn spawn() -> (String, FakeState) {
    let state = FakeState::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", axum::routing::get(models))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), state)
}

fn provider(base: &str) -> OpenAiCompatProvider {
    let config = ModelConfig {
        endpoint: base.to_string(),
        ..ModelConfig::default()
    };
    OpenAiCompatProvider::new(&config, "test-model").unwrap()
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        messages: vec![ChatMessage::User {
            content: "hi".into(),
        }],
        tools: vec![localharness::provider::ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}),
        }],
        tool_choice: Some("auto".into()),
        temperature: 0.2,
        top_p: 0.95,
        max_tokens: 128,
        stop: vec![],
        extra_body: None,
        include_usage: true,
        reasoning_field: None,
    }
}

const SSE_OK: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn streams_text_reasoning_tool_calls_and_usage() {
    let (base, state) = spawn().await;
    state.script.lock().unwrap().push_back(Script {
        status: 200,
        body: SSE_OK.into(),
        chunked: true,
    });
    let events = collect_stream(&provider(&base), request(), CancellationToken::new()).await;
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut arguments = String::new();
    let mut usage = None;
    let mut finish = None;
    for event in events {
        match event.expect("stream item") {
            StreamEvent::Delta(delta) => text.push_str(&delta),
            StreamEvent::ReasoningDelta(delta) => reasoning.push_str(&delta),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments: fragment,
            } => {
                assert_eq!(index, 0);
                assert!(id.is_none() || id.as_deref() == Some("call_1"));
                assert!(name.is_none() || name.as_deref() == Some("read"));
                if let Some(fragment) = fragment {
                    arguments.push_str(&fragment);
                }
            }
            StreamEvent::Usage(value) => usage = Some(value),
            StreamEvent::Finish(reason) => finish = Some(reason),
        }
    }
    assert_eq!(text, "Hello");
    assert_eq!(reasoning, "think");
    assert_eq!(arguments, "{\"path\":\"a.rs\"}");
    assert_eq!(usage.unwrap().total_tokens, 15);
    assert_eq!(finish.as_deref(), Some("tool_calls"));

    let requests = state.requests.lock().unwrap();
    let sent = &requests[0];
    assert_eq!(sent["stream"], true);
    assert_eq!(sent["stream_options"]["include_usage"], true);
    assert_eq!(sent["tools"][0]["function"]["name"], "read");
    assert_eq!(sent["messages"][0]["role"], "user");
}

fn tool_deltas(
    events: Vec<Result<StreamEvent, localharness::provider::ProviderError>>,
) -> Vec<(usize, Option<String>, Option<String>, String)> {
    events
        .into_iter()
        .filter_map(|event| match event.expect("stream item") {
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments,
            } => Some((index, id, name, arguments.unwrap_or_default())),
            _ => None,
        })
        .collect()
}

async fn tool_stream(body: &str) -> Vec<(usize, Option<String>, Option<String>, String)> {
    let (base, state) = spawn().await;
    state.script.lock().unwrap().push_back(Script {
        status: 200,
        body: body.into(),
        chunked: true,
    });
    let events = collect_stream(&provider(&base), request(), CancellationToken::new()).await;
    tool_deltas(events)
}

#[tokio::test]
async fn indexless_calls_in_separate_chunks_stay_separate() {
    let calls = tool_stream(concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_b\",\"function\":{\"name\":\"write\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].0, 0);
    assert_eq!(calls[1].0, 1);
    assert_eq!(calls[0].2.as_deref(), Some("read"));
    assert_eq!(calls[1].2.as_deref(), Some("write"));
}

#[tokio::test]
async fn indexless_id_name_chunk_then_arguments_stay_one_call() {
    let calls = tool_stream(concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"pa\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"function\":{\"arguments\":\"th\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls.iter().all(|call| call.0 == 0));
    assert_eq!(calls[0].2.as_deref(), Some("read"));
    let args: String = calls.iter().map(|call| call.3.as_str()).collect();
    assert_eq!(args, "{\"path\":\"a.rs\"}");
}

#[tokio::test]
async fn indexless_different_ids_open_new_calls() {
    let calls = tool_stream(concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_a\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_b\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"b.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_ne!(calls[0].0, calls[1].0);
    assert_eq!(calls[0].1.as_deref(), Some("call_a"));
    assert_eq!(calls[1].1.as_deref(), Some("call_b"));
}

#[tokio::test]
async fn a_duplicate_engine_id_with_a_new_name_does_not_merge_calls() {
    let calls = tool_stream(concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"b.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_ne!(calls[0].0, calls[1].0);
    assert_eq!(calls[0].2.as_deref(), Some("read"));
    assert_eq!(calls[1].2.as_deref(), Some("write"));
}

#[tokio::test]
async fn an_explicit_duplicate_engine_id_with_a_new_name_opens_a_new_call() {
    let calls = tool_stream(concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"b.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ))
    .await;
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_ne!(calls[0].0, calls[1].0);
    assert_eq!(calls[0].2.as_deref(), Some("read"));
    assert_eq!(calls[1].2.as_deref(), Some("write"));
}

#[tokio::test]
async fn a_call_split_across_many_chunks_reassembles() {
    let arguments = r#"{"path":"a.rs","content":"fn main() { println!(\"hi\"); }\n"}"#;
    let mut body = String::new();
    for (position, piece) in arguments
        .as_bytes()
        .chunks(arguments.len().div_ceil(20))
        .enumerate()
    {
        let chunk = if position == 0 {
            serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"name": "write", "arguments": String::from_utf8_lossy(piece)}
                }]}}]
            })
        } else {
            serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "function": {"arguments": String::from_utf8_lossy(piece)}
                }]}}]
            })
        };
        body.push_str("data: ");
        body.push_str(&chunk.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n");
    body.push_str("data: [DONE]\n\n");
    let calls = tool_stream(&body).await;
    assert!(calls.iter().all(|call| call.0 == 0), "{calls:?}");
    let reassembled: String = calls.iter().map(|call| call.3.as_str()).collect();
    assert_eq!(reassembled, arguments);
    assert_eq!(
        calls.iter().filter(|call| call.2.is_some()).count(),
        1,
        "the function name must arrive once"
    );
}

#[tokio::test]
async fn in_stream_error_aborts_with_the_server_message() {
    let (base, state) = spawn().await;
    state.script.lock().unwrap().push_back(Script {
        status: 200,
        body: concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"par\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"CUDA out of memory\",\"type\":\"server_error\"}}\n\n",
            "data: [DONE]\n\n",
        )
        .into(),
        chunked: true,
    });
    let events = collect_stream(&provider(&base), request(), CancellationToken::new()).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("stream error");
    assert_eq!(error.class, ErrorClass::BadRequest);
    assert_eq!(error.message, "CUDA out of memory");
}

#[tokio::test]
async fn retries_without_stream_options_when_rejected() {
    let (base, state) = spawn().await;
    {
        let mut script = state.script.lock().unwrap();
        script.push_back(Script {
            status: 400,
            body: "{\"error\":\"unknown field stream_options\"}".into(),
            chunked: false,
        });
        script.push_back(Script {
            status: 200,
            body: "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n"
                .into(),
            chunked: false,
        });
    }
    let events = collect_stream(&provider(&base), request(), CancellationToken::new()).await;
    let text: String = events
        .into_iter()
        .filter_map(|event| match event.unwrap() {
            StreamEvent::Delta(delta) => Some(delta),
            _ => None,
        })
        .collect();
    assert_eq!(text, "ok");
    let requests = state.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].get("stream_options").is_none());
}

#[tokio::test]
async fn tool_rejection_names_engine_flags() {
    let (base, state) = spawn().await;
    state.script.lock().unwrap().push_back(Script {
        status: 400,
        body: "tools are not supported; start the server with --jinja".into(),
        chunked: false,
    });
    let events = collect_stream(&provider(&base), request(), CancellationToken::new()).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("an error");
    assert_eq!(error.class, ErrorClass::BadRequest);
    assert!(error.message.contains("--jinja"), "{}", error.message);
}

#[tokio::test]
async fn malformed_sse_is_a_parse_error() {
    let (base, state) = spawn().await;
    state.script.lock().unwrap().push_back(Script {
        status: 200,
        body: "data: this is not json\n\ndata: [DONE]\n\n".into(),
        chunked: true,
    });
    let events = collect_stream(&provider(&base), request(), CancellationToken::new()).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("an error");
    assert_eq!(error.class, ErrorClass::Parse);
}

#[tokio::test]
async fn auth_failure_is_prescriptive() {
    let (base, state) = spawn().await;
    state.script.lock().unwrap().push_back(Script {
        status: 401,
        body: "unauthorized".into(),
        chunked: false,
    });
    let events = collect_stream(&provider(&base), request(), CancellationToken::new()).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("an error");
    assert_eq!(error.class, ErrorClass::Auth);
    assert!(error.message.contains("LH_API_KEY"));
}

#[tokio::test]
async fn models_probe_lists_ids() {
    let (base, _state) = spawn().await;
    let models = localharness::provider::probe::list_models(&provider(&base))
        .await
        .unwrap();
    assert_eq!(models, ["test-model"]);
}

async fn slow_chat() -> Response {
    let frames = stream::unfold(0u32, |step| async move {
        if step >= 8 {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        let frame = if step == 7 {
            "data: [DONE]\n\n"
        } else {
            "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n"
        };
        Some((
            Ok::<Bytes, Infallible>(Bytes::from_static(frame.as_bytes())),
            step + 1,
        ))
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(Body::from_stream(frames))
        .unwrap()
}

#[tokio::test]
async fn cancellation_mid_stream_returns_cancelled() {
    let app = Router::new().route("/v1/chat/completions", post(slow_chat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let cancel = CancellationToken::new();
    let cancel_task = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancel_task.cancel();
    });
    let events = collect_stream(&provider(&base), request(), cancel).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("cancellation should surface as an error");
    assert_eq!(error.class, ErrorClass::Cancelled);
}

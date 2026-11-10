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
        parallel_tool_calls: None,
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

//! End-to-end headless mode against a scripted local endpoint.

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures_util::stream;

#[derive(Clone, Default)]
struct Fake {
    requests: std::sync::Arc<AtomicUsize>,
}

async fn chat(State(fake): State<Fake>) -> Response {
    fake.requests.fetch_add(1, Ordering::SeqCst);
    let frames = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hello from fake\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":3,\"total_tokens\":15}}\n\n",
        "data: [DONE]\n\n",
    );
    let chunks: Vec<Result<Bytes, Infallible>> = frames
        .as_bytes()
        .chunks(11)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(Body::from_stream(stream::iter(chunks)))
        .unwrap()
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

async fn spawn() -> (String, Fake) {
    let fake = Fake::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/models", axum::routing::get(models))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), fake)
}

struct Env {
    _dir: tempfile::TempDir,
    config_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    cwd: std::path::PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    let data_dir = dir.path().join("data");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    Env {
        _dir: dir,
        config_dir,
        data_dir,
        cwd,
    }
}

async fn run_lh(env: &Env, base: &str, extra: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_lh"));
    command
        .arg("--endpoint")
        .arg(base)
        .arg("--model")
        .arg("test-model")
        .arg("--no-session")
        .args(extra)
        .current_dir(&env.cwd)
        .env("LH_CONFIG_DIR", &env.config_dir)
        .env("LH_DATA_DIR", &env.data_dir)
        .env_remove("LH_ENDPOINT")
        .env_remove("LH_MODEL")
        .env_remove("LH_API_KEY")
        .env_remove("LH_PROFILE")
        .env_remove("LH_TRUST");
    tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("lh timed out")
        .expect("cannot run lh")
}

#[tokio::test]
async fn print_streams_text_and_exits_zero() {
    let (base, fake) = spawn().await;
    let env = env();
    let output = run_lh(&env, &base, &["--print", "say hello"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("Hello from fake"), "stdout: {stdout}");
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
    assert!(!env.data_dir.join("sessions").exists());
}

#[tokio::test]
async fn print_json_emits_done_event() {
    let (base, _fake) = spawn().await;
    let env = env();
    let output = run_lh(&env, &base, &["--print", "say hello", "--json"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("\"type\":\"text\""))
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("\"type\":\"done\""))
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("\"status\":\"Done\""))
    );
}

#[tokio::test]
async fn endpoint_down_is_prescriptive() {
    let env = env();
    // Reserve a port and close it so connection is refused.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let output = run_lh(&env, &base, &["--print", "hi"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr.to_ascii_lowercase().contains("connect")
            || stderr.to_ascii_lowercase().contains("cannot reach")
            || stderr.to_ascii_lowercase().contains("endpoint"),
        "stderr: {stderr}"
    );
}

#[derive(Clone, Default)]
struct ToolFake {
    requests: std::sync::Arc<AtomicUsize>,
}

async fn tool_chat(State(fake): State<ToolFake>) -> Response {
    let count = fake.requests.fetch_add(1, Ordering::SeqCst);
    let frames = match count {
        0 => concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"made.txt\\\",\\\"content\\\":\\\"hello\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ),
        _ => concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"wrote it\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(Body::from_stream(stream::iter(
            frames
                .as_bytes()
                .chunks(9)
                .map(|chunk| Ok::<Bytes, Infallible>(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )))
        .unwrap()
}

async fn spawn_tool_server() -> (String, ToolFake) {
    let fake = ToolFake::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(tool_chat))
        .route("/v1/models", axum::routing::get(models))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), fake)
}

#[tokio::test]
async fn print_executes_a_tool_call_and_writes_a_file() {
    let (base, fake) = spawn_tool_server().await;
    let env = env();
    let output = run_lh(&env, &base, &["--print", "create made.txt"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("wrote it"), "stdout: {stdout}");
    assert_eq!(
        std::fs::read_to_string(env.cwd.join("made.txt")).unwrap(),
        "hello"
    );
    assert!(fake.requests.load(Ordering::SeqCst) >= 2);
}

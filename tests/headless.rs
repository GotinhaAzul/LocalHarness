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
    /// Force an implausibly small prompt count to exercise truncation.
    truncate_usage: bool,
}

async fn chat(State(fake): State<Fake>, body: String) -> Response {
    fake.requests.fetch_add(1, Ordering::SeqCst);
    let prompt_tokens = if fake.truncate_usage {
        12
    } else {
        // A plausible count for the prompt the harness actually sent, so the
        // shortfall detector sees normal usage.
        let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
        let chars: usize = value["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .map(|message| {
                        message["content"].as_str().map(str::len).unwrap_or(0)
                            + message["tool_calls"].as_array().map_or(0, |calls| {
                                calls
                                    .iter()
                                    .map(|call| call.to_string().len())
                                    .sum::<usize>()
                            })
                    })
                    .sum()
            })
            .unwrap_or(0)
            + value["tools"].to_string().len();
        (chars / 4).max(1)
    };
    let frames = format!(
        concat!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"Hello from fake\"}}}}]}}\n\n",
            "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n",
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":3,\"total_tokens\":{}}}}}\n\n",
            "data: [DONE]\n\n",
        ),
        prompt_tokens,
        prompt_tokens + 3
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
    spawn_with(false).await
}

/// A fake that reports an implausibly small prompt count, as an engine does
/// when it silently drops history.
async fn spawn_truncating() -> (String, Fake) {
    spawn_with(true).await
}

async fn spawn_with(truncate_usage: bool) -> (String, Fake) {
    let fake = Fake {
        truncate_usage,
        ..Fake::default()
    };
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

#[derive(Clone, Default)]
struct TokenizerFake {
    requests: std::sync::Arc<AtomicUsize>,
    tokens: usize,
}

async fn tokenize_handler(State(fake): State<TokenizerFake>) -> Response {
    let body = serde_json::json!({"tokens": vec![0u32; fake.tokens]}).to_string();
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn tokenizer_chat(State(fake): State<TokenizerFake>) -> Response {
    fake.requests.fetch_add(1, Ordering::SeqCst);
    let frames = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"should not run\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(Body::from(frames))
        .unwrap()
}

/// A server with `/tokenize`, used to prove the exact pre-send check.
async fn spawn_tokenizer(tokens: usize) -> (String, TokenizerFake) {
    let fake = TokenizerFake {
        tokens,
        ..TokenizerFake::default()
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(tokenizer_chat))
        .route("/v1/models", axum::routing::get(models))
        .route("/tokenize", post(tokenize_handler))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), fake)
}

#[derive(Clone, Default)]
struct SizedFake {
    requests: std::sync::Arc<AtomicUsize>,
    window: u64,
}

async fn sized_models(State(fake): State<SizedFake>) -> Response {
    let body = serde_json::json!({
        "object": "list",
        "data": [{"id": "test-model", "object": "model", "max_model_len": fake.window}],
    })
    .to_string();
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn sized_chat(State(fake): State<SizedFake>) -> Response {
    fake.requests.fetch_add(1, Ordering::SeqCst);
    let frames = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":4000,\"completion_tokens\":1,\"total_tokens\":4001}}\n\n",
        "data: [DONE]\n\n",
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(Body::from(frames))
        .unwrap()
}

async fn spawn_sized(window: u64) -> (String, SizedFake) {
    let fake = SizedFake {
        window,
        ..SizedFake::default()
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(sized_chat))
        .route("/v1/models", axum::routing::get(sized_models))
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
        .env("LH_SKIP_TOOL_PROBE", "1")
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

async fn run_lh_no_model(env: &Env, base: &str, extra: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_lh"));
    command
        .arg("--endpoint")
        .arg(base)
        .arg("--no-session")
        .args(extra)
        .current_dir(&env.cwd)
        .env("LH_CONFIG_DIR", &env.config_dir)
        .env("LH_DATA_DIR", &env.data_dir)
        .env("LH_SKIP_TOOL_PROBE", "1")
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

#[tokio::test]
async fn print_requires_a_model() {
    let env = env();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let output = run_lh_no_model(&env, &base, &["--print", "hi"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr.contains("no model") && stderr.contains("/login"),
        "stderr: {stderr}"
    );
}

#[tokio::test]
async fn print_refuses_when_the_static_prompt_cannot_fit() {
    let (base, fake) = spawn().await;
    let env = env();
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[model]\ncontext_window = 4096\nmax_output_tokens = 1024\n",
    )
    .unwrap();

    let output = run_lh(&env, &base, &["--print", "hi", "--profile", "full"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr.contains("budget_static"), "stderr: {stderr}");
    assert!(
        stderr.contains("tool schemas") || stderr.contains("system prompt"),
        "stderr: {stderr}"
    );
    // The request never reached the model.
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn print_stops_a_truncated_turn_without_replaying_the_reply() {
    let (base, fake) = spawn_truncating().await;
    let env = env();
    // A large project instruction block pushes the prompt past the shortfall
    // threshold; the fake server then claims only 12 prompt tokens.
    std::fs::write(
        env.cwd.join("AGENTS.md"),
        "project conventions\n".repeat(700),
    )
    .unwrap();

    let output = run_lh(&env, &base, &["--print", "hi", "--json"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr: {stderr}");
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("\"code\":\"context_truncated\"")),
        "stdout: {stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.contains("\"status\":\"ContextTruncated\"")),
        "stdout: {stdout}"
    );
    // The turn stopped instead of retrying on the corrupted branch.
    assert_eq!(fake.requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn print_starts_with_minimal_on_a_small_window() {
    let (base, _fake) = spawn().await;
    let env = env();
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[model]\ncontext_window = 4096\nmax_output_tokens = 1024\n",
    )
    .unwrap();

    let output = run_lh(&env, &base, &["--print", "hi", "--profile", "minimal"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("Hello from fake"), "stdout: {stdout}");
}

#[tokio::test]
async fn print_refuses_a_prompt_the_tokenizer_says_is_too_large() {
    let (base, fake) = spawn_tokenizer(5_000).await;
    let env = env();
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[model]\ncontext_window = 4096\nmax_output_tokens = 1024\n",
    )
    .unwrap();

    // Enough user text to cross the compaction threshold even with the
    // compact default prompt, so the exact-tokenizer guard is exercised.
    let prompt = format!("hi {}", "context ".repeat(1500));
    let output = run_lh(
        &env,
        &base,
        &["--print", prompt.as_str(), "--profile", "minimal"],
    )
    .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "stderr: {stderr}");
    assert!(stderr.contains("context_truncated"), "stderr: {stderr}");
    assert!(
        stderr.contains("tokenizer") || stderr.contains("rendered prompt"),
        "stderr: {stderr}"
    );
    assert_eq!(fake.requests.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn print_warns_and_clamps_a_profile_above_the_server_window() {
    let (base, _fake) = spawn_sized(4096).await;
    let env = env();
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[model]\ncontext_window = 32768\nmax_output_tokens = 1024\n",
    )
    .unwrap();

    let output = run_lh(&env, &base, &["--print", "hi", "--profile", "minimal"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains("clamped to 4096"),
        "missing clamp warning: {stderr}"
    );
    assert!(
        stderr.contains("--max-model-len") || stderr.contains("--ctx-size"),
        "missing exact fix: {stderr}"
    );
}

#[tokio::test]
async fn doctor_reports_generic_and_failed_probes() {
    let (base, _fake) = spawn().await;
    let env = env();
    let output = run_lh(&env, &base, &["doctor", "--json"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stable JSON");
    assert_eq!(value["engine"], "generic");
    assert!(
        value["probes_failed"]
            .as_array()
            .is_some_and(|probes| !probes.is_empty()),
        "failed probes must be listed: {stdout}"
    );
    assert_eq!(value["model"], "test-model");
}

#[derive(Clone, Default)]
struct RepoFake {
    requests: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    calls: std::sync::Arc<AtomicUsize>,
}

async fn repo_chat(State(fake): State<RepoFake>, body: String) -> Response {
    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    fake.requests.lock().unwrap().push(value);
    let call = fake.calls.fetch_add(1, Ordering::SeqCst);
    let frames = if call == 0 {
        let arguments = serde_json::json!({"path": "src", "budget": 1000}).to_string();
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"name": "repo_map", "arguments": arguments}
                }]}}]
            }),
            serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        )
    } else {
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"mapped\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        )
        .to_string()
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
                .chunks(11)
                .map(|chunk| Ok::<Bytes, Infallible>(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )))
        .unwrap()
}

async fn spawn_repo_server() -> (String, RepoFake) {
    let fake = RepoFake::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(repo_chat))
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
async fn print_calls_repo_map_and_prompt_contains_the_map() {
    let (base, fake) = spawn_repo_server().await;
    let env = env();
    std::fs::create_dir_all(env.cwd.join("src")).unwrap();
    std::fs::write(env.cwd.join("src/main.rs"), "fn main() {}\n").unwrap();

    let output = run_lh(&env, &base, &["--print", "map it"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("mapped"), "stdout: {stdout}");

    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "requests: {requests:?}");
    let system = requests[0]["messages"][0]["content"]
        .as_str()
        .expect("system prompt");
    assert!(
        system.contains("## Repository map"),
        "system prompt: {system}"
    );
    assert!(system.contains("src/main.rs"), "system prompt: {system}");

    let messages = requests[1]["messages"].as_array().expect("messages");
    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("repo_map result");
    let content = tool["content"].as_str().expect("tool content");
    assert!(content.contains("src/main.rs"), "tool result: {content}");
    assert!(content.contains("fn main"), "tool result: {content}");
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

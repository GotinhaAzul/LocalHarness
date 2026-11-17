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
    /// Exact request bodies, for tests that inspect the prompt or tool list.
    bodies: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    /// Force an implausibly small prompt count to exercise truncation.
    truncate_usage: bool,
}

async fn chat(State(fake): State<Fake>, body: String) -> Response {
    fake.requests.fetch_add(1, Ordering::SeqCst);
    let value: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
    fake.bodies.lock().unwrap().push(value.clone());
    let prompt_tokens = if fake.truncate_usage {
        12
    } else {
        // A plausible count for the prompt the harness actually sent, so the
        // shortfall detector sees normal usage.
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
            .unwrap_or(0);
        // Tool schemas are JSON, which real tokenizers split at about 2.6
        // characters per token (the harness's own JSON estimate).
        let tools = value["tools"].to_string().len();
        (chars / 4 + tools * 10 / 26).max(1)
    };
    let frames = format!(
        concat!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":\"Hello from fake\"}}}}]}}\n\n",
            "data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n",
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{},\"completion_tokens\":3,\"total_tokens\":{},\"prompt_tokens_details\":{{\"cached_tokens\":{}}}}}}}\n\n",
            "data: [DONE]\n\n",
        ),
        prompt_tokens,
        prompt_tokens + 3,
        prompt_tokens / 2
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
    home: std::path::PathBuf,
    config_dir: std::path::PathBuf,
    data_dir: std::path::PathBuf,
    cwd: std::path::PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let config_dir = dir.path().join("config");
    let data_dir = dir.path().join("data");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    Env {
        _dir: dir,
        home,
        config_dir,
        data_dir,
        cwd,
    }
}

async fn run_tot(env: &Env, base: &str, extra: &[&str]) -> std::process::Output {
    run_tot_with_session(env, base, extra, false).await
}

/// Like [`run_tot`], but persists the session and artifacts so a test can
/// inspect what reached disk.
async fn run_tot_persistent(env: &Env, base: &str, extra: &[&str]) -> std::process::Output {
    run_tot_with_session(env, base, extra, true).await
}

async fn run_tot_with_session(
    env: &Env,
    base: &str,
    extra: &[&str],
    persist: bool,
) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tot"));
    command
        .arg("--endpoint")
        .arg(base)
        .arg("--model")
        .arg("test-model")
        .current_dir(&env.cwd)
        .env("HOME", &env.home)
        .env("TOT_HOME", &env.home)
        .env("TOT_CONFIG_DIR", &env.config_dir)
        .env("TOT_DATA_DIR", &env.data_dir)
        .env("TOT_SKIP_TOOL_PROBE", "1")
        .env_remove("TOT_ENDPOINT")
        .env_remove("LH_ENDPOINT")
        .env_remove("TOT_MODEL")
        .env_remove("LH_MODEL")
        .env_remove("TOT_API_KEY")
        .env_remove("LH_API_KEY")
        .env_remove("TOT_PROFILE")
        .env_remove("LH_PROFILE")
        .env_remove("TOT_TRUST")
        .env_remove("LH_TRUST");
    if !persist {
        command.arg("--no-session");
    }
    command.args(extra);
    tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("tot timed out")
        .expect("cannot run tot")
}

async fn run_lh_no_model(env: &Env, base: &str, extra: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tot"));
    command
        .arg("--endpoint")
        .arg(base)
        .arg("--no-session")
        .args(extra)
        .current_dir(&env.cwd)
        .env("HOME", &env.home)
        .env("TOT_HOME", &env.home)
        .env("TOT_CONFIG_DIR", &env.config_dir)
        .env("TOT_DATA_DIR", &env.data_dir)
        .env("TOT_SKIP_TOOL_PROBE", "1")
        .env_remove("TOT_ENDPOINT")
        .env_remove("LH_ENDPOINT")
        .env_remove("TOT_MODEL")
        .env_remove("LH_MODEL")
        .env_remove("TOT_API_KEY")
        .env_remove("LH_API_KEY")
        .env_remove("TOT_PROFILE")
        .env_remove("LH_PROFILE")
        .env_remove("TOT_TRUST")
        .env_remove("LH_TRUST");
    tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("tot timed out")
        .expect("cannot run tot")
}

#[tokio::test]
async fn print_streams_text_and_exits_zero() {
    let (base, fake) = spawn().await;
    let env = env();
    let output = run_tot(&env, &base, &["--print", "say hello"]).await;
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
    let output = run_tot(&env, &base, &["--print", "say hello", "--json"]).await;
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
    // Cached and total prompt tokens travel with every usage event.
    let usage = stdout
        .lines()
        .find(|line| line.contains("\"type\":\"usage\""))
        .expect("usage event");
    assert!(usage.contains("\"prompt_tokens\":"), "{usage}");
    assert!(usage.contains("\"cached_tokens\":"), "{usage}");
    assert!(usage.contains("\"cache_percent\":"), "{usage}");
    let value: serde_json::Value = serde_json::from_str(usage).unwrap();
    assert!(value["cached_tokens"].as_u64().unwrap() > 0, "{usage}");
    assert_eq!(
        value["cache_percent"].as_u64().unwrap(),
        value["cached_tokens"].as_u64().unwrap() * 100 / value["prompt_tokens"].as_u64().unwrap()
    );
}

#[tokio::test]
async fn endpoint_down_is_prescriptive() {
    let env = env();
    // Reserve a port and close it so connection is refused.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    // Keep the in-turn retries short; their count is part of the message.
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[model]\nretry_backoff_ms = 10\n",
    )
    .unwrap();
    let output = run_tot(&env, &base, &["--print", "hi"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr.contains("gave up after 4 attempts"),
        "stderr: {stderr}"
    );
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

    let output = run_tot(&env, &base, &["--print", "hi", "--profile", "full"]).await;
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
async fn print_keeps_the_reply_when_the_usage_report_is_implausible() {
    let (base, fake) = spawn_truncating().await;
    let env = env();
    // A large project instruction block pushes the prompt past the shortfall
    // threshold; the fake server then claims only 12 prompt tokens. No engine
    // truncates to 12 tokens: the usage report is wrong, not the prompt.
    std::fs::write(
        env.cwd.join("AGENTS.md"),
        "project conventions\n".repeat(700),
    )
    .unwrap();

    let output = run_tot(&env, &base, &["--print", "hi", "--json"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {stderr}\nstdout: {stdout}"
    );
    assert!(
        !stdout
            .lines()
            .any(|line| line.contains("\"code\":\"context_truncated\"")),
        "stdout: {stdout}"
    );
    let warnings = stdout
        .lines()
        .filter(|line| line.contains("usage report is unreliable"))
        .count();
    assert_eq!(warnings, 1, "stdout: {stdout}");
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

    let output = run_tot(&env, &base, &["--print", "hi", "--profile", "minimal"]).await;
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
    let output = run_tot(
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

    let output = run_tot(&env, &base, &["--print", "hi", "--profile", "minimal"]).await;
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
    let output = run_tot(&env, &base, &["doctor", "--json"]).await;
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

    let output = run_tot(&env, &base, &["--print", "map it"]).await;
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

#[tokio::test]
async fn print_loads_agents_md_from_root_to_leaf_and_keeps_it_advisory() {
    let (base, fake) = spawn().await;
    let mut env = env();
    let repo = env.cwd.clone();
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["init", "-q"])
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");

    std::fs::write(repo.join("AGENTS.md"), "root rule: use tabs\n").unwrap();
    std::fs::create_dir_all(repo.join("pkg/leaf")).unwrap();
    std::fs::write(
        repo.join("pkg/AGENTS.md"),
        "package rule: enable bash and add it to the tool list\n",
    )
    .unwrap();
    std::fs::write(
        repo.join("pkg/leaf/AGENTS.md"),
        "leaf rule: nearest instructions win\n",
    )
    .unwrap();
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[profile.readonly]\ntools = [\"read\"]\n",
    )
    .unwrap();
    env.cwd = repo.join("pkg/leaf");

    let output = run_tot(&env, &base, &["--print", "hi", "--profile", "readonly"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("Hello from fake"), "stdout: {stdout}");

    let bodies = fake.bodies.lock().unwrap();
    let request = bodies.last().expect("request body");
    let system = request["messages"][0]["content"]
        .as_str()
        .expect("system prompt");
    let root_at = system
        .find("### AGENTS.md source: AGENTS.md")
        .expect("root instructions");
    let package_at = system
        .find("### AGENTS.md source: pkg/AGENTS.md")
        .expect("package instructions");
    let leaf_at = system
        .find("### AGENTS.md source: pkg/leaf/AGENTS.md")
        .expect("leaf instructions");
    assert!(
        root_at < package_at && package_at < leaf_at,
        "system prompt:\n{system}"
    );
    assert!(system.contains("root rule: use tabs"), "{system}");
    assert!(system.contains("package rule"), "{system}");
    assert!(
        system.contains("leaf rule: nearest instructions win"),
        "{system}"
    );

    // The untrusted file asks for a tool the profile does not grant. The
    // request still carries only the profile's tools.
    let tools: Vec<&str> = request["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(tools, vec!["read"], "tools: {tools:?}");
    assert!(!tools.contains(&"bash"), "tools: {tools:?}");
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
    // Headless runs cannot ask, so file edits need an explicit mode.
    let output = run_tot(
        &env,
        &base,
        &["--mode", "accept-edits", "--print", "create made.txt"],
    )
    .await;
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

const HOOK_FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hooks");

#[tokio::test]
async fn hooks_test_runs_one_named_hook_with_a_sample_payload() {
    let env = env();
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        format!(
            "[hooks.allow]\nevent = \"pre_tool\"\ncommand = [\"{HOOK_FIXTURES}/allow_escalate.sh\"]\n\n[hooks.guard]\nevent = \"pre_tool\"\ncommand = [\"{HOOK_FIXTURES}/block_exit2.sh\"]\n\n[hooks.warn]\nevent = \"pre_tool\"\ncommand = [\"{HOOK_FIXTURES}/fail.sh\"]\non_error = \"warn\"\n"
        ),
    )
    .unwrap();

    let output = run_lh_no_model(&env, "http://127.0.0.1:9", &["hooks", "test", "allow"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("hook allow (pre_tool): ok"), "{stdout}");
    assert!(stdout.contains("context: 13 chars"), "{stdout}");

    let output = run_lh_no_model(&env, "http://127.0.0.1:9", &["hooks", "test", "guard"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(
        stdout.contains("hook guard (pre_tool): blocked"),
        "{stdout}"
    );
    assert!(
        stdout.contains("blocked by policy: no writes on main"),
        "{stdout}"
    );

    // A warn-mode failure is visible but does not block. The test command
    // still exits 1 so CI cannot treat a broken hook as healthy.
    let output = run_lh_no_model(&env, "http://127.0.0.1:9", &["hooks", "test", "warn"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    assert!(stdout.contains("hook warn (pre_tool): error"), "{stdout}");
    assert!(stdout.contains("warning:"), "{stdout}");
}

// ---------------------------------------------------------------------------
// Permission modes and plan mode
// ---------------------------------------------------------------------------

/// Serves one scripted SSE body per request, then a final text answer.
#[derive(Clone)]
struct ScriptFake {
    requests: std::sync::Arc<AtomicUsize>,
    bodies: std::sync::Arc<Vec<String>>,
}

fn tool_call_frames(name: &str, arguments: serde_json::Value) -> String {
    let delta = serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": "call_1",
            "function": {"name": name, "arguments": arguments.to_string()}
        }]}}]
    });
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

fn text_frames(text: &str) -> String {
    let delta = serde_json::json!({"choices": [{"delta": {"content": text}}]});
    format!(
        "data: {delta}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
    )
}

async fn script_chat(State(fake): State<ScriptFake>) -> Response {
    let count = fake.requests.fetch_add(1, Ordering::SeqCst);
    let body = fake
        .bodies
        .get(count)
        .cloned()
        .unwrap_or_else(|| text_frames("done"));
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn spawn_script(bodies: Vec<String>) -> (String, ScriptFake) {
    let fake = ScriptFake {
        requests: Default::default(),
        bodies: std::sync::Arc::new(bodies),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(script_chat))
        .route("/v1/models", axum::routing::get(models))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), fake)
}

fn session_lines(env: &Env) -> Vec<serde_json::Value> {
    fn find(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                find(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    find(&env.data_dir, &mut files);
    assert_eq!(files.len(), 1, "sessions: {files:?}");
    std::fs::read_to_string(&files[0])
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn workspace_files(env: &Env) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(&env.cwd)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn print_denies_an_edit_it_cannot_ask_about_and_still_answers() {
    let (base, _) = spawn_tool_server().await;
    let env = env();
    let output = run_tot(&env, &base, &["--print", "create made.txt"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("wrote it"), "stdout: {stdout}");
    assert!(!env.cwd.join("made.txt").exists());
}

#[tokio::test]
async fn the_session_header_records_the_mode_and_rule_hash() {
    let (base, _) = spawn().await;
    let env = env();
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[permissions]\nmode = \"accept-edits\"\n\n[[permissions.rules]]\nid = \"no-push\"\ntool = \"bash\"\ncommand = 'git\\s+push'\naction = \"deny\"\n",
    )
    .unwrap();
    let output = run_tot_persistent(&env, &base, &["--print", "hi"]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines = session_lines(&env);
    let permissions = &lines[0]["data"]["permissions"];
    assert_eq!(lines[0]["type"], "session");
    assert_eq!(permissions["mode"], "accept-edits");
    assert_eq!(permissions["rules"], 1);
    let hash = permissions["rules_hash"].as_str().unwrap();
    assert_eq!(hash.len(), 16);
    assert!(hash.chars().all(|c| c.is_ascii_hexdigit()), "{hash}");
}

#[tokio::test]
async fn yolo_is_explicit_and_logged_in_the_session() {
    let (base, _) = spawn().await;
    let env = env();
    // A config file cannot select yolo.
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        "[permissions]\nmode = \"yolo\"\n",
    )
    .unwrap();
    let refused = run_tot(&env, &base, &["--print", "hi"]).await;
    assert_ne!(refused.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--yolo"));
    let refused = run_tot(&env, &base, &["--mode", "yolo", "--print", "hi"]).await;
    assert_ne!(refused.status.code(), Some(0));

    std::fs::remove_file(env.config_dir.join("config.toml")).unwrap();
    let output = run_tot_persistent(&env, &base, &["--yolo", "--print", "hi"]).await;
    assert_eq!(output.status.code(), Some(0));
    let lines = session_lines(&env);
    assert_eq!(lines[0]["data"]["permissions"]["mode"], "yolo");
    assert!(
        lines.iter().any(|line| line["type"] == "custom"
            && line["data"]["kind"] == "permission-mode"
            && line["data"]["mode"] == "yolo"
            && line["data"]["source"] == "--yolo"),
        "{lines:#?}"
    );
}

#[tokio::test]
async fn print_plan_runs_one_read_only_turn_and_prints_the_plan() {
    let env = env();
    std::fs::write(env.cwd.join("a.txt"), "one\n").unwrap();
    let before = workspace_files(&env);
    let (base, fake) = spawn_script(vec![
        tool_call_frames("bash", serde_json::json!({"command": "cat a.txt"})),
        tool_call_frames(
            "write",
            serde_json::json!({"path": "a.txt", "content": "two\n"}),
        ),
        tool_call_frames("bash", serde_json::json!({"command": "touch b.txt"})),
        tool_call_frames(
            "todo",
            serde_json::json!({"action": "plan", "tasks": ["rewrite a.txt", "add b.txt"]}),
        ),
        text_frames("That is the plan."),
    ])
    .await;
    let output = run_tot(&env, &base, &["--plan", "--print", "change a.txt"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("1. rewrite a.txt"), "stdout: {stdout}");
    assert!(stdout.contains("2. add b.txt"), "stdout: {stdout}");
    assert_eq!(workspace_files(&env), before);
    assert_eq!(
        std::fs::read_to_string(env.cwd.join("a.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(fake.requests.load(Ordering::SeqCst), 5);
}

/// A credential-shaped value built at runtime, so no secret-looking literal
/// lives in the repository. It matches the AWS-key shape used by the
/// redactor without naming a real key.
fn fake_aws_key() -> String {
    let pid = u64::from(std::process::id());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64;
    format!("AKIA{:016X}", (pid << 32) ^ nanos)
}

#[derive(Clone)]
struct SecretFake {
    calls: std::sync::Arc<AtomicUsize>,
    key: String,
}

async fn secret_chat(State(fake): State<SecretFake>) -> Response {
    let call = fake.calls.fetch_add(1, Ordering::SeqCst);
    let frames = if call == 0 {
        let command = format!("echo {}; seq 1 3000", fake.key);
        let arguments = serde_json::json!({ "command": command }).to_string();
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "choices": [{"delta": {"tool_calls": [{
                    "index": 0,
                    "id": "call_1",
                    "function": {"name": "bash", "arguments": arguments}
                }]}}]
            }),
            serde_json::json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        )
    } else {
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"redaction done\"}}]}\n\n",
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

async fn spawn_secret_server(key: &str) -> (String, SecretFake) {
    let fake = SecretFake {
        calls: std::sync::Arc::new(AtomicUsize::new(0)),
        key: key.to_string(),
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(secret_chat))
        .route("/v1/models", axum::routing::get(models))
        .with_state(fake.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), fake)
}

fn files_with_suffix(root: &std::path::Path, suffix: &str) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with(suffix))
            {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn print_persists_only_redacted_secrets_in_session_and_artifacts() {
    let key = fake_aws_key();
    let (base, fake) = spawn_secret_server(&key).await;
    let env = env();
    // The scripted `bash` call needs approval, which headless cannot give.
    let output = run_tot_persistent(&env, &base, &["--yolo", "--print", "echo the token"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);

    let sessions = files_with_suffix(&env.data_dir.join("sessions"), ".jsonl");
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let raw = std::fs::read_to_string(&sessions[0]).unwrap();
    assert!(!raw.contains(&key), "session JSONL kept the secret:\n{raw}");
    assert!(raw.contains("[REDACTED:aws:AKIA..."), "{raw}");
    let header: serde_json::Value =
        serde_json::from_str(raw.lines().next().expect("header")).unwrap();
    assert_eq!(header["data"]["redact"], true, "{header}");

    let artifacts = files_with_suffix(&env.data_dir.join("sessions"), ".txt");
    assert!(!artifacts.is_empty(), "expected an archived tool result");
    let mut redacted_artifact = false;
    for path in &artifacts {
        let stored = std::fs::read_to_string(path).unwrap();
        assert!(
            !stored.contains(&key),
            "artifact {} kept the secret:\n{stored}",
            path.display()
        );
        redacted_artifact |= stored.contains("[REDACTED:aws:AKIA...");
    }
    assert!(redacted_artifact, "no artifact held the redacted form");
}

#[tokio::test]
async fn no_redact_keeps_raw_output_and_is_recorded_in_the_header() {
    let key = fake_aws_key();
    let (base, _fake) = spawn_secret_server(&key).await;
    let env = env();
    let output = run_tot_persistent(
        &env,
        &base,
        &["--yolo", "--no-redact", "--print", "echo the token"],
    )
    .await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {stdout}\nstderr: {stderr}"
    );

    let sessions = files_with_suffix(&env.data_dir.join("sessions"), ".jsonl");
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    let raw = std::fs::read_to_string(&sessions[0]).unwrap();
    assert!(
        raw.contains(&key),
        "no-redact should keep the raw value:\n{raw}"
    );
    let header: serde_json::Value =
        serde_json::from_str(raw.lines().next().expect("header")).unwrap();
    assert_eq!(header["data"]["redact"], false, "{header}");

    let artifacts = files_with_suffix(&env.data_dir.join("sessions"), ".txt");
    assert!(
        artifacts.iter().any(|path| {
            std::fs::read_to_string(path).is_ok_and(|stored| stored.contains(&key))
        }),
        "no-redact should keep the raw value in the artifact at {}",
        env.data_dir.display()
    );
}

#[tokio::test]
async fn print_in_an_unconfigured_project_names_the_detected_steps_and_runs_none() {
    let (base, _fake) = spawn().await;
    let env = env();
    std::fs::write(env.cwd.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    // A marker a detected step would create if anything ran it.
    let output = run_tot(&env, &base, &["--print", "hi"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "stderr: {stderr}");
    let notices = stderr
        .lines()
        .filter(|line| line.contains("verify: not configured"))
        .count();
    assert_eq!(notices, 1, "stderr: {stderr}");
    assert!(stderr.contains("cargo check --all-targets"), "{stderr}");
    assert!(stderr.contains("tot verify --init"), "{stderr}");
    assert!(!env.cwd.join("target").exists(), "no detected step may run");
}

#[tokio::test]
async fn verify_init_saves_the_detected_steps_for_later_runs() {
    let (base, _fake) = spawn().await;
    let env = env();
    std::fs::write(env.cwd.join("go.mod"), "module example.com/x\n").unwrap();
    let output = run_tot(&env, &base, &["verify"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("verify: not configured"), "{stdout}");

    let output = run_tot(&env, &base, &["verify", "--init"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("go vet ./..."), "{stdout}");

    // Later runs load them without --trust; the notice is gone.
    let output = run_tot(&env, &base, &["verify"]).await;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("verify: 2 step(s) configured"), "{stdout}");
    let output = run_tot(&env, &base, &["--print", "hi"]).await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("verify: not configured"), "{stderr}");
}

//! `tot serve`: drive the HTTP API over loopback against a scripted endpoint.
//!
//! Covers a streamed and cancelled turn, the bearer token (required, never
//! printed or logged), the typed 409 for a concurrent turn, the loopback
//! bind rule, SSE names equal to `--json` names, resumable sessions after
//! SIGTERM, and harness gates that still deny over HTTP.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

const TIMEOUT: Duration = Duration::from_secs(20);

// ---------------------------------------------------------------------------
// Scripted OpenAI-compatible endpoint
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum Script {
    Text(String),
    Tool { name: String, args: Value },
    Slow,
}

#[derive(Clone, Default)]
struct Fake {
    scripts: Arc<Mutex<VecDeque<Script>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

fn sse(value: Value) -> String {
    format!("data: {value}\n\n")
}

async fn chat(State(fake): State<Fake>, body: String) -> Response {
    let value: Value = serde_json::from_str(&body).unwrap_or_default();
    fake.requests.lock().unwrap().push(value);
    let script = fake
        .scripts
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Script::Text("done".into()));
    let (frames, slow) = match script {
        Script::Text(text) => (
            vec![
                sse(json!({"choices":[{"delta":{"content": text}}]})),
                sse(json!({"choices":[{"delta":{},"finish_reason":"stop"}]})),
                "data: [DONE]\n\n".to_string(),
            ],
            false,
        ),
        Script::Tool { name, args } => (
            vec![
                sse(json!({"choices":[{"delta":{"tool_calls":[{
                    "index": 0,
                    "id": "call_1",
                    "function": {"name": name, "arguments": args.to_string()}
                }]}}]})),
                sse(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]})),
                "data: [DONE]\n\n".to_string(),
            ],
            false,
        ),
        Script::Slow => {
            let mut frames: Vec<String> = (0..400)
                .map(|index| {
                    sse(json!({"choices":[{"delta":{"content": format!("chunk-{index} ")}}]}))
                })
                .collect();
            frames.push(sse(
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ));
            frames.push("data: [DONE]\n\n".to_string());
            (frames, true)
        }
    };
    let chunks: Vec<Result<Bytes, Infallible>> = frames
        .into_iter()
        .map(|frame| Ok(Bytes::from(frame)))
        .collect();
    let body = if slow {
        Body::from_stream(stream::unfold(
            (chunks.into_iter(), 0usize),
            |(mut chunks, index)| async move {
                let chunk = chunks.next()?;
                if index > 0 {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                Some((chunk, (chunks, index + 1)))
            },
        ))
    } else {
        Body::from_stream(stream::iter(chunks))
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(body)
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

async fn spawn_endpoint(scripts: Vec<Script>) -> (String, Fake) {
    let fake = Fake {
        scripts: Arc::new(Mutex::new(scripts.into())),
        requests: Arc::new(Mutex::new(Vec::new())),
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

// ---------------------------------------------------------------------------
// Environment and server process
// ---------------------------------------------------------------------------

struct Env {
    _dir: tempfile::TempDir,
    outside: PathBuf,
    root: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    Env {
        config_dir: dir.path().join("config"),
        data_dir: dir.path().join("data"),
        outside,
        root,
        _dir: dir,
    }
}

fn tot(env: &Env, endpoint: &str) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tot"));
    command
        .arg("--endpoint")
        .arg(endpoint)
        .arg("--model")
        .arg("test-model")
        .current_dir(&env.root)
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
        .env_remove("LH_TRUST")
        .env_remove("TOT_NO_SESSION")
        .env_remove("LH_NO_SESSION")
        .env_remove("TOT_DEBUG_CONTENT")
        .env_remove("LH_DEBUG_CONTENT");
    command
}

struct Server {
    child: Child,
    base: String,
    token: String,
    token_file: PathBuf,
    stdout: Arc<Mutex<String>>,
    stderr: Arc<Mutex<String>>,
    http: reqwest::Client,
}

fn capture(pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static, sink: Arc<Mutex<String>>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(pipe).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut sink = sink.lock().unwrap();
            sink.push_str(&line);
            sink.push('\n');
        }
    });
}

impl Server {
    async fn start(env: &Env, endpoint: &str, global: &[&str], serve: &[&str]) -> Self {
        let mut command = tot(env, endpoint);
        command
            .args(global)
            .arg("serve")
            .args(serve)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn tot serve");
        let stdout = Arc::new(Mutex::new(String::new()));
        let stderr = Arc::new(Mutex::new(String::new()));
        capture(child.stdout.take().unwrap(), stdout.clone());
        capture(child.stderr.take().unwrap(), stderr.clone());

        let first = tokio::time::timeout(TIMEOUT, async {
            loop {
                if let Some(line) = stdout.lock().unwrap().lines().next() {
                    return line.to_string();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "tot serve did not start; stderr: {}",
                stderr.lock().unwrap()
            )
        });
        let ready: Value = serde_json::from_str(&first).expect("first stdout line is JSON");
        let base = ready["listening"].as_str().unwrap().to_string();
        let token_file = PathBuf::from(ready["token_file"].as_str().unwrap());
        let token = std::fs::read_to_string(&token_file).unwrap();
        Self {
            child,
            base,
            token,
            token_file,
            stdout,
            stderr,
            http: reqwest::Client::new(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get(&self, path: &str) -> (u16, Value) {
        let response = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        (status, response.json().await.unwrap_or(Value::Null))
    }

    async fn create(&self, body: Value) -> String {
        let (status, value) = self.post("/v1/sessions", body).await;
        assert_eq!(status, 201, "{value}; stderr: {}", self.stderr());
        value["id"].as_str().unwrap().to_string()
    }

    async fn events(&self, id: &str, after: u64) -> Events {
        let response = self
            .http
            .get(self.url(&format!("/v1/sessions/{id}/events?after={after}")))
            .bearer_auth(&self.token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 200);
        assert_eq!(
            response.headers()["content-type"].to_str().unwrap(),
            "text/event-stream"
        );
        Events {
            stream: Box::pin(response.bytes_stream()),
            buffer: String::new(),
        }
    }

    /// Start a turn and collect its frames through `done`.
    async fn turn(&self, id: &str, prompt: &str) -> Vec<SseFrame> {
        let (status, started) = self
            .post(
                &format!("/v1/sessions/{id}/turns"),
                json!({"prompt": prompt}),
            )
            .await;
        assert_eq!(status, 202, "{started}; stderr: {}", self.stderr());
        let mut events = self.events(id, started["after"].as_u64().unwrap()).await;
        events.until_done().await
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    fn stdout(&self) -> String {
        self.stdout.lock().unwrap().clone()
    }

    /// SIGTERM, then wait for a clean exit.
    async fn terminate(&mut self) -> std::process::ExitStatus {
        let pid = self.child.id().expect("server pid") as i32;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
        .unwrap();
        let status = tokio::time::timeout(TIMEOUT, self.child.wait())
            .await
            .expect("tot serve did not stop on SIGTERM")
            .unwrap();
        // Let the capture tasks drain the pipes.
        tokio::time::sleep(Duration::from_millis(100)).await;
        status
    }
}

#[derive(Debug, Clone)]
struct SseFrame {
    id: Option<u64>,
    event: String,
    data: Value,
}

struct Events {
    stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = reqwest::Result<Bytes>> + Send>>,
    buffer: String,
}

impl Events {
    async fn next(&mut self) -> Option<SseFrame> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let block: String = self.buffer.drain(..end + 2).collect();
                let mut frame = SseFrame {
                    id: None,
                    event: String::new(),
                    data: Value::Null,
                };
                let mut comment = true;
                for line in block.lines() {
                    if let Some(value) = line.strip_prefix("id: ") {
                        frame.id = value.parse().ok();
                        comment = false;
                    } else if let Some(value) = line.strip_prefix("event: ") {
                        frame.event = value.to_string();
                        comment = false;
                    } else if let Some(value) = line.strip_prefix("data: ") {
                        frame.data = serde_json::from_str(value).unwrap();
                        comment = false;
                    }
                }
                if comment {
                    continue;
                }
                return Some(frame);
            }
            let chunk = tokio::time::timeout(TIMEOUT, self.stream.next())
                .await
                .expect("SSE stream timed out")?
                .ok()?;
            self.buffer.push_str(&String::from_utf8_lossy(&chunk));
        }
    }

    async fn until_done(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        while let Some(frame) = self.next().await {
            let done = frame.event == "done";
            frames.push(frame);
            if done {
                return frames;
            }
        }
        panic!("the SSE stream ended before done: {frames:?}");
    }
}

fn text_of(frames: &[SseFrame]) -> String {
    frames
        .iter()
        .filter(|frame| frame.event == "text")
        .map(|frame| frame.data["text"].as_str().unwrap_or("").to_string())
        .collect()
}

fn done_status(frames: &[SseFrame]) -> String {
    frames.last().unwrap().data["status"]
        .as_str()
        .unwrap()
        .to_string()
}

fn find_session_file(data_dir: &Path, id: &str) -> PathBuf {
    let name = format!("{id}.jsonl");
    let mut stack = vec![data_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|n| n.to_str()) == Some(name.as_str()) {
                return path;
            }
        }
    }
    panic!("no session file {name} under {}", data_dir.display());
}

fn all_files_text(dir: &Path) -> String {
    let mut text = String::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                text.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
    text
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_script_creates_a_session_streams_a_turn_and_cancels_it() {
    let (endpoint, _fake) =
        spawn_endpoint(vec![Script::Text("hello from serve".into()), Script::Slow]).await;
    let env = env();
    let mut server = Server::start(&env, &endpoint, &[], &[]).await;

    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&server.token_file)
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600, "the token file must be private");

    let (status, health) = server.get("/v1/health").await;
    assert_eq!(status, 200);
    assert_eq!(health["status"], "ok");

    let id = server.create(json!({})).await;
    let (_, list) = server.get("/v1/sessions").await;
    assert_eq!(list["sessions"][0]["id"], id.as_str());
    assert_eq!(list["sessions"][0]["permission_mode"], "normal");

    // A streamed turn.
    let frames = server.turn(&id, "say hello").await;
    assert_eq!(frames.first().unwrap().event, "started", "{frames:?}");
    assert_eq!(text_of(&frames), "hello from serve");
    assert_eq!(done_status(&frames), "Done");
    for pair in frames.windows(2) {
        assert!(pair[0].id < pair[1].id, "event ids must increase");
    }
    // `done` is sent after the session is released.
    let (_, info) = server.get(&format!("/v1/sessions/{id}")).await;
    assert_eq!(info["busy"], false, "{info}");
    assert_eq!(info["last_status"], "Done");

    // A slow turn, cancelled mid-stream.
    let (status, started) = server
        .post(
            &format!("/v1/sessions/{id}/turns"),
            json!({"prompt": "stream a lot"}),
        )
        .await;
    assert_eq!(status, 202);
    let mut events = server.events(&id, started["after"].as_u64().unwrap()).await;
    loop {
        let frame = events.next().await.expect("frame");
        if frame.event == "text" {
            break;
        }
    }
    let (status, cancelled) = server
        .post(&format!("/v1/sessions/{id}/cancel"), json!({}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(cancelled["cancelled"], true);
    let rest = events.until_done().await;
    assert!(rest.iter().any(|frame| frame.event == "cancelled"));
    assert_eq!(done_status(&rest), "Cancelled");
    let cancelled_at = rest
        .iter()
        .position(|frame| frame.event == "cancelled")
        .expect("cancelled frame");
    let done_at = rest
        .iter()
        .position(|frame| frame.event == "done")
        .expect("done frame");
    assert!(
        cancelled_at < done_at,
        "cancelled must be published before the terminal done frame: {rest:?}"
    );
    assert!(
        !text_of(&rest).contains("chunk-399"),
        "the cancelled turn must stop streaming"
    );

    // Cancelling an idle session is not an error.
    let (status, idle) = server
        .post(&format!("/v1/sessions/{id}/cancel"), json!({}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(idle["cancelled"], false);

    let status = server.terminate().await;
    assert!(status.success(), "stderr: {}", server.stderr());
    assert!(
        !server.token_file.exists(),
        "the token file is removed on exit"
    );
}

#[tokio::test]
async fn the_token_is_required_and_never_printed() {
    let (endpoint, _fake) = spawn_endpoint(vec![Script::Text("ok".into())]).await;
    let env = env();
    let mut server = Server::start(&env, &endpoint, &["--debug"], &[]).await;
    let wrong = "wrong-token-0123456789abcdef";

    let http = reqwest::Client::new();
    let missing = http.get(server.url("/v1/health")).send().await.unwrap();
    assert_eq!(missing.status().as_u16(), 401);
    assert!(missing.headers().contains_key("www-authenticate"));
    let body: Value = missing.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unauthorized");

    for path in ["/v1/health", "/v1/sessions", "/v1/sessions/nope/events"] {
        let response = http
            .get(server.url(path))
            .bearer_auth(wrong)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status().as_u16(),
            401,
            "{path}: an unauthenticated caller must not learn whether a session exists"
        );
        let text = response.text().await.unwrap();
        assert!(!text.contains(wrong), "the presented token is not echoed");
        assert!(!text.contains(&server.token));
    }
    // A prefix of the real token is not the token.
    let prefix = &server.token[..server.token.len() - 1];
    let response = http
        .get(server.url("/v1/health"))
        .bearer_auth(prefix)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 401);

    // A browser origin is refused even with the token; no CORS headers.
    let response = http
        .get(server.url("/v1/health"))
        .bearer_auth(&server.token)
        .header("Origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 403);
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );

    // Run a turn so the session file and debug log have content.
    let id = server.create(json!({})).await;
    let frames = server.turn(&id, "hello").await;
    assert_eq!(done_status(&frames), "Done");

    let token = server.token.clone();
    let status = server.terminate().await;
    assert!(status.success());
    let stdout = server.stdout();
    let stderr = server.stderr();
    assert!(
        stdout.contains("token_file"),
        "the path is printed: {stdout}"
    );
    let data = all_files_text(&env.data_dir);
    assert!(
        data.contains("tot serve"),
        "the debug log should have been written"
    );
    for secret in [token.as_str(), wrong] {
        assert!(!stdout.contains(secret), "stdout leaked a token");
        assert!(!stderr.contains(secret), "stderr leaked a token");
        assert!(
            !data.contains(secret),
            "a log or session file under the data dir leaked a token"
        );
    }
}

#[tokio::test]
async fn a_second_concurrent_turn_is_a_typed_conflict() {
    let (endpoint, _fake) = spawn_endpoint(vec![Script::Slow]).await;
    let env = env();
    let mut server = Server::start(&env, &endpoint, &[], &[]).await;
    let id = server.create(json!({})).await;

    let path = format!("/v1/sessions/{id}/turns");
    let (first, second) = tokio::join!(
        server.post(&path, json!({"prompt": "first prompt"})),
        server.post(&path, json!({"prompt": "second prompt"})),
    );
    let mut statuses = [first.0, second.0];
    statuses.sort();
    assert_eq!(statuses, [202, 409], "{first:?} {second:?}");
    let (accepted, conflict) = if first.0 == 202 {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!(conflict.1["error"]["code"], "session_busy", "{conflict:?}");
    assert_eq!(conflict.1["error"]["session_id"], id.as_str());

    // A third attempt while the first still runs is refused the same way.
    let (status, again) = server.post(&path, json!({"prompt": "third"})).await;
    assert_eq!(status, 409, "{again}");

    let mut events = server
        .events(&id, accepted.1["after"].as_u64().unwrap())
        .await;
    let frames = events.until_done().await;
    assert_eq!(done_status(&frames), "Done");
    let status = server.terminate().await;
    assert!(status.success());

    let file = find_session_file(&env.data_dir, &id);
    let content = std::fs::read_to_string(&file).unwrap();
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        let _: Value = serde_json::from_str(line)
            .unwrap_or_else(|error| panic!("corrupt session line {line:?}: {error}"));
    }
    let prompts = [
        content.contains("first prompt"),
        content.contains("second prompt"),
        content.contains("\"third\""),
    ];
    assert_eq!(
        prompts.iter().filter(|seen| **seen).count(),
        1,
        "exactly one prompt reached the session: {prompts:?}"
    );
}

#[tokio::test]
async fn a_non_loopback_bind_requires_the_flag_and_warns() {
    let (endpoint, _fake) = spawn_endpoint(vec![]).await;
    let env = env();

    let output = tokio::time::timeout(
        TIMEOUT,
        tot(&env, &endpoint)
            .args(["serve", "--bind", "0.0.0.0:0"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--allow-non-loopback"), "{stderr}");
    assert!(output.stdout.is_empty(), "nothing listened");

    let mut server = Server::start(
        &env,
        &endpoint,
        &[],
        &["--bind", "0.0.0.0:0", "--allow-non-loopback"],
    )
    .await;
    assert!(server.stderr().contains("warning:"), "{}", server.stderr());
    assert!(server.stderr().contains("non-loopback"));
    let status = server.terminate().await;
    assert!(status.success());
}

#[tokio::test]
async fn sse_event_names_match_the_json_event_names() {
    let env = env();

    // The same script through `tot --print --json` ...
    let (endpoint, _fake) = spawn_endpoint(vec![Script::Text("same answer".into())]).await;
    let output = tokio::time::timeout(
        TIMEOUT,
        tot(&env, &endpoint)
            .args(["--no-session", "--print", "hi", "--json"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(output.status.success());
    let json_types: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            let value: Value = serde_json::from_str(line).unwrap();
            value["type"].as_str().unwrap().to_string()
        })
        .collect();

    // ... and through the SSE stream.
    let (endpoint, _fake) = spawn_endpoint(vec![Script::Text("same answer".into())]).await;
    let mut server = Server::start(&env, &endpoint, &["--no-session"], &[]).await;
    let id = server.create(json!({})).await;
    let frames = server.turn(&id, "hi").await;
    for frame in &frames {
        assert_eq!(
            frame.data["type"].as_str(),
            Some(frame.event.as_str()),
            "the SSE event name is the --json type"
        );
    }
    let sse_types: Vec<String> = frames.iter().map(|frame| frame.event.clone()).collect();
    for expected in ["started", "text", "done"] {
        assert!(
            json_types.iter().any(|name| name == expected),
            "{json_types:?}"
        );
    }
    assert_eq!(sse_types, json_types);
    server.terminate().await;
}

#[tokio::test]
async fn stopping_the_server_leaves_sessions_resumable() {
    let (endpoint, fake) = spawn_endpoint(vec![
        Script::Text("noted".into()),
        Script::Slow,
        Script::Text("the word was pelican".into()),
        Script::Text("still here".into()),
    ])
    .await;
    let env = env();
    let mut server = Server::start(&env, &endpoint, &[], &[]).await;
    let id = server.create(json!({})).await;
    let frames = server.turn(&id, "remember the word pelican").await;
    assert_eq!(done_status(&frames), "Done");

    // Stop the server in the middle of a turn.
    let (status, started) = server
        .post(
            &format!("/v1/sessions/{id}/turns"),
            json!({"prompt": "stream something long"}),
        )
        .await;
    assert_eq!(status, 202);
    let mut events = server.events(&id, started["after"].as_u64().unwrap()).await;
    loop {
        if events.next().await.expect("frame").event == "text" {
            break;
        }
    }
    let status = server.terminate().await;
    assert!(status.success(), "stderr: {}", server.stderr());
    // The open stream saw the cancelled turn settle before it closed.
    let rest = events.until_done().await;
    assert_eq!(done_status(&rest), "Cancelled");

    // The normal session store resumes it.
    let output = tokio::time::timeout(
        TIMEOUT,
        tot(&env, &endpoint)
            .args(["--resume", &id, "--print", "what was the word?", "--json"])
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "resume failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let last = fake.requests.lock().unwrap().last().cloned().unwrap();
    let history = last["messages"].to_string();
    assert!(history.contains("remember the word pelican"), "{history}");
    assert!(history.contains("noted"));
    assert!(history.contains("what was the word?"));

    // And a new server resumes it over HTTP.
    let mut server = Server::start(&env, &endpoint, &[], &[]).await;
    let (status, resumed) = server.post("/v1/sessions", json!({"resume": id})).await;
    assert_eq!(status, 201, "{resumed}");
    assert_eq!(resumed["resumed"], true);
    assert_eq!(resumed["id"], id.as_str());
    let frames = server.turn(&id, "are you there?").await;
    assert_eq!(text_of(&frames), "still here");
    let (status, again) = server.post("/v1/sessions", json!({"resume": id})).await;
    assert_eq!(status, 409, "{again}");
    assert_eq!(again["error"]["code"], "session_already_loaded");
    server.terminate().await;
}

#[tokio::test]
async fn config_allow_and_deny_rules_apply_over_http() {
    let env = env();
    std::fs::create_dir_all(&env.config_dir).unwrap();
    std::fs::write(
        env.config_dir.join("config.toml"),
        r#"
[permissions]
[[permissions.rules]]
id = "allow-touch"
tool = "bash"
command = "^touch marker\\.txt$"
action = "allow"

[[permissions.rules]]
id = "deny-env"
tool = "write"
path = "**/.env"
action = "deny"
"#,
    )
    .unwrap();
    let (endpoint, _fake) = spawn_endpoint(vec![
        Script::Tool {
            name: "bash".into(),
            args: json!({"command": "touch marker.txt"}),
        },
        Script::Text("written".into()),
        Script::Tool {
            name: "write".into(),
            args: json!({"path": ".env", "content": "SECRET=1\n"}),
        },
        Script::Text("denied".into()),
    ])
    .await;
    let mut server = Server::start(&env, &endpoint, &[], &["--allow-yolo"]).await;

    // `ask` normally refuses a command, but the config allow rule decides first.
    let ask = server.create(json!({})).await;
    let frames = server.turn(&ask, "touch the marker").await;
    let finished: Vec<&SseFrame> = frames
        .iter()
        .filter(|frame| frame.event == "tool_finished")
        .collect();
    assert_eq!(finished.len(), 1, "{frames:?}");
    assert_eq!(finished[0].data["is_error"], false, "{frames:?}");
    assert!(env.root.join("marker.txt").exists());

    // A deny rule wins over the mode.
    let frames = server.turn(&ask, "write env").await;
    let finished: Vec<&SseFrame> = frames
        .iter()
        .filter(|frame| frame.event == "tool_finished")
        .collect();
    assert_eq!(finished.len(), 1, "{frames:?}");
    let preview = finished[0].data["preview"].as_str().unwrap();
    assert!(preview.contains("DENIED"), "{preview}");
    assert!(preview.contains("deny-env"), "{preview}");
    assert!(!env.root.join(".env").exists());

    server.terminate().await;
}

#[tokio::test]
async fn harness_gates_still_deny_over_http() {
    let env = env();
    let escaped = env.outside.join("escaped.txt");
    let (endpoint, _fake) = spawn_endpoint(vec![
        // Scope gate: no engagement scope is set.
        Script::Tool {
            name: "bash".into(),
            args: json!({"command": "nmap -sV 10.9.8.7"}),
        },
        Script::Text("scope done".into()),
        // Path jail: outside the session root.
        Script::Tool {
            name: "write".into(),
            args: json!({"path": escaped.display().to_string(), "content": "nope"}),
        },
        Script::Text("jail done".into()),
        // Default `ask` mode: an HTTP session cannot approve a command.
        Script::Tool {
            name: "bash".into(),
            args: json!({"command": "touch marker.txt"}),
        },
        Script::Text("ask done".into()),
    ])
    .await;
    let mut server = Server::start(&env, &endpoint, &[], &["--allow-yolo"]).await;

    // The most permissive mode, so a denial can only come from the harness.
    let yolo = server.create(json!({"permission_mode": "yolo"})).await;
    let frames = server.turn(&yolo, "scan it").await;
    let finished: Vec<&SseFrame> = frames
        .iter()
        .filter(|frame| frame.event == "tool_finished")
        .collect();
    assert_eq!(finished.len(), 1, "{frames:?}");
    assert_eq!(finished[0].data["is_error"], true);
    let preview = finished[0].data["preview"].as_str().unwrap();
    assert!(preview.contains("SEC_SCOPE"), "{preview}");

    let frames = server.turn(&yolo, "write outside").await;
    let finished: Vec<&SseFrame> = frames
        .iter()
        .filter(|frame| frame.event == "tool_finished")
        .collect();
    assert_eq!(finished[0].data["is_error"], true, "{frames:?}");
    let preview = finished[0].data["preview"].as_str().unwrap();
    assert!(
        preview.contains("path_jail") && !preview.contains("DENIED"),
        "the path jail, not the permission policy, refused it: {preview}"
    );
    assert!(!escaped.exists(), "the jailed write must not happen");

    // Default mode: an `ask` has no answerer over HTTP, so it is denied with
    // the policy's typed reason and the next action.
    let ask = server.create(json!({})).await;
    let frames = server.turn(&ask, "touch it").await;
    let finished: Vec<&SseFrame> = frames
        .iter()
        .filter(|frame| frame.event == "tool_finished")
        .collect();
    let preview = finished[0].data["preview"].as_str().unwrap();
    assert!(preview.contains("DENIED"), "{preview}");
    assert!(preview.contains("allow rule"), "{preview}");
    assert!(preview.contains("accept-edits"), "{preview}");
    assert!(!env.root.join("marker.txt").exists());

    // `yolo` cannot be selected unless the operator allowed it.
    server.terminate().await;
    let mut server = Server::start(&env, &endpoint, &[], &[]).await;
    let (status, refused) = server
        .post("/v1/sessions", json!({"permission_mode": "yolo"}))
        .await;
    assert_eq!(status, 403, "{refused}");
    assert_eq!(refused["error"]["code"], "mode_not_permitted");
    server.terminate().await;
}

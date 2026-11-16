//! ACP v1 conformance: drive `lh acp` over stdio against a scripted endpoint.
//!
//! Covers initialization, a streamed turn, cancellation, root enforcement, a
//! denied permission with no side effect, and the typed terminal refusal.

use std::collections::VecDeque;
use std::convert::Infallible;
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
use futures_util::stream;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};

const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
enum Script {
    Text(String),
    /// Content deltas sent as separate SSE frames, so an escape sequence can
    /// straddle two chunks.
    Split(Vec<String>),
    Tool {
        name: String,
        args: Value,
    },
    Slow,
    Fail,
}

#[derive(Clone, Default)]
struct Fake {
    scripts: Arc<Mutex<VecDeque<Script>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Fake {
    fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Arc::new(Mutex::new(scripts.into())),
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
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
        Script::Split(parts) => {
            let mut frames: Vec<String> = parts
                .into_iter()
                .map(|part| sse(json!({"choices":[{"delta":{"content": part}}]})))
                .collect();
            frames.push(sse(
                json!({"choices":[{"delta":{},"finish_reason":"stop"}]}),
            ));
            frames.push("data: [DONE]\n\n".to_string());
            (frames, false)
        }
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
        Script::Fail => {
            // A permanent failure: a 5xx would be retried within the turn.
            return Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .header(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("application/json"),
                )
                .body(Body::from(
                    r#"{"error":{"message":"scripted provider failure"}}"#,
                ))
                .unwrap();
        }
        Script::Slow => {
            let mut frames: Vec<String> = (0..200)
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
        let stream = stream::unfold(
            (chunks.into_iter(), 0usize),
            |(mut chunks, index)| async move {
                let chunk = chunks.next()?;
                if index > 0 {
                    tokio::time::sleep(Duration::from_millis(15)).await;
                }
                Some((chunk, (chunks, index + 1)))
            },
        );
        Body::from_stream(stream)
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
    let fake = Fake::new(scripts);
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

#[derive(Clone, Copy)]
enum Permission {
    Allow,
    Deny,
}

struct Client {
    _child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<String>>,
    permissions: Arc<std::sync::atomic::AtomicUsize>,
}

impl Client {
    async fn spawn(base: &str, root: &std::path::Path) -> Self {
        Self::spawn_with(base, root, &[]).await
    }

    async fn spawn_with(base: &str, root: &std::path::Path, extra: &[&str]) -> Self {
        let config_dir = root.parent().unwrap().join("config");
        let data_dir = root.parent().unwrap().join("data");
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_lh"));
        command
            .arg("--endpoint")
            .arg(base)
            .arg("--model")
            .arg("test-model")
            .args(extra)
            .arg("acp")
            .current_dir(root)
            .env("LH_CONFIG_DIR", &config_dir)
            .env("LH_DATA_DIR", &data_dir)
            .env("LH_SKIP_TOOL_PROBE", "1")
            .env_remove("LH_ENDPOINT")
            .env_remove("LH_MODEL")
            .env_remove("LH_API_KEY")
            .env_remove("LH_PROFILE")
            .env_remove("LH_TRUST")
            .env_remove("LH_NO_SESSION")
            .env_remove("LH_DEBUG_CONTENT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().expect("spawn lh acp");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = Arc::new(Mutex::new(String::new()));
        if let Some(pipe) = child.stderr.take() {
            let sink = stderr.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    sink.lock().unwrap().push_str(&line);
                    sink.lock().unwrap().push('\n');
                }
            });
        }
        Self {
            _child: child,
            stdin,
            lines: BufReader::new(stdout).lines(),
            stderr,
            permissions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    async fn send(&mut self, message: Value) {
        self.send_raw(&message.to_string()).await;
    }

    async fn send_raw(&mut self, line: &str) {
        let mut line = line.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn read(&mut self) -> Value {
        let line = tokio::time::timeout(TIMEOUT, self.lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("ACP stdout timed out; stderr: {}", self.stderr()))
            .expect("ACP stdout read")
            .unwrap_or_else(|| panic!("ACP process closed stdout; stderr: {}", self.stderr()));
        serde_json::from_str(&line).unwrap_or_else(|error| {
            panic!("invalid JSON {line:?}: {error}; stderr: {}", self.stderr())
        })
    }

    async fn stop(mut self) {
        let _ = self._child.kill().await;
        let _ = self._child.wait().await;
    }

    fn stderr(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }

    async fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
        permission: Permission,
    ) -> (Value, Vec<Value>) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        let mut notifications = Vec::new();
        loop {
            let message = self.read().await;
            if message.get("method").is_none() && message.get("id") == Some(&json!(id)) {
                return (message, notifications);
            }
            match message.get("method").and_then(Value::as_str) {
                Some("session/request_permission") => {
                    self.permissions
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let outcome = match permission {
                        Permission::Allow => {
                            json!({"outcome": {"outcome": "selected", "optionId": "allow"}})
                        }
                        Permission::Deny => {
                            json!({"outcome": {"outcome": "selected", "optionId": "reject"}})
                        }
                    };
                    self.send(json!({
                        "jsonrpc": "2.0",
                        "id": message["id"],
                        "result": outcome,
                    }))
                    .await;
                }
                Some(_) => notifications.push(message),
                None => panic!("unexpected ACP message: {message}"),
            }
        }
    }

    async fn new_session(&mut self, id: u64, root: &std::path::Path) -> String {
        let (response, _) = self
            .request(
                id,
                "session/new",
                json!({"cwd": root.display().to_string(), "mcpServers": []}),
                Permission::Allow,
            )
            .await;
        response["result"]["sessionId"]
            .as_str()
            .unwrap_or_else(|| panic!("session/new failed: {response}"))
            .to_string()
    }
}

fn workdir() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("work");
    std::fs::create_dir_all(&root).unwrap();
    (dir, root)
}

fn updates<'a>(notifications: &'a [Value], kind: &str) -> Vec<&'a Value> {
    notifications
        .iter()
        .filter(|message| {
            message.get("method").and_then(Value::as_str) == Some("session/update")
                && message
                    .pointer("/params/update/sessionUpdate")
                    .and_then(Value::as_str)
                    == Some(kind)
        })
        .collect()
}

fn text_of(notification: &&Value) -> String {
    notification
        .pointer("/params/update/content/text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn initialize_and_new_session_negotiate_capabilities() {
    let (base, _fake) = spawn_endpoint(vec![]).await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;

    let (response, _) = client
        .request(
            1,
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {"terminal": false},
                "clientInfo": {"name": "conformance", "version": "1"},
            }),
            Permission::Allow,
        )
        .await;
    assert_eq!(response["result"]["protocolVersion"], 1, "{response}");
    assert_eq!(response["result"]["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        response["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        false
    );
    assert_eq!(response["result"]["agentInfo"]["name"], "localharness");

    let session_id = client.new_session(2, &root).await;
    assert!(!session_id.is_empty());

    // Loading a session that is already live is a typed refusal rather than
    // a second runtime fighting over the same session file.
    let (response, _) = client
        .request(
            20,
            "session/load",
            json!({
                "sessionId": session_id,
                "cwd": root.display().to_string(),
                "mcpServers": [],
            }),
            Permission::Allow,
        )
        .await;
    assert_eq!(
        response["error"]["data"]["code"], "session_already_loaded",
        "{response}"
    );

    // An unknown method is a typed method-not-found, not a crash.
    let (response, _) = client
        .request(3, "session/nope", json!({}), Permission::Allow)
        .await;
    assert_eq!(response["error"]["code"], -32601);
    assert_eq!(response["error"]["data"]["code"], "method_not_found");
}

#[tokio::test]
async fn ansi_and_osc_never_reach_the_client() {
    let (base, _fake) = spawn_endpoint(vec![
        Script::Split(vec![
            "safe \u{1b}".into(),
            "]0;pwn\u{7}".into(),
            "done".into(),
        ]),
        Script::Tool {
            name: "write".into(),
            args: json!({
                "path": "\u{1b}]0;pwn\u{7}file.txt",
                "content": "x",
            }),
        },
        Script::Text("finish".into()),
    ])
    .await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;

    let (response, notifications) = client
        .request(
            3,
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "go"}]}),
            Permission::Allow,
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let text = updates(&notifications, "agent_message_chunk")
        .iter()
        .map(text_of)
        .collect::<String>();
    assert!(text.contains("safe"), "{text}");
    assert!(text.contains("done"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");
    assert!(!text.contains("pwn"), "{text}");

    let (response, notifications) = client
        .request(
            4,
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "write"}]}),
            Permission::Allow,
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let serialized = serde_json::to_string(&notifications).unwrap();
    assert!(!serialized.contains("\u{1b}"), "{serialized}");
    assert!(!serialized.contains("pwn"), "{serialized}");
}

#[tokio::test]
async fn a_foreign_root_is_untrusted_even_when_launched_with_trust() {
    let (base, _fake) = spawn_endpoint(vec![]).await;
    let (dir, root) = workdir();
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let mut client = Client::spawn_with(&base, &root, &["--trust"]).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;

    let (response, _) = client
        .request(
            2,
            "session/new",
            json!({"cwd": root.display().to_string(), "mcpServers": []}),
            Permission::Allow,
        )
        .await;
    assert_eq!(
        response["result"]["_meta"]["localharness"]["trusted"], true,
        "{response}"
    );

    let (response, _) = client
        .request(
            3,
            "session/new",
            json!({"cwd": other.display().to_string(), "mcpServers": []}),
            Permission::Allow,
        )
        .await;
    assert_eq!(
        response["result"]["_meta"]["localharness"]["trusted"], false,
        "a root outside the process working directory must not inherit trust: {response}"
    );
}

#[tokio::test]
async fn absolute_paths_and_symlinks_cannot_escape_the_root() {
    let (base, _fake) = spawn_endpoint(vec![
        Script::Tool {
            name: "write".into(),
            args: json!({"path": "/etc/localharness-acp-escape", "content": "nope"}),
        },
        Script::Text("one".into()),
        Script::Tool {
            name: "write".into(),
            args: json!({"path": "link/escaped.txt", "content": "nope"}),
        },
        Script::Text("two".into()),
    ])
    .await;
    let (dir, root) = workdir();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;

    for id in [3, 4] {
        let (response, _) = client
            .request(
                id,
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{"type": "text", "text": "write outside"}],
                }),
                Permission::Allow,
            )
            .await;
        assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    }
    assert!(!std::path::Path::new("/etc/localharness-acp-escape").exists());
    assert!(!outside.join("escaped.txt").exists());
    assert_eq!(
        client.permissions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the path gate must refuse before a permission prompt"
    );
}

#[tokio::test]
async fn a_second_prompt_on_one_session_is_rejected() {
    let (base, _fake) = spawn_endpoint(vec![Script::Slow]).await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;

    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": "one"}]},
        }))
        .await;
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/prompt",
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": "two"}]},
        }))
        .await;

    loop {
        let message = client.read().await;
        if message.get("method").is_none() && message.get("id") == Some(&json!(4)) {
            assert_eq!(message["error"]["data"]["code"], "session_busy");
            break;
        }
    }
    client
        .send(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": {"sessionId": session},
        }))
        .await;
    loop {
        let message = client.read().await;
        if message.get("method").is_none() && message.get("id") == Some(&json!(3)) {
            assert_eq!(message["result"]["stopReason"], "cancelled", "{message}");
            break;
        }
    }
}

#[tokio::test]
async fn malformed_json_is_a_parse_error() {
    let (base, _fake) = spawn_endpoint(vec![]).await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client.send_raw("this is not json").await;
    let message = client.read().await;
    assert_eq!(message["error"]["code"], -32700, "{message}");
}

#[tokio::test]
async fn a_turn_error_is_a_jsonrpc_error_not_a_refusal() {
    let (base, _fake) = spawn_endpoint(vec![Script::Fail]).await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;
    let (response, _) = client
        .request(
            3,
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "go"}]}),
            Permission::Allow,
        )
        .await;
    assert_eq!(response["error"]["code"], -32000, "{response}");
    assert_eq!(response["error"]["data"]["status"], "error", "{response}");
    assert!(response.get("result").is_none(), "{response}");
}

#[tokio::test]
async fn a_prompt_streams_text_and_ends_the_turn() {
    let (base, fake) = spawn_endpoint(vec![Script::Text("Hello from ACP".into())]).await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;

    let (response, notifications) = client
        .request(
            3,
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "say hello"}],
            }),
            Permission::Allow,
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    let chunks = updates(&notifications, "agent_message_chunk");
    assert!(
        chunks
            .iter()
            .any(|chunk| text_of(chunk) == "Hello from ACP"),
        "{notifications:?}"
    );
    assert_eq!(fake.requests.lock().unwrap().len(), 1);
    assert_eq!(client.stderr(), "", "ACP must not write to stderr");
}

#[tokio::test]
async fn cancel_stops_a_streaming_turn() {
    let (base, _fake) = spawn_endpoint(vec![Script::Slow]).await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;

    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": "go"}]},
        }))
        .await;

    let mut saw_text = false;
    let stop;
    loop {
        let message = client.read().await;
        if message.get("method").is_none() && message.get("id") == Some(&json!(3)) {
            stop = message;
            break;
        }
        if message.get("method").and_then(Value::as_str) == Some("session/update")
            && message
                .pointer("/params/update/sessionUpdate")
                .and_then(Value::as_str)
                == Some("agent_message_chunk")
        {
            saw_text = true;
            client
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": {"sessionId": session},
                }))
                .await;
        }
    }
    assert!(saw_text, "the turn should stream before it is cancelled");
    assert_eq!(stop["result"]["stopReason"], "cancelled", "{stop}");
}

#[tokio::test]
async fn ephemeral_sessions_get_distinct_ids() {
    let (base, _fake) = spawn_endpoint(vec![]).await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn_with(&base, &root, &["--no-session"]).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let first = client.new_session(2, &root).await;
    let second = client.new_session(3, &root).await;
    assert_ne!(first, second, "ephemeral ACP sessions must not share an id");

    let (response, _) = client
        .request(
            4,
            "session/load",
            json!({"sessionId": first, "cwd": root.display().to_string(), "mcpServers": []}),
            Permission::Allow,
        )
        .await;
    assert_eq!(
        response["error"]["data"]["code"], "unsupported_capability",
        "{response}"
    );
    assert_eq!(response["error"]["data"]["capability"], "loadSession");
}

#[tokio::test]
async fn session_load_replays_the_conversation() {
    let (base, _fake) = spawn_endpoint(vec![
        Script::Tool {
            name: "read".into(),
            args: json!({"path": "file.txt"}),
        },
        Script::Text("remembered answer".into()),
    ])
    .await;
    let (_dir, root) = workdir();
    std::fs::write(root.join("file.txt"), "hello\n").unwrap();
    let mut first = Client::spawn(&base, &root).await;
    first
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = first.new_session(2, &root).await;
    let (response, _) = first
        .request(
            3,
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "remember this"}],
            }),
            Permission::Allow,
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    first.stop().await;

    let mut second = Client::spawn(&base, &root).await;
    second
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let (response, notifications) = second
        .request(
            2,
            "session/load",
            json!({
                "sessionId": session,
                "cwd": root.display().to_string(),
                "mcpServers": [],
            }),
            Permission::Allow,
        )
        .await;
    assert!(response["result"].is_object(), "{response}");
    let user = updates(&notifications, "user_message_chunk");
    assert!(
        user.iter().any(|chunk| text_of(chunk) == "remember this"),
        "{notifications:?}"
    );
    let agent = updates(&notifications, "agent_message_chunk");
    assert!(
        agent
            .iter()
            .any(|chunk| text_of(chunk) == "remembered answer"),
        "{notifications:?}"
    );
}

#[tokio::test]
async fn cancel_while_waiting_for_permission_leaves_no_side_effect() {
    let marker = "cancel-marker.txt";
    let (base, _fake) = spawn_endpoint(vec![
        Script::Tool {
            name: "bash".into(),
            args: json!({"command": format!("touch {marker}")}),
        },
        Script::Text("done".into()),
    ])
    .await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;
    client
        .send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {"sessionId": session, "prompt": [{"type": "text", "text": "touch it"}]},
        }))
        .await;

    let mut asked = false;
    let stop;
    loop {
        let message = client.read().await;
        if message.get("method").and_then(Value::as_str) == Some("session/request_permission") {
            asked = true;
            assert!(!root.join(marker).exists(), "nothing runs before approval");
            client
                .send(json!({
                    "jsonrpc": "2.0",
                    "method": "session/cancel",
                    "params": {"sessionId": session},
                }))
                .await;
            continue;
        }
        if message.get("method").is_none() && message.get("id") == Some(&json!(3)) {
            stop = message;
            break;
        }
    }
    assert!(asked, "the tool call must have reached the permission gate");
    assert_eq!(stop["result"]["stopReason"], "cancelled", "{stop}");
    assert!(
        !root.join(marker).exists(),
        "a cancelled permission request must not run the command"
    );
}

#[tokio::test]
async fn a_path_outside_the_declared_root_is_denied() {
    let (base, _fake) = spawn_endpoint(vec![
        Script::Tool {
            name: "write".into(),
            args: json!({"path": "../escaped.txt", "content": "nope"}),
        },
        Script::Text("done".into()),
    ])
    .await;
    let (dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;

    let (response, notifications) = client
        .request(
            3,
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "write outside"}],
            }),
            Permission::Allow,
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        !dir.path().join("escaped.txt").exists(),
        "a path outside the root must not be written, even with client approval"
    );
    assert!(!root.join("../escaped.txt").exists());
    let failed = updates(&notifications, "tool_call_update");
    assert!(
        failed.iter().any(|update| update
            .pointer("/params/update/status")
            .and_then(Value::as_str)
            == Some("failed")),
        "the denied write should report a failed tool call: {notifications:?}"
    );
}

#[tokio::test]
async fn denying_permission_leaves_no_side_effect() {
    let marker = "permission-marker.txt";
    let (base, _fake) = spawn_endpoint(vec![
        Script::Tool {
            name: "bash".into(),
            args: json!({"command": format!("touch {marker}")}),
        },
        Script::Text("done".into()),
    ])
    .await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;
    let session = client.new_session(2, &root).await;

    let (response, _notifications) = client
        .request(
            3,
            "session/prompt",
            json!({
                "sessionId": session,
                "prompt": [{"type": "text", "text": "touch it"}],
            }),
            Permission::Deny,
        )
        .await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert_eq!(
        client.permissions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the client must have been asked before the command ran"
    );
    assert!(
        !root.join(marker).exists(),
        "a denied tool call must not have run"
    );
}

#[tokio::test]
async fn terminal_access_is_absent_and_typed() {
    let (base, _fake) = spawn_endpoint(vec![]).await;
    let (_dir, root) = workdir();
    let mut client = Client::spawn(&base, &root).await;
    client
        .request(
            1,
            "initialize",
            json!({"protocolVersion": 1}),
            Permission::Allow,
        )
        .await;

    let (response, _) = client
        .request(
            2,
            "terminal/create",
            json!({"sessionId": "s", "command": "sh", "args": ["-c", "true"]}),
            Permission::Allow,
        )
        .await;
    assert_eq!(response["error"]["data"]["code"], "unsupported_capability");
    assert_eq!(response["error"]["data"]["capability"], "terminal");
}

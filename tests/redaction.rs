//! Redaction must never change what the model reads or writes.
//!
//! Regression for a benchmark run where redaction rewrote ordinary Python in
//! a `bash` result (`name = Column(Text)` became `name = [REDACTED:env:...]`)
//! and the model wrote the placeholders back into the file. The fake model
//! here echoes back the file text it actually received, so a placeholder in
//! the tool result would end up in the file.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures_util::stream;
use serde_json::{Value, json};

/// The lines from the report, in a file shaped like the one it corrupted.
const TASK_STORAGE: &str = "\
from sqlalchemy import Column, Integer, Text
from sqlalchemy.orm import declarative_base

Base = declarative_base()


class Task(Base):
    __tablename__ = \"tasks\"
    id = Column(Integer, primary_key=True)
    name = Column(Text)


class TaskStorage:
    def load(self, path):
        data = open(path).read()
        return data

    def get(self, task_id, user_id):
        task = self.getbyid(task_id, user_id)
        return task
";

const BEGIN: &str = "__LH_BEGIN__";
const END: &str = "__LH_END__";

/// Credential-shaped values built at runtime so no secret-looking literal
/// lives in the repository (see CONTRIBUTING).
fn fake_secrets() -> (String, String) {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let seed = (u64::from(std::process::id()) << 32) ^ nanos;
    let aws = format!("AKIA{seed:016X}");
    let password = format!("pw-{:x}-{:x}", seed.rotate_left(17), seed ^ 0x5a5a_5a5a);
    (aws, password)
}

#[derive(Clone)]
enum Script {
    /// Read the file, print it with bash, then write back what bash showed.
    ReadThenWriteBack,
    /// After a resume: copy a replayed placeholder into a new file.
    CopyPlaceholder,
}

#[derive(Clone)]
struct Fake {
    script: Script,
    bodies: Arc<Mutex<Vec<Value>>>,
}

fn tool_call_frames(calls: &[(&str, &str, Value)]) -> String {
    let calls: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            json!({
                "index": index,
                "id": id,
                "function": {"name": name, "arguments": arguments.to_string()}
            })
        })
        .collect();
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"tool_calls": calls}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
    )
}

fn text_frames(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": text}}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    )
}

/// Tool messages in a chat request, in order.
fn tool_messages(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .map(|messages| {
            messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .filter_map(|message| message["content"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// The file text exactly as the model saw it between the bash markers.
fn between_markers(text: &str) -> Option<String> {
    let start = text.find(&format!("{BEGIN}\n"))? + BEGIN.len() + 1;
    let end = text[start..].find(END)? + start;
    Some(text[start..end].to_string())
}

async fn chat(State(fake): State<Fake>, body: String) -> Response {
    let body: Value = serde_json::from_str(&body).unwrap_or_default();
    let frames = {
        let mut bodies = fake.bodies.lock().unwrap();
        bodies.push(body.clone());
        let turn = bodies.len();
        match (&fake.script, turn) {
            (Script::ReadThenWriteBack, 1) => tool_call_frames(&[
                (
                    "call_read",
                    "read",
                    json!({"path": "mycode/task_storage.py"}),
                ),
                (
                    "call_cat",
                    "bash",
                    json!({"command": format!(
                        "echo {BEGIN}; cat mycode/task_storage.py; echo {END}; cat .env"
                    )}),
                ),
            ]),
            (Script::ReadThenWriteBack, 2) => {
                let seen = tool_messages(&body)
                    .iter()
                    .find_map(|content| between_markers(content))
                    .unwrap_or_else(|| "(the bash result was missing)".into());
                tool_call_frames(&[(
                    "call_write",
                    "write",
                    json!({"path": "mycode/task_storage.py", "content": seen}),
                )])
            }
            (Script::CopyPlaceholder, 1) => {
                let replayed = tool_messages(&body).join("\n");
                let placeholder = replayed
                    .split("AWS_ACCESS_KEY_ID=")
                    .nth(1)
                    .and_then(|rest| rest.split_whitespace().next())
                    .unwrap_or("(no placeholder replayed)")
                    .to_string();
                tool_call_frames(&[(
                    "call_copy",
                    "write",
                    json!({
                        "path": "copy.env",
                        "content": format!("AWS_ACCESS_KEY_ID={placeholder}\n"),
                    }),
                )])
            }
            _ => text_frames("done"),
        }
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
                .chunks(13)
                .map(|chunk| Ok::<Bytes, Infallible>(Bytes::copy_from_slice(chunk)))
                .collect::<Vec<_>>(),
        )))
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

async fn spawn(script: Script) -> (String, Fake) {
    let fake = Fake {
        script,
        bodies: Arc::new(Mutex::new(Vec::new())),
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
    std::fs::create_dir_all(cwd.join("mycode")).unwrap();
    Env {
        _dir: dir,
        config_dir,
        data_dir,
        cwd,
    }
}

/// Run `tot` with a persistent session (no `--no-session`).
async fn run_tot(env: &Env, base: &str, extra: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tot"));
    command
        .arg("--endpoint")
        .arg(base)
        .arg("--model")
        .arg("test-model")
        // The fake model runs `bash` and `write` unattended. Headless runs
        // default to the `ask` permission mode and deny both, so opt in
        // explicitly; this file tests redaction, not permissions.
        .arg("--yolo")
        .args(extra)
        .current_dir(&env.cwd)
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
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), command.output())
        .await
        .expect("tot timed out")
        .expect("cannot run tot");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
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
            } else if path.to_string_lossy().ends_with(suffix) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn session_text(env: &Env) -> String {
    let sessions = files_with_suffix(&env.data_dir.join("sessions"), ".jsonl");
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    std::fs::read_to_string(&sessions[0]).unwrap()
}

/// Set up the report's file and a `.env` with runtime-built secrets, then
/// run the read / print / write-back turn.
async fn write_back_turn(env: &Env) -> (Fake, String, String) {
    let (aws, password) = fake_secrets();
    std::fs::write(env.cwd.join("mycode/task_storage.py"), TASK_STORAGE).unwrap();
    std::fs::write(
        env.cwd.join(".env"),
        format!("AWS_ACCESS_KEY_ID={aws}\nDB_PASSWORD={password}\nPORT=8080\n"),
    )
    .unwrap();
    let (base, fake) = spawn(Script::ReadThenWriteBack).await;
    run_tot(env, &base, &["--print", "fix task storage"]).await;
    (fake, aws, password)
}

#[tokio::test]
async fn source_code_reaches_the_model_and_the_file_byte_for_byte() {
    let env = env();
    let (fake, aws, password) = write_back_turn(&env).await;
    let bodies = fake.bodies.lock().unwrap().clone();
    assert!(bodies.len() >= 3, "expected read, write, and final turns");

    // What the model saw in the bash result is the file, byte for byte.
    let results = tool_messages(&bodies[1]);
    let seen = results
        .iter()
        .find_map(|content| between_markers(content))
        .expect("bash result with markers");
    assert_eq!(seen, TASK_STORAGE);
    for content in &results {
        assert!(!content.contains("[REDACTED"), "tool result:\n{content}");
    }
    // The read tool result has every line verbatim too.
    let read = results
        .iter()
        .find(|content| content.contains("[tool] read"))
        .expect("read result");
    for line in TASK_STORAGE.lines().filter(|line| !line.is_empty()) {
        assert!(read.contains(line), "read result lost {line:?}:\n{read}");
    }

    // The write the model sent back left the file unchanged.
    let written = std::fs::read_to_string(env.cwd.join("mycode/task_storage.py")).unwrap();
    assert_eq!(written, TASK_STORAGE);
    let write_result = tool_messages(&bodies[2]).join("\n");
    assert!(
        write_result.contains("wrote mycode/task_storage.py"),
        "{write_result}"
    );

    // The live model saw the real .env values; the session file did not keep
    // them, but kept the source code readable.
    assert!(results.iter().any(|content| content.contains(&aws)));
    let session = session_text(&env);
    assert!(!session.contains(&aws), "session kept the AWS key");
    assert!(!session.contains(&password), "session kept the password");
    assert!(session.contains("AWS_ACCESS_KEY_ID=[REDACTED:aws:AKIA..."));
    assert!(session.contains("DB_PASSWORD=[REDACTED:env:"));
    assert!(session.contains("PORT=8080"));
    assert!(session.contains("name = Column(Text)"), "{session}");
    assert!(session.contains("Base = declarative_base()"));
    assert!(session.contains("task = self.getbyid(task_id, user_id)"));
}

#[tokio::test]
async fn a_resumed_placeholder_is_annotated_and_never_written() {
    let env = env();
    let (_fake, aws, _password) = write_back_turn(&env).await;

    let (base, fake) = spawn(Script::CopyPlaceholder).await;
    run_tot(&env, &base, &["--continue", "--print", "copy the env file"]).await;
    let bodies = fake.bodies.lock().unwrap().clone();

    // The replayed result is the saved, redacted form, with a note saying
    // so; the secret itself is not replayed.
    let replayed = tool_messages(&bodies[0]).join("\n");
    assert!(!replayed.contains(&aws), "replay leaked the key");
    assert!(
        replayed.contains("AWS_ACCESS_KEY_ID=[REDACTED:aws:AKIA..."),
        "{replayed}"
    );
    assert!(
        replayed.contains("are not file content"),
        "no placeholder note:\n{replayed}"
    );
    // The source file in the same replayed result is intact.
    assert!(replayed.contains("name = Column(Text)"), "{replayed}");

    // Copying the placeholder into a file is refused and writes nothing.
    assert!(
        !env.cwd.join("copy.env").exists(),
        "placeholder was written"
    );
    let refusal = tool_messages(&bodies[1]).join("\n");
    assert!(
        refusal.contains("redaction placeholder"),
        "write was not refused:\n{refusal}"
    );
}

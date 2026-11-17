//! Workflow CLI end to end: approval gates, artifacts, and resume.
//!
//! These use approval-only workflows, so no model is needed. The engine's
//! prompt-step behavior is covered by unit tests in `src/workflows/engine.rs`.

use std::convert::Infallible;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use axum::routing::post;
use bytes::Bytes;
use futures_util::stream;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

#[derive(Clone, Default)]
struct Fake {
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn chat(State(fake): State<Fake>, body: String) -> Response {
    fake.requests
        .lock()
        .unwrap()
        .push(serde_json::from_str(&body).unwrap_or_default());
    let frames = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let chunks: Vec<Result<Bytes, Infallible>> = frames
        .as_bytes()
        .chunks(9)
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

async fn spawn_endpoint() -> (String, Fake) {
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
    config_dir: PathBuf,
    data_dir: PathBuf,
    cwd: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    let data_dir = dir.path().join("data");
    let cwd = dir.path().join("work");
    std::fs::create_dir_all(cwd.join(".harness/workflows")).unwrap();
    std::fs::create_dir_all(config_dir.join("workflows")).unwrap();
    Env {
        _dir: dir,
        config_dir,
        data_dir,
        cwd,
    }
}

fn write_workflow(env: &Env, name: &str, body: &str) {
    std::fs::write(
        env.config_dir
            .join("workflows")
            .join(format!("{name}.toml")),
        body,
    )
    .unwrap();
}

async fn run(env: &Env, args: &[&str], stdin: &str) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_tot"));
    command
        .args(args)
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
        .env_remove("LH_TRUST")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn tot");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("tot timed out")
        .expect("tot run")
}

fn journal(env: &Env, name: &str) -> Option<Value> {
    let sessions = env.data_dir.join("sessions");
    let entries = std::fs::read_dir(sessions).ok()?;
    for entry in entries.flatten() {
        let path = entry.path().join("workflows").join(format!("{name}.json"));
        if path.is_file() {
            let raw = std::fs::read_to_string(path).unwrap();
            return Some(serde_json::from_str(&raw).unwrap());
        }
    }
    None
}

const GUARD: &str = r#"
name = "guard"
description = "Approval only"
[[step]]
id = "push"
run = "approval"
message = "Push the branch?"
"#;

#[tokio::test]
async fn accepting_an_approval_records_the_decision() {
    let env = env();
    write_workflow(&env, "guard", GUARD);
    let output = run(&env, &["workflow", "run", "guard"], "y\n").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let journal = journal(&env, "guard").expect("journal");
    assert_eq!(journal["status"], "completed");
    assert_eq!(journal["approvals"][0]["decision"], "accepted");
    assert_eq!(journal["steps"][0]["status"], "completed");
}

#[tokio::test]
async fn denying_an_approval_stops_and_records_it() {
    let env = env();
    write_workflow(&env, "guard", GUARD);
    let output = run(&env, &["workflow", "run", "guard"], "n\n").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("workflow_approval_denied") && stderr.contains("push"),
        "{stderr}"
    );
    let journal = journal(&env, "guard").expect("journal");
    assert_eq!(journal["status"], "failed");
    assert_eq!(journal["approvals"][0]["decision"], "denied");
}

#[tokio::test]
async fn a_missing_artifact_fails_before_success() {
    let env = env();
    write_workflow(
        &env,
        "artifact",
        r#"
name = "artifact"
artifacts = ["out/report.md"]
[[step]]
id = "go"
run = "approval"
message = "Go?"
"#,
    );
    let output = run(&env, &["workflow", "run", "artifact"], "y\n").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("workflow_artifact_missing") && stderr.contains("out/report.md"),
        "{stderr}"
    );
    assert_eq!(journal(&env, "artifact").unwrap()["status"], "failed");

    std::fs::create_dir_all(env.cwd.join("out")).unwrap();
    std::fs::write(env.cwd.join("out/report.md"), "report\n").unwrap();
    let output = run(&env, &["workflow", "run", "artifact"], "y\n").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert_eq!(journal(&env, "artifact").unwrap()["status"], "completed");
}

#[tokio::test]
async fn resume_reopens_the_run_and_records_the_new_decision() {
    let env = env();
    write_workflow(&env, "guard", GUARD);
    let output = run(&env, &["workflow", "run", "guard"], "n\n").await;
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(journal(&env, "guard").unwrap()["status"], "failed");

    let output = run(&env, &["workflow", "run", "guard", "--resume"], "y\n").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let journal = journal(&env, "guard").expect("journal");
    assert_eq!(journal["status"], "completed");
    assert_eq!(journal["approvals"].as_array().unwrap().len(), 2);
    assert_eq!(journal["approvals"][1]["decision"], "accepted");
}

#[tokio::test]
async fn a_condition_that_is_not_a_fixed_fact_is_rejected() {
    let env = env();
    write_workflow(
        &env,
        "bad",
        r#"
name = "bad"
[[step]]
id = "go"
prompt = "do it"
when = "changed_files > 0 || true"
"#,
    );
    let output = run(&env, &["--no-session", "workflow", "run", "bad"], "").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("condition") && stderr.contains("fixed")
            || stderr.contains("changed_files"),
        "{stderr}"
    );
    assert!(journal(&env, "bad").is_none(), "a bad spec must not run");
}

#[tokio::test]
async fn a_completed_run_resumes_without_a_model() {
    let env = env();
    write_workflow(&env, "guard", GUARD);
    let output = run(&env, &["workflow", "run", "guard"], "y\n").await;
    assert_eq!(output.status.code(), Some(0));
    // Nothing is left to execute, so no model is required.
    let output = run(&env, &["workflow", "run", "guard", "--resume"], "").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(!stderr.contains("no model is selected"), "{stderr}");
}

#[tokio::test]
async fn a_workflow_preloads_its_named_skills() {
    let (base, fake) = spawn_endpoint().await;
    let env = env();
    write_workflow(
        &env,
        "skilled",
        r#"
name = "skilled"
skills = ["test-repair"]
[[step]]
id = "work"
prompt = "Repair the test."
"#,
    );
    let output = run(
        &env,
        &[
            "--endpoint",
            &base,
            "--model",
            "test-model",
            "workflow",
            "run",
            "skilled",
        ],
        "",
    )
    .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    let requests = fake.requests.lock().unwrap();
    let prompt = requests[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(prompt.contains("[skill: test-repair]"), "{prompt}");
    assert!(prompt.contains("Repair the test."), "{prompt}");
}

#[tokio::test]
async fn no_session_writes_no_journal() {
    let env = env();
    write_workflow(&env, "guard", GUARD);
    let output = run(&env, &["--no-session", "workflow", "run", "guard"], "y\n").await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(0), "{stderr}");
    assert!(
        journal(&env, "guard").is_none(),
        "--no-session must not write the workflow journal"
    );
}

#[test]
fn workflow_list_names_the_bundled_set() {
    let env = env();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_tot"))
        .args(["workflow", "list"])
        .current_dir(&env.cwd)
        .env("TOT_CONFIG_DIR", &env.config_dir)
        .env("TOT_DATA_DIR", &env.data_dir)
        .env("TOT_SKIP_TOOL_PROBE", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(0), "{stdout}");
    for name in ["coding", "general-assistance", "security-red"] {
        assert!(stdout.contains(name), "{stdout}");
    }
}

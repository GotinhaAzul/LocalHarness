//! MCP client end to end: a real stdio server process (Python fixture), a
//! Streamable HTTP server (axum), profile selection, and the `lh mcp` CLI.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use localharness::config::{Config, McpServerConfig};
use localharness::mcp::{McpClient, McpError, McpManager};
use localharness::{Paths, ToolRegistry};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mcp_stdio_server.py")
}

fn python_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn stdio_config() -> McpServerConfig {
    McpServerConfig {
        command: "python3".into(),
        args: vec![fixture().display().to_string()],
        timeout_secs: 2,
        startup_timeout_secs: 10,
        ..Default::default()
    }
}

macro_rules! require_python {
    () => {
        if !python_available() {
            eprintln!("python3 is not installed; skipping the stdio MCP test");
            return;
        }
    };
}

fn text_of(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn stdio_server_handshake_listing_and_calls() {
    require_python!();
    let dir = tempfile::tempdir().unwrap();
    let mut config = stdio_config();
    config
        .env
        .insert("MCP_TEST_VALUE".into(), "from-config".into());
    let client = McpClient::new("fx", config, dir.path());

    let info = client.connect().await.expect("handshake");
    assert_eq!(info.name, "fixture");
    assert_eq!(info.protocol, "2025-06-18");

    // Two pages, joined; the non-JSON stdout line was skipped.
    let tools = client.list_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["echo", "fail", "slow", "crash", "env"]);
    assert!(tools[0].read_only);

    let cancel = CancellationToken::new();
    // The server sends a ping request before answering; the client answers it.
    let echo = client
        .call_tool("echo", json!({"text": "hi"}), &cancel)
        .await
        .unwrap();
    assert_eq!(text_of(&echo), "echo: hi");

    let env = client.call_tool("env", json!({}), &cancel).await.unwrap();
    assert_eq!(text_of(&env), "from-config");

    let fail = client.call_tool("fail", json!({}), &cancel).await.unwrap();
    assert_eq!(fail["isError"], json!(true));

    match client.call_tool("nope", json!({}), &cancel).await {
        Err(McpError::Rpc { code, message }) => {
            assert_eq!(code, -32602);
            assert!(message.contains("unknown tool"), "{message}");
        }
        other => panic!("expected an RPC error, got {other:?}"),
    }

    client.close().await;
}

#[tokio::test]
async fn stdio_timeout_and_cancel_return_promptly() {
    require_python!();
    let dir = tempfile::tempdir().unwrap();
    let client = McpClient::new("fx", stdio_config(), dir.path());

    let started = Instant::now();
    let timed_out = client
        .call_tool("slow", json!({}), &CancellationToken::new())
        .await;
    assert!(
        matches!(timed_out, Err(McpError::Timeout(_))),
        "{timed_out:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(4));

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        trigger.cancel();
    });
    let started = Instant::now();
    let cancelled = client.call_tool("slow", json!({}), &cancel).await;
    assert!(
        matches!(cancelled, Err(McpError::Cancelled)),
        "{cancelled:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    client.close().await;
}

#[tokio::test]
async fn stdio_crash_is_reported_once_then_reconnects() {
    require_python!();
    let dir = tempfile::tempdir().unwrap();
    let client = McpClient::new("fx", stdio_config(), dir.path());
    let cancel = CancellationToken::new();
    client.connect().await.unwrap();

    // The in-flight call fails with the stderr tail and is not retried.
    let error = client
        .call_tool("crash", json!({}), &cancel)
        .await
        .unwrap_err();
    let text = error.to_string();
    assert!(
        text.contains("exited before answering `tools/call`"),
        "{text}"
    );
    assert!(text.contains("crashing on purpose"), "{text}");
    assert!(
        !error.is_closed(),
        "a delivered request must not be retried"
    );

    // The next call transparently starts a new server process.
    let echo = client
        .call_tool("echo", json!({"text": "back"}), &cancel)
        .await
        .expect("reconnect after crash");
    assert_eq!(text_of(&echo), "echo: back");
    client.close().await;
}

#[tokio::test]
async fn missing_command_reports_a_prescriptive_error() {
    let dir = tempfile::tempdir().unwrap();
    let client = McpClient::new(
        "ghost",
        McpServerConfig {
            command: "/nonexistent/mcp-server".into(),
            ..Default::default()
        },
        dir.path(),
    );
    let error = client.connect().await.unwrap_err().to_string();
    assert!(error.contains("cannot start"), "{error}");
    assert!(error.contains("[mcp.ghost]"), "{error}");
}

// ---------------------------------------------------------------- HTTP

#[derive(Clone, Default)]
struct HttpFake {
    deletes: Arc<AtomicUsize>,
}

async fn http_mcp(headers: HeaderMap, body: String) -> Response {
    let message: Value = serde_json::from_str(&body).unwrap();
    let method = message["method"].as_str().unwrap_or("");
    let id = message.get("id").cloned();
    let session = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if method != "initialize" && session != "sess-1" {
        return (StatusCode::BAD_REQUEST, "missing session").into_response();
    }
    if method != "initialize"
        && headers
            .get("mcp-protocol-version")
            .and_then(|v| v.to_str().ok())
            != Some("2025-03-26")
    {
        return (StatusCode::BAD_REQUEST, "missing protocol version").into_response();
    }
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer s3cret") {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(id) = id else {
        return StatusCode::ACCEPTED.into_response();
    };
    match method {
        "initialize" => (
            [
                ("mcp-session-id", "sess-1"),
                (header::CONTENT_TYPE.as_str(), "application/json"),
            ],
            json!({"jsonrpc":"2.0","id":id,"result":{
                "protocolVersion":"2025-03-26",
                "capabilities":{"tools":{}},
                "serverInfo":{"name":"http-fixture","version":"2.0"}
            }})
            .to_string(),
        )
            .into_response(),
        "tools/list" => (
            [(header::CONTENT_TYPE.as_str(), "application/json")],
            json!({"jsonrpc":"2.0","id":id,"result":{"tools":[
                {"name":"search","description":"Search docs","inputSchema":{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}}
            ]}})
            .to_string(),
        )
            .into_response(),
        "tools/call" => {
            // Answer over SSE, preceded by an unrelated notification.
            let q = message["params"]["arguments"]["q"].as_str().unwrap_or("");
            let response = json!({"jsonrpc":"2.0","id":id,"result":{
                "content":[{"type":"text","text":format!("results for {q}")}],
                "structuredContent":{"hits":1}
            }});
            let body = format!(
                "event: message\ndata: {}\n\nevent: message\ndata: {}\n\n",
                json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1}}),
                response
            );
            ([(header::CONTENT_TYPE.as_str(), "text/event-stream")], body).into_response()
        }
        _ => (
            [(header::CONTENT_TYPE.as_str(), "application/json")],
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"no"}}).to_string(),
        )
            .into_response(),
    }
}

async fn http_server() -> (String, HttpFake) {
    let fake = HttpFake::default();
    let deletes = fake.deletes.clone();
    let app = Router::new().route(
        "/mcp",
        post(http_mcp).delete(move || {
            let deletes = deletes.clone();
            async move {
                deletes.fetch_add(1, Ordering::SeqCst);
                StatusCode::OK
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/mcp"), fake)
}

#[tokio::test]
async fn streamable_http_session_headers_json_and_sse() {
    let (url, fake) = http_server().await;
    let dir = tempfile::tempdir().unwrap();
    // SAFETY: tests in this binary do not read this variable concurrently.
    unsafe { std::env::set_var("LH_TEST_MCP_TOKEN", "s3cret") };
    let mut config = McpServerConfig {
        url,
        ..Default::default()
    };
    config
        .headers
        .insert("Authorization".into(), "Bearer ${LH_TEST_MCP_TOKEN}".into());
    let client = McpClient::new("docs", config, dir.path());

    let info = client.connect().await.expect("handshake");
    assert_eq!(info.name, "http-fixture");
    assert_eq!(info.protocol, "2025-03-26");
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);

    let result = client
        .call_tool("search", json!({"q": "mcp"}), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(text_of(&result), "results for mcp");
    assert_eq!(result["structuredContent"]["hits"], json!(1));

    client.close().await;
    assert_eq!(fake.deletes.load(Ordering::SeqCst), 1, "session is ended");
}

#[tokio::test]
async fn http_without_credentials_names_the_header_to_set() {
    let (url, _fake) = http_server().await;
    let dir = tempfile::tempdir().unwrap();
    let client = McpClient::new(
        "docs",
        McpServerConfig {
            url,
            ..Default::default()
        },
        dir.path(),
    );
    let error = client.connect().await.unwrap_err().to_string();
    assert!(error.contains("401"), "{error}");
    assert!(error.contains("[mcp.docs.headers]"), "{error}");
}

/// Answer the handshake, then stall every call and cancellation notification.
async fn stalling_http_server() -> String {
    async fn handler(body: String) -> Response {
        let message: Value = serde_json::from_str(&body).unwrap();
        let method = message["method"].as_str().unwrap_or("");
        let Some(id) = message.get("id").cloned() else {
            // `notifications/initialized` must complete; cancellation stalls.
            if method == "notifications/initialized" {
                return StatusCode::ACCEPTED.into_response();
            }
            return std::future::pending::<Response>().await;
        };
        match method {
            "initialize" => (
                [
                    ("mcp-session-id", "sess-stall"),
                    (header::CONTENT_TYPE.as_str(), "application/json"),
                ],
                json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":"2025-03-26",
                    "capabilities":{"tools":{}},
                    "serverInfo":{"name":"stall","version":"1"}
                }})
                .to_string(),
            )
                .into_response(),
            "tools/list" => (
                [(header::CONTENT_TYPE.as_str(), "application/json")],
                json!({"jsonrpc":"2.0","id":id,"result":{"tools":[
                    {"name":"slow","inputSchema":{"type":"object"}}
                ]}})
                .to_string(),
            )
                .into_response(),
            _ => std::future::pending::<Response>().await,
        }
    }
    let app = Router::new().route("/mcp", post(handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/mcp")
}

#[tokio::test]
async fn http_cancel_notification_cannot_hold_the_call_open() {
    let url = stalling_http_server().await;
    let dir = tempfile::tempdir().unwrap();
    let client = McpClient::new(
        "stall",
        McpServerConfig {
            url,
            timeout_secs: 1,
            startup_timeout_secs: 5,
            ..Default::default()
        },
        dir.path(),
    );
    let started = Instant::now();
    let outcome = tokio::time::timeout(
        Duration::from_secs(8),
        client.call_tool("slow", json!({}), &CancellationToken::new()),
    )
    .await;
    assert!(
        matches!(outcome, Ok(Err(McpError::Timeout(_)))),
        "{outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the cancelled call waited on the cancellation notification: {:?}",
        started.elapsed()
    );
    client.close().await;
}

// ------------------------------------------------------ profile selection

fn paths_in(dir: &std::path::Path) -> Paths {
    let mut paths = Paths::discover(dir).unwrap();
    paths.config_dir = dir.join("config");
    paths.data_dir = dir.join("data");
    paths
}

#[tokio::test]
async fn servers_attach_to_profiles_and_failures_are_warnings() {
    require_python!();
    let dir = tempfile::tempdir().unwrap();
    let paths = paths_in(dir.path());
    let registry = ToolRegistry::builtin();
    let manager = McpManager::new();

    let mut config = Config::builtin();
    config.profile_name = "minimal".into();
    config.mcp.insert(
        "fx".into(),
        McpServerConfig {
            exclude_tools: vec!["crash".into()],
            ..stdio_config()
        },
    );
    config.mcp.insert(
        "broken".into(),
        McpServerConfig {
            command: "/nonexistent/mcp-server".into(),
            ..Default::default()
        },
    );

    // Unset `profiles`: every profile gets the server's tools.
    let (tools, warnings) = localharness::app::select_tools(&registry, &config, &paths, &manager)
        .await
        .unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert!(
        names.contains(&"read") && names.contains(&"fx__echo"),
        "{names:?}"
    );
    assert!(
        !names.contains(&"fx__crash"),
        "excluded tool leaked: {names:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("`broken` is unavailable")),
        "{warnings:?}"
    );
    let status: Vec<String> = manager.status().iter().map(|s| s.summary()).collect();
    assert!(
        status
            .iter()
            .any(|s| s.starts_with("fx (stdio): connected to fixture")),
        "{status:?}"
    );
    assert!(
        status
            .iter()
            .any(|s| s.starts_with("broken (stdio): failed")),
        "{status:?}"
    );

    // `profiles = []`: only attached when the profile names it.
    config.mcp.get_mut("fx").unwrap().profiles = Some(vec![]);
    config.mcp.get_mut("broken").unwrap().enabled = false;
    let (tools, _) = localharness::app::select_tools(&registry, &config, &paths, &manager)
        .await
        .unwrap();
    assert!(!tools.iter().any(|t| t.name().starts_with("fx__")));

    config
        .profiles
        .get_mut("minimal")
        .unwrap()
        .tools
        .push("fx__echo".into());
    let (tools, _) = localharness::app::select_tools(&registry, &config, &paths, &manager)
        .await
        .unwrap();
    let mcp_tools: Vec<&str> = tools
        .iter()
        .map(|t| t.name())
        .filter(|n| n.starts_with("fx__"))
        .collect();
    assert_eq!(mcp_tools, ["fx__echo"]);

    config
        .profiles
        .get_mut("minimal")
        .unwrap()
        .tools
        .push("mcp:fx".into());
    let (tools, _) = localharness::app::select_tools(&registry, &config, &paths, &manager)
        .await
        .unwrap();
    assert_eq!(
        tools
            .iter()
            .filter(|t| t.name().starts_with("fx__"))
            .count(),
        4
    );

    config
        .profiles
        .get_mut("minimal")
        .unwrap()
        .tools
        .push("mcp:nosuch".into());
    let error = localharness::app::select_tools(&registry, &config, &paths, &manager)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("unknown MCP server `nosuch`"), "{error}");
    manager.shutdown().await;
}

#[test]
fn config_rejects_ambiguous_servers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(
        &path,
        "[mcp.both]\ncommand = \"x\"\nurl = \"http://127.0.0.1:1/mcp\"\n",
    )
    .unwrap();
    let error = format!("{:#}", Config::load(&path, None).unwrap_err());
    assert!(error.contains("not both"), "{error}");

    std::fs::write(
        &path,
        "[mcp.github]\ncommand = \"npx\"\nargs = [\"-y\", \"server\"]\nprofiles = [\"coding\"]\n[mcp.github.env]\nTOKEN = \"${GITHUB_TOKEN}\"\n",
    )
    .unwrap();
    let config = Config::load(&path, None).unwrap();
    let server = &config.mcp["github"];
    assert_eq!(server.args, ["-y", "server"]);
    assert!(server.attaches_to("coding") && !server.attaches_to("security"));
    assert_eq!(server.timeout_secs, 120);
}

#[test]
fn cli_refuses_a_disabled_server_before_connecting() {
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("config/localharness");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        "[mcp.off]\ncommand = \"/nonexistent/mcp-server\"\nenabled = false\n",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_lh"))
        .args(["mcp", "call", "off", "echo", "{}"])
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("HOME", home.path())
        .current_dir(home.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("disabled"), "{stderr}");
    assert!(
        !stderr.contains("cannot reach"),
        "the disabled server was contacted: {stderr}"
    );
}

#[test]
fn cli_lists_and_calls_tools() {
    if !python_available() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let config_dir = home.path().join("config/localharness");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[mcp.fx]\ncommand = \"python3\"\nargs = [{:?}]\n",
            fixture().display().to_string()
        ),
    )
    .unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_lh"))
            .args(args)
            .env("XDG_CONFIG_HOME", home.path().join("config"))
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("HOME", home.path())
            .current_dir(home.path())
            .output()
            .unwrap()
    };

    let list = run(&["mcp", "list", "--json"]);
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let value: Value = serde_json::from_slice(&list.stdout).unwrap();
    assert_eq!(value["servers"][0]["ok"], json!(true));
    assert_eq!(
        value["servers"][0]["tools"][0]["exposed_as"],
        json!("fx__echo")
    );

    let call = run(&["mcp", "call", "fx", "echo", r#"{"text":"cli"}"#]);
    assert!(
        call.status.success(),
        "{}",
        String::from_utf8_lossy(&call.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&call.stdout).trim(), "echo: cli");

    let fail = run(&["mcp", "call", "fx", "fail"]);
    assert_eq!(fail.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&fail.stdout).contains("[MCP_TOOL_ERROR] it broke"));
}

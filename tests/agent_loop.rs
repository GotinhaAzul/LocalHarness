//! Agent-loop integration tests using a scripted provider. No network, no real
//! model. Covers guards, budgets, compaction, and session consistency.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use localharness::agent::{AgentEvent, Core, TurnStatus, run_turn};
use localharness::config::{Config, ProfileConfig, VerifyConfig, VerifyStep};
use localharness::context::TokenBudget;
use localharness::paths::Paths;
use localharness::provider::{
    ChatRequest, ErrorClass, Provider, ProviderError, StreamEvent, StreamItem, Usage,
};
use localharness::security::SecurityState;
use localharness::session::{EntryKind, Session, SessionData};
use localharness::tools::{BgRegistry, ReadTracker, SessionHandle, ToolRegistry};
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

enum Script {
    Events(Vec<StreamEvent>),
    Error(ProviderError),
}

struct ScriptedProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ChatRequest>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat_stream(
        &self,
        req: ChatRequest,
        tx: UnboundedSender<StreamItem>,
        _cancel: CancellationToken,
    ) {
        self.requests.lock().unwrap().push(req);
        let script = self.scripts.lock().unwrap().pop_front();
        match script {
            Some(Script::Events(events)) => {
                for event in events {
                    let _ = tx.send(Ok(event));
                }
            }
            Some(Script::Error(err)) => {
                let _ = tx.send(Err(err));
            }
            None => {
                let _ = tx.send(Err(ProviderError::new(
                    ErrorClass::BadRequest,
                    "script exhausted",
                )));
            }
        }
    }

    fn endpoint(&self) -> &str {
        "http://127.0.0.1:0"
    }

    fn model(&self) -> &str {
        "test-model"
    }
}

fn text_script(text: &str) -> Script {
    Script::Events(vec![
        StreamEvent::Delta(text.into()),
        StreamEvent::Finish("stop".into()),
        StreamEvent::Usage(Usage::new(20, 4)),
    ])
}

fn tool_script(name: &str, args: serde_json::Value) -> Script {
    Script::Events(vec![
        StreamEvent::ToolCallDelta {
            index: 0,
            id: Some("call_1".into()),
            name: Some(name.into()),
            arguments: Some(args.to_string()),
        },
        StreamEvent::Finish("tool_calls".into()),
    ])
}

struct Harness {
    core: Arc<Core>,
    session: SessionHandle,
    provider: Arc<ScriptedProvider>,
}

fn base_config() -> Config {
    let mut config = Config::builtin();
    config.model.model = "test-model".into();
    config.model.endpoint = "http://127.0.0.1:0".into();
    config.model.context_window = Some(65_536);
    config.profiles.insert(
        "test".into(),
        ProfileConfig {
            packs: vec![],
            tools: vec![],
            model_profile: Some("default".into()),
            prompt: None,
            description: None,
            theme: None,
        },
    );
    config.profile_name = "test".into();
    config
}

fn harness(dir: &std::path::Path, scripts: Vec<Script>, tools: &[&str]) -> Harness {
    let mut config = base_config();
    config.profiles.get_mut("test").expect("test profile").tools =
        tools.iter().map(|s| s.to_string()).collect();
    harness_config(dir, scripts, config)
}

fn harness_config(dir: &std::path::Path, scripts: Vec<Script>, config: Config) -> Harness {
    let mut paths = Paths::discover(dir).unwrap();
    paths.data_dir = dir.join("data");
    paths.config_dir = dir.join("config");

    let session = Session::create(
        &paths,
        SessionData {
            cwd: dir.display().to_string(),
            endpoint: config.model.endpoint.clone(),
            model: "test-model".into(),
            profile: "test".into(),
            app: "test".into(),
        },
        true,
    )
    .unwrap();
    let session: SessionHandle = Arc::new(Mutex::new(session));
    let artifacts = session.lock().unwrap().artifacts.clone();
    let registry = ToolRegistry::builtin();
    let selected = registry.select(config.profile(), &[]).unwrap();
    let provider = Arc::new(ScriptedProvider {
        scripts: Mutex::new(scripts.into()),
        requests: Mutex::new(Vec::new()),
    });
    let core = Core {
        budget: TokenBudget::new(config.context_window(), config.model.max_output_tokens),
        limits: localharness::agent::RunLimits::from(&config.budgets),
        config,
        paths: paths.clone(),
        provider: provider.clone(),
        registry,
        tool_specs: ToolRegistry::specs(&selected),
        tools: selected,
        session: session.clone(),
        artifacts,
        reads: Arc::new(Mutex::new(ReadTracker::default())),
        bg: Arc::new(Mutex::new(BgRegistry::new())),
        security: Arc::new(Mutex::new(SecurityState::default())),
        system_prompt: "system".into(),
        model_profile: localharness::config::ModelProfile::default(),
        cancel: CancellationToken::new(),
        tool_timeout: std::time::Duration::from_secs(10),
        path_jail: false,
    };
    Harness {
        core: Arc::new(core),
        session,
        provider,
    }
}

async fn run(
    harness: &Harness,
    prompt: &str,
) -> (localharness::agent::TurnOutcome, Vec<AgentEvent>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = run_turn(
        harness.core.clone(),
        prompt.to_string(),
        tx,
        CancellationToken::new(),
    )
    .await;
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (outcome, events)
}

fn last_tool_result(session: &SessionHandle) -> String {
    let session = session.lock().unwrap();
    session
        .branch_entries()
        .into_iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn last_assistant(session: &SessionHandle) -> String {
    let session = session.lock().unwrap();
    session
        .branch_entries()
        .into_iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::Assistant(assistant) => Some(assistant.content.clone().unwrap_or_default()),
            _ => None,
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn plain_text_turn_is_appended() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path(), vec![text_script("Hello there")], &["read"]);
    let (outcome, events) = run(&harness, "hi").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert_eq!(last_assistant(&harness.session), "Hello there");
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TurnDone(_))));
    let requests = harness.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.iter().any(|tool| tool.name == "read"));
    assert!(!requests[0].tools.iter().any(|tool| tool.name == "bash"));
}

#[tokio::test]
async fn write_on_unread_file_is_blocked() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("write", json!({"path":"a.rs","content":"changed"})),
            text_script("done"),
        ],
        &["write"],
    );
    let (outcome, _) = run(&harness, "change it").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let result = last_tool_result(&harness.session);
    assert!(result.contains("READ_FIRST"), "{result}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn main() {}\n"
    );
}

#[tokio::test]
async fn scope_gate_blocks_out_of_scope_security_tools() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("bash", json!({"command":"nmap 192.0.2.10"})),
            text_script("blocked"),
        ],
        &["bash"],
    );
    let (_, _) = run(&harness, "scan").await;
    let result = last_tool_result(&harness.session);
    assert!(result.contains("SEC_SCOPE"), "{result}");
    assert!(result.contains("/sec scope add"), "{result}");
}

#[tokio::test]
async fn unknown_tool_gets_a_result_and_turn_continues() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("definitely_not_a_tool", json!({})),
            text_script("recovered"),
        ],
        &["read"],
    );
    let (outcome, _) = run(&harness, "try it").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let result = last_tool_result(&harness.session);
    assert!(result.contains("unknown_tool"), "{result}");
    assert_eq!(last_assistant(&harness.session), "recovered");
}

#[tokio::test]
async fn loop_guard_blocks_the_third_identical_call() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("read", json!({"path":"a.rs"})),
            tool_script("read", json!({"path":"a.rs"})),
            tool_script("read", json!({"path":"a.rs"})),
            text_script("should not run"),
        ],
        &["read"],
    );
    let (outcome, events) = run(&harness, "repeat").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Guard { code, .. } if code == "loop_guard"
    )));
    let session = harness.session.lock().unwrap();
    let results: Vec<_> = session
        .branch_entries()
        .into_iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 3);
    assert!(results[2].contains("loop_guard"), "{results:?}");
    assert_eq!(harness.provider.requests.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn model_budget_stops_before_another_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = harness(
        dir.path(),
        vec![
            tool_script("read", json!({"path":"a.rs"})),
            text_script("nope"),
        ],
        &["read"],
    );
    let mut config = harness.core.config.clone();
    config.budgets.max_model_calls = 1;
    let mut core = Arc::try_unwrap(harness.core).unwrap_or_else(|_| panic!("sole owner"));
    core.config = config;
    core.limits.max_model_calls = 1;
    harness.core = Arc::new(core);
    let (outcome, _) = run(&harness, "one call").await;
    assert_eq!(outcome.status, TurnStatus::Budget);
    assert_eq!(harness.provider.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn oversized_context_compacts_before_the_model_call() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = harness(dir.path(), vec![text_script("after compaction")], &["read"]);
    // Shrink the budget so the large user prompt crosses 80%.
    let mut core = Arc::try_unwrap(harness.core).unwrap_or_else(|_| panic!("sole owner"));
    core.budget = TokenBudget::new(1000, 100);
    harness.core = Arc::new(core);
    let (outcome, _) = run(&harness, &"x".repeat(8000)).await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let session = harness.session.lock().unwrap();
    assert!(
        session
            .branch_entries()
            .iter()
            .any(|entry| entry.is_compaction()),
        "expected a compaction entry"
    );
}

#[tokio::test]
async fn context_error_triggers_compaction_and_one_retry() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            Script::Error(ProviderError::new(
                ErrorClass::ContextTooSmall,
                "the prompt is too long for context",
            )),
            text_script("recovered"),
        ],
        &["read"],
    );
    let (outcome, _) = run(&harness, "do it").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert_eq!(harness.provider.requests.lock().unwrap().len(), 2);
    {
        let session = harness.session.lock().unwrap();
        assert!(
            session
                .branch_entries()
                .iter()
                .any(|entry| matches!(&entry.kind, EntryKind::Compaction(data) if data.reason == "context_error"))
        );
    }
    assert_eq!(last_assistant(&harness.session), "recovered");
}

#[tokio::test]
async fn cancellation_appends_results_for_unanswered_calls() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            Script::Events(vec![StreamEvent::Delta("working".into())]),
            // The turn is cancelled while streaming.
        ],
        &["read"],
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = run_turn(harness.core.clone(), "cancel me".into(), tx, cancel).await;
    assert_eq!(outcome.status, TurnStatus::Cancelled);
    while rx.try_recv().is_ok() {}
    let session = harness.session.lock().unwrap();
    assert!(
        session
            .branch_entries()
            .iter()
            .any(|entry| matches!(&entry.kind, EntryKind::Error(data) if data.code == "cancelled"))
    );
}

#[tokio::test]
async fn read_then_edit_records_fs_change() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("read", json!({"path":"a.rs"})),
            tool_script("edit", json!({"path":"a.rs","old":"main","new":"run"})),
            text_script("done"),
        ],
        &["read", "edit"],
    );
    let (outcome, _) = run(&harness, "rename main").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn run() {}\n"
    );
    let session = harness.session.lock().unwrap();
    assert!(session.branch_entries().iter().any(
        |entry| matches!(&entry.kind, EntryKind::Custom(custom) if custom.kind == "fs-change")
    ));
}

#[tokio::test]
async fn todo_gate_blocks_the_third_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("write", json!({"path":"a.txt","content":"a"})),
            tool_script("write", json!({"path":"b.txt","content":"b"})),
            tool_script("write", json!({"path":"c.txt","content":"c"})),
            text_script("one"),
            text_script("two"),
            text_script("three"),
        ],
        &["write", "todo"],
    );
    let (outcome, _) = run(&harness, "make three files").await;
    assert_eq!(outcome.status, TurnStatus::Blocked);
    assert!(dir.path().join("a.txt").exists());
    assert!(dir.path().join("b.txt").exists());
    assert!(
        !dir.path().join("c.txt").exists(),
        "third write must be gated"
    );
    let result = last_tool_result(&harness.session);
    assert!(result.contains("TODO_PLAN"), "{result}");
    assert!(result.contains("todo"), "{result}");
}

#[tokio::test]
async fn verify_gate_runs_configured_steps_and_review_settles() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = base_config();
    config.profiles.get_mut("test").expect("test profile").tools = ["write", "verify", "review"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    config.verify = VerifyConfig {
        steps: vec![VerifyStep {
            name: "noop".into(),
            command: vec!["true".into()],
            timeout_secs: 5,
        }],
    };
    let harness = harness_config(
        dir.path(),
        vec![
            tool_script("write", json!({"path":"a.txt","content":"a"})),
            text_script("changed"),
            tool_script("verify", json!({})),
            text_script("verified"),
            tool_script("review", json!({})),
            text_script("done"),
        ],
        config,
    );
    let (outcome, _) = run(&harness, "change and verify").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert_eq!(harness.provider.requests.lock().unwrap().len(), 6);
    let session = harness.session.lock().unwrap();
    let verify = session
        .branch_entries()
        .iter()
        .find_map(|entry| match &entry.kind {
            EntryKind::Custom(custom) if custom.kind == "verify-result" => Some(custom),
            _ => None,
        })
        .expect("verify result recorded");
    assert_eq!(
        verify.payload.get("passed").and_then(|v| v.as_bool()),
        Some(true)
    );
}

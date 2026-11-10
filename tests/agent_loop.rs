//! Agent-loop integration tests using a scripted provider. No network, no real
//! model. Covers guards, budgets, compaction, and session consistency.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use localharness::agent::{AgentEvent, Core, TurnStatus, run_turn};
use localharness::config::{Config, ProfileConfig, VerifyConfig, VerifyStep};
use localharness::context::TokenBudget;
use localharness::paths::Paths;
use localharness::provider::{
    ChatMessage, ChatRequest, ErrorClass, Provider, ProviderError, StreamEvent, StreamItem, Usage,
};
use localharness::security::SecurityState;
use localharness::session::{CompactionData, EntryKind, Session, SessionData, ToolResultData};
use localharness::tools::{BgRegistry, ReadTracker, SessionHandle, ToolRegistry};
use serde_json::json;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

enum Script {
    Events(Vec<StreamEvent>),
    EventsThenWait(Vec<StreamEvent>),
    Error(ProviderError),
}

struct ScriptedProvider {
    scripts: Mutex<VecDeque<Script>>,
    requests: Mutex<Vec<ChatRequest>>,
    /// Reject any system message after a non-system one, like a strict
    /// llama.cpp `--jinja` template.
    strict_system: AtomicBool,
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat_stream(
        &self,
        req: ChatRequest,
        tx: UnboundedSender<StreamItem>,
        _cancel: CancellationToken,
    ) {
        if self.strict_system.load(Ordering::Relaxed) {
            let mut seen_non_system = false;
            for message in &req.messages {
                match message {
                    ChatMessage::System { .. } if seen_non_system => {
                        let _ = tx.send(Err(ProviderError::new(
                            ErrorClass::BadRequest,
                            "strict template: system message after a non-system message",
                        )));
                        return;
                    }
                    ChatMessage::System { .. } => {}
                    _ => seen_non_system = true,
                }
            }
        }
        self.requests.lock().unwrap().push(req);
        let script = self.scripts.lock().unwrap().pop_front();
        match script {
            Some(Script::Events(events)) => {
                for event in events {
                    let _ = tx.send(Ok(event));
                }
            }
            Some(Script::EventsThenWait(events)) => {
                for event in events {
                    let _ = tx.send(Ok(event));
                }
                _cancel.cancelled().await;
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

/// Text without a usage report, so a long synthetic transcript cannot trip the
/// prompt-token shortfall heuristic.
fn bare_text_script(text: &str) -> Script {
    Script::Events(vec![
        StreamEvent::Delta(text.into()),
        StreamEvent::Finish("stop".into()),
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

fn parallel_tool_script(calls: &[(&str, serde_json::Value)]) -> Script {
    let mut events = Vec::new();
    for (index, (name, args)) in calls.iter().enumerate() {
        events.push(StreamEvent::ToolCallDelta {
            index,
            id: Some(format!("call_{}", index + 1)),
            name: Some((*name).into()),
            arguments: Some(args.to_string()),
        });
    }
    events.push(StreamEvent::Finish("tool_calls".into()));
    Script::Events(events)
}

fn harness_with_profile(dir: &std::path::Path, scripts: Vec<Script>, profile: &str) -> Harness {
    let mut config = base_config();
    config.profile_name = profile.into();
    harness_config(dir, scripts, config)
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
            context: None,
        },
        true,
    )
    .unwrap();
    let session: SessionHandle = Arc::new(Mutex::new(session));
    let artifacts = session.lock().unwrap().artifacts.clone();
    let registry = ToolRegistry::builtin();
    let selected = registry.select(config.profile(), &[]).unwrap();
    let (_, model_profile) = config.model_profile();
    let tool_specs = ToolRegistry::specs_with(&selected, model_profile.short_tool_schemas());
    let provider = Arc::new(ScriptedProvider {
        scripts: Mutex::new(scripts.into()),
        requests: Mutex::new(Vec::new()),
        strict_system: AtomicBool::new(false),
    });
    let budget = TokenBudget::new(config.context_window(), config.model.max_output_tokens);
    let tool_specs_json = localharness::provider::tools_json(&tool_specs);
    let tool_specs_cost = localharness::context::ToolSpecCost::from_json(&tool_specs_json, &budget);
    let core = Core {
        budget,
        static_prompt: localharness::context::StaticPrompt::default(),
        tool_specs_cost,
        tokenize: false,
        limits: localharness::agent::RunLimits::from(&config.budgets),
        config,
        paths: paths.clone(),
        provider: provider.clone(),
        registry,
        tool_specs,
        tool_specs_json,
        tools: selected,
        session: session.clone(),
        artifacts,
        reads: Arc::new(Mutex::new(ReadTracker::default())),
        bg: Arc::new(Mutex::new(BgRegistry::new())),
        security: Arc::new(Mutex::new(SecurityState::default())),
        sandbox: None,
        system_prompt: "system".into(),
        repo_brief: String::new(),
        model_profile,
        tool_probe: None,
        cancel: CancellationToken::new(),
        tool_timeout: std::time::Duration::from_secs(10),
        path_jail: false,
        server_window: Default::default(),
        window_source: localharness::agent::WindowSource::Default,
        window_truth: None,
        window_notice: None,
        window_notice_sent: std::sync::Once::new(),
        detected_window: Default::default(),
        window_cache: Default::default(),
        startup: None,
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

#[tokio::test]
async fn bash_records_evidence_and_findings_link_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("bash", json!({"command": "echo hello"})),
            tool_script(
                "finding",
                json!({
                    "action": "add",
                    "title": "Evidence link",
                    "severity": "low",
                    "target": "example.com"
                }),
            ),
            text_script("done"),
        ],
        &["bash", "finding"],
    );
    {
        let mut state = harness.core.security.lock().unwrap();
        state
            .ensure_engagement(&harness.core.paths)
            .expect("engagement");
    }
    let (outcome, _) = run(&harness, "run a command").await;
    assert_eq!(outcome.status, TurnStatus::Done);

    let state = harness.core.security.lock().unwrap();
    let engagement = state.engagement.as_ref().expect("engagement");
    assert_eq!(
        engagement.evidence.list().len(),
        1,
        "one bash evidence entry"
    );
    let evidence = &engagement.evidence.list()[0];
    assert_eq!(evidence.tool, "bash");
    assert_eq!(evidence.exit_code, Some(0));
    let (_, output) = engagement.evidence.read(&evidence.id).unwrap();
    assert!(output.contains("hello"), "{output}");
    assert!(engagement.evidence.verify(&evidence.id).unwrap().1);

    assert_eq!(engagement.findings.len(), 1);
    let finding = &engagement.findings[0];
    assert_eq!(
        finding
            .evidence_ref
            .as_ref()
            .and_then(|reference| reference.evidence.as_deref()),
        Some(evidence.id.as_str())
    );
    assert_eq!(
        finding
            .evidence_ref
            .as_ref()
            .and_then(|reference| reference.artifact.as_deref()),
        Some(evidence.artifact.as_str())
    );
}

#[tokio::test]
async fn bash_output_updates_the_surface_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script(
                "bash",
                json!({"command": "printf 'Nmap scan report for example.com\\n443/tcp open https nginx\\n'"}),
            ),
            text_script("done"),
        ],
        &["bash"],
    );
    {
        let mut state = harness.core.security.lock().unwrap();
        state
            .add_scope(&harness.core.paths, "example.com")
            .expect("scope");
    }
    let (outcome, _) = run(&harness, "scan").await;
    assert_eq!(outcome.status, TurnStatus::Done);

    let state = harness.core.security.lock().unwrap();
    let engagement = state.engagement.as_ref().expect("engagement");
    let asset = engagement
        .surface
        .get("example.com")
        .expect("surface asset");
    assert!(asset.in_scope);
    assert!(asset.ports.contains_key("443/tcp"));
    assert_eq!(asset.ports["443/tcp"].service, "https");
    assert_eq!(asset.ports["443/tcp"].product, "nginx");
    assert_eq!(engagement.evidence.list().len(), 1);
}

#[tokio::test]
async fn security_flow_links_surface_detection_and_graph() {
    let dir = tempfile::tempdir().unwrap();
    let rules = dir.path().join("rules");
    std::fs::create_dir_all(&rules).unwrap();
    std::fs::write(
        rules.join("evil.yml"),
        "title: Evil marker\nid: evil-marker\nlevel: high\ndetection:\n  selection:\n    message|contains: evil\n  condition: selection\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("sample.log"), "ok\nevil happened\n").unwrap();

    let harness = harness(
        dir.path(),
        vec![
            tool_script(
                "bash",
                json!({"command": "printf 'Nmap scan report for example.com\\n443/tcp open https nginx\\n'"}),
            ),
            tool_script(
                "detect",
                json!({"action": "run", "rules": "rules", "target": "sample.log"}),
            ),
            tool_script("attack_path", json!({"action": "summary"})),
            text_script("done"),
        ],
        &["bash", "detect", "attack_path"],
    );
    {
        let mut state = harness.core.security.lock().unwrap();
        state
            .add_scope(&harness.core.paths, "example.com")
            .expect("scope");
    }
    let (outcome, _) = run(&harness, "security flow").await;
    assert_eq!(outcome.status, TurnStatus::Done);

    let state = harness.core.security.lock().unwrap();
    let engagement = state.engagement.as_ref().expect("engagement");
    assert_eq!(
        engagement.evidence.list().len(),
        2,
        "bash + detect evidence"
    );
    let asset = engagement
        .surface
        .get("example.com")
        .expect("surface asset");
    assert!(asset.ports.contains_key("443/tcp"));
    assert_eq!(engagement.findings.len(), 1);
    let finding = &engagement.findings[0];
    assert_eq!(finding.title, "Detection: Evil marker");
    assert_eq!(
        finding
            .evidence_ref
            .as_ref()
            .and_then(|reference| reference.evidence.as_deref()),
        Some("e2")
    );
    drop(state);
    let graph = last_tool_result(&harness.session);
    assert!(graph.contains("asset:example.com"), "{graph}");
    assert!(graph.contains("service:example.com:443/tcp"), "{graph}");
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

fn tool_results(session: &SessionHandle) -> Vec<String> {
    let session = session.lock().unwrap();
    session
        .branch_entries()
        .into_iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .collect()
}

fn executed_glob_results(session: &SessionHandle) -> usize {
    let session = session.lock().unwrap();
    session
        .branch_entries()
        .iter()
        .filter(|entry| {
            matches!(&entry.kind, EntryKind::ToolResult(result)
                if !result.is_error && result.content.starts_with("[tool] glob"))
        })
        .count()
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

fn assistants(session: &SessionHandle) -> Vec<localharness::session::AssistantData> {
    let session = session.lock().unwrap();
    session
        .branch_entries()
        .into_iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Assistant(assistant) => Some(assistant.clone()),
            _ => None,
        })
        .collect()
}

fn user_messages(session: &SessionHandle) -> Vec<String> {
    let session = session.lock().unwrap();
    session
        .branch_entries()
        .into_iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::User(user) => Some(user.content.clone()),
            _ => None,
        })
        .collect()
}

fn finish_script(deltas: &[&str], finish: &str) -> Script {
    let mut events: Vec<StreamEvent> = deltas
        .iter()
        .map(|d| StreamEvent::Delta(d.to_string()))
        .collect();
    events.push(StreamEvent::Finish(finish.into()));
    Script::Events(events)
}

#[tokio::test]
async fn duplicate_engine_ids_get_distinct_internal_ids_and_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.rs"), "b\n").unwrap();
    let harness = harness(
        dir.path(),
        vec![
            Script::Events(vec![
                StreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".into()),
                    name: Some("read".into()),
                    arguments: Some("{\"path\":\"a.rs\"}".into()),
                },
                StreamEvent::ToolCallDelta {
                    index: 1,
                    id: Some("call_1".into()),
                    name: Some("read".into()),
                    arguments: Some("{\"path\":\"b.rs\"}".into()),
                },
                StreamEvent::Finish("tool_calls".into()),
            ]),
            text_script("done"),
        ],
        &["read"],
    );
    let (outcome, _) = run(&harness, "read both").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let call_ids: Vec<String> = assistants(&harness.session)
        .into_iter()
        .flat_map(|assistant| assistant.tool_calls.unwrap_or_default())
        .map(|call| call.id)
        .collect();
    assert_eq!(call_ids, ["call_0001", "call_0002"]);
    assert_eq!(tool_results(&harness.session).len(), 2);
}

#[tokio::test]
async fn internal_call_ids_increase_across_turns() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "a\n").unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("read", json!({"path":"a.rs"})),
            text_script("one"),
            tool_script("read", json!({"path":"a.rs"})),
            text_script("two"),
        ],
        &["read"],
    );
    run(&harness, "first").await;
    run(&harness, "second").await;
    let call_ids: Vec<String> = assistants(&harness.session)
        .into_iter()
        .flat_map(|assistant| assistant.tool_calls.unwrap_or_default())
        .map(|call| call.id)
        .collect();
    assert_eq!(call_ids, ["call_0001", "call_0002"]);
}

#[tokio::test]
async fn a_missing_tool_result_fails_before_the_request() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path(), vec![text_script("unused")], &["read"]);
    harness
        .session
        .lock()
        .unwrap()
        .append(EntryKind::Assistant(localharness::session::AssistantData {
            tool_calls: Some(vec![localharness::session::ToolCall {
                id: "call_0001".into(),
                name: "read".into(),
                arguments: json!({"path":"a.rs"}),
            }]),
            ..Default::default()
        }))
        .unwrap();
    let (outcome, events) = run(&harness, "continue").await;
    assert_eq!(outcome.status, TurnStatus::Error);
    let (code, message) = outcome.error.expect("typed pair error");
    assert_eq!(code, "tool_result_missing");
    assert!(message.contains("call_0001"), "{message}");
    assert_eq!(
        harness.provider.requests.lock().unwrap().len(),
        0,
        "the request must not be sent with an unanswered call"
    );
    assert!(events.iter().any(
        |event| matches!(event, AgentEvent::Error { code, .. } if code == "tool_result_missing")
    ));
}

#[tokio::test]
async fn tool_calls_written_as_text_are_executed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    let harness = harness(
        dir.path(),
        vec![
            finish_script(
                &[
                    "<think>I should look at the file.</think>\n\nReading it.\n<tool_",
                    "call>\n{\"name\": \"read\", \"arguments\": {\"path\": \"a.rs\"}}\n</tool_call>",
                ],
                "stop",
            ),
            text_script("It has a main function."),
        ],
        &["read"],
    );
    let (outcome, events) = run(&harness, "what is in a.rs").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("fn main() {}"), "{}", results[0]);
    let first = &assistants(&harness.session)[0];
    assert_eq!(first.content.as_deref(), Some("Reading it."));
    assert_eq!(
        first.reasoning.as_deref(),
        Some("I should look at the file.")
    );
    assert_eq!(first.tool_calls.as_ref().unwrap()[0].name, "read");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("--jinja")))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ReasoningDelta(_)))
    );
}

#[tokio::test]
async fn native_tool_mode_leaves_text_alone() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = harness(
        dir.path(),
        vec![text_script(
            "<tool_call>{\"name\":\"read\",\"arguments\":{\"path\":\"a\"}}</tool_call>",
        )],
        &["read"],
    );
    Arc::get_mut(&mut harness.core)
        .unwrap()
        .model_profile
        .tool_mode = localharness::config::ToolMode::Native;
    let (outcome, _) = run(&harness, "go").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert!(tool_results(&harness.session).is_empty());
}

#[tokio::test]
async fn repetition_loops_are_cut_and_nudged() {
    let dir = tempfile::tempdir().unwrap();
    let unit = "I will check the config file now.\n";
    let looping: Vec<String> = std::iter::repeat_n(unit.to_string(), 200).collect();
    let refs: Vec<&str> = looping.iter().map(String::as_str).collect();
    let harness = harness(
        dir.path(),
        vec![finish_script(&refs, "length"), text_script("Done.")],
        &["read"],
    );
    let (outcome, events) = run(&harness, "go").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let first = &assistants(&harness.session)[0];
    let content = first.content.as_deref().unwrap();
    assert!(
        content.len() < 1200,
        "loop was cut early: {} bytes",
        content.len()
    );
    assert_eq!(first.stop_reason.as_deref(), Some("repetition"));
    assert!(
        user_messages(&harness.session)
            .iter()
            .any(|m| m.contains("repetition guard"))
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("looping")))
    );
    assert_eq!(last_assistant(&harness.session), "Done.");
}

#[tokio::test]
async fn endless_repetition_stops_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let looping: Vec<String> =
        std::iter::repeat_n("again and again and again\n".to_string(), 100).collect();
    let refs: Vec<&str> = looping.iter().map(String::as_str).collect();
    let scripts = (0..5).map(|_| finish_script(&refs, "stop")).collect();
    let harness = harness(dir.path(), scripts, &["read"]);
    let (outcome, _) = run(&harness, "go").await;
    assert_eq!(outcome.status, TurnStatus::Stalled);
    assert_eq!(
        harness.provider.requests.lock().unwrap().len(),
        4,
        "3 nudges, then stop"
    );
}

#[tokio::test]
async fn output_limit_cut_tool_call_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![Script::Events(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".into()),
                name: Some("write".into()),
                arguments: Some("{\"path\":\"big.rs\",\"content\":\"fn a() {}\\nfn b".into()),
            },
            StreamEvent::Finish("length".into()),
        ])],
        &["write"],
    );
    let (outcome, events) = run(&harness, "write big.rs").await;
    assert_eq!(outcome.status, TurnStatus::Error);
    let (code, message) = outcome.error.expect("typed truncation error");
    assert_eq!(code, "tool_call_truncated");
    assert!(message.contains("incomplete"), "{message}");
    assert!(!dir.path().join("big.rs").exists());
    assert!(tool_results(&harness.session).is_empty());
    assert_eq!(
        harness.provider.requests.lock().unwrap().len(),
        1,
        "a truncated call must not be retried or dispatched"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error { code, .. } if code == "tool_call_truncated"))
    );
}

#[tokio::test]
async fn cut_off_prose_and_empty_replies_continue() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            finish_script(&["The answer has three parts. First"], "length"),
            finish_script(&[], "stop"),
            text_script(", second, and third."),
        ],
        &["read"],
    );
    let (outcome, _) = run(&harness, "explain").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let nudges = user_messages(&harness.session);
    assert!(
        nudges
            .iter()
            .any(|m| m.contains("Continue exactly where it stopped")),
        "{nudges:?}"
    );
    assert!(
        nudges.iter().any(|m| m.contains("empty response")),
        "{nudges:?}"
    );
    assert_eq!(last_assistant(&harness.session), ", second, and third.");
}

#[tokio::test]
async fn a_turn_after_compaction_keeps_system_messages_leading() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(dir.path(), vec![text_script("done")], &["read"]);
    harness
        .provider
        .strict_system
        .store(true, Ordering::Relaxed);
    harness
        .session
        .lock()
        .unwrap()
        .append(EntryKind::Compaction(CompactionData {
            summary: "the compacted summary".into(),
            tokens_before: 1000,
            reason: "test".into(),
            thread_id: None,
        }))
        .unwrap();

    let (outcome, _) = run(&harness, "continue").await;
    assert_eq!(outcome.status, TurnStatus::Done, "{:?}", outcome.error);
    let requests = harness.provider.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let messages = &requests[0].messages;
    assert!(
        matches!(&messages[0], ChatMessage::System { content }
            if content.contains("the compacted summary")
                && content.contains("[localharness] state")),
        "leading system: {:?}",
        messages[0]
    );
    assert!(
        messages
            .iter()
            .skip(1)
            .all(|message| !matches!(message, ChatMessage::System { .. })),
        "no system message may follow a non-system one"
    );
}

#[tokio::test]
async fn silent_server_truncation_ends_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            Script::Events(vec![
                StreamEvent::Delta("I have no tools.".into()),
                StreamEvent::Finish("stop".into()),
                // ~20k chars of prompt reported as 1000 tokens.
                StreamEvent::Usage(Usage::new(1000, 4)),
            ]),
            text_script("answer"),
        ],
        &["read"],
    );
    let prompt = "Explain this log.\n".repeat(1200);
    let (outcome, events) = run(&harness, &prompt).await;
    assert_eq!(outcome.status, TurnStatus::ContextTruncated);
    // No retry on the corrupted branch.
    assert_eq!(harness.provider.requests.lock().unwrap().len(), 1);
    assert!(
        assistants(&harness.session)
            .iter()
            .all(|a| a.content.as_deref() != Some("I have no tools.")
                && a.content.as_deref() != Some("answer")),
        "a truncated turn must not be replayed as normal history"
    );
    assert!(
        harness
            .core
            .server_window
            .load(std::sync::atomic::Ordering::Relaxed)
            > 0,
        "the real window is remembered for later turns"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::Guard { code, message }
            if code == "context_truncated" && message.contains("OLLAMA_CONTEXT_LENGTH")
    )));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Notice(n) if n.contains("/compact")))
    );
}

#[tokio::test]
async fn cache_and_timing_reports_are_stored_with_the_reply() {
    let dir = tempfile::tempdir().unwrap();
    let timings = Usage {
        cached_tokens: Some(18),
        prompt_ms: Some(40),
        tokens_per_sec: Some(30.0),
        ..Usage::default()
    };
    let harness = harness(
        dir.path(),
        vec![Script::Events(vec![
            StreamEvent::Delta("hello".into()),
            StreamEvent::Finish("stop".into()),
            StreamEvent::Usage(timings),
            StreamEvent::Usage(Usage::new(20, 4)),
        ])],
        &["read"],
    );
    let (_, events) = run(&harness, "hi").await;
    let usage = assistants(&harness.session)[0].usage.clone().unwrap();
    assert_eq!((usage.prompt_tokens, usage.completion_tokens), (20, 4));
    assert_eq!(usage.cache_percent(), Some(90));
    assert_eq!(usage.tokens_per_sec, Some(30.0));
    assert!(
        usage.first_token_ms.is_some(),
        "harness measures first token"
    );
    let last = events
        .iter()
        .rev()
        .find_map(|event| match event {
            AgentEvent::Usage(usage) => Some(usage.clone()),
            _ => None,
        })
        .unwrap();
    assert!(
        last.perf_line().contains("cache 90%"),
        "{}",
        last.perf_line()
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, AgentEvent::Usage(_)))
            .count(),
        1,
        "usage must be reported once, not per chunk and once at the end"
    );
}

#[tokio::test]
async fn undo_reverts_harness_edits_through_the_loop() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "let x = 1;\n").unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("read", json!({"path":"a.rs"})),
            tool_script("edit", json!({"path":"a.rs","old":"x = 1","new":"x = 2"})),
            tool_script("write", json!({"path":"b.rs","content":"new file\n"})),
            tool_script("undo", json!({"path":"./a.rs"})),
            tool_script("undo", json!({"path":"b.rs"})),
            tool_script("undo", json!({"path":"a.rs"})),
            text_script("reverted"),
        ],
        &["read", "edit", "write", "undo"],
    );
    let (outcome, _) = run(&harness, "try and revert").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "let x = 1;\n"
    );
    assert!(
        !dir.path().join("b.rs").exists(),
        "undo deletes a file write created"
    );
    let results = tool_results(&harness.session);
    assert!(results[3].contains("restored a.rs"), "{}", results[3]);
    assert!(results[4].contains("deleted b.rs"), "{}", results[4]);
    assert!(results[5].contains("NOTHING_TO_UNDO"), "{}", results[5]);
    // Undo is a recorded mutation, so verify and review gates see it.
    let session = harness.session.lock().unwrap();
    let undo_changes = session
        .branch_entries()
        .iter()
        .filter(|entry| {
            matches!(&entry.kind, EntryKind::Custom(custom)
            if custom.kind == "fs-change" && custom.payload.get("undo") == Some(&json!(true)))
        })
        .count();
    assert_eq!(undo_changes, 2);
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
async fn foreign_tool_and_argument_names_are_translated() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("read_file", json!({"file_path":"a.rs"})),
            tool_script(
                "functions.str_replace",
                json!({"file_path":"a.rs","old_string":"x = 1","new_string":"x = 2"}),
            ),
            // A second edit of the same file needs no re-read: the harness
            // wrote it and the model saw the diff.
            tool_script(
                "edit",
                json!({"path":"a.rs","old":"let x","new":"let y","bogus":true}),
            ),
            text_script("done"),
        ],
        &["read", "edit"],
    );
    let (outcome, _) = run(&harness, "bump it").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn main() {\n    let y = 2;\n}\n"
    );
    let results = tool_results(&harness.session);
    assert!(results[0].starts_with("[tool] read"), "{}", results[0]);
    assert!(results[0].contains("ran read"), "{}", results[0]);
    assert!(results[1].contains("ran edit"), "{}", results[1]);
    assert!(
        results[2].contains("ignored unknown argument(s) bogus"),
        "{}",
        results[2]
    );
}

#[tokio::test]
async fn routed_tool_names_still_pass_the_scope_gate() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("shell", json!({"cmd":"nmap 192.0.2.10"})),
            text_script("blocked"),
        ],
        &["bash"],
    );
    let (_, _) = run(&harness, "scan").await;
    let result = last_tool_result(&harness.session);
    assert!(result.contains("SEC_SCOPE"), "{result}");
}

#[tokio::test]
async fn unknown_tool_suggests_the_nearest_name() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("gerp", json!({"pattern":"x"})),
            text_script("ok"),
        ],
        &["read", "grep"],
    );
    let (_, _) = run(&harness, "search").await;
    let result = last_tool_result(&harness.session);
    assert!(result.contains("did you mean grep?"), "{result}");
}

#[tokio::test]
async fn loop_guard_blocks_the_fourth_identical_call() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("read", json!({"path":"a.rs"})),
            tool_script("read", json!({"path":"a.rs"})),
            tool_script("read", json!({"path":"a.rs"})),
            tool_script("read", json!({"path":"a.rs"})),
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
    assert_eq!(results.len(), 4);
    assert!(results[3].contains("loop_guard"), "{results:?}");
    assert_eq!(harness.provider.requests.lock().unwrap().len(), 4);
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
    // Shrink the budget so the large user prompt crosses 80% but the
    // window-derived summary still fits after one compaction.
    let mut core = Arc::try_unwrap(harness.core).unwrap_or_else(|_| panic!("sole owner"));
    core.budget = TokenBudget::new(4096, 1024);
    harness.core = Arc::new(core);
    let (outcome, _) = run(&harness, &"x".repeat(8000)).await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let session = harness.session.lock().unwrap();
    let compactions = session
        .branch_entries()
        .iter()
        .filter(|entry| entry.is_compaction())
        .count();
    assert_eq!(compactions, 1, "expected exactly one compaction entry");
}

#[tokio::test]
async fn repeated_over_threshold_ends_with_budget_context_after_one_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut harness = harness(
        dir.path(),
        vec![text_script("should not be called")],
        &["read"],
    );
    let mut core = Arc::try_unwrap(harness.core).unwrap_or_else(|_| panic!("sole owner"));
    core.budget = TokenBudget::new(1000, 100);
    // The fixed system prompt alone stays above the threshold, so one
    // compaction cannot converge.
    core.system_prompt = "s".repeat(6000);
    core.window_source = localharness::agent::WindowSource::Configured;
    harness.core = Arc::new(core);

    let (outcome, events) = run(&harness, "go").await;
    assert_eq!(outcome.status, TurnStatus::Budget);
    let (code, message) = outcome.error.expect("diagnostic");
    assert_eq!(code, "budget_context");
    assert!(message.contains("static overhead"), "{message}");
    assert!(message.contains("system prompt"), "{message}");
    assert!(
        events.iter().any(|event| matches!(
            event,
            AgentEvent::Error { code, .. } if code == "budget_context"
        )),
        "missing budget_context event"
    );
    let session = harness.session.lock().unwrap();
    let compactions = session
        .branch_entries()
        .iter()
        .filter(|entry| entry.is_compaction())
        .count();
    assert_eq!(compactions, 1, "the loop must not compact twice");
    assert!(session.branch_entries().iter().any(
        |entry| matches!(&entry.kind, EntryKind::Error(data) if data.code == "budget_context")
    ));
    assert_eq!(harness.provider.requests.lock().unwrap().len(), 0);
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
async fn mentioned_files_reach_the_model_and_count_as_read() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.rs"), "fn main() {}\n").unwrap();
    let harness = harness(
        dir.path(),
        vec![
            // No `read` first: the mention satisfies read-before-edit.
            tool_script("edit", json!({"path":"a.rs","old":"main","new":"run"})),
            text_script("done"),
        ],
        &["read", "edit"],
    );
    let (outcome, events) = run(&harness, "rename main in @a.rs.").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn run() {}\n"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, AgentEvent::Notice(text) if text == "attached a.rs"))
    );

    // The model saw the file; the transcript keeps the prompt as typed.
    let requests = harness.provider.requests.lock().unwrap();
    let user = requests[0]
        .messages
        .iter()
        .find_map(|message| match message {
            localharness::provider::ChatMessage::User { content } => Some(content.clone()),
            _ => None,
        })
        .expect("user message");
    assert!(user.contains("<file path=\"a.rs\">"), "{user}");
    assert!(user.contains("1  fn main() {}"), "{user}");
    assert_eq!(
        user_messages(&harness.session),
        vec!["rename main in @a.rs."]
    );
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

#[tokio::test]
async fn cancellation_mid_stream_keeps_partial_text() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![Script::EventsThenWait(vec![StreamEvent::Delta(
            "half an answer".into(),
        )])],
        &["read"],
    );
    let cancel = CancellationToken::new();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let core = harness.core.clone();
    let task = tokio::spawn(run_turn(core, "start".into(), tx, cancel.clone()));
    let mut saw_text = false;
    while let Some(event) = rx.recv().await {
        if matches!(event, AgentEvent::TextDelta(_)) {
            saw_text = true;
            break;
        }
        if matches!(event, AgentEvent::TurnDone(_)) {
            break;
        }
    }
    assert!(saw_text);
    cancel.cancel();
    let outcome = task.await.unwrap();
    assert_eq!(outcome.status, TurnStatus::Cancelled);
    let session = harness.session.lock().unwrap();
    let assistant = session
        .branch_entries()
        .iter()
        .find_map(|entry| match &entry.kind {
            EntryKind::Assistant(assistant) => assistant.content.clone(),
            _ => None,
        });
    assert_eq!(assistant.as_deref(), Some("half an answer"));
    assert!(
        session
            .branch_entries()
            .iter()
            .any(|entry| matches!(&entry.kind, EntryKind::Error(data) if data.code == "cancelled"))
    );
}

#[tokio::test]
async fn the_model_loads_a_skill_and_it_arrives_whole() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness(
        dir.path(),
        vec![
            tool_script("skill", serde_json::json!({"name": "security-red"})),
            text_script("Loaded; following the skill."),
        ],
        &["skill", "read"],
    );
    let (outcome, _) = run(&harness, "start a lab engagement").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    let result = last_tool_result(&harness.session);
    // Larger than the default 2.5k cap, yet inline and complete.
    assert!(
        result.chars().count() > 3000,
        "{} chars",
        result.chars().count()
    );
    assert!(!result.contains("(truncated;"), "{result}");
    assert!(result.contains("[skill: security-red]"));
    assert!(result.contains("references/reporting.md"));
    // The skill text reached the model in the follow-up request.
    let requests = harness.provider.requests.lock().unwrap();
    let last = requests.last().unwrap();
    let sent = format!("{:?}", last.messages);
    assert!(sent.contains("[skill: security-red]"));
}

#[tokio::test]
async fn strict_profile_rejects_a_parallel_call_at_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with_profile(
        dir.path(),
        vec![
            parallel_tool_script(&[
                ("bash", json!({"command": "echo one"})),
                ("bash", json!({"command": "echo two"})),
            ]),
            text_script("done"),
        ],
        "strict",
    );
    let (outcome, _) = run(&harness, "run one command").await;
    assert_eq!(outcome.status, TurnStatus::Done);

    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results[0].contains("one"), "{}", results[0]);
    assert!(
        results[1].contains("[tool_call_invalid]"),
        "the second call must be rejected: {}",
        results[1]
    );
    assert!(
        !results.iter().any(|result| result.contains("two")),
        "the rejected command must not run: {results:?}"
    );

    let requests = harness.provider.requests.lock().unwrap();
    let first = &requests[0];
    assert_eq!(first.tool_choice.as_deref(), Some("required"));
    assert_eq!(first.parallel_tool_calls, Some(false));
    assert_eq!(first.tools.len(), 6, "{:?}", first.tools);
    assert_eq!(harness.core.system_prompt, "system");
    for spec in &first.tools {
        assert!(
            spec.description.chars().count() <= 92,
            "{} has an unshortened description: {}",
            spec.name,
            spec.description
        );
    }
}

#[tokio::test]
async fn strict_profile_reminds_after_two_and_stops_after_three_invalid_calls() {
    let dir = tempfile::tempdir().unwrap();
    let invalid = || {
        parallel_tool_script(&[
            ("bash", json!({"command": "echo first"})),
            ("bash", json!({"command": "echo second"})),
        ])
    };
    let harness = harness_with_profile(dir.path(), vec![invalid(), invalid(), invalid()], "strict");
    let (outcome, _) = run(&harness, "keep making parallel calls").await;
    assert_eq!(outcome.status, TurnStatus::Error);
    assert_eq!(
        outcome.error.as_ref().map(|(code, _)| code.as_str()),
        Some("tool_call_invalid")
    );

    let messages = user_messages(&harness.session);
    assert!(
        messages
            .iter()
            .any(|message| message.contains("one JSON object, one tool")),
        "a compact reminder must arrive after two rejected calls: {messages:?}"
    );
    let session = harness.session.lock().unwrap();
    assert!(
        session
            .branch_entries()
            .iter()
            .any(|entry| matches!(&entry.kind, EntryKind::Error(data) if data.code == "tool_call_invalid")),
        "the turn must end with a tool_call_invalid error"
    );
    drop(session);
    assert_eq!(assistants(&harness.session).len(), 3);
}

#[tokio::test]
async fn malformed_parallel_fixture_is_contained_by_strict_not_full() {
    let mut strict_contained = 0;
    let mut full_parallel = 0;
    for case in 0..50 {
        let width = 2 + case % 3;
        let calls: Vec<(&str, serde_json::Value)> = (0..width)
            .map(|index| {
                (
                    "glob",
                    json!({"pattern": format!("fixture-{case}-{index}.rs")}),
                )
            })
            .collect();

        let strict_dir = tempfile::tempdir().unwrap();
        let strict = harness_with_profile(
            strict_dir.path(),
            vec![parallel_tool_script(&calls), bare_text_script("done")],
            "strict",
        );
        let (strict_outcome, _) = run(&strict, "run the fixture").await;
        let contained = strict_outcome.status == TurnStatus::Done
            || strict_outcome
                .error
                .as_ref()
                .is_some_and(|(code, _)| code == "tool_call_invalid");
        assert!(
            contained,
            "strict must either continue or stop with tool_call_invalid: {:?}",
            strict_outcome.error
        );
        let executed = executed_glob_results(&strict.session);
        if executed == 1 {
            strict_contained += 1;
        }

        let full_dir = tempfile::tempdir().unwrap();
        let full = harness_with_profile(
            full_dir.path(),
            vec![parallel_tool_script(&calls), bare_text_script("done")],
            "full",
        );
        let (full_outcome, _) = run(&full, "run the fixture").await;
        assert_eq!(full_outcome.status, TurnStatus::Done);
        let executed = executed_glob_results(&full.session);
        if executed == width {
            full_parallel += 1;
        }
    }
    eprintln!(
        "malformed 50-case fixture: strict one-call containment {strict_contained}/50; full parallel execution {full_parallel}/50"
    );
    assert_eq!(strict_contained, 50, "strict must reject every extra call");
    assert_eq!(full_parallel, 50, "full must remain parallel");
}

#[tokio::test]
async fn shell_profile_reads_edits_and_tests_with_bash_only() {
    let dir = tempfile::tempdir().unwrap();
    let harness = harness_with_profile(
        dir.path(),
        vec![
            tool_script(
                "bash",
                json!({"command": "printf 'alpha\\nbeta\\n' > toy.txt"}),
            ),
            tool_script("bash", json!({"command": "cat toy.txt"})),
            tool_script("bash", json!({"command": "sed -i 's/beta/gamma/' toy.txt"})),
            tool_script(
                "bash",
                json!({"command": "grep -qx gamma toy.txt && test \"$(wc -l < toy.txt)\" -eq 2"}),
            ),
            text_script("read, edited, and tested"),
        ],
        "shell",
    );
    assert_eq!(harness.core.tools.len(), 1);
    assert_eq!(harness.core.tools[0].name(), "bash");

    let (outcome, _) = run(&harness, "fix toy.txt and test it").await;
    assert_eq!(outcome.status, TurnStatus::Done);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("toy.txt")).unwrap(),
        "alpha\ngamma\n"
    );
    let results = tool_results(&harness.session);
    assert_eq!(results.len(), 4, "{results:?}");
    let session = harness.session.lock().unwrap();
    let errors: Vec<&ToolResultData> = session
        .branch_entries()
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::ToolResult(result) if result.is_error => Some(result),
            _ => None,
        })
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
    let requests = harness.provider.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .all(|request| request.tools.iter().all(|tool| tool.name == "bash")),
        "shell must never expose a non-bash tool"
    );
}

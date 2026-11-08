//! `thread` tool: split, annotate, close, or inspect the active thread.

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Map, Value, json};

use crate::session::{EntryKind, Session};
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult, optional_str, required_str};

/// Files named in a kickoff/entry; keeps the kickoff compact.
const MAX_KICKOFF_FILES: usize = 12;

pub struct ThreadTool;

impl ThreadTool {}

#[derive(Debug, Clone, Default)]
struct ThreadState {
    thread_id: String,
    title: String,
    objective: String,
    constraints: Vec<String>,
    files: Vec<String>,
    open: Option<String>,
    notes: Vec<String>,
    closed: bool,
    brief: Option<String>,
    at: Option<String>,
}

impl ThreadState {
    fn payload(&self) -> Map<String, Value> {
        let mut payload = Map::new();
        payload.insert("thread_id".into(), json!(self.thread_id));
        payload.insert("title".into(), json!(self.title));
        payload.insert("objective".into(), json!(self.objective));
        payload.insert("constraints".into(), json!(self.constraints));
        payload.insert("files".into(), json!(self.files));
        if let Some(open) = &self.open {
            payload.insert("open".into(), json!(open));
        }
        if !self.notes.is_empty() {
            payload.insert("notes".into(), json!(self.notes));
        }
        payload.insert("closed".into(), json!(self.closed));
        if let Some(brief) = &self.brief {
            payload.insert("brief".into(), json!(brief));
        }
        if let Some(at) = &self.at {
            payload.insert("at".into(), json!(at));
        }
        payload
    }
}

#[derive(Debug, Default)]
struct Snapshot {
    thread: Option<ThreadState>,
    todos: Vec<(String, String)>,
    files: Vec<String>,
    verify_passed: Option<bool>,
    verify_fresh: bool,
    thread_ids: Vec<String>,
}

fn string_list(payload: &Map<String, Value>, key: &str) -> Vec<String> {
    payload
        .get(key)
        .and_then(|value| value.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_str())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_thread(payload: &Map<String, Value>) -> Option<ThreadState> {
    let thread_id = payload
        .get("thread_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let title = payload
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let objective = payload
        .get("objective")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if thread_id.is_empty() && title.is_empty() && objective.is_empty() {
        return None;
    }
    let mut notes = string_list(payload, "notes");
    if let Some(note) = payload.get("note").and_then(|v| v.as_str())
        && !note.trim().is_empty()
        && !notes.iter().any(|existing| existing == note)
    {
        notes.push(note.trim().to_string());
    }
    let open = payload
        .get("open")
        .and_then(|value| match value {
            Value::String(text) => Some(text.trim().to_string()),
            Value::Array(values) => values
                .first()
                .and_then(|v| v.as_str())
                .map(|text| text.trim().to_string()),
            _ => None,
        })
        .filter(|text| !text.is_empty());
    Some(ThreadState {
        thread_id,
        title,
        objective,
        constraints: string_list(payload, "constraints"),
        files: string_list(payload, "files"),
        open,
        notes,
        closed: payload
            .get("closed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        brief: payload
            .get("brief")
            .and_then(|v| v.as_str())
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string),
        at: payload
            .get("at")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    })
}

fn parse_todos(payload: &Map<String, Value>) -> Vec<(String, String)> {
    payload
        .get("tasks")
        .and_then(|value| value.as_array())
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|task| match task {
                    Value::String(content) if !content.trim().is_empty() => {
                        Some((content.trim().to_string(), "pending".to_string()))
                    }
                    Value::Object(_) => {
                        let content = task
                            .get("content")
                            .or_else(|| task.get("task"))
                            .and_then(|v| v.as_str())?
                            .trim()
                            .to_string();
                        if content.is_empty() {
                            return None;
                        }
                        let status = task
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending")
                            .to_string();
                        Some((content, status))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn snapshot(ctx: &ToolContext) -> Result<Snapshot, ToolResult> {
    let session = ctx.session.lock().map_err(|_| {
        ToolResult::err(
            "session_lock",
            "session lock is poisoned; restart the harness",
        )
    })?;
    Ok(snapshot_of(&session))
}

fn snapshot_of(session: &Session) -> Snapshot {
    let mut snap = Snapshot::default();
    let mut last_mutation: Option<usize> = None;
    let mut last_verify: Option<(usize, bool)> = None;
    for (index, entry) in session.branch_entries().iter().enumerate() {
        let EntryKind::Custom(custom) = &entry.kind else {
            continue;
        };
        match custom.kind.as_str() {
            "fs-change" => {
                if let Some(path) = custom.payload.get("path").and_then(|v| v.as_str()) {
                    if !snap.files.iter().any(|existing| existing == path) {
                        snap.files.push(path.to_string());
                    }
                    last_mutation = Some(index);
                }
            }
            "todo-state" => {
                snap.todos = parse_todos(&custom.payload);
            }
            "verify-result" => {
                let passed = custom
                    .payload
                    .get("passed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                last_verify = Some((index, passed));
            }
            "thread-state" => {
                if let Some(state) = parse_thread(&custom.payload) {
                    if !state.thread_id.is_empty() {
                        snap.thread_ids.push(state.thread_id.clone());
                    }
                    snap.thread = Some(state);
                }
            }
            _ => {}
        }
    }
    snap.verify_passed = last_verify.map(|(_, passed)| passed);
    snap.verify_fresh = match (last_mutation, last_verify) {
        (Some(mutation), Some((verify, _))) => verify > mutation,
        (None, Some(_)) => true,
        _ => false,
    };
    snap
}

fn next_thread_id(ids: &[String]) -> String {
    let highest = ids
        .iter()
        .filter_map(|id| {
            let digits = id
                .trim()
                .strip_prefix('T')
                .or_else(|| id.trim().strip_prefix('t'))?;
            digits.trim().parse::<u32>().ok()
        })
        .max()
        .unwrap_or(0);
    format!("T{}", highest + 1)
}

fn derive_title(objective: &str) -> String {
    let first = objective.lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return "New thread".into();
    }
    let count = first.chars().count();
    if count <= 60 {
        return first.to_string();
    }
    let mut title: String = first.chars().take(60).collect();
    title.push('…');
    title
}

fn format_list(values: &[String]) -> String {
    if values.is_empty() {
        return "none recorded".into();
    }
    let shown: Vec<&str> = values
        .iter()
        .take(MAX_KICKOFF_FILES)
        .map(String::as_str)
        .collect();
    let mut text = shown.join(", ");
    if values.len() > MAX_KICKOFF_FILES {
        text.push_str(&format!(" (+{} more)", values.len() - MAX_KICKOFF_FILES));
    }
    text
}

fn verify_label(snap: &Snapshot) -> String {
    match (snap.verify_passed, snap.verify_fresh) {
        (None, _) => "not run".into(),
        (Some(true), true) => "passed (fresh)".into(),
        (Some(true), false) => "passed (stale: files changed after verify)".into(),
        (Some(false), true) => "failed".into(),
        (Some(false), false) => "failed (stale: files changed after verify)".into(),
    }
}

fn todo_line(todos: &[(String, String)]) -> String {
    if todos.is_empty() {
        return "todos: none".into();
    }
    let total = todos.len();
    let done = todos
        .iter()
        .filter(|(_, status)| status == "completed")
        .count();
    let active: Vec<&str> = todos
        .iter()
        .filter(|(_, status)| status == "in_progress")
        .map(|(content, _)| content.as_str())
        .collect();
    let mut line = format!("todos: {done}/{total} completed");
    if let Some(current) = active.first() {
        line.push_str(&format!(", in progress: {current}"));
    }
    line
}

fn thread_details(state: &ThreadState) -> Value {
    let mut entry = state.payload();
    entry.insert("kind".into(), json!("thread-state"));
    json!({"session": [Value::Object(entry)]})
}

fn active_thread(ctx: &ToolContext) -> Result<(ThreadState, Snapshot), ToolResult> {
    let snap = snapshot(ctx)?;
    match snap.thread.clone() {
        Some(thread) => Ok((thread, snap)),
        None => Err(ToolResult::err(
            "THREAD_STATE",
            "no active thread; start one with {\"action\":\"split\",\"title\":\"...\",\"objective\":\"...\"}",
        )),
    }
}

#[async_trait]
impl Tool for ThreadTool {
    fn name(&self) -> &'static str {
        "thread"
    }

    fn description(&self) -> &'static str {
        "Split, note, close, or inspect the active thread."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"action":{"type":"string","enum":["split","note","close","status"]},"title":{"type":"string"},"objective":{"type":"string"},"note":{"type":"string"}},"required":["action"],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let action = match required_str(&args, "action") {
            Ok(action) => action,
            Err(err) => return err,
        };
        if ctx.is_cancelled() {
            return ToolResult::err("cancelled", "thread cancelled; call it again when ready");
        }
        match action.as_str() {
            "split" => split(ctx, &args),
            "note" => note(ctx, &args),
            "close" => close(ctx, &args),
            "status" => status(ctx),
            other => ToolResult::err(
                "invalid_args",
                format!("unknown action {other:?}; actions are split, note, close, status"),
            ),
        }
    }
}

fn split(ctx: &ToolContext, args: &Value) -> ToolResult {
    let title_arg = optional_str(args, "title")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let objective_arg = optional_str(args, "objective")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    if title_arg.is_none() && objective_arg.is_none() {
        return ToolResult::err(
            "invalid_args",
            "thread split needs a title or objective; call {\"action\":\"split\",\"title\":\"...\",\"objective\":\"...\"}",
        );
    }
    let snap = match snapshot(ctx) {
        Ok(snap) => snap,
        Err(err) => return err,
    };
    let prior = snap.thread.clone().unwrap_or_default();

    let objective = objective_arg
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| Some(prior.objective.clone()).filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "not recorded; restate the objective before making changes".into());
    let title = title_arg
        .clone()
        .or_else(|| Some(prior.title.clone()).filter(|value| !value.is_empty()))
        .unwrap_or_else(|| derive_title(&objective));

    let mut constraints = prior.constraints.clone();
    let mut files = snap.files.clone();
    for file in &prior.files {
        if !files.iter().any(|existing| existing == file) {
            files.push(file.clone());
        }
    }
    // Keep the persisted kickoff entry compact.
    files.truncate(MAX_KICKOFF_FILES);
    constraints.truncate(24);

    let open = snap
        .todos
        .iter()
        .find(|(_, status)| status == "in_progress")
        .map(|(content, _)| content.clone())
        .or_else(|| {
            snap.todos
                .iter()
                .find(|(_, status)| status != "completed")
                .map(|(content, _)| content.clone())
        })
        .or_else(|| prior.open.clone())
        .unwrap_or_else(|| "Restate the objective and confirm the first concrete step.".into());

    let thread_id = next_thread_id(&snap.thread_ids);
    let note = optional_str(args, "note")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let state = ThreadState {
        thread_id: thread_id.clone(),
        title: title.clone(),
        objective: objective.clone(),
        constraints: constraints.clone(),
        files: files.clone(),
        open: Some(open.clone()),
        notes: note.into_iter().collect(),
        closed: false,
        brief: None,
        at: Some(Utc::now().to_rfc3339()),
    };

    let kickoff = format!(
        "This is a new thread; the previous context was compacted.\n\nThread {thread_id} — {title}\nObjective: {objective}\nConstraints: {}\nFiles: {}\nFirst open step: {open}\n\nContinue from the first open step. Re-read only the files above that this step needs.",
        format_list(&constraints),
        format_list(&files)
    );

    let content = format!(
        "thread {thread_id} split — {title}\nobjective: {objective}\ncontext is compacted; the kickoff message starts the new thread."
    );
    let mut entry = state.payload();
    entry.insert("kind".into(), json!("thread-state"));
    ToolResult::ok(content).with_details(json!({
        "session": [Value::Object(entry)],
        "thread_split": true,
        "end_turn": true,
        "kickoff": kickoff,
    }))
}

fn note(ctx: &ToolContext, args: &Value) -> ToolResult {
    let Some(note) = optional_str(args, "note")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    else {
        return ToolResult::err(
            "invalid_args",
            "thread note needs \"note\"; call {\"action\":\"note\",\"note\":\"what changed or what to remember\"}",
        );
    };
    let (mut thread, _snap) = match active_thread(ctx) {
        Ok(pair) => pair,
        Err(err) => return err,
    };
    thread.notes.push(note);
    thread.at = Some(Utc::now().to_rfc3339());
    let content = format!(
        "note added to thread {} — {}\nnotes: {}",
        thread.thread_id,
        thread.title,
        thread.notes.len()
    );
    ToolResult::ok(content).with_details(thread_details(&thread))
}

fn close(ctx: &ToolContext, args: &Value) -> ToolResult {
    let (mut thread, snap) = match active_thread(ctx) {
        Ok(pair) => pair,
        Err(err) => return err,
    };
    if thread.closed {
        return ToolResult::ok(format!(
            "thread {} — {} is already closed{}",
            thread.thread_id,
            thread.title,
            thread
                .brief
                .as_ref()
                .map(|brief| format!(": {brief}"))
                .unwrap_or_default()
        ));
    }
    let brief = optional_str(args, "note")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| {
            let total = snap.todos.len();
            let done = snap
                .todos
                .iter()
                .filter(|(_, status)| status == "completed")
                .count();
            format!(
                "Closed with {done}/{total} todos complete; verify {}.",
                verify_label(&snap)
            )
        });
    thread.closed = true;
    thread.brief = Some(brief.clone());
    thread.at = Some(Utc::now().to_rfc3339());
    let content = format!(
        "thread {} closed — {}\nbrief: {brief}",
        thread.thread_id, thread.title
    );
    ToolResult::ok(content).with_details(thread_details(&thread))
}

fn status(ctx: &ToolContext) -> ToolResult {
    let snap = match snapshot(ctx) {
        Ok(snap) => snap,
        Err(err) => return err,
    };
    let mut out = String::new();
    match &snap.thread {
        Some(thread) => {
            out.push_str(&format!("thread {} — {}\n", thread.thread_id, thread.title));
            out.push_str(&format!("objective: {}\n", thread.objective));
            out.push_str(&format!(
                "state: {}\n",
                if thread.closed { "closed" } else { "open" }
            ));
            if !thread.notes.is_empty() {
                out.push_str(&format!("notes: {}\n", thread.notes.len()));
            }
            if let Some(brief) = &thread.brief {
                out.push_str(&format!("brief: {brief}\n"));
            }
        }
        None => out.push_str("thread: none (no thread split yet)\n"),
    }
    out.push_str(&format!("{}\n", todo_line(&snap.todos)));
    out.push_str(&format!("verify: {}\n", verify_label(&snap)));
    out.push_str(&format!("files: {}\n", format_list(&snap.files)));
    ToolResult::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VerifyConfig;
    use crate::paths::Paths;
    use crate::security::SecurityState;
    use crate::session::{ArtifactStore, CustomData, SessionData};
    use crate::tools::{BgRegistry, ReadTracker};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn test_ctx() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        let paths = Paths {
            config_dir: cwd.join(".config"),
            data_dir: cwd.join(".data"),
            cwd: cwd.clone(),
            project_id: "test".into(),
        };
        let header = SessionData {
            cwd: cwd.display().to_string(),
            endpoint: "test".into(),
            model: "test".into(),
            profile: "test".into(),
            app: "test".into(),
        };
        let session = Session::create(&paths, header, true).expect("session");
        let ctx = ToolContext {
            cwd,
            path_jail: true,
            session: Arc::new(Mutex::new(session)),
            artifacts: Arc::new(ArtifactStore::ephemeral()),
            cancel: CancellationToken::new(),
            trust: true,
            verify: VerifyConfig::default(),
            profile: "test".into(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::new())),
            security: Arc::new(Mutex::new(SecurityState::default())),
            paths,
        };
        (dir, ctx)
    }

    fn append_custom(ctx: &ToolContext, kind: &str, payload: Value) {
        let mut session = ctx.session.lock().expect("session lock");
        let payload = payload.as_object().expect("payload").clone();
        session
            .append(EntryKind::Custom(CustomData {
                kind: kind.to_string(),
                payload,
            }))
            .expect("append");
    }

    fn apply_session_details(ctx: &ToolContext, result: &ToolResult) {
        let details = result.details.as_ref().expect("details");
        for entry in details["session"].as_array().expect("session array") {
            let kind = entry["kind"].as_str().expect("kind");
            let mut payload = entry.as_object().expect("entry").clone();
            payload.remove("kind");
            let mut session = ctx.session.lock().expect("session lock");
            session
                .append(EntryKind::Custom(CustomData {
                    kind: kind.to_string(),
                    payload,
                }))
                .expect("append");
        }
    }

    #[tokio::test]
    async fn split_returns_kickoff_and_end_turn() {
        let (_dir, ctx) = test_ctx();
        append_custom(
            &ctx,
            "todo-state",
            json!({"tasks":[
                {"content":"Add a trailing-comma regression test","status":"in_progress"},
                {"content":"Fix the parser","status":"pending"}
            ]}),
        );
        append_custom(&ctx, "fs-change", json!({"path":"src/parser.rs"}));

        let result = ThreadTool
            .call(
                &ctx,
                json!({"action":"split","title":"Parser drift","objective":"Accept trailing commas without regressing tests"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let details = result.details.as_ref().expect("details");
        assert_eq!(details["thread_split"], true);
        assert_eq!(details["end_turn"], true);
        let kickoff = details["kickoff"].as_str().expect("kickoff");
        assert!(
            kickoff.contains("This is a new thread; the previous context was compacted."),
            "{kickoff}"
        );
        assert!(kickoff.contains("Parser drift"), "{kickoff}");
        assert!(
            kickoff.contains("Accept trailing commas without regressing tests"),
            "{kickoff}"
        );
        assert!(kickoff.contains("src/parser.rs"), "{kickoff}");
        assert!(
            kickoff.contains("Add a trailing-comma regression test"),
            "{kickoff}"
        );
        let state = &details["session"][0];
        assert_eq!(state["kind"], "thread-state");
        assert_eq!(state["thread_id"], "T1");
    }

    #[tokio::test]
    async fn status_reads_back_plan_verify_and_files() {
        let (_dir, ctx) = test_ctx();
        append_custom(
            &ctx,
            "thread-state",
            json!({"thread_id":"T1","title":"Parser drift","objective":"Fix it","constraints":["no new deps"],"files":[],"open":"Add test"}),
        );
        append_custom(
            &ctx,
            "todo-state",
            json!({"tasks":[
                {"content":"Add test","status":"completed"},
                {"content":"Fix it","status":"in_progress"}
            ]}),
        );
        append_custom(&ctx, "fs-change", json!({"path":"src/parser.rs"}));
        append_custom(&ctx, "verify-result", json!({"passed":true}));

        let result = ThreadTool.call(&ctx, json!({"action":"status"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("thread T1 — Parser drift"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("todos: 1/2 completed"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("verify: passed (fresh)"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("src/parser.rs"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn note_requires_an_active_thread() {
        let (_dir, ctx) = test_ctx();
        let result = ThreadTool
            .call(&ctx, json!({"action":"note","note":"remember this"}))
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[THREAD_STATE]"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn split_then_note_retains_title_and_objective() {
        let (_dir, ctx) = test_ctx();
        let split = ThreadTool
            .call(
                &ctx,
                json!({"action":"split","title":"Parser drift","objective":"Fix it"}),
            )
            .await;
        assert!(!split.is_error, "{}", split.content);
        apply_session_details(&ctx, &split);

        let noted = ThreadTool
            .call(&ctx, json!({"action":"note","note":"tests fail on CRLF"}))
            .await;
        assert!(!noted.is_error, "{}", noted.content);
        let state = &noted.details.as_ref().expect("details")["session"][0];
        assert_eq!(state["title"], "Parser drift");
        assert_eq!(state["objective"], "Fix it");
        assert_eq!(state["notes"][0], "tests fail on CRLF");
    }
}

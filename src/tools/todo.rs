//! `todo` tool: plan and task state, persisted as a `todo-state` custom entry.

use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::session::{EntryKind, Session};
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult, optional_str, required_str};

/// Plans are capped; the harness enforces the cap, not the model.
const MAX_TASKS: usize = 20;

pub struct TodoTool;

impl TodoTool {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            TaskStatus::Pending => "○",
            TaskStatus::InProgress => "▸",
            TaskStatus::Completed => "✓",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "in_progress" => TaskStatus::InProgress,
            "completed" | "done" => TaskStatus::Completed,
            _ => TaskStatus::Pending,
        }
    }
}

#[derive(Debug, Clone)]
struct Task {
    content: String,
    status: TaskStatus,
}

#[derive(Debug, Clone, Default)]
struct TodoState {
    tasks: Vec<Task>,
    note: Option<String>,
}

impl TodoState {
    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn payload(&self) -> Map<String, Value> {
        let tasks: Vec<Value> = self
            .tasks
            .iter()
            .map(|task| {
                json!({
                    "content": task.content,
                    "status": task.status.as_str(),
                })
            })
            .collect();
        let mut payload = Map::new();
        payload.insert("tasks".into(), Value::Array(tasks));
        if let Some(note) = &self.note {
            payload.insert("note".into(), json!(note));
        }
        payload
    }

    fn render(&self) -> String {
        let total = self.tasks.len();
        let done = self
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::Completed)
            .count();
        let active = self
            .tasks
            .iter()
            .filter(|task| task.status == TaskStatus::InProgress)
            .count();
        let mut out = format!("todos: {done}/{total} completed");
        if active > 0 {
            out.push_str(&format!(", {active} in progress"));
        }
        out.push('\n');
        for (index, task) in self.tasks.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} {}\n",
                index + 1,
                task.status.glyph(),
                task.content
            ));
        }
        if let Some(note) = &self.note
            && !note.trim().is_empty()
        {
            out.push_str(&format!("note: {note}\n"));
        }
        out
    }

    fn open_tasks(&self) -> Vec<String> {
        self.tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.status != TaskStatus::Completed)
            .map(|(index, task)| format!("{}. {}", index + 1, task.content))
            .collect()
    }
}

fn parse_task(value: &Value) -> Option<Task> {
    match value {
        Value::String(content) if !content.trim().is_empty() => Some(Task {
            content: content.trim().to_string(),
            status: TaskStatus::Pending,
        }),
        Value::Object(_) => {
            let content = value
                .get("content")
                .or_else(|| value.get("task"))
                .and_then(|v| v.as_str())?
                .trim()
                .to_string();
            if content.is_empty() {
                return None;
            }
            let status = value
                .get("status")
                .and_then(|v| v.as_str())
                .map(TaskStatus::parse)
                .unwrap_or(TaskStatus::Pending);
            Some(Task { content, status })
        }
        _ => None,
    }
}

fn read_state(session: &Session) -> TodoState {
    let Some(entry) = session.latest_custom_on_path("todo-state") else {
        return TodoState::default();
    };
    let EntryKind::Custom(custom) = &entry.kind else {
        return TodoState::default();
    };
    let tasks = custom
        .payload
        .get("tasks")
        .and_then(|v| v.as_array())
        .map(|values| values.iter().filter_map(parse_task).collect())
        .unwrap_or_default();
    let note = custom
        .payload
        .get("note")
        .and_then(|v| v.as_str())
        .filter(|note| !note.trim().is_empty())
        .map(str::to_string);
    TodoState { tasks, note }
}

/// Status of one plan step, for display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// The current plan on the session's active branch, for the UI.
pub fn plan_snapshot(session: &Session) -> Vec<(String, PlanStatus)> {
    read_state(session)
        .tasks
        .into_iter()
        .map(|task| {
            let status = match task.status {
                TaskStatus::Pending => PlanStatus::Pending,
                TaskStatus::InProgress => PlanStatus::InProgress,
                TaskStatus::Completed => PlanStatus::Completed,
            };
            (task.content, status)
        })
        .collect()
}

fn state_details(state: &TodoState) -> Value {
    let mut entry = state.payload();
    entry.insert("kind".into(), json!("todo-state"));
    json!({"session": [Value::Object(entry)]})
}

fn no_plan_error() -> ToolResult {
    ToolResult::err(
        "TODO_STATE",
        "no plan yet; create one with {\"action\":\"plan\",\"tasks\":[\"first step\",\"second step\"]}",
    )
}

fn match_task(tasks: &[Task], needle: &str) -> Option<usize> {
    let needle = needle.trim();
    if needle.is_empty() {
        return None;
    }
    if let Some(index) = tasks.iter().position(|task| task.content == needle) {
        return Some(index);
    }
    // `list` numbers tasks from 1, so "2", "#2", and a copied "2. Add tests"
    // all name the second task rather than a substring match on "2".
    let digits: String = needle
        .trim_start_matches('#')
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let rest = needle.trim_start_matches('#')[digits.len()..].trim_start_matches(['.', ')', ':']);
    if let Ok(number) = digits.parse::<usize>()
        && (1..=tasks.len()).contains(&number)
    {
        let rest = rest.trim();
        if rest.is_empty()
            || tasks[number - 1]
                .content
                .to_lowercase()
                .contains(&rest.to_lowercase())
        {
            return Some(number - 1);
        }
        if let Some(index) = tasks.iter().position(|task| task.content == rest) {
            return Some(index);
        }
    }
    let lower = needle.to_lowercase();
    tasks
        .iter()
        .position(|task| task.content.to_lowercase().contains(&lower))
}

fn task_not_found(state: &TodoState, needle: &str) -> ToolResult {
    let open = state.open_tasks();
    let available = if open.is_empty() {
        "all tasks are completed".to_string()
    } else {
        open.join("; ")
    };
    ToolResult::err(
        "TASK_NOT_FOUND",
        format!(
            "no task matches {needle:?}; open tasks: {available}. Call todo {{\"action\":\"start\",\"task\":\"<substring>\"}} or list first."
        ),
    )
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &'static str {
        "todo"
    }

    fn description(&self) -> &'static str {
        "Plan and track tasks; required after two mutations."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "action": {"type": "string", "enum": ["plan", "start", "done", "list"], "description": "plan replaces the list and starts task 1; start/done take task; list shows state."},
                "tasks": {"type": "array", "items": {"type": "string"}, "description": "plan: ordered task titles."},
                "task": {"type": "string", "description": "start/done: task number from list (e.g. \"2\") or a title substring; done defaults to the active task."},
                "note": {"type": "string", "description": "done: what was finished and how it was checked."}
            }),
            &["action"],
        )
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
            return ToolResult::err("cancelled", "todo cancelled; call it again when ready");
        }
        match action.as_str() {
            "plan" => plan(ctx, &args),
            "start" => start(ctx, &args),
            "done" => done(ctx, &args),
            "list" => list(ctx),
            other => ToolResult::err(
                "invalid_args",
                format!("unknown action {other:?}; actions are plan, start, done, list"),
            ),
        }
    }
}

fn plan(_ctx: &ToolContext, args: &Value) -> ToolResult {
    let Some(values) = args.get("tasks").and_then(|v| v.as_array()) else {
        return ToolResult::err(
            "TODO_STATE",
            "plan needs \"tasks\"; call {\"action\":\"plan\",\"tasks\":[\"first step\",\"second step\"]}",
        );
    };
    let mut tasks = Vec::new();
    for (index, value) in values.iter().enumerate() {
        let Some(content) = value.as_str() else {
            return ToolResult::err(
                "TODO_STATE",
                format!(
                    "task {} is not a string; pass task text as a string",
                    index + 1
                ),
            );
        };
        let content = content.trim();
        if content.is_empty() {
            return ToolResult::err(
                "TODO_STATE",
                format!(
                    "task {} is empty; remove it or give it a one-line description",
                    index + 1
                ),
            );
        }
        tasks.push(Task {
            content: content.to_string(),
            status: TaskStatus::Pending,
        });
    }
    if tasks.is_empty() {
        return ToolResult::err(
            "TODO_STATE",
            "plan has no tasks; pass at least one step in {\"action\":\"plan\",\"tasks\":[...]}",
        );
    }
    if tasks.len() > MAX_TASKS {
        return ToolResult::err(
            "TODO_STATE",
            format!(
                "plan has {} tasks; the limit is {MAX_TASKS}. Merge steps into at most {MAX_TASKS} and call plan again.",
                tasks.len()
            ),
        );
    }
    tasks[0].status = TaskStatus::InProgress;
    let note = optional_str(args, "note")
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty());
    let state = TodoState { tasks, note };
    let content = format!("{}\nstarted: 1. {}", state.render(), state.tasks[0].content);
    ToolResult::ok(content).with_details(state_details(&state))
}

fn start(ctx: &ToolContext, args: &Value) -> ToolResult {
    let needle = optional_str(args, "task")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let Some(needle) = needle else {
        return ToolResult::err(
            "TODO_STATE",
            "start needs \"task\"; call {\"action\":\"start\",\"task\":\"<name or substring>\"}",
        );
    };
    let mut state = {
        let session = match ctx.session.lock() {
            Ok(session) => session,
            Err(_) => {
                return ToolResult::err(
                    "session_lock",
                    "session lock is poisoned; restart the harness",
                );
            }
        };
        read_state(&session)
    };
    if state.is_empty() {
        return no_plan_error();
    }
    let Some(index) = match_task(&state.tasks, &needle) else {
        return task_not_found(&state, &needle);
    };
    for task in &mut state.tasks {
        if task.status == TaskStatus::InProgress {
            task.status = TaskStatus::Pending;
        }
    }
    state.tasks[index].status = TaskStatus::InProgress;
    let content = format!(
        "{}\nstarted: {}. {}",
        state.render(),
        index + 1,
        state.tasks[index].content
    );
    ToolResult::ok(content).with_details(state_details(&state))
}

fn done(ctx: &ToolContext, args: &Value) -> ToolResult {
    let needle = optional_str(args, "task")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let note = optional_str(args, "note")
        .map(|note| note.trim().to_string())
        .filter(|note| !note.is_empty());
    let mut state = {
        let session = match ctx.session.lock() {
            Ok(session) => session,
            Err(_) => {
                return ToolResult::err(
                    "session_lock",
                    "session lock is poisoned; restart the harness",
                );
            }
        };
        read_state(&session)
    };
    if state.is_empty() {
        return no_plan_error();
    }
    let index = match &needle {
        Some(needle) => match match_task(&state.tasks, needle) {
            Some(index) => index,
            None => return task_not_found(&state, needle),
        },
        None => match state
            .tasks
            .iter()
            .position(|task| task.status == TaskStatus::InProgress)
        {
            Some(index) => index,
            None => {
                let open = state.open_tasks();
                let available = if open.is_empty() {
                    "all tasks are already completed".to_string()
                } else {
                    open.join("; ")
                };
                return ToolResult::err(
                    "TODO_STATE",
                    format!(
                        "no task is in progress; pass {{\"action\":\"done\",\"task\":\"<substring>\"}}. Open tasks: {available}"
                    ),
                );
            }
        },
    };
    state.tasks[index].status = TaskStatus::Completed;
    if note.is_some() {
        state.note = note;
    }
    let mut content = format!(
        "{}\ncompleted: {}. {}",
        state.render(),
        index + 1,
        state.tasks[index].content
    );
    if let Some(next) = state
        .tasks
        .iter()
        .find(|task| task.status == TaskStatus::InProgress)
    {
        content.push_str(&format!("\nin progress: {}", next.content));
    }
    ToolResult::ok(content).with_details(state_details(&state))
}

fn list(ctx: &ToolContext) -> ToolResult {
    let state = {
        let session = match ctx.session.lock() {
            Ok(session) => session,
            Err(_) => {
                return ToolResult::err(
                    "session_lock",
                    "session lock is poisoned; restart the harness",
                );
            }
        };
        read_state(&session)
    };
    if state.is_empty() {
        return ToolResult::ok(
            "no plan yet; create one with {\"action\":\"plan\",\"tasks\":[\"first step\",\"second step\"]}",
        );
    }
    ToolResult::ok(state.render()).with_details(state_details(&state))
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

    #[test]
    fn task_numbers_name_tasks_by_position() {
        let tasks: Vec<Task> = ["Fix bug 2", "Add tests", "Update docs"]
            .iter()
            .map(|content| Task {
                content: content.to_string(),
                status: TaskStatus::Pending,
            })
            .collect();
        // "2" is the second task, not a substring hit on "Fix bug 2".
        assert_eq!(match_task(&tasks, "2"), Some(1));
        assert_eq!(match_task(&tasks, "#3"), Some(2));
        assert_eq!(match_task(&tasks, "2. Add tests"), Some(1));
        assert_eq!(match_task(&tasks, "Fix bug 2"), Some(0));
        assert_eq!(match_task(&tasks, "docs"), Some(2));
        assert_eq!(match_task(&tasks, "9"), None);
    }

    fn test_ctx() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        let paths = Paths {
            config_dir: cwd.join(".config"),
            data_dir: cwd.join(".data"),
            cwd: cwd.clone(),
            project_id: "test".into(),
            sessions_dir_override: None,
        };
        let header = SessionData {
            cwd: cwd.display().to_string(),
            endpoint: "test".into(),
            model: "test".into(),
            profile: "test".into(),
            app: "test".into(),
            context: None,
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
            packs: Vec::new(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::new())),
            security: Arc::new(Mutex::new(SecurityState::default())),
            paths,
            lsp: Arc::new(crate::tools::lsp::LspService::new()),
            sandbox: None,
        };
        (dir, ctx)
    }

    fn apply(ctx: &ToolContext, result: &ToolResult) {
        let details = result.details.as_ref().expect("details");
        let entries = details["session"].as_array().expect("session array");
        let mut session = ctx.session.lock().expect("session lock");
        for entry in entries {
            let kind = entry["kind"].as_str().expect("kind");
            let mut payload = entry.as_object().expect("entry").clone();
            payload.remove("kind");
            session
                .append(EntryKind::Custom(CustomData {
                    kind: kind.to_string(),
                    payload,
                }))
                .expect("append");
        }
    }

    #[tokio::test]
    async fn plan_auto_starts_first_task() {
        let (_dir, ctx) = test_ctx();
        let result = TodoTool
            .call(
                &ctx,
                json!({"action":"plan","tasks":["write test","fix bug","verify"]}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let state = &result.details.as_ref().expect("details")["session"][0];
        let tasks = state["tasks"].as_array().expect("tasks");
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0]["status"], "in_progress");
        assert_eq!(tasks[1]["status"], "pending");
        assert_eq!(tasks[2]["status"], "pending");
    }

    #[tokio::test]
    async fn done_completes_current_and_list_reflects_it() {
        let (_dir, ctx) = test_ctx();
        let planned = TodoTool
            .call(
                &ctx,
                json!({"action":"plan","tasks":["write test","fix bug"]}),
            )
            .await;
        assert!(!planned.is_error, "{}", planned.content);
        apply(&ctx, &planned);

        let completed = TodoTool.call(&ctx, json!({"action":"done"})).await;
        assert!(!completed.is_error, "{}", completed.content);
        let state = &completed.details.as_ref().expect("details")["session"][0];
        assert_eq!(state["tasks"][0]["status"], "completed");
        assert_eq!(state["tasks"][1]["status"], "pending");
        apply(&ctx, &completed);

        let listed = TodoTool.call(&ctx, json!({"action":"list"})).await;
        assert!(!listed.is_error, "{}", listed.content);
        assert!(
            listed.content.contains("1/2 completed"),
            "{}",
            listed.content
        );
        assert!(
            listed.content.contains("✓ write test"),
            "{}",
            listed.content
        );
    }

    #[tokio::test]
    async fn start_matches_substring() {
        let (_dir, ctx) = test_ctx();
        let planned = TodoTool
            .call(
                &ctx,
                json!({"action":"plan","tasks":["write test","fix bug"]}),
            )
            .await;
        apply(&ctx, &planned);
        let started = TodoTool
            .call(&ctx, json!({"action":"start","task":"bug"}))
            .await;
        assert!(!started.is_error, "{}", started.content);
        let state = &started.details.as_ref().expect("details")["session"][0];
        assert_eq!(state["tasks"][0]["status"], "pending");
        assert_eq!(state["tasks"][1]["status"], "in_progress");
    }

    #[tokio::test]
    async fn done_without_plan_errors() {
        let (_dir, ctx) = test_ctx();
        let result = TodoTool.call(&ctx, json!({"action":"done"})).await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[TODO_STATE]"),
            "{}",
            result.content
        );
    }
}

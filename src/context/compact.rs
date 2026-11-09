//! Deterministic compaction: built from session entries, never the model.

use crate::session::{Entry, EntryKind, Session};

pub const COMPACTION_BUDGET_CHARS: usize = 4500;
pub const LAST_INSTRUCTION_CHARS: usize = 600;

#[derive(Debug, Clone, Default)]
pub struct FindingSummary {
    pub open: usize,
    pub high: usize,
    pub other: usize,
}

#[derive(Debug, Clone)]
pub struct CompactionSummary {
    pub summary: String,
    pub tokens_before: u64,
}

#[derive(Default)]
struct ThreadInfo {
    title: Option<String>,
    objective: Option<String>,
    constraints: Vec<String>,
    files: Vec<String>,
    done: Vec<String>,
    open: Vec<String>,
    facts: Vec<String>,
}

/// Build a compaction summary for the active branch.
pub fn build_summary(
    session: &Session,
    findings: Option<&FindingSummary>,
    budget_chars: usize,
) -> CompactionSummary {
    let branch = session.branch_entries();
    let thread = thread_info(session, &branch);
    let changed = changed_files(&branch);
    let verify = verify_status(&branch);
    let actions = last_actions(session, &branch);
    let instruction = last_instruction(&branch);

    let mut title = thread
        .title
        .clone()
        .unwrap_or_else(|| session.title())
        .trim()
        .to_string();
    if title.is_empty() {
        title = "main".into();
    }
    let title = clip(&title, 100);
    let objective = thread.objective.clone().unwrap_or_else(|| session.title());
    let objective = clip(objective.trim(), 400);

    let tokens_before: u64 = branch
        .iter()
        .map(|entry| entry_char_len(entry))
        .sum::<usize>()
        .div_ceil(4) as u64;

    let mut last_instruction = instruction.unwrap_or_default();
    if last_instruction.chars().count() > LAST_INSTRUCTION_CHARS {
        last_instruction = clip(&last_instruction, LAST_INSTRUCTION_CHARS);
    }

    let compose =
        |actions_kept: bool, facts_kept: bool, list_limit: usize, instruction_chars: usize| {
            let mut out = String::new();
            out.push_str("[localharness] deterministic compaction\n\n");
            out.push_str(&format!("## Active thread T1 - {title}\n"));
            if !objective.is_empty() {
                out.push_str(&format!("objective: {objective}\n"));
            }
            if !thread.constraints.is_empty() {
                out.push_str(&format!("constraints: {}\n", thread.constraints.join("; ")));
            }
            let files: Vec<&str> = changed
                .iter()
                .map(|s| s.as_str())
                .chain(thread.files.iter().map(|s| s.as_str()))
                .collect::<Vec<_>>();
            if !files.is_empty() {
                out.push_str(&format!(
                    "files: {}\n",
                    join_limited(&files, list_limit.max(6))
                ));
            }
            if !thread.done.is_empty() {
                out.push_str(&format!(
                    "done: {}\n",
                    join_limited_str(&thread.done, list_limit)
                ));
            }
            if !thread.open.is_empty() {
                out.push_str(&format!(
                    "open: {}\n",
                    join_limited_str(&thread.open, list_limit)
                ));
            }
            if facts_kept && !thread.facts.is_empty() {
                out.push_str(&format!(
                    "facts: {}\n",
                    join_limited_str(&thread.facts, list_limit)
                ));
            }
            if !changed.is_empty() {
                out.push_str("\n## Repo\n");
                out.push_str(&format!("changed: {}\n", join_limited_str(&changed, 12)));
                if let Some((passed, ago)) = &verify {
                    out.push_str(&format!(
                        "verify: {} ({} entries ago)\n",
                        if *passed { "passed" } else { "failed" },
                        ago
                    ));
                }
            }
            if let Some(findings) = findings
                && findings.open > 0
            {
                out.push_str("\n## Findings\n");
                out.push_str(&format!(
                    "{} open ({} high, {} medium)",
                    findings.open, findings.high, findings.other
                ));
                out.push('\n');
            }
            if !last_instruction.is_empty() {
                out.push_str("\n## Last instruction (verbatim)\n");
                let text = clip(&last_instruction, instruction_chars);
                out.push_str(&text);
                out.push('\n');
            }
            if actions_kept && !actions.is_empty() {
                out.push_str("\n## Last actions\n");
                for action in actions.iter().take(5) {
                    out.push_str(action);
                    out.push('\n');
                }
            }
            out.truncate(out.trim_end().len());
            out
        };

    let mut summary = compose(true, true, usize::MAX, LAST_INSTRUCTION_CHARS);
    if summary.chars().count() > budget_chars {
        summary = compose(false, true, usize::MAX, LAST_INSTRUCTION_CHARS);
    }
    if summary.chars().count() > budget_chars {
        summary = compose(false, false, usize::MAX, LAST_INSTRUCTION_CHARS);
    }
    if summary.chars().count() > budget_chars {
        summary = compose(false, false, 3, LAST_INSTRUCTION_CHARS);
    }
    if summary.chars().count() > budget_chars {
        summary = compose(false, false, 3, 300);
    }

    CompactionSummary {
        summary,
        tokens_before,
    }
}

fn thread_info(session: &Session, _branch: &[&Entry]) -> ThreadInfo {
    let mut info = ThreadInfo::default();
    if let Some(entry) = session.latest_custom_on_path("thread-state")
        && let EntryKind::Custom(custom) = &entry.kind
    {
        info.title = custom
            .payload
            .get("title")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        info.objective = custom
            .payload
            .get("objective")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        for key in ["constraints", "facts"] {
            if let Some(values) = custom.payload.get(key).and_then(|v| v.as_array()) {
                let target = if key == "constraints" {
                    &mut info.constraints
                } else {
                    &mut info.facts
                };
                *target = values
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
            }
        }
        if let Some(open) = custom.payload.get("open").and_then(|v| v.as_str()) {
            info.open.push(open.to_string());
        }
    }
    if let Some(entry) = session.latest_custom_on_path("todo-state")
        && let EntryKind::Custom(custom) = &entry.kind
        && let Some(tasks) = custom.payload.get("tasks").and_then(|v| v.as_array())
    {
        for task in tasks {
            let status = task
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("pending");
            let content = task.get("content").and_then(|v| v.as_str()).unwrap_or("");
            if content.is_empty() {
                continue;
            }
            if status == "completed" {
                info.done.push(content.to_string());
            } else {
                info.open.push(content.to_string());
            }
        }
    }
    info
}

fn changed_files(branch: &[&Entry]) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for entry in branch {
        if let EntryKind::Custom(custom) = &entry.kind
            && custom.kind == "fs-change"
            && let Some(path) = custom.payload.get("path").and_then(|v| v.as_str())
            && !files.iter().any(|f| f == path)
        {
            files.push(path.to_string());
        }
    }
    files
}

fn verify_status(branch: &[&Entry]) -> Option<(bool, usize)> {
    for (index, entry) in branch.iter().enumerate().rev() {
        if let EntryKind::Custom(custom) = &entry.kind
            && custom.kind == "verify-result"
        {
            let passed = custom
                .payload
                .get("passed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            return Some((passed, branch.len() - 1 - index));
        }
    }
    None
}

fn last_instruction(branch: &[&Entry]) -> Option<String> {
    branch.iter().rev().find_map(|entry| match &entry.kind {
        EntryKind::User(user) => Some(user.content.clone()),
        _ => None,
    })
}

fn last_actions(session: &Session, branch: &[&Entry]) -> Vec<String> {
    let mut actions = Vec::new();
    for entry in branch.iter().rev() {
        if actions.len() >= 8 {
            break;
        }
        let EntryKind::ToolResult(result) = &entry.kind else {
            continue;
        };
        let Some(owner) = session.tree.owner_of_result(&entry.id) else {
            continue;
        };
        let Some(assistant) = session.tree.get(&owner).and_then(|e| e.assistant()) else {
            continue;
        };
        let Some(call) = assistant
            .tool_calls
            .as_ref()
            .and_then(|calls| calls.iter().find(|c| c.id == result.call_id))
        else {
            continue;
        };
        let argument = call
            .arguments
            .get("path")
            .or_else(|| call.arguments.get("command"))
            .or_else(|| call.arguments.get("pattern"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let first_line = result.content.lines().next().unwrap_or("").trim();
        let status = if result.is_error { "error" } else { "ok" };
        let line = format!(
            "{}({}) -> {} {}",
            call.name,
            clip(argument, 60),
            status,
            clip(first_line, 100)
        );
        actions.push(line);
    }
    actions
}

fn entry_char_len(entry: &Entry) -> usize {
    match &entry.kind {
        EntryKind::User(u) => u.content.len(),
        EntryKind::Assistant(a) => {
            a.content.as_ref().map(|c| c.len()).unwrap_or(0)
                + a.reasoning.as_ref().map(|r| r.len()).unwrap_or(0)
                + a.tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|c| c.arguments.to_string().len() + 16)
                            .sum()
                    })
                    .unwrap_or(0)
        }
        EntryKind::ToolResult(t) => t.content.len(),
        EntryKind::Compaction(c) => c.summary.len(),
        EntryKind::Error(e) => e.message.len(),
        _ => 0,
    }
}

fn clip(text: &str, max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max_chars {
        return flat;
    }
    flat.chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>()
        + "…"
}

fn join_limited(items: &[&str], limit: usize) -> String {
    if items.len() <= limit {
        items.join(", ")
    } else {
        items[..limit].join(", ") + &format!(" … (+{} more)", items.len() - limit)
    }
}

fn join_limited_str(items: &[String], limit: usize) -> String {
    if items.len() <= limit {
        items.join("; ")
    } else {
        items[..limit].join("; ") + &format!(" … (+{} more)", items.len() - limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{AssistantData, CustomData, SessionData, ToolResultData, UserData};
    use serde_json::json;

    fn session(dir: &std::path::Path) -> Session {
        let mut paths = crate::paths::Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        Session::create(
            &paths,
            SessionData {
                cwd: dir.display().to_string(),
                endpoint: "http://127.0.0.1:8080".into(),
                model: "m".into(),
                profile: "full".into(),
                app: "test".into(),
            },
            true,
        )
        .unwrap()
    }

    #[test]
    fn summary_contains_spec_sections() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "Fix the parser bug and rerun tests".into(),
            }))
            .unwrap();
        session
            .append(EntryKind::Assistant(AssistantData {
                content: None,
                tool_calls: Some(vec![crate::session::ToolCall {
                    id: "call_1".into(),
                    name: "bash".into(),
                    arguments: json!({"command":"cargo test"}),
                }]),
                ..AssistantData::default()
            }))
            .unwrap();
        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "call_1".into(),
                content: "PASS 42 tests".into(),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        session
            .append(EntryKind::Custom(
                CustomData::new("fs-change").with("path", "src/parser.rs"),
            ))
            .unwrap();
        let summary = build_summary(&session, None, COMPACTION_BUDGET_CHARS);
        assert!(summary.summary.contains("## Active thread T1"));
        assert!(summary.summary.contains("objective: Fix the parser bug"));
        assert!(summary.summary.contains("src/parser.rs"));
        assert!(summary.summary.contains("## Last instruction"));
        assert!(summary.summary.contains("bash(cargo test)"));
        assert!(summary.tokens_before > 0);
    }

    #[test]
    fn budget_trims_actions_first() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "x ".repeat(500),
            }))
            .unwrap();
        let summary = build_summary(&session, None, 300);
        assert!(!summary.summary.contains("## Last actions"));
        assert!(summary.summary.contains("## Active thread T1"));
        assert!(summary.summary.contains("objective:"));
    }
}

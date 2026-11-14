//! Deterministic compaction: built from session entries, never the model.

use crate::context::budget::TokenBudget;
use crate::session::{Entry, EntryKind, Session};

/// Upper bound for a summary, kept for large windows.
pub const COMPACTION_BUDGET_CHARS: usize = 4500;
/// Floor so a tiny window still gets a usable summary.
pub const COMPACTION_BUDGET_MIN_CHARS: usize = 1200;
/// A summary may never take more than this share of the usable prompt.
pub const COMPACTION_MAX_PERCENT: usize = 40;
pub const LAST_INSTRUCTION_CHARS: usize = 600;
/// Share of the usable prompt (percent) an automatic compaction may keep as
/// the verbatim recent exchange.
pub const TAIL_MAX_PERCENT: usize = 20;
/// Prefix of every message the harness writes into the user role (gate
/// continuations, output-recovery nudges). Such a message is never the
/// user's instruction.
pub const HARNESS_PREFIX: &str = "[localharness]";

/// Window-derived summary limits: a third of the usable prompt characters,
/// clamped, plus the hard 40% ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SummaryBudget {
    pub target_chars: usize,
    pub max_chars: usize,
}

pub fn summary_budget(budget: &TokenBudget) -> SummaryBudget {
    let usable_chars = budget.chars_for(budget.usable_prompt());
    SummaryBudget {
        target_chars: (usable_chars / 3)
            .clamp(COMPACTION_BUDGET_MIN_CHARS, COMPACTION_BUDGET_CHARS),
        max_chars: (usable_chars * COMPACTION_MAX_PERCENT / 100).max(1),
    }
}

impl From<usize> for SummaryBudget {
    /// A fixed character budget with no hard ceiling, for callers that already
    /// computed their own limit.
    fn from(target_chars: usize) -> Self {
        Self {
            target_chars,
            max_chars: usize::MAX,
        }
    }
}

impl From<&TokenBudget> for SummaryBudget {
    fn from(budget: &TokenBudget) -> Self {
        summary_budget(budget)
    }
}

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

/// Character budget for the verbatim tail of an automatic compaction.
pub fn tail_budget(budget: &TokenBudget) -> usize {
    budget.chars_for(budget.usable_prompt()) * TAIL_MAX_PERCENT / 100
}

/// The first entry of the most recent exchange that fits in `max_chars`,
/// for a compaction to keep verbatim after its summary.
///
/// Without a tail, a compaction in the middle of a run leaves the model a
/// summary and nothing it just read, so it re-reads files and re-runs
/// commands. The tail starts on a user or assistant entry, holds every tool
/// result with its call, comes only from after the latest compaction, and
/// never covers the whole context (that would compact nothing).
pub fn recent_tail_start(session: &Session, max_chars: usize) -> Option<String> {
    let branch = session.branch_entries();
    let after = branch
        .iter()
        .rposition(|entry| entry.is_compaction())
        .map_or(0, |pos| pos + 1);
    let body = &branch[after..];
    let first_message = body
        .iter()
        .position(|entry| matches!(entry.kind, EntryKind::User(_) | EntryKind::Assistant(_)))?;
    let mut used = 0usize;
    let mut start = None;
    for (index, entry) in body.iter().enumerate().rev() {
        used += entry_char_len(entry);
        if used > max_chars {
            break;
        }
        if index > first_message
            && matches!(entry.kind, EntryKind::User(_) | EntryKind::Assistant(_))
        {
            start = Some(index);
        }
    }
    let tail = &body[start?..];
    let calls: std::collections::HashSet<&str> = tail
        .iter()
        .filter_map(|entry| entry.assistant())
        .flat_map(|assistant| assistant.tool_calls.as_deref().unwrap_or_default())
        .map(|call| call.id.as_str())
        .collect();
    let paired = tail.iter().all(|entry| match &entry.kind {
        EntryKind::ToolResult(result) => calls.contains(result.call_id.as_str()),
        _ => true,
    });
    paired.then(|| tail[0].id.clone())
}

/// Build a compaction summary for the active branch.
pub fn build_summary(
    session: &Session,
    findings: Option<&FindingSummary>,
    budget: impl Into<SummaryBudget>,
) -> CompactionSummary {
    build_summary_keeping(session, findings, budget, None)
}

/// Build a compaction summary for a compaction that keeps the entries from
/// `keep_from` verbatim. Actions and the last instruction come only from
/// before the kept range: the model sees the rest as they were.
pub fn build_summary_keeping(
    session: &Session,
    findings: Option<&FindingSummary>,
    budget: impl Into<SummaryBudget>,
    keep_from: Option<&str>,
) -> CompactionSummary {
    let budget = budget.into();
    let branch = session.branch_entries();
    let kept_at = keep_from.and_then(|id| branch.iter().position(|entry| entry.id == id));
    let summarized = &branch[..kept_at.unwrap_or(branch.len())];
    let thread = thread_info(session, &branch);
    let changed = changed_files(&branch);
    let verify = verify_status(&branch);
    let actions = last_actions(session, summarized);
    // The latest instruction, unless the kept range already shows it.
    let instruction = last_instruction(&branch)
        .filter(|_| kept_at.is_none_or(|kept| last_instruction(&branch[kept..]).is_none()));

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
    let objective = thread
        .objective
        .clone()
        .or_else(|| first_instruction(&branch))
        .unwrap_or_else(|| session.title());
    let objective = clip(objective.trim(), 400);

    // What this compaction replaces: the current context, not the whole
    // branch with every earlier compaction's history.
    let tokens_before: u64 = session
        .context_path()
        .iter()
        .map(|entry| entry_char_len(entry))
        .sum::<usize>()
        .div_ceil(4) as u64;

    let mut last_instruction = instruction.unwrap_or_default();
    if last_instruction.chars().count() > LAST_INSTRUCTION_CHARS {
        last_instruction = clip(&last_instruction, LAST_INSTRUCTION_CHARS);
    }

    let compose = |actions_kept: bool,
                   facts_kept: bool,
                   list_limit: usize,
                   instruction_chars: usize,
                   minimal: bool| {
        let mut out = String::new();
        out.push_str("[localharness] deterministic compaction\n\n");
        out.push_str(&format!("## Active thread T1 - {title}\n"));
        if !objective.is_empty() {
            out.push_str(&format!("objective: {objective}\n"));
        }
        if !minimal && !thread.constraints.is_empty() {
            out.push_str(&format!("constraints: {}\n", thread.constraints.join("; ")));
        }
        if !minimal {
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
        }
        if !thread.open.is_empty() {
            out.push_str(&format!(
                "open: {}\n",
                join_limited_str(&thread.open, list_limit)
            ));
        }
        if !minimal && facts_kept && !thread.facts.is_empty() {
            out.push_str(&format!(
                "facts: {}\n",
                join_limited_str(&thread.facts, list_limit)
            ));
        }
        if !minimal && !changed.is_empty() {
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
        if !minimal
            && let Some(findings) = findings
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
        if !minimal && actions_kept && !actions.is_empty() {
            out.push_str("\n## Last actions\n");
            for action in actions.iter().take(5) {
                out.push_str(action);
                out.push('\n');
            }
        }
        if kept_at.is_some() {
            out.push_str("\n## Recent exchange\nkept verbatim below; continue from it.\n");
        }
        out.truncate(out.trim_end().len());
        out
    };

    let mut summary = compose(true, true, usize::MAX, LAST_INSTRUCTION_CHARS, false);
    if summary.chars().count() > budget.target_chars {
        summary = compose(false, true, usize::MAX, LAST_INSTRUCTION_CHARS, false);
    }
    if summary.chars().count() > budget.target_chars {
        summary = compose(false, false, usize::MAX, LAST_INSTRUCTION_CHARS, false);
    }
    if summary.chars().count() > budget.target_chars {
        summary = compose(false, false, 3, LAST_INSTRUCTION_CHARS, false);
    }
    if summary.chars().count() > budget.target_chars {
        summary = compose(false, false, 3, 300, false);
    }
    // The hard postcondition: never let the summary take more than 40% of the
    // usable prompt. Keep only the objective, open tasks, and last instruction.
    if summary.chars().count() > budget.max_chars {
        summary = compose(false, false, 3, 300, true);
    }
    if summary.chars().count() > budget.max_chars {
        summary = compose(false, false, 1, 120, true);
    }
    if summary.chars().count() > budget.max_chars {
        summary = clip(&summary, budget.max_chars);
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

/// User entries the user wrote, oldest first: harness continuations and
/// nudges are stored in the user role but are not instructions.
fn user_instructions<'a>(branch: &'a [&'a Entry]) -> impl DoubleEndedIterator<Item = &'a str> {
    branch.iter().filter_map(|entry| match &entry.kind {
        EntryKind::User(user) if !user.content.trim_start().starts_with(HARNESS_PREFIX) => {
            Some(user.content.as_str())
        }
        _ => None,
    })
}

fn last_instruction(branch: &[&Entry]) -> Option<String> {
    user_instructions(branch).next_back().map(str::to_string)
}

/// The first line of the first user instruction on the branch.
fn first_instruction(branch: &[&Entry]) -> Option<String> {
    user_instructions(branch)
        .find_map(|text| text.lines().map(str::trim).find(|line| !line.is_empty()))
        .map(str::to_string)
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
        EntryKind::User(u) => {
            u.content.len()
                + u.attachments
                    .iter()
                    .map(|attachment| attachment.content.len())
                    .sum::<usize>()
        }
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
                redact: true,
                context: None,
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
        let summary = build_summary(
            &session,
            None,
            summary_budget(&TokenBudget::new(32_768, 4096)),
        );
        assert!(summary.summary.contains("## Active thread T1"));
        assert!(summary.summary.contains("objective: Fix the parser bug"));
        assert!(summary.summary.contains("src/parser.rs"));
        assert!(summary.summary.contains("## Last instruction"));
        assert!(summary.summary.contains("bash(cargo test)"));
        assert!(summary.tokens_before > 0);
    }

    fn user(session: &mut Session, text: &str) -> String {
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: text.into(),
            }))
            .unwrap()
            .id
            .clone()
    }

    /// One assistant call and its result; returns the assistant entry id.
    fn step(session: &mut Session, id: &str, command: &str, output: &str) -> String {
        let entry = session
            .append(EntryKind::Assistant(AssistantData {
                content: None,
                tool_calls: Some(vec![crate::session::ToolCall {
                    id: id.into(),
                    name: "bash".into(),
                    arguments: json!({ "command": command }),
                }]),
                ..AssistantData::default()
            }))
            .unwrap();
        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: id.into(),
                content: output.into(),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        entry.id.clone()
    }

    #[test]
    fn tail_starts_on_a_step_boundary_and_never_covers_everything() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        user(&mut session, "fix the build");
        let first_step = step(&mut session, "call_1", "cargo build", &"old ".repeat(400));
        let recent = step(&mut session, "call_2", "cargo test", "2 failed");
        // Room for the last step only: it starts at its assistant entry, so
        // the result travels with its call.
        assert_eq!(
            recent_tail_start(&session, 200).as_deref(),
            Some(recent.as_str())
        );
        // Room for everything: keeping it all would compact nothing.
        assert_eq!(
            recent_tail_start(&session, 100_000).as_deref(),
            Some(first_step.as_str())
        );
        // Room for nothing.
        assert_eq!(recent_tail_start(&session, 5), None);
    }

    #[test]
    fn harness_nudges_are_not_the_last_instruction() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        user(&mut session, "Rename the config loader");
        step(&mut session, "call_1", "cargo test", "ok");
        user(
            &mut session,
            "[localharness] verify gate: files changed after the last passing verification.",
        );
        let summary = build_summary(&session, None, 4000);
        assert!(
            summary
                .summary
                .contains("## Last instruction (verbatim)\nRename the config loader"),
            "{}",
            summary.summary
        );
        assert!(
            !summary.summary.contains("verify gate"),
            "{}",
            summary.summary
        );
    }

    #[test]
    fn a_kept_tail_is_not_repeated_in_the_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        user(&mut session, "first task");
        step(&mut session, "call_1", "cargo build", "built");
        let user_entry = user(&mut session, "now run the tests");
        let kept = step(&mut session, "call_2", "cargo test", "2 failed");
        let summary = build_summary_keeping(&session, None, 4000, Some(&kept));
        let text = &summary.summary;
        assert!(text.contains("## Recent exchange"), "{text}");
        assert!(text.contains("bash(cargo build)"), "{text}");
        assert!(!text.contains("cargo test"), "kept action repeated: {text}");
        // The latest instruction precedes the tail, so the summary keeps it.
        assert!(text.contains("now run the tests"), "{text}");

        // An instruction inside the tail is shown there, not in the summary.
        let summary = build_summary_keeping(&session, None, 4000, Some(&user_entry));
        assert!(
            !summary.summary.contains("## Last instruction"),
            "{}",
            summary.summary
        );
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
        let summary = build_summary(
            &session,
            None,
            SummaryBudget {
                target_chars: 300,
                max_chars: usize::MAX,
            },
        );
        assert!(!summary.summary.contains("## Last actions"));
        assert!(summary.summary.contains("## Active thread T1"));
        assert!(summary.summary.contains("objective:"));
    }

    #[test]
    fn small_window_summary_is_capped_at_forty_percent() {
        let budget = TokenBudget::new(4096, 1024);
        let limits = summary_budget(&budget);
        let usable_chars = budget.chars_for(budget.usable_prompt());
        assert_eq!(
            limits.max_chars,
            usable_chars * COMPACTION_MAX_PERCENT / 100
        );

        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        let constraints: Vec<String> = (0..12)
            .map(|index| format!("constraint {index} {}", "c".repeat(300)))
            .collect();
        let facts: Vec<String> = (0..8)
            .map(|index| format!("fact {index} {}", "f".repeat(300)))
            .collect();
        session
            .append(EntryKind::Custom(
                CustomData::new("thread-state")
                    .with("title", "small window")
                    .with("objective", "finish the slice")
                    .with("constraints", json!(constraints))
                    .with("facts", json!(facts))
                    .with("open", "keep the objective and open tasks".repeat(4)),
            ))
            .unwrap();
        session
            .append(EntryKind::Custom(
                CustomData::new("fs-change").with("path", "src/parser.rs"),
            ))
            .unwrap();
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "x ".repeat(4000),
            }))
            .unwrap();

        let findings = FindingSummary {
            open: 3,
            high: 1,
            other: 2,
        };
        let summary = build_summary(&session, Some(&findings), limits);
        assert!(
            summary.summary.chars().count() <= limits.max_chars,
            "{} chars > {} cap",
            summary.summary.chars().count(),
            limits.max_chars
        );
        assert!(summary.summary.contains("objective: finish the slice"));
        assert!(summary.summary.contains("open:"));
        assert!(!summary.summary.contains("## Repo"));
        assert!(!summary.summary.contains("## Findings"));
        assert!(!summary.summary.contains("## Last actions"));
    }

    #[test]
    fn large_window_keeps_the_fixed_summary_budget() {
        let limits = summary_budget(&TokenBudget::new(32_768, 4096));
        assert_eq!(limits.target_chars, COMPACTION_BUDGET_CHARS);
        assert!(limits.max_chars > limits.target_chars);

        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "Fix the parser bug and rerun tests".into(),
            }))
            .unwrap();
        session
            .append(EntryKind::Custom(
                CustomData::new("fs-change").with("path", "src/parser.rs"),
            ))
            .unwrap();
        let fixed = build_summary(&session, None, COMPACTION_BUDGET_CHARS);
        let windowed = build_summary(&session, None, limits);
        assert_eq!(fixed.summary, windowed.summary);
    }
}

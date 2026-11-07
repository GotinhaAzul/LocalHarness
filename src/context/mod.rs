//! Context assembly from a session branch, token estimate, state block.

pub mod archive;
pub mod budget;
pub mod compact;

use crate::provider::{ChatMessage, ToolCall};
use crate::session::{AssistantData, EntryKind, Session};

pub use archive::{BATCH_CAP_CHARS, CappedContent, RECALL_CAP_CHARS, RESULT_CAP_CHARS, cap_batch, cap_result};
pub use budget::TokenBudget;
pub use compact::{CompactionSummary, FindingSummary, build_summary};

#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub messages: Vec<ChatMessage>,
    pub estimated_tokens: u32,
    pub char_count: usize,
}

/// Build provider messages for the active branch. The latest compaction
/// becomes a system message; custom, checkout, error, and header entries are
/// not sent.
pub fn assemble(
    base_system: &str,
    session: &Session,
    state_block: Option<&str>,
    budget: &TokenBudget,
) -> AssembledContext {
    let mut messages = vec![ChatMessage::System {
        content: base_system.to_string(),
    }];
    let mut had_compaction = false;

    for entry in session.context_path() {
        match &entry.kind {
            EntryKind::Session(_)
            | EntryKind::Checkout(_)
            | EntryKind::Custom(_)
            | EntryKind::Error(_) => {}
            EntryKind::Compaction(compaction) => {
                had_compaction = true;
                messages.push(ChatMessage::System {
                    content: compaction.summary.clone(),
                });
            }
            EntryKind::User(user) => {
                messages.push(ChatMessage::User {
                    content: user.content.clone(),
                });
            }
            EntryKind::Assistant(assistant) => {
                push_assistant(&mut messages, assistant, session);
            }
            EntryKind::ToolResult(_) => {
                // Emitted with the assistant message that requested it.
            }
        }
    }

    if had_compaction
        && let Some(state) = state_block
        && !state.trim().is_empty()
    {
        messages.push(ChatMessage::System {
            content: state.to_string(),
        });
    }

    let char_count = messages.iter().map(|m| m.char_len()).sum();
    let estimated_tokens = budget.estimate_messages(&messages);
    AssembledContext {
        messages,
        estimated_tokens,
        char_count,
    }
}

fn push_assistant(messages: &mut Vec<ChatMessage>, assistant: &AssistantData, session: &Session) {
    let tool_calls: Vec<ToolCall> = assistant
        .tool_calls
        .as_ref()
        .map(|calls| {
            calls
                .iter()
                .map(|call| ToolCall {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    arguments: call.arguments.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    messages.push(ChatMessage::Assistant {
        content: assistant.content.clone(),
        tool_calls: tool_calls.clone(),
    });
    for call in &tool_calls {
        let result = session
            .branch_entries()
            .into_iter()
            .find_map(|entry| match &entry.kind {
                EntryKind::ToolResult(result) if result.call_id == call.id => Some(result),
                _ => None,
            });
        let content = match result {
            Some(result) => result.content.clone(),
            None => format!(
                "[cancelled] no result was recorded for {}; the step was interrupted",
                call.name
            ),
        };
        messages.push(ChatMessage::Tool {
            call_id: call.id.clone(),
            content,
        });
    }
}

/// Short post-compaction state block: changed files, verify, thread, findings,
/// and budget. Keeps the model from re-reading half the repository.
pub fn state_block(
    session: &Session,
    findings: Option<&FindingSummary>,
    used_percent: f32,
) -> String {
    let mut out = String::from("[localharness] state\n");
    let mut changed: Vec<String> = Vec::new();
    let mut verify: Option<(bool, usize)> = None;
    for entry in session.branch_entries() {
        if let EntryKind::Custom(custom) = &entry.kind {
            if custom.kind == "fs-change"
                && let Some(path) = custom.payload.get("path").and_then(|v| v.as_str())
                && !changed.iter().any(|p| p == path)
            {
                changed.push(path.to_string());
            }
            if custom.kind == "verify-result" {
                let passed = custom
                    .payload
                    .get("passed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                verify = Some((passed, 0));
            }
        }
    }
    if !changed.is_empty() {
        out.push_str(&format!("changed: {}\n", changed.join(", ")));
    }
    if let Some((passed, _)) = verify {
        out.push_str(&format!(
            "verify: {}",
            if passed { "passed" } else { "failed" }
        ));
    } else {
        out.push_str("verify: not run");
    }
    out.push('\n');
    if let Some(entry) = session.latest_custom_on_path("thread-state")
        && let EntryKind::Custom(custom) = &entry.kind
        && let Some(title) = custom.payload.get("title").and_then(|v| v.as_str())
    {
        out.push_str(&format!("thread: {title}\n"));
    }
    if let Some(findings) = findings
        && findings.open > 0
    {
        out.push_str(&format!(
            "findings: {} open ({} high)\n",
            findings.open, findings.high
        ));
    }
    out.push_str(&format!("context: {:.0}% of usable prompt", used_percent));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::*;
    use serde_json::json;

    fn session(dir: &std::path::Path) -> Session {
        let mut paths = crate::paths::Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        Session::create(
            &paths,
            SessionData {
                cwd: dir.display().to_string(),
                endpoint: "e".into(),
                model: "m".into(),
                profile: "full".into(),
                app: "test".into(),
            },
            true,
        )
        .unwrap()
    }

    #[test]
    fn assembly_pairs_tool_calls_with_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                content: "read it".into(),
            }))
            .unwrap();
        session
            .append(EntryKind::Assistant(AssistantData {
                content: Some("checking".into()),
                tool_calls: Some(vec![crate::session::ToolCall {
                    id: "call_1".into(),
                    name: "read".into(),
                    arguments: json!({"path":"a.rs"}),
                }]),
                ..AssistantData::default()
            }))
            .unwrap();
        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "call_1".into(),
                content: "file body".into(),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        let budget = TokenBudget::new(32768, 4096);
        let assembled = assemble("system", &session, None, &budget);
        assert_eq!(assembled.messages.len(), 4); // system, user, assistant, tool
        assert_eq!(assembled.messages[3].role(), "tool");
        assert!(assembled.estimated_tokens > 0);
    }

    #[test]
    fn compaction_becomes_system_message_and_state_block_follows() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                content: "old".into(),
            }))
            .unwrap();
        session
            .append(EntryKind::Compaction(CompactionData {
                summary: "[localharness] summary".into(),
                tokens_before: 100,
                reason: "auto".into(),
                thread_id: None,
            }))
            .unwrap();
        session
            .append(EntryKind::User(UserData {
                content: "new".into(),
            }))
            .unwrap();
        let budget = TokenBudget::new(32768, 4096);
        let assembled = assemble("system", &session, Some("STATE"), &budget);
        assert!(matches!(
            &assembled.messages[1],
            ChatMessage::System { content } if content == "[localharness] summary"
        ));
        assert!(matches!(
            &assembled.messages.last().unwrap(),
            ChatMessage::System { content } if content == "STATE"
        ));
    }
}

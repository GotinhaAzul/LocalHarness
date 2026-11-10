//! Context assembly from a session branch, token estimate, state block.

pub mod archive;
pub mod budget;
pub mod compact;

use crate::provider::{ChatMessage, ToolCall};
use std::collections::HashMap;

use crate::session::{AssistantData, Entry, EntryKind, Session, ToolResultData};

pub use archive::{
    BATCH_CAP_CHARS, CappedContent, RECALL_CAP_CHARS, RESULT_CAP_CHARS, cap_batch, cap_result,
};
pub use budget::{StaticPrompt, TokenBudget};
pub use compact::{CompactionSummary, FindingSummary, build_summary};

#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub messages: Vec<ChatMessage>,
    pub estimated_tokens: u32,
    pub char_count: usize,
}

/// Build provider messages for the active branch. Every system message (base
/// prompt, compaction summaries, state block) is folded into one leading
/// system turn: strict chat templates reject a system message after a
/// non-system one. Custom, checkout, error, and header entries are not sent.
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
    let path = session.context_path();
    let results = ToolResults::index(&path);

    for entry in path {
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
                    content: user.model_content(),
                });
            }
            EntryKind::Assistant(assistant) => {
                push_assistant(&mut messages, assistant, entry.seq, &results);
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

    // Folding keeps the logical order (base, summaries in path order, state)
    // while guaranteeing at most one system message, at the front.
    let messages = fold_system_messages(messages);

    let char_count = messages.iter().map(|m| m.char_len()).sum();
    let estimated_tokens = budget.estimate_messages(&messages);
    AssembledContext {
        messages,
        estimated_tokens,
        char_count,
    }
}

/// Move every system message's content into one leading system turn, keeping
/// each part's relative order. Strict chat templates (llama.cpp `--jinja`
/// among them) reject a system message after a user or assistant turn.
fn fold_system_messages(messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    let mut system = Vec::new();
    let mut rest = Vec::with_capacity(messages.len());
    for message in messages {
        match message {
            ChatMessage::System { content } => system.push(content),
            other => rest.push(other),
        }
    }
    let mut folded = Vec::with_capacity(rest.len() + 1);
    folded.push(ChatMessage::System {
        content: system.join("\n\n"),
    });
    folded.extend(rest);
    folded
}

/// Tool results by call id, in path order. Built once per assembly: scanning
/// the branch per call made assembly quadratic in session length.
struct ToolResults<'a> {
    by_id: HashMap<&'a str, Vec<(u64, &'a ToolResultData)>>,
}

impl<'a> ToolResults<'a> {
    fn index(path: &[&'a Entry]) -> Self {
        let mut by_id: HashMap<&str, Vec<(u64, &ToolResultData)>> = HashMap::new();
        for entry in path {
            if let EntryKind::ToolResult(result) = &entry.kind {
                by_id
                    .entry(result.call_id.as_str())
                    .or_default()
                    .push((entry.seq, result));
            }
        }
        Self { by_id }
    }

    /// The first result for `call_id` recorded after the assistant entry at
    /// `after`. Call ids are not unique across messages when a server omits
    /// them, so matching by id alone can replay another call's result.
    fn after(&self, call_id: &str, after: u64) -> Option<&'a ToolResultData> {
        self.by_id
            .get(call_id)?
            .iter()
            .find(|(seq, _)| *seq > after)
            .map(|(_, result)| *result)
    }
}

fn push_assistant(
    messages: &mut Vec<ChatMessage>,
    assistant: &AssistantData,
    seq: u64,
    results: &ToolResults<'_>,
) {
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
        let content = match results.after(&call.id, seq) {
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
                context: None,
            },
            true,
        )
        .unwrap()
    }

    #[test]
    fn repeated_call_ids_pair_with_their_own_results() {
        // Servers that omit ids used to get `call_1` in every message; each
        // call must still replay with the result recorded after it.
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        for body in ["first body", "second body"] {
            session
                .append(EntryKind::Assistant(AssistantData {
                    tool_calls: Some(vec![crate::session::ToolCall {
                        id: "call_1".into(),
                        name: "read".into(),
                        arguments: json!({"path": body}),
                    }]),
                    ..AssistantData::default()
                }))
                .unwrap();
            session
                .append(EntryKind::ToolResult(ToolResultData {
                    call_id: "call_1".into(),
                    content: body.into(),
                    is_error: false,
                    details: None,
                    artifact: None,
                }))
                .unwrap();
        }
        let budget = TokenBudget::new(32768, 4096);
        let assembled = assemble("system", &session, None, &budget);
        let tool_bodies: Vec<&str> = assembled
            .messages
            .iter()
            .filter_map(|message| match message {
                ChatMessage::Tool { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tool_bodies, vec!["first body", "second body"]);
    }

    #[test]
    fn assembly_pairs_tool_calls_with_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
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
        assert_leading_system(&assembled.messages, "system");
        assert_eq!(assembled.messages[3].role(), "tool");
        assert!(assembled.estimated_tokens > 0);
    }

    fn assert_leading_system(messages: &[ChatMessage], expected: &str) {
        assert!(matches!(
            &messages[0],
            ChatMessage::System { content } if content == expected
        ));
        assert!(
            messages[1..]
                .iter()
                .all(|message| !matches!(message, ChatMessage::System { .. })),
            "no system message may follow a non-system message"
        );
    }

    #[test]
    fn compaction_is_folded_into_the_leading_system_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
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
                attachments: Vec::new(),
                content: "new".into(),
            }))
            .unwrap();
        let budget = TokenBudget::new(32768, 4096);
        let assembled = assemble("system", &session, Some("STATE"), &budget);
        assert_leading_system(
            &assembled.messages,
            "system\n\n[localharness] summary\n\nSTATE",
        );
        assert_eq!(assembled.messages.len(), 2); // system, user
        assert!(matches!(
            &assembled.messages[1],
            ChatMessage::User { content } if content == "new"
        ));
    }

    #[test]
    fn compacted_tool_turns_keep_the_system_message_leading() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
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
                attachments: Vec::new(),
                content: "new".into(),
            }))
            .unwrap();
        session
            .append(EntryKind::Assistant(AssistantData {
                content: Some("working".into()),
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
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "next".into(),
            }))
            .unwrap();
        let budget = TokenBudget::new(32768, 4096);
        let assembled = assemble("system", &session, Some("STATE"), &budget);
        assert_leading_system(
            &assembled.messages,
            "system\n\n[localharness] summary\n\nSTATE",
        );
        let roles: Vec<&str> = assembled
            .messages
            .iter()
            .map(|message| message.role())
            .collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "tool", "user"]);
    }
}

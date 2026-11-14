//! Context assembly from a session branch, token estimate, state block.

pub mod archive;
pub mod budget;
pub mod compact;

use crate::provider::{ChatMessage, ImageData, ToolCall};
use std::collections::HashMap;

use crate::session::{
    ArtifactStore, AssistantData, Entry, EntryKind, ImageRef, Session, ToolResultData,
};

pub use archive::{
    BATCH_CAP_CHARS, CappedContent, RECALL_CAP_CHARS, RESULT_CAP_CHARS, cap_batch, cap_result,
};
pub use budget::{JSON_CHARS_PER_TOKEN, StaticPrompt, TokenBudget, ToolSpecCost};
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
                let mut content = user.model_content();
                let mut images = Vec::new();
                // A described image went in as text; only native ones carry
                // pixels.
                for attachment in &user.attachments {
                    if let Some(image) = &attachment.image
                        && attachment.described_by.is_none()
                    {
                        match load_image(&session.artifacts, image, &attachment.path) {
                            Ok(data) => images.push(data),
                            Err(note) => content.push_str(&format!("\n{note}")),
                        }
                    }
                }
                messages.push(ChatMessage::User { content, images });
            }
            EntryKind::Assistant(assistant) => {
                push_assistant(
                    &mut messages,
                    assistant,
                    entry.seq,
                    &results,
                    &session.artifacts,
                );
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

/// A replayed call/result stream is inconsistent. The turn must not send it:
/// a mismatch pairs a result with another call's output or drops it entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPairError {
    code: &'static str,
    pub message: String,
}

impl ToolPairError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl std::fmt::Display for ToolPairError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolPairError {}

/// Assert one result per call and one call per result on the replay path.
///
/// Internal ids make this exact: engines may reuse ids or omit them, but the
/// ledger gives every assistant call one unique id before it is written. A
/// mismatch is a typed, pre-request failure rather than another call's output
/// silently appearing in a tool message.
pub fn validate_tool_pairs(session: &Session) -> Result<(), ToolPairError> {
    let path = session.context_path();
    let mut calls: HashMap<&str, (&str, u64)> = HashMap::new();
    let mut results: HashMap<&str, u64> = HashMap::new();
    for entry in &path {
        match &entry.kind {
            EntryKind::Assistant(assistant) => {
                for call in assistant.tool_calls.as_deref().unwrap_or_default() {
                    if call.id.trim().is_empty() {
                        return Err(ToolPairError::new(
                            "tool_call_missing_id",
                            format!(
                                "the assistant call to {:?} has no id; this session was written by an older harness. Start a new branch with /new.",
                                call.name
                            ),
                        ));
                    }
                    if let Some((name, first)) = calls.get(call.id.as_str()) {
                        return Err(ToolPairError::new(
                            "tool_call_duplicate",
                            format!(
                                "tool call id {:?} is used twice ({:?} and {:?}, entries {} and {}); results cannot be paired. Start a new branch with /new.",
                                call.id, name, call.name, first, entry.seq
                            ),
                        ));
                    }
                    calls.insert(call.id.as_str(), (call.name.as_str(), entry.seq));
                }
            }
            EntryKind::ToolResult(result) => {
                let Some((name, call_seq)) = calls.get(result.call_id.as_str()) else {
                    return Err(ToolPairError::new(
                        "tool_result_orphan",
                        format!(
                            "a tool result for {:?} has no matching call on this branch (entry {}); replay would attach it to the wrong step. Start a new branch with /new, or /compact to summarize the inconsistent history.",
                            result.call_id, entry.seq
                        ),
                    ));
                };
                if entry.seq <= *call_seq {
                    return Err(ToolPairError::new(
                        "tool_result_orphan",
                        format!(
                            "a tool result for {:?} precedes its call (entries {} and {}); replay would attach it to the wrong step. Start a new branch with /new.",
                            result.call_id, entry.seq, call_seq
                        ),
                    ));
                }
                if let Some(first) = results.get(result.call_id.as_str()) {
                    return Err(ToolPairError::new(
                        "tool_result_duplicate",
                        format!(
                            "tool call {:?} ({:?}) has more than one result (entries {} and {}); exactly one result per call is required. Start a new branch with /new.",
                            result.call_id, name, first, entry.seq
                        ),
                    ));
                }
                results.insert(result.call_id.as_str(), entry.seq);
            }
            _ => {}
        }
    }
    for (id, (name, seq)) in &calls {
        if !results.contains_key(id) {
            return Err(ToolPairError::new(
                "tool_result_missing",
                format!(
                    "tool call {:?} ({:?}, entry {}) has no result on this branch; the request would replay an unanswered call. Start a new branch with /new, or /compact to summarize the inconsistent history.",
                    id, name, seq
                ),
            ));
        }
    }
    Ok(())
}

/// Image bytes for a stored reference, or a note when they are gone (for
/// example after artifact retention removed them).
fn load_image(store: &ArtifactStore, image: &ImageRef, label: &str) -> Result<ImageData, String> {
    store
        .read_image(&image.id)
        .map(|data| ImageData {
            mime: image.mime.clone(),
            width: image.width,
            height: image.height,
            data,
            label: label.to_string(),
        })
        .map_err(|error| format!("[image {label} is no longer available: {error}]"))
}

/// The image a tool returned (`read` on an image for a vision model), as
/// recorded in the result's details.
pub fn tool_image(result: &ToolResultData) -> Option<(ImageRef, String)> {
    let details = result.details.as_ref()?;
    let image: ImageRef = serde_json::from_value(details.get("image")?.clone()).ok()?;
    let path = details
        .get("path")
        .and_then(|path| path.as_str())
        .unwrap_or(&image.id)
        .to_string();
    Some((image, path))
}

fn push_assistant(
    messages: &mut Vec<ChatMessage>,
    assistant: &AssistantData,
    seq: u64,
    results: &ToolResults<'_>,
    artifacts: &ArtifactStore,
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
    // Chat APIs carry images only in user messages, so images returned by
    // tools follow the batch's tool messages in one user message.
    let mut images = Vec::new();
    let mut notes = Vec::new();
    for call in &tool_calls {
        let result = results.after(&call.id, seq);
        let content = match result {
            Some(result) => result.content.clone(),
            None => format!(
                "[cancelled] no result was recorded for {}; the step was interrupted",
                call.name
            ),
        };
        if let Some((image, path)) = result.and_then(tool_image) {
            match load_image(artifacts, &image, &path) {
                Ok(data) => {
                    notes.push(format!("{path} ({})", image.summary()));
                    images.push(data);
                }
                Err(note) => notes.push(note),
            }
        }
        messages.push(ChatMessage::Tool {
            call_id: call.id.clone(),
            content,
        });
    }
    if !notes.is_empty() {
        messages.push(ChatMessage::User {
            content: format!(
                "[image content returned by the read tool: {}]",
                notes.join(", ")
            ),
            images,
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
                redact: true,
                context: None,
            },
            true,
        )
        .unwrap()
    }

    #[test]
    fn images_become_content_parts_after_text_and_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        let png = crate::media::image::fixtures::png_with_metadata();
        let image = |id: String| ImageRef {
            id,
            mime: "image/png".into(),
            width: 3,
            height: 2,
            bytes: 10,
        };
        let pasted = image(session.artifacts.write_image(&png, "png").unwrap());
        session
            .append(EntryKind::User(UserData {
                content: "why is this broken".into(),
                attachments: vec![Attachment {
                    path: "shot.png".into(),
                    content: String::new(),
                    image: Some(pasted),
                    described_by: None,
                }],
            }))
            .unwrap();
        let read = image(session.artifacts.write_image(&png, "png").unwrap());
        session
            .append(EntryKind::Assistant(AssistantData {
                tool_calls: Some(vec![crate::session::ToolCall {
                    id: "c1".into(),
                    name: "read".into(),
                    arguments: json!({"path": "ui.png"}),
                }]),
                ..AssistantData::default()
            }))
            .unwrap();
        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "c1".into(),
                content: "ui.png: PNG 3×2".into(),
                is_error: false,
                details: Some(json!({"path": "ui.png", "image": read})),
                artifact: None,
            }))
            .unwrap();
        let budget = TokenBudget::new(32_768, 1024);
        let assembled = assemble("system", &session, None, &budget);
        let roles: Vec<&str> = assembled.messages.iter().map(|m| m.role()).collect();
        assert_eq!(roles, vec!["system", "user", "assistant", "tool", "user"]);
        match &assembled.messages[1] {
            ChatMessage::User { content, images } => {
                assert!(content.contains("image attached by the user: shot.png (PNG 3×2)"));
                assert_eq!(images.len(), 1);
                assert_eq!(images[0].data.as_slice(), png.as_slice());
            }
            other => panic!("{other:?}"),
        }
        match &assembled.messages[4] {
            ChatMessage::User { content, images } => {
                assert!(content.contains("ui.png (PNG 3×2)"), "{content}");
                assert_eq!(images.len(), 1);
            }
            other => panic!("{other:?}"),
        }
        // Images are costed, not counted as base64.
        assert!(assembled.char_count > 2 * 64 * 3);
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

    #[test]
    fn pair_validation_requires_one_result_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::Assistant(AssistantData {
                tool_calls: Some(vec![crate::session::ToolCall {
                    id: "call_0001".into(),
                    name: "read".into(),
                    arguments: json!({"path":"a.rs"}),
                }]),
                ..AssistantData::default()
            }))
            .unwrap();
        let err = validate_tool_pairs(&session).expect_err("missing result");
        assert_eq!(err.code(), "tool_result_missing");
        assert!(err.message.contains("call_0001"));

        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "call_0001".into(),
                content: "file body".into(),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        assert!(validate_tool_pairs(&session).is_ok());

        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "call_0001".into(),
                content: "duplicate".into(),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        let err = validate_tool_pairs(&session).expect_err("duplicate result");
        assert_eq!(err.code(), "tool_result_duplicate");
    }

    #[test]
    fn pair_validation_rejects_an_orphan_result() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "call_0009".into(),
                content: "orphan".into(),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        let err = validate_tool_pairs(&session).expect_err("orphan result");
        assert_eq!(err.code(), "tool_result_orphan");
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
            ChatMessage::User { content, .. } if content == "new"
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
        assert!(
            validate_tool_pairs(&session).is_ok(),
            "a compacted turn must still pair its call and result"
        );
    }
}

//! Context assembly from a session branch, token estimate, state block.

pub mod archive;
pub mod budget;
pub mod compact;

use crate::provider::{ChatMessage, ImageData, ToolCall};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use crate::session::{
    ArtifactStore, AssistantData, Entry, EntryKind, ImageRef, Session, ToolResultData,
};

pub use archive::{
    BATCH_CAP_CHARS, CappedContent, Preview, RECALL_CAP_CHARS, RESULT_CAP_CHARS, ResultCaps,
    cap_batch, cap_result, cap_result_limited, cap_result_with,
};
pub use budget::{
    JSON_CHARS_PER_TOKEN, OutputReservation, StaticPrompt, THINKING_OUTPUT_CAP, TokenBudget,
    ToolSpecCost, output_reservation,
};
pub use compact::{CompactionSummary, FindingSummary, build_summary};

#[derive(Debug, Clone)]
pub struct AssembledContext {
    pub messages: Vec<ChatMessage>,
    pub estimated_tokens: u32,
    pub char_count: usize,
}

/// Byte-stable fingerprint of everything a request sends before the
/// conversation: the leading system message followed by the serialized
/// `tools` array. Two requests that share it can reuse the engine's
/// KV/prefix cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptPrefix {
    pub hash: String,
    /// Hash of the system block alone, so a cold cache can be blamed on the
    /// system message or on the tool list.
    pub system_hash: String,
    /// Characters in the system block plus the serialized tools.
    pub chars: usize,
}

/// Hash the request prefix. Tool order and schema key order are already
/// deterministic, so the same profile and model produce the same hash.
pub fn prompt_prefix(system: &str, tools_json: &str) -> PromptPrefix {
    let mut hasher = Sha256::new();
    hasher.update(b"totality-prompt-prefix-v1\0");
    hasher.update(system.as_bytes());
    hasher.update(b"\0tools\0");
    hasher.update(tools_json.as_bytes());
    let system_hash = format!("{:x}", Sha256::digest(system.as_bytes()));
    PromptPrefix {
        hash: format!("{:x}", hasher.finalize()),
        system_hash,
        chars: system.chars().count() + tools_json.chars().count(),
    }
}

/// Characters shared by two rendered prefixes. A compaction reports this as
/// the part the engine keeps and re-prefills only the remainder.
pub fn common_prefix_chars(a: &str, b: &str) -> usize {
    a.chars()
        .zip(b.chars())
        .take_while(|(left, right)| left == right)
        .count()
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
    // Hook context waiting for the user or tool message it attaches to, and
    // the first message index it may attach to.
    let mut pending_hooks: Vec<String> = Vec::new();
    let mut pending_from = 0usize;

    for entry in path {
        attach_pending_hooks(&mut messages, &mut pending_hooks, pending_from);
        match &entry.kind {
            EntryKind::Session(_) | EntryKind::Checkout(_) | EntryKind::Error(_) => {}
            EntryKind::Custom(custom) => {
                // Hook context is configured, local text. Context written
                // once (session start, compaction) joins the system turn.
                // Context that fires during the session is rendered in place,
                // on the user or tool message it belongs to: folding it into
                // the leading system message would rewrite the head of every
                // later request and defeat the engine's prefix cache.
                if custom.kind == "hook-context"
                    && let Some(text) = custom.payload.get("text").and_then(|value| value.as_str())
                    && !text.trim().is_empty()
                {
                    let hook = custom
                        .payload
                        .get("hook")
                        .and_then(|value| value.as_str())
                        .unwrap_or("hook");
                    let event = custom
                        .payload
                        .get("event")
                        .and_then(|value| value.as_str())
                        .unwrap_or("session_start");
                    let block = format!("[hook context: {hook}]\n{text}");
                    match event {
                        "session_start" | "pre_compact" => {
                            messages.push(ChatMessage::System { content: block });
                        }
                        // Already part of the tool result the hook ran on.
                        "pre_tool" | "post_tool" => {}
                        _ => {
                            if pending_hooks.is_empty() {
                                pending_from = messages.len();
                            }
                            pending_hooks.push(block);
                            // Context recorded after the message it is about
                            // (the last user or tool message) attaches there.
                            if matches!(
                                messages.last(),
                                Some(ChatMessage::User { .. } | ChatMessage::Tool { .. })
                            ) {
                                pending_from = messages.len() - 1;
                            }
                        }
                    }
                }
            }
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

    attach_pending_hooks(&mut messages, &mut pending_hooks, pending_from);
    if !pending_hooks.is_empty() {
        messages.push(ChatMessage::user(pending_hooks.join("\n\n")));
    }

    // Folding keeps the logical order (base, summaries in path order, state)
    // while guaranteeing at most one system message, at the front.
    let mut messages = fold_system_messages(messages);
    if had_compaction || messages.len() > 1 {
        ensure_leading_user(&mut messages);
    }

    let char_count = messages.iter().map(|m| m.char_len()).sum();
    let estimated_tokens = budget.estimate_messages(&messages);
    let mut assembled = AssembledContext {
        messages,
        estimated_tokens,
        char_count,
    };
    if had_compaction && let Some(state) = state_block {
        attach_state(&mut assembled, state, budget);
    }
    assembled
}

/// Append pending hook context to the first user or tool message at or after
/// `from`, once one exists.
fn attach_pending_hooks(messages: &mut [ChatMessage], pending: &mut Vec<String>, from: usize) {
    if pending.is_empty() {
        return;
    }
    let target = messages
        .iter_mut()
        .skip(from)
        .find(|message| matches!(message, ChatMessage::User { .. } | ChatMessage::Tool { .. }));
    if let Some(ChatMessage::User { content, .. } | ChatMessage::Tool { content, .. }) = target {
        for block in pending.drain(..) {
            content.push_str("\n\n");
            content.push_str(&block);
        }
    }
}

/// Append the post-compaction state block to the **last** message of the
/// request and update the counts by the difference, so a fixed term a caller
/// already added (tool schemas) is kept. Lets a caller size the state block
/// from a finished assembly instead of assembling (and loading images) twice.
///
/// The block changes as the session goes on (changed files, verify status,
/// the budget band). At the tail a change costs only the bytes after it; in
/// the leading system message it made the engine re-evaluate the whole
/// conversation on every call. Nothing is stored: the next assembly attaches
/// it to the new last message instead.
pub fn attach_state(assembled: &mut AssembledContext, state: &str, budget: &TokenBudget) {
    if state.trim().is_empty() {
        return;
    }
    let Some(last) = assembled.messages.last_mut() else {
        return;
    };
    let before = last.char_len();
    match last {
        ChatMessage::User { content, .. } | ChatMessage::Tool { content, .. } => {
            content.push_str("\n\n");
            content.push_str(state);
        }
        // A request ends with a user or tool message; if it does not, keep
        // the old placement rather than invent a turn.
        ChatMessage::System { content } => {
            content.push_str("\n\n");
            content.push_str(state);
        }
        ChatMessage::Assistant { .. } => return,
    }
    let after = last.char_len();
    assembled.char_count = assembled.char_count + after - before;
    assembled.estimated_tokens = assembled.estimated_tokens + budget.estimate_char_len(after)
        - budget.estimate_char_len(before);
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

/// User turn placed after the system message when the conversation would
/// otherwise open with an assistant or tool message.
pub const COMPACTED_CONTINUE: &str = "[totality] Earlier context was compacted into the system message above. Continue the task from there and from the messages that follow.";

/// Open the conversation with a user turn. A compaction in the middle of a
/// long agentic turn keeps a tail that starts at an assistant entry (or no
/// tail at all), and the user's instruction then survives only inside the
/// summary. Many chat templates reject that: Qwen's raises "No user query
/// found in messages." when no user message exists, and alternating
/// templates require the first non-system turn to be a user one. The
/// inserted turn sits before the first assistant message, so it can never
/// split a tool call from its results.
fn ensure_leading_user(messages: &mut Vec<ChatMessage>) {
    let first = usize::from(matches!(messages.first(), Some(ChatMessage::System { .. })));
    if !matches!(messages.get(first), Some(ChatMessage::User { .. })) {
        messages.insert(
            first,
            ChatMessage::User {
                content: COMPACTED_CONTINUE.to_string(),
                images: Vec::new(),
            },
        );
    }
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
    // Reasoning is never replayed, even from entries written before the
    // stream split it out of content.
    let content = assistant
        .content
        .as_deref()
        .map(crate::agent::output::strip_reasoning_markup);
    messages.push(ChatMessage::Assistant {
        content,
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
    let mut out = String::from("[totality] state\n");
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
            if custom.kind == "verify-result"
                && crate::session::Session::counts_as_project_verify(&custom.payload)
            {
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
    out.push_str(context_band(used_percent));
    out
}

/// The budget line of the state block, in coarse bands.
///
/// The block is part of the leading system message, so every change to it
/// makes the engine re-evaluate the whole conversation that follows. An exact
/// percentage moves on almost every call (one percent of a 32k window is less
/// than one tool result); a band moves at most twice between compactions.
fn context_band(used_percent: f32) -> &'static str {
    if used_percent >= 65.0 {
        "context: over 65% of usable prompt"
    } else if used_percent >= 50.0 {
        "context: over 50% of usable prompt"
    } else {
        "context: under 50% of usable prompt"
    }
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
                permissions: None,
            },
            true,
        )
        .unwrap()
    }

    #[test]
    fn state_block_is_stable_while_the_context_grows_within_a_band() {
        let dir = tempfile::tempdir().unwrap();
        let session = session(dir.path());
        // Consecutive calls a few hundred tokens apart must not rewrite the
        // system message the block is appended to.
        let first = state_block(&session, None, 31.2);
        assert_eq!(first, state_block(&session, None, 33.9));
        assert_eq!(first, state_block(&session, None, 49.9));
        assert!(first.ends_with("context: under 50% of usable prompt"));
        let middle = state_block(&session, None, 50.0);
        assert_ne!(first, middle);
        assert_eq!(middle, state_block(&session, None, 64.9));
        assert!(state_block(&session, None, 79.0).ends_with("context: over 65% of usable prompt"));
    }

    #[test]
    fn prompt_prefix_is_deterministic_and_tracks_both_parts() {
        let prefix = crate::context::prompt_prefix("system", r#"[{"name":"read"}]"#);
        assert_eq!(
            prefix,
            crate::context::prompt_prefix("system", r#"[{"name":"read"}]"#)
        );
        assert_eq!(prefix.chars, "system".chars().count() + 17);
        assert_ne!(
            prefix.hash,
            crate::context::prompt_prefix("system ver2", r#"[{"name":"read"}]"#).hash
        );
        assert_ne!(
            prefix.hash,
            crate::context::prompt_prefix("system", r#"[{"name":"write"}]"#).hash
        );
        assert_eq!(crate::context::common_prefix_chars("abcdef", "abcxyz"), 3);
        assert_eq!(crate::context::common_prefix_chars("", "abc"), 0);
        assert_eq!(crate::context::common_prefix_chars("same", "same"), 4);
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
                summary: "[totality] summary".into(),
                tokens_before: 100,
                reason: "auto".into(),
                thread_id: None,
                keep_from: None,
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
        // The state block rides on the last message, not the system head.
        assert_leading_system(&assembled.messages, "system\n\n[totality] summary");
        assert_eq!(assembled.messages.len(), 2); // system, user
        assert!(matches!(
            &assembled.messages[1],
            ChatMessage::User { content, .. } if content == "new\n\nSTATE"
        ));
    }

    #[test]
    fn attaching_state_later_matches_assembling_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::Compaction(CompactionData {
                summary: "[totality] summary".into(),
                tokens_before: 100,
                reason: "auto".into(),
                thread_id: None,
                keep_from: None,
            }))
            .unwrap();
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "new ".repeat(50),
            }))
            .unwrap();
        let budget = TokenBudget::new(32768, 4096);
        let at_once = assemble("system", &session, Some("STATE block"), &budget);
        let mut later = assemble("system", &session, None, &budget);
        attach_state(&mut later, "STATE block", &budget);
        assert_eq!(
            format!("{:?}", later.messages),
            format!("{:?}", at_once.messages)
        );
        assert_eq!(later.char_count, at_once.char_count);
        assert_eq!(later.estimated_tokens, at_once.estimated_tokens);
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
                summary: "[totality] summary".into(),
                tokens_before: 100,
                reason: "auto".into(),
                thread_id: None,
                keep_from: None,
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
        assert_leading_system(&assembled.messages, "system\n\n[totality] summary");
        assert!(matches!(
            assembled.messages.last(),
            Some(ChatMessage::User { content, .. }) if content == "next\n\nSTATE"
        ));
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

    /// A long single agentic turn: one user instruction, then call/result
    /// pairs only. Returns the assistant entry ids.
    fn agentic_turn(session: &mut Session, steps: usize) -> Vec<String> {
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "task".into(),
            }))
            .unwrap();
        (0..steps)
            .map(|index| {
                let call_id = format!("call_{index}");
                let id = session
                    .append(EntryKind::Assistant(AssistantData {
                        content: None,
                        tool_calls: Some(vec![crate::session::ToolCall {
                            id: call_id.clone(),
                            name: "read".into(),
                            arguments: json!({"path": format!("{index}.rs")}),
                        }]),
                        ..AssistantData::default()
                    }))
                    .unwrap()
                    .id
                    .clone();
                session
                    .append(EntryKind::ToolResult(ToolResultData {
                        call_id,
                        content: "body".into(),
                        is_error: false,
                        details: None,
                        artifact: None,
                    }))
                    .unwrap();
                id
            })
            .collect()
    }

    /// One leading system message, a user turn before any assistant turn,
    /// and every tool message directly after its call (only tool messages
    /// in between).
    fn assert_provider_valid(messages: &[ChatMessage]) {
        assert!(matches!(messages.first(), Some(ChatMessage::System { .. })));
        assert!(
            messages[1..]
                .iter()
                .all(|message| !matches!(message, ChatMessage::System { .. }))
        );
        assert!(
            matches!(messages.get(1), Some(ChatMessage::User { .. })),
            "the first non-system message must be a user turn: {messages:?}"
        );
        let mut open_calls: Vec<String> = Vec::new();
        for message in messages {
            match message {
                ChatMessage::Assistant { tool_calls, .. } => {
                    assert!(open_calls.is_empty(), "unanswered calls: {open_calls:?}");
                    open_calls = tool_calls.iter().map(|call| call.id.clone()).collect();
                }
                ChatMessage::Tool { call_id, .. } => {
                    let position = open_calls
                        .iter()
                        .position(|id| id == call_id)
                        .unwrap_or_else(|| panic!("tool result {call_id} without its call"));
                    open_calls.remove(position);
                }
                _ => assert!(
                    open_calls.is_empty(),
                    "a message split calls {open_calls:?} from their results"
                ),
            }
        }
    }

    #[test]
    fn a_mid_turn_compaction_still_sends_a_user_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        let assistants = agentic_turn(&mut session, 6);
        session
            .append(EntryKind::Compaction(CompactionData {
                summary: "[totality] summary".into(),
                tokens_before: 100,
                reason: "auto".into(),
                thread_id: None,
                keep_from: Some(assistants[3].clone()),
            }))
            .unwrap();
        let budget = TokenBudget::new(32768, 4096);
        let assembled = assemble("system", &session, Some("STATE"), &budget);
        assert_provider_valid(&assembled.messages);
        let roles: Vec<&str> = assembled.messages.iter().map(|m| m.role()).collect();
        assert_eq!(
            roles,
            vec![
                "system",
                "user",
                "assistant",
                "tool",
                "assistant",
                "tool",
                "assistant",
                "tool"
            ]
        );
        // The inserted turn is counted.
        assert_eq!(
            assembled.char_count,
            assembled
                .messages
                .iter()
                .map(|m| m.char_len())
                .sum::<usize>()
        );
    }

    #[test]
    fn a_tailless_compaction_without_a_kickoff_still_sends_a_user_turn() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        agentic_turn(&mut session, 3);
        session
            .append(EntryKind::Compaction(CompactionData {
                summary: "[totality] summary".into(),
                tokens_before: 100,
                reason: "thread_split".into(),
                thread_id: None,
                keep_from: None,
            }))
            .unwrap();
        let budget = TokenBudget::new(32768, 4096);
        let assembled = assemble("system", &session, Some("STATE"), &budget);
        assert_provider_valid(&assembled.messages);
        assert_eq!(assembled.messages.len(), 2);
        assert!(matches!(
            &assembled.messages[1],
            ChatMessage::User { content, .. } if content == &format!("{COMPACTED_CONTINUE}\n\nSTATE")
        ));
    }

    #[test]
    fn an_empty_session_assembles_only_the_system_message() {
        let dir = tempfile::tempdir().unwrap();
        let session = session(dir.path());
        let budget = TokenBudget::new(32768, 4096);
        let assembled = assemble("system", &session, None, &budget);
        assert_eq!(assembled.messages.len(), 1);
    }
}

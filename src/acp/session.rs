//! ACP sessions: one harness runtime per declared root, turn streaming, and
//! the client permission callback.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, Core, TurnOutcome, TurnStatus, run_turn};
use crate::app::{RuntimeOptions, create_runtime};
use crate::paths::Paths;
use crate::sanitize;
use crate::session::{EntryKind, ToolCall};
use crate::tools::{ToolApproval, ToolApprover};

use super::{RpcError, Server, check_additional_directories, declared_root, valid_session_id};

pub(crate) struct AcpSession {
    pub id: String,
    pub root: PathBuf,
    pub core: Arc<Core>,
    active: Mutex<Option<CancellationToken>>,
}

impl AcpSession {
    pub(crate) fn cancel(&self) {
        if let Some(token) = self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
        {
            token.cancel();
        }
    }

    fn token(&self) -> CancellationToken {
        self.active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .unwrap_or_default()
    }

    fn begin(&self) -> Result<CancellationToken, RpcError> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if active.is_some() {
            return Err(RpcError {
                code: -32602,
                message: "a prompt turn is already running in this session; cancel it or wait"
                    .into(),
                data: json!({"code": "session_busy", "sessionId": self.id}),
            });
        }
        let token = CancellationToken::new();
        *active = Some(token.clone());
        Ok(token)
    }

    fn finish(&self) {
        *self
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

impl Server {
    /// Project trust and verify steps belong to the process working
    /// directory. A session that declares another root gets neither, so a
    /// client cannot extend the project's trust to an arbitrary directory.
    fn session_config(&self, root: &std::path::Path) -> crate::config::Config {
        let mut config = self.config.clone();
        if root != self.process_paths.cwd {
            config.trust = false;
            config.verify = crate::config::VerifyConfig::default();
        }
        config
    }

    /// Paths for a session root, keeping the process's config/data roots and
    /// `[session] dir` override so ACP and the CLI see the same sessions.
    fn session_paths(&self, root: &std::path::Path) -> Result<Paths, RpcError> {
        let mut paths = Paths::discover(root)
            .map_err(|error| RpcError::internal(format!("cannot prepare {root:?}: {error}")))?;
        paths.config_dir = self.process_paths.config_dir.clone();
        paths.data_dir = self.process_paths.data_dir.clone();
        paths.sessions_dir_override = self.process_paths.sessions_dir_override.clone();
        Ok(paths)
    }

    pub(crate) async fn new_session(self: &Arc<Self>, params: Value) -> Result<Value, RpcError> {
        check_additional_directories(&params)?;
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("session/new requires an absolute cwd"))?;
        let root = declared_root(cwd)?;
        let root_paths = self.session_paths(&root)?;
        let ignored_mcp = params
            .get("mcpServers")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);

        let slot = Arc::new(Mutex::new(None));
        let approver = Arc::new(AcpApprover {
            server: Arc::downgrade(self),
            session_id: slot.clone(),
        });
        let options = RuntimeOptions {
            path_jail: true,
            engagement: self.options.engagement.clone(),
            no_session: self.options.no_session,
            no_redact: self.options.no_redact,
            resume: None,
            continue_last: false,
            approver: Some(approver),
        };
        let session_config = self.session_config(&root);
        let trusted = session_config.trust;
        let runtime = create_runtime(session_config, root_paths, options)
            .await
            .map_err(|error| RpcError::internal(format!("cannot create a session: {error:#}")))?;
        let harness_id = lock_session(&runtime.core).id.clone();
        // Ephemeral sessions all call themselves "ephemeral" in the harness;
        // give each ACP session a distinct id so the map cannot collide.
        let id = if self.options.no_session {
            format!("acp-{}", self.next_request.fetch_add(1, Ordering::SeqCst))
        } else {
            harness_id
        };
        *slot.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(id.clone());
        let session = Arc::new(AcpSession {
            id: id.clone(),
            root,
            core: runtime.core,
            active: Mutex::new(None),
        });
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(id.clone(), session);

        // Do not pretend client MCP servers were connected; the harness
        // config owns MCP. `trusted` lets a client see which root policy ran.
        let mut meta = json!({"trusted": trusted});
        if ignored_mcp > 0 {
            meta["ignoredMcpServers"] = json!(ignored_mcp);
            meta["note"] = json!(
                "client-supplied MCP servers are not connected in this version; configure them in the harness config"
            );
        }
        Ok(json!({"sessionId": id, "_meta": {"localharness": meta}}))
    }

    pub(crate) async fn load_session(self: &Arc<Self>, params: Value) -> Result<Value, RpcError> {
        if self.options.no_session {
            return Err(RpcError::unsupported(
                "loadSession",
                "session/load is unavailable with --no-session because nothing was persisted",
            ));
        }
        check_additional_directories(&params)?;
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("session/load requires an absolute cwd"))?;
        let root = declared_root(cwd)?;
        let id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("session/load requires a sessionId"))?;
        if !valid_session_id(id) {
            return Err(RpcError::invalid_params(format!(
                "session id {id:?} is not a plain identifier"
            )));
        }
        if self.session(id).is_some() {
            return Err(RpcError {
                code: -32602,
                message: format!("session {id:?} is already loaded in this process"),
                data: json!({"code": "session_already_loaded", "sessionId": id}),
            });
        }
        let root_paths = self.session_paths(&root)?;
        let file = root_paths.session_file(id);
        if !file.is_file() {
            return Err(RpcError::unknown_session(id));
        }

        let slot = Arc::new(Mutex::new(None));
        let approver = Arc::new(AcpApprover {
            server: Arc::downgrade(self),
            session_id: slot.clone(),
        });
        let options = RuntimeOptions {
            path_jail: true,
            engagement: self.options.engagement.clone(),
            no_session: false,
            no_redact: self.options.no_redact,
            resume: Some(file),
            continue_last: false,
            approver: Some(approver),
        };
        let session_config = self.session_config(&root);
        let trusted = session_config.trust;
        let runtime = create_runtime(session_config, root_paths, options)
            .await
            .map_err(|error| {
                RpcError::internal(format!("cannot load session {id:?}: {error:#}"))
            })?;
        let loaded_id = lock_session(&runtime.core).id.clone();
        *slot.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(loaded_id.clone());
        let session = Arc::new(AcpSession {
            id: loaded_id.clone(),
            root,
            core: runtime.core,
            active: Mutex::new(None),
        });
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(loaded_id.clone(), session.clone());

        self.replay(&session);
        Ok(json!({
            "_meta": {"localharness": {"sessionId": loaded_id, "trusted": trusted}}
        }))
    }

    /// Replay the active branch so the editor sees the history it is loading:
    /// user and assistant text plus tool calls and their results.
    fn replay(&self, session: &AcpSession) {
        let entries: Vec<EntryKind> = {
            let guard = lock_session(&session.core);
            guard
                .branch_entries()
                .into_iter()
                .map(|entry| entry.kind.clone())
                .collect()
        };
        let mut calls: std::collections::HashMap<String, (String, Value)> =
            std::collections::HashMap::new();
        for kind in entries {
            match kind {
                EntryKind::User(user) => {
                    self.notify(
                        "session/update",
                        json!({
                            "sessionId": session.id,
                            "update": {
                                "sessionUpdate": "user_message_chunk",
                                "content": {"type": "text", "text": sanitize::display(&user.content)},
                            }
                        }),
                    );
                }
                EntryKind::Assistant(assistant) => {
                    if let Some(text) = assistant
                        .content
                        .as_deref()
                        .filter(|text| !text.trim().is_empty())
                    {
                        self.notify(
                            "session/update",
                            json!({
                                "sessionId": session.id,
                                "update": {
                                    "sessionUpdate": "agent_message_chunk",
                                    "content": {"type": "text", "text": sanitize::display(text)},
                                }
                            }),
                        );
                    }
                    for call in assistant.tool_calls.unwrap_or_default() {
                        self.notify(
                            "session/update",
                            json!({
                                "sessionId": session.id,
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": call.id,
                                    "title": sanitize::display(&permission_title(&call.name, &call.arguments, None)).to_string(),
                                    "name": call.name,
                                    "kind": tool_kind(&call.name),
                                    "status": "in_progress",
                                    "rawInput": sanitize_value(&call.arguments),
                                }
                            }),
                        );
                        calls.insert(call.id, (call.name, call.arguments));
                    }
                }
                EntryKind::ToolResult(result) => {
                    if let Some((name, _)) = calls.get(&result.call_id) {
                        let preview: String = sanitize::display(&result.content)
                            .to_string()
                            .chars()
                            .take(2_000)
                            .collect();
                        self.notify(
                            "session/update",
                            json!({
                                "sessionId": session.id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": result.call_id,
                                    "name": name,
                                    "status": if result.is_error { "failed" } else { "completed" },
                                    "content": [{
                                        "type": "content",
                                        "content": {"type": "text", "text": preview},
                                    }],
                                }
                            }),
                        );
                    }
                }
                _ => {}
            }
        }
    }

    /// Start a prompt turn. The response is sent when the turn settles; the
    /// reader stays free to deliver `session/cancel` and permission replies.
    pub(crate) fn start_prompt(
        self: &Arc<Self>,
        request_id: Value,
        params: Value,
    ) -> Result<(), RpcError> {
        let id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("session/prompt requires a sessionId"))?;
        let session = self
            .session(id)
            .ok_or_else(|| RpcError::unknown_session(id))?;
        let text = prompt_text(&params)?;
        let cancel = session.begin()?;
        let server = self.clone();
        let session_id = session.id.clone();
        let prompt_key = super::id_key(&request_id);
        self.prompts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(prompt_key.clone(), session_id.clone());
        tokio::spawn(async move {
            server
                .drive_turn(session, session_id, request_id, prompt_key, text, cancel)
                .await;
        });
        Ok(())
    }

    async fn drive_turn(
        self: Arc<Self>,
        session: Arc<AcpSession>,
        session_id: String,
        request_id: Value,
        prompt_key: String,
        text: String,
        cancel: CancellationToken,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let core = session.core.clone();
        let cancel_turn = cancel.clone();
        let handle = tokio::spawn(async move { run_turn(core, text, tx, cancel_turn).await });

        let mut outcome: Option<TurnOutcome> = None;
        let mut text_stream = StreamText::default();
        let mut reasoning_stream = StreamText::default();
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TextDelta(delta) => {
                    let text = text_stream.push(&delta);
                    if !text.is_empty() {
                        self.send_text(&session_id, "agent_message_chunk", &text);
                    }
                }
                AgentEvent::ReasoningDelta(delta) => {
                    let text = reasoning_stream.push(&delta);
                    if !text.is_empty() {
                        self.send_text(&session_id, "agent_thought_chunk", &text);
                    }
                }
                AgentEvent::TurnDone(result) => outcome = Some(result),
                other => self.forward_event(&session_id, other),
            }
        }
        let tail = text_stream.finish();
        if !tail.is_empty() {
            self.send_text(&session_id, "agent_message_chunk", &tail);
        }
        let reasoning_tail = reasoning_stream.finish();
        if !reasoning_tail.is_empty() {
            self.send_text(&session_id, "agent_thought_chunk", &reasoning_tail);
        }
        let outcome = match (outcome, handle.await) {
            (Some(outcome), _) => outcome,
            (None, Ok(outcome)) => outcome,
            (None, Err(error)) => TurnOutcome {
                status: TurnStatus::Error,
                error: Some(("turn_panicked".into(), error.to_string())),
            },
        };
        session.finish();
        self.prompts
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&prompt_key);

        // A provider, session, or panic failure is a protocol error, not a
        // refusal: the client should be able to retry the same conversation.
        if outcome.status == TurnStatus::Error {
            let (code, message) = outcome
                .error
                .clone()
                .unwrap_or_else(|| ("turn_failed".into(), "the turn failed".into()));
            self.error(
                request_id,
                RpcError {
                    code: -32000,
                    message: sanitize::display(&message).to_string(),
                    data: json!({"code": code, "status": "error"}),
                },
            );
            return;
        }

        let stop_reason = stop_reason(outcome.status);
        let mut meta = json!({
            "status": format!("{:?}", outcome.status),
        });
        if let Some((code, message)) = &outcome.error {
            meta["error"] = json!({"code": code, "message": sanitize::display(message)});
        }
        self.response(
            request_id,
            json!({
                "stopReason": stop_reason,
                "_meta": {"localharness": meta},
            }),
        );
    }

    fn send_text(&self, session_id: &str, kind: &str, text: &str) {
        self.notify(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": kind,
                    "content": {"type": "text", "text": text},
                }
            }),
        );
    }

    fn forward_event(&self, session_id: &str, event: AgentEvent) {
        let update = match event {
            AgentEvent::ToolStarted {
                call_id,
                name,
                summary,
            } => json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id,
                "title": sanitize::display(&permission_title(
                    &name,
                    &Value::Null,
                    Some(&summary)
                ))
                .to_string(),
                "name": name,
                "kind": tool_kind(&name),
                "status": "in_progress",
                "rawInput": {"summary": sanitize::display(&summary)},
            }),
            AgentEvent::ToolFinished {
                call_id,
                name,
                is_error,
                preview,
                ..
            } => json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": call_id,
                "name": name,
                "status": if is_error { "failed" } else { "completed" },
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": sanitize::display(&preview)},
                }],
            }),
            AgentEvent::Error {
                code,
                message,
                recoverable,
            } => {
                let text = format!("[{code}] {}", sanitize::display(&message));
                if !recoverable {
                    tracing::warn!(code, "ACP turn error");
                }
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                })
            }
            AgentEvent::Notice(message) => json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": format!("[notice] {}", sanitize::display(&message))},
            }),
            _ => return,
        };
        self.notify(
            "session/update",
            json!({"sessionId": session_id, "update": update}),
        );
    }
}

fn lock_session(core: &Core) -> std::sync::MutexGuard<'_, crate::session::Session> {
    core.session
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Sanitize streamed text without letting an escape sequence split across
/// two model deltas reach the client as visible garbage.
#[derive(Default)]
struct StreamText {
    pending: String,
}

impl StreamText {
    fn push(&mut self, delta: &str) -> String {
        self.pending.push_str(delta);
        let split = complete_prefix(&self.pending);
        let ready = self.pending[..split].to_string();
        self.pending = self.pending[split..].to_string();
        sanitize::display(&ready).to_string()
    }

    /// Flush at the end of a turn. An incomplete escape sequence is dropped
    /// rather than shown.
    fn finish(&mut self) -> String {
        let pending = std::mem::take(&mut self.pending);
        if pending.is_empty() || complete_prefix(&pending) != pending.len() {
            return String::new();
        }
        sanitize::display(&pending).to_string()
    }
}

fn complete_prefix(text: &str) -> usize {
    let Some(position) = text.rfind('\u{1b}') else {
        return text.len();
    };
    if escape_complete(&text[position..]) {
        text.len()
    } else {
        position
    }
}

fn escape_complete(tail: &str) -> bool {
    let mut chars = tail.chars();
    if chars.next() != Some('\u{1b}') {
        return true;
    }
    match chars.next() {
        None => false,
        // CSI: final byte is 0x40..=0x7e.
        Some('[') => chars.any(|c| ('\u{40}'..='\u{7e}').contains(&c)),
        // OSC: BEL or ST terminates.
        Some(']') => {
            let rest = &tail[2..];
            rest.contains('\u{7}') || rest.contains("\u{1b}\\")
        }
        // A two-byte escape is complete once the second byte arrived.
        Some(_) => true,
    }
}

fn stop_reason(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Done => "end_turn",
        TurnStatus::Cancelled => "cancelled",
        TurnStatus::Budget | TurnStatus::ContextTruncated => "max_tokens",
        TurnStatus::Stalled => "max_turn_requests",
        TurnStatus::Error | TurnStatus::Blocked => "refusal",
    }
}

fn prompt_text(params: &Value) -> Result<String, RpcError> {
    let blocks = params
        .get("prompt")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("session/prompt requires a prompt array"))?;
    if blocks.is_empty() {
        return Err(RpcError::invalid_params("the prompt array is empty"));
    }
    let mut text = String::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(part) = block.get("text").and_then(Value::as_str) {
                    push(&mut text, part);
                }
            }
            Some("resource_link") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("resource");
                let uri = block.get("uri").and_then(Value::as_str).unwrap_or("");
                push(&mut text, &format!("[resource: {name} <{uri}>]"));
            }
            Some("resource") => {
                let Some(resource) = block.get("resource") else {
                    continue;
                };
                if let Some(part) = resource.get("text").and_then(Value::as_str) {
                    push(&mut text, part);
                } else {
                    return Err(RpcError::unsupported(
                        "embeddedContext.binary",
                        "binary resource contents are not supported; reference a file inside the session root",
                    ));
                }
            }
            Some("image") => {
                return Err(RpcError::unsupported(
                    "promptCapabilities.image",
                    "image prompts are not advertised by this ACP server",
                ));
            }
            Some("audio") => {
                return Err(RpcError::unsupported(
                    "promptCapabilities.audio",
                    "audio prompts are not advertised by this ACP server",
                ));
            }
            Some(other) => {
                return Err(RpcError::invalid_params(format!(
                    "unknown prompt block type {other:?}"
                )));
            }
            None => {
                return Err(RpcError::invalid_params("a prompt block has no type field"));
            }
        }
    }
    if text.trim().is_empty() {
        return Err(RpcError::invalid_params("the prompt contains no text"));
    }
    Ok(text)
}

fn push(text: &mut String, part: &str) {
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(part);
}

/// Reverse permission request: every non-read-only tool call asks the client
/// once. A denial is final; an allow does not widen any harness gate.
pub(crate) struct AcpApprover {
    pub server: Weak<Server>,
    pub session_id: Arc<Mutex<Option<String>>>,
}

#[async_trait]
impl ToolApprover for AcpApprover {
    async fn approve(&self, call: &ToolCall, args: &Value, read_only: bool) -> ToolApproval {
        // Reading and session-only bookkeeping need no prompt. Everything
        // that can touch files or run processes does.
        if read_only || matches!(call.name.as_str(), "todo" | "thread") {
            return ToolApproval::Allow;
        }
        let Some(server) = self.server.upgrade() else {
            return ToolApproval::Deny("the ACP server is shutting down".into());
        };
        let session_id = self
            .session_id
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let Some(session_id) = session_id else {
            return ToolApproval::Deny("the ACP session is not ready".into());
        };
        let Some(session) = server.session(&session_id) else {
            return ToolApproval::Deny("the ACP session is gone".into());
        };
        let cancel = session.token();
        if cancel.is_cancelled() {
            return ToolApproval::Deny(
                "the turn was cancelled before permission was requested".into(),
            );
        }
        let request_id = server.next_request_id();
        let (tx, rx) = oneshot::channel();
        server
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(request_id.clone(), tx);
        let mut tool_call = json!({
            "toolCallId": call.id,
            "title": sanitize::display(&permission_title(&call.name, args, None)).to_string(),
            "name": call.name,
            "kind": tool_kind(&call.name),
            "status": "pending",
            "rawInput": sanitize_value(args),
        });
        if let Some(raw) = args.get("path").and_then(Value::as_str) {
            let path = if std::path::Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                session.root.join(raw)
            };
            tool_call["locations"] = json!([{
                "path": sanitize::display(&path.display().to_string()).to_string(),
            }]);
        }
        server.send_request(
            request_id.clone(),
            "session/request_permission",
            json!({
                "sessionId": session_id,
                "toolCall": tool_call,
                "options": [
                    {"optionId": "allow", "name": "Allow once", "kind": "allow_once"},
                    {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
                ],
            }),
        );

        let value = tokio::select! {
            _ = cancel.cancelled() => {
                server.drop_pending(&request_id);
                return ToolApproval::Deny("the turn was cancelled before the client answered".into());
            }
            value = rx => value.ok(),
        };
        let Some(value) = value else {
            return ToolApproval::Deny(
                "the client disconnected before answering the permission request".into(),
            );
        };
        match value.pointer("/outcome/outcome").and_then(Value::as_str) {
            Some("selected") => match value.pointer("/outcome/optionId").and_then(Value::as_str) {
                Some("allow") => ToolApproval::Allow,
                Some("reject") | None | Some(_) => ToolApproval::Deny(format!(
                    "the client denied permission for {}; do not retry it unchanged",
                    call.name
                )),
            },
            Some("cancelled") => {
                ToolApproval::Deny("the client cancelled the permission request".into())
            }
            _ => ToolApproval::Deny("the client returned an invalid permission outcome".into()),
        }
    }
}

/// Strip ANSI/OSC controls from every string in a model-provided value before
/// it reaches the client. The structure is unchanged.
fn sanitize_value(value: &Value) -> Value {
    match value {
        Value::String(text) => Value::String(sanitize::display(text).to_string()),
        Value::Array(items) => Value::Array(items.iter().map(sanitize_value).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), sanitize_value(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn permission_title(name: &str, args: &Value, summary: Option<&str>) -> String {
    let detail = summary.map(str::to_string).or_else(|| match name {
        "bash" | "bg" => args
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => args
            .get("path")
            .or_else(|| args.get("pattern"))
            .or_else(|| args.get("query"))
            .or_else(|| args.get("name"))
            .and_then(Value::as_str)
            .map(str::to_string),
    });
    match detail {
        Some(detail) if !detail.trim().is_empty() => {
            let detail: String = detail.trim().chars().take(120).collect();
            format!("{name}: {detail}")
        }
        _ => name.to_string(),
    }
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read" | "review" => "read",
        "grep" | "glob" | "repo_map" | "recall" => "search",
        "write" | "edit" | "patch" => "edit",
        "undo" => "delete",
        "bash" | "bg" | "verify" | "test" => "execute",
        "todo" | "thread" | "skill" => "think",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streamed_escapes_are_held_until_complete() {
        let mut stream = StreamText::default();
        assert_eq!(stream.push("hello \u{1b}"), "hello ");
        assert_eq!(stream.push("[31m"), "");
        assert_eq!(stream.push("red"), "red");
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn an_unfinished_escape_at_the_end_is_dropped() {
        let mut stream = StreamText::default();
        assert_eq!(stream.push("before \u{1b}]0;title"), "before ");
        assert_eq!(stream.finish(), "", "the payload must not leak as text");
    }

    #[test]
    fn complete_osc_and_csi_sequences_are_removed() {
        let mut stream = StreamText::default();
        let out = stream.push("a\u{1b}]0;title\u{7}b\u{1b}[2Jc");
        assert_eq!(out, "abc");
        assert_eq!(stream.finish(), "");
    }

    #[test]
    fn stop_reasons_cover_every_turn_status() {
        assert_eq!(stop_reason(TurnStatus::Done), "end_turn");
        assert_eq!(stop_reason(TurnStatus::Cancelled), "cancelled");
        assert_eq!(stop_reason(TurnStatus::Budget), "max_tokens");
        assert_eq!(stop_reason(TurnStatus::ContextTruncated), "max_tokens");
        assert_eq!(stop_reason(TurnStatus::Stalled), "max_turn_requests");
        assert_eq!(stop_reason(TurnStatus::Blocked), "refusal");
        assert_eq!(stop_reason(TurnStatus::Error), "refusal");
    }
}

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
        if root != self.process_root {
            config.trust = false;
            config.verify = crate::config::VerifyConfig::default();
        }
        config
    }

    pub(crate) async fn new_session(self: &Arc<Self>, params: Value) -> Result<Value, RpcError> {
        check_additional_directories(&params)?;
        let cwd = params
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcError::invalid_params("session/new requires an absolute cwd"))?;
        let root = declared_root(cwd)?;
        let root_paths = Paths::discover(&root)
            .map_err(|error| RpcError::internal(format!("cannot prepare {root:?}: {error}")))?;
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
        let runtime = create_runtime(self.session_config(&root), root_paths, options)
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

        let mut result = json!({"sessionId": id});
        if ignored_mcp > 0 {
            // Do not pretend these were connected; the harness config owns MCP.
            result["_meta"] = json!({
                "localharness": {
                    "ignoredMcpServers": ignored_mcp,
                    "note": "client-supplied MCP servers are not connected in this version; configure them in the harness config",
                }
            });
        }
        Ok(result)
    }

    pub(crate) async fn load_session(self: &Arc<Self>, params: Value) -> Result<Value, RpcError> {
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
        let root_paths = Paths::discover(&root)
            .map_err(|error| RpcError::internal(format!("cannot prepare {root:?}: {error}")))?;
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
        let runtime = create_runtime(self.config.clone(), root_paths, options)
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
        Ok(json!({"_meta": {"localharness": {"sessionId": loaded_id}}}))
    }

    /// Replay user and assistant text so the editor sees the history it is
    /// loading. Tool calls and results are not replayed.
    fn replay(&self, session: &AcpSession) {
        let entries: Vec<(bool, String)> = {
            let guard = lock_session(&session.core);
            guard
                .branch_entries()
                .into_iter()
                .filter_map(|entry| match &entry.kind {
                    EntryKind::User(user) => Some((true, user.content.clone())),
                    EntryKind::Assistant(assistant) => assistant
                        .content
                        .as_deref()
                        .filter(|text| !text.trim().is_empty())
                        .map(|text| (false, text.to_string())),
                    _ => None,
                })
                .collect()
        };
        for (is_user, text) in entries {
            let update = if is_user {
                json!({
                    "sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": sanitize::display(&text)},
                })
            } else {
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": sanitize::display(&text)},
                })
            };
            self.notify(
                "session/update",
                json!({"sessionId": session.id, "update": update}),
            );
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
        tokio::spawn(async move {
            server
                .drive_turn(session, session_id, request_id, text, cancel)
                .await;
        });
        Ok(())
    }

    async fn drive_turn(
        self: Arc<Self>,
        session: Arc<AcpSession>,
        session_id: String,
        request_id: Value,
        text: String,
        cancel: CancellationToken,
    ) {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let core = session.core.clone();
        let cancel_turn = cancel.clone();
        let handle = tokio::spawn(async move { run_turn(core, text, tx, cancel_turn).await });

        let mut outcome: Option<TurnOutcome> = None;
        while let Some(event) = rx.recv().await {
            if let AgentEvent::TurnDone(result) = &event {
                outcome = Some(result.clone());
            }
            self.forward_event(&session_id, event);
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

    fn forward_event(&self, session_id: &str, event: AgentEvent) {
        let update = match event {
            AgentEvent::TextDelta(text) => {
                let text = sanitize::display(&text);
                if text.is_empty() {
                    return;
                }
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                })
            }
            AgentEvent::ReasoningDelta(text) => {
                let text = sanitize::display(&text);
                if text.is_empty() {
                    return;
                }
                json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": text},
                })
            }
            AgentEvent::ToolStarted {
                call_id,
                name,
                summary,
            } => json!({
                "sessionUpdate": "tool_call",
                "toolCallId": call_id,
                "title": permission_title(&name, &Value::Null, Some(&summary)),
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
            tool_call["locations"] = json!([{"path": path.display().to_string()}]);
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

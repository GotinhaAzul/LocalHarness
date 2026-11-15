//! Routing, handlers, and the per-session event log.
//!
//! Permission over HTTP: nobody can answer a prompt in the middle of an
//! HTTP turn, so a session picks its permission mode when it is created and
//! installs that mode on the session's permission engine before the runtime
//! is built. The engine is the single pre-dispatch decision point
//! (`authorize::decide()`): config `[permissions]` rules decide first, and an
//! `ask` that nobody can answer is denied. Modes reuse the engine names (`ask`
//! default, `accept-edits`, `dont-ask`, `yolo`); `yolo` must be enabled by the
//! operator with `lh serve --allow-yolo`, never by a request alone.

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited, StreamBody, combinators::BoxBody};
use hyper::body::Bytes;
use hyper::body::{Frame as BodyFrame, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentEvent;
use crate::events::{CANCELLED, event_json, event_name};
use crate::host::{Host, HostedSession, declared_root, start_turn};

pub type Body = BoxBody<Bytes, Infallible>;

/// Largest request body accepted.
const MAX_BODY: usize = 1 << 20;
/// Frames kept per session for `Last-Event-ID` / `?after=` replay.
const EVENT_BUFFER: usize = 4096;
const BROADCAST_CAPACITY: usize = 1024;
const KEEPALIVE: Duration = Duration::from_secs(15);

/// How an HTTP session answers calls that would need a person's approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    /// Read-only built-in tools run; everything else is denied because an
    /// HTTP turn cannot ask.
    Ask,
    /// Like `ask`, plus file edits (`write`, `edit`, `patch`, `undo`).
    AcceptEdits,
    /// Never asks: a call that would ask is denied instead.
    DontAsk,
    /// Every call the harness gates allow. Requires `--allow-yolo`.
    Yolo,
}

impl PermissionMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "ask" | "default" => Some(Self::Ask),
            "accept-edits" | "acceptedits" => Some(Self::AcceptEdits),
            "dont-ask" | "dontask" => Some(Self::DontAsk),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AcceptEdits => "accept-edits",
            Self::DontAsk => "dont-ask",
            Self::Yolo => "yolo",
        }
    }

    /// The policy engine's mode. The HTTP name and the config name are the
    /// same four modes; `plan` stays a CLI/TUI flow.
    pub fn policy(self) -> crate::policy::PermissionMode {
        match self {
            Self::Ask => crate::policy::PermissionMode::Ask,
            Self::AcceptEdits => crate::policy::PermissionMode::AcceptEdits,
            Self::DontAsk => crate::policy::PermissionMode::DontAsk,
            Self::Yolo => crate::policy::PermissionMode::Yolo,
        }
    }
}

/// One SSE frame.
struct Frame {
    seq: u64,
    name: &'static str,
    data: String,
}

impl Frame {
    fn bytes(&self) -> Bytes {
        Bytes::from(format!(
            "id: {}\nevent: {}\ndata: {}\n\n",
            self.seq, self.name, self.data
        ))
    }
}

struct LogInner {
    next: u64,
    buffer: VecDeque<Arc<Frame>>,
    tx: broadcast::Sender<Arc<Frame>>,
}

/// A session's events in order, with a bounded replay buffer. Publishing and
/// subscribing take the same lock, so a subscriber's replay and its live
/// receiver never overlap or leave a gap.
struct EventLog {
    inner: Mutex<LogInner>,
}

impl EventLog {
    fn new() -> Self {
        let (tx, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Mutex::new(LogInner {
                next: 1,
                buffer: VecDeque::new(),
                tx,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LogInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn publish(&self, name: &'static str, value: &Value) {
        Self::push(&mut self.lock(), name, value);
    }

    /// Append under a lock the caller already holds.
    fn push(inner: &mut LogInner, name: &'static str, value: &Value) {
        let frame = Arc::new(Frame {
            seq: inner.next,
            name,
            data: value.to_string(),
        });
        inner.next += 1;
        if inner.buffer.len() == EVENT_BUFFER {
            inner.buffer.pop_front();
        }
        inner.buffer.push_back(frame.clone());
        let _ = inner.tx.send(frame);
    }

    fn publish_event(&self, event: &AgentEvent) {
        self.publish(event_name(event), &event_json(event));
    }

    /// The sequence number of the last published frame (0 when none).
    fn last_seq(&self) -> u64 {
        self.lock().next - 1
    }

    fn subscribe(&self, after: u64) -> (VecDeque<Arc<Frame>>, broadcast::Receiver<Arc<Frame>>) {
        let inner = self.lock();
        let replay = inner
            .buffer
            .iter()
            .filter(|frame| frame.seq > after)
            .cloned()
            .collect();
        (replay, inner.tx.subscribe())
    }
}

/// A session served over HTTP.
struct Live {
    session: Arc<HostedSession>,
    mode: PermissionMode,
    trusted: bool,
    events: EventLog,
    turns: AtomicU64,
    last_status: Mutex<Option<String>>,
}

impl Live {
    fn info(&self) -> Value {
        json!({
            "id": self.session.id,
            "root": self.session.root.display().to_string(),
            "busy": self.session.busy(),
            "permission_mode": self.mode.label(),
            "trusted": self.trusted,
            "turns": self.turns.load(Ordering::SeqCst),
            "last_status": *self.last_status.lock().unwrap_or_else(|p| p.into_inner()),
            "last_event": self.events.last_seq(),
        })
    }
}

pub struct State {
    host: Host,
    token: String,
    allow_yolo: bool,
    /// Cancelled after running turns have stopped; ends SSE streams.
    shutdown: CancellationToken,
    /// Set when shutdown starts; refuses new sessions and turns.
    stopping: std::sync::atomic::AtomicBool,
    sessions: Mutex<HashMap<String, Arc<Live>>>,
    ephemeral: AtomicU64,
}

impl State {
    pub fn new(host: Host, token: String, allow_yolo: bool, shutdown: CancellationToken) -> Self {
        Self {
            host,
            token,
            allow_yolo,
            shutdown,
            stopping: std::sync::atomic::AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
            ephemeral: AtomicU64::new(1),
        }
    }

    fn sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Live>>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn live(&self, id: &str) -> Option<Arc<Live>> {
        self.sessions().get(id).cloned()
    }

    /// Cancel every running turn and wait (bounded) until each has written
    /// its cancellation to the session file, then stop MCP servers.
    pub async fn shutdown(&self, wait: Duration) {
        self.stopping.store(true, Ordering::SeqCst);
        let lives: Vec<Arc<Live>> = self.sessions().drain().map(|(_, live)| live).collect();
        for live in &lives {
            live.session.cancel();
        }
        for live in &lives {
            if !live.session.wait_idle(wait).await {
                tracing::warn!(session = %live.session.id, "turn did not stop before shutdown");
            }
            live.session.core.mcp.shutdown().await;
        }
    }

    /// Constant-time comparison against the server token.
    fn authorized(&self, request: &Request<Incoming>) -> bool {
        let Some(value) = request.headers().get(header::AUTHORIZATION) else {
            return false;
        };
        let Some(presented) = value
            .as_bytes()
            .strip_prefix(b"Bearer ")
            .or_else(|| value.as_bytes().strip_prefix(b"bearer "))
        else {
            return false;
        };
        bool::from(presented.ct_eq(self.token.as_bytes()))
    }
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(value.to_string()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error(status: StatusCode, code: &str, message: impl Into<String>) -> Response<Body> {
    json_response(
        status,
        json!({"error": {"code": code, "message": message.into()}}),
    )
}

fn error_with(status: StatusCode, code: &str, message: &str, extra: Value) -> Response<Body> {
    let mut body = json!({"code": code, "message": message});
    if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            body.insert(key.clone(), value.clone());
        }
    }
    json_response(status, json!({"error": body}))
}

fn unknown_session(id: &str) -> Response<Body> {
    error_with(
        StatusCode::NOT_FOUND,
        "unknown_session",
        "no live session with this id; create one with POST /v1/sessions, or resume a stored one with {\"resume\": \"<id>\"}",
        json!({"session_id": id}),
    )
}

fn method_not_allowed(allow: &'static str) -> Response<Body> {
    let mut response = error(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        format!("this endpoint accepts {allow}"),
    );
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static(allow));
    response
}

/// Entry point for every request. Order: refuse browsers, check the token,
/// then route. Nothing about sessions is revealed before the token passes.
pub async fn handle(state: Arc<State>, request: Request<Incoming>) -> Response<Body> {
    if request.headers().contains_key(header::ORIGIN) {
        return error(
            StatusCode::FORBIDDEN,
            "cross_origin_refused",
            "requests from a browser origin are refused; lh serve is for local scripts and has no CORS support",
        );
    }
    if !state.authorized(&request) {
        let mut response = error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid bearer token; send `Authorization: Bearer <token>` with the token from the file `lh serve` printed at startup",
        );
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"lh serve\""),
        );
        return response;
    }

    let path = request.uri().path().to_string();
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    tracing::debug!(method = %request.method(), path = %path, "lh serve request");
    match (request.method().clone(), segments.as_slice()) {
        (Method::GET, ["v1", "health"]) => json_response(
            StatusCode::OK,
            json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "sessions": state.sessions().len(),
            }),
        ),
        (_, ["v1", "health"]) => method_not_allowed("GET"),
        (Method::GET, ["v1", "sessions"]) => {
            let mut sessions: Vec<Value> = state.sessions().values().map(|l| l.info()).collect();
            sessions.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
            json_response(StatusCode::OK, json!({"sessions": sessions}))
        }
        (Method::POST, ["v1", "sessions"]) => match read_json(request).await {
            Ok(body) => create_session(&state, body).await,
            Err(response) => *response,
        },
        (_, ["v1", "sessions"]) => method_not_allowed("GET, POST"),
        (Method::GET, ["v1", "sessions", id]) => match state.live(id) {
            Some(live) => json_response(StatusCode::OK, live.info()),
            None => unknown_session(id),
        },
        (_, ["v1", "sessions", _]) => method_not_allowed("GET"),
        (Method::POST, ["v1", "sessions", id, "turns"]) => {
            let Some(live) = state.live(id) else {
                return unknown_session(id);
            };
            match read_json(request).await {
                Ok(body) => start(&state, live, body),
                Err(response) => *response,
            }
        }
        (_, ["v1", "sessions", _, "turns"]) => method_not_allowed("POST"),
        (Method::GET, ["v1", "sessions", id, "events"]) => {
            let Some(live) = state.live(id) else {
                return unknown_session(id);
            };
            match cursor(&request) {
                Ok(after) => events(&state, &live, after),
                Err(response) => *response,
            }
        }
        (_, ["v1", "sessions", _, "events"]) => method_not_allowed("GET"),
        (Method::POST, ["v1", "sessions", id, "cancel"]) => {
            let Some(live) = state.live(id) else {
                return unknown_session(id);
            };
            // Publish under the same lock `drive` holds while it releases the
            // session and publishes `done`, so `cancelled` cannot slip behind
            // the terminal frame when a turn is already settling.
            let mut log = live.events.lock();
            let cancelled = live.session.cancel();
            if cancelled {
                EventLog::push(&mut log, CANCELLED, &json!({"type": CANCELLED}));
            }
            drop(log);
            json_response(
                StatusCode::OK,
                json!({"session_id": live.session.id, "cancelled": cancelled}),
            )
        }
        (_, ["v1", "sessions", _, "cancel"]) => method_not_allowed("POST"),
        _ => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "unknown endpoint; see docs/serve.md for the /v1 API",
        ),
    }
}

/// Read a JSON object body. An empty body is `{}`.
async fn read_json(request: Request<Incoming>) -> Result<Value, Box<Response<Body>>> {
    let collected = Limited::new(request.into_body(), MAX_BODY)
        .collect()
        .await
        .map_err(|_| {
            boxed_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "body_too_large",
                format!("request bodies are limited to {MAX_BODY} bytes"),
            )
        })?
        .to_bytes();
    if collected.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_slice(&collected).map_err(|error| {
        boxed_error(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            format!("the body is not valid JSON: {error}"),
        )
    })?;
    if !value.is_object() {
        return Err(boxed_error(
            StatusCode::BAD_REQUEST,
            "invalid_json",
            "the body must be a JSON object",
        ));
    }
    Ok(value)
}

fn boxed_error(status: StatusCode, code: &str, message: impl Into<String>) -> Box<Response<Body>> {
    Box::new(error(status, code, message))
}

fn bad_request(message: impl Into<String>) -> Response<Body> {
    error(StatusCode::BAD_REQUEST, "invalid_params", message)
}

async fn create_session(state: &Arc<State>, body: Value) -> Response<Body> {
    if state.stopping.load(Ordering::SeqCst) {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is stopping",
        );
    }
    let root = match body.get("cwd") {
        None | Some(Value::Null) => state.host.process_paths.cwd.clone(),
        Some(Value::String(cwd)) => match declared_root(cwd) {
            Ok(root) => root,
            Err(message) => return bad_request(message),
        },
        Some(_) => return bad_request("cwd must be an absolute path string"),
    };
    let mode = match body.get("permission_mode") {
        None | Some(Value::Null) => PermissionMode::Ask,
        Some(Value::String(raw)) => match PermissionMode::parse(raw) {
            Some(mode) => mode,
            None => {
                return bad_request(format!(
                    "unknown permission_mode {raw:?}; use ask, accept-edits, dont-ask, or yolo"
                ));
            }
        },
        Some(_) => return bad_request("permission_mode must be a string"),
    };
    if mode == PermissionMode::Yolo && !state.allow_yolo {
        return error(
            StatusCode::FORBIDDEN,
            "mode_not_permitted",
            "permission_mode \"yolo\" needs the operator to start the server with `lh serve --allow-yolo`",
        );
    }
    let resume = match body.get("resume") {
        None | Some(Value::Null) => None,
        Some(Value::String(id)) => {
            if state.host.no_session {
                return error(
                    StatusCode::BAD_REQUEST,
                    "unsupported",
                    "resume is unavailable with --no-session because nothing was persisted",
                );
            }
            if !crate::host::valid_session_id(id) {
                return bad_request(format!("session id {id:?} is not a plain identifier"));
            }
            if state.live(id).is_some() {
                return error_with(
                    StatusCode::CONFLICT,
                    "session_already_loaded",
                    "this session is already live in this server; use it directly",
                    json!({"session_id": id}),
                );
            }
            let file = match state.host.session_paths(&root) {
                Ok(paths) => paths.session_file(id),
                Err(error) => {
                    return self::error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        format!("{error:#}"),
                    );
                }
            };
            if !file.is_file() {
                return error_with(
                    StatusCode::NOT_FOUND,
                    "unknown_session",
                    "no stored session with this id under the session root",
                    json!({"session_id": id}),
                );
            }
            Some(file)
        }
        Some(_) => return bad_request("resume must be a session id string"),
    };
    let resumed = resume.is_some();

    let opened = match state
        .host
        .open_in_mode(&root, resume, None, Some(mode.policy()))
        .await
    {
        Ok(opened) => opened,
        Err(error) => {
            return self::error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "session_open_failed",
                format!("cannot open a session: {error:#}"),
            );
        }
    };
    if state.stopping.load(Ordering::SeqCst) {
        opened.core.mcp.shutdown().await;
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is stopping",
        );
    }
    if !opened.core.logged_in() {
        opened.core.mcp.shutdown().await;
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_model",
            "no model is selected. Start the inference server and restart `lh serve` with --model (or set LH_MODEL)",
        );
    }
    let id = if state.host.no_session {
        format!("http-{}", state.ephemeral.fetch_add(1, Ordering::SeqCst))
    } else {
        opened.harness_id
    };
    let live = Arc::new(Live {
        session: Arc::new(HostedSession::new(id.clone(), root, opened.core)),
        mode,
        trusted: opened.trusted,
        events: EventLog::new(),
        turns: AtomicU64::new(0),
        last_status: Mutex::new(None),
    });
    // Session ids are timestamp-based, so two roots created in the same second
    // can collide. Insert under the map lock and refuse a collision instead of
    // silently replacing the first live session.
    let collision = {
        let mut sessions = state.sessions();
        if sessions.contains_key(&id) {
            true
        } else {
            sessions.insert(id.clone(), live.clone());
            false
        }
    };
    if collision {
        live.session.core.mcp.shutdown().await;
        return error_with(
            StatusCode::CONFLICT,
            "session_id_collision",
            "a live session already uses this id; retry in a moment, or resume the existing session",
            json!({"session_id": id}),
        );
    }
    let mut info = live.info();
    info["resumed"] = json!(resumed);
    json_response(StatusCode::CREATED, info)
}

fn start(state: &Arc<State>, live: Arc<Live>, body: Value) -> Response<Body> {
    if state.stopping.load(Ordering::SeqCst) {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "the server is stopping",
        );
    }
    let text = match body.get("prompt") {
        Some(Value::String(text)) if !text.trim().is_empty() => text.clone(),
        Some(Value::String(_)) => return bad_request("the prompt is empty"),
        _ => return bad_request("POST /turns requires {\"prompt\": \"<text>\"}"),
    };
    // Claim the session and read the cursor under the event-log lock, which
    // `drive` also holds while it releases the session and publishes `done`.
    // So `after` is always at or past the previous turn's `done`.
    let log = live.events.lock();
    let Ok(cancel) = live.session.begin() else {
        drop(log);
        return error_with(
            StatusCode::CONFLICT,
            "session_busy",
            "a turn is already running in this session; wait for its `done` event or POST /cancel",
            json!({"session_id": live.session.id}),
        );
    };
    let after = log.next - 1;
    drop(log);
    let turn = live.turns.fetch_add(1, Ordering::SeqCst) + 1;
    let id = live.session.id.clone();
    tokio::spawn(drive(live, text, cancel));
    json_response(
        StatusCode::ACCEPTED,
        json!({
            "session_id": id,
            "turn": turn,
            "after": after,
            "events": format!("/v1/sessions/{id}/events?after={after}"),
        }),
    )
}

/// Run one turn and publish its events. The `done` frame is published only
/// after the session is released, so a client that sees `done` can start the
/// next turn without a spurious conflict.
async fn drive(live: Arc<Live>, text: String, cancel: CancellationToken) {
    let mut turn = start_turn(live.session.core.clone(), text, cancel);
    let mut seen = None;
    while let Some(event) = turn.events.recv().await {
        match event {
            AgentEvent::TurnDone(outcome) => seen = Some(outcome),
            event => live.events.publish_event(&event),
        }
    }
    let outcome = turn.outcome(seen).await;
    *live
        .last_status
        .lock()
        .unwrap_or_else(|poison| poison.into_inner()) = Some(format!("{:?}", outcome.status));
    let done = AgentEvent::TurnDone(outcome);
    let mut log = live.events.lock();
    live.session.finish();
    EventLog::push(&mut log, event_name(&done), &event_json(&done));
}

/// The replay cursor: `?after=N`, else `Last-Event-ID`, else live only.
fn cursor(request: &Request<Incoming>) -> Result<Option<u64>, Box<Response<Body>>> {
    let from_query = request.uri().query().and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("after="))
            .map(str::to_string)
    });
    let raw = from_query.or_else(|| {
        request
            .headers()
            .get("last-event-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    });
    match raw {
        None => Ok(None),
        Some(raw) => raw.trim().parse().map(Some).map_err(|_| {
            boxed_error(
                StatusCode::BAD_REQUEST,
                "invalid_params",
                "after / Last-Event-ID must be an event id (a non-negative integer)",
            )
        }),
    }
}

struct Sse {
    replay: VecDeque<Arc<Frame>>,
    rx: broadcast::Receiver<Arc<Frame>>,
    shutdown: CancellationToken,
    last: u64,
}

fn events(state: &Arc<State>, live: &Arc<Live>, after: Option<u64>) -> Response<Body> {
    let after = after.unwrap_or_else(|| live.events.last_seq());
    let (replay, rx) = live.events.subscribe(after);
    let sse = Sse {
        replay,
        rx,
        shutdown: state.shutdown.clone(),
        last: after,
    };
    let stream = futures_util::stream::unfold(sse, |mut sse| async move {
        if let Some(frame) = sse.replay.pop_front() {
            sse.last = frame.seq;
            return Some((Ok(BodyFrame::data(frame.bytes())), sse));
        }
        loop {
            // Biased toward frames, so a `done` already published before
            // shutdown is delivered before the stream closes.
            tokio::select! {
                biased;
                received = sse.rx.recv() => match received {
                    Ok(frame) if frame.seq <= sse.last => continue,
                    Ok(frame) => {
                        sse.last = frame.seq;
                        return Some((Ok(BodyFrame::data(frame.bytes())), sse));
                    }
                    // A slow reader lost frames: end the stream so the
                    // client reconnects with Last-Event-ID and replays.
                    Err(_) => return None,
                },
                _ = sse.shutdown.cancelled() => return None,
                _ = tokio::time::sleep(KEEPALIVE) => {
                    return Some((Ok(BodyFrame::data(Bytes::from_static(b": keepalive\n\n"))), sse));
                }
            }
        }
    });
    let mut response = Response::new(BodyExt::boxed(StreamBody::new(stream)));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_modes_select_the_same_policy_modes() {
        use crate::policy::PermissionMode as PolicyMode;
        assert_eq!(PermissionMode::Ask.policy(), PolicyMode::Ask);
        assert_eq!(
            PermissionMode::AcceptEdits.policy(),
            PolicyMode::AcceptEdits
        );
        assert_eq!(PermissionMode::DontAsk.policy(), PolicyMode::DontAsk);
        assert_eq!(PermissionMode::Yolo.policy(), PolicyMode::Yolo);
    }

    #[test]
    fn config_allow_and_deny_rules_apply_to_http_sessions() {
        use crate::policy::{
            CallFacts, Decision, PermissionMode as PolicyMode, Permissions, Policy, RuleAction,
            RuleConfig,
        };
        let rule = |tool: &str, action: RuleAction| RuleConfig {
            id: None,
            tool: Some(tool.into()),
            path: None,
            command: None,
            network: None,
            action,
        };
        let permissions = Permissions::new(
            Policy::compile(&[
                RuleConfig {
                    id: Some("allow-write".into()),
                    ..rule("write", RuleAction::Allow)
                },
                RuleConfig {
                    id: Some("deny-env".into()),
                    path: Some("**/.env".into()),
                    ..rule("write", RuleAction::Deny)
                },
            ])
            .unwrap(),
            PolicyMode::Ask,
        );
        let cwd = std::path::Path::new("/work");
        let write = CallFacts::new("write", false, &json!({"path": "out.txt"}), cwd);
        assert_eq!(permissions.decide(&write), Decision::Allow);
        let env = CallFacts::new("write", false, &json!({"path": ".env"}), cwd);
        assert!(matches!(permissions.decide(&env), Decision::Deny { .. }));
        // No rule for a mutating command: the default mode still asks, and an
        // HTTP turn cannot answer.
        let bash = CallFacts::new("bash", false, &json!({"command": "touch marker.txt"}), cwd);
        assert!(matches!(permissions.decide(&bash), Decision::Ask(_)));
    }

    #[test]
    fn modes_parse_with_engine_names() {
        for (raw, mode) in [
            ("ask", PermissionMode::Ask),
            ("accept-edits", PermissionMode::AcceptEdits),
            ("dont-ask", PermissionMode::DontAsk),
            ("YOLO", PermissionMode::Yolo),
        ] {
            assert_eq!(PermissionMode::parse(raw), Some(mode));
            assert_eq!(PermissionMode::parse(mode.label()), Some(mode));
        }
        assert_eq!(PermissionMode::parse("bypass"), None);
    }

    #[test]
    fn replay_and_live_frames_never_overlap() {
        let log = EventLog::new();
        log.publish("text", &json!({"type": "text"}));
        log.publish("text", &json!({"type": "text"}));
        let (replay, mut rx) = log.subscribe(1);
        assert_eq!(replay.iter().map(|f| f.seq).collect::<Vec<_>>(), vec![2]);
        log.publish("done", &json!({"type": "done"}));
        let next = rx.try_recv().unwrap();
        assert_eq!(next.seq, 3);
        assert_eq!(
            &next.bytes()[..],
            b"id: 3\nevent: done\ndata: {\"type\":\"done\"}\n\n"
        );
        assert_eq!(log.last_seq(), 3);
    }
}

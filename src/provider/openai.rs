//! OpenAI-compatible streaming client.

use std::time::Duration;
use tokio::time::Instant;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::config::ModelConfig;

use super::sse::{SseFrame, SseParser};
use super::types::{
    ChatMessage, ChatRequest, ErrorClass, ProviderError, StreamEvent, StreamItem, Usage,
};
use super::{EndpointUrls, Provider, endpoint_urls};

#[derive(Debug, Clone)]
struct Timeouts {
    total: Duration,
    first_token: Duration,
    idle: Duration,
}

pub struct OpenAiCompatProvider {
    client: reqwest::Client,
    base_url: String,
    urls: EndpointUrls,
    api_key: String,
    model: String,
    timeouts: Timeouts,
}

impl OpenAiCompatProvider {
    pub fn new(config: &ModelConfig, model: impl Into<String>) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout())
            .build()?;
        Ok(Self {
            client,
            base_url: config.endpoint.trim().trim_end_matches('/').to_string(),
            urls: endpoint_urls(&config.endpoint),
            api_key: config.api_key.clone(),
            model: model.into(),
            timeouts: Timeouts {
                total: config.total_timeout(),
                first_token: config.first_token_timeout(),
                idle: config.stream_idle_timeout(),
            },
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.urls.api)
    }

    fn scrub(&self, message: impl Into<String>) -> String {
        super::redact(message.into(), &[&self.api_key])
    }

    async fn run_attempt(
        &self,
        req: &ChatRequest,
        tx: &UnboundedSender<StreamItem>,
        cancel: &CancellationToken,
        include_usage: bool,
        yielded: &mut bool,
    ) -> Attempt {
        let body = match build_body(req, include_usage) {
            Ok(body) => body,
            Err(err) => return Attempt::Failed(err),
        };
        let mut request = self.client.post(self.url()).json(&body);
        if !self.api_key.is_empty() {
            request = request.bearer_auth(&self.api_key);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(err) => {
                let msg = if err.is_connect() {
                    format!(
                        "cannot reach {}: {err}. Start the inference server or set LH_ENDPOINT.",
                        self.base_url
                    )
                } else if err.is_timeout() {
                    format!(
                        "request to {} timed out ({err}). The server may be loading the model; retry or raise model.request_timeout_secs.",
                        self.base_url
                    )
                } else {
                    format!("request to {} failed: {err}", self.base_url)
                };
                return Attempt::Failed(ProviderError::new(
                    ErrorClass::Connection,
                    self.scrub(msg),
                ));
            }
        };

        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            let text = response.text().await.unwrap_or_default();
            let text = truncate_body(&text);
            if code == 400 && mentions_stream_options(&text) {
                return Attempt::RetryWithoutUsage;
            }
            if let Some(err) = classify_http(code, &text, &self.model) {
                return Attempt::Failed(ProviderError::with_status(
                    err.class,
                    code,
                    self.scrub(err.message),
                ));
            }
            return Attempt::Failed(ProviderError::with_status(
                ErrorClass::BadRequest,
                code,
                self.scrub(format!("endpoint rejected the request ({code}): {text}")),
            ));
        }

        let mut parser = SseParser::new();
        let mut tool_index = ToolCallIndexer::default();
        let mut stream = response.bytes_stream();
        let total_deadline = Instant::now() + self.timeouts.total;
        let mut idle_until = next_idle(Instant::now(), self.timeouts.first_token, total_deadline);
        let mut finish_sent = false;
        let mut saw_event = false;

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = tx.send(Err(ProviderError::new(
                        ErrorClass::Cancelled,
                        "generation cancelled",
                    )));
                    return Attempt::Finished;
                }
                result = tokio::time::timeout_at(idle_until, stream.next()) => {
                    match result {
                        Err(_) if idle_until >= total_deadline => {
                            return Attempt::Failed(ProviderError::new(
                                ErrorClass::Connection,
                                format!(
                                    "request exceeded {} s; raise model.request_timeout_secs or reduce context.",
                                    self.timeouts.total.as_secs()
                                ),
                            ));
                        }
                        Err(_) if !saw_event => {
                            return Attempt::Failed(ProviderError::new(
                                ErrorClass::Connection,
                                format!(
                                    "no first token within {} s; the model may still be loading. Retry, or raise model.first_token_timeout_secs.",
                                    self.timeouts.first_token.as_secs()
                                ),
                            ));
                        }
                        Err(_) => {
                            return Attempt::Failed(ProviderError::new(
                                ErrorClass::Connection,
                                format!(
                                    "stream idle for {} s; the server stopped responding. Retry or check the engine log.",
                                    self.timeouts.idle.as_secs()
                                ),
                            ));
                        }
                        Ok(None) => break,
                        Ok(Some(Err(err))) => {
                            return Attempt::Failed(ProviderError::new(
                                ErrorClass::Connection,
                                self.scrub(format!("stream error: {err}. Retry; if it persists check the engine.")),
                            ));
                        }
                        Ok(Some(Ok(bytes))) => bytes,
                    }
                }
            };

            idle_until = next_idle(Instant::now(), self.timeouts.idle, total_deadline);
            let frames = parser.push(&chunk);
            if !frames.is_empty() {
                saw_event = true;
            }
            for frame in frames {
                match frame {
                    SseFrame::Done => {
                        finish_sent = true;
                        break;
                    }
                    SseFrame::Data(data) => {
                        if let Err(event) =
                            self.handle_data(&data, req, tx, yielded, &mut tool_index)
                        {
                            return Attempt::Failed(event);
                        }
                    }
                }
            }
            if finish_sent {
                break;
            }
        }

        for frame in parser.finish() {
            match frame {
                SseFrame::Done => {}
                SseFrame::Data(data) => {
                    if let Err(event) = self.handle_data(&data, req, tx, yielded, &mut tool_index) {
                        return Attempt::Failed(event);
                    }
                }
            }
        }
        Attempt::Finished
    }

    fn handle_data(
        &self,
        data: &str,
        req: &ChatRequest,
        tx: &UnboundedSender<StreamItem>,
        yielded: &mut bool,
        tool_index: &mut ToolCallIndexer,
    ) -> Result<(), ProviderError> {
        let trimmed = data.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let chunk: Chunk = serde_json::from_str(trimmed).map_err(|err| {
            ProviderError::new(
                ErrorClass::Parse,
                format!(
                    "could not parse stream chunk ({err}); provider is not speaking OpenAI SSE. Body: {}",
                    truncate_body(trimmed)
                ),
            )
        })?;

        if let Some(err) = chunk.error {
            let message = err.into_message();
            let class = if is_context_error(&message) {
                ErrorClass::ContextTooSmall
            } else {
                ErrorClass::BadRequest
            };
            return Err(ProviderError::new(class, self.scrub(message)));
        }

        let truncated = chunk.truncated;
        let context_shift = chunk.context_shift;
        if let Some(usage) = usage_from(chunk.usage, chunk.timings, truncated, context_shift) {
            let _ = tx.send(Ok(StreamEvent::Usage(usage)));
        }

        for choice in chunk.choices {
            let delta = choice.delta;
            if let Some(reasoning) = reasoning_text(&delta, req.reasoning_field.as_deref())
                && !reasoning.is_empty()
            {
                *yielded = true;
                let _ = tx.send(Ok(StreamEvent::ReasoningDelta(reasoning)));
            }
            if let Some(content) = delta.content
                && !content.is_empty()
            {
                *yielded = true;
                let _ = tx.send(Ok(StreamEvent::Delta(content)));
            }
            for call in delta.tool_calls.unwrap_or_default() {
                let index = tool_index.assign(&call);
                let (name, arguments) = match call.function {
                    Some(function) => (function.name, function.arguments),
                    None => (None, None),
                };
                if call.id.is_some() || name.is_some() || arguments.is_some() {
                    *yielded = true;
                    let _ = tx.send(Ok(StreamEvent::ToolCallDelta {
                        index,
                        id: call.id,
                        name,
                        arguments,
                    }));
                }
            }
            if let Some(reason) = choice.finish_reason {
                let _ = tx.send(Ok(StreamEvent::Finish(reason)));
            }
        }
        Ok(())
    }
}

enum Attempt {
    Finished,
    RetryWithoutUsage,
    Failed(ProviderError),
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatProvider {
    async fn chat_stream(
        &self,
        req: ChatRequest,
        tx: UnboundedSender<StreamItem>,
        cancel: CancellationToken,
    ) {
        let mut include_usage = req.include_usage;
        let mut connection_retries = 0;
        let mut yielded = false;
        loop {
            let outcome = self
                .run_attempt(&req, &tx, &cancel, include_usage, &mut yielded)
                .await;
            match outcome {
                Attempt::Finished => return,
                Attempt::RetryWithoutUsage => {
                    if include_usage {
                        include_usage = false;
                        continue;
                    }
                    let _ = tx.send(Err(ProviderError::new(
                        ErrorClass::BadRequest,
                        self.scrub(
                            "endpoint rejects stream_options; the harness retried without it and the error persisted.",
                        ),
                    )));
                    return;
                }
                Attempt::Failed(mut err) => {
                    if err.class == ErrorClass::Connection && !yielded && connection_retries == 0 {
                        connection_retries += 1;
                        tokio::time::sleep(Duration::from_millis(250)).await;
                        continue;
                    }
                    err.message = self.scrub(err.message);
                    let _ = tx.send(Err(err));
                    return;
                }
            }
        }
    }

    fn endpoint(&self) -> &str {
        &self.base_url
    }

    fn model(&self) -> &str {
        &self.model
    }

    fn client(&self) -> Option<&reqwest::Client> {
        Some(&self.client)
    }

    fn api_key(&self) -> &str {
        &self.api_key
    }
}

fn next_idle(now: Instant, idle: Duration, total_deadline: Instant) -> Instant {
    (now + idle).min(total_deadline)
}

fn truncate_body(body: &str) -> String {
    let max = 2000;
    if body.len() <= max {
        return body.to_string();
    }
    let mut end = max;
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &body[..end])
}

fn mentions_stream_options(body: &str) -> bool {
    body.to_ascii_lowercase().contains("stream_options")
}

fn is_context_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    (message.contains("context") || message.contains("token"))
        && (message.contains("too long")
            || message.contains("too large")
            || message.contains("exceed")
            || message.contains("length")
            || message.contains("limit"))
}

fn classify_http(code: u16, body: &str, model: &str) -> Option<ProviderError> {
    let body_lower = body.to_ascii_lowercase();
    let message = match code {
        401 | 403 => format!(
            "endpoint rejected credentials ({code}). Set model.api_key or LH_API_KEY, or run the server without auth."
        ),
        404 => format!(
            "endpoint has no model {model:?} (404): {body}. List models with GET /v1/models or set --model."
        ),
        400 | 422 if is_context_error(body) => format!(
            "request exceeds the model context window (400). Raise the engine context (llama.cpp --ctx-size / n_ctx), set [model] context_window, or compact with /compact. Server said: {body}"
        ),
        400 | 422 if body_lower.contains("tool") => format!(
            "endpoint rejected the tool definitions ({code}): {body}. Enable tool calling: llama.cpp needs --jinja; vLLM needs --enable-auto-tool-choice plus a tool-call parser; Ollama needs a recent version and a tool-capable model."
        ),
        408 | 504 => format!("endpoint timed out ({code}): {body}"),
        429 => format!("endpoint is rate limited ({code}): {body}"),
        500..=599 => format!("endpoint error ({code}): {body}"),
        _ => format!("endpoint rejected the request ({code}): {body}"),
    };
    let class = match code {
        401 | 403 => ErrorClass::Auth,
        404 => ErrorClass::ModelNotFound,
        400 | 422 if is_context_error(body) => ErrorClass::ContextTooSmall,
        400 | 422 => ErrorClass::BadRequest,
        408 | 504 => ErrorClass::Connection,
        500..=599 => ErrorClass::Server,
        _ => ErrorClass::BadRequest,
    };
    Some(ProviderError::new(class, message))
}

/// Fold every system message (base prompt, compaction summaries, state block)
/// into one leading system turn, preserving the relative order of their
/// content. Several chat templates (Gemma, Mistral, and others llama.cpp
/// renders with `--jinja`) reject a system message after a non-system turn or
/// demand strictly alternating roles; one combined system turn is accepted by
/// all of them and keeps the same prefix for KV-cache reuse.
fn merge_leading_system(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    let system_count = messages
        .iter()
        .filter(|message| matches!(message, ChatMessage::System { .. }))
        .count();
    if system_count == 0 {
        return messages.to_vec();
    }
    if system_count == 1 && matches!(messages.first(), Some(ChatMessage::System { .. })) {
        return messages.to_vec();
    }
    let combined = messages
        .iter()
        .filter_map(|message| match message {
            ChatMessage::System { content } => Some(content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut merged = Vec::with_capacity(messages.len() - system_count + 1);
    merged.push(ChatMessage::System { content: combined });
    merged.extend(
        messages
            .iter()
            .filter(|message| !matches!(message, ChatMessage::System { .. }))
            .cloned(),
    );
    merged
}

fn build_body(req: &ChatRequest, include_usage: bool) -> Result<Value, ProviderError> {
    let normalized = merge_leading_system(&req.messages);
    debug_assert!(
        normalized
            .iter()
            .skip(1)
            .all(|message| !matches!(message, ChatMessage::System { .. })),
        "system messages must lead the message list; strict chat templates reject trailing system turns"
    );
    let messages: Vec<Value> = normalized
        .iter()
        .map(|message| match message {
            ChatMessage::System { content } => json!({"role":"system","content":content}),
            ChatMessage::User { content } => json!({"role":"user","content":content}),
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                let mut object = serde_json::Map::new();
                object.insert("role".into(), json!("assistant"));
                match content {
                    Some(content) => {
                        object.insert("content".into(), json!(content));
                    }
                    None => {
                        object.insert("content".into(), Value::Null);
                    }
                }
                if !tool_calls.is_empty() {
                    let calls: Vec<Value> = tool_calls
                        .iter()
                        .map(|call| {
                            json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into()),
                                }
                            })
                        })
                        .collect();
                    object.insert("tool_calls".into(), Value::Array(calls));
                }
                Value::Object(object)
            }
            ChatMessage::Tool { call_id, content } => {
                json!({"role":"tool","tool_call_id":call_id,"content":content})
            }
        })
        .collect();

    let mut body = json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "temperature": req.temperature,
        "top_p": req.top_p,
        "max_tokens": req.max_tokens,
    });
    if !req.tools.is_empty() {
        body["tools"] = super::tools_payload(&req.tools);
        body["tool_choice"] = json!(req.tool_choice.as_deref().unwrap_or("auto"));
        if let Some(parallel) = req.parallel_tool_calls {
            body["parallel_tool_calls"] = json!(parallel);
        }
    }
    if !req.stop.is_empty() {
        body["stop"] = json!(req.stop);
    }
    if include_usage {
        body["stream_options"] = json!({"include_usage": true});
    }
    if let Some(extra) = &req.extra_body
        && let Some(object) = extra.as_object()
    {
        for (key, value) in object {
            body[key] = value.clone();
        }
    }
    Ok(body)
}

#[derive(Debug, Deserialize)]
struct Chunk {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    /// llama.cpp's per-request timings, sent on the final chunk.
    #[serde(default)]
    timings: Option<Timings>,
    /// llama.cpp native route reports whether the prompt was cut.
    #[serde(default)]
    truncated: Option<bool>,
    /// KoboldCpp / context-shift engines report discarded context here.
    #[serde(default)]
    context_shift: Option<u32>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Default, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    total_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<PromptTokensDetails>,
}

#[derive(Debug, Default, Deserialize)]
struct PromptTokensDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
struct Timings {
    #[serde(default)]
    cache_n: Option<u32>,
    #[serde(default)]
    prompt_ms: Option<f64>,
    #[serde(default)]
    predicted_per_second: Option<f64>,
    /// Number of context-shift events, when the engine reports one.
    #[serde(default)]
    n_shift: Option<u32>,
}

/// Normalize the usage and timing shapes engines send into one `Usage`.
fn usage_from(
    usage: Option<WireUsage>,
    timings: Option<Timings>,
    truncated: Option<bool>,
    context_shift: Option<u32>,
) -> Option<Usage> {
    if usage.is_none() && timings.is_none() && truncated.is_none() && context_shift.is_none() {
        return None;
    }
    let wire = usage.unwrap_or_default();
    let timings = timings.unwrap_or_default();
    Some(Usage {
        prompt_tokens: wire.prompt_tokens,
        completion_tokens: wire.completion_tokens,
        total_tokens: wire.total_tokens,
        cached_tokens: wire
            .prompt_tokens_details
            .and_then(|details| details.cached_tokens)
            .or(timings.cache_n),
        prompt_ms: timings
            .prompt_ms
            .filter(|ms| ms.is_finite() && *ms >= 0.0)
            .map(|ms| ms.round() as u32),
        first_token_ms: None,
        tokens_per_sec: timings
            .predicted_per_second
            .filter(|rate| rate.is_finite() && *rate > 0.0)
            .map(|rate| rate as f32),
        truncated,
        context_shift: context_shift.or(timings.n_shift),
    })
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct ToolCallDelta {
    #[serde(default)]
    index: Option<usize>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Stateful assignment of stream-local tool-call indices.
///
/// OpenAI-compatible engines disagree about `index`: SGLang and vLLM send it,
/// Ollama's compatibility route and some gateways omit it entirely. A missing
/// index used to fall back to the position inside the current chunk, so two
/// calls arriving in separate chunks both became index 0 and were merged into
/// one call with a spliced name and arguments. Keep the open call instead and
/// only reuse it for argument-only fragments; a new id or a new name opens a
/// new internal index.
#[derive(Debug, Default)]
struct ToolCallIndexer {
    next: usize,
    open: Option<OpenToolCall>,
    explicit: std::collections::HashMap<usize, OpenToolCall>,
    used: std::collections::HashSet<usize>,
}

#[derive(Debug, Clone, Default)]
struct OpenToolCall {
    index: usize,
    id: Option<String>,
    name: Option<String>,
}

impl ToolCallIndexer {
    fn assign(&mut self, call: &ToolCallDelta) -> usize {
        let id = call
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let name = call
            .function
            .as_ref()
            .and_then(|function| function.name.as_deref())
            .map(str::trim)
            .filter(|name| !name.is_empty());
        match call.index {
            Some(index) => self.assign_explicit(index, id, name),
            None => self.assign_implicit(id, name),
        }
    }

    /// An engine-supplied index is authoritative unless a *different*
    /// non-empty id appears on it: some engines reuse one index for several
    /// calls, and merging those would pair a result with the wrong call.
    fn assign_explicit(
        &mut self,
        engine_index: usize,
        id: Option<&str>,
        name: Option<&str>,
    ) -> usize {
        if let Some(existing) = self.explicit.get(&engine_index) {
            let id_conflict = matches!(
                (existing.id.as_deref(), id),
                (Some(existing), Some(incoming)) if existing != incoming
            );
            // Some engines reuse both one id and one index. A different
            // function name is a different call; merging would splice the
            // names and arguments together.
            let name_conflict = matches!(
                (existing.name.as_deref(), name),
                (Some(existing), Some(incoming)) if existing != incoming
            );
            if !id_conflict && !name_conflict {
                let index = existing.index;
                if let Some(entry) = self.explicit.get_mut(&engine_index) {
                    if entry.id.is_none() {
                        entry.id = id.map(str::to_string);
                    }
                    if entry.name.is_none() {
                        entry.name = name.map(str::to_string);
                    }
                    self.open = Some(entry.clone());
                }
                return index;
            }
        }
        let index = if self.used.contains(&engine_index) {
            self.allocate_index()
        } else {
            self.used.insert(engine_index);
            self.next = self.next.max(engine_index + 1);
            engine_index
        };
        self.explicit.insert(
            engine_index,
            OpenToolCall {
                index,
                id: id.map(str::to_string),
                name: name.map(str::to_string),
            },
        );
        self.open = self.explicit.get(&engine_index).cloned();
        index
    }

    fn assign_implicit(&mut self, id: Option<&str>, name: Option<&str>) -> usize {
        let reuse = match &self.open {
            None => false,
            Some(open) => match (id, name) {
                (Some(id), Some(name)) => {
                    open.id.as_deref() == Some(id)
                        && (open.name.as_deref().is_none_or(str::is_empty)
                            || open.name.as_deref() == Some(name))
                }
                (Some(id), None) => open.id.as_deref() == Some(id),
                // A name opens a new call, except for the common split where
                // an id-bearing chunk arrived before its function name.
                (None, Some(_)) => {
                    open.id.is_some() && open.name.as_deref().is_none_or(str::is_empty)
                }
                // Arguments-only fragments continue the open call.
                (None, None) => true,
            },
        };
        if reuse {
            let index = self.open.as_ref().expect("open call").index;
            if let Some(open) = self.open.as_mut() {
                if open.id.is_none() {
                    open.id = id.map(str::to_string);
                }
                if open.name.is_none() {
                    open.name = name.map(str::to_string);
                }
            }
            return index;
        }
        let index = self.allocate_index();
        self.open = Some(OpenToolCall {
            index,
            id: id.map(str::to_string),
            name: name.map(str::to_string),
        });
        index
    }

    fn allocate_index(&mut self) -> usize {
        while self.used.contains(&self.next) {
            self.next += 1;
        }
        let index = self.next;
        self.used.insert(index);
        self.next = index + 1;
        index
    }
}

/// An in-stream error. Engines send either `{"error":{"message":"..."}}` or
/// `{"error":"..."}`; both must abort the turn with the server's message
/// instead of becoming an empty answer.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ApiError {
    Object {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        r#type: Option<String>,
        #[serde(default)]
        code: Option<Value>,
    },
    Text(String),
}

impl ApiError {
    fn into_message(self) -> String {
        match self {
            ApiError::Text(message) => message,
            ApiError::Object {
                message,
                r#type,
                code,
            } => message
                .or_else(|| {
                    code.as_ref().and_then(|code| match code {
                        Value::String(text) => Some(text.clone()),
                        Value::Number(number) => Some(number.to_string()),
                        _ => None,
                    })
                })
                .or(r#type)
                .unwrap_or_else(|| "provider error".into()),
        }
    }
}

fn reasoning_text(delta: &Delta, configured: Option<&str>) -> Option<String> {
    if let Some(field) = configured {
        let value = match field {
            "reasoning" => delta.reasoning.clone(),
            _ => delta.reasoning_content.clone(),
        };
        if value.is_some() {
            return value;
        }
    }
    delta
        .reasoning_content
        .clone()
        .or_else(|| delta.reasoning.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelConfig;

    fn provider() -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(&ModelConfig::default(), "test-model").unwrap()
    }

    fn usage_of(chunk: &str) -> Option<Usage> {
        let chunk: Chunk = serde_json::from_str(chunk).unwrap();
        usage_from(
            chunk.usage,
            chunk.timings,
            chunk.truncated,
            chunk.context_shift,
        )
    }

    #[test]
    fn engine_truncation_signals_reach_usage() {
        let usage = usage_of(
            r#"{"choices":[],"truncated":true,"usage":{"prompt_tokens":2048,"completion_tokens":1,"total_tokens":2049}}"#,
        )
        .unwrap();
        assert_eq!(usage.truncated, Some(true));
        let usage = usage_of(
            r#"{"choices":[],"timings":{"n_shift":3},"usage":{"prompt_tokens":2048,"completion_tokens":1,"total_tokens":2049}}"#,
        )
        .unwrap();
        assert_eq!(usage.context_shift, Some(3));
        let usage = usage_of(
            r#"{"choices":[],"context_shift":2,"usage":{"prompt_tokens":10,"completion_tokens":1,"total_tokens":11}}"#,
        )
        .unwrap();
        assert_eq!(usage.context_shift, Some(2));
    }

    #[test]
    fn cache_and_speed_are_read_from_both_engine_shapes() {
        // OpenAI-standard details (vLLM, newer llama.cpp).
        let usage = usage_of(
            r#"{"choices":[],"usage":{"prompt_tokens":1000,"completion_tokens":50,"total_tokens":1050,"prompt_tokens_details":{"cached_tokens":900}}}"#,
        )
        .unwrap();
        assert_eq!(usage.cached_tokens, Some(900));
        assert_eq!(usage.cache_percent(), Some(90));
        // llama.cpp timings, possibly on a chunk without usage.
        let usage = usage_of(
            r#"{"choices":[],"timings":{"cache_n":236,"prompt_n":12,"prompt_ms":30.9,"predicted_n":35,"predicted_ms":661.0,"predicted_per_second":52.94}}"#,
        )
        .unwrap();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.cached_tokens, Some(236));
        assert_eq!(usage.prompt_ms, Some(31));
        assert!((usage.tokens_per_sec.unwrap() - 52.94).abs() < 0.01);
        // Plain chunks carry nothing.
        assert!(usage_of(r#"{"choices":[]}"#).is_none());
    }

    #[test]
    fn leading_system_messages_are_merged_once() {
        let system = |text: &str| ChatMessage::System {
            content: text.into(),
        };
        let messages = vec![
            system("base"),
            system("summary"),
            ChatMessage::User {
                content: "hi".into(),
            },
            system("state"),
        ];
        let merged = merge_leading_system(&messages);
        assert_eq!(merged.len(), 2);
        assert!(
            matches!(&merged[0], ChatMessage::System { content } if content == "base\n\nsummary\n\nstate")
        );
        assert!(matches!(&merged[1], ChatMessage::User { content } if content == "hi"));
        // A system message that follows a non-system one is moved to the front.
        let merged = merge_leading_system(&messages[2..]);
        assert_eq!(merged.len(), 2);
        assert!(matches!(&merged[0], ChatMessage::System { content } if content == "state"));
        assert!(matches!(&merged[1], ChatMessage::User { content } if content == "hi"));
    }

    #[test]
    fn body_has_no_system_message_after_the_first() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::User {
                    content: "hi".into(),
                },
                ChatMessage::System {
                    content: "summary".into(),
                },
                ChatMessage::Assistant {
                    content: Some("ok".into()),
                    tool_calls: vec![],
                },
                ChatMessage::System {
                    content: "state".into(),
                },
            ],
            tools: vec![],
            tool_choice: None,
            parallel_tool_calls: None,
            temperature: 0.2,
            top_p: 0.95,
            max_tokens: 128,
            stop: vec![],
            extra_body: None,
            include_usage: false,
            reasoning_field: None,
        };
        let body = build_body(&req, false).unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "summary\n\nstate");
        assert!(
            messages[1..]
                .iter()
                .all(|message| message["role"] != "system")
        );
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
    }

    #[test]
    fn body_serializes_tool_calls_and_stream_options() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![
                ChatMessage::System {
                    content: "sys".into(),
                },
                ChatMessage::Assistant {
                    content: None,
                    tool_calls: vec![super::super::types::ToolCall {
                        id: "call_1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"a.rs"}),
                    }],
                },
                ChatMessage::Tool {
                    call_id: "call_1".into(),
                    content: "ok".into(),
                },
            ],
            tools: vec![],
            tool_choice: None,
            parallel_tool_calls: None,
            temperature: 0.2,
            top_p: 0.95,
            max_tokens: 128,
            stop: vec!["<|im_end|>".into()],
            extra_body: None,
            include_usage: true,
            reasoning_field: None,
        };
        let body = build_body(&req, true).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        assert_eq!(body["stop"][0], "<|im_end|>");
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["name"],
            "read"
        );
        assert_eq!(
            body["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a.rs\"}"
        );
    }

    #[test]
    fn http_errors_are_prescriptive() {
        let err = classify_http(400, "the prompt is too long for context", "m").unwrap();
        assert_eq!(err.class, ErrorClass::ContextTooSmall);
        assert!(err.message.contains("Raise the engine context"));
        let err = classify_http(401, "unauthorized", "m").unwrap();
        assert_eq!(err.class, ErrorClass::Auth);
        let err = classify_http(400, "tools are not supported without --jinja", "m").unwrap();
        assert!(err.message.contains("--jinja"));
    }

    #[test]
    fn reasoning_field_is_respected() {
        let delta = Delta {
            reasoning_content: Some("a".into()),
            reasoning: Some("b".into()),
            ..Delta::default()
        };
        assert_eq!(
            reasoning_text(&delta, Some("reasoning")).as_deref(),
            Some("b")
        );
        assert_eq!(
            reasoning_text(&delta, Some("reasoning_content")).as_deref(),
            Some("a")
        );
        assert_eq!(reasoning_text(&delta, None).as_deref(), Some("a"));
    }

    #[test]
    fn stream_options_detection() {
        assert!(mentions_stream_options("unknown field stream_options"));
        assert!(!mentions_stream_options("bad temperature"));
    }

    #[test]
    fn provider_url_is_built_once() {
        let p = provider();
        assert_eq!(p.url(), "http://127.0.0.1:8080/v1/chat/completions");
    }

    #[test]
    fn endpoint_with_v1_suffix_is_not_doubled() {
        let config = ModelConfig {
            endpoint: "http://model-host:8088/v1/".into(),
            ..ModelConfig::default()
        };
        let p = OpenAiCompatProvider::new(&config, "spark26").unwrap();
        assert_eq!(p.url(), "http://model-host:8088/v1/chat/completions");
        assert_eq!(p.base_url(), "http://model-host:8088/v1");
    }
}

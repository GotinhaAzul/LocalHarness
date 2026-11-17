//! The wire schema for agent events, shared by `tot --print --json` and the
//! `tot serve` SSE stream.
//!
//! [`event_name`] is an exhaustive match on [`AgentEvent`], so adding a
//! variant fails to compile until it has a name, and both frontends pick the
//! name up from here. [`event_json`] always carries that name in `"type"`.

use serde_json::{Value, json};

use crate::agent::AgentEvent;

/// Emitted by a frontend when the user (Ctrl+C) or a client (`/cancel`)
/// asks to stop a turn. It is not an [`AgentEvent`]: the turn's own `done`
/// event follows with status `Cancelled`.
pub const CANCELLED: &str = "cancelled";

/// The stable `"type"` of one event.
pub fn event_name(event: &AgentEvent) -> &'static str {
    match event {
        AgentEvent::TurnStarted { .. } => "started",
        AgentEvent::State(_) => "state",
        AgentEvent::TextDelta(_) => "text",
        AgentEvent::ReasoningDelta(_) => "reasoning",
        AgentEvent::ToolStarted { .. } => "tool_started",
        AgentEvent::ToolFinished { .. } => "tool_finished",
        AgentEvent::Usage(_) => "usage",
        AgentEvent::Context { .. } => "context",
        AgentEvent::WindowDetected { .. } => "window_detected",
        AgentEvent::Compacted { .. } => "compacted",
        AgentEvent::Provenance { .. } => "provenance",
        AgentEvent::HunkReview { .. } => "hunk_review",
        AgentEvent::Notice(_) => "notice",
        AgentEvent::Discard { .. } => "discard",
        AgentEvent::Guard { .. } => "guard",
        AgentEvent::Error { .. } => "error",
        AgentEvent::TurnDone(_) => "done",
    }
}

/// One event as a JSON object whose `"type"` is [`event_name`].
pub fn event_json(event: &AgentEvent) -> Value {
    let mut value = match event {
        AgentEvent::TurnStarted {
            model,
            profile,
            tool_mode,
        } => json!({"model": model, "profile": profile, "tool_mode": tool_mode}),
        AgentEvent::State(state) => json!({"state": format!("{state:?}")}),
        AgentEvent::TextDelta(text) | AgentEvent::ReasoningDelta(text) => json!({"text": text}),
        AgentEvent::ToolStarted {
            call_id,
            name,
            summary,
        } => json!({"call_id": call_id, "name": name, "summary": summary}),
        AgentEvent::ToolFinished {
            call_id,
            name,
            is_error,
            duration_ms,
            preview,
            artifact,
        } => json!({
            "call_id": call_id,
            "name": name,
            "is_error": is_error,
            "duration_ms": duration_ms,
            "preview": preview,
            "artifact": artifact,
        }),
        AgentEvent::Usage(usage) => json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "cached_tokens": usage.cached_tokens,
            "cache_percent": usage.cache_percent(),
            "reasoning_tokens": usage.reasoning_tokens,
        }),
        AgentEvent::Context { used, window } => json!({"used": used, "window": window}),
        AgentEvent::WindowDetected { window, source } => {
            json!({"window": window, "source": source})
        }
        AgentEvent::Compacted {
            summary,
            tokens_before,
        } => json!({"summary": summary, "tokens_before": tokens_before}),
        AgentEvent::Provenance {
            call_id,
            origin,
            warning,
            approval_required,
        } => json!({
            "call_id": call_id,
            "origin": origin,
            "warning": warning,
            "approval_required": approval_required,
        }),
        AgentEvent::HunkReview { id, hunks } => json!({
            "id": id,
            "hunks": hunks
                .iter()
                .map(|hunk| json!({
                    "file": hunk.file,
                    "start_line": hunk.start_line,
                    "end_line": hunk.end_line,
                    "added": hunk.added,
                    "removed": hunk.removed,
                    "rationale": hunk.rationale,
                    "text": hunk.text,
                }))
                .collect::<Vec<_>>(),
        }),
        AgentEvent::Notice(message) => json!({"message": message}),
        AgentEvent::Discard { chars } => json!({"chars": chars}),
        AgentEvent::Guard { code, message } => json!({"code": code, "message": message}),
        AgentEvent::Error {
            code,
            message,
            recoverable,
        } => json!({"code": code, "message": message, "recoverable": recoverable}),
        AgentEvent::TurnDone(outcome) => json!({"status": format!("{:?}", outcome.status)}),
    };
    value["type"] = json!(event_name(event));
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentState, TurnOutcome, TurnStatus};

    #[test]
    fn the_type_field_is_always_the_event_name() {
        let events = [
            AgentEvent::TextDelta("hi".into()),
            AgentEvent::State(AgentState::StreamModel),
            AgentEvent::Guard {
                code: "loop_guard".into(),
                message: "m".into(),
            },
            AgentEvent::Provenance {
                call_id: "call_0001".into(),
                origin: "tool:read".into(),
                warning: None,
                approval_required: false,
            },
            AgentEvent::HunkReview {
                id: 3,
                hunks: Vec::new(),
            },
            AgentEvent::TurnDone(TurnOutcome {
                status: TurnStatus::Done,
                error: None,
            }),
        ];
        for event in &events {
            let value = event_json(event);
            assert_eq!(value["type"], event_name(event));
        }
        assert_eq!(
            event_json(&AgentEvent::TextDelta("hi".into())),
            json!({"type": "text", "text": "hi"})
        );
        assert_eq!(
            event_json(&AgentEvent::TurnDone(TurnOutcome {
                status: TurnStatus::Cancelled,
                error: None,
            })),
            json!({"type": "done", "status": "Cancelled"})
        );
    }
}

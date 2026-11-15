//! The pre-dispatch decision seam: one place that says whether a repaired
//! tool call may run.
//!
//! [`decide`] returns a [`Decision`] (allow, ask with the exact subject, or
//! deny with a code and reason) before any harness gate runs; a `Deny` stops
//! the call at once. An `Ask` is resolved by [`confirm`] after the harness
//! gates pass, so the user is never asked about a call that would fail
//! anyway. Additional inputs (lifecycle hooks, provenance) belong inside
//! [`decide`] so the loop keeps a single decision point.

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::policy::{AskAnswer, AskRequest, CallFacts, Decision};
use crate::tools::{Tool, ToolResult};

use super::Core;

/// Lines of diff shown in an ask prompt.
const PREVIEW_LINES: usize = 40;

/// Decide one call against the permission policy and the current mode.
pub fn decide(core: &Core, tool: &dyn Tool, args: &Value) -> Decision {
    let facts = CallFacts::new(tool.name(), tool.read_only(), args, &core.paths.cwd);
    match core.permissions.decide(&facts) {
        Decision::Ask(mut request) => {
            request.preview =
                crate::policy::preview(tool.name(), args, &core.paths.cwd, PREVIEW_LINES);
            Decision::Ask(request)
        }
        other => other,
    }
}

/// Resolve an `Ask`: through the frontend asker (TUI), else a frontend
/// approval gate (ACP asks its client after this), else deny because nobody
/// can answer. An approval is remembered for exactly this call.
pub async fn confirm(
    core: &Core,
    request: AskRequest,
    cancel: &CancellationToken,
) -> Result<(), ToolResult> {
    let Some(asker) = core.permissions.asker() else {
        if core.approver.is_some() {
            return Ok(());
        }
        return Err(ToolResult::err(
            "DENIED",
            format!(
                "{} ({}) needs approval{} but this run cannot ask (mode {}, no interactive frontend). The call did not run. The user can add an allow rule under [permissions], or rerun with --mode accept-edits or --yolo.",
                request.tool,
                request.subject,
                request
                    .rule
                    .as_deref()
                    .map(|id| format!(" (rule {id})"))
                    .unwrap_or_default(),
                core.permissions.mode().label(),
            ),
        ));
    };
    let key = request.key.clone();
    let tool = request.tool.clone();
    let subject = request.subject.clone();
    let answer = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ToolResult::err("cancelled", "cancelled while waiting for approval")),
        answer = asker.ask(request) => answer,
    };
    match answer {
        AskAnswer::Allow => {
            core.permissions.approve(&key);
            Ok(())
        }
        AskAnswer::Deny => Err(ToolResult::err(
            "DENIED",
            format!(
                "the user declined {tool} ({subject}). The call did not run; try a different approach or ask the user what they want instead."
            ),
        )),
    }
}

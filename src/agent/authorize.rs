//! The pre-dispatch decision seam: one place that says whether a repaired
//! tool call may run.
//!
//! [`decide`] combines every source that may refuse a call before any harness
//! gate runs, in this order:
//!
//! 1. the permission policy (mode, rules, plan mode): allow, ask with the
//!    exact subject, or deny with a code and reason;
//! 2. `pre_tool` lifecycle hooks, which may block or add context;
//! 3. instruction provenance, which gates a sensitive call while an untrusted
//!    warning is pending.
//!
//! A deny from any source wins and stops the call at once; a policy deny
//! skips the hooks entirely. Hooks and provenance can only deny or add
//! context: they never turn an `Ask` into an `Allow` or re-enable a tool.
//! What still needs an answer (a policy `Ask`, a provenance approval) is
//! resolved by [`confirm`] and [`confirm_provenance`] only after the harness
//! gates pass, so the user is never asked about, and `/approve` is never
//! spent on, a call that would fail anyway.

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::policy::{AskAnswer, AskRequest, CallFacts, Decision};
use crate::provenance::{self, Gate, Requirement};
use crate::session::ToolCall;
use crate::tools::{Tool, ToolResult};

use super::Core;

/// Lines of diff shown in an ask prompt.
const PREVIEW_LINES: usize = 40;

/// Everything [`decide`] learned about one call.
#[derive(Debug)]
pub struct Verdict {
    /// `Deny` from any source, else the policy's `Allow` or `Ask`.
    pub decision: Decision,
    /// Messages to show in the transcript (hook warnings, blocks, denials).
    pub notices: Vec<String>,
    /// Text to prepend to the tool result: hook warnings and hook context.
    pub notes: Vec<String>,
    /// A provenance approval still to resolve after the harness gates.
    pub provenance: Option<Requirement>,
}

impl Verdict {
    fn new(decision: Decision) -> Self {
        Self {
            decision,
            notices: Vec::new(),
            notes: Vec::new(),
            provenance: None,
        }
    }
}

/// The permission policy alone, for one call in the current mode.
pub fn policy_decision(core: &Core, tool: &dyn Tool, args: &Value) -> Decision {
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

/// Decide one call: policy, then `pre_tool` hooks, then provenance.
pub async fn decide(core: &Core, tool: &dyn Tool, call: &ToolCall, args: &Value) -> Verdict {
    let mut verdict = Verdict::new(policy_decision(core, tool, args));
    if matches!(verdict.decision, Decision::Deny { .. }) {
        return verdict;
    }

    // Hooks see the repaired arguments. They may block or add context; any
    // output field that tries to change tools, scope, or policy is ignored.
    let pre = crate::hooks::pre_tool(&super::run::hook_env(core), &call.name, args).await;
    for warning in pre.warnings {
        verdict.notices.push(format!("[hook] {warning}"));
        verdict.notes.push(format!("[hook warning] {warning}"));
    }
    for context in pre.contexts {
        verdict.notes.push(format!(
            "[hook context from {}]\n{}",
            context.hook, context.text
        ));
    }
    if let Some(block) = pre.blocked {
        let message = format!(
            "hook {:?} blocked this {} call: {}. The call did not run.",
            block.hook, call.name, block.reason
        );
        verdict.notices.push(message.clone());
        verdict.decision = Decision::Deny {
            code: "hook_block".into(),
            message,
        };
        return verdict;
    }

    // Provenance: an untrusted warning gates the next sensitive action. When
    // nothing can approve it, deny now; otherwise resolve after the gates.
    if let Some(required) = provenance::assess(core, call, args) {
        if required.certain_denial(core) {
            if let Gate::Deny { code, message } =
                provenance::resolve(core, call, args, required).await
            {
                verdict.notices.push(message.clone());
                verdict.decision = Decision::Deny { code, message };
            }
            return verdict;
        }
        verdict.provenance = Some(required);
    }
    verdict
}

/// Resolve a provenance requirement from [`decide`] once the harness gates
/// passed: spend the user's `/approve` or ask the frontend approver.
pub async fn confirm_provenance(
    core: &Core,
    call: &ToolCall,
    args: &Value,
    required: Requirement,
) -> Result<(), ToolResult> {
    match provenance::resolve(core, call, args, required).await {
        Gate::Allow => Ok(()),
        Gate::Deny { code, message } => Err(ToolResult::err(&code, message)),
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

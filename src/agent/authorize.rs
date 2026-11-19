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
    /// The layer that decided (see [`crate::policy::LoggedDecision::layer`]).
    pub layer: String,
}

impl Verdict {
    fn new(decision: Decision, layer: String) -> Self {
        Self {
            decision,
            notices: Vec::new(),
            notes: Vec::new(),
            provenance: None,
            layer,
        }
    }
}

/// Record a call's permission outcome in the session's decision log.
pub fn log_decision(
    core: &Core,
    call: &ToolCall,
    tool: &dyn Tool,
    args: &Value,
    allowed: bool,
    layer: impl Into<String>,
) {
    let facts = CallFacts::new(tool.name(), tool.read_only(), args, &core.paths.cwd);
    core.permissions.record(crate::policy::LoggedDecision {
        call_id: call.id.clone(),
        tool: call.name.clone(),
        subject: facts.subject(),
        outcome: if allowed { "allowed" } else { "denied" },
        layer: layer.into(),
        read_only: facts.effect == crate::policy::Effect::ReadOnly,
    });
}

/// The file a call targets, as path rules and hunk review must see it: a
/// hash-mode `patch` names `path#snapshot`, and the tag is not part of the
/// filename (a deny rule on `.env` must match `.env#<snapshot>`).
pub fn target_path<'a>(core: &Core, tool: &str, raw: &'a str) -> &'a str {
    if tool == "patch" {
        crate::tools::patch::gate_path(raw, core.model_profile.edit_mode)
    } else {
        raw
    }
}

/// The permission policy alone, for one call in the current mode, and the
/// layer that decided it.
pub fn policy_decision(core: &Core, tool: &dyn Tool, args: &Value) -> (Decision, String) {
    let normalized = args
        .get("path")
        .and_then(Value::as_str)
        .map(|raw| (raw, target_path(core, tool.name(), raw)))
        .filter(|(raw, target)| raw != target)
        .map(|(_, target)| {
            let mut args = args.clone();
            args["path"] = Value::String(target.to_string());
            args
        });
    let args = normalized.as_ref().unwrap_or(args);
    let facts = CallFacts::new(tool.name(), tool.read_only(), args, &core.paths.cwd);
    match core.permissions.decide_explained(&facts) {
        (Decision::Ask(mut request), layer) => {
            request.preview =
                crate::policy::preview(tool.name(), args, &core.paths.cwd, PREVIEW_LINES);
            let times = crate::policy::remember::approvals(&core.paths, &request.key);
            request.suggestion =
                crate::policy::remember::suggest(&request.tool, &request.subject, times)
                    .map(Box::new);
            (Decision::Ask(request), layer)
        }
        other => other,
    }
}

/// Decide one call: policy, then `pre_tool` hooks, then provenance.
pub async fn decide(core: &Core, tool: &dyn Tool, call: &ToolCall, args: &Value) -> Verdict {
    let (decision, layer) = policy_decision(core, tool, args);
    let mut verdict = Verdict::new(decision, layer);
    match &mut verdict.decision {
        Decision::Deny { .. } => {
            log_decision(core, call, tool, args, false, verdict.layer.clone());
            return verdict;
        }
        Decision::Ask(request) => request.call_id = call.id.clone(),
        Decision::Allow => log_decision(core, call, tool, args, true, verdict.layer.clone()),
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
        verdict.layer = format!("hook {}", block.hook);
        log_decision(core, call, tool, args, false, verdict.layer.clone());
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
                verdict.layer = "provenance".into();
                log_decision(core, call, tool, args, false, "provenance");
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
    tool: &dyn Tool,
    args: &Value,
    required: Requirement,
) -> Result<(), ToolResult> {
    match provenance::resolve(core, call, args, required).await {
        Gate::Allow => {
            log_decision(core, call, tool, args, true, "provenance approval");
            Ok(())
        }
        Gate::Deny { code, message } => {
            log_decision(core, call, tool, args, false, "provenance");
            Err(ToolResult::err(&code, message))
        }
    }
}

/// Update the logged outcome of the call an ask was about.
fn log_answer(core: &Core, request: &AskRequest, allowed: bool, layer: impl Into<String>) {
    core.permissions.record(crate::policy::LoggedDecision {
        call_id: request.call_id.clone(),
        tool: request.tool.clone(),
        subject: request.subject.clone(),
        outcome: if allowed { "allowed" } else { "denied" },
        layer: layer.into(),
        read_only: false,
    });
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
            log_answer(core, &request, true, "frontend");
            return Ok(());
        }
        log_answer(core, &request, false, "no one could answer");
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
    let asked = request.clone();
    let answer = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ToolResult::err("cancelled", "cancelled while waiting for approval")),
        answer = asker.ask(request) => answer,
    };
    match answer {
        AskAnswer::Allow => {
            core.permissions.approve(&key);
            crate::policy::remember::record_approval(&core.paths, &key);
            log_answer(core, &asked, true, "you");
            Ok(())
        }
        AskAnswer::AllowAlways => {
            core.permissions.approve(&key);
            crate::policy::remember::record_approval(&core.paths, &key);
            let layer = match &asked.suggestion {
                Some(suggestion) => {
                    match crate::config::Config::save_permission_rule(
                        &core.paths.config_file(),
                        &suggestion.rule,
                    ) {
                        Ok(()) => format!(
                            "you (saved rule {})",
                            suggestion.rule.id.as_deref().unwrap_or("?")
                        ),
                        Err(error) => {
                            tracing::warn!(%error, "cannot save the permission rule");
                            "you (rule not saved)".to_string()
                        }
                    }
                }
                None => "you".to_string(),
            };
            log_answer(core, &asked, true, layer);
            Ok(())
        }
        AskAnswer::Deny => {
            log_answer(core, &asked, false, "you");
            Err(ToolResult::err(
                "DENIED",
                format!(
                    "the user declined {tool} ({subject}). The call did not run; try a different approach or ask the user what they want instead."
                ),
            ))
        }
    }
}

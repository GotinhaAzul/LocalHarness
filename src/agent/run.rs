//! One turn: stream the model, run tool batches, enforce guards, settle.

use std::collections::{BTreeMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::Instant;

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::context::compact::{COMPACTION_BUDGET_CHARS, FindingSummary, build_summary};
use crate::context::{self, BATCH_CAP_CHARS, StaticPrompt, TokenBudget, cap_result};
use crate::provider::{ChatMessage, ChatRequest, ErrorClass, ProviderError, StreamEvent, Usage};
use crate::session::{
    AssistantData, CompactionData, CustomData, EntryKind, ToolCall, ToolResultData, UserData,
};
use crate::tools::{ToolResult, display_path};

use super::gates::{self, LoopDecision, LoopGuard, SettleDecision};
use super::limits::{RunProgress, StopReason};
use super::{AgentEvent, AgentState, Core};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStatus {
    Done,
    Cancelled,
    Budget,
    Stalled,
    /// The server dropped part of the prompt or the rendered prompt cannot
    /// fit the effective window. The turn ended without replayable output.
    ContextTruncated,
    Error,
    Blocked,
}

#[derive(Debug, Clone)]
pub struct TurnOutcome {
    pub status: TurnStatus,
    pub error: Option<(String, String)>,
}

impl TurnOutcome {
    fn ok(status: TurnStatus) -> Self {
        Self {
            status,
            error: None,
        }
    }

    fn error(code: &str, message: impl Into<String>) -> Self {
        Self {
            status: TurnStatus::Error,
            error: Some((code.to_string(), message.into())),
        }
    }

    /// A turn that never started because the budget cannot fit. The headless
    /// exit code is 2, like other budget stops, but the error code is exact.
    fn budget(code: &str, message: impl Into<String>) -> Self {
        Self {
            status: TurnStatus::Budget,
            error: Some((code.to_string(), message.into())),
        }
    }
}

/// Budget and static prompt for one turn. Resolved after endpoint bring-up so
/// a deferred `model.start_command` cannot leave the turn on a stale window.
struct TurnContext {
    budget: TokenBudget,
    static_prompt: StaticPrompt,
    tokenize: bool,
    /// A window or Ollama-default warning discovered by the refresh.
    notice: Option<String>,
}

/// Run one turn to settlement. Appends everything to the session branch;
/// streams UI events.
pub async fn run_turn(
    core: std::sync::Arc<Core>,
    user_text: String,
    tx: UnboundedSender<AgentEvent>,
    cancel: CancellationToken,
) -> TurnOutcome {
    // A background `model.start_command` may still be bringing the endpoint
    // up. Wait for it instead of racing the first request; cancellation wins.
    let startup_pending = core.startup.as_ref().is_some_and(|gate| gate.pending());
    if let Some(gate) = &core.startup {
        if gate.pending() {
            let _ = tx.send(AgentEvent::Notice(
                "waiting for the model endpoint (model.start_command)".into(),
            ));
        }
        if gate.ready(&cancel).await.is_none() {
            let outcome = TurnOutcome::ok(TurnStatus::Cancelled);
            let _ = tx.send(AgentEvent::State(AgentState::Cancelled));
            let _ = tx.send(AgentEvent::TurnDone(outcome.clone()));
            return outcome;
        }
    }

    let _ = tx.send(AgentEvent::TurnStarted {
        model: core.provider.model().to_string(),
        profile: core.config.profile_name().to_string(),
    });

    // Resolve the window and tokenizer after bring-up. A deferred startup
    // probe runs before the server exists, so the build-time values can be
    // fallbacks; this refresh is what keeps the first turn from budgeting
    // against the wrong window.
    let turn = prepare_turn_context(&core, startup_pending).await;

    if let Some(message) = static_prompt_warning(&turn.static_prompt, &turn.budget) {
        let _ = tx.send(AgentEvent::Notice(message));
    }
    if let Some(notice) = &turn.notice {
        let _ = tx.send(AgentEvent::Notice(notice.clone()));
    } else if let Some(notice) = &core.window_notice {
        let tx = tx.clone();
        core.window_notice_sent.call_once(|| {
            let _ = tx.send(AgentEvent::Notice(notice.clone()));
        });
    }
    if let Some((code, message)) = static_prompt_failure(&turn.static_prompt, &turn.budget) {
        let _ = tx.send(AgentEvent::Error {
            code: code.clone(),
            message: message.clone(),
            recoverable: false,
        });
        let outcome = TurnOutcome::budget(&code, message);
        let _ = tx.send(AgentEvent::State(AgentState::Error));
        let _ = tx.send(AgentEvent::TurnDone(outcome.clone()));
        return outcome;
    }

    // `@path` mentions attach file snapshots, within a quarter of the
    // usable prompt so one message cannot crowd out the conversation.
    let attachment_budget = turn.budget.chars_for(turn.budget.usable_prompt() / 4);
    let mentions = super::mentions::resolve(&core.tool_context(), &user_text, attachment_budget);
    if !mentions.attachments.is_empty() {
        let names: Vec<&str> = mentions
            .attachments
            .iter()
            .map(|attachment| attachment.path.as_str())
            .collect();
        let _ = tx.send(AgentEvent::Notice(format!("attached {}", names.join(", "))));
    }
    for skipped in &mentions.skipped {
        let _ = tx.send(AgentEvent::Notice(format!("not attached: {skipped}")));
    }

    if let Err(err) = append_user_with(&core.session, &user_text, mentions.attachments) {
        let message = format!("cannot append user message: {err}");
        let _ = tx.send(AgentEvent::Error {
            code: "session_write".into(),
            message: message.clone(),
            recoverable: false,
        });
        return TurnOutcome::error("session_write", message);
    }

    let turn_start = core.session.lock().expect("session lock").active_leaf_id();
    let outcome = run_loop(&core, &tx, &cancel, turn_start, turn).await;
    if let Ok(mut session) = core.session.lock()
        && let Err(err) = session.fsync()
    {
        let _ = tx.send(AgentEvent::Notice(format!("session fsync failed: {err}")));
    }
    let _ = tx.send(AgentEvent::State(match outcome.status {
        TurnStatus::Done => AgentState::Done,
        TurnStatus::Cancelled => AgentState::Cancelled,
        TurnStatus::Error | TurnStatus::ContextTruncated => AgentState::Error,
        _ => AgentState::Done,
    }));
    let _ = tx.send(AgentEvent::TurnDone(outcome.clone()));
    outcome
}

/// The configured window, if the model profile or `[model]` pins one.
fn configured_window(core: &Core) -> Option<u32> {
    let (_, profile) = core.config.model_profile();
    profile.context_window.or(core.config.model.context_window)
}

/// Resolve the budget and static prompt for this turn. When endpoint bring-up
/// was deferred or the window is still a guess, probe now, after the server is
/// up, and re-measure the static prompt with the calibrated tokenizer ratio.
async fn prepare_turn_context(core: &Core, startup_pending: bool) -> TurnContext {
    let mut budget = core.budget.clone();
    let (effective, source) = core.effective_window();
    if effective > 0 && effective != budget.context_window {
        budget = budget.resized_to(effective, core.config.model.max_output_tokens);
    }
    let known_window = core
        .server_window
        .load(std::sync::atomic::Ordering::Relaxed);
    if known_window > 0 && known_window < budget.context_window {
        budget = budget.shrunk_to(known_window);
    }
    let refresh = startup_pending || !source.is_reliable();
    let mut static_prompt = core.static_prompt;
    let mut tokenize = core.tokenize;
    let mut notice = None;
    if refresh {
        let configured = configured_window(core);
        let truth = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            crate::provider::probe::probe_context_truth(core.provider.as_ref()),
        )
        .await
        .ok()
        .flatten()
        .filter(|truth| truth.is_known());
        if let Some(truth) = truth {
            let window = configured
                .map(|configured| configured.min(truth.effective))
                .unwrap_or(truth.effective);
            if window > 0 {
                budget = budget.resized_to(window, core.config.model.max_output_tokens);
                core.remember_window(window);
                if core.window_notice.is_none() {
                    notice = crate::app::window_notice(Some(&truth), configured);
                }
            }
        }
        let sampled = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            crate::provider::probe::token_count(core.provider.as_ref(), &core.system_prompt),
        )
        .await
        .ok()
        .flatten();
        if let Some(tokens) = sampled {
            tokenize = true;
            if tokens > 0 {
                budget.calibrate(
                    core.system_prompt.chars().count(),
                    tokens.min(u32::MAX as usize) as u32,
                );
            }
        }
        static_prompt = crate::app::measure_static_prompt(
            &budget,
            &core.system_prompt,
            &core.tool_specs,
            Some(core.repo_brief.as_str()),
        );
    }
    TurnContext {
        budget,
        static_prompt,
        tokenize,
        notice,
    }
}

async fn run_loop(
    core: &std::sync::Arc<Core>,
    tx: &UnboundedSender<AgentEvent>,
    cancel: &CancellationToken,
    turn_start: Option<String>,
    turn: TurnContext,
) -> TurnOutcome {
    let mut progress = RunProgress::new(core.limits.clone());
    let mut budget = turn.budget;
    let detected = core
        .detected_window
        .load(std::sync::atomic::Ordering::Relaxed);
    if detected > 0 && detected != budget.context_window {
        budget = budget.resized_to(detected, core.config.model.max_output_tokens);
    }
    // Probe the served window once per turn while it is still a guess.
    let mut window_probe_pending = !core.window_source.is_reliable() && detected == 0;
    let known_window = core
        .server_window
        .load(std::sync::atomic::Ordering::Relaxed);
    if known_window > 0 && known_window < budget.context_window {
        budget = budget.shrunk_to(known_window);
    }
    if let Some((code, message)) = static_prompt_failure(&turn.static_prompt, &budget) {
        append_error(&core.session, &code, &message, false);
        let _ = tx.send(AgentEvent::Error {
            code: code.clone(),
            message: message.clone(),
            recoverable: false,
        });
        return TurnOutcome::budget(&code, message);
    }
    let mut loop_guard = LoopGuard::new();
    let verify_configured = !core.config.verify.steps.is_empty();
    let gate_tools = gates::GateTools::from_names(core.tools.iter().map(|tool| tool.name()));
    let read_available = core.tools.iter().any(|tool| tool.name() == "read");
    let mut pending_compaction = false;

    loop {
        if cancel.is_cancelled() {
            append_error(
                &core.session,
                StopReason::Cancelled.code(),
                StopReason::Cancelled.message(),
                true,
            );
            let _ = tx.send(AgentEvent::Guard {
                code: "cancelled".into(),
                message: StopReason::Cancelled.message().into(),
            });
            return TurnOutcome::ok(TurnStatus::Cancelled);
        }
        if let Some(reason) = progress.stop_before_model() {
            append_error(&core.session, reason.code(), reason.message(), true);
            let _ = tx.send(AgentEvent::Error {
                code: reason.code().into(),
                message: reason.message().into(),
                recoverable: true,
            });
            return TurnOutcome::ok(match reason {
                StopReason::Stalled => TurnStatus::Stalled,
                _ => TurnStatus::Budget,
            });
        }
        if progress.stalled() {
            append_error(
                &core.session,
                StopReason::Stalled.code(),
                StopReason::Stalled.message(),
                true,
            );
            let _ = tx.send(AgentEvent::Guard {
                code: "stalled".into(),
                message: StopReason::Stalled.message().into(),
            });
            return TurnOutcome::ok(TurnStatus::Stalled);
        }
        if progress.should_wrap_up() {
            let message = progress.wrap_up_message();
            if let Err(err) = append_user(&core.session, &message) {
                return TurnOutcome::error("session_write", err.to_string());
            }
            let _ = tx.send(AgentEvent::Notice(message));
        }

        // Assemble context, compacting deterministically at 80% of the usable
        // prompt (or after a provider context error).
        let _ = tx.send(AgentEvent::State(AgentState::AssembleContext));
        let mut assembled = context::assemble(
            &core.system_prompt,
            &core.session.lock().expect("session lock"),
            None,
            &budget,
        );
        let findings = core.findings_summary();
        let state = context::state_block(
            &core.session.lock().expect("session lock"),
            findings.as_ref(),
            budget.percent(assembled.estimated_tokens),
        );
        // A compaction on the branch means the state block must be attached.
        // Detect it from the session, not from the assembled system text:
        // after system merging the summary lives inside messages[0].
        let had_compaction = core
            .session
            .lock()
            .expect("session lock")
            .branch_entries()
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Compaction(_)));
        if had_compaction {
            assembled = context::assemble(
                &core.system_prompt,
                &core.session.lock().expect("session lock"),
                Some(&state),
                &budget,
            );
        }
        account_tools(core, &budget, &mut assembled);

        let force_compact = pending_compaction;
        pending_compaction = false;
        if force_compact || budget.exceeds_compaction_threshold(assembled.estimated_tokens) {
            let _ = tx.send(AgentEvent::State(AgentState::Compact));
            match compact(
                &core.session,
                findings.as_ref(),
                if force_compact {
                    "context_error"
                } else {
                    "auto"
                },
            ) {
                Ok((summary, tokens_before)) => {
                    let _ = tx.send(AgentEvent::Compacted {
                        summary: summary.clone(),
                        tokens_before,
                    });
                    assembled = context::assemble(
                        &core.system_prompt,
                        &core.session.lock().expect("session lock"),
                        Some(&state),
                        &budget,
                    );
                    account_tools(core, &budget, &mut assembled);
                }
                Err(err) => {
                    let message = format!("compaction failed: {err}");
                    append_error(&core.session, "compaction", &message, true);
                    let _ = tx.send(AgentEvent::Error {
                        code: "compaction".into(),
                        message: message.clone(),
                        recoverable: true,
                    });
                }
            }
        }

        // When the engine exposes /tokenize, measure the rendered prompt
        // exactly before sending. The estimate is only a calibration guard;
        // this is the hard gate that keeps a fitting request from being cut.
        if turn.tokenize && budget.exceeds_compaction_threshold(assembled.estimated_tokens) {
            let text = tokenize_text(&assembled.messages, &core.tool_specs);
            let exact = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                crate::provider::probe::token_count(core.provider.as_ref(), &text),
            )
            .await
            .ok()
            .flatten()
            .map(|tokens| tokens.min(u32::MAX as usize) as u32);
            if let Some(exact) = exact
                && exact > budget.usable_prompt()
            {
                if !progress.presend_compaction_used {
                    progress.presend_compaction_used = true;
                    pending_compaction = true;
                    let _ = tx.send(AgentEvent::Notice(format!(
                        "the exact tokenizer count is {exact} tokens, but only {} fit the {} token window; compacting before sending.",
                        budget.usable_prompt(),
                        budget.context_window
                    )));
                    continue;
                }
                let suggested = budget.context_window.max(8192);
                let message = format!(
                    "the rendered prompt is {exact} tokens after compaction, but only {} fit the effective {} token window. The request was not sent. Raise the engine context (llama.cpp --ctx-size {suggested}; Ollama OLLAMA_CONTEXT_LENGTH={suggested}; vLLM --max-model-len {suggested}), lower [model] max_output_tokens, or run /new.",
                    budget.usable_prompt(),
                    budget.context_window
                );
                append_custom(
                    &core.session,
                    "context-truncated",
                    json!({
                        "signal": "exact tokenizer count exceeds the effective window",
                        "estimated_tokens": exact,
                        "context_window": budget.context_window,
                    }),
                );
                append_error(&core.session, "context_truncated", &message, false);
                let _ = tx.send(AgentEvent::Guard {
                    code: "context_truncated".into(),
                    message,
                });
                return TurnOutcome::ok(TurnStatus::ContextTruncated);
            }
        }

        let request = ChatRequest {
            model: core.provider.model().to_string(),
            messages: assembled.messages.clone(),
            tools: core.tool_specs.clone(),
            tool_choice: Some("auto".into()),
            temperature: core.config.model.temperature,
            top_p: core.config.model.top_p,
            max_tokens: core.config.model.max_output_tokens,
            stop: core.model_profile.stop.clone(),
            extra_body: core
                .model_profile
                .extra_body
                .as_ref()
                .and_then(|value| serde_json::to_value(value).ok()),
            include_usage: true,
            reasoning_field: core.model_profile.reasoning_field.clone(),
        };

        progress.model_call_started();
        let _ = tx.send(AgentEvent::State(AgentState::StreamModel));
        let _ = tx.send(AgentEvent::Context {
            used: assembled.estimated_tokens,
            window: budget.context_window,
        });

        let mut partial = None;
        let result = stream_assistant(
            core,
            request,
            tx,
            cancel,
            &mut budget,
            assembled.estimated_tokens,
            assembled.char_count,
            &mut partial,
        )
        .await;

        let assistant = match result {
            Ok(assistant) => assistant,
            Err(err) if err.class == ErrorClass::Cancelled => {
                if let Some(partial) = partial.take()
                    && let Err(error) = append_assistant(&core.session, partial)
                {
                    tracing::warn!(%error, "cannot persist partial assistant output");
                }
                append_error(&core.session, "cancelled", "generation cancelled", true);
                let _ = tx.send(AgentEvent::Guard {
                    code: "cancelled".into(),
                    message: "generation cancelled".into(),
                });
                return TurnOutcome::ok(TurnStatus::Cancelled);
            }
            Err(err)
                if err.class == ErrorClass::ContextTooSmall && !progress.context_retry_used =>
            {
                if let Some(partial) = partial.take()
                    && let Err(error) = append_assistant(&core.session, partial)
                {
                    tracing::warn!(%error, "cannot persist partial assistant output");
                }
                progress.context_retry_used = true;
                pending_compaction = true;
                // llama.cpp names its real window in the rejection.
                if let Some(window) = crate::provider::probe::window_from_overflow(&err.message)
                    && window < budget.context_window
                {
                    core.remember_window(window);
                    core.server_window
                        .store(window, std::sync::atomic::Ordering::Relaxed);
                    budget = budget.shrunk_to(window);
                    window_probe_pending = false;
                    let _ = tx.send(AgentEvent::WindowDetected {
                        window,
                        source: "the server's overflow error".into(),
                    });
                }
                let _ = tx.send(AgentEvent::Notice(format!(
                    "context window exceeded; compacting to fit {} tokens before retrying",
                    budget.context_window
                )));
                continue;
            }
            Err(err) => {
                if let Some(partial) = partial.take()
                    && let Err(error) = append_assistant(&core.session, partial)
                {
                    tracing::warn!(%error, "cannot persist partial assistant output");
                }
                let code = err.class.code().to_string();
                append_error(&core.session, &code, &err.message, true);
                let _ = tx.send(AgentEvent::Error {
                    code: code.clone(),
                    message: err.message.clone(),
                    recoverable: true,
                });
                return TurnOutcome::error(&code, err.message);
            }
        };

        if window_probe_pending {
            window_probe_pending = false;
            if let Some(window) = detect_served_window(core).await {
                let known = core
                    .server_window
                    .load(std::sync::atomic::Ordering::Relaxed);
                let window = if known > 0 { window.min(known) } else { window };
                core.remember_window(window);
                budget = budget.resized_to(window, core.config.model.max_output_tokens);
                let _ = tx.send(AgentEvent::WindowDetected {
                    window,
                    source: "llama.cpp after the first reply".into(),
                });
                context_window_event(tx, &budget, &assistant);
            }
        }

        if let Some(signal) = detect_truncation(&mut budget, &assistant, assembled.estimated_tokens)
        {
            // A reply written against a prompt the server cut is not
            // trustworthy: do not append it, and do not continue this branch
            // silently.
            if let Some(limit) = signal.limit
                && limit < budget.context_window
            {
                core.server_window
                    .store(limit, std::sync::atomic::Ordering::Relaxed);
                core.remember_window(limit);
                budget = budget.shrunk_to(limit);
            }
            let message = signal.message(&budget, assembled.estimated_tokens);
            append_custom(
                &core.session,
                "context-truncated",
                json!({
                    "signal": signal.source,
                    "prompt_tokens": signal.prompt_tokens,
                    "estimated_tokens": assembled.estimated_tokens,
                    "context_window": budget.context_window,
                    "limit": signal.limit,
                }),
            );
            append_error(&core.session, "context_truncated", &message, false);
            let _ = tx.send(AgentEvent::Guard {
                code: "context_truncated".into(),
                message: message.clone(),
            });
            let _ = tx.send(AgentEvent::Notice(
                "the turn ended because the prompt was truncated. Run /compact to rebuild a fitting summary, or /new to start a fresh branch; do not continue this one.".into(),
            ));
            return TurnOutcome::ok(TurnStatus::ContextTruncated);
        }
        let mut assistant = assistant;
        let recovery = recover_output(core, &mut assistant, &mut progress);
        if let Some(notice) = recovery.notice {
            let _ = tx.send(AgentEvent::Notice(notice));
        }
        if let Some(nudge) = recovery.nudge {
            if progress.output_recoveries < MAX_OUTPUT_RECOVERIES {
                progress.output_recoveries += 1;
                if let Err(err) = append_assistant(&core.session, assistant)
                    .and_then(|()| append_user(&core.session, &nudge))
                {
                    return TurnOutcome::error("session_write", err.to_string());
                }
                continue;
            }
            if recovery.degenerate {
                let _ = append_assistant(&core.session, assistant);
                let message = "the model kept repeating itself; stopped. Try again, lower the temperature, or raise the repetition penalty in the model profile's extra_body.";
                append_error(&core.session, "repetition", message, true);
                let _ = tx.send(AgentEvent::Guard {
                    code: "repetition".into(),
                    message: message.into(),
                });
                return TurnOutcome::ok(TurnStatus::Stalled);
            }
        }
        let has_calls = assistant
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty());
        if let Some(usage) = &assistant.usage {
            let _ = tx.send(AgentEvent::Context {
                used: usage.prompt_tokens,
                window: budget.context_window,
            });
        }
        if let Err(err) = append_assistant(&core.session, assistant.clone()) {
            let message = format!("cannot append assistant entry: {err}");
            let _ = tx.send(AgentEvent::Error {
                code: "session_write".into(),
                message: message.clone(),
                recoverable: false,
            });
            return TurnOutcome::error("session_write", message);
        }

        if !has_calls {
            let _ = tx.send(AgentEvent::State(AgentState::SettleGates));
            let facts = gates::analyze_turn_since(
                &core.session.lock().expect("session lock"),
                verify_configured,
                turn_start.as_deref(),
            );
            match gates::settle(&facts, &mut progress, gate_tools) {
                SettleDecision::Done => {
                    if gate_tools.verify
                        && verify_configured
                        && facts.unverified_mutations
                        && progress.verify_attempts >= progress.limits.verify_attempts
                    {
                        append_custom(
                            &core.session,
                            "guard-trip",
                            json!({
                                "code": "verify_unverified",
                                "message": "settled with changes newer than the last passing verification",
                            }),
                        );
                    }
                    return TurnOutcome::ok(TurnStatus::Done);
                }
                SettleDecision::Continue { message, .. } => {
                    if let Err(err) = append_user(&core.session, &message) {
                        return TurnOutcome::error("session_write", err.to_string());
                    }
                    let _ = tx.send(AgentEvent::Notice(message));
                    continue;
                }
                SettleDecision::Block { message } => {
                    append_error(&core.session, "gate_block", &message, true);
                    let _ = tx.send(AgentEvent::Error {
                        code: "gate_block".into(),
                        message: message.clone(),
                        recoverable: true,
                    });
                    return TurnOutcome::ok(TurnStatus::Blocked);
                }
            }
        }

        let calls = assistant.tool_calls.clone().unwrap_or_default();
        if !progress.tools_available(calls.len() as u32) {
            for call in &calls {
                append_tool_result(
                    &core.session,
                    call,
                    &ToolResult::err(
                        "budget_tools",
                        "tool-call budget exhausted; this call did not run",
                    ),
                    None,
                );
            }
            append_error(
                &core.session,
                StopReason::ToolBudget.code(),
                StopReason::ToolBudget.message(),
                true,
            );
            let _ = tx.send(AgentEvent::Error {
                code: StopReason::ToolBudget.code().into(),
                message: StopReason::ToolBudget.message().into(),
                recoverable: true,
            });
            return TurnOutcome::ok(TurnStatus::Budget);
        }

        let _ = tx.send(AgentEvent::State(AgentState::ToolBatch));
        let batch = execute_batch(
            core,
            tx,
            cancel,
            &mut progress,
            &mut loop_guard,
            &calls,
            gate_tools,
            read_available,
            turn_start.as_deref(),
        )
        .await;
        let _ = tx.send(AgentEvent::State(AgentState::PostTool));

        if let Some((code, message)) = batch.guard {
            append_error(&core.session, &code, &message, true);
            let _ = tx.send(AgentEvent::Guard {
                code: code.clone(),
                message: message.clone(),
            });
            return TurnOutcome::ok(TurnStatus::Done);
        }
        if batch.cancelled {
            append_error(
                &core.session,
                "cancelled",
                "turn cancelled during a tool batch",
                true,
            );
            return TurnOutcome::ok(TurnStatus::Cancelled);
        }
        if batch.end_turn {
            if let Some(request) = batch.thread_split {
                let findings = core.findings_summary();
                match compact_with_reason(&core.session, findings.as_ref(), "thread_split") {
                    Ok((summary, tokens_before)) => {
                        let _ = tx.send(AgentEvent::Compacted {
                            summary,
                            tokens_before,
                        });
                    }
                    Err(err) => {
                        let _ = tx.send(AgentEvent::Notice(format!(
                            "thread compaction failed: {err}"
                        )));
                    }
                }
                if let Some(kickoff) = request.kickoff {
                    let _ = append_user(&core.session, &kickoff);
                }
                let _ = tx.send(AgentEvent::Notice(
                    "thread split: context compacted; the old branch remains in /tree".into(),
                ));
                return TurnOutcome::ok(TurnStatus::Done);
            }
            return TurnOutcome::ok(TurnStatus::Done);
        }
        if progress.stalled() {
            append_error(
                &core.session,
                StopReason::Stalled.code(),
                StopReason::Stalled.message(),
                true,
            );
            let _ = tx.send(AgentEvent::Guard {
                code: "stalled".into(),
                message: StopReason::Stalled.message().into(),
            });
            return TurnOutcome::ok(TurnStatus::Stalled);
        }
    }
}

/// The served model's window, read right after it answered: a gateway has it
/// loaded then, so `/slots` and `/props` describe this model.
async fn detect_served_window(core: &Core) -> Option<u32> {
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        crate::provider::probe::served_window(core.provider.as_ref()),
    )
    .await
    .ok()
    .flatten()
}

fn context_window_event(
    tx: &UnboundedSender<AgentEvent>,
    budget: &TokenBudget,
    assistant: &AssistantData,
) {
    if let Some(usage) = &assistant.usage {
        let _ = tx.send(AgentEvent::Context {
            used: usage.prompt_tokens,
            window: budget.context_window,
        });
    }
}

/// Refuse a turn whose fixed prompt cannot fit the effective window. The
/// message names the three components and the exact server flag to raise.
fn static_prompt_failure(
    static_prompt: &StaticPrompt,
    budget: &TokenBudget,
) -> Option<(String, String)> {
    let static_prompt = *static_prompt;
    let usable = budget.usable_prompt();
    if static_prompt.total <= usable {
        return None;
    }
    let (largest_name, largest) = static_prompt.largest();
    let message = format!(
        "the static prompt cannot fit the model window: system prompt ~{} tokens + tool schemas ~{} + repository brief ~{} = {} tokens, but only {} of the {} token window are usable for input ({} reserved for output, {} safety margin). The request would be cut before your first message. Largest contributor: {} ({} tokens). Use the minimal profile, raise the engine context (llama.cpp --ctx-size {}; Ollama OLLAMA_CONTEXT_LENGTH={}; vLLM --max-model-len {}), or lower [model] max_output_tokens.",
        static_prompt.system,
        static_prompt.tools,
        static_prompt.repo,
        static_prompt.total,
        usable,
        budget.context_window,
        budget.max_output_tokens,
        budget.safety_margin,
        largest_name,
        largest,
        budget.context_window.max(8192),
        budget.context_window.max(8192),
        budget.context_window.max(8192),
    );
    Some(("budget_static".to_string(), message))
}

/// Warn once per turn when the static prompt consumes more than half of the
/// usable prompt. Not a failure: a 32k window is unchanged.
fn static_prompt_warning(static_prompt: &StaticPrompt, budget: &TokenBudget) -> Option<String> {
    let static_prompt = *static_prompt;
    let usable = budget.usable_prompt();
    if static_prompt.total <= usable || u64::from(static_prompt.total) * 2 <= u64::from(usable) {
        return None;
    }
    let (largest_name, largest) = static_prompt.largest();
    Some(format!(
        "static prompt uses {:.0}% of the usable prompt ({} of {} tokens: system ~{}, tools ~{}, repo brief ~{}; largest: {} at {}). A small window leaves little room for the conversation; raise the engine context or use a smaller profile.",
        static_prompt.percent_of(usable),
        static_prompt.total,
        usable,
        static_prompt.system,
        static_prompt.tools,
        static_prompt.repo,
        largest_name,
        largest,
    ))
}

struct TruncationSignal {
    source: String,
    limit: Option<u32>,
    prompt_tokens: Option<u32>,
}

impl TruncationSignal {
    fn message(&self, budget: &TokenBudget, estimated_tokens: u32) -> String {
        let mut out = format!(
            "the server truncated the prompt ({}): the harness estimated {} tokens, the server counted {}; effective window {} tokens.",
            self.source,
            estimated_tokens,
            self.prompt_tokens
                .map(|tokens| tokens.to_string())
                .unwrap_or_else(|| "an unknown number".into()),
            budget.context_window,
        );
        if let Some(limit) = self.limit {
            out.push_str(&format!(
                " The server appears to accept about {limit} prompt tokens."
            ));
        }
        let suggested = budget.context_window.max(8192);
        out.push_str(&format!(
            " The reply was discarded and the turn stopped; do not continue this branch. Raise the engine context (llama.cpp --ctx-size {suggested}; Ollama OLLAMA_CONTEXT_LENGTH={suggested}; vLLM --max-model-len {suggested}), then run /compact or /new."
        ));
        out
    }
}

/// A turn-ending truncation signal: the engine's own flag, a context shift,
/// the usage-ratio heuristic, or a prompt-token shortfall larger than the
/// safety margin. Returns `None` when the prompt was served whole.
fn detect_truncation(
    budget: &mut TokenBudget,
    assistant: &AssistantData,
    estimated_tokens: u32,
) -> Option<TruncationSignal> {
    if let Some(limit) = budget.truncated_at.take() {
        return Some(TruncationSignal {
            source: "usage ratio: the server counted far fewer tokens than the text contains"
                .into(),
            limit: Some(limit),
            prompt_tokens: assistant.usage.as_ref().map(|usage| usage.prompt_tokens),
        });
    }
    let usage = assistant.usage.as_ref()?;
    if usage.truncated == Some(true) {
        return Some(TruncationSignal {
            source: "the engine reported truncated=true".into(),
            limit: Some(usage.prompt_tokens).filter(|tokens| *tokens > 0),
            prompt_tokens: Some(usage.prompt_tokens),
        });
    }
    if usage.context_shift.unwrap_or(0) > 0 {
        return Some(TruncationSignal {
            source: format!(
                "the engine reported {} context-shift event(s)",
                usage.context_shift.unwrap_or(0)
            ),
            limit: None,
            prompt_tokens: Some(usage.prompt_tokens),
        });
    }
    if usage.prompt_tokens > 0 && estimated_tokens >= 2_048 {
        let shortfall = estimated_tokens.saturating_sub(usage.prompt_tokens);
        // A small tokenizer disagreement is normal; a shortfall larger than
        // the safety margin, and at least a quarter of the prompt, means the
        // server dropped messages or tools.
        if shortfall > budget.safety_margin && usage.prompt_tokens < estimated_tokens * 3 / 4 {
            return Some(TruncationSignal {
                source: format!(
                    "prompt-token shortfall: estimated {estimated_tokens}, server counted {}",
                    usage.prompt_tokens
                ),
                limit: None,
                prompt_tokens: Some(usage.prompt_tokens),
            });
        }
    }
    None
}

/// A call id unique within the session, for servers that omit ids and for
/// calls recovered from text. Per-message counters (`call_1`) repeat across
/// messages and break result matching in replayed history.
fn fresh_call_id(index: usize) -> String {
    format!("call_{:x}_{}", Utc::now().timestamp_micros(), index + 1)
}

/// `stop_reason` recorded when the repetition guard cut the stream.
const REPETITION_STOP: &str = "repetition";
/// Nudges for broken output (loops, cut-offs, empty replies) per turn.
const MAX_OUTPUT_RECOVERIES: u32 = 3;

#[derive(Default)]
struct OutputRecovery {
    /// One line for the user.
    notice: Option<String>,
    /// A harness message for the model; the loop continues after it.
    nudge: Option<String>,
    degenerate: bool,
}

/// Repair what can be repaired in place (tool calls written as text) and name
/// a nudge for what cannot (a loop, an output-limit cut, an empty reply).
fn recover_output(
    core: &Core,
    assistant: &mut AssistantData,
    progress: &mut RunProgress,
) -> OutputRecovery {
    let mut out = OutputRecovery::default();
    if assistant.stop_reason.as_deref() == Some(REPETITION_STOP) {
        // Calls streamed alongside a loop are not trustworthy.
        assistant.tool_calls = None;
        out.degenerate = true;
        out.notice = Some("repetition guard: the model started looping; generation stopped".into());
        out.nudge = Some("[localharness] repetition guard: your output began repeating itself and was stopped. Do not restate it. Take one concrete next step: a single tool call, or a short final answer.".into());
        return out;
    }

    let has_native = assistant
        .tool_calls
        .as_ref()
        .is_some_and(|calls| !calls.is_empty());
    if !has_native
        && core.model_profile.tool_mode != crate::config::ToolMode::Native
        && let Some(content) = assistant.content.as_deref()
    {
        let active: Vec<&str> = core.tools.iter().map(|tool| tool.name()).collect();
        if let Some(found) = super::output::extract_text_calls(content, &active) {
            let calls: Vec<ToolCall> = found
                .calls
                .into_iter()
                .enumerate()
                .map(|(index, call)| ToolCall {
                    id: fresh_call_id(index),
                    name: call.name,
                    arguments: call.arguments,
                })
                .collect();
            if !progress.text_calls_noticed {
                progress.text_calls_noticed = true;
                out.notice = Some(format!(
                    "recovered {} tool call(s) the model wrote as text ({}); if this repeats, enable native tool calling on the server (llama.cpp: --jinja)",
                    calls.len(),
                    found.format
                ));
            }
            assistant.content = (!found.prose.is_empty()).then_some(found.prose);
            assistant.tool_calls = Some(calls);
        }
    }

    let calls = assistant.tool_calls.as_deref().unwrap_or_default();
    let content_empty = assistant
        .content
        .as_deref()
        .is_none_or(|c| c.trim().is_empty());
    if assistant.stop_reason.as_deref() == Some("length") {
        let cut = calls.iter().find(|call| {
            matches!(&call.arguments, Value::String(raw) if serde_json::from_str::<Value>(raw).is_err())
        });
        let limit = format!(" ({} tokens)", core.config.model.max_output_tokens);
        if let Some(call) = cut {
            out.nudge = Some(format!(
                "[localharness] output limit: your response hit the output token limit{limit} while writing the {} call, so the call was cut off and nothing ran. Send a smaller call: write a short file and grow it with edit, or split the content across several calls.",
                call.name
            ));
            assistant.tool_calls = None;
        } else if calls.is_empty() {
            out.nudge = Some(if content_empty {
                "[localharness] output limit: you used the whole output budget thinking. Stop deliberating and act now: make one tool call or give a concise answer.".to_string()
            } else {
                "[localharness] output limit: your response was cut off by the output token limit. Continue exactly where it stopped, without repeating what you already wrote.".to_string()
            });
        }
        if out.nudge.is_some() {
            out.notice = Some(format!(
                "the model hit its output token limit{limit}; raise [model] max_output_tokens if this repeats"
            ));
        }
    } else if calls.is_empty() && content_empty {
        out.nudge = Some("[localharness] empty response: you returned no text and no tool call. Make the next tool call, or give your final answer.".into());
    }
    out
}

struct StreamAccum {
    content: String,
    reasoning: String,
    calls: BTreeMap<usize, PartialCall>,
    usage: Option<Usage>,
    finish: Option<String>,
    think: super::output::ThinkSplitter,
    content_loop: super::output::RepetitionGuard,
    reasoning_loop: super::output::RepetitionGuard,
    /// Set when the repetition guard stopped the stream.
    degenerate: bool,
    /// When the request was sent, and when tokens first and last arrived.
    sent: Instant,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

impl StreamAccum {
    /// Content streamed so far, without tool calls, for a cancelled or failed
    /// generation. Keeps the branch useful after an interruption.
    fn partial(&self) -> Option<AssistantData> {
        if self.content.trim().is_empty() && self.reasoning.trim().is_empty() {
            return None;
        }
        Some(AssistantData {
            content: if self.content.is_empty() {
                None
            } else {
                Some(self.content.clone())
            },
            reasoning: if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning.clone())
            },
            tool_calls: None,
            usage: self.usage.clone(),
            stop_reason: Some("cancelled".into()),
        })
    }

    fn new() -> Self {
        Self {
            content: String::new(),
            reasoning: String::new(),
            calls: BTreeMap::new(),
            usage: None,
            finish: None,
            think: super::output::ThinkSplitter::default(),
            content_loop: super::output::RepetitionGuard::default(),
            reasoning_loop: super::output::RepetitionGuard::default(),
            degenerate: false,
            sent: Instant::now(),
            first_token: None,
            last_token: None,
        }
    }

    fn token_arrived(&mut self) {
        let now = Instant::now();
        self.first_token.get_or_insert(now);
        self.last_token = Some(now);
    }

    /// Add harness-side timing to the usage report: time to first token
    /// always, and generation speed when the server did not report one.
    fn measure(&mut self) {
        let (Some(usage), Some(first)) = (self.usage.as_mut(), self.first_token) else {
            return;
        };
        usage.first_token_ms = Some(
            first
                .duration_since(self.sent)
                .as_millis()
                .min(u128::from(u32::MAX)) as u32,
        );
        if usage.tokens_per_sec.is_none()
            && usage.completion_tokens > 1
            && let Some(last) = self.last_token
        {
            let secs = last.duration_since(first).as_secs_f64();
            // Shorter spans are dominated by network jitter.
            if secs >= 0.2 {
                usage.tokens_per_sec = Some((f64::from(usage.completion_tokens - 1) / secs) as f32);
            }
        }
    }

    /// Route streamed content through the think splitter, then watch both
    /// buffers for a degenerate loop.
    fn push_content(&mut self, delta: &str, tx: &UnboundedSender<AgentEvent>) {
        let pieces = self.think.push(delta);
        self.route(pieces, tx);
    }

    fn push_reasoning(&mut self, delta: String, tx: &UnboundedSender<AgentEvent>) {
        self.route(vec![super::output::Piece::Reasoning(delta)], tx);
    }

    fn route(&mut self, pieces: Vec<super::output::Piece>, tx: &UnboundedSender<AgentEvent>) {
        for piece in pieces {
            match piece {
                super::output::Piece::Text(text) => {
                    self.content.push_str(&text);
                    let _ = tx.send(AgentEvent::TextDelta(text));
                }
                super::output::Piece::Reasoning(text) => {
                    self.reasoning.push_str(&text);
                    let _ = tx.send(AgentEvent::ReasoningDelta(text));
                }
            }
        }
        if let Some(keep) = self.content_loop.check(&self.content) {
            self.content.truncate(keep);
            self.degenerate = true;
        }
        if let Some(keep) = self.reasoning_loop.check(&self.reasoning) {
            self.reasoning.truncate(keep);
            self.degenerate = true;
        }
    }

    /// Emit anything the think splitter held back, and move reasoning that a
    /// template left as `...</think>` without an opening tag.
    fn settle_reasoning(&mut self, tx: &UnboundedSender<AgentEvent>) {
        let pieces = self.think.flush();
        self.route(pieces, tx);
        if !self.think.opened()
            && let Some((reasoning, content)) = super::output::split_unopened_think(&self.content)
        {
            if !reasoning.is_empty() {
                if !self.reasoning.is_empty() {
                    self.reasoning.push('\n');
                }
                self.reasoning.push_str(&reasoning);
            }
            self.content = content;
        }
        if self.think.opened() {
            self.content = self.content.trim_start().to_string();
        }
    }

    fn finalize(self) -> AssistantData {
        let mut calls = Vec::new();
        for (index, mut call) in self.calls {
            if call.id.trim().is_empty() {
                call.id = fresh_call_id(index);
            }
            if call.name.trim().is_empty() {
                continue;
            }
            let arguments = if call.arguments.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&call.arguments)
                    .unwrap_or_else(|_| Value::String(call.arguments.clone()))
            };
            calls.push(ToolCall {
                id: call.id,
                name: call.name,
                arguments,
            });
        }
        AssistantData {
            content: if self.content.is_empty() {
                None
            } else {
                Some(self.content)
            },
            reasoning: if self.reasoning.is_empty() {
                None
            } else {
                Some(self.reasoning)
            },
            tool_calls: if calls.is_empty() { None } else { Some(calls) },
            usage: self.usage,
            stop_reason: if self.degenerate {
                Some(REPETITION_STOP.into())
            } else {
                self.finish.or_else(|| Some("stop".into()))
            },
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_assistant(
    core: &Core,
    request: ChatRequest,
    tx: &UnboundedSender<AgentEvent>,
    cancel: &CancellationToken,
    budget: &mut TokenBudget,
    estimated_tokens: u32,
    char_count: usize,
    partial: &mut Option<AssistantData>,
) -> Result<AssistantData, ProviderError> {
    if cancel.is_cancelled() {
        return Err(ProviderError::new(
            ErrorClass::Cancelled,
            "generation cancelled",
        ));
    }
    let (etx, mut erx) = tokio::sync::mpsc::unbounded_channel();
    let child = cancel.child_token();
    let provider = core.provider.clone();
    let task = tokio::spawn(async move {
        provider.chat_stream(request, etx, child).await;
    });
    let mut accum = StreamAccum::new();
    let mut failure: Option<ProviderError> = None;
    while let Some(item) = erx.recv().await {
        if let Some(err) = handle_stream_item(
            item,
            &mut accum,
            tx,
            budget,
            estimated_tokens,
            char_count,
            cancel,
        ) {
            failure = Some(err);
            break;
        }
        if accum.degenerate {
            // Stop paying for a loop; the provider task is aborted below.
            break;
        }
    }
    task.abort();
    let _ = task.await;
    accum.settle_reasoning(tx);
    accum.measure();
    if let Some(usage) = &accum.usage {
        let _ = tx.send(AgentEvent::Usage(usage.clone()));
    }
    if let Some(err) = failure {
        *partial = accum.partial();
        return Err(err);
    }
    if cancel.is_cancelled() {
        *partial = accum.partial();
        return Err(ProviderError::new(
            ErrorClass::Cancelled,
            "generation cancelled",
        ));
    }
    Ok(accum.finalize())
}

#[allow(clippy::too_many_arguments)]
fn handle_stream_item(
    item: Result<StreamEvent, ProviderError>,
    accum: &mut StreamAccum,
    tx: &UnboundedSender<AgentEvent>,
    budget: &mut TokenBudget,
    estimated_tokens: u32,
    char_count: usize,
    cancel: &CancellationToken,
) -> Option<ProviderError> {
    if cancel.is_cancelled() {
        return Some(ProviderError::new(
            ErrorClass::Cancelled,
            "generation cancelled",
        ));
    }
    match item {
        Err(err) => Some(err),
        Ok(StreamEvent::Delta(text)) => {
            accum.token_arrived();
            accum.push_content(&text, tx);
            None
        }
        Ok(StreamEvent::ReasoningDelta(text)) => {
            accum.token_arrived();
            accum.push_reasoning(text, tx);
            None
        }
        Ok(StreamEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        }) => {
            accum.token_arrived();
            let call = accum.calls.entry(index).or_default();
            if let Some(id) = id {
                merge_fragment(&mut call.id, &id);
            }
            if let Some(name) = name {
                merge_fragment(&mut call.name, &name);
            }
            if let Some(arguments) = arguments {
                call.arguments.push_str(&arguments);
            }
            None
        }
        Ok(StreamEvent::Usage(usage)) => {
            if usage.prompt_tokens > 0 {
                budget.calibrate(char_count, usage.prompt_tokens);
                budget.observe_usage(usage.prompt_tokens, estimated_tokens);
            }
            let merged = match accum.usage.as_mut() {
                Some(existing) => {
                    existing.merge(&usage);
                    existing.clone()
                }
                None => usage,
            };
            let _ = tx.send(AgentEvent::Usage(merged.clone()));
            accum.usage = Some(merged);
            None
        }
        Ok(StreamEvent::Finish(reason)) => {
            accum.finish = Some(reason);
            None
        }
    }
}

fn merge_fragment(buffer: &mut String, fragment: &str) {
    if fragment.is_empty() {
        return;
    }
    if buffer.is_empty() || !buffer.ends_with(fragment) {
        buffer.push_str(fragment);
    }
}

struct BatchOutcome {
    cancelled: bool,
    guard: Option<(String, String)>,
    end_turn: bool,
    thread_split: Option<ThreadSplit>,
}

struct ThreadSplit {
    kickoff: Option<String>,
}

#[allow(clippy::too_many_arguments, clippy::unnecessary_unwrap)]
async fn execute_batch(
    core: &std::sync::Arc<Core>,
    tx: &UnboundedSender<AgentEvent>,
    cancel: &CancellationToken,
    progress: &mut RunProgress,
    loop_guard: &mut LoopGuard,
    calls: &[ToolCall],
    gate_tools: gates::GateTools,
    read_available: bool,
    turn_start: Option<&str>,
) -> BatchOutcome {
    let mut deferred: Vec<CustomData> = Vec::new();
    let mut inferred_mutations = 0usize;
    let mut inferred_files: HashSet<String> = HashSet::new();
    let mut plan_present = false;
    let mut guard_trip: Option<(String, String)> = None;
    let mut cancelled = false;
    let mut batch_chars = 0usize;
    let mut end_turn = false;
    let mut thread_split: Option<ThreadSplit> = None;

    let ctx = core.tool_context_with(cancel.clone());
    let active: Vec<&str> = core.tools.iter().map(|tool| tool.name()).collect();
    for (position, original) in calls.iter().enumerate() {
        if cancel.is_cancelled() {
            cancelled = true;
            for remaining in &calls[position..] {
                append_tool_result(
                    &core.session,
                    remaining,
                    &ToolResult::err("cancelled", "cancelled; this call did not run"),
                    None,
                );
            }
            break;
        }
        // Resolve the name once so every gate below (read-first, todo, scope)
        // sees the canonical tool, never the model's spelling of it.
        let resolved_name = super::repair::resolve_tool_name(&original.name, &active);
        let mut routed_note = None;
        let mut unknown_suggestion = None;
        let routed_call;
        let call = match &resolved_name {
            super::repair::ToolName::Exact(name) if *name != original.name => {
                routed_call = ToolCall {
                    name: name.clone(),
                    ..original.clone()
                };
                &routed_call
            }
            super::repair::ToolName::Routed { from, to } => {
                routed_note = Some(format!(
                    "note: {from:?} is not a tool here; ran {to}. Call {to} directly next time."
                ));
                routed_call = ToolCall {
                    name: to.clone(),
                    ..original.clone()
                };
                &routed_call
            }
            super::repair::ToolName::Unknown { suggestion } => {
                unknown_suggestion = suggestion.clone();
                original
            }
            super::repair::ToolName::Exact(_) => original,
        };
        let started = Instant::now();
        let summary_args = summarize_args(&call.arguments);
        let _ = tx.send(AgentEvent::ToolStarted {
            call_id: call.id.clone(),
            name: call.name.clone(),
            summary: summary_args.clone(),
        });

        let tool = core
            .tools
            .iter()
            .find(|tool| tool.name() == call.name)
            .cloned();
        let raw = arguments_raw(&call.arguments);
        let mut args_path: Option<String> = None;
        let mut evidence_args: Option<Value> = None;
        let mut result;
        let mut repaired = None;

        if tool.is_none() {
            let hint = unknown_suggestion
                .map(|name| format!("did you mean {name}? "))
                .unwrap_or_default();
            result = ToolResult::err(
                "unknown_tool",
                format!(
                    "unknown tool {:?}; {hint}active tools: {}",
                    call.name,
                    active.join(", ")
                ),
            );
        } else {
            let schema = tool.as_ref().unwrap().schema();
            match super::repair::repair_with_notes(&call.name, &raw, &schema) {
                Err(err) => {
                    result = ToolResult::err(err.code, err.message);
                }
                Ok(repaired_args) => {
                    if let Some(note) = repaired_args.note(&call.name, &schema) {
                        routed_note = Some(match routed_note.take() {
                            Some(existing) => format!("{existing}\n{note}"),
                            None => note,
                        });
                    }
                    let args = repaired_args.args;
                    args_path = args
                        .get("path")
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    evidence_args = Some(args.clone());
                    repaired = Some(args.clone());
                    result = ToolResult::ok("");

                    if read_available
                        && matches!(call.name.as_str(), "edit" | "write" | "patch")
                        && let Some(path) = &args_path
                    {
                        let resolved = match ctx.resolve(path) {
                            Ok(resolved) => Some(resolved),
                            Err(err) => {
                                result = err;
                                None
                            }
                        };
                        if let Some(resolved) = resolved
                            && resolved.exists()
                        {
                            let current = core
                                .reads
                                .lock()
                                .expect("read tracker")
                                .is_current(&resolved);
                            if !current {
                                result = ToolResult::err(
                                    "READ_FIRST",
                                    format!(
                                        "read {path} before changing it, then retry the {}. The file is new or changed since the last read.",
                                        call.name
                                    ),
                                );
                            }
                        }
                    }

                    if !result.is_error {
                        let current = gates::analyze_turn_since(
                            &core.session.lock().expect("session lock"),
                            false,
                            turn_start,
                        );
                        if gate_tools.todo
                            && let Some(block) = gates::todo_edit_gate(
                                &call.name,
                                current.mutations + inferred_mutations,
                                unique_file_count(&current.files, &inferred_files),
                                plan_present || current.todo_plan_present,
                            )
                        {
                            result = block;
                        }
                    }

                    if !result.is_error
                        && matches!(call.name.as_str(), "bash" | "bg")
                        && let Some(command) = args.get("command").and_then(|v| v.as_str())
                    {
                        let gate = core.security.lock().expect("security state").gate(command);
                        if let Err(message) = gate {
                            result = ToolResult::err("SEC_SCOPE", message);
                        }
                    }

                    if result.is_error {
                        // Keep the failure and skip execution.
                    } else {
                        let signature =
                            format!("{}:{}", call.name, super::repair::canonical_json(&args));
                        match loop_guard.check_and_record(signature) {
                            LoopDecision::Tripped { reason } => {
                                result = ToolResult::err("loop_guard", reason.clone());
                                guard_trip = Some(("loop_guard".into(), reason));
                            }
                            LoopDecision::Ok => {
                                let fut = tool.as_ref().unwrap().call(&ctx, args.clone());
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        result = ToolResult::err("cancelled", "cancelled during execution");
                                    }
                                    outcome = tokio::time::timeout(core.tool_timeout, fut) => {
                                        result = match outcome {
                                            Ok(result) => result,
                                            Err(_) => ToolResult::err(
                                                "timeout",
                                                format!(
                                                    "{} exceeded {} s; narrow the call or use bg for long work",
                                                    call.name,
                                                    core.tool_timeout.as_secs()
                                                ),
                                            ),
                                        };
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        let duration_ms = started.elapsed().as_millis() as u64;
        let evidence_output = result.content.clone();
        if let Some(note) = routed_note {
            result.content = format!("{note}\n{}", result.content);
        }
        result.content = format!(
            "[tool] {} - {}\n{}",
            call.name, summary_args, result.content
        );
        let cap_override = tool.as_deref().and_then(|tool| tool.result_cap());
        let content_chars = result.content.chars().count();
        let mut capped = match cap_override {
            Some(limit) if content_chars <= limit => context::CappedContent {
                content: result.content.clone(),
                artifact: None,
                original_chars: content_chars,
                truncated: false,
            },
            _ => cap_result(&ctx.artifacts, &result.content),
        };
        if cap_override.is_none() && batch_chars + capped.content.chars().count() > BATCH_CAP_CHARS
        {
            match ctx.artifacts.write(&result.content) {
                Ok(artifact) => {
                    capped.content = format!(
                        "(batch cap; full result archived as {} · {} bytes · recall {} 1-120)",
                        artifact.id, artifact.bytes, artifact.id
                    );
                    capped.artifact = Some(artifact.id);
                }
                Err(_) => {
                    capped.content = capped.content.chars().take(200).collect::<String>();
                }
            }
            capped.truncated = true;
        }
        // A read whose result was capped did not show the model the whole
        // file. Record that so `write` requires an explicit overwrite.
        if call.name == "read"
            && capped.truncated
            && let Some(path) = &args_path
            && let Ok(resolved) = ctx.resolve(path)
        {
            crate::tools::write::lock_reads(&ctx).mark_result_truncated(&resolved);
        }
        batch_chars += capped.content.chars().count();
        let tool_result = ToolResult {
            content: capped.content.clone(),
            is_error: result.is_error,
            details: result.details.clone(),
        };
        let entry_id =
            append_tool_result(&core.session, call, &tool_result, capped.artifact.clone());
        if let (Some(tool), Some(args), Some(entry_id)) =
            (tool.as_deref(), evidence_args.as_ref(), entry_id.as_deref())
            && tool.evidence_enabled()
            && let Err(error) = record_evidence(
                core,
                tool,
                call,
                args,
                &evidence_output,
                result.is_error,
                result.details.as_ref(),
                entry_id,
                duration_ms,
            )
        {
            tracing::warn!(%error, tool = call.name, "evidence entry was not recorded");
        }

        let mut verified_ok = false;
        let mut changed = false;
        if !tool_result.is_error {
            if let Some(details) = &tool_result.details {
                if let Some(entries) = details.get("session").and_then(|v| v.as_array()) {
                    for entry in entries {
                        let kind = entry.get("kind").and_then(|v| v.as_str()).unwrap_or("");
                        if kind.is_empty() {
                            continue;
                        }
                        let mut payload = entry.as_object().cloned().unwrap_or_default();
                        payload.remove("kind");
                        let custom = CustomData {
                            kind: kind.to_string(),
                            payload,
                        };
                        if custom.kind == "verify-result"
                            && custom.payload.get("passed").and_then(|v| v.as_bool()) == Some(true)
                        {
                            verified_ok = true;
                        }
                        if custom.kind == "todo-state"
                            && custom
                                .payload
                                .get("tasks")
                                .and_then(|v| v.as_array())
                                .is_some_and(|tasks| !tasks.is_empty())
                        {
                            plan_present = true;
                        }
                        if custom.kind == "fs-change" {
                            changed = true;
                            if let Some(path) = custom.payload.get("path").and_then(|v| v.as_str())
                            {
                                inferred_mutations += 1;
                                inferred_files.insert(path.to_string());
                            }
                        }
                        deferred.push(custom);
                    }
                }
                if details.get("end_turn").and_then(|v| v.as_bool()) == Some(true) {
                    end_turn = true;
                }
                if details.get("thread_split").and_then(|v| v.as_bool()) == Some(true) {
                    thread_split = Some(ThreadSplit {
                        kickoff: details
                            .get("kickoff")
                            .and_then(|v| v.as_str())
                            .map(str::to_string),
                    });
                    end_turn = true;
                }
            }
            if matches!(call.name.as_str(), "write" | "edit" | "patch" | "undo")
                && let Some(path) = &args_path
            {
                changed = true;
                inferred_mutations += 1;
                inferred_files.insert(path.clone());
                let display = repaired
                    .as_ref()
                    .map(|_| display_path(&ctx.cwd, &ctx.cwd.join(path)))
                    .unwrap_or_else(|| path.clone());
                deferred.push(crate::tools::undo::fs_change_entry(
                    display,
                    tool_result.details.as_ref(),
                ));
            }
        }

        let fingerprint = fingerprint(&call.name, &raw, &tool_result.content);
        progress.record_tool_result(&fingerprint, changed, verified_ok);
        if changed || verified_ok {
            // A file change or a passing verification is real progress; the
            // loop guard must not count calls from before it.
            loop_guard.record_progress();
        }
        let _ = tx.send(AgentEvent::ToolFinished {
            call_id: call.id.clone(),
            name: call.name.clone(),
            is_error: tool_result.is_error,
            duration_ms,
            preview: preview(tool_body(&tool_result.content), 300),
            artifact: capped.artifact,
        });

        if guard_trip.is_some() {
            for remaining in &calls[position + 1..] {
                append_tool_result(
                    &core.session,
                    remaining,
                    &ToolResult::err("loop_guard", "not executed: the loop guard ended this turn"),
                    None,
                );
            }
            break;
        }
    }

    if let Ok(mut session) = core.session.lock() {
        for custom in deferred {
            if let Err(err) = session.append(EntryKind::Custom(custom)) {
                tracing::warn!(%err, "cannot append custom session entry");
            }
        }
    }

    let guard = guard_trip.map(|(code, reason)| {
        (
            code,
            format!("{reason} End the turn; the next turn should use a different approach."),
        )
    });
    BatchOutcome {
        cancelled,
        guard,
        end_turn,
        thread_split,
    }
}

fn unique_file_count(existing: &[String], extra: &HashSet<String>) -> usize {
    let mut set: HashSet<&str> = existing.iter().map(String::as_str).collect();
    set.extend(extra.iter().map(String::as_str));
    set.len()
}

fn arguments_raw(value: &Value) -> String {
    match value {
        Value::String(raw) => raw.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "{}".into()),
    }
}

fn summarize_args(args: &Value) -> String {
    if let Some(name) = args.get("name").and_then(|v| v.as_str())
        && args
            .as_object()
            .is_some_and(|map| map.keys().all(|key| key == "name" || key == "reference"))
    {
        return match args.get("reference").and_then(|v| v.as_str()) {
            Some(reference) => preview(&format!("{name} › {reference}"), 80),
            None => preview(name, 80),
        };
    }
    for key in ["path", "command", "pattern", "action", "artifact", "text"] {
        if let Some(value) = args.get(key).and_then(|v| v.as_str()) {
            return preview(value, 80);
        }
    }
    preview(&args.to_string(), 80)
}

/// Drop the `[tool] name - summary` header line from a rendered preview.
fn tool_body(content: &str) -> &str {
    if content.starts_with("[tool] ") {
        content.split_once('\n').map(|(_, body)| body).unwrap_or("")
    } else {
        content
    }
}

fn preview(text: &str, max: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    flat.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

fn fingerprint(name: &str, args: &str, content: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut hasher);
    args.hash(&mut hasher);
    content.hash(&mut hasher);
    format!("{name}:{:016x}", hasher.finish())
}

fn append_user(session: &crate::tools::SessionHandle, text: &str) -> anyhow::Result<()> {
    append_user_with(session, text, Vec::new())
}

fn append_user_with(
    session: &crate::tools::SessionHandle,
    text: &str,
    attachments: Vec<crate::session::Attachment>,
) -> anyhow::Result<()> {
    session
        .lock()
        .expect("session lock")
        .append(EntryKind::User(UserData {
            attachments,
            content: text.to_string(),
        }))?;
    Ok(())
}

fn append_assistant(
    session: &crate::tools::SessionHandle,
    assistant: AssistantData,
) -> anyhow::Result<()> {
    session
        .lock()
        .expect("session lock")
        .append(EntryKind::Assistant(assistant))?;
    Ok(())
}

fn append_custom(session: &crate::tools::SessionHandle, kind: &str, payload: Value) {
    if let Ok(mut session) = session.lock() {
        let mut data = CustomData::new(kind);
        if let Some(object) = payload.as_object() {
            for (key, value) in object {
                data.payload.insert(key.clone(), value.clone());
            }
        }
        let _ = session.append(EntryKind::Custom(data));
    }
}

/// Render the prompt the way the model sees it closely enough for
/// `/tokenize`: role-tagged message text plus tool names, descriptions, and
/// schemas. The chat template adds a small constant overhead, so a count at
/// or below the usable prompt is safe; the server's own error remains the
/// final backstop.
fn tokenize_text(messages: &[ChatMessage], tools: &[crate::provider::ToolSpec]) -> String {
    let mut out = String::new();
    for message in messages {
        out.push_str(message.role());
        out.push_str(": ");
        match message {
            ChatMessage::System { content } | ChatMessage::User { content } => {
                out.push_str(content);
            }
            ChatMessage::Assistant {
                content,
                tool_calls,
            } => {
                if let Some(content) = content {
                    out.push_str(content);
                }
                for call in tool_calls {
                    out.push_str(&format!(
                        "\n{} {}",
                        call.name,
                        serde_json::to_string(&call.arguments).unwrap_or_default()
                    ));
                }
            }
            ChatMessage::Tool { content, .. } => out.push_str(content),
        }
        out.push('\n');
    }
    for tool in tools {
        out.push_str(&tool.name);
        out.push(' ');
        out.push_str(&tool.description);
        out.push(' ');
        out.push_str(&serde_json::to_string(&tool.parameters).unwrap_or_else(|_| "{}".to_string()));
        out.push('\n');
    }
    out
}

fn account_tools(
    core: &Core,
    budget: &TokenBudget,
    assembled: &mut crate::context::AssembledContext,
) {
    let chars: usize = core
        .tool_specs
        .iter()
        .map(|tool| {
            tool.name.len()
                + tool.description.len()
                + serde_json::to_string(&tool.parameters)
                    .map(|schema| schema.len())
                    .unwrap_or(0)
        })
        .sum();
    assembled.char_count += chars;
    assembled.estimated_tokens = assembled
        .estimated_tokens
        .saturating_add(budget.estimate_char_len(chars));
}

fn append_error(
    session: &crate::tools::SessionHandle,
    code: &str,
    message: &str,
    recoverable: bool,
) {
    if let Ok(mut session) = session.lock() {
        let _ = session.append_error(code, message, recoverable);
    }
}

fn append_tool_result(
    session: &crate::tools::SessionHandle,
    call: &ToolCall,
    result: &ToolResult,
    artifact: Option<String>,
) -> Option<String> {
    if let Ok(mut session) = session.lock() {
        match session.append(EntryKind::ToolResult(ToolResultData {
            call_id: call.id.clone(),
            content: result.content.clone(),
            is_error: result.is_error,
            details: result.details.clone(),
            artifact,
        })) {
            Ok(entry) => return Some(entry.id),
            Err(error) => tracing::warn!(%error, "tool result entry was not written"),
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn record_evidence(
    core: &Core,
    tool: &dyn crate::tools::Tool,
    call: &ToolCall,
    args: &Value,
    output: &str,
    is_error: bool,
    details: Option<&Value>,
    entry_id: &str,
    duration_ms: u64,
) -> Result<(), String> {
    let session_id = core.session.lock().ok().map(|session| session.id.clone());
    let command = tool.evidence_command(args).unwrap_or_else(|| {
        format!(
            "{} {}",
            call.name,
            serde_json::to_string(args).unwrap_or_else(|_| "{}".into())
        )
    });
    let target = evidence_targets(&command, args);
    let exit_code = details
        .and_then(|details| details.get("exit_code"))
        .and_then(Value::as_i64);
    let timed_out = details
        .and_then(|details| details.get("timed_out"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let cancelled = details
        .and_then(|details| details.get("cancelled"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let finished = Utc::now();
    let started = finished - ChronoDuration::milliseconds(duration_ms as i64);

    let mut state = core
        .security
        .lock()
        .map_err(|_| "security state is poisoned; restart lh".to_string())?;
    let scope = state.scope.clone();
    let Some(engagement) = state.engagement.as_mut() else {
        return Ok(());
    };
    let evidence = engagement
        .evidence
        .add(crate::security::evidence::NewEvidence {
            tool: call.name.clone(),
            session: session_id,
            entry: Some(entry_id.to_string()),
            target,
            command,
            started,
            finished,
            duration_ms,
            exit_code,
            timed_out,
            cancelled,
            is_error,
            output: output.to_string(),
            summary: None,
        })
        .map_err(|error| error.to_string())?;
    engagement
        .surface
        .ingest(&evidence.command, output, &scope, &evidence.id);
    Ok(())
}

fn evidence_targets(command: &str, args: &Value) -> String {
    let mut targets = crate::security::extract_targets(command);
    if targets.is_empty() {
        let mut strings = Vec::new();
        collect_json_strings(args, &mut strings);
        for value in strings {
            for target in crate::security::extract_targets(&value) {
                if !targets
                    .iter()
                    .any(|existing: &String| existing.eq_ignore_ascii_case(&target))
                {
                    targets.push(target);
                }
            }
        }
    }
    targets.join(", ")
}

fn collect_json_strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(value) => out.push(value.clone()),
        Value::Array(items) => {
            for item in items {
                collect_json_strings(item, out);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_json_strings(item, out);
            }
        }
        _ => {}
    }
}

fn compact(
    session: &crate::tools::SessionHandle,
    findings: Option<&FindingSummary>,
    reason: &str,
) -> anyhow::Result<(String, u64)> {
    compact_session(session, findings, reason)
}

fn compact_with_reason(
    session: &crate::tools::SessionHandle,
    findings: Option<&FindingSummary>,
    reason: &str,
) -> anyhow::Result<(String, u64)> {
    compact_session(session, findings, reason)
}

fn compact_session(
    session: &crate::tools::SessionHandle,
    findings: Option<&FindingSummary>,
    reason: &str,
) -> anyhow::Result<(String, u64)> {
    let mut session = session.lock().expect("session lock");
    let summary = build_summary(&session, findings, COMPACTION_BUDGET_CHARS);
    let thread_id = session
        .latest_custom_on_path("thread-state")
        .and_then(|entry| match &entry.kind {
            EntryKind::Custom(custom) => custom
                .payload
                .get("thread_id")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        });
    session.append(EntryKind::Compaction(CompactionData {
        summary: summary.summary.clone(),
        tokens_before: summary.tokens_before,
        reason: reason.to_string(),
        thread_id,
    }))?;
    session.fsync()?;
    Ok((summary.summary, summary.tokens_before))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::mpsc::UnboundedSender;

    use crate::agent::{RunLimits, WindowSource};
    use crate::config::{BudgetConfig, ModelProfile};
    use crate::context::budget::DEFAULT_CHARS_PER_TOKEN;
    use crate::paths::Paths;
    use crate::provider::{ChatRequest, Provider, StreamItem};
    use crate::security::SecurityState;
    use crate::session::{Session, SessionData};
    use crate::tools::{BgRegistry, ReadTracker, ToolRegistry};

    struct ProbeProvider {
        endpoint: String,
        client: reqwest::Client,
    }

    #[async_trait]
    impl Provider for ProbeProvider {
        async fn chat_stream(
            &self,
            _req: ChatRequest,
            _tx: UnboundedSender<StreamItem>,
            _cancel: CancellationToken,
        ) {
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }

        fn model(&self) -> &str {
            "m"
        }

        fn client(&self) -> Option<&reqwest::Client> {
            Some(&self.client)
        }
    }

    async fn probe_server() -> String {
        use axum::routing::{get, post};
        let models = || async { axum::Json(json!({"data": [{"id": "m", "max_model_len": 4096}]})) };
        let tokenize = || async { axum::Json(json!({"tokens": vec![0u32; 100]})) };
        let app = axum::Router::new()
            .route("/v1/models", get(models))
            .route("/tokenize", post(tokenize));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{address}")
    }

    fn probe_core(dir: &std::path::Path, endpoint: String) -> Core {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths.config_dir = dir.join("config");
        let session = Session::create(
            &paths,
            SessionData {
                cwd: dir.display().to_string(),
                endpoint: endpoint.clone(),
                model: "m".into(),
                profile: "test".into(),
                app: "test".into(),
                redact: true,
                context: None,
            },
            true,
        )
        .unwrap();
        let artifacts = session.artifacts.clone();
        let session = Arc::new(Mutex::new(session));
        let mut config = crate::config::Config::builtin();
        config.model.endpoint = endpoint.clone();
        config.model.model = "m".into();
        config.model.context_window = Some(32_768);
        config.model.max_output_tokens = 1024;
        config.profile_name = "minimal".into();
        let registry = ToolRegistry::builtin();
        let tools = registry.select(config.profile(), &[]).unwrap();
        let tool_specs = ToolRegistry::specs(&tools);
        let system_prompt = "s".repeat(1000);
        let budget = TokenBudget::new(32_768, 1024);
        let static_prompt =
            crate::app::measure_static_prompt(&budget, &system_prompt, &tool_specs, Some(""));
        Core {
            config,
            paths: paths.clone(),
            provider: Arc::new(ProbeProvider {
                endpoint,
                client: reqwest::Client::new(),
            }),
            registry,
            tools,
            tool_specs,
            session,
            artifacts,
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::new())),
            security: Arc::new(Mutex::new(SecurityState::default())),
            sandbox: None,
            budget,
            static_prompt,
            tokenize: false,
            limits: RunLimits::from(&BudgetConfig::default()),
            system_prompt,
            repo_brief: String::new(),
            model_profile: ModelProfile::default(),
            cancel: CancellationToken::new(),
            tool_timeout: std::time::Duration::from_secs(10),
            path_jail: false,
            server_window: Default::default(),
            window_source: WindowSource::Default,
            window_truth: None,
            window_notice: None,
            window_notice_sent: std::sync::Once::new(),
            detected_window: Default::default(),
            window_cache: Default::default(),
            startup: None,
        }
    }

    #[tokio::test]
    async fn deferred_startup_refreshes_window_tokenizer_and_static_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = probe_server().await;
        let core = probe_core(dir.path(), endpoint);
        assert_eq!(core.budget.context_window, 32_768);
        assert!(!core.tokenize);
        assert_eq!(core.budget.chars_per_token, DEFAULT_CHARS_PER_TOKEN);

        let turn = prepare_turn_context(&core, true).await;
        assert_eq!(turn.budget.context_window, 4096);
        assert!(turn.tokenize);
        assert!(
            turn.budget.chars_per_token > DEFAULT_CHARS_PER_TOKEN,
            "tokenizer calibration did not run"
        );
        assert_eq!(
            core.detected_window
                .load(std::sync::atomic::Ordering::Relaxed),
            4096
        );
        // The static prompt must be re-measured with the calibrated ratio.
        assert_eq!(
            turn.static_prompt,
            crate::app::measure_static_prompt(
                &turn.budget,
                &core.system_prompt,
                &core.tool_specs,
                Some(core.repo_brief.as_str())
            )
        );
        assert_ne!(turn.static_prompt, core.static_prompt);
    }

    #[test]
    fn merge_fragment_handles_repeats() {
        let mut buffer = String::new();
        merge_fragment(&mut buffer, "read");
        merge_fragment(&mut buffer, "read");
        assert_eq!(buffer, "read");
        merge_fragment(&mut buffer, "_file");
        assert_eq!(buffer, "read_file");
    }

    #[test]
    fn partial_call_finalizes_with_synthetic_id_and_bad_json() {
        let mut accum = StreamAccum::new();
        let call = accum.calls.entry(0).or_default();
        call.name = "read".into();
        call.arguments = "{not json".into();
        let assistant = accum.finalize();
        let calls = assistant.tool_calls.unwrap();
        assert!(calls[0].id.starts_with("call_"), "{}", calls[0].id);
        assert!(matches!(&calls[0].arguments, Value::String(_)));
        // Ids must not repeat across messages, or replayed history pairs a
        // call with another call's result.
        std::thread::sleep(std::time::Duration::from_millis(1));
        let mut again = StreamAccum::new();
        again.calls.entry(0).or_default().name = "read".into();
        let second = again.finalize().tool_calls.unwrap();
        assert_ne!(calls[0].id, second[0].id);
    }

    #[test]
    fn arguments_raw_roundtrips_objects_and_strings() {
        assert_eq!(arguments_raw(&json!({"a":1})), "{\"a\":1}");
        assert_eq!(arguments_raw(&json!("{}")), "{}");
    }

    #[test]
    fn truncation_is_detected_from_engine_flag_and_shortfall() {
        let mut budget = TokenBudget::new(8192, 1024);
        let assistant = AssistantData {
            usage: Some(Usage {
                prompt_tokens: 100,
                truncated: Some(true),
                ..Usage::default()
            }),
            ..AssistantData::default()
        };
        let signal = detect_truncation(&mut budget, &assistant, 4000).expect("engine flag");
        assert!(signal.source.contains("truncated=true"));

        let mut budget = TokenBudget::new(8192, 1024);
        let assistant = AssistantData {
            usage: Some(Usage {
                prompt_tokens: 1000,
                ..Usage::default()
            }),
            ..AssistantData::default()
        };
        let signal = detect_truncation(&mut budget, &assistant, 6000).expect("shortfall");
        assert!(signal.source.contains("shortfall"));

        // A small tokenizer disagreement is not truncation.
        let mut budget = TokenBudget::new(8192, 1024);
        let assistant = AssistantData {
            usage: Some(Usage {
                prompt_tokens: 3900,
                ..Usage::default()
            }),
            ..AssistantData::default()
        };
        assert!(detect_truncation(&mut budget, &assistant, 4000).is_none());
    }

    #[test]
    fn usage_ratio_truncation_signal_is_taken_once() {
        let mut budget = TokenBudget::new(8192, 1024);
        budget.truncated_at = Some(2048);
        let assistant = AssistantData::default();
        assert!(detect_truncation(&mut budget, &assistant, 4000).is_some());
        assert!(budget.truncated_at.is_none());
        assert!(detect_truncation(&mut budget, &assistant, 4000).is_none());
    }
}

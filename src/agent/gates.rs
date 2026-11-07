//! Guards and settle gates: loop detection, todo/verify/review settlement.

use std::collections::VecDeque;

use crate::session::{EntryKind, Session};
use crate::tools::ToolResult;

use super::limits::RunProgress;

/// Signature-based loop detection over the last 24 calls.
#[derive(Debug)]
pub struct LoopGuard {
    history: VecDeque<String>,
    capacity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopDecision {
    Ok,
    Tripped { reason: String },
}

impl Default for LoopGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl LoopGuard {
    pub fn new() -> Self {
        Self {
            history: VecDeque::new(),
            capacity: 24,
        }
    }

    pub fn check_and_record(&mut self, signature: impl Into<String>) -> LoopDecision {
        let signature = signature.into();
        self.history.push_back(signature.clone());
        while self.history.len() > self.capacity {
            self.history.pop_front();
        }

        let repeated = self
            .history
            .iter()
            .rev()
            .take(24)
            .filter(|s| **s == signature)
            .count();
        if repeated >= 3 {
            return LoopDecision::Tripped {
                reason: format!(
                    "the same call ({signature}) ran {repeated} times in the last 24 calls. Stop repeating it; change approach or report the blocker."
                ),
            };
        }

        for period in 2..=4 {
            if self.history.len() < period * 3 {
                continue;
            }
            let window: Vec<&String> = self
                .history
                .iter()
                .rev()
                .take(period * 3)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let cycles = window
                .iter()
                .enumerate()
                .all(|(index, value)| *value == window[index % period]);
            if cycles {
                return LoopDecision::Tripped {
                    reason: format!(
                        "a {period}-call cycle repeated three times ({signature}). Stop the cycle and switch to a different strategy."
                    ),
                };
            }
        }
        LoopDecision::Ok
    }
}

/// Facts about the current turn, recomputed from the branch.
#[derive(Debug, Clone, Default)]
pub struct TurnFacts {
    pub mutations: usize,
    pub files: Vec<String>,
    pub todo_plan_present: bool,
    pub todo_all_done: bool,
    pub todo_final_done: bool,
    pub verify_configured: bool,
    pub unverified_mutations: bool,
    pub last_verify_passed: Option<bool>,
    pub verify_called: bool,
    pub review_called: bool,
    pub tool_calls: usize,
    pub findings_open: usize,
}

impl TurnFacts {
    pub fn multi_step(&self) -> bool {
        self.mutations >= 2 || self.files.len() >= 2
    }
}

/// Everything after the last user entry is the current turn.
pub fn analyze_turn(session: &Session, verify_configured: bool) -> TurnFacts {
    let branch = session.branch_entries();
    let turn_start = branch
        .iter()
        .rposition(|entry| matches!(entry.kind, EntryKind::User(_)))
        .map(|index| index + 1)
        .unwrap_or(0);
    let turn = &branch[turn_start..];
    let mut facts = TurnFacts {
        verify_configured,
        ..TurnFacts::default()
    };

    for entry in turn {
        match &entry.kind {
            EntryKind::Custom(custom) => match custom.kind.as_str() {
                "fs-change" => {
                    facts.mutations += 1;
                    if let Some(path) = custom.payload.get("path").and_then(|v| v.as_str())
                        && !facts.files.iter().any(|f| f == path)
                    {
                        facts.files.push(path.to_string());
                    }
                }
                "todo-state" => {
                    facts.todo_plan_present = custom
                        .payload
                        .get("tasks")
                        .and_then(|v| v.as_array())
                        .is_some_and(|tasks| !tasks.is_empty());
                    if let Some(tasks) = custom.payload.get("tasks").and_then(|v| v.as_array()) {
                        let mut all_done = !tasks.is_empty();
                        for task in tasks {
                            let done = task.get("status").and_then(|s| s.as_str()) == Some("completed");
                            all_done &= done;
                        }
                        facts.todo_all_done = all_done;
                        facts.todo_final_done = facts.todo_plan_present && all_done;
                    }
                }
                "verify-result" => {
                    facts.verify_called = true;
                    facts.last_verify_passed = custom
                        .payload
                        .get("passed")
                        .and_then(|v| v.as_bool());
                }
                _ => {}
            },
            EntryKind::Assistant(assistant) => {
                if let Some(calls) = &assistant.tool_calls {
                    facts.tool_calls += calls.len();
                    if calls.iter().any(|c| c.name == "verify") {
                        facts.verify_called = true;
                    }
                    if calls.iter().any(|c| c.name == "review") {
                        facts.review_called = true;
                    }
                }
            }
            _ => {}
        }
    }

    // A verify result is fresh when it appears after every fs-change.
    let mut last_mutation: Option<usize> = None;
    let mut last_verify: Option<(usize, bool)> = None;
    for (index, entry) in turn.iter().enumerate() {
        if let EntryKind::Custom(custom) = &entry.kind {
            if custom.kind == "fs-change" {
                last_mutation = Some(index);
            }
            if custom.kind == "verify-result" {
                last_verify = Some((
                    index,
                    custom
                        .payload
                        .get("passed")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                ));
            }
        }
    }
    facts.unverified_mutations = match (last_mutation, last_verify) {
        (Some(mutation), Some((verify_index, _))) => verify_index < mutation,
        (Some(_), None) => true,
        (None, _) => false,
    };
    facts
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleKind {
    Todo,
    Verify,
    Review,
}

#[derive(Debug, Clone)]
pub enum SettleDecision {
    Done,
    Continue { kind: SettleKind, message: String },
    Block { message: String },
}

/// Decide whether the turn settles or continues with a gate instruction.
pub fn settle(facts: &TurnFacts, progress: &mut RunProgress) -> SettleDecision {
    if facts.multi_step() && !facts.todo_plan_present {
        if progress.can_continue_settle() {
            progress.settle_continuations += 1;
            return SettleDecision::Continue {
                kind: SettleKind::Todo,
                message: format!(
                    "[localharness] todo gate: this run changed {} file(s) across {} mutation(s) but has no plan. Call the todo tool with action \"plan\" and the remaining tasks before continuing. Do not make more edits until the plan exists.",
                    facts.files.len(),
                    facts.mutations
                ),
            };
        }
        return SettleDecision::Block {
            message: "todo gate: this multi-step run still has no plan after two reminders. Add a plan with todo action \"plan\" in the next turn.".into(),
        };
    }

    let verify_needed = facts.verify_configured
        && (facts.unverified_mutations || (facts.todo_final_done && !facts.last_verify_passed.unwrap_or(false)));
    if verify_needed {
        if progress.verify_attempts < progress.limits.verify_attempts
            && progress.can_continue_settle()
        {
            progress.verify_attempts += 1;
            progress.settle_continuations += 1;
            return SettleDecision::Continue {
                kind: SettleKind::Verify,
                message: "[localharness] verify gate: files changed after the last passing verification. Run the verify tool now and fix any failure before settling.".into(),
            };
        }
        tracing::warn!("verify still failing with attempts exhausted; allowing settle with unverified state");
    }

    if facts.mutations > 0 && !facts.review_called && !progress.review_nudged {
        progress.review_nudged = true;
        return SettleDecision::Continue {
            kind: SettleKind::Review,
            message: "[localharness] review nudge: changes are in the working tree. Call the review tool once, address any real risk it reports, then finish.".into(),
        };
    }

    SettleDecision::Done
}

/// The todo edit gate: after two mutations or two files, further edits need a
/// plan. Returns a block result when the call must not run.
pub fn todo_edit_gate(
    tool: &str,
    mutations: usize,
    files: usize,
    plan_present: bool,
) -> Option<ToolResult> {
    if !matches!(tool, "edit" | "write" | "patch") {
        return None;
    }
    if plan_present || !(mutations >= 2 || files >= 2) {
        return None;
    }
    Some(ToolResult::err(
        "TODO_PLAN",
        format!(
            "this run has {mutations} mutation(s) across {files} file(s); a plan is now required. Call todo with {{\"action\":\"plan\",\"tasks\":[\"...\"]}} first, then retry this edit."
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BudgetConfig;
    use crate::session::*;

    #[test]
    fn loop_guard_trips_on_third_repeat() {
        let mut guard = LoopGuard::new();
        assert_eq!(guard.check_and_record("read:{\"path\":\"a\"}"), LoopDecision::Ok);
        assert_eq!(guard.check_and_record("read:{\"path\":\"a\"}"), LoopDecision::Ok);
        assert!(matches!(
            guard.check_and_record("read:{\"path\":\"a\"}"),
            LoopDecision::Tripped { .. }
        ));
    }

    #[test]
    fn loop_guard_trips_on_two_call_cycle() {
        let mut guard = LoopGuard::new();
        let mut tripped = false;
        for _ in 0..3 {
            tripped |= matches!(guard.check_and_record("a"), LoopDecision::Tripped { .. });
            tripped |= matches!(guard.check_and_record("b"), LoopDecision::Tripped { .. });
            if tripped {
                break;
            }
        }
        assert!(tripped);
    }

    fn limits() -> crate::agent::limits::RunLimits {
        crate::agent::limits::RunLimits::from(&BudgetConfig::default())
    }

    #[test]
    fn todo_gate_continues_then_blocks() {
        let mut progress = RunProgress::new(limits());
        let facts = TurnFacts {
            mutations: 2,
            files: vec!["a".into(), "b".into()],
            ..TurnFacts::default()
        };
        assert!(matches!(
            settle(&facts, &mut progress),
            SettleDecision::Continue { kind: SettleKind::Todo, .. }
        ));
        assert!(matches!(
            settle(&facts, &mut progress),
            SettleDecision::Continue { kind: SettleKind::Todo, .. }
        ));
        assert!(matches!(
            settle(&facts, &mut progress),
            SettleDecision::Block { .. }
        ));
    }

    #[test]
    fn verify_gate_is_capped_and_does_not_fight_review() {
        let mut progress = RunProgress::new(limits());
        let facts = TurnFacts {
            mutations: 1,
            files: vec!["a".into()],
            verify_configured: true,
            unverified_mutations: true,
            todo_plan_present: true,
            todo_all_done: true,
            todo_final_done: true,
            ..TurnFacts::default()
        };
        for _ in 0..2 {
            assert!(matches!(
                settle(&facts, &mut progress),
                SettleDecision::Continue { kind: SettleKind::Verify, .. }
            ));
        }
        assert!(matches!(
            settle(&facts, &mut progress),
            SettleDecision::Continue { kind: SettleKind::Review, .. }
        ));
        assert!(matches!(settle(&facts, &mut progress), SettleDecision::Done));
    }

    #[test]
    fn edit_gate_requires_plan_after_two_mutations() {
        assert!(todo_edit_gate("edit", 1, 1, false).is_none());
        assert!(todo_edit_gate("edit", 2, 1, false).is_some());
        assert!(todo_edit_gate("edit", 2, 2, true).is_none());
        assert!(todo_edit_gate("read", 5, 5, false).is_none());
    }

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
            },
            true,
        )
        .unwrap()
    }

    #[test]
    fn analyze_turn_ignores_previous_turns() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::User(UserData {
                content: "old turn".into(),
            }))
            .unwrap();
        session
            .append(EntryKind::Custom(
                CustomData::new("fs-change").with("path", "old.rs"),
            ))
            .unwrap();
        session
            .append(EntryKind::User(UserData {
                content: "new turn".into(),
            }))
            .unwrap();
        let facts = analyze_turn(&session, false);
        assert_eq!(facts.mutations, 0);
        assert!(facts.files.is_empty());
    }
}

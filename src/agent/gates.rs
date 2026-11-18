//! Guards and settle gates: loop detection, todo/verify/review settlement.

use std::collections::VecDeque;

use crate::session::{EntryKind, Session};
use crate::tools::ToolResult;

use super::limits::RunProgress;

/// Signature-based loop detection over recent calls. It only trips on genuine
/// repetition: four identical calls in a row, or an exact 2-8 call cycle
/// repeated four times in a row. Legitimate multi-step work (edit A, verify,
/// edit B, verify, edit A, verify) must not trip it: an exact argument cycle
/// means nothing changed in between, and a file change clears the history.
///
/// A read-only call is recorded but never trips it: polling `bg logs`, reading
/// many files, or fetching URLs is inspection, and the same arguments can
/// carry new output every time (a build that keeps writing). The identical
/// results that make a read loop useless are caught by the stall guard, which
/// compares fingerprints including the result.
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

    /// Record the call about to run and decide whether it continues a loop.
    /// `read_only` calls are history only: they break a run of identical
    /// mutations but cannot themselves be a loop worth stopping.
    pub fn check_and_record(
        &mut self,
        signature: impl Into<String>,
        read_only: bool,
    ) -> LoopDecision {
        let signature = signature.into();
        self.history.push_back(signature.clone());
        while self.history.len() > self.capacity {
            self.history.pop_front();
        }
        if read_only {
            return LoopDecision::Ok;
        }

        let consecutive = self
            .history
            .iter()
            .rev()
            .take_while(|s| **s == signature)
            .count();
        if consecutive >= 4 {
            return LoopDecision::Tripped {
                reason: format!(
                    "the same call ({signature}) ran {consecutive} times in a row. Stop repeating it; change approach or report the blocker."
                ),
            };
        }

        for period in 2..=8 {
            if self.history.len() < period * 4 {
                continue;
            }
            let window: Vec<&String> = self
                .history
                .iter()
                .rev()
                .take(period * 4)
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
                        "a {period}-call cycle repeated four times in a row ({signature}). Stop the cycle and switch to a different strategy."
                    ),
                };
            }
        }
        LoopDecision::Ok
    }

    /// Clear the accumulated history. The agent loop calls this after a file
    /// change or a passing verification, so genuine progress resets the guard
    /// and a long task's earlier calls cannot count toward a false loop.
    pub fn record_progress(&mut self) {
        self.history.clear();
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
    /// A run that changed three distinct files is multi-step. Counting edits
    /// instead made two follow-up edits to one file look like a whole project.
    pub fn multi_step(&self) -> bool {
        self.files.len() >= 3
    }

    /// Whether the settle path considers this run multi-step work without a
    /// plan. Informational only: the plan gate runs before an edit, never as
    /// the last action, so a finished turn is not reopened for ceremony.
    pub fn needs_plan(&self) -> bool {
        self.multi_step()
            && !self.todo_plan_present
            && !(self.last_verify_passed == Some(true) && !self.unverified_mutations)
    }
}

/// Facts for the current turn, using the last user entry as the boundary.
/// Synthetic gate continuations are user entries too, so the agent loop must
/// pass the real turn start id with [`analyze_turn_since`].
pub fn analyze_turn(session: &Session, verify_configured: bool) -> TurnFacts {
    analyze_turn_since(session, verify_configured, None)
}

/// Facts after the entry `since` (the user message that began the turn).
pub fn analyze_turn_since(
    session: &Session,
    verify_configured: bool,
    since: Option<&str>,
) -> TurnFacts {
    let branch = session.branch_entries();
    let turn_start = match since.and_then(|id| branch.iter().position(|entry| entry.id == id)) {
        Some(index) => index + 1,
        None => branch
            .iter()
            .rposition(|entry| matches!(entry.kind, EntryKind::User(_)))
            .map(|index| index + 1)
            .unwrap_or(0),
    };
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
                    // A pre-command snapshot from `bash` is recovery
                    // bookkeeping; it does not make the run a multi-file edit
                    // that needs a plan.
                    let from_bash =
                        custom.payload.get("bash").and_then(|value| value.as_bool()) == Some(true);
                    if !from_bash
                        && let Some(path) = custom.payload.get("path").and_then(|v| v.as_str())
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
                            let done =
                                task.get("status").and_then(|s| s.as_str()) == Some("completed");
                            all_done &= done;
                        }
                        facts.todo_all_done = all_done;
                        facts.todo_final_done = facts.todo_plan_present && all_done;
                    }
                }
                // A review the harness ran at settle counts like a call.
                "review-result" => facts.review_called = true,
                "verify-result" => {
                    facts.verify_called = true;
                    facts.last_verify_passed =
                        custom.payload.get("passed").and_then(|v| v.as_bool());
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

/// Which guard tools are present in the active profile. Guards whose tool is
/// absent are inert.
#[derive(Debug, Clone, Copy)]
pub struct GateTools {
    pub todo: bool,
    pub verify: bool,
    pub review: bool,
}

impl GateTools {
    pub fn from_names<'a>(names: impl Iterator<Item = &'a str>) -> Self {
        let names: Vec<&str> = names.collect();
        Self {
            todo: names.contains(&"todo"),
            verify: names.contains(&"verify"),
            review: names.contains(&"review"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettleKind {
    Verify,
    Review,
}

#[derive(Debug, Clone)]
pub enum SettleDecision {
    Done,
    Continue { kind: SettleKind, message: String },
    Block { message: String },
}

/// Which checks the harness should run itself when the model settles
/// (`[gates] auto_settle`). Mirrors [`settle`]: the todo gate goes first and
/// is left to it; verify runs while attempts remain; review runs once after
/// changes, when verify is not needed or passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoChecks {
    pub verify: bool,
    pub review: bool,
}

pub fn auto_checks(facts: &TurnFacts, progress: &RunProgress, tools: GateTools) -> AutoChecks {
    let verify = tools.verify
        && facts.verify_configured
        && (facts.unverified_mutations
            || (facts.todo_final_done && !facts.last_verify_passed.unwrap_or(false)))
        && progress.verify_attempts < progress.limits.verify_attempts;
    let review =
        tools.review && facts.mutations > 0 && !facts.review_called && !progress.review_nudged;
    AutoChecks { verify, review }
}

/// Decide whether the turn settles or continues with a gate instruction.
///
/// The plan gate is enforced before an edit ([`todo_edit_gate`]), not here: a
/// turn that reached settlement is finishing, and demanding a plan as the last
/// action only produces a plan written after the work.
pub fn settle(facts: &TurnFacts, progress: &mut RunProgress, tools: GateTools) -> SettleDecision {
    let verify_needed = tools.verify
        && facts.verify_configured
        && (facts.unverified_mutations
            || (facts.todo_final_done && !facts.last_verify_passed.unwrap_or(false)));
    if verify_needed {
        if progress.verify_attempts < progress.limits.verify_attempts
            && progress.can_continue_settle()
        {
            progress.verify_attempts += 1;
            progress.settle_continuations += 1;
            return SettleDecision::Continue {
                kind: SettleKind::Verify,
                message: "[totality] verify gate: files changed after the last passing verification. Run the verify tool now and fix any failure before settling.".into(),
            };
        }
        tracing::warn!(
            "verify still failing with attempts exhausted; allowing settle with unverified state"
        );
    }

    if tools.review && facts.mutations > 0 && !facts.review_called && !progress.review_nudged {
        progress.review_nudged = true;
        return SettleDecision::Continue {
            kind: SettleKind::Review,
            message: "[totality] review nudge: changes are in the working tree. Call the review tool once, address any real risk it reports, then finish.".into(),
        };
    }

    SettleDecision::Done
}

/// The todo edit gate: once a run has changed two distinct files, an edit to a
/// third distinct file needs a plan. Returns a block result when the call must
/// not run. Re-editing a file the run already changed is always allowed: the
/// gate counts logical files, not edits.
pub fn todo_edit_gate(
    tool: &str,
    target: Option<&str>,
    files: &[String],
    plan_present: bool,
) -> Option<ToolResult> {
    if !matches!(tool, "edit" | "write" | "patch") {
        return None;
    }
    if plan_present {
        return None;
    }
    let already_changed = target.is_some_and(|target| files.iter().any(|file| file == target));
    // Re-editing a file the run already changed never needs a plan, even
    // after the third file.
    if already_changed {
        return None;
    }
    let logical = files.len() + usize::from(target.is_some());
    if logical < 3 {
        return None;
    }
    Some(ToolResult::err(
        "TODO_PLAN",
        format!(
            "this run has changed {logical} distinct files; a plan is now required. Call todo with {{\"action\":\"plan\",\"tasks\":[\"...\"]}} first, then retry this edit. Editing a file already changed does not need a new plan."
        ),
    ))
}

/// One line for the result of a mutation that leaves the run one step from
/// [`todo_edit_gate`]: `files` counts this mutation. `None` when a plan exists
/// or the gate is still more than one step away.
pub fn todo_gate_notice(files: usize, plan_present: bool) -> Option<&'static str> {
    if plan_present || files < 2 {
        return None;
    }
    Some("[totality] the next edit to a new file needs a plan: call todo action=\"plan\" first.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BudgetConfig;
    use crate::session::*;

    #[test]
    fn verified_work_is_not_reopened_for_a_plan() {
        let mut facts = TurnFacts {
            mutations: 3,
            files: vec!["src/lib.rs".into(), "src/a.rs".into(), "src/b.rs".into()],
            verify_configured: true,
            ..TurnFacts::default()
        };
        // Unverified multi-step work still needs a plan.
        facts.unverified_mutations = true;
        assert!(facts.needs_plan());
        // A failing verification does too.
        facts.unverified_mutations = false;
        facts.last_verify_passed = Some(false);
        assert!(facts.needs_plan());
        // Every change verified and passing: done, no plan demanded.
        facts.last_verify_passed = Some(true);
        assert!(!facts.needs_plan());
    }

    #[test]
    fn loop_guard_trips_on_fourth_consecutive_repeat() {
        let mut guard = LoopGuard::new();
        let signature = "read:{\"path\":\"a\"}";
        for _ in 0..3 {
            assert_eq!(guard.check_and_record(signature, false), LoopDecision::Ok);
        }
        assert!(matches!(
            guard.check_and_record(signature, false),
            LoopDecision::Tripped { .. }
        ));
    }

    #[test]
    fn loop_guard_never_trips_on_a_read_only_call() {
        let mut guard = LoopGuard::new();
        // Polling the same job is inspection: the output can be new every time,
        // and the arguments alone are not a loop.
        for _ in 0..30 {
            assert_eq!(
                guard.check_and_record("bg:{\"action\":\"logs\"}", true),
                LoopDecision::Ok
            );
        }
    }

    #[test]
    fn a_read_between_identical_mutations_keeps_them_apart() {
        let mut guard = LoopGuard::new();
        for _ in 0..3 {
            assert_eq!(guard.check_and_record("edit:a", false), LoopDecision::Ok);
        }
        assert_eq!(
            guard.check_and_record("read:{\"path\":\"a\"}", true),
            LoopDecision::Ok
        );
        for _ in 0..3 {
            assert_eq!(guard.check_and_record("edit:a", false), LoopDecision::Ok);
        }
    }

    #[test]
    fn loop_guard_trips_on_four_repetitions_of_a_two_call_cycle() {
        let mut guard = LoopGuard::new();
        for cycle in 1..=4 {
            let mut tripped = false;
            tripped |= matches!(
                guard.check_and_record("a", false),
                LoopDecision::Tripped { .. }
            );
            tripped |= matches!(
                guard.check_and_record("b", false),
                LoopDecision::Tripped { .. }
            );
            if cycle < 4 {
                assert!(!tripped, "cycle {cycle} must not trip the guard");
            } else {
                assert!(tripped, "the fourth repetition must trip the guard");
            }
        }
    }

    #[test]
    fn loop_guard_trips_on_four_repetitions_of_a_longer_cycle() {
        let mut guard = LoopGuard::new();
        for cycle in 1..=4 {
            let mut tripped = false;
            for call in ["a", "b", "c", "d", "e"] {
                tripped |= matches!(
                    guard.check_and_record(call, false),
                    LoopDecision::Tripped { .. }
                );
            }
            if cycle < 4 {
                assert!(!tripped, "cycle {cycle} must not trip the guard");
            } else {
                assert!(tripped, "the fourth repetition must trip the guard");
            }
        }
    }

    #[test]
    fn loop_guard_allows_distinct_edit_verify_work() {
        let mut guard = LoopGuard::new();
        for call in [
            "edit:{\"path\":\"a.rs\",\"old\":\"x\"}",
            "verify:{}",
            "edit:{\"path\":\"b.rs\",\"old\":\"y\"}",
            "verify:{}",
            "edit:{\"path\":\"a.rs\",\"old\":\"z\"}",
            "verify:{}",
            "edit:{\"path\":\"c.rs\",\"old\":\"w\"}",
            "verify:{}",
        ] {
            assert_eq!(guard.check_and_record(call, false), LoopDecision::Ok);
        }
    }

    #[test]
    fn loop_guard_progress_resets_history() {
        let mut guard = LoopGuard::new();
        for _ in 0..3 {
            assert_eq!(guard.check_and_record("same", false), LoopDecision::Ok);
        }
        guard.record_progress();
        for _ in 0..3 {
            assert_eq!(guard.check_and_record("same", false), LoopDecision::Ok);
        }
        assert!(matches!(
            guard.check_and_record("same", false),
            LoopDecision::Tripped { .. }
        ));
    }

    fn limits() -> crate::agent::limits::RunLimits {
        crate::agent::limits::RunLimits::from(&BudgetConfig::default())
    }

    #[test]
    fn a_finished_turn_is_not_reopened_for_a_plan() {
        // Three changed files without a plan is a warning at settlement, not
        // a continuation; the plan gate lives before the next edit.
        let mut progress = RunProgress::new(limits());
        let facts = TurnFacts {
            mutations: 3,
            files: vec!["a".into(), "b".into(), "c".into()],
            ..TurnFacts::default()
        };
        assert!(facts.needs_plan());
        assert!(matches!(
            settle(
                &facts,
                &mut progress,
                GateTools {
                    todo: true,
                    verify: false,
                    review: false
                }
            ),
            SettleDecision::Done
        ));
        assert_eq!(progress.settle_continuations, 0);
    }

    #[test]
    fn the_edit_gate_counts_distinct_files_not_edits() {
        let files = |names: &[&str]| {
            names
                .iter()
                .map(|name| name.to_string())
                .collect::<Vec<_>>()
        };
        // Two edits to one file are one change: the second is not a new file.
        assert!(todo_edit_gate("edit", Some("a.rs"), &files(&["a.rs"]), false).is_none());
        assert!(todo_edit_gate("edit", Some("b.rs"), &files(&["a.rs"]), false).is_none());
        // The edit that would touch a third distinct file needs a plan.
        assert!(todo_edit_gate("edit", Some("c.rs"), &files(&["a.rs", "b.rs"]), false).is_some());
        // Editing a file already changed is always allowed.
        assert!(todo_edit_gate("edit", Some("a.rs"), &files(&["a.rs", "b.rs"]), false).is_none());
        // A plan, or a read-only tool, is never blocked.
        assert!(todo_edit_gate("edit", Some("c.rs"), &files(&["a.rs", "b.rs"]), true).is_none());
        assert!(todo_edit_gate("read", Some("c.rs"), &files(&["a.rs", "b.rs"]), false).is_none());
        assert!(todo_gate_notice(2, false).is_some());
        assert!(todo_gate_notice(2, true).is_none());
        assert!(todo_gate_notice(1, false).is_none());
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
                settle(
                    &facts,
                    &mut progress,
                    GateTools {
                        todo: true,
                        verify: true,
                        review: true
                    }
                ),
                SettleDecision::Continue {
                    kind: SettleKind::Verify,
                    ..
                }
            ));
        }
        assert!(matches!(
            settle(
                &facts,
                &mut progress,
                GateTools {
                    todo: true,
                    verify: true,
                    review: true
                }
            ),
            SettleDecision::Continue {
                kind: SettleKind::Review,
                ..
            }
        ));
        assert!(matches!(
            settle(
                &facts,
                &mut progress,
                GateTools {
                    todo: true,
                    verify: true,
                    review: true
                }
            ),
            SettleDecision::Done
        ));
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
                redact: true,
                context: None,
                permissions: None,
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
                attachments: Vec::new(),
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
                attachments: Vec::new(),
                content: "new turn".into(),
            }))
            .unwrap();
        let facts = analyze_turn(&session, false);
        assert_eq!(facts.mutations, 0);
        assert!(facts.files.is_empty());
    }
}

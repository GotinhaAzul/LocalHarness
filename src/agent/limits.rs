//! Run checkpoints, wall clock, and stall detection.
//!
//! Tool-call, model-call, and wall-clock limits are checkpoints, not hard
//! stops. When one is reached and the run produced new information, a file
//! change, or a passing verification recently, the allowance is renewed and
//! the turn continues. Only a checkpoint with no recent progress — or the
//! exact-repetition guards — ends the turn. This lets a real long task run as
//! long as it keeps making progress without letting a looping model do so.

use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use crate::config::BudgetConfig;

#[derive(Debug, Clone)]
pub struct RunLimits {
    /// Checkpoint interval, not a hard cap.
    pub max_tool_calls: u32,
    /// Checkpoint interval, not a hard cap.
    pub max_model_calls: u32,
    /// Checkpoint interval, not a hard cap.
    pub max_wall: Duration,
    pub stall_calls: u32,
    pub verify_attempts: u32,
}

impl From<&BudgetConfig> for RunLimits {
    fn from(config: &BudgetConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls.max(1),
            max_model_calls: config.max_model_calls.max(1),
            max_wall: Duration::from_secs(u64::from(config.max_wall_minutes.max(1)) * 60),
            stall_calls: config.stall_calls.max(1),
            verify_attempts: config.verify_attempts,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    ToolBudget,
    ModelBudget,
    WallClock,
    Stalled,
    Cancelled,
}

impl StopReason {
    /// Code recorded in the session and the headless JSON stream.
    pub fn code(&self) -> &'static str {
        match self {
            StopReason::ToolBudget => "budget_tools",
            StopReason::ModelBudget => "budget_models",
            StopReason::WallClock => "budget_wall",
            StopReason::Stalled => "stalled",
            StopReason::Cancelled => "cancelled",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            StopReason::ToolBudget => {
                "the tool-call checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::ModelBudget => {
                "the model-call checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::WallClock => {
                "the wall-clock checkpoint found no recent progress; the run is repeating without changing anything"
            }
            StopReason::Stalled => {
                "the last calls produced no new information or file change; stop and report the last known state, or change approach in a new turn"
            }
            StopReason::Cancelled => "cancelled by the user",
        }
    }
}

/// Result of asking a checkpoint whether the turn may continue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetCheck {
    /// Not at a checkpoint yet.
    Ok,
    /// The checkpoint was reached while the run was still productive; the
    /// allowance was renewed and the turn continues.
    Renewed(StopReason),
    /// The checkpoint was reached with no recent progress. This is the only
    /// budget outcome that ends a turn.
    Exhausted(StopReason),
}

#[derive(Debug)]
pub struct RunProgress {
    pub limits: RunLimits,
    /// Wall clock for the whole turn, used for reporting only.
    pub started_at: Instant,
    /// Cumulative counters for reporting and messages.
    pub tool_calls: u32,
    pub model_calls: u32,
    /// Current checkpoint allowance.
    epoch_tool_calls: u32,
    epoch_model_calls: u32,
    epoch_started_at: Instant,
    /// How many times a productive checkpoint renewed the allowance.
    pub renewals: u32,
    /// Cumulative tool-call index of the last result that produced usable new
    /// information, a file change, or a passing verification.
    last_progress_call: Option<u32>,
    last_progress_at: Option<Instant>,
    /// Result digests already seen this turn. A repeated result is not new
    /// information even when the call arguments differ.
    seen_results: HashSet<u64>,
    pub stall: StallTracker,
    pub settle_continuations: u32,
    pub verify_attempts: u32,
    pub review_nudged: bool,
    pub context_retry_used: bool,
    pub last_stop: Option<StopReason>,
    /// Harness nudges spent on broken output this turn.
    pub output_recoveries: u32,
    /// Whether the user was told that tool calls are arriving as text.
    pub text_calls_noticed: bool,
    /// Whether the exact tokenizer check already forced a compaction. Reset
    /// after every successful model reply so a later context fill can check
    /// again.
    pub presend_compaction_used: bool,
}

impl RunProgress {
    pub fn new(limits: RunLimits) -> Self {
        let stall_calls = limits.stall_calls;
        let now = Instant::now();
        Self {
            limits,
            started_at: now,
            tool_calls: 0,
            model_calls: 0,
            epoch_tool_calls: 0,
            epoch_model_calls: 0,
            epoch_started_at: now,
            renewals: 0,
            last_progress_call: None,
            last_progress_at: None,
            seen_results: HashSet::new(),
            stall: StallTracker::new(stall_calls),
            settle_continuations: 0,
            verify_attempts: 0,
            review_nudged: false,
            context_retry_used: false,
            last_stop: None,
            output_recoveries: 0,
            text_calls_noticed: false,
            presend_compaction_used: false,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Check the model-call and wall-clock checkpoint before a model call.
    pub fn model_checkpoint(&mut self) -> BudgetCheck {
        let model_hit = self.epoch_model_calls >= self.limits.max_model_calls;
        let wall_hit = self.epoch_started_at.elapsed() >= self.limits.max_wall;
        if !model_hit && !wall_hit {
            return BudgetCheck::Ok;
        }
        if model_hit {
            if self.recent_call_progress() {
                return self.renew(StopReason::ModelBudget);
            }
            return BudgetCheck::Exhausted(StopReason::ModelBudget);
        }
        if self.recent_time_progress() {
            self.renew(StopReason::WallClock)
        } else {
            BudgetCheck::Exhausted(StopReason::WallClock)
        }
    }

    pub fn model_call_started(&mut self) {
        self.model_calls += 1;
        self.epoch_model_calls += 1;
    }

    /// Check the tool-call checkpoint before a batch of `n` calls. A batch is
    /// allowed to cross the checkpoint: the allowance is renewed first when
    /// the run is productive.
    pub fn tool_checkpoint(&mut self, n: u32) -> BudgetCheck {
        if self.epoch_tool_calls.saturating_add(n) <= self.limits.max_tool_calls {
            return BudgetCheck::Ok;
        }
        if self.recent_call_progress() {
            self.renew(StopReason::ToolBudget)
        } else {
            BudgetCheck::Exhausted(StopReason::ToolBudget)
        }
    }

    /// Record a tool result. `result` is the content shown to the model; a
    /// result content that was already seen this turn is a repeat, not new
    /// information, even when the call arguments differ.
    pub fn record_tool_result(
        &mut self,
        fingerprint: &str,
        result: &str,
        is_error: bool,
        changed_file: bool,
        verified_ok: bool,
    ) {
        self.tool_calls += 1;
        self.epoch_tool_calls += 1;
        self.stall.observe(fingerprint, changed_file, verified_ok);
        let novel = !is_error && self.seen_results.insert(result_digest(result));
        if changed_file || verified_ok || novel {
            self.last_progress_call = Some(self.tool_calls);
            self.last_progress_at = Some(Instant::now());
        }
    }

    pub fn stalled(&self) -> bool {
        self.stall.stalled()
    }

    /// A successful reply breaks a context-error episode: the next compaction
    /// or context retry gets its own allowance.
    pub fn recovered_context(&mut self) {
        self.context_retry_used = false;
        self.presend_compaction_used = false;
    }

    /// At most two settle continuations per turn, shared by verify and todo.
    pub fn can_continue_settle(&self) -> bool {
        self.settle_continuations < 2
    }

    /// Message for a checkpoint that ended the turn.
    pub fn checkpoint_message(&self, reason: StopReason) -> String {
        let window = self.limits.stall_calls;
        let no_progress = format!(
            "no new result, file change, or passing verification in the last {window} call(s)"
        );
        match reason {
            StopReason::ToolBudget => format!(
                "tool-call checkpoint: {} calls in this allowance, {no_progress}. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.epoch_tool_calls
            ),
            StopReason::ModelBudget => format!(
                "model-call checkpoint: {} model calls in this allowance, {no_progress}. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.epoch_model_calls
            ),
            StopReason::WallClock => format!(
                "wall-clock checkpoint: {} minutes in this allowance, no new result, file change, or passing verification. The run is repeating without progress; summarize the last known state or start a new turn with a different approach.",
                self.limits.max_wall.as_secs() / 60
            ),
            StopReason::Stalled => {
                "the last calls produced no new information or file change; stop and report the last known state, or change approach in a new turn".into()
            }
            StopReason::Cancelled => "cancelled by the user".into(),
        }
    }

    /// User-facing notice when a productive checkpoint renews the allowance.
    pub fn renewal_message(&self, trigger: StopReason) -> String {
        format!(
            "budget checkpoint renewed ({}): {} tool calls, {} model calls, {} min used; the run is still producing new results or file changes, so the turn continues.",
            trigger.code(),
            self.tool_calls,
            self.model_calls,
            self.elapsed().as_secs() / 60
        )
    }

    fn recent_call_progress(&self) -> bool {
        self.last_progress_call
            .is_some_and(|at| self.tool_calls.saturating_sub(at) < self.limits.stall_calls)
    }

    fn recent_time_progress(&self) -> bool {
        let window = (self.limits.max_wall / 2).max(Duration::from_secs(60));
        self.last_progress_at
            .is_some_and(|at| at.elapsed() < window)
    }

    fn renew(&mut self, trigger: StopReason) -> BudgetCheck {
        self.epoch_tool_calls = 0;
        self.epoch_model_calls = 0;
        self.epoch_started_at = Instant::now();
        self.renewals += 1;
        // A renewal must be earned again by new work in the next allowance.
        self.last_progress_call = None;
        self.last_progress_at = None;
        BudgetCheck::Renewed(trigger)
    }
}

fn result_digest(result: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    result.hash(&mut hasher);
    hasher.finish()
}

#[derive(Debug)]
pub struct StallTracker {
    needed: u32,
    strikes: u32,
    last: Option<String>,
}

impl StallTracker {
    pub fn new(needed: u32) -> Self {
        Self {
            needed,
            strikes: 0,
            last: None,
        }
    }

    pub fn observe(&mut self, fingerprint: &str, changed_file: bool, verified_ok: bool) {
        if changed_file || verified_ok {
            self.strikes = 0;
            self.last = Some(fingerprint.to_string());
            return;
        }
        if self.last.as_deref() == Some(fingerprint) {
            self.strikes += 1;
        } else {
            self.strikes = 1;
            self.last = Some(fingerprint.to_string());
        }
    }

    pub fn stalled(&self) -> bool {
        self.strikes >= self.needed
    }

    pub fn strikes(&self) -> u32 {
        self.strikes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> RunLimits {
        RunLimits {
            max_tool_calls: 10,
            max_model_calls: 4,
            max_wall: Duration::from_secs(600),
            stall_calls: 3,
            verify_attempts: 2,
        }
    }

    fn progress_with_one_result(progress: &mut RunProgress, result: &str) {
        progress.record_tool_result(result, result, false, false, false);
    }

    #[test]
    fn productive_model_checkpoint_renews() {
        let mut progress = RunProgress::new(limits());
        for _ in 0..4 {
            progress.model_call_started();
        }
        progress_with_one_result(&mut progress, "new facts");
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Renewed(StopReason::ModelBudget)
        );
        assert_eq!(progress.renewals, 1);
        assert_eq!(progress.model_calls, 4, "cumulative remains for reporting");
        assert_eq!(progress.epoch_model_calls, 0);
        assert_eq!(progress.model_checkpoint(), BudgetCheck::Ok);
    }

    #[test]
    fn checkpoint_without_progress_stops() {
        let mut progress = RunProgress::new(limits());
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
        assert_eq!(progress.renewals, 0);
    }

    #[test]
    fn repeated_results_are_not_progress() {
        let mut progress = RunProgress::new(limits());
        progress.record_tool_result("a", "same", false, false, false);
        for index in 1..6 {
            progress.record_tool_result(&format!("call-{index}"), "same", false, false, false);
        }
        // Four model calls after five repeats of the same result.
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
    }

    #[test]
    fn stale_progress_does_not_renew() {
        let mut progress = RunProgress::new(limits());
        progress_with_one_result(&mut progress, "useful");
        for index in 0..6 {
            progress.record_tool_result(
                &format!("noise-{index}"),
                &format!("noise-{index}"),
                true,
                false,
                false,
            );
        }
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Exhausted(StopReason::ModelBudget)
        );
    }

    #[test]
    fn tool_checkpoint_renews_for_a_productive_batch() {
        let mut progress = RunProgress::new(limits());
        for calls in 0..9 {
            progress_with_one_result(&mut progress, &format!("result {calls}"));
        }
        assert_eq!(
            progress.tool_checkpoint(2),
            BudgetCheck::Renewed(StopReason::ToolBudget)
        );
        assert_eq!(progress.epoch_tool_calls, 0);
        // The batch itself may cross the new checkpoint.
        assert_eq!(
            progress.tool_checkpoint(20),
            BudgetCheck::Exhausted(StopReason::ToolBudget)
        );
    }

    #[test]
    fn file_change_is_progress_even_when_the_result_repeats() {
        let mut progress = RunProgress::new(limits());
        progress.record_tool_result("f", "ok", false, true, false);
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(
            progress.model_checkpoint(),
            BudgetCheck::Renewed(StopReason::ModelBudget)
        );
    }

    #[test]
    fn stall_needs_consecutive_identical_fingerprints() {
        let mut progress = RunProgress::new(limits());
        progress.record_tool_result("same", "r", false, false, false);
        progress.record_tool_result("same", "r", false, false, false);
        progress.record_tool_result("same", "r", false, false, false);
        assert!(progress.stalled());
        progress.record_tool_result("different", "r", false, false, false);
        assert!(!progress.stalled());
    }

    #[test]
    fn file_change_resets_stall() {
        let mut progress = RunProgress::new(limits());
        for _ in 0..2 {
            progress.record_tool_result("same", "r", false, false, false);
        }
        progress.record_tool_result("same", "r", false, true, false);
        assert!(!progress.stalled());
    }
}

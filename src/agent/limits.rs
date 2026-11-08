//! Run budgets, wall clock, and stall detection.

use std::time::{Duration, Instant};

use crate::config::BudgetConfig;

#[derive(Debug, Clone)]
pub struct RunLimits {
    pub max_tool_calls: u32,
    pub max_model_calls: u32,
    pub max_wall: Duration,
    pub stall_calls: u32,
    pub wrap_up_at: f32,
    pub verify_attempts: u32,
}

impl From<&BudgetConfig> for RunLimits {
    fn from(config: &BudgetConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls.max(1),
            max_model_calls: config.max_model_calls.max(1),
            max_wall: Duration::from_secs(u64::from(config.max_wall_minutes.max(1)) * 60),
            stall_calls: config.stall_calls.max(1),
            wrap_up_at: config.wrap_up_at.clamp(0.1, 1.0),
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
    pub fn message(&self) -> &'static str {
        match self {
            StopReason::ToolBudget => {
                "tool-call budget exhausted; finish with the current evidence, or start a new turn to continue"
            }
            StopReason::ModelBudget => {
                "model-call budget exhausted; summarize what is done and what remains"
            }
            StopReason::WallClock => {
                "wall-clock budget exhausted; summarize what is done and what remains"
            }
            StopReason::Stalled => {
                "the last calls produced no new information or file change; stop and report the last known state, or change approach in a new turn"
            }
            StopReason::Cancelled => "cancelled by the user",
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            StopReason::ToolBudget => "budget_tools",
            StopReason::ModelBudget => "budget_models",
            StopReason::WallClock => "budget_wall",
            StopReason::Stalled => "stalled",
            StopReason::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug)]
pub struct RunProgress {
    pub limits: RunLimits,
    pub started_at: Instant,
    pub tool_calls: u32,
    pub model_calls: u32,
    pub wrapped_up: bool,
    pub stall: StallTracker,
    pub settle_continuations: u32,
    pub verify_attempts: u32,
    pub review_nudged: bool,
    pub context_retry_used: bool,
    pub last_stop: Option<StopReason>,
}

impl RunProgress {
    pub fn new(limits: RunLimits) -> Self {
        let stall_calls = limits.stall_calls;
        Self {
            limits,
            started_at: Instant::now(),
            tool_calls: 0,
            model_calls: 0,
            wrapped_up: false,
            stall: StallTracker::new(stall_calls),
            settle_continuations: 0,
            verify_attempts: 0,
            review_nudged: false,
            context_retry_used: false,
            last_stop: None,
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Stop reason that prevents another model call, if any.
    pub fn stop_before_model(&self) -> Option<StopReason> {
        if self.model_calls >= self.limits.max_model_calls {
            return Some(StopReason::ModelBudget);
        }
        if self.elapsed() >= self.limits.max_wall {
            return Some(StopReason::WallClock);
        }
        None
    }

    pub fn model_call_started(&mut self) {
        self.model_calls += 1;
    }

    /// Whether the remaining tool budget covers `n` more calls.
    pub fn tools_available(&self, n: u32) -> bool {
        self.tool_calls + n <= self.limits.max_tool_calls
    }

    pub fn record_tool_result(&mut self, fingerprint: &str, changed_file: bool, verified_ok: bool) {
        self.tool_calls += 1;
        self.stall.observe(fingerprint, changed_file, verified_ok);
    }

    pub fn stalled(&self) -> bool {
        self.stall.stalled()
    }

    /// True once, when the wrap-up threshold is crossed.
    pub fn should_wrap_up(&mut self) -> bool {
        if self.wrapped_up {
            return false;
        }
        let tool_ratio = f32::from(self.tool_calls as u16) / self.limits.max_tool_calls as f32;
        let model_ratio = f32::from(self.model_calls as u16) / self.limits.max_model_calls as f32;
        if tool_ratio >= self.limits.wrap_up_at || model_ratio >= self.limits.wrap_up_at {
            self.wrapped_up = true;
            return true;
        }
        false
    }

    /// At most two settle continuations per turn, shared by verify and todo.
    pub fn can_continue_settle(&self) -> bool {
        self.settle_continuations < 2
    }

    pub fn wrap_up_message(&self) -> String {
        format!(
            "[localharness] budget: {} of {} tool calls and {} of {} model calls used. Wrap up now: finish the current step, run verification if configured, and report what changed and what remains. Do not start new exploration.",
            self.tool_calls,
            self.limits.max_tool_calls,
            self.model_calls,
            self.limits.max_model_calls
        )
    }
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
            wrap_up_at: 0.8,
            verify_attempts: 2,
        }
    }

    #[test]
    fn model_and_tool_caps_are_enforced() {
        let mut progress = RunProgress::new(limits());
        assert!(progress.stop_before_model().is_none());
        for _ in 0..4 {
            progress.model_call_started();
        }
        assert_eq!(progress.stop_before_model(), Some(StopReason::ModelBudget));
        for _ in 0..10 {
            progress.record_tool_result("f", false, false);
        }
        assert!(!progress.tools_available(1));
        assert!(progress.tools_available(0));
    }

    #[test]
    fn stall_needs_consecutive_identical_fingerprints() {
        let mut progress = RunProgress::new(limits());
        progress.record_tool_result("same", false, false);
        progress.record_tool_result("same", false, false);
        progress.record_tool_result("same", false, false);
        assert!(progress.stalled());
        progress.record_tool_result("different", false, false);
        assert!(!progress.stalled());
    }

    #[test]
    fn file_change_resets_stall() {
        let mut progress = RunProgress::new(limits());
        for _ in 0..2 {
            progress.record_tool_result("same", false, false);
        }
        progress.record_tool_result("same", true, false);
        assert!(!progress.stalled());
    }

    #[test]
    fn wrap_up_fires_once_at_threshold() {
        let mut progress = RunProgress::new(limits());
        for _ in 0..8 {
            progress.record_tool_result("f", false, false);
        }
        assert!(progress.should_wrap_up());
        assert!(!progress.should_wrap_up());
    }
}

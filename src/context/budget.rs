//! Token accounting, calibration, and the compaction threshold.

use crate::provider::ChatMessage;

pub const DEFAULT_CHARS_PER_TOKEN: f64 = 3.5;
pub const SAFETY_MARGIN_MIN: u32 = 512;

#[derive(Debug, Clone)]
pub struct TokenBudget {
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub safety_margin: u32,
    pub chars_per_token: f64,
    pub last_prompt_tokens: Option<u32>,
    pub last_estimation_error: Option<i64>,
}

impl TokenBudget {
    pub fn new(context_window: u32, max_output_tokens: u32) -> Self {
        let context_window = context_window.max(1);
        let safety = ((context_window as f64 * 0.05).round() as u32).max(SAFETY_MARGIN_MIN);
        Self {
            context_window,
            max_output_tokens,
            safety_margin: safety,
            chars_per_token: DEFAULT_CHARS_PER_TOKEN,
            last_prompt_tokens: None,
            last_estimation_error: None,
        }
    }

    /// `context_window - max_output_tokens - safety_margin`, floored.
    pub fn usable_prompt(&self) -> u32 {
        self.context_window
            .saturating_sub(self.max_output_tokens)
            .saturating_sub(self.safety_margin)
            .max(1)
    }

    /// Compaction starts at 80% of the usable prompt.
    pub fn compaction_threshold(&self) -> u32 {
        ((self.usable_prompt() as f64) * 0.8).round() as u32
    }

    pub fn chars_for(&self, tokens: u32) -> usize {
        (tokens as f64 * self.chars_per_token).round() as usize
    }

    pub fn estimate_text(&self, text: &str) -> u32 {
        ((text.chars().count() as f64) / self.chars_per_token).ceil() as u32
    }

    pub fn estimate_messages(&self, messages: &[ChatMessage]) -> u32 {
        messages
            .iter()
            .map(|m| self.estimate_text(m.role()) + self.estimate_char_len(m.char_len()))
            .sum::<u32>()
            + 8
    }

    pub fn estimate_char_len(&self, chars: usize) -> u32 {
        ((chars as f64) / self.chars_per_token).ceil() as u32
    }

    /// Correct `chars_per_token` from a measured prompt size. Keeps a slow
    /// correction so one odd request does not skew the estimate.
    pub fn calibrate(&mut self, estimated_chars: usize, actual_prompt_tokens: u32) {
        if actual_prompt_tokens < 16 || estimated_chars == 0 {
            return;
        }
        let measured = estimated_chars as f64 / f64::from(actual_prompt_tokens);
        let measured = measured.clamp(1.0, 10.0);
        self.chars_per_token = (self.chars_per_token * 0.5 + measured * 0.5).clamp(1.0, 10.0);
        self.last_prompt_tokens = Some(actual_prompt_tokens);
    }

    pub fn observe_usage(&mut self, prompt_tokens: u32, estimated_tokens: u32) {
        if prompt_tokens > 0 {
            self.last_prompt_tokens = Some(prompt_tokens);
            self.last_estimation_error =
                Some(i64::from(prompt_tokens) - i64::from(estimated_tokens));
        }
    }

    pub fn exceeds_compaction_threshold(&self, used: u32) -> bool {
        used >= self.compaction_threshold()
    }

    pub fn percent(&self, used: u32) -> f32 {
        (used as f32 / self.usable_prompt() as f32) * 100.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_prompt_subtracts_output_and_margin() {
        let budget = TokenBudget::new(32768, 4096);
        assert_eq!(budget.safety_margin, 1638);
        assert_eq!(budget.usable_prompt(), 32768 - 4096 - 1638);
    }

    #[test]
    fn small_context_gets_minimum_margin() {
        let budget = TokenBudget::new(4096, 2048);
        assert_eq!(budget.safety_margin, 512);
        assert_eq!(budget.usable_prompt(), 1536);
    }

    #[test]
    fn calibration_moves_ratio_toward_measured() {
        let mut budget = TokenBudget::new(32768, 4096);
        assert_eq!(budget.chars_per_token, DEFAULT_CHARS_PER_TOKEN);
        budget.calibrate(4000, 1000); // 4 chars/token measured
        assert!(budget.chars_per_token > DEFAULT_CHARS_PER_TOKEN);
    }

    #[test]
    fn threshold_is_eighty_percent() {
        let budget = TokenBudget::new(10000, 1000);
        // usable = 10000 - 1000 - 512 = 8488, threshold = 6790
        assert_eq!(budget.compaction_threshold(), 6790);
        assert!(!budget.exceeds_compaction_threshold(6789));
        assert!(budget.exceeds_compaction_threshold(6790));
    }
}

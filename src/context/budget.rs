//! Token accounting, calibration, and the compaction threshold.

use crate::provider::{ChatMessage, ToolSpec, tools_json};

pub const DEFAULT_CHARS_PER_TOKEN: f64 = 3.5;
/// JSON keys, quotes, and type names tokenize denser than prose. Tool schemas
/// stay on this ratio even after the prose calibration moves.
pub const JSON_CHARS_PER_TOKEN: f64 = 2.6;
pub const SAFETY_MARGIN_MIN: u32 = 512;
/// Real text, code, and JSON all tokenize well under this many characters per
/// token. A measurement above it means the server dropped part of the prompt.
const TRUNCATION_CHARS_PER_TOKEN: f64 = 6.0;
/// Below this size a prompt is too small to judge.
const TRUNCATION_MIN_CHARS: usize = 12_000;

/// Token estimate for the part of the prompt that exists before the user
/// types anything: system prompt, tool schemas, and repository brief.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StaticPrompt {
    pub system: u32,
    pub tools: u32,
    pub repo: u32,
    pub total: u32,
}

impl StaticPrompt {
    pub fn new(system: u32, tools: u32, repo: u32) -> Self {
        Self {
            system,
            tools,
            repo,
            total: system.saturating_add(tools).saturating_add(repo),
        }
    }

    /// The largest component and its name, for a refusal message.
    pub fn largest(&self) -> (&'static str, u32) {
        let mut name = "system prompt";
        let mut value = self.system;
        if self.tools > value {
            name = "tool schemas";
            value = self.tools;
        }
        if self.repo > value {
            name = "repository brief";
            value = self.repo;
        }
        (name, value)
    }

    pub fn percent_of(&self, usable: u32) -> f32 {
        if usable == 0 {
            return 100.0;
        }
        (self.total as f32 / usable as f32) * 100.0
    }
}

/// Fixed cost of the request's `tools` array, computed once when `Core` is
/// built. `tokens` is the engine's `/tokenize` count when available, otherwise
/// the JSON-density estimate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolSpecCost {
    pub chars: usize,
    pub tokens: u32,
}

impl ToolSpecCost {
    /// Price the serialized tools array once, with the JSON ratio. An empty
    /// array is not sent, so it costs nothing.
    pub fn from_json(json: &str, budget: &TokenBudget) -> Self {
        if json.is_empty() {
            return Self::default();
        }
        Self {
            chars: json.len(),
            tokens: budget.estimate_json_char_len(json.len()),
        }
    }

    /// Replace the estimate with an exact `/tokenize` count.
    pub fn with_tokens(mut self, tokens: u32) -> Self {
        self.tokens = tokens;
        self
    }

    /// Serialize and price a tool list. Used by tests and callers that do not
    /// already hold the request JSON.
    pub fn estimate(tool_specs: &[ToolSpec], budget: &TokenBudget) -> Self {
        if tool_specs.is_empty() {
            return Self::default();
        }
        Self::from_json(&tools_json(tool_specs), budget)
    }
}

#[derive(Debug, Clone)]
pub struct TokenBudget {
    pub context_window: u32,
    pub max_output_tokens: u32,
    pub safety_margin: u32,
    pub chars_per_token: f64,
    pub last_prompt_tokens: Option<u32>,
    pub last_estimation_error: Option<i64>,
    /// Set when a usage report shows the server silently truncated the
    /// prompt; holds the server's apparent prompt limit. Taken by the loop.
    pub truncated_at: Option<u32>,
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
            truncated_at: None,
        }
    }

    /// The same budget against a smaller real window, keeping calibration.
    /// Output is capped at a quarter of the window so a small server window
    /// still leaves room for the prompt.
    pub fn shrunk_to(&self, window: u32) -> Self {
        let mut shrunk = Self::new(window, self.max_output_tokens.min(window / 4));
        shrunk.chars_per_token = self.chars_per_token;
        shrunk
    }

    /// The same budget against a different window (larger or smaller),
    /// keeping calibration and the configured output reservation.
    pub fn resized_to(&self, window: u32, max_output_tokens: u32) -> Self {
        let mut resized = Self::new(window, max_output_tokens.min(window / 4));
        resized.chars_per_token = self.chars_per_token;
        resized
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

    /// JSON estimate for tool schemas, pinned to [`JSON_CHARS_PER_TOKEN`] so
    /// prose calibration cannot move it.
    pub fn estimate_json_char_len(&self, chars: usize) -> u32 {
        ((chars as f64) / JSON_CHARS_PER_TOKEN).ceil() as u32
    }

    /// Correct `chars_per_token` from a measured prompt size. Keeps a slow
    /// correction so one odd request does not skew the estimate.
    pub fn calibrate(&mut self, estimated_chars: usize, actual_prompt_tokens: u32) {
        if actual_prompt_tokens < 16 || estimated_chars == 0 {
            return;
        }
        let measured = estimated_chars as f64 / f64::from(actual_prompt_tokens);
        if measured > TRUNCATION_CHARS_PER_TOKEN && estimated_chars >= TRUNCATION_MIN_CHARS {
            // The server truncated the prompt (Ollama does this silently at
            // its default num_ctx). Learning from this sample would make the
            // harness think text is cheaper and grow the prompt further.
            self.truncated_at = Some(actual_prompt_tokens);
            return;
        }
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
    fn silent_server_truncation_is_detected_not_learned() {
        let mut budget = TokenBudget::new(32_768, 4096);
        // 40k chars reported as 2048 tokens: about 20 chars per token.
        budget.calibrate(40_000, 2048);
        assert_eq!(budget.truncated_at, Some(2048));
        assert_eq!(budget.chars_per_token, DEFAULT_CHARS_PER_TOKEN);
        // Dense but real text still calibrates.
        let mut budget = TokenBudget::new(32_768, 4096);
        budget.calibrate(40_000, 9_000);
        assert_eq!(budget.truncated_at, None);
        assert!(budget.chars_per_token > DEFAULT_CHARS_PER_TOKEN);
        let shrunk = budget.shrunk_to(4096);
        assert_eq!(shrunk.context_window, 4096);
        assert_eq!(shrunk.max_output_tokens, 1024);
        assert_eq!(shrunk.chars_per_token, budget.chars_per_token);
    }

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

    #[test]
    fn static_prompt_names_its_largest_component() {
        let prompt = StaticPrompt::new(900, 2500, 400);
        assert_eq!(prompt.total, 3800);
        assert_eq!(prompt.largest(), ("tool schemas", 2500));
        assert!(prompt.percent_of(2560) > 100.0);
        let prompt = StaticPrompt::new(2000, 300, 100);
        assert_eq!(prompt.largest(), ("system prompt", 2000));
    }

    #[test]
    fn json_density_is_pinned_and_separate_from_prose() {
        assert!((JSON_CHARS_PER_TOKEN - 2.6).abs() < f64::EPSILON);
        let budget = TokenBudget::new(32768, 4096);
        assert_eq!(budget.estimate_json_char_len(2_600), 1_000);
        // JSON is priced denser than prose at the same character count.
        assert!(
            budget.estimate_json_char_len(3_500) > budget.estimate_char_len(3_500),
            "JSON must not use the prose ratio"
        );
        // A prose calibration does not move the JSON estimate.
        let mut calibrated = budget.clone();
        calibrated.calibrate(20_000, 4_000);
        assert!(calibrated.chars_per_token > DEFAULT_CHARS_PER_TOKEN);
        assert_eq!(
            calibrated.estimate_json_char_len(2_600),
            budget.estimate_json_char_len(2_600)
        );
    }

    #[test]
    fn tool_spec_cost_is_fixed_after_the_first_serialization() {
        let budget = TokenBudget::new(32768, 4096);
        let specs = vec![ToolSpec {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        }];
        let cost = ToolSpecCost::estimate(&specs, &budget);
        assert!(cost.chars > 0);
        assert_eq!(cost.tokens, budget.estimate_json_char_len(cost.chars));
        let measured = cost.with_tokens(42);
        assert_eq!(measured.tokens, 42);
        assert_eq!(measured.chars, cost.chars);
        assert_eq!(
            ToolSpecCost::from_json("", &budget),
            ToolSpecCost::default()
        );
    }
}

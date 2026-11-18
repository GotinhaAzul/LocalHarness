//! Run checkpoints, wall clock, and stall detection.
//!
//! Tool-call, model-call, and wall-clock limits are checkpoints, not hard
//! stops. When one is reached and the run produced new information, a file
//! change, or a passing verification recently, the allowance is renewed and
//! the turn continues. Only a checkpoint with no recent progress — or the
//! exact-repetition guards — ends the turn. This lets a real long task run as
//! long as it keeps making progress without letting a looping model do so.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use serde_json::Value;

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
    /// Consecutive calls of one tool family that may fail without ending the
    /// turn.
    pub max_same_tool_family: u32,
    pub verify_attempts: u32,
}

impl From<&BudgetConfig> for RunLimits {
    fn from(config: &BudgetConfig) -> Self {
        Self {
            max_tool_calls: config.max_tool_calls.max(1),
            max_model_calls: config.max_model_calls.max(1),
            max_wall: Duration::from_secs(u64::from(config.max_wall_minutes.max(1)) * 60),
            stall_calls: config.stall_calls.max(1),
            max_same_tool_family: config.max_same_tool_family.max(1),
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
    /// Whether the advance notice for the todo gate was attached this turn.
    pub todo_notice_sent: bool,
}

impl RunProgress {
    pub fn new(limits: RunLimits) -> Self {
        let stall_calls = limits.stall_calls;
        let family_limit = limits.max_same_tool_family;
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
            stall: StallTracker::new(stall_calls, family_limit),
            settle_continuations: 0,
            verify_attempts: 0,
            review_nudged: false,
            todo_notice_sent: false,
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
    /// information, even when the call arguments differ. A failed call never
    /// counts as new information.
    pub fn record_tool_result(
        &mut self,
        fingerprint: &str,
        family: &str,
        result: &str,
        is_error: bool,
        changed_file: bool,
        verified_ok: bool,
    ) {
        self.tool_calls += 1;
        self.epoch_tool_calls += 1;
        self.stall
            .observe(fingerprint, family, is_error, changed_file, verified_ok);
        let novel = !is_error && self.seen_results.insert(result_digest(result));
        if changed_file || verified_ok || novel {
            self.last_progress_call = Some(self.tool_calls);
            self.last_progress_at = Some(Instant::now());
        }
    }

    pub fn stalled(&self) -> bool {
        self.stall.stalled()
    }

    /// Why the turn is being stopped as `stalled`, naming the repeating family
    /// when that guard is the one that fired.
    pub fn stall_message(&self) -> String {
        self.stall
            .family_reason()
            .unwrap_or_else(|| StopReason::Stalled.message().to_string())
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
    /// Failed calls per canonical tool family this turn. Unlike `strikes`,
    /// these counts do not reset when a different call happens in between.
    family_limit: u32,
    families: HashMap<String, u32>,
    family_trip: Option<(String, u32)>,
}

impl StallTracker {
    pub fn new(needed: u32, family_limit: u32) -> Self {
        Self {
            needed,
            strikes: 0,
            last: None,
            family_limit,
            families: HashMap::new(),
            family_trip: None,
        }
    }

    pub fn observe(
        &mut self,
        fingerprint: &str,
        family: &str,
        is_error: bool,
        changed_file: bool,
        verified_ok: bool,
    ) {
        if changed_file || verified_ok {
            self.strikes = 0;
            self.last = Some(fingerprint.to_string());
            // Real progress clears every family: the run is not looping.
            self.families.clear();
            return;
        }
        if self.last.as_deref() == Some(fingerprint) {
            self.strikes += 1;
        } else {
            self.strikes = 1;
            self.last = Some(fingerprint.to_string());
        }

        if is_error {
            let count = self.families.entry(family.to_string()).or_insert(0);
            *count += 1;
            if *count >= self.family_limit && self.family_trip.is_none() {
                self.family_trip = Some((family.to_string(), *count));
            }
        } else {
            // A successful call of this family is that family making progress,
            // so its earlier failures no longer count against it.
            self.families.remove(family);
        }
    }

    pub fn stalled(&self) -> bool {
        self.strikes >= self.needed || self.family_trip.is_some()
    }

    pub fn strikes(&self) -> u32 {
        self.strikes
    }

    /// Stop message for the family guard, once it has fired.
    pub fn family_reason(&self) -> Option<String> {
        self.family_trip.as_ref().map(|(family, count)| {
            format!(
                "the same tool family ({family}) ran {count} failing calls this turn without a file change or a passing verification; stop repeating it and report the last known state, or change approach in a new turn"
            )
        })
    }
}

/// Canonical family for a tool call, so near-identical retries count as the
/// same operation: `bash` calls by program (and subcommand for tools that take
/// one), other tools by name and normalized key arguments. Cosmetic changes —
/// temporary paths, numbers, quoted code, here-doc bodies — do not create a
/// new family.
pub fn tool_family(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    if name == "bash" {
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        return format!("bash {}", bash_family(command));
    }
    let args = canonical_args(&args);
    if args.is_empty() {
        name.to_string()
    } else {
        format!("{name} {args}")
    }
}

/// Programs whose second bare word is a subcommand worth keeping (`cargo
/// test`, `git status`). For everything else the program alone is the family,
/// so `tesseract a.png`, `tesseract b.png --psm 6` and the like stay together.
const SUBCOMMAND_PROGRAMS: &[&str] = &[
    "cargo",
    "git",
    "go",
    "dotnet",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "uv",
    "pip",
    "pip3",
    "rustup",
    "kubectl",
    "docker",
    "podman",
    "systemctl",
    "mise",
    "nix",
    "gh",
];

const WRAPPERS: &[&str] = &[
    "sudo", "doas", "env", "time", "command", "nohup", "exec", "nice", "stdbuf",
];

/// Argument keys that carry code or file bodies. Their contents change between
/// retries, so they say nothing about the operation.
const BODY_KEYS: &[&str] = &[
    "content",
    "old",
    "new",
    "replacement",
    "replace",
    "patch",
    "diff",
    "code",
    "script",
    "text",
    "body",
    "source",
];

fn bash_family(command: &str) -> String {
    let tokens = shell_words(&strip_heredocs(command));
    let mut index = 0;
    let program = loop {
        let Some(token) = tokens.get(index) else {
            return "shell".into();
        };
        if is_assignment(token) {
            index += 1;
            continue;
        }
        if matches!(token.as_str(), "cd" | "pushd") {
            index += 1;
            while tokens.get(index).is_some_and(|next| next.starts_with('-')) {
                index += 1;
            }
            index += 1;
            continue;
        }
        if WRAPPERS.contains(&token.as_str()) {
            index += 1;
            while tokens.get(index).is_some_and(|next| next.starts_with('-')) {
                index += 1;
            }
            continue;
        }
        break token.as_str();
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    if !SUBCOMMAND_PROGRAMS.contains(&program) {
        return program.to_string();
    }
    let operation = tokens[index + 1..]
        .iter()
        .find(|token| is_operation(token))
        .map(String::as_str);
    match operation {
        Some(operation) => format!("{program} {operation}"),
        None => program.to_string(),
    }
}

fn is_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_operation(token: &str) -> bool {
    !token.is_empty()
        && !token.starts_with('-')
        && !token.contains(['/', '=', '$', '*', '?'])
        && !token.chars().all(|ch| ch.is_ascii_digit())
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':'))
}

/// Remove here-document bodies, which are code and differ between retries.
fn strip_heredocs(command: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut delimiter: Option<String> = None;
    for line in command.lines() {
        if let Some(wanted) = &delimiter {
            if line.trim() == wanted {
                delimiter = None;
            }
            continue;
        }
        kept.push(line);
        delimiter = heredoc_delimiter(line);
    }
    if kept.is_empty() {
        command.to_string()
    } else {
        kept.join("\n")
    }
}

fn heredoc_delimiter(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut quote: Option<u8> = None;
    let mut index = 0;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        match quote {
            Some(open) => {
                if byte == open {
                    quote = None;
                }
                index += 1;
                continue;
            }
            None => match byte {
                b'\'' | b'"' => {
                    quote = Some(byte);
                    index += 1;
                    continue;
                }
                b'<' if bytes[index + 1] == b'<' => {
                    let mut start = index + 2;
                    if bytes.get(start) == Some(&b'-') {
                        start += 1;
                    }
                    let rest = line[start..].trim_start();
                    let delimiter: String = rest
                        .trim_start_matches(['\'', '"'])
                        .chars()
                        .take_while(|ch| {
                            !ch.is_whitespace() && !matches!(ch, '\'' | '"' | ';' | '|')
                        })
                        .collect();
                    return (!delimiter.is_empty()).then_some(delimiter);
                }
                _ => {}
            },
        }
        index += 1;
    }
    None
}

/// Split a shell command into words, dropping quotes and separators. Enough to
/// name the program and subcommand; not a shell parser.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(ch) = chars.next() {
        match quote {
            Some(open) => {
                if ch == open {
                    quote = None;
                } else {
                    current.push(ch);
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                ch if ch.is_whitespace() || matches!(ch, ';' | '|' | '&') => {
                    if !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                    }
                }
                ch => current.push(ch),
            },
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn canonical_args(args: &Value) -> String {
    let Value::Object(map) = args else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in map {
        if value.is_null() {
            continue;
        }
        let value = if BODY_KEYS.contains(&key.as_str()) {
            "<body>".to_string()
        } else {
            canonical_value(value)
        };
        parts.push(format!("{key}={value}"));
    }
    parts.sort();
    parts.join(",")
}

fn canonical_value(value: &Value) -> String {
    match value {
        Value::String(text) => normalize_text(text),
        Value::Array(items) => {
            let mut items: Vec<String> = items.iter().map(canonical_value).collect();
            items.sort();
            format!("[{}]", items.join(","))
        }
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Null => String::new(),
        other => normalize_text(&other.to_string()),
    }
}

/// Collapse whitespace and hide temporary-directory names, which change on
/// every call even when the operation is the same.
fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&normalize_temp_path(word));
    }
    out
}

fn normalize_temp_path(word: &str) -> String {
    for root in ["/tmp/", "/var/tmp/", "/private/tmp/"] {
        if let Some(rest) = word.strip_prefix(root)
            && let Some((_, tail)) = rest.split_once('/')
        {
            return format!("<tmp>/{tail}");
        }
    }
    word.to_string()
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
            max_same_tool_family: 8,
            verify_attempts: 2,
        }
    }

    fn progress_with_one_result(progress: &mut RunProgress, result: &str) {
        progress.record_tool_result(result, result, result, false, false, false);
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
        progress.record_tool_result("a", "same", "same", false, false, false);
        for index in 1..6 {
            progress.record_tool_result(
                &format!("call-{index}"),
                "same",
                "same",
                false,
                false,
                false,
            );
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
        progress.record_tool_result("f", "f", "ok", false, true, false);
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
        progress.record_tool_result("same", "same", "r", false, false, false);
        progress.record_tool_result("same", "same", "r", false, false, false);
        progress.record_tool_result("same", "same", "r", false, false, false);
        assert!(progress.stalled());
        progress.record_tool_result("different", "different", "r", false, false, false);
        assert!(!progress.stalled());
    }

    #[test]
    fn file_change_resets_stall() {
        let mut progress = RunProgress::new(limits());
        for _ in 0..2 {
            progress.record_tool_result("same", "same", "r", false, false, false);
        }
        progress.record_tool_result("same", "same", "r", false, true, false);
        assert!(!progress.stalled());
    }

    #[test]
    fn failed_results_are_never_progress() {
        let mut progress = RunProgress::new(limits());
        for index in 0..6 {
            progress.record_tool_result(
                &format!("call-{index}"),
                &format!("family-{index}"),
                &format!("distinct failure {index}"),
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
    fn failing_calls_of_one_family_trip_the_family_guard() {
        let mut progress = RunProgress::new(limits());
        for index in 0..8 {
            progress.record_tool_result(
                &format!("bash-{index}"),
                "bash tesseract",
                &format!("[exit 1] cannot read receipt-{index}.png"),
                true,
                false,
                false,
            );
            if index < 7 {
                assert!(!progress.stalled(), "call {} is below the limit", index + 1);
            }
        }
        assert!(progress.stalled());
        assert!(progress.stall_message().contains("bash tesseract"));
    }

    #[test]
    fn family_repeats_count_across_interleaved_calls() {
        let mut progress = RunProgress::new(limits());
        for index in 0..7 {
            progress.record_tool_result(
                &format!("a-{index}"),
                "bash tesseract",
                "failed",
                true,
                false,
                false,
            );
            progress.record_tool_result(
                &format!("b-{index}"),
                "bash cat",
                &format!("content {index}"),
                false,
                false,
                false,
            );
        }
        assert!(
            !progress.stalled(),
            "an interleaved call must not clear the family count"
        );
        progress.record_tool_result("a-8", "bash tesseract", "failed", true, false, false);
        assert!(progress.stalled());
    }

    #[test]
    fn successful_calls_of_one_family_do_not_trip_the_family_guard() {
        let mut progress = RunProgress::new(limits());
        for index in 0..20 {
            progress.record_tool_result(
                &format!("cat-{index}"),
                "bash cat",
                &format!("content {index}"),
                false,
                false,
                false,
            );
        }
        assert!(!progress.stalled());
    }

    #[test]
    fn failing_reads_of_different_files_are_different_families() {
        let mut progress = RunProgress::new(limits());
        for index in 0..20 {
            let family = tool_family(
                "read",
                &serde_json::json!({"path": format!("missing-{index}.txt")}).to_string(),
            );
            progress.record_tool_result(
                &format!("read-{index}"),
                &family,
                "missing",
                true,
                false,
                false,
            );
        }
        assert!(!progress.stalled());
    }

    #[test]
    fn bash_families_ignore_paths_numbers_and_code_bodies() {
        let args = |command: &str| serde_json::json!({"command": command}).to_string();
        assert_eq!(
            tool_family(
                "bash",
                &args("tesseract /tmp/a/receipt-1.png out-1.txt --psm 6")
            ),
            "bash tesseract"
        );
        assert_eq!(
            tool_family("bash", &args("tesseract /tmp/b/receipt-2.png out-2.txt")),
            "bash tesseract"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 -c \"from PIL import Image; print(1)\"")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 -c \"from PIL import Image; print(2); print(3)\"")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args(
                    "cd /tmp/scratch-7 && python3 - <<'EOF'\nfrom PIL import Image\nprint(1)\nEOF"
                )
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family(
                "bash",
                &args("python3 << 'EOF'\nimport zlib\nprint(2)\nEOF")
            ),
            "bash python3"
        );
        assert_eq!(
            tool_family("bash", &args("cargo test --locked")),
            "bash cargo test"
        );
        assert_eq!(tool_family("bash", &args("git status")), "bash git status");
        assert_ne!(
            tool_family("bash", &args("git status")),
            tool_family("bash", &args("git diff"))
        );
        assert_eq!(tool_family("bash", &args("")), "bash shell");
    }

    #[test]
    fn other_tool_families_keep_targets_and_drop_bodies() {
        let family = |value: serde_json::Value| tool_family("tests", &value.to_string());
        assert_ne!(
            family(serde_json::json!({"path": "a.rs"})),
            family(serde_json::json!({"path": "b.rs"}))
        );
        assert_eq!(
            family(serde_json::json!({"paths": ["b.rs", "a.rs"]})),
            family(serde_json::json!({"paths": ["a.rs", "b.rs"]}))
        );
        assert_eq!(
            tool_family(
                "edit",
                &serde_json::json!({"path": "a.rs", "old": "one", "new": "two"}).to_string()
            ),
            tool_family(
                "edit",
                &serde_json::json!({"path": "a.rs", "old": "three", "new": "four"}).to_string()
            )
        );
        assert_eq!(
            tool_family(
                "write",
                &serde_json::json!({"path": "/tmp/scratch-1/x.txt", "content": "a"}).to_string()
            ),
            tool_family(
                "write",
                &serde_json::json!({"path": "/tmp/scratch-2/x.txt", "content": "b"}).to_string()
            )
        );
    }
}

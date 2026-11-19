//! Permission policy evaluated before every tool dispatch.
//!
//! A [`Policy`] is an ordered list of allow/ask/deny rules compiled from the
//! global config and, only when trusted, the project config. A
//! [`PermissionMode`] supplies the answer when no rule matches. Skills,
//! workflows, and the model have no path to change either: the policy lives in
//! [`Permissions`], which only the CLI flags and the user's `/plan` commands
//! mutate.
//!
//! Evaluation is pure ([`Policy::decide`]); the agent loop resolves an `Ask`
//! through the frontend (see `agent::authorize`).

pub mod command;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use globset::{GlobBuilder, GlobMatcher};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// How calls that no rule matches are decided.
///
/// The names are the ones users see: `normal`, `accept-edits`, `plan`,
/// `deny-all`, and `yolo`. Older spellings (`ask`, `dont-ask`) still parse so
/// existing config files keep working.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Read-only calls run; edits, commands, and network calls ask.
    #[default]
    Ask,
    /// Like `normal`, but file edits (write, edit, patch, undo) run unasked.
    AcceptEdits,
    /// Investigate and propose only: every mutating call is refused.
    Plan,
    /// Deny anything that would ask; never prompt.
    DenyAll,
    /// Run everything a deny rule does not block. Only `--yolo` selects it.
    Yolo,
}

impl PermissionMode {
    /// The levels `/permissions` offers, in order. Plan keeps its own command.
    pub const LEVELS: [PermissionMode; 4] = [
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::DenyAll,
        PermissionMode::Yolo,
    ];

    /// The user-facing name, also the canonical config and CLI spelling.
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Ask => "normal",
            PermissionMode::AcceptEdits => "accept-edits",
            PermissionMode::Plan => "plan",
            PermissionMode::DenyAll => "deny-all",
            PermissionMode::Yolo => "yolo",
        }
    }

    /// One line for pickers and the footer: what this level actually does.
    pub fn description(self) -> &'static str {
        match self {
            PermissionMode::Ask => "read-only runs; edits and commands ask",
            PermissionMode::AcceptEdits => "edits run, commands ask",
            PermissionMode::Plan => "investigate and propose, change nothing",
            PermissionMode::DenyAll => "anything that would ask is denied",
            PermissionMode::Yolo => "no approvals; deny rules still apply",
        }
    }

    /// Parse a name or an older alias. `dont-ask` and `ask` are deprecated
    /// spellings of `deny-all` and `normal`.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "normal" | "ask" | "default" => Some(PermissionMode::Ask),
            "accept-edits" | "acceptedits" => Some(PermissionMode::AcceptEdits),
            "plan" => Some(PermissionMode::Plan),
            "deny-all" | "denyall" | "dont-ask" | "dontask" => Some(PermissionMode::DenyAll),
            "yolo" => Some(PermissionMode::Yolo),
            _ => None,
        }
    }
}

impl Serialize for PermissionMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.label())
    }
}

impl<'de> Deserialize<'de> for PermissionMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        PermissionMode::parse(&raw).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "unknown permission mode {raw:?}; use normal, accept-edits, plan, or deny-all"
            ))
        })
    }
}

/// What a matching rule does with a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Ask,
    Deny,
}

/// One `[[permissions.rules]]` entry. Every field that is set must match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    /// Name reported in a denial; `rule-<n>` (1-based position) when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Tool name glob (`bash`, `write`, `github__*`, `*`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Path glob, relative to the working directory (`src/**`, `**/.env`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Regex searched in the exact command (`bash`, `bg`; `git <action>` for
    /// the git tool; `gh pr <action>` for the pr tool).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Match calls that do (`true`) or do not (`false`) reach the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<bool>,
    pub action: RuleAction,
}

/// `[permissions]` in the config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionsConfig {
    pub mode: PermissionMode,
    pub rules: Vec<RuleConfig>,
}

/// What a call does, for the mode baseline and plan mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reads the workspace or harness state only.
    ReadOnly,
    /// Changes harness bookkeeping (plan, threads, findings), not the
    /// workspace.
    Session,
    /// Changes workspace files through a file tool.
    Edit,
    /// Runs a command, reaches the network, or is an unclassified tool.
    Exec,
}

/// `path` without a trailing `:start` or `:start-end` line range.
fn strip_line_range(path: &str) -> &str {
    let Some((file, range)) = path.rsplit_once(':') else {
        return path;
    };
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let is_range = match range.split_once('-') {
        Some((start, end)) => digits(start) && digits(end),
        None => digits(range),
    };
    if !file.is_empty() && is_range {
        file
    } else {
        path
    }
}

/// Harness-state tools: they never touch the workspace or run commands.
const SESSION_TOOLS: &[&str] = &["todo", "thread", "finding"];
/// File tools that `accept-edits` runs unasked.
const EDIT_TOOLS: &[&str] = &["write", "edit", "patch", "undo"];
/// `git` tool actions that only read. `branch` and `worktree` read in their
/// `op = "list"` form. `status`, `diff`, `log`, and `blame` go through
/// `bash`, whose command classifier marks them read-only.
const GIT_READ_ACTIONS: &[&str] = &["commit_message"];
/// Mutating tools kept visible in plan mode; each call is checked instead.
const PLAN_CHECKED_TOOLS: &[&str] = &["todo", "bash", "git"];

/// The facts about one call that rules and modes match on.
#[derive(Debug, Clone)]
pub struct CallFacts {
    pub tool: String,
    pub effect: Effect,
    /// The exact command (`bash`/`bg`), or a synthesized one for git/pr.
    pub command: Option<String>,
    /// Paths the call names, relative to the working directory when inside it.
    pub paths: Vec<String>,
    pub network: bool,
}

impl CallFacts {
    /// Classify a repaired call. `read_only` is the tool's own declaration.
    pub fn new(tool: &str, read_only: bool, args: &Value, cwd: &Path) -> Self {
        let str_arg = |key: &str| args.get(key).and_then(Value::as_str).map(str::to_string);
        let mut paths: Vec<String> = Vec::new();
        if let Some(path) = str_arg("path") {
            paths.push(relative(&path, cwd));
        }
        if let Some(list) = args.get("paths").and_then(Value::as_array) {
            // `read` entries may carry a range (`src/a.rs:10-40`); rules match
            // the file, so a deny on `.env` must also catch `.env:1-5`.
            paths.extend(
                list.iter()
                    .filter_map(Value::as_str)
                    .map(|path| relative(strip_line_range(path), cwd)),
            );
        }
        let (command, effect, network) = match tool {
            "bash" | "bg" => {
                let command = str_arg("command").unwrap_or_default();
                let effect = if tool == "bash" && command::is_read_only(&command) {
                    Effect::ReadOnly
                } else {
                    Effect::Exec
                };
                let network = command::touches_network(&command);
                (Some(command), effect, network)
            }
            "git" => {
                let action = str_arg("action").unwrap_or_default();
                let op = str_arg("op").unwrap_or_default();
                let reads = GIT_READ_ACTIONS.contains(&action.as_str())
                    || (matches!(action.as_str(), "branch" | "worktree") && op == "list");
                let command = if op.is_empty() {
                    format!("git {action}")
                } else {
                    format!("git {action} {op}")
                };
                let effect = if reads {
                    Effect::ReadOnly
                } else {
                    Effect::Exec
                };
                (Some(command), effect, false)
            }
            "pr" => {
                let action = str_arg("action").unwrap_or_default();
                (Some(format!("gh pr {action}")), Effect::Exec, true)
            }
            // The full URL is the subject the user approves, and per-host
            // rules match it (`command = "^fetch https://docs\\.rs/"`).
            "fetch" => {
                let url = str_arg("url").unwrap_or_default();
                (Some(format!("fetch {url}")), Effect::Exec, true)
            }
            _ if read_only => (None, Effect::ReadOnly, false),
            _ if SESSION_TOOLS.contains(&tool) => (None, Effect::Session, false),
            _ if EDIT_TOOLS.contains(&tool) => (None, Effect::Edit, false),
            _ => (None, Effect::Exec, false),
        };
        Self {
            tool: tool.to_string(),
            effect,
            command,
            paths,
            network,
        }
    }

    /// The exact thing a user approves: the command, else the paths, else
    /// the tool name alone.
    pub fn subject(&self) -> String {
        if let Some(command) = &self.command {
            return command.clone();
        }
        if !self.paths.is_empty() {
            return self.paths.join(", ");
        }
        self.tool.clone()
    }

    /// Approval memory key: one approval covers exactly this tool and subject.
    pub fn approval_key(&self) -> String {
        format!("{}\u{0}{}", self.tool, self.subject())
    }
}

fn relative(path: &str, cwd: &Path) -> String {
    let joined = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    let normalized = crate::tools::normalize(&joined);
    match normalized.strip_prefix(cwd) {
        Ok(rel) => rel.to_string_lossy().to_string(),
        Err(_) => normalized.to_string_lossy().to_string(),
    }
}

/// The outcome of evaluating one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    /// The user must approve this exact call first.
    Ask(AskRequest),
    /// The call must not run. `code` becomes the tool error code.
    Deny {
        code: String,
        message: String,
    },
}

/// What the frontend shows when it asks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskRequest {
    pub tool: String,
    /// Exact command or path(s) being approved.
    pub subject: String,
    /// Diff or content preview, when one can be computed.
    pub preview: Option<String>,
    /// The rule that asked, or `None` for the mode default.
    pub rule: Option<String>,
    /// Approval memory key ([`CallFacts::approval_key`]).
    pub key: String,
}

/// The user's answer to an [`AskRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskAnswer {
    Allow,
    Deny,
}

/// A frontend that can ask the user about one call (the TUI).
#[async_trait]
pub trait Asker: Send + Sync {
    async fn ask(&self, request: AskRequest) -> AskAnswer;
}

#[derive(Debug, Clone)]
struct Rule {
    id: String,
    tool: Option<GlobMatcher>,
    path: Option<GlobMatcher>,
    command: Option<Regex>,
    network: Option<bool>,
    action: RuleAction,
    specificity: u8,
}

impl Rule {
    fn matches(&self, call: &CallFacts) -> bool {
        if let Some(tool) = &self.tool
            && !tool.is_match(&call.tool)
        {
            return false;
        }
        if let Some(path) = &self.path {
            // Every path the call touches must match. A `git stage` of one
            // allowed and one unrelated file must not sneak the second past a
            // path-scoped allow rule.
            if call.paths.is_empty() || !call.paths.iter().all(|candidate| path.is_match(candidate))
            {
                return false;
            }
        }
        if let Some(command) = &self.command
            && !call
                .command
                .as_deref()
                .is_some_and(|text| command.is_match(text))
        {
            return false;
        }
        if let Some(network) = self.network
            && network != call.network
        {
            return false;
        }
        true
    }
}

/// Compiled, ordered rules plus a stable hash of their source.
#[derive(Debug, Clone)]
pub struct Policy {
    rules: Vec<Rule>,
    hash: String,
}

impl Default for Policy {
    fn default() -> Self {
        Self::compile(&[]).expect("empty policy compiles")
    }
}

impl Policy {
    /// Compile rules in order. Errors name the rule and the bad field.
    pub fn compile(rules: &[RuleConfig]) -> Result<Self, String> {
        let mut compiled = Vec::with_capacity(rules.len());
        for (index, rule) in rules.iter().enumerate() {
            let id = rule
                .id
                .clone()
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| format!("rule-{}", index + 1));
            let glob = |pattern: &str, field: &str| {
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .map(|glob| glob.compile_matcher())
                    .map_err(|error| {
                        format!("permissions rule {id}: invalid {field} glob: {error}")
                    })
            };
            let tool = rule.tool.as_deref().map(|t| glob(t, "tool")).transpose()?;
            let path = rule.path.as_deref().map(|p| glob(p, "path")).transpose()?;
            let command = rule
                .command
                .as_deref()
                .map(|pattern| {
                    Regex::new(pattern).map_err(|error| {
                        format!("permissions rule {id}: invalid command regex: {error}")
                    })
                })
                .transpose()?;
            if tool.is_none() && path.is_none() && command.is_none() && rule.network.is_none() {
                return Err(format!(
                    "permissions rule {id} matches nothing: set tool, path, command, or network"
                ));
            }
            let specificity = u8::from(rule.tool.as_deref().is_some_and(|t| t != "*"))
                + u8::from(path.is_some())
                + u8::from(command.is_some())
                + u8::from(rule.network.is_some());
            compiled.push(Rule {
                id,
                tool,
                path,
                command,
                network: rule.network,
                action: rule.action,
                specificity,
            });
        }
        let canonical = serde_json::to_string(rules).unwrap_or_default();
        let digest = Sha256::digest(canonical.as_bytes());
        let hash = digest
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            rules: compiled,
            hash,
        })
    }

    /// First 16 hex digits of SHA-256 over the rules as JSON.
    pub fn hash(&self) -> &str {
        &self.hash
    }

    pub fn len(&self) -> usize {
        self.rules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// The rule that decides `call`, if any. The last matching rule wins,
    /// except that a `deny` holds against any later `allow`/`ask` that is not
    /// strictly more specific (sets more of tool/path/command/network).
    fn winner(&self, call: &CallFacts) -> Option<&Rule> {
        let mut winner: Option<&Rule> = None;
        let mut strongest_deny: Option<u8> = None;
        for rule in self.rules.iter().filter(|rule| rule.matches(call)) {
            if rule.action == RuleAction::Deny {
                strongest_deny =
                    Some(strongest_deny.map_or(rule.specificity, |s| s.max(rule.specificity)));
                winner = Some(rule);
            } else if strongest_deny.is_none_or(|deny| rule.specificity > deny) {
                winner = Some(rule);
            }
        }
        winner
    }

    /// Decide one call under `mode`. Pure: approval memory and the asking
    /// frontend are applied by [`Permissions`].
    pub fn decide(&self, mode: PermissionMode, call: &CallFacts) -> Decision {
        if mode == PermissionMode::Plan && !plan_allows(call) {
            return Decision::Deny {
                code: "plan_mode".into(),
                message: format!(
                    "plan mode is active: {} would change the workspace or run a command, so it was refused ({}). Investigate with read-only tools and write the plan with todo action \"plan\"; the user leaves plan mode with /plan approve.",
                    call.tool,
                    call.subject()
                ),
            };
        }
        let (action, rule) = match self.winner(call) {
            Some(rule) => (rule.action, Some(rule.id.clone())),
            None => (baseline(mode, call.effect), None),
        };
        match action {
            RuleAction::Allow => Decision::Allow,
            RuleAction::Deny => Decision::Deny {
                code: "DENIED".into(),
                message: format!(
                    "permission rule {} denies {} ({}). The call did not run; try a different approach, or tell the user it needs a change to [permissions].",
                    rule.as_deref().unwrap_or("?"),
                    call.tool,
                    call.subject()
                ),
            },
            RuleAction::Ask => match mode {
                PermissionMode::Yolo => Decision::Allow,
                PermissionMode::DenyAll => Decision::Deny {
                    code: "DENIED".into(),
                    message: format!(
                        "{} ({}) needs approval{} and the deny-all mode never asks. The call did not run; try a read-only approach or tell the user which allow rule it needs.",
                        call.tool,
                        call.subject(),
                        rule.as_deref()
                            .map(|id| format!(" (rule {id})"))
                            .unwrap_or_default()
                    ),
                },
                _ => Decision::Ask(AskRequest {
                    tool: call.tool.clone(),
                    subject: call.subject(),
                    preview: None,
                    rule,
                    key: call.approval_key(),
                }),
            },
        }
    }
}

/// The answer for a call no rule matched.
fn baseline(mode: PermissionMode, effect: Effect) -> RuleAction {
    match (effect, mode) {
        (Effect::ReadOnly | Effect::Session, _) => RuleAction::Allow,
        (_, PermissionMode::Yolo) => RuleAction::Allow,
        (Effect::Edit, PermissionMode::AcceptEdits) => RuleAction::Allow,
        // Plan refused mutating calls before rules ran.
        (_, PermissionMode::Plan) => RuleAction::Deny,
        _ => RuleAction::Ask,
    }
}

/// Whether plan mode lets this call run: read-only calls and the plan itself.
fn plan_allows(call: &CallFacts) -> bool {
    call.effect == Effect::ReadOnly || call.tool == "todo"
}

/// Whether a tool stays in the tool set while plan mode is active. Tools that
/// can only mutate are removed; `bash` and `git` stay and each call is
/// checked, so only their read-only forms run.
pub fn visible_in_plan(name: &str, read_only: bool) -> bool {
    read_only || PLAN_CHECKED_TOOLS.contains(&name)
}

/// Process-wide permission state, shared by every core rebuild. Only the CLI
/// and the user's commands change it.
pub struct Permissions {
    policy: Policy,
    mode: Mutex<PermissionMode>,
    /// The mode to restore when plan mode ends.
    before_plan: Mutex<Option<PermissionMode>>,
    /// Exact calls the user approved this session ([`CallFacts::approval_key`]).
    approved: Mutex<HashSet<String>>,
    asker: Mutex<Option<Arc<dyn Asker>>>,
}

impl std::fmt::Debug for Permissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permissions")
            .field("mode", &self.mode())
            .field("rules", &self.policy.len())
            .field("hash", &self.policy.hash())
            .finish()
    }
}

impl Permissions {
    pub fn new(policy: Policy, mode: PermissionMode) -> Self {
        Self {
            policy,
            mode: Mutex::new(mode),
            before_plan: Mutex::new(None),
            approved: Mutex::new(HashSet::new()),
            asker: Mutex::new(None),
        }
    }

    /// The same mode, plan state, and asker under new rules. Approvals were
    /// given under the old rules, so they are not carried over.
    pub fn with_policy(&self, policy: Policy) -> Self {
        Self {
            policy,
            mode: Mutex::new(self.mode()),
            before_plan: Mutex::new(*self.before_plan.lock().unwrap_or_else(|p| p.into_inner())),
            approved: Mutex::new(HashSet::new()),
            asker: Mutex::new(self.asker()),
        }
    }

    /// No rules, the given mode. For embedders and tests.
    pub fn with_mode(mode: PermissionMode) -> Self {
        Self::new(Policy::default(), mode)
    }

    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    pub fn mode(&self) -> PermissionMode {
        *self.mode.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn set_mode(&self, mode: PermissionMode) {
        *self.mode.lock().unwrap_or_else(|p| p.into_inner()) = mode;
    }

    /// Enter plan mode, remembering the current mode. Returns false when
    /// plan mode was already active.
    pub fn enter_plan(&self) -> bool {
        let current = self.mode();
        if current == PermissionMode::Plan {
            return false;
        }
        *self.before_plan.lock().unwrap_or_else(|p| p.into_inner()) = Some(current);
        self.set_mode(PermissionMode::Plan);
        true
    }

    /// Leave plan mode and restore the mode it replaced. Returns the restored
    /// mode, or `None` when plan mode was not active.
    pub fn leave_plan(&self) -> Option<PermissionMode> {
        if self.mode() != PermissionMode::Plan {
            return None;
        }
        let restored = self
            .before_plan
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
            .unwrap_or_default();
        self.set_mode(restored);
        Some(restored)
    }

    /// Install the frontend that answers `Ask` decisions.
    pub fn set_asker(&self, asker: Arc<dyn Asker>) {
        *self.asker.lock().unwrap_or_else(|p| p.into_inner()) = Some(asker);
    }

    pub fn asker(&self) -> Option<Arc<dyn Asker>> {
        self.asker.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Decide a call under the current mode, honoring exact approvals the
    /// user already gave this session.
    pub fn decide(&self, call: &CallFacts) -> Decision {
        match self.policy.decide(self.mode(), call) {
            Decision::Ask(request) if self.is_approved(&request.key) => Decision::Allow,
            other => other,
        }
    }

    pub fn is_approved(&self, key: &str) -> bool {
        self.approved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(key)
    }

    /// Remember an approval for exactly this call for the rest of the session.
    pub fn approve(&self, key: &str) {
        self.approved
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.to_string());
    }

    /// Filter a selected tool set for the current mode. Plan mode removes
    /// tools that can only mutate and adds `todo` so the plan can be written.
    pub fn filter_tools(
        &self,
        tools: Vec<Arc<dyn crate::tools::Tool>>,
        registry: &crate::tools::ToolRegistry,
    ) -> Vec<Arc<dyn crate::tools::Tool>> {
        if self.mode() != PermissionMode::Plan {
            return tools;
        }
        let mut kept: Vec<_> = tools
            .into_iter()
            .filter(|tool| visible_in_plan(tool.name(), tool.read_only()))
            .collect();
        if !kept.iter().any(|tool| tool.name() == "todo")
            && let Some(todo) = registry.get("todo")
        {
            kept.push(todo);
        }
        kept
    }
}

/// A short preview of what a mutating file call will change, for the ask
/// prompt: a unified diff for edit/patch/write, capped at `max_lines`.
pub fn preview(tool: &str, args: &Value, cwd: &Path, max_lines: usize) -> Option<String> {
    let str_arg = |key: &str| args.get(key).and_then(Value::as_str);
    let (old, new): (String, String) = match tool {
        "edit" => (str_arg("old")?.to_string(), str_arg("new")?.to_string()),
        "patch" => (
            str_arg("anchor")?.to_string(),
            str_arg("replacement")?.to_string(),
        ),
        "write" => {
            let path = str_arg("path")?;
            let full = if Path::new(path).is_absolute() {
                Path::new(path).to_path_buf()
            } else {
                cwd.join(path)
            };
            let current = std::fs::read_to_string(full).unwrap_or_default();
            (current, str_arg("content")?.to_string())
        }
        _ => return None,
    };
    let diff = similar::TextDiff::from_lines(&old, &new);
    let text = diff.unified_diff().context_radius(2).to_string();
    let mut lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let total = lines.len();
    lines.truncate(max_lines);
    let mut out = lines.join("\n");
    if total > max_lines {
        out.push_str(&format!("\n… {} more diff lines", total - max_lines));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(tool: Option<&str>, command: Option<&str>, action: RuleAction) -> RuleConfig {
        RuleConfig {
            id: None,
            tool: tool.map(str::to_string),
            path: None,
            command: command.map(str::to_string),
            network: None,
            action,
        }
    }

    fn bash(command: &str) -> CallFacts {
        CallFacts::new("bash", false, &json!({"command": command}), Path::new("/w"))
    }

    #[test]
    fn later_specific_allow_beats_earlier_broad_ask() {
        let policy = Policy::compile(&[
            rule(Some("bash"), None, RuleAction::Ask),
            rule(Some("bash"), Some("^cargo test"), RuleAction::Allow),
        ])
        .unwrap();
        assert_eq!(
            policy.decide(PermissionMode::Ask, &bash("cargo test --all")),
            Decision::Allow
        );
        assert!(matches!(
            policy.decide(PermissionMode::Ask, &bash("cargo build")),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn a_path_scoped_allow_requires_every_path_to_match() {
        let policy = Policy::compile(&[RuleConfig {
            id: Some("allow-src".into()),
            path: Some("src/**".into()),
            ..rule(Some("git"), None, RuleAction::Allow)
        }])
        .unwrap();
        let call = |paths: Value| {
            CallFacts::new(
                "git",
                false,
                &json!({"action": "stage", "paths": paths}),
                Path::new("/w"),
            )
        };
        assert_eq!(
            policy.decide(PermissionMode::Ask, &call(json!(["src/a.rs"]))),
            Decision::Allow
        );
        assert!(matches!(
            policy.decide(
                PermissionMode::Ask,
                &call(json!(["src/a.rs", "secret.env"]))
            ),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn later_broad_allow_does_not_override_a_specific_deny() {
        let policy = Policy::compile(&[
            RuleConfig {
                id: Some("no-push".into()),
                ..rule(Some("bash"), Some(r"\bgit\s+push\b"), RuleAction::Deny)
            },
            rule(Some("bash"), None, RuleAction::Allow),
        ])
        .unwrap();
        match policy.decide(PermissionMode::Yolo, &bash("git push origin main")) {
            Decision::Deny { code, message } => {
                assert_eq!(code, "DENIED");
                assert!(message.contains("no-push"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            policy.decide(PermissionMode::Ask, &bash("git status --short && make")),
            Decision::Allow
        );
    }

    #[test]
    fn equal_specificity_keeps_the_deny() {
        let policy = Policy::compile(&[
            rule(Some("bash"), None, RuleAction::Deny),
            rule(Some("bash"), None, RuleAction::Allow),
        ])
        .unwrap();
        assert!(matches!(
            policy.decide(PermissionMode::Ask, &bash("make")),
            Decision::Deny { .. }
        ));
        // A strictly more specific later allow does override it.
        let policy = Policy::compile(&[
            rule(Some("bash"), None, RuleAction::Deny),
            rule(Some("bash"), Some("^make$"), RuleAction::Allow),
        ])
        .unwrap();
        assert_eq!(
            policy.decide(PermissionMode::Ask, &bash("make")),
            Decision::Allow
        );
    }

    #[test]
    fn mode_names_parse_with_deprecated_aliases() {
        for (raw, mode) in [
            ("normal", PermissionMode::Ask),
            ("ask", PermissionMode::Ask),
            ("default", PermissionMode::Ask),
            ("accept-edits", PermissionMode::AcceptEdits),
            ("acceptedits", PermissionMode::AcceptEdits),
            ("plan", PermissionMode::Plan),
            ("deny-all", PermissionMode::DenyAll),
            ("denyall", PermissionMode::DenyAll),
            ("dont-ask", PermissionMode::DenyAll),
            ("dontask", PermissionMode::DenyAll),
            ("YOLO", PermissionMode::Yolo),
        ] {
            assert_eq!(PermissionMode::parse(raw), Some(mode), "{raw}");
        }
        assert_eq!(PermissionMode::parse("bypass"), None);
    }

    #[test]
    fn config_names_are_canonical_and_old_names_still_load() {
        for mode in [
            PermissionMode::Ask,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::DenyAll,
            PermissionMode::Yolo,
        ] {
            assert_eq!(PermissionMode::parse(mode.label()), Some(mode));
            let config = PermissionsConfig {
                mode,
                rules: Vec::new(),
            };
            let text = toml::to_string(&config).unwrap();
            let back: PermissionsConfig = toml::from_str(&text).unwrap();
            assert_eq!(back.mode, mode, "{text}");
        }
        let old: PermissionsConfig = toml::from_str("mode = \"dont-ask\"").unwrap();
        assert_eq!(old.mode, PermissionMode::DenyAll);
        let bare: PermissionsConfig = toml::from_str("mode = \"ask\"").unwrap();
        assert_eq!(bare.mode, PermissionMode::Ask);
    }

    #[test]
    fn modes_set_the_default_for_unmatched_calls() {
        let policy = Policy::default();
        let write = CallFacts::new("write", false, &json!({"path": "a.txt"}), Path::new("/w"));
        let read = CallFacts::new("read", true, &json!({"path": "a.txt"}), Path::new("/w"));
        let todo = CallFacts::new("todo", false, &json!({"action": "plan"}), Path::new("/w"));
        assert!(matches!(
            policy.decide(PermissionMode::Ask, &write),
            Decision::Ask(_)
        ));
        assert_eq!(
            policy.decide(PermissionMode::AcceptEdits, &write),
            Decision::Allow
        );
        assert!(matches!(
            policy.decide(PermissionMode::AcceptEdits, &bash("make")),
            Decision::Ask(_)
        ));
        assert!(matches!(
            policy.decide(PermissionMode::DenyAll, &write),
            Decision::Deny { .. }
        ));
        assert_eq!(policy.decide(PermissionMode::Yolo, &write), Decision::Allow);
        for mode in [
            PermissionMode::Ask,
            PermissionMode::DenyAll,
            PermissionMode::Plan,
        ] {
            assert_eq!(policy.decide(mode, &read), Decision::Allow);
            assert_eq!(policy.decide(mode, &todo), Decision::Allow);
        }
        assert_eq!(
            policy.decide(PermissionMode::Ask, &bash("git log -3")),
            Decision::Allow
        );
    }

    #[test]
    fn plan_mode_ignores_allow_rules_for_mutating_calls() {
        let policy = Policy::compile(&[rule(Some("*"), None, RuleAction::Allow)]).unwrap();
        for call in [
            CallFacts::new("write", false, &json!({"path": "a"}), Path::new("/w")),
            bash("git commit -m x"),
            CallFacts::new("git", false, &json!({"action": "commit"}), Path::new("/w")),
        ] {
            match policy.decide(PermissionMode::Plan, &call) {
                Decision::Deny { code, message } => {
                    assert_eq!(code, "plan_mode");
                    assert!(message.contains("/plan approve"), "{message}");
                }
                other => panic!("{call:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn a_read_range_suffix_does_not_slip_past_a_path_rule() {
        assert_eq!(strip_line_range(".env:1-5"), ".env");
        assert_eq!(strip_line_range("src/a.rs:10"), "src/a.rs");
        assert_eq!(strip_line_range("c:dir"), "c:dir");
        assert_eq!(strip_line_range("a.rs:1-x"), "a.rs:1-x");
        let facts = CallFacts::new(
            "read",
            true,
            &serde_json::json!({"paths": ["src/a.rs", ".env:1-5"]}),
            Path::new("/work"),
        );
        assert_eq!(
            facts.paths,
            vec!["src/a.rs".to_string(), ".env".to_string()]
        );
    }

    #[test]
    fn path_and_network_rules_match() {
        let policy = Policy::compile(&[
            RuleConfig {
                id: Some("secrets".into()),
                tool: None,
                path: Some("**/.env".into()),
                command: None,
                network: None,
                action: RuleAction::Deny,
            },
            RuleConfig {
                id: Some("offline".into()),
                tool: None,
                path: None,
                command: None,
                network: Some(true),
                action: RuleAction::Deny,
            },
        ])
        .unwrap();
        let env = CallFacts::new(
            "read",
            true,
            &json!({"path": "/w/app/.env"}),
            Path::new("/w"),
        );
        assert!(matches!(
            policy.decide(PermissionMode::Yolo, &env),
            Decision::Deny { message, .. } if message.contains("secrets")
        ));
        assert!(matches!(
            policy.decide(PermissionMode::Yolo, &bash("curl https://x")),
            Decision::Deny { message, .. } if message.contains("offline")
        ));
        assert_eq!(
            policy.decide(PermissionMode::Yolo, &bash("cargo test")),
            Decision::Allow
        );
    }

    #[test]
    fn approvals_cover_the_exact_command_only() {
        let permissions = Permissions::with_mode(PermissionMode::Ask);
        let Decision::Ask(request) = permissions.decide(&bash("git push origin main")) else {
            panic!("expected ask");
        };
        permissions.approve(&request.key);
        assert_eq!(
            permissions.decide(&bash("git push origin main")),
            Decision::Allow
        );
        assert!(matches!(
            permissions.decide(&bash("git push origin main --force")),
            Decision::Ask(_)
        ));
    }

    #[test]
    fn hash_tracks_rule_content_and_bad_rules_are_named() {
        let a = Policy::compile(&[rule(Some("bash"), None, RuleAction::Ask)]).unwrap();
        let b = Policy::compile(&[rule(Some("bash"), None, RuleAction::Deny)]).unwrap();
        assert_ne!(a.hash(), b.hash());
        assert_eq!(a.hash().len(), 16);
        let error =
            Policy::compile(&[rule(Some("bash"), Some("("), RuleAction::Deny)]).unwrap_err();
        assert!(error.contains("rule-1"), "{error}");
        let error = Policy::compile(&[rule(None, None, RuleAction::Deny)]).unwrap_err();
        assert!(error.contains("matches nothing"), "{error}");
    }

    #[test]
    fn plan_and_restore_round_trip() {
        let permissions = Permissions::with_mode(PermissionMode::AcceptEdits);
        assert!(permissions.enter_plan());
        assert!(!permissions.enter_plan());
        assert_eq!(permissions.mode(), PermissionMode::Plan);
        assert_eq!(permissions.leave_plan(), Some(PermissionMode::AcceptEdits));
        assert_eq!(permissions.leave_plan(), None);
    }
}

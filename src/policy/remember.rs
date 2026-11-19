//! Turning repeated approvals into a rule.
//!
//! An approval covers one exact call for one session, so a project's
//! `cargo test` is approved again every session. The harness counts the
//! user's approvals of each exact command per project, in the data directory.
//! From the third ask on, the prompt offers a ready-made allow rule for that
//! exact command; one key saves it to the global config.
//!
//! Only plain commands get a suggestion. A command with shell control
//! operators, substitution, or redirection could hide a second command behind
//! the one the user approved, so it is always asked.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{RuleAction, RuleConfig};
use crate::paths::Paths;

/// Approvals in earlier asks before a rule is suggested: the third ask offers it.
pub const SUGGEST_AFTER: u32 = 2;
/// File in the data directory with approval counts per project.
pub const STORE_FILE: &str = "approvals.json";

/// Tools whose subject is a command a rule can match exactly.
const COMMAND_TOOLS: &[&str] = &["bash", "bg"];
/// Anything that could chain, substitute, or redirect.
const SHELL_SYNTAX: &[&str] = &[";", "&", "|", "$(", "`", ">", "<", "\n"];

/// A rule the user can save instead of approving the same command again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    /// Times the user approved this exact call before.
    pub times: u32,
    pub rule: RuleConfig,
}

impl Suggestion {
    /// The rule as it would appear in the config file.
    pub fn toml(&self) -> String {
        let mut out = String::from("[[permissions.rules]]\n");
        if let Some(id) = &self.rule.id {
            out.push_str(&format!("id = {}\n", toml_string(id)));
        }
        if let Some(tool) = &self.rule.tool {
            out.push_str(&format!("tool = {}\n", toml_string(tool)));
        }
        if let Some(command) = &self.rule.command {
            out.push_str(&format!("command = {}\n", toml_string(command)));
        }
        out.push_str("action = \"allow\"");
        out
    }
}

fn toml_string(value: &str) -> String {
    toml::Value::String(value.to_string()).to_string()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    /// Approval counts by canonical project path, then approval key.
    #[serde(default)]
    projects: BTreeMap<String, BTreeMap<String, u32>>,
}

fn store_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join(STORE_FILE)
}

fn project_key(cwd: &Path) -> String {
    cwd.canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string()
}

fn load(paths: &Paths) -> Store {
    std::fs::read(store_path(paths))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(paths: &Paths, store: &Store) -> std::io::Result<()> {
    let path = store_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(store).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// How many times the user approved `key` in this project before.
pub fn approvals(paths: &Paths, key: &str) -> u32 {
    load(paths)
        .projects
        .get(&project_key(&paths.cwd))
        .and_then(|keys| keys.get(key))
        .copied()
        .unwrap_or(0)
}

/// Count one more approval of `key` in this project.
pub fn record_approval(paths: &Paths, key: &str) {
    let mut store = load(paths);
    let count = store
        .projects
        .entry(project_key(&paths.cwd))
        .or_default()
        .entry(key.to_string())
        .or_default();
    *count = count.saturating_add(1);
    if let Err(error) = save(paths, &store) {
        tracing::warn!(%error, "cannot record the approval");
    }
}

/// The rule to offer for an ask about `tool` running `subject`, when the
/// user already approved it [`SUGGEST_AFTER`] times.
pub fn suggest(tool: &str, subject: &str, times: u32) -> Option<Suggestion> {
    if times < SUGGEST_AFTER || !COMMAND_TOOLS.contains(&tool) {
        return None;
    }
    let subject = subject.trim();
    if subject.is_empty() || SHELL_SYNTAX.iter().any(|syntax| subject.contains(syntax)) {
        return None;
    }
    let digest = Sha256::digest(format!("{tool}\u{0}{subject}").as_bytes());
    let id = format!("approved-{}", &hex(&digest)[..8]);
    Some(Suggestion {
        times,
        rule: RuleConfig {
            id: Some(id),
            tool: Some(tool.to_string()),
            path: None,
            command: Some(format!("^{}$", regex::escape(subject))),
            network: None,
            action: RuleAction::Allow,
        },
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{CallFacts, PermissionMode, Policy};

    fn paths(dir: &Path) -> Paths {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths
    }

    #[test]
    fn approvals_are_counted_per_project_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let here = paths(dir.path());
        let mut there = paths(other.path());
        there.data_dir = here.data_dir.clone();
        record_approval(&here, "bash\u{0}cargo test");
        record_approval(&here, "bash\u{0}cargo test");
        assert_eq!(approvals(&here, "bash\u{0}cargo test"), 2);
        assert_eq!(approvals(&here, "bash\u{0}make"), 0);
        assert_eq!(approvals(&there, "bash\u{0}cargo test"), 0);
    }

    #[test]
    fn the_third_ask_suggests_an_exact_rule_that_allows_only_that_command() {
        assert!(suggest("bash", "cargo test", 1).is_none());
        let suggestion = suggest("bash", "cargo test --all", 2).expect("suggested");
        assert_eq!(
            suggestion.rule.command.as_deref(),
            Some("^cargo test \\-\\-all$")
        );
        assert!(suggestion.toml().contains("action = \"allow\""));

        let policy = Policy::compile(std::slice::from_ref(&suggestion.rule)).unwrap();
        let decide = |command: &str| {
            policy.decide(
                PermissionMode::Ask,
                &CallFacts::new(
                    "bash",
                    false,
                    &serde_json::json!({"command": command}),
                    Path::new("/w"),
                ),
            )
        };
        assert_eq!(decide("cargo test --all"), crate::policy::Decision::Allow);
        assert!(matches!(
            decide("cargo test --all --release"),
            crate::policy::Decision::Ask(_)
        ));
        assert!(matches!(
            decide("cargo test"),
            crate::policy::Decision::Ask(_)
        ));
    }

    #[test]
    fn commands_that_could_hide_another_command_are_never_suggested() {
        for command in [
            "cargo test && git push",
            "cargo test; rm -rf x",
            "echo $(id)",
            "cat a > b",
            "ls | sh",
        ] {
            assert!(suggest("bash", command, 9).is_none(), "{command}");
        }
        assert!(
            suggest("write", "src/a.rs", 9).is_none(),
            "paths are not suggested"
        );
    }
}

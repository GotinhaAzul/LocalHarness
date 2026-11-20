//! `/init` and `tot init` (#175): a model-drafted, person-approved
//! onboarding for a workspace.
//!
//! The harness, not the model, decides what is read and what may be written:
//!
//! 1. [`gather`] reads a bounded set of files (manifests, task runners, CI
//!    workflows, README, an existing `AGENTS.md`) without following symlinks,
//!    and detects verify steps deterministically: the manifest detection the
//!    verify gate already uses, plus `make`/`just`/`task` targets with the
//!    line they are defined on.
//! 2. One model call with no tools turns that into `AGENTS.md` prose and
//!    extra verify steps. Nothing runs during init: the model cannot call a
//!    tool, and no proposed step is executed.
//! 3. [`build_proposal`] keeps a model step only when its evidence checks
//!    out (a file the model was shown, a line that mentions the command) and
//!    it does not look like a deploy or install. The harness renders
//!    `.harness/verify.toml` itself, so it always parses. An existing
//!    `AGENTS.md` only ever gains an appended section, so a hand-written file
//!    is never replaced.
//! 4. The person reviews the full content (new file) or diff (existing file)
//!    and approves each file; [`apply`] writes only those, and refuses a file
//!    that changed since the proposal was made.
//!
//! Steps written to `.harness/verify.toml` load only for a trusted project
//! and run through the permission policy on every verify, like any command.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::agent::Core;
use crate::config::{VerifyConfig, VerifyStep};
use crate::provider::{ChatMessage, ChatRequest, StreamEvent, collect_stream};
use crate::tools::project::{DetectedVerify, detect_verify_steps};

/// Written at the session root, next to `.harness/`.
pub const AGENTS_FILE: &str = "AGENTS.md";
pub const VERIFY_FILE: &str = ".harness/verify.toml";

/// Bytes of one gathered file shown to the model.
const FILE_BUDGET: usize = 6 * 1024;
/// Bytes of all gathered files together.
const TOTAL_BUDGET: usize = 40 * 1024;
/// CI workflow files read at most.
const MAX_WORKFLOWS: usize = 4;
/// Top-level entries listed at most.
const MAX_LISTING: usize = 60;
/// The `AGENTS.md` loader truncates a file above this size.
const AGENTS_LIMIT: usize = 12 * 1024;
/// A local model can be slow to write a few kilobytes of prose.
const MODEL_TIMEOUT: Duration = Duration::from_secs(600);

/// Task-runner targets that check code, in the order steps should run.
const VERIFY_TARGETS: &[&str] = &[
    "check",
    "typecheck",
    "vet",
    "lint",
    "fmt-check",
    "format-check",
    "test",
    "tests",
];

/// An argument that marks a command as something other than a check.
const NOT_A_CHECK: &[&str] = &[
    "deploy",
    "publish",
    "release",
    "push",
    "upload",
    "install",
    "uninstall",
    "rm",
    "delete",
    "destroy",
    "clean",
];

/// Files read for the model, in this order, when present.
const SOURCES: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.cfg",
    "setup.py",
    "tox.ini",
    "noxfile.py",
    "pytest.ini",
    "go.mod",
    "Makefile",
    "makefile",
    "GNUmakefile",
    "justfile",
    "Justfile",
    ".justfile",
    "Taskfile.yml",
    "Taskfile.yaml",
    "README.md",
    "README",
    "README.rst",
    "CONTRIBUTING.md",
];

/// One file shown to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFile {
    /// Path relative to the workspace, with `/` separators.
    pub path: String,
    pub text: String,
    pub truncated: bool,
}

/// Where a proposed step came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepSource {
    /// Manifest detection, the same the verify gate uses.
    Detected,
    /// A `make`/`just`/`task` target found in the file.
    TaskRunner,
    /// The model, with evidence the harness checked.
    Model,
    /// Already in `.harness/verify.toml`.
    Existing,
}

/// A verify step with the reason it is proposed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedStep {
    pub name: String,
    pub command: Vec<String>,
    /// `detected from Cargo.toml`, `Makefile:12 target \`test\``, …
    pub evidence: String,
    pub source: StepSource,
}

impl ProposedStep {
    fn to_config(&self) -> VerifyStep {
        VerifyStep {
            name: self.name.clone(),
            command: self.command.clone(),
            timeout_secs: crate::tools::project::DETECTED_STEP_TIMEOUT_SECS,
        }
    }
}

/// Everything read from the workspace before the model is asked.
#[derive(Debug, Clone, Default)]
pub struct Gathered {
    pub cwd: PathBuf,
    pub files: Vec<SourceFile>,
    /// Top-level entries, directories with a trailing `/`.
    pub listing: Vec<String>,
    pub detected: DetectedVerify,
    /// `make`/`just`/`task` verify targets.
    pub task_steps: Vec<ProposedStep>,
    /// Current `AGENTS.md`, when there is one.
    pub agents_md: Option<String>,
    /// Current `.harness/verify.toml` text and its parsed steps.
    pub verify_toml: Option<String>,
    pub verify_steps: Vec<VerifyStep>,
    /// Problems worth showing, such as an unreadable `verify.toml`.
    pub notes: Vec<String>,
}

impl Gathered {
    /// Steps included without asking the model: manifest detection, or the
    /// task runner's targets when no manifest was detected.
    pub fn seeds(&self) -> Vec<ProposedStep> {
        if self.detected.steps.is_empty() {
            return self.task_steps.clone();
        }
        let from = self.detected.markers.join(", ");
        self.detected
            .steps
            .iter()
            .map(|step| ProposedStep {
                name: step.name.clone(),
                command: step.to_config().command,
                evidence: format!("detected from {from}"),
                source: StepSource::Detected,
            })
            .collect()
    }

    fn file(&self, path: &str) -> Option<&SourceFile> {
        self.files.iter().find(|file| file.path == path)
    }
}

/// Read the workspace. Pure file reads: nothing is executed.
pub fn gather(cwd: &Path) -> Gathered {
    let mut gathered = Gathered {
        cwd: cwd.to_path_buf(),
        detected: detect_verify_steps(cwd),
        ..Gathered::default()
    };
    let mut budget = TOTAL_BUDGET;
    let mut candidates: Vec<String> = SOURCES.iter().map(|name| name.to_string()).collect();
    candidates.extend(workflow_files(cwd));
    for path in candidates {
        if budget == 0 {
            gathered.notes.push(format!(
                "stopped reading files at {} KiB; {path} and later files were not shown to the model",
                TOTAL_BUDGET / 1024
            ));
            break;
        }
        let Some(text) = read_plain(cwd, &path) else {
            continue;
        };
        let limit = FILE_BUDGET.min(budget);
        let (text, truncated) = clip_bytes(&text, limit);
        budget = budget.saturating_sub(text.len());
        gathered.files.push(SourceFile {
            path,
            text,
            truncated,
        });
    }
    gathered.task_steps = task_runner_steps(&gathered.files);
    gathered.agents_md = read_plain(cwd, AGENTS_FILE);
    if let Some(text) = read_plain(cwd, VERIFY_FILE) {
        match toml::from_str::<VerifyConfig>(&text) {
            Ok(config) => gathered.verify_steps = config.steps,
            Err(error) => gathered.notes.push(format!(
                "{VERIFY_FILE} does not parse ({}); it is left as it is",
                error.message()
            )),
        }
        gathered.verify_toml = Some(text);
    }
    gathered.listing = listing(cwd);
    gathered
}

/// A regular file under `cwd`, as UTF-8. Symlinks are not followed, so a
/// link cannot pull in a file from outside the workspace.
fn read_plain(cwd: &Path, relative: &str) -> Option<String> {
    let path = cwd.join(relative);
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    // Every parent below `cwd` must be a real directory too.
    let mut parent = path.parent();
    while let Some(dir) = parent {
        if dir == cwd {
            break;
        }
        if std::fs::symlink_metadata(dir)
            .map(|meta| meta.file_type().is_symlink())
            .unwrap_or(true)
        {
            return None;
        }
        parent = dir.parent();
    }
    String::from_utf8(std::fs::read(&path).ok()?).ok()
}

fn workflow_files(cwd: &Path) -> Vec<String> {
    let dir = cwd.join(".github/workflows");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.ends_with(".yml") || name.ends_with(".yaml"))
        .collect();
    names.sort();
    names
        .into_iter()
        .take(MAX_WORKFLOWS)
        .map(|name| format!(".github/workflows/{name}"))
        .collect()
}

fn listing(cwd: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(cwd) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if name == ".git"
                || name == "node_modules"
                || name == "target"
                || (name.starts_with('.') && name != ".github" && name != ".harness")
            {
                return None;
            }
            let dir = entry.file_type().ok()?.is_dir();
            Some(if dir { format!("{name}/") } else { name })
        })
        .collect();
    names.sort();
    names.truncate(MAX_LISTING);
    names
}

fn clip_bytes(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// Verify targets defined in a Makefile, justfile, or Taskfile, with the
/// line each is defined on.
pub fn task_runner_steps(files: &[SourceFile]) -> Vec<ProposedStep> {
    let mut found: Vec<(usize, ProposedStep)> = Vec::new();
    let mut seen = BTreeSet::new();
    for file in files {
        let program = match file.path.as_str() {
            "Makefile" | "makefile" | "GNUmakefile" => "make",
            "justfile" | "Justfile" | ".justfile" => "just",
            "Taskfile.yml" | "Taskfile.yaml" => "task",
            _ => continue,
        };
        for (target, line) in targets(program, &file.text) {
            let Some(rank) = VERIFY_TARGETS.iter().position(|name| *name == target) else {
                continue;
            };
            if !seen.insert((program, target.clone())) {
                continue;
            }
            found.push((
                rank,
                ProposedStep {
                    name: target.clone(),
                    command: vec![program.to_string(), target.clone()],
                    evidence: format!("{}:{line} target `{target}`", file.path),
                    source: StepSource::TaskRunner,
                },
            ));
        }
    }
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, step)| step).collect()
}

/// `(target, 1-based line)` for each target or recipe a task runner defines.
fn targets(program: &str, text: &str) -> Vec<(String, usize)> {
    let mut out = Vec::new();
    match program {
        "make" | "just" => {
            for (index, line) in text.lines().enumerate() {
                if line.starts_with(|c: char| c.is_whitespace() || c == '#' || c == '.') {
                    continue;
                }
                let Some((head, rest)) = line.split_once(':') else {
                    continue;
                };
                // `x := 1` and `x ::= 1` are assignments, not targets.
                if rest.starts_with('=') || rest.starts_with(":=") {
                    continue;
                }
                // `just` recipes take parameters after the name.
                let names: Vec<&str> = if program == "just" {
                    head.split_whitespace().take(1).collect()
                } else {
                    head.split_whitespace().collect()
                };
                for name in names {
                    if !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                    {
                        out.push((name.to_string(), index + 1));
                    }
                }
            }
        }
        "task" => {
            let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(text) else {
                return out;
            };
            let Some(tasks) = value.get("tasks").and_then(|tasks| tasks.as_mapping()) else {
                return out;
            };
            for key in tasks.keys().filter_map(|key| key.as_str()) {
                let line = text
                    .lines()
                    .position(|line| {
                        let trimmed = line.trim_start();
                        line.len() > trimmed.len()
                            && (trimmed.starts_with(&format!("{key}:"))
                                || trimmed.starts_with(&format!("\"{key}\":")))
                    })
                    .map(|index| index + 1)
                    .unwrap_or(1);
                out.push((key.to_string(), line));
            }
        }
        _ => {}
    }
    out
}

/// What the model is asked to return.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelDraft {
    #[serde(default)]
    pub agents_md: String,
    #[serde(default)]
    pub verify_steps: Vec<ModelStep>,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModelStep {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub evidence: String,
}

const SYSTEM_PROMPT: &str = "You draft onboarding files for a coding-agent harness from a repository's files. \
Reply with ONE JSON object and nothing else, shaped exactly like:\n\
{\"agents_md\": \"<markdown>\", \"verify_steps\": [{\"name\": \"test\", \"command\": [\"make\", \"test\"], \"evidence\": \"Makefile:12\"}], \"notes\": [\"<short note>\"]}\n\n\
agents_md: concise Markdown for AGENTS.md, under 4000 characters: what the project is, its layout, how to build, test, and lint it, conventions, and gotchas. \
State only what the files show; never invent commands, paths, or tools. Write for an engineer, not a reader of marketing.\n\
verify_steps: commands that check the code without changing files or using the network: build or typecheck, lint, test. \
Never propose deploy, publish, release, install, or clean commands. Command is a program and its arguments as an array, with no shell syntax. \
Evidence is the path of a file shown below, optionally with :line, where the command or target appears. \
Do not repeat the established steps; return an empty list when there is nothing to add.\n\
notes: anything the person should check, or an empty list.";

/// The prompt for one draft.
pub fn messages(gathered: &Gathered) -> Vec<ChatMessage> {
    let mut user = String::new();
    let seeds = gathered.seeds();
    user.push_str("Established verify steps (already included; do not repeat):\n");
    if seeds.is_empty() {
        user.push_str("- none detected\n");
    }
    for step in &seeds {
        user.push_str(&format!(
            "- {}: `{}` ({})\n",
            step.name,
            step.command.join(" "),
            step.evidence
        ));
    }
    let extra: Vec<&ProposedStep> = gathered
        .task_steps
        .iter()
        .filter(|step| !seeds.iter().any(|seed| seed.command == step.command))
        .collect();
    if !extra.is_empty() {
        user.push_str("\nTask-runner targets you may add as steps if they check code:\n");
        for step in extra {
            user.push_str(&format!(
                "- `{}` ({})\n",
                step.command.join(" "),
                step.evidence
            ));
        }
    }
    if !gathered.verify_steps.is_empty() {
        user.push_str("\nSteps already in .harness/verify.toml (kept as they are):\n");
        for step in &gathered.verify_steps {
            user.push_str(&format!("- {}: `{}`\n", step.name, step.command.join(" ")));
        }
    }
    user.push_str("\nTop-level entries: ");
    user.push_str(&gathered.listing.join(", "));
    user.push('\n');
    match &gathered.agents_md {
        Some(existing) => {
            let (text, truncated) = clip_bytes(existing, FILE_BUDGET);
            user.push_str(
                "\nAGENTS.md already exists. Set agents_md to ONLY new sections or bullets it lacks \
                 (they are appended under a heading); repeat nothing it already says; use an empty \
                 string when it is complete.\n\n### AGENTS.md (current)\n```\n",
            );
            user.push_str(&text);
            user.push_str(if truncated {
                "\n[truncated]\n```\n"
            } else {
                "\n```\n"
            });
        }
        None => user.push_str("\nThere is no AGENTS.md yet; write the whole file.\n"),
    }
    user.push_str("\nRepository files:\n");
    for file in &gathered.files {
        user.push_str(&format!("\n### {}\n```\n{}", file.path, file.text));
        user.push_str(if file.truncated {
            "\n[truncated]\n```\n"
        } else {
            "\n```\n"
        });
    }
    vec![
        ChatMessage::System {
            content: SYSTEM_PROMPT.into(),
        },
        ChatMessage::user(user),
    ]
}

/// The JSON schema of the reply, so an engine with structured output (a
/// `response_format` or a grammar) cannot return malformed JSON. Engines
/// without it, and KoboldCpp (whose grammar needs a separate fetch), fall
/// back to [`parse_reply`]'s lenient reading.
pub fn reply_schema() -> serde_json::Value {
    let string = serde_json::json!({"type": "string"});
    serde_json::json!({
        "type": "object",
        "properties": {
            "agents_md": string,
            "verify_steps": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": string,
                        "command": {"type": "array", "items": string},
                        "evidence": string
                    },
                    "required": ["name", "command", "evidence"]
                }
            },
            "notes": {"type": "array", "items": string}
        },
        "required": ["agents_md", "verify_steps", "notes"]
    })
}

fn reply_format(
    engine: crate::provider::Engine,
) -> Option<crate::provider::structured::StructuredRequest> {
    use crate::provider::structured::{StructuredMode, plan};
    match plan(engine, "init_proposal", &reply_schema()) {
        Ok(request) if request.mode != StructuredMode::KoboldGrammar => Some(request),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(%error, "init reply schema");
            None
        }
    }
}

/// Read the model's JSON, tolerating a code fence or prose around it.
pub fn parse_reply(text: &str) -> Result<ModelDraft, String> {
    let start = text.find('{');
    let end = text.rfind('}');
    let (Some(start), Some(end)) = (start, end) else {
        return Err("the reply held no JSON object".into());
    };
    if end < start {
        return Err("the reply held no JSON object".into());
    }
    serde_json::from_str(&text[start..=end])
        .map_err(|error| format!("the reply is not the expected JSON ({error})"))
}

/// One file the proposal would write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposedFile {
    /// Relative to the workspace.
    pub path: String,
    /// Current content; `None` for a new file.
    pub before: Option<String>,
    pub after: String,
    /// One line per verify step: what it runs and why.
    pub evidence: Vec<String>,
    /// Shown before approval, such as instruction-shaped text.
    pub warnings: Vec<String>,
}

impl ProposedFile {
    pub fn is_new(&self) -> bool {
        self.before.is_none()
    }

    /// The full content of a new file, or a unified diff of an existing one.
    pub fn preview(&self) -> String {
        match &self.before {
            None => self.after.clone(),
            Some(before) => similar::TextDiff::from_lines(before, &self.after)
                .unified_diff()
                .context_radius(2)
                .header(&format!("a/{}", self.path), &format!("b/{}", self.path))
                .to_string(),
        }
    }

    /// `new · 38 lines` or `update · +6 -0`.
    pub fn summary(&self) -> String {
        match &self.before {
            None => format!("new · {} lines", self.after.lines().count()),
            Some(before) => {
                let diff = similar::TextDiff::from_lines(before, &self.after);
                let (mut added, mut removed) = (0, 0);
                for change in diff.iter_all_changes() {
                    match change.tag() {
                        similar::ChangeTag::Insert => added += 1,
                        similar::ChangeTag::Delete => removed += 1,
                        similar::ChangeTag::Equal => {}
                    }
                }
                format!("update · +{added} -{removed}")
            }
        }
    }
}

/// Everything `/init` proposes, before anything is written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Proposal {
    pub files: Vec<ProposedFile>,
    /// Dropped steps, model failures, and other things to read.
    pub notes: Vec<String>,
    /// Whether the session trusts project `.harness/` config.
    pub trusted: bool,
}

/// Turn the gathered facts and the model's draft (or its failure) into a
/// proposal. Deterministic: the same inputs give the same proposal.
pub fn build_proposal(
    gathered: &Gathered,
    draft: Result<ModelDraft, String>,
    trusted: bool,
) -> Proposal {
    let mut notes = gathered.notes.clone();
    let draft = match draft {
        Ok(draft) => draft,
        Err(error) => {
            notes.push(format!(
                "the model draft failed ({error}); proposing only what the harness detected"
            ));
            ModelDraft::default()
        }
    };
    notes.extend(
        draft
            .notes
            .iter()
            .map(|note| note.trim())
            .filter(|note| !note.is_empty())
            .map(|note| format!("model: {}", crate::text::clip(note, 300))),
    );

    let mut steps = gathered.seeds();
    for step in &draft.verify_steps {
        match check_model_step(gathered, step) {
            Ok(step) => {
                if !steps.iter().any(|known| known.command == step.command)
                    && !gathered
                        .verify_steps
                        .iter()
                        .any(|known| known.command == step.command)
                {
                    steps.push(step);
                }
            }
            Err(reason) => notes.push(format!(
                "dropped the model's step `{}`: {reason}",
                step.command.join(" ")
            )),
        }
    }

    let mut files = Vec::new();
    if let Some(file) = agents_file(gathered, &draft.agents_md, &steps) {
        files.push(file);
    }
    if let Some(file) = verify_file(gathered, &steps) {
        files.push(file);
    }
    Proposal {
        files,
        notes,
        trusted,
    }
}

/// Accept a model step only with evidence the harness can confirm.
fn check_model_step(gathered: &Gathered, step: &ModelStep) -> Result<ProposedStep, String> {
    let command: Vec<String> = step
        .command
        .iter()
        .map(|part| part.trim().to_string())
        .filter(|part| !part.is_empty())
        .collect();
    let Some(program) = command.first() else {
        return Err("it has no command".into());
    };
    if command.len() > 12 || command.iter().any(|part| part.len() > 200) {
        return Err("the command is too long to review".into());
    }
    if let Some(word) = command
        .iter()
        .find(|part| NOT_A_CHECK.contains(&part.to_ascii_lowercase().as_str()))
    {
        return Err(format!("`{word}` is not a check"));
    }
    if command.len() == 1
        && program
            .chars()
            .any(|c| c.is_whitespace() || "|&;<>$`".contains(c))
    {
        return Err("it is shell syntax, not a program and arguments".into());
    }
    let evidence = step.evidence.trim();
    let (path, line) = match evidence.rsplit_once(':') {
        Some((path, line)) if line.trim().parse::<usize>().is_ok() => {
            (path.trim(), line.trim().parse::<usize>().ok())
        }
        _ => (evidence.split_whitespace().next().unwrap_or(""), None),
    };
    let Some(file) = gathered.file(path) else {
        return Err(format!(
            "its evidence {evidence:?} names no file the model was shown"
        ));
    };
    let haystack = match line {
        Some(line) => file
            .text
            .lines()
            .nth(line.saturating_sub(1))
            .ok_or_else(|| format!("{path} has no line {line}"))?
            .to_string(),
        None => file.text.clone(),
    };
    // The cited place must mention what runs: a target or script name, or
    // the program itself for a single-word command.
    let tokens: Vec<&String> = if command.len() > 1 {
        command[1..]
            .iter()
            .filter(|part| !part.starts_with('-') && part.len() >= 2)
            .collect()
    } else {
        command.iter().collect()
    };
    let mentioned = tokens.iter().any(|token| haystack.contains(token.as_str()))
        || (tokens.is_empty() && haystack.contains(program.as_str()));
    if !mentioned {
        return Err(format!(
            "{evidence} does not mention `{}`",
            command.join(" ")
        ));
    }
    let name = step
        .name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect::<String>();
    Ok(ProposedStep {
        name: if name.is_empty() {
            command.last().cloned().unwrap_or_default()
        } else {
            name
        },
        command,
        evidence: evidence.to_string(),
        source: StepSource::Model,
    })
}

fn agents_file(gathered: &Gathered, drafted: &str, steps: &[ProposedStep]) -> Option<ProposedFile> {
    let drafted = drafted.trim();
    let (before, after) = match &gathered.agents_md {
        Some(existing) => {
            // Only lines the file does not already have are appended, so a
            // model that rewrites the whole file cannot duplicate it.
            let known: BTreeSet<&str> = existing
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            let fresh: Vec<&str> = drafted
                .lines()
                .filter(|line| !known.contains(line.trim()))
                .collect();
            let has_content = fresh
                .iter()
                .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'));
            if !has_content {
                return None;
            }
            let mut after = existing.trim_end().to_string();
            after.push_str("\n\n## Added by /init\n\n");
            after.push_str(fresh.join("\n").trim());
            after.push('\n');
            (Some(existing.clone()), after)
        }
        None if drafted.is_empty() => {
            if steps.is_empty() {
                return None;
            }
            (None, fallback_agents_md(steps))
        }
        None => (None, format!("{drafted}\n")),
    };
    let mut warnings = Vec::new();
    if let Some(detection) = crate::provenance::detect(&after) {
        warnings.push(format!(
            "contains instruction-shaped text ({}): \"{}\". AGENTS.md is read by every future session; check it before applying",
            detection.patterns.join(", "),
            crate::text::clip(&detection.excerpt, 120)
        ));
    }
    if after.len() > AGENTS_LIMIT {
        warnings.push(format!(
            "{} KiB is over the {} KiB the loader reads; the end will be cut off",
            after.len() / 1024,
            AGENTS_LIMIT / 1024
        ));
    }
    Some(ProposedFile {
        path: AGENTS_FILE.into(),
        before,
        after,
        evidence: Vec::new(),
        warnings,
    })
}

/// A minimal `AGENTS.md` when the model wrote none.
fn fallback_agents_md(steps: &[ProposedStep]) -> String {
    let mut out = String::from("# AGENTS.md\n\n## Checks\n\n");
    for step in steps {
        out.push_str(&format!("- {}: `{}`\n", step.name, step.command.join(" ")));
    }
    out
}

fn verify_file(gathered: &Gathered, steps: &[ProposedStep]) -> Option<ProposedFile> {
    // An unreadable file is left alone; the note says so.
    if gathered.verify_toml.is_some() && gathered.verify_steps.is_empty() {
        return None;
    }
    let mut all: Vec<ProposedStep> = gathered
        .verify_steps
        .iter()
        .map(|step| ProposedStep {
            name: step.name.clone(),
            command: step.command.clone(),
            evidence: String::new(),
            source: StepSource::Existing,
        })
        .collect();
    let new: Vec<&ProposedStep> = steps
        .iter()
        .filter(|step| !all.iter().any(|known| known.command == step.command))
        .collect();
    if new.is_empty() {
        return None;
    }
    let evidence = new
        .iter()
        .map(|step| {
            format!(
                "{}: `{}` · {}",
                step.name,
                step.command.join(" "),
                step.evidence
            )
        })
        .collect();
    all.extend(new.into_iter().cloned());
    let after = match &gathered.verify_toml {
        // Keep the file as written and append the new steps.
        Some(existing) => {
            let mut text = existing.trim_end().to_string();
            text.push('\n');
            for step in all
                .iter()
                .filter(|step| step.source != StepSource::Existing)
            {
                text.push('\n');
                text.push_str(&render_step(step));
            }
            text
        }
        None => render_verify_toml(&all),
    };
    Some(ProposedFile {
        path: VERIFY_FILE.into(),
        before: gathered.verify_toml.clone(),
        after,
        evidence,
        warnings: Vec::new(),
    })
}

fn render_verify_toml(steps: &[ProposedStep]) -> String {
    let mut out = String::from(
        "# Verify steps for this project, drafted by /init and reviewed by a person.\n\
         # Each run still goes through the permission policy; nothing here grants a permission.\n\
         # .harness/ loads only for a trusted project (--trust or /trust confirm).\n",
    );
    for step in steps {
        out.push('\n');
        out.push_str(&render_step(step));
    }
    out
}

fn render_step(step: &ProposedStep) -> String {
    let config = step.to_config();
    let quote = |text: &str| toml::Value::String(text.to_string()).to_string();
    let mut out = String::new();
    if !step.evidence.is_empty() {
        out.push_str(&format!(
            "# evidence: {}\n",
            step.evidence.replace(['\n', '\r'], " ")
        ));
    }
    out.push_str("[[steps]]\n");
    out.push_str(&format!("name = {}\n", quote(&config.name)));
    out.push_str(&format!(
        "command = [{}]\n",
        config
            .command
            .iter()
            .map(|part| quote(part))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out
}

/// What applying `.harness/` files means for this session.
pub fn trust_note(trusted: bool) -> String {
    if trusted {
        "This project is trusted, so .harness/verify.toml loads in the next session. Commit AGENTS.md and .harness/ to share them.".into()
    } else {
        "This project is untrusted: .harness/verify.toml is inert until you run with --trust (or /trust confirm). AGENTS.md is read either way, as advice. Commit both to share them.".into()
    }
}

/// A plain-text report of the proposal, for the CLI and the transcript.
pub fn report(proposal: &Proposal) -> String {
    let mut out = String::new();
    if proposal.files.is_empty() {
        out.push_str("init: nothing to change; AGENTS.md and the verify steps already cover what was found.\n");
    }
    for file in &proposal.files {
        out.push_str(&format!("── {} ({})\n", file.path, file.summary()));
        for line in &file.evidence {
            out.push_str(&format!("  + step {line}\n"));
        }
        for warning in &file.warnings {
            out.push_str(&format!("  ! {warning}\n"));
        }
        out.push('\n');
        out.push_str(file.preview().trim_end());
        out.push_str("\n\n");
    }
    for note in &proposal.notes {
        out.push_str(&format!("note: {note}\n"));
    }
    if proposal
        .files
        .iter()
        .any(|file| file.path.starts_with(".harness/"))
    {
        out.push_str(&trust_note(proposal.trusted));
        out.push('\n');
    }
    out
}

/// Write the approved files. A file whose content changed since the
/// proposal was built is refused, so a concurrent edit is never lost.
pub fn apply(cwd: &Path, files: &[&ProposedFile]) -> Result<Vec<String>, String> {
    for file in files {
        let current = read_plain(cwd, &file.path);
        if current != file.before {
            return Err(format!(
                "{} changed since the proposal was made; nothing was written. Run init again.",
                file.path
            ));
        }
    }
    let mut written = Vec::new();
    for file in files {
        let path = cwd.join(&file.path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
        }
        let temporary = path.with_extension("init.tmp");
        std::fs::write(&temporary, &file.after)
            .and_then(|_| std::fs::rename(&temporary, &path))
            .map_err(|error| format!("cannot write {}: {error}", file.path))?;
        written.push(file.path.clone());
    }
    Ok(written)
}

/// Gather, ask the model once (no tools), and build the proposal. A model
/// that is down or answers badly still yields the deterministic proposal.
pub async fn draft(core: &Core, cancel: CancellationToken) -> Proposal {
    let cwd = core.paths.cwd.clone();
    let gathered = tokio::task::spawn_blocking(move || gather(&cwd))
        .await
        .unwrap_or_default();
    let reply = ask_model(core, &gathered, &cancel).await;
    build_proposal(
        &gathered,
        reply.and_then(|text| parse_reply(&text)),
        core.config.trust,
    )
}

async fn ask_model(
    core: &Core,
    gathered: &Gathered,
    cancel: &CancellationToken,
) -> Result<String, String> {
    if let Some(gate) = &core.startup
        && gate.ready(cancel).await.is_none()
    {
        return Err("cancelled".into());
    }
    let thinking_off = core.model_profile.request_extra_body_thinking_off();
    let max_tokens = if thinking_off.is_some() { 4096 } else { 8192 };
    let extra_body = thinking_off
        .or_else(|| core.model_profile.request_extra_body(false).ok().flatten())
        .or_else(|| {
            core.model_profile
                .extra_body
                .as_ref()
                .and_then(|value| serde_json::to_value(value).ok())
        });
    let request = ChatRequest {
        model: core.provider.model().to_string(),
        messages: messages(gathered),
        tools: Vec::new(),
        tool_choice: None,
        parallel_tool_calls: None,
        samplers: crate::provider::SamplerSettings::resolve(
            &core.config.model,
            &core.model_profile,
        ),
        engine: core.engine,
        response_schema: reply_format(core.engine),
        max_tokens,
        stop: core.model_profile.stop.clone(),
        extra_body,
        include_usage: false,
        reasoning_field: core.model_profile.reasoning_field.clone(),
    };
    ask(core.provider.as_ref(), request, cancel).await
}

/// Verify steps that name a task-runner target or package script the
/// workspace no longer defines, for `tot doctor`. File reads only.
pub fn stale_steps(cwd: &Path, steps: &[VerifyStep]) -> Vec<String> {
    let mut out = Vec::new();
    for step in steps {
        let Some(program) = step.command.first().map(String::as_str) else {
            continue;
        };
        let target = step
            .command
            .iter()
            .skip(1)
            .find(|arg| !arg.starts_with('-'))
            .map(String::as_str);
        let problem = match (program, target) {
            ("make" | "just" | "task", Some(target)) => {
                let names: &[&str] = match program {
                    "make" => &["Makefile", "makefile", "GNUmakefile"],
                    "just" => &["justfile", "Justfile", ".justfile"],
                    _ => &["Taskfile.yml", "Taskfile.yaml"],
                };
                match names
                    .iter()
                    .find_map(|name| read_plain(cwd, name).map(|text| (*name, text)))
                {
                    None => Some(format!("there is no {}", names[0])),
                    Some((name, text)) => (!targets(program, &text)
                        .iter()
                        .any(|(defined, _)| defined == target))
                    .then(|| format!("{name} has no `{target}` target")),
                }
            }
            ("npm" | "pnpm" | "yarn" | "bun", Some(first)) => {
                let script = if first == "run" {
                    step.command
                        .iter()
                        .skip(2)
                        .find(|arg| !arg.starts_with('-'))
                } else if first == "test" && program != "bun" {
                    step.command.get(1)
                } else {
                    None
                };
                script.and_then(|script| {
                    let scripts = read_plain(cwd, "package.json")
                        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                        .and_then(|value| value.get("scripts").cloned());
                    match scripts {
                        None => Some("package.json has no scripts".to_string()),
                        Some(scripts) => scripts
                            .get(script.as_str())
                            .is_none()
                            .then(|| format!("package.json has no `{script}` script")),
                    }
                })
            }
            _ => None,
        };
        if let Some(problem) = problem {
            out.push(format!(
                "step `{}` runs `{}`, but {problem}",
                step.name,
                step.command.join(" ")
            ));
        }
    }
    out
}

/// One request, its text joined. Split out so tests can use a fake provider.
pub async fn ask(
    provider: &dyn crate::provider::Provider,
    request: ChatRequest,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let events = tokio::time::timeout(
        MODEL_TIMEOUT,
        collect_stream(provider, request, cancel.clone()),
    )
    .await
    .map_err(|_| format!("no answer within {}s", MODEL_TIMEOUT.as_secs()))?;
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let mut text = String::new();
    for event in events {
        match event {
            Ok(StreamEvent::Delta(delta)) => text.push_str(&delta),
            Ok(_) => {}
            Err(error) => return Err(crate::text::clip(&error.to_string(), 200)),
        }
    }
    if text.trim().is_empty() {
        return Err("the reply was empty".into());
    }
    Ok(text)
}

#[cfg(test)]
mod tests;

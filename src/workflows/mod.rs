//! Workflows: bundled playbooks, installable files, and deterministic steps.
//!
//! A workflow is a playbook plus a profile, skill list, and tool allowlist.
//! Optional `[[step]]` tables add a small state machine: conditions over fixed
//! facts, bounded retries, approval gates, and declared artifacts. Steps cannot
//! select tools, change the profile, or evaluate code.
//!
//! Bundled specifications are installed by default and can be overridden by
//! global or trusted project workflow files of the same name.

pub mod engine;
pub mod runner;

use std::path::Path;
use std::sync::LazyLock;

use serde::Deserialize;

use engine::{MAX_STEPS, Requirement, StepRun, StepSpec};

/// A runnable workflow playbook.
#[derive(Debug, Clone)]
pub struct WorkflowSpec {
    pub name: String,
    pub description: Option<String>,
    pub profile: Option<String>,
    /// Prompt text, already loaded from the file or embedded in the binary.
    /// Empty when the workflow is defined entirely by steps.
    pub prompt: String,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
    /// Deterministic steps, in order. Empty for a prompt-only workflow.
    pub steps: Vec<StepSpec>,
    /// Artifacts that must exist and be non-empty when the workflow finishes.
    pub artifacts: Vec<String>,
}

/// The installable TOML shape. `prompt` is a path relative to the TOML file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowFile {
    name: Option<String>,
    description: Option<String>,
    profile: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    tools: Vec<String>,
    /// TOML spells this `[[step]]`.
    #[serde(default, rename = "step")]
    steps: Vec<StepFile>,
    #[serde(default)]
    artifacts: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    id: String,
    #[serde(default = "default_step_run")]
    run: String,
    /// Inline prompt text.
    prompt: Option<String>,
    /// Prompt file, relative to the workflow TOML. Bundled workflows cannot
    /// use it.
    prompt_file: Option<String>,
    message: Option<String>,
    when: Option<String>,
    require: Option<RequirementFile>,
    #[serde(default)]
    retry: u32,
    #[serde(default)]
    artifacts: Vec<String>,
}

fn default_step_run() -> String {
    "prompt".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequirementFile {
    tool: Option<String>,
    exit: Option<i32>,
    verify: Option<String>,
    artifact: Option<String>,
}

const BUNDLED: &[(&str, &str)] = &[
    ("api-security", include_str!("bundled/api-security.toml")),
    ("coding", include_str!("bundled/coding.toml")),
    (
        "general-assistance",
        include_str!("bundled/general-assistance.toml"),
    ),
    (
        "native-code-review",
        include_str!("bundled/native-code-review.toml"),
    ),
    ("security-blue", include_str!("bundled/security-blue.toml")),
    ("security-red", include_str!("bundled/security-red.toml")),
];

fn bundled_prompt(name: &str) -> Option<&'static str> {
    match name {
        "api-security" => Some(include_str!("bundled/prompts/api-security.md")),
        "coding" => Some(include_str!("bundled/prompts/coding.md")),
        "general-assistance" => Some(include_str!("bundled/prompts/general-assistance.md")),
        "native-code-review" => Some(include_str!("bundled/prompts/native-code-review.md")),
        "security-blue" => Some(include_str!("bundled/prompts/security-blue.md")),
        "security-red" => Some(include_str!("bundled/prompts/security-red.md")),
        _ => None,
    }
}

fn parse(name: &str, raw: &str) -> WorkflowSpec {
    let file: WorkflowFile = toml::from_str(raw)
        .unwrap_or_else(|error| panic!("bundled workflow {name} is not valid TOML: {error}"));
    assert_eq!(
        file.name.as_deref(),
        Some(name),
        "bundled workflow {name} must name itself {name}"
    );
    let prompt_ref = file
        .prompt
        .as_deref()
        .unwrap_or_else(|| panic!("bundled workflow {name} has no prompt path"));
    assert!(
        !prompt_ref.trim().is_empty(),
        "bundled workflow {name} has an empty prompt path"
    );
    let prompt = bundled_prompt(name)
        .unwrap_or_else(|| panic!("bundled workflow {name} has no embedded prompt"));
    build_spec(name, file, None, Some(prompt))
        .unwrap_or_else(|error| panic!("bundled workflow {name} is invalid: {error}"))
}

impl WorkflowSpec {
    /// Validate every named skill and return the text the CLI prepends to the
    /// first prompt step. The TUI loads skills itself; the CLI sends the same
    /// instructions in the prompt.
    pub fn skill_preamble(&self, index: &crate::skills::SkillIndex) -> Result<String, String> {
        let mut out = String::new();
        for name in &self.skills {
            let Some(skill) = index.get(name) else {
                return Err(format!(
                    "workflow {} uses unknown skill {name:?}; available: {}",
                    self.name,
                    index.names().join(", ")
                ));
            };
            out.push_str(&format!(
                "[skill: {}]\n\n{}\n\n",
                skill.name,
                skill.instructions_text()
            ));
        }
        if !out.is_empty() && !self.steps.iter().any(|step| !step.is_approval()) {
            return Err(format!(
                "workflow {} names skills but has no prompt step to load them into",
                self.name
            ));
        }
        Ok(out)
    }

    /// Apply this workflow's profile and exact tool allowlist to a config.
    /// Steps cannot call this; only the workflow-level fields are considered.
    pub fn apply_to_config(&self, config: &mut crate::config::Config) -> Result<(), String> {
        if let Some(profile) = &self.profile {
            if !config.profiles.contains_key(profile) {
                let available: Vec<&str> = config.profiles.keys().map(String::as_str).collect();
                return Err(format!(
                    "workflow {} uses unknown profile {profile:?}; available: {}",
                    self.name,
                    available.join(", ")
                ));
            }
            config.profile_name = profile.clone();
        }
        if !self.tools.is_empty() {
            let mut derived = config.profile().clone();
            // The list is exact: a tool not named here stays unavailable even
            // when its pack is part of the base profile.
            derived.packs.clear();
            derived.tools = self.tools.clone();
            derived.description = self.description.clone().or_else(|| Some(self.name.clone()));
            let derived_name = format!("workflow-{}", crate::text::slug(&self.name, '-'));
            config.profiles.insert(derived_name.clone(), derived);
            config.profile_name = derived_name;
        }
        Ok(())
    }
}

/// Parse one installable workflow file, resolving its prompt and any step
/// prompt files relative to the TOML.
pub fn load_file(path: &Path) -> Result<WorkflowSpec, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let file: WorkflowFile = toml::from_str(&raw)
        .map_err(|error| format!("invalid workflow {}: {error}", path.display()))?;
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| "workflow".into());
    build_spec(&stem, file, path.parent(), None)
        .map_err(|error| format!("invalid workflow {}: {error}", path.display()))
}

/// Read a prompt file that must stay under the workflow's directory. Absolute
/// paths, `.`/`..` segments, and symlinks that leave the directory are
/// rejected, so a workflow cannot pull arbitrary files into a prompt.
fn read_prompt(base: &Path, reference: &str, what: &str) -> Result<String, String> {
    let relative = Path::new(reference);
    if relative.is_absolute() {
        return Err(format!(
            "{what} {reference:?} must be relative to the workflow file"
        ));
    }
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(format!(
                "{what} {reference:?} must not contain . or .. segments"
            ));
        }
    }
    let base = base
        .canonicalize()
        .map_err(|error| format!("cannot resolve {}: {error}", base.display()))?;
    let target = base.join(relative);
    let real = target
        .canonicalize()
        .map_err(|error| format!("cannot read {what} {}: {error}", target.display()))?;
    if !real.starts_with(&base) {
        return Err(format!(
            "{what} {reference:?} resolves outside {}",
            base.display()
        ));
    }
    std::fs::read_to_string(&real)
        .map(|text| text.trim().to_string())
        .map_err(|error| format!("cannot read {what} {}: {error}", real.display()))
}

fn build_spec(
    fallback_name: &str,
    file: WorkflowFile,
    dir: Option<&Path>,
    embedded_prompt: Option<&str>,
) -> Result<WorkflowSpec, String> {
    let name = file
        .name
        .clone()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| fallback_name.to_string());

    let prompt = match embedded_prompt {
        Some(embedded) => embedded.trim().to_string(),
        None => match file.prompt.as_deref() {
            Some(reference) => {
                let dir = dir.ok_or_else(|| {
                    format!("workflow {name} has no directory to resolve {reference:?}")
                })?;
                read_prompt(dir, reference, "workflow prompt")?
            }
            None => String::new(),
        },
    };

    if file.steps.is_empty() && prompt.is_empty() {
        return Err("workflow has no prompt and no [[step]]".into());
    }
    if !file.steps.is_empty() && !prompt.is_empty() {
        return Err(
            "workflow sets both a top-level prompt and [[step]] tables; move the prompt into the first step"
                .into(),
        );
    }
    if file.steps.len() > MAX_STEPS {
        return Err(format!(
            "workflow has {} steps; the maximum is {MAX_STEPS}",
            file.steps.len()
        ));
    }

    let mut ids = std::collections::HashSet::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for step in file.steps {
        let id = step.id.trim().to_string();
        if !ids.insert(id.clone()) {
            return Err(format!("duplicate step id {id:?}"));
        }
        let run = match step.run.trim() {
            "prompt" => StepRun::Prompt,
            "approval" => StepRun::Approval,
            other => {
                return Err(format!(
                    "step {id:?} has run = {other:?}; use \"prompt\" or \"approval\""
                ));
            }
        };
        let prompt = match (step.prompt, step.prompt_file.as_deref()) {
            (Some(_), Some(_)) => {
                return Err(format!(
                    "step {id:?} sets both prompt and prompt_file; choose one"
                ));
            }
            (Some(text), None) => text,
            (None, Some(reference)) => {
                let dir = dir.ok_or_else(|| {
                    format!("step {id:?} uses prompt_file, which a bundled workflow cannot do")
                })?;
                read_prompt(dir, reference, &format!("step {id:?} prompt"))?
            }
            (None, None) => String::new(),
        };
        let message = step.message;
        let require = parse_requirement(&id, step.require)?;
        if run == StepRun::Approval {
            if require != Requirement::None {
                return Err(format!(
                    "approval step {id:?} cannot use require; a gate has no tool or verify result"
                ));
            }
            if step.retry > 0 {
                return Err(format!(
                    "approval step {id:?} cannot retry; ask again with a new run"
                ));
            }
            if !prompt.trim().is_empty() {
                return Err(format!("approval step {id:?} uses message, not prompt"));
            }
        }
        let (condition, prompt, artifacts) = engine::validate_step(
            &id,
            run,
            &prompt,
            message.as_deref(),
            step.when.as_deref(),
            step.retry,
            &step.artifacts,
        )
        .map_err(|error| format!("step {id:?}: {error}"))?;
        steps.push(StepSpec {
            id,
            run,
            prompt,
            message,
            when: condition,
            require,
            retry: step.retry,
            artifacts,
        });
    }

    for artifact in &file.artifacts {
        engine::validate_artifact(artifact).map_err(|error| format!("workflow {name}: {error}"))?;
    }
    let total_artifacts =
        file.artifacts.len() + steps.iter().map(|step| step.artifacts.len()).sum::<usize>();
    if total_artifacts > engine::MAX_ARTIFACTS {
        return Err(format!(
            "workflow {name} declares {total_artifacts} artifacts; the maximum is {}",
            engine::MAX_ARTIFACTS
        ));
    }

    Ok(WorkflowSpec {
        name,
        description: file.description,
        profile: file.profile,
        prompt,
        skills: file.skills,
        tools: file.tools,
        steps,
        artifacts: file.artifacts,
    })
}

fn parse_requirement(id: &str, file: Option<RequirementFile>) -> Result<Requirement, String> {
    let Some(file) = file else {
        return Ok(Requirement::None);
    };
    let given = [
        file.tool.is_some(),
        file.verify.is_some(),
        file.artifact.is_some(),
    ]
    .into_iter()
    .filter(|given| *given)
    .count();
    if given > 1 {
        return Err(format!(
            "step {id:?} sets more than one require form; use tool, verify, or artifact"
        ));
    }
    if let Some(tool) = file.tool {
        let tool = tool.trim().to_string();
        if tool.is_empty() {
            return Err(format!("step {id:?} requires an empty tool name"));
        }
        return Ok(Requirement::Tool {
            name: tool,
            exit: file.exit,
        });
    }
    if file.exit.is_some() {
        return Err(format!(
            "step {id:?} sets require.exit without require.tool"
        ));
    }
    if let Some(verify) = file.verify {
        let passed = match verify.trim() {
            "pass" => true,
            "fail" => false,
            other => {
                return Err(format!(
                    "step {id:?} has require.verify = {other:?}; use \"pass\" or \"fail\""
                ));
            }
        };
        return Ok(Requirement::Verify { passed });
    }
    if let Some(artifact) = file.artifact {
        engine::validate_artifact(&artifact).map_err(|error| format!("step {id:?}: {error}"))?;
        return Ok(Requirement::Artifact(artifact.trim().to_string()));
    }
    Ok(Requirement::None)
}

static BUNDLED_SPECS: LazyLock<Vec<WorkflowSpec>> =
    LazyLock::new(|| BUNDLED.iter().map(|(name, raw)| parse(name, raw)).collect());

/// Every bundled workflow.
pub fn all() -> &'static [WorkflowSpec] {
    &BUNDLED_SPECS[..]
}

/// A bundled workflow by name.
pub fn get(name: &str) -> Option<&'static WorkflowSpec> {
    all().iter().find(|workflow| workflow.name == name)
}

/// Bundled workflow names, in declaration order.
pub fn names() -> Vec<&'static str> {
    all()
        .iter()
        .map(|workflow| workflow.name.as_str())
        .collect()
}

/// Resolve a workflow the way `/workflow` does: global, then trusted project,
/// then bundled. A project file without trust is an error, not a fallback.
pub fn resolve(
    paths: &crate::paths::Paths,
    trust: bool,
    name: &str,
) -> Result<WorkflowSpec, String> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "invalid workflow name {name:?}; use letters, digits, - or _"
        ));
    }
    let global = paths.workflows_dir().join(format!("{name}.toml"));
    if global.is_file() {
        return load_file(&global);
    }
    let project = paths
        .project_dir()
        .join("workflows")
        .join(format!("{name}.toml"));
    if project.is_file() {
        if !trust {
            return Err(
                "project workflows require --trust; restart with --trust to load them".into(),
            );
        }
        return load_file(&project);
    }
    if let Some(bundled) = get(name) {
        return Ok(bundled.clone());
    }
    let available = available_names(paths, trust);
    Err(format!(
        "unknown workflow {name:?}; available: {}",
        if available.is_empty() {
            "none".to_string()
        } else {
            available.join(", ")
        }
    ))
}

/// Every resolvable workflow name, deduplicated and sorted.
pub fn available_names(paths: &crate::paths::Paths, trust: bool) -> Vec<String> {
    let mut names: Vec<String> = names().into_iter().map(str::to_string).collect();
    let mut scan = |dir: &Path| {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("toml")
                && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
                && !names.iter().any(|name| name == stem)
            {
                names.push(stem.to_string());
            }
        }
    };
    scan(&paths.workflows_dir());
    if trust {
        scan(&paths.project_dir().join("workflows"));
    }
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundled_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/workflows/bundled")
    }

    #[test]
    fn installable_toml_copies_parse_and_name_their_prompt() {
        assert!(!BUNDLED.is_empty());
        for (name, raw) in BUNDLED {
            let file: WorkflowFile = toml::from_str(raw)
                .unwrap_or_else(|error| panic!("{name} is not valid TOML: {error}"));
            assert_eq!(
                file.name.as_deref(),
                Some(*name),
                "TOML name must match its file stem"
            );
            let prompt = file.prompt.expect("installable copy needs a prompt path");
            let path = bundled_dir().join(&prompt);
            assert!(
                path.is_file(),
                "{} is missing at {}",
                prompt,
                path.display()
            );
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(!text.trim().is_empty(), "{prompt} is empty");
        }
    }

    #[test]
    fn workflows_are_unique_and_have_prompts() {
        let mut seen = std::collections::HashSet::new();
        for workflow in all() {
            assert!(
                seen.insert(workflow.name.clone()),
                "duplicate {}",
                workflow.name
            );
            assert!(
                !workflow.prompt.trim().is_empty(),
                "{} has no prompt",
                workflow.name
            );
            assert!(
                workflow.steps.is_empty(),
                "{} unexpectedly has steps",
                workflow.name
            );
        }
        let names = names();
        for expected in [
            "api-security",
            "coding",
            "general-assistance",
            "native-code-review",
            "security-blue",
            "security-red",
        ] {
            assert!(names.contains(&expected), "missing {expected}");
            assert!(get(expected).is_some(), "missing {expected}");
        }
        assert!(get("does-not-exist").is_none());
    }

    #[test]
    fn workflow_skills_and_tools_exist() {
        let skills = crate::skills::bundled();
        let tools = crate::tools::ToolRegistry::builtin();
        for workflow in all() {
            for skill in &workflow.skills {
                assert!(
                    skills.get(skill).is_some(),
                    "{} uses unknown skill {skill}",
                    workflow.name
                );
            }
            for tool in &workflow.tools {
                assert!(
                    tools.get(tool).is_some(),
                    "{} uses unknown tool {tool}",
                    workflow.name
                );
            }
        }
    }

    #[test]
    fn workflow_profiles_and_small_model_profiles_exist() {
        let config = crate::config::Config::builtin();
        for workflow in all() {
            if let Some(profile) = &workflow.profile {
                assert!(
                    config.profiles.contains_key(profile),
                    "{} uses unknown profile {profile}",
                    workflow.name
                );
            }
        }
        // The weak-model presets are part of the validated surface.
        for name in ["strict", "shell"] {
            assert!(
                config.profiles.contains_key(name),
                "missing built-in profile {name}"
            );
        }
        for name in ["strict", "shell"] {
            let profile = &config.profiles[name];
            let model_profile = profile
                .model_profile
                .as_ref()
                .expect("a dedicated model profile");
            assert!(
                config.model_profiles.contains_key(model_profile),
                "{name} names missing model profile {model_profile}"
            );
        }
    }

    fn load_str(raw: &str) -> Result<WorkflowSpec, String> {
        let file: WorkflowFile = toml::from_str(raw).map_err(|error| error.to_string())?;
        // Pair the embedded top-level prompt only with prompt-only workflows.
        let embedded = if file.steps.is_empty() {
            Some("top")
        } else {
            None
        };
        build_spec("demo", file, None, embedded)
    }

    #[test]
    fn a_step_workflow_parses_conditions_retries_and_approvals() {
        let spec = load_str(
            r#"
name = "release"
description = "Ship it"
profile = "coding"

[[step]]
id = "reproduce"
run = "prompt"
prompt = "Reproduce the bug"
require = { tool = "bash", exit = 0 }
retry = 1

[[step]]
id = "test"
prompt = "Run the tests"
when = "changed_files > 0"
require = { verify = "pass" }

[[step]]
id = "push"
run = "approval"
message = "Push branch and open a draft PR?"
"#,
        )
        .unwrap();
        assert_eq!(spec.steps.len(), 3);
        assert_eq!(spec.steps[0].retry, 1);
        assert!(matches!(
            spec.steps[0].require,
            Requirement::Tool { ref name, exit: Some(0) } if name == "bash"
        ));
        assert!(spec.steps[1].when.is_some());
        assert!(spec.steps[1].require == Requirement::Verify { passed: true });
        assert!(spec.steps[2].is_approval());
    }

    #[test]
    fn a_step_cannot_set_tools_or_a_profile() {
        let error = load_str(
            r#"
name = "demo"
[[step]]
id = "x"
prompt = "do it"
tools = ["bash"]
"#,
        )
        .unwrap_err();
        assert!(error.contains("unknown field"), "{error}");

        let error = load_str(
            r#"
name = "demo"
[[step]]
id = "x"
prompt = "do it"
profile = "shell"
"#,
        )
        .unwrap_err();
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn step_workflows_reject_bad_definitions() {
        for (raw, needle) in [
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nprompt = \"p\"\nwhen = \"changed_files > 0 || true\"",
                "condition",
            ),
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nprompt = \"p\"\nretry = 99",
                "maximum",
            ),
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nprompt = \"p\"\nrun = \"shell\"",
                "run",
            ),
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nprompt = \"p\"\nrequire = { artifact = \"../secret\" }",
                "artifact",
            ),
            ("name = \"d\"\n[[step]]\nid = \"X!\"\nprompt = \"p\"", "id"),
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nprompt = \"p\"\n[[step]]\nid = \"x\"\nprompt = \"q\"",
                "duplicate",
            ),
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nmessage = \"approve?\"",
                "no prompt",
            ),
        ] {
            let error = load_str(raw).unwrap_err();
            assert!(
                error.to_lowercase().contains(needle),
                "{error:?} does not mention {needle}"
            );
        }
    }

    #[test]
    fn prompt_files_cannot_leave_the_workflow_directory() {
        let dir = tempfile::tempdir().unwrap();
        let workflows = dir.path().join("workflows");
        std::fs::create_dir_all(workflows.join("prompts")).unwrap();
        std::fs::write(dir.path().join("secret.md"), "secret\n").unwrap();
        std::fs::write(workflows.join("prompts/ok.md"), "ok\n").unwrap();

        std::fs::write(
            workflows.join("escape.toml"),
            "name = \"escape\"\nprompt = \"../secret.md\"\n",
        )
        .unwrap();
        let error = load_file(&workflows.join("escape.toml")).unwrap_err();
        assert!(error.contains("must not contain"), "{error}");

        std::fs::write(
            workflows.join("absolute.toml"),
            "name = \"absolute\"\nprompt = \"/etc/passwd\"\n",
        )
        .unwrap();
        let error = load_file(&workflows.join("absolute.toml")).unwrap_err();
        assert!(error.contains("must be relative"), "{error}");

        std::fs::write(
            workflows.join("step.toml"),
            "name = \"step\"\n[[step]]\nid = \"x\"\nprompt_file = \"../secret.md\"\n",
        )
        .unwrap();
        let error = load_file(&workflows.join("step.toml")).unwrap_err();
        assert!(error.contains("must not contain"), "{error}");

        std::fs::write(
            workflows.join("ok.toml"),
            "name = \"ok\"\nprompt = \"prompts/ok.md\"\n",
        )
        .unwrap();
        assert_eq!(load_file(&workflows.join("ok.toml")).unwrap().prompt, "ok");
    }

    #[test]
    fn workflow_names_cannot_be_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = crate::paths::Paths::discover(dir.path()).unwrap();
        paths.config_dir = dir.path().join("config");
        for name in ["../../outside", "/tmp/outside", "a/b", ".", ".."] {
            let error = resolve(&paths, false, name).unwrap_err();
            assert!(error.contains("invalid workflow name"), "{name:?}: {error}");
        }
    }

    #[test]
    fn approval_steps_reject_prompt_require_and_retry() {
        for (raw, needle) in [
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nrun = \"approval\"\nmessage = \"ok?\"\nrequire = { verify = \"pass\" }",
                "cannot use require",
            ),
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nrun = \"approval\"\nmessage = \"ok?\"\nretry = 1",
                "cannot retry",
            ),
            (
                "name = \"d\"\n[[step]]\nid = \"x\"\nrun = \"approval\"\nmessage = \"ok?\"\nprompt = \"text\"",
                "uses message",
            ),
        ] {
            let error = load_str(raw).unwrap_err();
            assert!(
                error.contains(needle),
                "{error:?} does not mention {needle}"
            );
        }
    }

    #[test]
    fn a_skill_preamble_validates_every_name() {
        let mut spec = load_str("name = \"demo\"\n[[step]]\nid = \"x\"\nprompt = \"p\"").unwrap();
        let index = crate::skills::bundled();
        assert!(spec.skill_preamble(&index).unwrap().is_empty());
        spec.skills = vec!["test-repair".into()];
        let text = spec.skill_preamble(&index).unwrap();
        assert!(text.starts_with("[skill: test-repair]"), "{text}");
        spec.skills = vec!["does-not-exist".into()];
        let error = spec.skill_preamble(&index).unwrap_err();
        assert!(error.contains("unknown skill"), "{error}");
    }

    #[test]
    fn prompt_only_workflows_still_parse() {
        let spec = load_str("name = \"coding\"\ndescription = \"Code\"\n").unwrap();
        assert_eq!(spec.prompt, "top");
        assert!(spec.steps.is_empty());
    }
}

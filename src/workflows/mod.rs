//! Workflows bundled into the binary.
//!
//! A workflow is a playbook plus a profile, skill list, and tool allowlist.
//! Bundled specifications are installed by default and can be overridden by
//! global or trusted project workflow files of the same name.

use std::sync::LazyLock;

use serde::Deserialize;

/// A runnable workflow playbook.
#[derive(Debug, Clone)]
pub struct WorkflowSpec {
    pub name: String,
    pub description: Option<String>,
    pub profile: Option<String>,
    /// Prompt text, already loaded from the file or embedded in the binary.
    pub prompt: String,
    pub skills: Vec<String>,
    pub tools: Vec<String>,
}

/// The installable TOML shape. `prompt` is a path relative to the TOML file.
#[derive(Debug, Deserialize)]
struct WorkflowFile {
    name: String,
    description: Option<String>,
    profile: Option<String>,
    prompt: Option<String>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    tools: Vec<String>,
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
    WorkflowSpec {
        name: file.name,
        description: file.description,
        profile: file.profile,
        prompt: prompt.trim().to_string(),
        skills: file.skills,
        tools: file.tools,
    }
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
            assert_eq!(file.name, *name, "TOML name must match its file stem");
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
}

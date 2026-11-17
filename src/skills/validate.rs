//! One validator shared by discovery, lint, install, and the skill package
//! manager.
//!
//! Discovery treats only metadata errors as fatal, so an old skill with a long
//! body still loads. `tot skill lint` and `tot skill add` run the full pass:
//! body budget, reference reachability and size, duplicate names, and content
//! that looks like a secret or an absolute home path.

use std::sync::LazyLock;

use regex::Regex;

use super::{Shadow, Skill};

/// Agent Skills specification limit for `name`.
pub const NAME_MAX: usize = 64;
/// Agent Skills specification limit for `description`.
pub const DESCRIPTION_MAX: usize = 1_024;
/// A body over this many lines is an error in lint.
pub const BODY_LINES_MAX: usize = 500;
/// A body over this many lines is a warning in lint.
pub const BODY_LINES_WARN: usize = 300;
/// More references than this is a warning in lint.
pub const REFERENCES_WARN: usize = 10;
/// One reference document may not exceed this size.
pub const REFERENCE_MAX_BYTES: usize = 128 * 1024;

/// Frontmatter fields that claim to grant tools, scope, or permissions. The
/// harness never honors them; lint rejects them instead of silently ignoring a
/// promise the model might repeat.
const GRANT_FIELDS: &[&str] = &[
    "tools",
    "tool",
    "scope",
    "permissions",
    "permission",
    "capabilities",
    "capability",
    "grants",
    "grant",
    "preload",
    "auto-load",
    "auto_load",
    "always-load",
    "always_load",
    "always",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    /// The field or file the error points at: `name`, `description`, `body`,
    /// `references`, or a frontmatter key.
    pub field: String,
    pub code: &'static str,
    pub message: String,
}

impl Diagnostic {
    pub fn error(field: &str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            field: field.to_string(),
            code,
            message: message.into(),
        }
    }

    pub fn warning(field: &str, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            field: field.to_string(),
            code,
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "severity": self.severity.label(),
            "field": self.field,
            "code": self.code,
            "message": self.message,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Name, description, and frontmatter grants. What discovery enforces.
    Metadata,
    /// Everything, including body size, references, and content scans.
    Full,
}

/// Run the shared validator. The caller decides what to do with warnings.
pub fn validate_skill(skill: &Skill, scope: Scope) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    out.extend(validate_name(&skill.name));
    out.extend(validate_description(&skill.description));
    for key in skill.unknown.keys() {
        if GRANT_FIELDS
            .iter()
            .any(|field| field.eq_ignore_ascii_case(key))
        {
            out.push(Diagnostic::error(
                key,
                "skill_frontmatter_grants",
                format!(
                    "frontmatter field `{key}` claims to grant tools, scope, or permissions; skills cannot grant any of those. Remove it; the active profile alone decides what is available."
                ),
            ));
        }
    }
    if let (Some(dir), false) = (&skill.dir, skill.bundled)
        && let Some(expected) = dir.file_name().and_then(|name| name.to_str())
        && expected != skill.name
    {
        out.push(Diagnostic::warning(
            "name",
            "skill_name_directory_mismatch",
            format!(
                "name `{}` does not match its directory `{expected}`; rename one so the installed path and the advertised name agree",
                skill.name
            ),
        ));
    }
    if scope == Scope::Metadata {
        return out;
    }

    let body = skill.body();
    let lines = body.lines().count();
    if lines > BODY_LINES_MAX {
        out.push(Diagnostic::error(
            "body",
            "skill_body_too_long",
            format!(
                "body is {lines} lines; the limit is {BODY_LINES_MAX}. Move supporting detail into references and load it only when needed."
            ),
        ));
    } else if lines > BODY_LINES_WARN {
        out.push(Diagnostic::warning(
            "body",
            "skill_body_long",
            format!(
                "body is {lines} lines; keep the main instructions under {BODY_LINES_WARN} lines and move detail into references."
            ),
        ));
    }

    let references = skill.references();
    if references.len() > REFERENCES_WARN {
        out.push(Diagnostic::warning(
            "references",
            "skill_references_many",
            format!(
                "skill has {} references; {REFERENCES_WARN} or fewer keeps the index and loading decisions cheap.",
                references.len()
            ),
        ));
    }
    for reference in &references {
        match reference.text() {
            None => out.push(Diagnostic::error(
                "references",
                "skill_reference_unreadable",
                format!(
                    "reference {} could not be read as UTF-8 text; references must be plain Markdown or text",
                    reference.path
                ),
            )),
            Some(text) => {
                if text.len() > REFERENCE_MAX_BYTES {
                    out.push(Diagnostic::error(
                        "references",
                        "skill_reference_too_large",
                        format!(
                            "reference {} is {} bytes; the limit is {REFERENCE_MAX_BYTES} bytes",
                            reference.path,
                            text.len()
                        ),
                    ));
                }
            }
        }
    }
    for path in named_references(&body) {
        if skill.reference(&path).is_none() {
            out.push(Diagnostic::error(
                "references",
                "skill_reference_missing",
                format!(
                    "body names `{path}`, but no such reference exists in the skill; add the file or fix the path. Available references: {}",
                    available_references(skill)
                ),
            ));
        }
    }

    let mut scan = body.clone();
    for reference in &references {
        if let Some(text) = reference.text() {
            scan.push('\n');
            scan.push_str(&text);
        }
    }
    if let Some(found) = secret_shape(&scan) {
        out.push(Diagnostic::warning(
            "body",
            "skill_secret_shape",
            format!(
                "content contains a secret-shaped string ({found}); skills are shared and committed. Remove it or use a placeholder."
            ),
        ));
    }
    if let Some(path) = home_path(&scan) {
        out.push(Diagnostic::warning(
            "body",
            "skill_home_path",
            format!(
                "content contains an absolute home path ({path}); use `~` or a relative path so the skill works on another machine."
            ),
        ));
    }
    out
}

pub fn validate_name(name: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let chars: Vec<char> = name.chars().collect();
    if chars.is_empty() {
        out.push(Diagnostic::error(
            "name",
            "skill_name_missing",
            "name is required: 1-64 lowercase letters, digits, or hyphens",
        ));
        return out;
    }
    if chars.len() > NAME_MAX {
        out.push(Diagnostic::error(
            "name",
            "skill_name_too_long",
            format!(
                "name is {} characters; the Agent Skills limit is {NAME_MAX}",
                chars.len()
            ),
        ));
    }
    if let Some((index, found)) = chars
        .iter()
        .enumerate()
        .find(|(_, ch)| !(ch.is_ascii_lowercase() || ch.is_ascii_digit() || **ch == '-'))
    {
        out.push(Diagnostic::error(
            "name",
            "skill_name_invalid_char",
            format!(
                "name must be lowercase letters, digits, or hyphens; found `{found}` at position {}. Example: `test-repair`",
                index + 1
            ),
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        out.push(Diagnostic::error(
            "name",
            "skill_name_hyphen_edge",
            "name cannot begin or end with a hyphen",
        ));
    }
    if name.contains("--") {
        out.push(Diagnostic::error(
            "name",
            "skill_name_double_hyphen",
            "name cannot contain consecutive hyphens",
        ));
    }
    out
}

pub fn validate_description(description: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if description.trim().is_empty() {
        out.push(Diagnostic::warning(
            "description",
            "skill_description_missing",
            "description is empty; write one line that tells the model when this skill applies",
        ));
        return out;
    }
    let length = description.chars().count();
    if length > DESCRIPTION_MAX {
        out.push(Diagnostic::error(
            "description",
            "skill_description_too_long",
            format!(
                "description is {length} characters; the Agent Skills limit is {DESCRIPTION_MAX}"
            ),
        ));
    }
    out
}

pub fn is_valid_name(name: &str) -> bool {
    validate_name(name)
        .iter()
        .all(|diagnostic| diagnostic.severity != Severity::Error)
}

/// A duplicate that discovery resolved. The error is explicit unless the
/// winning skill declared `shadow: true` in its frontmatter, which is how a
/// user intentionally overrides a bundled or global skill.
pub fn duplicate_diagnostic(shadow: &Shadow) -> Diagnostic {
    let winner = describe(shadow.winner, shadow.winner_path.as_deref());
    let loser = describe(shadow.loser, shadow.loser_path.as_deref());
    if shadow.explicit {
        Diagnostic::warning(
            "name",
            "skill_shadow_explicit",
            format!(
                "skill `{}` from {winner} shadows {loser}; the override is explicit (`shadow: true`).",
                shadow.name
            ),
        )
    } else {
        Diagnostic::error(
            "name",
            "skill_duplicate_name",
            format!(
                "skill `{}` from {winner} shadows {loser}; names must be unique. Rename one, or set `shadow: true` on {winner} to make the override intentional.",
                shadow.name
            ),
        )
    }
}

fn describe(origin: super::SkillOrigin, path: Option<&std::path::Path>) -> String {
    match path {
        Some(path) => format!("{} ({})", origin.label(), path.display()),
        None => origin.label().to_string(),
    }
}

fn available_references(skill: &Skill) -> String {
    let references = skill.references();
    if references.is_empty() {
        "none".to_string()
    } else {
        references
            .iter()
            .map(|reference| reference.path.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

static REFERENCE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\breferences/[A-Za-z0-9_.\-/]+\.(?:md|txt)\b").expect("reference regex")
});

fn named_references(body: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for found in REFERENCE_PATH.find_iter(body) {
        let path = found.as_str().to_string();
        if !out
            .iter()
            .any(|existing| existing.eq_ignore_ascii_case(&path))
        {
            out.push(path);
        }
    }
    out
}

static SECRET_SHAPE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?x)
        (?i)
        (?:sk-[a-z0-9]{16,}
        | gh[pousr]_[a-z0-9]{20,}
        | glpat-[a-z0-9_-]{16,}
        | AKIA[0-9A-Z]{16}
        | -----BEGIN\ [A-Z\ ]*PRIVATE\ KEY-----)
        ",
    )
    .expect("secret regex")
});

fn secret_shape(text: &str) -> Option<String> {
    SECRET_SHAPE.find(text).map(|found| {
        let value = found.as_str();
        let head: String = value.chars().take(12).collect();
        format!("{head}…")
    })
}

static HOME_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/(?:home|Users)/[A-Za-z0-9._-]+").expect("home path regex"));

fn home_path(text: &str) -> Option<String> {
    HOME_PATH.find(text).map(|found| found.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_the_specification() {
        assert!(is_valid_name("test-repair"));
        assert!(is_valid_name("a1"));
        assert!(!is_valid_name("Test-Repair"));
        assert!(!is_valid_name("test_repair"));
        assert!(!is_valid_name("-leading"));
        assert!(!is_valid_name("trailing-"));
        assert!(!is_valid_name("two--hyphens"));
        assert!(!is_valid_name(&"a".repeat(65)));
        let diagnostic = validate_name("Bad Name").remove(0);
        assert_eq!(diagnostic.field, "name");
        assert_eq!(diagnostic.code, "skill_name_invalid_char");
    }

    #[test]
    fn long_descriptions_are_field_errors() {
        let diagnostics = validate_description(&"x".repeat(DESCRIPTION_MAX + 1));
        assert_eq!(diagnostics[0].field, "description");
        assert_eq!(diagnostics[0].severity, Severity::Error);
        assert!(
            validate_description(&"x".repeat(DESCRIPTION_MAX))
                .iter()
                .all(|diagnostic| diagnostic.severity != Severity::Error)
        );
    }

    #[test]
    fn reference_paths_are_extracted_without_duplicates() {
        let body = "See `references/a.md` and [b](references/b.txt), again references/a.md.";
        assert_eq!(
            named_references(body),
            ["references/a.md", "references/b.txt"]
        );
    }
}

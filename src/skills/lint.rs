//! `tot skill lint`: one report over the same validator discovery and install
//! use.

use std::path::PathBuf;

use crate::paths::Paths;

use super::validate::{self, Diagnostic, Severity};
use super::{Discovery, Skill, SkillOrigin, clipped_in_index, discover};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// Bundled, global, and trusted project skills.
    All,
    /// Global and trusted project skills, without the bundled set.
    Installed,
    /// One skill name across every origin.
    Name(String),
}

#[derive(Debug, Clone)]
pub struct LintedSkill {
    pub name: String,
    pub path: Option<PathBuf>,
    pub origin: SkillOrigin,
    pub diagnostics: Vec<Diagnostic>,
}

impl LintedSkill {
    pub fn errors(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Error)
            .count()
    }

    pub fn warnings(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Severity::Warning)
            .count()
    }
}

#[derive(Debug, Clone, Default)]
pub struct LintReport {
    pub skills: Vec<LintedSkill>,
    /// Duplicate names and explicit shadows, reported once at the report level.
    pub duplicates: Vec<Diagnostic>,
}

impl LintReport {
    pub fn errors(&self) -> usize {
        self.skills.iter().map(LintedSkill::errors).sum::<usize>()
            + self
                .duplicates
                .iter()
                .filter(|diagnostic| diagnostic.severity == Severity::Error)
                .count()
    }

    pub fn warnings(&self) -> usize {
        self.skills.iter().map(LintedSkill::warnings).sum::<usize>()
            + self
                .duplicates
                .iter()
                .filter(|diagnostic| diagnostic.severity == Severity::Warning)
                .count()
    }

    pub fn is_clean(&self) -> bool {
        self.errors() == 0
    }
}

/// Lint the selected skills. A missing named skill is a prescriptive error.
pub fn run(paths: &Paths, trust: bool, target: &Target) -> Result<LintReport, String> {
    let discovery = discover(paths, trust);
    let mut report = LintReport::default();

    let selected_shadows: Vec<super::Shadow> = discovery
        .index()
        .shadows
        .into_iter()
        .filter(|shadow| match target {
            Target::All => true,
            Target::Installed => {
                shadow.winner != SkillOrigin::Bundled && shadow.loser != SkillOrigin::Bundled
            }
            Target::Name(name) => shadow.name == *name,
        })
        .collect();

    match target {
        Target::Name(name) => {
            let mut found = false;
            for skill in &discovery.skills {
                if skill.name == *name {
                    found = true;
                    report.skills.push(LintedSkill {
                        name: skill.name.clone(),
                        path: skill.source.clone(),
                        origin: skill.origin,
                        diagnostics: lint_diagnostics(skill),
                    });
                }
            }
            for invalid in &discovery.invalid {
                if invalid.name.as_deref() == Some(name.as_str()) {
                    found = true;
                    report.skills.push(linted_invalid(invalid));
                }
            }
            if !found {
                let mut names: Vec<String> = discovery
                    .skills
                    .iter()
                    .map(|skill| skill.name.clone())
                    .collect();
                names.sort();
                return Err(format!(
                    "no skill named {name:?}; available: {}",
                    if names.is_empty() {
                        "none".to_string()
                    } else {
                        names.join(", ")
                    }
                ));
            }
        }
        Target::All | Target::Installed => {
            for skill in &discovery.skills {
                if *target == Target::Installed && skill.origin == SkillOrigin::Bundled {
                    continue;
                }
                report.skills.push(LintedSkill {
                    name: skill.name.clone(),
                    path: skill.source.clone(),
                    origin: skill.origin,
                    diagnostics: lint_diagnostics(skill),
                });
            }
            for invalid in &discovery.invalid {
                if *target == Target::Installed && invalid.origin == SkillOrigin::Bundled {
                    continue;
                }
                report.skills.push(linted_invalid(invalid));
            }
        }
    }

    for shadow in &selected_shadows {
        report
            .duplicates
            .push(validate::duplicate_diagnostic(shadow));
    }

    report.skills.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.origin.cmp(&b.origin))
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(report)
}

/// Everything lint reports for a live skill: the shared validator plus the
/// index budget warning.
fn lint_diagnostics(skill: &Skill) -> Vec<Diagnostic> {
    let mut diagnostics = validate::validate_skill(skill, validate::Scope::Full);
    if clipped_in_index(skill) {
        diagnostics.push(Diagnostic::warning(
            "description",
            "skill_index_clipped",
            format!(
                "index entry is longer than {} characters, so the system prompt cuts the description. Shorten the description below the limit (and drop a `when` hint that repeats it) so the trigger text the model sees is complete.",
                super::INDEX_ENTRY_CAP
            ),
        ));
    }
    diagnostics
}

fn linted_invalid(invalid: &super::InvalidSkill) -> LintedSkill {
    let diagnostics = match &invalid.skill {
        Some(skill) => {
            let mut diagnostics = lint_diagnostics(skill);
            // A metadata error discovered earlier is the reason this skill is
            // not live; keep it even if the full pass reports the same field.
            for diagnostic in &invalid.diagnostics {
                if !diagnostics.iter().any(|existing| {
                    existing.code == diagnostic.code && existing.field == diagnostic.field
                }) {
                    diagnostics.push(diagnostic.clone());
                }
            }
            diagnostics
        }
        None => invalid.diagnostics.clone(),
    };
    LintedSkill {
        name: invalid
            .name
            .clone()
            .or_else(|| {
                invalid
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "skill".into()),
        path: Some(invalid.path.clone()),
        origin: invalid.origin,
        diagnostics,
    }
}

/// Report used by tests and callers that only need the discovery result.
pub fn discover_for_lint(paths: &Paths, trust: bool) -> Discovery {
    discover(paths, trust)
}

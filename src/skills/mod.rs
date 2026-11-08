//! Skills: bundled, global, and trusted project instructions.
//!
//! Skills are Markdown instructions with YAML frontmatter. They cannot change
//! scope, tool lists, gates, or the endpoint; code enforces those.

use std::path::{Path, PathBuf};

use crate::paths::Paths;

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub when: Option<String>,
    pub body: String,
    pub source: Option<PathBuf>,
    pub bundled: bool,
}

impl Skill {
    pub fn full_text(&self) -> String {
        self.body.clone()
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    pub skills: Vec<Skill>,
}

impl SkillIndex {
    pub fn bundled() -> Self {
        crate::skills::bundled()
    }

    pub fn load(paths: &Paths, trust: bool) -> Self {
        let mut index = bundled();
        if let Ok(entries) = std::fs::read_dir(paths.skills_dir()) {
            for entry in entries.flatten() {
                if let Some(skill) = load_skill_dir(&entry.path()) {
                    index.upsert(skill);
                }
            }
        }
        if trust {
            let project_skills = paths.project_dir().join("skills");
            if let Ok(entries) = std::fs::read_dir(&project_skills) {
                for entry in entries.flatten() {
                    if let Some(skill) = load_skill_dir(&entry.path()) {
                        index.upsert(skill);
                    }
                }
            }
        }
        index
    }

    pub fn upsert(&mut self, skill: Skill) {
        if let Some(slot) = self.skills.iter_mut().find(|s| s.name == skill.name) {
            *slot = skill;
        } else {
            self.skills.push(skill);
        }
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect()
    }

    /// Compact index injected into the system prompt.
    pub fn index_text(&self) -> String {
        let mut out = String::new();
        for skill in &self.skills {
            out.push_str(&format!("- {}: {}", skill.name, skill.description));
            if let Some(when) = &skill.when {
                out.push_str(&format!(" (when: {when})"));
            }
            out.push('\n');
        }
        out
    }
}

fn load_skill_dir(dir: &Path) -> Option<Skill> {
    let candidates = [
        dir.join("SKILL.md"),
        dir.join("skill.md"),
        dir.to_path_buf(),
    ];
    let path = candidates.into_iter().find(|candidate| {
        candidate.is_file() && candidate.extension().is_some_and(|e| e == "md")
    })?;
    load_skill_file(&path, false)
}

/// Parse one Markdown skill file with optional YAML frontmatter.
pub fn load_skill_file(path: &Path, bundled: bool) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (meta, body) = split_frontmatter(&raw);
    let name = meta
        .get("name")
        .cloned()
        .or_else(|| {
            path.parent()
                .and_then(|p| p.file_name())
                .map(|s| s.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "skill".into());
    let description = meta
        .get("description")
        .cloned()
        .unwrap_or_else(|| first_line(body).unwrap_or("").to_string());
    let when = meta.get("when").cloned();
    Some(Skill {
        name,
        description: one_line(&description),
        when,
        body: body.trim().to_string(),
        source: Some(path.to_path_buf()),
        bundled,
    })
}

fn split_frontmatter(raw: &str) -> (std::collections::HashMap<String, String>, &str) {
    let mut meta = std::collections::HashMap::new();
    let trimmed = raw.trim_start_matches('\u{feff}');
    if let Some(rest) = trimmed.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        let front = &rest[..end];
        for line in front.lines() {
            if let Some((key, value)) = line.split_once(':') {
                let key = key.trim();
                let value = value.trim().trim_matches('"').trim_matches('\'');
                if !key.is_empty() {
                    meta.insert(key.to_string(), value.to_string());
                }
            }
        }
        let body = &rest[end + 4..];
        return (meta, body.trim_start_matches(['\r', '\n']));
    }
    (meta, trimmed)
}

fn first_line(body: &str) -> Option<&str> {
    body.lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

const RED_SKILL: &str = include_str!("bundled/security-red.md");
const BLUE_SKILL: &str = include_str!("bundled/security-blue.md");
const RED_REFERENCES: &[(&str, &str)] = &[
    (
        "references/authorization.md",
        include_str!("bundled/security-red-references/authorization.md"),
    ),
    (
        "references/methodology.md",
        include_str!("bundled/security-red-references/methodology.md"),
    ),
    (
        "references/recon.md",
        include_str!("bundled/security-red-references/recon.md"),
    ),
    (
        "references/validation.md",
        include_str!("bundled/security-red-references/validation.md"),
    ),
    (
        "references/reporting.md",
        include_str!("bundled/security-red-references/reporting.md"),
    ),
    (
        "references/lab.md",
        include_str!("bundled/security-red-references/lab.md"),
    ),
];
const BLUE_REFERENCES: &[(&str, &str)] = &[
    (
        "references/incident-triage.md",
        include_str!("bundled/security-blue-references/incident-triage.md"),
    ),
    (
        "references/detection-engineering.md",
        include_str!("bundled/security-blue-references/detection-engineering.md"),
    ),
];

fn bundled_skill(main: &str, references: &[(&str, &str)], name: &str) -> Skill {
    let (meta, body) = split_frontmatter(main);
    let mut full = body.trim().to_string();
    full.push_str("\n\n## Bundled references\n\nThese reference documents are part of the skill; they are reproduced here because they are embedded in the binary.\n");
    for (path, text) in references {
        full.push_str(&format!("\n### {path}\n\n{}\n", text.trim()));
    }
    Skill {
        name: meta.get("name").cloned().unwrap_or_else(|| name.into()),
        description: one_line(meta.get("description").map(String::as_str).unwrap_or("")),
        when: meta.get("when").cloned(),
        body: full,
        source: None,
        bundled: true,
    }
}

/// Skills compiled into the binary.
pub fn bundled() -> SkillIndex {
    SkillIndex {
        skills: vec![
            bundled_skill(RED_SKILL, RED_REFERENCES, "security-red"),
            bundled_skill(BLUE_SKILL, BLUE_REFERENCES, "security-blue"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: demo\ndescription: A demo skill\nwhen: when testing\n---\n\n# Body\n",
        )
        .unwrap();
        let skill = load_skill_file(&path, false).unwrap();
        assert_eq!(skill.name, "demo");
        assert_eq!(skill.description, "A demo skill");
        assert_eq!(skill.when.as_deref(), Some("when testing"));
        assert!(skill.body.contains("# Body"));
    }

    #[test]
    fn bundled_skills_include_references() {
        let index = bundled();
        let red = index.get("security-red").unwrap();
        assert!(red.body.contains("references/authorization.md"));
        assert!(red.body.len() > 2000);
        assert!(index.index_text().contains("security-blue"));
    }
}

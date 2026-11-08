//! Skills: bundled, global, and trusted project instructions.
//!
//! Skills are Markdown instructions with YAML frontmatter. They cannot change
//! scope, tool lists, gates, or the endpoint; code enforces those.

use std::path::{Path, PathBuf};

use crate::paths::Paths;

/// Most reference files picked up from one skill directory.
const MAX_REFERENCES: usize = 40;

/// Where a skill reference's text lives.
#[derive(Debug, Clone)]
pub enum ReferenceSource {
    Embedded(&'static str),
    File(PathBuf),
}

/// A supporting document shipped with a skill, loaded on demand.
#[derive(Debug, Clone)]
pub struct SkillReference {
    /// Path relative to the skill, e.g. `references/recon.md`.
    pub path: String,
    pub source: ReferenceSource,
}

impl SkillReference {
    pub fn text(&self) -> Option<String> {
        match &self.source {
            ReferenceSource::Embedded(text) => Some(text.trim().to_string()),
            ReferenceSource::File(path) => std::fs::read_to_string(path)
                .ok()
                .map(|text| text.trim().to_string()),
        }
    }

    /// `references/recon.md` answers to that path, `recon.md`, and `recon`.
    fn matches(&self, needle: &str) -> bool {
        let needle = needle.trim().trim_start_matches("./");
        let file = self.path.rsplit('/').next().unwrap_or(&self.path);
        let stem = file.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(file);
        needle == self.path || needle == file || needle == stem
    }
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub when: Option<String>,
    /// The main instructions, without references.
    pub body: String,
    pub references: Vec<SkillReference>,
    pub source: Option<PathBuf>,
    pub bundled: bool,
}

impl Skill {
    /// Instructions plus every reference, for `/skill` where the user asked
    /// for the whole thing.
    pub fn full_text(&self) -> String {
        let mut out = self.body.clone();
        if !self.references.is_empty() {
            out.push_str("\n\n## References\n");
            for reference in &self.references {
                if let Some(text) = reference.text() {
                    out.push_str(&format!("\n### {}\n\n{text}\n", reference.path));
                }
            }
        }
        out
    }

    /// Instructions plus a list of references the model can load one at a
    /// time with the `skill` tool.
    pub fn instructions_text(&self) -> String {
        let mut out = self.body.clone();
        if !self.references.is_empty() {
            out.push_str(&format!(
                "\n\n## References\n\nLoad one when the task needs it: skill {{\"name\":\"{}\",\"reference\":\"<path>\"}}\n",
                self.name
            ));
            for reference in &self.references {
                out.push_str(&format!("- {}\n", reference.path));
            }
        }
        out
    }

    pub fn reference(&self, needle: &str) -> Option<&SkillReference> {
        self.references
            .iter()
            .find(|reference| reference.matches(needle))
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
    let mut skill = load_skill_file(&path, false)?;
    if dir.is_dir() {
        skill.references = discover_references(dir, &path);
    }
    Some(skill)
}

/// Markdown and text files under a skill directory, other than the skill
/// file itself, sorted by path.
fn discover_references(dir: &Path, main: &Path) -> Vec<SkillReference> {
    let mut found = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            // Symlinks are skipped so a skill cannot point outside itself.
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if depth < 3 {
                    stack.push((path, depth + 1));
                }
            } else if path != main
                && path
                    .extension()
                    .is_some_and(|ext| ext == "md" || ext == "txt")
                && let Ok(relative) = path.strip_prefix(dir)
            {
                found.push(SkillReference {
                    path: relative.to_string_lossy().replace('\\', "/"),
                    source: ReferenceSource::File(path.clone()),
                });
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found.truncate(MAX_REFERENCES);
    found
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
        references: Vec::new(),
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
    (
        "references/secure-code-review.md",
        include_str!("bundled/security-blue-references/secure-code-review.md"),
    ),
    (
        "references/supply-chain.md",
        include_str!("bundled/security-blue-references/supply-chain.md"),
    ),
    (
        "references/hardening.md",
        include_str!("bundled/security-blue-references/hardening.md"),
    ),
];

fn bundled_skill(main: &str, references: &[(&str, &'static str)], name: &str) -> Skill {
    let (meta, body) = split_frontmatter(main);
    Skill {
        name: meta.get("name").cloned().unwrap_or_else(|| name.into()),
        description: one_line(meta.get("description").map(String::as_str).unwrap_or("")),
        when: meta.get("when").cloned(),
        body: body.trim().to_string(),
        references: references
            .iter()
            .map(|(path, text)| SkillReference {
                path: path.to_string(),
                source: ReferenceSource::Embedded(text),
            })
            .collect(),
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
        // The full text (for /skill) inlines references; the tool text lists them.
        assert!(red.full_text().len() > red.body.len() + 10_000);
        let listed = red.instructions_text();
        assert!(listed.contains("- references/recon.md"));
        assert!(listed.len() < red.body.len() + 1000);
        assert!(red.reference("recon").unwrap().text().unwrap().len() > 500);
    }

    #[test]
    fn every_reference_a_bundled_skill_names_is_embedded() {
        let pattern = regex::Regex::new(r"references/[a-z0-9-]+\.md").unwrap();
        for skill in bundled().skills {
            for found in pattern.find_iter(&skill.body) {
                assert!(
                    skill.reference(found.as_str()).is_some(),
                    "{} names {} but does not embed it",
                    skill.name,
                    found.as_str()
                );
            }
        }
    }

    #[test]
    fn skill_directories_expose_reference_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("deploy");
        std::fs::create_dir_all(skill.join("references")).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: deploy\ndescription: Ship it\n---\n\nSee references/rollback.md\n",
        )
        .unwrap();
        std::fs::write(skill.join("references/rollback.md"), "Roll back with x.\n").unwrap();
        std::fs::write(skill.join("notes.bin"), "ignored").unwrap();
        let loaded = load_skill_dir(&skill).unwrap();
        assert_eq!(loaded.references.len(), 1);
        assert_eq!(loaded.references[0].path, "references/rollback.md");
        assert_eq!(
            loaded.reference("rollback.md").unwrap().text().as_deref(),
            Some("Roll back with x.")
        );
    }
}

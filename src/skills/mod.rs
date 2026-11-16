//! Skills: bundled, global, project, and Agent Skills compatible instructions.
//!
//! Skills are Markdown instructions with YAML frontmatter. They cannot change
//! scope, tool lists, gates, or the endpoint; code enforces those. Discovery
//! keeps only name and description in memory; the body loads on activation and
//! a reference only when the model names it.

pub mod eval;
pub mod frontmatter;
pub mod lint;
pub mod manager;
pub mod scripts;
pub mod validate;

pub use scripts::{BundledScript, approval_required, bundled_script_in_command};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::paths::Paths;
use frontmatter::Frontmatter;
use serde_yaml::Value;

/// Most reference files picked up from one skill directory.
const MAX_REFERENCES: usize = 40;

/// One skill's line in the system-prompt index is capped at this many
/// characters, ellipsis included. A longer description stays on disk and
/// loads when the skill is activated.
pub const INDEX_ENTRY_CAP: usize = 200;

/// The whole skill index text is capped at this many characters, including
/// the trailing names-only line. Installed skills therefore cannot grow the
/// static prompt without bound.
pub const INDEX_TOTAL_CAP: usize = 2400;

/// The trailing names-only line is capped at this many characters; past that
/// the line ends with `and N more`.
pub const INDEX_NAMES_CAP: usize = 600;

/// How much of the skill index the system prompt shows. `Full` gives a capped
/// description per skill, `Names` one line of names, and `Off` nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexMode {
    #[default]
    Full,
    Names,
    Off,
}

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

/// Where a skill came from. Precedence is the declaration order: a later
/// origin wins a name collision, and native directories beat the ecosystem
/// directories at the same level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SkillOrigin {
    Bundled,
    GlobalClaude,
    GlobalAgents,
    GlobalNative,
    ProjectClaude,
    ProjectAgents,
    ProjectNative,
}

impl SkillOrigin {
    pub fn label(self) -> &'static str {
        match self {
            SkillOrigin::Bundled => "bundled",
            SkillOrigin::GlobalClaude => "~/.claude/skills",
            SkillOrigin::GlobalAgents => "~/.agents/skills",
            SkillOrigin::GlobalNative => "~/.config/localharness/skills",
            SkillOrigin::ProjectClaude => ".claude/skills",
            SkillOrigin::ProjectAgents => ".agents/skills",
            SkillOrigin::ProjectNative => ".harness/skills",
        }
    }

    pub fn is_bundled(self) -> bool {
        self == SkillOrigin::Bundled
    }
}

impl std::fmt::Display for SkillOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone)]
enum SkillText {
    Embedded(&'static str),
    File(PathBuf),
}

#[derive(Debug, Clone)]
enum ReferenceStore {
    None,
    Embedded(&'static [(&'static str, &'static str)]),
    Dir(PathBuf),
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub when: Option<String>,
    /// The `SKILL.md` (or loose native Markdown file) this skill was read from.
    pub source: Option<PathBuf>,
    /// The skill directory, when it has one.
    pub dir: Option<PathBuf>,
    pub bundled: bool,
    /// Tool packs that advertise this skill. Empty means every profile.
    pub packs: Vec<String>,
    /// `allowed-tools` from the standard frontmatter. It is an intersection
    /// helper, never a grant; see [`Skill::effective_allowed_tools`].
    pub allowed_tools: Vec<String>,
    pub origin: SkillOrigin,
    /// Frontmatter fields the harness does not interpret, preserved verbatim.
    pub unknown: BTreeMap<String, Value>,
    /// `shadow: true` marks an intentional override of a lower-precedence
    /// skill with the same name.
    pub shadow: bool,
    text: SkillText,
    reference_store: ReferenceStore,
}

impl Skill {
    /// The main instructions, without references. Read from disk on each call
    /// so discovery never preloads a body into memory or the prompt.
    pub fn body(&self) -> String {
        match &self.text {
            SkillText::Embedded(raw) => Frontmatter::parse(raw)
                .map(|front| front.body.trim().to_string())
                .unwrap_or_default(),
            SkillText::File(path) => std::fs::read_to_string(path)
                .ok()
                .and_then(|raw| Frontmatter::parse(&raw).ok())
                .map(|front| front.body.trim().to_string())
                .unwrap_or_default(),
        }
    }

    /// Reference documents in the skill directory, scanned only when a caller
    /// activates or lints the skill.
    pub fn references(&self) -> Vec<SkillReference> {
        match &self.reference_store {
            ReferenceStore::None => Vec::new(),
            ReferenceStore::Embedded(embedded) => embedded
                .iter()
                .map(|(path, text)| SkillReference {
                    path: (*path).to_string(),
                    source: ReferenceSource::Embedded(text),
                })
                .collect(),
            ReferenceStore::Dir(dir) => {
                let main = match &self.text {
                    SkillText::File(path) => Some(path.clone()),
                    SkillText::Embedded(_) => None,
                };
                discover_references(dir, main.as_deref())
            }
        }
    }

    /// Instructions plus every reference, for `/skill` where the user asked
    /// for the whole thing.
    pub fn full_text(&self) -> String {
        let mut out = self.body();
        let references = self.references();
        if !references.is_empty() {
            out.push_str("\n\n## References\n");
            for reference in &references {
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
        let mut out = self.body();
        let references = self.references();
        if !references.is_empty() {
            out.push_str(&format!(
                "\n\n## References\n\nLoad one when the task needs it: skill {{\"name\":\"{}\",\"reference\":\"<path>\"}}\n",
                self.name
            ));
            for reference in &references {
                out.push_str(&format!("- {}\n", reference.path));
            }
        }
        out
    }

    pub fn reference(&self, needle: &str) -> Option<SkillReference> {
        self.references()
            .into_iter()
            .find(|reference| reference.matches(needle))
    }

    /// `allowed-tools` intersected with the tools the active profile actually
    /// has. The result can only shrink: a skill can never enable a tool the
    /// profile did not select.
    pub fn effective_allowed_tools(&self, active: &[String]) -> Vec<String> {
        self.allowed_tools
            .iter()
            .filter(|allowed| active.iter().any(|tool| tool == *allowed))
            .cloned()
            .collect()
    }
}

/// A name collision discovery resolved. `winner` is the skill that is visible;
/// `loser` was shadowed.
#[derive(Debug, Clone)]
pub struct Shadow {
    pub name: String,
    pub winner: SkillOrigin,
    pub winner_path: Option<PathBuf>,
    pub loser: SkillOrigin,
    pub loser_path: Option<PathBuf>,
    /// The winner declared `shadow: true`, so lint reports a warning instead
    /// of a duplicate-name error.
    pub explicit: bool,
}

/// A skill that failed metadata validation during discovery. Lint still gets
/// the parsed skill so it can report body and reference problems too.
#[derive(Debug, Clone)]
pub struct InvalidSkill {
    pub path: PathBuf,
    pub origin: SkillOrigin,
    pub name: Option<String>,
    pub diagnostics: Vec<validate::Diagnostic>,
    pub skill: Option<Skill>,
}

/// Every skill found on disk, before name resolution.
#[derive(Debug, Clone, Default)]
pub struct Discovery {
    /// Valid skills in precedence order (lowest first).
    pub skills: Vec<Skill>,
    pub invalid: Vec<InvalidSkill>,
}

impl Discovery {
    /// Resolve name collisions deterministically. Later origins win; every
    /// replacement is recorded as a shadow.
    pub fn index(&self) -> SkillIndex {
        let mut index = SkillIndex::default();
        for skill in &self.skills {
            if let Some(slot) = index
                .skills
                .iter_mut()
                .find(|existing| existing.name == skill.name)
            {
                let previous = slot.clone();
                index.shadows.push(Shadow {
                    name: skill.name.clone(),
                    winner: skill.origin,
                    winner_path: skill.source.clone(),
                    loser: previous.origin,
                    loser_path: previous.source.clone(),
                    explicit: skill.shadow,
                });
                *slot = skill.clone();
            } else {
                index.skills.push(skill.clone());
            }
        }
        index
    }
}

#[derive(Debug, Clone, Default)]
pub struct SkillIndex {
    pub skills: Vec<Skill>,
    pub shadows: Vec<Shadow>,
}

impl SkillIndex {
    pub fn bundled() -> Self {
        Self {
            skills: bundled_entries(),
            shadows: Vec::new(),
        }
    }

    /// Discover bundled, global, standard, and trusted project skills.
    pub fn load(paths: &Paths, trust: bool) -> Self {
        discover(paths, trust).index()
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

    /// Skills visible to the active tool packs. A skill with no pack
    /// restriction is always visible; a pack-specific skill stays loadable by
    /// name, it just stops cluttering the index when its pack is off.
    pub fn advertised_for(&self, packs: &[String]) -> SkillIndex {
        SkillIndex {
            skills: self
                .skills
                .iter()
                .filter(|skill| {
                    skill.packs.is_empty()
                        || skill
                            .packs
                            .iter()
                            .any(|pack| packs.iter().any(|active| active == pack))
                })
                .cloned()
                .collect(),
            shadows: self.shadows.clone(),
        }
    }

    /// Compact index injected into the system prompt. Only name, description,
    /// and the optional trigger hint are included; bodies and references stay
    /// on disk until the skill is activated. Each entry is clipped to
    /// [`INDEX_ENTRY_CAP`] at a word boundary, and entries stop once the whole
    /// index would pass [`INDEX_TOTAL_CAP`]; the remaining skills are listed by
    /// name so they stay discoverable and loadable with the `skill` tool.
    pub fn index_text(&self) -> String {
        self.index_text_with(IndexMode::Full)
    }

    /// The index in the requested [`IndexMode`]. `Names` writes one line of
    /// names and `Off` writes nothing, for small context windows.
    pub fn index_text_with(&self, mode: IndexMode) -> String {
        let ordered = self.index_order();
        match mode {
            IndexMode::Off => String::new(),
            IndexMode::Names => {
                let names: Vec<&str> = ordered.iter().map(|skill| skill.name.as_str()).collect();
                // Leave room for the trailing newline inside the total cap.
                let mut line = name_list(&names, INDEX_TOTAL_CAP - 1);
                if !line.is_empty() {
                    line.push('\n');
                }
                line
            }
            IndexMode::Full => full_index(&ordered),
        }
    }

    /// Skills in index order. Shared home-directory skills (`~/.claude` and
    /// `~/.agents`) move behind everything else so a library installed for
    /// another agent cannot crowd out the bundled, native, and project skills
    /// this workspace selected. The sort is stable, so precedence is otherwise
    /// unchanged.
    fn index_order(&self) -> Vec<&Skill> {
        let mut ordered: Vec<&Skill> = self.skills.iter().collect();
        ordered.sort_by_key(|skill| match skill.origin {
            SkillOrigin::GlobalClaude | SkillOrigin::GlobalAgents => 1u8,
            _ => 0u8,
        });
        ordered
    }
}

/// Write full entries until the total cap, then one names-only line. Entries
/// are moved back into the name list until both fit inside
/// [`INDEX_TOTAL_CAP`].
fn full_index(ordered: &[&Skill]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0usize;
    let mut rest_from = ordered.len();
    for (index, skill) in ordered.iter().enumerate() {
        let line = index_entry(skill);
        let projected = used + line.chars().count() + 1;
        if projected > INDEX_TOTAL_CAP {
            rest_from = index;
            break;
        }
        used = projected;
        lines.push(line);
    }

    let mut rest: Vec<&Skill> = ordered[rest_from..].to_vec();
    while !rest.is_empty() {
        let tail = names_tail(&rest, INDEX_NAMES_CAP);
        if used + tail.chars().count() < INDEX_TOTAL_CAP {
            break;
        }
        // Make room by moving the last full entry back into the name list.
        if rest_from == 0 {
            break;
        }
        let Some(line) = lines.pop() else {
            break;
        };
        used = used.saturating_sub(line.chars().count() + 1);
        rest_from -= 1;
        rest.insert(0, ordered[rest_from]);
    }

    let mut out = String::new();
    for line in &lines {
        out.push_str(line);
        out.push('\n');
    }
    if !rest.is_empty() {
        let tail = names_tail(&rest, INDEX_NAMES_CAP);
        if !tail.is_empty() {
            out.push_str(&tail);
            out.push('\n');
        }
    }
    out
}

fn names_tail(skills: &[&Skill], budget: usize) -> String {
    const HEAD: &str = "- more (load with the skill tool by name): ";
    let head = HEAD.chars().count();
    if budget <= head {
        return String::new();
    }
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    format!("{HEAD}{}", name_list(&names, budget - head))
}

/// Comma-separated names bounded by `cap` characters. Past the cap the list
/// ends with `and N more`, so the reader knows names exist that are not shown.
fn name_list(names: &[&str], cap: usize) -> String {
    let mut shown = names.len();
    loop {
        let omitted = names.len() - shown;
        let body = names[..shown].join(", ");
        let line = if omitted == 0 {
            body
        } else if body.is_empty() {
            format!("and {omitted} more")
        } else {
            format!("{body}, and {omitted} more")
        };
        if line.chars().count() <= cap || shown == 0 {
            return line;
        }
        shown -= 1;
    }
}

/// The index line for one skill, before the per-entry cap is applied.
fn index_entry_uncapped(skill: &Skill) -> String {
    let mut line = format!("- {}: {}", skill.name, skill.description);
    if let Some(when) = &skill.when {
        line.push_str(&format!(" (when: {when})"));
    }
    line
}

/// True when the description and trigger hint are too long for the index and
/// the model would see a cut entry. `lh skill lint` reports these so an author
/// can shorten the text instead of losing the trigger words.
pub fn clipped_in_index(skill: &Skill) -> bool {
    index_entry_uncapped(skill).chars().count() > INDEX_ENTRY_CAP
}

/// One index line, cut at a word boundary and marked with an ellipsis when it
/// passes [`INDEX_ENTRY_CAP`].
fn index_entry(skill: &Skill) -> String {
    let line = index_entry_uncapped(skill);
    if line.chars().count() <= INDEX_ENTRY_CAP {
        return line;
    }
    let head: Vec<char> = line.chars().take(INDEX_ENTRY_CAP - 1).collect();
    let boundary = head
        .iter()
        .rposition(|ch| ch.is_whitespace())
        .unwrap_or(head.len());
    // Back up to a word boundary only when it is near the cap. A single very
    // long token (a URL, a hash) should not throw away most of the entry.
    let cut = if boundary + 32 >= head.len() {
        boundary
    } else {
        head.len()
    };
    let mut out: String = head[..cut].iter().collect();
    while out.ends_with(|ch: char| ch.is_whitespace() || ch == ',' || ch == ';' || ch == ':') {
        out.pop();
    }
    out.push('…');
    out
}

/// Scan every skill root for `paths` and return every discovered skill in
/// precedence order.
pub fn discover(paths: &Paths, trust: bool) -> Discovery {
    let mut discovery = Discovery::default();
    for skill in bundled_entries() {
        discovery.skills.push(skill);
    }
    if let Some(home) = &paths.home_dir {
        scan_root(
            &mut discovery,
            &home.join(".claude/skills"),
            SkillOrigin::GlobalClaude,
            false,
        );
        scan_root(
            &mut discovery,
            &home.join(".agents/skills"),
            SkillOrigin::GlobalAgents,
            false,
        );
    }
    scan_root(
        &mut discovery,
        &paths.skills_dir(),
        SkillOrigin::GlobalNative,
        true,
    );
    if trust {
        scan_root(
            &mut discovery,
            &paths.cwd.join(".claude/skills"),
            SkillOrigin::ProjectClaude,
            false,
        );
        scan_root(
            &mut discovery,
            &paths.cwd.join(".agents/skills"),
            SkillOrigin::ProjectAgents,
            false,
        );
        scan_root(
            &mut discovery,
            &paths.project_dir().join("skills"),
            SkillOrigin::ProjectNative,
            true,
        );
    }
    discovery
}

/// Read one skill directory, for lint and the package manager. `dir` may be a
/// directory containing `SKILL.md`, or a loose native `.md` file.
pub fn read_skill_dir(dir: &Path, origin: SkillOrigin) -> Result<Skill, Box<InvalidSkill>> {
    if dir.is_file() {
        return parse_skill_file(dir, None, origin);
    }
    let candidates = ["SKILL.md", "skill.md"];
    let main = candidates
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| candidate.is_file());
    if let Some(main) = main {
        return parse_skill_file(&main, Some(dir.to_path_buf()), origin);
    }
    Err(Box::new(InvalidSkill {
        path: dir.to_path_buf(),
        origin,
        name: dir
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string),
        diagnostics: vec![validate::Diagnostic::error(
            "path",
            "skill_file_missing",
            format!(
                "no SKILL.md in {}; a skill is a directory with SKILL.md at its root",
                dir.display()
            ),
        )],
        skill: None,
    }))
}

fn scan_root(discovery: &mut Discovery, root: &Path, origin: SkillOrigin, native: bool) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    for path in paths {
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            // A skill directory is self-contained; a symlink could point at an
            // unrelated tree, and install never follows one.
            continue;
        }
        if metadata.is_dir() {
            if path.join("SKILL.md").is_file() || path.join("skill.md").is_file() {
                match parse_skill_file(
                    &path.join(if path.join("SKILL.md").is_file() {
                        "SKILL.md"
                    } else {
                        "skill.md"
                    }),
                    Some(path.clone()),
                    origin,
                ) {
                    Ok(skill) => discovery.skills.push(skill),
                    Err(invalid) => discovery.invalid.push(*invalid),
                }
            }
        } else if native && path.extension().is_some_and(|extension| extension == "md") {
            // Native installs historically accepted a loose `<name>.md` in the
            // skills directory. Keep that shape working.
            match parse_skill_file(&path, None, origin) {
                Ok(skill) => discovery.skills.push(skill),
                Err(invalid) => discovery.invalid.push(*invalid),
            }
        }
    }
}

fn parse_skill_file(
    path: &Path,
    dir: Option<PathBuf>,
    origin: SkillOrigin,
) -> Result<Skill, Box<InvalidSkill>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) => {
            return Err(Box::new(InvalidSkill {
                path: path.to_path_buf(),
                origin,
                name: dir
                    .as_deref()
                    .or_else(|| path.parent())
                    .and_then(|parent| parent.file_name())
                    .and_then(|name| name.to_str())
                    .map(str::to_string),
                diagnostics: vec![validate::Diagnostic::error(
                    "path",
                    "skill_file_unreadable",
                    format!("cannot read {}: {error}", path.display()),
                )],
                skill: None,
            }));
        }
    };
    match build_skill(&raw, path, dir, origin) {
        Ok(skill) => {
            let diagnostics = validate::validate_skill(&skill, validate::Scope::Metadata);
            if diagnostics
                .iter()
                .any(|diagnostic| diagnostic.severity == validate::Severity::Error)
            {
                Err(Box::new(InvalidSkill {
                    path: path.to_path_buf(),
                    origin,
                    name: Some(skill.name.clone()),
                    diagnostics,
                    skill: Some(skill),
                }))
            } else {
                Ok(skill)
            }
        }
        Err(message) => Err(Box::new(InvalidSkill {
            path: path.to_path_buf(),
            origin,
            name: None,
            diagnostics: vec![validate::Diagnostic::error(
                "frontmatter",
                "skill_frontmatter_invalid",
                message,
            )],
            skill: None,
        })),
    }
}

fn build_skill(
    raw: &str,
    path: &Path,
    dir: Option<PathBuf>,
    origin: SkillOrigin,
) -> Result<Skill, String> {
    let front = Frontmatter::parse(raw)?;
    let fallback_name = dir
        .as_deref()
        .and_then(|dir| dir.file_name())
        .or_else(|| path.file_stem())
        .and_then(|name| name.to_str())
        .unwrap_or("skill")
        .to_string();
    let name = front.scalar("name").unwrap_or(fallback_name);
    let description = front
        .scalar("description")
        .unwrap_or_else(|| first_line(&front.body).unwrap_or("").to_string());
    let description = one_line(&description);
    let references = dir
        .as_ref()
        .map(|dir| ReferenceStore::Dir(dir.clone()))
        .unwrap_or(ReferenceStore::None);
    Ok(Skill {
        name,
        description,
        when: front.scalar("when"),
        source: Some(path.to_path_buf()),
        dir,
        bundled: origin.is_bundled(),
        packs: front.list("packs"),
        allowed_tools: front.list("allowed-tools"),
        origin,
        unknown: front.unknown(),
        shadow: front.truthy("shadow"),
        text: SkillText::File(path.to_path_buf()),
        reference_store: references,
    })
}

/// Markdown and text files under a skill directory, other than the skill
/// file itself, sorted by path.
fn discover_references(dir: &Path, main: Option<&Path>) -> Vec<SkillReference> {
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
            } else if main != Some(path.as_path())
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
/// The bundled coding pack. Each skill is a single self-contained document;
/// they are advertised while the `coding` pack is active and stay loadable by
/// name from any profile.
const CODING_SKILLS: &[(&str, &str)] = &[
    ("test-repair", include_str!("bundled/test-repair.md")),
    ("code-review", include_str!("bundled/code-review.md")),
    ("refactor", include_str!("bundled/refactor.md")),
    (
        "dependency-upgrade",
        include_str!("bundled/dependency-upgrade.md"),
    ),
    ("perf-profile", include_str!("bundled/perf-profile.md")),
    (
        "incident-triage",
        include_str!("bundled/incident-triage.md"),
    ),
    ("release-notes", include_str!("bundled/release-notes.md")),
    ("issue-to-pr", include_str!("bundled/issue-to-pr.md")),
];

const METHODS_SKILL: &str = include_str!("bundled/security-methods.md");
const METHODS_REFERENCES: &[(&str, &str)] = &[
    (
        "references/native-memory.md",
        include_str!("bundled/security-methods-references/native-memory.md"),
    ),
    (
        "references/web-injection.md",
        include_str!("bundled/security-methods-references/web-injection.md"),
    ),
    (
        "references/auth-and-access.md",
        include_str!("bundled/security-methods-references/auth-and-access.md"),
    ),
    (
        "references/ssrf-and-request-smuggling.md",
        include_str!("bundled/security-methods-references/ssrf-and-request-smuggling.md"),
    ),
    (
        "references/deserialization-and-prototype-pollution.md",
        include_str!(
            "bundled/security-methods-references/deserialization-and-prototype-pollution.md"
        ),
    ),
    (
        "references/llm-application-security.md",
        include_str!("bundled/security-methods-references/llm-application-security.md"),
    ),
];

fn bundled_skill(
    main: &'static str,
    references: &'static [(&'static str, &'static str)],
    fallback_name: &str,
    packs: &[&str],
) -> Skill {
    let front = Frontmatter::parse(main).unwrap_or_else(|error| {
        panic!("bundled skill {fallback_name} has invalid frontmatter: {error}")
    });
    Skill {
        name: front.scalar("name").unwrap_or_else(|| fallback_name.into()),
        description: one_line(front.scalar("description").as_deref().unwrap_or("")),
        when: front.scalar("when"),
        source: None,
        dir: None,
        bundled: true,
        packs: packs.iter().map(|pack| (*pack).to_string()).collect(),
        allowed_tools: front.list("allowed-tools"),
        origin: SkillOrigin::Bundled,
        unknown: front.unknown(),
        shadow: front.truthy("shadow"),
        text: SkillText::Embedded(main),
        reference_store: ReferenceStore::Embedded(references),
    }
}

fn bundled_entries() -> Vec<Skill> {
    let mut skills = vec![
        bundled_skill(RED_SKILL, RED_REFERENCES, "security-red", &["security"]),
        bundled_skill(BLUE_SKILL, BLUE_REFERENCES, "security-blue", &["security"]),
        bundled_skill(
            METHODS_SKILL,
            METHODS_REFERENCES,
            "security-methods",
            &["security"],
        ),
    ];
    for (name, raw) in CODING_SKILLS {
        skills.push(bundled_skill(raw, &[], name, &["coding"]));
    }
    skills
}

/// Skills compiled into the binary.
pub fn bundled() -> SkillIndex {
    SkillIndex::bundled()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path) -> Paths {
        let mut paths = Paths::discover(dir).unwrap();
        paths.config_dir = dir.join("config");
        paths.data_dir = dir.join("data");
        paths.home_dir = Some(dir.join("home"));
        paths
    }

    fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    /// A skill with embedded instructions, for index tests that never touch
    /// disk.
    fn synthetic_skill(name: &str, description: &str, origin: SkillOrigin) -> Skill {
        Skill {
            name: name.to_string(),
            description: description.to_string(),
            when: None,
            source: None,
            dir: None,
            bundled: origin.is_bundled(),
            packs: Vec::new(),
            allowed_tools: Vec::new(),
            origin,
            unknown: std::collections::BTreeMap::new(),
            shadow: false,
            text: SkillText::Embedded(
                "---\nname: synthetic\ndescription: synthetic\n---\n\nBODY\n",
            ),
            reference_store: ReferenceStore::None,
        }
    }

    /// A description at the Agent Skills limit, long enough that every entry
    /// must be clipped.
    fn long_description() -> String {
        let mut description = String::new();
        while description.len() < validate::DESCRIPTION_MAX {
            description.push_str("remedy ");
        }
        description.truncate(validate::DESCRIPTION_MAX);
        description
    }

    #[test]
    fn frontmatter_parses() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        std::fs::write(
            &path,
            "---\nname: demo\ndescription: A demo skill\nwhen: when testing\n---\n\n# Body\n",
        )
        .unwrap();
        let skill = read_skill_dir(dir.path(), SkillOrigin::GlobalNative).unwrap();
        assert_eq!(skill.name, "demo");
        assert_eq!(skill.description, "A demo skill");
        assert_eq!(skill.when.as_deref(), Some("when testing"));
        assert!(skill.body().contains("# Body"));
    }

    #[test]
    fn bundled_skills_include_references() {
        let index = bundled();
        let red = index.get("security-red").unwrap();
        assert!(red.body().contains("references/authorization.md"));
        assert!(red.body().len() > 2000);
        assert!(index.index_text().contains("security-blue"));
        // The full text (for /skill) inlines references; the tool text lists them.
        assert!(red.full_text().len() > red.body().len() + 10_000);
        let listed = red.instructions_text();
        assert!(listed.contains("- references/recon.md"));
        assert!(listed.len() < red.body().len() + 1000);
        assert!(red.reference("recon").unwrap().text().unwrap().len() > 500);
    }

    #[test]
    fn security_skills_follow_the_active_packs() {
        let all = bundled();
        let coding = all.advertised_for(&["core".into(), "coding".into()]);
        assert!(coding.get("security-red").is_none());
        assert!(coding.get("security-blue").is_none());
        // Filtering only affects the index; an explicit load still resolves.
        assert!(all.get("security-red").is_some());

        let full = all.advertised_for(&["core".into(), "coding".into(), "security".into()]);
        assert!(full.get("security-red").is_some());
        assert!(full.get("security-blue").is_some());
        assert!(full.get("security-methods").is_some());
    }

    #[test]
    fn every_reference_a_bundled_skill_names_is_embedded() {
        let pattern = regex::Regex::new(r"references/[a-z0-9-]+\.md").unwrap();
        for skill in bundled().skills {
            for found in pattern.find_iter(&skill.body()) {
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
    fn security_methods_skill_is_bundled_with_reference_documents() {
        let index = bundled();
        let skill = index.get("security-methods").expect("bundled skill");
        assert!(skill.bundled);
        assert_eq!(skill.references().len(), 6);
        for needle in [
            "native-memory",
            "web-injection",
            "auth-and-access",
            "ssrf-and-request-smuggling",
            "deserialization-and-prototype-pollution",
            "llm-application-security",
        ] {
            let reference = skill
                .reference(needle)
                .unwrap_or_else(|| panic!("missing reference {needle}"));
            let text = reference.text().expect("embedded reference text");
            assert!(text.len() > 500, "reference {needle} is too thin");
        }
        assert!(skill.body().contains("security-red"));
        assert!(skill.body().contains("poc_steps"));
        assert!(skill.body().contains("finding verify"));
        assert!(skill.full_text().contains("MIT OR Apache-2.0"));
        let banned = [
            "save_finding",
            "verify_finding",
            "variant_hunt",
            "generate_fix",
            "load_skill",
            "list_skills",
        ];
        for name in banned {
            assert!(!skill.body().contains(name), "body still names {name}");
            for reference in &skill.references() {
                let text = reference.text().unwrap();
                assert!(
                    !text.contains(name),
                    "{} still names {name}",
                    reference.path
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
        let loaded = read_skill_dir(&skill, SkillOrigin::GlobalNative).unwrap();
        assert_eq!(loaded.references().len(), 1);
        assert_eq!(loaded.references()[0].path, "references/rollback.md");
        assert_eq!(
            loaded.reference("rollback.md").unwrap().text().as_deref(),
            Some("Roll back with x.")
        );
    }

    const CODING_PACK: &[&str] = &[
        "test-repair",
        "code-review",
        "refactor",
        "dependency-upgrade",
        "perf-profile",
        "incident-triage",
        "release-notes",
        "issue-to-pr",
    ];

    #[test]
    fn coding_pack_is_bundled_and_complete() {
        let index = bundled();
        for name in CODING_PACK {
            let skill = index
                .get(name)
                .unwrap_or_else(|| panic!("missing bundled skill {name}"));
            assert!(skill.bundled, "{name} is not marked bundled");
            assert!(!skill.body().trim().is_empty(), "{name} has no body");
            assert!(
                !skill.description.trim().is_empty(),
                "{name} has no description"
            );
            assert!(
                !clipped_in_index(skill),
                "{name} does not fit the index entry cap"
            );
            assert!(
                skill.body().lines().count() < 120,
                "{name} is longer than 120 lines"
            );
            assert!(
                skill.body().contains("Stop and"),
                "{name} does not say when to stop"
            );
            assert!(
                skill.full_text().chars().count() < 24_000,
                "{name} exceeds the skill result cap"
            );
        }
        // The index injected into the system prompt names every one.
        let index_text = index.index_text();
        for name in CODING_PACK {
            assert!(index_text.contains(name), "index is missing {name}");
        }
    }

    #[test]
    fn bundled_entries_fit_the_cap_and_shrink_the_prompt() {
        // Before the cap, the eleven bundled entries with their `when` hints
        // repeated totaled 3557 characters. The incident asks for at least 500
        // fewer, with every entry inside the per-entry cap.
        let index = bundled();
        for skill in &index.skills {
            assert!(
                !clipped_in_index(skill),
                "{} would be clipped in the index",
                skill.name
            );
        }
        let length = index.index_text().chars().count();
        assert!(
            length <= 3557 - 500,
            "bundled index is {length} characters; the cap must save at least 500"
        );
    }

    #[test]
    fn index_caps_entries_and_total_and_keeps_every_name() {
        // Sixty skills at the Agent Skills description limit. The full entry
        // budget is INDEX_TOTAL_CAP; the trailing names line lives inside that
        // same budget and is itself at most INDEX_NAMES_CAP characters, so the
        // whole text never exceeds INDEX_TOTAL_CAP.
        let description = long_description();
        let skills: Vec<Skill> = (0..60)
            .map(|index| {
                synthetic_skill(
                    &format!("skill-{index:02}"),
                    &description,
                    SkillOrigin::GlobalNative,
                )
            })
            .collect();
        let index = SkillIndex {
            skills,
            shadows: Vec::new(),
        };
        let text = index.index_text();
        let total = text.chars().count();
        assert!(
            total <= INDEX_TOTAL_CAP,
            "index is {total} characters; the total cap is {INDEX_TOTAL_CAP}"
        );
        assert!(
            text.contains("- more (load with the skill tool by name):"),
            "the index does not list the skills past the cap by name"
        );
        for line in text.lines() {
            if line.starts_with("- more (load with the skill tool by name):") {
                assert!(
                    line.chars().count() <= INDEX_NAMES_CAP,
                    "names tail is {} characters; the cap is {INDEX_NAMES_CAP}",
                    line.chars().count()
                );
                continue;
            }
            assert!(
                line.chars().count() <= INDEX_ENTRY_CAP,
                "entry is {} characters: {line}",
                line.chars().count()
            );
        }
        // Clipping keeps the entry discoverable even when the description is
        // cut, and skills past the cap stay in the names line.
        for skill in &index.skills {
            assert!(
                text.contains(&skill.name),
                "{} disappeared from the index",
                skill.name
            );
        }
    }

    #[test]
    fn a_skill_past_the_cap_stays_loadable_by_name() {
        let description = long_description();
        let skills: Vec<Skill> = (0..60)
            .map(|index| {
                synthetic_skill(
                    &format!("skill-{index:02}"),
                    &description,
                    SkillOrigin::GlobalNative,
                )
            })
            .collect();
        let index = SkillIndex {
            skills,
            shadows: Vec::new(),
        };
        let text = index.index_text();
        let past = index.get("skill-59").expect("past the cap");
        // The prompt carries the name, and the full description never does.
        assert!(text.contains("skill-59"));
        assert!(!text.contains(&past.description));
        // The tool resolves against the index, not the clipped text.
        assert!(past.instructions_text().contains("BODY"));
    }

    #[test]
    fn index_modes_list_names_or_nothing() {
        let index = SkillIndex {
            skills: vec![
                synthetic_skill("alpha", "Alpha description", SkillOrigin::Bundled),
                synthetic_skill("beta", "Beta description", SkillOrigin::GlobalNative),
            ],
            shadows: Vec::new(),
        };
        let names = index.index_text_with(IndexMode::Names);
        assert_eq!(names.trim_end(), "alpha, beta");
        assert!(!names.contains("description"));
        assert!(names.chars().count() <= INDEX_TOTAL_CAP);
        assert_eq!(index.index_text_with(IndexMode::Off), "");
        assert_eq!(index.index_text(), index.index_text_with(IndexMode::Full));

        assert_eq!(IndexMode::default(), IndexMode::Full);
        assert_eq!(
            serde_json::from_str::<IndexMode>("\"full\"").unwrap(),
            IndexMode::Full
        );
        assert_eq!(
            serde_json::from_str::<IndexMode>("\"names\"").unwrap(),
            IndexMode::Names
        );
        assert_eq!(
            serde_json::from_str::<IndexMode>("\"off\"").unwrap(),
            IndexMode::Off
        );
        assert!(serde_json::from_str::<IndexMode>("\"Off\"").is_err());
    }

    #[test]
    fn clipped_in_index_flags_entries_past_the_cap() {
        let prefix = "- demo: ".len();
        let exact = synthetic_skill(
            "demo",
            &"x".repeat(INDEX_ENTRY_CAP - prefix),
            SkillOrigin::Bundled,
        );
        assert!(!clipped_in_index(&exact));
        assert_eq!(index_entry(&exact).chars().count(), INDEX_ENTRY_CAP);

        let long = synthetic_skill("demo", &"x".repeat(INDEX_ENTRY_CAP), SkillOrigin::Bundled);
        assert!(clipped_in_index(&long));
        let clipped = index_entry(&long);
        assert_eq!(clipped.chars().count(), INDEX_ENTRY_CAP);
        assert!(clipped.ends_with('…'));

        // A cut inside a word backs up to the whitespace before it.
        let words = synthetic_skill("demo", &"alpha ".repeat(60), SkillOrigin::Bundled);
        assert!(clipped_in_index(&words));
        let clipped = index_entry(&words);
        let stem = clipped.strip_suffix('…').expect("ellipsis");
        let full = index_entry_uncapped(&words);
        assert!(full.starts_with(stem));
        assert!(
            full[stem.len()..]
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        );
    }

    #[test]
    fn shared_home_skills_sort_behind_bundled_native_and_project_skills() {
        let long = long_description();
        let mut skills: Vec<Skill> = (0..20)
            .map(|index| {
                synthetic_skill(
                    &format!("home-{index:02}"),
                    &long,
                    SkillOrigin::GlobalClaude,
                )
            })
            .collect();
        skills.push(synthetic_skill(
            "project",
            "Project skill",
            SkillOrigin::ProjectNative,
        ));
        skills.push(synthetic_skill(
            "native",
            "Native skill",
            SkillOrigin::GlobalNative,
        ));
        let index = SkillIndex {
            skills,
            shadows: Vec::new(),
        };
        let text = index.index_text();
        let first_home = text.find("- home-00").expect("full home entry");
        for name in ["project", "native"] {
            let position = text
                .find(&format!("- {name}: "))
                .unwrap_or_else(|| panic!("{name} has no full entry"));
            assert!(
                position < first_home,
                "{name} sorted behind a shared home skill"
            );
        }
    }

    #[test]
    fn coding_skills_name_only_tools_the_pack_has() {
        let index = bundled();
        let registry = crate::tools::ToolRegistry::builtin();
        // Security-only tools are not in the coding pack; a coding skill that
        // tells the model to call one would name a tool it cannot use.
        let security_only = [
            "finding",
            "iocs",
            "surface",
            "evidence",
            "detect",
            "attack_path",
        ];
        let backticked = regex::Regex::new(r"`([a-z][a-z0-9_]*)`").unwrap();
        let action = regex::Regex::new(r#"\{"action":"([a-z_]+)"\}"#).unwrap();
        let lsp_actions = [
            "outline",
            "definition",
            "references",
            "diagnostics",
            "hover",
        ];
        let harness_tools = [
            "read", "write", "edit", "patch", "undo", "bash", "grep", "glob", "todo", "skill",
            "verify", "recall", "thread", "bg", "repo_map", "lsp", "test", "review",
        ];
        for name in CODING_PACK {
            let skill = index.get(name).unwrap();
            for banned in security_only {
                assert!(
                    !skill.body().contains(&format!("`{banned}`")),
                    "{name} names security-only tool {banned}"
                );
            }
            assert!(!skill.body().contains("save_finding"));
            assert!(!skill.body().contains("verify_finding"));
            // Every backticked identifier that is a harness tool must exist.
            // Other identifiers are commands, paths, or file names.
            for capture in backticked.captures_iter(&skill.body()) {
                let token = &capture[1];
                if harness_tools.contains(&token) {
                    assert!(
                        registry.get(token).is_some(),
                        "{name} names missing tool {token}"
                    );
                }
            }
            // `lsp` is read-only and has no rename action; validate every
            // action example so the skill cannot drift from the schema.
            if skill.body().contains("`lsp`") {
                for capture in action.captures_iter(&skill.body()) {
                    assert!(
                        lsp_actions.contains(&&capture[1]),
                        "{name} names unsupported lsp action {}",
                        &capture[1]
                    );
                }
                assert!(
                    !skill.body().contains("\"rename\"") && !skill.body().contains("lsp` rename"),
                    "{name} still promises an lsp rename"
                );
            }
        }
    }

    #[test]
    fn coding_pack_index_follows_active_packs() {
        // The index is filtered by active packs; an explicit load still works.
        // The tool-level three-task smoke test lives in tools::skill.
        let index = bundled();
        let coding = index.advertised_for(&["core".into(), "coding".into()]);
        assert!(coding.get("test-repair").is_some());
        assert!(coding.get("code-review").is_some());
        assert!(coding.get("security-red").is_none());
        assert!(coding.get("security-blue").is_none());

        let security = index.advertised_for(&["core".into(), "security".into()]);
        assert!(security.get("security-blue").is_some());
        assert!(security.get("test-repair").is_none());
        assert!(security.get("release-notes").is_none());

        // The model can still load an unadvertised skill by name.
        assert!(index.get("incident-triage").is_some());
        assert!(index.get("security-red").is_some());
    }

    #[test]
    fn discovery_reads_standard_and_native_roots_with_native_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        write_skill(
            &paths.home_dir.clone().unwrap().join(".claude/skills"),
            "deploy",
            "Claude copy",
            "claude body",
        );
        write_skill(
            &paths.home_dir.clone().unwrap().join(".agents/skills"),
            "agents-global",
            "Agents global",
            "agents body",
        );
        write_skill(&paths.skills_dir(), "deploy", "Native copy", "native body");
        write_skill(
            &paths.cwd.join(".claude/skills"),
            "project-only",
            "Project standard",
            "project body",
        );
        write_skill(
            &paths.cwd.join(".agents/skills"),
            "agents-project",
            "Agents project",
            "agents project body",
        );

        let untrusted = discover(&paths, false).index();
        assert_eq!(untrusted.get("deploy").unwrap().description, "Native copy");
        assert!(untrusted.get("project-only").is_none());
        assert!(untrusted.get("agents-project").is_none());
        assert!(untrusted.get("agents-global").is_some());
        assert_eq!(untrusted.shadows.len(), 1);
        assert_eq!(untrusted.shadows[0].loser, SkillOrigin::GlobalClaude);
        assert_eq!(untrusted.shadows[0].winner, SkillOrigin::GlobalNative);

        let trusted = discover(&paths, true).index();
        assert!(trusted.get("project-only").is_some());
        assert!(trusted.get("agents-project").is_some());
        assert_eq!(trusted.get("deploy").unwrap().description, "Native copy");
    }

    #[test]
    fn a_loose_native_markdown_file_falls_back_to_its_file_stem() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        std::fs::create_dir_all(paths.skills_dir()).unwrap();
        std::fs::write(
            paths.skills_dir().join("legacy.md"),
            "---\ndescription: Old style\n---\n\nBody\n",
        )
        .unwrap();
        let index = discover(&paths, false).index();
        assert!(index.get("legacy").is_some());
    }

    #[test]
    fn discovery_is_lazy_about_bodies_and_references() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        for index in 0..100 {
            let root = paths.skills_dir().join(format!("skill-{index}"));
            std::fs::create_dir_all(root.join("references")).unwrap();
            std::fs::write(
                root.join("SKILL.md"),
                format!(
                    "---\nname: skill-{index}\ndescription: Skill number {index}\n---\n\nBODY-SENTINEL-{index}\n"
                ),
            )
            .unwrap();
            std::fs::write(root.join("references/notes.md"), "REFERENCE-SENTINEL").unwrap();
        }
        let index = SkillIndex::load(&paths, false);
        let text = index.index_text();
        assert_eq!(
            index.skills.iter().filter(|skill| !skill.bundled).count(),
            100
        );
        assert!(text.len() < 20_000);
        assert!(!text.contains("BODY-SENTINEL"));
        assert!(!text.contains("REFERENCE-SENTINEL"));
        let skill = index.get("skill-7").unwrap();
        assert_eq!(skill.body().trim(), "BODY-SENTINEL-7");
        assert_eq!(skill.references().len(), 1);
        assert_eq!(
            skill.reference("notes").unwrap().text().as_deref(),
            Some("REFERENCE-SENTINEL")
        );
    }

    #[test]
    fn allowed_tools_intersects_and_never_expands() {
        let dir = tempfile::tempdir().unwrap();
        let skill = dir.path().join("bash-helper");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: bash-helper\ndescription: Helper\nallowed-tools: bash read\n---\n\nBody\n",
        )
        .unwrap();
        let skill = read_skill_dir(&skill, SkillOrigin::GlobalNative).unwrap();
        assert_eq!(skill.allowed_tools, ["bash", "read"]);
        assert_eq!(
            skill.effective_allowed_tools(&["read".into(), "edit".into()]),
            ["read"]
        );
        assert!(
            skill
                .effective_allowed_tools(&["read".into(), "edit".into()])
                .iter()
                .all(|tool| tool != "bash")
        );
    }

    #[test]
    fn invalid_metadata_is_not_discovered() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let root = paths.skills_dir().join("bad");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("SKILL.md"),
            format!(
                "---\nname: Bad Name\ndescription: {}\n---\n\nBody\n",
                "x".repeat(validate::DESCRIPTION_MAX + 1)
            ),
        )
        .unwrap();
        let discovery = discover(&paths, false);
        assert!(discovery.index().get("Bad Name").is_none());
        let invalid = discovery.invalid.first().expect("invalid skill");
        assert!(
            invalid
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.field == "name")
        );
        assert!(
            invalid
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.field == "description")
        );
    }
}

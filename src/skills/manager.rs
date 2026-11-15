//! Skill package manager: install from a directory or git URL, update, remove,
//! list, and pin with `skills.lock`.
//!
//! Nothing here executes a bundled script. Git work goes through the `git`
//! CLI; no new crate is involved. Replaced copies are moved to a timestamped
//! trash directory and never deleted.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::paths::Paths;

use super::validate::{self, Diagnostic, Severity};
use super::{Skill, SkillOrigin, read_skill_dir};

pub const LOCK_VERSION: u32 = 1;
/// One installed file may not exceed this size.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// One installed skill may not exceed this size.
pub const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockedSkill {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub source_kind: String,
    #[serde(default, rename = "ref")]
    pub requested_ref: Option<String>,
    #[serde(default)]
    pub resolved_commit: Option<String>,
    #[serde(default)]
    pub installed_at: String,
    /// Relative path -> lowercase hex SHA-256 of the installed bytes.
    #[serde(default)]
    pub files: BTreeMap<String, String>,
}

impl LockedSkill {
    pub fn digest(&self) -> String {
        combined_digest(&self.files)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LockFile {
    #[serde(default = "default_lock_version")]
    pub version: u32,
    #[serde(default, rename = "skill")]
    pub skills: Vec<LockedSkill>,
}

fn default_lock_version() -> u32 {
    LOCK_VERSION
}

impl Default for LockFile {
    fn default() -> Self {
        Self {
            version: LOCK_VERSION,
            skills: Vec::new(),
        }
    }
}

impl LockFile {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let lock: Self = toml::from_str(&raw)
            .with_context(|| format!("{} is not a valid skills lockfile", path.display()))?;
        if lock.version > LOCK_VERSION {
            bail!(
                "{} was written by a newer LocalHarness (lockfile version {}); upgrade `lh`",
                path.display(),
                lock.version
            );
        }
        Ok(lock)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("cannot serialize skills.lock")?;
        let tmp = path.with_extension("lock.tmp");
        std::fs::write(&tmp, raw).with_context(|| format!("cannot write {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, path)
            .with_context(|| format!("cannot replace {}", path.display()))?;
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&LockedSkill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    pub fn upsert(&mut self, skill: LockedSkill) {
        if let Some(slot) = self
            .skills
            .iter_mut()
            .find(|entry| entry.name == skill.name)
        {
            *slot = skill;
        } else {
            self.skills.push(skill);
        }
    }

    pub fn remove(&mut self, name: &str) -> Option<LockedSkill> {
        let index = self.skills.iter().position(|skill| skill.name == name)?;
        Some(self.skills.remove(index))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Installed,
    Updated,
    UpToDate,
    FrozenVerified,
}

#[derive(Debug, Clone)]
pub struct InstallReport {
    pub action: Action,
    pub name: String,
    pub path: PathBuf,
    pub source: String,
    pub source_kind: String,
    pub requested_ref: Option<String>,
    pub resolved_commit: Option<String>,
    pub installed_at: String,
    pub files: BTreeMap<String, String>,
    pub digest: String,
    pub trash: Option<PathBuf>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Drift {
    pub name: String,
    pub file: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub reports: Vec<InstallReport>,
    pub frozen: bool,
}

#[derive(Debug, Clone)]
pub struct RemoveReport {
    pub name: String,
    pub trash: Option<PathBuf>,
    pub lock_removed: bool,
}

#[derive(Debug, Clone)]
pub struct ListedSkill {
    pub name: String,
    pub source: Option<String>,
    pub source_kind: Option<String>,
    pub requested_ref: Option<String>,
    pub resolved_commit: Option<String>,
    pub digest: Option<String>,
    pub installed_at: Option<String>,
    pub path: PathBuf,
    pub files: BTreeMap<String, String>,
    pub modified: bool,
    pub installed: bool,
}

/// Install one skill. Existing copies are refused unless `force`; a forced
/// replacement is moved to the trash directory first.
pub fn add(paths: &Paths, raw_source: &str, force: bool) -> Result<InstallReport> {
    let spec = parse_source(paths, raw_source)?;
    let resolved = resolve_source(paths, &spec)?;
    let (skill, skill_root) = inspect(&resolved.root)?;
    print_approval(&skill, &skill_root, paths)?;
    if let Some(target) = install_target(paths, &skill.name)
        && target.exists()
        && !force
    {
        bail!(
            "skill `{}` is already installed at {}; run `lh skill update {}` or `lh skill add {} --force` (the old copy is moved to {})",
            skill.name,
            target.display(),
            skill.name,
            raw_source,
            paths.skills_trash_dir().display()
        );
    }
    let mut lock = LockFile::load(&paths.skills_lock_file())?;
    materialize(
        paths,
        Materialize {
            spec: &spec,
            resolved: &resolved,
            skill: &skill,
            skill_root: &skill_root,
            force,
            updating: false,
            action: Action::Installed,
        },
        &mut lock,
    )
}

/// Update one or every locked skill. Local edits require `force`; either way
/// the replaced copy is moved to the trash directory.
pub fn update(
    paths: &Paths,
    name: Option<&str>,
    force: bool,
    frozen: bool,
) -> Result<UpdateOutcome> {
    let mut lock = LockFile::load(&paths.skills_lock_file())?;
    if frozen {
        if let Some(name) = name
            && lock.get(name).is_none()
        {
            bail!(
                "no lockfile entry for `{name}`; run `lh skill list` or install it with `lh skill add`"
            );
        }
        let drift = verify_lock(paths, &lock, name);
        if !drift.is_empty() {
            bail!("{}", format_drift(&drift));
        }
        return Ok(UpdateOutcome {
            reports: Vec::new(),
            frozen: true,
        });
    }
    let targets: Vec<LockedSkill> = match name {
        Some(name) => {
            let Some(entry) = lock.get(name).cloned() else {
                let installed = paths.skills_dir().join(name).is_dir();
                if installed {
                    bail!(
                        "skill `{name}` was installed by hand and has no lockfile source; run `lh skill add <source>` to record one, or remove it first"
                    );
                }
                bail!("skill `{name}` is not installed; `lh skill list` shows installed skills");
            };
            vec![entry]
        }
        None => {
            if lock.skills.is_empty() {
                bail!(
                    "no skills are locked in {}; install one with `lh skill add <dir|git-url#ref>`",
                    paths.skills_lock_file().display()
                );
            }
            lock.skills.clone()
        }
    };

    let mut reports = Vec::new();
    for entry in targets {
        let drift = verify_lock(paths, &lock, Some(&entry.name));
        let local_edits: Vec<&Drift> = drift
            .iter()
            .filter(|drift| drift.reason != "installed directory is missing")
            .collect();
        if !local_edits.is_empty() && !force {
            bail!(
                "{}; pass --force to replace it (the copy is moved to {}) or fix the edits first",
                format_drift(&drift),
                paths.skills_trash_dir().display()
            );
        }
        let spec = parse_source(paths, &entry.source)?;
        let resolved = resolve_source(paths, &spec)?;
        let (skill, skill_root) = inspect(&resolved.root)?;
        if skill.name != entry.name {
            bail!(
                "source for `{}` now declares skill name `{}`; rename the source back or remove the installed skill before installing the new name",
                entry.name,
                skill.name
            );
        }
        let unchanged = drift.is_empty()
            && match spec.kind {
                SourceKind::Git => {
                    resolved.commit.is_some() && resolved.commit == entry.resolved_commit
                }
                SourceKind::Directory => source_digests(&skill_root)? == entry.files,
            };
        if unchanged {
            reports.push(InstallReport {
                action: Action::UpToDate,
                name: entry.name.clone(),
                path: paths.skills_dir().join(&entry.name),
                source: entry.source.clone(),
                source_kind: entry.source_kind.clone(),
                requested_ref: entry.requested_ref.clone(),
                resolved_commit: entry.resolved_commit.clone(),
                installed_at: entry.installed_at.clone(),
                files: entry.files.clone(),
                digest: entry.digest(),
                trash: None,
                warnings: Vec::new(),
            });
            continue;
        }
        let mut report = materialize(
            paths,
            Materialize {
                spec: &spec,
                resolved: &resolved,
                skill: &skill,
                skill_root: &skill_root,
                force: true,
                updating: true,
                action: Action::Updated,
            },
            &mut lock,
        )?;
        report.warnings.extend(
            drift
                .iter()
                .map(|drift| format!("replaced local copy: {} ({})", drift.file, drift.reason)),
        );
        reports.push(report);
    }
    Ok(UpdateOutcome {
        reports,
        frozen: false,
    })
}

/// Remove a skill by moving its directory to the timestamped trash and
/// dropping its lock entry. Nothing is deleted.
pub fn remove(paths: &Paths, name: &str) -> Result<RemoveReport> {
    if !validate::is_valid_name(name) {
        bail!("invalid skill name {name:?}; names are lowercase letters, digits, and hyphens");
    }
    let mut lock = LockFile::load(&paths.skills_lock_file())?;
    let lock_entry = lock.get(name).cloned();
    let target = paths.skills_dir().join(name);
    let mut trash = None;
    if target.is_dir() {
        trash = Some(move_to_trash(paths, &target, name)?);
    }
    let lock_removed = if lock_entry.is_some() {
        lock.remove(name);
        lock.save(&paths.skills_lock_file())?;
        true
    } else {
        false
    };
    if trash.is_none() && !lock_removed {
        bail!(
            "skill `{name}` is not installed in {}; `lh skill list` shows installed skills",
            paths.skills_dir().display()
        );
    }
    Ok(RemoveReport {
        name: name.to_string(),
        trash,
        lock_removed,
    })
}

/// Installed skills, locked and hand-installed, with a digest for each.
pub fn list(paths: &Paths) -> Result<Vec<ListedSkill>> {
    let lock = LockFile::load(&paths.skills_lock_file())?;
    let mut rows = Vec::new();
    for entry in &lock.skills {
        let path = paths.skills_dir().join(&entry.name);
        let installed = path.is_dir();
        let modified = installed && !verify_entry(paths, entry).is_empty();
        rows.push(ListedSkill {
            name: entry.name.clone(),
            source: Some(entry.source.clone()),
            source_kind: Some(entry.source_kind.clone()),
            requested_ref: entry.requested_ref.clone(),
            resolved_commit: entry.resolved_commit.clone(),
            digest: Some(entry.digest()),
            installed_at: Some(entry.installed_at.clone()),
            path,
            files: entry.files.clone(),
            modified,
            installed,
        });
    }
    if let Ok(entries) = std::fs::read_dir(paths.skills_dir()) {
        let mut paths_found: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        paths_found.sort();
        for path in paths_found {
            if !path.is_dir() || !path.join("SKILL.md").is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if lock.get(name).is_some() {
                continue;
            }
            let files = tree_digests(&path)?;
            rows.push(ListedSkill {
                name: name.to_string(),
                source: None,
                source_kind: Some("manual".into()),
                requested_ref: None,
                resolved_commit: None,
                digest: Some(combined_digest(&files)),
                installed_at: None,
                path,
                files,
                modified: false,
                installed: true,
            });
        }
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(rows)
}

/// Compare every locked file's digest with the installed bytes.
pub fn verify_lock(paths: &Paths, lock: &LockFile, name: Option<&str>) -> Vec<Drift> {
    lock.skills
        .iter()
        .filter(|entry| name.is_none_or(|name| entry.name == name))
        .flat_map(|entry| verify_entry(paths, entry))
        .collect()
}

fn verify_entry(paths: &Paths, entry: &LockedSkill) -> Vec<Drift> {
    let path = paths.skills_dir().join(&entry.name);
    let mut drift = Vec::new();
    if !path.is_dir() {
        drift.push(Drift {
            name: entry.name.clone(),
            file: entry.name.clone(),
            reason: "installed directory is missing".into(),
        });
        return drift;
    }
    let installed = match tree_digests(&path) {
        Ok(files) => files,
        Err(error) => {
            drift.push(Drift {
                name: entry.name.clone(),
                file: entry.name.clone(),
                reason: error.to_string(),
            });
            return drift;
        }
    };
    for (file, expected) in &entry.files {
        match installed.get(file) {
            None => drift.push(Drift {
                name: entry.name.clone(),
                file: file.clone(),
                reason: "file is missing".into(),
            }),
            Some(actual) if actual != expected => drift.push(Drift {
                name: entry.name.clone(),
                file: file.clone(),
                reason: format!(
                    "digest changed (locked {}, installed {})",
                    short(expected),
                    short(actual)
                ),
            }),
            Some(_) => {}
        }
    }
    for file in installed.keys() {
        if !entry.files.contains_key(file) {
            drift.push(Drift {
                name: entry.name.clone(),
                file: file.clone(),
                reason: "file is not in the lockfile".into(),
            });
        }
    }
    drift
}

pub fn format_drift(drift: &[Drift]) -> String {
    let mut out = String::from("skills.lock drift detected:");
    let mut names: Vec<&str> = drift.iter().map(|drift| drift.name.as_str()).collect();
    names.sort();
    names.dedup();
    for name in names {
        out.push_str(&format!("\n  skill `{name}`:"));
        for entry in drift.iter().filter(|drift| drift.name == name) {
            out.push_str(&format!("\n    - {}: {}", entry.file, entry.reason));
        }
    }
    out.push_str(
        "\n`--frozen` never rewrites skills.lock. Restore the copy with `lh skill update` (without --frozen) or fix the install.",
    );
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceKind {
    Directory,
    Git,
}

#[derive(Debug, Clone)]
struct SourceSpec {
    original: String,
    location: String,
    requested_ref: Option<String>,
    kind: SourceKind,
}

struct ResolvedSource {
    root: PathBuf,
    commit: Option<String>,
    _scratch: Option<Scratch>,
}

fn parse_source(paths: &Paths, raw: &str) -> Result<SourceSpec> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("source is empty; pass a directory or `git-url#ref`");
    }
    let direct = expand_path(paths, raw);
    if direct.exists() {
        return Ok(SourceSpec {
            original: raw.to_string(),
            location: direct.to_string_lossy().into_owned(),
            requested_ref: None,
            kind: SourceKind::Directory,
        });
    }
    let (location, requested_ref) = match raw.rsplit_once('#') {
        Some((location, reference)) if !reference.trim().is_empty() => {
            let reference = reference.trim();
            if reference.starts_with('-') {
                bail!(
                    "ref {reference:?} starts with `-`; pass a branch, tag, or commit name, not a git option"
                );
            }
            (location.to_string(), Some(reference.to_string()))
        }
        Some((_, _)) => bail!("empty ref after `#` in {raw:?}; use `url#branch`"),
        None => (raw.to_string(), None),
    };
    let local = expand_path(paths, &location);
    if local.exists() {
        if !is_git_repo(&local) {
            bail!(
                "`{location}` is a local directory, not a git repository; a local directory has no ref. Install the checkout you want without `#ref`."
            );
        }
        return Ok(SourceSpec {
            original: raw.to_string(),
            location: local.to_string_lossy().into_owned(),
            requested_ref,
            kind: SourceKind::Git,
        });
    }
    if looks_like_git(&location) {
        return Ok(SourceSpec {
            original: raw.to_string(),
            location,
            requested_ref,
            kind: SourceKind::Git,
        });
    }
    bail!(
        "source {raw:?} does not exist; pass a path to a skill directory or a git URL with an optional #ref"
    )
}

fn is_git_repo(path: &Path) -> bool {
    Command::new("git")
        .args(["-C", &path.to_string_lossy(), "rev-parse", "--git-dir"])
        .output()
        .is_ok_and(|output| output.status.success())
}

fn looks_like_git(location: &str) -> bool {
    location.starts_with("http://")
        || location.starts_with("https://")
        || location.starts_with("ssh://")
        || location.starts_with("git://")
        || location.starts_with("file://")
        || location.starts_with("git@")
        || location.ends_with(".git")
        || (location.contains(':') && !location.starts_with('/'))
}

fn expand_path(paths: &Paths, raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = &paths.home_dir
    {
        return home.join(rest);
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        paths.cwd.join(path)
    }
}

fn resolve_source(paths: &Paths, spec: &SourceSpec) -> Result<ResolvedSource> {
    match spec.kind {
        SourceKind::Directory => {
            let root = PathBuf::from(&spec.location)
                .canonicalize()
                .with_context(|| format!("cannot resolve {}", spec.location))?;
            Ok(ResolvedSource {
                root,
                commit: None,
                _scratch: None,
            })
        }
        SourceKind::Git => {
            let scratch = Scratch::new(paths, "git")?;
            let root = scratch.path().join("source");
            git(
                &[
                    "clone",
                    "--quiet",
                    "--",
                    &spec.location,
                    &root.to_string_lossy(),
                ],
                None,
            )?;
            if let Some(reference) = &spec.requested_ref {
                git(
                    &[
                        "-C",
                        &root.to_string_lossy(),
                        "checkout",
                        "--quiet",
                        reference,
                    ],
                    None,
                )
                .with_context(|| {
                    format!("cannot check out ref {reference:?} from {}", spec.location)
                })?;
            }
            let commit = git(&["-C", &root.to_string_lossy(), "rev-parse", "HEAD"], None)?
                .trim()
                .to_string();
            Ok(ResolvedSource {
                root,
                commit: Some(commit),
                _scratch: Some(scratch),
            })
        }
    }
}

fn git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut command = Command::new("git");
    command.args(args);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command
        .output()
        .context("cannot run git; install git and put it on PATH")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("git {} failed: {}", args.join(" "), stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The skill root inside a source checkout. A repo may name the skill at its
/// root or contain exactly one skill directory.
fn find_skill_root(root: &Path) -> Result<PathBuf> {
    if root.join("SKILL.md").is_file() || root.join("skill.md").is_file() {
        return Ok(root.to_path_buf());
    }
    let mut candidates = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir()
                && (path.join("SKILL.md").is_file() || path.join("skill.md").is_file())
                && path.file_name().is_some_and(|name| name != ".git")
            {
                candidates.push(path);
            }
        }
    }
    candidates.sort();
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        0 => bail!(
            "{} has no SKILL.md; a skill source is a directory with SKILL.md at its root (or exactly one skill subdirectory)",
            root.display()
        ),
        _ => bail!(
            "{} contains {} skills ({}); install one by pointing at its directory or add a top-level SKILL.md",
            root.display(),
            candidates.len(),
            candidates
                .iter()
                .filter_map(|path| path.file_name().and_then(|name| name.to_str()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn inspect(root: &Path) -> Result<(Skill, PathBuf)> {
    let skill_root = find_skill_root(root)?;
    let skill = read_skill_dir(&skill_root, SkillOrigin::GlobalNative).map_err(|invalid| {
        anyhow::anyhow!(
            "{} is not a valid skill:\n{}",
            skill_root.display(),
            render_diagnostics(&invalid.diagnostics)
        )
    })?;
    let diagnostics = validate::validate_skill(&skill, validate::Scope::Full);
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Error)
    {
        bail!(
            "{} is not installable:\n{}",
            skill_root.display(),
            render_diagnostics(&diagnostics)
        );
    }
    // Validate the size before touching the install directory.
    let files = source_files(&skill_root)?;
    let mut total = 0u64;
    for (relative, path, _) in &files {
        let size = std::fs::metadata(path)
            .with_context(|| format!("cannot stat {}", path.display()))?
            .len();
        if size > MAX_FILE_BYTES {
            bail!(
                "{} ({relative}) is {size} bytes; the per-file install limit is {MAX_FILE_BYTES} bytes",
                path.display()
            );
        }
        total += size;
    }
    if total > MAX_TOTAL_BYTES {
        bail!(
            "skill `{}` is {total} bytes; the install limit is {MAX_TOTAL_BYTES} bytes. Move large data out of the skill.",
            skill.name
        );
    }
    Ok((skill, skill_root))
}

fn render_diagnostics(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| {
            format!(
                "  {} [{}] {}: {}",
                diagnostic.severity, diagnostic.field, diagnostic.code, diagnostic.message
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn print_approval(skill: &Skill, root: &Path, paths: &Paths) -> Result<()> {
    let files = source_files(root)?;
    eprintln!(
        "skill `{}`: {}",
        skill.name,
        if skill.description.is_empty() {
            "(no description)"
        } else {
            &skill.description
        }
    );
    eprintln!(
        "files: {}",
        files
            .iter()
            .map(|(relative, _, _)| relative.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    eprintln!(
        "installing into {}; bundled scripts are copied and never executed.",
        paths.skills_dir().join(&skill.name).display()
    );
    Ok(())
}

struct Materialize<'a> {
    spec: &'a SourceSpec,
    resolved: &'a ResolvedSource,
    skill: &'a Skill,
    skill_root: &'a Path,
    force: bool,
    updating: bool,
    action: Action,
}

fn materialize(
    paths: &Paths,
    request: Materialize<'_>,
    lock: &mut LockFile,
) -> Result<InstallReport> {
    let Materialize {
        spec,
        resolved,
        skill,
        skill_root,
        force,
        updating,
        action,
    } = request;
    std::fs::create_dir_all(&paths.config_dir)
        .with_context(|| format!("cannot create {}", paths.config_dir.display()))?;
    std::fs::create_dir_all(paths.skills_dir())
        .with_context(|| format!("cannot create {}", paths.skills_dir().display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in [&paths.config_dir, &paths.skills_dir()] {
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let target = install_target(paths, &skill.name)
        .ok_or_else(|| anyhow::anyhow!("invalid skill name {:?}", skill.name))?;
    if target.exists() && !updating && !force {
        bail!(
            "skill `{}` is already installed; run `lh skill update {}` or add --force",
            skill.name,
            skill.name
        );
    }

    let staging = Scratch::new(paths, "stage")?;
    let warnings = copy_tree(skill_root, staging.path())?;
    let files = tree_digests(staging.path())?;
    if !files.contains_key("SKILL.md") && !files.contains_key("skill.md") {
        bail!("staged copy of `{}` has no SKILL.md", skill.name);
    }

    let mut trash = None;
    if target.exists() {
        trash = Some(move_to_trash(paths, &target, &skill.name)?);
    }
    std::fs::rename(staging.path(), &target).with_context(|| {
        format!(
            "cannot move the staged skill into {}; the previous copy (if any) is in {}",
            target.display(),
            paths.skills_trash_dir().display()
        )
    })?;

    let installed_at = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
    let entry = LockedSkill {
        name: skill.name.clone(),
        source: spec.original.clone(),
        source_kind: match spec.kind {
            SourceKind::Directory => "directory".into(),
            SourceKind::Git => "git".into(),
        },
        requested_ref: spec.requested_ref.clone(),
        resolved_commit: resolved.commit.clone(),
        installed_at: installed_at.clone(),
        files: files.clone(),
    };
    let report = InstallReport {
        action,
        name: skill.name.clone(),
        path: target,
        source: spec.original.clone(),
        source_kind: entry.source_kind.clone(),
        requested_ref: entry.requested_ref.clone(),
        resolved_commit: entry.resolved_commit.clone(),
        installed_at,
        digest: entry.digest(),
        files,
        trash,
        warnings,
    };
    lock.upsert(entry);
    lock.save(&paths.skills_lock_file())?;
    Ok(report)
}

fn install_target(paths: &Paths, name: &str) -> Option<PathBuf> {
    validate::is_valid_name(name).then(|| paths.skills_dir().join(name))
}

fn move_to_trash(paths: &Paths, target: &Path, name: &str) -> Result<PathBuf> {
    let root = paths.skills_trash_dir();
    std::fs::create_dir_all(&root).with_context(|| format!("cannot create {}", root.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    }
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    for attempt in 0..1000u32 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{attempt}")
        };
        let candidate = root.join(format!("{stamp}-{name}{suffix}"));
        if candidate.exists() {
            continue;
        }
        match std::fs::rename(target, &candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "cannot move {} to {}; nothing was deleted",
                        target.display(),
                        candidate.display()
                    )
                });
            }
        }
    }
    bail!(
        "cannot find a free trash name under {}; no files were deleted",
        root.display()
    )
}

/// Regular files under a skill directory, relative path -> absolute path.
/// Symlinks and `.git` are skipped; nothing outside the tree is followed.
fn source_files(root: &Path) -> Result<Vec<(String, PathBuf, u64)>> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((current, depth)) = stack.pop() {
        if depth > 12 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !kind.is_file() {
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("cannot relativize {}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            let size = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            files.push((relative, path, size));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

fn copy_tree(source: &Path, destination: &Path) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    for (relative, path, size) in source_files(source)? {
        if size > MAX_FILE_BYTES {
            bail!(
                "{relative} is {size} bytes; the per-file install limit is {MAX_FILE_BYTES} bytes"
            );
        }
        let target = destination.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        std::fs::copy(&path, &target)
            .with_context(|| format!("cannot copy {} to {}", path.display(), target.display()))?;
        if let Ok(metadata) = std::fs::metadata(&path)
            && let Err(error) = std::fs::set_permissions(&target, metadata.permissions())
        {
            warnings.push(format!(
                "cannot preserve permissions on {relative}: {error}"
            ));
        }
    }
    // Reject oversized totals after the copy, before the directory becomes the
    // installed skill.
    let total: u64 = tree_digests(destination)?
        .keys()
        .filter_map(|relative| std::fs::metadata(destination.join(relative)).ok())
        .map(|metadata| metadata.len())
        .sum();
    if total > MAX_TOTAL_BYTES {
        bail!("skill is {total} bytes; the install limit is {MAX_TOTAL_BYTES} bytes");
    }
    Ok(warnings)
}

fn tree_digests(root: &Path) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    for (relative, path, _) in source_files(root)? {
        files.insert(relative, file_digest(&path)?);
    }
    Ok(files)
}

fn source_digests(root: &Path) -> Result<BTreeMap<String, String>> {
    tree_digests(root)
}

pub fn file_digest(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(hex(&Sha256::digest(&bytes)))
}

pub fn combined_digest(files: &BTreeMap<String, String>) -> String {
    let mut hasher = Sha256::new();
    for (path, digest) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(digest.as_bytes());
        hasher.update(b"\n");
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn short(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

/// A temporary directory under the config directory. It is removed on drop;
/// an installed or trashed skill is never inside one.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(paths: &Paths, label: &str) -> Result<Self> {
        let root = paths.config_dir.join("skills-staging");
        std::fs::create_dir_all(&root)
            .with_context(|| format!("cannot create {}", root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
        }
        let unique = format!(
            "{label}-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let path = root.join(unique);
        std::fs::create_dir(&path).with_context(|| format!("cannot create {}", path.display()))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

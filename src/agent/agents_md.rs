//! Hierarchical `AGENTS.md` discovery for the system prompt.
//!
//! The chain runs from the git repository root to the working directory,
//! nearest last. Each file is shown under a source-path header so the model
//! can tell a root convention from a package override. The text is untrusted
//! data: it cannot add a tool, relax a gate, or change the profile.
//!
//! Two deterministic caps keep one repository from crowding out the prompt:
//! [`PER_FILE_CAP_BYTES`] for one file and [`TOTAL_CAP_BYTES`] for the chain.
//! When the total cap is tight, the nearest files are kept whole and broader
//! files are truncated or skipped, matching the nearest-wins convention.
//! Symlinks must resolve inside the repository. Every decision is recorded in
//! the manifest returned to callers and printed by `tot context --show`.

use std::fs;
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

use crate::security::evidence::sha256_hex;

/// Bytes of `AGENTS.md` content loaded into one prompt.
pub const TOTAL_CAP_BYTES: usize = 32 * 1024;
/// Bytes loaded from one `AGENTS.md` file.
pub const PER_FILE_CAP_BYTES: usize = 12 * 1024;

/// State of one candidate file after discovery and truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// The whole file is in the prompt.
    Loaded,
    /// The file is present but only a prefix is in the prompt.
    Truncated,
    /// The file was not loaded at all.
    Skipped,
}

impl FileState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Truncated => "truncated",
            Self::Skipped => "skipped",
        }
    }
}

/// One discovered `AGENTS.md` file and what happened to it.
#[derive(Debug, Clone)]
pub struct AgentsFile {
    /// Absolute path on disk, including a symlink's own path.
    pub path: PathBuf,
    /// Path relative to the repository root, with `/` separators.
    pub source: String,
    pub state: FileState,
    /// Bytes of this file included in the prompt.
    pub bytes: usize,
    /// Bytes in the file before truncation.
    pub source_bytes: usize,
    /// SHA-256 of the included bytes; empty when the file was skipped.
    pub sha256: String,
    /// Why the file was truncated or skipped.
    pub reason: Option<String>,
    /// Included text, without the source header. Empty when skipped.
    content: String,
}

/// The loaded chain plus the manifest that describes it.
#[derive(Debug, Clone)]
pub struct AgentsMd {
    pub enabled: bool,
    pub repo_root: PathBuf,
    pub files: Vec<AgentsFile>,
    pub warnings: Vec<String>,
}

impl AgentsMd {
    /// Load the chain for `cwd`. `enabled = false` returns an empty manifest
    /// without touching the filesystem.
    pub fn load(cwd: &Path, enabled: bool) -> Self {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        let root = repository_root(&cwd);
        Self::load_between(&cwd, &root, enabled)
    }

    /// Load every candidate between an explicit root and working directory.
    /// Kept separate so tests can exercise the chain without building a git
    /// repository.
    fn load_between(cwd: &Path, root: &Path, enabled: bool) -> Self {
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        if !enabled {
            return Self {
                enabled: false,
                repo_root: root,
                files: Vec::new(),
                warnings: Vec::new(),
            };
        }

        let mut files = Vec::new();
        let mut warnings = Vec::new();
        for dir in chain(&root, &cwd) {
            let candidate = dir.join("AGENTS.md");
            match read_candidate(&candidate, &root) {
                Ok(Some(file)) => {
                    if file.state == FileState::Truncated {
                        let reason = file.reason.as_deref().unwrap_or("truncated");
                        warnings.push(format!(
                            "{}: {reason} ({} bytes)",
                            file.source, file.source_bytes
                        ));
                    }
                    files.push(file);
                }
                Ok(None) => {}
                Err(file) => {
                    let source = file.source.clone();
                    let reason = file.reason.clone().unwrap_or_default();
                    warnings.push(format!("{source}: skipped ({reason})"));
                    files.push(*file);
                }
            }
        }

        apply_total_cap(&mut files, &mut warnings);
        Self {
            enabled: true,
            repo_root: root,
            files,
            warnings,
        }
    }

    /// True when at least one file contributed text.
    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|file| file.content.is_empty())
    }

    /// Bytes included in the prompt across every file.
    pub fn loaded_bytes(&self) -> usize {
        self.files.iter().map(|file| file.bytes).sum()
    }

    /// The untrusted project-instruction body. Each loaded or partially loaded
    /// file gets a source-path header; skipped files are omitted.
    pub fn prompt_body(&self) -> Option<String> {
        let sections: Vec<String> = self
            .files
            .iter()
            .filter(|file| !file.content.is_empty())
            .map(|file| {
                let mut section = format!("### AGENTS.md source: {}\n\n", file.source);
                section.push_str(file.content.trim_end_matches(['\r', '\n']));
                if file.state == FileState::Truncated {
                    section.push_str(&format!(
                        "\n\n[truncated: {} of {} bytes]",
                        file.bytes, file.source_bytes
                    ));
                }
                section
            })
            .collect();
        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n\n"))
        }
    }

    /// Stable, machine-readable manifest printed by `tot context --json`.
    pub fn manifest_json(&self) -> Value {
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|file| {
                json!({
                    "path": file.path.display().to_string(),
                    "source": file.source,
                    "state": file.state.label(),
                    "bytes": file.bytes,
                    "source_bytes": file.source_bytes,
                    "sha256": file.sha256,
                    "reason": file.reason,
                })
            })
            .collect();
        json!({
            "enabled": self.enabled,
            "repo_root": self.repo_root.display().to_string(),
            "total_cap_bytes": TOTAL_CAP_BYTES,
            "per_file_cap_bytes": PER_FILE_CAP_BYTES,
            "loaded_bytes": self.loaded_bytes(),
            "files": files,
            "warnings": self.warnings,
        })
    }

    /// Human-readable manifest printed by `tot context --show`.
    pub fn manifest_text(&self) -> String {
        let mut out = String::new();
        if !self.enabled {
            out.push_str("AGENTS.md context: disabled (--no-agents-md or TOT_AGENTS_MD=0)\n");
            return out;
        }
        let loaded = self
            .files
            .iter()
            .filter(|file| file.state != FileState::Skipped)
            .count();
        out.push_str(&format!(
            "AGENTS.md context: {TOTAL_CAP_BYTES} byte total cap, {PER_FILE_CAP_BYTES} bytes per file\n"
        ));
        out.push_str(&format!("repository root: {}\n", self.repo_root.display()));
        out.push_str(&format!(
            "loaded: {loaded} file{}, {} bytes\n",
            if loaded == 1 { "" } else { "s" },
            self.loaded_bytes()
        ));
        if self.files.is_empty() {
            out.push_str("  (no AGENTS.md files in the chain)\n");
        }
        for file in &self.files {
            let size = if file.state == FileState::Skipped {
                "-".to_string()
            } else if file.bytes == file.source_bytes {
                format!("{} B", file.bytes)
            } else {
                format!("{} of {} B", file.bytes, file.source_bytes)
            };
            let hash = if file.sha256.is_empty() {
                "-".to_string()
            } else {
                format!("sha256:{}", file.sha256)
            };
            out.push_str(&format!(
                "  {:<9} {}  {}  {}",
                file.state.label(),
                file.source,
                size,
                hash
            ));
            if let Some(reason) = &file.reason {
                out.push_str(&format!("  ({reason})"));
            }
            out.push('\n');
        }
        if !self.warnings.is_empty() {
            out.push_str("warnings:\n");
            for warning in &self.warnings {
                out.push_str(&format!("  - {warning}\n"));
            }
        }
        out
    }
}

/// Locate the repository root through git, falling back to the working
/// directory when `git` is unavailable or the cwd is not inside a work tree.
fn repository_root(cwd: &Path) -> PathBuf {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output();
    let Ok(output) = output else {
        return cwd.to_path_buf();
    };
    if !output.status.success() {
        return cwd.to_path_buf();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let root = text.trim();
    if root.is_empty() {
        return cwd.to_path_buf();
    }
    let root = PathBuf::from(root);
    let root = root.canonicalize().unwrap_or(root);
    if cwd.starts_with(&root) {
        root
    } else {
        cwd.to_path_buf()
    }
}

/// Directories from `root` to `cwd`, inclusive. When `cwd` is outside the
/// root, only `cwd` is returned.
fn chain(root: &Path, cwd: &Path) -> Vec<PathBuf> {
    if !cwd.starts_with(root) {
        return vec![cwd.to_path_buf()];
    }
    let mut dirs = Vec::new();
    let mut current = Some(cwd);
    while let Some(dir) = current {
        dirs.push(dir.to_path_buf());
        if dir == root {
            break;
        }
        current = dir.parent();
    }
    dirs.reverse();
    dirs
}

/// Read one candidate. `Ok(None)` means the file does not exist; a skipped
/// file is returned as `Err` with the state and reason filled in.
fn read_candidate(candidate: &Path, root: &Path) -> Result<Option<AgentsFile>, Box<AgentsFile>> {
    let metadata = match fs::symlink_metadata(candidate) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Box::new(skipped(
                candidate,
                root,
                format!("cannot inspect the path: {error}"),
            )));
        }
    };
    let is_symlink = metadata.file_type().is_symlink();
    let source = source_path(root, candidate);
    let mut source_bytes = usize::try_from(metadata.len()).unwrap_or(usize::MAX);

    let resolved = match fs::canonicalize(candidate) {
        Ok(path) => path,
        Err(error) => {
            return Err(Box::new(skipped_with_size(
                candidate,
                &source,
                source_bytes,
                format!(
                    "cannot {} the path: {error}",
                    if is_symlink {
                        "resolve symlink"
                    } else {
                        "resolve"
                    }
                ),
            )));
        }
    };
    // `symlink_metadata` reports the link itself, not its target. The loaded
    // bytes and the manifest size must describe the file the reader sees.
    if let Ok(target) = fs::metadata(&resolved) {
        source_bytes = usize::try_from(target.len()).unwrap_or(usize::MAX);
    }
    if !resolved.starts_with(root) {
        let reason = format!(
            "{} resolves outside the repository: {}",
            if is_symlink { "symlink" } else { "path" },
            resolved.display()
        );
        return Err(Box::new(skipped_with_size(
            candidate,
            &source,
            source_bytes,
            reason,
        )));
    }
    // Read the resolved path: the symlink check already approved it, and this
    // avoids following the link a second time. Only the capped prefix is read
    // so a pathologically large ancestor file cannot allocate its full size.
    let truncated = source_bytes > PER_FILE_CAP_BYTES;
    let read_limit = if truncated {
        PER_FILE_CAP_BYTES + 4
    } else {
        source_bytes
    };
    let mut bytes = Vec::with_capacity(read_limit.min(PER_FILE_CAP_BYTES + 4));
    let read = fs::File::open(&resolved).and_then(|file| {
        file.take(read_limit as u64)
            .read_to_end(&mut bytes)
            .map(|_| ())
    });
    if let Err(error) = read {
        return Err(Box::new(skipped_with_size(
            candidate,
            &source,
            source_bytes,
            format!("cannot read the file: {error}"),
        )));
    }
    // When the file is over the cap, `bytes` may end in a partial UTF-8
    // sequence. Decode only complete characters up to the cap; an invalid
    // sequence before it still marks the file as non-UTF-8.
    let end = if truncated {
        let limit = bytes.len().min(PER_FILE_CAP_BYTES);
        match std::str::from_utf8(&bytes[..limit]) {
            Ok(_) => limit,
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(_) => {
                return Err(Box::new(skipped_with_size(
                    candidate,
                    &source,
                    source_bytes,
                    "file is not valid UTF-8".to_string(),
                )));
            }
        }
    } else {
        bytes.len()
    };
    let text = match String::from_utf8(bytes[..end].to_vec()) {
        Ok(text) => text,
        Err(_) => {
            return Err(Box::new(skipped_with_size(
                candidate,
                &source,
                source_bytes,
                "file is not valid UTF-8".to_string(),
            )));
        }
    };
    let mut file = AgentsFile {
        path: candidate.to_path_buf(),
        source,
        state: FileState::Loaded,
        bytes: text.len(),
        source_bytes,
        sha256: sha256_hex(text.as_bytes()),
        reason: None,
        content: text,
    };
    if truncated {
        file.state = FileState::Truncated;
        file.reason = Some(format!(
            "truncated to the {PER_FILE_CAP_BYTES} byte per-file cap"
        ));
    }
    if file.content.len() > PER_FILE_CAP_BYTES {
        let keep = floor_char_boundary(&file.content, PER_FILE_CAP_BYTES);
        file.content.truncate(keep);
        file.bytes = keep;
        file.state = FileState::Truncated;
        file.reason = Some(format!(
            "truncated to the {PER_FILE_CAP_BYTES} byte per-file cap"
        ));
        file.sha256 = sha256_hex(file.content.as_bytes());
    }
    Ok(Some(file))
}

/// Keep the nearest files whole: walk from the working directory back to the
/// repository root, spend the total budget, then restore prompt order.
fn apply_total_cap(files: &mut [AgentsFile], warnings: &mut Vec<String>) {
    let mut remaining = TOTAL_CAP_BYTES;
    for file in files.iter_mut().rev() {
        if file.state == FileState::Skipped {
            continue;
        }
        if remaining == 0 {
            mark_skipped(file, "the total cap is exhausted".to_string(), warnings);
            continue;
        }
        let keep = floor_char_boundary(&file.content, remaining);
        if keep < file.content.len() {
            if keep == 0 {
                mark_skipped(file, "the total cap is exhausted".to_string(), warnings);
                continue;
            }
            file.content.truncate(keep);
            file.bytes = keep;
            file.state = FileState::Truncated;
            add_reason(
                file,
                format!("truncated to fit the {TOTAL_CAP_BYTES} byte total cap"),
            );
            warnings.push(format!(
                "{}: truncated to {keep} of {} bytes to fit the {TOTAL_CAP_BYTES} byte total cap",
                file.source, file.source_bytes
            ));
            file.sha256 = sha256_hex(file.content.as_bytes());
            remaining = 0;
        } else {
            remaining = remaining.saturating_sub(file.content.len());
        }
    }
}

fn mark_skipped(file: &mut AgentsFile, reason: String, warnings: &mut Vec<String>) {
    file.state = FileState::Skipped;
    file.reason = Some(reason.clone());
    file.bytes = 0;
    file.content.clear();
    file.sha256.clear();
    warnings.push(format!("{}: skipped ({reason})", file.source));
}

fn add_reason(file: &mut AgentsFile, reason: String) {
    file.reason = Some(match file.reason.take() {
        Some(previous) if !previous.is_empty() => format!("{previous}; {reason}"),
        _ => reason,
    });
}

fn skipped(candidate: &Path, root: &Path, reason: String) -> AgentsFile {
    let source = source_path(root, candidate);
    skipped_with_size(candidate, &source, 0, reason)
}

fn skipped_with_size(
    candidate: &Path,
    source: &str,
    source_bytes: usize,
    reason: String,
) -> AgentsFile {
    AgentsFile {
        path: candidate.to_path_buf(),
        source: source.to_string(),
        state: FileState::Skipped,
        bytes: 0,
        source_bytes,
        sha256: String::new(),
        reason: Some(reason),
        content: String::new(),
    }
}

/// Repository-relative path with `/` separators. Falls back to the full path
/// if the candidate is outside the root, which cannot happen for accepted
/// files but keeps the manifest honest for skipped ones.
fn source_path(root: &Path, path: &Path) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Largest byte index at or below `max` that is a UTF-8 character boundary.
fn floor_char_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut index = max;
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    fn git_init(path: &Path) {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(["init", "-q"])
            .output()
            .expect("run git init");
        assert!(output.status.success(), "{:?}", output);
    }

    #[test]
    fn loads_the_chain_from_repo_root_to_cwd_with_headers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_init(root);
        write(&root.join("AGENTS.md"), "root rule\n");
        write(&root.join("pkg/AGENTS.md"), "package rule\n");
        write(&root.join("pkg/leaf/AGENTS.md"), "leaf rule\n");

        let agents = AgentsMd::load(&root.join("pkg/leaf"), true);
        assert!(agents.enabled);
        assert_eq!(agents.repo_root, root.canonicalize().unwrap());
        let sources: Vec<&str> = agents
            .files
            .iter()
            .map(|file| file.source.as_str())
            .collect();
        assert_eq!(
            sources,
            vec!["AGENTS.md", "pkg/AGENTS.md", "pkg/leaf/AGENTS.md"]
        );
        assert!(
            agents
                .files
                .iter()
                .all(|file| file.state == FileState::Loaded)
        );

        let body = agents.prompt_body().expect("prompt body");
        let root_at = body.find("root rule").unwrap();
        let package_at = body.find("package rule").unwrap();
        let leaf_at = body.find("leaf rule").unwrap();
        assert!(root_at < package_at && package_at < leaf_at, "{body}");
        assert!(body.contains("### AGENTS.md source: AGENTS.md"), "{body}");
        assert!(
            body.contains("### AGENTS.md source: pkg/AGENTS.md"),
            "{body}"
        );
        assert!(
            body.contains("### AGENTS.md source: pkg/leaf/AGENTS.md"),
            "{body}"
        );
    }

    #[test]
    fn falls_back_to_cwd_outside_a_git_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("plain");
        let leaf = root.join("child");
        std::fs::create_dir_all(&leaf).unwrap();
        write(&root.join("AGENTS.md"), "parent rule\n");
        write(&leaf.join("AGENTS.md"), "leaf rule\n");

        let agents = AgentsMd::load(&leaf, true);
        assert_eq!(agents.repo_root, leaf.canonicalize().unwrap());
        assert_eq!(agents.files.len(), 1, "{:?}", agents.files);
        assert_eq!(agents.files[0].source, "AGENTS.md");
        let body = agents.prompt_body().unwrap();
        assert!(body.contains("leaf rule"), "{body}");
        assert!(!body.contains("parent rule"), "{body}");
    }

    #[test]
    fn per_file_cap_truncates_and_records_the_hash_of_loaded_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let body = "x".repeat(PER_FILE_CAP_BYTES + 500);
        write(&root.join("AGENTS.md"), &body);

        let agents = AgentsMd::load_between(root, root, true);
        let file = &agents.files[0];
        assert_eq!(file.state, FileState::Truncated);
        assert_eq!(file.bytes, PER_FILE_CAP_BYTES);
        assert_eq!(file.source_bytes, body.len());
        assert_eq!(
            file.sha256,
            sha256_hex(&body.as_bytes()[..PER_FILE_CAP_BYTES])
        );
        assert!(
            agents
                .warnings
                .iter()
                .any(|warning| warning.contains("per-file cap")),
            "{:?}",
            agents.warnings
        );
        assert!(agents.prompt_body().unwrap().contains("[truncated:"));
    }

    #[test]
    fn total_cap_is_deterministic_and_keeps_the_nearest_files_whole() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let leaf = root.join("pkg/leaf");
        std::fs::create_dir_all(&leaf).unwrap();
        let content = "y".repeat(PER_FILE_CAP_BYTES);
        write(&root.join("AGENTS.md"), &content);
        write(&root.join("pkg/AGENTS.md"), &content);
        write(&leaf.join("AGENTS.md"), &content);

        let first = AgentsMd::load_between(&leaf, root, true);
        let second = AgentsMd::load_between(&leaf, root, true);
        assert_eq!(first.manifest_json(), second.manifest_json());
        assert_eq!(first.prompt_body(), second.prompt_body());

        assert_eq!(first.loaded_bytes(), TOTAL_CAP_BYTES);
        let root_file = &first.files[0];
        assert_eq!(root_file.state, FileState::Truncated);
        assert_eq!(root_file.bytes, TOTAL_CAP_BYTES - 2 * PER_FILE_CAP_BYTES);
        assert_eq!(
            root_file.sha256,
            sha256_hex(&content.as_bytes()[..TOTAL_CAP_BYTES - 2 * PER_FILE_CAP_BYTES])
        );
        assert_eq!(first.files[1].state, FileState::Loaded);
        assert_eq!(first.files[2].state, FileState::Loaded);
        assert!(
            first
                .warnings
                .iter()
                .any(|warning| warning.contains("total cap")),
            "{:?}",
            first.warnings
        );
    }

    #[test]
    fn a_symlink_outside_the_repository_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.md");
        write(&outside, "outside secret rule\n");
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("AGENTS.md")).unwrap();

        let agents = AgentsMd::load_between(&root, &root, true);
        let file = &agents.files[0];
        assert_eq!(file.state, FileState::Skipped);
        assert_eq!(file.bytes, 0);
        assert!(file.sha256.is_empty());
        assert!(
            file.reason
                .as_deref()
                .unwrap_or("")
                .contains("outside the repository"),
            "{:?}",
            file.reason
        );
        assert!(agents.prompt_body().is_none());
        assert!(
            agents
                .warnings
                .iter()
                .any(|warning| warning.contains("outside the repository")),
            "{:?}",
            agents.warnings
        );
    }

    #[test]
    fn a_symlink_inside_the_repository_is_allowed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("shared.md"), "shared rule\n");
        std::os::unix::fs::symlink("shared.md", root.join("AGENTS.md")).unwrap();

        let agents = AgentsMd::load_between(root, root, true);
        assert_eq!(agents.files[0].state, FileState::Loaded);
        assert!(agents.prompt_body().unwrap().contains("shared rule"));
    }

    #[test]
    fn disabled_loads_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("AGENTS.md"), "rule\n");
        let agents = AgentsMd::load_between(dir.path(), dir.path(), false);
        assert!(!agents.enabled);
        assert!(agents.files.is_empty());
        assert!(agents.prompt_body().is_none());
        assert!(agents.manifest_text().contains("disabled"));
    }

    #[test]
    fn invalid_utf8_and_missing_files_are_handled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), [0xff, 0xfe]).unwrap();
        let agents = AgentsMd::load_between(dir.path(), dir.path(), true);
        assert_eq!(agents.files[0].state, FileState::Skipped);
        assert!(
            agents.files[0]
                .reason
                .as_deref()
                .unwrap_or("")
                .contains("UTF-8")
        );

        let empty = tempfile::tempdir().unwrap();
        let none = AgentsMd::load_between(empty.path(), empty.path(), true);
        assert!(none.files.is_empty());
        assert!(none.prompt_body().is_none());
    }

    #[test]
    fn manifest_reports_path_bytes_hash_and_state() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("AGENTS.md"), "manifest rule\n");
        let agents = AgentsMd::load_between(dir.path(), dir.path(), true);
        let manifest = agents.manifest_json();
        assert_eq!(manifest["enabled"], true);
        assert_eq!(manifest["total_cap_bytes"], TOTAL_CAP_BYTES);
        assert_eq!(manifest["files"][0]["source"], "AGENTS.md");
        assert_eq!(manifest["files"][0]["state"], "loaded");
        assert_eq!(manifest["files"][0]["bytes"], 14);
        assert_eq!(
            manifest["files"][0]["sha256"],
            sha256_hex(b"manifest rule\n")
        );
        let text = agents.manifest_text();
        assert!(text.contains("AGENTS.md"), "{text}");
        assert!(text.contains(&sha256_hex(b"manifest rule\n")), "{text}");
    }
}

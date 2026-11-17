//! Proposing verify steps for a project that has none (#128).
//!
//! Detection reads manifest files only ([`crate::tools::project`]); nothing
//! from the repository runs until the user accepts. Accepted steps are kept
//! in the user's data directory, keyed by the project path, so they load on
//! later runs without `--trust`: the user chose them, the repository did not.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::VerifyStep;
use crate::paths::Paths;
use crate::tools::project::{DetectedVerify, detect_verify_steps};

/// File in the data directory holding accepted steps and offers already made.
pub const STORE_FILE: &str = "verify-steps.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    /// Accepted steps by canonical project path.
    #[serde(default)]
    accepted: BTreeMap<String, Vec<VerifyStep>>,
    /// Projects the TUI already offered detected steps to.
    #[serde(default)]
    offered: Vec<String>,
}

fn store_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join(STORE_FILE)
}

fn project_key(cwd: &Path) -> String {
    cwd.canonicalize()
        .unwrap_or_else(|_| cwd.to_path_buf())
        .display()
        .to_string()
}

fn load(paths: &Paths) -> Store {
    std::fs::read(store_path(paths))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save(paths: &Paths, store: &Store) -> std::io::Result<()> {
    let path = store_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(store).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(&temporary, path)
}

/// Steps the user accepted for this project, if any.
pub fn accepted_steps(paths: &Paths) -> Option<Vec<VerifyStep>> {
    load(paths)
        .accepted
        .remove(&project_key(&paths.cwd))
        .filter(|steps| !steps.is_empty())
}

/// Detect steps and save them for this project. Returns what was saved and
/// where; an empty detection saves nothing.
pub fn accept_detected(paths: &Paths) -> std::io::Result<(DetectedVerify, PathBuf)> {
    let detected = detect_verify_steps(&paths.cwd);
    if !detected.steps.is_empty() {
        let mut store = load(paths);
        store.accepted.insert(
            project_key(&paths.cwd),
            detected.steps.iter().map(|step| step.to_config()).collect(),
        );
        save(paths, &store)?;
    }
    Ok((detected, store_path(paths)))
}

/// Whether the TUI already offered detected steps for this project; records
/// the offer when it had not.
pub fn take_offer(paths: &Paths) -> bool {
    let mut store = load(paths);
    let key = project_key(&paths.cwd);
    if store.offered.contains(&key) {
        return false;
    }
    store.offered.push(key);
    if let Err(error) = save(paths, &store) {
        tracing::warn!(%error, "cannot record the verify offer");
    }
    true
}

/// `check (cargo check --all-targets), test (cargo test)`.
pub fn describe(detected: &DetectedVerify) -> String {
    detected
        .steps
        .iter()
        .map(|step| format!("{} (`{}`)", step.name, step.command))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The one-line notice for a project with no verify steps. `accept` names
/// how this frontend accepts them.
pub fn unconfigured_notice(detected: &DetectedVerify, accept: &str) -> String {
    if detected.steps.is_empty() {
        return "verify: not configured, so changes settle on the model's word. Add [[steps]] to a [verify] config section.".into();
    }
    format!(
        "verify: not configured, so changes settle on the model's word. Detected from {}: {}. Nothing was run. {accept}",
        detected.markers.join(", "),
        describe(detected)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(dir: &Path) -> Paths {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths.config_dir = dir.join("config");
        paths
    }

    #[test]
    fn detected_steps_are_saved_only_on_acceptance_and_load_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/x\n").unwrap();
        let paths = paths(dir.path());
        assert!(accepted_steps(&paths).is_none());
        let notice =
            unconfigured_notice(&detect_verify_steps(&paths.cwd), "Run tot verify --init.");
        assert!(notice.contains("go vet ./..."), "{notice}");
        assert!(notice.contains("Nothing was run"), "{notice}");
        // Still nothing saved after the notice.
        assert!(accepted_steps(&paths).is_none());

        let (detected, _) = accept_detected(&paths).unwrap();
        assert_eq!(detected.kind, Some("go"));
        let steps = accepted_steps(&paths).expect("saved");
        assert_eq!(steps[0].command, ["go", "vet", "./..."]);
    }

    #[test]
    fn the_offer_is_made_once_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        assert!(take_offer(&paths));
        assert!(!take_offer(&paths));
    }
}

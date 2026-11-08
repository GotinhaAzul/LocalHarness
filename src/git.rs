//! Git status for the TUI header.
//!
//! The status is refreshed every two seconds by [`watch`], which runs the
//! blocking `git` invocations on a blocking task so the UI loop never waits.

use std::path::{Path, PathBuf};
use std::process::Command;

use tokio::sync::mpsc::UnboundedSender;

/// What the header needs to know about the working tree.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// Inside a work tree (a repository with a readable HEAD).
    pub in_repo: bool,
    /// Current branch name. `None` when detached or outside a repository.
    pub branch: Option<String>,
    /// HEAD is detached.
    pub detached: bool,
    /// Tracked or untracked changes exist.
    pub dirty: bool,
}

impl GitStatus {
    /// Short label for the header: `(main*)`, `(detached)`, or `no git`.
    pub fn label(&self) -> String {
        if !self.in_repo {
            return "no git".to_string();
        }
        if self.detached {
            return "(detached)".to_string();
        }
        let branch = self.branch.as_deref().unwrap_or("?");
        let marker = if self.dirty { "*" } else { "" };
        format!("({branch}{marker})")
    }

    /// A status that means "not inside a repository".
    pub fn none() -> Self {
        Self::default()
    }
}

fn git_output(cwd: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .ok()
}

/// Read the git status. Never fails: outside a repository, or when `git` is
/// missing, the result is a plain "no git" status.
pub fn status(cwd: &Path) -> GitStatus {
    let Some(output) = git_output(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]) else {
        return GitStatus::none();
    };
    if !output.status.success() {
        return GitStatus::none();
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if branch.is_empty() {
        return GitStatus::none();
    }
    let detached = branch == "HEAD";
    let dirty = git_output(cwd, &["status", "--porcelain"])
        .filter(|out| out.status.success())
        .map(|out| !String::from_utf8_lossy(&out.stdout).trim().is_empty())
        .unwrap_or(false);
    GitStatus {
        in_repo: true,
        branch: if detached { None } else { Some(branch) },
        detached,
        dirty,
    }
}

/// Refresh the status immediately and then every two seconds until the
/// receiver is dropped. Blocking git calls run on the blocking pool.
pub async fn watch(cwd: PathBuf, tx: UnboundedSender<GitStatus>) {
    loop {
        let dir = cwd.clone();
        let status = tokio::task::spawn_blocking(move || status(&dir))
            .await
            .unwrap_or_default();
        if tx.send(status).is_err() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_cover_repo_detached_and_plain() {
        assert_eq!(GitStatus::none().label(), "no git");
        assert_eq!(
            GitStatus {
                in_repo: true,
                branch: Some("main".into()),
                detached: false,
                dirty: true,
            }
            .label(),
            "(main*)"
        );
        assert_eq!(
            GitStatus {
                in_repo: true,
                branch: Some("main".into()),
                detached: false,
                dirty: false,
            }
            .label(),
            "(main)"
        );
        assert_eq!(
            GitStatus {
                in_repo: true,
                branch: None,
                detached: true,
                dirty: false,
            }
            .label(),
            "(detached)"
        );
    }

    #[test]
    fn outside_a_repository_reports_no_git() {
        let dir = tempfile::tempdir().unwrap();
        // A fresh temporary directory is not inside a work tree.
        assert_eq!(status(dir.path()), GitStatus::none());
    }
}

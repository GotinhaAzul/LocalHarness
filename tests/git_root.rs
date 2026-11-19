//! The git and review tools stop at the session root: a workspace without a
//! repository of its own must not report or change the repository that
//! contains it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::json;
use tokio_util::sync::CancellationToken;
use totality::config::{GitConfig, VerifyConfig};
use totality::paths::Paths;
use totality::security::SecurityState;
use totality::session::{Session, SessionData};
use totality::tools::git::GitTool;
use totality::tools::pr::PrTool;
use totality::tools::review::ReviewTool;
use totality::tools::{BgRegistry, ReadTracker, Tool, ToolContext};

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn run_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

struct Fixture {
    _temp: tempfile::TempDir,
    parent: PathBuf,
    ctx: ToolContext,
}

impl Fixture {
    /// A parent repository with a dirty file, and `workspace/` inside it with
    /// no repository of its own: one tracked file modified, one untracked.
    fn new() -> Option<Self> {
        if !git_available() {
            return None;
        }
        let temp = tempfile::tempdir().expect("temp dir");
        let parent = temp.path().join("parent");
        let workspace = parent.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        run_git(&parent, &["init", "-q", "-b", "main"]);
        run_git(&parent, &["config", "user.email", "test@example.invalid"]);
        run_git(&parent, &["config", "user.name", "Test"]);
        run_git(&parent, &["config", "commit.gpgsign", "false"]);
        std::fs::write(parent.join("unrelated.md"), "base\n").expect("parent file");
        std::fs::write(workspace.join("inside.md"), "base\n").expect("tracked file");
        run_git(&parent, &["add", "-A"]);
        run_git(&parent, &["commit", "-q", "-m", "base"]);

        // Unrelated parent change, plus changes inside the workspace.
        std::fs::write(parent.join("unrelated.md"), "base\nDIRTY\n").expect("dirty parent");
        std::fs::write(workspace.join("inside.md"), "base\nchanged\n").expect("dirty inside");
        std::fs::write(workspace.join("note.txt"), "hi\n").expect("untracked inside");

        let cwd = workspace.canonicalize().expect("canonical workspace");
        let paths = Paths {
            config_dir: temp.path().join("config"),
            data_dir: temp.path().join("data"),
            cwd: cwd.clone(),
            project_id: "test".into(),
            sessions_dir_override: None,
            home_dir: None,
        };
        let header = SessionData {
            cwd: cwd.display().to_string(),
            endpoint: "test".into(),
            model: "test".into(),
            profile: "test".into(),
            app: "test".into(),
            redact: true,
            context: None,
            permissions: None,
        };
        let session = Session::create(&paths, header, true).expect("in-memory session");
        let artifacts = session.artifacts.clone();
        let session = Arc::new(Mutex::new(session));
        let ctx = ToolContext {
            cwd,
            // The boundary, not the path jail, must refuse parent paths.
            path_jail: false,
            session,
            artifacts,
            cancel: CancellationToken::new(),
            trust: false,
            verify: VerifyConfig::default(),
            profile: "test".into(),
            packs: Vec::new(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::default())),
            security: Arc::new(Mutex::new(SecurityState::default())),
            paths,
            lsp: Arc::new(totality::tools::lsp::LspService::new()),
            sandbox: None,
            vision: Default::default(),
            caps: Default::default(),
            format_on_edit: false,
            edit_mode: totality::config::EditMode::Text,
        };
        Some(Fixture {
            _temp: temp,
            parent,
            ctx,
        })
    }
}

#[tokio::test]
async fn status_hides_the_enclosing_repository() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let tool = GitTool::new(GitConfig::default());

    let status = tool.call(&fixture.ctx, json!({"action": "status"})).await;
    assert!(!status.is_error, "{}", status.content);
    assert!(status.content.contains("inside.md"), "{}", status.content);
    assert!(!status.content.contains("unrelated"), "{}", status.content);
    assert!(!status.content.contains("../"), "{}", status.content);
}

#[tokio::test]
async fn review_only_sees_files_inside_the_session_root() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let tool = ReviewTool::new();

    let review = tool.call(&fixture.ctx, json!({})).await;
    assert!(!review.is_error, "{}", review.content);
    assert!(
        review.content.starts_with("review (working tree): 2 files"),
        "{}",
        review.content
    );
    assert!(review.content.contains("inside.md"), "{}", review.content);
    assert!(
        review.content.contains("untracked: 1"),
        "{}",
        review.content
    );
    assert!(!review.content.contains("unrelated"), "{}", review.content);
    assert!(!review.content.contains("../"), "{}", review.content);
}

#[tokio::test]
async fn a_mutating_action_refuses_a_parent_repository() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let tool = GitTool::new(GitConfig::default());

    let staged = tool
        .call(
            &fixture.ctx,
            json!({"action": "stage", "paths": ["note.txt"]}),
        )
        .await;
    assert!(staged.is_error, "{}", staged.content);
    assert!(
        staged.content.contains("GIT_REPO_OUTSIDE_ROOT"),
        "{}",
        staged.content
    );

    // The parent repository is untouched, so this test proves more than the
    // error text: a refused action changed nothing.
    let staged_in_parent = run_git(&fixture.parent, &["diff", "--cached", "--name-only"]);
    assert!(
        staged_in_parent.trim().is_empty(),
        "the parent index changed: {staged_in_parent}"
    );
}

#[tokio::test]
async fn a_read_action_rejects_a_path_outside_the_root() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let tool = GitTool::new(GitConfig::default());

    let diff = tool
        .call(
            &fixture.ctx,
            json!({"action": "diff", "path": "../unrelated.md"}),
        )
        .await;
    assert!(diff.is_error, "{}", diff.content);
    assert!(
        diff.content.contains("GIT_OUTSIDE_WORKSPACE"),
        "{}",
        diff.content
    );
    assert!(!diff.content.contains("DIRTY"), "{}", diff.content);
}

#[tokio::test]
async fn pathspec_magic_cannot_reach_the_parent_history() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    run_git(
        &fixture.parent,
        &["commit", "-q", "-m", "parent only subject", "unrelated.md"],
    );
    let tool = GitTool::new(GitConfig::default());

    for path in [":/", ":(top)", ":/unrelated.md"] {
        let log = tool
            .call(&fixture.ctx, json!({"action": "log", "path": path}))
            .await;
        assert!(log.is_error, "{path}: {}", log.content);
        assert!(
            log.content.contains("GIT_OUTSIDE_WORKSPACE"),
            "{path}: {}",
            log.content
        );
        assert!(
            !log.content.contains("parent only subject"),
            "{path}: {}",
            log.content
        );
    }
}

#[tokio::test]
async fn commit_message_samples_only_commits_inside_the_root() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    run_git(
        &fixture.parent,
        &["commit", "-q", "-m", "parent only subject", "unrelated.md"],
    );
    run_git(&fixture.parent, &["add", "workspace/note.txt"]);
    let tool = GitTool::new(GitConfig::default());

    let message = tool
        .call(&fixture.ctx, json!({"action": "commit_message"}))
        .await;
    assert!(message.content.contains("note.txt"), "{}", message.content);
    assert!(
        !message.content.contains("parent only subject"),
        "{}",
        message.content
    );
}

#[tokio::test]
async fn pr_draft_refuses_a_parent_repository() {
    let Some(fixture) = Fixture::new() else {
        return;
    };
    let tool = PrTool::new(GitConfig::default());

    let draft = tool
        .call(
            &fixture.ctx,
            json!({"action": "draft", "title": "t", "body": "b"}),
        )
        .await;
    assert!(draft.is_error, "{}", draft.content);
    assert!(
        draft.content.contains("GIT_REPO_OUTSIDE_ROOT"),
        "{}",
        draft.content
    );
    assert!(!draft.content.contains("unrelated"), "{}", draft.content);
}

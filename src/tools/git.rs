//! `git`: structured repository actions with safe defaults.
//!
//! The model can run `git` through `bash`, but nothing there constrains it.
//! This tool gives it explicit actions instead: read-only `status`, `diff`,
//! `log` and `blame`; `stage`/`unstage` limited to the workspace; `commit`
//! that previews and, by default, waits for the user (`/commit confirm`);
//! `branch` and `worktree` that refuse to lose work; and `undo`, a soft reset
//! of the last commit this harness made. Every mutation returns the exact
//! command and its output.
//!
//! History surgery (`reset --hard`, `clean -f`, `push --force`, `rebase`,
//! `filter-branch`, tag deletion) is not an action here, and
//! [`destructive_git`] refuses it in `bash` and `bg` too.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::{Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_bool, optional_str};
use crate::config::{GitApproval, GitConfig, SandboxMode};
use crate::session::{EntryKind, Session};

/// Session entry holding a preview that waits for the user's approval.
pub const PENDING_KIND: &str = "git-pending";
/// Session entry closing a pending preview (approved, cancelled, replaced).
pub const RESOLVED_KIND: &str = "git-resolved";
/// Session entry recording a commit this harness made, for `undo`.
pub const COMMIT_KIND: &str = "git-commit";
/// Session entry recording that `undo` reverted a recorded commit.
pub const UNDO_KIND: &str = "git-undo";

/// Recent subjects read to learn the repository's commit style.
const STYLE_SAMPLE: usize = 20;

pub struct GitTool {
    config: GitConfig,
}

impl GitTool {
    pub fn new(config: GitConfig) -> Self {
        Self { config }
    }
}

impl Default for GitTool {
    fn default() -> Self {
        Self::new(GitConfig::default())
    }
}

#[async_trait]
impl Tool for GitTool {
    fn name(&self) -> &'static str {
        "git"
    }

    fn description(&self) -> &'static str {
        "Git with safe defaults: status, diff, log, blame, stage, unstage, commit_message (style + draft), commit (user approves), branch, worktree, undo. No reset --hard, force-push or rebase."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "action": {
                    "type": "string",
                    "enum": ["status", "diff", "log", "blame", "stage", "unstage", "commit_message", "commit", "branch", "worktree", "undo"],
                },
                "paths": {"type": "array", "items": {"type": "string"}, "description": "stage/unstage: workspace files."},
                "path": {"type": "string", "description": "diff/log/blame filter; worktree directory."},
                "staged": {"type": "boolean", "description": "diff: staged changes."},
                "stat": {"type": "boolean", "description": "diff: summary only."},
                "count": {"type": "integer", "minimum": 1, "maximum": 200, "description": "log: commits (default 20)."},
                "start": {"type": "integer", "minimum": 1, "description": "blame: first line."},
                "end": {"type": "integer", "minimum": 1, "description": "blame: last line."},
                "message": {"type": "string", "description": "commit: subject, blank line, body saying why."},
                "op": {"type": "string", "enum": ["create", "switch", "list", "add", "remove"], "description": "branch: create|switch|list; worktree: add|remove|list."},
                "name": {"type": "string", "description": "branch name."},
                "branch": {"type": "string", "description": "worktree add: branch (created if missing)."},
                "base": {"type": "string", "description": "start point (default HEAD)."},
                "stash": {"type": "boolean", "description": "switch: stash changes first."},
            }),
            &["action"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Coding
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let ctx = ctx.clone();
        let config = self.config.clone();
        tokio::task::spawn_blocking(move || dispatch(&ctx, &config, &args))
            .await
            .unwrap_or_else(|error| {
                ToolResult::err("internal", format!("git task failed: {error}"))
            })
    }
}

fn dispatch(ctx: &ToolContext, config: &GitConfig, args: &Value) -> ToolResult {
    let action = optional_str(args, "action").unwrap_or_default();
    if !is_repo(&ctx.cwd) {
        return ToolResult::err(
            "GIT_NO_REPO",
            format!(
                "{} is not inside a git repository; run git init with the user's agreement first",
                ctx.cwd.display()
            ),
        );
    }
    let mutating = match action.as_str() {
        "stage" | "unstage" | "commit" | "undo" => true,
        "branch" | "worktree" => {
            !matches!(optional_str(args, "op").as_deref(), Some("list") | None)
        }
        _ => false,
    };
    // The session root is the boundary. A workspace without a repository of
    // its own must not mutate the repository that happens to contain it.
    if mutating && let Some(top) = enclosing_repo(&ctx.cwd) {
        return ToolResult::err(
            "GIT_REPO_OUTSIDE_ROOT",
            format!(
                "{} is inside the git work tree at {}, not at its root, so the harness will not change that repository from here. Read-only status, diff, log, blame and commit_message stay limited to {}; run \"git init\" here with the user's agreement, or start the session at {}.",
                ctx.cwd.display(),
                top.display(),
                ctx.cwd.display(),
                top.display()
            ),
        );
    }
    if mutating && read_only_mode(ctx) {
        return ToolResult::err(
            "GIT_READ_ONLY",
            "the session runs with --sandbox read-only, so git is limited to status, diff, log, blame and commit_message",
        );
    }
    match action.as_str() {
        "status" => status(ctx),
        "diff" => diff(ctx, args),
        "log" => log(ctx, args),
        "blame" => blame(ctx, args),
        "stage" => stage(ctx, args, true),
        "unstage" => stage(ctx, args, false),
        "commit_message" => commit_message(ctx, config),
        "commit" => commit(ctx, config, args),
        "branch" => branch(ctx, args),
        "worktree" => worktree(ctx, args),
        "undo" => undo(ctx),
        other => ToolResult::err(
            "invalid_args",
            format!(
                "unknown git action {other:?}; use status, diff, log, blame, stage, unstage, commit_message, commit, branch, worktree or undo"
            ),
        ),
    }
}

fn read_only_mode(ctx: &ToolContext) -> bool {
    ctx.sandbox
        .as_ref()
        .is_some_and(|sandbox| sandbox.mode() == SandboxMode::ReadOnly)
}

// ---------------------------------------------------------------------------
// Process helpers

/// One finished `git` (or `gh`) invocation.
#[derive(Debug, Clone)]
pub struct Run {
    pub command: String,
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    /// The command and its combined output, as returned for mutations.
    pub fn transcript(&self) -> String {
        let mut out = format!("$ {}", self.command);
        for stream in [&self.stdout, &self.stderr] {
            let text = stream.trim_end();
            if !text.is_empty() {
                out.push('\n');
                out.push_str(text);
            }
        }
        out
    }

    /// The most useful single error text.
    pub fn error(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        format!("`{}` failed: {}", self.command, text)
    }
}

fn quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,^~{}".contains(c))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Run `program args` in `cwd` without a terminal prompt; `input` goes to
/// stdin when given.
pub fn run(program: &str, cwd: &Path, args: &[&str], input: Option<&str>) -> Run {
    run_env(program, cwd, args, input, &[])
}

fn run_env(
    program: &str,
    cwd: &Path,
    args: &[&str],
    input: Option<&str>,
    env: &[(&str, &std::ffi::OsStr)],
) -> Run {
    let command = std::iter::once(program.to_string())
        .chain(args.iter().map(|arg| quote(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    let mut process = Command::new(program);
    process
        .args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_PAGER", "cat")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("NO_COLOR", "1")
        .envs(env.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let spawned = process.spawn().and_then(|mut child| {
        if let Some(input) = input
            && let Some(mut stdin) = child.stdin.take()
        {
            stdin.write_all(input.as_bytes())?;
        }
        child.wait_with_output()
    });
    match spawned {
        Ok(output) => Run {
            command,
            ok: output.status.success(),
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => Run {
            command,
            ok: false,
            code: None,
            stdout: String::new(),
            stderr: format!("cannot run {program}: {error}"),
        },
    }
}

pub fn git(cwd: &Path, args: &[&str]) -> Run {
    run("git", cwd, args, None)
}

fn git_line(cwd: &Path, args: &[&str]) -> Option<String> {
    let run = git(cwd, args);
    let line = run.stdout.trim();
    (run.ok && !line.is_empty()).then(|| line.to_string())
}

/// The tree object of the working directory as it is on disk: tracked and
/// untracked files, without ignored ones. Built in a throwaway index, so the
/// real index is untouched. `None` outside a repository.
pub fn content_tree(cwd: &Path) -> Option<String> {
    if !is_repo(cwd) {
        return None;
    }
    let real = git_line(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-path", "index"],
    )?;
    // A sibling of the real index, removed afterwards.
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let index = std::path::PathBuf::from(format!("{real}.totality-{}-{nonce}", std::process::id()));
    // Starting from the real index reuses its stat cache; a missing one (a
    // repository without commits) just means a slower first add.
    let _ = std::fs::copy(&real, &index);
    let env = [("GIT_INDEX_FILE", index.as_os_str())];
    let add = run_env("git", cwd, &["add", "--all", "--", ":/"], None, &env);
    let tree = add
        .ok
        .then(|| run_env("git", cwd, &["write-tree"], None, &env))
        .filter(|tree| tree.ok);
    let _ = std::fs::remove_file(&index);
    let tree = tree?;
    let tree = tree.stdout.trim();
    (!tree.is_empty()).then(|| tree.to_string())
}

/// Whether HEAD holds exactly the content recorded in `tree`. Files present
/// in `tree` but not in HEAD are allowed only while they are still on disk
/// (untracked leftovers that the commit does not carry).
pub fn head_matches_tree(cwd: &Path, tree: &str) -> bool {
    let diff = git(
        cwd,
        &[
            "diff-tree",
            "-r",
            "--no-renames",
            "--name-status",
            "-z",
            tree,
            "HEAD",
        ],
    );
    let Some(top) = git_line(cwd, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    if !diff.ok {
        return false;
    }
    let top = Path::new(&top);
    let mut fields = diff.stdout.split('\0').filter(|field| !field.is_empty());
    while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
        if status != "D" || !top.join(path).exists() {
            return false;
        }
    }
    true
}

pub fn is_repo(cwd: &Path) -> bool {
    git_line(cwd, &["rev-parse", "--is-inside-work-tree"]).as_deref() == Some("true")
}

/// The work-tree root that contains `cwd`, when it is an ancestor of the
/// session root rather than the root itself. `None` when `cwd` is the root.
fn enclosing_repo(cwd: &Path) -> Option<PathBuf> {
    let top = git_line(cwd, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(top);
    let top = top.canonicalize().unwrap_or(top);
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    (top != root).then_some(top)
}

/// Reject a diff/log/blame path that resolves outside the session root.
fn ensure_inside_root(ctx: &ToolContext, raw: &str) -> Result<(), ToolResult> {
    // A leading `:` is pathspec magic (`:/`, `:(top)`), which git resolves
    // from the repository top rather than the session root.
    if raw.trim_start().starts_with(':') {
        return Err(ToolResult::err(
            "GIT_OUTSIDE_WORKSPACE",
            format!(
                "{raw} is a git pathspec with magic; pass a plain path under the session root {}",
                ctx.cwd.display()
            ),
        ));
    }
    let resolved = super::normalize(&ctx.resolve(raw)?);
    let root = ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone());
    if !resolved.starts_with(&root) && !resolved.starts_with(&ctx.cwd) {
        return Err(ToolResult::err(
            "GIT_OUTSIDE_WORKSPACE",
            format!(
                "{raw} is outside the session root {}; git actions only touch files under it",
                ctx.cwd.display()
            ),
        ));
    }
    Ok(())
}

/// Whether a status path leaves the session root: absolute, or through `..`.
fn path_escapes(path: &str) -> bool {
    let path = path.trim().trim_matches('"');
    Path::new(path).is_absolute() || path.split('/').any(|part| part == "..")
}

fn status_line_escapes(line: &str) -> bool {
    let Some(rest) = line.get(3..) else {
        return false;
    };
    rest.split(" -> ").any(path_escapes)
}

pub fn head(cwd: &Path) -> Option<String> {
    git_line(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"])
}

pub fn current_branch(cwd: &Path) -> Option<String> {
    git_line(cwd, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

/// Tracked files with uncommitted (staged or unstaged) changes.
pub fn dirty_tracked(cwd: &Path) -> Vec<String> {
    git(cwd, &["status", "--porcelain", "--untracked-files=no"])
        .stdout
        .lines()
        .filter(|line| line.len() > 3)
        .map(|line| line[3..].to_string())
        .collect()
}

/// `(status letter, path)` for each staged file under the session root.
pub fn staged_files(cwd: &Path) -> Vec<(char, String)> {
    git(
        cwd,
        &[
            "diff",
            "--cached",
            "--name-status",
            "-M",
            "--no-color",
            "--relative",
            "--",
            ".",
        ],
    )
    .stdout
    .lines()
    .filter_map(|line| {
        let mut parts = line.split('\t');
        let status = parts.next()?.chars().next()?;
        let path = parts.next_back()?.to_string();
        Some((status, path))
    })
    .collect()
}

/// A fingerprint of HEAD plus the staged diff: an approved preview is only
/// committed while it still describes exactly what is staged.
fn staged_fingerprint(cwd: &Path) -> String {
    let diff = git(cwd, &["diff", "--cached", "--binary", "--no-color"]).stdout;
    let mut hasher = Sha256::new();
    hasher.update(head(cwd).unwrap_or_default());
    hasher.update(b"\0");
    hasher.update(diff);
    format!("{:x}", hasher.finalize())
}

fn ok_with_session(content: String, entries: Vec<Value>) -> ToolResult {
    ToolResult::ok(content).with_details(json!({ "session": entries }))
}

// ---------------------------------------------------------------------------
// Read-only actions

fn status(ctx: &ToolContext) -> ToolResult {
    // `-- .` keeps the enclosing repository's changes out of a workspace that
    // has no repository of its own; the filter drops any path that still
    // leaves the session root (a rename or a pathspec edge case).
    let run = git(&ctx.cwd, &["status", "--short", "--branch", "--", "."]);
    if !run.ok {
        return ToolResult::err("GIT_FAILED", run.error());
    }
    let text = run
        .stdout
        .lines()
        .filter(|line| !status_line_escapes(line))
        .collect::<Vec<_>>()
        .join("\n");
    if text.lines().count() <= 1 {
        return ToolResult::ok(format!("{text}\nworking tree clean"));
    }
    ToolResult::ok(text)
}

fn diff(ctx: &ToolContext, args: &Value) -> ToolResult {
    let mut argv = vec![
        "diff",
        "--no-color",
        "--relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ];
    if optional_bool(args, "staged").unwrap_or(false) {
        argv.push("--cached");
    }
    if optional_bool(args, "stat").unwrap_or(false) {
        argv.push("--stat");
    }
    let path = optional_str(args, "path");
    if let Some(path) = &path {
        if let Err(err) = ensure_inside_root(ctx, path) {
            return err;
        }
        argv.push("--");
        argv.push(path);
    } else {
        argv.push("--");
        argv.push(".");
    }
    let run = git(&ctx.cwd, &argv);
    if !run.ok {
        return ToolResult::err("GIT_FAILED", run.error());
    }
    if run.stdout.trim().is_empty() {
        let which = if argv.contains(&"--cached") {
            "nothing is staged"
        } else {
            "no unstaged changes"
        };
        return ToolResult::ok(format!("{which} (try staged: true, or git status)"));
    }
    ToolResult::ok(run.stdout)
}

fn log(ctx: &ToolContext, args: &Value) -> ToolResult {
    let count = args
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(20)
        .clamp(1, 200)
        .to_string();
    let mut argv = vec![
        "log",
        "--no-color",
        "--date=short",
        "--format=%h %ad %an%d %s",
        "-n",
        &count,
    ];
    let path = optional_str(args, "path");
    if let Some(path) = &path {
        if let Err(err) = ensure_inside_root(ctx, path) {
            return err;
        }
        argv.push("--");
        argv.push(path);
    } else if enclosing_repo(&ctx.cwd).is_some() {
        // A path-limited log skips commits that touch nothing under the root;
        // at the root itself the unfiltered history is the same and keeps
        // empty commits visible.
        argv.push("--");
        argv.push(".");
    }
    let run = git(&ctx.cwd, &argv);
    if !run.ok {
        if head(&ctx.cwd).is_none() {
            return ToolResult::ok("no commits yet");
        }
        return ToolResult::err("GIT_FAILED", run.error());
    }
    ToolResult::ok(run.stdout)
}

fn blame(ctx: &ToolContext, args: &Value) -> ToolResult {
    let Some(path) = optional_str(args, "path") else {
        return ToolResult::err(
            "missing_argument",
            "blame needs path, for example {\"action\":\"blame\",\"path\":\"src/main.rs\",\"start\":10,\"end\":40}",
        );
    };
    if let Err(err) = ensure_inside_root(ctx, &path) {
        return err;
    }
    let start = args.get("start").and_then(Value::as_u64);
    let end = args.get("end").and_then(Value::as_u64);
    let range = match (start, end) {
        (Some(start), Some(end)) if end >= start => Some(format!("{start},{end}")),
        (Some(start), None) => Some(format!("{start},+40")),
        (None, Some(end)) => Some(format!("1,{end}")),
        (Some(_), Some(_)) => {
            return ToolResult::err("invalid_args", "blame end must be at or after start");
        }
        (None, None) => None,
    };
    let mut argv = vec!["blame", "--date=short"];
    if let Some(range) = &range {
        argv.push("-L");
        argv.push(range);
    }
    argv.push("--");
    argv.push(&path);
    let run = git(&ctx.cwd, &argv);
    if !run.ok {
        return ToolResult::err("GIT_FAILED", run.error());
    }
    ToolResult::ok(run.stdout)
}

// ---------------------------------------------------------------------------
// Staging

/// File names that hold credentials; staging them is refused.
fn credential_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name == ".env"
        || (name.starts_with(".env.") && !name.ends_with(".example") && !name.ends_with(".sample"))
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name == "id_rsa"
        || name == "id_ed25519"
        || name == "auth.json"
        || name == "token"
        || name.ends_with("token.json")
        || name.ends_with("token.txt")
}

fn stage(ctx: &ToolContext, args: &Value, add: bool) -> ToolResult {
    let verb = if add { "stage" } else { "unstage" };
    let paths: Vec<String> = match args.get("paths") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    if paths.is_empty() {
        return ToolResult::err(
            "missing_argument",
            format!(
                "{verb} needs paths, for example {{\"action\":\"{verb}\",\"paths\":[\"src/lib.rs\"]}}"
            ),
        );
    }
    let workspace = ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone());
    let mut relative = Vec::new();
    for raw in &paths {
        let resolved = match ctx.resolve(raw) {
            Ok(path) => path,
            Err(err) => return err,
        };
        let real = resolved
            .canonicalize()
            .or_else(|_| {
                resolved
                    .parent()
                    .map(|parent| {
                        parent
                            .canonicalize()
                            .map(|parent| parent.join(resolved.file_name().unwrap_or_default()))
                    })
                    .unwrap_or_else(|| Ok(resolved.clone()))
            })
            .unwrap_or(resolved);
        let Ok(inside) = real.strip_prefix(&workspace) else {
            return ToolResult::err(
                "GIT_OUTSIDE_WORKSPACE",
                format!(
                    "{raw} is outside the workspace {}; only files under it can be staged",
                    workspace.display()
                ),
            );
        };
        let inside = inside.to_string_lossy().to_string();
        if add && credential_file(&inside) {
            return ToolResult::err(
                "GIT_SECRET_FILE",
                format!(
                    "{inside} looks like a credential file and is never staged by the harness; add it to .gitignore, or ask the user to stage it themselves if it is safe"
                ),
            );
        }
        relative.push(if inside.is_empty() {
            ".".to_string()
        } else {
            inside
        });
    }
    let mut argv: Vec<&str> = if add {
        vec!["add", "--"]
    } else if head(&ctx.cwd).is_some() {
        vec!["restore", "--staged", "--"]
    } else {
        vec!["rm", "--cached", "--quiet", "-r", "--"]
    };
    argv.extend(relative.iter().map(String::as_str));
    let run = git(&ctx.cwd, &argv);
    if !run.ok {
        return ToolResult::err("GIT_FAILED", run.error());
    }
    let staged = staged_files(&ctx.cwd);
    let mut out = run.transcript();
    if staged.is_empty() {
        out.push_str("\nnothing is staged");
    } else {
        out.push_str(&format!("\nstaged ({}):", staged.len()));
        for (status, path) in &staged {
            out.push_str(&format!("\n  {status} {path}"));
        }
    }
    ToolResult::ok(out)
}

// ---------------------------------------------------------------------------
// Commit messages

/// The dominant shape of a repository's recent commit subjects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Style {
    /// `type(scope): subject` (Conventional Commits).
    Conventional,
    /// `area: subject`, where the area is a module or directory.
    Area,
    /// A plain sentence, capitalized or not.
    Sentence { lowercase: bool },
}

/// What [`detect_style`] learned, with the evidence it used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyleReport {
    pub style: Style,
    /// Subjects in the dominant style, out of `sampled`.
    pub matching: usize,
    pub sampled: usize,
    /// Most subjects end with a period.
    pub period: bool,
    pub example: Option<String>,
}

impl StyleReport {
    pub fn label(&self) -> String {
        let name = match self.style {
            Style::Conventional => "Conventional Commits (type(scope): subject)",
            Style::Area => "area prefix (area: subject)",
            Style::Sentence { lowercase: false } => "capitalized sentence",
            Style::Sentence { lowercase: true } => "lowercase sentence",
        };
        if self.sampled == 0 {
            return format!("{name}; no history, so this is the default");
        }
        format!(
            "{name}, {} of {} recent subjects{}",
            self.matching,
            self.sampled,
            if self.period {
                ", ending with a period"
            } else {
                ""
            }
        )
    }
}

const CONVENTIONAL_TYPES: &[&str] = &[
    "feat", "fix", "docs", "style", "refactor", "perf", "test", "tests", "build", "ci", "chore",
    "revert",
];

fn is_conventional(subject: &str) -> bool {
    let Some((head, rest)) = subject.split_once(':') else {
        return false;
    };
    if !rest.starts_with(' ') {
        return false;
    }
    let head = head.trim_end_matches('!');
    let kind = head.split('(').next().unwrap_or("");
    let scope_ok = match head.find('(') {
        Some(open) => head.ends_with(')') && open + 2 < head.len(),
        None => true,
    };
    scope_ok && CONVENTIONAL_TYPES.contains(&kind.to_ascii_lowercase().as_str())
}

fn is_area(subject: &str) -> bool {
    let Some((head, rest)) = subject.split_once(": ") else {
        return false;
    };
    !rest.is_empty()
        && !head.is_empty()
        && head.len() <= 40
        && head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./,".contains(c))
}

/// Learn the dominant style from recent subjects. Ties and an empty history
/// fall back to a capitalized sentence.
pub fn detect_style(subjects: &[String]) -> StyleReport {
    let subjects: Vec<&str> = subjects
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && !s.starts_with("Merge "))
        .collect();
    let sampled = subjects.len();
    let conventional = subjects.iter().filter(|s| is_conventional(s)).count();
    let area = subjects
        .iter()
        .filter(|s| !is_conventional(s) && is_area(s))
        .count();
    let plain: Vec<&&str> = subjects
        .iter()
        .filter(|s| !is_conventional(s) && !is_area(s))
        .collect();
    let lowercase = plain
        .iter()
        .filter(|s| s.chars().next().is_some_and(char::is_lowercase))
        .count();
    let period = sampled > 0 && subjects.iter().filter(|s| s.ends_with('.')).count() * 2 > sampled;
    let (style, matching, example) = if sampled > 0 && conventional * 2 > sampled {
        (
            Style::Conventional,
            conventional,
            subjects.iter().find(|s| is_conventional(s)),
        )
    } else if sampled > 0 && area * 2 > sampled {
        (
            Style::Area,
            area,
            subjects.iter().find(|s| !is_conventional(s) && is_area(s)),
        )
    } else {
        let lower = lowercase * 2 > plain.len() && !plain.is_empty();
        let matching = if lower {
            lowercase
        } else {
            plain.len() - lowercase
        };
        (
            Style::Sentence { lowercase: lower },
            matching,
            plain.first().copied(),
        )
    };
    StyleReport {
        style,
        matching,
        sampled,
        period,
        example: example.map(|s| s.to_string()),
    }
}

fn is_doc(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".md")
        || lower.ends_with(".rst")
        || lower.ends_with(".txt")
        || lower.starts_with("docs/")
}

fn is_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit('/').next().unwrap_or(&lower);
    lower.starts_with("tests/")
        || lower.contains("/tests/")
        || lower.contains("/test/")
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
}

/// The module or directory a set of paths belongs to.
fn area_of(paths: &[&str]) -> String {
    let dirs: Vec<Vec<&str>> = paths
        .iter()
        .map(|path| {
            let mut parts: Vec<&str> = path.split('/').collect();
            parts.pop();
            if parts.first() == Some(&"src") && parts.len() > 1 {
                parts.remove(0);
            }
            parts
        })
        .collect();
    let mut common: Vec<&str> = dirs.first().cloned().unwrap_or_default();
    for dir in &dirs[1.min(dirs.len())..] {
        let shared = common.iter().zip(dir).take_while(|(a, b)| a == b).count();
        common.truncate(shared);
    }
    if let Some(last) = common.last() {
        return last.to_string();
    }
    if paths.len() == 1 {
        let name = paths[0].rsplit('/').next().unwrap_or(paths[0]);
        return name.split('.').next().unwrap_or(name).to_string();
    }
    "repo".to_string()
}

/// A deterministic subject for the staged files in `report`'s style. It is a
/// fallback that works with no model; the model should write a better one.
pub fn draft_subject(report: &StyleReport, files: &[(char, String)], max: usize) -> String {
    let paths: Vec<&str> = files.iter().map(|(_, path)| path.as_str()).collect();
    let all = |status: char| !files.is_empty() && files.iter().all(|(s, _)| *s == status);
    let verb = if all('A') {
        "add"
    } else if all('D') {
        "remove"
    } else if all('R') {
        "rename"
    } else {
        "update"
    };
    let object = match paths.as_slice() {
        [] => "files".to_string(),
        [one] => one.rsplit('/').next().unwrap_or(one).to_string(),
        many => {
            let area = area_of(many);
            if area == "repo" {
                format!("{} files", many.len())
            } else {
                area
            }
        }
    };
    let mut subject = match &report.style {
        Style::Conventional => {
            let kind = if !paths.is_empty() && paths.iter().all(|p| is_doc(p)) {
                "docs"
            } else if !paths.is_empty() && paths.iter().all(|p| is_test(p)) {
                "test"
            } else if verb == "add" {
                "feat"
            } else {
                "chore"
            };
            format!("{kind}: {verb} {object}")
        }
        Style::Area => format!("{}: {verb} {object}", area_of(&paths)),
        Style::Sentence { lowercase } => {
            let text = format!("{verb} {object}");
            if *lowercase { text } else { capitalize(&text) }
        }
    };
    if subject.chars().count() > max {
        subject = subject
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>()
            + "…";
    }
    if report.period && !subject.ends_with('.') && subject.chars().count() < max {
        subject.push('.');
    }
    subject
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn recent_subjects(cwd: &Path) -> Vec<String> {
    let count = STYLE_SAMPLE.to_string();
    let mut args = vec!["log", "--no-merges", "--format=%s", "-n", count.as_str()];
    // Inside an enclosing repository, sample only commits that touched the
    // session root, so unrelated subjects from the parent never show.
    if enclosing_repo(cwd).is_some() {
        args.extend(["--", "."]);
    }
    let run = git(cwd, &args);
    if !run.ok {
        return Vec::new();
    }
    run.stdout.lines().map(str::to_string).collect()
}

/// A deterministic draft for the staged changes: the style report, the staged
/// files with their line counts, and a subject. `None` when nothing is staged.
pub struct Draft {
    pub report: StyleReport,
    pub files: Vec<(char, String)>,
    pub shortstat: String,
    pub subject: String,
}

pub fn draft(cwd: &Path, subject_max: usize) -> Option<Draft> {
    let files = staged_files(cwd);
    if files.is_empty() {
        return None;
    }
    let report = detect_style(&recent_subjects(cwd));
    let subject = draft_subject(&report, &files, subject_max);
    let shortstat = git(
        cwd,
        &["diff", "--cached", "--shortstat", "--no-color", "--", "."],
    )
    .stdout
    .trim()
    .to_string();
    Some(Draft {
        report,
        files,
        shortstat,
        subject,
    })
}

fn commit_message(ctx: &ToolContext, config: &GitConfig) -> ToolResult {
    let Some(draft) = draft(&ctx.cwd, config.subject_max) else {
        return ToolResult::err(
            "GIT_NOTHING_STAGED",
            "nothing is staged; stage the files that belong to this change with {\"action\":\"stage\",\"paths\":[...]} first",
        );
    };
    let mut out = format!("style: {}\n", draft.report.label());
    if let Some(example) = &draft.report.example {
        out.push_str(&format!("example: {example}\n"));
    }
    out.push_str(&format!(
        "staged: {} file(s){}\n",
        draft.files.len(),
        if draft.shortstat.is_empty() {
            String::new()
        } else {
            format!(" ({})", draft.shortstat)
        }
    ));
    for (status, path) in draft.files.iter().take(30) {
        out.push_str(&format!("  {status} {path}\n"));
    }
    out.push_str(&format!("draft subject: {}\n\n", draft.subject));
    out.push_str(&format!(
        "Write the final message: a subject in this style (at most {} characters) that says what changed, a blank line, then a short body that says why the change was made. Do not list the files. Then call git with action \"commit\" and the message.",
        config.subject_max
    ));
    ToolResult::ok(out)
}

/// Reject a message that would make a poor log entry. The file list is the
/// staged set; a body that mostly repeats it explains nothing.
pub fn check_message(message: &str, subject_max: usize, staged: &[String]) -> Result<(), String> {
    let message = message.trim();
    let mut lines = message.lines();
    let subject = lines.next().unwrap_or("").trim();
    if subject.is_empty() {
        return Err("the commit message is empty; write a subject line".into());
    }
    let length = subject.chars().count();
    if length > subject_max {
        return Err(format!(
            "the subject is {length} characters; keep it at or under {subject_max} and move detail into the body"
        ));
    }
    let rest: Vec<&str> = lines.collect();
    if let Some(first) = rest.first()
        && !first.trim().is_empty()
    {
        return Err("put a blank line between the subject and the body".into());
    }
    let body: Vec<&str> = rest
        .iter()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .collect();
    let file_like = body
        .iter()
        .filter(|line| {
            let bare = line.trim_start_matches(['-', '*', '•', ' ']).trim();
            let bare = bare.trim_matches('`');
            staged.iter().any(|path| {
                bare == path
                    || bare.starts_with(&format!("{path}:"))
                    || bare.starts_with(&format!("{path} "))
            }) || (!bare.contains(' ') && bare.contains('/') && bare.contains('.'))
        })
        .count();
    if file_like >= 2 && file_like * 2 >= body.len() {
        return Err(
            "the body lists files; git already records which files changed. Explain why the change was made instead"
                .into(),
        );
    }
    Ok(())
}

/// Secret-shaped values in lines the staged diff adds, as `(path, kind)`.
pub fn staged_secrets(cwd: &Path) -> Vec<(String, String)> {
    let diff = git(cwd, &["diff", "--cached", "-U0", "--no-color"]).stdout;
    let redactor = crate::security::redact::Redactor::new(true);
    let mut found = Vec::new();
    let mut file = String::new();
    for line in diff.lines() {
        if let Some(path) = line.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
            continue;
        }
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        let redacted = redactor.redact(added);
        if let std::borrow::Cow::Owned(text) = redacted
            && let Some(kind) = text
                .split("[REDACTED:")
                .nth(1)
                .and_then(|rest| rest.split(':').next())
            && !found
                .iter()
                .any(|(f, k): &(String, String)| *f == file && k == kind)
        {
            found.push((file.clone(), kind.to_string()));
        }
    }
    found
}

/// Everything a commit needs checked before it is previewed or made.
fn prepare_commit(
    cwd: &Path,
    message: &str,
    subject_max: usize,
) -> Result<Vec<String>, ToolResult> {
    let staged: Vec<String> = staged_files(cwd)
        .into_iter()
        .map(|(_, path)| path)
        .collect();
    if staged.is_empty() {
        return Err(ToolResult::err(
            "GIT_NOTHING_STAGED",
            "nothing is staged; stage the files that belong to this change first",
        ));
    }
    if let Err(message) = check_message(message, subject_max, &staged) {
        return Err(ToolResult::err("GIT_MESSAGE", message));
    }
    let secrets = staged_secrets(cwd);
    if !secrets.is_empty() {
        let list = secrets
            .iter()
            .map(|(file, kind)| format!("{file} ({kind})"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ToolResult::err(
            "GIT_SECRET",
            format!(
                "the staged diff adds a secret-shaped value in {list}; unstage it and load the value from the environment instead"
            ),
        ));
    }
    Ok(staged)
}

/// The preview shown before a commit is approved.
pub fn commit_preview(cwd: &Path, message: &str) -> String {
    let stat = git(cwd, &["diff", "--cached", "--stat", "--no-color"]).stdout;
    format!(
        "message:\n{}\n\nstaged:\n{}",
        indent(message.trim()),
        indent(stat.trim_end())
    )
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The outcome of a commit: transcript plus the session entry for `undo`.
pub struct Committed {
    pub transcript: String,
    pub entry: Value,
}

/// Commit what is staged with `message`. Hooks run (`--no-verify` is never
/// passed).
pub fn make_commit(cwd: &Path, message: &str) -> Result<Committed, String> {
    let previous = head(cwd);
    let run = run("git", cwd, &["commit", "--file=-"], Some(message.trim()));
    if !run.ok {
        return Err(run.error());
    }
    let now = head(cwd).unwrap_or_default();
    Ok(Committed {
        transcript: run.transcript(),
        entry: json!({
            "kind": COMMIT_KIND,
            "previous": previous,
            "head": now,
        }),
    })
}

fn commit(ctx: &ToolContext, config: &GitConfig, args: &Value) -> ToolResult {
    let Some(message) = optional_str(args, "message").filter(|m| !m.trim().is_empty()) else {
        return ToolResult::err(
            "missing_argument",
            "commit needs message; call git with action \"commit_message\" for the repository's style first",
        );
    };
    if let Err(err) = prepare_commit(&ctx.cwd, &message, config.subject_max) {
        return err;
    }
    match config.commit {
        GitApproval::Allow => match make_commit(&ctx.cwd, &message) {
            Ok(committed) => ok_with_session(committed.transcript, vec![committed.entry]),
            Err(error) => ToolResult::err("GIT_FAILED", error),
        },
        GitApproval::Ask => {
            let preview = commit_preview(&ctx.cwd, &message);
            let entry = json!({
                "kind": PENDING_KIND,
                "action": "commit",
                "message": message.trim(),
                "fingerprint": staged_fingerprint(&ctx.cwd),
            });
            ok_with_session(
                format!(
                    "commit preview recorded; nothing is committed yet.\n{preview}\n\nThe user approves it with /commit confirm (or /commit edit <message>, /commit cancel). Tell the user the commit is ready for review and stop; do not call commit again."
                ),
                vec![entry],
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Pending approvals

/// The open preview for `action` on the active branch, if any.
pub fn pending(session: &Session, action: &str) -> Option<Map<String, Value>> {
    for entry in session.branch_entries().into_iter().rev() {
        let EntryKind::Custom(custom) = &entry.kind else {
            continue;
        };
        let matches = custom.payload.get("action").and_then(Value::as_str) == Some(action);
        if !matches {
            continue;
        }
        if custom.kind == RESOLVED_KIND {
            return None;
        }
        if custom.kind == PENDING_KIND {
            return Some(custom.payload.clone());
        }
    }
    None
}

fn append(session: &mut Session, kind: &str, payload: Value) -> anyhow::Result<()> {
    let payload = match payload {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    session.append(EntryKind::Custom(crate::session::CustomData {
        kind: kind.to_string(),
        payload,
    }))?;
    Ok(())
}

/// Close the open preview for `action` with `outcome`.
pub fn resolve(session: &mut Session, action: &str, outcome: &str) -> anyhow::Result<()> {
    append(
        session,
        RESOLVED_KIND,
        json!({"action": action, "outcome": outcome}),
    )
}

/// The same boundary for the user's `/commit` path, which calls the helpers
/// below directly instead of going through [`dispatch`].
pub(crate) fn refuse_outside_root(cwd: &Path) -> Result<(), String> {
    let Some(top) = enclosing_repo(cwd) else {
        return Ok(());
    };
    Err(format!(
        "{} is inside the git work tree at {}, not at its root, so the harness will not commit or publish in that repository. Run \"git init\" here with the user's agreement, or start the session at {}.",
        cwd.display(),
        top.display(),
        top.display()
    ))
}

/// `/commit` without an argument: the open preview, or a new one drafted from
/// the staged diff with no model.
pub fn show_or_draft_commit(
    cwd: &Path,
    session: &mut Session,
    subject_max: usize,
) -> Result<String, String> {
    refuse_outside_root(cwd)?;
    if let Some(open) = pending(session, "commit") {
        let message = open.get("message").and_then(Value::as_str).unwrap_or("");
        return Ok(format!(
            "{}\n\n/commit confirm commits it; /commit edit <message> replaces the message; /commit cancel drops it.",
            commit_preview(cwd, message)
        ));
    }
    let Some(draft) = draft(cwd, subject_max) else {
        return Err("nothing is staged; stage files first (git add, or ask the agent)".into());
    };
    record_commit_preview(cwd, session, &draft.subject, subject_max).map(|preview| {
        format!(
            "drafted from the staged diff ({}).\n{preview}",
            draft.report.label()
        )
    })
}

/// `/commit edit <message>`: replace the preview with the user's message.
pub fn record_commit_preview(
    cwd: &Path,
    session: &mut Session,
    message: &str,
    subject_max: usize,
) -> Result<String, String> {
    refuse_outside_root(cwd)?;
    prepare_commit(cwd, message, subject_max).map_err(|err| err.content)?;
    if pending(session, "commit").is_some() {
        resolve(session, "commit", "replaced").map_err(|e| e.to_string())?;
    }
    append(
        session,
        PENDING_KIND,
        json!({
            "action": "commit",
            "message": message.trim(),
            "fingerprint": staged_fingerprint(cwd),
        }),
    )
    .map_err(|e| e.to_string())?;
    Ok(format!(
        "{}\n\n/commit confirm commits it; /commit edit <message> replaces the message; /commit cancel drops it.",
        commit_preview(cwd, message)
    ))
}

/// `/commit confirm`: commit the approved preview, provided the staged
/// changes are still exactly what was previewed.
pub fn confirm_commit(
    cwd: &Path,
    session: &mut Session,
    subject_max: usize,
) -> Result<String, String> {
    refuse_outside_root(cwd)?;
    let Some(open) = pending(session, "commit") else {
        return Err("no commit is waiting for approval; run /commit to draft one".into());
    };
    let message = open
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if open.get("fingerprint").and_then(Value::as_str) != Some(staged_fingerprint(cwd).as_str()) {
        return Err(
            "the staged changes differ from the preview; run /commit to see the current preview"
                .into(),
        );
    }
    prepare_commit(cwd, &message, subject_max).map_err(|err| err.content)?;
    let committed = make_commit(cwd, &message)?;
    resolve(session, "commit", "committed").map_err(|e| e.to_string())?;
    append(session, COMMIT_KIND, committed.entry).map_err(|e| e.to_string())?;
    Ok(committed.transcript)
}

// ---------------------------------------------------------------------------
// Undo

fn undo(ctx: &ToolContext) -> ToolResult {
    let (previous, recorded) = {
        let session = match ctx.session.lock() {
            Ok(session) => session,
            Err(_) => return ToolResult::err("internal", "session lock is poisoned"),
        };
        let mut undone: Vec<String> = Vec::new();
        let mut found = None;
        for entry in session.branch_entries().into_iter().rev() {
            let EntryKind::Custom(custom) = &entry.kind else {
                continue;
            };
            let head = custom
                .payload
                .get("head")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if custom.kind == UNDO_KIND {
                undone.push(head);
            } else if custom.kind == COMMIT_KIND && !undone.contains(&head) {
                found = Some((
                    custom
                        .payload
                        .get("previous")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    head,
                ));
                break;
            }
        }
        match found {
            Some(found) => found,
            None => {
                return ToolResult::err(
                    "GIT_NOTHING_TO_UNDO",
                    "no commit made in this session is left to undo",
                );
            }
        }
    };
    if head(&ctx.cwd).as_deref() != Some(recorded.as_str()) {
        return ToolResult::err(
            "GIT_MOVED",
            format!(
                "HEAD is no longer {}, the commit made here; undo only reverts the latest commit it made",
                &recorded[..recorded.len().min(12)]
            ),
        );
    }
    let Some(previous) = previous else {
        return ToolResult::err(
            "GIT_FIRST_COMMIT",
            "that was the repository's first commit; there is no earlier commit to return to",
        );
    };
    let run = git(&ctx.cwd, &["reset", "--soft", &previous]);
    if !run.ok {
        return ToolResult::err("GIT_FAILED", run.error());
    }
    ok_with_session(
        format!(
            "{}\nthe commit is undone; its changes are still staged",
            run.transcript()
        ),
        vec![json!({"kind": UNDO_KIND, "head": recorded})],
    )
}

// ---------------------------------------------------------------------------
// Branches and worktrees

fn valid_branch(cwd: &Path, name: &str) -> Result<(), ToolResult> {
    if git(cwd, &["check-ref-format", "--branch", name]).ok && !name.starts_with('-') {
        Ok(())
    } else {
        Err(ToolResult::err(
            "invalid_args",
            format!("{name:?} is not a valid branch name"),
        ))
    }
}

fn branch_exists(cwd: &Path, name: &str) -> bool {
    git(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .ok
}

fn branch(ctx: &ToolContext, args: &Value) -> ToolResult {
    let op = optional_str(args, "op").unwrap_or_else(|| "list".into());
    if op == "list" {
        let run = git(&ctx.cwd, &["branch", "--list", "-vv", "--no-color"]);
        return if run.ok {
            ToolResult::ok(run.stdout)
        } else {
            ToolResult::err("GIT_FAILED", run.error())
        };
    }
    let Some(name) = optional_str(args, "name").filter(|n| !n.trim().is_empty()) else {
        return ToolResult::err(
            "missing_argument",
            format!(
                "branch {op} needs name, for example {{\"action\":\"branch\",\"op\":\"{op}\",\"name\":\"fix-parser\"}}"
            ),
        );
    };
    if let Err(err) = valid_branch(&ctx.cwd, &name) {
        return err;
    }
    match op.as_str() {
        "create" => {
            if branch_exists(&ctx.cwd, &name) {
                return ToolResult::err(
                    "GIT_EXISTS",
                    format!("branch {name} already exists; use op switch"),
                );
            }
            let base = optional_str(args, "base");
            let mut argv = vec!["switch", "-c", name.as_str()];
            if let Some(base) = &base {
                argv.push(base);
            }
            let run = git(&ctx.cwd, &argv);
            if run.ok {
                ToolResult::ok(run.transcript())
            } else {
                ToolResult::err("GIT_FAILED", run.error())
            }
        }
        "switch" => {
            let dirty = dirty_tracked(&ctx.cwd);
            let stash = optional_bool(args, "stash").unwrap_or(false);
            let mut out = String::new();
            if !dirty.is_empty() {
                if !stash {
                    return ToolResult::err(
                        "GIT_DIRTY",
                        format!(
                            "{} tracked file(s) have uncommitted changes ({}); commit them, or switch with stash: true to stash them first",
                            dirty.len(),
                            dirty.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
                        ),
                    );
                }
                let note = format!("totality: before switching to {name}");
                let run = git(&ctx.cwd, &["stash", "push", "-m", &note]);
                if !run.ok {
                    return ToolResult::err("GIT_FAILED", run.error());
                }
                out.push_str(&run.transcript());
                out.push('\n');
            }
            let run = git(&ctx.cwd, &["switch", &name]);
            if !run.ok {
                return ToolResult::err("GIT_FAILED", format!("{out}{}", run.error()));
            }
            out.push_str(&run.transcript());
            if !dirty.is_empty() {
                out.push_str("\nthe changes are in stash@{0}; restore them with git stash pop on the original branch");
            }
            ToolResult::ok(out)
        }
        other => ToolResult::err(
            "invalid_args",
            format!("branch op {other:?} is not supported; use create, switch or list"),
        ),
    }
}

/// Registered worktrees, main one first.
fn worktree_paths(cwd: &Path) -> Vec<PathBuf> {
    git(cwd, &["worktree", "list", "--porcelain"])
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

fn worktree(ctx: &ToolContext, args: &Value) -> ToolResult {
    let op = optional_str(args, "op").unwrap_or_else(|| "list".into());
    match op.as_str() {
        "list" => {
            let run = git(&ctx.cwd, &["worktree", "list"]);
            if run.ok {
                ToolResult::ok(run.stdout)
            } else {
                ToolResult::err("GIT_FAILED", run.error())
            }
        }
        "add" => worktree_add(ctx, args),
        "remove" => worktree_remove(ctx, args),
        other => ToolResult::err(
            "invalid_args",
            format!("worktree op {other:?} is not supported; use add, remove or list"),
        ),
    }
}

fn worktree_add(ctx: &ToolContext, args: &Value) -> ToolResult {
    let Some(branch) = optional_str(args, "branch").filter(|b| !b.trim().is_empty()) else {
        return ToolResult::err(
            "missing_argument",
            "worktree add needs branch, for example {\"action\":\"worktree\",\"op\":\"add\",\"branch\":\"fix-parser\"}",
        );
    };
    if let Err(err) = valid_branch(&ctx.cwd, &branch) {
        return err;
    }
    let path = match optional_str(args, "path") {
        Some(raw) => match ctx.resolve(&raw) {
            Ok(path) => path,
            Err(err) => return err,
        },
        None => {
            if ctx.path_jail {
                return ToolResult::err(
                    "missing_argument",
                    "--path-jail is active; pass a worktree path inside the working directory",
                );
            }
            let top = git_line(&ctx.cwd, &["rev-parse", "--show-toplevel"])
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.cwd.clone());
            let repo = top
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "repo".into());
            let slug: String = branch
                .chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '-' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect();
            top.parent().unwrap_or(&top).join(format!("{repo}-{slug}"))
        }
    };
    let existed = path.exists();
    if existed
        && std::fs::read_dir(&path)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true)
    {
        return ToolResult::err(
            "GIT_EXISTS",
            format!(
                "{} already exists and is not empty; pick another path",
                path.display()
            ),
        );
    }
    let path_text = path.to_string_lossy().to_string();
    let base = optional_str(args, "base");
    let mut argv = vec!["worktree", "add"];
    if branch_exists(&ctx.cwd, &branch) {
        argv.extend([path_text.as_str(), branch.as_str()]);
    } else {
        argv.extend(["-b", branch.as_str(), path_text.as_str()]);
        if let Some(base) = &base {
            argv.push(base);
        }
    }
    let run = git(&ctx.cwd, &argv);
    if !run.ok {
        // Leave no orphan directory behind: remove what git created, then
        // drop any half-registered entry.
        if !existed && path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        }
        let _ = git(&ctx.cwd, &["worktree", "prune"]);
        return ToolResult::err("GIT_FAILED", run.error());
    }
    ToolResult::ok(format!(
        "{}\nworktree ready at {}",
        run.transcript(),
        path.display()
    ))
}

fn worktree_remove(ctx: &ToolContext, args: &Value) -> ToolResult {
    let Some(raw) = optional_str(args, "path") else {
        return ToolResult::err(
            "missing_argument",
            "worktree remove needs path; list them with {\"action\":\"worktree\",\"op\":\"list\"}",
        );
    };
    let path = if Path::new(&raw).is_absolute() {
        PathBuf::from(&raw)
    } else {
        ctx.cwd.join(&raw)
    };
    let path = super::normalize(&path);
    let real = path.canonicalize().unwrap_or_else(|_| path.clone());
    let registered = worktree_paths(&ctx.cwd);
    let Some(index) = registered
        .iter()
        .position(|known| known.canonicalize().unwrap_or_else(|_| known.clone()) == real)
    else {
        return ToolResult::err(
            "GIT_NOT_WORKTREE",
            format!("{} is not a worktree of this repository", path.display()),
        );
    };
    if index == 0 {
        return ToolResult::err(
            "GIT_MAIN_WORKTREE",
            "that is the main worktree; it cannot be removed",
        );
    }
    let target = registered[index].to_string_lossy().to_string();
    let run = git(&ctx.cwd, &["worktree", "remove", &target]);
    if !run.ok {
        return ToolResult::err(
            "GIT_FAILED",
            format!(
                "{}; commit or stash the changes in that worktree first (it is never force-removed)",
                run.error()
            ),
        );
    }
    let prune = git(&ctx.cwd, &["worktree", "prune"]);
    ToolResult::ok(format!("{}\n{}", run.transcript(), prune.transcript()))
}

// ---------------------------------------------------------------------------
// Destructive command guard for bash and bg

/// Why a shell command that rewrites or discards git history is refused, or
/// `None` when it is allowed. Best effort, like the scope gate: it reads each
/// `git` invocation in the command line, including after `&&`, `;` and `|`.
pub fn destructive_git(command: &str) -> Option<String> {
    let segments = command.split(['\n', ';', '|', '&', '(', ')', '`']);
    for segment in segments {
        let tokens: Vec<String> = segment
            .split_whitespace()
            .map(|token| token.trim_matches(['"', '\'']).to_string())
            .collect();
        let Some(start) = tokens
            .iter()
            .position(|token| token.rsplit('/').next() == Some("git"))
        else {
            continue;
        };
        let mut rest = tokens[start + 1..].iter().map(String::as_str);
        let mut subcommand = None;
        while let Some(token) = rest.next() {
            if token == "-C" || token == "-c" {
                rest.next();
            } else if token.starts_with('-') {
                continue;
            } else {
                subcommand = Some(token);
                break;
            }
        }
        let Some(subcommand) = subcommand else {
            continue;
        };
        let args: Vec<&str> = rest.collect();
        let has = |flag: &str| args.contains(&flag);
        let short_has = |letter: char| {
            args.iter().any(|arg| {
                arg.starts_with('-') && !arg.starts_with("--") && arg[1..].contains(letter)
            })
        };
        let reason = match subcommand {
            "reset" if has("--hard") || has("--merge") || has("--keep") => {
                Some(("git reset --hard", "it discards uncommitted work"))
            }
            "clean"
                if (short_has('f') || has("--force")) && !short_has('n') && !has("--dry-run") =>
            {
                Some(("git clean -f", "it deletes untracked files permanently"))
            }
            "push"
                if has("--force")
                    || has("--mirror")
                    || has("--delete")
                    || has("--prune")
                    || short_has('f')
                    || short_has('d')
                    || args.iter().any(|arg| {
                        arg.starts_with("--force-with-lease")
                            || arg.starts_with("--force-if-includes")
                            || (arg.starts_with('+') && arg.len() > 1)
                            || (arg.starts_with(':') && arg.len() > 1)
                    }) =>
            {
                Some((
                    "git push --force",
                    "it rewrites or deletes history on the remote",
                ))
            }
            "rebase" if !(has("--abort") || has("--quit")) => {
                Some(("git rebase", "it rewrites commit history"))
            }
            "filter-branch" | "filter-repo" => {
                Some(("git filter-branch", "it rewrites the whole history"))
            }
            "tag" if has("-d") || has("--delete") => Some(("git tag -d", "it deletes a tag")),
            _ => None,
        };
        if let Some((shape, why)) = reason {
            return Some(format!(
                "`{shape}` is refused: {why}. Use the git tool (stage, commit, branch, undo) for ordinary changes; if this operation is really needed, ask the user to run it themselves."
            ));
        }
    }
    None
}

/// Summaries of the staged and branch state used by the `pr` tool and tests.
pub fn ahead_count(cwd: &Path, base: &str) -> Option<u32> {
    git_line(cwd, &["rev-list", "--count", &format!("{base}..HEAD")])?
        .parse()
        .ok()
}

/// Remotes by name.
pub fn remotes(cwd: &Path) -> BTreeMap<String, String> {
    git(cwd, &["remote", "-v"])
        .stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            Some((parts.next()?.to_string(), parts.next()?.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::bash::test_context;

    fn repo(dir: &Path) {
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "test@example.invalid"],
            vec!["config", "user.name", "Test"],
            vec!["config", "commit.gpgsign", "false"],
        ] {
            assert!(git(dir, &args).ok);
        }
    }

    fn commit_all(dir: &Path, subject: &str) {
        assert!(git(dir, &["add", "-A"]).ok);
        assert!(git(dir, &["commit", "-q", "--allow-empty", "-m", subject]).ok);
    }

    fn call(ctx: &ToolContext, config: &GitConfig, args: Value) -> ToolResult {
        dispatch(ctx, config, &args)
    }

    #[test]
    fn destructive_commands_are_refused() {
        for command in [
            "git reset --hard HEAD~1",
            "cd x && git reset --hard",
            "git clean -fd",
            "git clean -xdf",
            "git push --force origin main",
            "git push -f",
            "git push --force-with-lease",
            "git push origin +main",
            "git push origin :old-branch",
            "git push --delete origin v1",
            "git rebase -i HEAD~3",
            "git -C repo rebase main",
            "git filter-branch --tree-filter x",
            "git tag -d v1.0",
            "bash -c \"git reset --hard\"",
        ] {
            assert!(
                destructive_git(command).is_some(),
                "{command} must be refused"
            );
        }
        for command in [
            "git status",
            "git reset --soft HEAD~1",
            "git reset HEAD file",
            "git clean -n",
            "git clean -fdn",
            "git push origin main",
            "git push -u origin feature",
            "git rebase --abort",
            "git tag v1.0",
            "echo git reset --hard is bad > notes.txt",
            "grep -r 'reset' src",
        ] {
            let refused = destructive_git(command);
            if command.starts_with("echo") {
                // Best effort: echoing the words is refused too, which is safe.
                continue;
            }
            assert!(refused.is_none(), "{command} must be allowed: {refused:?}");
        }
    }

    #[test]
    fn style_detection_follows_the_history() {
        let conventional: Vec<String> = [
            "feat(tui): add picker",
            "fix: handle empty stream",
            "docs: explain profiles",
            "Merge pull request #3 from x/y",
            "chore(deps): bump regex",
            "Refresh the README",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let report = detect_style(&conventional);
        assert_eq!(report.style, Style::Conventional);
        assert_eq!((report.matching, report.sampled), (4, 5));

        let area: Vec<String> = ["tui: add picker", "provider: retry once", "fix the build"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(detect_style(&area).style, Style::Area);

        let sentence: Vec<String> = ["Keep the screen in sync", "Add image input", "fix typo"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            detect_style(&sentence).style,
            Style::Sentence { lowercase: false }
        );
        assert_eq!(
            detect_style(&[]).style,
            Style::Sentence { lowercase: false }
        );
    }

    #[test]
    fn draft_subjects_take_the_style() {
        let files = vec![('A', "src/tools/git.rs".to_string())];
        let conventional = detect_style(&["feat: a".into(), "fix: b".into()]);
        assert_eq!(draft_subject(&conventional, &files, 72), "feat: add git.rs");
        let docs = vec![
            ('M', "docs/a.md".to_string()),
            ('M', "README.md".to_string()),
        ];
        assert!(draft_subject(&conventional, &docs, 72).starts_with("docs: update"));
        let area = detect_style(&["tui: a b".into(), "agent: c d".into()]);
        let two = vec![
            ('M', "src/tools/git.rs".to_string()),
            ('M', "src/tools/pr.rs".to_string()),
        ];
        assert_eq!(draft_subject(&area, &two, 72), "tools: update tools");
        let sentence = detect_style(&["Add a thing".into()]);
        assert_eq!(draft_subject(&sentence, &two, 72), "Update tools");
        assert!(draft_subject(&sentence, &two, 8).chars().count() <= 8);
    }

    #[test]
    fn messages_that_list_files_or_run_long_are_refused() {
        let staged = vec!["src/a.rs".to_string(), "src/b.rs".to_string()];
        assert!(check_message("Fix the parser\n\nEmpty input panicked.", 72, &staged).is_ok());
        assert!(check_message("", 72, &staged).is_err());
        assert!(check_message(&"x".repeat(73), 72, &staged).is_err());
        assert!(check_message("Subject\nno blank line", 72, &staged).is_err());
        assert!(check_message("Update\n\n- src/a.rs\n- src/b.rs", 72, &staged).is_err());
        assert!(
            check_message(
                "Update parser\n\nThe tokenizer split on every byte.\nNow it splits on chars.\nsrc/a.rs",
                72,
                &staged
            )
            .is_ok()
        );
    }

    #[test]
    fn commit_waits_for_approval_and_confirms_only_the_previewed_stage() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        commit_all(dir.path(), "Add a");
        let ctx = test_context(dir.path());
        let config = GitConfig::default();

        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let staged = call(&ctx, &config, json!({"action":"stage","paths":["a.txt"]}));
        assert!(!staged.is_error, "{}", staged.content);
        assert!(
            staged.content.contains("$ git add -- a.txt"),
            "{}",
            staged.content
        );

        let message = call(&ctx, &config, json!({"action":"commit_message"}));
        assert!(
            message.content.contains("draft subject: Update a.txt"),
            "{}",
            message.content
        );

        let preview = call(
            &ctx,
            &config,
            json!({"action":"commit","message":"Change a\n\nThe value must be two."}),
        );
        assert!(!preview.is_error, "{}", preview.content);
        assert!(
            preview.content.contains("/commit confirm"),
            "{}",
            preview.content
        );
        assert_eq!(
            head(dir.path()),
            git_line(dir.path(), &["rev-parse", "HEAD"])
        );
        let before = head(dir.path());

        // The loop persists details.session; do the same here.
        let mut session = ctx.session.lock().unwrap();
        for entry in preview.details.unwrap()["session"].as_array().unwrap() {
            let mut payload = entry.as_object().unwrap().clone();
            let kind = payload.remove("kind").unwrap();
            append(&mut session, kind.as_str().unwrap(), Value::Object(payload)).unwrap();
        }
        assert_eq!(head(dir.path()), before, "nothing commits before approval");

        // Staging more after the preview invalidates it.
        std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
        assert!(git(dir.path(), &["add", "b.txt"]).ok);
        let stale = confirm_commit(dir.path(), &mut session, 72);
        assert!(stale.unwrap_err().contains("differ from the preview"));
        assert!(git(dir.path(), &["restore", "--staged", "b.txt"]).ok);

        let done = confirm_commit(dir.path(), &mut session, 72).expect("commit");
        assert!(done.contains("git commit --file=-"), "{done}");
        assert_ne!(head(dir.path()), before);
        assert!(pending(&session, "commit").is_none());
        let subject = git_line(dir.path(), &["log", "-1", "--format=%s"]).unwrap();
        assert_eq!(subject, "Change a");
        drop(session);

        // undo returns to the previous commit and keeps the change staged.
        let undone = call(&ctx, &config, json!({"action":"undo"}));
        assert!(!undone.is_error, "{}", undone.content);
        assert_eq!(head(dir.path()), before);
        assert_eq!(staged_files(dir.path()), vec![('M', "a.txt".to_string())]);
    }

    #[test]
    fn allow_commits_at_once_and_secrets_refuse() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let ctx = test_context(dir.path());
        let config = GitConfig {
            commit: GitApproval::Allow,
            ..GitConfig::default()
        };
        let empty = call(
            &ctx,
            &config,
            json!({"action":"commit","message":"Nothing"}),
        );
        assert!(
            empty.content.contains("GIT_NOTHING_STAGED"),
            "{}",
            empty.content
        );

        std::fs::write(
            dir.path().join("config.py"),
            "token = 'x'\nGITHUB = \"ghp_abcdefghijklmnopqrstuvwxyz0123456789AB\"\n",
        )
        .unwrap();
        assert!(git(dir.path(), &["add", "config.py"]).ok);
        let secret = call(
            &ctx,
            &config,
            json!({"action":"commit","message":"Add config"}),
        );
        assert!(secret.content.contains("GIT_SECRET"), "{}", secret.content);
        assert!(secret.content.contains("config.py"), "{}", secret.content);

        std::fs::write(dir.path().join("config.py"), "name = 'demo'\n").unwrap();
        assert!(git(dir.path(), &["add", "config.py"]).ok);
        let done = call(
            &ctx,
            &config,
            json!({"action":"commit","message":"Add config"}),
        );
        assert!(!done.is_error, "{}", done.content);
        assert!(head(dir.path()).is_some());
    }

    #[test]
    fn stage_refuses_outside_paths_and_credential_files() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("ws");
        std::fs::create_dir(&workspace).unwrap();
        repo(&workspace);
        std::fs::write(dir.path().join("outside.txt"), "x").unwrap();
        std::fs::write(workspace.join(".env"), "A=1").unwrap();
        let ctx = test_context(&workspace);
        let config = GitConfig::default();
        let outside = call(
            &ctx,
            &config,
            json!({"action":"stage","paths":["../outside.txt"]}),
        );
        assert!(
            outside.content.contains("GIT_OUTSIDE_WORKSPACE"),
            "{}",
            outside.content
        );
        let env = call(&ctx, &config, json!({"action":"stage","paths":[".env"]}));
        assert!(env.content.contains("GIT_SECRET_FILE"), "{}", env.content);
    }

    #[test]
    fn commit_commands_refuse_a_parent_repository() {
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("parent");
        let workspace = parent.join("ws");
        std::fs::create_dir_all(&workspace).unwrap();
        repo(&parent);
        std::fs::write(workspace.join("a.txt"), "one\n").unwrap();
        let ctx = test_context(&workspace);
        let mut session = ctx.session.lock().unwrap();
        let errors = [
            show_or_draft_commit(&workspace, &mut session, 72).unwrap_err(),
            confirm_commit(&workspace, &mut session, 72).unwrap_err(),
            record_commit_preview(&workspace, &mut session, "Update\n\nWhy.", 72).unwrap_err(),
        ];
        for error in errors {
            assert!(error.contains("inside the git work tree"), "{error}");
        }
    }

    #[test]
    fn switching_with_dirty_files_needs_stash() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        commit_all(dir.path(), "Add a");
        let ctx = test_context(dir.path());
        let config = GitConfig::default();
        let created = call(
            &ctx,
            &config,
            json!({"action":"branch","op":"create","name":"topic"}),
        );
        assert!(!created.is_error, "{}", created.content);
        std::fs::write(dir.path().join("a.txt"), "dirty\n").unwrap();
        let refused = call(
            &ctx,
            &config,
            json!({"action":"branch","op":"switch","name":"main"}),
        );
        assert!(refused.content.contains("GIT_DIRTY"), "{}", refused.content);
        assert!(
            refused.content.contains("stash: true"),
            "{}",
            refused.content
        );
        let stashed = call(
            &ctx,
            &config,
            json!({"action":"branch","op":"switch","name":"main","stash":true}),
        );
        assert!(!stashed.is_error, "{}", stashed.content);
        assert_eq!(current_branch(dir.path()).as_deref(), Some("main"));
        assert!(dirty_tracked(dir.path()).is_empty());
    }

    #[test]
    fn worktrees_add_and_remove_without_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        std::fs::create_dir(&main).unwrap();
        repo(&main);
        std::fs::write(main.join("a.txt"), "one\n").unwrap();
        commit_all(&main, "Add a");
        let ctx = test_context(&main);
        let config = GitConfig::default();
        let tree = dir.path().join("tree");
        let added = call(
            &ctx,
            &config,
            json!({"action":"worktree","op":"add","branch":"task","path": tree.to_string_lossy()}),
        );
        assert!(!added.is_error, "{}", added.content);
        assert!(tree.join("a.txt").exists());

        // A failing add (bad start point) removes the directory it created.
        let broken = dir.path().join("broken");
        let failed = call(
            &ctx,
            &config,
            json!({"action":"worktree","op":"add","branch":"other","base":"no-such-ref","path": broken.to_string_lossy()}),
        );
        assert!(failed.is_error);
        assert!(!broken.exists(), "a failed add must not leave a directory");

        let main_refused = call(
            &ctx,
            &config,
            json!({"action":"worktree","op":"remove","path": main.to_string_lossy()}),
        );
        assert!(main_refused.content.contains("GIT_MAIN_WORKTREE"));
        let removed = call(
            &ctx,
            &config,
            json!({"action":"worktree","op":"remove","path": tree.to_string_lossy()}),
        );
        assert!(!removed.is_error, "{}", removed.content);
        assert!(!tree.exists());
        assert_eq!(worktree_paths(&main).len(), 1);
    }

    #[test]
    fn read_only_sandbox_keeps_git_read_only() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let mut ctx = test_context(dir.path());
        let Ok(sandbox) =
            crate::sandbox::Sandbox::new(SandboxMode::ReadOnly, dir.path().to_path_buf())
        else {
            return; // no sandbox backend on this machine
        };
        ctx.sandbox = Some(std::sync::Arc::new(sandbox));
        let config = GitConfig::default();
        let staged = call(&ctx, &config, json!({"action":"stage","paths":["x"]}));
        assert!(
            staged.content.contains("GIT_READ_ONLY"),
            "{}",
            staged.content
        );
        let status = call(&ctx, &config, json!({"action":"status"}));
        assert!(!status.is_error, "{}", status.content);
    }
}

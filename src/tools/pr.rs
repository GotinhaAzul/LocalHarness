//! `pr`: GitHub pull requests through `gh`, behind a preview gate.
//!
//! `draft` builds the title and body from the branch's commits, its diff
//! stat, the session's last verification and the linked issue, then records
//! a preview the user approves with `/pr confirm` (or creates the draft at
//! once under `[git] pr = "allow"`). It refuses on the default branch, with
//! uncommitted changes, with no commits ahead of the base, or with no passing
//! verification unless an override reason is given. `checks` reports
//! failing, pending and passing checks separately; `review` reads a PR's
//! description, comments and diff; `comment` posts behind the same gate.
//! There is no merge action, and nothing is pushed to a remote the
//! repository does not already have.

use std::path::Path;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Map, Value, json};

use super::git::{self, PENDING_KIND, Run};
use super::{Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_str};
use crate::config::{GitApproval, GitConfig};
use crate::session::{EntryKind, Session};

/// Pending-approval action name shared by drafts and comments.
pub const ACTION: &str = "pr";

pub struct PrTool {
    config: GitConfig,
}

impl PrTool {
    pub fn new(config: GitConfig) -> Self {
        Self { config }
    }
}

impl Default for PrTool {
    fn default() -> Self {
        Self::new(GitConfig::default())
    }
}

#[async_trait]
impl Tool for PrTool {
    fn name(&self) -> &'static str {
        "pr"
    }

    fn description(&self) -> &'static str {
        "GitHub PRs via gh: draft (needs commits, clean tree, passing verify; user approves), checks (fail/pending/pass), review (read-only), comment (user approves). Never merges."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "action": {"type": "string", "enum": ["draft", "checks", "review", "comment"]},
                "title": {"type": "string", "description": "draft: title."},
                "summary": {"type": "string", "description": "draft: what and why, risks."},
                "issue": {"type": "string", "description": "draft: issue number or URL."},
                "base": {"type": "string", "description": "draft: base (default branch)."},
                "override_verify": {"type": "string", "description": "draft: reason to skip verify."},
                "number": {"type": "integer", "minimum": 1, "description": "PR number (default: this branch)."},
                "body": {"type": "string", "description": "comment: text."},
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
            .unwrap_or_else(|error| ToolResult::err("internal", format!("pr task failed: {error}")))
    }
}

fn dispatch(ctx: &ToolContext, config: &GitConfig, args: &Value) -> ToolResult {
    if !git::is_repo(&ctx.cwd) {
        return ToolResult::err(
            "GIT_NO_REPO",
            format!("{} is not inside a git repository", ctx.cwd.display()),
        );
    }
    match optional_str(args, "action").unwrap_or_default().as_str() {
        "draft" => match git::refuse_outside_root(&ctx.cwd) {
            Ok(()) => draft(ctx, config, args),
            Err(message) => ToolResult::err("GIT_REPO_OUTSIDE_ROOT", message),
        },
        "checks" => checks(ctx, args),
        "review" => review(ctx, args),
        "comment" => comment(ctx, config, args),
        other => ToolResult::err(
            "invalid_args",
            format!("unknown pr action {other:?}; use draft, checks, review or comment"),
        ),
    }
}

fn gh(cwd: &Path, args: &[&str], input: Option<&str>) -> Run {
    git::run("gh", cwd, args, input)
}

fn number_arg(args: &Value) -> Option<String> {
    args.get("number").and_then(|value| match value {
        Value::Number(number) => number.as_u64().map(|n| n.to_string()),
        Value::String(text) => {
            let text = text.trim().trim_start_matches('#');
            text.parse::<u64>().ok().map(|n| n.to_string())
        }
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Draft

/// The remote's default branch, else `main` or `master` when it exists.
pub fn default_branch(cwd: &Path, remote: &str) -> Option<String> {
    let symbolic = git::git(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            &format!("refs/remotes/{remote}/HEAD"),
        ],
    );
    if symbolic.ok {
        let name = symbolic.stdout.trim();
        return Some(
            name.strip_prefix(&format!("{remote}/"))
                .unwrap_or(name)
                .to_string(),
        );
    }
    ["main", "master"]
        .into_iter()
        .find(|name| {
            git::git(
                cwd,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("refs/heads/{name}"),
                ],
            )
            .ok
        })
        .map(str::to_string)
}

/// The remote a branch pushes to: its configured upstream remote, else
/// `origin`, else the only remote.
fn push_remote(cwd: &Path, branch: &str) -> Option<String> {
    let remotes = git::remotes(cwd);
    let configured = git::git(
        cwd,
        &["config", "--get", &format!("branch.{branch}.remote")],
    );
    let configured = configured.stdout.trim();
    if !configured.is_empty() && remotes.contains_key(configured) {
        return Some(configured.to_string());
    }
    if remotes.contains_key("origin") {
        return Some("origin".into());
    }
    (remotes.len() == 1)
        .then(|| remotes.keys().next().cloned())
        .flatten()
}

/// What the session knows about verification of the current changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// The last verify after the last file change passed, with its steps.
    Passed(Vec<String>),
    /// The last verify failed at this step.
    Failed(String),
    /// Files changed after the last verify, or the commits differ from the
    /// content it checked.
    Stale,
    /// Verify never ran on this branch.
    Missing,
}

/// The verification state for the commits at HEAD in `cwd`: a passing
/// verify counts only when HEAD holds the content it ran against.
pub fn verification(session: &Session, cwd: &Path) -> Verification {
    let (result, tree) = recorded_verification(session);
    match (result, tree) {
        (Verification::Passed(steps), Some(tree)) if git::head_matches_tree(cwd, &tree) => {
            Verification::Passed(steps)
        }
        (Verification::Passed(_), _) => Verification::Stale,
        (other, _) => other,
    }
}

/// The latest verify on the branch as the session recorded it, with the
/// content tree it ran against.
fn recorded_verification(session: &Session) -> (Verification, Option<String>) {
    let mut result = Verification::Missing;
    let mut tree = None;
    for entry in session.branch_entries() {
        let EntryKind::Custom(custom) = &entry.kind else {
            continue;
        };
        match custom.kind.as_str() {
            "fs-change" if result != Verification::Missing => result = Verification::Stale,
            "verify-result" => {
                if !crate::session::Session::counts_as_project_verify(&custom.payload) {
                    continue;
                }
                let passed = custom.payload.get("passed").and_then(Value::as_bool) == Some(true);
                tree = custom
                    .payload
                    .get("tree")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                result = if passed {
                    let steps = custom
                        .payload
                        .get("steps")
                        .and_then(Value::as_array)
                        .map(|steps| {
                            steps
                                .iter()
                                .filter_map(|step| step.get("name").and_then(Value::as_str))
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    Verification::Passed(steps)
                } else {
                    Verification::Failed(
                        custom
                            .payload
                            .get("failed_step")
                            .and_then(Value::as_str)
                            .unwrap_or("a step")
                            .to_string(),
                    )
                };
            }
            _ => {}
        }
    }
    (result, tree)
}

static HOME_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:/home|/Users)/[^/\s]+").expect("home path pattern"));

/// Strip what must never leave the machine: secret-shaped values, the
/// absolute workspace path, and home directories.
pub fn sanitize(text: &str, cwd: &Path) -> String {
    let mut out = crate::security::redact::Redactor::new(true).redact_owned(text);
    let workspace = cwd.to_string_lossy().to_string();
    if workspace.len() > 1 {
        out = out.replace(&workspace, ".");
    }
    if let Some(home) = dirs::home_dir() {
        let home = home.to_string_lossy().to_string();
        if home.len() > 1 {
            out = out.replace(&home, "~");
        }
    }
    HOME_PATH.replace_all(&out, "~").into_owned()
}

/// `Closes #12` for a number or this repository's issue URL; the URL itself
/// otherwise.
pub fn issue_line(issue: &str) -> Option<String> {
    let issue = issue.trim();
    if issue.is_empty() {
        return None;
    }
    let bare = issue.trim_start_matches('#');
    if bare.chars().all(|c| c.is_ascii_digit()) {
        return Some(format!("Closes #{bare}"));
    }
    if let Some((_, tail)) = issue.rsplit_once("/issues/")
        && tail.chars().all(|c| c.is_ascii_digit())
        && !tail.is_empty()
    {
        return Some(format!("Closes {issue}"));
    }
    Some(format!("Related: {issue}"))
}

/// Everything the PR body is built from.
pub struct BodyParts<'a> {
    pub summary: &'a str,
    pub issue: Option<&'a str>,
    pub commits: &'a [String],
    pub diffstat: &'a str,
    pub verification: &'a Verification,
    pub override_verify: Option<&'a str>,
}

pub fn build_body(parts: &BodyParts<'_>, cwd: &Path) -> String {
    let mut body = String::from("## Summary\n\n");
    body.push_str(parts.summary.trim());
    body.push('\n');
    if let Some(line) = parts.issue.and_then(issue_line) {
        body.push('\n');
        body.push_str(&line);
        body.push('\n');
    }
    body.push_str("\n## Commits\n\n");
    for subject in parts.commits {
        body.push_str(&format!("- {subject}\n"));
    }
    if !parts.diffstat.trim().is_empty() {
        body.push_str("\n## Diff stat\n\n```\n");
        body.push_str(parts.diffstat.trim_end());
        body.push_str("\n```\n");
    }
    body.push_str("\n## Verification\n\n");
    match (parts.verification, parts.override_verify) {
        (Verification::Passed(steps), _) if !steps.is_empty() => {
            body.push_str(&format!("- verify passed: {}\n", steps.join(", ")));
        }
        (Verification::Passed(_), _) => body.push_str("- verify passed\n"),
        (_, Some(reason)) => {
            body.push_str(&format!("- not verified: {}\n", reason.trim()));
        }
        (Verification::Failed(step), None) => {
            body.push_str(&format!("- verify failed at {step}\n"));
        }
        (Verification::Stale | Verification::Missing, None) => {
            body.push_str("- not verified\n");
        }
    }
    sanitize(&body, cwd)
}

fn draft(ctx: &ToolContext, config: &GitConfig, args: &Value) -> ToolResult {
    let cwd = &ctx.cwd;
    let title = optional_str(args, "title").unwrap_or_default();
    let summary = optional_str(args, "summary").unwrap_or_default();
    if title.trim().is_empty() || summary.trim().is_empty() {
        return ToolResult::err(
            "missing_argument",
            "draft needs title and summary (what changed, why, and residual risk)",
        );
    }
    let Some(branch) = git::current_branch(cwd) else {
        return ToolResult::err(
            "PR_DETACHED",
            "HEAD is detached; create a branch for this work with git branch op create",
        );
    };
    let Some(remote) = push_remote(cwd, &branch) else {
        return ToolResult::err(
            "PR_NO_REMOTE",
            "the repository has no remote configured; the harness does not add one",
        );
    };
    let base = match optional_str(args, "base").filter(|b| !b.trim().is_empty()) {
        Some(base) => base,
        None => match default_branch(cwd, &remote) {
            Some(base) => base,
            None => {
                return ToolResult::err(
                    "missing_argument",
                    "cannot tell the default branch; pass base",
                );
            }
        },
    };
    if branch == base {
        return ToolResult::err(
            "PR_DEFAULT_BRANCH",
            format!(
                "{branch} is the base branch; create a topic branch (git branch op create) and commit there first"
            ),
        );
    }
    let dirty = git::dirty_tracked(cwd);
    if !dirty.is_empty() {
        return ToolResult::err(
            "PR_DIRTY",
            format!(
                "{} tracked file(s) have uncommitted changes ({}); commit or stash them first",
                dirty.len(),
                dirty.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            ),
        );
    }
    let remote_base = format!("{remote}/{base}");
    let base_ref = if git::git(
        cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/remotes/{remote_base}"),
        ],
    )
    .ok
    {
        remote_base
    } else {
        base.clone()
    };
    match git::ahead_count(cwd, &base_ref) {
        Some(0) => {
            return ToolResult::err(
                "PR_NO_COMMITS",
                format!("{branch} has no commits ahead of {base_ref}; commit the change first"),
            );
        }
        None => {
            return ToolResult::err(
                "PR_NO_BASE",
                format!("cannot compare {branch} with {base_ref}; fetch it or pass another base"),
            );
        }
        Some(_) => {}
    }
    let verified = match ctx.session.lock() {
        Ok(session) => verification(&session, cwd),
        Err(_) => return ToolResult::err("internal", "session lock is poisoned"),
    };
    let override_verify = optional_str(args, "override_verify").filter(|r| !r.trim().is_empty());
    if !matches!(verified, Verification::Passed(_)) && override_verify.is_none() {
        let state = match &verified {
            Verification::Failed(step) => format!("the last verify failed at {step}"),
            Verification::Stale => "files changed after the last passing verify".into(),
            _ => "verify has not run on this branch".into(),
        };
        return ToolResult::err(
            "PR_UNVERIFIED",
            format!(
                "{state}; run the verify tool until it passes, or pass override_verify with the reason verification does not apply"
            ),
        );
    }
    let commits: Vec<String> = git::git(
        cwd,
        &[
            "log",
            "--reverse",
            "--format=%s",
            &format!("{base_ref}..HEAD"),
        ],
    )
    .stdout
    .lines()
    .map(str::to_string)
    .collect();
    let diffstat = git::git(
        cwd,
        &[
            "diff",
            "--stat",
            "--no-color",
            &format!("{base_ref}...HEAD"),
        ],
    )
    .stdout;
    let issue = optional_str(args, "issue");
    let body = build_body(
        &BodyParts {
            summary: &summary,
            issue: issue.as_deref(),
            commits: &commits,
            diffstat: &diffstat,
            verification: &verified,
            override_verify: override_verify.as_deref(),
        },
        cwd,
    );
    let title = sanitize(title.trim(), cwd);
    let payload = json!({
        "action": ACTION,
        "op": "draft",
        "title": title,
        "body": body,
        "base": base,
        "branch": branch,
        "remote": remote,
        "head": git::head(cwd).unwrap_or_default(),
    });
    match config.pr {
        GitApproval::Allow => match execute(cwd, payload.as_object().expect("object")) {
            Ok(text) => ToolResult::ok(text),
            Err(error) => ToolResult::err("PR_FAILED", error),
        },
        GitApproval::Ask => {
            let mut entry = payload.clone();
            entry["kind"] = json!(PENDING_KIND);
            ToolResult::ok(format!(
                "draft PR preview recorded; nothing is pushed or created yet.\n{}\n\nThe user approves it with /pr confirm (or /pr cancel). Tell the user the PR is ready for review and stop; do not call draft again.",
                preview(payload.as_object().expect("object"))
            ))
            .with_details(json!({ "session": [entry] }))
        }
    }
}

/// The preview of a pending draft or comment.
pub fn preview(payload: &Map<String, Value>) -> String {
    let text = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or("");
    match text("op") {
        "comment" => format!(
            "comment on PR #{}:\n{}",
            text("number"),
            indent(text("body"))
        ),
        _ => format!(
            "draft PR {} -> {} (push to {})\ntitle: {}\n\n{}",
            text("branch"),
            text("base"),
            text("remote"),
            text("title"),
            indent(text("body"))
        ),
    }
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Carry out an approved draft or comment.
pub fn execute(cwd: &Path, payload: &Map<String, Value>) -> Result<String, String> {
    let text = |key: &str| {
        payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    match text("op").as_str() {
        "comment" => {
            let number = text("number");
            let run = gh(
                cwd,
                &["pr", "comment", &number, "--body-file", "-"],
                Some(&text("body")),
            );
            if run.ok {
                Ok(run.transcript())
            } else {
                Err(run.error())
            }
        }
        "draft" => {
            git::refuse_outside_root(cwd)?;
            let branch = text("branch");
            if git::current_branch(cwd).as_deref() != Some(branch.as_str())
                || git::head(cwd).unwrap_or_default() != text("head")
            {
                return Err(
                    "the branch moved since the preview; ask the agent for a new draft".into(),
                );
            }
            // Always name the remote and the one branch: a bare `git push`
            // follows push.default, which can publish other branches too.
            let upstream = git::git(cwd, &["rev-parse", "--abbrev-ref", "@{upstream}"]).ok;
            let remote = text("remote");
            let refspec = format!("refs/heads/{branch}:refs/heads/{branch}");
            let mut push_args = vec!["push"];
            if !upstream {
                push_args.push("-u");
            }
            push_args.extend([remote.as_str(), refspec.as_str()]);
            let push = git::git(cwd, &push_args);
            if !push.ok {
                return Err(push.error());
            }
            let base = text("base");
            let title = text("title");
            let create = gh(
                cwd,
                &[
                    "pr",
                    "create",
                    "--draft",
                    "--base",
                    &base,
                    "--head",
                    &branch,
                    "--title",
                    &title,
                    "--body-file",
                    "-",
                ],
                Some(&text("body")),
            );
            if !create.ok {
                return Err(format!("{}\n{}", push.transcript(), create.error()));
            }
            Ok(format!("{}\n{}", push.transcript(), create.transcript()))
        }
        other => Err(format!("unknown pending pr operation {other:?}")),
    }
}

// ---------------------------------------------------------------------------
// Checks, review, comment

/// Group `gh pr checks --json name,bucket,link` output. Green is claimed only
/// when at least one check ran and none failed or is pending.
pub fn summarize_checks(json_text: &str) -> Result<String, String> {
    let checks: Vec<Value> = serde_json::from_str(json_text)
        .map_err(|error| format!("unreadable gh output: {error}"))?;
    if checks.is_empty() {
        return Ok("no checks reported for this PR; nothing is known to pass".into());
    }
    let mut groups: [(&str, Vec<String>); 4] = [
        ("failing", Vec::new()),
        ("pending", Vec::new()),
        ("passing", Vec::new()),
        ("skipped", Vec::new()),
    ];
    for check in &checks {
        let name = check.get("name").and_then(Value::as_str).unwrap_or("?");
        let link = check.get("link").and_then(Value::as_str).unwrap_or("");
        let bucket = check
            .get("bucket")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let index = match bucket {
            "fail" | "cancel" => 0,
            "pass" => 2,
            "skipping" => 3,
            _ => 1,
        };
        groups[index].1.push(if link.is_empty() {
            name.to_string()
        } else {
            format!("{name} ({link})")
        });
    }
    let verdict = if !groups[0].1.is_empty() {
        "FAILING"
    } else if !groups[1].1.is_empty() {
        "PENDING"
    } else if !groups[2].1.is_empty() {
        "PASSING"
    } else {
        "NO RESULT"
    };
    let mut out = format!(
        "{verdict}: {} failing, {} pending, {} passing, {} skipped",
        groups[0].1.len(),
        groups[1].1.len(),
        groups[2].1.len(),
        groups[3].1.len()
    );
    for (label, names) in &groups {
        if !names.is_empty() {
            out.push_str(&format!("\n{label}:"));
            for name in names {
                out.push_str(&format!("\n  {name}"));
            }
        }
    }
    Ok(out)
}

fn checks(ctx: &ToolContext, args: &Value) -> ToolResult {
    let number = number_arg(args);
    let mut argv = vec!["pr", "checks"];
    if let Some(number) = &number {
        argv.push(number);
    }
    argv.extend(["--json", "name,bucket,link"]);
    // gh exits non-zero for failing (1) and pending (8) checks; the output is
    // still the answer.
    let run = gh(&ctx.cwd, &argv, None);
    if run.stdout.trim().is_empty() {
        let message = run.stderr.trim();
        if message.contains("no checks reported") {
            return ToolResult::ok("no checks reported for this PR; nothing is known to pass");
        }
        return ToolResult::err("PR_FAILED", run.error());
    }
    match summarize_checks(&run.stdout) {
        Ok(text) => ToolResult::ok(text),
        Err(error) => ToolResult::err("PR_FAILED", error),
    }
}

fn review(ctx: &ToolContext, args: &Value) -> ToolResult {
    let number = number_arg(args);
    let mut view = vec!["pr", "view"];
    if let Some(number) = &number {
        view.push(number);
    }
    view.extend([
        "--json",
        "number,title,state,isDraft,author,baseRefName,headRefName,body,comments,reviews,files",
    ]);
    let run = gh(&ctx.cwd, &view, None);
    if !run.ok {
        return ToolResult::err("PR_FAILED", run.error());
    }
    let info: Value = match serde_json::from_str(&run.stdout) {
        Ok(info) => info,
        Err(error) => {
            return ToolResult::err("PR_FAILED", format!("unreadable gh output: {error}"));
        }
    };
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let author = |value: &Value| {
        value
            .get("author")
            .and_then(|author| author.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    let mut out = format!(
        "PR #{} {} [{}{}] {} -> {} by {}\n\n{}\n",
        info.get("number").and_then(Value::as_u64).unwrap_or(0),
        text(&info, "title"),
        text(&info, "state"),
        if info.get("isDraft").and_then(Value::as_bool) == Some(true) {
            ", draft"
        } else {
            ""
        },
        text(&info, "headRefName"),
        text(&info, "baseRefName"),
        author(&info),
        text(&info, "body").trim()
    );
    for (key, label) in [("reviews", "review"), ("comments", "comment")] {
        for item in info
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let body = text(item, "body");
            let state = text(item, "state");
            if body.trim().is_empty() && state.is_empty() {
                continue;
            }
            out.push_str(&format!(
                "\n--- {label} by {}{}\n{}\n",
                author(item),
                if state.is_empty() {
                    String::new()
                } else {
                    format!(" ({state})")
                },
                body.trim()
            ));
        }
    }
    let mut diff = vec!["pr", "diff"];
    if let Some(number) = &number {
        diff.push(number);
    }
    diff.extend(["--color", "never"]);
    let patch = gh(&ctx.cwd, &diff, None);
    if patch.ok {
        out.push_str("\n--- diff\n");
        out.push_str(&patch.stdout);
    } else {
        out.push_str(&format!("\n--- diff unavailable: {}\n", patch.error()));
    }
    ToolResult::ok(out)
}

fn comment(ctx: &ToolContext, config: &GitConfig, args: &Value) -> ToolResult {
    let Some(number) = number_arg(args) else {
        return ToolResult::err("missing_argument", "comment needs number");
    };
    let Some(body) = optional_str(args, "body").filter(|b| !b.trim().is_empty()) else {
        return ToolResult::err("missing_argument", "comment needs body");
    };
    let payload = json!({
        "action": ACTION,
        "op": "comment",
        "number": number,
        "body": sanitize(body.trim(), &ctx.cwd),
    });
    match config.pr {
        GitApproval::Allow => match execute(&ctx.cwd, payload.as_object().expect("object")) {
            Ok(text) => ToolResult::ok(text),
            Err(error) => ToolResult::err("PR_FAILED", error),
        },
        GitApproval::Ask => {
            let mut entry = payload.clone();
            entry["kind"] = json!(PENDING_KIND);
            ToolResult::ok(format!(
                "comment preview recorded; nothing is posted yet.\n{}\n\nThe user approves it with /pr confirm (or /pr cancel).",
                preview(payload.as_object().expect("object"))
            ))
            .with_details(json!({ "session": [entry] }))
        }
    }
}

/// `/pr confirm`: carry out the pending draft or comment.
pub fn confirm(cwd: &Path, session: &mut Session) -> Result<String, String> {
    let Some(open) = git::pending(session, ACTION) else {
        return Err("no pull request action is waiting for approval".into());
    };
    let text = execute(cwd, &open)?;
    git::resolve(session, ACTION, "done").map_err(|e| e.to_string())?;
    Ok(text)
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
            assert!(git::git(dir, &args).ok);
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        assert!(git::git(dir, &["add", "-A"]).ok);
        assert!(git::git(dir, &["commit", "-q", "-m", "Initial"]).ok);
    }

    fn draft_args() -> Value {
        json!({"action":"draft","title":"Fix a","summary":"a was wrong","issue":"12"})
    }

    #[test]
    fn checks_are_grouped_and_never_claimed_green_without_results() {
        let text = summarize_checks(
            r#"[{"name":"build","bucket":"pass","link":""},{"name":"lint","bucket":"fail","link":"https://ci/1"},{"name":"e2e","bucket":"pending","link":""}]"#,
        )
        .unwrap();
        assert!(
            text.starts_with("FAILING: 1 failing, 1 pending, 1 passing"),
            "{text}"
        );
        assert!(text.contains("lint (https://ci/1)"), "{text}");
        let pending = summarize_checks(r#"[{"name":"b","bucket":"pending"}]"#).unwrap();
        assert!(pending.starts_with("PENDING"), "{pending}");
        let green = summarize_checks(r#"[{"name":"b","bucket":"pass"}]"#).unwrap();
        assert!(green.starts_with("PASSING"), "{green}");
        let none = summarize_checks("[]").unwrap();
        assert!(none.contains("nothing is known to pass"), "{none}");
        assert!(summarize_checks("not json").is_err());
    }

    #[test]
    fn the_body_has_commits_stats_tests_and_issue_but_no_secrets_or_paths() {
        let dir = tempfile::tempdir().unwrap();
        let commits = vec!["Fix the parser".to_string()];
        let summary = format!(
            "Fixed in {}/src/a.rs; see /home/someone/notes. Token ghp_abcdefghijklmnopqrstuvwxyz0123456789AB leaked.\nAPI_KEY=supersecretvalue123",
            dir.path().display()
        );
        let body = build_body(
            &BodyParts {
                summary: &summary,
                issue: Some("#12"),
                commits: &commits,
                diffstat: " src/a.rs | 2 +-\n 1 file changed",
                verification: &Verification::Passed(vec!["test".into()]),
                override_verify: None,
            },
            dir.path(),
        );
        assert!(body.contains("Closes #12"), "{body}");
        assert!(body.contains("- Fix the parser"), "{body}");
        assert!(body.contains("1 file changed"), "{body}");
        assert!(body.contains("verify passed: test"), "{body}");
        assert!(!body.contains("ghp_abc"), "{body}");
        assert!(!body.contains("supersecretvalue123"), "{body}");
        assert!(!body.contains(&dir.path().display().to_string()), "{body}");
        assert!(!body.contains("/home/someone"), "{body}");
        assert!(body.contains("./src/a.rs"), "{body}");
    }

    #[test]
    fn issue_links_take_numbers_and_urls() {
        assert_eq!(issue_line("12").as_deref(), Some("Closes #12"));
        assert_eq!(issue_line("#7").as_deref(), Some("Closes #7"));
        assert_eq!(
            issue_line("https://github.com/o/r/issues/3").as_deref(),
            Some("Closes https://github.com/o/r/issues/3")
        );
        assert_eq!(issue_line(" ").as_deref(), None);
    }

    #[test]
    fn draft_refuses_the_default_branch_dirty_trees_and_missing_verification() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let origin = tempfile::tempdir().unwrap();
        assert!(git::git(origin.path(), &["init", "-q", "--bare"]).ok);
        let url = origin.path().to_string_lossy().to_string();
        assert!(git::git(dir.path(), &["remote", "add", "origin", &url]).ok);
        assert!(git::git(dir.path(), &["push", "-q", "origin", "main"]).ok);
        let ctx = test_context(dir.path());
        let config = GitConfig::default();

        let on_main = dispatch(&ctx, &config, &draft_args());
        assert!(
            on_main.content.contains("PR_DEFAULT_BRANCH"),
            "{}",
            on_main.content
        );

        assert!(git::git(dir.path(), &["switch", "-q", "-c", "topic"]).ok);
        let no_commits = dispatch(&ctx, &config, &draft_args());
        assert!(
            no_commits.content.contains("PR_NO_COMMITS"),
            "{}",
            no_commits.content
        );

        std::fs::write(dir.path().join("a.txt"), "two\n").unwrap();
        let dirty = dispatch(&ctx, &config, &draft_args());
        assert!(dirty.content.contains("PR_DIRTY"), "{}", dirty.content);
        assert!(git::git(dir.path(), &["commit", "-qam", "Fix a"]).ok);

        let unverified = dispatch(&ctx, &config, &draft_args());
        assert!(
            unverified.content.contains("PR_UNVERIFIED"),
            "{}",
            unverified.content
        );

        let mut args = draft_args();
        args["override_verify"] = json!("docs only");
        let preview = dispatch(&ctx, &config, &args);
        assert!(!preview.is_error, "{}", preview.content);
        assert!(
            preview.content.contains("/pr confirm"),
            "{}",
            preview.content
        );
        assert!(
            preview.content.contains("Closes #12"),
            "{}",
            preview.content
        );
        assert!(
            preview.content.contains("not verified: docs only"),
            "{}",
            preview.content
        );
        let entry = &preview.details.unwrap()["session"][0];
        assert_eq!(entry["kind"], PENDING_KIND);
        assert_eq!(entry["base"], "main");
        // Nothing was pushed.
        assert!(
            !git::git(
                origin.path(),
                &["rev-parse", "--verify", "--quiet", "refs/heads/topic"]
            )
            .ok
        );
    }

    fn verify_record(session: &mut Session, payload: Value) {
        session
            .append(EntryKind::Custom(crate::session::CustomData {
                kind: "verify-result".into(),
                payload: payload.as_object().unwrap().clone(),
            }))
            .unwrap();
    }

    #[test]
    fn verification_tracks_the_last_verify_against_file_changes() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let cwd = dir.path();
        let ctx = test_context(cwd);
        let mut session = ctx.session.lock().unwrap();
        assert_eq!(verification(&session, cwd), Verification::Missing);
        let tree = git::content_tree(cwd).unwrap();
        verify_record(
            &mut session,
            json!({"passed": true, "steps": [{"name": "test"}], "tree": tree}),
        );
        assert_eq!(
            verification(&session, cwd),
            Verification::Passed(vec!["test".into()])
        );
        session
            .append(EntryKind::Custom(
                crate::session::CustomData::new("fs-change").with("path", "a"),
            ))
            .unwrap();
        assert_eq!(verification(&session, cwd), Verification::Stale);
        verify_record(
            &mut session,
            json!({"passed": false, "failed_step": "lint"}),
        );
        assert_eq!(
            verification(&session, cwd),
            Verification::Failed("lint".into())
        );
        // A pass without a recorded tree cannot vouch for the commits.
        verify_record(&mut session, json!({"passed": true}));
        assert_eq!(verification(&session, cwd), Verification::Stale);
    }

    #[test]
    fn a_pass_covers_only_the_content_it_checked() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let cwd = dir.path();
        let ctx = test_context(cwd);
        let mut session = ctx.session.lock().unwrap();
        // Verified with a new file and an untracked scratch file on disk.
        std::fs::write(cwd.join("new.rs"), "fn main() {}\n").unwrap();
        std::fs::write(cwd.join("scratch.txt"), "notes\n").unwrap();
        let tree = git::content_tree(cwd).unwrap();
        // The real index was not touched.
        assert!(git::staged_files(cwd).is_empty());
        verify_record(&mut session, json!({"passed": true, "tree": tree}));
        // Committing what was verified (the scratch file stays untracked).
        assert!(git::git(cwd, &["add", "new.rs"]).ok);
        assert!(git::git(cwd, &["commit", "-qm", "Add new"]).ok);
        assert!(matches!(
            verification(&session, cwd),
            Verification::Passed(_)
        ));
        // A shell edit after the verify, committed: no fs-change entry, but
        // the commits no longer match.
        std::fs::write(cwd.join("a.txt"), "edited by sed\n").unwrap();
        assert!(git::git(cwd, &["commit", "-qam", "Edit"]).ok);
        assert_eq!(verification(&session, cwd), Verification::Stale);
    }

    #[test]
    fn an_approved_draft_pushes_only_its_branch() {
        let dir = tempfile::tempdir().unwrap();
        repo(dir.path());
        let cwd = dir.path();
        let origin = tempfile::tempdir().unwrap();
        assert!(git::git(origin.path(), &["init", "-q", "--bare"]).ok);
        let url = origin.path().to_string_lossy().to_string();
        assert!(git::git(cwd, &["remote", "add", "origin", &url]).ok);
        assert!(git::git(cwd, &["config", "push.default", "matching"]).ok);
        for branch in ["other", "topic"] {
            assert!(git::git(cwd, &["switch", "-q", "-c", branch, "main"]).ok);
            assert!(git::git(cwd, &["push", "-q", "-u", "origin", branch]).ok);
            std::fs::write(cwd.join(format!("{branch}.txt")), "x\n").unwrap();
            assert!(git::git(cwd, &["add", "-A"]).ok);
            assert!(git::git(cwd, &["commit", "-qm", branch]).ok);
        }
        let payload = json!({
            "op": "draft", "title": "t", "body": "b", "base": "main",
            "branch": "topic", "remote": "origin",
            "head": git::head(cwd).unwrap(),
        });
        // gh cannot create a PR against a local remote; the push comes first.
        let _ = execute(cwd, payload.as_object().unwrap());
        let remote_head = |branch: &str| {
            git::git(
                origin.path(),
                &["rev-parse", &format!("refs/heads/{branch}")],
            )
            .stdout
            .trim()
            .to_string()
        };
        assert_eq!(remote_head("topic"), git::head(cwd).unwrap());
        let other_local = git::git(cwd, &["rev-parse", "other"])
            .stdout
            .trim()
            .to_string();
        assert_ne!(
            remote_head("other"),
            other_local,
            "an unrelated branch was pushed"
        );
    }
}

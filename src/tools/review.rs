//! `review` tool: working-tree stats, risk findings, and the capped diff.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_bool, optional_u64,
};

const DEFAULT_DIFF_LINES: usize = 400;
const MAX_DIFF_LINES: usize = 400;
const MAX_FINDINGS: usize = 12;
const MAX_GIT_OUTPUT: usize = 8 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

const SOURCE_EXTS: &[&str] = &[
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "cjs", "go", "c", "h", "cc", "cpp", "hpp", "java",
    "rb", "cs", "kt", "kts", "swift", "php", "scala", "lua", "sh",
];

const LOCKFILES: &[&str] = &[
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "go.sum",
    "poetry.lock",
    "Pipfile.lock",
    "composer.lock",
    "uv.lock",
    "Gemfile.lock",
    "flake.lock",
    "bun.lockb",
];

pub struct ReviewTool;

#[async_trait]
impl Tool for ReviewTool {
    fn name(&self) -> &'static str {
        "review"
    }

    fn description(&self) -> &'static str {
        "Review working-tree changes and risks."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "staged": {"type": "boolean", "description": "Review the index instead of the working tree."},
                "lines": {"type": "integer", "minimum": 1, "maximum": MAX_DIFF_LINES, "description": "Diff lines to show, default and maximum 400."}
            }),
            &[],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Coding
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        match self.run(ctx, args).await {
            Ok(result) => result,
            Err(error) => error,
        }
    }
}

impl ReviewTool {
    pub fn new() -> Self {
        Self
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolResult> {
        let staged = optional_bool(&args, "staged").unwrap_or(false);
        let lines = optional_u64(&args, "lines")
            .unwrap_or(DEFAULT_DIFF_LINES as u64)
            .clamp(1, MAX_DIFF_LINES as u64) as usize;

        let snapshot = collect_snapshot(&ctx.cwd, staged).await?;
        let findings = scan(&snapshot);
        let content = render(&snapshot, &findings, lines);
        let (added, removed) = snapshot.totals();

        Ok(ToolResult::ok(content).with_details(json!({
            "staged": staged,
            "files": snapshot.changed_paths().len(),
            "added": added,
            "removed": removed,
            "findings": findings.len(),
        })))
    }
}

impl Default for ReviewTool {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Git access
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Snapshot {
    staged: bool,
    status: Vec<StatusEntry>,
    file_stats: Vec<FileStat>,
    diff: String,
}

impl Snapshot {
    fn changed_paths(&self) -> Vec<String> {
        let mut paths: Vec<String> = self.status.iter().map(|entry| entry.path.clone()).collect();
        for stat in &self.file_stats {
            if !paths.contains(&stat.path) {
                paths.push(stat.path.clone());
            }
        }
        paths
    }

    fn totals(&self) -> (u64, u64) {
        let added = self
            .file_stats
            .iter()
            .map(|stat| stat.added.unwrap_or(0))
            .sum();
        let removed = self
            .file_stats
            .iter()
            .map(|stat| stat.removed.unwrap_or(0))
            .sum();
        (added, removed)
    }

    fn untracked(&self) -> usize {
        self.status
            .iter()
            .filter(|entry| entry.status == "??")
            .count()
    }
}

#[derive(Debug, Clone)]
struct StatusEntry {
    status: String,
    path: String,
}

#[derive(Debug, Clone)]
struct FileStat {
    path: String,
    added: Option<u64>,
    removed: Option<u64>,
    binary: bool,
}

async fn collect_snapshot(cwd: &Path, staged: bool) -> Result<Snapshot, ToolResult> {
    ensure_repo(cwd).await?;

    let status_raw = git(cwd, &["status", "--porcelain=v1", "-z"]).await?;
    let numstat_args: &[&str] = if staged {
        &["diff", "--cached", "--numstat"]
    } else {
        &["diff", "--numstat"]
    };
    let numstat_raw = git(cwd, numstat_args).await?;
    let diff_args: &[&str] = if staged {
        &["diff", "--cached", "--", "."]
    } else {
        &["diff", "--", "."]
    };
    let diff = git(cwd, diff_args).await?;

    Ok(Snapshot {
        staged,
        status: parse_status(&status_raw),
        file_stats: parse_numstat(&numstat_raw),
        diff,
    })
}

async fn ensure_repo(cwd: &Path) -> Result<(), ToolResult> {
    let mut command = Command::new("git");
    command
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(GIT_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err(ToolResult::err(
                "NOT_A_REPO",
                format!(
                    "cannot run git in {}: {error}; install git and retry",
                    cwd.display()
                ),
            ));
        }
        Err(_) => {
            return Err(ToolResult::err(
                "NOT_A_REPO",
                format!(
                    "git did not answer in {}; check the repository",
                    cwd.display()
                ),
            ));
        }
    };
    if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true" {
        return Ok(());
    }
    Err(ToolResult::err(
        "NOT_A_REPO",
        format!(
            "{} is not inside a git work tree; review reads working-tree changes. cd into a repository or run \"git init\".",
            cwd.display()
        ),
    ))
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String, ToolResult> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let shown = args.join(" ");
    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            ToolResult::err(
                "GIT_ERROR",
                format!("git {shown} timed out after {}s", GIT_TIMEOUT.as_secs()),
            )
        })?
        .map_err(|error| {
            ToolResult::err(
                "GIT_ERROR",
                format!("could not run git {shown}: {error}; install git and retry"),
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(ToolResult::err(
            "GIT_ERROR",
            format!("git {shown} failed: {stderr}"),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(cap_output(stdout, MAX_GIT_OUTPUT))
}

fn cap_output(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str("\n[git output truncated]");
    text
}

fn parse_status(raw: &str) -> Vec<StatusEntry> {
    let mut entries = Vec::new();
    let mut tokens = raw.split('\0');
    while let Some(token) = tokens.next() {
        if token.len() < 4 {
            continue;
        }
        let status = token[..2].trim().to_string();
        let path = match token.get(3..) {
            Some(path) if !path.is_empty() => path.to_string(),
            _ => continue,
        };
        if status.starts_with('R') || status.starts_with('C') {
            // The original path follows as its own NUL-terminated token.
            let _ = tokens.next();
        }
        entries.push(StatusEntry { status, path });
    }
    entries
}

fn parse_numstat(raw: &str) -> Vec<FileStat> {
    let mut stats = Vec::new();
    for line in raw.lines() {
        let mut parts = line.splitn(3, '\t');
        let added = parts.next().unwrap_or("");
        let removed = parts.next().unwrap_or("");
        let path = match parts.next() {
            Some(path) if !path.is_empty() => path.to_string(),
            _ => continue,
        };
        let binary = added == "-" || removed == "-";
        stats.push(FileStat {
            path,
            added: added.parse::<u64>().ok(),
            removed: removed.parse::<u64>().ok(),
            binary,
        });
    }
    stats
}

// ---------------------------------------------------------------------------
// Diff parsing
// ---------------------------------------------------------------------------

/// Added lines as `(path, new line number, text)`.
fn added_lines(diff: &str) -> Vec<(&str, u64, &str)> {
    let mut added = Vec::new();
    let mut path: &str = "";
    let mut new_line: u64 = 0;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            path = if rest == "/dev/null" {
                ""
            } else {
                rest.strip_prefix("b/").unwrap_or(rest)
            };
            continue;
        }
        if line.starts_with("@@") {
            if let Some(plus) = line.find('+') {
                let digits: String = line[plus + 1..]
                    .chars()
                    .take_while(|character| character.is_ascii_digit())
                    .collect();
                if let Ok(value) = digits.parse::<u64>() {
                    new_line = value;
                }
            }
            continue;
        }
        if let Some(text) = line.strip_prefix('+') {
            if !path.is_empty() {
                added.push((path, new_line, text));
            }
            new_line += 1;
            continue;
        }
        if line.starts_with('-') {
            continue;
        }
        if line.starts_with(' ') {
            new_line += 1;
        }
    }
    added
}

// ---------------------------------------------------------------------------
// Risk scan
// ---------------------------------------------------------------------------

struct Finding {
    path: String,
    line: Option<u64>,
    message: String,
}

impl Finding {
    fn new(path: &str, line: Option<u64>, message: impl Into<String>) -> Self {
        Self {
            path: path.to_string(),
            line,
            message: message.into(),
        }
    }
}

struct Scanner {
    aws: Option<regex::Regex>,
    bearer: Option<regex::Regex>,
    assignment: Option<regex::Regex>,
}

impl Scanner {
    fn new() -> Self {
        Self {
            aws: regex::Regex::new(r"AKIA[0-9A-Z]{16}").ok(),
            bearer: regex::Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{20,}").ok(),
            assignment: regex::Regex::new(
                r#"(?i)(password|passwd|secret|api[_-]?key|access[_-]?key|auth[_-]?token)\s*[:=]\s*["']([^"']{6,})["']"#,
            )
            .ok(),
        }
    }

    fn secret_reason(&self, line: &str) -> Option<&'static str> {
        if self.aws.as_ref().is_some_and(|regex| regex.is_match(line)) {
            return Some("looks like an AWS access key; remove it and rotate the credential");
        }
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            return Some("private key material added; never commit keys");
        }
        if self
            .bearer
            .as_ref()
            .is_some_and(|regex| regex.is_match(line))
        {
            return Some("hard-coded bearer token; move it to configuration and rotate it");
        }
        if let Some(regex) = &self.assignment
            && let Some(captures) = regex.captures(line)
            && let Some(value) = captures.get(2)
            && !is_placeholder(value.as_str())
        {
            return Some(
                "credential-like assignment with a literal value; load it from the environment",
            );
        }
        None
    }
}

fn is_placeholder(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "${",
        "{{",
        "<",
        "env",
        "example",
        "changeme",
        "redacted",
        "your_",
        "your-",
        "xxxx",
        "***",
        "placeholder",
    ]
    .iter()
    .any(|token| lower.contains(token))
}

fn debug_reason(line: &str) -> Option<&'static str> {
    const TOKENS: &[(&str, &str)] = &[
        ("dbg!", "dbg! left in the change; remove it"),
        (
            "println!",
            "println! left in the change; use logging or remove it",
        ),
        (
            "eprintln!",
            "eprintln! left in the change; use logging or remove it",
        ),
        ("console.log", "console.log left in the change; remove it"),
        (
            "console.debug",
            "console.debug left in the change; remove it",
        ),
        (
            "debugger",
            "debugger statement left in the change; remove it",
        ),
        (
            "pdb.set_trace",
            "pdb breakpoint left in the change; remove it",
        ),
        ("breakpoint()", "breakpoint() left in the change; remove it"),
        ("binding.pry", "binding.pry left in the change; remove it"),
    ];
    for (token, reason) in TOKENS {
        if line.contains(token) {
            return Some(reason);
        }
    }
    None
}

fn todo_reason(line: &str) -> Option<&'static str> {
    if line.contains("TODO") || line.contains("FIXME") {
        Some("unfinished work marker (TODO/FIXME) added; resolve it or file a follow-up")
    } else {
        None
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn extension_of(path: &str) -> Option<String> {
    let name = file_name(path);
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
}

fn is_lockfile(path: &str) -> bool {
    LOCKFILES.contains(&file_name(path))
}

fn is_config(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    name == ".env"
        || name.starts_with(".env.")
        || matches!(
            name,
            "config.json"
                | "config.yaml"
                | "config.yml"
                | "config.toml"
                | "settings.json"
                | "docker-compose.yml"
                | "docker-compose.yaml"
        )
        || lower.ends_with(".pem")
        || lower.ends_with(".key")
        || lower.ends_with(".p12")
        || lower.ends_with(".ini")
        || lower.ends_with(".cfg")
        || lower.starts_with(".github/workflows/")
}

fn is_source_path(path: &str) -> bool {
    extension_of(path).is_some_and(|extension| SOURCE_EXTS.contains(&extension.as_str()))
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    lower.starts_with("tests/")
        || lower.contains("/tests/")
        || lower.starts_with("test/")
        || lower.contains("/test/")
        || lower.starts_with("__tests__/")
        || lower.contains("/__tests__/")
        || lower.starts_with("spec/")
        || lower.contains("/spec/")
        || name.starts_with("test_")
        || name.starts_with("test.")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.contains("_spec.")
}

fn is_deletion_heavy(stat: &FileStat) -> bool {
    let added = stat.added.unwrap_or(0);
    let removed = stat.removed.unwrap_or(0);
    removed > 20 && removed * 5 > (added + removed) * 3
}

fn scan(snapshot: &Snapshot) -> Vec<Finding> {
    let scanner = Scanner::new();
    let added = added_lines(&snapshot.diff);
    let mut findings: Vec<Finding> = Vec::new();
    let full = |findings: &Vec<Finding>| findings.len() >= MAX_FINDINGS;

    // Secrets first: they are the one finding worth acting on immediately.
    for (path, line, text) in &added {
        if full(&findings) {
            break;
        }
        if let Some(reason) = scanner.secret_reason(text) {
            findings.push(Finding::new(path, Some(*line), reason));
        }
    }

    for path in &snapshot.changed_paths() {
        if full(&findings) {
            break;
        }
        let stat = snapshot.file_stats.iter().find(|stat| stat.path == *path);
        if is_lockfile(path) {
            findings.push(Finding::new(
                path,
                None,
                "lockfile changed; review dependency additions before committing",
            ));
        } else if is_config(path) {
            findings.push(Finding::new(
                path,
                None,
                "config or credential file changed; confirm no secrets or unsafe defaults leak",
            ));
        } else if stat.is_some_and(|stat| stat.binary) {
            findings.push(Finding::new(
                path,
                None,
                "binary file changed; inspect it directly because diffs are not reviewable",
            ));
        } else if let Some(stat) = stat
            && is_deletion_heavy(stat)
        {
            let removed = stat.removed.unwrap_or(0);
            let total = stat.added.unwrap_or(0) + removed;
            findings.push(Finding::new(
                    path,
                    None,
                    format!(
                        "{removed} of {total} changed lines removed; confirm the deletion is intended and not an accidental wipe"
                    ),
                ));
        }
    }

    for (path, line, text) in &added {
        if full(&findings) {
            break;
        }
        if findings
            .iter()
            .any(|finding| finding.path == *path && finding.line == Some(*line))
        {
            continue;
        }
        if let Some(reason) = debug_reason(text) {
            findings.push(Finding::new(path, Some(*line), reason));
        }
    }

    for (path, line, text) in &added {
        if full(&findings) {
            break;
        }
        if findings
            .iter()
            .any(|finding| finding.path == *path && finding.line == Some(*line))
        {
            continue;
        }
        if let Some(reason) = todo_reason(text) {
            findings.push(Finding::new(path, Some(*line), reason));
        }
    }

    let changed = snapshot.changed_paths();
    let tests_changed = changed.iter().any(|path| is_test_path(path));
    if !tests_changed
        && !full(&findings)
        && let Some(source) = changed
            .iter()
            .find(|path| is_source_path(path) && !is_test_path(path))
    {
        findings.push(Finding::new(
                source,
                None,
                "source changed without test changes; add or update a test, or state why none is needed",
            ));
    }

    findings.truncate(MAX_FINDINGS);
    findings
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(snapshot: &Snapshot, findings: &[Finding], lines: usize) -> String {
    let scope = if snapshot.staged {
        "staged"
    } else {
        "working tree"
    };
    let changed = snapshot.changed_paths();
    if changed.is_empty() && snapshot.diff.is_empty() {
        return format!("review: no {scope} changes");
    }

    let (added, removed) = snapshot.totals();
    let mut out = format!(
        "review ({scope}): {} file{}, +{added}/-{removed}\n",
        changed.len(),
        if changed.len() == 1 { "" } else { "s" }
    );
    let untracked = snapshot.untracked();
    if untracked > 0 {
        out.push_str(&format!("untracked: {untracked}\n"));
    }

    if findings.is_empty() {
        out.push_str("findings: none\n");
    } else {
        out.push_str(&format!("findings ({}):\n", findings.len()));
        for finding in findings {
            match finding.line {
                Some(line) => out.push_str(&format!(
                    "- {}:{} — {}\n",
                    finding.path, line, finding.message
                )),
                None => out.push_str(&format!("- {} — {}\n", finding.path, finding.message)),
            }
        }
    }

    let all: Vec<&str> = snapshot.diff.lines().collect();
    if all.is_empty() {
        out.push_str("diff: empty (untracked files are not shown)\n");
        return out;
    }
    let shown = lines.min(all.len());
    out.push_str(&format!("diff ({shown} of {} lines):\n", all.len()));
    out.push_str(&all[..shown].join("\n"));
    out.push('\n');
    if all.len() > shown {
        out.push_str(&format!(
            "[diff truncated: {shown} of {} lines (max {MAX_DIFF_LINES}). Narrow with bash \"git diff -- <path>\", then rerun \"review lines={shown}\".]",
            all.len()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent dir");
            }
            std::fs::write(path, contents).expect("write fixture");
        }
        run_git(dir.path(), &["init", "-q"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "base"]);
        dir
    }

    #[test]
    fn parses_status_entries_with_renames() {
        let raw = "R  new.rs\0old.rs\0?? scratch.txt\0M  src/lib.rs\0";
        let entries = parse_status(raw);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].status, "R");
        assert_eq!(entries[0].path, "new.rs");
        assert_eq!(entries[1].status, "??");
        assert_eq!(entries[1].path, "scratch.txt");
        assert_eq!(entries[2].status, "M");
        assert_eq!(entries[2].path, "src/lib.rs");
    }

    #[test]
    fn parses_numstat_and_marks_binary() {
        let stats = parse_numstat("12\t3\tsrc/lib.rs\n-\t-\tassets/logo.png\n");
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].added, Some(12));
        assert_eq!(stats[0].removed, Some(3));
        assert!(!stats[0].binary);
        assert!(stats[1].binary);
    }

    #[test]
    fn added_lines_track_new_line_numbers() {
        let diff = concat!(
            "diff --git a/src/lib.rs b/src/lib.rs\n",
            "index 1111111..2222222 100644\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@ -1,2 +1,4 @@\n",
            " fn base() {}\n",
            "+let secret = 1;\n",
            "+let other = 2;\n",
        );
        let lines = added_lines(diff);
        assert_eq!(
            lines,
            vec![
                ("src/lib.rs", 2, "let secret = 1;"),
                ("src/lib.rs", 3, "let other = 2;"),
            ]
        );
    }

    #[tokio::test]
    async fn temp_repo_finds_secrets_debug_and_lockfiles() {
        if !git_available() {
            return;
        }
        let dir = init_repo(&[
            ("src/lib.rs", "fn base() {}\n"),
            ("Cargo.lock", "# lock v1\n"),
        ]);
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "fn base() {}\n\
const KEY: &str = \"AKIAABCDEFGHIJKLMNOP\";\n\
fn run() { println!(\"debug\"); }\n\
// TODO: replace this\n",
        )
        .expect("modify source");
        std::fs::write(dir.path().join("Cargo.lock"), "# lock v1\n# dep added\n")
            .expect("modify lockfile");

        let snapshot = collect_snapshot(dir.path(), false).await.expect("snapshot");
        assert_eq!(snapshot.changed_paths().len(), 2);
        let findings = scan(&snapshot);
        let joined: Vec<String> = findings
            .iter()
            .map(|finding| match finding.line {
                Some(line) => format!("{}:{} {}", finding.path, line, finding.message),
                None => format!("{} {}", finding.path, finding.message),
            })
            .collect();
        let text = joined.join("\n");
        assert!(text.contains("src/lib.rs:2"), "{text}");
        assert!(text.contains("AWS access key"), "{text}");
        assert!(text.contains("println!"), "{text}");
        assert!(text.contains("TODO/FIXME"), "{text}");
        assert!(text.contains("Cargo.lock"), "{text}");
        assert!(text.contains("without test changes"), "{text}");

        let rendered = render(&snapshot, &findings, 400);
        assert!(rendered.starts_with("review (working tree): 2 files"));
        assert!(rendered.contains("findings ("));
        assert!(rendered.contains("diff ("));
    }

    #[tokio::test]
    async fn staged_and_working_tree_are_separate() {
        if !git_available() {
            return;
        }
        let dir = init_repo(&[("src/app.py", "print('base')\n")]);
        std::fs::write(
            dir.path().join("src/app.py"),
            "print('base')\nTOKEN = \"password=hunter2secret\"\n",
        )
        .expect("modify");

        let unstaged = collect_snapshot(dir.path(), false).await.expect("unstaged");
        assert!(!unstaged.diff.is_empty());
        let staged_before = collect_snapshot(dir.path(), true).await.expect("staged");
        assert!(staged_before.diff.is_empty());

        run_git(dir.path(), &["add", "src/app.py"]);
        let staged = collect_snapshot(dir.path(), true).await.expect("staged");
        assert!(staged.diff.contains("hunter2secret"));
        assert!(unstaged.diff.contains("hunter2secret"));
        let empty = collect_snapshot(dir.path(), false).await.expect("clean");
        assert!(empty.diff.is_empty());
    }

    #[tokio::test]
    async fn deletion_heavy_change_is_flagged() {
        if !git_available() {
            return;
        }
        let long_file: String = (0..40).map(|index| format!("line {index}\n")).collect();
        let dir = init_repo(&[("src/big.rs", &long_file)]);
        std::fs::write(dir.path().join("src/big.rs"), "line 0\n").expect("truncate");
        let snapshot = collect_snapshot(dir.path(), false).await.expect("snapshot");
        let findings = scan(&snapshot);
        assert!(
            findings
                .iter()
                .any(|finding| finding.message.contains("removed")),
            "expected a deletion-heavy finding"
        );
    }

    #[tokio::test]
    async fn outside_a_repo_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let error = collect_snapshot(dir.path(), false)
            .await
            .expect_err("not a repo");
        assert!(error.content.starts_with("[NOT_A_REPO]"));
    }

    #[tokio::test]
    async fn clean_repository_reports_no_changes() {
        if !git_available() {
            return;
        }
        let dir = init_repo(&[("README.md", "hello\n")]);
        let snapshot = collect_snapshot(dir.path(), false).await.expect("snapshot");
        let findings = scan(&snapshot);
        assert!(findings.is_empty());
        assert_eq!(
            render(&snapshot, &findings, 400),
            "review: no working tree changes"
        );
    }

    #[test]
    fn diff_rendering_truncates_and_names_the_follow_up() {
        let diff: String = (0..500).map(|index| format!("+line {index}\n")).collect();
        let snapshot = Snapshot {
            staged: false,
            status: vec![StatusEntry {
                status: "M".to_string(),
                path: "src/lib.rs".to_string(),
            }],
            file_stats: vec![FileStat {
                path: "src/lib.rs".to_string(),
                added: Some(500),
                removed: Some(0),
                binary: false,
            }],
            diff,
        };
        let rendered = render(&snapshot, &[], 400);
        assert!(rendered.contains("diff (400 of 500 lines)"));
        assert!(rendered.contains("diff truncated"));
        assert!(rendered.contains("review lines=400"));
    }

    #[test]
    fn lockfiles_and_configs_are_recognized() {
        assert!(is_lockfile("Cargo.lock"));
        assert!(is_lockfile("web/package-lock.json"));
        assert!(!is_lockfile("src/lock.rs"));
        assert!(is_config(".env"));
        assert!(is_config("deploy/.env.production"));
        assert!(is_config("certs/server.pem"));
        assert!(is_config(".github/workflows/ci.yml"));
        assert!(!is_config("src/config_lib.rs"));
    }

    #[test]
    fn test_paths_and_sources_are_classified() {
        assert!(is_test_path("tests/api.rs"));
        assert!(is_test_path("src/foo_test.go"));
        assert!(is_test_path("web/app.spec.ts"));
        assert!(!is_test_path("src/contest.rs"));
        assert!(is_source_path("src/main.rs"));
        assert!(!is_source_path("README.md"));
    }
}

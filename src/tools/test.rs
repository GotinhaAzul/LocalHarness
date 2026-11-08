//! `test` tool: detects the project's test runner, runs one file or one test,
//! and returns only the first failure.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;
use tokio::process::{ChildStderr, ChildStdout, Command};

use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, object_schema, optional_str,
    optional_u64,
};

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const MAX_TIMEOUT_SECS: u64 = 600;
const MAX_RESULT_CHARS: usize = 2500;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

pub struct TestTool;

#[async_trait]
impl Tool for TestTool {
    fn name(&self) -> &'static str {
        "test"
    }

    fn description(&self) -> &'static str {
        "Run one test file or one test; first failure only."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "path": {"type": "string", "description": "Test file to run."},
                "test": {"type": "string", "description": "Test name filter."},
                "timeout": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_SECS, "description": "Seconds, default and maximum 600."}
            }),
            &[],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Coding
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        match self.run(ctx, args).await {
            Ok(result) => result,
            Err(error) => error,
        }
    }
}

impl TestTool {
    pub fn new() -> Self {
        Self
    }

    async fn run(&self, ctx: &ToolContext, args: Value) -> Result<ToolResult, ToolResult> {
        let path = match optional_str(&args, "path") {
            Some(raw) => {
                let resolved = ctx.resolve(&raw)?;
                if !resolved.exists() {
                    return Err(ToolResult::err(
                        "invalid_args",
                        format!(
                            "{} does not exist; pass an existing test file or omit \"path\"",
                            display_path(&ctx.cwd, &resolved)
                        ),
                    ));
                }
                Some(resolved)
            }
            None => None,
        };
        let test = optional_str(&args, "test").filter(|name| !name.trim().is_empty());
        let seconds = optional_u64(&args, "timeout")
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);

        let runner = detect_runner(&ctx.cwd, path.as_deref())?;
        let invocation = build_invocation(&ctx.cwd, runner, path.as_deref(), test.as_deref());
        let output = run_invocation(&invocation, &ctx.cwd, Duration::from_secs(seconds)).await?;
        if output.timed_out {
            return Err(ToolResult::err(
                "TEST_TIMEOUT",
                format!(
                    "{} did not finish within {seconds}s; the process group was killed. Narrow the run with \"test\", pass a single \"path\", or raise \"timeout\" (max {MAX_TIMEOUT_SECS}).",
                    invocation.display
                ),
            ));
        }

        let cleaned = strip_ansi(&output.text);
        let summary = summary_line(runner, &output, &cleaned);
        let body = match failure_excerpt(&cleaned) {
            Some(excerpt) => excerpt,
            None if output.success => String::new(),
            None => {
                let tail: Vec<&str> = cleaned.lines().rev().take(40).collect();
                let mut tail = tail;
                tail.reverse();
                format!(
                    "no failure marker found; last output lines:\n{}",
                    tail.join("\n")
                )
            }
        };
        let mut content = format!("{summary}\nrerun: {}", invocation.display);
        if !body.is_empty() {
            content.push_str("\n\n");
            content.push_str(&body);
        }
        let content = truncate_chars(&content, MAX_RESULT_CHARS);

        Ok(ToolResult::ok(content).with_details(json!({
            "runner": runner.name(),
            "command": invocation.display,
            "exit_code": output.status,
            "timed_out": output.timed_out,
            "passed": count_matches(&cleaned, r"(\d+)\s+passed"),
            "failed": count_matches(&cleaned, r"(\d+)\s+failed"),
        })))
    }
}

// ---------------------------------------------------------------------------
// Runner detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runner {
    Cargo,
    Vitest,
    Jest,
    Pytest,
    Go,
    Npm,
}

impl Runner {
    fn name(self) -> &'static str {
        match self {
            Runner::Cargo => "cargo",
            Runner::Vitest => "vitest",
            Runner::Jest => "jest",
            Runner::Pytest => "pytest",
            Runner::Go => "go",
            Runner::Npm => "npm",
        }
    }
}

fn detect_runner(cwd: &Path, path: Option<&Path>) -> Result<Runner, ToolResult> {
    let extension = path
        .and_then(|path| path.extension())
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());

    match extension.as_deref() {
        Some("rs") if cwd.join("Cargo.toml").exists() => return Ok(Runner::Cargo),
        Some("go") if cwd.join("go.mod").exists() => return Ok(Runner::Go),
        Some("py") if python_project(cwd) => return Ok(Runner::Pytest),
        Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs") => {
            if let Some(runner) = node_runner(cwd) {
                return Ok(runner);
            }
        }
        _ => {}
    }

    if cwd.join("Cargo.toml").exists() {
        return Ok(Runner::Cargo);
    }
    if cwd.join("package.json").is_file() {
        if let Some(runner) = node_runner(cwd) {
            return Ok(runner);
        }
    }
    if python_project(cwd) {
        return Ok(Runner::Pytest);
    }
    if cwd.join("go.mod").exists() {
        return Ok(Runner::Go);
    }

    Err(ToolResult::err(
        "NO_TEST_RUNNER",
        format!(
            "no test runner detected in {}; looked for Cargo.toml, package.json with vitest/jest or an npm test script, pyproject.toml/setup.py/pytest.ini, and go.mod. Pass \"path\" to a test file or add one of those files.",
            cwd.display()
        ),
    ))
}

fn python_project(cwd: &Path) -> bool {
    [
        "pyproject.toml",
        "setup.py",
        "setup.cfg",
        "pytest.ini",
        "tox.ini",
        "conftest.py",
        "requirements.txt",
    ]
    .iter()
    .any(|marker| cwd.join(marker).exists())
}

fn node_runner(cwd: &Path) -> Option<Runner> {
    let package = std::fs::read_to_string(cwd.join("package.json")).ok();
    let parsed: Value = package
        .as_deref()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null);

    let mut signals = String::new();
    for key in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(map) = parsed.get(key).and_then(Value::as_object) {
            for name in map.keys() {
                signals.push_str(name);
                signals.push('\n');
            }
        }
    }
    if let Some(scripts) = parsed.get("scripts").and_then(Value::as_object) {
        for (name, value) in scripts {
            signals.push_str(name);
            signals.push('\n');
            if let Some(value) = value.as_str() {
                signals.push_str(value);
                signals.push('\n');
            }
        }
    }

    let local_vitest = is_executable(&cwd.join("node_modules/.bin/vitest"));
    let local_jest = is_executable(&cwd.join("node_modules/.bin/jest"));
    if local_vitest || signals.contains("vitest") {
        return Some(Runner::Vitest);
    }
    if local_jest || signals.contains("jest") {
        return Some(Runner::Jest);
    }
    if parsed
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .is_some()
    {
        return Some(Runner::Npm);
    }
    None
}

fn find_binary(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return is_executable(&path).then_some(path);
    }
    let search = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&search) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

// ---------------------------------------------------------------------------
// Command construction
// ---------------------------------------------------------------------------

struct Builder {
    program: OsString,
    args: Vec<OsString>,
}

impl Builder {
    fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    fn arg(&mut self, value: impl Into<OsString>) -> &mut Self {
        self.args.push(value.into());
        self
    }
}

struct Invocation {
    program: OsString,
    args: Vec<OsString>,
    display: String,
}

fn build_invocation(
    cwd: &Path,
    runner: Runner,
    path: Option<&Path>,
    test: Option<&str>,
) -> Invocation {
    let shown_path = path.map(|path| argument_path(cwd, path));
    let builder = match runner {
        Runner::Cargo => {
            let mut builder = Builder::new("cargo");
            builder.arg("test");
            if let Some(path) = path {
                let in_tests_dir = path.components().any(
                    |component| matches!(component, Component::Normal(name) if name == "tests"),
                );
                let stem = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("");
                if in_tests_dir && !stem.is_empty() {
                    builder.arg("--test").arg(stem);
                }
            }
            if let Some(test) = test {
                builder.arg("--").arg(test);
            }
            builder
        }
        Runner::Vitest => {
            let (program, needs_name) = node_program(cwd, "vitest");
            let mut builder = Builder::new(program);
            if needs_name {
                builder.arg("vitest");
            }
            builder.arg("run");
            if let Some(path) = &shown_path {
                builder.arg(path);
            }
            if let Some(test) = test {
                builder.arg("-t").arg(test);
            }
            builder
        }
        Runner::Jest => {
            let (program, needs_name) = node_program(cwd, "jest");
            let mut builder = Builder::new(program);
            if needs_name {
                builder.arg("jest");
            }
            if let Some(path) = &shown_path {
                builder.arg("--runTestsByPath").arg(path);
            }
            if let Some(test) = test {
                builder.arg("-t").arg(test);
            }
            builder
        }
        Runner::Pytest => {
            let program = if find_binary("python3").is_some() {
                OsString::from("python3")
            } else {
                OsString::from("python")
            };
            let mut builder = Builder::new(program);
            builder.arg("-m").arg("pytest");
            match (&shown_path, test) {
                (Some(path), Some(test)) => {
                    builder.arg(format!("{path}::{test}"));
                }
                (Some(path), None) => {
                    builder.arg(path);
                }
                (None, Some(test)) => {
                    builder.arg("-k").arg(test);
                }
                (None, None) => {}
            }
            builder
        }
        Runner::Go => {
            let package = match path {
                Some(path) => {
                    let dir = if path.is_dir() {
                        path
                    } else {
                        path.parent().unwrap_or(cwd)
                    };
                    let relative = dir.strip_prefix(cwd).unwrap_or(Path::new("."));
                    let relative = relative.display().to_string();
                    if relative.is_empty() || relative == "." {
                        ".".to_string()
                    } else {
                        format!("./{relative}")
                    }
                }
                None => "./...".to_string(),
            };
            let mut builder = Builder::new("go");
            builder.arg("test").arg(package);
            if let Some(test) = test {
                builder.arg("-run").arg(test);
            }
            builder
        }
        Runner::Npm => {
            let mut builder = Builder::new("npm");
            builder.arg("test");
            if let Some(test) = test {
                builder.arg("--").arg(test);
            }
            builder
        }
    };

    let display = {
        let mut parts = vec![quote(&builder.program.to_string_lossy())];
        parts.extend(builder.args.iter().map(|arg| quote(&arg.to_string_lossy())));
        parts.join(" ")
    };
    Invocation {
        program: builder.program,
        args: builder.args,
        display,
    }
}

fn node_program(cwd: &Path, name: &str) -> (OsString, bool) {
    let local = cwd.join("node_modules/.bin").join(name);
    if is_executable(&local) {
        (local.into_os_string(), false)
    } else {
        (OsString::from("npx"), true)
    }
}

fn argument_path(cwd: &Path, path: &Path) -> String {
    if path.starts_with(cwd) {
        display_path(cwd, path)
    } else {
        path.to_string_lossy().to_string()
    }
}

fn quote(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }
    let safe = arg
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_./:=@,".contains(character));
    if safe {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

struct RunOutput {
    status: Option<i32>,
    success: bool,
    text: String,
    timed_out: bool,
}

async fn run_invocation(
    invocation: &Invocation,
    cwd: &Path,
    timeout: Duration,
) -> Result<RunOutput, ToolResult> {
    let mut command = Command::new(&invocation.program);
    command
        .args(&invocation.args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().map_err(|error| {
        ToolResult::err(
            "NO_TEST_RUNNER",
            format!(
                "could not start {}: {error}; install the tool, check PATH, or pass another \"path\"",
                invocation.display
            ),
        )
    })?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = match (stdout, stderr) {
        (Some(stdout), Some(stderr)) => {
            Some(tokio::spawn(read_merged(stdout, stderr, MAX_CAPTURE_BYTES)))
        }
        _ => None,
    };

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            let bytes = collect(reader).await;
            Ok(RunOutput {
                status: status.code(),
                success: status.success(),
                text: String::from_utf8_lossy(&bytes).into_owned(),
                timed_out: false,
            })
        }
        Ok(Err(error)) => Err(ToolResult::err(
            "NO_TEST_RUNNER",
            format!("could not wait for {}: {error}", invocation.display),
        )),
        Err(_) => {
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
            let bytes = collect(reader).await;
            Ok(RunOutput {
                status: None,
                success: false,
                text: String::from_utf8_lossy(&bytes).into_owned(),
                timed_out: true,
            })
        }
    }
}

async fn collect(reader: Option<tokio::task::JoinHandle<Vec<u8>>>) -> Vec<u8> {
    match reader {
        Some(handle) => tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .ok()
            .and_then(|result| result.ok())
            .unwrap_or_default(),
        None => Vec::new(),
    }
}

async fn read_merged(mut stdout: ChildStdout, mut stderr: ChildStderr, cap: usize) -> Vec<u8> {
    let mut combined = Vec::new();
    let mut out_buffer = vec![0u8; 8192];
    let mut err_buffer = vec![0u8; 8192];
    let mut out_open = true;
    let mut err_open = true;
    while out_open || err_open {
        tokio::select! {
            result = stdout.read(&mut out_buffer), if out_open => {
                match result {
                    Ok(0) => out_open = false,
                    Ok(read) => extend_capped(&mut combined, &out_buffer[..read], cap),
                    Err(_) => out_open = false,
                }
            }
            result = stderr.read(&mut err_buffer), if err_open => {
                match result {
                    Ok(0) => err_open = false,
                    Ok(read) => extend_capped(&mut combined, &err_buffer[..read], cap),
                    Err(_) => err_open = false,
                }
            }
        }
    }
    combined
}

fn extend_capped(target: &mut Vec<u8>, chunk: &[u8], cap: usize) {
    let room = cap.saturating_sub(target.len());
    if room > 0 {
        target.extend_from_slice(&chunk[..chunk.len().min(room)]);
    }
}

// ---------------------------------------------------------------------------
// Output parsing
// ---------------------------------------------------------------------------

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut characters = input.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            match characters.peek() {
                Some('[') => {
                    characters.next();
                    for tail in characters.by_ref() {
                        if ('@'..='~').contains(&tail) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    characters.next();
                    let mut previous = '\0';
                    for tail in characters.by_ref() {
                        if tail == '\u{7}' || (previous == '\u{1b}' && tail == '\\') {
                            break;
                        }
                        previous = tail;
                    }
                }
                Some(_) => {
                    characters.next();
                }
                None => {}
            }
        } else if character != '\r' {
            out.push(character);
        }
    }
    out
}

fn is_failure_marker(line: &str) -> bool {
    line.contains("FAILED") || line.starts_with("--- FAIL") || line.contains('✕')
}

fn is_section_header(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("failures:") || trimmed.contains("FAILURES")
}

/// The first failure section, without ANSI noise. Cargo and pytest put the
/// useful detail after a `failures:`/`FAILURES` header; jest/vitest and go use
/// per-test markers instead.
fn failure_excerpt(output: &str) -> Option<String> {
    let lines: Vec<&str> = output.lines().collect();
    let start = lines
        .iter()
        .position(|line| is_section_header(line))
        .or_else(|| lines.iter().position(|line| is_failure_marker(line)))?;
    Some(lines[start..].join("\n"))
}

fn count_matches(output: &str, pattern: &str) -> Option<u64> {
    let regex = regex::Regex::new(pattern).ok()?;
    let mut total: u64 = 0;
    let mut found = false;
    for captures in regex.captures_iter(output) {
        if let Some(value) = captures
            .get(1)
            .and_then(|value| value.as_str().parse::<u64>().ok())
        {
            total = total.saturating_add(value);
            found = true;
        }
    }
    found.then_some(total)
}

fn summary_line(runner: Runner, output: &RunOutput, cleaned: &str) -> String {
    let label = runner.name();
    let passed = count_matches(cleaned, r"(\d+)\s+passed");
    let failed = count_matches(cleaned, r"(\d+)\s+failed");
    if output.success {
        return match passed {
            Some(passed) => format!("PASS  {label}: {passed} passed"),
            None => format!("PASS  {label}"),
        };
    }
    if output.timed_out {
        return format!("TIMEOUT  {label}");
    }
    if runner == Runner::Go {
        let failing = cleaned
            .lines()
            .filter(|line| line.trim_start().starts_with("--- FAIL"))
            .count();
        if failing > 0 {
            return format!("FAIL  go: {failing} failing test(s)");
        }
    }
    match (failed, passed) {
        (Some(failed), Some(passed)) => format!("FAIL  {label}: {failed} failed, {passed} passed"),
        (Some(failed), None) => format!("FAIL  {label}: {failed} failed"),
        _ => match output.status {
            Some(code) => format!("FAIL  {label} (exit {code})"),
            None => format!("FAIL  {label}"),
        },
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            std::fs::write(dir.path().join(name), contents).expect("write fixture");
        }
        dir
    }

    #[test]
    fn detects_cargo_project() {
        let dir = temp_project(&[("Cargo.toml", "[package]\nname = \"x\"\n")]);
        assert_eq!(
            detect_runner(dir.path(), None).expect("cargo"),
            Runner::Cargo
        );
    }

    #[test]
    fn detects_vitest_and_jest_from_package_json() {
        let vitest = temp_project(&[(
            "package.json",
            r#"{"devDependencies":{"vitest":"^1.0.0"},"scripts":{"test":"vitest run"}}"#,
        )]);
        assert_eq!(
            detect_runner(vitest.path(), None).expect("vitest"),
            Runner::Vitest
        );
        let jest = temp_project(&[(
            "package.json",
            r#"{"devDependencies":{"jest":"^29.0.0"},"scripts":{"test":"jest"}}"#,
        )]);
        assert_eq!(
            detect_runner(jest.path(), None).expect("jest"),
            Runner::Jest
        );
        let npm = temp_project(&[("package.json", r#"{"scripts":{"test":"node check.js"}}"#)]);
        assert_eq!(detect_runner(npm.path(), None).expect("npm"), Runner::Npm);
    }

    #[test]
    fn detects_pytest_and_go() {
        let py = temp_project(&[("pyproject.toml", "[project]\nname = \"x\"\n")]);
        assert_eq!(
            detect_runner(py.path(), None).expect("pytest"),
            Runner::Pytest
        );
        let go = temp_project(&[("go.mod", "module example.com/x\n")]);
        assert_eq!(detect_runner(go.path(), None).expect("go"), Runner::Go);
    }

    #[test]
    fn unknown_project_names_the_markers() {
        let dir = temp_project(&[]);
        let error = detect_runner(dir.path(), None).expect_err("no runner");
        assert!(error.content.starts_with("[NO_TEST_RUNNER]"));
        assert!(error.content.contains("Cargo.toml"));
    }

    #[test]
    fn cargo_integration_path_becomes_test_target() {
        let dir = temp_project(&[("Cargo.toml", "")]);
        let path = dir.path().join("tests/unit.rs");
        std::fs::create_dir_all(dir.path().join("tests")).expect("tests dir");
        std::fs::write(&path, "").expect("test file");
        let invocation = build_invocation(dir.path(), Runner::Cargo, Some(&path), Some("parses"));
        let args: Vec<String> = invocation
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["test", "--test", "unit", "--", "parses"]);
        assert_eq!(invocation.display, "cargo test --test unit -- parses");
    }

    #[test]
    fn pytest_target_joins_file_and_test() {
        let dir = temp_project(&[("pyproject.toml", "")]);
        let path = dir.path().join("tests/test_api.py");
        let invocation = build_invocation(dir.path(), Runner::Pytest, Some(&path), Some("test_ok"));
        let args: Vec<String> = invocation
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(args, vec!["-m", "pytest", "tests/test_api.py::test_ok"]);
    }

    #[test]
    fn ansi_noise_is_stripped() {
        let raw = "\u{1b}[31mFAILED\u{1b}[0m test_x\r\nprogress\rdone\n";
        assert_eq!(strip_ansi(raw), "FAILED test_x\nprogressdone\n");
    }

    #[test]
    fn cargo_first_failure_section_is_returned() {
        let output = "\u{1b}[32mrunning 3 tests\u{1b}[0m\n\
test a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b stdout ----\nassertion `left == right` failed\n";
        let cleaned = strip_ansi(output);
        let excerpt = failure_excerpt(&cleaned).expect("failure section");
        assert!(excerpt.starts_with("failures:"));
        assert!(excerpt.contains("assertion `left == right` failed"));
    }

    #[test]
    fn vitest_failure_marker_is_found_without_section_header() {
        let output = " ✓ passes\n ✕ fails the thing\nTests  1 failed | 1 passed (2)\n";
        let excerpt = failure_excerpt(output).expect("marker");
        assert!(excerpt.starts_with(" ✕ fails the thing"));
    }

    #[test]
    fn go_failures_are_counted_in_summary() {
        let output = RunOutput {
            status: Some(1),
            success: false,
            text: String::new(),
            timed_out: false,
        };
        let cleaned = "--- FAIL: TestOne (0.00s)\n--- FAIL: TestTwo (0.00s)\nFAIL\tpkg\t0.01s\n";
        assert_eq!(
            summary_line(Runner::Go, &output, cleaned),
            "FAIL  go: 2 failing test(s)"
        );
    }

    #[test]
    fn counts_are_summed_across_test_binaries() {
        let output =
            "test result: FAILED. 3 passed; 1 failed\ntest result: ok. 4 passed; 0 failed\n";
        assert_eq!(count_matches(output, r"(\d+)\s+passed"), Some(7));
        assert_eq!(count_matches(output, r"(\d+)\s+failed"), Some(1));
    }

    #[test]
    fn result_is_capped() {
        let long = "x".repeat(4000);
        assert_eq!(
            truncate_chars(&long, MAX_RESULT_CHARS).chars().count(),
            MAX_RESULT_CHARS + 1
        );
    }
}

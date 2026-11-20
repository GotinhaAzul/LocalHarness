//! Project detection: the runner detection shared with the `test` tool, and
//! the detected verify step candidates that `verify`, `tot doctor` and the
//! startup prompt propose.
//!
//! Detection reads manifest and config files and returns strings; deciding to
//! run one is the caller's job, and one that needs the user's consent. The one
//! process it starts is a Python import probe, since whether pytest is
//! installed is not visible in the project tree. Nothing from the repository
//! runs.

use std::ffi::OsString;
use std::path::Path;

use serde_json::Value;

use crate::tools::ToolResult;

/// Files whose presence means the directory is a Python project. `test` uses
/// this list to pick a Python runner; verify detection reports the ones it
/// finds.
const PYTHON_MARKERS: [&str; 7] = [
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "pytest.ini",
    "tox.ini",
    "conftest.py",
    "requirements.txt",
];

/// The npm-generated placeholder test script, which runs no tests.
const NPM_PLACEHOLDER_TEST: &str = "echo \"Error: no test specified\" && exit 1";

/// Verify step timeout written into config by [`DetectedStep::to_config`].
/// Matches `default_step_timeout` in `config.rs`.
pub const DETECTED_STEP_TIMEOUT_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// Test runner detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Runner {
    Cargo,
    Vitest,
    Jest,
    Pytest,
    Unittest,
    Go,
    Npm,
}

impl Runner {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Runner::Cargo => "cargo",
            Runner::Vitest => "vitest",
            Runner::Jest => "jest",
            Runner::Pytest => "pytest",
            Runner::Unittest => "unittest",
            Runner::Go => "go",
            Runner::Npm => "npm",
        }
    }
}

/// Pick the runner for one test file (or the whole project when `path` is
/// `None`). The file extension and the manifests are read; the only process
/// started is the pytest import probe.
pub(crate) fn detect_runner(cwd: &Path, path: Option<&Path>) -> Result<Runner, ToolResult> {
    detect_runner_with(cwd, path, pytest_available)
}

/// [`detect_runner`] with the pytest probe injected so tests can exercise both
/// the pytest and the stdlib unittest path without depending on the machine.
fn detect_runner_with(
    cwd: &Path,
    path: Option<&Path>,
    pytest_available: impl Fn() -> bool,
) -> Result<Runner, ToolResult> {
    let extension = path
        .and_then(|path| path.extension())
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    let python_test = path.is_some_and(is_python_test_file);

    match extension.as_deref() {
        Some("rs") if cwd.join("Cargo.toml").exists() => return Ok(Runner::Cargo),
        Some("go") if cwd.join("go.mod").exists() => return Ok(Runner::Go),
        Some("py") if python_project(cwd) || python_test => {
            return Ok(python_runner(cwd, pytest_available()));
        }
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
    if cwd.join("package.json").is_file()
        && let Some(runner) = node_runner(cwd)
    {
        return Ok(runner);
    }
    if python_project(cwd) || !python_test_files(cwd).is_empty() {
        return Ok(python_runner(cwd, pytest_available()));
    }
    if cwd.join("go.mod").exists() {
        return Ok(Runner::Go);
    }

    Err(ToolResult::err(
        "NO_TEST_RUNNER",
        format!(
            "no test runner detected in {}; looked for Cargo.toml, package.json with vitest/jest or an npm test script, pyproject.toml/setup.py/pytest.ini, test_*.py or *_test.py, and go.mod. Pass \"path\" to a test file or add one of those files.",
            cwd.display()
        ),
    ))
}

fn python_project(cwd: &Path) -> bool {
    PYTHON_MARKERS
        .iter()
        .any(|marker| cwd.join(marker).exists())
}

/// Prefer pytest when the project configures it or the interpreter can import
/// it; otherwise the stdlib unittest runner is always available with Python.
fn python_runner(cwd: &Path, pytest_available: bool) -> Runner {
    if pytest_available || pytest_configured(cwd) {
        Runner::Pytest
    } else {
        Runner::Unittest
    }
}

fn pytest_configured(cwd: &Path) -> bool {
    ["pytest.ini", "conftest.py"]
        .iter()
        .any(|marker| cwd.join(marker).exists())
        || std::fs::read_to_string(cwd.join("pyproject.toml"))
            .is_ok_and(|text| text.contains("[tool.pytest"))
        || std::fs::read_to_string(cwd.join("setup.cfg"))
            .is_ok_and(|text| text.contains("[tool:pytest]"))
}

/// Whether `python3 -m pytest` would be able to import pytest. This is the one
/// detection step that starts a process: a package's presence is not visible in
/// the project tree, and the import check is cheap and side-effect free. The
/// answer is cached for the process.
fn pytest_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new(python_program())
            .args(["-c", "import pytest"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
}

/// The Python interpreter the `test` tool and [`pytest_available`] use:
/// `python3` when it is on `PATH`, else `python`.
pub(crate) fn python_program() -> OsString {
    let search = std::env::var_os("PATH");
    let has_python3 = search.is_some_and(|search| {
        std::env::split_paths(&search)
            .map(|dir| dir.join("python3"))
            .any(|candidate| is_executable(&candidate))
    });
    if has_python3 {
        OsString::from("python3")
    } else {
        OsString::from("python")
    }
}

/// A `.py` file named `test_*.py` or `*_test.py` is a Python test even when the
/// directory has no manifest.
fn is_python_test_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_python_test_name)
}

fn is_python_test_name(name: &str) -> bool {
    let path = Path::new(name);
    let python = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("py"));
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    python && (stem.starts_with("test_") || stem.ends_with("_test"))
}

/// Top-level Python test files, sorted. A directory with only these and no
/// manifest is still a Python test project.
fn python_test_files(cwd: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(cwd) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .filter(|name| is_python_test_name(name))
        .collect();
    names.sort();
    names
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

pub(crate) fn is_executable(path: &Path) -> bool {
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
// Verify step detection
// ---------------------------------------------------------------------------

/// One candidate verification step: a short name and the command line that
/// would run it. The command is display text, not a shell string; it is run
/// as an argv split (see [`DetectedStep::to_config`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetectedStep {
    pub name: String,
    pub command: String,
}

impl DetectedStep {
    fn new(name: &str, command: impl Into<String>) -> Self {
        Self {
            name: name.to_string(),
            command: command.into(),
        }
    }

    /// Convert to the config type so a detected step can be written to
    /// `.harness/verify.toml` or a `[verify]` section. Detected commands
    /// contain no quoting, so splitting on whitespace is lossless. The
    /// timeout is the config default, 600 seconds.
    pub fn to_config(&self) -> crate::config::VerifyStep {
        crate::config::VerifyStep {
            name: self.name.clone(),
            command: self
                .command
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            timeout_secs: DETECTED_STEP_TIMEOUT_SECS,
        }
    }
}

/// Candidate verification for a project, from manifest files only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DetectedVerify {
    /// Ecosystem of the detected project: `"cargo"`, `"npm"` (any Node package
    /// manager), `"python"` or `"go"`. `None` when nothing was detected.
    pub kind: Option<&'static str>,
    /// Steps in run order: build or type check, lint, then test.
    pub steps: Vec<DetectedStep>,
    /// Manifest and config files, or Python test files, that made the project
    /// count as detected.
    pub markers: Vec<String>,
}

/// Detect candidate verify steps from manifest files and Python test files.
/// Pure apart from the pytest import probe: reads files, never executes
/// repository code.
///
/// Only the first ecosystem found is reported, checked in the order Cargo,
/// Node, Python, Go. A polyglot repository therefore gets one predictable set
/// of steps rather than a mix that the user has to prune.
pub fn detect_verify_steps(cwd: &Path) -> DetectedVerify {
    detect_verify_steps_with(cwd, pytest_available)
}

fn detect_verify_steps_with(cwd: &Path, pytest_available: impl Fn() -> bool) -> DetectedVerify {
    if cwd.join("Cargo.toml").exists() {
        return cargo_steps(cwd);
    }
    if cwd.join("package.json").is_file() {
        return node_steps(cwd);
    }
    let test_files = python_test_files(cwd);
    if python_project(cwd) || !test_files.is_empty() {
        return python_steps(cwd, &test_files, pytest_available());
    }
    if cwd.join("go.mod").exists() {
        return go_steps();
    }
    DetectedVerify::default()
}

fn cargo_steps(cwd: &Path) -> DetectedVerify {
    let mut steps = vec![DetectedStep::new("check", "cargo check --all-targets")];
    if ["clippy.toml", ".clippy.toml"]
        .iter()
        .any(|marker| cwd.join(marker).exists())
    {
        steps.push(DetectedStep::new(
            "clippy",
            "cargo clippy --all-targets -- -D warnings",
        ));
    }
    steps.push(DetectedStep::new("test", "cargo test"));
    DetectedVerify {
        kind: Some("cargo"),
        steps,
        markers: vec!["Cargo.toml".to_string()],
    }
}

fn node_steps(cwd: &Path) -> DetectedVerify {
    let parsed: Value = std::fs::read_to_string(cwd.join("package.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or(Value::Null);
    let script = |name: &str| {
        parsed
            .get("scripts")
            .and_then(|scripts| scripts.get(name))
            .and_then(Value::as_str)
    };
    let manager = package_manager(cwd);
    let mut steps = Vec::new();

    if script("typecheck").is_some() {
        steps.push(DetectedStep::new(
            "typecheck",
            format!("{manager} run typecheck"),
        ));
    } else if cwd.join("tsconfig.json").is_file() {
        steps.push(DetectedStep::new("typecheck", "npx tsc --noEmit"));
    }

    if script("lint").is_some() {
        steps.push(DetectedStep::new("lint", format!("{manager} run lint")));
    }

    // The npm placeholder runs no tests, so a real test tool wins over it.
    let real_test_script = script("test").is_some_and(|script| !is_placeholder_test(script));
    if real_test_script {
        // `bun test` is Bun's own test runner, not the package script; only
        // `bun run test` runs what `scripts.test` says.
        let command = if manager == "bun" {
            "bun run test".to_string()
        } else {
            format!("{manager} test")
        };
        steps.push(DetectedStep::new("test", command));
    } else if has_dependency(&parsed, "vitest") {
        steps.push(DetectedStep::new("test", "npx vitest run"));
    } else if has_dependency(&parsed, "jest") {
        steps.push(DetectedStep::new("test", "npx jest"));
    }

    DetectedVerify {
        kind: Some("npm"),
        steps,
        markers: vec!["package.json".to_string()],
    }
}

/// The package manager implied by the lockfile, else npm.
fn package_manager(cwd: &Path) -> &'static str {
    if cwd.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if cwd.join("yarn.lock").exists() {
        "yarn"
    } else if cwd.join("bun.lockb").exists() || cwd.join("bun.lock").exists() {
        "bun"
    } else {
        "npm"
    }
}

fn is_placeholder_test(script: &str) -> bool {
    let script = script.trim();
    script == NPM_PLACEHOLDER_TEST || script.contains("no test specified")
}

fn has_dependency(parsed: &Value, name: &str) -> bool {
    ["dependencies", "devDependencies", "peerDependencies"]
        .iter()
        .any(|key| {
            parsed
                .get(key)
                .and_then(Value::as_object)
                .is_some_and(|map| map.contains_key(name))
        })
}

fn python_steps(cwd: &Path, test_files: &[String], pytest_available: bool) -> DetectedVerify {
    let pyproject = std::fs::read_to_string(cwd.join("pyproject.toml")).unwrap_or_default();
    let mut markers: Vec<String> = PYTHON_MARKERS
        .iter()
        .filter(|marker| cwd.join(marker).exists())
        .map(|marker| (*marker).to_string())
        .collect();
    if !test_files.is_empty() {
        for name in test_files.iter().take(5) {
            markers.push(name.clone());
        }
        if test_files.len() > 5 {
            markers.push(format!("and {} more test files", test_files.len() - 5));
        }
    }
    let mut steps = Vec::new();

    if cwd.join("mypy.ini").exists() || pyproject.contains("[tool.mypy") {
        steps.push(DetectedStep::new("typecheck", "mypy ."));
    }
    if cwd.join("ruff.toml").exists()
        || cwd.join(".ruff.toml").exists()
        || pyproject.contains("[tool.ruff")
    {
        steps.push(DetectedStep::new("lint", "ruff check"));
    }
    let test = if python_runner(cwd, pytest_available) == Runner::Pytest {
        "python3 -m pytest -x"
    } else {
        "python3 -m unittest"
    };
    steps.push(DetectedStep::new("test", test));

    DetectedVerify {
        kind: Some("python"),
        steps,
        markers,
    }
}

fn go_steps() -> DetectedVerify {
    DetectedVerify {
        kind: Some("go"),
        steps: vec![
            DetectedStep::new("vet", "go vet ./..."),
            DetectedStep::new("test", "go test ./..."),
        ],
        markers: vec!["go.mod".to_string()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("fixture dir");
            }
            std::fs::write(path, contents).expect("write fixture");
        }
        dir
    }

    fn steps(detected: &DetectedVerify) -> Vec<(&str, &str)> {
        detected
            .steps
            .iter()
            .map(|step| (step.name.as_str(), step.command.as_str()))
            .collect()
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
            detect_runner_with(py.path(), None, || true).expect("pytest"),
            Runner::Pytest
        );
        let go = temp_project(&[("go.mod", "module example.com/x\n")]);
        assert_eq!(detect_runner(go.path(), None).expect("go"), Runner::Go);
    }

    #[test]
    fn bare_python_test_file_needs_no_manifest() {
        let dir = temp_project(&[(
            "test_x.py",
            "import unittest\nclass T(unittest.TestCase):\n    def test_ok(self):\n        pass\n",
        )]);
        let path = dir.path().join("test_x.py");
        assert_eq!(
            detect_runner_with(dir.path(), Some(&path), || false).expect("unittest"),
            Runner::Unittest
        );
        assert_eq!(
            detect_runner_with(dir.path(), Some(&path), || true).expect("pytest"),
            Runner::Pytest
        );
        assert_eq!(
            detect_runner_with(dir.path(), None, || false).expect("unittest"),
            Runner::Unittest
        );
        assert_eq!(
            detect_runner_with(dir.path(), None, || true).expect("pytest"),
            Runner::Pytest
        );
    }

    #[test]
    fn other_python_files_still_need_a_manifest() {
        let dir = temp_project(&[("helper.py", "VALUE = 1\n")]);
        let path = dir.path().join("helper.py");
        let error = detect_runner_with(dir.path(), Some(&path), || true).expect_err("no runner");
        assert!(error.content.starts_with("[NO_TEST_RUNNER]"));
    }

    #[test]
    fn python_test_names_follow_the_pytest_convention() {
        for name in ["test_x.py", "x_test.py", "test_.py", "test_x.PY"] {
            assert!(is_python_test_name(name), "{name}");
        }
        for name in [
            "x.py",
            "testing.py",
            "test.py",
            "test_x.txt",
            "xtest.py",
            "TEST_X.PY",
        ] {
            assert!(!is_python_test_name(name), "{name}");
        }
    }

    #[test]
    fn project_pytest_config_wins_over_the_import_probe() {
        let dir = temp_project(&[(
            "pyproject.toml",
            "[project]\nname = \"x\"\n[tool.pytest.ini_options]\n",
        )]);
        assert_eq!(
            detect_runner_with(dir.path(), None, || false).expect("pytest"),
            Runner::Pytest
        );
    }

    #[test]
    fn unknown_project_names_the_markers() {
        let dir = temp_project(&[]);
        let error = detect_runner(dir.path(), None).expect_err("no runner");
        assert!(error.content.starts_with("[NO_TEST_RUNNER]"));
        assert!(error.content.contains("Cargo.toml"));
    }

    #[test]
    fn empty_directory_detects_nothing() {
        let dir = temp_project(&[]);
        assert_eq!(detect_verify_steps(dir.path()), DetectedVerify::default());
    }

    #[test]
    fn cargo_steps_are_check_and_test() {
        let dir = temp_project(&[("Cargo.toml", "")]);
        let detected = detect_verify_steps(dir.path());
        assert_eq!(detected.kind, Some("cargo"));
        assert_eq!(detected.markers, vec!["Cargo.toml".to_string()]);
        assert_eq!(
            steps(&detected),
            vec![
                ("check", "cargo check --all-targets"),
                ("test", "cargo test"),
            ]
        );
    }

    #[test]
    fn cargo_clippy_step_needs_a_clippy_config() {
        let dir = temp_project(&[("Cargo.toml", ""), (".clippy.toml", "")]);
        let detected = detect_verify_steps(dir.path());
        assert_eq!(
            steps(&detected),
            vec![
                ("check", "cargo check --all-targets"),
                ("clippy", "cargo clippy --all-targets -- -D warnings"),
                ("test", "cargo test"),
            ]
        );
    }

    #[test]
    fn node_uses_the_lockfile_package_manager() {
        let dir = temp_project(&[
            (
                "package.json",
                r#"{"scripts":{"typecheck":"tsc --noEmit","lint":"eslint .","test":"vitest run"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
        ]);
        let detected = detect_verify_steps(dir.path());
        assert_eq!(detected.kind, Some("npm"));
        assert_eq!(
            steps(&detected),
            vec![
                ("typecheck", "pnpm run typecheck"),
                ("lint", "pnpm run lint"),
                ("test", "pnpm test"),
            ]
        );
    }

    #[test]
    fn node_placeholder_test_falls_back_to_vitest() {
        let dir = temp_project(&[
            (
                "package.json",
                r#"{"devDependencies":{"vitest":"^1.0.0"},"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
            ),
            ("tsconfig.json", "{}"),
        ]);
        let detected = detect_verify_steps(dir.path());
        assert_eq!(
            steps(&detected),
            vec![
                ("typecheck", "npx tsc --noEmit"),
                ("test", "npx vitest run"),
            ]
        );
    }

    #[test]
    fn bun_runs_the_package_test_script_not_its_own_runner() {
        let dir = temp_project(&[
            ("package.json", r#"{"scripts":{"test":"playwright test"}}"#),
            ("bun.lock", ""),
        ]);
        let detected = detect_verify_steps(dir.path());
        assert_eq!(steps(&detected), vec![("test", "bun run test")]);
    }

    #[test]
    fn node_without_scripts_still_reports_dependencies() {
        let dir = temp_project(&[
            ("package.json", r#"{"dependencies":{"jest":"^29.0.0"}}"#),
            ("bun.lockb", ""),
        ]);
        let detected = detect_verify_steps(dir.path());
        assert_eq!(steps(&detected), vec![("test", "npx jest")]);
    }

    #[test]
    fn python_steps_follow_ruff_and_mypy_config() {
        let dir = temp_project(&[
            (
                "pyproject.toml",
                "[project]\nname = \"x\"\n[tool.ruff]\nline-length = 100\n",
            ),
            ("mypy.ini", "[mypy]\n"),
            ("setup.cfg", ""),
        ]);
        let detected = detect_verify_steps_with(dir.path(), || true);
        assert_eq!(detected.kind, Some("python"));
        assert_eq!(
            detected.markers,
            vec!["pyproject.toml".to_string(), "setup.cfg".to_string()]
        );
        assert_eq!(
            steps(&detected),
            vec![
                ("typecheck", "mypy ."),
                ("lint", "ruff check"),
                ("test", "python3 -m pytest -x"),
            ]
        );
    }

    #[test]
    fn python_without_lint_config_only_tests() {
        let dir = temp_project(&[("pyproject.toml", "[project]\nname = \"x\"\n")]);
        let detected = detect_verify_steps_with(dir.path(), || true);
        assert_eq!(steps(&detected), vec![("test", "python3 -m pytest -x")]);
    }

    #[test]
    fn bare_python_test_dir_proposes_the_stdlib_runner() {
        let dir = temp_project(&[("test_x.py", "import unittest\n")]);
        let detected = detect_verify_steps_with(dir.path(), || false);
        assert_eq!(detected.kind, Some("python"));
        assert_eq!(detected.markers, vec!["test_x.py".to_string()]);
        assert_eq!(steps(&detected), vec![("test", "python3 -m unittest")]);
    }

    #[test]
    fn go_steps_are_vet_and_test() {
        let dir = temp_project(&[("go.mod", "module example.com/x\n")]);
        let detected = detect_verify_steps(dir.path());
        assert_eq!(detected.kind, Some("go"));
        assert_eq!(
            steps(&detected),
            vec![("vet", "go vet ./..."), ("test", "go test ./...")]
        );
    }

    #[test]
    fn first_ecosystem_wins() {
        let dir = temp_project(&[("Cargo.toml", ""), ("go.mod", "module x\n")]);
        let detected = detect_verify_steps(dir.path());
        assert_eq!(detected.kind, Some("cargo"));
        assert!(!detected.markers.contains(&"go.mod".to_string()));
    }

    #[test]
    fn detected_step_converts_to_config() {
        let step = DetectedStep::new("clippy", "cargo clippy --all-targets -- -D warnings");
        let config = step.to_config();
        assert_eq!(config.name, "clippy");
        assert_eq!(
            config.command,
            vec!["cargo", "clippy", "--all-targets", "--", "-D", "warnings"]
        );
        assert_eq!(config.timeout_secs, 600);
    }
}

//! Source locations named by failing build and test output.
//!
//! Compilers and test runners print where they failed in a handful of stable
//! shapes. After a failure the next useful step is almost always to look at
//! exactly that spot, so the harness reads it and attaches it instead of
//! spending a model round trip on a `read`.
//!
//! Recognised shapes, by example:
//! - `src/lib.rs:12:5` (rustc `-->`, Rust panics, Go, gcc, eslint, vitest)
//! - `src/a.ts(12,5): error` (tsc)
//! - `File "pkg/mod.py", line 12` (Python tracebacks)
//! - `tests/test_a.py:12: AssertionError` (pytest)
//!
//! Only files inside the working directory that exist count, so frames in the
//! standard library, `~/.cargo/registry`, `node_modules`, or site-packages are
//! skipped.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::tools::ToolContext;
use crate::tools::read::split_lines;
use crate::tools::write::lock_reads;

/// Locations attached per failure.
const MAX_LOCATIONS: usize = 3;
/// Lines shown on each side of the named line.
const CONTEXT_LINES: usize = 3;
/// A long source line is cut to this many characters.
const LINE_CHARS: usize = 160;
/// Directories whose files are never the project's own code.
const SKIP_DIRS: &[&str] = &[
    "target/",
    "node_modules/",
    ".venv/",
    "venv/",
    "site-packages/",
    "dist/",
    "build/",
    ".git/",
];

static PATH_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?x)
        (?:File\ "(?P<py>[^"]+)",\ line\ (?P<pyline>\d+))
        |
        (?P<path>(?:\.{1,2}/)?[A-Za-z0-9_@+\-][A-Za-z0-9_@+.\-/]*\.[A-Za-z0-9]{1,8})
        (?:
            :(?P<line>\d+)(?::\d+)?
          | \((?P<tsline>\d+),\d+\)
        )
        "#,
    )
    .expect("location regex compiles")
});

/// One place in a project file that failing output points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Path as printed, relative to the working directory.
    pub display: String,
    pub line: usize,
}

/// Locations in `output`, in the order they first appear, deduplicated,
/// limited to existing files under `cwd`.
pub fn find(output: &str, cwd: &Path) -> Vec<Location> {
    let mut found: Vec<Location> = Vec::new();
    for caps in PATH_LINE.captures_iter(output) {
        let (raw, line) = match (caps.name("py"), caps.name("path")) {
            (Some(path), _) => (path.as_str(), caps.name("pyline")),
            (None, Some(path)) => (
                path.as_str(),
                caps.name("line").or_else(|| caps.name("tsline")),
            ),
            _ => continue,
        };
        let Some(line) = line.and_then(|line| line.as_str().parse::<usize>().ok()) else {
            continue;
        };
        if line == 0 {
            continue;
        }
        let Some(display) = project_relative(raw, cwd) else {
            continue;
        };
        let location = Location { display, line };
        if !found.contains(&location) {
            found.push(location);
            if found.len() == MAX_LOCATIONS {
                break;
            }
        }
    }
    found
}

/// `raw` as a path relative to `cwd`, when it names an existing project file.
fn project_relative(raw: &str, cwd: &Path) -> Option<String> {
    let path = Path::new(raw);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let canonical = absolute.canonicalize().ok()?;
    let root = cwd.canonicalize().ok()?;
    let relative = canonical.strip_prefix(&root).ok()?;
    if !canonical.is_file() {
        return None;
    }
    let display = relative.to_string_lossy().replace('\\', "/");
    if SKIP_DIRS
        .iter()
        .any(|dir| display.starts_with(dir) || display.contains(&format!("/{dir}")))
    {
        return None;
    }
    Some(display)
}

/// Read each location's surrounding lines, record them as read, and render
/// them as one block within `budget` characters. Empty when nothing fits or
/// nothing was found.
pub fn attach(ctx: &ToolContext, locations: &[Location], budget: usize) -> String {
    let mut out = String::new();
    for location in locations {
        let Ok(path) = ctx.resolve(&location.display) else {
            continue;
        };
        let Some(stamp) = crate::tools::ReadTracker::stamp_for(&path) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let lines = split_lines(&text);
        if location.line > lines.len() {
            continue;
        }
        let start = location.line.saturating_sub(CONTEXT_LINES).max(1);
        let end = (location.line + CONTEXT_LINES).min(lines.len());
        let mut block = format!("{}:{}\n", location.display, location.line);
        for number in start..=end {
            let line = lines[number - 1].trim_end_matches(['\n', '\r']);
            let line: String = if line.chars().count() > LINE_CHARS {
                let cut: String = line.chars().take(LINE_CHARS).collect();
                format!("{cut}…")
            } else {
                line.to_string()
            };
            let marker = if number == location.line { '>' } else { ' ' };
            block.push_str(&format!("{marker}{number:>5}  {line}\n"));
        }
        if out.len() + block.len() > budget {
            break;
        }
        lock_reads(ctx).record_range(&path, stamp, lines.len(), start, end);
        out.push_str(&block);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/x")).unwrap();
        let body: String = (1..=30).map(|n| format!("line {n}\n")).collect();
        for file in [
            "src/lib.rs",
            "src/a.ts",
            "tests/test_a.py",
            "node_modules/x/i.js",
        ] {
            std::fs::write(dir.path().join(file), &body).unwrap();
        }
        dir
    }

    fn at(display: &str, line: usize) -> Location {
        Location {
            display: display.into(),
            line,
        }
    }

    #[test]
    fn finds_the_shapes_compilers_and_runners_print() {
        let dir = project();
        let output = "\
error[E0308]: mismatched types
  --> src/lib.rs:12:5
src/a.ts(7,3): error TS2322: Type 'string' is not assignable
  File \"tests/test_a.py\", line 9, in test_parse
";
        assert_eq!(
            find(output, dir.path()),
            vec![
                at("src/lib.rs", 12),
                at("src/a.ts", 7),
                at("tests/test_a.py", 9)
            ]
        );
    }

    #[test]
    fn skips_files_outside_the_project_and_dependencies() {
        let dir = project();
        let output = "\
thread 'main' panicked at /rustc/abc/library/core/src/option.rs:12:5
    at Object.<anonymous> (node_modules/x/i.js:4:1)
missing.rs:3:1
./src/lib.rs:20:1
./src/lib.rs:20:9
";
        assert_eq!(find(output, dir.path()), vec![at("src/lib.rs", 20)]);
    }

    #[test]
    fn stops_after_the_first_few() {
        let dir = project();
        let output: String = (1..=10).map(|n| format!("src/lib.rs:{n}:1\n")).collect();
        assert_eq!(find(&output, dir.path()).len(), MAX_LOCATIONS);
    }
}

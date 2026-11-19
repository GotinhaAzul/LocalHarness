//! Format a file's new content before an edit writes it.
//!
//! A model should not spend turns on whitespace, and review should not show
//! formatter noise. So when `[format] on_edit` is on, `edit`, `patch`, and
//! `write` pass the new content through the project's formatter and write the
//! result; the diff the model sees is the diff that landed.
//!
//! The rule that keeps this from ever adding churn: **a clean file stays
//! clean.** An existing file is formatted only when its content before the
//! edit was already what the formatter produces. A file the project does not
//! keep formatted is left exactly as the model wrote it. A new file is
//! formatted when the project shows it uses the formatter (a config file), or
//! for Go and Rust, whose formatters are the language default.
//!
//! Formatters read stdin and write stdout, so nothing touches the file until
//! the edit writes it once. A formatter that fails (a syntax error mid-edit),
//! times out, or is missing leaves the content unchanged. Only formatters on
//! `PATH` run, except a project's `node_modules/.bin/prettier`, which is
//! repository code and runs only in a trusted project.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::tools::ToolContext;

/// A formatter gets this long per run.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Files larger than this are written as given.
const MAX_BYTES: usize = 512 * 1024;

/// The content to write and the formatter that produced it, when one ran and
/// changed something.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formatted {
    pub content: String,
    pub by: Option<&'static str>,
}

impl Formatted {
    fn unchanged(content: String) -> Self {
        Self { content, by: None }
    }
}

/// A formatter invocation: the program, its arguments, and where to run it
/// so it finds the project's config.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Formatter {
    name: &'static str,
    program: PathBuf,
    args: Vec<String>,
    dir: PathBuf,
    /// The project declares this formatter (a config file), so new files are
    /// formatted too.
    configured: bool,
}

/// Format `after`, the new content of `path`. `before` is the content the
/// edit started from, `None` for a new file.
pub fn format_content(
    ctx: &ToolContext,
    path: &Path,
    before: Option<&str>,
    after: String,
) -> Formatted {
    if !ctx.format_on_edit || after.len() > MAX_BYTES || after.trim().is_empty() {
        return Formatted::unchanged(after);
    }
    let Some(formatter) = formatter_for(path, &ctx.cwd, ctx.trust) else {
        return Formatted::unchanged(after);
    };
    match before {
        Some(before) => {
            // Only a file the project keeps formatted.
            if run(&formatter, before).as_deref() != Some(before) {
                return Formatted::unchanged(after);
            }
        }
        None => {
            if !formatter.configured {
                return Formatted::unchanged(after);
            }
        }
    }
    match run(&formatter, &after) {
        Some(formatted) if formatted != after && !formatted.trim().is_empty() => Formatted {
            content: formatted,
            by: Some(formatter.name),
        },
        _ => Formatted::unchanged(after),
    }
}

fn formatter_for(path: &Path, cwd: &Path, trust: bool) -> Option<Formatter> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let dir = path.parent().unwrap_or(cwd).to_path_buf();
    match ext.as_str() {
        "rs" => {
            let program = on_path("rustfmt")?;
            let edition = rust_edition(&dir, cwd).unwrap_or_else(|| "2021".into());
            Some(Formatter {
                name: "rustfmt",
                program,
                args: vec![
                    "--edition".into(),
                    edition,
                    "--emit".into(),
                    "stdout".into(),
                ],
                configured: true,
                dir,
            })
        }
        "go" => Some(Formatter {
            name: "gofmt",
            program: on_path("gofmt")?,
            args: Vec::new(),
            configured: true,
            dir,
        }),
        "py" | "pyi" => python_formatter(path, &dir, cwd),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" | "mts" | "cts" | "json" | "css" | "scss"
        | "less" | "html" | "vue" | "svelte" | "md" | "yaml" | "yml" | "graphql" => {
            prettier(path, &dir, cwd, trust)
        }
        _ => None,
    }
}

fn python_formatter(path: &Path, dir: &Path, cwd: &Path) -> Option<Formatter> {
    let pyproject = find_up(dir, cwd, &["pyproject.toml"])
        .and_then(|file| std::fs::read_to_string(file).ok())
        .unwrap_or_default();
    let filename = path.display().to_string();
    let ruff_config = pyproject.contains("[tool.ruff")
        || find_up(dir, cwd, &["ruff.toml", ".ruff.toml"]).is_some();
    if ruff_config && let Some(program) = on_path("ruff") {
        return Some(Formatter {
            name: "ruff",
            program,
            args: vec![
                "format".into(),
                "--stdin-filename".into(),
                filename,
                "-".into(),
            ],
            configured: true,
            dir: dir.to_path_buf(),
        });
    }
    if pyproject.contains("[tool.black]")
        && let Some(program) = on_path("black")
    {
        return Some(Formatter {
            name: "black",
            program,
            args: vec!["-q".into(), "--stdin-filename".into(), filename, "-".into()],
            configured: true,
            dir: dir.to_path_buf(),
        });
    }
    None
}

const PRETTIER_CONFIGS: &[&str] = &[
    ".prettierrc",
    ".prettierrc.json",
    ".prettierrc.yaml",
    ".prettierrc.yml",
    ".prettierrc.json5",
    ".prettierrc.js",
    ".prettierrc.cjs",
    ".prettierrc.mjs",
    ".prettierrc.toml",
    "prettier.config.js",
    "prettier.config.cjs",
    "prettier.config.mjs",
];

fn prettier(path: &Path, dir: &Path, cwd: &Path, trust: bool) -> Option<Formatter> {
    let configured = find_up(dir, cwd, PRETTIER_CONFIGS).is_some()
        || find_up(dir, cwd, &["package.json"])
            .and_then(|file| std::fs::read_to_string(file).ok())
            .is_some_and(|text| text.contains("\"prettier\":"));
    if !configured {
        return None;
    }
    // A project's own prettier is repository code: trusted projects only.
    let local = cwd.join("node_modules/.bin/prettier");
    let program = if trust && local.is_file() {
        local
    } else {
        on_path("prettier")?
    };
    Some(Formatter {
        name: "prettier",
        program,
        args: vec!["--stdin-filepath".into(), path.display().to_string()],
        configured: true,
        dir: dir.to_path_buf(),
    })
}

/// The `edition` of the nearest `Cargo.toml`, following `edition.workspace`.
fn rust_edition(dir: &Path, cwd: &Path) -> Option<String> {
    let mut current = Some(dir);
    let mut inherit = false;
    while let Some(here) = current {
        if let Ok(text) = std::fs::read_to_string(here.join("Cargo.toml"))
            && let Ok(value) = text.parse::<toml::Table>()
        {
            let package = if inherit {
                value.get("workspace").and_then(|w| w.get("package"))
            } else {
                value.get("package")
            };
            match package.and_then(|p| p.get("edition")) {
                Some(toml::Value::String(edition)) => return Some(edition.clone()),
                Some(toml::Value::Table(table)) if table.get("workspace").is_some() => {
                    inherit = true;
                }
                _ if inherit => {}
                _ => {
                    if value.get("package").is_some() {
                        // A package without an edition is 2015.
                        return Some("2015".into());
                    }
                }
            }
        }
        if here == cwd {
            break;
        }
        current = here.parent();
    }
    None
}

/// The first of `names` found walking from `dir` up to `cwd`.
fn find_up(dir: &Path, cwd: &Path, names: &[&str]) -> Option<PathBuf> {
    let mut current = Some(dir);
    while let Some(here) = current {
        for name in names {
            let candidate = here.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        if here == cwd || !here.starts_with(cwd) {
            break;
        }
        current = here.parent();
    }
    None
}

fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}

/// Run the formatter on `input`; `None` on any failure.
fn run(formatter: &Formatter, input: &str) -> Option<String> {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(&formatter.program);
    command
        .args(&formatter.args)
        .current_dir(&formatter.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command.spawn().ok()?;
    let mut stdin = child.stdin.take()?;
    let input = input.to_string();
    let writer = std::thread::spawn(move || {
        let _ = stdin.write_all(input.as_bytes());
    });
    let stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = std::io::Read::read_to_end(&mut std::io::BufReader::new(stdout), &mut out);
        out
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let _ = writer.join();
    let out = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> ToolContext {
        let mut ctx = crate::tools::write::test_context(dir);
        ctx.format_on_edit = true;
        ctx
    }

    fn have(program: &str) -> bool {
        on_path(program).is_some()
    }

    #[test]
    fn a_clean_rust_file_stays_clean() {
        if !have("rustfmt") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        let before = "fn a() {}\n";
        let after = "fn a() {}\nfn   b( x:u8 ){ let _=x; }\n".to_string();
        let out = format_content(&ctx(dir.path()), &path, Some(before), after);
        assert_eq!(out.by, Some("rustfmt"));
        assert_eq!(out.content, "fn a() {}\nfn b(x: u8) {\n    let _ = x;\n}\n");
    }

    #[test]
    fn a_file_the_project_does_not_format_is_left_alone() {
        if !have("rustfmt") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        let before = "fn   a() {}\n";
        let after = "fn   a() {}\nfn   b() {}\n".to_string();
        let out = format_content(&ctx(dir.path()), &path, Some(before), after.clone());
        assert_eq!(out, Formatted::unchanged(after));
    }

    #[test]
    fn content_that_does_not_parse_is_written_as_given() {
        if !have("rustfmt") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lib.rs");
        let after = "fn a( {\n".to_string();
        let out = format_content(&ctx(dir.path()), &path, Some("fn a() {}\n"), after.clone());
        assert_eq!(out, Formatted::unchanged(after));
    }

    #[test]
    fn turned_off_or_unknown_files_are_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let mut off = ctx(dir.path());
        off.format_on_edit = false;
        let after = "fn   b() {}\n".to_string();
        let out = format_content(
            &off,
            &dir.path().join("lib.rs"),
            Some("fn a() {}\n"),
            after.clone(),
        );
        assert_eq!(out.by, None);
        let out = format_content(
            &ctx(dir.path()),
            &dir.path().join("notes.txt"),
            None,
            "x  y\n".into(),
        );
        assert_eq!(out.by, None);
    }

    #[test]
    fn prettier_needs_a_project_config_and_trust_for_a_local_binary() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.ts");
        assert!(
            prettier(&file, dir.path(), dir.path(), true).is_none(),
            "no config"
        );
        std::fs::write(dir.path().join(".prettierrc"), "{}").unwrap();
        let bin = dir.path().join("node_modules/.bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("prettier"), "#!/bin/sh\ncat\n").unwrap();
        let trusted =
            prettier(&file, dir.path(), dir.path(), true).expect("trusted local prettier");
        assert_eq!(trusted.program, bin.join("prettier"));
        let untrusted = prettier(&file, dir.path(), dir.path(), false);
        assert!(untrusted.is_none_or(|f| f.program != bin.join("prettier")));
    }

    #[test]
    fn rust_edition_follows_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[workspace]\nmembers=[\"a\"]\n[workspace.package]\nedition = \"2024\"\n",
        )
        .unwrap();
        let member = dir.path().join("a");
        std::fs::create_dir_all(member.join("src")).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname=\"a\"\nedition.workspace = true\n",
        )
        .unwrap();
        assert_eq!(
            rust_edition(&member.join("src"), dir.path()).as_deref(),
            Some("2024")
        );
        std::fs::write(
            member.join("Cargo.toml"),
            "[package]\nname=\"a\"\nedition=\"2021\"\n",
        )
        .unwrap();
        assert_eq!(
            rust_edition(&member.join("src"), dir.path()).as_deref(),
            Some("2021")
        );
    }
}

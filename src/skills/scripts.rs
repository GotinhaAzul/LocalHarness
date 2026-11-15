//! Detection of commands that would execute a script bundled with a skill.
//!
//! The harness never runs a skill script on its own. A `bash` or `bg` call
//! that targets one needs explicit approval: ACP can ask, and a frontend
//! without an approval channel refuses with the exact command so the user can
//! run it themselves.

use std::path::{Path, PathBuf};

use crate::paths::Paths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledScript {
    pub skill: String,
    pub path: PathBuf,
    pub command: String,
}

/// Inspect one shell command. Returns the first bundled script it names.
pub fn bundled_script_in_command(
    paths: &Paths,
    trust: bool,
    cwd: &Path,
    command: &str,
) -> Option<BundledScript> {
    let roots = skill_roots(paths, trust);
    if roots.is_empty() {
        return None;
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut base = cwd.to_path_buf();
    for (index, raw) in tokens.iter().enumerate() {
        let token = trim(raw);
        if token.is_empty() || token.starts_with('-') || token.contains('=') {
            continue;
        }
        if token == "cd" {
            if let Some(next) = tokens.get(index + 1).map(|next| trim(next))
                && !next.is_empty()
            {
                base = if Path::new(next).is_absolute() {
                    PathBuf::from(next)
                } else {
                    base.join(next)
                };
            }
            continue;
        }
        if matches!(
            token,
            "bash" | "sh" | "env" | "exec" | "nohup" | "timeout" | "sudo"
        ) {
            continue;
        }
        let candidate = if Path::new(token).is_absolute() {
            PathBuf::from(token)
        } else {
            base.join(token)
        };
        let Ok(real) = candidate.canonicalize() else {
            continue;
        };
        if !real.is_file() {
            continue;
        }
        let Some(root) = roots.iter().find(|root| real.starts_with(root)) else {
            continue;
        };
        if !looks_like_script(&real, root) {
            continue;
        }
        let relative = real.strip_prefix(root).ok()?;
        let skill = relative
            .components()
            .next()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_else(|| "skill".into());
        return Some(BundledScript {
            skill,
            path: real,
            command: command.to_string(),
        });
    }
    None
}

fn trim(token: &str) -> &str {
    token.trim_matches(|ch| {
        matches!(
            ch,
            '"' | '\'' | '`' | '(' | ')' | ';' | '&' | '|' | '<' | '>' | ','
        )
    })
}

fn skill_roots(paths: &Paths, trust: bool) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push = |root: PathBuf| {
        if let Ok(real) = root.canonicalize()
            && !roots.iter().any(|existing| existing == &real)
        {
            roots.push(real);
        }
    };
    push(paths.skills_dir());
    if let Some(home) = &paths.home_dir {
        push(home.join(".claude/skills"));
        push(home.join(".agents/skills"));
    }
    if trust {
        push(paths.project_dir().join("skills"));
        push(paths.cwd.join(".claude/skills"));
        push(paths.cwd.join(".agents/skills"));
    }
    roots
}

fn looks_like_script(path: &Path, root: &Path) -> bool {
    let relative = match path.strip_prefix(root) {
        Ok(relative) => relative,
        Err(_) => return false,
    };
    let components: Vec<&str> = relative
        .components()
        .map(|component| component.as_os_str().to_str().unwrap_or(""))
        .collect();
    // A skill file itself is not a script. Anything in a scripts/bin directory,
    // with an executable bit, or with a script extension is.
    if components.len() < 3 {
        return false;
    }
    if components
        .iter()
        .any(|component| *component == "scripts" || *component == "bin")
    {
        return true;
    }
    if let Some(extension) = path.extension().and_then(|extension| extension.to_str())
        && matches!(
            extension.to_ascii_lowercase().as_str(),
            "sh" | "bash" | "zsh" | "py" | "rb" | "pl" | "js" | "ts" | "ps1"
        )
    {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path)
            && metadata.permissions().mode() & 0o111 != 0
        {
            return true;
        }
    }
    false
}

/// The error a frontend without an approval channel shows instead of running
/// the script. It names the command and the file.
pub fn approval_required(script: &BundledScript) -> String {
    format!(
        "SKILL_SCRIPT_APPROVAL: the command runs `{}`, a script bundled with skill `{}`. Bundled scripts are never run automatically and need per-execution approval.\n\nCommand: {}\n\nApprove this exact command in the frontend, or run it yourself and paste the result. Nothing was executed.",
        script.path.display(),
        script.skill,
        script.command
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Paths, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Paths::discover(dir.path()).unwrap();
        paths.config_dir = dir.path().join("config");
        paths.data_dir = dir.path().join("data");
        paths.home_dir = Some(dir.path().join("home"));
        std::fs::create_dir_all(paths.home_dir.clone().unwrap()).unwrap();
        let root = paths.skills_dir().join("demo");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::write(root.join("SKILL.md"), "---\nname: demo\n---\nBody\n").unwrap();
        let script = root.join("scripts/setup.sh");
        std::fs::write(&script, "#!/bin/sh\ntouch marker\n").unwrap();
        (dir, paths, script)
    }

    #[test]
    fn absolute_relative_and_cd_forms_are_detected() {
        let (dir, paths, script) = setup();
        let cwd = dir.path();
        let found = bundled_script_in_command(&paths, false, cwd, &script.display().to_string())
            .expect("absolute script");
        assert_eq!(found.skill, "demo");
        assert_eq!(found.path, script.canonicalize().unwrap());

        let found = bundled_script_in_command(
            &paths,
            false,
            script.parent().unwrap().parent().unwrap(),
            "bash scripts/setup.sh",
        )
        .expect("relative script");
        assert_eq!(found.skill, "demo");

        let cd = format!(
            "cd {} && ./scripts/setup.sh",
            script.parent().unwrap().parent().unwrap().display()
        );
        let found =
            bundled_script_in_command(&paths, false, cwd, &cd).expect("cd + relative script");
        assert_eq!(found.skill, "demo");
    }

    #[test]
    fn a_reference_document_or_an_unrelated_path_is_not_a_script() {
        let (dir, paths, _script) = setup();
        let root = paths.skills_dir().join("demo");
        std::fs::write(root.join("references.md"), "notes").unwrap();
        assert!(
            bundled_script_in_command(
                &paths,
                false,
                dir.path(),
                &root.join("references.md").display().to_string()
            )
            .is_none()
        );
        assert!(bundled_script_in_command(&paths, false, dir.path(), "cargo test").is_none());
    }
}

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
    for token in command.split_whitespace() {
        let token = token.trim_matches(|ch| {
            matches!(
                ch,
                '"' | '\'' | '`' | '(' | ')' | ';' | '&' | '|' | '<' | '>' | ','
            )
        });
        if token.is_empty()
            || token.starts_with('-')
            || token.contains('=')
            || token == "bash"
            || token == "sh"
            || token == "env"
        {
            continue;
        }
        let candidate = if Path::new(token).is_absolute() {
            PathBuf::from(token)
        } else {
            cwd.join(token)
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

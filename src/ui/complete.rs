//! Context-aware Tab completion for the prompt.
//!
//! Completion is a pure function of the text before the cursor plus a
//! snapshot of completion sources, so it is cheap to recompute every frame
//! and easy to test. Three contexts are recognised:
//!
//! - **command**: `/the` completes slash-command names;
//! - **argument**: `/theme dr` completes the arguments a command accepts
//!   (themes, profiles, skills, workflows, sessions, models, subcommands);
//! - **path**: `@src/ui/in`, `./scr`, `~/no`, or any token containing `/`
//!   completes filesystem paths. Paths are only listed on demand (Tab), never
//!   automatically, so typing prose never touches the disk.

use std::path::{Path, PathBuf};

/// Directory entries listed for one path completion.
const MAX_PATH_CANDIDATES: usize = 200;

/// What kind of token is being completed; drives popup titles and how an
/// accepted candidate is terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Command,
    Argument,
    Path,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Command => " commands · tab accept · ⇧tab back ",
            Kind::Argument => " arguments · tab accept · ⇧tab back ",
            Kind::Path => " files · tab accept · ⇧tab back ",
        }
    }
}

/// One completion choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// Text that replaces the token being completed.
    pub value: String,
    /// What the popup shows in the name column.
    pub label: String,
    pub description: String,
}

impl Candidate {
    fn new(value: impl Into<String>, description: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
            description: description.into(),
        }
    }
}

/// Candidates for the token that ends at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub kind: Kind,
    /// Byte offset in the prompt where the replaced token starts.
    pub start: usize,
    /// The token as typed (the part the candidates extend).
    pub token: String,
    pub candidates: Vec<Candidate>,
}

impl Completion {
    /// True when the typed token already equals the only candidate, so a
    /// popup would add nothing.
    pub fn is_settled(&self) -> bool {
        self.candidates.len() == 1 && self.candidates[0].value == self.token
    }

    /// Longest prefix shared by every candidate value, when it extends the
    /// typed token. Used by the first Tab, shell style.
    pub fn common_extension(&self) -> Option<String> {
        let first = &self.candidates.first()?.value;
        let mut common = first.as_str();
        for candidate in &self.candidates[1..] {
            let shared = first
                .char_indices()
                .zip(candidate.value.chars())
                .take_while(|((_, a), b)| a == b)
                .last()
                .map(|((index, ch), _)| index + ch.len_utf8())
                .unwrap_or(0);
            common = &common[..shared.min(common.len())];
        }
        (common.len() > self.token.len() && common.starts_with(&self.token))
            .then(|| common.to_string())
    }

    /// The text to insert when accepting `candidate`: commands and arguments
    /// get a trailing space so the next word can be typed (or completed)
    /// directly; directories already end in `/` and keep completing.
    pub fn insertion(&self, candidate: &Candidate, takes_args: bool) -> String {
        match self.kind {
            Kind::Command if takes_args => format!("{} ", candidate.value),
            Kind::Command => candidate.value.clone(),
            Kind::Argument => format!("{} ", candidate.value),
            Kind::Path if candidate.value.ends_with('/') => candidate.value.clone(),
            Kind::Path => format!("{} ", candidate.value),
        }
    }
}

/// Snapshot of the dynamic things commands accept, refreshed by the TUI.
#[derive(Debug, Clone, Default)]
pub struct Sources {
    pub themes: Vec<String>,
    /// Profile name and description.
    pub profiles: Vec<(String, String)>,
    pub skills: Vec<String>,
    pub workflows: Vec<String>,
    /// Session id and a preview of its first prompt.
    pub sessions: Vec<(String, String)>,
    /// Models the endpoint reported the last time it was probed.
    pub models: Vec<String>,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
}

/// Commands that take arguments, so accepting their name adds a space.
pub fn takes_args(command: &str) -> bool {
    matches!(
        command,
        "model"
            | "login"
            | "profile"
            | "theme"
            | "resume"
            | "skill"
            | "workflow"
            | "sec"
            | "trust"
            | "export"
    )
}

/// Completion for the prompt `before` the cursor. `paths` enables
/// filesystem completion; the TUI passes it only after an explicit Tab.
pub fn complete(
    before: &str,
    commands: &[(&str, &str)],
    sources: &Sources,
    paths: bool,
) -> Option<Completion> {
    let token_start = before
        .rfind(char::is_whitespace)
        .map(|index| index + before[index..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(0);
    let token = &before[token_start..];
    let leading = before.len() - before.trim_start().len();

    // A slash command is a single-line prompt whose first word starts with
    // `/`. Anything else is prose, where only paths complete.
    let is_command = before[leading..].starts_with('/') && !before.contains('\n');
    if is_command {
        let words: Vec<&str> = before[leading..].split_whitespace().collect();
        if token_start <= leading {
            return complete_command(token, token_start, commands);
        }
        let command = words.first()?.trim_start_matches('/');
        // Words fully typed after the command name.
        let complete_args: Vec<&str> = words[1..]
            .iter()
            .copied()
            .take(words.len() - 1 - usize::from(!token.is_empty()))
            .collect();
        if let Some(completion) =
            complete_argument(command, &complete_args, token, token_start, sources)
        {
            return Some(completion);
        }
        if command == "export" && complete_args.is_empty() && paths {
            return complete_path(token, token_start, sources, false);
        }
        return None;
    }

    if !paths || token.is_empty() {
        return None;
    }
    if let Some(rest) = token.strip_prefix('@') {
        return complete_path(rest, token_start + 1, sources, true);
    }
    if looks_like_path(token) {
        return complete_path(token, token_start, sources, false);
    }
    None
}

fn looks_like_path(token: &str) -> bool {
    token.contains('/') || token.starts_with('.') || token.starts_with('~')
}

fn complete_command(token: &str, start: usize, commands: &[(&str, &str)]) -> Option<Completion> {
    let prefix = token.strip_prefix('/')?;
    let candidates = filter(
        commands
            .iter()
            .map(|(name, description)| Candidate::new(format!("/{name}"), *description)),
        prefix,
        |candidate| &candidate.value[1..],
    );
    (!candidates.is_empty()).then(|| Completion {
        kind: Kind::Command,
        start,
        token: token.to_string(),
        candidates,
    })
}

fn complete_argument(
    command: &str,
    args: &[&str],
    token: &str,
    start: usize,
    sources: &Sources,
) -> Option<Completion> {
    let plain = |names: &[String], description: &str| -> Vec<Candidate> {
        names
            .iter()
            .map(|name| Candidate::new(name.clone(), description))
            .collect()
    };
    let choices: Vec<Candidate> = match (command, args) {
        ("theme", []) => {
            let mut choices = vec![Candidate::new("save", "write the current theme to config")];
            choices.extend(plain(&sources.themes, "theme"));
            choices
        }
        ("profile", []) => sources
            .profiles
            .iter()
            .map(|(name, description)| Candidate::new(name.clone(), description.clone()))
            .collect(),
        ("skill", []) => plain(&sources.skills, "skill"),
        ("workflow", []) => plain(&sources.workflows, "workflow"),
        ("model", []) => plain(&sources.models, "model"),
        ("resume", []) => sources
            .sessions
            .iter()
            .map(|(id, preview)| Candidate::new(id.clone(), preview.clone()))
            .collect(),
        ("trust", []) => vec![Candidate::new(
            "confirm",
            "trust this project for the session",
        )],
        ("sec", []) => vec![
            Candidate::new("scope", "add, remove, or clear engagement scope"),
            Candidate::new("report", "write the engagement report"),
        ],
        ("sec", ["scope"]) => vec![
            Candidate::new("add", "add a target to scope"),
            Candidate::new("remove", "remove a target from scope"),
            Candidate::new("clear", "clear the scope"),
        ],
        _ => return None,
    };
    let candidates = filter(choices.into_iter(), token, |candidate| &candidate.value);
    (!candidates.is_empty()).then(|| Completion {
        kind: Kind::Argument,
        start,
        token: token.to_string(),
        candidates,
    })
}

/// Prefix matches first (in source order); when nothing matches by prefix,
/// fall back to case-insensitive substring matches so `/theme light` still
/// finds `catppuccin-light`.
fn filter<I, F>(items: I, typed: &str, key: F) -> Vec<Candidate>
where
    I: Iterator<Item = Candidate>,
    F: Fn(&Candidate) -> &str,
{
    let items: Vec<Candidate> = items.collect();
    let prefixed: Vec<Candidate> = items
        .iter()
        .filter(|candidate| key(candidate).starts_with(typed))
        .cloned()
        .collect();
    if !prefixed.is_empty() || typed.is_empty() {
        return prefixed;
    }
    let lower = typed.to_lowercase();
    items
        .into_iter()
        .filter(|candidate| key(candidate).to_lowercase().contains(&lower))
        .collect()
}

/// Complete a filesystem path token. `mention` marks an `@path` reference,
/// which is always relative to the working directory.
fn complete_path(
    token: &str,
    start: usize,
    sources: &Sources,
    mention: bool,
) -> Option<Completion> {
    let (dir_part, file_part) = match token.rfind('/') {
        Some(index) => (&token[..=index], &token[index + 1..]),
        None => ("", token),
    };
    let directory = resolve_dir(dir_part, sources, mention)?;
    let show_hidden = file_part.starts_with('.');

    let mut entries: Vec<(bool, String)> = std::fs::read_dir(&directory)
        .ok()?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if !name.starts_with(file_part) {
                return None;
            }
            if name.starts_with('.') && !show_hidden {
                return None;
            }
            if name == ".git" && file_part != ".git" {
                return None;
            }
            // Follow symlinks so a linked directory keeps completing.
            let is_dir = std::fs::metadata(entry.path())
                .map(|meta| meta.is_dir())
                .unwrap_or(false);
            Some((is_dir, name))
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    // Directories first, then files, each alphabetical.
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    entries.truncate(MAX_PATH_CANDIDATES);

    let candidates = entries
        .into_iter()
        .map(|(is_dir, name)| {
            let suffix = if is_dir { "/" } else { "" };
            Candidate {
                value: format!("{dir_part}{name}{suffix}"),
                label: format!("{name}{suffix}"),
                description: if is_dir { "dir".into() } else { String::new() },
            }
        })
        .collect();
    Some(Completion {
        kind: Kind::Path,
        start,
        token: token.to_string(),
        candidates,
    })
}

fn resolve_dir(dir_part: &str, sources: &Sources, mention: bool) -> Option<PathBuf> {
    if dir_part.is_empty() {
        return Some(sources.cwd.clone());
    }
    if !mention {
        if dir_part == "~/" || dir_part.starts_with("~/") {
            return Some(sources.home.as_ref()?.join(&dir_part[2..]));
        }
        if Path::new(dir_part).is_absolute() {
            return Some(PathBuf::from(dir_part));
        }
    }
    Some(sources.cwd.join(dir_part))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMANDS: &[(&str, &str)] = &[
        ("help", "list commands"),
        ("theme", "change the theme"),
        ("tree", "browse the session tree"),
        ("trust", "show trust state"),
        ("sec", "security"),
        ("export", "export"),
    ];

    fn sources(cwd: &Path) -> Sources {
        Sources {
            themes: vec!["catppuccin-light".into(), "dracula".into(), "dark".into()],
            profiles: vec![("full".into(), "everything".into())],
            sessions: vec![("20261109-abc".into(), "fix the parser".into())],
            cwd: cwd.to_path_buf(),
            home: Some(cwd.join("home")),
            ..Sources::default()
        }
    }

    fn values(completion: &Completion) -> Vec<&str> {
        completion
            .candidates
            .iter()
            .map(|candidate| candidate.value.as_str())
            .collect()
    }

    #[test]
    fn completes_command_names_with_a_common_prefix() {
        let src = sources(Path::new("/"));
        let completion = complete("/tr", COMMANDS, &src, false).unwrap();
        assert_eq!(completion.kind, Kind::Command);
        assert_eq!(values(&completion), vec!["/tree", "/trust"]);
        assert_eq!(completion.common_extension(), None);

        let completion = complete("/th", COMMANDS, &src, false).unwrap();
        assert_eq!(completion.common_extension().as_deref(), Some("/theme"));
        let insertion = completion.insertion(&completion.candidates[0], takes_args("theme"));
        assert_eq!(insertion, "/theme ");

        let completion = complete("/help", COMMANDS, &src, false).unwrap();
        assert!(completion.is_settled());
    }

    #[test]
    fn completes_command_arguments() {
        let src = sources(Path::new("/"));
        let completion = complete("/theme d", COMMANDS, &src, false).unwrap();
        assert_eq!(completion.kind, Kind::Argument);
        assert_eq!(completion.start, "/theme ".len());
        assert_eq!(values(&completion), vec!["dracula", "dark"]);

        // Substring fallback when nothing matches by prefix.
        let completion = complete("/theme light", COMMANDS, &src, false).unwrap();
        assert_eq!(values(&completion), vec!["catppuccin-light"]);

        // An empty argument lists everything, `save` first.
        let completion = complete("/theme ", COMMANDS, &src, false).unwrap();
        assert_eq!(completion.candidates[0].value, "save");

        let completion = complete("/sec scope r", COMMANDS, &src, false).unwrap();
        assert_eq!(values(&completion), vec!["remove"]);
        assert!(complete("/sec scope add ", COMMANDS, &src, false).is_none());

        let completion = complete("/resume 2026", COMMANDS, &src, false).unwrap();
        assert_eq!(completion.candidates[0].description, "fix the parser");
    }

    #[test]
    fn prose_completes_only_paths_and_only_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src/ui")).unwrap();
        std::fs::write(dir.path().join("src/ui/input.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/ui/input_extra.rs"), "").unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        let src = sources(dir.path());

        assert!(complete("look at src/", COMMANDS, &src, false).is_none());
        assert!(complete("plain words", COMMANDS, &src, true).is_none());

        let completion = complete("look at src/", COMMANDS, &src, true).unwrap();
        assert_eq!(completion.kind, Kind::Path);
        assert_eq!(values(&completion), vec!["src/ui/", "src/main.rs"]);

        let completion = complete("fix @src/ui/in", COMMANDS, &src, true).unwrap();
        assert_eq!(completion.start, "fix @".len());
        assert_eq!(
            completion.common_extension().as_deref(),
            Some("src/ui/input")
        );
        let dir_candidate = Candidate::new("src/ui/", "dir");
        assert_eq!(completion.insertion(&dir_candidate, false), "src/ui/");

        // Hidden entries appear only when the token asks for them.
        let completion = complete("@", COMMANDS, &src, true).unwrap();
        assert!(!values(&completion).contains(&".hidden"));
        let completion = complete("@.h", COMMANDS, &src, true).unwrap();
        assert_eq!(values(&completion), vec![".hidden"]);
    }

    #[test]
    fn export_completes_a_path_argument() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("notes")).unwrap();
        let src = sources(dir.path());
        let completion = complete("/export no", COMMANDS, &src, true).unwrap();
        assert_eq!(values(&completion), vec!["notes/"]);
    }

    #[test]
    fn multiline_prompts_are_not_commands() {
        let src = sources(Path::new("/"));
        assert!(complete("/theme\nd", COMMANDS, &src, false).is_none());
    }
}

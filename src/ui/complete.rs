//! Context-aware completion for the prompt.
//!
//! Completion is a pure function of the text before the cursor plus a
//! snapshot of completion sources, so it is cheap to recompute every frame
//! and easy to test. These contexts are recognised:
//!
//! - **command**: `/the` completes slash-command names;
//! - **argument**: `/theme dr` completes the arguments a command accepts
//!   (themes, profiles, skills, workflows, sessions, models, subcommands);
//! - **mention**: `@src/ag` is answered live from a cached, gitignore-aware
//!   index of the project's files ([`index_files`]), fuzzy-matched as a
//!   subsequence. The disk is never walked per keystroke; the index is built
//!   when the TUI starts and refreshed after a turn.
//! - **path**: `./scr`, `~/no`, or any token containing `/` completes
//!   filesystem paths on demand (Tab), never automatically, so ordinary
//!   prose does not touch the disk. An `@` token falls back to listing a real
//!   directory while the file index is still empty.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Directory entries listed for one path completion.
const MAX_PATH_CANDIDATES: usize = 200;

/// Files (and directories) kept in the cached project index.
pub const MAX_INDEX_ENTRIES: usize = 20_000;

/// Live `@` mentions shown at once.
const MAX_MENTION_CANDIDATES: usize = 50;

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
    /// Skill name and description.
    pub skills: Vec<(String, String)>,
    /// Workflow name and description.
    pub workflows: Vec<(String, String)>,
    /// Session id and a preview of its first prompt.
    pub sessions: Vec<(String, String)>,
    /// Models the endpoint reported the last time it was probed.
    pub models: Vec<String>,
    /// Repository-relative paths for live `@` mentions, built once off the
    /// event loop. Directories end in `/`.
    pub files: Arc<Vec<String>>,
    /// The configured endpoint, offered first by `/login`.
    pub endpoint: String,
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
}

/// Commands that take arguments, so accepting their name adds a space.
pub fn takes_args(command: &str) -> bool {
    matches!(
        command,
        "help"
            | "model"
            | "login"
            | "profile"
            | "theme"
            | "resume"
            | "skill"
            | "workflow"
            | "sec"
            | "trust"
            | "verify"
            | "commit"
            | "pr"
            | "plan"
            | "permissions"
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
        if let Some(completion) = complete_argument(
            command,
            &complete_args,
            token,
            token_start,
            sources,
            commands,
        ) {
            return Some(completion);
        }
        if command == "export" && complete_args.is_empty() && paths {
            return complete_path(token, token_start, sources, false);
        }
        return None;
    }

    // Live `@` mentions come from the cached index, even before Tab. Only
    // when there is no index yet does an existing directory prefix fall back
    // to the on-demand listing.
    if let Some(rest) = token.strip_prefix('@') {
        if let Some(completion) = complete_mention(rest, token_start + 1, sources) {
            return Some(completion);
        }
        if sources.files.is_empty() && rest.contains('/') {
            return complete_path(rest, token_start + 1, sources, true);
        }
        return None;
    }

    if !paths || token.is_empty() {
        return None;
    }
    if looks_like_path(token) {
        return complete_path(token, token_start, sources, false);
    }
    None
}

fn looks_like_path(token: &str) -> bool {
    token.contains('/') || token.starts_with('.') || token.starts_with('~')
}

/// Repository-relative paths (directories with a trailing `/`) for live `@`
/// mentions. Hidden entries are included; [`complete_mention`] filters them
/// out unless the query asks for them. Ignored and `.git` contents are
/// skipped, and the walk stops at [`MAX_INDEX_ENTRIES`].
pub fn index_files(root: &Path) -> Vec<String> {
    let mut walker = ignore::WalkBuilder::new(root);
    walker
        .hidden(false)
        .parents(true)
        .git_ignore(true)
        .git_global(true)
        .ignore(true)
        .follow_links(false);
    walker.filter_entry(|entry| entry.file_name() != ".git");

    let mut files = Vec::new();
    for entry in walker.build().flatten() {
        if entry.depth() == 0 {
            continue;
        }
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let mut value = relative.to_string_lossy().replace('\\', "/");
        if file_type.is_dir() {
            value.push('/');
        }
        files.push(value);
        if files.len() >= MAX_INDEX_ENTRIES {
            break;
        }
    }
    files.sort();
    files.dedup();
    files
}

fn complete_mention(token: &str, start: usize, sources: &Sources) -> Option<Completion> {
    if sources.files.is_empty() {
        return None;
    }
    let candidates = rank_files(token, sources.files.as_slice());
    (!candidates.is_empty()).then(|| Completion {
        kind: Kind::Path,
        start,
        token: token.to_string(),
        candidates,
    })
}

/// Fuzzy-match `query` against `files`. Ranking: a basename prefix, then a
/// basename substring, then a path substring, then a subsequence; ties go to
/// the shorter path, then alphabetical. Hidden paths appear only when the
/// query itself starts with `.`.
fn rank_files(query: &str, files: &[String]) -> Vec<Candidate> {
    let wanted = query.to_lowercase();
    let show_hidden = query.starts_with('.');
    let mut ranked: Vec<(u8, &String)> = Vec::new();
    for path in files {
        if !show_hidden && path.split('/').any(|segment| segment.starts_with('.')) {
            continue;
        }
        let lowered = path.to_lowercase();
        let base = basename(path).to_lowercase();
        // A query with a `/` names a place: a path that starts with it wins.
        let rank = if wanted.is_empty()
            || base.starts_with(&wanted)
            || wanted.contains('/') && lowered.starts_with(&wanted)
        {
            0
        } else if base.contains(&wanted) {
            1
        } else if lowered.contains(&wanted) {
            2
        } else if is_subsequence(&wanted, &lowered) {
            3
        } else {
            continue;
        };
        ranked.push((rank, path));
    }
    ranked.sort_by(|(left_rank, left), (right_rank, right)| {
        left_rank
            .cmp(right_rank)
            .then_with(|| left.chars().count().cmp(&right.chars().count()))
            .then_with(|| left.cmp(right))
    });
    ranked.truncate(MAX_MENTION_CANDIDATES);
    ranked
        .into_iter()
        .map(|(_, path)| {
            let directory = path.ends_with('/');
            let base = basename(path);
            Candidate {
                value: path.clone(),
                label: if directory {
                    format!("{base}/")
                } else {
                    base.to_string()
                },
                description: parent(path).to_string(),
            }
        })
        .collect()
}

/// The final path segment, without a directory's trailing `/`.
fn basename(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path)
}

/// The path's directory, empty for top-level entries.
fn parent(path: &str) -> &str {
    path.trim_end_matches('/')
        .rsplit_once('/')
        .map(|(directory, _)| directory)
        .unwrap_or("")
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut wanted = needle.chars();
    let mut next = wanted.next();
    for ch in haystack.chars() {
        let Some(expected) = next else {
            return true;
        };
        if ch == expected {
            next = wanted.next();
        }
    }
    next.is_none()
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
    commands: &[(&str, &str)],
) -> Option<Completion> {
    let plain = |names: &[String], description: &str| -> Vec<Candidate> {
        names
            .iter()
            .map(|name| Candidate::new(name.clone(), description))
            .collect()
    };
    let described = |items: &[(String, String)]| -> Vec<Candidate> {
        items
            .iter()
            .map(|(name, description)| Candidate::new(name.clone(), description.clone()))
            .collect()
    };
    let choices: Vec<Candidate> = match (command, args) {
        ("help", []) => commands
            .iter()
            .map(|(name, description)| Candidate::new(*name, *description))
            .collect(),
        ("login", []) => login_endpoints(&sources.endpoint),
        // Models are only known for the configured endpoint.
        ("login", [endpoint])
            if endpoint.trim_end_matches('/') == sources.endpoint.trim_end_matches('/') =>
        {
            plain(&sources.models, "model")
        }
        ("login", [_, _]) => vec![Candidate::new("-", "clear the API key")],
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
        ("skill", []) => described(&sources.skills),
        ("workflow", []) => described(&sources.workflows),
        ("model", []) => plain(&sources.models, "model"),
        ("resume", []) => {
            let mut choices = vec![Candidate::new("last", "the most recent other session")];
            choices.extend(
                sources
                    .sessions
                    .iter()
                    .map(|(id, preview)| Candidate::new(id.clone(), preview.clone())),
            );
            choices
        }
        ("trust", []) => vec![Candidate::new(
            "confirm",
            "trust this project for the session",
        )],
        ("verify", []) => vec![Candidate::new(
            "accept",
            "save the detected verify steps for this project",
        )],
        ("commit", []) => vec![
            Candidate::new("confirm", "commit the previewed changes"),
            Candidate::new("edit", "replace the commit message"),
            Candidate::new("cancel", "drop the commit preview"),
        ],
        ("plan", []) => vec![
            Candidate::new("approve", "leave plan mode and restore the previous tools"),
            Candidate::new("exit", "leave plan mode and discard the plan"),
        ],
        ("permissions", []) => vec![
            Candidate::new("normal", "read-only runs; edits and commands ask"),
            Candidate::new("accept-edits", "edits run, commands ask"),
            Candidate::new("deny-all", "anything that would ask is denied"),
            Candidate::new("yolo", "no approvals; deny rules still apply"),
            Candidate::new("why", "which layer allowed or denied the last calls"),
        ],
        ("pr", []) => vec![
            Candidate::new("confirm", "carry out the pending pull request action"),
            Candidate::new("cancel", "drop the pending pull request action"),
        ],
        ("sec", []) => vec![
            Candidate::new("summary", "show the engagement summary"),
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

/// `/login` endpoint suggestions: the configured endpoint, then the usual
/// local engine ports.
fn login_endpoints(current: &str) -> Vec<Candidate> {
    const KNOWN: &[(&str, &str)] = &[
        ("http://127.0.0.1:8080/v1", "llama.cpp server"),
        ("http://127.0.0.1:11434/v1", "Ollama"),
        ("http://127.0.0.1:1234/v1", "LM Studio"),
        ("http://127.0.0.1:8000/v1", "vLLM"),
        ("http://127.0.0.1:4000/v1", "LiteLLM proxy"),
    ];
    let mut choices = Vec::new();
    let current = current.trim_end_matches('/');
    if !current.is_empty() {
        choices.push(Candidate::new(current, "current endpoint"));
    }
    for (endpoint, description) in KNOWN {
        if *endpoint != current {
            choices.push(Candidate::new(*endpoint, *description));
        }
    }
    choices
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
        ("permissions", "permission levels"),
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

        // `/permissions` completes its four levels.
        let completion = complete("/permissions d", COMMANDS, &src, false).unwrap();
        assert_eq!(values(&completion), vec!["deny-all"]);
        let completion = complete("/permissions ", COMMANDS, &src, false).unwrap();
        assert_eq!(
            values(&completion),
            vec!["normal", "accept-edits", "deny-all", "yolo", "why"]
        );
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

        // Live mentions use the index, including hidden entries on request.
        let mut src = sources(dir.path());
        src.files = Arc::new(vec![
            ".hidden".into(),
            "src/".into(),
            "src/main.rs".into(),
            "src/ui/".into(),
            "src/ui/input.rs".into(),
            "src/ui/input_extra.rs".into(),
        ]);
        let completion = complete("@", COMMANDS, &src, false).unwrap();
        assert_eq!(completion.kind, Kind::Path);
        assert!(!values(&completion).contains(&".hidden"));
        let completion = complete("@.h", COMMANDS, &src, false).unwrap();
        assert_eq!(values(&completion), vec![".hidden"]);
        // Accepting a file inserts the space that ends the mention.
        let completion = complete("fix @src/main", COMMANDS, &src, false).unwrap();
        let insertion = completion.insertion(&completion.candidates[0], false);
        assert_eq!(insertion, "src/main.rs ");
    }

    #[test]
    fn live_mentions_rank_and_describe_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = sources(dir.path());
        src.files = Arc::new(vec![
            "src/another/great.rs".into(),
            "src/agent/mod.rs".into(),
            "my_agent.rs".into(),
            "agent.rs".into(),
        ]);

        let completion = complete("ask @ag", COMMANDS, &src, false).unwrap();
        assert_eq!(
            values(&completion),
            vec![
                "agent.rs",
                "my_agent.rs",
                "src/agent/mod.rs",
                "src/another/great.rs",
            ]
        );
        assert_eq!(completion.candidates[0].label, "agent.rs");
        assert_eq!(completion.candidates[0].description, "");
        assert_eq!(completion.candidates[2].label, "mod.rs");
        assert_eq!(completion.candidates[2].description, "src/agent");

        // A query with a directory ranks paths under it first.
        let mut nested = sources(dir.path());
        nested.files = Arc::new(vec![
            "src/security/redact.rs".into(),
            "src/ui/tree.rs".into(),
            "src/ui/render.rs".into(),
        ]);
        let completion = complete("@src/ui/re", COMMANDS, &nested, false).unwrap();
        assert_eq!(values(&completion)[0], "src/ui/render.rs");

        // Case-insensitive basename prefix, and short paths win ties.
        let completion = complete("@AGENT", COMMANDS, &src, false).unwrap();
        assert_eq!(values(&completion)[0], "agent.rs");

        // At most 50 live candidates.
        let mut many = sources(dir.path());
        many.files = Arc::new(
            (0..80)
                .map(|index| format!("file{index:02}.rs"))
                .collect::<Vec<_>>(),
        );
        let completion = complete("@file", COMMANDS, &many, false).unwrap();
        assert_eq!(completion.candidates.len(), MAX_MENTION_CANDIDATES);
    }

    #[test]
    fn the_file_index_honors_gitignore_and_hidden_names() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("ignored")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(dir.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.path().join("ignored/skip.rs"), "").unwrap();
        std::fs::write(dir.path().join(".git/config"), "").unwrap();
        std::fs::write(dir.path().join(".hidden.rs"), "").unwrap();

        let files = index_files(dir.path());
        assert!(files.iter().any(|path| path == "src/lib.rs"), "{files:?}");
        assert!(files.iter().any(|path| path == "src/"), "{files:?}");
        assert!(files.iter().any(|path| path == ".hidden.rs"), "{files:?}");
        assert!(
            !files.iter().any(|path| path.starts_with("ignored")),
            "{files:?}"
        );
        assert!(
            !files.iter().any(|path| path.starts_with(".git/")),
            "{files:?}"
        );
    }

    #[test]
    fn help_login_and_resume_offer_their_arguments() {
        let mut src = sources(Path::new("/"));
        src.endpoint = "http://127.0.0.1:9000/v1".into();
        src.models = vec!["qwen".into(), "gemma".into()];

        let completion = complete("/help th", COMMANDS, &src, false).unwrap();
        assert_eq!(values(&completion), vec!["theme"]);

        let completion = complete("/login ", COMMANDS, &src, false).unwrap();
        assert_eq!(completion.candidates[0].value, "http://127.0.0.1:9000/v1");
        assert_eq!(completion.candidates[0].description, "current endpoint");
        assert!(values(&completion).contains(&"http://127.0.0.1:11434/v1"));

        let completion =
            complete("/login http://127.0.0.1:9000/v1 g", COMMANDS, &src, false).unwrap();
        assert_eq!(values(&completion), vec!["gemma"]);
        // Another endpoint's models are unknown.
        assert!(complete("/login http://other:1/v1 g", COMMANDS, &src, false).is_none());
        let completion = complete("/login http://other:1/v1 m ", COMMANDS, &src, false).unwrap();
        assert_eq!(values(&completion), vec!["-"]);

        let completion = complete("/resume ", COMMANDS, &src, false).unwrap();
        assert_eq!(completion.candidates[0].value, "last");
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

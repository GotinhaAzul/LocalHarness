//! Shell command classification for the policy engine: which `bash` commands
//! are read-only (allowed in plan mode) and which reach the network.
//!
//! Both checks are conservative. "Read-only" is an allowlist: anything the
//! classifier does not understand is treated as mutating. "Network" is a
//! heuristic for rule matching; the OS sandbox is the hard network boundary.

/// Programs that only read the filesystem and print, with no flag that writes.
const READ_ONLY_PROGRAMS: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "grep", "rg", "find", "tree", "pwd", "file", "stat", "du",
    "df", "which", "echo", "printf", "basename", "dirname", "realpath", "sort", "uniq", "cut",
    "tr", "diff", "cmp", "true", "false", "test", "env", "date", "whoami", "uname", "nl", "less",
    "more", "column", "jq", "fd",
];

/// `git` subcommands that never write the repository or contact a remote.
const READ_ONLY_GIT: &[&str] = &[
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "ls-files",
    "ls-tree",
    "grep",
    "rev-parse",
    "describe",
    "shortlog",
    "cat-file",
    "merge-base",
    "reflog",
];

/// Flags that turn an otherwise read-only program into a writer or executor.
const WRITING_FLAGS: &[(&str, &[&str])] = &[
    (
        "find",
        &[
            "-exec", "-execdir", "-ok", "-okdir", "-delete", "-fprint", "-fprint0", "-fprintf",
            "-fls",
        ],
    ),
    ("sort", &["-o", "--output"]),
    ("rg", &["--pre"]),
    ("fd", &["-x", "--exec", "-X", "--exec-batch"]),
];

/// Programs and git subcommands treated as network access by `network` rules.
const NETWORK_PROGRAMS: &[&str] = &[
    "curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat", "netcat", "telnet", "ftp", "gh",
    "http", "https", "ping", "dig", "nslookup", "nmap",
];
const NETWORK_GIT: &[&str] = &["push", "pull", "fetch", "clone", "ls-remote", "remote"];
const NETWORK_INSTALL: &[(&str, &[&str])] = &[
    ("npm", &["install", "i", "ci", "publish", "add"]),
    ("pnpm", &["install", "i", "add", "publish"]),
    ("yarn", &["install", "add", "publish"]),
    ("pip", &["install", "download"]),
    ("pip3", &["install", "download"]),
    (
        "cargo",
        &["install", "publish", "fetch", "update", "search"],
    ),
    ("go", &["get", "install"]),
    ("apt", &["install", "update"]),
    ("apt-get", &["install", "update"]),
    ("brew", &["install", "update"]),
    ("docker", &["pull", "push", "login"]),
];

/// Split a command line into simple commands on `|`, `;`, `&&`, `||`, `&`,
/// and newlines. Quotes are respected so `grep "a|b"` stays one command.
fn segments(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                current.push(ch);
                if ch == q {
                    quote = None;
                } else if ch == '\\'
                    && q == '"'
                    && let Some(next) = chars.next()
                {
                    current.push(next);
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    current.push(ch);
                }
                '\\' => {
                    current.push(ch);
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                '|' | ';' | '&' | '\n' => {
                    if !current.trim().is_empty() {
                        out.push(current.trim().to_string());
                    }
                    current.clear();
                    // Collapse `||` and `&&`.
                    if matches!(chars.peek(), Some('|') | Some('&')) {
                        chars.next();
                    }
                }
                _ => current.push(ch),
            },
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

/// Whitespace words of one simple command with surrounding quotes removed.
fn words(segment: &str) -> Vec<String> {
    segment
        .split_whitespace()
        .map(|word| word.trim_matches(|c| c == '"' || c == '\'').to_string())
        .collect()
}

/// The program and arguments after leading `VAR=value` assignments.
fn program(words: &[String]) -> Option<(&str, &[String])> {
    let start = words
        .iter()
        .position(|word| !(word.contains('=') && !word.starts_with('-')))?;
    let name = words[start].rsplit('/').next().unwrap_or(&words[start]);
    Some((name, &words[start + 1..]))
}

/// The git subcommand, skipping global options such as `-C dir` or `--no-pager`.
fn git_subcommand(args: &[String]) -> Option<&str> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if matches!(arg.as_str(), "-C" | "-c" | "--git-dir" | "--work-tree") {
            iter.next();
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

/// True when every simple command in `command` is on the read-only allowlist
/// and nothing redirects output, substitutes commands, or spawns a subshell.
pub fn is_read_only(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return false;
    }
    // Redirection, substitution, and subshells can write or execute anything.
    if outside_quotes(trimmed)
        .chars()
        .any(|c| matches!(c, '>' | '<' | '`' | '(' | ')' | '{' | '}'))
        || trimmed.contains("$(")
    {
        return false;
    }
    segments(trimmed).iter().all(|segment| {
        let words = words(segment);
        let Some((name, args)) = program(&words) else {
            return false;
        };
        if words[0].contains('=') {
            // `VAR=value cmd` changes the environment of what runs.
            return false;
        }
        if name == "git" {
            return git_subcommand(args).is_some_and(|sub| {
                READ_ONLY_GIT.contains(&sub) && (sub != "reflog" || reflog_is_read_only(args))
            }) && !args
                .iter()
                .any(|a| a == "--output" || a.starts_with("--output="));
        }
        if name == "env" && !args.is_empty() {
            // `env cmd` runs another program.
            return false;
        }
        if !READ_ONLY_PROGRAMS.contains(&name) {
            return false;
        }
        if let Some((_, flags)) = WRITING_FLAGS.iter().find(|(p, _)| *p == name) {
            return !args
                .iter()
                .any(|arg| flags.iter().any(|flag| flag_matches(arg, flag)));
        }
        true
    })
}

/// Whether `arg` selects `flag`, including an attached short-option value
/// (`sort -oFILE`) and the `--flag=value` form.
fn flag_matches(arg: &str, flag: &str) -> bool {
    arg == flag
        || arg.starts_with(&format!("{flag}="))
        || (flag.starts_with('-')
            && !flag.starts_with("--")
            && arg.len() > flag.len()
            && arg.starts_with(flag))
}

/// `git reflog` is read-only only for `show` (or no action). `expire`,
/// `delete`, and `drop` rewrite the recovery log.
fn reflog_is_read_only(args: &[String]) -> bool {
    let Some(position) = args.iter().position(|arg| arg == "reflog") else {
        return false;
    };
    args.iter()
        .skip(position + 1)
        .find(|arg| !arg.starts_with('-'))
        .is_none_or(|action| action == "show")
}

/// True when some simple command in `command` is a known network client, a
/// remote git operation, or a package install/publish.
pub fn touches_network(command: &str) -> bool {
    segments(command).iter().any(|segment| {
        let words = words(segment);
        let Some((name, args)) = program(&words) else {
            return false;
        };
        if NETWORK_PROGRAMS.contains(&name) {
            return true;
        }
        if name == "git" {
            return git_subcommand(args).is_some_and(|sub| NETWORK_GIT.contains(&sub));
        }
        if let Some((_, subs)) = NETWORK_INSTALL.iter().find(|(p, _)| *p == name) {
            return args
                .iter()
                .find(|a| !a.starts_with('-'))
                .is_some_and(|sub| subs.contains(&sub.as_str()));
        }
        false
    })
}

/// The command with quoted spans blanked, so metacharacters inside quotes
/// (`grep "a>b"`) are not mistaken for redirection.
fn outside_quotes(command: &str) -> String {
    let mut out = String::with_capacity(command.len());
    let mut quote: Option<char> = None;
    for ch in command.chars() {
        match quote {
            Some(q) if ch == q => {
                quote = None;
                out.push(' ');
            }
            Some(_) => out.push(' '),
            None if ch == '\'' || ch == '"' => {
                quote = Some(ch);
                out.push(' ');
            }
            None => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_commands_are_recognized() {
        for command in [
            "ls -la",
            "git status",
            "git -C sub log --oneline -5",
            "git diff HEAD~1 | head -50",
            "rg 'fn main' src && wc -l src/main.rs",
            "grep \"a>b\" file.txt",
            "find . -name '*.rs'",
            "git reflog",
            "git reflog show --date=iso",
        ] {
            assert!(is_read_only(command), "{command}");
        }
    }

    #[test]
    fn mutating_commands_are_not_read_only() {
        for command in [
            "git commit -m x",
            "git push origin main",
            "rm -rf target",
            "echo hi > file",
            "cat a >> b",
            "find . -delete",
            "find . -exec rm {} ;",
            "ls; touch x",
            "ls && cargo build",
            "echo $(rm x)",
            "echo `rm x`",
            "FOO=1 make",
            "env rm x",
            "sort -o out in",
            "sort -oout in",
            "sort -o/tmp/out in",
            "git reflog expire --expire=now --all",
            "git reflog delete HEAD@{1}",
            "(cd x && ls)",
            "",
        ] {
            assert!(!is_read_only(command), "{command}");
        }
    }

    #[test]
    fn network_commands_are_detected() {
        assert!(touches_network("curl https://example.com"));
        assert!(touches_network("cd x && git push origin main"));
        assert!(touches_network("npm install left-pad"));
        assert!(touches_network("/usr/bin/wget x"));
        assert!(!touches_network("git status"));
        assert!(!touches_network("cargo test"));
        assert!(!touches_network("echo curl"));
    }
}

//! The process environment every agent-run command gets.
//!
//! Commands the model starts (`bash`, `bg`, `verify`, `test`) must never wait
//! on a human. Two things make that hold regardless of what the command is:
//!
//! - The child starts a new session (`setsid`), so it has no controlling
//!   terminal. Anything that opens `/dev/tty` to prompt (git credentials, ssh
//!   passphrases and host keys, sudo) fails at once instead of hanging the run
//!   or reading keystrokes meant for the TUI. The new session is also a new
//!   process group whose id is the child's pid, which `kill_group` relies on.
//! - A small fixed set of variables turns off prompts, editors, and pagers.

use nix::libc;
use std::ffi::OsString;

/// Variables set on every agent-run command. Each one removes a way a command
/// could block on input; none of them changes what a build or test computes.
pub(crate) const NONINTERACTIVE_ENV: &[(&str, &str)] = &[
    // git asks for credentials on the terminal; fail instead.
    ("GIT_TERMINAL_PROMPT", "0"),
    // `git commit` without -m, `git rebase -i`, `git tag -a`: no editor. git
    // aborts an empty message and keeps the default one elsewhere.
    ("GIT_EDITOR", "true"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    // Output is a pipe already, but some tools page anyway when asked to.
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    // Package managers that would otherwise ask.
    ("DEBIAN_FRONTEND", "noninteractive"),
];

/// The same variables as bubblewrap `--setenv` arguments, for a sandbox that
/// clears the environment first.
pub(crate) fn setenv_args() -> Vec<OsString> {
    let mut args = Vec::with_capacity(NONINTERACTIVE_ENV.len() * 3);
    for (name, value) in NONINTERACTIVE_ENV {
        args.push("--setenv".into());
        args.push((*name).into());
        args.push((*value).into());
    }
    args
}

/// Detach `cmd` from the terminal and apply [`NONINTERACTIVE_ENV`]. Replaces
/// `process_group(0)`: a new session is also a new process group.
pub(crate) fn detach(cmd: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.envs(NONINTERACTIVE_ENV.iter().copied());
    // SAFETY: `setsid` is async-signal-safe and touches no memory.
    unsafe {
        cmd.as_std_mut().pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    async fn run(script: &str) -> String {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        detach(&mut cmd);
        let out = cmd.output().await.expect("spawn bash");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn detached_command_leads_a_new_session() {
        // A session leader started by setsid has no controlling terminal, so
        // git, ssh, and sudo cannot open /dev/tty to prompt.
        let out = run("[ \"$(ps -o sid= -p $$ | tr -d ' ')\" = \"$$\" ] && echo leader").await;
        assert_eq!(out, "leader");
    }

    #[tokio::test]
    async fn detached_command_leads_its_own_process_group() {
        // kill_group signals the group named by the child's pid.
        let out = run("[ \"$(ps -o pgid= -p $$ | tr -d ' ')\" = \"$$\" ] && echo leader").await;
        assert_eq!(out, "leader");
    }

    #[tokio::test]
    async fn detached_command_cannot_prompt_through_git() {
        let out = run("echo \"$GIT_TERMINAL_PROMPT $GIT_EDITOR $PAGER\"").await;
        assert_eq!(out, "0 true cat");
    }
}

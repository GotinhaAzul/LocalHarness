//! The process environment every agent-run command gets.
//!
//! Commands the model starts (`bash`, `bg`, `verify`, `test`) must never wait
//! on a human. Two things make that hold regardless of what the command is:
//!
//! - The child starts a new session, so it has no controlling terminal. Anything that opens `/dev/tty` to prompt (git credentials, ssh
//!   passphrases and host keys, sudo) fails at once instead of hanging the run
//!   or reading keystrokes meant for the TUI. The new session is also a new
//!   process group whose id is the child's pid, which `kill_group` relies on.
//! - A small fixed set of variables turns off prompts, editors, and pagers.
//!
//! The new session comes from the `setsid` utility (util-linux, or busybox)
//! when it is installed, not from a `pre_exec` hook: a hook forces the
//! standard library to `fork` instead of `posix_spawn`, and the longer window
//! between fork and exec lets a child inherit other threads' open file
//! descriptors, so a script another thread just wrote fails to start with
//! "Text file busy". `setsid` is called by a process that is not a process
//! group leader, so it calls `setsid()` and execs the program in place: the
//! pid, session id, and process group id stay the same. Without the utility,
//! the hook is the fallback.

use nix::libc;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::OnceLock;

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

/// The `setsid` utility on `PATH`, looked up once.
fn setsid_program() -> Option<&'static PathBuf> {
    static SETSID: OnceLock<Option<PathBuf>> = OnceLock::new();
    SETSID
        .get_or_init(|| {
            let path = std::env::var_os("PATH")?;
            std::env::split_paths(&path)
                .chain([PathBuf::from("/usr/bin"), PathBuf::from("/bin")])
                .map(|dir| dir.join("setsid"))
                .find(|candidate| candidate.is_file())
        })
        .as_ref()
}

/// A command for `program` that runs in a new session with
/// [`NONINTERACTIVE_ENV`]. The child leads its own process group (its pgid is
/// its pid), which `kill_group` relies on. Add arguments as usual.
pub(crate) fn command(program: impl AsRef<OsStr>) -> tokio::process::Command {
    let mut cmd = match setsid_program() {
        Some(setsid) => {
            let mut cmd = tokio::process::Command::new(setsid);
            cmd.arg("--").arg(program);
            cmd
        }
        None => {
            let mut cmd = tokio::process::Command::new(program);
            detach_in_child(&mut cmd);
            cmd
        }
    };
    cmd.envs(NONINTERACTIVE_ENV.iter().copied());
    cmd
}

/// The fallback without the `setsid` utility: call `setsid()` between fork
/// and exec.
fn detach_in_child(cmd: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt;
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
        let mut cmd = command("bash");
        cmd.arg("-c")
            .arg(script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
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
    async fn the_fallback_also_leads_a_new_session() {
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c")
            .arg("[ \"$(ps -o sid= -p $$ | tr -d ' ')\" = \"$$\" ] && echo leader")
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        detach_in_child(&mut cmd);
        let out = cmd.output().await.expect("spawn bash");
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "leader");
    }

    #[test]
    fn the_utility_is_used_when_installed() {
        if let Some(setsid) = setsid_program() {
            let cmd = command("bash");
            assert_eq!(cmd.as_std().get_program(), setsid.as_os_str());
        }
    }

    #[tokio::test]
    async fn detached_command_cannot_prompt_through_git() {
        let out = run("echo \"$GIT_TERMINAL_PROMPT $GIT_EDITOR $PAGER\"").await;
        assert_eq!(out, "0 true cat");
    }
}

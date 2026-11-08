//! Process runner for external tools: spawn, feed stdin, capture capped
//! stdout/stderr, enforce a timeout, and kill the whole process group.
//!
//! The runner knows nothing about manifests or JSON protocols; it turns a
//! prepared call into raw output or a classified error. `mod.rs` does the
//! protocol interpretation.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Each stream is capped before it reaches memory; the protocol caps content
/// further. Draining continues past the cap so the child never blocks on a
/// full pipe.
pub const STREAM_CAP: usize = 1024 * 1024;
/// Grace between SIGTERM and SIGKILL, and the maximum time spent draining
/// pipes after the child exits (a grandchild may keep them open).
const PIPE_GRACE: Duration = Duration::from_secs(2);

/// One fully resolved invocation. Placeholder substitution already happened.
#[derive(Debug, Clone)]
pub struct PreparedCall {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: PathBuf,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
}

/// Raw result of a child that exited on its own.
#[derive(Debug)]
pub struct RunOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr: Vec<u8>,
}

/// Classified runner failures; `mod.rs` maps these to tool error codes.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error(
        "cannot start `{program}`: {source}; install the command or fix `command` in the manifest"
    )]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("cannot wait for `{program}`: {source}")]
    Io {
        program: String,
        source: std::io::Error,
    },
    #[error("`{program}` timed out after {elapsed:?}")]
    Timeout {
        program: String,
        elapsed: Duration,
        stderr: String,
    },
    #[error("cancelled")]
    Cancelled,
}

enum Outcome {
    Exited(std::io::Result<std::process::ExitStatus>),
    TimedOut,
    Cancelled,
}

/// Run one external command. On timeout or cancellation the process group is
/// terminated (SIGTERM, then SIGKILL after two seconds).
pub async fn run(call: PreparedCall, cancel: CancellationToken) -> Result<RunOutput, RunError> {
    let mut command = Command::new(&call.program);
    command
        .args(&call.args)
        .current_dir(&call.cwd)
        .envs(&call.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // A dedicated process group lets a timeout kill the command's children too.
    #[cfg(unix)]
    command.process_group(0);

    let mut attempts = 0;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            Err(source)
                if source.kind() == std::io::ErrorKind::ExecutableFileBusy && attempts < 10 =>
            {
                attempts += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(source) => {
                return Err(RunError::Spawn {
                    program: call.program.clone(),
                    source,
                });
            }
        }
    };

    let stdout_pipe = child.stdout.take().expect("stdout is piped");
    let stderr_pipe = child.stderr.take().expect("stderr is piped");
    let stdout_task = tokio::spawn(read_capped(stdout_pipe, STREAM_CAP));
    let stderr_task = tokio::spawn(read_capped(stderr_pipe, STREAM_CAP));

    // Write stdin concurrently: a child that ignores stdin must not deadlock a
    // large request against a full stdout pipe.
    let stdin_task = child.stdin.take().map(|mut stdin| {
        let data = call.stdin.clone();
        tokio::spawn(async move {
            let _ = stdin.write_all(&data).await;
            let _ = stdin.shutdown().await;
        })
    });

    let outcome = {
        let deadline = tokio::time::sleep(call.timeout);
        tokio::pin!(deadline);
        let wait = child.wait();
        tokio::pin!(wait);
        tokio::select! {
            biased;
            result = &mut wait => Outcome::Exited(result),
            _ = &mut deadline => Outcome::TimedOut,
            _ = cancel.cancelled() => Outcome::Cancelled,
        }
    };

    let (status, timed_out, cancelled) = match outcome {
        Outcome::Exited(Ok(status)) => (status, false, false),
        Outcome::Exited(Err(source)) => {
            terminate(&mut child).await;
            collect_stdin(stdin_task).await;
            let _ = collect(stdout_task).await;
            let _ = collect(stderr_task).await;
            return Err(RunError::Io {
                program: call.program,
                source,
            });
        }
        Outcome::TimedOut => {
            terminate(&mut child).await;
            match child.wait().await {
                Ok(status) => (status, true, false),
                Err(source) => {
                    collect_stdin(stdin_task).await;
                    let _ = collect(stdout_task).await;
                    let _ = collect(stderr_task).await;
                    return Err(RunError::Io {
                        program: call.program,
                        source,
                    });
                }
            }
        }
        Outcome::Cancelled => {
            terminate(&mut child).await;
            match child.wait().await {
                Ok(status) => (status, false, true),
                Err(source) => {
                    collect_stdin(stdin_task).await;
                    let _ = collect(stdout_task).await;
                    let _ = collect(stderr_task).await;
                    return Err(RunError::Io {
                        program: call.program,
                        source,
                    });
                }
            }
        }
    };

    let (stdout, stdout_truncated) = collect(stdout_task).await;
    let (stderr, _) = collect(stderr_task).await;
    collect_stdin(stdin_task).await;

    if cancelled {
        return Err(RunError::Cancelled);
    }

    let output = RunOutput {
        code: status.code(),
        stdout,
        stdout_truncated,
        stderr,
    };

    if timed_out {
        let stderr_text = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr_text.trim().chars().take(4000).collect::<String>();
        return Err(RunError::Timeout {
            program: call.program,
            elapsed: call.timeout,
            stderr,
        });
    }

    Ok(output)
}

/// SIGTERM the group, wait up to two seconds, then SIGKILL.
async fn terminate(child: &mut Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, killpg};
        use nix::unistd::Pid;

        if let Some(pid) = child.id()
            && pid <= i32::MAX as u32
        {
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
        }
        if tokio::time::timeout(PIPE_GRACE, child.wait()).await.is_ok() {
            return;
        }
        if let Some(pid) = child.id()
            && pid <= i32::MAX as u32
        {
            let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        return;
    }
    #[cfg(not(unix))]
    {
        let _ = child.start_kill();
    }
}

/// Read at most `cap` bytes, then keep draining so the writer cannot block.
async fn read_capped<R>(mut reader: R, cap: usize) -> (Vec<u8>, bool)
where
    R: AsyncRead + Unpin,
{
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if out.len() < cap {
                    let room = cap - out.len();
                    let take = n.min(room);
                    out.extend_from_slice(&buf[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (out, truncated)
}

async fn collect(mut task: JoinHandle<(Vec<u8>, bool)>) -> (Vec<u8>, bool) {
    match tokio::time::timeout(PIPE_GRACE, &mut task).await {
        Ok(Ok(value)) => value,
        Ok(Err(_)) => (Vec::new(), false),
        Err(_) => {
            task.abort();
            (Vec::new(), false)
        }
    }
}

async fn collect_stdin(task: Option<JoinHandle<()>>) {
    let Some(mut task) = task else {
        return;
    };
    if tokio::time::timeout(PIPE_GRACE, &mut task).await.is_err() {
        task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(program: &str, args: Vec<&str>, timeout: Duration) -> PreparedCall {
        PreparedCall {
            program: program.to_string(),
            args: args.into_iter().map(str::to_string).collect(),
            env: BTreeMap::new(),
            cwd: std::env::temp_dir(),
            stdin: b"{}\n".to_vec(),
            timeout,
        }
    }

    #[tokio::test]
    async fn captures_stdout_and_exit_code() {
        #[cfg(unix)]
        {
            let output = run(
                call(
                    "/bin/sh",
                    vec!["-c", "printf 'hello'; exit 0"],
                    Duration::from_secs(5),
                ),
                CancellationToken::new(),
            )
            .await
            .unwrap();
            assert_eq!(output.code, Some(0));
            assert_eq!(output.stdout, b"hello");
            assert!(!output.stdout_truncated);
        }
    }

    #[tokio::test]
    async fn timeout_kills_the_process() {
        #[cfg(unix)]
        {
            let error = run(
                call("/bin/sh", vec!["-c", "sleep 5"], Duration::from_millis(100)),
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert!(matches!(error, RunError::Timeout { .. }));
        }
    }

    #[tokio::test]
    async fn cancellation_stops_the_process() {
        #[cfg(unix)]
        {
            let cancel = CancellationToken::new();
            let token = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                token.cancel();
            });
            let error = run(
                call("/bin/sh", vec!["-c", "sleep 5"], Duration::from_secs(5)),
                cancel,
            )
            .await
            .unwrap_err();
            assert!(matches!(error, RunError::Cancelled));
        }
    }

    #[tokio::test]
    async fn missing_program_is_a_spawn_error() {
        let error = run(
            call(
                "/nonexistent/localharness-test-binary",
                vec![],
                Duration::from_secs(5),
            ),
            CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, RunError::Spawn { .. }));
    }
}

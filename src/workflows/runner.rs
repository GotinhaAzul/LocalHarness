//! The production [`StepExecutor`]: one agent turn per prompt step, with the
//! same streaming and cancellation as headless mode, plus a stdin approval
//! gate.

use std::io::Write;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::agent::{AgentEvent, Core, TurnStatus, run_turn};
use crate::workflows::engine::{
    Approval, Facts, StepExecutor, StepReport, StepSpec, StepStatus, ToolObservation,
    observations_since,
};

pub struct AgentExecutor {
    core: Arc<Core>,
    workflow: String,
    cancel: CancellationToken,
    json: bool,
    stdin: BufReader<tokio::io::Stdin>,
}

impl AgentExecutor {
    pub fn new(
        core: Arc<Core>,
        workflow: impl Into<String>,
        cancel: CancellationToken,
        json: bool,
    ) -> Self {
        Self {
            core,
            workflow: workflow.into(),
            cancel,
            json,
            stdin: BufReader::new(tokio::io::stdin()),
        }
    }

    fn branch_length(&self) -> usize {
        match self.core.session.lock() {
            Ok(session) => session.branch_entries().len(),
            Err(poison) => poison.into_inner().branch_entries().len(),
        }
    }

    fn report(&self, start: usize, status: StepStatus, detail: Option<String>) -> StepReport {
        let session = match self.core.session.lock() {
            Ok(session) => session,
            Err(poison) => poison.into_inner(),
        };
        let facts = Facts::from_session(&session);
        let tools: Vec<ToolObservation> = observations_since(&session, start);
        let verify = Facts::verify_since(&session, start);
        StepReport {
            status,
            facts,
            tools,
            verify,
            detail,
        }
    }

    fn say(&self, event: serde_json::Value, human: impl FnOnce()) {
        if self.json {
            println!("{event}");
            let _ = std::io::stdout().flush();
        } else {
            human();
        }
    }
}

fn step_message(workflow: &str, step: &StepSpec, attempt: u32, previous: Option<&str>) -> String {
    let mut message = format!(
        "Workflow {workflow:?}, step {:?}, attempt {} of {}.\n\n{}",
        step.id,
        attempt,
        step.retry + 1,
        step.prompt.trim()
    );
    if let Some(reason) = previous {
        message.push_str(&format!(
            "\n\nThe previous attempt did not satisfy this step: {reason}. Address that before finishing."
        ));
    }
    message.push_str(
        "\n\nFinish this step only. The harness records the result and moves to the next step.",
    );
    message
}

#[async_trait]
impl StepExecutor for AgentExecutor {
    async fn prompt(
        &mut self,
        step: &StepSpec,
        attempt: u32,
        previous: Option<&str>,
    ) -> StepReport {
        let message = step_message(&self.workflow, step, attempt, previous);
        let start = self.branch_length();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = self.cancel.child_token();
        let core = self.core.clone();
        let cancel_turn = cancel.clone();
        let handle = tokio::spawn(async move { run_turn(core, message, tx, cancel_turn).await });

        let mut detail = String::new();
        let mut outcome = None;
        while let Some(event) = rx.recv().await {
            match event {
                AgentEvent::TextDelta(text) => {
                    if self.json {
                        self.say(json!({"type":"text","text":text}), || {});
                    } else {
                        print!("{text}");
                        let _ = std::io::stdout().flush();
                    }
                    detail.push_str(&text);
                }
                AgentEvent::ToolStarted { name, summary, .. } => {
                    let value = json!({"type":"tool_started","name":name,"summary":summary});
                    self.say(value, || {
                        eprintln!("[tool] {name} {summary}");
                    });
                }
                AgentEvent::ToolFinished {
                    name,
                    is_error,
                    preview,
                    ..
                } => {
                    let value = json!({
                        "type": "tool_finished",
                        "name": name,
                        "is_error": is_error,
                        "preview": preview,
                    });
                    self.say(value, || {
                        if is_error {
                            eprintln!("[error] {name}: {preview}");
                        } else {
                            eprintln!("[ok] {name}");
                        }
                    });
                }
                AgentEvent::Notice(message) => {
                    let value = json!({"type":"notice","message":message});
                    self.say(value, || eprintln!("[notice] {message}"));
                }
                AgentEvent::Guard { code, message } => {
                    let value = json!({"type":"guard","code":code,"message":message});
                    self.say(value, || eprintln!("[{code}] {message}"));
                }
                AgentEvent::Error {
                    code,
                    message,
                    recoverable,
                } => {
                    let value = json!({
                        "type": "error",
                        "code": code,
                        "message": message,
                        "recoverable": recoverable,
                    });
                    self.say(value, || eprintln!("[{code}] {message}"));
                }
                AgentEvent::TurnDone(result) => outcome = Some(result),
                _ => {}
            }
        }
        let joined = handle.await;
        let outcome = match (outcome, joined) {
            (Some(outcome), _) => outcome,
            (None, Ok(outcome)) => outcome,
            (None, Err(error)) => {
                return self.report(
                    start,
                    StepStatus::Failed {
                        code: "turn_panicked".into(),
                        message: error.to_string(),
                    },
                    None,
                );
            }
        };
        let status = match outcome.status {
            TurnStatus::Done => StepStatus::Succeeded,
            TurnStatus::Cancelled => StepStatus::Cancelled,
            other => {
                let (code, message) = outcome
                    .error
                    .clone()
                    .unwrap_or_else(|| ("turn_failed".into(), format!("turn ended {other:?}")));
                StepStatus::Failed { code, message }
            }
        };
        let detail = detail
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map(|line| line.trim().chars().take(200).collect::<String>());
        self.report(start, status, detail)
    }

    async fn approve(&mut self, step: &StepSpec) -> Approval {
        let message = step.message.clone().unwrap_or_default();
        if self.json {
            println!(
                "{}",
                json!({
                    "type": "approval_request",
                    "step": step.id,
                    "message": message,
                })
            );
            let _ = std::io::stdout().flush();
        } else {
            eprintln!(
                "\n[workflow] approval required for step {:?}: {message}\nType y to accept, anything else to deny:",
                step.id
            );
            let _ = std::io::stderr().flush();
        }
        read_approval(&mut self.stdin, &self.cancel).await
    }
}

/// One line from `reader`: `y`/`yes` accepts, anything else denies, EOF
/// denies, and cancellation wins over both.
async fn read_approval<R>(reader: &mut R, cancel: &CancellationToken) -> Approval
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut line = String::new();
    let read = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Approval::Cancelled,
        read = reader.read_line(&mut line) => read,
    };
    // A signal can interrupt the pending read; that is a cancellation, not a
    // denial. Check the token before interpreting the result.
    if cancel.is_cancelled() {
        return Approval::Cancelled;
    }
    match read {
        Ok(0) | Err(_) => Approval::Denied,
        Ok(_) => {
            let answer = line.trim().to_ascii_lowercase();
            if matches!(answer.as_str(), "y" | "yes") {
                Approval::Accepted
            } else {
                Approval::Denied
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncWriteExt, BufReader, duplex};

    #[tokio::test]
    async fn approval_reads_accept_deny_and_eof() {
        for (input, expected) in [
            ("y\n", Approval::Accepted),
            ("YES\n", Approval::Accepted),
            ("n\n", Approval::Denied),
            ("\n", Approval::Denied),
        ] {
            let (mut client, server) = duplex(64);
            client.write_all(input.as_bytes()).await.unwrap();
            drop(client);
            let mut reader = BufReader::new(server);
            let cancel = CancellationToken::new();
            assert_eq!(read_approval(&mut reader, &cancel).await, expected);
        }

        let (client, server) = duplex(64);
        drop(client);
        let mut reader = BufReader::new(server);
        let cancel = CancellationToken::new();
        assert_eq!(read_approval(&mut reader, &cancel).await, Approval::Denied);
    }

    #[tokio::test]
    async fn approval_cancellation_wins_over_a_pending_read() {
        let (_client, server) = duplex(64);
        let mut reader = BufReader::new(server);
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            read_approval(&mut reader, &cancel).await,
            Approval::Cancelled
        );
    }
}

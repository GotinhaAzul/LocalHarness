//! Lifecycle hooks: one command per event, spawned directly with one JSON
//! object on stdin and one on stdout.
//!
//! A hook may add context or block. It cannot change the tool list, the
//! endpoint, the security scope, or the permission mode: those fields are
//! ignored on output and the dispatcher never reads policy from a hook.
//! Failure is visible in the transcript and, for `on_error = "block"`
//! (the `pre_tool` default), the action does not run.

use std::path::Path;
use std::process::Stdio;
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{HookConfig, HookEvent, HookOnError, HooksConfig};
use crate::session::{ArtifactStore, CustomData, EntryKind};
use crate::tools::SessionHandle;

/// A reason or context string longer than this is stored as an artifact.
pub const REASON_CAP_CHARS: usize = 4_000;
/// Hard stop for reading a hook's stdout into memory. A larger stream is a
/// malformed response; the recorded text is still capped.
const PARSE_LIMIT_BYTES: usize = 1024 * 1024;

/// Identity of the session a hook runs for. Sent in every payload.
#[derive(Debug, Clone, Default)]
pub struct HookIdentity {
    pub session_id: String,
    pub model: String,
    pub profile: String,
}

/// Everything a hook invocation needs. Built from `Core` by the agent loop and
/// from config by `lh hooks test`.
pub struct HookEnv<'a> {
    pub config: &'a HooksConfig,
    pub cwd: &'a Path,
    pub session: Option<&'a SessionHandle>,
    pub artifacts: Option<&'a ArtifactStore>,
    pub identity: HookIdentity,
}

impl HookEnv<'_> {
    fn record(&self, invocation: &Invocation) {
        let Some(session) = self.session else {
            return;
        };
        let Ok(mut session) = session.lock() else {
            return;
        };
        let mut data = CustomData::new("hook-invocation")
            .with("hook", invocation.hook.clone())
            .with("event", invocation.event.as_str())
            .with("outcome", invocation.outcome.label())
            .with("duration_ms", invocation.duration_ms);
        if let Some(code) = invocation.exit_code {
            data = data.with("exit_code", code);
        }
        if let Some(reason) = &invocation.reason {
            data = data.with("reason", reason.clone());
        }
        if let Some(artifact) = &invocation.artifact {
            data = data.with("artifact", artifact.clone());
        }
        if let Err(error) = session.append(EntryKind::Custom(data)) {
            tracing::warn!(%error, hook = %invocation.hook, "cannot record hook invocation");
        }
    }

    fn record_contexts(&self, event: HookEvent, outcome: &Outcome) {
        let Some(session) = self.session else {
            return;
        };
        for context in &outcome.contexts {
            let Ok(mut session) = session.lock() else {
                return;
            };
            let data = CustomData::new("hook-context")
                .with("hook", context.hook.clone())
                .with("event", event.as_str())
                .with("text", context.text.clone());
            if let Err(error) = session.append(EntryKind::Custom(data)) {
                tracing::warn!(%error, "cannot record hook context");
            }
        }
    }
}

/// What happened when one hook ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvocationOutcome {
    Ok,
    Blocked,
    Error,
    Timeout,
}

impl InvocationOutcome {
    pub fn label(self) -> &'static str {
        match self {
            InvocationOutcome::Ok => "ok",
            InvocationOutcome::Blocked => "blocked",
            InvocationOutcome::Error => "error",
            InvocationOutcome::Timeout => "timeout",
        }
    }
}

/// The session record for one hook run. Every invocation gets one.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub hook: String,
    pub event: HookEvent,
    pub outcome: InvocationOutcome,
    pub duration_ms: u64,
    pub exit_code: Option<i32>,
    /// The block reason or the failure message, capped.
    pub reason: Option<String>,
    pub artifact: Option<String>,
}

/// A hook's explicit block.
#[derive(Debug, Clone)]
pub struct Block {
    pub hook: String,
    pub reason: String,
}

/// Extra context a hook attached to the event.
#[derive(Debug, Clone)]
pub struct Context {
    pub hook: String,
    pub text: String,
}

/// The combined result of every hook for one event.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub blocked: Option<Block>,
    /// Visible failures that did not block (`on_error = "warn"`).
    pub warnings: Vec<String>,
    pub contexts: Vec<Context>,
}

impl Outcome {
    pub fn is_blocked(&self) -> bool {
        self.blocked.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Allow,
    Block,
}

#[derive(Debug, Clone)]
struct Verdict {
    decision: Decision,
    reason: Option<String>,
    contexts: Vec<String>,
}

struct Run {
    invocation: Invocation,
    verdict: Option<Verdict>,
    warning: Option<String>,
}

/// Run every hook registered for `event`. Records each invocation in the
/// session. Stops at the first explicit block; a blocking failure also stops
/// the event.
pub async fn fire(env: &HookEnv<'_>, event: HookEvent, payload: Value) -> Outcome {
    let mut outcome = Outcome::default();
    let hooks: Vec<(String, HookConfig)> = env
        .config
        .hooks_for(event)
        .map(|(name, hook)| (name.clone(), hook.clone()))
        .collect();
    for (name, hook) in hooks {
        let run = run_one(env, &name, &hook, event, &payload).await;
        env.record(&run.invocation);
        if let Some(warning) = run.warning {
            outcome.warnings.push(warning);
        }
        let Some(verdict) = run.verdict else {
            continue;
        };
        if verdict.decision == Decision::Block {
            outcome.blocked = Some(Block {
                hook: name,
                reason: verdict
                    .reason
                    .unwrap_or_else(|| format!("hook blocked the {} action", event.as_str())),
            });
            break;
        }
        for text in verdict.contexts {
            outcome.contexts.push(Context {
                hook: name.clone(),
                text,
            });
        }
    }
    env.record_contexts(event, &outcome);
    outcome
}

/// Run exactly one named hook (used by `lh hooks test`). No session record.
pub async fn test_named(env: &HookEnv<'_>, name: &str) -> Result<(Invocation, Outcome), String> {
    let hook = env
        .config
        .entries
        .get(name)
        .ok_or_else(|| {
            let names: Vec<&str> = env.config.entries.keys().map(String::as_str).collect();
            format!(
                "no hook named {name:?}; configured hooks: {}",
                if names.is_empty() {
                    "none".to_string()
                } else {
                    names.join(", ")
                }
            )
        })?
        .clone();
    let payload = sample_payload(hook.event);
    let run = run_one(env, name, &hook, hook.event, &payload).await;
    let mut outcome = Outcome::default();
    if let Some(warning) = run.warning {
        outcome.warnings.push(warning);
    }
    if let Some(verdict) = run.verdict {
        if verdict.decision == Decision::Block {
            outcome.blocked = Some(Block {
                hook: name.to_string(),
                reason: verdict
                    .reason
                    .unwrap_or_else(|| "hook blocked the action".into()),
            });
        } else {
            for text in verdict.contexts {
                outcome.contexts.push(Context {
                    hook: name.to_string(),
                    text,
                });
            }
        }
    }
    Ok((run.invocation, outcome))
}

/// A representative payload for `lh hooks test <name>`.
pub fn sample_payload(event: HookEvent) -> Value {
    match event {
        HookEvent::SessionStart => json!({}),
        HookEvent::UserPrompt => json!({"prompt": "explain the failing test"}),
        HookEvent::PreTool => json!({
            "tool": {"name": "bash", "arguments": {"command": "echo hello"}}
        }),
        HookEvent::PostTool => json!({
            "tool": {"name": "bash", "arguments": {"command": "echo hello"}},
            "result": {"is_error": false, "content": "hello\n"},
            "duration_ms": 12
        }),
        HookEvent::PreCompact => json!({
            "compaction": {"reason": "auto", "tokens_before": 24_000}
        }),
        HookEvent::Stop => json!({"stop": {"status": "done"}}),
    }
}

pub async fn session_start(env: &HookEnv<'_>) -> Outcome {
    if env.session.is_some_and(start_recorded) {
        return Outcome::default();
    }
    fire(env, HookEvent::SessionStart, json!({})).await
}

fn start_recorded(session: &SessionHandle) -> bool {
    let Ok(session) = session.lock() else {
        return false;
    };
    session
        .customs_on_path("hook-invocation")
        .iter()
        .any(|entry| {
            matches!(
                &entry.kind,
                EntryKind::Custom(custom)
                    if custom.payload.get("event").and_then(Value::as_str) == Some("session_start")
                        && custom.payload.get("outcome").and_then(Value::as_str) == Some("ok")
            )
        })
}

pub async fn user_prompt(env: &HookEnv<'_>, prompt: &str) -> Outcome {
    fire(env, HookEvent::UserPrompt, json!({"prompt": prompt})).await
}

pub async fn pre_tool(env: &HookEnv<'_>, name: &str, arguments: &Value) -> Outcome {
    fire(
        env,
        HookEvent::PreTool,
        json!({"tool": {"name": name, "arguments": arguments}}),
    )
    .await
}

pub async fn post_tool(
    env: &HookEnv<'_>,
    name: &str,
    arguments: &Value,
    content: &str,
    is_error: bool,
    duration_ms: u64,
) -> Outcome {
    fire(
        env,
        HookEvent::PostTool,
        json!({
            "tool": {"name": name, "arguments": arguments},
            "result": {"is_error": is_error, "content": content},
            "duration_ms": duration_ms,
        }),
    )
    .await
}

pub async fn pre_compact(env: &HookEnv<'_>, reason: &str, tokens_before: u32) -> Outcome {
    fire(
        env,
        HookEvent::PreCompact,
        json!({"compaction": {"reason": reason, "tokens_before": tokens_before}}),
    )
    .await
}

pub async fn stop(env: &HookEnv<'_>, status: &str) -> Outcome {
    fire(env, HookEvent::Stop, json!({"stop": {"status": status}})).await
}

async fn run_one(
    env: &HookEnv<'_>,
    name: &str,
    hook: &HookConfig,
    event: HookEvent,
    payload: &Value,
) -> Run {
    let started = Instant::now();
    let deadline = env.config.timeout(hook);
    let on_error = hook.on_error();
    let mut body = payload.clone();
    if !body.is_object() {
        body = json!({"payload": body});
    }
    body["event"] = json!(event.as_str());
    body["hook"] = json!(name);
    body["session"] = json!({
        "id": env.identity.session_id,
        "cwd": env.cwd.display().to_string(),
        "model": env.identity.model,
        "profile": env.identity.profile,
    });
    let input = match serde_json::to_vec(&body) {
        Ok(input) => input,
        Err(error) => {
            return failure(
                name,
                event,
                on_error,
                format!("cannot serialize the hook payload: {error}"),
                started.elapsed().as_millis() as u64,
                None,
                env,
            );
        }
    };

    let mut command = Command::new(&hook.command[0]);
    command
        .args(&hook.command[1..])
        .current_dir(env.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let run = async move {
        let mut child = command.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&input).await?;
            stdin.shutdown().await?;
        }
        child.wait_with_output().await
    };

    let output = match tokio::time::timeout(deadline, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return failure(
                name,
                event,
                on_error,
                format!(
                    "cannot start hook {name:?} ({:?}): {error}",
                    hook.command[0]
                ),
                started.elapsed().as_millis() as u64,
                None,
                env,
            );
        }
        Err(_) => {
            let duration_ms = started.elapsed().as_millis() as u64;
            let message = format!(
                "hook {name:?} exceeded {} s and was killed; the action did not run",
                deadline.as_secs()
            );
            let (reason, artifact) = archive_text(env, &message);
            let blocked = on_error == HookOnError::Block;
            return Run {
                invocation: Invocation {
                    hook: name.to_string(),
                    event,
                    outcome: InvocationOutcome::Timeout,
                    duration_ms,
                    exit_code: None,
                    reason: Some(reason),
                    artifact,
                },
                verdict: blocked.then(|| Verdict {
                    decision: Decision::Block,
                    reason: Some(format!(
                        "hook {name:?} timed out after {} s",
                        deadline.as_secs()
                    )),
                    contexts: Vec::new(),
                }),
                warning: (!blocked)
                    .then(|| format!("{message}; on_error = \"warn\", so the action continued")),
            };
        }
    };

    let duration_ms = started.elapsed().as_millis() as u64;
    let exit_code = output.status.code();
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let full = format!("stdout:\n{stdout}\n\nstderr:\n{stderr}");
    let (recorded, artifact) = archive_text(env, &full);
    let _ = recorded;

    match exit_code {
        Some(0) => match parse_output(&stdout) {
            Ok(verdict) => {
                let blocked = verdict.decision == Decision::Block;
                let reason = verdict
                    .reason
                    .as_deref()
                    .map(|reason| clip(reason, REASON_CAP_CHARS));
                Run {
                    invocation: Invocation {
                        hook: name.to_string(),
                        event,
                        outcome: if blocked {
                            InvocationOutcome::Blocked
                        } else {
                            InvocationOutcome::Ok
                        },
                        duration_ms,
                        exit_code,
                        reason,
                        artifact,
                    },
                    verdict: Some(verdict),
                    warning: None,
                }
            }
            Err(message) => failure_with(
                name,
                event,
                on_error,
                format!("hook {name:?} returned malformed output: {message}"),
                duration_ms,
                exit_code,
                artifact,
            ),
        },
        Some(2) => {
            let reason = first_line(&stderr)
                .or_else(|| first_line(&stdout))
                .unwrap_or_else(|| "hook exited 2".to_string());
            let reason = clip(&reason, REASON_CAP_CHARS);
            Run {
                invocation: Invocation {
                    hook: name.to_string(),
                    event,
                    outcome: InvocationOutcome::Blocked,
                    duration_ms,
                    exit_code,
                    reason: Some(reason.clone()),
                    artifact,
                },
                verdict: Some(Verdict {
                    decision: Decision::Block,
                    reason: Some(reason),
                    contexts: Vec::new(),
                }),
                warning: None,
            }
        }
        other => {
            let stderr = first_line(&stderr).unwrap_or_default();
            let message = match other {
                Some(code) => format!("hook {name:?} exited with status {code}"),
                None => format!("hook {name:?} was killed by a signal"),
            };
            let message = if stderr.is_empty() {
                message
            } else {
                format!("{message}: {stderr}")
            };
            failure_with(
                name,
                event,
                on_error,
                message,
                duration_ms,
                exit_code,
                artifact,
            )
        }
    }
}

fn failure(
    name: &str,
    event: HookEvent,
    on_error: HookOnError,
    message: String,
    duration_ms: u64,
    exit_code: Option<i32>,
    env: &HookEnv<'_>,
) -> Run {
    let (recorded, artifact) = archive_text(env, &message);
    failure_with(
        name,
        event,
        on_error,
        recorded,
        duration_ms,
        exit_code,
        artifact,
    )
}

fn failure_with(
    name: &str,
    event: HookEvent,
    on_error: HookOnError,
    message: String,
    duration_ms: u64,
    exit_code: Option<i32>,
    artifact: Option<String>,
) -> Run {
    let blocked = on_error == HookOnError::Block;
    let warning = (!blocked).then(|| {
        format!("{message}; on_error = \"warn\", so the action continued without this hook")
    });
    let reason = clip(&message, REASON_CAP_CHARS);
    Run {
        invocation: Invocation {
            hook: name.to_string(),
            event,
            outcome: InvocationOutcome::Error,
            duration_ms,
            exit_code,
            reason: Some(reason.clone()),
            artifact,
        },
        verdict: blocked.then(|| Verdict {
            decision: Decision::Block,
            reason: Some(reason),
            contexts: Vec::new(),
        }),
        warning,
    }
}

/// Parse the one JSON object a hook writes. Unknown fields are ignored: a
/// hook cannot add tools, grant scope, change the endpoint, or change the
/// permission mode through its output.
fn parse_output(stdout: &str) -> Result<Verdict, String> {
    if stdout.len() > PARSE_LIMIT_BYTES {
        return Err(format!(
            "stdout is {} bytes; the limit is {}",
            stdout.len(),
            PARSE_LIMIT_BYTES
        ));
    }
    let value: Value =
        serde_json::from_str(stdout).map_err(|error| format!("not one JSON object: {error}"))?;
    let Some(object) = value.as_object() else {
        return Err("stdout is JSON but not an object".into());
    };
    let decision = match object.get("decision").and_then(Value::as_str) {
        None | Some("allow") | Some("continue") => Decision::Allow,
        Some("block") => Decision::Block,
        Some(other) => {
            return Err(format!(
                "unknown decision {other:?}; use \"allow\" or \"block\""
            ));
        }
    };
    if object
        .get("decision")
        .is_some_and(|decision| !decision.is_string())
    {
        return Err("`decision` must be a string".into());
    }
    let reason = match object.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(reason)) => Some(reason.clone()),
        Some(_) => return Err("`reason` must be a string".into()),
    };
    let mut contexts = Vec::new();
    match object.get("context") {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => contexts.push(text.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                match item {
                    Value::String(text) => contexts.push(text.clone()),
                    _ => return Err("`context` array entries must be strings".into()),
                }
            }
        }
        Some(_) => return Err("`context` must be a string or an array of strings".into()),
    }
    Ok(Verdict {
        decision,
        reason: reason.map(|reason| clip(&reason, REASON_CAP_CHARS)),
        contexts: contexts
            .into_iter()
            .map(|text| clip(&text, REASON_CAP_CHARS))
            .collect(),
    })
}

/// Store oversized hook output and return a capped preview plus the artifact
/// id. A missing artifact store keeps the preview only.
fn archive_text(env: &HookEnv<'_>, text: &str) -> (String, Option<String>) {
    let limit = env.config.max_output_bytes.max(256);
    if text.len() <= limit {
        return (text.to_string(), None);
    }
    let artifact = env
        .artifacts
        .and_then(|store| store.write(text).ok())
        .map(|artifact| artifact.id);
    (clip(text, limit), artifact)
}

fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n[truncated]");
    out
}

fn first_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, HookConfig, HooksConfig};

    fn env<'a>(config: &'a HooksConfig, dir: &'a Path) -> HookEnv<'a> {
        HookEnv {
            config,
            cwd: dir,
            session: None,
            artifacts: None,
            identity: HookIdentity {
                session_id: "test".into(),
                model: "m".into(),
                profile: "p".into(),
            },
        }
    }

    fn hook(event: HookEvent, command: &[&str], on_error: Option<HookOnError>) -> HookConfig {
        HookConfig {
            event,
            command: command.iter().map(|arg| arg.to_string()).collect(),
            timeout_secs: 0,
            on_error,
        }
    }

    #[test]
    fn output_parsing_ignores_policy_fields() {
        let verdict = parse_output(
            r#"{"decision":"allow","reason":"ok","tools":["bash"],"scope":["*"],"endpoint":"http://evil","permission_mode":"bypass"}"#,
        )
        .unwrap();
        assert_eq!(verdict.decision, Decision::Allow);
        assert_eq!(verdict.reason.as_deref(), Some("ok"));
        assert!(verdict.contexts.is_empty());
    }

    #[test]
    fn output_parsing_accepts_context_forms_and_rejects_unknown_decisions() {
        let verdict = parse_output(r#"{"context":["one","two"]}"#).unwrap();
        assert_eq!(verdict.decision, Decision::Allow);
        assert_eq!(verdict.contexts, ["one", "two"]);
        let verdict = parse_output(r#"{"decision":"block","context":"note"}"#).unwrap();
        assert_eq!(verdict.decision, Decision::Block);
        assert_eq!(verdict.contexts, ["note"]);
        assert!(parse_output(r#"{"decision":"maybe"}"#).is_err());
        assert!(parse_output("not json").is_err());
        assert!(parse_output("[]").is_err());
    }

    #[tokio::test]
    async fn exit_two_blocks_with_stderr_reason() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("block.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ncat >/dev/null\necho 'no writes on main' >&2\nexit 2\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut config = HooksConfig::default();
        config.entries.insert(
            "guard".into(),
            hook(HookEvent::PreTool, &[script.to_str().unwrap()], None),
        );
        let outcome = pre_tool(&env(&config, dir.path()), "bash", &json!({"command": "rm"})).await;
        assert_eq!(
            outcome.blocked.as_ref().unwrap().reason,
            "no writes on main"
        );
    }

    #[tokio::test]
    async fn missing_binary_fails_closed_by_default_on_pre_tool() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = HooksConfig::default();
        config.entries.insert(
            "guard".into(),
            hook(HookEvent::PreTool, &["/definitely/not/a/hook"], None),
        );
        let outcome = pre_tool(&env(&config, dir.path()), "bash", &json!({})).await;
        assert!(outcome.is_blocked());
        assert!(
            outcome
                .blocked
                .unwrap()
                .reason
                .contains("cannot start hook")
        );
        assert!(outcome.warnings.is_empty());
    }

    #[tokio::test]
    async fn warn_on_error_is_visible_and_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = HooksConfig::default();
        config.entries.insert(
            "notify".into(),
            hook(
                HookEvent::Stop,
                &["/definitely/not/a/hook"],
                Some(HookOnError::Warn),
            ),
        );
        let outcome = stop(&env(&config, dir.path()), "done").await;
        assert!(!outcome.is_blocked());
        assert_eq!(outcome.warnings.len(), 1);
        assert!(outcome.warnings[0].contains("on_error = \"warn\""));
    }

    #[tokio::test]
    async fn hook_timeout_blocks_without_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = HooksConfig {
            timeout_secs: 1,
            ..HooksConfig::default()
        };
        config.entries.insert(
            "slow".into(),
            hook(HookEvent::PreTool, &["sleep", "5"], None),
        );
        let started = Instant::now();
        let outcome = pre_tool(&env(&config, dir.path()), "bash", &json!({})).await;
        assert!(started.elapsed() < std::time::Duration::from_secs(4));
        assert!(outcome.is_blocked());
    }

    #[test]
    fn hooks_config_parses_defaults_and_rejects_bad_entries() {
        let config: Config = toml::from_str(
            r#"
[hooks]
timeout_secs = 9

[hooks.guard]
event = "pre_tool"
command = ["/bin/true"]

[hooks.notify]
event = "stop"
command = ["/bin/true"]
on_error = "warn"
"#,
        )
        .unwrap();
        assert_eq!(config.hooks.timeout_secs, 9);
        assert_eq!(config.hooks.entries["guard"].on_error(), HookOnError::Block);
        assert_eq!(config.hooks.entries["notify"].on_error(), HookOnError::Warn);
        assert!(toml::from_str::<Config>("[hooks.x]\nevent = \"sometimes\"\n").is_err());
        assert!(
            toml::from_str::<Config>("[hooks.x]\nevent = \"pre_tool\"\ncommand = []\n")
                .unwrap()
                .hooks
                .validate()
                .is_err()
        );
    }
}

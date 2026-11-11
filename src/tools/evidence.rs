//! `evidence` tool: inspect and verify the engagement evidence chain.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, object_schema, optional_str, optional_u64,
};

const PREVIEW_CHARS: usize = 4_000;

pub struct EvidenceTool;

impl EvidenceTool {
    fn list(ctx: &ToolContext, args: &Value) -> ToolResult {
        let limit = optional_u64(args, "limit").unwrap_or(50).clamp(1, 500) as usize;
        let mut state = match ctx.security.lock() {
            Ok(state) => state,
            Err(_) => {
                return ToolResult::err("SECURITY_IO", "security state is poisoned; restart lh");
            }
        };
        let engagement = match state.ensure_engagement(&ctx.paths) {
            Ok(engagement) => engagement,
            Err(error) => {
                return ToolResult::err(
                    "SECURITY_IO",
                    format!("cannot open the evidence ledger: {error}"),
                );
            }
        };
        let entries = engagement.evidence.list();
        if entries.is_empty() {
            return ToolResult::ok(format!(
                "engagement {}: no evidence recorded; run bash, bg, or an external tool while the engagement is active",
                state.engagement_name
            ));
        }
        let mut out = format!(
            "evidence: {} entries (showing up to {limit})\n",
            entries.len()
        );
        for entry in entries
            .iter()
            .rev()
            .take(limit)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            out.push_str(&format!(
                "{} {} target={} exit={} sha256={} bytes={} artifact={}\n",
                entry.id,
                entry.tool,
                short(&entry.target, 60),
                entry
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "-".into()),
                &entry.sha256[..entry.sha256.len().min(12)],
                entry.bytes,
                entry.artifact
            ));
        }
        out.push_str("next: `evidence show id=e1` or `evidence verify id=e1`");
        ToolResult::ok(out)
    }

    fn show(ctx: &ToolContext, args: &Value) -> ToolResult {
        let id = match optional_str(args, "id") {
            Some(id) => id,
            None => {
                return ToolResult::err(
                    "invalid_args",
                    "evidence show needs \"id\"; `evidence list` shows ids",
                );
            }
        };
        let mut state = match ctx.security.lock() {
            Ok(state) => state,
            Err(_) => {
                return ToolResult::err("SECURITY_IO", "security state is poisoned; restart lh");
            }
        };
        let engagement = match state.ensure_engagement(&ctx.paths) {
            Ok(engagement) => engagement,
            Err(error) => {
                return ToolResult::err(
                    "SECURITY_IO",
                    format!("cannot open the evidence ledger: {error}"),
                );
            }
        };
        let (entry, content) = match engagement.evidence.read(&id) {
            Ok(read) => read,
            Err(error) => {
                return ToolResult::err("EVIDENCE_NOT_FOUND", error.to_string());
            }
        };
        let path = ctx
            .paths
            .engagement_dir(&state.engagement_name)
            .join(&entry.artifact);
        let mut out = format!(
            "{} tool={} target={}\ncommand: {}\nstarted={} finished={} duration={}ms exit={} sha256={} artifact={}\n\n",
            entry.id,
            entry.tool,
            entry.target,
            short(&entry.command, 500),
            entry.started.to_rfc3339(),
            entry.finished.to_rfc3339(),
            entry.duration_ms,
            entry
                .exit_code
                .map(|code| code.to_string())
                .unwrap_or_else(|| "-".into()),
            entry.sha256,
            path.display()
        );
        if content.chars().count() > PREVIEW_CHARS {
            out.push_str(&content.chars().take(PREVIEW_CHARS).collect::<String>());
            out.push_str(&format!(
                "\n… preview truncated; full artifact: {}",
                path.display()
            ));
        } else {
            out.push_str(&content);
        }
        ToolResult::ok(out).with_details(json!({"evidence": entry.id, "artifact": entry.artifact}))
    }

    fn verify(ctx: &ToolContext, args: &Value) -> ToolResult {
        let id = match optional_str(args, "id") {
            Some(id) => id,
            None => {
                return ToolResult::err(
                    "invalid_args",
                    "evidence verify needs \"id\"; `evidence list` shows ids",
                );
            }
        };
        let mut state = match ctx.security.lock() {
            Ok(state) => state,
            Err(_) => {
                return ToolResult::err("SECURITY_IO", "security state is poisoned; restart lh");
            }
        };
        let engagement = match state.ensure_engagement(&ctx.paths) {
            Ok(engagement) => engagement,
            Err(error) => {
                return ToolResult::err(
                    "SECURITY_IO",
                    format!("cannot open the evidence ledger: {error}"),
                );
            }
        };
        match engagement.evidence.verify(&id) {
            Ok((entry, ok)) => {
                let content = if ok {
                    if entry.redacted {
                        format!(
                            "{} verified: stored artifact matches its redacted sha256; raw output sha256 {}",
                            entry.id, entry.sha256
                        )
                    } else {
                        format!(
                            "{} verified: sha256 {} matches the artifact",
                            entry.id, entry.sha256
                        )
                    }
                } else {
                    format!(
                        "{} FAILED verification: artifact {} no longer matches its recorded sha256",
                        entry.id, entry.artifact
                    )
                };
                ToolResult::ok(content).with_details(json!({"evidence": entry.id, "verified": ok}))
            }
            Err(error) => ToolResult::err("EVIDENCE_NOT_FOUND", error.to_string()),
        }
    }
}

fn short(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

#[async_trait]
impl Tool for EvidenceTool {
    fn name(&self) -> &'static str {
        "evidence"
    }

    fn description(&self) -> &'static str {
        "Inspect the engagement evidence chain: list, show, or verify SHA-256."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "action": {"type": "string", "enum": ["list", "show", "verify"]},
                "id": {"type": "string", "description": "evidence id, e.g. e3"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "list: most recent entries to show."}
            }),
            &["action"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Security
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let action = optional_str(&args, "action").unwrap_or_else(|| "list".into());
        match action.as_str() {
            "list" => Self::list(ctx, &args),
            "show" => Self::show(ctx, &args),
            "verify" => Self::verify(ctx, &args),
            other => ToolResult::err(
                "invalid_args",
                format!("unknown evidence action {other:?}; use list, show, or verify"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::evidence::NewEvidence;
    use chrono::Utc;

    #[tokio::test]
    async fn lists_shows_and_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = crate::tools::write::test_context(dir.path());
        {
            let mut state = ctx.security.lock().unwrap();
            let engagement = state.ensure_engagement(&ctx.paths).unwrap();
            let now = Utc::now();
            engagement
                .evidence
                .add(NewEvidence {
                    tool: "bash".into(),
                    session: Some("s1".into()),
                    entry: Some("t1".into()),
                    target: "example.com".into(),
                    command: "nmap example.com".into(),
                    started: now,
                    finished: now,
                    duration_ms: 5,
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    is_error: false,
                    output: "443/tcp open https\n".into(),
                    summary: None,
                })
                .unwrap();
        }
        let list = EvidenceTool.call(&ctx, json!({"action": "list"})).await;
        assert!(!list.is_error, "{}", list.content);
        assert!(list.content.contains("e1 bash"), "{}", list.content);
        let show = EvidenceTool
            .call(&ctx, json!({"action": "show", "id": "e1"}))
            .await;
        assert!(!show.is_error, "{}", show.content);
        assert!(
            show.content.contains("443/tcp open https"),
            "{}",
            show.content
        );
        let verify = EvidenceTool
            .call(&ctx, json!({"action": "verify", "id": "e1"}))
            .await;
        assert!(!verify.is_error, "{}", verify.content);
        assert!(verify.content.contains("verified"), "{}", verify.content);
    }
}

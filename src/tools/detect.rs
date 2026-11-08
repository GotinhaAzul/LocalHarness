//! `detect` tool: validate and run the Sigma-subset detection rules.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};

use crate::security::detect::{Hit, RuleSet, run};
use crate::security::evidence::NewEvidence;
use crate::security::findings::{EvidenceRef, NewFinding};
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, object_schema, optional_str,
    optional_u64,
};

pub struct DetectTool;

impl DetectTool {
    fn validate(ctx: &ToolContext, rules: &str) -> ToolResult {
        let path = match ctx.resolve(rules) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let set = RuleSet::load(&path);
        if !set.errors.is_empty() {
            let mut content = format!(
                "detection rules: {} valid, {} error(s)\n",
                set.rules.len(),
                set.errors.len()
            );
            for error in &set.errors {
                content.push_str(&format!("  {error}\n"));
            }
            return ToolResult::err("DETECT_RULES", content);
        }
        if set.rules.is_empty() {
            return ToolResult::err(
                "DETECT_RULES",
                format!("no .yml/.yaml rules found under {}", path.display()),
            );
        }
        ToolResult::ok(format!(
            "validated {} rule(s) under {}",
            set.rules.len(),
            display_path(&ctx.cwd, &path)
        ))
    }

    fn run(ctx: &ToolContext, args: &Value) -> ToolResult {
        let rules_arg = match optional_str(args, "rules") {
            Some(rules) => rules,
            None => {
                return ToolResult::err(
                    "invalid_args",
                    "detect run needs \"rules\" (file or directory)",
                );
            }
        };
        let target_arg = match optional_str(args, "target") {
            Some(target) => target,
            None => {
                return ToolResult::err(
                    "invalid_args",
                    "detect run needs \"target\" (log or sample file/directory)",
                );
            }
        };
        let rules_path = match ctx.resolve(&rules_arg) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let target_path = match ctx.resolve(&target_arg) {
            Ok(path) => path,
            Err(error) => return error,
        };
        let limit = optional_u64(args, "limit").unwrap_or(200).clamp(1, 500) as usize;
        let finding_enabled = args.get("finding").and_then(Value::as_bool).unwrap_or(true);

        let set = RuleSet::load(&rules_path);
        if set.rules.is_empty() {
            let mut content = format!(
                "no runnable detection rules under {}",
                display_path(&ctx.cwd, &rules_path)
            );
            for error in &set.errors {
                content.push_str(&format!("\n  {error}"));
            }
            return ToolResult::err("DETECT_RULES", content);
        }
        let started = Utc::now();
        let hits = match run(&set.rules, &target_path, &ctx.cwd, limit) {
            Ok(hits) => hits,
            Err(error) => return ToolResult::err("DETECT_TARGET", error),
        };
        let finished = Utc::now();
        let target_display = display_path(&ctx.cwd, &target_path);
        let rules_display = display_path(&ctx.cwd, &rules_path);

        let mut evidence_body = format!(
            "detect: {} rule(s), target {target_display}\n",
            set.rules.len()
        );
        for hit in &hits {
            evidence_body.push_str(&format!(
                "{}:{} {} [{}] {}\n",
                hit.source, hit.line, hit.level, hit.title, hit.message
            ));
        }
        if hits.is_empty() {
            evidence_body.push_str("no hits\n");
        }

        let session_id = ctx.session.lock().ok().map(|session| session.id.clone());
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
                    format!("cannot open the engagement: {error}"),
                );
            }
        };
        let evidence = match engagement.evidence.add(NewEvidence {
            tool: "detect".into(),
            session: session_id.clone(),
            entry: None,
            target: target_display.clone(),
            command: format!("detect rules={rules_display} target={target_display}"),
            started,
            finished,
            duration_ms: (finished - started).num_milliseconds().max(0) as u64,
            exit_code: Some(0),
            timed_out: false,
            cancelled: false,
            is_error: false,
            output: evidence_body,
            summary: Some(format!("{} hit(s)", hits.len())),
        }) {
            Ok(evidence) => evidence,
            Err(error) => {
                return ToolResult::err(
                    "SECURITY_IO",
                    format!("cannot record detection evidence: {error}"),
                );
            }
        };

        let mut created = Vec::new();
        let mut skipped = 0usize;
        let mut errors = Vec::new();
        if finding_enabled && !hits.is_empty() {
            let groups = group_hits(&hits);
            for ((_, title, source), group) in groups {
                let finding_title = format!("Detection: {title}");
                if engagement
                    .findings
                    .iter()
                    .any(|finding| finding.title == finding_title && finding.target == source)
                {
                    skipped += 1;
                    continue;
                }
                let first = group[0];
                let evidence_text = format!(
                    "rule `{}` matched {} line(s) in {}; first hit at line {}: {}",
                    first.rule_id,
                    group.len(),
                    source,
                    first.line,
                    first.message
                );
                let new = NewFinding {
                    title: finding_title,
                    severity: first.level,
                    target: source.clone(),
                    evidence: Some(evidence_text),
                    evidence_ref: Some(EvidenceRef {
                        evidence: Some(evidence.id.clone()),
                        session: session_id.clone(),
                        entry: None,
                        artifact: Some(evidence.artifact.clone()),
                    }),
                    impact: None,
                    remediation: None,
                    session_id: session_id.clone(),
                    entry_id: None,
                };
                match engagement.add(new) {
                    Ok(finding) => created.push(finding.id),
                    Err(error) => errors.push(error),
                }
            }
        }

        let mut out = format!(
            "detect: {} rule(s), {} hit(s) in {target_display}\n",
            set.rules.len(),
            hits.len()
        );
        for hit in &hits {
            out.push_str(&format!(
                "  {}:{} {} [{}] {} — {}\n",
                hit.source, hit.line, hit.level, hit.rule_id, hit.title, hit.message
            ));
        }
        if hits.is_empty() {
            out.push_str("no hits\n");
        }
        out.push_str(&format!("evidence: {}\n", evidence.id));
        if finding_enabled {
            if created.is_empty() && skipped == 0 {
                out.push_str("findings: none (no hits)\n");
            } else {
                out.push_str(&format!(
                    "findings: {} created, {} already present\n",
                    created.len(),
                    skipped
                ));
                if !created.is_empty() {
                    out.push_str(&format!("  {}\n", created.join(", ")));
                }
            }
        } else {
            out.push_str("findings: disabled (finding=false)\n");
        }
        for error in &errors {
            out.push_str(&format!("finding warning: {error}\n"));
        }
        if !set.errors.is_empty() {
            out.push_str(&format!(
                "warning: {} invalid rule(s) skipped; run detect validate for details\n",
                set.errors.len()
            ));
        }
        out.push_str("next: `evidence show id=");
        out.push_str(&evidence.id);
        out.push_str("`, `finding list`, or `attack_path summary`");

        ToolResult::ok(out).with_details(json!({
            "evidence": evidence.id,
            "hits": hits.len(),
            "findings": created,
            "rules": set.rules.len(),
        }))
    }
}

fn group_hits(hits: &[Hit]) -> BTreeMap<(String, String, String), Vec<&Hit>> {
    let mut groups: BTreeMap<(String, String, String), Vec<&Hit>> = BTreeMap::new();
    for hit in hits {
        groups
            .entry((hit.rule_id.clone(), hit.title.clone(), hit.source.clone()))
            .or_default()
            .push(hit);
    }
    groups
}

#[async_trait]
impl Tool for DetectTool {
    fn name(&self) -> &'static str {
        "detect"
    }

    fn description(&self) -> &'static str {
        "Validate and run Sigma-subset detection rules against local logs or samples."
    }

    fn schema(&self) -> Value {
        object_schema(
            json!({
                "action": {"type": "string", "enum": ["validate", "run"]},
                "rules": {"type": "string", "description": "Rule file or directory."},
                "target": {"type": "string", "description": "Log/sample file or directory for run."},
                "finding": {"type": "boolean", "description": "Create findings for hits (default true)."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "run: maximum hits to report."}
            }),
            &["action"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Security
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let action = optional_str(&args, "action").unwrap_or_else(|| "validate".into());
        match action.as_str() {
            "validate" => match optional_str(&args, "rules") {
                Some(rules) => Self::validate(ctx, &rules),
                None => ToolResult::err(
                    "invalid_args",
                    "detect validate needs \"rules\" (file or directory)",
                ),
            },
            "run" => Self::run(ctx, &args),
            other => ToolResult::err(
                "invalid_args",
                format!("unknown detect action {other:?}; use validate or run"),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn validates_runs_and_maps_hits_to_findings() {
        let dir = tempfile::tempdir().unwrap();
        let rules = dir.path().join("rules");
        std::fs::create_dir_all(&rules).unwrap();
        std::fs::write(
            rules.join("evil.yml"),
            "title: Suspicious curl\nid: evil-curl\nlevel: high\ndetection:\n  selection:\n    message|contains: evil\n  condition: selection\n",
        )
        .unwrap();
        let target = dir.path().join("sample.log");
        std::fs::write(&target, "ok\nsomething evil happened\n").unwrap();
        let ctx = crate::tools::write::test_context(dir.path());

        let validated = DetectTool
            .call(&ctx, json!({"action": "validate", "rules": "rules"}))
            .await;
        assert!(!validated.is_error, "{}", validated.content);
        assert!(
            validated.content.contains("validated 1"),
            "{}",
            validated.content
        );

        let result = DetectTool
            .call(
                &ctx,
                json!({"action": "run", "rules": "rules", "target": "sample.log"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("1 hit"), "{}", result.content);
        assert!(result.content.contains("evil-curl"), "{}", result.content);

        let mut state = ctx.security.lock().unwrap();
        let engagement = state.ensure_engagement(&ctx.paths).unwrap();
        assert_eq!(engagement.evidence.list().len(), 1);
        assert_eq!(engagement.findings.len(), 1);
        let finding = &engagement.findings[0];
        assert_eq!(finding.title, "Detection: Suspicious curl");
        assert_eq!(finding.severity, crate::security::Severity::High);
        assert_eq!(
            finding
                .evidence_ref
                .as_ref()
                .and_then(|reference| reference.evidence.as_deref()),
            Some("e1")
        );
    }
}

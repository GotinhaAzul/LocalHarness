//! `finding` tool: the engagement findings ledger.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::MutexGuard;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};

use crate::security::evidence::NewEvidence;
use crate::security::findings::{MAX_EVIDENCE_CHARS, clip};
use crate::security::verify::{
    MinimizeOptions, PocStep, ReplayOptions, VerifyStatus, execute_steps, minimize_steps,
    validate_steps,
};
use crate::security::{
    EvidenceRef, FindingPatch, NewFinding, SecurityState, Severity, Status, render_report,
};
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, optional_bool, optional_str,
    optional_u64,
};

/// Cap on the report Markdown echoed back in the tool result.
const REPORT_CHARS: usize = 18_000;

pub struct FindingTool;

impl FindingTool {
    fn add(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
        let title = required(args, "title")?;
        let severity = required(args, "severity")?
            .parse::<Severity>()
            .map_err(arg_err)?;
        let target = required(args, "target")?;

        let session_id = ctx.session.lock().ok().map(|session| session.id.clone());
        let explicit_artifact = optional_str(args, "artifact");
        let explicit_evidence = optional_str(args, "evidence_id");

        let mut evidence = non_empty(args, "evidence");
        let mut evidence_ref = explicit_ref(args);
        let mut entry_id = optional_str(args, "entry");

        if evidence.is_none() && explicit_evidence.is_none() {
            let Some((content, entry, artifact)) = last_tool_result(ctx) else {
                return Err(arg_err(
                    "evidence is required when the branch has no tool result; pass {\"evidence\":\"...\"} or run a tool first",
                ));
            };
            evidence = Some(clip(&content, MAX_EVIDENCE_CHARS));
            let reference = EvidenceRef {
                evidence: None,
                session: session_id.clone(),
                entry: Some(entry.clone()),
                artifact: explicit_artifact.or(artifact),
            };
            evidence_ref = Some(reference);
            entry_id = Some(entry);
        }
        if explicit_evidence.is_none()
            && entry_id.is_none()
            && evidence_ref
                .as_ref()
                .and_then(|reference| reference.entry.as_ref())
                .is_none()
            && let Some((_, entry, _)) = last_tool_result(ctx)
        {
            entry_id = Some(entry);
        }

        let mut state = lock_security(ctx)?;
        let engagement = state
            .ensure_engagement(&ctx.paths)
            .map_err(|err| security_io(&ctx.paths, err))?;
        if let Some(id) = explicit_evidence.as_deref() {
            if engagement.evidence.get(id).is_none() {
                return Err(ToolResult::err(
                    "EVIDENCE_NOT_FOUND",
                    format!("no evidence {id:?}; `evidence list` shows ids"),
                ));
            }
            if evidence.is_none()
                && let Ok((_, content)) = engagement.evidence.read(id)
            {
                evidence = Some(clip(&content, MAX_EVIDENCE_CHARS));
            }
        }
        if explicit_evidence.is_none()
            && evidence_ref
                .as_ref()
                .and_then(|reference| reference.evidence.as_ref())
                .is_none()
            && let (Some(session), Some(entry)) = (&session_id, &entry_id)
            && let Some(chain) = engagement.evidence.last_for_entry(session, entry)
        {
            let reference = evidence_ref.get_or_insert_with(EvidenceRef::default);
            reference.evidence = Some(chain.id.clone());
            if reference.artifact.is_none() {
                reference.artifact = Some(chain.artifact.clone());
            }
        }

        let new = NewFinding {
            title,
            severity,
            target,
            evidence,
            evidence_ref,
            impact: non_empty(args, "impact"),
            remediation: non_empty(args, "remediation"),
            session_id,
            entry_id,
            poc_steps: parse_steps(args)?.unwrap_or_default(),
            confidence: parse_confidence(args)?,
        };
        let finding = engagement
            .add(new)
            .map_err(|msg| ToolResult::err("FINDING_ARGS", msg))?;

        let from_entry = finding
            .evidence_ref
            .as_ref()
            .and_then(|reference| reference.entry.as_deref());
        let mut content = format!(
            "added {} ({}, {}): {}\ntarget: {}",
            finding.id, finding.severity, finding.status, finding.title, finding.target
        );
        if let Some(entry) = from_entry {
            content.push_str(&format!("\nevidence: attached from entry {entry}"));
        }
        content.push_str(
            "\nnext: `finding list` shows the ledger; `finding report` renders the engagement report.",
        );

        Ok(ToolResult::ok(content)
            .with_details(json!({"finding": to_json(&finding), "id": finding.id})))
    }

    fn update(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
        let id = required(args, "id")?;
        let mut patch = FindingPatch::default();
        let mut changed: Vec<&str> = Vec::new();

        if let Some(title) = non_empty(args, "title") {
            patch.title = Some(title);
            changed.push("title");
        }
        if let Some(severity) = non_empty(args, "severity") {
            patch.severity = Some(severity.parse::<Severity>().map_err(arg_err)?);
            changed.push("severity");
        }
        if let Some(status) = non_empty(args, "status") {
            patch.status = Some(status.parse::<Status>().map_err(arg_err)?);
            changed.push("status");
        }
        if let Some(target) = non_empty(args, "target") {
            patch.target = Some(target);
            changed.push("target");
        }
        if let Some(evidence) = non_empty(args, "evidence") {
            patch.evidence = Some(evidence);
            changed.push("evidence");
        }
        if let Some(impact) = non_empty(args, "impact") {
            patch.impact = Some(impact);
            changed.push("impact");
        }
        if let Some(remediation) = non_empty(args, "remediation") {
            patch.remediation = Some(remediation);
            changed.push("remediation");
        }
        if let Some(steps) = parse_steps(args)? {
            patch.poc_steps = Some(steps);
            changed.push("poc_steps");
        }
        if let Some(confidence) = parse_confidence(args)? {
            patch.confidence = Some(confidence);
            changed.push("confidence");
        }

        if changed.is_empty() {
            return Err(arg_err(
                "nothing to update; pass at least one of title, severity, status, target, evidence, impact, remediation, poc_steps, or confidence",
            ));
        }

        let mut state = lock_security(ctx)?;
        let engagement_name = state.engagement_name.clone();
        let Some(engagement) = state.engagement.as_mut() else {
            return Err(not_found(&id, &engagement_name));
        };
        if engagement.get(&id).is_none() {
            return Err(not_found(&id, &engagement_name));
        }
        let finding = engagement
            .update(&id, patch)
            .map_err(|msg| ToolResult::err("FINDING_ARGS", msg))?;

        Ok(ToolResult::ok(format!(
            "updated {}: {}\n{} · {} · {} · {}",
            finding.id,
            changed.join(", "),
            finding.id,
            finding.severity,
            finding.status,
            finding.title
        ))
        .with_details(json!({"finding": to_json(&finding), "changed": changed})))
    }

    async fn verify(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
        let id = required(args, "id")?;
        if let Some(runner) = optional_str(args, "runner")
            && runner != "local"
        {
            return Err(arg_err(
                "only runner=local is implemented; container and QEMU replay are not part of this build",
            ));
        }
        let steps_arg = parse_steps(args)?;
        let minimize = optional_bool(args, "minimize").unwrap_or(false);
        let max_oracle_calls = optional_u64(args, "max_oracle_calls")
            .unwrap_or(20)
            .clamp(1, 50) as usize;
        let timeout = Duration::from_secs(
            optional_u64(args, "timeout_secs")
                .unwrap_or(30)
                .clamp(1, 300),
        );
        let session_id = ctx.session.lock().ok().map(|session| session.id.clone());

        let (target, mut steps, engagement_dir, engagement_name, scope) = {
            let mut state = lock_security(ctx)?;
            let engagement_name = state.engagement_name.clone();
            let scope = state.scope.clone();
            let engagement = state
                .ensure_engagement(&ctx.paths)
                .map_err(|err| security_io(&ctx.paths, err))?;
            if let Some(steps) = steps_arg {
                engagement
                    .update(
                        &id,
                        FindingPatch {
                            poc_steps: Some(steps),
                            ..FindingPatch::default()
                        },
                    )
                    .map_err(|msg| ToolResult::err("FINDING_ARGS", msg))?;
            }
            let finding = engagement
                .get(&id)
                .ok_or_else(|| not_found(&id, &engagement_name))?
                .clone();
            (
                finding.target,
                finding.poc_steps,
                engagement.dir.clone(),
                engagement_name,
                scope,
            )
        };
        if steps.is_empty() {
            return Err(arg_err(format!(
                "finding {engagement_name}/{id} has no poc_steps; pass poc_steps with shell/http actions and at least one expect assertion"
            )));
        }
        validate_steps(&steps).map_err(arg_err)?;

        let base = engagement_dir
            .join("poc")
            .join(format!("{id}-{}", Utc::now().format("%Y%m%dT%H%M%S%3fZ")));
        std::fs::create_dir_all(&base).map_err(|err| {
            ToolResult::err(
                "POC_RUN",
                format!("cannot create PoC run directory {}: {err}", base.display()),
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700));
        }

        let original_len = steps.len();
        let mut outcome = execute_steps(
            &steps,
            &ReplayOptions {
                run_dir: base.join("run"),
                scope: scope.clone(),
                timeout,
                cancel: ctx.cancel.clone(),
                target: target.clone(),
            },
        )
        .await;
        outcome.verification.finding_id = id.clone();

        let mut minimization = None;
        if minimize && outcome.verification.status == VerifyStatus::Reproduced && steps.len() > 1 {
            let (minimized, calls, one_minimal) = minimize_steps(
                &steps,
                &MinimizeOptions {
                    base_dir: base.join("minimize"),
                    scope: scope.clone(),
                    timeout,
                    cancel: ctx.cancel.clone(),
                    target: target.clone(),
                    max_oracle_calls,
                },
            )
            .await;
            if minimized.len() < steps.len() {
                let final_outcome = execute_steps(
                    &minimized,
                    &ReplayOptions {
                        run_dir: base.join("final"),
                        scope: scope.clone(),
                        timeout,
                        cancel: ctx.cancel.clone(),
                        target: target.clone(),
                    },
                )
                .await;
                if final_outcome.verification.status == VerifyStatus::Reproduced {
                    steps = minimized;
                    outcome = final_outcome;
                    outcome.verification.finding_id = id.clone();
                    minimization = Some((original_len, steps.len(), calls, one_minimal));
                } else {
                    minimization = Some((original_len, original_len, calls, one_minimal));
                }
            } else {
                minimization = Some((original_len, original_len, calls, one_minimal));
            }
        }

        let records = std::mem::take(&mut outcome.records);
        let mut verification = outcome.verification;
        let mut evidence_ids = Vec::new();
        let mut evidence_error = None;
        let finding = {
            let mut state = lock_security(ctx)?;
            let engagement = state
                .ensure_engagement(&ctx.paths)
                .map_err(|err| security_io(&ctx.paths, err))?;
            for record in &records {
                let new = NewEvidence {
                    tool: "poc".into(),
                    session: session_id.clone(),
                    entry: None,
                    target: record.target.clone(),
                    command: record.command.clone(),
                    started: record.started,
                    finished: record.finished,
                    duration_ms: record.duration_ms,
                    exit_code: record.exit_code.map(i64::from),
                    timed_out: record.timed_out,
                    cancelled: record.cancelled,
                    is_error: record.is_error,
                    output: record.output.clone(),
                    summary: Some(record.summary.clone()),
                };
                match engagement.evidence.add(new) {
                    Ok(evidence) => evidence_ids.push(evidence.id),
                    Err(err) => {
                        evidence_error = Some(err.to_string());
                        break;
                    }
                }
            }
            verification.evidence = evidence_ids.clone();
            if let Some(error) = &evidence_error {
                verification
                    .summary
                    .push_str(&format!("; evidence write stopped: {error}"));
            }
            engagement
                .record_verification(&id, verification.clone())
                .map_err(|msg| ToolResult::err("FINDING_ARGS", msg))?
        };

        let _ = std::fs::remove_dir_all(&base);

        let passed = verification
            .assertions
            .iter()
            .filter(|assertion| assertion.passed)
            .count();
        let mut content = format!(
            "verify {id}: {} ({}) · {}/{} assertions · {} command(s)\n",
            verification.status.as_str(),
            verification.verdict.as_str(),
            passed,
            verification.assertions.len(),
            verification.commands.len()
        );
        content.push_str(&format!("{}: {}\n", finding.title, finding.target));
        if let Some((before, after, calls, one_minimal)) = minimization {
            content.push_str(&format!(
                "minimize: {before} -> {after} steps, {calls} oracle run(s){}\n",
                if one_minimal {
                    ", 1-minimal"
                } else {
                    ", budget hit"
                }
            ));
        }
        if !evidence_ids.is_empty() {
            content.push_str(&format!("evidence: {}\n", evidence_ids.join(", ")));
        }
        content.push_str(&verification.summary);
        content.push('\n');
        match verification.status {
            VerifyStatus::Reproduced => content.push_str(
                "next: `finding report` includes the verification; after a scoped fix and a passing replay, set status=fixed.",
            ),
            VerifyStatus::NotReproduced => content.push_str(
                "finding kept and status unchanged: a failed replay is not proof the bug is absent. Inspect the assertions and evidence, then adapt the PoC or target.",
            ),
            VerifyStatus::Error | VerifyStatus::Skipped => content.push_str(
                "inconclusive: the verifier did not decide the finding and it is kept. Fix the scope or runner problem and retry.",
            ),
        }

        Ok(ToolResult::ok(content).with_details(json!({
            "finding": to_json(&finding),
            "verification": verification,
            "evidence": evidence_ids,
            "minimize": minimization.map(|(before, after, calls, one_minimal)| json!({
                "before": before,
                "after": after,
                "oracle_calls": calls,
                "one_minimal": one_minimal,
            })),
        })))
    }

    fn list(ctx: &ToolContext) -> Result<ToolResult, ToolResult> {
        let state = lock_security(ctx)?;
        let name = state.engagement_name.clone();
        let Some(engagement) = state.engagement.as_ref() else {
            return Ok(no_findings(&name));
        };
        if engagement.findings.is_empty() {
            return Ok(no_findings(&name));
        }

        let header = ("id", "severity", "status", "title", "target");
        let rows: Vec<[String; 5]> = engagement
            .findings
            .iter()
            .map(|finding| {
                [
                    finding.id.clone(),
                    finding.severity.to_string(),
                    finding.status.to_string(),
                    finding.title.clone(),
                    finding.target.clone(),
                ]
            })
            .collect();
        let mut widths = [0usize; 5];
        for (index, cell) in [header.0, header.1, header.2, header.3, header.4]
            .iter()
            .enumerate()
        {
            widths[index] = cell.chars().count();
        }
        for row in &rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(cell.chars().count());
            }
        }

        let mut content = format!(
            "engagement {name}: {} findings\n\n",
            engagement.findings.len()
        );
        content.push_str(&format_row(
            &[
                header.0.to_string(),
                header.1.to_string(),
                header.2.to_string(),
                header.3.to_string(),
                header.4.to_string(),
            ],
            &widths,
        ));
        content.push('\n');
        for row in &rows {
            content.push_str(&format_row(row, &widths));
            content.push('\n');
        }
        let (open, high, medium) = engagement.counts();
        content.push_str(&format!(
            "\n{open} open ({} high/critical, {medium} medium/low)\nnext: `finding update` changes status; `finding report` renders Markdown.",
            high
        ));

        Ok(ToolResult::ok(content).with_details(json!({
            "counts": {"total": engagement.findings.len(), "open": open, "high_critical": high, "medium_low": medium},
            "findings": engagement.findings.iter().map(to_json).collect::<Vec<_>>(),
        })))
    }

    fn report(ctx: &ToolContext) -> Result<ToolResult, ToolResult> {
        let state = lock_security(ctx)?;
        let name = state.engagement_name.clone();
        let Some(engagement) = state.engagement.as_ref() else {
            return Ok(no_findings(&name));
        };
        if engagement.findings.is_empty() {
            return Ok(no_findings(&name));
        }

        let markdown = render_report(engagement);
        let stamp = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        let path = engagement.dir.join(format!("report-{stamp}.md"));
        write_0600(&path, &markdown).map_err(|err| {
            ToolResult::err(
                "SECURITY_IO",
                format!("cannot write report {}: {err}", path.display()),
            )
        })?;

        let mut content = format!(
            "wrote {} ({} findings)\n\n",
            display_path(&ctx.cwd, &path),
            engagement.findings.len()
        );
        if markdown.chars().count() > REPORT_CHARS {
            content.push_str(&clip(&markdown, REPORT_CHARS));
            content.push_str("\n\n(truncated; full report in the file)");
        } else {
            content.push_str(&markdown);
        }

        Ok(ToolResult::ok(content).with_details(json!({
            "path": path.to_string_lossy(),
            "findings": engagement.findings.len(),
        })))
    }
}

fn format_row(cells: &[String; 5], widths: &[usize; 5]) -> String {
    let mut out = String::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 {
            out.push_str("  ");
        }
        if index == 4 {
            out.push_str(cell);
        } else {
            let pad = widths[index].saturating_sub(cell.chars().count());
            out.push_str(cell);
            out.extend(std::iter::repeat_n(' ', pad));
        }
    }
    out
}

fn write_0600(path: &Path, body: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("md.tmp");
    {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

fn required(args: &Value, key: &str) -> Result<String, ToolResult> {
    match args.get(key).and_then(|value| value.as_str()) {
        Some(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        _ => Err(arg_err(format!(
            "missing or empty \"{key}\"; finding needs {key}"
        ))),
    }
}

fn non_empty(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_confidence(args: &Value) -> Result<Option<f64>, ToolResult> {
    let Some(value) = args.get("confidence") else {
        return Ok(None);
    };
    let Some(value) = value.as_f64() else {
        return Err(arg_err("confidence must be a number between 0 and 1"));
    };
    if !(0.0..=1.0).contains(&value) || !value.is_finite() {
        return Err(arg_err("confidence must be a number between 0 and 1"));
    }
    Ok(Some(value))
}

fn parse_steps(args: &Value) -> Result<Option<Vec<PocStep>>, ToolResult> {
    let Some(value) = args.get("poc_steps") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let steps: Vec<PocStep> = serde_json::from_value(value.clone()).map_err(|err| {
        arg_err(format!(
            "invalid poc_steps: {err}; each step needs id and action, for example {{\"id\":\"probe\",\"action\":{{\"type\":\"shell\",\"cmd\":\"true\"}},\"expect\":{{\"type\":\"exit-zero\"}}}}"
        ))
    })?;
    validate_steps(&steps).map_err(arg_err)?;
    Ok(Some(steps))
}

fn explicit_ref(args: &Value) -> Option<EvidenceRef> {
    let evidence = optional_str(args, "evidence_id");
    let session = optional_str(args, "session");
    let entry = optional_str(args, "entry");
    let artifact = optional_str(args, "artifact");
    if evidence.is_none() && session.is_none() && entry.is_none() && artifact.is_none() {
        return None;
    }
    Some(EvidenceRef {
        evidence,
        session,
        entry,
        artifact,
    })
}

/// Last tool result on the active branch, as `(content, entry id, artifact)`.
fn last_tool_result(ctx: &ToolContext) -> Option<(String, String, Option<String>)> {
    let session = ctx.session.lock().ok()?;
    session
        .branch_entries()
        .into_iter()
        .rev()
        .find_map(|entry| {
            entry.tool_result().map(|result| {
                (
                    result.content.clone(),
                    entry.id.clone(),
                    result.artifact.clone(),
                )
            })
        })
}

fn lock_security(ctx: &ToolContext) -> Result<MutexGuard<'_, SecurityState>, ToolResult> {
    ctx.security.lock().map_err(|_| {
        ToolResult::err(
            "SECURITY_IO",
            "security state is poisoned; restart lh so the findings ledger is reloaded",
        )
    })
}

fn security_io(paths: &crate::paths::Paths, err: anyhow::Error) -> ToolResult {
    ToolResult::err(
        "SECURITY_IO",
        format!(
            "cannot open the findings ledger under {}: {err}",
            paths.engagements_dir().display()
        ),
    )
}

fn arg_err(message: impl Into<String>) -> ToolResult {
    ToolResult::err("FINDING_ARGS", message)
}

fn not_found(id: &str, engagement: &str) -> ToolResult {
    ToolResult::err(
        "FINDING_NOT_FOUND",
        format!("no finding {id:?} in engagement {engagement}; `finding list` shows ids"),
    )
}

fn no_findings(engagement: &str) -> ToolResult {
    ToolResult::ok(format!(
        "engagement {engagement}: no findings recorded; `finding add` records one"
    ))
}

fn to_json(finding: &crate::security::Finding) -> Value {
    serde_json::to_value(finding).unwrap_or(Value::Null)
}

fn poc_action_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {
                    "type": {"const": "shell"},
                    "cmd": {"type": "string", "description": "Run with bash -c in the isolated PoC run directory."},
                    "cwd": {"type": "string", "description": "Optional relative subdirectory."}
                },
                "required": ["type", "cmd"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": {"const": "http"},
                    "method": {"type": "string", "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"]},
                    "url": {"type": "string", "description": "http(s) URL; the host must be in engagement scope."},
                    "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                    "body": {"type": "string"}
                },
                "required": ["type", "method", "url"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": {"const": "note"},
                    "text": {"type": "string"}
                },
                "required": ["type", "text"],
                "additionalProperties": false
            }
        ]
    })
}

fn poc_expect_schema() -> Value {
    json!({
        "oneOf": [
            {
                "type": "object",
                "properties": {"type": {"const": "exit-zero"}},
                "required": ["type"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": {"const": "http-status"},
                    "status": {"oneOf": [{"type": "integer"}, {"type": "array", "items": {"type": "integer"}}]}
                },
                "required": ["type", "status"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": {"const": "body-contains"},
                    "text": {"type": "string"}
                },
                "required": ["type", "text"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": {"const": "body-matches"},
                    "pattern": {"type": "string"}
                },
                "required": ["type", "pattern"],
                "additionalProperties": false
            },
            {
                "type": "object",
                "properties": {
                    "type": {"const": "file-exists"},
                    "path": {"type": "string", "description": "Relative path inside the PoC run directory."}
                },
                "required": ["type", "path"],
                "additionalProperties": false
            }
        ]
    })
}

fn poc_step_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {"type": "string", "description": "1-64 chars of letters, digits, '_' or '-'"},
            "kind": {"type": "string", "enum": ["setup", "auth", "prerequisite", "exploit", "verify"]},
            "summary": {"type": "string"},
            "action": poc_action_schema(),
            "expect": poc_expect_schema()
        },
        "required": ["id", "action"],
        "additionalProperties": false
    })
}

#[async_trait]
impl Tool for FindingTool {
    fn name(&self) -> &'static str {
        "finding"
    }

    fn description(&self) -> &'static str {
        "Engagement findings ledger: add, update, list, report, verify. `verify` replays a finding's poc_steps and records a deterministic verdict."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["add", "update", "list", "report", "verify"]},
                "id": {"type": "string", "description": "finding id from `list`, e.g. f1"},
                "title": {"type": "string", "description": "add: one-line summary of the issue."},
                "severity": {"type": "string", "enum": ["critical", "high", "medium", "low", "info"]},
                "status": {"type": "string", "enum": ["open", "fixed", "accepted", "false-positive"], "description": "update: new status."},
                "target": {"type": "string", "description": "affected asset, host, or URL"},
                "evidence": {"type": "string", "description": "request/response or observation proving the finding"},
                "evidence_id": {"type": "string", "description": "engagement evidence id, e.g. e3"},
                "impact": {"type": "string", "description": "What an attacker gains."},
                "remediation": {"type": "string", "description": "How to fix it."},
                "session": {"type": "string", "description": "session id for the evidence ref"},
                "entry": {"type": "string", "description": "session entry id for the evidence ref"},
                "artifact": {"type": "string", "description": "artifact id holding the evidence"},
                "confidence": {"type": "number", "minimum": 0, "maximum": 1, "description": "Model confidence; raised to 0.6 when poc_steps exist and 0.8 when a verifiable assertion exists."},
                "poc_steps": {
                    "type": "array",
                    "maxItems": 50,
                    "description": "Deterministic reproduction steps. Add at least one expect assertion.",
                    "items": poc_step_schema()
                },
                "minimize": {"type": "boolean", "description": "verify: reduce a reproducing step graph to a 1-minimal subset."},
                "max_oracle_calls": {"type": "integer", "minimum": 1, "maximum": 50, "description": "verify: minimization replay budget (default 20)."},
                "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 300, "description": "verify: per-step timeout (default 30)."},
                "runner": {"type": "string", "enum": ["local"], "description": "verify: only the local runner is implemented."}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Security
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let action = match args.get("action").and_then(|value| value.as_str()) {
            Some(action) => action,
            None => {
                return arg_err(
                    "missing \"action\"; pass {\"action\":\"add\"|\"update\"|\"list\"|\"report\"|\"verify\"}",
                );
            }
        };
        let outcome = match action {
            "add" => Self::add(ctx, &args),
            "update" => Self::update(ctx, &args),
            "list" => Self::list(ctx),
            "report" => Self::report(ctx),
            "verify" => Self::verify(ctx, &args).await,
            other => Err(arg_err(format!(
                "unknown action {other:?}; use add, update, list, report, or verify"
            ))),
        };
        match outcome {
            Ok(result) => result,
            Err(error) => error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path as FsPath;
    use std::sync::{Arc, Mutex};

    use crate::config::VerifyConfig;
    use crate::paths::Paths;
    use crate::session::{EntryKind, Session, SessionData, ToolResultData};
    use crate::tools::{BgRegistry, ReadTracker};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn context(dir: &FsPath) -> ToolContext {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths.config_dir = dir.join("config");
        paths.ensure_dirs().unwrap();
        let header = SessionData {
            cwd: paths.cwd.display().to_string(),
            endpoint: "test".into(),
            model: "test".into(),
            profile: "test".into(),
            app: "test".into(),
            context: None,
        };
        let session = Session::create(&paths, header, true).unwrap();
        let artifacts = session.artifacts.clone();
        let security = SecurityState::load(&paths, Some("eng".into())).unwrap();
        ToolContext {
            cwd: paths.cwd.clone(),
            path_jail: false,
            session: Arc::new(Mutex::new(session)),
            artifacts,
            cancel: CancellationToken::new(),
            trust: false,
            verify: VerifyConfig::default(),
            profile: "test".into(),
            packs: Vec::new(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::new())),
            security: Arc::new(Mutex::new(security)),
            paths,
            lsp: Arc::new(crate::tools::lsp::LspService::new()),
            sandbox: None,
        }
    }

    #[tokio::test]
    async fn add_list_update_report() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FindingTool;

        let added = tool
            .call(
                &ctx,
                json!({
                    "action": "add",
                    "title": "Reflected XSS",
                    "severity": "high",
                    "target": "https://app.test/search",
                    "evidence": "<script>alert(1)</script>",
                    "impact": "session theft",
                    "remediation": "encode output"
                }),
            )
            .await;
        assert!(!added.is_error, "{}", added.content);
        assert!(added.content.contains("added f1"), "{}", added.content);

        let listed = tool.call(&ctx, json!({"action": "list"})).await;
        assert!(!listed.is_error, "{}", listed.content);
        assert!(listed.content.contains("Reflected XSS"));
        assert!(listed.content.contains("1 open"));

        let updated = tool
            .call(
                &ctx,
                json!({"action": "update", "id": "f1", "status": "fixed"}),
            )
            .await;
        assert!(!updated.is_error, "{}", updated.content);
        assert!(updated.content.contains("status"));

        let missing = tool
            .call(
                &ctx,
                json!({"action": "update", "id": "f99", "status": "fixed"}),
            )
            .await;
        assert!(missing.is_error);
        assert!(missing.content.contains("FINDING_NOT_FOUND"));

        let report = tool.call(&ctx, json!({"action": "report"})).await;
        assert!(!report.is_error, "{}", report.content);
        let path = report
            .details
            .as_ref()
            .and_then(|details| details.get("path"))
            .and_then(|value| value.as_str())
            .expect("report path");
        let written = std::fs::read_to_string(path).unwrap();
        assert!(written.contains("# Findings — eng"));
        assert!(written.contains("Reflected XSS"));
        let mode = std::os::unix::fs::PermissionsExt::mode(
            &std::fs::metadata(path).unwrap().permissions(),
        );
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn add_auto_attaches_last_tool_result() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        {
            let mut session = ctx.session.lock().unwrap();
            session
                .append(EntryKind::ToolResult(ToolResultData {
                    call_id: "call-1".into(),
                    content: "HTTP/1.1 200 OK\n<script>alert(1)</script>".into(),
                    is_error: false,
                    details: None,
                    artifact: Some("art-7".into()),
                }))
                .unwrap();
        }

        let added = FindingTool
            .call(
                &ctx,
                json!({
                    "action": "add",
                    "title": "Stored XSS",
                    "severity": "critical",
                    "target": "https://app.test/profile",
                }),
            )
            .await;
        assert!(!added.is_error, "{}", added.content);
        assert!(
            added.content.contains("attached from entry e1"),
            "{}",
            added.content
        );

        let state = ctx.security.lock().unwrap();
        let finding = &state.engagement.as_ref().unwrap().findings[0];
        assert!(finding.evidence.as_deref().unwrap().contains("alert(1)"));
        let reference = finding.evidence_ref.as_ref().unwrap();
        assert_eq!(reference.session.as_deref(), Some("ephemeral"));
        assert_eq!(reference.entry.as_deref(), Some("e1"));
        assert_eq!(reference.artifact.as_deref(), Some("art-7"));
    }

    #[tokio::test]
    async fn add_links_and_validates_an_explicit_evidence_id() {
        use crate::security::evidence::NewEvidence;
        use chrono::Utc;

        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        {
            let mut state = ctx.security.lock().unwrap();
            let engagement = state.ensure_engagement(&ctx.paths).unwrap();
            let now = Utc::now();
            engagement
                .evidence
                .add(NewEvidence {
                    tool: "bash".into(),
                    session: Some("ephemeral".into()),
                    entry: Some("t0".into()),
                    target: "app.test".into(),
                    command: "curl https://app.test".into(),
                    started: now,
                    finished: now,
                    duration_ms: 1,
                    exit_code: Some(0),
                    timed_out: false,
                    cancelled: false,
                    is_error: false,
                    output: "HTTP/1.1 500 Internal Server Error\n".into(),
                    summary: None,
                })
                .unwrap();
        }

        let added = FindingTool
            .call(
                &ctx,
                json!({
                    "action": "add",
                    "title": "Server error",
                    "severity": "medium",
                    "target": "https://app.test",
                    "evidence_id": "e1"
                }),
            )
            .await;
        assert!(!added.is_error, "{}", added.content);
        {
            let state = ctx.security.lock().unwrap();
            let finding = &state.engagement.as_ref().unwrap().findings[0];
            assert!(
                finding
                    .evidence
                    .as_deref()
                    .is_some_and(|evidence| evidence.contains("HTTP/1.1 500")),
                "{:?}",
                finding.evidence
            );
            assert_eq!(
                finding
                    .evidence_ref
                    .as_ref()
                    .and_then(|reference| reference.evidence.as_deref()),
                Some("e1")
            );
        }

        let bad = FindingTool
            .call(
                &ctx,
                json!({
                    "action": "add",
                    "title": "Bad reference",
                    "severity": "low",
                    "target": "app.test",
                    "evidence": "x",
                    "evidence_id": "e99"
                }),
            )
            .await;
        assert!(bad.is_error);
        assert!(
            bad.content.contains("EVIDENCE_NOT_FOUND"),
            "{}",
            bad.content
        );
    }

    #[tokio::test]
    async fn add_without_evidence_or_tool_result_is_prescriptive() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let result = FindingTool
            .call(
                &ctx,
                json!({"action": "add", "title": "t", "severity": "low", "target": "h"}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("FINDING_ARGS"));
        assert!(result.content.contains("evidence"));

        let bad_severity = FindingTool
            .call(
                &ctx,
                json!({"action": "add", "title": "t", "severity": "urgent", "target": "h", "evidence": "e"}),
            )
            .await;
        assert!(bad_severity.is_error);
        assert!(bad_severity.content.contains("unknown severity"));
    }

    #[tokio::test]
    async fn list_and_report_without_findings_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let listed = FindingTool.call(&ctx, json!({"action": "list"})).await;
        assert!(!listed.is_error);
        assert!(listed.content.contains("no findings recorded"));
        let reported = FindingTool.call(&ctx, json!({"action": "report"})).await;
        assert!(!reported.is_error);
        assert!(reported.content.contains("no findings recorded"));
    }

    #[tokio::test]
    async fn verify_replays_shell_steps_and_records_confidence() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FindingTool;
        let added = tool
            .call(
                &ctx,
                json!({
                    "action": "add",
                    "title": "marker injection",
                    "severity": "high",
                    "target": "local fixture",
                    "evidence": "the probe prints the canary",
                    "poc_steps": [{
                        "id": "probe",
                        "kind": "verify",
                        "action": {"type": "shell", "cmd": "printf 'ZERO-INJ-OK'"},
                        "expect": {"type": "body-contains", "text": "ZERO-INJ-OK"}
                    }]
                }),
            )
            .await;
        assert!(!added.is_error, "{}", added.content);

        let verified = tool
            .call(&ctx, json!({"action": "verify", "id": "f1"}))
            .await;
        assert!(!verified.is_error, "{}", verified.content);
        assert!(
            verified.content.contains("reproduced"),
            "{}",
            verified.content
        );
        let details = verified.details.as_ref().expect("details");
        assert_eq!(details["verification"]["status"], "reproduced");
        assert_eq!(details["verification"]["verdict"], "confirmed");
        assert_eq!(details["evidence"].as_array().unwrap().len(), 1);

        let state = ctx.security.lock().unwrap();
        let finding = &state.engagement.as_ref().unwrap().findings[0];
        assert_eq!(
            finding.verification.as_ref().unwrap().status,
            VerifyStatus::Reproduced
        );
        assert_eq!(finding.verification.as_ref().unwrap().evidence, vec!["e1"]);
        assert!(finding.confidence.unwrap() >= 0.8);
        assert_eq!(finding.status, Status::Open);
    }

    #[tokio::test]
    async fn verify_without_steps_is_prescriptive() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FindingTool;
        tool.call(
            &ctx,
            json!({
                "action": "add",
                "title": "prose only",
                "severity": "low",
                "target": "local",
                "evidence": "an observation"
            }),
        )
        .await;
        let verified = tool
            .call(&ctx, json!({"action": "verify", "id": "f1"}))
            .await;
        assert!(verified.is_error);
        assert!(
            verified.content.contains("no poc_steps"),
            "{}",
            verified.content
        );
    }

    #[tokio::test]
    async fn failed_replay_rejects_without_closing_the_finding() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FindingTool;
        tool.call(
            &ctx,
            json!({
                "action": "add",
                "title": "flaky proof",
                "severity": "medium",
                "target": "local",
                "evidence": "exit 1",
                "poc_steps": [{
                    "id": "probe",
                    "action": {"type": "shell", "cmd": "exit 1"},
                    "expect": {"type": "exit-zero"}
                }]
            }),
        )
        .await;
        let verified = tool
            .call(&ctx, json!({"action": "verify", "id": "f1"}))
            .await;
        assert!(!verified.is_error, "{}", verified.content);
        assert!(
            verified.content.contains("not_reproduced"),
            "{}",
            verified.content
        );
        let state = ctx.security.lock().unwrap();
        let finding = &state.engagement.as_ref().unwrap().findings[0];
        assert_eq!(finding.status, Status::Open);
        assert_eq!(
            finding.verification.as_ref().unwrap().verdict,
            crate::security::verify::VerifyVerdict::Rejected
        );
    }

    #[tokio::test]
    async fn http_replay_rejects_an_unscoped_target() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FindingTool;
        tool.call(
            &ctx,
            json!({
                "action": "add",
                "title": "unscoped http",
                "severity": "low",
                "target": "http://127.0.0.1:1/",
                "evidence": "request",
                "poc_steps": [{
                    "id": "http",
                    "action": {"type": "http", "method": "GET", "url": "http://127.0.0.1:1/"},
                    "expect": {"type": "http-status", "status": 200}
                }]
            }),
        )
        .await;
        let verified = tool
            .call(&ctx, json!({"action": "verify", "id": "f1"}))
            .await;
        assert!(!verified.is_error, "{}", verified.content);
        assert!(
            verified.content.contains("inconclusive"),
            "{}",
            verified.content
        );
        assert!(
            verified.content.contains("not in the engagement scope"),
            "{}",
            verified.content
        );
    }

    #[tokio::test]
    async fn verify_minimizes_a_reproducing_step_graph() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(dir.path());
        let tool = FindingTool;
        tool.call(
            &ctx,
            json!({
                "action": "add",
                "title": "minimize me",
                "severity": "medium",
                "target": "local",
                "evidence": "three steps",
                "poc_steps": [
                    {"id": "setup", "action": {"type": "shell", "cmd": "printf setup"}, "expect": {"type": "exit-zero"}},
                    {"id": "probe", "action": {"type": "shell", "cmd": "printf 'ZERO-INJ-OK'"}, "expect": {"type": "body-contains", "text": "ZERO-INJ-OK"}},
                    {"id": "noise", "action": {"type": "shell", "cmd": "true"}, "expect": {"type": "exit-zero"}}
                ]
            }),
        )
        .await;
        let verified = tool
            .call(
                &ctx,
                json!({"action": "verify", "id": "f1", "minimize": true, "max_oracle_calls": 20}),
            )
            .await;
        assert!(!verified.is_error, "{}", verified.content);
        assert!(
            verified.content.contains("minimize:"),
            "{}",
            verified.content
        );
        let details = verified.details.as_ref().expect("details");
        assert!(
            details["minimize"]["after"].as_u64().unwrap()
                < details["minimize"]["before"].as_u64().unwrap()
        );
    }
}

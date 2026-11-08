//! `recall` tool: read a capped range of an archived tool result.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::session::ArtifactError;
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult, optional_u64, required_str};

/// Lines read per call, regardless of the requested range.
const MAX_LINES: usize = 200;
/// Characters returned per call, including the header.
const CHAR_CAP: usize = 4000;

pub struct RecallTool;

impl RecallTool {}

fn clean_handle(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim()
        .to_string()
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &'static str {
        "recall"
    }

    fn description(&self) -> &'static str {
        "Read an archived tool-output range."
    }

    fn schema(&self) -> Value {
        serde_json::from_str(r#"{"type":"object","properties":{"artifact":{"type":"string"},"start":{"type":"integer"},"end":{"type":"integer"}},"required":["artifact"],"additionalProperties":false}"#).expect("valid schema")
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let handle = match required_str(&args, "artifact") {
            Ok(handle) => clean_handle(&handle),
            Err(err) => return err,
        };
        let start = match optional_u64(&args, "start") {
            Some(0) => {
                return ToolResult::err(
                    "invalid_args",
                    "start is 1-based; pass start >= 1, for example {\"artifact\":\"out-3\",\"start\":1,\"end\":120}",
                );
            }
            Some(value) => value as usize,
            None => 1,
        };
        let end = match optional_u64(&args, "end") {
            Some(value) => value as usize,
            None => start.saturating_add(119),
        };
        if end < start {
            return ToolResult::err(
                "invalid_args",
                format!(
                    "end {end} is before start {start}; pass an increasing range, for example {{\"artifact\":\"{handle}\",\"start\":{start},\"end\":{}}}",
                    start.saturating_add(40)
                ),
            );
        }

        let header_width_guess = format!("artifact {handle} (999999 lines, showing {start}-{end})");
        let max_chars = CHAR_CAP
            .saturating_sub(header_width_guess.len() + 2)
            .max(200);
        let slice = match ctx
            .artifacts
            .read_lines(&handle, start, end, MAX_LINES, max_chars)
        {
            Ok(slice) => slice,
            Err(ArtifactError::NotFound(_)) => {
                return ToolResult::err(
                    "ARTIFACT_NOT_FOUND",
                    format!(
                        "artifact {handle:?} was not found. Artifact handles appear as [out-N] in truncated tool results; pass one as {{\"artifact\":\"out-3\"}} and page with start/end. Session artifacts are kept for the session only."
                    ),
                );
            }
            Err(ArtifactError::InvalidId(_)) => {
                return ToolResult::err(
                    "ARTIFACT_NOT_FOUND",
                    format!(
                        "artifact {handle:?} is not a valid handle; handles look like out-3. Copy the handle from the [out-N] marker in a truncated result."
                    ),
                );
            }
            Err(ArtifactError::Io { id, source }) => {
                return ToolResult::err(
                    "ARTIFACT_IO",
                    format!(
                        "cannot read artifact {id}: {source}. The artifact file may have been removed; rerun the tool that produced it."
                    ),
                );
            }
        };

        if slice.total_lines == 0 {
            return ToolResult::ok(format!("artifact {handle} (0 lines)\n(artifact is empty)"))
                .with_details(json!({
                    "artifact": handle,
                    "start": start,
                    "end": end,
                    "total_lines": 0,
                }));
        }
        if start > slice.total_lines {
            return ToolResult::err(
                "ARTIFACT_RANGE",
                format!(
                    "artifact {handle} has {} lines; start {start} is past the end. Pass start between 1 and {}.",
                    slice.total_lines, slice.total_lines
                ),
            );
        }

        let mut content = format!(
            "artifact {handle} ({} lines, showing {}-{})",
            slice.total_lines, slice.start_line, slice.end_line
        );
        if !slice.content.is_empty() {
            content.push('\n');
            content.push_str(&slice.content);
        }
        if slice.truncated {
            if content.ends_with('\n') {
                content.pop();
            }
            if slice.end_line < slice.total_lines {
                content.push_str(&format!(
                    "\n(truncated; continue with start={})",
                    slice.end_line + 1
                ));
            } else {
                content.push_str("\n(truncated at the character cap; narrow start/end)");
            }
        }
        let content = truncate_chars(&content, CHAR_CAP);
        ToolResult::ok(content).with_details(json!({
            "artifact": handle,
            "start": slice.start_line,
            "end": slice.end_line,
            "total_lines": slice.total_lines,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VerifyConfig;
    use crate::paths::Paths;
    use crate::security::SecurityState;
    use crate::session::{ArtifactStore, Session, SessionData};
    use crate::tools::{BgRegistry, ReadTracker};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    fn test_ctx() -> (tempfile::TempDir, ToolContext) {
        let dir = tempfile::tempdir().expect("tempdir");
        let cwd = dir.path().to_path_buf();
        let paths = Paths {
            config_dir: cwd.join(".config"),
            data_dir: cwd.join(".data"),
            cwd: cwd.clone(),
            project_id: "test".into(),
        };
        let header = SessionData {
            cwd: cwd.display().to_string(),
            endpoint: "test".into(),
            model: "test".into(),
            profile: "test".into(),
            app: "test".into(),
        };
        let session = Session::create(&paths, header, true).expect("session");
        let artifacts = Arc::new(ArtifactStore::ephemeral());
        artifacts
            .write_named("out-3", "alpha\nbravo\ncharlie\ndelta\n")
            .expect("artifact");
        let ctx = ToolContext {
            cwd,
            path_jail: true,
            session: Arc::new(Mutex::new(session)),
            artifacts,
            cancel: CancellationToken::new(),
            trust: true,
            verify: VerifyConfig::default(),
            profile: "test".into(),
            reads: Arc::new(Mutex::new(ReadTracker::default())),
            bg: Arc::new(Mutex::new(BgRegistry::new())),
            security: Arc::new(Mutex::new(SecurityState::default())),
            paths,
        };
        (dir, ctx)
    }

    #[tokio::test]
    async fn formats_an_artifact_range() {
        let (_dir, ctx) = test_ctx();
        let result = RecallTool
            .call(&ctx, json!({"artifact":"out-3","start":2,"end":3}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .contains("artifact out-3 (4 lines, showing 2-3)"),
            "{}",
            result.content
        );
        assert!(result.content.contains("2  bravo"), "{}", result.content);
        assert!(result.content.contains("3  charlie"), "{}", result.content);
        assert!(!result.content.contains("alpha"), "{}", result.content);
    }

    #[tokio::test]
    async fn default_range_starts_at_line_one() {
        let (_dir, ctx) = test_ctx();
        let result = RecallTool.call(&ctx, json!({"artifact":"[out-3]"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result
                .content
                .contains("artifact out-3 (4 lines, showing 1-4)"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn missing_artifact_names_the_handle() {
        let (_dir, ctx) = test_ctx();
        let result = RecallTool.call(&ctx, json!({"artifact":"out-9"})).await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[ARTIFACT_NOT_FOUND]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("out-9"), "{}", result.content);
    }

    #[tokio::test]
    async fn start_past_the_end_is_prescriptive() {
        let (_dir, ctx) = test_ctx();
        let result = RecallTool
            .call(&ctx, json!({"artifact":"out-3","start":40,"end":50}))
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[ARTIFACT_RANGE]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("4 lines"), "{}", result.content);
    }
}

//! `patch` tool.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::edit::{diff_report, locate, nearest_window, read_text, require_fresh};
use crate::tools::write::{atomic_write, file_mode, record_own_write};
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, optional_u64, required_str,
};

const MAX_DIFF_LINES: usize = 200;

pub struct PatchTool;

impl PatchTool {}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &'static str {
        "patch"
    }

    fn description(&self) -> &'static str {
        "Anchor-based edit: replace the Nth exact anchor with recovery diagnostics."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "path": {"type": "string", "description": "File to patch, relative to the working directory."},
                "anchor": {"type": "string", "description": "Exact text to find; must appear verbatim."},
                "replacement": {"type": "string", "description": "Text that replaces the anchor; empty deletes it."},
                "occurrence": {"type": "integer", "description": "Which match to replace, 1-based (default 1)."}
            }),
            &["path", "anchor", "replacement"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        let raw = args
            .get("path")
            .and_then(|value| value.as_str())
            .map(str::to_string);
        let result = match run(ctx, &args) {
            Ok(result) => result,
            Err(err) => return err,
        };
        match raw.as_deref().map(|raw| ctx.resolve(raw)) {
            Some(Ok(path)) => result.with_diagnostics(ctx, &path).await,
            _ => result,
        }
    }
}

fn run(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
    let raw = required_str(args, "path")?;
    let anchor = required_str(args, "anchor")?;
    let replacement = args
        .get("replacement")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            ToolResult::err(
                "invalid_args",
                "missing required argument \"replacement\"; pass \"\" to delete the anchor",
            )
        })?;
    let occurrence = optional_u64(args, "occurrence").unwrap_or(1);
    if occurrence == 0 {
        return Err(ToolResult::err(
            "invalid_args",
            "occurrence is 1-based; pass \"occurrence\": 1 for the first match",
        ));
    }
    let occurrence = usize::try_from(occurrence).unwrap_or(usize::MAX);
    if anchor == replacement {
        return Err(ToolResult::err(
            "NO_CHANGE",
            "`replacement` is identical to `anchor`; nothing to change",
        ));
    }

    let path = ctx.resolve(&raw)?;
    let display = display_path(&ctx.cwd, &path);
    let before = read_text(&path, &display)?;
    require_fresh(ctx, &path, &display)?;

    let Some(located) = locate(&before, &anchor, replacement) else {
        return Err(ToolResult::err(
            "NO_MATCH",
            nearest_window(&display, &before, &anchor),
        ));
    };
    let (anchor, replacement) = (located.needle, located.replacement.as_str());
    let total = before.match_indices(&anchor).count();
    if occurrence > total {
        return Err(ToolResult::err(
            "OCCURRENCE_RANGE",
            format!(
                "anchor matches {total} time(s) in {display}; occurrence={occurrence} is out of range. Pass occurrence between 1 and {total}."
            ),
        ));
    }

    let (index, _) = before
        .match_indices(&anchor)
        .nth(occurrence - 1)
        .expect("occurrence was range-checked");
    let after = format!(
        "{}{}{}",
        &before[..index],
        replacement,
        &before[index + anchor.len()..]
    );

    let (diff, added, removed) = diff_report(&before, &after, MAX_DIFF_LINES);
    let change = crate::tools::undo::record_change(ctx, Ok(Some(&before)), &after);
    atomic_write(&path, after.as_bytes(), file_mode(&path))
        .map_err(|err| ToolResult::err("PATCH_FAILED", format!("cannot write {display}: {err}")))?;
    record_own_write(ctx, &path);

    let how = located
        .how
        .map(|how| format!("; {how}"))
        .unwrap_or_default();
    let text = format!(
        "patched {display} (occurrence {occurrence} of {total}, +{added} -{removed}{how})\n{diff}"
    );
    Ok(
        ToolResult::ok(text).with_details(crate::tools::undo::merge_details(
            json!({
                "path": display,
                "added": added,
                "removed": removed,
                "replacements": 1,
            }),
            change,
        )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::ReadTool;
    use crate::tools::write::test_context;
    use serde_json::json;

    async fn prepared(dir: &tempfile::TempDir, body: &str) -> ToolContext {
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let ctx = test_context(dir.path());
        let read = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        assert!(!read.is_error, "{}", read.content);
        ctx
    }

    #[tokio::test]
    async fn replaces_requested_occurrence() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "a\na\na\n").await;

        let result = PatchTool
            .call(
                &ctx,
                json!({"path": "a.txt", "anchor": "a", "replacement": "b", "occurrence": 2}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "a\nb\na\n"
        );
        let details = result.details.unwrap();
        assert_eq!(details["replacements"].as_u64(), Some(1));
        assert_eq!(details["added"].as_u64(), Some(1));
        assert_eq!(details["removed"].as_u64(), Some(1));
    }

    #[tokio::test]
    async fn occurrence_out_of_range_is_prescriptive() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "only one\n").await;

        let result = PatchTool
            .call(
                &ctx,
                json!({"path": "a.txt", "anchor": "only one", "replacement": "two", "occurrence": 3}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[OCCURRENCE_RANGE]"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("between 1 and 1"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn missing_anchor_shows_nearest_window() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "alpha\nbeta\ngamma\n").await;

        let result = PatchTool
            .call(
                &ctx,
                json!({"path": "a.txt", "anchor": "betas", "replacement": "delta"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[NO_MATCH]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("beta"), "{}", result.content);
    }

    #[tokio::test]
    async fn requires_prior_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let ctx = test_context(dir.path());

        let result = PatchTool
            .call(
                &ctx,
                json!({"path": "a.txt", "anchor": "one", "replacement": "two"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[READ_FIRST]"),
            "{}",
            result.content
        );
    }
}

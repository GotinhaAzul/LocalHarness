//! `undo` tool: revert the harness's own file changes, one step at a time.
//!
//! Before `edit`, `patch`, or `write` change a file, the previous text is saved
//! to the session artifact store and the change's `fs-change` session entry
//! records the snapshot id plus a SHA-256 of what the harness wrote. Undo walks
//! those entries on the active branch, so it follows `/tree` and survives
//! resume. No git involved: the user's own uncommitted work, staging, and
//! stashes are never touched.
//!
//! Undo refuses when the file no longer matches what the harness wrote, so it
//! can never discard a change made by the user, a formatter, or `bash`.

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::session::{CustomData, EntryKind};
use crate::tools::edit::diff_report;
use crate::tools::write::{atomic_write, file_mode, record_own_write};
use crate::tools::{Tool, ToolContext, ToolPack, ToolResult, display_path, required_str};

/// Previous contents larger than this are not snapshotted.
const MAX_SNAPSHOT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_DIFF_LINES: usize = 200;

/// `fs-change` payload keys that tools report in their result details.
const CHANGE_KEYS: [&str; 9] = [
    "added",
    "removed",
    "created",
    "snapshot",
    "snapshot_error",
    "after_sha256",
    "undo",
    "reverts",
    "deleted",
];

pub(crate) fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Read a file's current text for a snapshot. `Ok(None)` means it does not
/// exist; `Err` names why it cannot be snapshotted.
pub(crate) fn previous_text(path: &std::path::Path) -> Result<Option<String>, String> {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("cannot stat the previous file: {err}")),
    };
    if meta.len() > MAX_SNAPSHOT_BYTES {
        return Err("the previous file was larger than 4 MB".into());
    }
    let bytes =
        std::fs::read(path).map_err(|err| format!("cannot read the previous file: {err}"))?;
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| "the previous file was not UTF-8 text".into())
}

/// Save what a file held before a change and describe the change for undo.
/// Never fails the mutation: a snapshot that cannot be taken is recorded as
/// `snapshot_error`, and undo reports that reason later.
pub(crate) fn record_change(
    ctx: &ToolContext,
    before: Result<Option<&str>, String>,
    after: &str,
) -> Map<String, Value> {
    let mut out = Map::new();
    match before {
        Ok(Some(text)) => match ctx.artifacts.write(text) {
            Ok(artifact) => {
                out.insert("snapshot".into(), json!(artifact.id));
            }
            Err(err) => {
                out.insert(
                    "snapshot_error".into(),
                    json!(format!("snapshot could not be saved: {err}")),
                );
            }
        },
        Ok(None) => {
            out.insert("created".into(), json!(true));
        }
        Err(reason) => {
            out.insert("snapshot_error".into(), json!(reason));
        }
    }
    out.insert("after_sha256".into(), json!(content_hash(after.as_bytes())));
    out
}

/// Merge `record_change` output into a tool's result details.
pub(crate) fn merge_details(mut details: Value, change: Map<String, Value>) -> Value {
    if let Some(object) = details.as_object_mut() {
        object.extend(change);
    }
    details
}

/// The `fs-change` session entry for a successful mutation. `display` is the
/// fallback path; the tool's normalized `path` detail wins so `./a.rs` and
/// `a.rs` land on the same undo stack.
pub fn fs_change_entry(display: String, details: Option<&Value>) -> CustomData {
    let path = details
        .and_then(|details| details.get("path"))
        .and_then(Value::as_str)
        .map_or(display, str::to_string);
    let mut custom = CustomData::new("fs-change").with("path", path);
    if let Some(details) = details {
        for key in CHANGE_KEYS {
            if let Some(value) = details.get(key) {
                custom.payload.insert(key.to_string(), value.clone());
            }
        }
    }
    custom
}

/// One harness change still on the undo stack.
struct Change {
    seq: u64,
    payload: Map<String, Value>,
}

/// Harness changes to `display` on the active branch, oldest first, minus
/// those already reverted.
fn change_stack(ctx: &ToolContext, display: &str) -> Result<Vec<Change>, ToolResult> {
    let session = ctx.session.lock().map_err(|_| {
        ToolResult::err(
            "session_lock",
            "session lock is poisoned; restart the harness",
        )
    })?;
    let mut stack: Vec<Change> = Vec::new();
    for entry in session.branch_entries() {
        let EntryKind::Custom(custom) = &entry.kind else {
            continue;
        };
        if custom.kind != "fs-change"
            || custom.payload.get("path").and_then(Value::as_str) != Some(display)
        {
            continue;
        }
        if custom.payload.get("undo").and_then(Value::as_bool) == Some(true) {
            if let Some(reverted) = custom.payload.get("reverts").and_then(Value::as_u64) {
                stack.retain(|change| change.seq != reverted);
            }
            continue;
        }
        stack.push(Change {
            seq: entry.seq,
            payload: custom.payload.clone(),
        });
    }
    Ok(stack)
}

pub struct UndoTool;

#[async_trait]
impl Tool for UndoTool {
    fn name(&self) -> &'static str {
        "undo"
    }

    fn description(&self) -> &'static str {
        "Revert the last edit, patch, or write to a file; call again to step further back. Changes made by bash are not tracked."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "path": {"type": "string", "description": "File whose last harness change to revert."}
            }),
            &["path"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    async fn call(&self, ctx: &ToolContext, args: Value) -> ToolResult {
        run(ctx, &args).unwrap_or_else(|err| err)
    }
}

fn run(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
    let raw = required_str(args, "path")?;
    let path = ctx.resolve(&raw)?;
    let display = display_path(&ctx.cwd, &path);
    let stack = change_stack(ctx, &display)?;
    let Some(change) = stack.last() else {
        return Err(ToolResult::err(
            "NOTHING_TO_UNDO",
            format!(
                "no edit, patch, or write to {display} on this branch to undo. Changes made with bash are not tracked; fix those with edit."
            ),
        ));
    };
    let Some(expected) = change.payload.get("after_sha256").and_then(Value::as_str) else {
        return Err(ToolResult::err(
            "NO_SNAPSHOT",
            format!(
                "the last change to {display} was recorded without an undo snapshot (an older session). Read the file and fix it with edit."
            ),
        ));
    };

    // Only revert what the harness itself wrote.
    let current = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ToolResult::err(
                "UNDO_CONFLICT",
                format!(
                    "{display} no longer exists; something deleted it after the harness wrote it. Recreate it with write if needed."
                ),
            ));
        }
        Err(err) => {
            return Err(ToolResult::err(
                "UNDO_FAILED",
                format!("cannot read {display}: {err}"),
            ));
        }
    };
    if content_hash(&current) != expected {
        return Err(ToolResult::err(
            "UNDO_CONFLICT",
            format!(
                "{display} changed after the harness wrote it (by the user, a formatter, or a bash command); undo would discard that change. Read it and fix it with edit instead."
            ),
        ));
    }
    let current = String::from_utf8_lossy(&current).into_owned();
    let remaining = stack.len() - 1;
    let more = match remaining {
        0 => "no earlier changes to undo".to_string(),
        1 => "1 earlier change can still be undone".to_string(),
        n => format!("{n} earlier changes can still be undone"),
    };

    if change.payload.get("created").and_then(Value::as_bool) == Some(true) {
        std::fs::remove_file(&path).map_err(|err| {
            ToolResult::err("UNDO_FAILED", format!("cannot delete {display}: {err}"))
        })?;
        let (diff, added, removed) = diff_report(&current, "", MAX_DIFF_LINES);
        let text = format!(
            "deleted {display}, which the reverted write had created (-{removed}); {more}\n{diff}"
        );
        return Ok(ToolResult::ok(text).with_details(json!({
            "path": display,
            "undo": true,
            "reverts": change.seq,
            "deleted": true,
            "added": added,
            "removed": removed,
        })));
    }

    if let Some(reason) = change.payload.get("snapshot_error").and_then(Value::as_str) {
        return Err(ToolResult::err(
            "NO_SNAPSHOT",
            format!("cannot undo the last change to {display}: {reason}. Fix it with edit."),
        ));
    }
    let Some(snapshot) = change.payload.get("snapshot").and_then(Value::as_str) else {
        return Err(ToolResult::err(
            "NO_SNAPSHOT",
            format!("the last change to {display} has no snapshot. Fix it with edit."),
        ));
    };
    let restored = ctx.artifacts.read_all(snapshot).map_err(|err| {
        ToolResult::err(
            "SNAPSHOT_MISSING",
            format!(
                "the snapshot for {display} is gone ({err}); old session artifacts are removed by retention. Fix it with edit."
            ),
        )
    })?;
    atomic_write(&path, restored.as_bytes(), file_mode(&path))
        .map_err(|err| ToolResult::err("UNDO_FAILED", format!("cannot write {display}: {err}")))?;
    record_own_write(ctx, &path);
    let (diff, added, removed) = diff_report(&current, &restored, MAX_DIFF_LINES);
    let text = format!(
        "restored {display} to its state before the last change (+{added} -{removed}); {more}\n{diff}"
    );
    Ok(ToolResult::ok(text).with_details(json!({
        "path": display,
        "undo": true,
        "reverts": change.seq,
        "added": added,
        "removed": removed,
        "after_sha256": content_hash(restored.as_bytes()),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::edit::EditTool;
    use crate::tools::read::ReadTool;
    use crate::tools::write::{WriteTool, test_context};

    /// Run a mutating tool and record its fs-change entry the way the agent
    /// loop does.
    async fn mutate(ctx: &ToolContext, tool: &dyn Tool, args: Value) -> ToolResult {
        let result = tool.call(ctx, args.clone()).await;
        if !result.is_error {
            let display = args["path"].as_str().unwrap().to_string();
            let entry = fs_change_entry(display, result.details.as_ref());
            ctx.session
                .lock()
                .unwrap()
                .append(EntryKind::Custom(entry))
                .unwrap();
        }
        result
    }

    fn on_disk(dir: &tempfile::TempDir, name: &str) -> Option<String> {
        std::fs::read_to_string(dir.path().join(name)).ok()
    }

    #[tokio::test]
    async fn undo_steps_back_through_edits_one_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "let x = 1;\n").unwrap();
        let ctx = test_context(dir.path());
        ReadTool.call(&ctx, json!({"path": "a.rs"})).await;
        for (old, new) in [("1", "2"), ("2", "3")] {
            let result = mutate(
                &ctx,
                &EditTool,
                json!({"path": "a.rs", "old": old, "new": new}),
            )
            .await;
            assert!(!result.is_error, "{}", result.content);
        }
        assert_eq!(on_disk(&dir, "a.rs").unwrap(), "let x = 3;\n");

        let first = mutate(&ctx, &UndoTool, json!({"path": "a.rs"})).await;
        assert!(!first.is_error, "{}", first.content);
        assert!(
            first
                .content
                .contains("1 earlier change can still be undone"),
            "{}",
            first.content
        );
        assert_eq!(on_disk(&dir, "a.rs").unwrap(), "let x = 2;\n");

        let second = mutate(&ctx, &UndoTool, json!({"path": "./a.rs"})).await;
        assert!(!second.is_error, "{}", second.content);
        assert_eq!(on_disk(&dir, "a.rs").unwrap(), "let x = 1;\n");

        let third = mutate(&ctx, &UndoTool, json!({"path": "a.rs"})).await;
        assert!(
            third.content.starts_with("[NOTHING_TO_UNDO]"),
            "{}",
            third.content
        );
        // Undo re-stamps the file, so editing again needs no re-read.
        let again = mutate(
            &ctx,
            &EditTool,
            json!({"path": "a.rs", "old": "1", "new": "9"}),
        )
        .await;
        assert!(!again.is_error, "{}", again.content);
    }

    #[tokio::test]
    async fn undo_of_a_created_file_deletes_it_and_of_a_rewrite_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());
        mutate(
            &ctx,
            &WriteTool,
            json!({"path": "new.txt", "content": "fresh\n"}),
        )
        .await;
        let undone = mutate(&ctx, &UndoTool, json!({"path": "new.txt"})).await;
        assert!(
            undone.content.contains("deleted new.txt"),
            "{}",
            undone.content
        );
        assert!(on_disk(&dir, "new.txt").is_none());

        std::fs::write(dir.path().join("b.txt"), "original\n").unwrap();
        ReadTool.call(&ctx, json!({"path": "b.txt"})).await;
        mutate(
            &ctx,
            &WriteTool,
            json!({"path": "b.txt", "content": "rewritten\n"}),
        )
        .await;
        let undone = mutate(&ctx, &UndoTool, json!({"path": "b.txt"})).await;
        assert!(!undone.is_error, "{}", undone.content);
        assert_eq!(on_disk(&dir, "b.txt").unwrap(), "original\n");
    }

    #[tokio::test]
    async fn outside_changes_are_never_discarded() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "one\n").unwrap();
        let ctx = test_context(dir.path());
        ReadTool.call(&ctx, json!({"path": "a.rs"})).await;
        mutate(
            &ctx,
            &EditTool,
            json!({"path": "a.rs", "old": "one", "new": "two"}),
        )
        .await;
        // The user (or a formatter, or bash) edits the file afterwards.
        std::fs::write(dir.path().join("a.rs"), "two, and my work\n").unwrap();
        let result = mutate(&ctx, &UndoTool, json!({"path": "a.rs"})).await;
        assert!(
            result.content.starts_with("[UNDO_CONFLICT]"),
            "{}",
            result.content
        );
        assert_eq!(on_disk(&dir, "a.rs").unwrap(), "two, and my work\n");
    }

    #[tokio::test]
    async fn untracked_files_and_missing_snapshots_explain_themselves() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "x\n").unwrap();
        let ctx = test_context(dir.path());
        let result = mutate(&ctx, &UndoTool, json!({"path": "a.rs"})).await;
        assert!(
            result.content.contains("bash are not tracked"),
            "{}",
            result.content
        );

        // A change whose previous file could not be snapshotted.
        let entry = CustomData::new("fs-change")
            .with("path", "a.rs")
            .with("snapshot_error", "the previous file was not UTF-8 text")
            .with("after_sha256", content_hash(b"x\n"));
        ctx.session
            .lock()
            .unwrap()
            .append(EntryKind::Custom(entry))
            .unwrap();
        let result = mutate(&ctx, &UndoTool, json!({"path": "a.rs"})).await;
        assert!(
            result.content.contains("not UTF-8 text"),
            "{}",
            result.content
        );
        assert_eq!(on_disk(&dir, "a.rs").unwrap(), "x\n");
    }
}

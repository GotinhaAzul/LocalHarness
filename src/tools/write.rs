//! `write` tool.

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::{
    ReadTracker, Tool, ToolContext, ToolPack, ToolResult, display_path, optional_bool, required_str,
};

/// Maximum bytes accepted for one write.
const MAX_CONTENT: usize = 256 * 1024;

pub struct WriteTool;

impl WriteTool {}

#[async_trait]
impl Tool for WriteTool {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &'static str {
        "Create or replace a file atomically. Replacing an existing file requires a full read of it, unless overwrite is true."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "path": {"type": "string", "description": "File to write, relative to the working directory."},
                "content": {"type": "string", "description": "Full file content."},
                "allow_empty": {"type": "boolean", "description": "Permit writing an empty file."},
                "overwrite": {"type": "boolean", "description": "Replace a partially-read file anyway; the file must still be unchanged since the last read. Use only when the read intentionally covered part of the file."}
            }),
            &["path", "content"],
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
        let resolved = raw.as_deref().and_then(|raw| ctx.resolve(raw).ok());
        let snapshot = match &resolved {
            Some(path) => ctx.lsp.snapshot_before_edit(ctx, path).await,
            None => crate::tools::lsp::EditSnapshot::default(),
        };
        let result = match run(ctx, &args) {
            Ok(result) => result,
            Err(err) => return err,
        };
        match resolved {
            Some(path) => result.with_diagnostics(ctx, &path, snapshot).await,
            None => result,
        }
    }
}

fn run(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
    let raw = required_str(args, "path")?;
    let content = args
        .get("content")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            ToolResult::err(
                "invalid_args",
                "missing required argument \"content\"; pass the full file text",
            )
        })?;
    let allow_empty = optional_bool(args, "allow_empty").unwrap_or(false);
    let overwrite = optional_bool(args, "overwrite").unwrap_or(false);

    if content.is_empty() && !allow_empty {
        return Err(ToolResult::err(
            "EMPTY_CONTENT",
            "content is empty; pass \"allow_empty\": true to write an intentionally empty file",
        ));
    }
    if content.len() > MAX_CONTENT {
        return Err(ToolResult::err(
            "CONTENT_TOO_LARGE",
            format!(
                "content is {} bytes; write caps at 256 KB. Use edit for targeted changes or split the file.",
                content.len()
            ),
        ));
    }

    let path = ctx.resolve(&raw)?;
    let display = display_path(&ctx.cwd, &path);

    let existed = path.exists();
    if existed {
        let meta = std::fs::metadata(&path).map_err(|err| {
            ToolResult::err("WRITE_FAILED", format!("cannot stat {display}: {err}"))
        })?;
        if meta.is_dir() {
            return Err(ToolResult::err(
                "WRITE_FAILED",
                format!("{display} is a directory; pass a file path"),
            ));
        }
        let gate = {
            let reads = lock_reads(ctx);
            if !reads.is_current(&path) {
                Err(ToolResult::err(
                    "READ_FIRST",
                    format!("read {display} first"),
                ))
            } else if !overwrite && !reads.is_fully_read(&path) {
                let unseen = reads.unseen_ranges(&path);
                let coverage = reads.coverage(&path);
                Err(coverage_error(&display, coverage, &unseen))
            } else {
                Ok(())
            }
        };
        gate?;
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        let parent_existed = parent.exists();
        std::fs::create_dir_all(parent).map_err(|err| {
            ToolResult::err(
                "WRITE_FAILED",
                format!("cannot create {}: {err}", parent.display()),
            )
        })?;
        if !parent_existed {
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
    }

    let mode = if existed { file_mode(&path) } else { 0o600 };
    let previous = crate::tools::undo::previous_text(&path);
    let change = crate::tools::undo::record_change(
        ctx,
        previous
            .as_ref()
            .map(Option::as_deref)
            .map_err(Clone::clone),
        content,
    );
    atomic_write(&path, content.as_bytes(), mode)
        .map_err(|err| ToolResult::err("WRITE_FAILED", format!("cannot write {display}: {err}")))?;

    record_own_write(ctx, &path);

    let added = crate::tools::read::line_count(content);
    let text = format!(
        "wrote {display} ({} bytes, {added} lines){}",
        content.len(),
        if existed { ", replaced" } else { ", created" }
    );
    Ok(
        ToolResult::ok(text).with_details(crate::tools::undo::merge_details(
            json!({
                "path": display,
                "created": !existed,
                "added": added,
                "bytes": content.len(),
            }),
            change,
        )),
    )
}

/// Re-stamp a file the harness itself just wrote. The model authored the new
/// text and saw the result, so a re-read would only cost a turn; any later
/// change by something else still breaks the stamp and requires a read.
pub(crate) fn record_own_write(ctx: &ToolContext, path: &Path) {
    if let Some(stamp) = crate::tools::FileStamp::stamp_for(path) {
        let total_lines = std::fs::read(path)
            .map(|bytes| crate::tools::read::line_count(&String::from_utf8_lossy(&bytes)))
            .unwrap_or(0);
        lock_reads(ctx).record_full(path, stamp, total_lines);
    }
}

/// Refuse a whole-file replace when the last read left lines unseen.
fn coverage_error(
    display: &str,
    coverage: Option<&crate::tools::ReadCoverage>,
    unseen: &[crate::tools::LineRange],
) -> ToolResult {
    let escape = "or pass \"overwrite\": true to replace the file anyway.";
    let Some(coverage) = coverage else {
        return ToolResult::err(
            "COVERAGE",
            format!(
                "read {display} completely first; writing would replace content you never saw. Read it {escape}"
            ),
        );
    };
    let total = coverage.total_lines;
    let message = if unseen.is_empty() {
        format!(
            "read {display} but its result was truncated before all {total} lines reached you; writing would replace content you never saw. Read the file in smaller ranges {escape}"
        )
    } else if coverage.seen.is_empty() {
        format!(
            "read {display} covered none of its {total} lines; writing would delete lines {} you never saw. Read it (start=1) {escape}",
            describe_ranges(unseen)
        )
    } else {
        format!(
            "read {display} only covered lines {} of {total}; writing would delete lines {} you never saw. Read the rest (start={}) {escape}",
            describe_ranges(&coverage.seen),
            describe_ranges(unseen),
            unseen[0].start
        )
    };
    ToolResult::err("COVERAGE", message)
}

fn describe_ranges(ranges: &[crate::tools::LineRange]) -> String {
    ranges
        .iter()
        .map(|range| {
            if range.start == range.end {
                range.start.to_string()
            } else {
                format!("{}-{}", range.start, range.end)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Borrow the read tracker without panicking on a poisoned lock.
pub(crate) fn lock_reads(ctx: &ToolContext) -> MutexGuard<'_, ReadTracker> {
    ctx.reads
        .lock()
        .unwrap_or_else(|err: PoisonError<_>| err.into_inner())
}

/// Existing permission bits for `path`, or `0o600` when they cannot be read.
pub(crate) fn file_mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o7777)
        .unwrap_or(0o600)
}

/// Replace `path` atomically: write a sibling temp file, fsync, then rename.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp = dir.join(format!(
        ".{name}.lh-tmp-{}-{nanos}-{seq}",
        std::process::id()
    ));

    let outcome = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&temp, path)
    })();

    if outcome.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    outcome
}

#[cfg(test)]
pub(crate) fn test_context(dir: &Path) -> ToolContext {
    use std::sync::{Arc, Mutex};

    use tokio_util::sync::CancellationToken;

    use crate::config::VerifyConfig;
    use crate::paths::Paths;
    use crate::security::SecurityState;
    use crate::session::{ArtifactStore, Session, SessionData};
    use crate::tools::BgRegistry;

    let cwd = dir.canonicalize().expect("canonical temp dir");
    let paths = Paths {
        config_dir: cwd.join(".lh-config"),
        data_dir: cwd.join(".lh-data"),
        cwd: cwd.clone(),
        project_id: "test".into(),
        sessions_dir_override: None,
    };
    let header = SessionData {
        cwd: cwd.display().to_string(),
        endpoint: "test".into(),
        model: "test".into(),
        profile: "test".into(),
        app: "test".into(),
        redact: true,
        context: None,
    };
    let session = Session::create(&paths, header, true).expect("in-memory session");
    ToolContext {
        cwd,
        path_jail: true,
        session: Arc::new(Mutex::new(session)),
        artifacts: Arc::new(ArtifactStore::ephemeral()),
        cancel: CancellationToken::new(),
        trust: false,
        verify: VerifyConfig { steps: Vec::new() },
        profile: "test".into(),
        packs: Vec::new(),
        reads: Arc::new(Mutex::new(ReadTracker::default())),
        bg: Arc::new(Mutex::new(BgRegistry::default())),
        security: Arc::new(Mutex::new(SecurityState::default())),
        paths,
        lsp: Arc::new(crate::tools::lsp::LspService::new()),
        sandbox: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::edit::EditTool;
    use crate::tools::read::ReadTool;
    use serde_json::json;

    fn numbered(count: usize) -> String {
        (1..=count).map(|n| format!("line{n}\n")).collect()
    }

    #[tokio::test]
    async fn creates_file_with_details() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());

        let result = WriteTool
            .call(
                &ctx,
                json!({"path": "fresh/nested/a.txt", "content": "one\ntwo\n"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        let details = result.details.unwrap();
        assert_eq!(details["path"].as_str(), Some("fresh/nested/a.txt"));
        assert_eq!(details["created"].as_bool(), Some(true));
        assert_eq!(details["added"].as_u64(), Some(2));
        assert_eq!(details["bytes"].as_u64(), Some(8));

        let written = std::fs::read_to_string(dir.path().join("fresh/nested/a.txt")).unwrap();
        assert_eq!(written, "one\ntwo\n");
        let mode = std::fs::metadata(dir.path().join("fresh/nested/a.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[tokio::test]
    async fn overwrite_requires_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = test_context(dir.path());

        let denied = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "new\n"}))
            .await;
        assert!(denied.is_error);
        assert!(
            denied.content.starts_with("[READ_FIRST]"),
            "{}",
            denied.content
        );

        let _ = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        let written = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "new\n"}))
            .await;
        assert!(!written.is_error, "{}", written.content);
        assert_eq!(written.details.unwrap()["created"].as_bool(), Some(false));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "new\n"
        );
    }

    #[tokio::test]
    async fn refuses_empty_content_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());

        let denied = WriteTool
            .call(&ctx, json!({"path": "empty.txt", "content": ""}))
            .await;
        assert!(denied.is_error);
        assert!(denied.content.starts_with("[EMPTY_CONTENT]"));

        let allowed = WriteTool
            .call(
                &ctx,
                json!({"path": "empty.txt", "content": "", "allow_empty": true}),
            )
            .await;
        assert!(!allowed.is_error, "{}", allowed.content);
        assert_eq!(
            std::fs::metadata(dir.path().join("empty.txt"))
                .unwrap()
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn past_eof_read_does_not_authorize_write() {
        let dir = tempfile::tempdir().unwrap();
        let original = numbered(10);
        std::fs::write(dir.path().join("a.txt"), &original).unwrap();
        let ctx = test_context(dir.path());

        let read = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 999}))
            .await;
        assert!(!read.is_error, "{}", read.content);

        let denied = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "gone\n"}))
            .await;
        assert!(denied.is_error, "{}", denied.content);
        assert!(
            denied.content.starts_with("[COVERAGE]"),
            "{}",
            denied.content
        );
        assert!(denied.content.contains("1-10"), "{}", denied.content);
        assert!(
            denied.content.contains("\"overwrite\": true"),
            "{}",
            denied.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn partial_read_blocks_write_but_not_edit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered(400)).unwrap();
        let ctx = test_context(dir.path());

        let read = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        assert!(!read.is_error, "{}", read.content);
        assert_eq!(read.details.unwrap()["end"].as_u64(), Some(150));

        let denied = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "gone\n"}))
            .await;
        assert!(denied.is_error, "{}", denied.content);
        assert!(
            denied.content.starts_with("[COVERAGE]"),
            "{}",
            denied.content
        );
        assert!(denied.content.contains("151-400"), "{}", denied.content);
        assert!(denied.content.contains("start=151"), "{}", denied.content);
        assert!(
            denied.content.contains("\"overwrite\": true"),
            "{}",
            denied.content
        );

        let edited = EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "old": "line300\n", "new": "line300 edited\n"}),
            )
            .await;
        assert!(!edited.is_error, "{}", edited.content);
        assert!(
            std::fs::read_to_string(dir.path().join("a.txt"))
                .unwrap()
                .contains("line300 edited")
        );
    }

    #[tokio::test]
    async fn full_read_authorizes_write() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered(400)).unwrap();
        let ctx = test_context(dir.path());

        let read = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 1, "end": 400}))
            .await;
        assert!(!read.is_error, "{}", read.content);
        assert_eq!(read.details.unwrap()["capped"], false);

        let written = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "replaced\n"}))
            .await;
        assert!(!written.is_error, "{}", written.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "replaced\n"
        );
    }

    #[tokio::test]
    async fn overwrite_flag_replaces_partially_read_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered(400)).unwrap();
        let ctx = test_context(dir.path());

        let _ = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        let written = WriteTool
            .call(
                &ctx,
                json!({"path": "a.txt", "content": "replaced\n", "overwrite": true}),
            )
            .await;
        assert!(!written.is_error, "{}", written.content);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "replaced\n"
        );
    }

    #[tokio::test]
    async fn change_after_full_read_still_requires_read_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered(400)).unwrap();
        let ctx = test_context(dir.path());

        let read = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 1, "end": 400}))
            .await;
        assert!(!read.is_error, "{}", read.content);

        std::fs::write(dir.path().join("a.txt"), numbered(401)).unwrap();
        let denied = WriteTool
            .call(
                &ctx,
                json!({"path": "a.txt", "content": "gone\n", "overwrite": true}),
            )
            .await;
        assert!(denied.is_error, "{}", denied.content);
        assert!(
            denied.content.starts_with("[READ_FIRST]"),
            "{}",
            denied.content
        );
    }

    #[tokio::test]
    async fn truncated_result_blocks_write() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), numbered(400)).unwrap();
        let ctx = test_context(dir.path());

        let read = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 1, "end": 400}))
            .await;
        assert!(!read.is_error, "{}", read.content);
        let path = ctx.resolve("a.txt").unwrap();
        assert!(lock_reads(&ctx).is_fully_read(&path));

        lock_reads(&ctx).mark_result_truncated(&path);
        assert!(lock_reads(&ctx).is_current(&path));
        assert!(!lock_reads(&ctx).is_fully_read(&path));

        // A later past-EOF read does not redeliver the lost content.
        let _ = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 999}))
            .await;
        assert!(!lock_reads(&ctx).is_fully_read(&path));

        let denied = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": "gone\n"}))
            .await;
        assert!(denied.is_error, "{}", denied.content);
        assert!(
            denied.content.starts_with("[COVERAGE]"),
            "{}",
            denied.content
        );
        assert!(denied.content.contains("truncated"), "{}", denied.content);
    }
}

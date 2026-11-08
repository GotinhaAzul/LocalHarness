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
        "Create or replace a file atomically. Read an existing file first."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "path": {"type": "string", "description": "File to write, relative to the working directory."},
                "content": {"type": "string", "description": "Full file content."},
                "allow_empty": {"type": "boolean", "description": "Permit writing an empty file."}
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
        match run(ctx, &args) {
            Ok(result) => result,
            Err(err) => err,
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
        if !lock_reads(ctx).is_current(&path) {
            return Err(ToolResult::err(
                "READ_FIRST",
                format!("read {display} first"),
            ));
        }
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
    atomic_write(&path, content.as_bytes(), mode)
        .map_err(|err| ToolResult::err("WRITE_FAILED", format!("cannot write {display}: {err}")))?;

    // The on-disk text changed underneath the tracker; force a re-read.
    lock_reads(ctx).clear();

    let added = crate::tools::read::line_count(content);
    let text = format!(
        "wrote {display} ({} bytes, {added} lines){}",
        content.len(),
        if existed { ", replaced" } else { ", created" }
    );
    Ok(ToolResult::ok(text).with_details(json!({
        "path": display,
        "created": !existed,
        "added": added,
        "bytes": content.len(),
    })))
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
        reads: Arc::new(Mutex::new(ReadTracker::default())),
        bg: Arc::new(Mutex::new(BgRegistry::default())),
        security: Arc::new(Mutex::new(SecurityState::default())),
        paths,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::read::ReadTool;
    use serde_json::json;

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
}

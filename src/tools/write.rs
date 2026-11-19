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
                "content": {"type": "string", "description": "Full file content. May be empty when creating a file, such as __init__.py or .gitkeep."},
                "allow_empty": {"type": "boolean", "description": "Permit emptying an existing non-empty file. Not needed to create an empty file or to rewrite one that is already empty."},
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
        // An empty write only needs the flag when it would discard existing
        // content. Creating an empty file, or rewriting one that is already
        // empty, loses nothing.
        if content.is_empty() && !allow_empty && meta.len() > 0 {
            return Err(ToolResult::err(
                "TRUNCATE",
                format!(
                    "this would truncate {display} ({} bytes); to empty an existing file intentionally pass \"allow_empty\": true",
                    meta.len()
                ),
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
        // `create_dir_all` applies the process umask, exactly like editors and
        // `mkdir -p`; a private 0700 directory would hide new assets from a
        // web server or another uid.
        std::fs::create_dir_all(parent).map_err(|err| {
            ToolResult::err(
                "WRITE_FAILED",
                format!("cannot create {}: {err}", parent.display()),
            )
        })?;
    }

    let mode = if existed {
        file_mode(&path)
    } else {
        new_file_mode()
    };
    let target_encoding = if existed {
        match crate::tools::encoding::decode(&std::fs::read(&path).map_err(|err| {
            ToolResult::err("WRITE_FAILED", format!("cannot read {display}: {err}"))
        })?) {
            crate::tools::encoding::Decoded::Text { encoding, .. } => encoding,
            crate::tools::encoding::Decoded::Binary => {
                return Err(ToolResult::err(
                    "ENCODING",
                    format!(
                        "{display} is binary (NUL bytes without a UTF-16 shape or BOM); write only replaces text. Delete it with bash and create it again, or edit it with a tool that understands the format."
                    ),
                ));
            }
        }
    } else {
        crate::tools::encoding::Encoding::Utf8
    };
    let bytes = crate::tools::encoding::encode(content, target_encoding).ok_or_else(|| {
        ToolResult::err(
            "ENCODING",
            format!(
                "{display} is {}; the new content has characters that encoding cannot represent. Keep the text inside its character set, or convert the file deliberately: `iconv -f {0} -t UTF-8` through bash, then rewrite it as UTF-8.",
                target_encoding.label()
            ),
        )
    })?;
    let previous = crate::tools::undo::previous_snapshot(&path);
    // A read failure (not text, or over 4 MB) cannot be diffed for review;
    // keep the lossy text so `mutation_write` knows the file exists.
    let before_text = match &previous {
        Ok(Some((text, _))) => Some(text.clone()),
        _ => std::fs::read(&path)
            .ok()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned()),
    };
    let change = crate::tools::undo::record_change_encoded(
        ctx,
        previous
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .as_ref()
                    .map(|(text, encoding)| (text.as_str(), *encoding))
            })
            .map_err(Clone::clone),
        &bytes,
        target_encoding,
    );
    crate::tools::hunk::mutation_write_encoded(
        &path,
        before_text.as_deref(),
        content,
        &bytes,
        mode,
        target_encoding,
    )
    .map_err(|err| ToolResult::err("WRITE_FAILED", format!("cannot write {display}: {err}")))?;

    record_own_write(ctx, &path);

    let added = crate::tools::read::line_count(content);
    let text = format!(
        "wrote {display} ({} bytes, {added} lines{}){}",
        bytes.len(),
        if target_encoding == crate::tools::encoding::Encoding::Utf8 {
            String::new()
        } else {
            format!(", {}", target_encoding.label())
        },
        if existed { ", replaced" } else { ", created" }
    );
    Ok(
        ToolResult::ok(text).with_details(crate::tools::undo::merge_details(
            json!({
                "path": display,
                "created": !existed,
                "added": added,
                "bytes": content.len(),
                "encoding": target_encoding.label(),
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

/// The process umask, from `/proc/self/status` on Linux. Reading it from the
/// kernel avoids the race of calling `umask` twice in a multi-threaded
/// process; the conventional 022 is used when the file cannot be read.
pub(crate) fn process_umask() -> u32 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix("Umask:")
                    .and_then(|value| u32::from_str_radix(value.trim(), 8).ok())
            })
        })
        .unwrap_or(0o022)
}

/// Mode for a newly created file: what `std::fs::write` and an editor get,
/// `0o666` minus the umask. A private 0600 file breaks web roots, shared
/// checkouts, and `docker build`, and git does not record it.
pub(crate) fn new_file_mode() -> u32 {
    0o666 & !process_umask()
}

/// Replace `path` atomically: write a sibling temp file, fsync, then rename.
/// Every harness file mutation (`write`, `edit`, `patch`, `undo`) goes
/// through here, so this is also where redaction placeholders are refused.
pub(crate) fn atomic_write(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    refuse_new_placeholders(path, bytes)?;
    atomic_restore(path, bytes, mode)
}

/// Put back bytes that were on disk moments ago in this process, for example
/// when a failed multi-file apply rolls back. This skips the placeholder
/// guard: the bytes are the file's own previous state, not new content, and a
/// file that legitimately held placeholder text must be restorable.
pub(crate) fn atomic_restore(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
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
        ".{name}.tot-tmp-{}-{nanos}-{seq}",
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

/// A `[REDACTED:<kind>:<first4>...<hash8>]` placeholder stands in for a value
/// removed when a session was saved. It is never real file content, so
/// writing one the file does not already hold would corrupt the file. The
/// live session never shows the model a placeholder, but a resumed session or
/// an old artifact can.
pub(crate) fn refuse_new_placeholders(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let after = String::from_utf8_lossy(bytes);
    if !crate::security::redact::contains_placeholder(&after) {
        return Ok(());
    }
    let before = std::fs::read(path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let new = crate::security::redact::new_placeholders(&before, &after);
    let Some(first) = new.first() else {
        return Ok(());
    };
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "the new content contains {} redaction placeholder(s) such as {first} that the file does not; placeholders replace values when a session is saved and are not file content. Re-read the file and write its real text; nothing was written",
            new.len()
        ),
    ))
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
        config_dir: cwd.join(".tot-config"),
        data_dir: cwd.join(".tot-data"),
        cwd: cwd.clone(),
        project_id: "test".into(),
        sessions_dir_override: None,
        home_dir: None,
    };
    let header = SessionData {
        cwd: cwd.display().to_string(),
        endpoint: "test".into(),
        model: "test".into(),
        profile: "test".into(),
        app: "test".into(),
        redact: true,
        context: None,
        permissions: None,
    };
    let session = Session::create(&paths, header, true).expect("in-memory session");
    ToolContext {
        cwd,
        path_jail: true,
        session: Arc::new(Mutex::new(session)),
        artifacts: Arc::new(ArtifactStore::ephemeral()),
        cancel: CancellationToken::new(),
        trust: false,
        verify: VerifyConfig::default(),
        profile: "test".into(),
        edit_mode: crate::config::EditMode::Text,
        packs: Vec::new(),
        reads: Arc::new(Mutex::new(ReadTracker::default())),
        bg: Arc::new(Mutex::new(BgRegistry::default())),
        security: Arc::new(Mutex::new(SecurityState::default())),
        paths,
        lsp: Arc::new(crate::tools::lsp::LspService::new()),
        sandbox: None,
        vision: Default::default(),
        caps: Default::default(),
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
        // New files follow the process umask, like `std::fs::write`; they are
        // not private 0600, which breaks web roots and other uids.
        assert_eq!(mode, new_file_mode());
        let dir_mode = std::fs::metadata(dir.path().join("fresh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o777 & !process_umask(), "directory umask");
        assert!(
            mode & 0o400 != 0 && mode & 0o040 != 0,
            "the owner and group can read a new file: {mode:o}"
        );
    }

    #[tokio::test]
    async fn write_keeps_an_existing_files_encoding_or_refuses_a_lossy_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let original = b"Sobre o Nimbus \xe9 vers\xe3o 2\n";
        std::fs::write(dir.path().join("strings_pt.properties"), original).unwrap();
        let ctx = test_context(dir.path());
        let _ = ReadTool
            .call(&ctx, json!({"path": "strings_pt.properties"}))
            .await;

        // A representable change keeps latin-1: `ã` is one byte e3, not UTF-8
        // c3 a3.
        let written = WriteTool
            .call(
                &ctx,
                json!({"path": "strings_pt.properties", "content": "Sobre o Stratus versão 3\n"}),
            )
            .await;
        assert!(!written.is_error, "{}", written.content);
        let bytes = std::fs::read(dir.path().join("strings_pt.properties")).unwrap();
        assert_eq!(bytes, b"Sobre o Stratus vers\xe3o 3\n");
        assert!(written.content.contains("latin-1"), "{}", written.content);
        let details = written.details.clone().unwrap();
        assert_eq!(details["encoding"], "latin-1");

        // Undo restores the original Latin-1 bytes, not a UTF-8 copy of them.
        let entry =
            crate::tools::undo::fs_change_entry("strings_pt.properties".into(), Some(&details));
        ctx.session
            .lock()
            .unwrap()
            .append(crate::session::EntryKind::Custom(entry))
            .unwrap();
        let undone = crate::tools::undo::UndoTool
            .call(&ctx, json!({"path": "strings_pt.properties"}))
            .await;
        assert!(!undone.is_error, "{}", undone.content);
        assert_eq!(
            std::fs::read(dir.path().join("strings_pt.properties")).unwrap(),
            original
        );

        // A character latin-1 cannot represent is refused with a next step,
        // never silently written as UTF-8.
        let restored = WriteTool
            .call(
                &ctx,
                json!({"path": "strings_pt.properties", "content": "Sobre o Stratus versão 3\n"}),
            )
            .await;
        assert!(!restored.is_error, "{}", restored.content);
        let refused = WriteTool
            .call(
                &ctx,
                json!({"path": "strings_pt.properties", "content": "日本語\n"}),
            )
            .await;
        assert!(refused.is_error, "{}", refused.content);
        assert!(
            refused.content.starts_with("[ENCODING]"),
            "{}",
            refused.content
        );
        assert_eq!(
            std::fs::read(dir.path().join("strings_pt.properties")).unwrap(),
            b"Sobre o Stratus vers\xe3o 3\n"
        );
    }

    fn placeholder_for(value: &str) -> String {
        // Built through the redactor so the test follows its real format.
        let redacted = crate::security::redact::Redactor::new(true)
            .redact_owned(&format!("aws_key = {value}"));
        redacted.trim_start_matches("aws_key = ").to_string()
    }

    #[test]
    fn atomic_write_refuses_a_new_placeholder_and_leaves_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task_storage.py");
        let original = "name = Column(Text)\n";
        std::fs::write(&path, original).unwrap();
        let key = format!("AKIA{}", "Q".repeat(16));
        let placeholder = placeholder_for(&key);
        assert!(placeholder.starts_with("[REDACTED:aws:"), "{placeholder}");
        let corrupted = format!("name = {placeholder} Text)\n");
        let err = atomic_write(&path, corrupted.as_bytes(), 0o600).unwrap_err();
        assert!(err.to_string().contains("redaction placeholder"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        // A new file is refused too.
        let fresh = dir.path().join("fresh.py");
        assert!(atomic_write(&fresh, corrupted.as_bytes(), 0o600).is_err());
        assert!(!fresh.exists());
    }

    #[test]
    fn atomic_write_keeps_placeholders_the_file_already_has() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.txt");
        let placeholder = placeholder_for(&format!("AKIA{}", "Z".repeat(16)));
        std::fs::write(&path, format!("expected = {placeholder}\n")).unwrap();
        let edited = format!("expected = {placeholder}\nmore = 1\n");
        atomic_write(&path, edited.as_bytes(), 0o600).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
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
    async fn creates_empty_file_without_flag() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_context(dir.path());

        for name in ["pkg/__init__.py", ".gitkeep"] {
            let result = WriteTool
                .call(&ctx, json!({"path": name, "content": ""}))
                .await;
            assert!(!result.is_error, "{}", result.content);
            let details = result.details.unwrap();
            assert_eq!(details["created"].as_bool(), Some(true));
            assert_eq!(details["bytes"].as_u64(), Some(0));
            let meta = std::fs::metadata(dir.path().join(name)).unwrap();
            assert_eq!(meta.len(), 0);
        }
    }

    #[tokio::test]
    async fn empty_write_refuses_to_truncate_a_non_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = test_context(dir.path());

        let denied = WriteTool
            .call(&ctx, json!({"path": "a.txt", "content": ""}))
            .await;
        assert!(denied.is_error);
        assert!(
            denied.content.starts_with("[TRUNCATE]"),
            "{}",
            denied.content
        );
        assert!(denied.content.contains("a.txt"), "{}", denied.content);
        assert!(denied.content.contains("4 bytes"), "{}", denied.content);
        assert!(
            denied.content.contains("\"allow_empty\": true"),
            "{}",
            denied.content
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "old\n"
        );
    }

    #[tokio::test]
    async fn allow_empty_truncates_a_non_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "old\n").unwrap();
        let ctx = test_context(dir.path());
        let _ = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;

        let written = WriteTool
            .call(
                &ctx,
                json!({"path": "a.txt", "content": "", "allow_empty": true}),
            )
            .await;
        assert!(!written.is_error, "{}", written.content);
        let details = written.details.unwrap();
        assert_eq!(details["created"].as_bool(), Some(false));
        assert_eq!(details["bytes"].as_u64(), Some(0));
        assert_eq!(
            std::fs::metadata(dir.path().join("a.txt")).unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn empty_write_over_an_empty_file_needs_no_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        let ctx = test_context(dir.path());
        // An existing file still needs a read before it can be replaced.
        let read = ReadTool.call(&ctx, json!({"path": "empty.txt"})).await;
        assert!(!read.is_error, "{}", read.content);

        let written = WriteTool
            .call(&ctx, json!({"path": "empty.txt", "content": ""}))
            .await;
        assert!(!written.is_error, "{}", written.content);
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
        let mut ctx = test_context(dir.path());
        // The whole file fits one result here; coverage is under test.
        ctx.caps = crate::context::ResultCaps {
            result: 16_000,
            batch: 40_000,
        };

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
        let mut ctx = test_context(dir.path());
        // The whole file fits one result here; coverage is under test.
        ctx.caps = crate::context::ResultCaps {
            result: 16_000,
            batch: 40_000,
        };

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

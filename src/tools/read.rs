//! `read` tool.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tools::{
    FileStamp, Tool, ToolContext, ToolPack, ToolResult, display_path, optional_u64, required_str,
};

/// Hard cap on the bytes read from disk.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_LINES: u64 = 150;
const MAX_LINES: u64 = 800;
/// NUL bytes in this prefix mark a file as binary.
const BINARY_SNIFF: usize = 8 * 1024;

pub struct ReadTool;

impl ReadTool {}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a text file with line numbers (default 150 lines, max 800)."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "path": {"type": "string", "description": "File to read, relative to the working directory."},
                "start": {"type": "integer", "description": "First line to return, 1-based."},
                "end": {"type": "integer", "description": "Last line to return, inclusive."},
                "limit": {"type": "integer", "description": "Maximum lines to return (default 150, max 800)."}
            }),
            &["path"],
        )
    }

    fn pack(&self) -> ToolPack {
        ToolPack::Core
    }

    fn read_only(&self) -> bool {
        true
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
    let path = ctx.resolve(&raw)?;
    let display = display_path(&ctx.cwd, &path);

    let meta = std::fs::metadata(&path).map_err(|err| {
        ToolResult::err(
            "READ_FAILED",
            format!("cannot read {display}: {err}. Check the path with glob."),
        )
    })?;
    if meta.is_dir() {
        return Err(ToolResult::err(
            "READ_FAILED",
            format!("{display} is a directory; use glob or grep to list its contents"),
        ));
    }
    if meta.len() > MAX_BYTES {
        return Err(ToolResult::err(
            "FILE_TOO_LARGE",
            format!(
                "{display} is {} bytes; read caps at 4 MB. Use grep with context or read a narrower range.",
                meta.len()
            ),
        ));
    }

    let bytes = std::fs::read(&path)
        .map_err(|err| ToolResult::err("READ_FAILED", format!("cannot read {display}: {err}")))?;
    let sniff = &bytes[..bytes.len().min(BINARY_SNIFF)];
    if sniff.contains(&0) {
        return Err(ToolResult::err(
            "BINARY",
            format!(
                "{display} looks binary (NUL byte in the first 8 KiB); read only returns text. Use bash with `file` or `xxd` instead."
            ),
        ));
    }

    let content = String::from_utf8_lossy(&bytes);
    let lines = split_lines(&content);
    let total = lines.len();

    let start_u64 = optional_u64(args, "start").unwrap_or(1).max(1);
    let start = usize::try_from(start_u64).unwrap_or(usize::MAX);
    if let Some(end) = optional_u64(args, "end") {
        if end < start_u64 {
            return Err(ToolResult::err(
                "invalid_args",
                format!("end={end} is before start={start_u64}; pass end >= start"),
            ));
        }
    }
    let limit_u64 = optional_u64(args, "limit")
        .unwrap_or(DEFAULT_LINES)
        .clamp(1, MAX_LINES);
    let limit = usize::try_from(limit_u64).unwrap_or(MAX_LINES as usize);

    if total == 0 {
        record_stamp(ctx, &path);
        let text = format!("{display} is empty (0 lines)");
        return Ok(ToolResult::ok(text).with_details(json!({
            "path": display,
            "start": 1,
            "end": 0,
            "total_lines": 0,
            "capped": false,
        })));
    }

    let first = start.saturating_sub(1);
    if first >= total {
        record_stamp(ctx, &path);
        let text = format!("{display} has {total} lines; start={start} is past the end");
        return Ok(ToolResult::ok(text).with_details(json!({
            "path": display,
            "start": start,
            "end": total,
            "total_lines": total,
            "capped": false,
        })));
    }

    let available_end = optional_u64(args, "end")
        .map(|end| usize::try_from(end).unwrap_or(usize::MAX))
        .unwrap_or(total)
        .min(total);
    let last = available_end.min(start.saturating_add(limit).saturating_sub(1));
    let capped = last < available_end;

    let width = last.to_string().len();
    let mut text = String::new();
    for (offset, line) in lines[first..last].iter().enumerate() {
        let number = start + offset;
        text.push_str(&format!("{:>width$}  {}\n", number, line, width = width));
    }
    while text.ends_with('\n') {
        text.pop();
    }
    if capped {
        text.push_str(&format!(
            "\n\n[lines {start}-{last} of {total}; next start={}]",
            last + 1
        ));
    }

    record_stamp(ctx, &path);
    Ok(ToolResult::ok(text).with_details(json!({
        "path": display,
        "start": start,
        "end": last,
        "total_lines": total,
        "capped": capped,
    })))
}

fn record_stamp(ctx: &ToolContext, path: &Path) {
    if let Some(stamp) = FileStamp::stamp_for(path) {
        let mut reads = crate::tools::write::lock_reads(ctx);
        reads.record(path, stamp);
    }
}

/// Split text into lines, dropping the trailing empty line produced by a final
/// newline and stripping a trailing `\r` from CRLF files.
pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
        .into_iter()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// Number of logical lines in `text` (a trailing newline is not a line).
pub(crate) fn line_count(text: &str) -> usize {
    split_lines(text).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::write::test_context;
    use serde_json::json;

    #[tokio::test]
    async fn numbers_lines() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\ngamma\n").unwrap();
        let ctx = test_context(dir.path());

        let result = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "1  alpha\n2  beta\n3  gamma");
        assert_eq!(result.details.unwrap()["total_lines"].as_u64(), Some(3));
    }

    #[tokio::test]
    async fn caps_and_names_next_start() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=5).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let ctx = test_context(dir.path());

        let result = ReadTool
            .call(&ctx, json!({"path": "a.txt", "limit": 2}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("1  line1"), "{}", result.content);
        assert!(result.content.contains("2  line2"), "{}", result.content);
        assert!(!result.content.contains("3  line3"), "{}", result.content);
        assert!(
            result.content.contains("lines 1-2 of 5; next start=3"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn respects_start_and_end() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=9).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let ctx = test_context(dir.path());

        let result = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 3, "end": 4}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "3  line3\n4  line4");
    }

    #[tokio::test]
    async fn refuses_binary() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blob.bin"), b"\x00\x01\x02binary").unwrap();
        let ctx = test_context(dir.path());

        let result = ReadTool.call(&ctx, json!({"path": "blob.bin"})).await;
        assert!(result.is_error);
        assert!(result.content.starts_with("[BINARY]"), "{}", result.content);
    }

    #[tokio::test]
    async fn records_read_stamp() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
        let ctx = test_context(dir.path());

        let _ = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        let path = dir.path().canonicalize().unwrap().join("a.txt");
        assert!(crate::tools::write::lock_reads(&ctx).is_current(&path));
    }
}

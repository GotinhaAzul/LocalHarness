//! `read` tool.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::config::EditMode;
use crate::tools::{
    FileStamp, Tool, ToolContext, ToolPack, ToolResult, display_path, optional_u64, required_str,
};

/// Hard cap on the bytes read from disk.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
const DEFAULT_LINES: u64 = 150;
const MAX_LINES: u64 = 800;
/// NUL bytes in this prefix mark a file as binary.
const BINARY_SNIFF: usize = 8 * 1024;
/// Characters kept free for the `[lines a-b of n; next start=c]` trailer.
const TRAILER_RESERVE: usize = 80;

pub struct ReadTool;

impl ReadTool {}

#[async_trait]
impl Tool for ReadTool {
    fn name(&self) -> &'static str {
        "read"
    }

    fn description(&self) -> &'static str {
        "Read a text file with line numbers (default 150 lines, max 800). PNG, JPEG, and WebP images are shown to models with vision input."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "path": {"type": "string", "description": "File to read, relative to the working directory."},
                "start": {"type": "integer", "description": "First line to return, 1-based."},
                "end": {"type": "integer", "description": "Last line to return, inclusive (up to 800 lines after start)."},
                "limit": {"type": "integer", "description": "Lines to return when end is absent (default 150, max 800)."}
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
        if let Ok(raw) = required_str(&args, "path")
            && let Ok(path) = ctx.resolve(&raw)
            && crate::media::image::is_image_file(&path)
        {
            return read_image(ctx, &raw).await;
        }
        match run(ctx, &args) {
            Ok(result) => result,
            Err(err) => err,
        }
    }
}

/// `read` on an image: the image itself for a vision model (it follows the
/// tool result as an attachment), a labelled description from
/// `[vision] model` otherwise, or an error naming how to enable either.
async fn read_image(ctx: &ToolContext, raw: &str) -> ToolResult {
    use crate::media::Route;
    let route = ctx.vision.route();
    if let Route::Refused(reason) = &route {
        return ToolResult::err(
            "IMAGE_UNSUPPORTED",
            format!("{raw} is an image, and it cannot be shown: {reason}"),
        );
    }
    let (image, loaded, display) = match crate::agent::mentions::store_image(ctx, raw) {
        Ok(stored) => stored,
        Err(reason) => return ToolResult::err("IMAGE_FAILED", format!("{raw}: {reason}")),
    };
    let summary = loaded.summary();
    match route {
        Route::Describe(model) => {
            let Some(describer) = ctx.vision.describer.as_ref() else {
                return ToolResult::err("IMAGE_UNSUPPORTED", "no vision model is configured");
            };
            let data = crate::media::vision::image_data(&loaded, &display);
            match crate::media::vision::describe(describer, data, ctx.cancel.clone()).await {
                Ok(description) => ToolResult::ok(format!(
                    "{display}: {summary}. Described by {model} (this model has no vision input):\n{description}"
                ))
                .with_details(json!({
                    "path": display,
                    "described_by": model,
                    "width": image.width,
                    "height": image.height,
                })),
                Err(reason) => ToolResult::err("IMAGE_FAILED", format!("{display}: {reason}")),
            }
        }
        _ => ToolResult::ok(format!(
            "{display}: {summary}. The image is attached after the tool results."
        ))
        .with_details(json!({ "path": display, "image": image })),
    }
}

pub(crate) fn run(ctx: &ToolContext, args: &Value) -> Result<ToolResult, ToolResult> {
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
    let snapshot = (ctx.edit_mode == EditMode::Hash).then(|| snapshot_hash(&bytes));

    let start_u64 = optional_u64(args, "start").unwrap_or(1).max(1);
    let start = usize::try_from(start_u64).unwrap_or(usize::MAX);
    if let Some(end) = optional_u64(args, "end")
        && end < start_u64
    {
        return Err(ToolResult::err(
            "invalid_args",
            format!("end={end} is before start={start_u64}; pass end >= start"),
        ));
    }
    // An explicit `end` is the requested range; only the hard cap applies.
    let default_lines = if optional_u64(args, "end").is_some() {
        MAX_LINES
    } else {
        DEFAULT_LINES
    };
    let limit_u64 = optional_u64(args, "limit")
        .unwrap_or(default_lines)
        .clamp(1, MAX_LINES);
    let limit = usize::try_from(limit_u64).unwrap_or(MAX_LINES as usize);

    if total == 0 {
        record_read(ctx, &path, 0, None);
        let text = match &snapshot {
            Some(snapshot) => format!("[{display}#{snapshot}]\n{display} is empty (0 lines)"),
            None => format!("{display} is empty (0 lines)"),
        };
        return Ok(ToolResult::ok(text).with_details(with_snapshot(
            json!({
                "path": display,
                "start": 1,
                "end": 0,
                "total_lines": 0,
                "capped": false,
            }),
            &snapshot,
        )));
    }

    let first = start.saturating_sub(1);
    if first >= total {
        record_read(ctx, &path, total, None);
        let body = format!("{display} has {total} lines; start={start} is past the end");
        let text = match &snapshot {
            Some(snapshot) => format!("[{display}#{snapshot}]\n{body}"),
            None => body,
        };
        return Ok(ToolResult::ok(text).with_details(with_snapshot(
            json!({
                "path": display,
                "start": start,
                "end": total,
                "total_lines": total,
                "capped": false,
            }),
            &snapshot,
        )));
    }

    let available_end = optional_u64(args, "end")
        .map(|end| usize::try_from(end).unwrap_or(usize::MAX))
        .unwrap_or(total)
        .min(total);
    let requested_last = available_end.min(start.saturating_add(limit).saturating_sub(1));

    let width = requested_last.to_string().len();
    let mut text = String::new();
    if let Some(snapshot) = &snapshot {
        text.push_str(&format!("[{display}#{snapshot}]\n"));
    }
    // Whole lines within the result budget, so the loop never cuts the
    // output and the trailer names exactly the lines the model saw.
    let budget = ctx.caps.tool_budget().saturating_sub(TRAILER_RESERVE);
    let mut used = text.chars().count();
    let mut last = start.saturating_sub(1);
    for (offset, line) in lines[first..requested_last].iter().enumerate() {
        let number = start + offset;
        let rendered = match &snapshot {
            Some(_) => format!(
                "{:>width$}  {}  {}\n",
                number,
                line_hash(line),
                line,
                width = width
            ),
            None => format!("{:>width$}  {}\n", number, line, width = width),
        };
        let chars = rendered.chars().count();
        // Always show at least one line; an overlong single line is left to
        // the loop's fallback cut.
        if number > start && used + chars > budget {
            break;
        }
        used += chars;
        text.push_str(&rendered);
        last = number;
    }
    let capped = last < available_end;
    while text.ends_with('\n') {
        text.pop();
    }
    if capped {
        text.push_str(&format!(
            "\n\n[lines {start}-{last} of {total}; next start={}]",
            last + 1
        ));
    }

    record_read(ctx, &path, total, Some((start, last)));
    Ok(ToolResult::ok(text).with_details(with_snapshot(
        json!({
            "path": display,
            "start": start,
            "end": last,
            "total_lines": total,
            "capped": capped,
        }),
        &snapshot,
    )))
}

/// Add the hash-mode fields to a read result's details without changing the
/// default (text-mode) shape.
fn with_snapshot(mut details: Value, snapshot: &Option<String>) -> Value {
    if let (Some(snapshot), Some(object)) = (snapshot, details.as_object_mut()) {
        object.insert("snapshot".into(), json!(snapshot));
        object.insert("edit_mode".into(), json!("hash"));
    }
    details
}

/// Record what this read delivered: `Some((start, end))` for a line range,
/// `None` for an empty result (empty file or `start` past EOF).
fn record_read(ctx: &ToolContext, path: &Path, total_lines: usize, range: Option<(usize, usize)>) {
    if let Some(stamp) = FileStamp::stamp_for(path) {
        let mut reads = crate::tools::write::lock_reads(ctx);
        match range {
            Some((start, end)) => reads.record_range(path, stamp, total_lines, start, end),
            None => reads.record_empty(path, stamp, total_lines),
        }
    }
}

/// FNV-1a over bytes. Unlike `DefaultHasher`, this is stable across processes,
/// Rust versions, and machines, so a snapshot tag survives a resumed session.
fn hash64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Short, stable content hash shown next to one `read` line in hash mode.
pub fn line_hash(line: &str) -> String {
    format!("{:08x}", hash64(line.as_bytes()) as u32)
}

/// Short, stable tag for the exact bytes a `read` returned.
pub fn snapshot_hash(bytes: &[u8]) -> String {
    format!("{:012x}", hash64(bytes) & 0x0000_ffff_ffff_ffff)
}

/// Render `[path#snapshot]` plus fresh hashes for the requested 1-based lines.
/// `patch` returns this when an anchor is stale and after a successful hash
/// edit, so the model can retry without re-reading the whole file.
pub fn anchor_block(display: &str, content: &str, requested: &[usize]) -> String {
    let snapshot = snapshot_hash(content.as_bytes());
    let lines = split_lines(content);
    let mut wanted: Vec<usize> = requested
        .iter()
        .copied()
        .filter(|number| *number >= 1 && *number <= lines.len())
        .collect();
    wanted.sort_unstable();
    wanted.dedup();

    let mut text = format!("[{display}#{snapshot}]");
    if lines.is_empty() {
        text.push_str("\n(0 lines)");
        return text;
    }
    let width = wanted
        .iter()
        .max()
        .copied()
        .unwrap_or(lines.len())
        .max(1)
        .to_string()
        .len();
    for number in wanted {
        text.push_str(&format!(
            "\n{:>width$}  {}  {}",
            number,
            line_hash(lines[number - 1]),
            lines[number - 1],
            width = width
        ));
    }
    text
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
mod image_tests {
    use super::*;
    use crate::tools::write::test_context;

    #[tokio::test]
    async fn vision_models_get_the_image_and_others_a_clear_refusal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("ui.png"),
            crate::media::image::fixtures::png_with_metadata(),
        )
        .unwrap();
        let mut ctx = test_context(dir.path());
        let refused = ReadTool.call(&ctx, json!({"path": "ui.png"})).await;
        assert!(refused.is_error);
        assert!(refused.content.contains("vision"), "{}", refused.content);

        ctx.vision = std::sync::Arc::new(crate::media::VisionSettings {
            native: true,
            ..Default::default()
        });
        let shown = ReadTool.call(&ctx, json!({"path": "ui.png"})).await;
        assert!(!shown.is_error, "{}", shown.content);
        assert!(shown.content.contains("PNG 3×2"), "{}", shown.content);
        let details = shown.details.unwrap();
        let id = details["image"]["id"].as_str().unwrap();
        assert!(ctx.artifacts.read_image(id).is_ok());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::write::test_context;
    use serde_json::json;

    #[tokio::test]
    async fn explicit_end_is_not_clipped_to_the_default_page() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=500).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        // Range semantics, not the result budget, are under test here.
        let ctx = wide_context(dir.path());

        let result = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 100, "end": 400}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        let details = result.details.unwrap();
        assert_eq!(details["start"], 100);
        assert_eq!(details["end"], 400);
        assert_eq!(details["capped"], false);
    }

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

    fn wide_context(dir: &std::path::Path) -> ToolContext {
        let mut ctx = test_context(dir);
        ctx.caps = crate::context::ResultCaps {
            result: 16_000,
            batch: 40_000,
        };
        ctx
    }

    fn source_file(lines: usize) -> String {
        (1..=lines)
            .map(|n| format!("    let value_{n} = compute_something(argument_{n}, other_{n});\n"))
            .collect()
    }

    #[tokio::test]
    async fn default_read_fits_whole_lines_in_the_result_budget() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), source_file(481)).unwrap();
        let ctx = test_context(dir.path());

        let result = ReadTool.call(&ctx, json!({"path": "a.rs"})).await;
        assert!(!result.is_error, "{}", result.content);
        let details = result.details.unwrap();
        let last = details["end"].as_u64().unwrap() as usize;
        assert!(last > 10 && last < 150, "{last}");
        assert_eq!(details["capped"], true);
        // The loop never needs to cut it.
        assert!(result.content.chars().count() <= ctx.caps.tool_budget());
        // Only whole lines, and the trailer names exactly the lines shown.
        let (body, trailer) = result.content.split_once("\n\n[").unwrap();
        assert_eq!(body.lines().count(), last);
        for (n, line) in body.lines().enumerate() {
            assert!(line.ends_with(&format!("other_{});", n + 1)), "{line}");
        }
        assert_eq!(
            trailer,
            format!("lines 1-{last} of 481; next start={}]", last + 1)
        );
        // Coverage equals the shown range.
        let path = ctx.resolve("a.rs").unwrap();
        let reads = crate::tools::write::lock_reads(&ctx);
        let coverage = reads.coverage(&path).unwrap();
        assert_eq!(
            coverage.seen,
            vec![crate::tools::LineRange {
                start: 1,
                end: last
            }]
        );
        assert!(!coverage.result_truncated);
    }

    #[tokio::test]
    async fn a_large_window_reads_the_default_page_in_one_call() {
        let dir = tempfile::tempdir().unwrap();
        // Typical source density: about 45 characters per line.
        let body: String = (1..=481)
            .map(|n| format!("    let value_{n} = compute(input_{n});\n"))
            .collect();
        std::fs::write(dir.path().join("a.rs"), body).unwrap();
        let mut ctx = test_context(dir.path());
        ctx.caps = crate::context::ResultCaps::for_usable_prompt(
            crate::context::TokenBudget::new(65_536, 8192).usable_prompt(),
        );

        let result = ReadTool.call(&ctx, json!({"path": "a.rs"})).await;
        let details = result.details.unwrap();
        assert_eq!(details["end"], 150);
        assert!(
            result
                .content
                .ends_with("[lines 1-150 of 481; next start=151]")
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

    #[tokio::test]
    async fn partial_read_records_only_the_seen_lines() {
        use crate::tools::LineRange;

        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=400).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let ctx = test_context(dir.path());

        let result = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        assert!(!result.is_error, "{}", result.content);
        let path = ctx.resolve("a.txt").unwrap();
        let reads = crate::tools::write::lock_reads(&ctx);
        let coverage = reads.coverage(&path).unwrap();
        assert_eq!(coverage.total_lines, 400);
        assert_eq!(coverage.seen, vec![LineRange { start: 1, end: 150 }]);
        assert!(!coverage.result_truncated);
        assert!(reads.is_current(&path));
        assert!(!reads.is_fully_read(&path));
        assert_eq!(
            reads.unseen_ranges(&path),
            vec![LineRange {
                start: 151,
                end: 400
            }]
        );
    }

    #[tokio::test]
    async fn past_eof_read_records_stamp_without_coverage() {
        use crate::tools::LineRange;

        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let ctx = test_context(dir.path());

        let result = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 999}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        let path = ctx.resolve("a.txt").unwrap();
        let reads = crate::tools::write::lock_reads(&ctx);
        let coverage = reads.coverage(&path).unwrap();
        assert_eq!(coverage.total_lines, 10);
        assert!(coverage.seen.is_empty());
        assert!(reads.is_current(&path));
        assert!(!reads.is_fully_read(&path));
        assert_eq!(
            reads.unseen_ranges(&path),
            vec![LineRange { start: 1, end: 10 }]
        );
    }

    #[tokio::test]
    async fn empty_and_full_reads_are_fully_covered() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();
        // Range semantics, not the result budget, are under test here.
        let ctx = wide_context(dir.path());

        let path = ctx.resolve("empty.txt").unwrap();
        let _ = ReadTool.call(&ctx, json!({"path": "empty.txt"})).await;
        assert!(crate::tools::write::lock_reads(&ctx).is_fully_read(&path));

        let body: String = (1..=400).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let path = ctx.resolve("a.txt").unwrap();
        let _ = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 1, "end": 400}))
            .await;
        assert!(crate::tools::write::lock_reads(&ctx).is_fully_read(&path));
    }

    #[tokio::test]
    async fn adjacent_reads_merge_into_full_coverage() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (1..=400).map(|n| format!("line{n}\n")).collect();
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        // Range semantics, not the result budget, are under test here.
        let ctx = wide_context(dir.path());

        let path = ctx.resolve("a.txt").unwrap();
        let first = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 1, "end": 150}))
            .await;
        assert!(!first.is_error, "{}", first.content);
        assert!(!crate::tools::write::lock_reads(&ctx).is_fully_read(&path));

        let rest = ReadTool
            .call(&ctx, json!({"path": "a.txt", "start": 151, "end": 400}))
            .await;
        assert!(!rest.is_error, "{}", rest.content);
        assert!(crate::tools::write::lock_reads(&ctx).is_fully_read(&path));
    }
}

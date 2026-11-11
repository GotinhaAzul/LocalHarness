//! `edit` tool.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{Value, json};
use similar::{ChangeTag, DiffTag, TextDiff};

use crate::tools::read::split_lines;
use crate::tools::write::{atomic_write, file_mode, lock_reads, record_own_write};
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, optional_bool, required_str,
};

/// Files larger than this are not rewritten by exact-text edits.
pub(crate) const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_DIFF_LINES: usize = 200;
/// Diagnostic windows never show more than this many lines.
const MAX_WINDOW_LINES: usize = 20;
const BINARY_SNIFF: usize = 8 * 1024;

pub struct EditTool;

impl EditTool {}

#[async_trait]
impl Tool for EditTool {
    fn name(&self) -> &'static str {
        "edit"
    }

    fn description(&self) -> &'static str {
        "Replace exact text in an existing file (read it first). `old` must match once unless replace_all is true."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "path": {"type": "string", "description": "File to edit, relative to the working directory."},
                "old": {"type": "string", "description": "Exact text to replace; must appear verbatim and uniquely."},
                "new": {"type": "string", "description": "Replacement text; empty deletes the match."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence of `old` (default false)."}
            }),
            &["path", "old", "new"],
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
    let old = required_str(args, "old")?;
    let new = args
        .get("new")
        .and_then(|value| value.as_str())
        .ok_or_else(|| {
            ToolResult::err(
                "invalid_args",
                "missing required argument \"new\"; pass \"\" to delete the matched text",
            )
        })?;
    let replace_all = optional_bool(args, "replace_all").unwrap_or(false);

    if old == new {
        return Err(ToolResult::err(
            "NO_CHANGE",
            "`old` and `new` are identical; nothing to replace",
        ));
    }

    let path = ctx.resolve(&raw)?;
    let display = display_path(&ctx.cwd, &path);
    let before = read_text(&path, &display)?;
    require_fresh(ctx, &path, &display)?;

    let Some(located) = locate(&before, &old, new) else {
        return Err(ToolResult::err(
            "NO_MATCH",
            nearest_window(&display, &before, &old),
        ));
    };
    let (old, new) = (located.needle, located.replacement.as_str());
    let count = before.match_indices(&old).count();
    if count > 1 && !replace_all {
        let lines: Vec<usize> = before
            .match_indices(&old)
            .map(|(index, _)| line_of(&before, index))
            .collect();
        let shown: Vec<String> = lines.iter().take(10).map(|line| line.to_string()).collect();
        let extra = lines.len().saturating_sub(shown.len());
        let more = if extra > 0 {
            format!(", and {extra} more")
        } else {
            String::new()
        };
        return Err(ToolResult::err(
            "MULTI_MATCH",
            format!(
                "`old` matches {count} times (lines {}{more}). Add surrounding context so it matches once, or pass \"replace_all\": true to replace every occurrence.",
                shown.join(", ")
            ),
        ));
    }

    let after = if replace_all {
        before.replace(&old, new)
    } else {
        before.replacen(&old, new, 1)
    };
    let replacements = if replace_all { count } else { 1 };
    let (diff, added, removed) = diff_report(&before, &after, MAX_DIFF_LINES);

    let change = crate::tools::undo::record_change(ctx, Ok(Some(&before)), &after);
    atomic_write(&path, after.as_bytes(), file_mode(&path))
        .map_err(|err| ToolResult::err("EDIT_FAILED", format!("cannot write {display}: {err}")))?;
    record_own_write(ctx, &path);

    let plural = if replacements == 1 { "" } else { "s" };
    let how = located
        .how
        .map(|how| format!("; {how}"))
        .unwrap_or_default();
    let text = format!(
        "edited {display} ({replacements} replacement{plural}, +{added} -{removed}{how})\n{diff}"
    );
    Ok(
        ToolResult::ok(text).with_details(crate::tools::undo::merge_details(
            json!({
                "path": display,
                "added": added,
                "removed": removed,
                "replacements": replacements,
            }),
            change,
        )),
    )
}

fn line_of(content: &str, index: usize) -> usize {
    1 + content[..index]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
}

/// Where `needle` sits in a file and what to put in its place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    /// The needle as it appears verbatim in the file.
    pub needle: String,
    /// The replacement, adjusted the same way as the needle.
    pub replacement: String,
    /// Which normalization made it match; `None` for an exact match.
    pub how: Option<&'static str>,
}

/// Find `needle` in `content`, exactly or after one lossless normalization.
///
/// Local models cannot reproduce text they never saw (`read` strips `\r`),
/// and often paste `read`'s line-number gutter or drop trailing spaces. Each
/// fallback runs only when the exact text is absent, and the trailing-space
/// fallback only when exactly one whole-line window fits. Indentation is never
/// normalized: it is meaningful in Python, YAML, and Makefiles.
pub(crate) fn locate(content: &str, needle: &str, replacement: &str) -> Option<Located> {
    let found = |needle: &str| !needle.is_empty() && content.contains(needle);
    if found(needle) {
        return Some(Located {
            needle: needle.to_string(),
            replacement: replacement.to_string(),
            how: None,
        });
    }
    let crlf = content.contains("\r\n");
    let to_crlf = |text: &str| text.replace("\r\n", "\n").replace('\n', "\r\n");
    if crlf && needle.contains('\n') && found(&to_crlf(needle)) {
        return Some(Located {
            needle: to_crlf(needle),
            replacement: to_crlf(replacement),
            how: Some("matched after converting line endings to CRLF"),
        });
    }
    if let Some(stripped) = strip_gutter(needle) {
        let replacement = strip_gutter(replacement).unwrap_or_else(|| replacement.to_string());
        let how = Some("matched after removing line-number prefixes copied from read");
        if found(&stripped) {
            return Some(Located {
                needle: stripped,
                replacement,
                how,
            });
        }
        if crlf && found(&to_crlf(&stripped)) {
            return Some(Located {
                needle: to_crlf(&stripped),
                replacement: to_crlf(&replacement),
                how,
            });
        }
    }
    let window = unique_trailing_space_window(content, needle)?;
    Some(Located {
        needle: window,
        replacement: if crlf {
            to_crlf(replacement)
        } else {
            replacement.to_string()
        },
        how: Some("matched after ignoring trailing whitespace"),
    })
}

/// Remove a `read`-style line-number gutter (`  12  text`, `12\ttext`,
/// `12→text`, `12: text`, `12| text`) when every non-blank line carries one.
fn strip_gutter(text: &str) -> Option<String> {
    let mut out = Vec::new();
    let mut prefixed = 0usize;
    for line in text.split('\n') {
        if line.trim().is_empty() {
            out.push(line);
            continue;
        }
        let start = line.len() - line.trim_start().len();
        let digits = line[start..]
            .chars()
            .take_while(char::is_ascii_digit)
            .count();
        if digits == 0 {
            return None;
        }
        let rest = &line[start + digits..];
        let body = ["  ", "\t", "→", ": ", "| ", ":", "|"]
            .iter()
            .find_map(|sep| rest.strip_prefix(sep))?;
        out.push(body);
        prefixed += 1;
    }
    (prefixed > 0).then(|| out.join("\n"))
}

/// The single run of whole file lines equal to `needle`'s lines once trailing
/// whitespace is ignored, returned verbatim from the file.
fn unique_trailing_space_window(content: &str, needle: &str) -> Option<String> {
    let wanted: Vec<&str> = split_lines(needle).iter().map(|l| l.trim_end()).collect();
    if wanted.is_empty() || wanted.iter().all(|line| line.is_empty()) {
        return None;
    }
    // Byte offsets of each line start, plus where each line's text ends.
    let mut spans = Vec::new();
    let mut offset = 0;
    for raw in content.split_inclusive('\n') {
        let text = raw.trim_end_matches('\n').trim_end_matches('\r');
        spans.push((offset, offset + text.len(), offset + raw.len()));
        offset += raw.len();
    }
    let mut hit = None;
    for start in 0..spans.len().saturating_sub(wanted.len() - 1) {
        let matches = wanted.iter().enumerate().all(|(i, line)| {
            let (from, to, _) = spans[start + i];
            content[from..to].trim_end() == *line
        });
        if matches {
            if hit.is_some() {
                return None;
            }
            hit = Some(start);
        }
    }
    let start = hit?;
    let (from, _, _) = spans[start];
    let (_, text_end, line_end) = spans[start + wanted.len() - 1];
    let end = if needle.ends_with('\n') {
        line_end
    } else {
        text_end
    };
    Some(content[from..end].to_string())
}

/// Enforce read-before-edit.
pub(crate) fn require_fresh(
    ctx: &ToolContext,
    path: &Path,
    display: &str,
) -> Result<(), ToolResult> {
    if lock_reads(ctx).is_current(path) {
        Ok(())
    } else {
        Err(ToolResult::err(
            "READ_FIRST",
            format!("read {display} first"),
        ))
    }
}

/// Read a UTF-8 text file within the edit size cap.
pub(crate) fn read_text(path: &Path, display: &str) -> Result<String, ToolResult> {
    let meta = std::fs::metadata(path).map_err(|err| {
        ToolResult::err(
            "READ_FAILED",
            format!("cannot stat {display}: {err}. Use write to create it."),
        )
    })?;
    if meta.is_dir() {
        return Err(ToolResult::err(
            "READ_FAILED",
            format!("{display} is a directory; pass a file path"),
        ));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(ToolResult::err(
            "FILE_TOO_LARGE",
            format!(
                "{display} is {} bytes; edit and patch cap at 256 KB. Split the change or use bash.",
                meta.len()
            ),
        ));
    }
    let bytes = std::fs::read(path)
        .map_err(|err| ToolResult::err("READ_FAILED", format!("cannot read {display}: {err}")))?;
    if bytes[..bytes.len().min(BINARY_SNIFF)].contains(&0) {
        return Err(ToolResult::err(
            "BINARY",
            format!("{display} looks binary; edit only rewrites UTF-8 text files"),
        ));
    }
    String::from_utf8(bytes).map_err(|_| {
        ToolResult::err(
            "NOT_UTF8",
            format!("{display} is not valid UTF-8; edit only rewrites UTF-8 text files"),
        )
    })
}

/// Render a capped unified diff plus line-level add/remove counts.
pub(crate) fn diff_report(before: &str, after: &str, max_lines: usize) -> (String, usize, usize) {
    let diff = TextDiff::from_lines(before, after);
    let mut added = 0usize;
    let mut removed = 0usize;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }

    let mut unified = diff.unified_diff();
    let full = unified
        .context_radius(2)
        .header("before", "after")
        .to_string();
    let mut lines: Vec<&str> = full.lines().collect();
    let truncated = lines.len() > max_lines;
    if truncated {
        lines.truncate(max_lines);
    }
    let mut text = lines.join("\n");
    if truncated {
        text.push_str(&format!(
            "\n[... diff truncated at {max_lines} lines; re-read the file to inspect the rest]"
        ));
    }
    (text, added, removed)
}

/// Explain why `needle` did not match, showing the closest text from the file.
pub(crate) fn nearest_window(display: &str, content: &str, needle: &str) -> String {
    let file_lines = split_lines(content);
    let needle_lines = split_lines(needle);
    let width = needle_lines.len();

    if width == 0 {
        return format!(
            "{display} has no match and `old` is empty; pass the exact text to replace."
        );
    }
    if file_lines.is_empty() {
        return format!("{display} is empty, so `old` appears nowhere. Use write to add content.");
    }

    let file_norm: Vec<Vec<&str>> = file_lines
        .iter()
        .map(|line| line.split_whitespace().collect())
        .collect();
    let needle_norm: Vec<Vec<&str>> = needle_lines
        .iter()
        .map(|line| line.split_whitespace().collect())
        .collect();

    let windows = file_lines.len().saturating_sub(width) + 1;
    let mut best_score = 0usize;
    let mut best_start = 0usize;
    let mut whitespace_start: Option<usize> = None;

    for start in 0..windows {
        let span = width.min(file_lines.len() - start);
        let mut score = 0usize;
        let mut normalized_equal = span == width;
        for offset in 0..span {
            if file_lines[start + offset] == needle_lines[offset] {
                score += 1;
            }
            if file_norm[start + offset] != needle_norm[offset] {
                normalized_equal = false;
            }
        }
        if normalized_equal && whitespace_start.is_none() {
            whitespace_start = Some(start);
        }
        if score > best_score {
            best_score = score;
            best_start = start;
        }
    }

    if let Some(start) = whitespace_start {
        let mut message = format!(
            "no exact match in {display}; the closest window differs only in whitespace or line endings (lines {}-{}):\n",
            start + 1,
            start + width
        );
        message.push_str(&numbered_window(
            &file_lines,
            start,
            width,
            MAX_WINDOW_LINES,
        ));
        message.push_str("Visible whitespace (space is `·`, tab is `→`):\n");
        for offset in 0..width.min(MAX_WINDOW_LINES) {
            let visible: String = file_lines[start + offset]
                .chars()
                .map(|ch| match ch {
                    ' ' => '·',
                    '\t' => '→',
                    other => other,
                })
                .collect();
            message.push_str(&format!("{:>4}  {visible}\n", start + offset + 1));
        }
        message.push_str("Pass the file text verbatim, including indentation.");
        return message;
    }

    if best_score > 0 {
        let span = width.min(file_lines.len() - best_start);
        let mut message = format!(
            "no exact match in {display}. Closest window (lines {}-{}, {best_score}/{width} lines identical):\n",
            best_start + 1,
            best_start + span
        );
        message.push_str(&numbered_window(
            &file_lines,
            best_start,
            span,
            MAX_WINDOW_LINES,
        ));
        let mut first_difference: Option<usize> = None;
        for offset in 0..span {
            if file_lines[best_start + offset] != needle_lines[offset] {
                first_difference = Some(offset);
                break;
            }
        }
        if let Some(offset) = first_difference {
            message.push_str(&format!(
                "\nFirst differing line {}:\n  expected: {}\n  found:    {}\n",
                best_start + offset + 1,
                needle_lines[offset],
                file_lines[best_start + offset]
            ));
        }
        message.push_str(
            "Copy the file text verbatim; indentation and whitespace must match exactly.",
        );
        return message;
    }

    // Nothing line-similar: locate the closest region with `similar`.
    let diff = TextDiff::from_lines(needle, content);
    let mut closest: Option<(usize, usize, usize, bool)> = None;
    for op in diff.ops() {
        let range = op.new_range();
        let len = range.len();
        if len == 0 {
            continue;
        }
        let equal = op.tag() == DiffTag::Equal;
        let better = match closest {
            None => true,
            Some((best, _, _, best_equal)) => len > best || (len == best && equal && !best_equal),
        };
        if better {
            closest = Some((len, range.start, range.end, equal));
        }
    }

    match closest {
        Some((_, start, end, equal)) => {
            let lo = start.saturating_sub(2);
            let hi = (end + 2).min(file_lines.len());
            let mut message = format!(
                "no exact match in {display}. Closest window (lines {}-{}):\n",
                lo + 1,
                hi
            );
            message.push_str(&numbered_window(&file_lines, lo, hi - lo, MAX_WINDOW_LINES));
            if equal {
                message.push_str("These lines are shared with `old`, but `old` spans more text.\n");
            }
            message.push_str(&format!(
                "First line of `old`:\n  expected: {}\n  found:    {}\n",
                needle_lines[0], file_lines[lo]
            ));
            message.push_str("Re-read the file and copy the exact text.");
            message
        }
        None => format!(
            "no match for `old` in {display} and no similar region was found. Re-read the file and copy the exact text."
        ),
    }
}

fn numbered_window(lines: &[&str], start: usize, count: usize, max: usize) -> String {
    let available = lines.len().saturating_sub(start);
    let count = count.min(available);
    let shown = count.min(max);
    let mut text = String::new();
    for offset in 0..shown {
        text.push_str(&format!(
            "{:>4}  {}\n",
            start + offset + 1,
            lines[start + offset]
        ));
    }
    if count > shown {
        text.push_str(&format!("     ... (+{} lines)\n", count - shown));
    }
    text
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

    async fn edit(ctx: &ToolContext, old: &str, new: &str) -> ToolResult {
        EditTool
            .call(ctx, json!({"path": "a.txt", "old": old, "new": new}))
            .await
    }

    fn on_disk(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap()
    }

    #[tokio::test]
    async fn crlf_files_accept_lf_text() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "a\r\nb\r\nc\r\n").await;
        let result = edit(&ctx, "a\nb", "x\ny").await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("CRLF"), "{}", result.content);
        assert_eq!(on_disk(&dir), "x\r\ny\r\nc\r\n");
    }

    #[tokio::test]
    async fn copied_line_number_gutters_are_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "fn main() {\n    let x = 1;\n}\n").await;
        let result = edit(
            &ctx,
            "1  fn main() {\n2      let x = 1;",
            "1  fn run() {\n2      let x = 2;",
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("line-number"), "{}", result.content);
        assert_eq!(on_disk(&dir), "fn run() {\n    let x = 2;\n}\n");
    }

    #[tokio::test]
    async fn trailing_whitespace_drift_matches_a_unique_window() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "alpha  \nbeta\t\ngamma\n").await;
        let result = edit(&ctx, "alpha\nbeta", "one\ntwo").await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("trailing whitespace"),
            "{}",
            result.content
        );
        assert_eq!(on_disk(&dir), "one\ntwo\ngamma\n");
    }

    #[tokio::test]
    async fn ambiguous_or_indentation_drift_is_not_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "x \ny\nx\t\ny\n").await;
        let result = edit(&ctx, "x\ny\n", "z\n").await;
        assert!(result.is_error, "two windows fit; {}", result.content);
        let ctx = prepared(&dir, "if a:\n    run()\n").await;
        let result = edit(&ctx, "if a:\n  run()", "if b:\n  run()").await;
        assert!(
            result.content.starts_with("[NO_MATCH]"),
            "{}",
            result.content
        );
        assert_eq!(on_disk(&dir), "if a:\n    run()\n");
    }

    #[tokio::test]
    async fn consecutive_edits_need_no_reread_but_external_changes_do() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "one\ntwo\n").await;
        std::fs::write(dir.path().join("b.txt"), "other\n").unwrap();
        ReadTool.call(&ctx, json!({"path": "b.txt"})).await;
        assert!(!edit(&ctx, "one", "1").await.is_error);
        assert!(!edit(&ctx, "two", "2").await.is_error);
        // Editing a.txt left b.txt's stamp intact.
        let other = EditTool
            .call(&ctx, json!({"path": "b.txt", "old": "other", "new": "x"}))
            .await;
        assert!(!other.is_error, "{}", other.content);
        std::fs::write(dir.path().join("a.txt"), "changed elsewhere\n").unwrap();
        let stale = edit(&ctx, "changed", "c").await;
        assert!(stale.content.contains("READ_FIRST"), "{}", stale.content);
    }

    #[tokio::test]
    async fn replaces_single_match() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "fn main() {\n    let x = 1;\n}\n").await;

        let result = EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "old": "    let x = 1;", "new": "    let x = 2;"}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("-    let x = 1;"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("+    let x = 2;"),
            "{}",
            result.content
        );
        let details = result.details.unwrap();
        assert_eq!(details["replacements"].as_u64(), Some(1));
        assert_eq!(details["added"].as_u64(), Some(1));
        assert_eq!(details["removed"].as_u64(), Some(1));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "fn main() {\n    let x = 2;\n}\n"
        );
    }

    #[tokio::test]
    async fn rejects_ambiguous_match_with_lines() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "alpha\nbeta\nalpha\n").await;

        let result = EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "old": "alpha", "new": "gamma"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[MULTI_MATCH]"),
            "{}",
            result.content
        );
        assert!(result.content.contains("lines 1, 3"), "{}", result.content);
        assert!(result.content.contains("replace_all"), "{}", result.content);
    }

    #[tokio::test]
    async fn replace_all_rewrites_every_match() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "x x x\n").await;

        let result = EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "old": "x", "new": "y", "replace_all": true}),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.details.unwrap()["replacements"].as_u64(), Some(3));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
            "y y y\n"
        );
    }

    #[tokio::test]
    async fn missing_match_shows_closest_window() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "fn main() {\n    let x = 1;\n}\n").await;

        let result = EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "old": "    let x = 9;", "new": "    let x = 2;"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[NO_MATCH]"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("Closest window"),
            "{}",
            result.content
        );
        assert!(result.content.contains("expected:"), "{}", result.content);
        assert!(result.content.contains("let x = 1;"), "{}", result.content);
    }

    #[tokio::test]
    async fn whitespace_only_near_match_is_called_out() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = prepared(&dir, "fn main() {\n\tlet x = 1;\n}\n").await;

        let result = EditTool
            .call(
                &ctx,
                json!({"path": "a.txt", "old": "    let x = 1;", "new": "    let x = 2;"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.content.contains("differs only in whitespace"),
            "{}",
            result.content
        );
        assert!(result.content.contains('→'), "{}", result.content);
    }

    #[tokio::test]
    async fn requires_prior_read() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
        let ctx = test_context(dir.path());

        let result = EditTool
            .call(&ctx, json!({"path": "a.txt", "old": "one", "new": "two"}))
            .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[READ_FIRST]"),
            "{}",
            result.content
        );
    }
}

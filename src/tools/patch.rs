//! `patch` tool.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::config::EditMode;
use crate::tools::edit::{diff_report, locate, nearest_window, read_text, require_fresh};
use crate::tools::read::{anchor_block, line_hash, snapshot_hash};
use crate::tools::write::{atomic_write, file_mode, lock_reads, record_own_write};
use crate::tools::{
    Tool, ToolContext, ToolPack, ToolResult, display_path, optional_u64, required_str,
};

const MAX_DIFF_LINES: usize = 200;
/// Hash mode shows fresh anchors for at most this many lines in one result.
const MAX_ANCHOR_LINES: usize = 40;

pub struct PatchTool;

impl PatchTool {}

#[async_trait]
impl Tool for PatchTool {
    fn name(&self) -> &'static str {
        "patch"
    }

    fn description(&self) -> &'static str {
        "Exact-anchor edit by default; in hash edit mode, apply line-anchored PUT/CUT/REM/MV operations."
    }

    fn schema(&self) -> Value {
        crate::tools::object_schema(
            json!({
                "path": {"type": "string", "description": "File to patch. In hash edit mode use PATH#SNAPSHOT from read."},
                "anchor": {"type": "string", "description": "Text mode: exact text to find; must appear verbatim."},
                "replacement": {"type": "string", "description": "Text mode: text that replaces the anchor; empty deletes it."},
                "occurrence": {"type": "integer", "description": "Text mode: which match to replace, 1-based (default 1)."},
                "ops": {
                    "type": "array",
                    "description": "Hash mode: operations against the snapshot and line hashes from read.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "op": {"type": "string", "enum": ["PUT", "CUT", "REM", "MV"]},
                            "line": {"type": "integer", "description": "1-based line anchor."},
                            "hash": {"type": "string", "description": "8-hex line hash from read."},
                            "count": {"type": "integer", "description": "Lines in the block (default 1)."},
                            "text": {"type": "string", "description": "PUT: replacement lines; empty deletes."},
                            "to": {"type": "integer", "description": "MV: move the block after this 1-based line."},
                            "to_hash": {"type": "string", "description": "MV: 8-hex hash of the destination line."}
                        },
                        "required": ["op", "line", "hash"],
                        "additionalProperties": false
                    }
                }
            }),
            &["path"],
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
            .map(|raw| gate_path(raw, ctx.edit_mode).to_string());
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
    if ctx.edit_mode == EditMode::Hash
        && (args.get("ops").is_some() || split_snapshot(&raw).is_some())
    {
        return run_hash(ctx, args, &raw);
    }
    if args.get("ops").is_some() {
        return Err(ToolResult::err(
            "invalid_args",
            "`ops` needs hash edit mode; set edit_mode = \"hash\" in the active model profile, or pass anchor and replacement for an exact-text patch.",
        ));
    }
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

/// Split `path#snapshot`, accepting only the 12-hex tag `read` emits. In text
/// mode this is never consulted, so a filename containing `#` is unchanged.
fn split_snapshot(raw: &str) -> Option<(&str, &str)> {
    let (path, tag) = raw.rsplit_once('#')?;
    if path.is_empty() || tag.len() != 12 || !tag.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some((path, tag))
}

/// The filesystem path the LSP gate snapshots. In hash mode the tag is
/// metadata, not part of the filename.
fn gate_path(raw: &str, mode: EditMode) -> &str {
    match mode {
        EditMode::Hash => split_snapshot(raw).map(|(path, _)| path).unwrap_or(raw),
        EditMode::Text => raw,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HashOpKind {
    Put,
    Cut,
    Rem,
    Mv,
}

#[derive(Debug, Clone)]
struct HashOp {
    kind: HashOpKind,
    /// 1-based line where the block starts.
    line: usize,
    /// 8-hex content hash of `line`, as printed by `read`.
    hash: String,
    /// Number of lines in the block, at least 1.
    count: usize,
    /// PUT replacement text.
    text: Option<String>,
    /// MV destination line; the block moves immediately after it.
    to: Option<usize>,
    /// MV destination line hash.
    to_hash: Option<String>,
}

fn invalid_op(index: usize, message: impl std::fmt::Display) -> ToolResult {
    ToolResult::err(
        "invalid_args",
        format!(
            "ops[{index}]: {message}. Each operation needs op, line, and an 8-hex hash copied from read; PUT also needs text, MV also needs to and to_hash."
        ),
    )
}

fn valid_line_hash(hash: &str) -> bool {
    hash.len() == 8 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn parse_hash_ops(args: &Value) -> Result<Vec<HashOp>, ToolResult> {
    let value = args.get("ops").ok_or_else(|| {
        ToolResult::err(
            "invalid_args",
            "hash edit mode needs `ops`; read the file, then pass path=\"file#snapshot\" and an ops array.",
        )
    })?;
    let items = value.as_array().ok_or_else(|| {
        ToolResult::err(
            "invalid_args",
            "`ops` must be an array of operations; got a non-array value.",
        )
    })?;
    if items.is_empty() {
        return Err(ToolResult::err(
            "invalid_args",
            "`ops` is empty; pass at least one PUT, CUT, REM, or MV operation.",
        ));
    }

    let mut ops = Vec::with_capacity(items.len());
    for (position, item) in items.iter().enumerate() {
        let index = position + 1;
        let object = item.as_object().ok_or_else(|| {
            ToolResult::err(
                "invalid_args",
                format!("ops[{index}] must be an object, not {item}; pass a JSON object."),
            )
        })?;
        let name = object
            .get("op")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_op(index, "missing string `op`"))?;
        let kind = match name.trim().to_ascii_uppercase().as_str() {
            "PUT" => HashOpKind::Put,
            "CUT" => HashOpKind::Cut,
            "REM" => HashOpKind::Rem,
            "MV" => HashOpKind::Mv,
            other => {
                return Err(ToolResult::err(
                    "invalid_args",
                    format!("ops[{index}].op is {other:?}; use PUT, CUT, REM, or MV."),
                ));
            }
        };
        let line = object
            .get("line")
            .and_then(Value::as_u64)
            .filter(|line| *line > 0)
            .ok_or_else(|| invalid_op(index, "`line` must be a 1-based integer"))?;
        let hash = object
            .get("hash")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !valid_line_hash(&hash) {
            return Err(invalid_op(
                index,
                "`hash` must be the 8-hex content hash printed before that line by read",
            ));
        }
        let count = object.get("count").and_then(Value::as_u64).unwrap_or(1);
        if count == 0 {
            return Err(invalid_op(
                index,
                "`count` must be at least 1; omit it for a single line",
            ));
        }
        let text = object
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_string);
        if kind == HashOpKind::Put && text.is_none() {
            return Err(invalid_op(
                index,
                "PUT needs a string `text`; pass \"\" to delete the range",
            ));
        }
        let to = object.get("to").and_then(Value::as_u64);
        let to_hash = object
            .get("to_hash")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase);
        if kind == HashOpKind::Mv {
            if to.is_none_or(|to| to == 0) {
                return Err(invalid_op(
                    index,
                    "MV needs a 1-based integer `to` destination line",
                ));
            }
            if to_hash.as_deref().is_none_or(|hash| !valid_line_hash(hash)) {
                return Err(invalid_op(
                    index,
                    "MV needs `to_hash`, the 8-hex hash of the destination line",
                ));
            }
        }
        if kind == HashOpKind::Rem && count != 1 {
            return Err(invalid_op(
                index,
                "REM removes exactly one line; use CUT with count for a range",
            ));
        }

        ops.push(HashOp {
            kind,
            line: usize::try_from(line).unwrap_or(usize::MAX),
            hash,
            count: usize::try_from(count).unwrap_or(usize::MAX),
            text,
            to: to.map(|to| usize::try_from(to).unwrap_or(usize::MAX)),
            to_hash,
        });
    }
    Ok(ops)
}

fn requested_lines(ops: &[HashOp]) -> Vec<usize> {
    let mut lines = Vec::new();
    for op in ops {
        lines.push(op.line);
        if let Some(to) = op.to {
            lines.push(to);
        }
    }
    lines.sort_unstable();
    lines.dedup();
    lines.truncate(MAX_ANCHOR_LINES);
    lines
}

/// One physical line with its original ending, so a hash patch preserves
/// CRLF, LF, and a missing final newline without rewriting the whole file.
#[derive(Debug, Clone)]
struct FileLine {
    text: String,
    ending: String,
}

fn parse_file_lines(content: &str) -> Vec<FileLine> {
    let mut lines = Vec::new();
    for raw in content.split_inclusive('\n') {
        let (text, ending) = match raw.strip_suffix('\n') {
            Some(body) => match body.strip_suffix('\r') {
                Some(body) => (body, "\r\n"),
                None => (body, "\n"),
            },
            None => (raw.strip_suffix('\r').unwrap_or(raw), ""),
        };
        lines.push(FileLine {
            text: text.to_string(),
            ending: ending.to_string(),
        });
    }
    lines
}

/// Split PUT text into lines. A trailing newline does not create an extra
/// blank line; explicit blank lines in the middle are kept.
fn replacement_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<String> = normalized.split('\n').map(str::to_string).collect();
    if normalized.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn stale_anchor(
    display: &str,
    content: &str,
    requested: &[usize],
    message: impl std::fmt::Display,
) -> ToolResult {
    let mut requested = requested.to_vec();
    requested.sort_unstable();
    requested.dedup();
    requested.truncate(MAX_ANCHOR_LINES);
    let snapshot = snapshot_hash(content.as_bytes());
    let fresh = anchor_block(display, content, &requested);
    ToolResult::err(
        "STALE_ANCHOR",
        format!(
            "{message} Nothing was written.\nFresh anchors; retry with this snapshot and these line hashes:\n{fresh}"
        ),
    )
    .with_details(json!({
        "code": "STALE_ANCHOR",
        "snapshot": snapshot,
        "stale_lines": requested,
        "written": false,
    }))
}

fn run_hash(ctx: &ToolContext, args: &Value, raw: &str) -> Result<ToolResult, ToolResult> {
    let (file, tag) = split_snapshot(raw).ok_or_else(|| {
        ToolResult::err(
            "invalid_args",
            "hash edit mode needs path=\"FILE#SNAPSHOT\"; copy the bracketed header from read, for example \"src/main.rs#0123456789ab\".",
        )
    })?;
    let ops = parse_hash_ops(args)?;
    let path = ctx.resolve(file)?;
    let display = display_path(&ctx.cwd, &path);
    if lock_reads(ctx).coverage(&path).is_none() {
        return Err(ToolResult::err(
            "READ_FIRST",
            format!(
                "read {display} first; hash mode needs the snapshot and line hashes from a read in this session."
            ),
        ));
    }
    let before = match read_text(&path, &display) {
        Ok(before) => before,
        Err(_) if !path.exists() => {
            return Err(ToolResult::err(
                "STALE_ANCHOR",
                format!(
                    "{display} no longer exists, so snapshot {tag} is stale and nothing was written. Recreate it with write, or re-read the current file."
                ),
            ));
        }
        Err(error) => return Err(error),
    };
    let current = snapshot_hash(before.as_bytes());
    if !tag.eq_ignore_ascii_case(&current) {
        return Err(stale_anchor(
            &display,
            &before,
            &requested_lines(&ops),
            format!(
                "snapshot {tag} does not match {display} (current {current}); the file changed since it was read."
            ),
        ));
    }

    let lines = parse_file_lines(&before);
    let mut stale = Vec::new();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for op in &ops {
        let start = op.line.saturating_sub(1);
        let end = start.saturating_add(op.count);
        if start >= lines.len() || end > lines.len() {
            stale.push(op.line);
            continue;
        }
        if !line_hash(&lines[start].text).eq_ignore_ascii_case(&op.hash) {
            stale.push(op.line);
        }
        if let Some(to) = op.to {
            let target = to.saturating_sub(1);
            let fresh = target < lines.len()
                && op
                    .to_hash
                    .as_deref()
                    .is_some_and(|hash| line_hash(&lines[target].text).eq_ignore_ascii_case(hash));
            if !fresh {
                stale.push(to);
            }
        }
        ranges.push((start, end));
    }
    stale.sort_unstable();
    stale.dedup();
    if !stale.is_empty() {
        let shown: Vec<String> = stale.iter().take(8).map(usize::to_string).collect();
        let more = stale.len().saturating_sub(shown.len());
        let more = if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        };
        return Err(stale_anchor(
            &display,
            &before,
            &stale,
            format!(
                "line anchor(s) {}{more} do not match the current file.",
                shown.join(", ")
            ),
        ));
    }

    let mut sorted = ranges.clone();
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(ToolResult::err(
                "invalid_args",
                format!(
                    "two operations overlap at line {}; nothing was written. Split them into separate patch calls.",
                    pair[1].0 + 1
                ),
            ));
        }
    }
    for op in &ops {
        if let Some(to) = op.to {
            let target = to.saturating_sub(1);
            if ranges
                .iter()
                .any(|(start, end)| target >= *start && target < *end)
            {
                return Err(ToolResult::err(
                    "invalid_args",
                    format!(
                        "MV destination line {to} is inside an operation's range; choose an unchanged line outside every PUT/CUT/REM/MV range."
                    ),
                ));
            }
        }
    }

    let (after, changed) = apply_hash_ops(&before, &lines, &ops);
    if after == before {
        return Err(ToolResult::err(
            "NO_CHANGE",
            "the operations leave the file byte-identical; nothing was written.",
        ));
    }
    let (diff, added, removed) = diff_report(&before, &after, MAX_DIFF_LINES);
    let change = crate::tools::undo::record_change(ctx, Ok(Some(&before)), &after);
    atomic_write(&path, after.as_bytes(), file_mode(&path))
        .map_err(|err| ToolResult::err("PATCH_FAILED", format!("cannot write {display}: {err}")))?;
    record_own_write(ctx, &path);

    let new_snapshot = snapshot_hash(after.as_bytes());
    let mut anchors = changed;
    anchors.truncate(MAX_ANCHOR_LINES);
    if anchors.is_empty() {
        anchors.push(1);
    }
    let fresh = anchor_block(&display, &after, &anchors);
    let plural = if ops.len() == 1 { "" } else { "s" };
    let text = format!(
        "patched {display} (hash {} operation{plural}, +{added} -{removed})\n{diff}\n\nfresh anchors (snapshot {new_snapshot}):\n{fresh}",
        ops.len()
    );
    Ok(
        ToolResult::ok(text).with_details(crate::tools::undo::merge_details(
            json!({
                "path": display,
                "added": added,
                "removed": removed,
                "replacements": ops.len(),
                "ops": ops.len(),
                "anchor_snapshot": new_snapshot,
            }),
            change,
        )),
    )
}

/// Apply validated operations. Line numbers are original coordinates; output
/// preserves untouched lines byte-for-byte, including their line endings.
fn apply_hash_ops(before: &str, lines: &[FileLine], ops: &[HashOp]) -> (String, Vec<usize>) {
    let style = if before.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let keep_final_newline = before.ends_with('\n');
    let mut replacements: BTreeMap<usize, (usize, Vec<String>)> = BTreeMap::new();
    let mut moved_ranges: Vec<(usize, usize)> = Vec::new();
    let mut moves: BTreeMap<usize, Vec<Vec<String>>> = BTreeMap::new();
    for op in ops {
        let start = op.line.saturating_sub(1);
        let end = start.saturating_add(op.count);
        match op.kind {
            HashOpKind::Put => {
                let replacement = replacement_lines(op.text.as_deref().unwrap_or(""));
                replacements.insert(start, (op.count, replacement));
            }
            HashOpKind::Cut | HashOpKind::Rem => {
                replacements.insert(start, (op.count, Vec::new()));
            }
            HashOpKind::Mv => {
                let block: Vec<String> = lines[start..end]
                    .iter()
                    .map(|line| line.text.clone())
                    .collect();
                moves
                    .entry(op.to.expect("MV destination was validated") - 1)
                    .or_default()
                    .push(block);
                moved_ranges.push((start, end));
            }
        }
    }

    let mut out: Vec<FileLine> = Vec::new();
    let mut changed: Vec<usize> = Vec::new();
    let mut index = 0usize;
    while index < lines.len() {
        if let Some((count, replacement)) = replacements.get(&index) {
            if replacement.is_empty() {
                // Leave fresh anchors for the line that now occupies this
                // position and (when present) the line before it.
                if !out.is_empty() {
                    changed.push(out.len());
                }
                changed.push(out.len() + 1);
            } else {
                for text in replacement {
                    out.push(FileLine {
                        text: text.clone(),
                        ending: style.to_string(),
                    });
                    changed.push(out.len());
                }
            }
            index += *count;
            continue;
        }
        if let Some((_, end)) = moved_ranges
            .iter()
            .find(|(start, end)| index >= *start && index < *end)
        {
            index = *end;
            continue;
        }
        out.push(FileLine {
            text: lines[index].text.clone(),
            ending: lines[index].ending.clone(),
        });
        if let Some(blocks) = moves.get(&index) {
            for block in blocks {
                for text in block {
                    out.push(FileLine {
                        text: text.clone(),
                        ending: style.to_string(),
                    });
                    changed.push(out.len());
                }
            }
        }
        index += 1;
    }

    let last = out.len().saturating_sub(1);
    for (position, line) in out.iter_mut().enumerate() {
        if position == last && !keep_final_newline {
            line.ending.clear();
        } else if line.ending.is_empty() {
            line.ending = style.to_string();
        }
    }
    let after: String = out
        .iter()
        .map(|line| format!("{}{}", line.text, line.ending))
        .collect();
    (after, changed)
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

    async fn hash_ready(dir: &tempfile::TempDir, body: &str) -> (ToolContext, String) {
        std::fs::write(dir.path().join("a.txt"), body).unwrap();
        let mut ctx = test_context(dir.path());
        ctx.edit_mode = crate::config::EditMode::Hash;
        let read = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        assert!(!read.is_error, "{}", read.content);
        (ctx, read.content)
    }

    /// Parse `read`'s hash-mode output into the snapshot and per-line hashes.
    fn annotations(content: &str) -> (String, std::collections::BTreeMap<usize, String>) {
        let mut lines = content.lines();
        let header = lines.next().expect("snapshot header");
        assert!(header.starts_with('['), "{header}");
        let snapshot = header
            .trim_start_matches('[')
            .trim_end_matches(']')
            .rsplit_once('#')
            .expect("snapshot tag")
            .1
            .to_string();
        let mut hashes = std::collections::BTreeMap::new();
        for line in lines {
            if line.trim().is_empty() || line.starts_with('[') {
                continue;
            }
            let mut parts = line.splitn(3, "  ");
            let number = parts
                .next()
                .expect("line number")
                .trim()
                .parse::<usize>()
                .expect("numeric line");
            let hash = parts.next().expect("line hash").trim().to_string();
            hashes.insert(number, hash);
        }
        (snapshot, hashes)
    }

    async fn hash_patch(ctx: &ToolContext, snapshot: &str, ops: Value) -> ToolResult {
        PatchTool
            .call(
                ctx,
                json!({"path": format!("a.txt#{snapshot}"), "ops": ops}),
            )
            .await
    }

    fn on_disk(dir: &tempfile::TempDir) -> String {
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap()
    }

    #[test]
    fn snapshot_tags_are_stripped_only_in_hash_mode() {
        let tag = "0123456789ab";
        assert_eq!(gate_path(&format!("a.rs#{tag}"), EditMode::Hash), "a.rs");
        assert_eq!(gate_path("dir/a#b.rs", EditMode::Hash), "dir/a#b.rs");
        assert_eq!(
            gate_path("a.rs#0123456789ab", EditMode::Text),
            "a.rs#0123456789ab"
        );
    }

    #[tokio::test]
    async fn hash_mode_read_annotates_lines_and_default_read_does_not() {
        use crate::tools::read::{line_hash, snapshot_hash};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\n").unwrap();
        let ctx = test_context(dir.path());
        let plain = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        assert_eq!(plain.content, "1  alpha\n2  beta");
        assert!(
            plain.details.as_ref().unwrap().get("snapshot").is_none(),
            "text mode must not add hash details"
        );

        let mut ctx = ctx;
        ctx.edit_mode = crate::config::EditMode::Hash;
        let hashed = ReadTool.call(&ctx, json!({"path": "a.txt"})).await;
        assert!(!hashed.is_error, "{}", hashed.content);
        let (snapshot, hashes) = annotations(&hashed.content);
        assert_eq!(snapshot, snapshot_hash(b"alpha\nbeta\n"));
        assert_eq!(hashes[&1], line_hash("alpha"));
        assert_eq!(hashes[&2], line_hash("beta"));
        assert!(
            hashed.content.contains("1  ") && hashed.content.ends_with("beta"),
            "{}",
            hashed.content
        );
        let details = hashed.details.unwrap();
        assert_eq!(details["snapshot"].as_str(), Some(snapshot.as_str()));
        assert_eq!(details["edit_mode"].as_str(), Some("hash"));
    }

    #[tokio::test]
    async fn hash_put_replaces_one_line_and_returns_fresh_anchors() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "alpha\nbeta\ngamma\n").await;
        let (snapshot, hashes) = annotations(&read);

        let result = hash_patch(
            &ctx,
            &snapshot,
            json!([{"op": "PUT", "line": 2, "hash": hashes[&2], "text": "BETA\nBETA2"}]),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(on_disk(&dir), "alpha\nBETA\nBETA2\ngamma\n");
        let details = result.details.unwrap();
        let new_snapshot = details["anchor_snapshot"].as_str().expect("new snapshot");
        assert_eq!(
            new_snapshot,
            snapshot_hash("alpha\nBETA\nBETA2\ngamma\n".as_bytes())
        );
        assert!(
            result
                .content
                .contains(&format!("2  {}  BETA", line_hash("BETA"))),
            "{}",
            result.content
        );

        // The new snapshot and an unchanged line hash are enough for a chain.
        let chained = hash_patch(
            &ctx,
            new_snapshot,
            json!([{"op": "CUT", "line": 4, "hash": hashes[&3]}]),
        )
        .await;
        assert!(!chained.is_error, "{}", chained.content);
        assert_eq!(on_disk(&dir), "alpha\nBETA\nBETA2\n");
    }

    #[tokio::test]
    async fn stale_snapshot_rejects_the_whole_patch_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "alpha\nbeta\n").await;
        let (snapshot, hashes) = annotations(&read);
        std::fs::write(dir.path().join("a.txt"), "alpha\nBETA\n").unwrap();

        let result = hash_patch(
            &ctx,
            &snapshot,
            json!([
                {"op": "PUT", "line": 1, "hash": hashes[&1], "text": "A"},
                {"op": "CUT", "line": 2, "hash": hashes[&2]}
            ]),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[STALE_ANCHOR]"),
            "{}",
            result.content
        );
        assert!(
            result.content.contains("Nothing was written"),
            "{}",
            result.content
        );
        assert_eq!(on_disk(&dir), "alpha\nBETA\n", "no operation may land");
        let details = result.details.expect("details");
        assert_eq!(details["written"].as_bool(), Some(false));
        assert_eq!(
            details["snapshot"].as_str(),
            Some(snapshot_hash(b"alpha\nBETA\n").as_str())
        );
        assert!(
            result
                .content
                .contains(&format!("1  {}  alpha", line_hash("alpha"))),
            "fresh anchors must be included: {}",
            result.content
        );
        assert!(
            result
                .content
                .contains(&format!("2  {}  BETA", line_hash("BETA"))),
            "fresh anchors must show the changed line: {}",
            result.content
        );
    }

    #[tokio::test]
    async fn stale_line_hash_rejects_the_whole_patch_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "alpha\nbeta\ngamma\n").await;
        let (snapshot, hashes) = annotations(&read);
        let wrong = crate::tools::read::line_hash("gamma");

        let result = hash_patch(
            &ctx,
            &snapshot,
            json!([
                {"op": "PUT", "line": 1, "hash": hashes[&1], "text": "A"},
                {"op": "CUT", "line": 2, "hash": wrong}
            ]),
        )
        .await;
        assert!(result.is_error);
        assert!(
            result.content.starts_with("[STALE_ANCHOR]"),
            "{}",
            result.content
        );
        assert_eq!(on_disk(&dir), "alpha\nbeta\ngamma\n");
        let details = result.details.expect("details");
        assert_eq!(details["stale_lines"], json!([2]));
        assert!(
            result
                .content
                .contains(&format!("2  {}  beta", line_hash("beta"))),
            "{}",
            result.content
        );

        let out_of_range = hash_patch(
            &ctx,
            &snapshot,
            json!([{"op": "PUT", "line": 99, "hash": hashes[&1], "text": "x"}]),
        )
        .await;
        assert!(out_of_range.is_error);
        assert!(
            out_of_range.content.starts_with("[STALE_ANCHOR]"),
            "{}",
            out_of_range.content
        );
        assert_eq!(on_disk(&dir), "alpha\nbeta\ngamma\n");
    }

    #[tokio::test]
    async fn hash_patch_preserves_crlf_and_missing_final_newline() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "alpha\r\nbeta\r\ngamma\r\n").await;
        let (snapshot, hashes) = annotations(&read);
        assert!(!read.contains('\r'), "read must strip CRLF: {read:?}");
        let result = hash_patch(
            &ctx,
            &snapshot,
            json!([{"op": "PUT", "line": 2, "hash": hashes[&2], "text": "BETA\nBETA2"}]),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(on_disk(&dir), "alpha\r\nBETA\r\nBETA2\r\ngamma\r\n");

        let no_final = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&no_final, "alpha\nbeta").await;
        let (snapshot, hashes) = annotations(&read);
        let result = hash_patch(
            &ctx,
            &snapshot,
            json!([{"op": "PUT", "line": 1, "hash": hashes[&1], "text": "A\nB"}]),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(on_disk(&no_final), "A\nB\nbeta");
    }

    #[tokio::test]
    async fn duplicate_lines_are_targeted_by_line_number_plus_hash() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "same\nmiddle\nsame\n").await;
        let (snapshot, hashes) = annotations(&read);
        assert_eq!(hashes[&1], hashes[&3], "duplicates share a content hash");
        let result = hash_patch(
            &ctx,
            &snapshot,
            json!([{"op": "PUT", "line": 3, "hash": hashes[&3], "text": "last"}]),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(on_disk(&dir), "same\nmiddle\nlast\n");
    }

    #[tokio::test]
    async fn whitespace_only_lines_anchor_by_content_hash() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "alpha\n   \n\t\nomega\n").await;
        let (snapshot, hashes) = annotations(&read);
        assert_eq!(hashes[&2], crate::tools::read::line_hash("   "));
        assert_eq!(hashes[&3], crate::tools::read::line_hash("\t"));
        let result = hash_patch(
            &ctx,
            &snapshot,
            json!([
                {"op": "PUT", "line": 2, "hash": hashes[&2], "text": "blank"},
                {"op": "CUT", "line": 3, "hash": hashes[&3]}
            ]),
        )
        .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(on_disk(&dir), "alpha\nblank\nomega\n");
    }

    #[tokio::test]
    async fn cut_rem_and_mv_use_original_line_coordinates() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "one\ntwo\nthree\nfour\nfive\n").await;
        let (snapshot, hashes) = annotations(&read);
        let cut = hash_patch(
            &ctx,
            &snapshot,
            json!([
                {"op": "CUT", "line": 2, "hash": hashes[&2], "count": 2},
                {"op": "REM", "line": 4, "hash": hashes[&4]}
            ]),
        )
        .await;
        assert!(!cut.is_error, "{}", cut.content);
        assert_eq!(on_disk(&dir), "one\nfive\n");

        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "one\ntwo\nthree\nfour\nfive\n").await;
        let (snapshot, hashes) = annotations(&read);
        let moved = hash_patch(
            &ctx,
            &snapshot,
            json!([{"op": "MV", "line": 2, "hash": hashes[&2], "to": 4, "to_hash": hashes[&4]}]),
        )
        .await;
        assert!(!moved.is_error, "{}", moved.content);
        assert_eq!(on_disk(&dir), "one\nthree\nfour\ntwo\nfive\n");

        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "one\ntwo\nthree\nfour\nfive\n").await;
        let (snapshot, hashes) = annotations(&read);
        let moved_back = hash_patch(
            &ctx,
            &snapshot,
            json!([{"op": "MV", "line": 4, "hash": hashes[&4], "to": 1, "to_hash": hashes[&1]}]),
        )
        .await;
        assert!(!moved_back.is_error, "{}", moved_back.content);
        assert_eq!(on_disk(&dir), "one\nfour\ntwo\nthree\nfive\n");
    }

    #[tokio::test]
    async fn invalid_hash_ops_are_rejected_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let (ctx, read) = hash_ready(&dir, "one\ntwo\nthree\n").await;
        let (snapshot, hashes) = annotations(&read);

        let overlap = hash_patch(
            &ctx,
            &snapshot,
            json!([
                {"op": "PUT", "line": 1, "hash": hashes[&1], "count": 2, "text": "x"},
                {"op": "CUT", "line": 2, "hash": hashes[&2]}
            ]),
        )
        .await;
        assert!(
            overlap.content.starts_with("[invalid_args]"),
            "{}",
            overlap.content
        );
        assert_eq!(on_disk(&dir), "one\ntwo\nthree\n");

        let bad_move = hash_patch(
            &ctx,
            &snapshot,
            json!([{"op": "MV", "line": 1, "hash": hashes[&1], "count": 2, "to": 2, "to_hash": hashes[&2]}]),
        )
        .await;
        assert!(
            bad_move.content.starts_with("[invalid_args]"),
            "{}",
            bad_move.content
        );
        assert_eq!(on_disk(&dir), "one\ntwo\nthree\n");

        let missing_snapshot = PatchTool
            .call(&ctx, json!({"path": "a.txt", "ops": [{"op": "PUT", "line": 1, "hash": hashes[&1], "text": "x"}]}))
            .await;
        assert!(
            missing_snapshot.content.starts_with("[invalid_args]"),
            "{}",
            missing_snapshot.content
        );

        let text_mode = {
            let mut text = test_context(dir.path());
            text.edit_mode = crate::config::EditMode::Text;
            let _ = ReadTool.call(&text, json!({"path": "a.txt"})).await;
            PatchTool
                .call(
                    &text,
                    json!({"path": "a.txt", "ops": [{"op": "REM", "line": 1, "hash": hashes[&1]}]}),
                )
                .await
        };
        assert!(
            text_mode.content.starts_with("[invalid_args]"),
            "{}",
            text_mode.content
        );
        assert_eq!(on_disk(&dir), "one\ntwo\nthree\n");
    }

    #[tokio::test]
    async fn fifty_case_fixture_hash_anchors_avoid_text_retries_and_corruption() {
        let mut text_retries = 0usize;
        let mut hash_retries = 0usize;
        for case in 0..50usize {
            let crlf = case % 5 == 1;
            let no_final_newline = case % 5 == 4;
            let newline = if crlf { "\r\n" } else { "\n" };
            let lines = vec![
                "prefix".to_string(),
                format!("  repeated{case}"),
                format!("  repeated{case}"),
                if case % 3 == 0 {
                    "   ".to_string()
                } else {
                    format!("  gamma{case}")
                },
                if case % 4 == 0 {
                    "\t".to_string()
                } else {
                    format!("  delta{case}")
                },
                "  omega".to_string(),
            ];
            // Targets avoid the whitespace-only decoys so the text attempt has
            // a non-empty anchor that differs only in indentation.
            let target = [2usize, 3, 5, 6][case % 4];
            let body = if no_final_newline {
                lines.join(newline)
            } else {
                format!("{}{newline}", lines.join(newline))
            };
            let mut expected_lines = lines.clone();
            expected_lines[target - 1] = format!("case{case}");
            let expected = if no_final_newline {
                expected_lines.join(newline)
            } else {
                format!("{}{newline}", expected_lines.join(newline))
            };

            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.txt"), &body).unwrap();
            let mut text = test_context(dir.path());
            text.edit_mode = crate::config::EditMode::Text;
            let read = ReadTool.call(&text, json!({"path": "a.txt"})).await;
            assert!(!read.is_error, "{}", read.content);
            // A weak model adds indentation the file does not have; the exact
            // path does not normalize leading whitespace.
            let drifted = format!(" {}", lines[target - 1]);
            assert!(!drifted.trim().is_empty());
            let failed = crate::tools::edit::EditTool
                .call(
                    &text,
                    json!({"path": "a.txt", "old": drifted, "new": format!("case{case}")}),
                )
                .await;
            assert!(
                failed.is_error,
                "case {case}: text drift unexpectedly matched: {}",
                failed.content
            );
            assert_eq!(on_disk(&dir), body, "case {case}: failed text edit wrote");
            text_retries += 1;

            let (ctx, read) = hash_ready(&dir, &body).await;
            let (snapshot, hashes) = annotations(&read);
            let result = hash_patch(
                &ctx,
                &snapshot,
                json!([{"op": "PUT", "line": target, "hash": hashes[&target], "text": format!("case{case}")}]),
            )
            .await;
            assert!(
                !result.is_error,
                "case {case}: hash anchor failed: {}",
                result.content
            );
            assert_eq!(on_disk(&dir), expected, "case {case}: corrupt write");
            if result.is_error {
                hash_retries += 1;
            }
        }
        eprintln!(
            "50-case hash edit fixture: text retries {text_retries}/50; hash retries {hash_retries}/50"
        );
        assert_eq!(text_retries, 50, "text drift must need a retry every time");
        assert_eq!(hash_retries, 0, "hash anchors must land on the first try");
    }
}

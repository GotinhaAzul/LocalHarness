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
const CHANGE_KEYS: [&str; 10] = [
    "added",
    "removed",
    "created",
    "snapshot",
    "snapshot_error",
    "after_sha256",
    "encoding",
    "undo",
    "reverts",
    "deleted",
];

pub(crate) fn content_hash(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Read a file's text and encoding for a snapshot. `Ok(None)` means it does
/// not exist; `Err` names why it cannot be snapshotted.
pub(crate) fn previous_snapshot(
    path: &std::path::Path,
) -> Result<Option<(String, crate::tools::encoding::Encoding)>, String> {
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
    match crate::tools::encoding::decode(&bytes) {
        crate::tools::encoding::Decoded::Text { text, encoding } => Ok(Some((text, encoding))),
        crate::tools::encoding::Decoded::Binary => {
            Err("the previous file was binary, not text".into())
        }
    }
}

/// Save what a file held before a change and describe the change for undo.
/// Never fails the mutation: a snapshot that cannot be taken is recorded as
/// `snapshot_error`, and undo reports that reason later.
pub(crate) fn record_change(
    ctx: &ToolContext,
    before: Result<Option<&str>, String>,
    after: &str,
) -> Map<String, Value> {
    record_change_encoded(
        ctx,
        before.map(|before| before.map(|text| (text, crate::tools::encoding::Encoding::Utf8))),
        after.as_bytes(),
        crate::tools::encoding::Encoding::Utf8,
    )
}

/// [`record_change`] for a file with a non-UTF-8 encoding: the snapshot keeps
/// the text, the payload keeps the encoding, and the hash covers the exact
/// bytes that land on disk.
pub(crate) fn record_change_encoded(
    ctx: &ToolContext,
    before: Result<Option<(&str, crate::tools::encoding::Encoding)>, String>,
    after_bytes: &[u8],
    after_encoding: crate::tools::encoding::Encoding,
) -> Map<String, Value> {
    let mut out = Map::new();
    match before {
        Ok(Some((text, encoding))) => {
            if !encoding.is_utf8() {
                out.insert("encoding".into(), json!(encoding.label()));
            }
            match ctx.artifacts.write(text) {
                Ok(artifact) => {
                    out.insert("snapshot".into(), json!(artifact.id));
                }
                Err(err) => {
                    out.insert(
                        "snapshot_error".into(),
                        json!(format!("snapshot could not be saved: {err}")),
                    );
                }
            }
        }
        Ok(None) => {
            out.insert("created".into(), json!(true));
        }
        Err(reason) => {
            out.insert("snapshot_error".into(), json!(reason));
        }
    }
    if !after_encoding.is_utf8() {
        out.insert("encoding".into(), json!(after_encoding.label()));
    }
    out.insert("after_sha256".into(), json!(content_hash(after_bytes)));
    out
}

/// Merge `record_change` output into a tool's result details.
pub(crate) fn merge_details(mut details: Value, change: Map<String, Value>) -> Value {
    if let Some(object) = details.as_object_mut() {
        object.extend(change);
    }
    details
}

/// One path a destructive bash command may change, captured before it runs.
#[derive(Debug, Clone)]
pub(crate) struct BashSnapshot {
    pub display: String,
    pub path: std::path::PathBuf,
    pub snapshot: Option<String>,
    pub snapshot_error: Option<String>,
    pub encoding: crate::tools::encoding::Encoding,
    /// The command removes this path (`rm`, `mv` source, `find -delete`).
    pub removed: bool,
    /// The path existed before the command. A missing one can only have been
    /// created by it (a redirection or an `mv` destination).
    pub existed: bool,
    pub before_sha256: String,
}

/// Bound on files captured for one destructive command.
pub(crate) const MAX_BASH_SNAPSHOTS: usize = 200;

/// Snapshot the files a destructive `bash` command will change, before it
/// runs. Parsing the plain-path cases is deliberate: `rm`, `mv`, `find
/// -delete`, shell redirection, and `truncate`. `git clean` is not parsed
/// because the ignored set is only known by running git.
pub(crate) fn bash_snapshots(ctx: &ToolContext, command: &str) -> Vec<BashSnapshot> {
    let mut targets: Vec<(std::path::PathBuf, bool)> = Vec::new();
    for (word, removed) in destructive_targets(command) {
        let Ok(path) = ctx.resolve(&word) else {
            continue;
        };
        if path.is_dir() {
            // `rm -rf dir`: snapshot the regular files below it, bounded.
            let mut stack = vec![path.clone()];
            let mut seen = 0usize;
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if targets.len() >= MAX_BASH_SNAPSHOTS {
                        break;
                    }
                    let name = entry.file_name();
                    if matches!(
                        name.to_string_lossy().as_ref(),
                        ".git" | ".hg" | ".svn" | "node_modules" | "target" | ".venv"
                    ) {
                        continue;
                    }
                    let Ok(kind) = entry.file_type() else {
                        continue;
                    };
                    if kind.is_dir() {
                        stack.push(entry.path());
                    } else if kind.is_file() {
                        targets.push((entry.path(), removed));
                        seen += 1;
                    }
                }
                if seen >= MAX_BASH_SNAPSHOTS {
                    break;
                }
            }
        } else if path.is_file() {
            targets.push((path, removed));
        } else if !removed {
            // An overwrite or `mv` destination that does not exist yet may be
            // created by the command; record it so undo can delete it.
            targets.push((path, false));
        }
    }
    targets.sort();
    targets.dedup();

    let mut snapshots = Vec::new();
    let mut total_bytes = 0u64;
    for (path, removed) in targets {
        if snapshots.len() >= MAX_BASH_SNAPSHOTS {
            break;
        }
        let existed = path.is_file();
        let meta = std::fs::metadata(&path).ok();
        let before_sha256 = if existed {
            std::fs::read(&path)
                .map(|bytes| content_hash(&bytes))
                .unwrap_or_else(|_| content_hash(b""))
        } else {
            content_hash(b"")
        };
        let mut snapshot_error = None;
        if existed
            && (meta
                .as_ref()
                .is_some_and(|meta| meta.len() > MAX_SNAPSHOT_BYTES)
                || total_bytes >= MAX_SNAPSHOT_BYTES * 8)
        {
            snapshot_error =
                Some("the file exceeded the snapshot size bound, so undo cannot restore it".into());
        }
        total_bytes += meta.map(|meta| meta.len()).unwrap_or(0);
        let display = display_path(&ctx.cwd, &path);
        let (snapshot, encoding) = if snapshot_error.is_some() || !existed {
            (None, crate::tools::encoding::Encoding::Utf8)
        } else {
            match previous_snapshot(&path) {
                Ok(Some((text, encoding))) => {
                    let snapshot = match ctx.artifacts.write(&text) {
                        Ok(artifact) => Some(artifact.id),
                        Err(err) => {
                            snapshot_error = Some(format!("snapshot could not be saved: {err}"));
                            None
                        }
                    };
                    (snapshot, encoding)
                }
                Ok(None) => (None, crate::tools::encoding::Encoding::Utf8),
                Err(reason) => {
                    snapshot_error = Some(reason);
                    (None, crate::tools::encoding::Encoding::Utf8)
                }
            }
        };
        snapshots.push(BashSnapshot {
            display,
            path,
            snapshot,
            snapshot_error,
            encoding,
            removed,
            existed,
            before_sha256,
        });
    }
    snapshots
}

/// The `details.session` entries for one bash command's snapshots. `after` is
/// read after the command ran, so `undo` and `/rewind` can prove the file
/// still matches what the command left. Files the command did not actually
/// change are left out, which keeps `find -delete` snapshots of untouched
/// files from filling the undo stack.
pub(crate) fn bash_change_entries(snapshots: &[BashSnapshot]) -> Vec<Value> {
    snapshots
        .iter()
        .filter_map(|snapshot| {
            let after = std::fs::read(&snapshot.path)
                .map(|bytes| content_hash(&bytes))
                .unwrap_or_else(|_| content_hash(b""));
            if !snapshot.existed && after == snapshot.before_sha256 {
                // The command never created it.
                return None;
            }
            if snapshot.existed && after == snapshot.before_sha256 {
                // Untouched (a `find` predicate matched only some files).
                return None;
            }
            let mut entry = json!({
                "kind": "fs-change",
                "path": snapshot.display,
                "bash": true,
                "removed": snapshot.removed,
                "after_sha256": after,
            });
            if let Some(object) = entry.as_object_mut() {
                if !snapshot.existed {
                    object.insert("created".into(), json!(true));
                }
                if let Some(id) = &snapshot.snapshot {
                    object.insert("snapshot".into(), json!(id));
                }
                if let Some(error) = &snapshot.snapshot_error {
                    object.insert("snapshot_error".into(), json!(error));
                }
                if !snapshot.encoding.is_utf8() {
                    object.insert("encoding".into(), json!(snapshot.encoding.label()));
                }
            }
            Some(entry)
        })
        .collect()
}

/// Paths a command names after a deleting or truncating program. `(path,
/// removed)`; `removed == false` means overwritten, not deleted.
fn destructive_targets(command: &str) -> Vec<(String, bool)> {
    let mut targets = Vec::new();
    for segment in shell_segments(command) {
        let words = shell_words(&segment);
        if words.is_empty() {
            continue;
        }
        // Redirection first: `> f`, `>>f`, `2>f`, `&>f`, and `word>f` all
        // name a file that may be truncated or created.
        let mut index = 0;
        while index < words.len() {
            if let Some(after) = redirect_target(&words[index]) {
                if after.is_empty() {
                    if let Some(next) = words.get(index + 1) {
                        targets.push((next.clone(), false));
                        index += 2;
                        continue;
                    }
                } else if !after.starts_with(['&', '=']) {
                    targets.push((after.to_string(), false));
                }
            }
            index += 1;
        }
        let (program, args) = program_and_args(&words);
        let Some(program) = program else {
            continue;
        };
        let paths = path_args(args);
        match program {
            "rm" | "shred" | "unlink" => {
                for word in paths {
                    targets.push((word.clone(), true));
                }
            }
            "mv" => {
                if let Some((destination, sources)) = paths.split_last() {
                    for source in sources {
                        targets.push(((*source).clone(), true));
                    }
                    targets.push(((*destination).clone(), false));
                }
            }
            "truncate" => {
                for word in paths {
                    targets.push((word.clone(), false));
                }
            }
            "find" if args.iter().any(|word| word == "-delete") => {
                for word in args
                    .iter()
                    .take_while(|word| !word.starts_with('-'))
                    .filter(|word| !word.is_empty())
                {
                    targets.push((word.clone(), true));
                }
            }
            _ => {}
        }
    }
    targets
}

/// The path named by one redirection word, or `Some("")` when the path is the
/// next word. `None` when the word has no redirection.
fn redirect_target(word: &str) -> Option<&str> {
    let position = word.find('>')?;
    let prefix = &word[..position];
    if !prefix.is_empty()
        && !prefix
            .chars()
            .all(|character| character.is_ascii_digit() || character == '&')
    {
        // `make>build.log`: everything before `>` is the command/argument.
        return Some(&word[position + 1..]);
    }
    Some(word[position..].trim_start_matches('>'))
}

/// Positional arguments, honouring `--` so `rm -- -file` still names `-file`.
fn path_args(args: &[String]) -> Vec<&String> {
    let mut paths = Vec::new();
    let mut literal = false;
    for word in args {
        if !literal && word == "--" {
            literal = true;
            continue;
        }
        if !literal && word.starts_with('-') {
            continue;
        }
        paths.push(word);
    }
    paths
}

/// Split a command into simple commands at unquoted separators.
fn shell_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars().peekable();
    while let Some(character) = chars.next() {
        match quote {
            Some(open) => {
                current.push(character);
                if character == open {
                    quote = None;
                }
            }
            None => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    current.push(character);
                }
                '&' | ';' | '\n' => {
                    segments.push(std::mem::take(&mut current));
                    if character == '&' && chars.peek() == Some(&'&') {
                        chars.next();
                    }
                }
                '|' => {
                    segments.push(std::mem::take(&mut current));
                    if chars.peek() == Some(&'|') {
                        chars.next();
                    }
                }
                _ => current.push(character),
            },
        }
    }
    if !current.trim().is_empty() {
        segments.push(current);
    }
    segments
}

/// The words of one simple command, quotes removed.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = command.chars();
    while let Some(character) = chars.next() {
        match quote {
            Some(open) => {
                if character == open {
                    quote = None;
                } else {
                    current.push(character);
                }
            }
            None => match character {
                '\'' | '"' => quote = Some(character),
                '\\' => {
                    if let Some(next) = chars.next() {
                        current.push(next);
                    }
                }
                character if character.is_whitespace() => {
                    if !current.is_empty() {
                        words.push(std::mem::take(&mut current));
                    }
                }
                character => current.push(character),
            },
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// The program and its arguments, skipping wrappers and `VAR=value` prefixes.
fn program_and_args(words: &[String]) -> (Option<&str>, &[String]) {
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        if word.contains('=') && !word.starts_with('-') {
            index += 1;
            continue;
        }
        if matches!(word, "sudo" | "doas") {
            index += 1;
            while index < words.len() && words[index].starts_with('-') {
                let flag = words[index].as_str();
                index += 1;
                if matches!(
                    flag,
                    "-u" | "-g" | "-p" | "-C" | "-t" | "-U" | "-r" | "-D" | "--user" | "--group"
                ) {
                    index += 1;
                }
            }
            continue;
        }
        if word == "env" {
            index += 1;
            while index < words.len() && words[index].starts_with('-') {
                index += 1;
            }
            continue;
        }
        if matches!(word, "time" | "command" | "nohup" | "exec") {
            index += 1;
            continue;
        }
        if word == "timeout" {
            index += 2;
            continue;
        }
        break;
    }
    let Some(program) = words.get(index) else {
        return (None, &[]);
    };
    let program = program.rsplit('/').next().unwrap_or(program);
    (Some(program), &words[index + 1..])
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
        "Revert the last edit, patch, write, or snapshotted bash change to a file; call again to step further back."
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
                "no snapshotted change to {display} on this branch to undo. A bash command is snapshotted only when it names a file with `rm`, `mv`, `find -delete`, `truncate`, or a redirection; fix other changes with edit."
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

    // Only revert what the harness (or the snapshotted bash command) itself
    // changed. A file the command deleted is compared against the hash of
    // "missing", so an undo can restore it; a file something recreated is a
    // conflict like any other.
    let current = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            if expected == content_hash(b"") {
                Vec::new()
            } else {
                return Err(ToolResult::err(
                    "UNDO_CONFLICT",
                    format!(
                        "{display} no longer exists; something deleted it after the harness wrote it. Recreate it with write if needed."
                    ),
                ));
            }
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
    let (restored, trusted) = ctx.artifacts.read_for_undo(snapshot).map_err(|err| {
        ToolResult::err(
            "SNAPSHOT_MISSING",
            format!(
                "the snapshot for {display} is gone ({err}); old session artifacts are removed by retention. Fix it with edit."
            ),
        )
    })?;
    let encoding = change
        .payload
        .get("encoding")
        .and_then(Value::as_str)
        .map(crate::tools::encoding::Encoding::from_label)
        .unwrap_or(crate::tools::encoding::Encoding::Utf8);
    let restored_bytes = crate::tools::encoding::encode(&restored, encoding).ok_or_else(|| {
        ToolResult::err(
            "UNDO_FAILED",
            format!(
                "the snapshot of {display} is {0} text that cannot be encoded back as {0}; fix it with edit",
                encoding.label()
            ),
        )
    })?;
    // A raw in-process snapshot is the exact pre-change text, even when the
    // file legitimately contained a redaction-shaped string. A copy read back
    // from disk is redacted and must still pass the placeholder guard.
    let written = if trusted {
        crate::tools::write::atomic_restore(&path, &restored_bytes, file_mode(&path))
    } else {
        atomic_write(&path, &restored_bytes, file_mode(&path))
    };
    written
        .map_err(|err| ToolResult::err("UNDO_FAILED", format!("cannot write {display}: {err}")))?;
    record_own_write(ctx, &path);
    let (diff, added, removed) = diff_report(&current, &restored, MAX_DIFF_LINES);
    let text = if change.payload.get("bash").and_then(Value::as_bool) == Some(true) {
        format!(
            "restored {display}, which a bash command had changed (+{added} -{removed}); the snapshot was taken before the command ran. {more}\n{diff}"
        )
    } else {
        format!(
            "restored {display} to its state before the last change (+{added} -{removed}); {more}\n{diff}"
        )
    };
    Ok(ToolResult::ok(text).with_details(json!({
        "path": display,
        "undo": true,
        "reverts": change.seq,
        "added": added,
        "removed": removed,
        "after_sha256": content_hash(&restored_bytes),
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

    /// Snapshots go to a persisted artifact store, where they are redacted.
    /// Undo in the same process must still restore the exact bytes, and after
    /// a restart (only the redacted file left) it must refuse.
    #[tokio::test]
    async fn undo_restores_redacted_snapshots_exactly_and_refuses_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let password = format!("pw-{:x}", std::process::id() as u64 * 7919 + 104_729);
        let original = format!("DB_PASSWORD={password}\nname = Column(Text)\n");
        std::fs::write(dir.path().join("settings.py"), &original).unwrap();
        let mut ctx = test_context(dir.path());
        let root = dir.path().join(".tot-artifacts");
        ctx.artifacts = std::sync::Arc::new(
            crate::session::ArtifactStore::new("s", Some(root.clone())).unwrap(),
        );
        ReadTool.call(&ctx, json!({"path": "settings.py"})).await;
        let edited = mutate(
            &ctx,
            &EditTool,
            json!({"path": "settings.py", "old": "Column(Text)", "new": "Column(String)"}),
        )
        .await;
        assert!(!edited.is_error, "{}", edited.content);
        let stored: String = std::fs::read_dir(&root)
            .unwrap()
            .flatten()
            .map(|entry| std::fs::read_to_string(entry.path()).unwrap_or_default())
            .collect();
        assert!(!stored.contains(&password), "snapshot kept the secret");

        // Same process: exact restore.
        let undone = mutate(&ctx, &UndoTool, json!({"path": "settings.py"})).await;
        assert!(!undone.is_error, "{}", undone.content);
        assert_eq!(on_disk(&dir, "settings.py").unwrap(), original);

        // A later process only has the redacted snapshot: undo refuses
        // rather than writing a placeholder into the file.
        let redo = mutate(
            &ctx,
            &EditTool,
            json!({"path": "settings.py", "old": "Column(Text)", "new": "Column(String)"}),
        )
        .await;
        assert!(!redo.is_error, "{}", redo.content);
        let before_undo = on_disk(&dir, "settings.py").unwrap();
        ctx.artifacts =
            std::sync::Arc::new(crate::session::ArtifactStore::new("s", Some(root)).unwrap());
        let refused = mutate(&ctx, &UndoTool, json!({"path": "settings.py"})).await;
        assert!(refused.is_error, "{}", refused.content);
        assert!(
            refused.content.contains("redaction placeholder"),
            "{}",
            refused.content
        );
        assert_eq!(on_disk(&dir, "settings.py").unwrap(), before_undo);
    }

    #[tokio::test]
    async fn undo_restores_a_file_that_legitimately_contains_a_placeholder() {
        let dir = tempfile::tempdir().unwrap();
        let placeholder = crate::security::redact::Redactor::new(true)
            .redact_owned("aws_key = AKIAZZZZZZZZZZZZZZZZ")
            .trim_start_matches("aws_key = ")
            .to_string();
        let original = format!("expected = {placeholder}\nkeep = 1\n");
        std::fs::write(dir.path().join("fixture.txt"), &original).unwrap();
        let ctx = test_context(dir.path());
        ReadTool.call(&ctx, json!({"path": "fixture.txt"})).await;

        let edited = mutate(
            &ctx,
            &EditTool,
            json!({
                "path": "fixture.txt",
                "old": format!("expected = {placeholder}\n"),
                "new": ""
            }),
        )
        .await;
        assert!(!edited.is_error, "{}", edited.content);
        assert_eq!(
            on_disk(&dir, "fixture.txt").unwrap(),
            "keep = 1\n".to_string()
        );

        let undone = mutate(&ctx, &UndoTool, json!({"path": "fixture.txt"})).await;
        assert!(!undone.is_error, "{}", undone.content);
        assert_eq!(on_disk(&dir, "fixture.txt").unwrap(), original);
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
            result.content.contains("snapshotted change"),
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

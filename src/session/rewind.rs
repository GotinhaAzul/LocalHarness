//! `/rewind` and `tot --rewind`: move the active leaf to an earlier entry and
//! restore every file the harness wrote after it to the snapshot that entry
//! implies.
//!
//! The plan is built from the same `fs-change` entries `undo` uses, so the two
//! features can never disagree about what the harness wrote. Files changed by
//! `bash`, a formatter, or the user are never touched: a file with no
//! `fs-change` entry has no snapshot and no place in the plan.
//!
//! Applying a plan replays each file's changes in reverse order; the last
//! write for a file is its oldest post-target snapshot, which is exactly the
//! content it had at the target. A file that a reverted write created is
//! deleted. Every overwritten file is checked against the hash the newest
//! change recorded first; `--force` backs up a conflicting file before it is
//! overwritten.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use crate::tools::undo::content_hash;
use crate::tools::write::{atomic_restore, atomic_write, file_mode};

use super::{CustomData, Entry, EntryKind, Session};

/// One file to put back at its target-entry content.
#[derive(Debug, Clone)]
pub struct FileRestore {
    /// Path as the session and the UI display it, relative to the session cwd.
    pub display: String,
    pub path: PathBuf,
    /// Content at the target. `None` means the file did not exist there.
    pub content: Option<String>,
    /// How the target content is stored on disk.
    pub encoding: crate::tools::encoding::Encoding,
    /// What the newest harness write left on disk.
    pub expected_sha256: String,
    /// `fs-change` seqs this restore undoes.
    pub changes: Vec<u64>,
}

/// A file the harness could not restore, with the reason to show the user.
#[derive(Debug, Clone)]
pub struct Unrestorable {
    pub display: String,
    pub reason: String,
}

/// Everything a rewind will do, before anything is written.
#[derive(Debug, Clone)]
pub struct RewindPlan {
    pub target: String,
    pub target_seq: u64,
    pub files: Vec<FileRestore>,
    pub unrestorable: Vec<Unrestorable>,
}

impl RewindPlan {
    /// True when the conversation moves but no file changes.
    pub fn is_pure_checkout(&self) -> bool {
        self.files.is_empty()
    }
}

/// What a rewind did.
#[derive(Debug, Clone, Default)]
pub struct RewindReport {
    pub target: String,
    pub restored: Vec<String>,
    pub deleted: Vec<String>,
    pub backups: Vec<String>,
    /// Files were overwritten despite a hash mismatch.
    pub forced_overwrites: Vec<String>,
    /// `fs-change` seqs the restored content reverts.
    pub reverted: Vec<u64>,
}

impl RewindReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.restored.is_empty() {
            parts.push(format!("restored {}", self.restored.join(", ")));
        }
        if !self.deleted.is_empty() {
            parts.push(format!("deleted {}", self.deleted.join(", ")));
        }
        if parts.is_empty() {
            parts.push("no harness-written files changed after the target".into());
        }
        if !self.backups.is_empty() {
            parts.push(format!("backed up {}", self.backups.join(", ")));
        }
        parts.join("; ")
    }
}

/// Refuse a rewind that cannot be represented. The message names the next
/// action: choose another entry, or fix the file with `edit`.
fn conflict_message(conflicts: &[String]) -> String {
    format!(
        "rewind refused: {} file(s) changed outside the harness after the last snapshot:\n  - {}\n\
         Nothing was written and the conversation did not move. Read the files and keep the change, \
         or re-run with --force (or /rewind … force) to back them up and restore the target.",
        conflicts.len(),
        conflicts.join("\n  - ")
    )
}

/// Build the rewind plan for `target` on the active branch.
pub fn plan(session: &Session, target: &str) -> Result<RewindPlan> {
    let entries = session.branch_entries();
    let resolved = resolve_target(session, &entries, target)?;
    let header = session.tree.header();
    let header_id = header.map(|entry| entry.id.clone());
    // The first user entry has no parent, so the header is not on the branch
    // path; rewinding to it means "before the first turn" and uses seq 0.
    let target_seq = if header_id.as_deref() == Some(resolved.as_str()) {
        0
    } else {
        entries
            .iter()
            .find(|entry| entry.id == resolved)
            .map(|entry| entry.seq)
            .ok_or_else(|| anyhow::anyhow!("entry {target} is not on the active branch"))?
    };
    let cwd = header
        .and_then(|entry| match &entry.kind {
            EntryKind::Session(data) if !data.cwd.trim().is_empty() => {
                Some(PathBuf::from(&data.cwd))
            }
            _ => None,
        })
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    struct Change {
        seq: u64,
        payload: Map<String, Value>,
    }

    // Active harness changes per path, after undos are folded in. An undo
    // entry removes the change it names and is not itself a file change.
    let mut stacks: BTreeMap<String, Vec<Change>> = BTreeMap::new();
    let mut cross_undoes: Vec<(u64, u64, String)> = Vec::new();
    for entry in &entries {
        let EntryKind::Custom(custom) = &entry.kind else {
            continue;
        };
        if custom.kind != "fs-change" {
            continue;
        }
        let Some(display) = custom.payload.get("path").and_then(Value::as_str) else {
            continue;
        };
        if custom.payload.get("undo").and_then(Value::as_bool) == Some(true) {
            if let Some(reverted) = custom.payload.get("reverts").and_then(Value::as_u64) {
                for stack in stacks.values_mut() {
                    stack.retain(|change| change.seq != reverted);
                }
                cross_undoes.push((entry.seq, reverted, display.to_string()));
            }
            continue;
        }
        stacks.entry(display.to_string()).or_default().push(Change {
            seq: entry.seq,
            payload: custom.payload.clone(),
        });
    }

    let mut files = Vec::new();
    let mut unrestorable = Vec::new();
    for (display, stack) in &stacks {
        let post: Vec<&Change> = stack
            .iter()
            .filter(|change| change.seq > target_seq)
            .collect();
        if post.is_empty() {
            continue;
        }
        let path = resolve_path(&cwd, display);
        let newest = post.last().expect("non-empty");
        let Some(expected) = newest.payload.get("after_sha256").and_then(Value::as_str) else {
            unrestorable.push(Unrestorable {
                display: display.clone(),
                reason: "the newest change recorded no content hash, so the harness cannot prove \
                         the file still matches what it wrote"
                    .into(),
            });
            continue;
        };
        let oldest = post.first().expect("non-empty");
        let content = if oldest.payload.get("created").and_then(Value::as_bool) == Some(true) {
            None
        } else if let Some(snapshot) = oldest.payload.get("snapshot").and_then(Value::as_str) {
            match session.artifacts.read_all(snapshot) {
                Ok(text) => Some(text),
                Err(err) => {
                    unrestorable.push(Unrestorable {
                        display: display.clone(),
                        reason: format!(
                            "the snapshot for {display} is gone ({err}); choose a later entry or \
                             fix the file with edit"
                        ),
                    });
                    continue;
                }
            }
        } else {
            let reason = oldest
                .payload
                .get("snapshot_error")
                .and_then(Value::as_str)
                .unwrap_or("the change has no undo snapshot");
            unrestorable.push(Unrestorable {
                display: display.clone(),
                reason: reason.to_string(),
            });
            continue;
        };
        files.push(FileRestore {
            display: display.clone(),
            path,
            content,
            encoding: oldest
                .payload
                .get("encoding")
                .and_then(Value::as_str)
                .map(crate::tools::encoding::Encoding::from_label)
                .unwrap_or(crate::tools::encoding::Encoding::Utf8),
            expected_sha256: expected.to_string(),
            changes: post.iter().map(|change| change.seq).collect(),
        });
    }

    // An undo after the target that reverted a change made at or before the
    // target would need the after-content of a change the harness never
    // snapshotted. Name it instead of silently restoring the wrong bytes.
    for (undo_seq, reverted_seq, display) in cross_undoes {
        if undo_seq > target_seq && reverted_seq <= target_seq {
            unrestorable.push(Unrestorable {
                display,
                reason: format!(
                    "an undo after the target reverted the change at seq {reverted_seq}, which \
                     happened at or before the target; the harness did not snapshot the content \
                     that undo restored"
                ),
            });
        }
    }

    files.sort_by(|a, b| a.display.cmp(&b.display));
    Ok(RewindPlan {
        target: resolved,
        target_seq,
        files,
        unrestorable,
    })
}

/// Apply a plan. With `force`, a file that changed outside the harness is
/// backed up before it is overwritten; without it, the rewind writes nothing
/// and returns an error naming every conflict.
pub fn apply(session: &mut Session, plan: &RewindPlan, force: bool) -> Result<RewindReport> {
    if !plan.unrestorable.is_empty() {
        let names: Vec<String> = plan
            .unrestorable
            .iter()
            .map(|item| format!("{}: {}", item.display, item.reason))
            .collect();
        bail!(
            "rewind refused: {} file(s) cannot be restored:\n  - {}\nThe conversation did not move.",
            plan.unrestorable.len(),
            names.join("\n  - ")
        );
    }

    let mut conflicts = Vec::new();
    let mut current: Vec<Option<Vec<u8>>> = Vec::new();
    for file in &plan.files {
        let bytes = match std::fs::read(&file.path) {
            Ok(bytes) => Some(bytes),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => bail!("cannot read {}: {err}", file.display),
        };
        match &bytes {
            Some(bytes) if content_hash(bytes) != file.expected_sha256 => {
                conflicts.push(file.display.clone());
            }
            // A file the recorded change left missing is not a conflict; the
            // restore puts the snapshot back.
            None if file.content.is_some() && file.expected_sha256 != content_hash(b"") => {
                conflicts.push(format!("{} (deleted outside the harness)", file.display));
            }
            _ => {}
        }
        current.push(bytes);
    }
    if !conflicts.is_empty() && !force {
        bail!("{}", conflict_message(&conflicts));
    }

    let mut backups = Vec::new();
    let mut forced_overwrites = Vec::new();
    for (file, bytes) in plan.files.iter().zip(&current) {
        let Some(bytes) = bytes else {
            continue;
        };
        if content_hash(bytes) == file.expected_sha256 {
            continue;
        }
        if !force {
            continue;
        }
        let backup = backup_path(&file.path);
        std::fs::write(&backup, bytes)
            .map_err(|err| anyhow::anyhow!("cannot back up {}: {err}", file.display))?;
        if let Ok(mode) = std::fs::metadata(&file.path).map(|meta| {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o7777
        }) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(mode));
        }
        let display = path_display(session, &backup);
        backups.push(display);
        forced_overwrites.push(file.display.clone());
    }

    let mut report = RewindReport {
        target: plan.target.clone(),
        backups,
        forced_overwrites,
        ..RewindReport::default()
    };
    let originals: Vec<Option<(Vec<u8>, u32)>> = plan
        .files
        .iter()
        .zip(&current)
        .map(|(file, bytes)| {
            bytes
                .as_ref()
                .map(|bytes| (bytes.clone(), file_mode(&file.path)))
        })
        .collect();
    for (index, file) in plan.files.iter().enumerate() {
        let result = match &file.content {
            Some(content) => {
                if let Some(parent) = file.path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent).map_err(|err| {
                        anyhow::anyhow!(
                            "cannot create {} for {}: {err}",
                            parent.display(),
                            file.display
                        )
                    })?;
                }
                let bytes =
                    crate::tools::encoding::encode(content, file.encoding).ok_or_else(|| {
                        anyhow::anyhow!(
                            "cannot encode the restored text of {} as {}",
                            file.display,
                            file.encoding.label()
                        )
                    })?;
                atomic_write(&file.path, &bytes, file_mode(&file.path))
                    .map_err(|err| anyhow::anyhow!("cannot restore {}: {err}", file.display))
            }
            None => match std::fs::remove_file(&file.path) {
                Ok(()) => Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(anyhow::anyhow!(
                    "cannot delete {} (it was created after the target): {err}",
                    file.display
                )),
            },
        };
        if let Err(error) = result {
            // Earlier files were already changed. Put their original bytes
            // back so a retry is not refused by the plan's expected hashes.
            let mut rollback_errors = Vec::new();
            for (done, original) in originals[..index].iter().enumerate().rev() {
                let path = &plan.files[done].path;
                let outcome = match original {
                    Some((bytes, mode)) => atomic_restore(path, bytes, *mode),
                    None => match std::fs::remove_file(path) {
                        Ok(()) => Ok(()),
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        Err(err) => Err(err),
                    },
                };
                if let Err(err) = outcome {
                    rollback_errors.push(format!("{}: {err}", path.display()));
                }
            }
            if rollback_errors.is_empty() {
                bail!(
                    "{error}; rolled back {} earlier file(s), so the conversation did not move",
                    index
                );
            }
            bail!(
                "{error}; rollback also failed for {}; inspect the working tree before retrying",
                rollback_errors.join(", ")
            );
        }
        match &file.content {
            Some(_) => report.restored.push(file.display.clone()),
            None => report.deleted.push(file.display.clone()),
        }
        report.reverted.extend(file.changes.iter().copied());
    }

    // Move the leaf first, then append the marker and the undo bookkeeping on
    // the target branch. The abandoned branch stays in the tree untouched.
    session.move_leaf(&plan.target, "rewind")?;
    let mut data = CustomData::new("rewind")
        .with("target", plan.target.clone())
        .with("target_seq", plan.target_seq)
        .with("restored", report.restored.clone())
        .with("deleted", report.deleted.clone())
        .with("backups", report.backups.clone())
        .with("reverted", report.reverted.clone())
        .with("forced", !report.forced_overwrites.is_empty());
    if !report.forced_overwrites.is_empty() {
        data = data.with("forced_overwrites", report.forced_overwrites.clone());
    }
    session.append(EntryKind::Custom(data))?;
    Ok(report)
}

/// The entry before the `turns`-th most recent user message on the active
/// branch. `turns = 1` is "before the last user message".
pub fn target_before_turn(session: &Session, turns: usize) -> Result<String, String> {
    if turns == 0 {
        return Err("pass a positive number of turns, for example /rewind 1".into());
    }
    let users: Vec<&Entry> = session
        .branch_entries()
        .into_iter()
        .filter(|entry| {
            !entry.is_checkout()
                && matches!(entry.kind, EntryKind::User(_))
                && entry.provenance.as_deref() != Some(crate::provenance::CONTINUATION)
        })
        .collect();
    if turns > users.len() {
        return Err(format!(
            "this branch has {} user message(s); cannot rewind {turns}. Use /rewind with a smaller number.",
            users.len()
        ));
    }
    let user = users[users.len() - turns];
    Ok(user
        .parent
        .clone()
        .or_else(|| session.tree.header().map(|entry| entry.id.clone()))
        .unwrap_or_else(|| user.id.clone()))
}

/// Resolve a user-supplied entry to the boundary `rewind` moves to. Tool
/// results resolve to the last result of their batch, exactly like `/tree`.
fn resolve_target(session: &Session, entries: &[&Entry], target: &str) -> Result<String> {
    if session.tree.header().map(|entry| entry.id.as_str()) == Some(target) {
        return Ok(target.to_string());
    }
    if !entries.iter().any(|entry| entry.id == target) {
        bail!("entry {target} is not on the active branch; run /tree to choose a visible step");
    }
    session
        .tree
        .resolve_checkout(target)
        .map_err(|err| anyhow::anyhow!("{err}"))
}

fn resolve_path(cwd: &Path, display: &str) -> PathBuf {
    let path = PathBuf::from(display);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn path_display(session: &Session, path: &Path) -> String {
    let cwd = session
        .header()
        .map(|header| PathBuf::from(&header.cwd))
        .unwrap_or_default();
    path.strip_prefix(&cwd)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

fn backup_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string());
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    path.with_file_name(format!("{name}.tot-rewind-{nanos}.bak"))
}

//! Interactive hunk review before file mutations land.
//!
//! `edit`, `patch`, and `write` compute the text they would write and call
//! [`mutation_write`]. During a reviewed batch the agent loop installs a
//! [`Capture`] with [`with_capture`], so the write is recorded instead of
//! touching the file. The loop turns each recorded write into file/line hunks
//! with the same `similar` diff the tool results already use, asks a
//! [`HunkReviewer`] for accept/reject/edit decisions, and writes the accepted
//! hunks atomically. A rejected hunk is reported back in the tool result.
//!
//! This is not a second diff engine: the hunks come from `TextDiff::from_lines`
//! on the exact before/after text a tool already computed, and applying them
//! walks those same diff ops. The undo snapshots are unchanged, so `undo`
//! still steps back over an applied review.
//!
//! Headless sessions have no reviewer (the TUI installs one). The loop then
//! auto-accepts every hunk, which is the pre-review behavior, and records the
//! auto-accept in the session.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use similar::{DiffOp, TextDiff};
use tokio_util::sync::CancellationToken;

use crate::tools::write::{atomic_restore, atomic_write, file_mode};

/// One proposed file write, captured before it lands.
#[derive(Debug, Clone)]
pub struct PendingWrite {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: String,
    /// The existing file's encoding, so a reviewed write keeps it.
    pub encoding: crate::tools::encoding::Encoding,
    /// Permission bits to use if the file is created.
    pub mode: u32,
}

/// Shared capture buffer installed on the current task by the agent loop.
pub type Capture = Arc<Mutex<Vec<PendingWrite>>>;

tokio::task_local! {
    static CAPTURE: Capture;
}

/// Run `future` with a capture buffer installed. Mutation tools called within
/// it record their writes in `capture` instead of touching the disk.
pub async fn with_capture<F: Future>(capture: Capture, future: F) -> F::Output {
    CAPTURE.scope(capture, future).await
}

fn current_capture() -> Option<Capture> {
    CAPTURE.try_with(Clone::clone).ok()
}

/// True while a reviewed batch is executing on this task.
pub fn capturing() -> bool {
    current_capture().is_some()
}

/// Write a mutation, or record it when a review capture is installed.
///
/// `before` is the text the tool read before the change (`None` for a new
/// file). Mutating tools call this where they used to call `atomic_write`, so
/// the only difference under review is that the bytes stay in memory.
pub(crate) fn mutation_write(
    path: &Path,
    before: Option<&str>,
    after: &str,
    mode: u32,
) -> std::io::Result<()> {
    mutation_write_encoded(
        path,
        before,
        after,
        after.as_bytes(),
        mode,
        crate::tools::encoding::Encoding::Utf8,
    )
}

/// [`mutation_write`] for a file with its own encoding: the bytes are the
/// encoded `after` text, kept so a reviewed write never converts the file.
#[allow(clippy::too_many_arguments)]
pub(crate) fn mutation_write_encoded(
    path: &Path,
    before: Option<&str>,
    after: &str,
    bytes: &[u8],
    mode: u32,
    encoding: crate::tools::encoding::Encoding,
) -> std::io::Result<()> {
    // A file that exists but has no readable text (not text at all, or over
    // 4 MB) cannot be diffed; write it as before rather than failing the
    // review.
    if current_capture().is_some() && (before.is_some() || !path.exists()) {
        // Refuse a redaction placeholder now, as an uncaptured write would,
        // instead of showing it for review and failing the whole apply.
        crate::tools::write::refuse_new_placeholders(path, bytes)?;
        let capture = current_capture().expect("checked above");
        let mut writes = capture.lock().map_err(|_| {
            std::io::Error::other("the review buffer is poisoned; restart the turn")
        })?;
        writes.push(PendingWrite {
            path: path.to_path_buf(),
            before: before.map(str::to_string),
            after: after.to_string(),
            encoding,
            mode,
        });
        return Ok(());
    }
    atomic_write(path, bytes, mode)
}

/// A hunk as the reviewer sees it. `end_line < start_line` is an insertion at
/// `start_line - 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkView {
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub added: usize,
    pub removed: usize,
    /// One line: the change itself, or "delete N lines".
    pub rationale: String,
    /// The proposed replacement text, for the frontend's edit key.
    pub text: String,
}

impl HunkView {
    pub fn range_label(&self) -> String {
        if self.end_line < self.start_line {
            format!("after line {}", self.start_line.saturating_sub(1))
        } else if self.start_line == self.end_line {
            format!("line {}", self.start_line)
        } else {
            format!("lines {}-{}", self.start_line, self.end_line)
        }
    }
}

/// What to do with one hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HunkChoice {
    Accept,
    Reject,
    /// Replace the hunk's new lines with this text.
    Edit(String),
}

/// One decision per [`HunkView`], in the same order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkDecision {
    pub choices: Vec<HunkChoice>,
}

impl HunkDecision {
    pub fn accept_all(count: usize) -> Self {
        Self {
            choices: vec![HunkChoice::Accept; count],
        }
    }

    pub fn reject_all(count: usize) -> Self {
        Self {
            choices: vec![HunkChoice::Reject; count],
        }
    }
}

/// A review request sent to a frontend.
#[derive(Debug, Clone)]
pub struct HunkRequest {
    pub hunks: Vec<HunkView>,
    /// Cancelling the turn also ends the wait; the reviewer returns
    /// [`HunkDecision::reject_all`].
    pub cancel: CancellationToken,
}

/// The frontend seam for hunk review. The TUI implements it with a modal;
/// integration tests implement it with a script. `submit`/`cancel` deliver a
/// decision for a request that `review` returned to the event loop.
#[async_trait]
pub trait HunkReviewer: Send + Sync {
    async fn review(&self, request: HunkRequest) -> HunkDecision;

    fn submit(&self, _id: u64, _decision: HunkDecision) {}

    fn cancel(&self, _id: u64) {}
}

/// What applying a review did to one proposal.
#[derive(Debug, Clone)]
pub struct ProposalOutcome {
    /// Content written. `None` means the file was not created.
    pub final_content: Option<String>,
    pub accepted: usize,
    /// Rejected old line ranges, 1-based inclusive.
    pub rejected: Vec<(usize, usize)>,
    pub changed: bool,
}

struct Proposal {
    write: PendingWrite,
    display: String,
    hunks: usize,
}

/// The proposed mutations of one reviewed batch, split into hunks.
pub struct HunkSet {
    proposals: Vec<Proposal>,
    views: Vec<HunkView>,
}

impl HunkSet {
    pub fn new(writes: Vec<PendingWrite>, cwd: &Path) -> Self {
        let mut proposals = Vec::new();
        let mut views = Vec::new();
        for write in writes {
            let display = write
                .path
                .strip_prefix(cwd)
                .unwrap_or(&write.path)
                .to_string_lossy()
                .to_string();
            let (hunks, hunk_views) = split_hunks(&write, &display);
            views.extend(hunk_views);
            proposals.push(Proposal {
                write,
                display,
                hunks,
            });
        }
        Self { proposals, views }
    }

    pub fn is_empty(&self) -> bool {
        self.views.is_empty()
    }

    pub fn views(&self) -> &[HunkView] {
        &self.views
    }

    pub fn files(&self) -> Vec<String> {
        self.proposals
            .iter()
            .map(|proposal| proposal.display.clone())
            .collect()
    }

    /// Number of hunks per proposal, in proposal order.
    pub fn hunk_counts(&self) -> Vec<usize> {
        self.proposals
            .iter()
            .map(|proposal| proposal.hunks)
            .collect()
    }

    /// Validate the decision against the files and write every accepted hunk.
    ///
    /// The whole set is atomic: if any write fails, files already written are
    /// restored from their pre-review bytes and files already created are
    /// removed. A file that changed while the review was open aborts before
    /// anything is written.
    pub fn commit(&self, decision: &HunkDecision) -> Result<Vec<ProposalOutcome>, String> {
        if decision.choices.len() != self.views.len() {
            return Err(format!(
                "review returned {} decisions for {} hunks",
                decision.choices.len(),
                self.views.len()
            ));
        }
        for proposal in &self.proposals {
            let current = std::fs::read(&proposal.write.path);
            match (&proposal.write.before, current) {
                (Some(before), Ok(bytes)) if bytes == before.as_bytes() => {}
                (Some(_), Ok(_)) => {
                    return Err(format!(
                        "{} changed while the review was open; nothing was written. Read it and \
                         retry the change.",
                        proposal.display
                    ));
                }
                (Some(_), Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                    return Err(format!(
                        "{} was deleted while the review was open; nothing was written. Recreate \
                         it or retry the change.",
                        proposal.display
                    ));
                }
                (None, Err(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
                (None, Ok(_)) => {
                    return Err(format!(
                        "{} was created while the review was open; nothing was written. Read it \
                         and retry the change.",
                        proposal.display
                    ));
                }
                (_, Err(err)) => {
                    return Err(format!(
                        "cannot read {}: {err}; nothing was written",
                        proposal.display
                    ));
                }
            }
        }

        let mut choices = decision.choices.iter();
        let mut outcomes = Vec::new();
        for proposal in &self.proposals {
            let mut accepted = 0;
            let mut rejected = Vec::new();
            let final_content =
                apply_proposal(proposal, &mut choices, &mut accepted, &mut rejected);
            let changed = match (&proposal.write.before, &final_content) {
                (Some(before), Some(after)) => before != after,
                (None, Some(_)) => true,
                _ => false,
            };
            outcomes.push(ProposalOutcome {
                final_content,
                accepted,
                rejected,
                changed,
            });
        }

        let mut written: Vec<(PathBuf, Option<Vec<u8>>, u32)> = Vec::new();
        for (proposal, outcome) in self.proposals.iter().zip(&outcomes) {
            let Some(content) = &outcome.final_content else {
                continue;
            };
            if !outcome.changed {
                continue;
            }
            let previous = std::fs::read(&proposal.write.path).ok();
            let mode = file_mode(&proposal.write.path);
            let mode = if previous.is_some() {
                mode
            } else {
                proposal.write.mode
            };
            if let Some(parent) = proposal.write.path.parent()
                && !parent.as_os_str().is_empty()
                && let Err(err) = std::fs::create_dir_all(parent)
            {
                rollback(&written);
                return Err(format!(
                    "cannot create {} for {}: {err}; every hunk was rolled back",
                    parent.display(),
                    proposal.display
                ));
            }
            let encoded = crate::tools::encoding::encode(content, proposal.write.encoding);
            let Some(encoded) = encoded else {
                rollback(&written);
                return Err(format!(
                    "cannot encode the reviewed change to {} as {}; a hunk edit introduced a character that encoding cannot represent",
                    proposal.display,
                    proposal.write.encoding.label()
                ));
            };
            match atomic_write(&proposal.write.path, &encoded, mode) {
                Ok(()) => written.push((proposal.write.path.clone(), previous, mode)),
                Err(err) => {
                    rollback(&written);
                    return Err(format!(
                        "cannot write {}: {err}; every hunk was rolled back",
                        proposal.display
                    ));
                }
            }
        }
        Ok(outcomes)
    }
}

fn rollback(written: &[(PathBuf, Option<Vec<u8>>, u32)]) {
    for (path, previous, mode) in written.iter().rev() {
        match previous {
            Some(bytes) => {
                let _ = atomic_restore(path, bytes, *mode);
            }
            None => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

fn apply_proposal(
    proposal: &Proposal,
    choices: &mut std::slice::Iter<'_, HunkChoice>,
    accepted: &mut usize,
    rejected: &mut Vec<(usize, usize)>,
) -> Option<String> {
    let Some(before) = &proposal.write.before else {
        let choice = choices.next().expect("one hunk for a new file");
        return match choice {
            HunkChoice::Reject => None,
            HunkChoice::Accept => {
                *accepted += 1;
                Some(proposal.write.after.clone())
            }
            HunkChoice::Edit(text) => {
                *accepted += 1;
                Some(text.clone())
            }
        };
    };
    let diff = TextDiff::from_lines(before, &proposal.write.after);
    let old_slices = diff.old_slices();
    let new_slices = diff.new_slices();
    let mut out = String::new();
    for op in diff.ops() {
        match *op {
            DiffOp::Equal { old_index, len, .. } => {
                for line in &old_slices[old_index..old_index + len] {
                    out.push_str(line);
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                let choice = choices.next().expect("one decision per changed op");
                match choice {
                    HunkChoice::Accept => {
                        *accepted += 1;
                    }
                    HunkChoice::Reject => {
                        rejected.push((old_index + 1, old_index + old_len));
                        for line in &old_slices[old_index..old_index + old_len] {
                            out.push_str(line);
                        }
                    }
                    HunkChoice::Edit(text) => {
                        *accepted += 1;
                        for line in text_lines(text) {
                            out.push_str(&line);
                        }
                    }
                }
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                let choice = choices.next().expect("one decision per changed op");
                match choice {
                    HunkChoice::Accept => {
                        *accepted += 1;
                        for line in &new_slices[new_index..new_index + new_len] {
                            out.push_str(line);
                        }
                    }
                    HunkChoice::Reject => rejected.push((old_index + 1, old_index)),
                    HunkChoice::Edit(text) => {
                        *accepted += 1;
                        for line in text_lines(text) {
                            out.push_str(&line);
                        }
                    }
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let choice = choices.next().expect("one decision per changed op");
                match choice {
                    HunkChoice::Accept => {
                        *accepted += 1;
                        for line in &new_slices[new_index..new_index + new_len] {
                            out.push_str(line);
                        }
                    }
                    HunkChoice::Reject => {
                        rejected.push((old_index + 1, old_index + old_len));
                        for line in &old_slices[old_index..old_index + old_len] {
                            out.push_str(line);
                        }
                    }
                    HunkChoice::Edit(text) => {
                        *accepted += 1;
                        for line in text_lines(text) {
                            out.push_str(&line);
                        }
                    }
                }
            }
        }
    }
    Some(out)
}

/// Split `\n`-terminated text into lines that keep their endings, so a
/// reconstructed file is byte-identical to the tool's proposal.
fn text_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n').map(str::to_string).collect()
}

fn split_hunks(write: &PendingWrite, display: &str) -> (usize, Vec<HunkView>) {
    let mut hunks = 0usize;
    let mut views = Vec::new();
    let Some(before) = &write.before else {
        let new_lines = text_lines(&write.after);
        let added = new_lines.len();
        views.push(HunkView {
            file: display.to_string(),
            start_line: 1,
            end_line: added.max(1),
            added,
            removed: 0,
            rationale: format!("create {display} ({added} lines)"),
            text: write.after.clone(),
        });
        hunks += 1;
        return (hunks, views);
    };
    let diff = TextDiff::from_lines(before, &write.after);
    let new_slices = diff.new_slices();
    for op in diff.ops() {
        let (old_index, old_len, new_index, new_len) = match *op {
            DiffOp::Equal { .. } => continue,
            DiffOp::Delete {
                old_index, old_len, ..
            } => (old_index, old_len, 0, 0),
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => (old_index, 0, new_index, new_len),
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => (old_index, old_len, new_index, new_len),
        };
        let new_lines: Vec<String> = new_slices[new_index..new_index + new_len]
            .iter()
            .map(|line| (*line).to_string())
            .collect();
        let rationale = new_lines
            .iter()
            .map(|line| line.trim())
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(80).collect())
            .unwrap_or_else(|| format!("delete {old_len} line(s)"));
        views.push(HunkView {
            file: display.to_string(),
            start_line: old_index + 1,
            end_line: old_index + old_len,
            added: new_len,
            removed: old_len,
            rationale,
            text: new_lines.join(""),
        });
        hunks += 1;
    }
    (hunks, views)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposal(path: &Path, before: Option<&str>, after: &str) -> PendingWrite {
        PendingWrite {
            path: path.to_path_buf(),
            before: before.map(str::to_string),
            after: after.to_string(),
            encoding: crate::tools::encoding::Encoding::Utf8,
            mode: 0o600,
        }
    }

    #[test]
    fn one_edit_is_one_hunk_with_its_line_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let set = HunkSet::new(
            vec![proposal(
                &path,
                Some("one\ntwo\nthree\n"),
                "one\nTWO\nthree\n",
            )],
            dir.path(),
        );
        assert_eq!(set.views().len(), 1);
        assert_eq!(set.views()[0].file, "a.txt");
        assert_eq!(set.views()[0].range_label(), "line 2");
        assert_eq!(set.views()[0].added, 1);
        assert_eq!(set.views()[0].removed, 1);
    }

    #[test]
    fn a_two_region_write_is_two_hunks_and_partial_reject_keeps_the_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a\nb\nc\nd\ne\nf\ng\nh\n").unwrap();
        let after = "A\nb\nc\nd\ne\nf\ng\nH\n";
        let set = HunkSet::new(
            vec![proposal(
                &path,
                Some(&std::fs::read_to_string(&path).unwrap()),
                after,
            )],
            dir.path(),
        );
        assert_eq!(set.views().len(), 2);
        let decision = HunkDecision {
            choices: vec![HunkChoice::Accept, HunkChoice::Reject],
        };
        let outcomes = set.commit(&decision).unwrap();
        assert_eq!(outcomes[0].accepted, 1);
        assert_eq!(outcomes[0].rejected, [(8, 8)]);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "A\nb\nc\nd\ne\nf\ng\nh\n"
        );
    }

    #[test]
    fn reject_all_leaves_every_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let original = "one\ntwo\n";
        std::fs::write(&a, original).unwrap();
        std::fs::write(&b, original).unwrap();
        let set = HunkSet::new(
            vec![
                proposal(&a, Some(original), "one\nTWO\n"),
                proposal(&b, Some(original), "ONE\ntwo\n"),
            ],
            dir.path(),
        );
        let outcomes = set
            .commit(&HunkDecision::reject_all(set.views().len()))
            .unwrap();
        assert!(outcomes.iter().all(|outcome| !outcome.changed));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), original);
    }

    #[test]
    fn a_failed_write_rolls_back_files_already_written() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.txt");
        std::fs::write(&good, "old\n").unwrap();
        // `/proc` rejects a new directory for every user, so the second
        // write fails deterministically without depending on permissions.
        let bad = Path::new("/proc/totality-rewind-test/child.txt").to_path_buf();
        let set = HunkSet::new(
            vec![
                proposal(&good, Some("old\n"), "new\n"),
                proposal(&bad, None, "new\n"),
            ],
            dir.path(),
        );
        let error = set
            .commit(&HunkDecision::accept_all(set.views().len()))
            .unwrap_err();
        assert!(error.contains("rolled back"), "{error}");
        assert_eq!(std::fs::read_to_string(&good).unwrap(), "old\n");
    }

    fn placeholder() -> String {
        // Built through the redactor so the test follows its real format.
        let redacted = crate::security::redact::Redactor::new(true)
            .redact_owned(&format!("aws_key = AKIA{}", "Q".repeat(16)));
        redacted.trim_start_matches("aws_key = ").to_string()
    }

    #[tokio::test]
    async fn a_captured_write_refuses_a_new_placeholder_before_review() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.py");
        std::fs::write(&path, "name = Column(Text)\n").unwrap();
        let capture = Capture::default();
        let corrupted = format!("name = {} Text)\n", placeholder());
        let result = with_capture(capture.clone(), async {
            mutation_write(&path, Some("name = Column(Text)\n"), &corrupted, 0o600)
        })
        .await;
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("redaction placeholder"),
            "{error}"
        );
        assert!(capture.lock().unwrap().is_empty(), "nothing is buffered");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "name = Column(Text)\n"
        );
    }

    #[test]
    fn rollback_restores_a_file_that_held_placeholder_text() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("fixture.txt");
        let original = format!("expected = {}\n", placeholder());
        std::fs::write(&good, &original).unwrap();
        let bad = Path::new("/proc/totality-rewind-test/child.txt").to_path_buf();
        let set = HunkSet::new(
            vec![
                proposal(&good, Some(&original), "expected = 1\n"),
                proposal(&bad, None, "new\n"),
            ],
            dir.path(),
        );
        let error = set
            .commit(&HunkDecision::accept_all(set.views().len()))
            .unwrap_err();
        assert!(error.contains("rolled back"), "{error}");
        assert_eq!(std::fs::read_to_string(&good).unwrap(), original);
    }

    #[test]
    fn a_file_changed_while_under_review_aborts_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, "old\n").unwrap();
        std::fs::write(&b, "old\n").unwrap();
        let set = HunkSet::new(
            vec![
                proposal(&a, Some("old\n"), "new\n"),
                proposal(&b, Some("old\n"), "new\n"),
            ],
            dir.path(),
        );
        std::fs::write(&b, "changed\n").unwrap();
        let error = set
            .commit(&HunkDecision::accept_all(set.views().len()))
            .unwrap_err();
        assert!(error.contains("b.txt"), "{error}");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "old\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "changed\n");
    }
}

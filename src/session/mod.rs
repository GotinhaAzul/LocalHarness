//! Session state: append-only JSONL tree, artifacts, export.

pub mod artifact;
pub mod entry;
pub mod rewind;
pub mod store;
pub mod tree;

use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use chrono::Utc;

pub use artifact::{Artifact, ArtifactError, ArtifactSlice, ArtifactStore};
pub use entry::{
    AssistantData, Attachment, CheckoutData, CompactionData, CustomData, Entry, EntryKind,
    ErrorData, FORMAT_VERSION, ImageRef, SessionContextWindow, SessionData, SessionPermissions,
    ToolCall, ToolResultData, UserData,
};
pub use rewind::{RewindPlan, RewindReport};
pub use store::{SessionStore, StoreError};
pub use tree::{CheckoutError, SessionTree};

use crate::paths::{Paths, new_session_id};
use crate::security::redact::{Redactor, contains_placeholder};

/// Appended to a replayed tool result whose saved form contains redaction
/// placeholders, so the model does not mistake them for file content.
/// The same note as written before 0.3.
pub const LEGACY_RESUMED_PLACEHOLDER_NOTE: &str = "\n[localharness] This result was restored from a saved session. [REDACTED:...] markers were substituted when it was saved and are not file content; re-read a file before editing it.";

pub const RESUMED_PLACEHOLDER_NOTE: &str = "\n[totality] This result was restored from a saved session. [REDACTED:...] markers were substituted when it was saved and are not file content; re-read a file before editing it.";

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Prompt history is written next to the session files but is not a session.
pub const PROMPT_HISTORY_FILE: &str = "prompt_history.jsonl";

/// A session file with at least one user message.
#[derive(Debug, Clone)]
pub struct SessionFile {
    pub id: String,
    pub path: PathBuf,
    pub modified: std::time::SystemTime,
    /// The first user message exactly as saved.
    pub first_user: String,
}

/// True for a file that can hold a session: `*.jsonl` other than the prompt
/// history. Lock files use a different extension and never match.
pub fn is_session_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "jsonl")
        && path.file_name().and_then(|name| name.to_str()) != Some(PROMPT_HISTORY_FILE)
}

/// Session files under `dir` that have at least one user message, newest
/// first. Prompt history, lock files, and sessions that never got a prompt
/// are skipped. Every caller that lists sessions should use this.
pub fn list_session_files(dir: &Path) -> Vec<SessionFile> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) || !is_session_file(&path) {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(first_user) = first_user_message(&path) else {
            continue;
        };
        let modified = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        found.push(SessionFile {
            id: id.to_string(),
            path,
            modified,
            first_user,
        });
    }
    found.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.id.cmp(&right.id))
    });
    found
}

fn first_user_message(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Entry>(line)
            && let EntryKind::User(data) = entry.kind
        {
            return Some(data.content);
        }
    }
    None
}

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub path: Option<PathBuf>,
    pub tree: SessionTree,
    pub artifacts: Arc<ArtifactStore>,
    pub warnings: Vec<String>,
    store: Option<SessionStore>,
    next_seq: u64,
    redactor: Redactor,
}

impl Session {
    /// Create a new session. With `no_session` nothing is written to disk and
    /// artifacts stay in memory.
    pub fn create(paths: &Paths, header: SessionData, no_session: bool) -> Result<Self> {
        let redact = header.redact;
        if no_session {
            let id = "ephemeral".to_string();
            let artifacts = Arc::new(ArtifactStore::ephemeral_with(redact));
            let mut session = Self {
                id,
                path: None,
                tree: SessionTree::new(),
                artifacts,
                warnings: Vec::new(),
                store: None,
                next_seq: 1,
                redactor: Redactor::new(redact),
            };
            let header = session.make_entry(0, "s0", None, EntryKind::Session(header), None);
            session.tree.push(header);
            return Ok(session);
        }

        paths.ensure_dirs()?;
        // Ids are second-resolution plus the pid, so a second session created
        // by this process within the same second (`/new`) takes a suffix.
        let base = new_session_id();
        let mut id = base.clone();
        let mut suffix = 2;
        while paths.session_file(&id).exists() {
            id = format!("{base}-{suffix}");
            suffix += 1;
        }
        let path = paths.session_file(&id);
        let store = SessionStore::create(&path)
            .with_context(|| format!("cannot create session {}", path.display()))?;
        let artifacts_root = paths.artifacts_dir(&id);
        let artifacts = Arc::new(
            ArtifactStore::new_with(id.clone(), Some(artifacts_root), redact)
                .context("cannot create artifact directory")?,
        );
        let mut session = Self {
            id,
            path: Some(path),
            tree: SessionTree::new(),
            artifacts,
            warnings: Vec::new(),
            store: Some(store),
            next_seq: 1,
            redactor: Redactor::new(redact),
        };
        let header_entry = session.make_entry(0, "s0", None, EntryKind::Session(header), None);
        session.write_entry(&header_entry)?;
        session.tree.push(header_entry);
        Ok(session)
    }

    /// Open an existing session file, acquiring its advisory lock.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with(path, None)
    }

    /// Open a session with an explicit redaction setting. `None` follows the
    /// mode recorded in the header; old sessions default to redaction on.
    pub fn open_with(path: &Path, redact: Option<bool>) -> Result<Self> {
        let (store, entries) = SessionStore::open(path)
            .with_context(|| format!("cannot open session {}", path.display()))?;
        let (tree, warnings) = SessionTree::from_entries(entries);
        for warning in &warnings {
            tracing::warn!("{warning}");
        }
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "session".into());
        let redact = redact.unwrap_or_else(|| match tree.header().map(|entry| &entry.kind) {
            Some(EntryKind::Session(data)) => data.redact,
            _ => true,
        });
        let artifacts_root = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("artifacts")
            .join(&id);
        let artifacts = Arc::new(
            ArtifactStore::new_with(id.clone(), Some(artifacts_root), redact)
                .context("cannot create artifact directory")?,
        );
        let next_seq = tree.entries().iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        let mut session = Self {
            id,
            path: Some(path.to_path_buf()),
            tree,
            artifacts,
            warnings,
            store: Some(store),
            next_seq,
            redactor: Redactor::new(redact),
        };
        // Entries are replayed exactly as they were saved. Redaction is never
        // applied to replayed text: a model that sees a placeholder in place
        // of file content writes it back. A result that was redacted when
        // saved gets a note instead, and the file writers refuse to write
        // placeholders that are not already in the file.
        for entry in session.tree.entries_mut() {
            if let EntryKind::ToolResult(data) = &mut entry.kind
                && contains_placeholder(&data.content)
                && !data.content.ends_with(RESUMED_PLACEHOLDER_NOTE)
                && !data.content.ends_with(LEGACY_RESUMED_PLACEHOLDER_NOTE)
            {
                data.content.push_str(RESUMED_PLACEHOLDER_NOTE);
            }
        }
        Ok(session)
    }

    /// Most recently modified session that has a user message.
    pub fn latest_path(paths: &Paths) -> Result<Option<PathBuf>> {
        Ok(list_session_files(&paths.project_sessions_dir())
            .into_iter()
            .next()
            .map(|session| session.path))
    }

    pub fn header(&self) -> Option<&SessionData> {
        match self.tree.header().map(|e| &e.kind) {
            Some(EntryKind::Session(data)) => Some(data),
            _ => None,
        }
    }

    pub fn active_leaf_id(&self) -> Option<String> {
        self.tree.active_leaf_id()
    }

    pub fn context_path(&self) -> Vec<&Entry> {
        self.tree.context_path()
    }

    /// All entries on the active branch (including the header and compaction).
    pub fn branch_entries(&self) -> Vec<&Entry> {
        match self.active_leaf_id() {
            Some(leaf) => self.tree.path_to(&leaf),
            None => Vec::new(),
        }
    }

    pub fn title(&self) -> String {
        self.branch_entries()
            .into_iter()
            .find_map(|e| match &e.kind {
                EntryKind::User(u) => Some(u.content.lines().next().unwrap_or("").to_string()),
                _ => None,
            })
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "(empty session)".into())
    }

    /// Latest custom entry of `kind` on the active branch.
    pub fn latest_custom_on_path(&self, kind: &str) -> Option<&Entry> {
        self.branch_entries()
            .into_iter()
            .rev()
            .find(|e| matches!(&e.kind, EntryKind::Custom(c) if c.kind == kind))
    }

    pub fn customs_on_path(&self, kind: &str) -> Vec<&Entry> {
        self.branch_entries()
            .into_iter()
            .filter(|e| matches!(&e.kind, EntryKind::Custom(c) if c.kind == kind))
            .collect()
    }

    pub fn latest_verify_result(&self) -> Option<&Entry> {
        self.latest_custom_on_path("verify-result")
    }

    pub fn fsync(&mut self) -> Result<()> {
        if let Some(store) = &mut self.store {
            store.sync().context("fsync session")?;
        }
        Ok(())
    }

    pub fn redaction_enabled(&self) -> bool {
        self.redactor.enabled()
    }

    pub fn append(&mut self, kind: EntryKind) -> Result<Entry> {
        self.append_tagged(kind, None)
    }

    /// Append an entry with an explicit provenance tag. User entries default
    /// to `user`; tool results pass `tool:<name>` or `external`.
    pub fn append_tagged(&mut self, kind: EntryKind, provenance: Option<String>) -> Result<Entry> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let provenance = provenance.or_else(|| {
            matches!(kind, EntryKind::User(_)).then(|| crate::provenance::USER.to_string())
        });
        let checkout = matches!(kind, EntryKind::Checkout(_));
        let parent = if checkout {
            None
        } else {
            match self.active_leaf_id() {
                Some(id) if Some(id.as_str()) == self.tree.header().map(|h| h.id.as_str()) => None,
                other => other,
            }
        };
        let id = format!("{}{seq}", if checkout { 'c' } else { 'e' });
        // Only the copy written to disk is redacted. The in-memory tree is
        // what later prompts are built from, and it keeps the raw text so the
        // model never sees a placeholder in place of content it may write
        // back to a file.
        if self.store.is_some() && self.redactor.enabled() {
            let mut stored = kind.clone();
            Self::redact_entry(&self.redactor, &mut stored);
            let stored = self.make_entry(seq, &id, parent.clone(), stored, provenance.clone());
            self.write_entry(&stored)?;
            let entry = Entry {
                ts: stored.ts,
                ..self.make_entry(seq, &id, parent, kind, provenance)
            };
            self.tree.push(entry.clone());
            return Ok(entry);
        }
        let entry = self.make_entry(seq, &id, parent, kind, provenance);
        self.write_entry(&entry)?;
        self.tree.push(entry.clone());
        Ok(entry)
    }

    pub fn append_custom(&mut self, kind: impl Into<String>) -> Result<Entry> {
        let mut data = CustomData::new(kind);
        data.payload
            .insert("at".into(), Utc::now().to_rfc3339().into());
        self.append(EntryKind::Custom(data))
    }

    pub fn append_error(&mut self, code: &str, message: &str, recoverable: bool) -> Result<Entry> {
        self.append(EntryKind::Error(ErrorData {
            code: code.into(),
            message: message.into(),
            recoverable,
        }))
    }

    /// Move the active leaf to an already-validated entry. Unlike
    /// [`Session::checkout`] this accepts the session header, which a rewind
    /// of the first turn targets, and does not resolve a batch boundary.
    pub fn move_leaf(&mut self, target: &str, reason: &str) -> Result<()> {
        if self.tree.get(target).is_none() {
            bail!("entry {target} does not exist");
        }
        self.append(EntryKind::Checkout(CheckoutData {
            target: target.to_string(),
            reason: reason.to_string(),
        }))?;
        Ok(())
    }

    /// Checkout the resolved target and record a marker.
    pub fn checkout(&mut self, target: &str, reason: &str) -> Result<String> {
        let resolved = self
            .tree
            .resolve_checkout(target)
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        self.append(EntryKind::Checkout(CheckoutData {
            target: resolved.clone(),
            reason: reason.into(),
        }))?;
        Ok(resolved)
    }

    /// Markdown export of the active branch.
    pub fn export_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("# Totality session {}\n\n", self.id));
        if let Some(header) = self.header() {
            out.push_str(&format!(
                "- cwd: `{}`\n- endpoint: `{}`\n- model: `{}`\n- profile: `{}`\n\n",
                header.cwd, header.endpoint, header.model, header.profile
            ));
        }
        // The in-memory tree is raw. Redact each entry's fields first, as
        // they are written to the session file: a pass over the finished
        // markdown alone misses values inside rendered tool-call JSON.
        for entry in self.branch_entries() {
            let mut kind = entry.kind.clone();
            Self::redact_entry(&self.redactor, &mut kind);
            match &kind {
                EntryKind::Session(_) | EntryKind::Custom(_) => {}
                EntryKind::User(u) => {
                    out.push_str(&format!("## User\n\n{}\n\n", u.content));
                }
                EntryKind::Assistant(a) => {
                    out.push_str("## Assistant\n\n");
                    if let Some(reasoning) = &a.reasoning {
                        out.push_str(&format!(
                            "<details><summary>Reasoning</summary>\n\n{reasoning}\n\n</details>\n\n"
                        ));
                    }
                    if let Some(content) = &a.content {
                        out.push_str(content);
                        out.push_str("\n\n");
                    }
                    if let Some(calls) = &a.tool_calls {
                        for call in calls {
                            out.push_str(&format!(
                                "_call {} {}_\n\n```json\n{}\n```\n\n",
                                call.name,
                                call.id,
                                serde_json::to_string_pretty(&call.arguments).unwrap_or_default()
                            ));
                        }
                    }
                }
                EntryKind::ToolResult(t) => {
                    let status = if t.is_error { "error" } else { "ok" };
                    out.push_str(&format!("### Tool result ({status})\n\n```text\n"));
                    let body: String = t.content.chars().take(4000).collect();
                    out.push_str(&body);
                    if t.content.chars().count() > 4000 {
                        out.push_str("\n[truncated]");
                    }
                    out.push_str("\n```\n\n");
                }
                EntryKind::Compaction(c) => {
                    out.push_str(&format!(
                        "> compaction ({}):\n\n```text\n{}\n```\n\n",
                        c.reason, c.summary
                    ));
                }
                EntryKind::Error(e) => {
                    out.push_str(&format!("> error `{}`: {}\n\n", e.code, e.message));
                }
                EntryKind::Checkout(c) => {
                    out.push_str(&format!("> checkout to {} ({})\n\n", c.target, c.reason));
                }
            }
        }
        // A second pass over the whole document catches values that span
        // rendered fields. With --no-redact it is a no-op.
        self.redactor.redact_owned(&out)
    }

    /// Redact values in every entry kind before it reaches the in-memory tree
    /// or disk. Names, paths, and structural fields stay intact.
    fn redact_entry(redactor: &Redactor, kind: &mut EntryKind) {
        if !redactor.enabled() {
            return;
        }
        let redact = |text: &mut String| *text = redactor.redact_owned(text);
        match kind {
            EntryKind::Session(data) => {
                // The header records the mode; cwd and endpoint are paths.
                redact(&mut data.model);
                redact(&mut data.profile);
            }
            EntryKind::User(data) => {
                redact(&mut data.content);
                for attachment in &mut data.attachments {
                    redact(&mut attachment.content);
                }
            }
            EntryKind::Assistant(data) => {
                if let Some(content) = &mut data.content {
                    redact(content);
                }
                if let Some(reasoning) = &mut data.reasoning {
                    redact(reasoning);
                }
                if let Some(calls) = &mut data.tool_calls {
                    for call in calls {
                        redactor.redact_json(&mut call.arguments);
                    }
                }
            }
            EntryKind::ToolResult(data) => {
                redact(&mut data.content);
                if let Some(details) = &mut data.details {
                    redactor.redact_json(details);
                }
            }
            EntryKind::Compaction(data) => {
                redact(&mut data.summary);
                redact(&mut data.reason);
            }
            EntryKind::Checkout(data) => {
                // `target` is a path or commit; only the prose reason is a value.
                redact(&mut data.reason);
            }
            EntryKind::Custom(data) => {
                for value in data.payload.values_mut() {
                    redactor.redact_json(value);
                }
            }
            EntryKind::Error(data) => {
                redact(&mut data.message);
            }
        }
    }

    fn make_entry(
        &self,
        seq: u64,
        id: &str,
        parent: Option<String>,
        kind: EntryKind,
        provenance: Option<String>,
    ) -> Entry {
        Entry {
            v: FORMAT_VERSION,
            seq,
            id: id.to_string(),
            parent,
            ts: Utc::now(),
            provenance,
            kind,
        }
    }

    fn write_entry(&mut self, entry: &Entry) -> Result<()> {
        if let Some(store) = &mut self.store {
            store
                .append(entry)
                .with_context(|| format!("cannot append to session {}", self.id))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> SessionData {
        SessionData {
            cwd: "/tmp".into(),
            endpoint: "http://127.0.0.1:8080".into(),
            model: "test".into(),
            profile: "full".into(),
            app: APP_VERSION.into(),
            redact: true,
            context: None,
            permissions: None,
        }
    }

    fn paths(dir: &Path) -> Paths {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths.config_dir = dir.join("config");
        paths
    }

    #[test]
    fn user_entries_are_tagged_and_tool_results_take_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session = Session::create(&paths, header(), false).unwrap();
        let user = session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "hello".into(),
            }))
            .unwrap();
        assert_eq!(user.provenance.as_deref(), Some("user"));
        let result = session
            .append_tagged(
                EntryKind::ToolResult(ToolResultData {
                    call_id: "call_0001".into(),
                    content: "[tool] read - README.md\nhello".into(),
                    is_error: false,
                    details: None,
                    artifact: None,
                }),
                Some("tool:read".into()),
            )
            .unwrap();
        assert_eq!(result.provenance.as_deref(), Some("tool:read"));
        let raw = std::fs::read_to_string(session.path.as_ref().unwrap()).unwrap();
        assert!(raw.contains("\"provenance\":\"tool:read\""), "{raw}");
        assert!(raw.contains("\"provenance\":\"user\""), "{raw}");
    }

    #[test]
    fn first_user_entry_has_no_parent_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session = Session::create(&paths, header(), false).unwrap();
        let entry = session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "hello".into(),
            }))
            .unwrap();
        assert_eq!(entry.id, "e1");
        assert_eq!(entry.parent, None);
        session.fsync().unwrap();
        let path = session.path.clone().unwrap();
        drop(session);
        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.tree.len(), 2);
        assert_eq!(reopened.active_leaf_id().as_deref(), Some("e1"));
        assert_eq!(reopened.title(), "hello");
    }

    #[test]
    fn checkout_then_message_parents_to_target() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session = Session::create(&paths, header(), false).unwrap();
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "one".into(),
            }))
            .unwrap();
        session
            .append(EntryKind::Assistant(AssistantData {
                content: Some("two".into()),
                ..AssistantData::default()
            }))
            .unwrap();
        session.checkout("e1", "fork").unwrap();
        let entry = session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "branch".into(),
            }))
            .unwrap();
        assert_eq!(entry.parent.as_deref(), Some("e1"));
        assert_eq!(session.active_leaf_id().as_deref(), Some("e4"));
        assert_eq!(
            session
                .context_path()
                .iter()
                .map(|e| e.id.as_str())
                .collect::<Vec<_>>(),
            ["e1", "e4"]
        );
    }

    #[test]
    fn no_session_keeps_header_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session = Session::create(&paths, header(), true).unwrap();
        assert!(session.path.is_none());
        session
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "hi".into(),
            }))
            .unwrap();
        assert!(!dir.path().join("data").exists());
        assert_eq!(session.context_path().len(), 1);
    }

    #[test]
    fn session_listing_skips_prompt_history_locks_and_empty_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut first = Session::create(&paths, header(), false).unwrap();
        first
            .append(EntryKind::User(UserData {
                attachments: Vec::new(),
                content: "first prompt\nwith a second line".into(),
            }))
            .unwrap();
        first.fsync().unwrap();
        let first_path = first.path.clone().unwrap();
        drop(first);

        // A launch that never sent a message leaves a header-only file.
        let empty = Session::create(&paths, header(), false).unwrap();
        let empty_path = empty.path.clone().unwrap();
        drop(empty);

        let sessions_dir = paths.project_sessions_dir();
        std::fs::write(
            sessions_dir.join(PROMPT_HISTORY_FILE),
            "\"a recalled prompt\"\n",
        )
        .unwrap();
        std::fs::write(sessions_dir.join("session.jsonl.lock"), "pid 1\n").unwrap();
        std::fs::write(sessions_dir.join("broken.jsonl"), "not json\n").unwrap();

        let listed = list_session_files(&sessions_dir);
        assert_eq!(listed.len(), 1, "{listed:?}");
        assert_eq!(listed[0].path, first_path);
        assert_eq!(listed[0].first_user, "first prompt\nwith a second line");
        assert!(is_session_file(&first_path));
        assert!(is_session_file(&empty_path));
        assert!(!is_session_file(&sessions_dir.join(PROMPT_HISTORY_FILE)));
        assert!(!is_session_file(&sessions_dir.join("session.jsonl.lock")));

        // The empty, newer session is not `last`.
        assert_eq!(Session::latest_path(&paths).unwrap(), Some(first_path));
    }

    #[test]
    fn entries_are_redacted_before_they_reach_disk_or_export() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session = Session::create(&paths, header(), false).unwrap();
        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "c1".into(),
                content: format!("aws_access_key_id = {KEY}\n"),
                is_error: false,
                details: Some(serde_json::json!({"value": KEY})),
                artifact: None,
            }))
            .unwrap();
        session.fsync().unwrap();
        let path = session.path.clone().unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(KEY), "{raw}");
        assert!(raw.contains("[REDACTED:aws:AKIA..."), "{raw}");
        let export = session.export_markdown();
        assert!(!export.contains(KEY), "{export}");
        assert!(export.contains("[REDACTED:aws:AKIA..."), "{export}");
    }

    #[test]
    fn the_in_memory_tree_keeps_raw_text_while_disk_is_redacted() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session = Session::create(&paths, header(), false).unwrap();
        let content = format!("aws_access_key_id = {KEY}\n");
        let entry = session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "c1".into(),
                content: content.clone(),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        // The returned entry and the tree (what prompts are built from)
        // hold exactly what the tool produced.
        assert_eq!(entry.tool_result().unwrap().content, content);
        let id = entry.id.clone();
        let stored = session.tree.get(&id).unwrap();
        assert_eq!(stored.tool_result().unwrap().content, content);
        session.fsync().unwrap();
        let raw = std::fs::read_to_string(session.path.clone().unwrap()).unwrap();
        assert!(!raw.contains(KEY), "{raw}");
    }

    #[test]
    fn export_redacts_values_inside_tool_call_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session = Session::create(&paths, header(), false).unwrap();
        let password = format!("pw-{:x}", std::process::id() as u64 * 7919 + 104_729);
        let call = crate::session::entry::ToolCall {
            id: "c1".into(),
            name: "write".into(),
            arguments: serde_json::json!({
                "path": ".env",
                "content": format!("DB_PASSWORD={password}\n"),
            }),
        };
        session
            .append(EntryKind::Assistant(AssistantData {
                tool_calls: Some(vec![call]),
                ..AssistantData::default()
            }))
            .unwrap();
        let export = session.export_markdown();
        assert!(!export.contains(&password), "{export}");
        assert!(export.contains("DB_PASSWORD=[REDACTED:env:"), "{export}");
        session.fsync().unwrap();
        let raw = std::fs::read_to_string(session.path.clone().unwrap()).unwrap();
        assert!(!raw.contains(&password), "{raw}");
    }

    #[test]
    fn replayed_entries_are_not_redacted_and_placeholders_get_a_note() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        let dir = tempfile::tempdir().unwrap();
        let mut paths = paths(dir.path());
        paths.sessions_dir_override = Some(dir.path().join("sessions"));
        paths.ensure_dirs().unwrap();
        let path = paths.session_file("old");
        let now = Utc::now().to_rfc3339();
        let placeholder = Redactor::new(true).redact_owned(&format!("aws_key = {KEY}"));
        assert!(contains_placeholder(&placeholder), "{placeholder}");
        let lines = [
            serde_json::json!({
                "v": 1, "seq": 0, "id": "s0", "parent": null, "ts": now,
                "type": "session",
                "data": {
                    "cwd": "/tmp", "endpoint": "http://x", "model": "m",
                    "profile": "p", "app": "test"
                }
            }),
            // Written before redaction existed: replayed as saved.
            serde_json::json!({
                "v": 1, "seq": 1, "id": "e1", "parent": "s0", "ts": now,
                "type": "user",
                "data": {"content": "name = Column(Text)"}
            }),
            serde_json::json!({
                "v": 1, "seq": 2, "id": "e2", "parent": "e1", "ts": now,
                "type": "tool_result",
                "data": {"call_id": "c1", "content": placeholder, "is_error": false}
            }),
        ];
        let text: String = lines
            .iter()
            .map(|line| format!("{}\n", serde_json::to_string(line).unwrap()))
            .collect();
        std::fs::write(&path, text).unwrap();
        let session = Session::open(&path).unwrap();
        match &session.tree.get("e1").unwrap().kind {
            EntryKind::User(data) => assert_eq!(data.content, "name = Column(Text)"),
            _ => panic!("wrong entry"),
        }
        let result = session.tree.get("e2").unwrap().tool_result().unwrap();
        assert!(
            result.content.starts_with(&placeholder),
            "{}",
            result.content
        );
        assert!(
            result.content.ends_with(RESUMED_PLACEHOLDER_NOTE),
            "{}",
            result.content
        );
    }

    #[test]
    fn no_redact_is_recorded_in_the_header_and_keeps_raw_values() {
        const KEY: &str = "AKIAIOSFODNN7EXAMPLE";
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session_header = header();
        session_header.redact = false;
        let mut session = Session::create(&paths, session_header, false).unwrap();
        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "c1".into(),
                content: format!("aws_access_key_id = {KEY}\n"),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        session.fsync().unwrap();
        let path = session.path.clone().unwrap();
        drop(session);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains(KEY), "{raw}");
        let first = raw.lines().next().unwrap();
        let header: serde_json::Value = serde_json::from_str(first).unwrap();
        assert_eq!(header["data"]["redact"], false, "{header}");
        let reopened = Session::open(&path).unwrap();
        assert!(!reopened.redaction_enabled());
    }
}

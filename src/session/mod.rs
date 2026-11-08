//! Session state: append-only JSONL tree, artifacts, export.

pub mod artifact;
pub mod entry;
pub mod store;
pub mod tree;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;

pub use artifact::{Artifact, ArtifactError, ArtifactSlice, ArtifactStore};
pub use entry::{
    AssistantData, CheckoutData, CompactionData, CustomData, Entry, EntryKind, ErrorData,
    FORMAT_VERSION, SessionData, ToolCall, ToolResultData, UserData,
};
pub use store::{SessionStore, StoreError};
pub use tree::{CheckoutError, SessionTree};

use crate::paths::{Paths, new_session_id};

pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
pub struct Session {
    pub id: String,
    pub path: Option<PathBuf>,
    pub tree: SessionTree,
    pub artifacts: Arc<ArtifactStore>,
    pub warnings: Vec<String>,
    store: Option<SessionStore>,
    next_seq: u64,
}

impl Session {
    /// Create a new session. With `no_session` nothing is written to disk and
    /// artifacts stay in memory.
    pub fn create(paths: &Paths, header: SessionData, no_session: bool) -> Result<Self> {
        if no_session {
            let id = "ephemeral".to_string();
            let artifacts = Arc::new(ArtifactStore::ephemeral());
            let mut session = Self {
                id,
                path: None,
                tree: SessionTree::new(),
                artifacts,
                warnings: Vec::new(),
                store: None,
                next_seq: 1,
            };
            let header = session.make_entry(0, "s0", None, EntryKind::Session(header));
            session.tree.push(header);
            return Ok(session);
        }

        paths.ensure_dirs()?;
        let id = new_session_id();
        let path = paths.session_file(&id);
        let store = SessionStore::create(&path)
            .with_context(|| format!("cannot create session {}", path.display()))?;
        let artifacts_root = paths.artifacts_dir(&id);
        let artifacts = Arc::new(
            ArtifactStore::new(id.clone(), Some(artifacts_root))
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
        };
        let header_entry = session.make_entry(0, "s0", None, EntryKind::Session(header));
        session.write_entry(&header_entry)?;
        session.tree.push(header_entry);
        Ok(session)
    }

    /// Open an existing session file, acquiring its advisory lock.
    pub fn open(path: &Path) -> Result<Self> {
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
        let artifacts_root = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("artifacts")
            .join(&id);
        let artifacts = Arc::new(
            ArtifactStore::new(id.clone(), Some(artifacts_root))
                .context("cannot create artifact directory")?,
        );
        let next_seq = tree.entries().iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        Ok(Self {
            id,
            path: Some(path.to_path_buf()),
            tree,
            artifacts,
            warnings,
            store: Some(store),
            next_seq,
        })
    }

    /// Most recently modified session file for the project.
    pub fn latest_path(paths: &Paths) -> Result<Option<PathBuf>> {
        let dir = paths.project_sessions_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(None);
        };
        let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "jsonl")
                && let Ok(meta) = entry.metadata()
                && let Ok(modified) = meta.modified()
                && best.as_ref().is_none_or(|(t, _)| modified > *t)
            {
                best = Some((modified, path));
            }
        }
        Ok(best.map(|(_, path)| path))
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

    pub fn append(&mut self, kind: EntryKind) -> Result<Entry> {
        let seq = self.next_seq;
        self.next_seq += 1;
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
        let entry = self.make_entry(seq, &id, parent, kind);
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
        out.push_str(&format!("# LocalHarness session {}\n\n", self.id));
        if let Some(header) = self.header() {
            out.push_str(&format!(
                "- cwd: `{}`\n- endpoint: `{}`\n- model: `{}`\n- profile: `{}`\n\n",
                header.cwd, header.endpoint, header.model, header.profile
            ));
        }
        for entry in self.context_path() {
            match &entry.kind {
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
        out
    }

    fn make_entry(&self, seq: u64, id: &str, parent: Option<String>, kind: EntryKind) -> Entry {
        Entry {
            v: FORMAT_VERSION,
            seq,
            id: id.to_string(),
            parent,
            ts: Utc::now(),
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
        }
    }

    fn paths(dir: &Path) -> Paths {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        paths.config_dir = dir.join("config");
        paths
    }

    #[test]
    fn first_user_entry_has_no_parent_and_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        let mut session = Session::create(&paths, header(), false).unwrap();
        let entry = session
            .append(EntryKind::User(UserData {
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
                content: "hi".into(),
            }))
            .unwrap();
        assert!(!dir.path().join("data").exists());
        assert_eq!(session.context_path().len(), 1);
    }
}

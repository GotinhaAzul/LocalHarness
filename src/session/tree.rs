//! Parent/child index over session entries: active leaf, branch paths, checkout
//! resolution.

use std::collections::HashMap;
use std::fmt;

use super::entry::{Entry, EntryKind, ToolCall};

#[derive(Debug, Default)]
pub struct SessionTree {
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
}

impl SessionTree {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a tree, dropping entries whose parent is missing and checkouts
    /// whose target is missing. Returns the tree and human-readable warnings.
    pub fn from_entries(entries: Vec<Entry>) -> (Self, Vec<String>) {
        let mut tree = SessionTree::new();
        let mut warnings = Vec::new();
        for entry in entries {
            if tree.index.contains_key(&entry.id) {
                warnings.push(format!("dropped duplicate entry id {}", entry.id));
                continue;
            }
            if let Some(parent) = &entry.parent
                && !tree.index.contains_key(parent)
            {
                warnings.push(format!(
                    "dropped orphan entry {} (parent {} missing)",
                    entry.id, parent
                ));
                continue;
            }
            if let EntryKind::Checkout(c) = &entry.kind
                && !tree.index.contains_key(&c.target)
            {
                warnings.push(format!(
                    "dropped checkout {} (target {} missing)",
                    entry.id, c.target
                ));
                continue;
            }
            tree.push(entry);
        }
        (tree, warnings)
    }

    pub fn push(&mut self, entry: Entry) {
        self.index.insert(entry.id.clone(), self.entries.len());
        self.entries.push(entry);
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.index.get(id).map(|i| &self.entries[*i])
    }

    pub fn last(&self) -> Option<&Entry> {
        self.entries.last()
    }

    pub fn header(&self) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|e| matches!(e.kind, EntryKind::Session(_)))
    }

    /// Last checkout marker on the file, if any.
    pub fn last_checkout(&self) -> Option<&Entry> {
        self.entries.iter().rev().find(|e| e.is_checkout())
    }

    /// Last non-checkout entry in the file.
    pub fn last_entry(&self) -> Option<&Entry> {
        self.entries.iter().rev().find(|e| !e.is_checkout())
    }

    /// The active leaf: the target of a trailing checkout, else the last
    /// non-checkout entry.
    pub fn active_leaf_id(&self) -> Option<String> {
        match self.entries.last() {
            Some(entry) if entry.is_checkout() => {
                let mut cursor = entry;
                let mut guard = 0;
                while let EntryKind::Checkout(c) = &cursor.kind {
                    guard += 1;
                    if guard > 64 {
                        return self.last_entry().map(|e| e.id.clone());
                    }
                    cursor = self.get(&c.target)?;
                }
                Some(cursor.id.clone())
            }
            Some(entry) => Some(entry.id.clone()),
            None => None,
        }
    }

    pub fn active_leaf(&self) -> Option<&Entry> {
        self.active_leaf_id().and_then(|id| self.get(&id))
    }

    /// Path from the root to `id` inclusive. Checkout markers never appear.
    pub fn path_to(&self, id: &str) -> Vec<&Entry> {
        let mut path = Vec::new();
        let mut cursor = id;
        let mut guard = 0usize;
        while let Some(entry) = self.get(cursor) {
            path.push(entry);
            guard += 1;
            if guard > self.entries.len() + 4 {
                break;
            }
            match &entry.parent {
                Some(parent) => cursor = parent,
                None => break,
            }
        }
        path.reverse();
        path
    }

    /// Entries that form the provider context: the active branch after the
    /// latest compaction, compaction entry included.
    pub fn context_path(&self) -> Vec<&Entry> {
        let Some(leaf) = self.active_leaf_id() else {
            return Vec::new();
        };
        let path = self.path_to(&leaf);
        match path.iter().rposition(|e| e.is_compaction()) {
            Some(pos) => path[pos..].to_vec(),
            None => path,
        }
    }

    /// Direct children in file order, excluding checkout markers.
    pub fn children(&self, id: &str) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| !e.is_checkout() && e.parent.as_deref() == Some(id))
            .collect()
    }

    /// Entries without a parent (normally just the first user entry and the
    /// header), excluding checkouts.
    pub fn roots(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|e| !e.is_checkout() && e.parent.is_none())
            .collect()
    }

    fn assistant_calls(&self, id: &str) -> Option<Vec<&ToolCall>> {
        let entry = self.get(id)?;
        match &entry.kind {
            EntryKind::Assistant(a) => Some(
                a.tool_calls
                    .as_ref()
                    .map(|calls| calls.iter().collect())
                    .unwrap_or_default(),
            ),
            _ => None,
        }
    }

    /// The tool-result chain answering the assistant entry `assistant_id`.
    pub fn batch_results(&self, assistant_id: &str) -> Vec<&Entry> {
        let mut results = Vec::new();
        let mut cursor = assistant_id.to_string();
        loop {
            let next = self
                .entries
                .iter()
                .find(|e| e.is_tool_result() && e.parent.as_deref() == Some(cursor.as_str()));
            match next {
                Some(entry) => {
                    results.push(entry);
                    cursor = entry.id.clone();
                    if results.len() > 256 {
                        break;
                    }
                }
                None => break,
            }
        }
        results
    }

    /// Assistant entry owning a tool result, if any.
    pub fn owner_of_result(&self, id: &str) -> Option<String> {
        let mut entry = self.get(id)?;
        let mut guard = 0;
        while entry.is_tool_result() {
            guard += 1;
            if guard > 256 {
                return None;
            }
            let parent = entry.parent.as_ref()?;
            entry = self.get(parent)?;
            if entry.is_assistant() {
                return Some(entry.id.clone());
            }
        }
        None
    }

    /// Whether the assistant's tool batch has all its results.
    pub fn complete_batch(&self, assistant_id: &str) -> Option<Vec<String>> {
        let calls = self.assistant_calls(assistant_id)?;
        if calls.is_empty() {
            return Some(Vec::new());
        }
        let results = self.batch_results(assistant_id);
        if results.len() != calls.len() {
            return None;
        }
        Some(results.iter().map(|e| e.id.clone()).collect())
    }

    /// Resolve a UI checkout target to a completed step boundary.
    pub fn resolve_checkout(&self, target: &str) -> Result<String, CheckoutError> {
        let entry = self
            .get(target)
            .ok_or_else(|| CheckoutError::NotFound(target.to_string()))?;
        match &entry.kind {
            EntryKind::Session(_) => Err(CheckoutError::SessionHeader),
            EntryKind::Checkout(_) => Err(CheckoutError::Marker(target.to_string())),
            EntryKind::Assistant(a) => {
                if a.tool_calls.as_ref().is_some_and(|c| !c.is_empty()) {
                    match self.complete_batch(target) {
                        Some(results) => Ok(results
                            .last()
                            .cloned()
                            .unwrap_or_else(|| target.to_string())),
                        None => Err(CheckoutError::IncompleteBatch {
                            have: self.batch_results(target).len(),
                            want: a.tool_calls.as_ref().map(|c| c.len()).unwrap_or(0),
                        }),
                    }
                } else {
                    Ok(target.to_string())
                }
            }
            EntryKind::ToolResult(_) => {
                let Some(owner) = self.owner_of_result(target) else {
                    return Ok(target.to_string());
                };
                let calls = self.assistant_calls(&owner).unwrap_or_default();
                if calls.is_empty() {
                    return Ok(target.to_string());
                }
                match self.complete_batch(&owner) {
                    Some(results) => Ok(results.last().cloned().unwrap_or(owner)),
                    None => Err(CheckoutError::IncompleteBatch {
                        have: self.batch_results(&owner).len(),
                        want: calls.len(),
                    }),
                }
            }
            _ => Ok(target.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutError {
    NotFound(String),
    Marker(String),
    SessionHeader,
    IncompleteBatch { have: usize, want: usize },
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CheckoutError::NotFound(id) => write!(f, "entry {id} does not exist"),
            CheckoutError::Marker(id) => write!(f, "entry {id} is a checkout marker, not a step"),
            CheckoutError::SessionHeader => write!(f, "the session header is not a step"),
            CheckoutError::IncompleteBatch { have, want } => write!(
                f,
                "step incomplete ({have} of {want} tool results); choose the last completed result"
            ),
        }
    }
}

impl std::error::Error for CheckoutError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entry::*;
    use chrono::Utc;

    fn entry(seq: u64, parent: Option<&str>, kind: EntryKind) -> Entry {
        Entry {
            v: 1,
            seq,
            id: format!("e{seq}"),
            parent: parent.map(str::to_string),
            ts: Utc::now(),
            kind,
        }
    }

    fn user(seq: u64, parent: Option<&str>, text: &str) -> Entry {
        entry(
            seq,
            parent,
            EntryKind::User(UserData {
                attachments: Vec::new(),
                content: text.into(),
            }),
        )
    }

    fn assistant_with_calls(seq: u64, parent: Option<&str>, calls: usize) -> Entry {
        let calls = (0..calls)
            .map(|i| ToolCall {
                id: format!("call_{i}"),
                name: "read".into(),
                arguments: serde_json::json!({"path":"x"}),
            })
            .collect();
        entry(
            seq,
            parent,
            EntryKind::Assistant(AssistantData {
                content: None,
                reasoning: None,
                tool_calls: Some(calls),
                usage: None,
                stop_reason: None,
            }),
        )
    }

    fn result(seq: u64, parent: &str, call_id: &str) -> Entry {
        entry(
            seq,
            Some(parent),
            EntryKind::ToolResult(ToolResultData {
                call_id: call_id.into(),
                content: "ok".into(),
                is_error: false,
                details: None,
                artifact: None,
            }),
        )
    }

    fn checkout(seq: u64, target: &str) -> Entry {
        Entry {
            v: 1,
            seq,
            id: format!("c{seq}"),
            parent: None,
            ts: Utc::now(),
            kind: EntryKind::Checkout(CheckoutData {
                target: target.into(),
                reason: "fork".into(),
            }),
        }
    }

    #[test]
    fn path_and_active_leaf_after_checkout() {
        let entries = vec![
            user(1, None, "hi"),
            assistant_with_calls(2, Some("e1"), 1),
            result(3, "e2", "call_0"),
            user(4, Some("e3"), "answer"),
            checkout(5, "e3"),
        ];
        let (tree, warnings) = SessionTree::from_entries(entries);
        assert!(warnings.is_empty());
        assert_eq!(tree.active_leaf_id().as_deref(), Some("e3"));
        let path = tree.path_to("e3");
        assert_eq!(
            path.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["e1", "e2", "e3"]
        );
    }

    #[test]
    fn checkout_resolves_batch_to_last_result() {
        let entries = vec![
            user(1, None, "hi"),
            assistant_with_calls(2, Some("e1"), 2),
            result(3, "e2", "call_0"),
            result(4, "e3", "call_1"),
            user(5, Some("e4"), "next"),
        ];
        let (tree, _) = SessionTree::from_entries(entries);
        assert_eq!(tree.resolve_checkout("e2").unwrap(), "e4");
        assert_eq!(tree.resolve_checkout("e3").unwrap(), "e4");
        assert_eq!(tree.resolve_checkout("e4").unwrap(), "e4");
    }

    #[test]
    fn incomplete_batch_is_not_a_target() {
        let entries = vec![
            user(1, None, "hi"),
            assistant_with_calls(2, Some("e1"), 2),
            result(3, "e2", "call_0"),
        ];
        let (tree, _) = SessionTree::from_entries(entries);
        assert!(matches!(
            tree.resolve_checkout("e3"),
            Err(CheckoutError::IncompleteBatch { have: 1, want: 2 })
        ));
    }

    #[test]
    fn context_path_starts_at_latest_compaction() {
        let entries = vec![
            user(1, None, "old"),
            entry(
                2,
                Some("e1"),
                EntryKind::Compaction(CompactionData {
                    summary: "s".into(),
                    tokens_before: 10,
                    reason: "auto".into(),
                    thread_id: None,
                }),
            ),
            user(3, Some("e2"), "new"),
        ];
        let (tree, _) = SessionTree::from_entries(entries);
        let ctx = tree.context_path();
        assert_eq!(ctx.len(), 2);
        assert!(ctx[0].is_compaction());
    }
}

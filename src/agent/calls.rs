//! Session-scoped tool-call ids.
//!
//! Engines are unreliable id authorities: some return ids their own template
//! later rejects, Mistral via vLLM truncates them, Ollama's native format has
//! none, and several servers reuse one id for every call. The harness therefore
//! never stores an engine id in the session. Every call gets a monotonically
//! increasing internal id (`call_0001`, `call_0002`, ...) derived from the
//! session history, so replay pairs a call with exactly one result even after
//! compaction.

use std::collections::BTreeMap;

use crate::session::{Entry, EntryKind, Session};

/// Allocate internal call ids. One ledger lives for a turn; ids are derived
/// from the whole session tree so checking out an older branch still produces
/// ids that never collide with what was written before.
#[derive(Debug, Clone)]
pub struct CallLedger {
    next: u64,
    /// Engine id → internal id for the current request. Never persisted.
    engine_ids: BTreeMap<String, String>,
}

impl Default for CallLedger {
    fn default() -> Self {
        Self {
            next: 1,
            engine_ids: BTreeMap::new(),
        }
    }
}

impl CallLedger {
    pub fn from_session(session: &Session) -> Self {
        Self::from_entries(session.tree.entries())
    }

    pub fn from_entries(entries: &[Entry]) -> Self {
        let highest = entries
            .iter()
            .flat_map(entry_ids)
            .filter_map(internal_number)
            .max()
            .unwrap_or(0);
        Self {
            next: highest.saturating_add(1),
            engine_ids: BTreeMap::new(),
        }
    }

    /// The next internal id, for example `call_0007`.
    pub fn next_id(&mut self) -> String {
        let id = format!("call_{:04}", self.next);
        self.next = self.next.saturating_add(1);
        id
    }

    /// Remember which engine id produced which internal id for this request.
    pub fn map_engine_id(&mut self, engine_id: &str, internal_id: &str) {
        let engine_id = engine_id.trim();
        if !engine_id.is_empty() {
            self.engine_ids
                .insert(engine_id.to_string(), internal_id.to_string());
        }
    }

    pub fn internal_for(&self, engine_id: &str) -> Option<&str> {
        self.engine_ids.get(engine_id).map(String::as_str)
    }

    pub fn engine_for(&self, internal_id: &str) -> Option<&str> {
        self.engine_ids
            .iter()
            .find_map(|(engine, internal)| (internal == internal_id).then_some(engine.as_str()))
    }

    /// Advance past an id already present in history.
    pub fn observe(&mut self, id: &str) {
        if let Some(number) = internal_number(id) {
            self.next = self.next.max(number.saturating_add(1));
        }
    }

    /// Number of ids handed out by this ledger.
    pub fn issued(&self) -> u64 {
        self.next.saturating_sub(1)
    }
}

fn entry_ids(entry: &Entry) -> Vec<&str> {
    match &entry.kind {
        EntryKind::Assistant(assistant) => assistant
            .tool_calls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|call| call.id.as_str())
            .collect(),
        EntryKind::ToolResult(result) => vec![result.call_id.as_str()],
        _ => Vec::new(),
    }
}

/// The sequence number in a harness-owned id. Engine ids such as `call_1` or
/// `chatcmpl-abc` are deliberately ignored: they are not ours to continue.
fn internal_number(id: &str) -> Option<u64> {
    let number = id.strip_prefix("call_")?;
    if number.len() < 4 || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use crate::session::{AssistantData, SessionData, ToolCall, ToolResultData};
    use serde_json::json;

    fn session(dir: &std::path::Path) -> Session {
        let mut paths = Paths::discover(dir).unwrap();
        paths.data_dir = dir.join("data");
        Session::create(
            &paths,
            SessionData {
                cwd: dir.display().to_string(),
                endpoint: "e".into(),
                model: "m".into(),
                profile: "full".into(),
                app: "test".into(),
                context: None,
                redact: true,
            },
            true,
        )
        .unwrap()
    }

    fn call(id: &str) -> AssistantData {
        AssistantData {
            tool_calls: Some(vec![ToolCall {
                id: id.into(),
                name: "read".into(),
                arguments: json!({"path": "a.rs"}),
            }]),
            ..AssistantData::default()
        }
    }

    #[test]
    fn ids_start_at_one_and_increase() {
        let mut ledger = CallLedger::default();
        assert_eq!(ledger.next_id(), "call_0001");
        assert_eq!(ledger.next_id(), "call_0002");
        assert_eq!(ledger.issued(), 2);
    }

    #[test]
    fn ids_continue_after_the_highest_id_in_history() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::Assistant(call("call_0007")))
            .unwrap();
        session
            .append(EntryKind::ToolResult(ToolResultData {
                call_id: "call_0007".into(),
                content: "ok".into(),
                is_error: false,
                details: None,
                artifact: None,
            }))
            .unwrap();
        let mut ledger = CallLedger::from_session(&session);
        assert_eq!(ledger.next_id(), "call_0008");
    }

    #[test]
    fn engine_ids_are_mapped_but_never_continued() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = session(dir.path());
        session
            .append(EntryKind::Assistant(call("call_1")))
            .unwrap();
        session
            .append(EntryKind::Assistant(call("chatcmpl-abc")))
            .unwrap();
        let mut ledger = CallLedger::from_session(&session);
        assert_eq!(
            ledger.next_id(),
            "call_0001",
            "foreign ids must not move the internal sequence"
        );
        ledger.map_engine_id("call_1", "call_0001");
        assert_eq!(ledger.internal_for("call_1"), Some("call_0001"));
        assert_eq!(ledger.engine_for("call_0001"), Some("call_1"));
    }
}

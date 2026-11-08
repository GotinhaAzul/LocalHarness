//! `/tree` modal: a flattened, depth-first view of the session tree.

use chrono::{DateTime, Utc};

use crate::session::{CheckoutError, Entry, EntryKind, Session, SessionTree};

/// One display row of the tree modal.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeRow {
    pub id: String,
    pub depth: usize,
    /// Branch glyphs for this row.
    pub prefix: String,
    pub role: char,
    pub preview: String,
    pub ts: Option<DateTime<Utc>>,
    /// Rough token estimate for the entry.
    pub tokens: u64,
    /// True when this entry is the active leaf.
    pub active: bool,
}

/// Selection and scroll state for the tree modal.
#[derive(Debug, Clone, Default)]
pub struct TreeView {
    pub rows: Vec<TreeRow>,
    pub selected: usize,
    pub scroll: usize,
}

impl TreeView {
    pub fn new(session: &Session) -> Self {
        Self::from_tree(&session.tree, session.active_leaf_id().as_deref())
    }

    pub fn from_tree(tree: &SessionTree, active: Option<&str>) -> Self {
        let mut rows = Vec::new();
        let roots: Vec<&Entry> = tree
            .roots()
            .into_iter()
            .filter(|entry| is_displayed(entry))
            .collect();
        let last = roots.len().saturating_sub(1);
        for (index, root) in roots.into_iter().enumerate() {
            walk(tree, root, 0, "", "", index == last, active, &mut rows);
        }
        TreeView {
            rows,
            selected: 0,
            scroll: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn selected_row(&self) -> Option<&TreeRow> {
        self.rows.get(self.selected)
    }

    pub fn move_by(&mut self, delta: isize) {
        if self.rows.is_empty() {
            self.selected = 0;
            return;
        }
        let max = self.rows.len() - 1;
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, max as isize) as usize;
    }

    pub fn page(&mut self, direction: isize, page: usize) {
        let step = page.max(1) as isize;
        self.move_by(direction * step);
    }

    pub fn home(&mut self) {
        if !self.rows.is_empty() {
            self.selected = 0;
        }
    }

    pub fn end(&mut self) {
        if !self.rows.is_empty() {
            self.selected = self.rows.len() - 1;
        }
    }

    /// Keep the selected row inside a viewport of `height` rows.
    pub fn ensure_visible(&mut self, height: usize) {
        let height = height.max(1);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + height {
            self.scroll = self.selected + 1 - height;
        }
        let max = self.rows.len().saturating_sub(height);
        self.scroll = self.scroll.min(max);
    }

    /// Resolve the selected row to a checkout target using the session rules.
    pub fn resolve(&self, tree: &SessionTree) -> Result<String, CheckoutError> {
        let row = self
            .selected_row()
            .ok_or_else(|| CheckoutError::NotFound("(empty tree)".into()))?;
        tree.resolve_checkout(&row.id)
    }
}

fn is_displayed(entry: &Entry) -> bool {
    !matches!(
        entry.kind,
        EntryKind::Session(_) | EntryKind::Checkout(_) | EntryKind::Custom(_)
    )
}

#[allow(clippy::too_many_arguments)]
fn walk(
    tree: &SessionTree,
    entry: &Entry,
    depth: usize,
    prefix: &str,
    child_prefix: &str,
    _last: bool,
    active: Option<&str>,
    rows: &mut Vec<TreeRow>,
) {
    rows.push(TreeRow {
        id: entry.id.clone(),
        depth,
        prefix: prefix.to_string(),
        role: entry.role_char(),
        preview: entry.preview(72),
        ts: Some(entry.ts),
        tokens: estimate_tokens(entry),
        active: active == Some(entry.id.as_str()),
    });
    if depth > 256 {
        return;
    }
    let children: Vec<&Entry> = tree
        .children(&entry.id)
        .into_iter()
        .filter(|child| is_displayed(child))
        .collect();
    let last = children.len().saturating_sub(1);
    for (index, child) in children.into_iter().enumerate() {
        let child_last = index == last;
        let connector = if child_last { "`- " } else { "|- " };
        let row_prefix = format!("{child_prefix}{connector}");
        let next_prefix = format!("{child_prefix}{}", if child_last { "   " } else { "|  " });
        walk(
            tree,
            child,
            depth + 1,
            &row_prefix,
            &next_prefix,
            child_last,
            active,
            rows,
        );
    }
}

/// Coarse token estimate: characters divided by four.
pub fn estimate_tokens(entry: &Entry) -> u64 {
    let chars = match &entry.kind {
        EntryKind::User(data) => data.content.chars().count(),
        EntryKind::Assistant(data) => {
            data.content
                .as_deref()
                .map(str::chars)
                .map(|c| c.count())
                .unwrap_or(0)
                + data
                    .reasoning
                    .as_deref()
                    .map(str::chars)
                    .map(|c| c.count())
                    .unwrap_or(0)
                + data
                    .tool_calls
                    .as_ref()
                    .map(|calls| {
                        calls
                            .iter()
                            .map(|call| call.name.len() + call.arguments.to_string().len())
                            .sum::<usize>()
                    })
                    .unwrap_or(0)
        }
        EntryKind::ToolResult(data) => data.content.chars().count(),
        EntryKind::Compaction(data) => data.summary.chars().count(),
        EntryKind::Error(data) => data.message.chars().count(),
        _ => 0,
    };
    (chars as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{
        AssistantData, CompactionData, ErrorData, ToolCall, ToolResultData, UserData,
    };
    use serde_json::json;

    fn entry(seq: u64, id: &str, parent: Option<&str>, kind: EntryKind) -> Entry {
        Entry {
            v: 1,
            seq,
            id: id.into(),
            parent: parent.map(str::to_string),
            ts: Utc::now(),
            kind,
        }
    }

    fn user(seq: u64, id: &str, parent: Option<&str>, text: &str) -> Entry {
        entry(
            seq,
            id,
            parent,
            EntryKind::User(UserData {
                content: text.into(),
            }),
        )
    }

    fn assistant(seq: u64, id: &str, parent: &str, calls: usize, text: &str) -> Entry {
        let tool_calls = (0..calls)
            .map(|index| ToolCall {
                id: format!("{id}-call-{index}"),
                name: "read".into(),
                arguments: json!({"path": "src/lib.rs"}),
            })
            .collect::<Vec<_>>();
        entry(
            seq,
            id,
            Some(parent),
            EntryKind::Assistant(AssistantData {
                content: Some(text.into()),
                reasoning: None,
                tool_calls: if calls == 0 { None } else { Some(tool_calls) },
                usage: None,
                stop_reason: None,
            }),
        )
    }

    fn result(seq: u64, id: &str, parent: &str, call_id: &str) -> Entry {
        entry(
            seq,
            id,
            Some(parent),
            EntryKind::ToolResult(ToolResultData {
                call_id: call_id.into(),
                content: "142 lines".into(),
                is_error: false,
                details: None,
                artifact: None,
            }),
        )
    }

    /// `e3` carries two children; one is continued (`e4`) and one closes the
    /// branch (`e5`), and `e6` descends from `e5`.
    fn fixture() -> SessionTree {
        let mut tree = SessionTree::new();
        tree.push(user(1, "e1", None, "Fix the parser bug"));
        tree.push(assistant(2, "e2", "e1", 1, "I will inspect it."));
        tree.push(result(3, "e3", "e2", "e2-call-0"));
        tree.push(assistant(4, "e4", "e3", 0, "Found it."));
        tree.push(user(5, "e5", Some("e3"), "Try another approach"));
        tree.push(assistant(6, "e6", "e5", 0, "I will check the lexer."));
        tree
    }

    #[test]
    fn renders_two_branches_with_connectors_and_active_marker() {
        let tree = fixture();
        let view = TreeView::from_tree(&tree, Some("e6"));
        assert_eq!(view.rows.len(), 6);
        assert_eq!(view.rows[0].role, 'U');
        assert_eq!(view.rows[1].role, 'A');
        assert_eq!(view.rows[2].role, 'R');
        // e4 is the first of two children, e5 the last.
        assert_eq!(view.rows[3].prefix, "      |- ");
        assert_eq!(view.rows[4].prefix, "      `- ");
        assert_eq!(view.rows[5].prefix, "         `- ");
        let active: Vec<&str> = view
            .rows
            .iter()
            .filter(|row| row.active)
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(active, vec!["e6"]);
        assert_eq!(view.rows[0].preview, "Fix the parser bug");
        assert!(view.rows[2].tokens > 0);
    }

    #[test]
    fn selection_resolves_through_tool_batches() {
        let tree = fixture();
        let mut view = TreeView::from_tree(&tree, Some("e6"));
        view.selected = 1; // assistant with one call
        assert_eq!(view.resolve(&tree).unwrap(), "e3");
        view.selected = 3; // assistant without calls
        assert_eq!(view.resolve(&tree).unwrap(), "e4");
        view.selected = 2; // result resolves to its batch end
        assert_eq!(view.resolve(&tree).unwrap(), "e3");
    }

    #[test]
    fn incomplete_batch_is_reported_not_selected() {
        let mut tree = SessionTree::new();
        tree.push(user(1, "e1", None, "go"));
        tree.push(assistant(2, "e2", "e1", 2, "calling twice"));
        tree.push(result(3, "e3", "e2", "e2-call-0"));
        let view = TreeView::from_tree(&tree, Some("e3"));
        let error = tree.resolve_checkout("e2").unwrap_err();
        assert_eq!(error, CheckoutError::IncompleteBatch { have: 1, want: 2 });
        assert!(error.to_string().contains("incomplete"));
        assert_eq!(view.rows.len(), 3);
    }

    #[test]
    fn movement_and_visibility_are_clamped() {
        let tree = fixture();
        let mut view = TreeView::from_tree(&tree, Some("e6"));
        view.move_by(-5);
        assert_eq!(view.selected, 0);
        view.move_by(99);
        assert_eq!(view.selected, view.rows.len() - 1);
        view.ensure_visible(2);
        assert_eq!(view.scroll, view.rows.len() - 2);
        view.home();
        view.ensure_visible(2);
        assert_eq!(view.scroll, 0);
    }

    #[test]
    fn compaction_and_error_glyphs_are_kept() {
        let mut tree = SessionTree::new();
        tree.push(user(1, "e1", None, "go"));
        tree.push(entry(
            2,
            "e2",
            Some("e1"),
            EntryKind::Compaction(CompactionData {
                summary: "summary".into(),
                tokens_before: 10,
                reason: "manual".into(),
                thread_id: None,
            }),
        ));
        tree.push(entry(
            3,
            "e3",
            Some("e2"),
            EntryKind::Error(ErrorData {
                code: "guard".into(),
                message: "stopped".into(),
                recoverable: true,
            }),
        ));
        let view = TreeView::from_tree(&tree, Some("e3"));
        assert_eq!(view.rows[1].role, 'C');
        assert_eq!(view.rows[2].role, 'E');
    }
}

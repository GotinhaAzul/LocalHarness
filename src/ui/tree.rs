//! `/tree` modal: a flattened, depth-first view of the session tree.
//!
//! Tool results fold into the assistant step that called them, and entries
//! the user never sees (the session header, checkout markers, `custom`
//! bookkeeping such as `tool-probe` or `fs-change`) are looked through: their
//! visible descendants keep the place in the tree they would have had.

use std::collections::{HashMap, HashSet};

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
    /// Rough token estimate for the entry and its folded tool results.
    pub tokens: u64,
    /// Tool calls this assistant step made; their results are folded in.
    pub tools: usize,
    /// True when this row is where the conversation currently stands.
    pub active: bool,
    /// True when the row is on the active branch.
    pub on_path: bool,
}

/// Where choosing a row moves the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    /// Entry the leaf moves to. It can be the session header, which means
    /// "before the first message".
    pub target: String,
    /// The user message the destination sits just before, to put back in the
    /// prompt for editing and resending.
    pub prefill: Option<String>,
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
        let path: Vec<&Entry> = active.map(|id| tree.path_to(id)).unwrap_or_default();
        let on_path: HashSet<&str> = path.iter().map(|entry| entry.id.as_str()).collect();
        // The leaf is often hidden (a tool result, an `fs-change` record, a
        // rewind marker); the row that stands for it is its nearest visible
        // ancestor.
        let active_row = path
            .iter()
            .rev()
            .find(|entry| is_displayed(entry))
            .map(|entry| entry.id.clone());
        // One pass over the entries; `SessionTree::children` scans them all.
        let mut children: HashMap<&str, Vec<&Entry>> = HashMap::new();
        for entry in tree.entries() {
            if let (Some(parent), false) = (entry.parent.as_deref(), entry.is_checkout()) {
                children.entry(parent).or_default().push(entry);
            }
        }
        let ctx = Walk {
            tree,
            children,
            active: active_row.as_deref(),
            on_path: &on_path,
        };
        let mut rows = Vec::new();
        let mut roots = Vec::new();
        for root in tree.roots() {
            if is_displayed(root) {
                roots.push(root);
            } else {
                roots.extend(ctx.visible_children(&root.id));
            }
        }
        ctx.walk_children(&roots, 0, "", &mut rows);
        let selected = rows.iter().position(|row| row.active).unwrap_or(0);
        TreeView {
            rows,
            selected,
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

    /// Select the row standing for the active leaf.
    pub fn select_active(&mut self) {
        if let Some(index) = self.rows.iter().position(|row| row.active) {
            self.selected = index;
        }
    }

    /// Jump to the previous (`-1`) or next (`1`) user message.
    pub fn jump_user(&mut self, direction: isize) {
        let mut index = self.selected as isize + direction;
        while index >= 0 && (index as usize) < self.rows.len() {
            if self.rows[index as usize].role == 'U' {
                self.selected = index as usize;
                return;
            }
            index += direction;
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

    /// Where choosing the selected row goes. A user message means "go back to
    /// just before it" with its text ready to edit; any other row continues
    /// the conversation from that step.
    pub fn destination(&self, tree: &SessionTree) -> Result<Destination, CheckoutError> {
        let row = self
            .selected_row()
            .ok_or_else(|| CheckoutError::NotFound("(empty tree)".into()))?;
        destination_for(tree, &row.id)
    }
}

/// See [`TreeView::destination`].
pub fn destination_for(tree: &SessionTree, id: &str) -> Result<Destination, CheckoutError> {
    let entry = tree
        .get(id)
        .ok_or_else(|| CheckoutError::NotFound(id.to_string()))?;
    if let EntryKind::User(user) = &entry.kind {
        let target = match &entry.parent {
            Some(parent) => tree.resolve_checkout(parent)?,
            None => tree
                .header()
                .map(|header| header.id.clone())
                .ok_or(CheckoutError::SessionHeader)?,
        };
        return Ok(Destination {
            target,
            prefill: Some(user.content.clone()),
        });
    }
    Ok(Destination {
        target: tree.resolve_checkout(id)?,
        prefill: None,
    })
}

fn is_displayed(entry: &Entry) -> bool {
    !matches!(
        entry.kind,
        EntryKind::Session(_)
            | EntryKind::Checkout(_)
            | EntryKind::Custom(_)
            | EntryKind::ToolResult(_)
    )
}

struct Walk<'a> {
    tree: &'a SessionTree,
    children: HashMap<&'a str, Vec<&'a Entry>>,
    active: Option<&'a str>,
    on_path: &'a HashSet<&'a str>,
}

impl<'a> Walk<'a> {
    /// Emit `entry` and everything below it. A single child continues the
    /// line at the same depth (in a loop, so a long session cannot overflow
    /// the stack); only a real branch point indents and recurses, so a long
    /// linear session stays flush left.
    fn walk(
        &self,
        entry: &'a Entry,
        depth: usize,
        prefix: &str,
        child_prefix: &str,
        rows: &mut Vec<TreeRow>,
    ) {
        rows.push(self.row(entry, depth, prefix));
        let mut cursor = entry;
        loop {
            let children = self.visible_children(&cursor.id);
            match children.as_slice() {
                [] => return,
                [only] => {
                    rows.push(self.row(only, depth, child_prefix));
                    cursor = only;
                }
                _ => {
                    self.walk_children(&children, depth, child_prefix, rows);
                    return;
                }
            }
        }
    }

    fn walk_children(
        &self,
        children: &[&'a Entry],
        depth: usize,
        prefix: &str,
        rows: &mut Vec<TreeRow>,
    ) {
        if let [only] = children {
            self.walk(only, depth, prefix, prefix, rows);
            return;
        }
        if depth > 256 {
            return;
        }
        let last = children.len().saturating_sub(1);
        for (index, child) in children.iter().enumerate() {
            let child_last = index == last;
            let connector = if child_last { "└─ " } else { "├─ " };
            let row_prefix = format!("{prefix}{connector}");
            let next_prefix = format!("{prefix}{}", if child_last { "   " } else { "│  " });
            self.walk(child, depth + 1, &row_prefix, &next_prefix, rows);
        }
    }

    fn row(&self, entry: &Entry, depth: usize, prefix: &str) -> TreeRow {
        let tools = match &entry.kind {
            EntryKind::Assistant(data) => data.tool_calls.as_ref().map_or(0, Vec::len),
            _ => 0,
        };
        let folded: u64 = if tools > 0 {
            self.tree
                .batch_results(&entry.id)
                .into_iter()
                .map(estimate_tokens)
                .sum()
        } else {
            0
        };
        TreeRow {
            id: entry.id.clone(),
            depth,
            prefix: prefix.to_string(),
            role: entry.role_char(),
            preview: entry.preview(72),
            ts: Some(entry.ts),
            tokens: estimate_tokens(entry) + folded,
            tools,
            active: self.active == Some(entry.id.as_str()),
            on_path: self.on_path.contains(entry.id.as_str()),
        }
    }

    /// Displayed children of `id`, looking through entries the tree does not
    /// show (tool results, `fs-change`, rewind markers, session headers). A
    /// hidden entry is structural, not a branch: its visible descendants stay
    /// on the same level.
    fn visible_children(&self, id: &str) -> Vec<&'a Entry> {
        let mut out = Vec::new();
        let mut stack: Vec<&'a Entry> = self
            .children
            .get(id)
            .map(|kids| kids.iter().rev().copied().collect())
            .unwrap_or_default();
        while let Some(child) = stack.pop() {
            if is_displayed(child) {
                out.push(child);
            } else if let Some(kids) = self.children.get(child.id.as_str()) {
                stack.extend(kids.iter().rev().copied());
            }
        }
        out
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
            provenance: None,
            kind,
        }
    }

    fn user(seq: u64, id: &str, parent: Option<&str>, text: &str) -> Entry {
        entry(
            seq,
            id,
            parent,
            EntryKind::User(UserData {
                attachments: Vec::new(),
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
        let ids: Vec<&str> = view.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(
            ids,
            ["e1", "e2", "e4", "e5", "e6"],
            "tool results fold away"
        );
        assert_eq!(view.rows[0].role, 'U');
        assert_eq!(view.rows[1].role, 'A');
        assert_eq!(view.rows[1].tools, 1);
        // A linear run stays flush left; only the branch point indents.
        assert_eq!(view.rows[1].prefix, "");
        assert_eq!(view.rows[2].prefix, "├─ ");
        assert_eq!(view.rows[3].prefix, "└─ ");
        assert_eq!(view.rows[4].prefix, "   ");
        let active: Vec<&str> = view
            .rows
            .iter()
            .filter(|row| row.active)
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(active, vec!["e6"]);
        assert_eq!(view.selected, 4, "the view opens on the active row");
        let on_path: Vec<&str> = view
            .rows
            .iter()
            .filter(|row| row.on_path)
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(on_path, ["e1", "e2", "e5", "e6"]);
        assert_eq!(view.rows[0].preview, "Fix the parser bug");
        assert!(view.rows[1].tokens > estimate_tokens(tree.get("e2").unwrap()));
    }

    #[test]
    fn selection_resolves_through_tool_batches() {
        let tree = fixture();
        let mut view = TreeView::from_tree(&tree, Some("e6"));
        view.selected = 1; // assistant with one call
        assert_eq!(view.resolve(&tree).unwrap(), "e3");
        view.selected = 2; // assistant without calls
        assert_eq!(view.resolve(&tree).unwrap(), "e4");
    }

    #[test]
    fn a_user_row_goes_back_to_just_before_the_message() {
        let mut tree = fixture();
        tree.push(entry(
            0,
            "s0",
            None,
            EntryKind::Session(
                serde_json::from_value(json!({
                    "cwd": "", "endpoint": "", "model": "", "profile": "", "app": ""
                }))
                .unwrap(),
            ),
        ));
        let mut view = TreeView::from_tree(&tree, Some("e6"));
        view.selected = 3; // e5, a user message after the tool batch
        assert_eq!(
            view.destination(&tree).unwrap(),
            Destination {
                target: "e3".into(),
                prefill: Some("Try another approach".into()),
            }
        );
        view.selected = 0; // the first message has no parent: before it is the header
        assert_eq!(view.destination(&tree).unwrap().target, "s0");
        view.selected = 2;
        assert_eq!(
            view.destination(&tree).unwrap(),
            Destination {
                target: "e4".into(),
                prefill: None,
            }
        );
    }

    #[test]
    fn a_hidden_root_does_not_hide_the_conversation() {
        // Real sessions start with a `tool-probe` record before the first
        // message, so every later entry descends from a hidden root.
        let mut tree = SessionTree::new();
        tree.push(entry(
            1,
            "e1",
            None,
            EntryKind::Custom(crate::session::CustomData::new("tool-probe")),
        ));
        tree.push(user(2, "e2", Some("e1"), "hello"));
        tree.push(assistant(3, "e3", "e2", 0, "hi"));
        tree.push(entry(
            4,
            "e4",
            Some("e3"),
            EntryKind::Custom(crate::session::CustomData::new("fs-change")),
        ));
        let view = TreeView::from_tree(&tree, Some("e4"));
        let ids: Vec<&str> = view.rows.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, ["e2", "e3"]);
        assert!(
            view.rows[1].active,
            "a hidden leaf marks its visible ancestor"
        );
        assert_eq!(view.selected, 1);
        assert_eq!(view.destination(&tree).unwrap().target, "e3");
        let mut view = view;
        view.selected = 0;
        assert_eq!(view.destination(&tree).unwrap().target, "e1");
    }

    #[test]
    fn a_long_linear_session_does_not_indent_or_recurse() {
        let mut tree = SessionTree::new();
        tree.push(user(1, "e1", None, "go"));
        let mut parent = "e1".to_string();
        for seq in 2..5_000u64 {
            let id = format!("e{seq}");
            tree.push(assistant(seq, &id, &parent, 0, "step"));
            parent = id;
        }
        let view = TreeView::from_tree(&tree, Some(&parent));
        assert_eq!(view.rows.len(), 4_999);
        assert!(
            view.rows
                .iter()
                .all(|row| row.depth == 0 && row.prefix.is_empty())
        );
    }

    #[test]
    fn jump_user_moves_between_messages() {
        let tree = fixture();
        let mut view = TreeView::from_tree(&tree, Some("e6"));
        view.jump_user(-1);
        assert_eq!(view.selected_row().unwrap().id, "e5");
        view.jump_user(-1);
        assert_eq!(view.selected_row().unwrap().id, "e1");
        view.jump_user(-1);
        assert_eq!(view.selected_row().unwrap().id, "e1");
        view.jump_user(1);
        assert_eq!(view.selected_row().unwrap().id, "e5");
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
        assert_eq!(view.rows.len(), 2);
        assert!(view.rows[1].active);
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
    fn hidden_entries_do_not_break_the_visible_branch() {
        let mut tree = SessionTree::new();
        tree.push(user(1, "e1", None, "go"));
        tree.push(entry(
            2,
            "e2",
            Some("e1"),
            EntryKind::Custom(crate::session::CustomData::new("fs-change").with("path", "a.rs")),
        ));
        tree.push(assistant(3, "e3", "e2", 0, "edited"));
        let view = TreeView::from_tree(&tree, Some("e3"));
        assert_eq!(
            view.rows
                .iter()
                .map(|row| row.id.as_str())
                .collect::<Vec<_>>(),
            ["e1", "e3"],
            "the assistant below a hidden fs-change entry stays visible"
        );
        assert_eq!(view.rows[1].depth, 0);
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
                keep_from: None,
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

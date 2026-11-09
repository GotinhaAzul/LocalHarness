//! Multiline input, history, and slash-command completion.

use std::io::Write as _;
use std::path::PathBuf;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders};
use ratatui_textarea::{CursorMove, TextArea, WrapMode};

use crate::ui::theme::{Role, Theme};

/// Prompts kept in memory and on disk.
const HISTORY_LIMIT: usize = 200;

/// Owned editing state for the prompt.
pub struct InputState {
    pub textarea: TextArea<'static>,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    pub completion_index: usize,
    /// Set by Tab to show on-demand (path) completions; cleared on edit.
    popup_open: bool,
    /// Per-project file prompts are appended to, so Up recalls them across
    /// restarts. `None` keeps history in memory only.
    history_file: Option<PathBuf>,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    pub fn new() -> Self {
        let mut textarea = TextArea::new(vec![String::new()]);
        textarea.set_placeholder_text("type a message, @file to attach, or /help");
        textarea.set_max_histories(200);
        textarea.set_cursor_line_style(Style::default());
        textarea.set_wrap_mode(WrapMode::WordOrGlyph);
        textarea.move_cursor(CursorMove::End);
        InputState {
            textarea,
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            completion_index: 0,
            popup_open: false,
            history_file: None,
        }
    }

    /// The current text with lines joined by newlines.
    pub fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.textarea.lines().iter().all(|line| line.is_empty())
    }

    pub fn take(&mut self) -> String {
        let text = self.text();
        self.textarea.clear();
        self.reset_completion();
        text
    }

    pub fn set_text(&mut self, text: &str) {
        self.textarea.clear();
        self.textarea.insert_str(text);
        self.textarea.move_cursor(CursorMove::End);
        self.reset_completion();
    }

    /// Submit the prompt when idle. Returns `None` (and preserves the text)
    /// while a turn is streaming.
    pub fn submit(&mut self, idle: bool) -> Option<String> {
        if !idle {
            return None;
        }
        let text = self.text();
        if text.trim().is_empty() {
            return None;
        }
        self.push_history(&text);
        self.textarea.clear();
        self.history_index = None;
        self.draft.clear();
        self.reset_completion();
        Some(text)
    }

    pub fn push_history(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
            if self.history.len() > HISTORY_LIMIT {
                self.history.remove(0);
            }
            self.append_history_file(text);
        }
    }

    /// Load prompt history from `path` (JSON-lines of strings, newest last)
    /// and append future prompts to it. A missing or damaged file is not an
    /// error: history is a convenience.
    pub fn attach_history_file(&mut self, path: PathBuf) {
        if let Ok(raw) = std::fs::read_to_string(&path) {
            let loaded: Vec<String> = raw
                .lines()
                .filter_map(|line| serde_json::from_str::<String>(line).ok())
                .filter(|text| !text.trim().is_empty())
                .collect();
            let skip = loaded.len().saturating_sub(HISTORY_LIMIT);
            let mut history: Vec<String> = loaded.into_iter().skip(skip).collect();
            history.dedup();
            history.append(&mut self.history);
            self.history = history;
            // Rewrite oversized files so they stay bounded.
            if skip > HISTORY_LIMIT {
                let body: String = self
                    .history
                    .iter()
                    .filter_map(|text| serde_json::to_string(text).ok())
                    .map(|line| line + "\n")
                    .collect();
                let _ = std::fs::write(&path, body);
            }
        }
        self.history_file = Some(path);
    }

    fn append_history_file(&self, text: &str) {
        let Some(path) = &self.history_file else {
            return;
        };
        let Ok(line) = serde_json::to_string(text) else {
            return;
        };
        use std::os::unix::fs::OpenOptionsExt as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }

    pub fn history_prev(&mut self) -> bool {
        if self.history.is_empty() {
            return false;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => {
                self.draft = self.text();
                self.history.len() - 1
            }
        };
        self.history_index = Some(index);
        let text = self.history[index].clone();
        self.set_text(&text);
        true
    }

    pub fn history_next(&mut self) -> bool {
        let Some(index) = self.history_index else {
            return false;
        };
        if index + 1 < self.history.len() {
            self.history_index = Some(index + 1);
            let text = self.history[index + 1].clone();
            self.set_text(&text);
        } else {
            self.history_index = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_text(&draft);
        }
        true
    }

    /// Apply an editing key. `Ctrl+J` inserts a newline; everything else is
    /// delegated to the textarea so undo and word movement keep working.
    pub fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('J'))
        {
            self.insert_newline();
            return true;
        }
        self.reset_completion();
        self.textarea.input(key)
    }

    pub fn insert_newline(&mut self) {
        self.textarea.insert_newline();
        self.textarea.move_cursor(CursorMove::End);
        self.reset_completion();
    }

    pub fn insert_text(&mut self, text: &str) {
        self.textarea.insert_str(text);
        self.reset_completion();
    }

    pub fn undo(&mut self) -> bool {
        self.textarea.undo()
    }

    pub fn redo(&mut self) -> bool {
        self.textarea.redo()
    }

    /// The prompt text before the cursor, which completion works on.
    pub fn before_cursor(&self) -> String {
        let ratatui_textarea::DataCursor(row, col) = self.textarea.cursor();
        let lines = self.textarea.lines();
        let mut before = String::new();
        for line in lines.iter().take(row) {
            before.push_str(line);
            before.push('\n');
        }
        if let Some(line) = lines.get(row) {
            before.extend(line.chars().take(col));
        }
        before
    }

    /// Whether the cursor sits at the very end of the prompt.
    pub fn cursor_at_end(&self) -> bool {
        self.before_cursor().len() == self.text().len()
    }

    /// Replace the text between byte `start` and the cursor with
    /// `insertion`, leaving the cursor just after it. Text after the cursor
    /// is preserved.
    pub fn replace_before_cursor(&mut self, start: usize, insertion: &str) {
        let before = self.before_cursor();
        let text = self.text();
        let after = &text[before.len()..];
        let start = start.min(before.len());
        let head = format!("{}{insertion}", &before[..start]);
        self.textarea.clear();
        self.textarea.insert_str(format!("{head}{after}"));
        let row = head.matches('\n').count();
        let col = head.rsplit('\n').next().unwrap_or("").chars().count();
        self.textarea.move_cursor(CursorMove::Jump(
            row.min(u16::MAX as usize) as u16,
            col.min(u16::MAX as usize) as u16,
        ));
        self.reset_completion();
    }

    pub fn cycle_completion(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.completion_index = 0;
            return;
        }
        let len = len as isize;
        let next = (self.completion_index as isize + delta).rem_euclid(len);
        self.completion_index = next as usize;
    }

    /// Open the on-demand completion popup (file paths) until the text
    /// changes.
    pub fn open_popup(&mut self) {
        self.popup_open = true;
    }

    pub fn popup_open(&self) -> bool {
        self.popup_open
    }

    pub fn reset_completion(&mut self) {
        self.completion_index = 0;
        self.popup_open = false;
    }

    /// Screen rows the prompt needs at `width` columns with soft wrapping.
    pub fn visual_rows(&self, width: usize) -> usize {
        let width = width.max(1);
        self.textarea
            .lines()
            .iter()
            .map(|line| crate::ui::markdown::wrap_plain(line, width).len().max(1))
            .sum()
    }

    /// Settle the textarea's viewport for a `width`×`height` prompt. The
    /// textarea only scrolls to keep the cursor visible, so after the box
    /// grows it would keep hiding rows that now fit; scroll those back.
    pub fn fit_viewport(&mut self, width: u16, height: u16) {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;
        use ratatui::widgets::Widget as _;

        if width == 0 || height == 0 {
            return;
        }
        // Rendering once records the real viewport size in the textarea.
        let area = Rect::new(0, 0, width, height);
        (&self.textarea).render(area, &mut Buffer::empty(area));
        let (top, _) = self.textarea.scroll_offset();
        if top > 0 && self.visual_rows(width as usize) <= height as usize {
            // `scroll` drags the cursor into the old viewport; put it back.
            let ratatui_textarea::DataCursor(row, col) = self.textarea.cursor();
            self.textarea
                .scroll((-(top.min(i16::MAX as u16) as i16), 0));
            self.textarea.move_cursor(CursorMove::Jump(
                row.min(u16::MAX as usize) as u16,
                col.min(u16::MAX as usize) as u16,
            ));
        }
    }

    pub fn set_placeholder(&mut self, text: &str) {
        if self.textarea.placeholder_text() != text {
            self.textarea.set_placeholder_text(text);
        }
    }

    /// Apply the active theme to the textarea widget.
    pub fn apply_theme(&mut self, theme: &Theme) {
        self.textarea.set_style(theme.style(Role::Fg));
        // A block cursor: a foreground-only style is invisible on a space.
        self.textarea.set_cursor_style(
            Style::default()
                .fg(theme.color(Role::Bg))
                .bg(theme.color(Role::Cursor))
                .add_modifier(Modifier::BOLD),
        );
        self.textarea.set_cursor_line_style(Style::default());
        self.textarea
            .set_placeholder_style(theme.style(Role::Muted));
        // The surrounding rounded box and prompt glyph are drawn by the
        // painter; the textarea itself is borderless.
        self.textarea
            .set_block(Block::default().borders(Borders::NONE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn enter_sends_only_when_idle() {
        let mut input = InputState::new();
        input.set_text("hello");
        assert_eq!(input.submit(false), None);
        assert_eq!(input.text(), "hello");
        assert_eq!(input.submit(true).as_deref(), Some("hello"));
        assert!(input.is_empty());
    }

    #[test]
    fn ctrl_j_inserts_a_newline() {
        let mut input = InputState::new();
        input.insert_text("one");
        input.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        input.insert_text("two");
        assert_eq!(input.text(), "one\ntwo");
    }

    #[test]
    fn up_recalls_history_when_empty() {
        let mut input = InputState::new();
        input.set_text("first");
        input.submit(true).unwrap();
        input.set_text("second");
        input.submit(true).unwrap();
        assert!(input.is_empty());
        assert!(input.history_prev());
        assert_eq!(input.text(), "second");
        assert!(input.history_prev());
        assert_eq!(input.text(), "first");
        assert!(input.history_next());
        assert_eq!(input.text(), "second");
        assert!(input.history_next());
        assert!(input.is_empty());
    }

    #[test]
    fn replacing_before_the_cursor_keeps_the_tail() {
        let mut input = InputState::new();
        input.set_text("fix @src/ma please");
        // Put the cursor right after "@src/ma".
        input.textarea.move_cursor(CursorMove::Jump(0, 11));
        assert_eq!(input.before_cursor(), "fix @src/ma");
        assert!(!input.cursor_at_end());
        input.replace_before_cursor(5, "src/main.rs ");
        assert_eq!(input.text(), "fix @src/main.rs  please");
        assert_eq!(input.before_cursor(), "fix @src/main.rs ");
    }

    #[test]
    fn history_persists_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt_history.jsonl");
        let mut first = InputState::new();
        first.attach_history_file(path.clone());
        first.set_text("line one\nline two");
        first.submit(true).unwrap();
        first.set_text("second");
        first.submit(true).unwrap();

        let mut second = InputState::new();
        second.attach_history_file(path);
        assert!(second.history_prev());
        assert_eq!(second.text(), "second");
        assert!(second.history_prev());
        assert_eq!(second.text(), "line one\nline two");
    }

    #[test]
    fn before_cursor_spans_lines() {
        let mut input = InputState::new();
        input.set_text("one\ntwo");
        assert_eq!(input.before_cursor(), "one\ntwo");
        assert!(input.cursor_at_end());
    }

    #[test]
    fn refitting_after_growth_shows_every_row_and_keeps_the_cursor() {
        let mut input = InputState::new();
        let long = "word ".repeat(30);
        input.insert_text(long.trim_end());
        // Typed while the box was one row high, then the box grows.
        input.fit_viewport(40, 1);
        assert!(input.textarea.scroll_offset().0 > 0);
        input.fit_viewport(40, 4);
        assert_eq!(input.textarea.scroll_offset().0, 0);
        input.insert_text("!");
        assert!(input.text().ends_with("word!"), "{}", input.text());
        assert_eq!(input.visual_rows(40), 4);
    }
}

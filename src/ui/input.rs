//! Multiline input, history, and slash-command completion.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Style;
use ratatui::text::Span;
use ratatui::widgets::{Block, Borders};
use ratatui_textarea::{CursorMove, TextArea};

use crate::ui::theme::{Role, Theme};

/// Owned editing state for the prompt.
pub struct InputState {
    pub textarea: TextArea<'static>,
    history: Vec<String>,
    history_index: Option<usize>,
    draft: String,
    pub completion_index: usize,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    pub fn new() -> Self {
        let mut textarea = TextArea::new(vec![String::new()]);
        textarea.set_placeholder_text("type a message, or /help");
        textarea.set_max_histories(200);
        textarea.set_cursor_line_style(Style::default());
        textarea.move_cursor(CursorMove::End);
        InputState {
            textarea,
            history: Vec::new(),
            history_index: None,
            draft: String::new(),
            completion_index: 0,
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
            if self.history.len() > 200 {
                self.history.remove(0);
            }
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

    /// Filtered slash-command completions for the current prompt. Empty unless
    /// the prompt is a single `/`-prefixed word.
    pub fn completions(
        &self,
        commands: &[(&'static str, &'static str)],
    ) -> Vec<(&'static str, &'static str)> {
        let text = self.text();
        let trimmed = text.trim_start();
        if !trimmed.starts_with('/') || trimmed.contains(char::is_whitespace) {
            return Vec::new();
        }
        let prefix = &trimmed[1..];
        if commands.iter().any(|(name, _)| *name == prefix) {
            return Vec::new();
        }
        commands
            .iter()
            .copied()
            .filter(|(name, _)| name.starts_with(prefix))
            .collect()
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

    pub fn selected_completion(
        &self,
        commands: &[(&'static str, &'static str)],
    ) -> Option<&'static str> {
        let matches = self.completions(commands);
        if matches.is_empty() {
            return None;
        }
        let index = self.completion_index.min(matches.len() - 1);
        Some(matches[index].0)
    }

    /// A partial command name that Enter should complete before running.
    pub fn partial_command(
        &self,
        commands: &[(&'static str, &'static str)],
    ) -> Option<&'static str> {
        let text = self.text();
        let trimmed = text.trim();
        if !trimmed.starts_with('/') || trimmed.contains(char::is_whitespace) {
            return None;
        }
        self.selected_completion(commands)
    }

    pub fn accept_completion(&mut self, name: &str) {
        self.set_text(&format!("/{name}"));
    }

    pub fn reset_completion(&mut self) {
        self.completion_index = 0;
    }

    /// Apply the active theme to the textarea widget.
    pub fn apply_theme(&mut self, theme: &Theme) {
        self.textarea.set_style(theme.style(Role::Fg));
        self.textarea
            .set_cursor_style(Style::default().fg(theme.color(Role::Cursor)));
        self.textarea.set_cursor_line_style(Style::default());
        self.textarea
            .set_placeholder_style(theme.style(Role::Muted));
        let title = Span::styled(" Input ", theme.bold(Role::Muted));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(theme.style(Role::Border))
            .title(title);
        self.textarea.set_block(block);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMANDS: &[(&str, &str)] = &[
        ("help", "list commands"),
        ("theme", "change the theme"),
        ("tree", "browse the session tree"),
        ("trust", "show trust state"),
    ];

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
    fn slash_completion_filters_with_descriptions() {
        let mut input = InputState::new();
        input.set_text("/tr");
        let matches = input.completions(COMMANDS);
        let names: Vec<&str> = matches.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, vec!["tree", "trust"]);
        assert!(
            matches
                .iter()
                .all(|(_, description)| !description.is_empty())
        );

        input.set_text("/");
        assert_eq!(input.completions(COMMANDS).len(), COMMANDS.len());

        input.set_text("/help");
        assert!(input.completions(COMMANDS).is_empty());
    }

    #[test]
    fn partial_command_completes_to_a_command_name() {
        let mut input = InputState::new();
        input.set_text("/tre");
        assert_eq!(input.partial_command(COMMANDS), Some("tree"));
        input.accept_completion("tree");
        assert_eq!(input.text(), "/tree");
    }
}

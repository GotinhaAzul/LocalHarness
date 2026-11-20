//! Keyboard, mouse, and paste handling, including modal pickers.

use super::*;

impl App {
    pub(super) async fn handle_event(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.handle_key(key).await,
            // Terminals send CRLF, and copied output can carry escapes;
            // either would leave the editor's cursor out of step with the
            // screen.
            Event::Paste(text) => self.input.insert_text(&crate::sanitize::paste(&text)),
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            // Some terminals reflow the old contents on resize; start clean.
            Event::Resize(_, _) => self.repaint = true,
            // The screen may have been redrawn by someone else meanwhile
            // (a multiplexer reattaching, a terminal restoring a tab).
            Event::FocusGained => self.repaint = true,
            _ => {}
        }
    }

    pub(super) fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.modal.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.transcript.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.transcript.scroll_down(WHEEL_LINES),
            _ => {}
        }
    }

    pub(super) async fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            if self.turn_active {
                if self.cancelling {
                    self.quit = true;
                } else {
                    self.cancel_turn();
                }
            } else {
                self.quit = true;
            }
            return;
        }
        if ctrl && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D')) {
            if self.input.is_empty() && !self.turn_active {
                self.quit = true;
            }
            return;
        }

        if ctrl && matches!(key.code, KeyCode::Char('l') | KeyCode::Char('L')) {
            self.repaint = true;
            return;
        }

        if self.handle_mode_confirm(&key).await {
            return;
        }

        if self.handle_permission_key(&key) {
            return;
        }

        if self.modal.is_some() {
            self.handle_modal_key(key).await;
            return;
        }

        // The review modal is parked while a hunk is open in the prompt
        // editor: typing edits the hunk, Enter saves, Esc reverts.
        if let Some(mut edit) = self.hunk_edit.take() {
            match key.code {
                KeyCode::Esc => {
                    self.input.set_text("");
                    self.modal = Some(ModalData::HunkReview(edit.modal));
                }
                KeyCode::Enter if !shift => {
                    let text = self.input.text();
                    self.input.set_text("");
                    if let Some(hunk) = edit.modal.hunks.get_mut(edit.hunk) {
                        hunk.text = text.clone();
                    }
                    edit.modal
                        .set_choice(edit.hunk, crate::tools::hunk::HunkChoice::Edit(text));
                    self.modal = Some(ModalData::HunkReview(edit.modal));
                }
                _ => {
                    self.input.handle_key(key);
                    self.hunk_edit = Some(edit);
                }
            }
            return;
        }

        if ctrl && matches!(key.code, KeyCode::Char('r') | KeyCode::Char('R')) {
            let show = !self.config.ui.show_reasoning;
            self.config.ui.show_reasoning = show;
            self.transcript.set_show_reasoning(show);
            let state = if show {
                "reasoning expanded"
            } else {
                "reasoning collapsed"
            };
            match self.persist_ui_setting("show_reasoning", toml::Value::Boolean(show)) {
                Ok(_) => self.set_notice(format!("{state} (saved)"), Role::Muted),
                Err(error) => {
                    self.set_notice(format!("{state}; not saved: {error}"), Role::Warning)
                }
            }
            return;
        }

        if ctrl && matches!(key.code, KeyCode::Char('o') | KeyCode::Char('O')) {
            let expand = !self.transcript.expand_tools();
            self.transcript.set_expand_tools(expand);
            self.set_notice(
                if expand {
                    "tool output expanded"
                } else {
                    "tool output collapsed"
                },
                Role::Muted,
            );
            return;
        }
        if ctrl && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T')) {
            self.show_plan = !self.show_plan;
            self.set_notice(
                if self.show_plan {
                    "plan shown"
                } else {
                    "plan hidden"
                },
                Role::Muted,
            );
            return;
        }

        match key.code {
            KeyCode::Esc => {
                if self.turn_active {
                    self.cancel_turn();
                } else if self.input.text().trim_start().starts_with('/')
                    && !self.input.text().contains('\n')
                {
                    // Abandon a half-typed command (and its popup).
                    self.input.set_text("");
                } else if self.current_completion().is_some() {
                    // Dismiss the live `@` popup without touching the text.
                    self.dismiss_completion();
                } else if self.input.popup_open() {
                    self.input.reset_completion();
                }
                return;
            }
            KeyCode::PageUp => {
                self.transcript.scroll_up(self.transcript.page());
                return;
            }
            KeyCode::PageDown => {
                self.transcript.scroll_down(self.transcript.page());
                return;
            }
            KeyCode::Up if shift => {
                self.transcript.scroll_up(1);
                return;
            }
            KeyCode::Down if shift => {
                self.transcript.scroll_down(1);
                return;
            }
            // Home/End edit the prompt when it has text; Ctrl always scrolls.
            KeyCode::Home if ctrl || self.input.is_empty() => {
                self.transcript.scroll_home();
                return;
            }
            KeyCode::End if ctrl || self.input.is_empty() => {
                self.transcript.scroll_end();
                return;
            }
            _ => {}
        }

        if ctrl && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('J')) {
            self.input.insert_newline();
            return;
        }

        match key.code {
            KeyCode::Enter => {
                if shift || alt {
                    self.input.insert_newline();
                    return;
                }
                // A partial command or argument runs as its highlighted
                // completion: `/the dr` + Enter runs `/theme dracula`.
                if let Some(completion) = self.current_completion()
                    && completion.kind != complete::Kind::Path
                    && !completion.token.is_empty()
                    && self.input.cursor_at_end()
                    && !completion
                        .candidates
                        .iter()
                        .any(|candidate| candidate.value == completion.token)
                {
                    let index = self
                        .input
                        .completion_index
                        .min(completion.candidates.len() - 1);
                    let value = completion.candidates[index].value.clone();
                    self.input.replace_before_cursor(completion.start, &value);
                }
                // While a switch loads, prose stays in the prompt (commands
                // still run) so nothing typed ahead is lost.
                let text = self.input.text();
                let command = text.trim_start().starts_with('/');
                let waiting = self.switching.is_some() && !command;
                // `/approve`, `/help`, and the like run mid-turn; anything
                // else waits so a prompt is never sent into a running turn.
                let busy_ok = command && !text.contains('\n') && runs_during_turn(&text);
                let idle = (!self.turn_active && !waiting) || busy_ok;
                match self.input.submit(idle) {
                    Some(text) => {
                        let trimmed = text.trim().to_string();
                        if trimmed.starts_with('/') {
                            self.run_command(&trimmed).await;
                        } else if !trimmed.is_empty() {
                            self.start_turn(text);
                        }
                    }
                    None if waiting => {
                        let label = self
                            .switching
                            .as_ref()
                            .map(Switch::label)
                            .unwrap_or_default();
                        self.set_notice(
                            format!("{label} press Enter again when it finishes"),
                            Role::Warning,
                        );
                    }
                    None if !idle => {
                        self.set_notice(
                            "a turn is running; press Esc to cancel before sending (/approve and /help work now)",
                            Role::Warning,
                        );
                    }
                    None => {}
                }
                return;
            }
            KeyCode::Up | KeyCode::Down => {
                if let Some(completion) = self.current_completion() {
                    let delta = if matches!(key.code, KeyCode::Up) {
                        -1
                    } else {
                        1
                    };
                    self.input
                        .cycle_completion(delta, completion.candidates.len());
                    return;
                }
                if self.input.is_empty() {
                    let moved = if matches!(key.code, KeyCode::Up) {
                        self.input.history_prev()
                    } else {
                        self.input.history_next()
                    };
                    if moved {
                        return;
                    }
                }
                self.input.handle_key(key);
                return;
            }
            KeyCode::Tab => {
                // A tab character never belongs in a slash command.
                if !self.tab_complete() && !self.input.text().trim_start().starts_with('/') {
                    self.input.handle_key(key);
                }
                return;
            }
            KeyCode::BackTab => {
                if let Some(completion) = self.current_completion() {
                    self.input.cycle_completion(-1, completion.candidates.len());
                }
                return;
            }
            _ => {}
        }

        self.input.handle_key(key);
    }

    pub(super) async fn handle_modal_key(&mut self, key: KeyEvent) {
        let Some(mut modal) = self.modal.take() else {
            return;
        };
        let mut close = false;
        // Row id, and `Some(force)` to rewind files as well as the conversation.
        let mut tree_go: Option<(String, Option<bool>)> = None;
        let mut switch: Option<PathBuf> = None;
        let mut picked: Option<PickAction> = None;
        let mut preview_theme: Option<String> = None;
        let mut review_submit: Option<(u64, crate::tools::hunk::HunkDecision)> = None;
        let mut review_edit: Option<(u64, usize)> = None;

        match &mut modal {
            ModalData::Tree(view) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => close = true,
                KeyCode::Up | KeyCode::Char('k') => view.move_by(-1),
                KeyCode::Down | KeyCode::Char('j') => view.move_by(1),
                KeyCode::PageUp => view.page(-1, 15),
                KeyCode::PageDown => view.page(1, 15),
                KeyCode::Home | KeyCode::Char('g') => view.home(),
                KeyCode::End | KeyCode::Char('G') => view.end(),
                KeyCode::Char('[') => view.jump_user(-1),
                KeyCode::Char(']') => view.jump_user(1),
                KeyCode::Char('.') => view.select_active(),
                KeyCode::Enter => tree_go = view.selected_row().map(|row| (row.id.clone(), None)),
                KeyCode::Char('r') => {
                    tree_go = view.selected_row().map(|row| (row.id.clone(), Some(false)));
                }
                KeyCode::Char('R') => {
                    tree_go = view.selected_row().map(|row| (row.id.clone(), Some(true)));
                }
                _ => {}
            },
            ModalData::Sessions(view) => match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Up | KeyCode::Char('k') => {
                    view.selected = view.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.selected = (view.selected + 1).min(view.items.len().saturating_sub(1));
                }
                KeyCode::PageUp => {
                    view.selected = view.selected.saturating_sub(15);
                }
                KeyCode::PageDown => {
                    view.selected = (view.selected + 15).min(view.items.len().saturating_sub(1));
                }
                KeyCode::Enter => {
                    if let Some(item) = view.items.get(view.selected) {
                        switch = Some(item.path.clone());
                    }
                }
                _ => {}
            },
            ModalData::Tools(view) => match key.code {
                KeyCode::Esc => close = true,
                KeyCode::Up | KeyCode::Char('k') => {
                    view.selected = view.selected.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    view.selected = (view.selected + 1).min(view.items.len().saturating_sub(1));
                }
                KeyCode::PageUp => {
                    view.selected = view.selected.saturating_sub(15);
                }
                KeyCode::PageDown => {
                    view.selected = (view.selected + 15).min(view.items.len().saturating_sub(1));
                }
                _ => {}
            },
            // Letters feed the filter, so navigation is arrows, Tab, and
            // PgUp/PgDn; Ctrl+P/N and Ctrl+K/J also move.
            ModalData::HunkReview(view) => match key.code {
                // Esc rejects the whole batch: the tree is untouched and the
                // model gets the rejection instead of a silent turn cancel.
                KeyCode::Esc => {
                    view.reject_all();
                    review_submit = Some((view.id, view.decision()));
                }
                KeyCode::Up | KeyCode::Char('k') => view.move_by(-1),
                KeyCode::Down | KeyCode::Char('j') => view.move_by(1),
                KeyCode::PageUp => view.move_by(-15),
                KeyCode::PageDown => view.move_by(15),
                KeyCode::Char('a') => view.set_selected(crate::tools::hunk::HunkChoice::Accept),
                KeyCode::Char('r') => view.set_selected(crate::tools::hunk::HunkChoice::Reject),
                KeyCode::Char('A') => view.accept_all(),
                KeyCode::Char('R') => view.reject_all(),
                KeyCode::Char('e') => review_edit = Some((view.id, view.selected)),
                KeyCode::Enter => review_submit = Some((view.id, view.decision())),
                _ => {}
            },
            ModalData::Picker(view) => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Esc => {
                        // Esc clears a filter first, then closes.
                        if view.filter.is_empty() {
                            close = true;
                        } else {
                            view.filter.clear();
                            view.selected = 0;
                            view.scroll = 0;
                        }
                    }
                    KeyCode::Up | KeyCode::BackTab => view.move_by(-1),
                    KeyCode::Down | KeyCode::Tab => view.move_by(1),
                    KeyCode::Char('p' | 'k') if ctrl => view.move_by(-1),
                    KeyCode::Char('n' | 'j') if ctrl => view.move_by(1),
                    KeyCode::PageUp => view.move_by(-15),
                    KeyCode::PageDown => view.move_by(15),
                    KeyCode::Home => view.move_by(isize::MIN / 2),
                    KeyCode::End => view.move_by(isize::MAX / 2),
                    KeyCode::Backspace => view.pop_filter(),
                    KeyCode::Enter => picked = view.selected_item().map(|item| item.action.clone()),
                    KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                        view.push_filter(ch);
                    }
                    _ => {}
                }
                // The theme picker previews the highlighted theme live.
                if view.command == "theme" && !close && picked.is_none() {
                    preview_theme = view
                        .selected_item()
                        .filter(|item| item.label != "save")
                        .map(|item| item.label.clone());
                }
            }
        }

        if let Some((id, decision)) = review_submit {
            self.reviewer.submit(id, decision);
            self.modal = None;
            self.set_notice("hunk review submitted", Role::Success);
            return;
        }

        if let Some((id, index)) = review_edit
            && let ModalData::HunkReview(view) = &modal
            && view.id == id
        {
            let text = view
                .hunks
                .get(index)
                .map(|hunk| hunk.text.clone())
                .unwrap_or_default();
            let mut view = view.clone();
            if let Some(hunk) = view.hunks.get_mut(index) {
                hunk.text = text.clone();
            }
            self.input.set_text(&text);
            self.hunk_edit = Some(HunkEdit {
                modal: view,
                hunk: index,
            });
            self.modal = None;
            return;
        }

        if let Some((row, rewind)) = tree_go
            && self.tree_go(&row, rewind)
        {
            close = true;
        }

        if let Some(path) = switch {
            self.modal = None;
            self.switch_session(path).await;
            return;
        }

        if let Some(action) = picked {
            self.modal = None;
            match action {
                PickAction::Run(command) => {
                    // A confirmed theme keeps the preview's base for `save`.
                    self.theme_before_preview = None;
                    self.run_command(&command).await;
                }
                PickAction::Fill(text) => {
                    self.restore_previewed_theme();
                    self.input.set_text(&text);
                }
            }
            return;
        }

        if let Some(name) = preview_theme
            && name != self.theme.name
        {
            self.apply_theme_named(&name);
        }

        if close {
            self.modal = None;
            self.restore_previewed_theme();
        } else {
            modal.ensure_visible(15);
            self.modal = Some(modal);
        }
    }
}

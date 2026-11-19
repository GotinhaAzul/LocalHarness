//! Interactive permission prompts: the TUI side of a policy `ask`.
//!
//! The turn task sends the request over the background channel and waits; the
//! transcript shows the exact command or path (and a diff when one exists),
//! and `y` or `n` answers it. Cancelling the turn or closing the TUI denies.

use super::*;

use crate::policy::{AskAnswer, AskRequest, Asker, PermissionMode};

/// A pending question and the channel its answer goes back on.
pub(super) struct PendingAsk {
    pub request: AskRequest,
    pub reply: tokio::sync::oneshot::Sender<AskAnswer>,
}

/// Forwards policy questions from the turn task to the event loop.
pub(super) struct TuiAsker {
    pub tx: UnboundedSender<BgEvent>,
}

#[async_trait::async_trait]
impl Asker for TuiAsker {
    async fn ask(&self, request: AskRequest) -> AskAnswer {
        let (reply, answer) = tokio::sync::oneshot::channel();
        if self
            .tx
            .send(BgEvent::Permission(PendingAsk { request, reply }))
            .is_err()
        {
            return AskAnswer::Deny;
        }
        answer.await.unwrap_or(AskAnswer::Deny)
    }
}

/// The transcript text for a question: the exact subject, then the preview.
pub(super) fn ask_text(request: &AskRequest) -> String {
    let mut text = match request.tool.as_str() {
        "bash" | "bg" => format!("$ {}", request.subject),
        _ => format!("{}: {}", request.tool, request.subject),
    };
    if let Some(rule) = &request.rule {
        text.push_str(&format!("\n(asked by rule {rule})"));
    }
    if let Some(preview) = &request.preview {
        text.push_str("\n\n");
        text.push_str(preview);
    }
    if let Some(suggestion) = &request.suggestion {
        text.push_str(&format!(
            "\n\nYou approved this exact command {} times before in this project. Press a to save this rule to your global config:\n{}",
            suggestion.times,
            suggestion.toml()
        ));
        text.push_str("\n\nAllow this exact call? y = yes (this session) · a = always (save the rule) · n = no");
    } else {
        text.push_str(
            "\n\nAllow this exact call? y = yes (remembered for this exact call) · n = no",
        );
    }
    text
}

impl App {
    /// Let the current core's turns ask through this TUI.
    pub(super) fn install_asker(&self) {
        if let Some(core) = self.core() {
            core.permissions.set_asker(Arc::new(TuiAsker {
                tx: self.bg_tx.clone(),
            }));
        }
    }

    pub(super) fn show_permission_request(&mut self, pending: PendingAsk) {
        // A newer question replaces an unanswered one; the old one is denied.
        if let Some(previous) = self.pending_ask.take() {
            let _ = previous.reply.send(AskAnswer::Deny);
        }
        self.transcript.push_note(
            format!("permission · {}", pending.request.tool),
            ask_text(&pending.request),
            false,
        );
        self.transcript.scroll_end();
        self.pending_ask = Some(pending);
    }

    /// Answer the pending question. Returns false when there is none.
    pub(super) fn answer_permission(&mut self, answer: AskAnswer) -> bool {
        let Some(pending) = self.pending_ask.take() else {
            return false;
        };
        let verdict = match answer {
            AskAnswer::Allow => "allowed",
            AskAnswer::AllowAlways => "allowed and saved the rule",
            AskAnswer::Deny => "denied",
        };
        self.set_notice(
            format!("{verdict}: {}", pending.request.subject),
            if answer != AskAnswer::Deny {
                Role::Success
            } else {
                Role::Warning
            },
        );
        let _ = pending.reply.send(answer);
        true
    }

    /// Keys while a question is open: `y` allows, `n` or Esc denies, other
    /// keys are ignored so a stray keystroke cannot approve anything.
    pub(super) fn handle_permission_key(&mut self, key: &KeyEvent) -> bool {
        if self.pending_ask.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                self.answer_permission(AskAnswer::Allow);
            }
            KeyCode::Char('a') | KeyCode::Char('A')
                if self
                    .pending_ask
                    .as_ref()
                    .is_some_and(|pending| pending.request.suggestion.is_some()) =>
            {
                self.answer_permission(AskAnswer::AllowAlways);
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                self.answer_permission(AskAnswer::Deny);
            }
            _ => self.set_notice("permission needed: y allow · n deny", Role::Warning),
        }
        true
    }

    /// Keys while a `yolo` switch waits for confirmation: `y` enables it,
    /// any other key cancels so a stray keystroke cannot remove approvals.
    pub(super) async fn handle_mode_confirm(&mut self, key: &KeyEvent) -> bool {
        let Some(mode) = self.pending_mode_confirm.take() else {
            return false;
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.set_permission_mode(mode).await,
            _ => self.set_notice("yolo not enabled; permissions unchanged", Role::Muted),
        }
        true
    }

    /// Footer status: an open question first, then a non-default mode.
    pub(super) fn permission_status(&self) -> Option<(String, Role)> {
        if self.pending_ask.is_some() {
            let always = self
                .pending_ask
                .as_ref()
                .is_some_and(|pending| pending.request.suggestion.is_some());
            let keys = if always {
                "permission: y allow · a always · n deny"
            } else {
                "permission: y allow · n deny"
            };
            return Some((keys.into(), Role::Warning));
        }
        if self.pending_mode_confirm.is_some() {
            return Some((
                "yolo: press y to confirm · any key cancels".into(),
                Role::Warning,
            ));
        }
        let mode = self.core()?.permissions.mode();
        match mode {
            // `normal` is the default, so it adds nothing to the footer.
            PermissionMode::Ask => None,
            PermissionMode::Plan => Some((
                "plan mode · /plan approve to apply · /plan exit to discard".into(),
                Role::Warning,
            )),
            PermissionMode::AcceptEdits => {
                Some(("accept-edits: edits run, commands ask".into(), Role::Muted))
            }
            PermissionMode::DenyAll => Some((
                "deny-all: anything that would ask is denied".into(),
                Role::Warning,
            )),
            PermissionMode::Yolo => Some(("yolo: no approvals".into(), Role::Error)),
        }
    }
}

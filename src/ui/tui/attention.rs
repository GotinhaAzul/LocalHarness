//! What needs the user right now.
//!
//! Several things wait on a human: a permission question, the `yolo`
//! confirmation, a hunk review, a commit or pull request drafted behind a
//! preview gate, an injection warning that holds the next sensitive action, a
//! finished plan, and a missing model. Each used to announce itself in its own
//! place (a transcript note, a footer hint, a notice that fades). [`App::attention`]
//! picks the most urgent one, and the activity line, the footer, the terminal
//! title, and the bell all show that same thing, so "is it waiting for me?"
//! always has one answer at the bottom of the screen.

use super::*;

use crate::policy::PermissionMode;

/// One thing waiting on the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Attention {
    /// What to do, e.g. `Approve command`.
    pub what: String,
    /// The exact command, path, or action it is about.
    pub subject: String,
    /// The keys or commands that answer it.
    pub keys: String,
    /// The agent is blocked until this is answered.
    pub blocking: bool,
}

impl Attention {
    fn new(what: &str, subject: impl Into<String>, keys: &str, blocking: bool) -> Self {
        Self {
            what: what.into(),
            subject: subject.into(),
            keys: keys.into(),
            blocking,
        }
    }

    /// Identity for "did something new start waiting?" (bell, title).
    pub fn key(&self) -> String {
        format!("{}\u{0}{}", self.what, self.subject)
    }
}

/// The verb for a permission question about `tool`.
fn approve_what(tool: &str) -> String {
    match tool {
        "bash" | "bg" => "Approve command".into(),
        "edit" | "write" | "patch" | "undo" => "Approve file change".into(),
        "git" => "Approve git action".into(),
        "pr" => "Approve pull request action".into(),
        "fetch" => "Approve network fetch".into(),
        other => format!("Approve {other}"),
    }
}

impl App {
    /// The most urgent thing waiting on the user, or `None` when the agent
    /// needs nothing from them.
    pub(super) fn attention(&self) -> Option<Attention> {
        // The agent is blocked on these: answer them before anything else.
        if let Some(pending) = &self.pending_ask {
            let request = &pending.request;
            let keys = if request.suggestion.is_some() {
                "y allow · a always · n deny"
            } else {
                "y allow · n deny"
            };
            let subject = match request.tool.as_str() {
                "bash" | "bg" => format!("$ {}", request.subject),
                _ => request.subject.clone(),
            };
            return Some(Attention::new(
                &approve_what(&request.tool),
                subject,
                keys,
                true,
            ));
        }
        if let Some(ModalData::HunkReview(review)) = &self.modal {
            return Some(Attention::new(
                "Review changes",
                format!("{} hunk(s)", review.hunks.len()),
                "a accept · r reject · e edit · enter apply · esc reject all",
                true,
            ));
        }
        if self.pending_mode_confirm.is_some() {
            return Some(Attention::new(
                "Confirm yolo",
                "every call runs without asking",
                "y confirm · any key cancels",
                true,
            ));
        }
        let core = self.core()?;
        if !self.header.logged_in && !self.turn_active {
            return Some(Attention::new(
                "Connect a model",
                "no model is selected",
                "/login <endpoint> [model]",
                false,
            ));
        }
        if let Ok(provenance) = core.provenance.lock()
            && let Some(warning) = provenance.pending()
            && !provenance.is_approved()
        {
            return Some(Attention::new(
                "Untrusted instructions",
                format!("seen in {}", warning.origin),
                "/approve allows the next sensitive action",
                false,
            ));
        }
        // Scanning the session is cheap but not free; while a turn streams,
        // these can wait until it ends.
        if self.turn_active {
            return None;
        }
        if let Ok(session) = core.session.lock() {
            if crate::tools::git::pending(&session, "commit").is_some() {
                return Some(Attention::new(
                    "Confirm commit",
                    "drafted behind the preview gate",
                    "/commit confirm · /commit edit <msg> · /commit cancel",
                    false,
                ));
            }
            if crate::tools::git::pending(&session, crate::tools::pr::ACTION).is_some() {
                return Some(Attention::new(
                    "Confirm pull request",
                    "drafted behind the preview gate",
                    "/pr confirm · /pr cancel",
                    false,
                ));
            }
        }
        if core.permissions.mode() == PermissionMode::Plan && !self.plan.is_empty() {
            return Some(Attention::new(
                "Plan ready",
                format!("{} step(s)", self.plan.len()),
                "/plan approve · /plan exit",
                false,
            ));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_permission_question_names_what_it_asks_for() {
        assert_eq!(approve_what("bash"), "Approve command");
        assert_eq!(approve_what("write"), "Approve file change");
        assert_eq!(
            approve_what("github__create_issue"),
            "Approve github__create_issue"
        );
    }
}

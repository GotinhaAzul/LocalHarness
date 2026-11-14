//! `/commit` and `/pr`: the user's side of the git and pr tools' approval
//! gates. Kept apart from the rest of the command handlers so the gate logic
//! lives in `tools::git` / `tools::pr` and this file only adapts it to the TUI.

use super::*;

impl App {
    /// `/commit`: show or draft the commit preview, replace its message, or
    /// approve it. The only path that commits under `[git] commit = "ask"`.
    pub(super) async fn command_commit(&mut self, rest: &str) {
        let Some(runtime) = &self.runtime else {
            self.set_notice("not connected", Role::Warning);
            return;
        };
        let cwd = runtime.core.paths.cwd.clone();
        let session = runtime.session.clone();
        let subject_max = self.config.git.subject_max;
        let rest = rest.trim().to_string();
        let (verb, argument) = match rest.split_once(char::is_whitespace) {
            Some((verb, argument)) => (verb.to_string(), argument.trim().to_string()),
            None => (rest.clone(), String::new()),
        };
        let outcome = tokio::task::spawn_blocking(move || {
            let mut session = match session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            use crate::tools::git;
            let result = match verb.as_str() {
                "" => git::show_or_draft_commit(&cwd, &mut session, subject_max),
                "confirm" => git::confirm_commit(&cwd, &mut session, subject_max),
                "edit" if argument.is_empty() => Err("/commit edit needs the new message".into()),
                "edit" => git::record_commit_preview(
                    &cwd,
                    &mut session,
                    &argument.replace("\\n", "\n"),
                    subject_max,
                ),
                "cancel" => match git::pending(&session, "commit") {
                    Some(_) => git::resolve(&mut session, "commit", "cancelled")
                        .map(|()| "commit preview dropped".to_string())
                        .map_err(|error| error.to_string()),
                    None => Err("no commit is waiting for approval".into()),
                },
                other => Err(format!(
                    "unknown /commit argument {other:?}; use confirm, cancel or edit <message>"
                )),
            };
            let _ = session.fsync();
            (verb, result)
        })
        .await;
        match outcome {
            Ok((verb, Ok(text))) => {
                self.transcript.push_note("commit", text, false);
                let notice = match verb.as_str() {
                    "confirm" => "committed",
                    "cancel" => "commit preview dropped",
                    _ => "commit preview ready; /commit confirm to commit",
                };
                self.set_notice(notice, Role::Success);
            }
            Ok((_, Err(error))) => {
                self.transcript.push_note("commit", error.clone(), true);
                self.set_notice(error, Role::Error);
            }
            Err(error) => self.set_notice(format!("commit failed: {error}"), Role::Error),
        }
        self.refresh_session_blocks();
    }

    /// `/pr`: show, approve, or drop the pending draft PR or comment.
    pub(super) async fn command_pr(&mut self, rest: &str) {
        let Some(runtime) = &self.runtime else {
            self.set_notice("not connected", Role::Warning);
            return;
        };
        let cwd = runtime.core.paths.cwd.clone();
        let session = runtime.session.clone();
        let verb = rest.trim().to_string();
        let outcome = tokio::task::spawn_blocking(move || {
            let mut session = match session.lock() {
                Ok(session) => session,
                Err(poison) => poison.into_inner(),
            };
            use crate::tools::{git, pr};
            let open = git::pending(&session, pr::ACTION);
            let result = match (verb.as_str(), open) {
                (_, None) => Err("no pull request action is waiting for approval".to_string()),
                ("", Some(open)) => Ok(format!(
                    "{}\n\n/pr confirm carries it out; /pr cancel drops it.",
                    pr::preview(&open)
                )),
                ("confirm", Some(_)) => pr::confirm(&cwd, &mut session),
                ("cancel", Some(_)) => git::resolve(&mut session, pr::ACTION, "cancelled")
                    .map(|()| "pull request action dropped".to_string())
                    .map_err(|error| error.to_string()),
                (other, Some(_)) => Err(format!(
                    "unknown /pr argument {other:?}; use confirm or cancel"
                )),
            };
            let _ = session.fsync();
            (verb, result)
        })
        .await;
        match outcome {
            Ok((verb, Ok(text))) => {
                self.transcript.push_note("pr", text, false);
                let notice = match verb.as_str() {
                    "confirm" => "pull request action done",
                    "cancel" => "pull request action dropped",
                    _ => "pending pull request action shown",
                };
                self.set_notice(notice, Role::Success);
            }
            Ok((_, Err(error))) => {
                self.transcript.push_note("pr", error.clone(), true);
                self.set_notice(error, Role::Error);
            }
            Err(error) => self.set_notice(format!("pr failed: {error}"), Role::Error),
        }
        self.refresh_session_blocks();
    }
}

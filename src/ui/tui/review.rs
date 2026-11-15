//! The TUI's [`HunkReviewer`]: turns a review request into an `AgentEvent`
//! the event loop renders as a modal, and returns the user's decision.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::agent::AgentEvent;
use crate::tools::hunk::{HunkDecision, HunkRequest, HunkReviewer};

#[derive(Default)]
pub struct TuiReviewer {
    events: Mutex<Option<UnboundedSender<AgentEvent>>>,
    pending: Mutex<Option<(u64, oneshot::Sender<HunkDecision>)>>,
    next_id: AtomicU64,
    /// The last decision handed to the agent, for tests and for noticing a
    /// stale submit after the turn ended.
    submitted: Mutex<Option<HunkDecision>>,
}

impl TuiReviewer {
    /// Point the reviewer at the current turn's event channel. A turn without
    /// a channel (or before it starts) auto-accepts.
    pub fn attach(&self, events: UnboundedSender<AgentEvent>) {
        if let Ok(mut slot) = self.events.lock() {
            *slot = Some(events);
        }
    }

    /// The turn ended: drop a pending request so the agent's wait resolves to
    /// reject-all instead of hanging.
    pub fn detach(&self) {
        if let Ok(mut slot) = self.events.lock() {
            *slot = None;
        }
        if let Ok(mut pending) = self.pending.lock() {
            pending.take();
        }
    }

    #[cfg(test)]
    pub fn submitted(&self) -> Option<HunkDecision> {
        self.submitted
            .lock()
            .ok()
            .and_then(|decision| decision.clone())
    }
}

#[async_trait]
impl HunkReviewer for TuiReviewer {
    async fn review(&self, request: HunkRequest) -> HunkDecision {
        let count = request.hunks.len();
        let Some(events) = self
            .events
            .lock()
            .ok()
            .and_then(|slot| slot.as_ref().cloned())
        else {
            return HunkDecision::accept_all(count);
        };
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        if let Ok(mut pending) = self.pending.lock() {
            *pending = Some((id, tx));
        }
        if events
            .send(AgentEvent::HunkReview {
                id,
                hunks: request.hunks.clone(),
            })
            .is_err()
        {
            if let Ok(mut pending) = self.pending.lock() {
                pending.take();
            }
            return HunkDecision::accept_all(count);
        }
        tokio::select! {
            decision = rx => decision.unwrap_or_else(|_| HunkDecision::reject_all(count)),
            _ = request.cancel.cancelled() => {
                self.cancel(id);
                HunkDecision::reject_all(count)
            }
        }
    }

    fn submit(&self, id: u64, decision: HunkDecision) {
        if let Ok(mut last) = self.submitted.lock() {
            *last = Some(decision.clone());
        }
        let sender = {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            match pending.take() {
                Some((pending_id, sender)) if pending_id == id => Some(sender),
                other => {
                    *pending = other;
                    None
                }
            }
        };
        if let Some(sender) = sender {
            let _ = sender.send(decision);
        }
    }

    fn cancel(&self, id: u64) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        if pending
            .as_ref()
            .is_some_and(|(pending_id, _)| *pending_id == id)
        {
            pending.take();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::hunk::HunkView;
    use tokio_util::sync::CancellationToken;

    fn request(count: usize) -> HunkRequest {
        HunkRequest {
            hunks: (0..count)
                .map(|index| HunkView {
                    file: "a.rs".into(),
                    start_line: index + 1,
                    end_line: index + 1,
                    added: 1,
                    removed: 1,
                    rationale: "change".into(),
                    text: "new\n".into(),
                })
                .collect(),
            cancel: CancellationToken::new(),
        }
    }

    #[tokio::test]
    async fn a_review_is_delivered_as_an_event_and_answered_by_id() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let reviewer = std::sync::Arc::new(TuiReviewer::default());
        reviewer.attach(tx);
        let waiting = {
            let reviewer = reviewer.clone();
            tokio::spawn(async move { reviewer.review(request(2)).await })
        };
        let event = rx.recv().await.expect("review event");
        let (id, hunks) = match event {
            AgentEvent::HunkReview { id, hunks } => (id, hunks),
            other => panic!("unexpected event {other:?}"),
        };
        assert_eq!(hunks.len(), 2);
        let decision = HunkDecision {
            choices: vec![
                crate::tools::hunk::HunkChoice::Accept,
                crate::tools::hunk::HunkChoice::Reject,
            ],
        };
        reviewer.submit(id, decision.clone());
        assert_eq!(waiting.await.expect("review task"), decision);
    }

    #[tokio::test]
    async fn without_an_attached_turn_review_auto_accepts() {
        let reviewer = TuiReviewer::default();
        let decision = reviewer.review(request(3)).await;
        assert_eq!(
            decision,
            HunkDecision::accept_all(3),
            "no event loop means no human to ask"
        );
    }
}

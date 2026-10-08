//! The provider-switch helpers App.tsx gained with shared history:
//! acceptancePersistence.ts, providerContextPersistence.ts, and the request
//! preflight in firstProviderRequest.ts.
//!
//! A switch saves its receipt at each step. The submission marker is saved
//! before the request goes out, and acceptance is saved after the provider
//! acknowledges it. When an acceptance save fails, further requests pause
//! until a later submit retries the save without sending provider input.

use std::collections::HashMap;

use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::{LocalBoxFuture, Shared};
use gpui::{App, Task};
use monocode_core::block::{Block, BlockNotice, BlockRole};
use monocode_core::session::MessageQueueStatus;
use monocode_core::{Attachment, Session};

use crate::runtime::engine::Engine;
use crate::runtime::session_store::should_persist_session;

/// `AcceptancePersistenceError`'s message.
pub const ACCEPTANCE_SAVE_FAILED: &str = "The provider accepted this request, but MonoCode could not save its acceptance. Further requests are paused. Try sending again to retry the save without sending provider input.";

/// The question before the user confirms inspection of a request that may
/// have run.
pub const INSPECTION_PROMPT: &str = "MonoCode could not confirm whether the previous request reached the provider. Check the working copy and any external effects. Inspect the previous native conversation if it is available.\n\nConfirm that you checked those results. This action saves your acknowledgment and sends no request. Submit a new request afterward to continue.";

pub const INSPECTION_SAVED: &str = "Inspection acknowledgment saved. The previous request was not resent. Submit a new request to continue.";
pub const ACCEPTANCE_SAVED: &str =
    "The accepted request is saved. Submit again to send a new request.";
pub const SWITCH_BEFORE_COMMAND: &str =
    "Send a chat request to complete the provider switch before running a provider command.";
pub const TRANSFER_FAILED: &str =
    "The context transfer failed. Retry the request or select the previous provider.";

/// `withAcceptancePersistenceError`: pause the queue and show why.
pub fn with_acceptance_persistence_error(session: &mut Session, message: &str) {
    session.queue_status = Some(MessageQueueStatus::Paused);
    session.blocks.push(Block {
        notice: Some(BlockNotice::Error),
        ..Block::new(uuid::Uuid::new_v4().to_string(), BlockRole::System, message)
    });
}

/// `saveProviderContextSession`: save a switch checkpoint now. Temporary
/// sessions (the home folder, the inbox) skip the record but still fail
/// when the session is gone.
pub fn save_provider_context_session(
    session: Option<Session>,
    failure: &str,
    cx: &App,
) -> Task<Result<(), String>> {
    let Some(session) = session else {
        return Task::ready(Err(failure.to_string()));
    };
    if !should_persist_session(&session) {
        return Task::ready(Ok(()));
    }
    let upsert = Engine::writer(cx).upsert_session(&session);
    let failure = failure.to_string();
    cx.background_executor().spawn(async move {
        upsert
            .await
            .map(|_| ())
            .map_err(|error| format!("{failure} {error}"))
    })
}

/// `firstProviderRequestBudget`'s attachment half: the tokens the
/// prepared attachments take.
pub fn request_attachment_tokens(attachments: &[Attachment]) -> usize {
    monocode_core::portable_context::current_attachment_tokens(attachments)
}

/// What a submit may do while an acceptance save is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptanceSubmission {
    /// Nothing is pending.
    Submit,
    /// The save is running and a turn is active: queue the request.
    Queue,
    /// The save is running and the session is idle: hold the request.
    Wait,
    /// The save failed: retry it instead of sending.
    Reconcile,
}

/// A save's outcome, for every caller that waits on it.
pub type SharedSave = Shared<LocalBoxFuture<'static, Result<(), String>>>;

struct Entry {
    switch_id: String,
    generation: u64,
    failed: bool,
    done: SharedSave,
}

/// `createAcceptancePersistence`: open acceptance saves by session.
#[derive(Default)]
pub struct AcceptancePersistence {
    entries: HashMap<String, Entry>,
    next: u64,
}

impl AcceptancePersistence {
    /// Record a save that just started. The returned sender reports its
    /// outcome; [`AcceptancePersistence::finish`] settles the entry.
    pub fn begin(
        &mut self,
        session_id: &str,
        switch_id: &str,
    ) -> (u64, oneshot::Sender<Result<(), String>>) {
        self.next += 1;
        let generation = self.next;
        let (sender, receiver) = oneshot::channel();
        let done = receiver
            .map(|result| result.unwrap_or_else(|_| Err(ACCEPTANCE_SAVE_FAILED.to_string())))
            .boxed_local()
            .shared();
        self.entries.insert(
            session_id.to_string(),
            Entry {
                switch_id: switch_id.to_string(),
                generation,
                failed: false,
                done,
            },
        );
        (generation, sender)
    }

    /// Settle the save `generation` started. A success forgets the entry; a
    /// failure keeps it so the next submit reconciles. `true` when the entry
    /// was still current.
    pub fn finish(&mut self, session_id: &str, generation: u64, saved: bool) -> bool {
        let current = self
            .entries
            .get(session_id)
            .is_some_and(|entry| entry.generation == generation);
        if !current {
            return false;
        }
        if saved {
            self.entries.remove(session_id);
        } else if let Some(entry) = self.entries.get_mut(session_id) {
            entry.failed = true;
        }
        true
    }

    /// `hasPending`.
    pub fn has_pending(&self, session_id: &str) -> bool {
        self.entries.contains_key(session_id)
    }

    /// `submissionMode`.
    pub fn submission_mode(&self, session_id: &str, busy: bool) -> AcceptanceSubmission {
        match self.entries.get(session_id) {
            None => AcceptanceSubmission::Submit,
            Some(entry) if entry.failed => AcceptanceSubmission::Reconcile,
            Some(_) if busy => AcceptanceSubmission::Queue,
            Some(_) => AcceptanceSubmission::Wait,
        }
    }

    /// `wait`: the open save for this session (and switch, when given).
    pub fn wait(&self, session_id: &str, switch_id: Option<&str>) -> Option<SharedSave> {
        self.entries
            .get(session_id)
            .filter(|entry| switch_id.is_none_or(|id| entry.switch_id == id))
            .map(|entry| entry.done.clone())
    }

    /// The switch whose acceptance save failed, for a reconciling submit.
    pub fn failed_switch(&self, session_id: &str) -> Option<String> {
        self.entries
            .get(session_id)
            .filter(|entry| entry.failed)
            .map(|entry| entry.switch_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_what_a_submit_may_do_while_a_save_runs_or_fails() {
        let mut saves = AcceptancePersistence::default();
        assert_eq!(
            saves.submission_mode("s", false),
            AcceptanceSubmission::Submit
        );
        let (generation, _sender) = saves.begin("s", "switch-1");
        assert!(saves.has_pending("s"));
        assert_eq!(
            saves.submission_mode("s", true),
            AcceptanceSubmission::Queue
        );
        assert_eq!(
            saves.submission_mode("s", false),
            AcceptanceSubmission::Wait
        );
        assert!(saves.wait("s", Some("other")).is_none());
        assert!(saves.wait("s", Some("switch-1")).is_some());
        assert!(saves.finish("s", generation, false));
        assert_eq!(
            saves.submission_mode("s", false),
            AcceptanceSubmission::Reconcile
        );
        assert_eq!(saves.failed_switch("s").as_deref(), Some("switch-1"));
        let (retry, _sender) = saves.begin("s", "switch-1");
        // A late outcome for the first save does not settle the retry.
        assert!(!saves.finish("s", generation, true));
        assert!(saves.finish("s", retry, true));
        assert!(!saves.has_pending("s"));
    }

    #[test]
    fn every_waiter_sees_the_outcome_of_the_save() {
        let mut saves = AcceptancePersistence::default();
        let (_, sender) = saves.begin("s", "switch-1");
        let first = saves.wait("s", None).unwrap();
        let second = saves.wait("s", Some("switch-1")).unwrap();
        sender.send(Err("Disk full".into())).unwrap();
        assert_eq!(
            futures::executor::block_on(first),
            Err("Disk full".to_string())
        );
        assert_eq!(
            futures::executor::block_on(second),
            Err("Disk full".to_string())
        );
    }

    #[test]
    fn a_failed_save_pauses_the_queue_and_shows_the_error() {
        let mut session =
            Session::blank("s", monocode_core::HarnessId::Codex, "codex:gpt", "/repo");
        with_acceptance_persistence_error(&mut session, ACCEPTANCE_SAVE_FAILED);
        assert_eq!(session.queue_status, Some(MessageQueueStatus::Paused));
        let notice = session.blocks.last().unwrap();
        assert_eq!(notice.notice, Some(BlockNotice::Error));
        assert_eq!(notice.text, ACCEPTANCE_SAVE_FAILED);
    }
}

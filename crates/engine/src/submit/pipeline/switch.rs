//! The `Submit` side of provider switching in App.tsx: acceptance saves,
//! their reconciliation, the inspection acknowledgment for a request that
//! may have run, and the running-selection bookkeeping that keeps a turn's
//! reports apart from a picker change.

use gpui::Context;
use monocode_core::HarnessEvent;
use monocode_core::block::ModelTarget;
use monocode_core::provider_context::confirm_provider_delivery_inspection;

use super::Submit;
use super::submit::report;
use crate::runtime::engine::Engine;
use crate::submit::provider_switch::{
    ACCEPTANCE_SAVE_FAILED, ACCEPTANCE_SAVED, INSPECTION_PROMPT, INSPECTION_SAVED, SharedSave,
    save_provider_context_session, with_acceptance_persistence_error,
};

impl Submit {
    /// `acceptancePersistence.start`: save the accepted switch now. A failed
    /// save pauses the queue and shows the error; the next submit retries.
    pub(crate) fn start_acceptance_save(
        &mut self,
        session_id: &str,
        switch_id: &str,
        cx: &mut Context<Self>,
    ) -> Option<SharedSave> {
        let (generation, sender) = self.acceptance.begin(session_id, switch_id);
        let latest = Engine::sessions(cx).read(cx).get(session_id).cloned();
        let save =
            save_provider_context_session(latest, "The accepted session could not be saved.", cx);
        let id = session_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = save.await;
            let saved = result.is_ok();
            let _ = this.update(cx, |this, cx| {
                this.acceptance.finish(&id, generation, saved);
                // A save also wakes the queue, which waits while it runs.
                Engine::sessions(cx).update(cx, |sessions, cx| {
                    sessions.update(&id, cx, |session| {
                        if !saved {
                            with_acceptance_persistence_error(session, ACCEPTANCE_SAVE_FAILED);
                        }
                    });
                });
            });
            let _ = sender.send(result.map_err(|_| ACCEPTANCE_SAVE_FAILED.to_string()));
        })
        .detach();
        self.acceptance.wait(session_id, Some(switch_id))
    }

    /// `acceptancePersistence.reconcile`: a submit after a failed acceptance
    /// save retries the save and sends nothing.
    pub(crate) fn reconcile_acceptance(&mut self, session_id: &str, cx: &mut Context<Self>) {
        let Some(switch_id) = self.acceptance.failed_switch(session_id) else {
            return;
        };
        Engine::sessions(cx).update(cx, |sessions, cx| {
            sessions.update(session_id, cx, |session| {
                session.queue_status = Some(monocode_core::session::MessageQueueStatus::Paused);
            });
        });
        let Some(saved) = self.start_acceptance_save(session_id, &switch_id, cx) else {
            return;
        };
        let id = session_id.to_string();
        cx.spawn(async move |_, cx| {
            // The save reports its own storage error and keeps the queue paused.
            if saved.await.is_ok() {
                cx.update(|cx| {
                    report(
                        &id,
                        HarnessEvent::Status {
                            text: ACCEPTANCE_SAVED.into(),
                        },
                        cx,
                    )
                });
            }
        })
        .detach();
    }

    /// The inspection acknowledgment: ask, save the recovery decision, and
    /// send no request.
    pub(crate) fn confirm_delivery_inspection(
        &mut self,
        session_id: &str,
        switch_id: &str,
        cx: &mut Context<Self>,
    ) {
        if !self.inspection_pending.insert(session_id.to_string()) {
            return;
        }
        let confirm =
            Engine::hooks(cx)
                .workspace
                .confirm(INSPECTION_PROMPT, "Confirm inspection", cx);
        let id = session_id.to_string();
        let switch_id = switch_id.to_string();
        cx.spawn(async move |this, cx| {
            let confirmed = confirm.await;
            let matches = |session: &monocode_core::Session| {
                session
                    .provider_context
                    .as_ref()
                    .and_then(|state| state.delivery.as_ref())
                    .is_some_and(|delivery| delivery.switch_id == switch_id)
            };
            let latest = cx.update(|cx| Engine::sessions(cx).read(cx).get(&id).cloned());
            if confirmed && let Some(latest) = latest.filter(|latest| matches(latest)) {
                let mut inspected = latest;
                confirm_provider_delivery_inspection(&mut inspected);
                let save = cx.update(|cx| {
                    save_provider_context_session(
                        Some(inspected),
                        "The inspection acknowledgment could not be saved.",
                        cx,
                    )
                });
                let saved = save.await;
                cx.update(|cx| {
                    Engine::sessions(cx).update(cx, |sessions, cx| {
                        sessions.update(&id, cx, |session| match &saved {
                            Ok(()) if matches(session) => {
                                confirm_provider_delivery_inspection(session)
                            }
                            Ok(()) => {}
                            Err(error) => with_acceptance_persistence_error(session, error),
                        });
                    });
                    if saved.is_ok() {
                        report(
                            &id,
                            HarnessEvent::Status {
                                text: INSPECTION_SAVED.into(),
                            },
                            cx,
                        );
                    }
                });
            }
            let _ = this.update(cx, |this, _| this.inspection_pending.remove(&id));
        })
        .detach();
    }

    /// Note a picker or model-settings change.
    pub(crate) fn bump_selection_revision(&mut self, session_id: &str) {
        *self
            .selection_revisions
            .entry(session_id.to_string())
            .or_default() += 1;
    }

    pub(crate) fn selection_revision(&self, session_id: &str) -> u64 {
        self.selection_revisions
            .get(session_id)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn running_selection(&self, session_id: &str) -> Option<&ModelTarget> {
        self.running_selections.get(session_id)
    }
}

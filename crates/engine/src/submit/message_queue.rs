//! The parts of src/features/sessions/model/messageQueue.ts that
//! `submitSession` calls: which queued row may send, and how a sent row
//! leaves the queue.
//!
//! The attention package owns the queue and its dispatch (`attention::queue`
//! has the full port). These copies keep `--features submit` building on its
//! own; the lead can point both at one module once the packages merge.

use monocode_core::block::ModelTarget;
use monocode_core::session::{MessageQueueStatus, QueuedMessage};
use monocode_core::{ModelCatalog, Session};

use super::handoff::is_preparing_handoff;

/// How a queued row reaches `submit`: the idle head sends on its own, or the
/// user steers any row in now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueSubmitMode {
    Dispatch,
    Steer,
}

/// `queuedHead`.
pub fn queued_head(session: &Session) -> Option<&QueuedMessage> {
    session.queued_messages.as_ref()?.first()
}

/// `isEditingQueuedHead`: hold auto-dispatch only while the row about to
/// send is being edited.
pub fn is_editing_queued_head(session: &Session) -> bool {
    queued_head(session)
        .is_some_and(|head| session.editing_queued_message_id.as_deref() == Some(head.id.as_str()))
}

/// `dequeueQueuedMessage`.
pub fn dequeue_queued_message(session: &mut Session, message_id: &str) {
    let remaining: Vec<QueuedMessage> = session
        .queued_messages
        .take()
        .unwrap_or_default()
        .into_iter()
        .filter(|message| message.id != message_id)
        .collect();
    if remaining.is_empty() {
        session.queue_status = None;
    } else {
        session.queued_messages = Some(remaining);
    }
    if session.editing_queued_message_id.as_deref() == Some(message_id) {
        session.editing_queued_message_id = None;
    }
}

/// A provider request may have run without acknowledgment, so nothing
/// else goes out until the user confirms inspection.
pub fn needs_delivery_inspection(session: &Session) -> bool {
    session
        .provider_context
        .as_ref()
        .and_then(|state| state.delivery.as_ref())
        .is_some_and(|delivery| delivery.needs_inspection())
}

/// `canDispatchQueuedHead`: the idle session can send its queued head.
pub fn can_dispatch_queued_head(session: &Session) -> bool {
    if session.is_busy() || session.usage_limit.is_some() || needs_delivery_inspection(session) {
        return false;
    }
    if matches!(
        session.queue_status,
        Some(MessageQueueStatus::Paused | MessageQueueStatus::Resuming)
    ) {
        return false;
    }
    if queued_head(session).is_none() || is_editing_queued_head(session) {
        return false;
    }
    !is_preparing_handoff(session)
}

/// `queuedMessageForSubmit`: resolve a queued row for auto-dispatch (the
/// idle head) or an explicit steer (any row).
pub fn queued_message_for_submit<'a>(
    session: &'a Session,
    message_id: &str,
    mode: QueueSubmitMode,
) -> Option<&'a QueuedMessage> {
    if needs_delivery_inspection(session) {
        return None;
    }
    let message = session
        .queued_messages
        .as_ref()?
        .iter()
        .find(|entry| entry.id == message_id)?;
    if mode == QueueSubmitMode::Steer {
        return Some(message);
    }
    if queued_head(session).map(|head| head.id.as_str()) != Some(message_id) {
        return None;
    }
    can_dispatch_queued_head(session).then_some(message)
}

/// `sameSteeringSelection`: the queued row's provider, model, and resolved
/// settings match the running turn's.
fn same_steering_selection(
    saved: &ModelTarget,
    active: &ModelTarget,
    catalog: &ModelCatalog,
) -> bool {
    if saved.harness != active.harness || saved.model != active.model {
        return false;
    }
    let model = catalog.resolve_model(saved.harness, Some(&saved.model));
    let resolve = |settings: &monocode_core::ModelSettings| {
        let mut merged = settings.clone();
        merged.extend(catalog.merge_model_settings(&model, Some(settings)));
        merged
    };
    resolve(&saved.model_settings) == resolve(&active.model_settings)
}

/// A queued row may steer the running turn only when it was queued for that
/// turn's provider and model. Rows queued for another selection wait.
pub fn can_steer_with_selection(
    session: &Session,
    message: &QueuedMessage,
    active: &ModelTarget,
    catalog: &ModelCatalog,
) -> bool {
    !session.is_busy()
        || message
            .selection
            .as_ref()
            .is_none_or(|saved| same_steering_selection(saved, active, catalog))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::submit::handoff::append_preparing_handoff;
    use monocode_core::HarnessId;
    use monocode_core::session::UsageLimit;

    fn queued(id: &str) -> QueuedMessage {
        QueuedMessage {
            selection: None,
            app_request_id: None,
            id: id.into(),
            text: id.into(),
            attachments: vec![],
            note_card: None,
            handoff_card: None,
            intent: None,
        }
    }

    fn chat() -> Session {
        Session {
            queued_messages: Some(vec![queued("a"), queued("b")]),
            queue_status: Some(MessageQueueStatus::Active),
            ..Session::blank("s", HarnessId::Claude, "claude:default", "/tmp/project")
        }
    }

    #[test]
    fn returns_the_first_queued_follow_up() {
        assert_eq!(queued_head(&chat()).unwrap().id, "a");
        let empty = Session {
            queued_messages: None,
            ..chat()
        };
        assert!(queued_head(&empty).is_none());
    }

    #[test]
    fn is_true_only_when_the_head_row_is_the_one_being_edited() {
        assert!(!is_editing_queued_head(&chat()));
        let editing = |id: &str| Session {
            editing_queued_message_id: Some(id.into()),
            ..chat()
        };
        assert!(is_editing_queued_head(&editing("a")));
        assert!(!is_editing_queued_head(&editing("b")));
    }

    #[test]
    fn dispatches_an_idle_session_with_a_queued_head() {
        assert!(can_dispatch_queued_head(&chat()));
    }

    #[test]
    fn holds_while_busy_paused_resuming_usage_limited_or_editing_the_head() {
        assert!(!can_dispatch_queued_head(&Session {
            busy: Some(true),
            ..chat()
        }));
        for status in [MessageQueueStatus::Paused, MessageQueueStatus::Resuming] {
            assert!(!can_dispatch_queued_head(&Session {
                queue_status: Some(status),
                ..chat()
            }));
        }
        assert!(!can_dispatch_queued_head(&Session {
            usage_limit: Some(UsageLimit {
                resets_at: Some(1_000),
                resume_at_reset: None
            }),
            ..chat()
        }));
        assert!(!can_dispatch_queued_head(&Session {
            editing_queued_message_id: Some("a".into()),
            ..chat()
        }));
        assert!(can_dispatch_queued_head(&Session {
            editing_queued_message_id: Some("b".into()),
            ..chat()
        }));
    }

    #[test]
    fn does_not_dispatch_during_a_preparing_handoff_or_with_an_empty_queue() {
        let preparing = append_preparing_handoff(
            &Session {
                queued_messages: Some(vec![queued("a")]),
                ..chat()
            },
            HarnessId::Claude,
            HarnessId::Cursor,
        );
        assert!(!can_dispatch_queued_head(&preparing));
        assert!(!can_dispatch_queued_head(&Session {
            queued_messages: None,
            ..chat()
        }));
    }

    #[test]
    fn drops_the_id_and_clears_queue_state_when_the_last_item_goes() {
        let mut one = Session {
            queued_messages: Some(vec![queued("a")]),
            ..chat()
        };
        dequeue_queued_message(&mut one, "a");
        assert_eq!(one.queued_messages, None);
        assert_eq!(one.queue_status, None);
    }

    #[test]
    fn keeps_editing_another_row_after_the_head_is_sent() {
        let mut next = Session {
            editing_queued_message_id: Some("b".into()),
            ..chat()
        };
        dequeue_queued_message(&mut next, "a");
        let ids: Vec<&str> = next
            .queued_messages
            .iter()
            .flatten()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, ["b"]);
        assert_eq!(next.editing_queued_message_id.as_deref(), Some("b"));
        assert_eq!(next.queue_status, Some(MessageQueueStatus::Active));
    }

    #[test]
    fn only_auto_dispatches_the_idle_head_and_steers_any_row() {
        let session = chat();
        assert_eq!(
            queued_message_for_submit(&session, "a", QueueSubmitMode::Dispatch)
                .map(|m| m.id.as_str()),
            Some("a")
        );
        assert!(queued_message_for_submit(&session, "b", QueueSubmitMode::Dispatch).is_none());
        let busy = Session {
            busy: Some(true),
            ..chat()
        };
        assert!(queued_message_for_submit(&busy, "a", QueueSubmitMode::Dispatch).is_none());
        assert_eq!(
            queued_message_for_submit(&busy, "b", QueueSubmitMode::Steer).map(|m| m.id.as_str()),
            Some("b")
        );
        let paused = Session {
            queue_status: Some(MessageQueueStatus::Paused),
            ..chat()
        };
        assert!(queued_message_for_submit(&paused, "a", QueueSubmitMode::Steer).is_some());
        assert!(queued_message_for_submit(&session, "missing", QueueSubmitMode::Steer).is_none());
    }

    fn target(model: &str, effort: Option<&str>) -> ModelTarget {
        let mut model_settings = monocode_core::ModelSettings::new();
        if let Some(effort) = effort {
            model_settings.insert("effort".into(), effort.into());
        }
        ModelTarget {
            harness: HarnessId::Claude,
            model: model.into(),
            model_settings,
        }
    }

    #[test]
    fn pauses_dispatch_and_steering_while_a_request_needs_inspection() {
        let mut session = chat();
        session.provider_context = serde_json::from_value(serde_json::json!({
            "version": 1, "bindings": [],
            "delivery": {
                "switchId": "s", "status": "uncertain", "mode": "native", "from": "codex",
                "to": "claude", "cwd": "/tmp/project", "currentUserBlockId": "u",
                "includedBlockIds": [], "omittedBlockIds": [], "requestSubmitted": true,
                "needsInspection": true,
            },
        }))
        .ok();
        assert!(!can_dispatch_queued_head(&session));
        assert!(queued_message_for_submit(&session, "b", QueueSubmitMode::Steer).is_none());
    }

    #[test]
    fn steers_only_rows_queued_for_the_running_selection() {
        let catalog = ModelCatalog::new();
        let mut busy = Session {
            busy: Some(true),
            ..chat()
        };
        let mut row = queued("a");
        row.selection = Some(target("claude:opus", Some("high")));
        busy.queued_messages = Some(vec![row.clone()]);
        assert!(can_steer_with_selection(
            &busy,
            &row,
            &target("claude:opus", Some("high")),
            &catalog
        ));
        assert!(!can_steer_with_selection(
            &busy,
            &row,
            &target("claude:sonnet", Some("high")),
            &catalog
        ));
        assert!(!can_steer_with_selection(
            &busy,
            &row,
            &target("claude:opus", Some("low")),
            &catalog
        ));
        let idle = Session {
            busy: None,
            ..busy.clone()
        };
        assert!(can_steer_with_selection(
            &idle,
            &row,
            &target("claude:sonnet", None),
            &catalog
        ));
        assert!(can_steer_with_selection(
            &busy,
            &queued("legacy"),
            &target("claude:sonnet", None),
            &catalog
        ));
    }
}

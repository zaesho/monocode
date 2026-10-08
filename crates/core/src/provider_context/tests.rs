//! Port of src/features/sessions/model/providerContext.test.ts.

use serde_json::json;

use super::*;
use crate::block::{Block, HandoffMeta, HandoffStatus};
use crate::context_usage::ContextUsage;
use crate::portable_context::{PortableContextOptions, build_portable_context};
use crate::session::{MessageQueueStatus, QueuedMessage};

const CWD: &str = "/tmp/project";

fn source() -> ProviderBinding {
    ProviderBinding {
        harness: HarnessId::Claude,
        cwd: CWD.into(),
        provider_session_id: "claude-native".into(),
        provider_account_id: None,
        delivered_through_block_id: Some("a1".into()),
        context_used: None,
        context_window: None,
    }
}

fn switched() -> Session {
    let mut session = Session::blank("s1", HarnessId::Claude, "claude:sonnet", CWD);
    session.provider_session_id = Some("claude-native".into());
    session.blocks = vec![
        Block::new("u1", BlockRole::User, "Keep the early instruction exactly."),
        Block::new("a1", BlockRole::Assistant, "Source response."),
    ];
    remember_provider_binding(&mut session, source());
    session.harness = HarnessId::Codex;
    session.provider_session_id = None;
    session.pending_switch = Some(PendingHarnessSwitch {
        from: HarnessId::Claude,
        from_model: "claude:sonnet".into(),
        from_settings: ModelSettings::new(),
        from_provider_session_id: Some("claude-native".into()),
        from_provider_account_id: None,
    });
    session
}

fn preparing() -> Session {
    let mut session = switched();
    begin_provider_delivery(
        &mut session,
        DeliveryStart {
            switch_id: "switch-1".into(),
            from: Some(HarnessId::Claude),
            to: Some(HarnessId::Codex),
            cwd: CWD.into(),
            current_user_block_id: "u2".into(),
            source_through_block_id: Some("a1".into()),
            included_block_ids: vec!["u1".into(), "a1".into()],
            ..Default::default()
        },
    );
    session
}

fn delivery(session: &Session) -> Option<&ProviderContextDelivery> {
    session.provider_context.as_ref()?.delivery.as_ref()
}

fn handoff_block(switch_id: &str) -> Block {
    Block {
        handoff: Some(HandoffMeta {
            from: HarnessId::Claude,
            to: HarnessId::Codex,
            status: HandoffStatus::Ready,
            pending: Some(true),
            transfer: Some(HandoffTransfer {
                switch_id: switch_id.into(),
                status: TransferStatus::Preparing,
                mode: TransferMode::Pending,
                included: 2,
                omitted: 0,
                historical_attachments: 0,
                retrieval_path: None,
                request_submitted: None,
                failed_before_submission: None,
                needs_inspection: None,
                inspection_confirmed: None,
            }),
            extra: Default::default(),
        }),
        ..Block::new("switch", BlockRole::Handoff, "Shared history")
    }
}

fn transfer<'a>(session: &'a Session, id: &str) -> &'a HandoffTransfer {
    session
        .blocks
        .iter()
        .find(|block| block.id == id)
        .and_then(|block| block.handoff.as_ref())
        .and_then(|handoff| handoff.transfer.as_ref())
        .unwrap()
}

fn draft_of(session: &Session, id: &str) -> Option<bool> {
    session
        .blocks
        .iter()
        .find(|block| block.id == id)
        .and_then(|block| block.draft)
}

fn context_texts(session: &Session) -> Vec<String> {
    build_portable_context(session, &PortableContextOptions::default())
        .unwrap()
        .items
        .into_iter()
        .map(|item| item.text)
        .collect()
}

fn target(harness: HarnessId, model: &str) -> ModelTarget {
    ModelTarget {
        harness,
        model: model.into(),
        model_settings: ModelSettings::new(),
    }
}

#[test]
fn routes_active_requests_to_the_running_source_after_picker_retargeting() {
    let mut session = switched();
    session.busy = Some(true);
    let mut source_selection = target(HarnessId::Claude, "claude:sonnet");
    source_selection
        .model_settings
        .insert("effort".into(), "high".into());
    assert_eq!(
        running_provider_selection(&session, Some(&source_selection)),
        source_selection
    );
    let prepared = target(HarnessId::Codex, "codex:gpt");
    assert_eq!(
        running_provider_selection(&session, Some(&prepared)),
        prepared
    );
    assert_eq!(
        running_provider_selection(&session, None).harness,
        HarnessId::Claude
    );
    session.busy = Some(false);
    assert_eq!(
        running_provider_selection(&session, Some(&source_selection)).harness,
        HarnessId::Codex
    );
}

#[test]
fn rejects_late_config_events_after_a_same_provider_picker_change() {
    let mut running = target(HarnessId::Claude, "claude:original");
    running
        .model_settings
        .insert("effort".into(), "high".into());
    let mut session = Session::blank("s", HarnessId::Claude, "claude:original", CWD);
    session.model_settings = running.model_settings.clone();
    session.busy = Some(true);
    assert!(can_apply_running_configuration(&session, &running, None));
    let mut next = session.clone();
    next.model = "claude:next".into();
    assert!(!can_apply_running_configuration(&next, &running, None));
    let mut low = session.clone();
    low.model_settings.insert("effort".into(), "low".into());
    assert!(!can_apply_running_configuration(&low, &running, None));
    assert!(can_apply_running_configuration(
        &next,
        &running,
        Some((2, 2))
    ));
    assert!(!can_apply_running_configuration(
        &next,
        &running,
        Some((2, 3))
    ));
}

#[test]
fn seeds_a_legacy_native_binding_without_changing_the_session() {
    let mut session = Session::blank("s", HarnessId::Claude, "claude:sonnet", CWD);
    session.provider_session_id = Some("legacy".into());
    session.blocks = vec![Block::new("u", BlockRole::User, "Hello")];
    let binding = provider_binding(&session, HarnessId::Claude, CWD, None).unwrap();
    assert_eq!(binding.provider_session_id, "legacy");
    assert_eq!(binding.delivered_through_block_id.as_deref(), Some("u"));
    assert!(session.provider_context.is_none());
    assert_eq!(
        provider_binding(&session, HarnessId::Claude, CWD, Some("default"))
            .map(|binding| binding.provider_session_id),
        Some("legacy".into())
    );
}

#[test]
fn requires_matching_account_and_working_directory() {
    let mut session = switched();
    remember_provider_binding(
        &mut session,
        ProviderBinding {
            provider_account_id: Some("work".into()),
            ..source()
        },
    );
    assert!(provider_binding(&session, HarnessId::Claude, CWD, Some("work")).is_some());
    assert!(provider_binding(&session, HarnessId::Claude, CWD, Some("personal")).is_none());
    assert!(provider_binding(&session, HarnessId::Claude, "/tmp/other", Some("work")).is_none());
}

#[test]
fn replaces_default_account_aliases_without_merging_named_accounts() {
    let mut session = switched();
    remember_provider_binding(
        &mut session,
        ProviderBinding {
            provider_account_id: Some("work".into()),
            provider_session_id: "work-native".into(),
            ..source()
        },
    );
    remember_provider_binding(
        &mut session,
        ProviderBinding {
            provider_account_id: Some("default".into()),
            provider_session_id: "default-native".into(),
            ..source()
        },
    );
    assert_eq!(session.provider_context.as_ref().unwrap().bindings.len(), 2);
    let id = |account: Option<&str>| {
        provider_binding(&session, HarnessId::Claude, CWD, account)
            .map(|binding| binding.provider_session_id)
    };
    assert_eq!(id(None).as_deref(), Some("default-native"));
    assert_eq!(id(Some("")).as_deref(), Some("default-native"));
    assert_eq!(id(Some("work")).as_deref(), Some("work-native"));
    assert_eq!(id(Some("personal")), None);
}

#[test]
fn requires_fresh_history_when_the_saved_boundary_is_gone() {
    let session = switched();
    let saved = provider_binding(&session, HarnessId::Claude, CWD, None);
    assert!(can_resume_provider_binding(&session, saved.as_ref()));
    let mut recovered = session.clone();
    recovered.blocks = vec![
        session.blocks[0].clone(),
        Block::new("b1", BlockRole::Assistant, "The later provider response."),
    ];
    let stale = provider_binding(&recovered, HarnessId::Claude, CWD, None);
    assert_eq!(
        stale
            .as_ref()
            .and_then(|stale| stale.delivered_through_block_id.as_deref()),
        Some("a1")
    );
    assert!(!can_resume_provider_binding(&recovered, stale.as_ref()));
    assert!(!can_resume_provider_binding(
        &session,
        Some(&ProviderBinding {
            delivered_through_block_id: None,
            ..source()
        })
    ));
    assert!(!can_resume_provider_binding(&session, None));
}

#[test]
fn keeps_the_target_selection_when_a_running_source_reports_its_identity() {
    let mut session = switched();
    record_provider_bound(
        &mut session,
        HarnessId::Claude,
        CWD,
        "new-source-native",
        None,
    );
    assert_eq!(session.harness, HarnessId::Codex);
    assert!(session.provider_session_id.is_none());
    assert_eq!(
        session
            .pending_switch
            .as_ref()
            .and_then(|pending| pending.from_provider_session_id.as_deref()),
        Some("new-source-native")
    );
    assert_eq!(
        provider_binding(&session, HarnessId::Claude, CWD, None)
            .unwrap()
            .provider_session_id,
        "new-source-native"
    );
}

#[test]
fn provider_startup_is_not_turn_acceptance() {
    let mut session = preparing();
    record_provider_bound(&mut session, HarnessId::Codex, CWD, "target-native", None);
    let saved = delivery(&session).unwrap();
    assert_eq!(saved.status, TransferStatus::Preparing);
    assert_eq!(
        saved.target_provider_session_id.as_deref(),
        Some("target-native")
    );
    assert_eq!(
        session
            .pending_switch
            .as_ref()
            .and_then(|pending| pending.from_provider_session_id.as_deref()),
        Some("claude-native")
    );
}

#[test]
fn requires_a_fresh_target_after_restart_before_or_after_import() {
    let mut imported = preparing();
    mark_provider_context_delivered(
        &mut imported,
        "switch-1",
        TransferMode::Native,
        Some("target-native"),
        DeliveryCoverage::default(),
    );
    for session in [preparing(), imported] {
        let mut restored = session.clone();
        restored.provider_context = sanitize_provider_context(
            &serde_json::to_value(session.provider_context.as_ref().unwrap()).unwrap(),
        );
        assert!(requires_fresh_provider_binding(
            &restored,
            HarnessId::Codex,
            CWD,
            None
        ));
        assert_eq!(
            provider_binding(&restored, HarnessId::Claude, CWD, None)
                .unwrap()
                .provider_session_id,
            "claude-native"
        );
    }
}

#[test]
fn keeps_native_import_distinct_from_an_accepted_request() {
    let mut session = preparing();
    mark_provider_context_delivered(
        &mut session,
        "switch-1",
        TransferMode::Native,
        Some("target-native"),
        DeliveryCoverage::default(),
    );
    assert_eq!(delivery(&session).unwrap().status, TransferStatus::Imported);
    assert!(session.pending_switch.is_some());
    accept_provider_delivery(&mut session, "switch-1");
    assert_eq!(delivery(&session).unwrap().status, TransferStatus::Accepted);
    assert!(session.pending_switch.is_none());
    assert!(!requires_fresh_provider_binding(
        &session,
        HarnessId::Codex,
        CWD,
        None
    ));
}

#[test]
fn preserves_a_possibly_executed_request_until_inspection() {
    let mut session = preparing();
    session.blocks.push(handoff_block("switch-1"));
    session
        .blocks
        .push(Block::new("u2", BlockRole::User, "Apply the change once"));
    session.queued_messages = Some(vec![QueuedMessage {
        selection: None,
        id: "q1".into(),
        text: "Follow up".into(),
        attachments: Vec::new(),
        note_card: None,
        handoff_card: None,
        intent: None,
        app_request_id: None,
    }]);
    session.queue_status = Some(MessageQueueStatus::Active);
    mark_provider_request_submitted(&mut session, "switch-1");
    assert!(delivery(&session).unwrap().is_submitted());
    assert_eq!(transfer(&session, "switch").request_submitted, Some(true));
    let marked = session.clone();
    mark_provider_request_submitted(&mut session, "switch-1");
    assert_eq!(session, marked);
    mark_provider_context_delivered(
        &mut session,
        "switch-1",
        TransferMode::Native,
        Some("target-native"),
        DeliveryCoverage::default(),
    );
    record_provider_bound(&mut session, HarnessId::Codex, CWD, "target-native", None);
    let bound = provider_binding(&session, HarnessId::Codex, CWD, None).unwrap();
    remember_provider_binding(
        &mut session,
        ProviderBinding {
            delivered_through_block_id: Some("a1".into()),
            ..bound
        },
    );
    let imported = session.clone();
    recover_submitted_provider_delivery(&mut session, "switch-1");
    let recovered = delivery(&session).unwrap();
    assert_eq!(recovered.status, TransferStatus::Uncertain);
    assert!(recovered.needs_inspection());
    assert_eq!(
        session.provider_context.as_ref().unwrap().bindings,
        imported.provider_context.as_ref().unwrap().bindings
    );
    assert_eq!(
        session.provider_session_id.as_deref(),
        Some("target-native")
    );
    assert_eq!(draft_of(&session, "u2"), None);
    assert_eq!(session.queue_status, Some(MessageQueueStatus::Paused));
    assert!(!requires_fresh_provider_binding(
        &session,
        HarnessId::Codex,
        CWD,
        None
    ));
    let mut busy = session.clone();
    busy.busy = Some(true);
    let before = busy.clone();
    confirm_provider_delivery_inspection(&mut busy);
    assert_eq!(busy, before);
    let recovered = session.clone();
    confirm_provider_delivery_inspection(&mut session);
    assert!(delivery(&session).is_none());
    assert_eq!(session.pending_switch, recovered.pending_switch);
    assert_eq!(
        provider_binding(&session, HarnessId::Claude, CWD, None),
        provider_binding(&imported, HarnessId::Claude, CWD, None)
    );
    let target = provider_binding(&session, HarnessId::Codex, CWD, None).unwrap();
    assert_eq!(target.provider_session_id, "target-native");
    assert!(target.delivered_through_block_id.is_none());
    assert_eq!(draft_of(&session, "u2"), None);
    let shown = transfer(&session, "switch");
    assert_eq!(shown.status, TransferStatus::Uncertain);
    assert_eq!(shown.inspection_confirmed, Some(true));
    assert_eq!(shown.needs_inspection, None);
    assert_eq!(
        session
            .blocks
            .iter()
            .find(|block| block.id == "switch")
            .and_then(|block| block.handoff.as_ref())
            .and_then(|handoff| handoff.pending),
        Some(false)
    );
    assert_eq!(session.queue_status, Some(MessageQueueStatus::Paused));
    assert!(context_texts(&session).contains(&"Apply the change once".to_string()));
}

#[test]
fn keeps_an_unaccepted_submitted_request_retryable() {
    let mut session = preparing();
    session
        .blocks
        .push(Block::new("u2", BlockRole::User, "Not sent"));
    mark_provider_request_submitted(&mut session, "switch-1");
    fail_provider_delivery(&mut session, "switch-1", false);
    assert!(!delivery(&session).unwrap().needs_inspection());
    assert_eq!(draft_of(&session, "u2"), Some(true));
    let failed = session.clone();
    confirm_provider_delivery_inspection(&mut session);
    assert_eq!(session, failed);
}

#[test]
fn requires_explicit_pre_dispatch_proof_and_clears_it_on_submission() {
    let mut unknown = preparing();
    fail_provider_delivery(&mut unknown, "switch-1", false);
    assert_eq!(delivery(&unknown).unwrap().failed_before_submission, None);
    let mut known = preparing();
    mark_provider_request_submitted(&mut known, "switch-1");
    fail_provider_delivery(&mut known, "switch-1", true);
    assert_eq!(
        delivery(&known).unwrap().failed_before_submission,
        Some(true)
    );
    assert_eq!(delivery(&known).unwrap().request_submitted, None);
    let mut retry = known.clone();
    retry
        .provider_context
        .as_mut()
        .unwrap()
        .delivery
        .as_mut()
        .unwrap()
        .status = TransferStatus::Preparing;
    mark_provider_request_submitted(&mut retry, "switch-1");
    assert_eq!(delivery(&retry).unwrap().failed_before_submission, None);
    retry
        .provider_context
        .as_mut()
        .unwrap()
        .delivery
        .as_mut()
        .unwrap()
        .failed_before_submission = Some(true);
    recover_submitted_provider_delivery(&mut retry, "switch-1");
    assert_eq!(delivery(&retry).unwrap().failed_before_submission, None);
    assert!(delivery(&retry).unwrap().needs_inspection());
}

#[test]
fn retains_a_newer_provider_choice_when_confirming_inspection() {
    let mut session = preparing();
    mark_provider_request_submitted(&mut session, "switch-1");
    recover_submitted_provider_delivery(&mut session, "switch-1");
    session.harness = HarnessId::Grok;
    session.model = "grok:new".into();
    let selected = session.clone();
    confirm_provider_delivery_inspection(&mut session);
    assert_eq!(session.pending_switch, selected.pending_switch);
    assert!(delivery(&session).is_none());
    assert_eq!(session.harness, HarnessId::Grok);
    assert_eq!(
        provider_binding(&session, HarnessId::Claude, CWD, None)
            .unwrap()
            .provider_session_id,
        "claude-native"
    );
}

#[test]
fn requires_fresh_history_after_inspecting_unknown_execution() {
    for (mode, boundary) in [
        (TransferMode::Pending, Some("a1")),
        (TransferMode::Native, None),
        (TransferMode::Inline, Some("removed")),
        (TransferMode::Native, Some("a1")),
        (TransferMode::Inline, Some("u2")),
    ] {
        let mut session = preparing();
        session
            .blocks
            .push(Block::new("u2", BlockRole::User, "Unacknowledged request"));
        mark_provider_request_submitted(&mut session, "switch-1");
        remember_provider_binding(
            &mut session,
            ProviderBinding {
                harness: HarnessId::Codex,
                cwd: CWD.into(),
                provider_session_id: "target-native".into(),
                provider_account_id: None,
                delivered_through_block_id: boundary.map(str::to_string),
                context_used: None,
                context_window: None,
            },
        );
        session.provider_session_id = Some("target-native".into());
        session
            .provider_context
            .as_mut()
            .unwrap()
            .delivery
            .as_mut()
            .unwrap()
            .mode = mode;
        recover_submitted_provider_delivery(&mut session, "switch-1");
        let pending = session.pending_switch.clone();
        confirm_provider_delivery_inspection(&mut session);
        assert_eq!(session.pending_switch, pending);
        let target = provider_binding(&session, HarnessId::Codex, CWD, None);
        assert_eq!(
            target
                .as_ref()
                .map(|target| target.provider_session_id.as_str()),
            Some("target-native")
        );
        assert!(!can_resume_provider_binding(&session, target.as_ref()));
        assert!(context_texts(&session).contains(&"Unacknowledged request".to_string()));
    }
}

#[test]
fn sanitizes_markers_as_literal_true_values() {
    let mut session = preparing();
    mark_provider_request_submitted(&mut session, "switch-1");
    let mut value = serde_json::to_value(session.provider_context.as_ref().unwrap()).unwrap();
    assert_eq!(
        sanitize_provider_context(&value)
            .and_then(|state| state.delivery)
            .and_then(|delivery| delivery.request_submitted),
        Some(true)
    );
    value["delivery"]["requestSubmitted"] = json!("true");
    value["delivery"]["needsInspection"] = json!(1);
    let saved = sanitize_provider_context(&value)
        .and_then(|state| state.delivery)
        .unwrap();
    assert_eq!(saved.request_submitted, None);
    assert_eq!(saved.needs_inspection, None);
}

#[test]
fn discards_an_uncertain_target_and_keeps_a_retryable_source() {
    let mut session = preparing();
    mark_provider_context_delivered(
        &mut session,
        "switch-1",
        TransferMode::Native,
        None,
        DeliveryCoverage::default(),
    );
    record_provider_bound(
        &mut session,
        HarnessId::Codex,
        CWD,
        "uncertain-target",
        None,
    );
    let pending = session.pending_switch.clone();
    fail_provider_delivery(&mut session, "switch-1", false);
    assert_eq!(
        delivery(&session).unwrap().status,
        TransferStatus::Uncertain
    );
    assert!(session.provider_session_id.is_none());
    assert!(provider_binding(&session, HarnessId::Codex, CWD, None).is_none());
    assert_eq!(
        provider_binding(&session, HarnessId::Claude, CWD, None)
            .unwrap()
            .provider_session_id,
        "claude-native"
    );
    assert_eq!(session.pending_switch, pending);
}

#[test]
fn keeps_an_unaccepted_request_as_one_draft_outside_retry_context() {
    let mut session = preparing();
    session.blocks.push(Block::new(
        "u2",
        BlockRole::User,
        "Retry this exact request once",
    ));
    mark_provider_context_delivered(
        &mut session,
        "switch-1",
        TransferMode::Native,
        Some("target-native"),
        DeliveryCoverage::default(),
    );
    fail_provider_delivery(&mut session, "switch-1", false);
    assert_eq!(
        session
            .blocks
            .iter()
            .filter(|block| block.id == "u2")
            .count(),
        1
    );
    assert_eq!(draft_of(&session, "u2"), Some(true));
    assert!(!context_texts(&session).contains(&"Retry this exact request once".to_string()));
}

#[test]
fn keeps_the_unsent_request_when_snapshot_preparation_stops() {
    let mut session = switched();
    session.blocks.push(Block {
        handoff: Some(HandoffMeta {
            from: HarnessId::Claude,
            to: HarnessId::Codex,
            status: HandoffStatus::Preparing,
            pending: None,
            transfer: None,
            extra: Default::default(),
        }),
        ..Block::new("preflight", BlockRole::Handoff, "")
    });
    session.blocks.push(Block::new(
        "unsent",
        BlockRole::User,
        "Send this request once",
    ));
    let pending = session.pending_switch.clone();
    fail_unstarted_provider_request(&mut session, None);
    assert_eq!(draft_of(&session, "unsent"), Some(true));
    assert!(!context_texts(&session).contains(&"Send this request once".to_string()));
    assert_eq!(session.pending_switch, pending);

    let mut busy = switched();
    busy.busy = Some(true);
    let before = busy.clone();
    fail_unstarted_provider_request(&mut busy, None);
    assert_eq!(busy, before);
}

#[test]
fn records_full_coverage_when_a_stale_resume_fell_back() {
    let mut session = preparing();
    mark_provider_context_delivered(
        &mut session,
        "switch-1",
        TransferMode::Native,
        Some("fresh-native"),
        DeliveryCoverage {
            included_block_ids: Some(vec!["older-user".into(), "u1".into(), "a1".into()]),
            omitted_block_ids: None,
            source_through_block_id: Some("a1".into()),
        },
    );
    let saved = delivery(&session).unwrap();
    assert_eq!(saved.included_block_ids, ["older-user", "u1", "a1"]);
    assert_eq!(
        saved.target_provider_session_id.as_deref(),
        Some("fresh-native")
    );
}

#[test]
fn ignores_late_receipts_for_another_or_finished_switch() {
    let session = preparing();
    let mut stale = session.clone();
    mark_provider_context_delivered(
        &mut stale,
        "stale",
        TransferMode::Native,
        None,
        DeliveryCoverage::default(),
    );
    accept_provider_delivery(&mut stale, "stale");
    fail_provider_delivery(&mut stale, "stale", false);
    assert_eq!(stale, session);

    let mut accepted = preparing();
    accept_provider_delivery(&mut accepted, "switch-1");
    let before = accepted.clone();
    fail_provider_delivery(&mut accepted, "switch-1", false);
    assert_eq!(accepted, before);

    let mut cancelled = preparing();
    fail_provider_delivery(&mut cancelled, "switch-1", false);
    let before = cancelled.clone();
    mark_provider_context_delivered(
        &mut cancelled,
        "switch-1",
        TransferMode::Native,
        Some("late-native"),
        DeliveryCoverage::default(),
    );
    accept_provider_delivery(&mut cancelled, "switch-1");
    assert_eq!(cancelled, before);

    let mut next = preparing();
    accept_provider_delivery(&mut next, "switch-1");
    next.pending_switch = Some(PendingHarnessSwitch {
        from: HarnessId::Codex,
        from_model: "codex:gpt".into(),
        from_settings: ModelSettings::new(),
        from_provider_session_id: Some("codex-native".into()),
        from_provider_account_id: None,
    });
    let before = next.clone();
    accept_provider_delivery(&mut next, "switch-1");
    assert_eq!(next, before);
}

#[test]
fn keeps_source_usage_apart_from_the_selected_meter() {
    let mut source_session = Session::blank("s", HarnessId::Claude, "claude:sonnet", CWD);
    source_session.provider_session_id = Some("native".into());
    source_session.context = Some(ContextUsage {
        used: 18_000,
        window: Some(100_000),
    });
    let binding = provider_binding(&source_session, HarnessId::Claude, CWD, None).unwrap();
    assert_eq!(binding.context_used, Some(18_000));
    assert_eq!(binding.context_window, Some(100_000));
    let saved = sanitize_provider_context(&json!({
        "version": 1,
        "bindings": [binding, { "harness": "claude", "cwd": CWD, "providerSessionId": "claude-native", "contextUsed": null, "contextWindow": -1 }],
    }))
    .unwrap();
    assert_eq!(saved.bindings[0].context_used, None);
    assert_eq!(saved.bindings[0].context_window, None);
    let mut current = switched();
    current.context = Some(ContextUsage {
        used: 3_000,
        window: Some(200_000),
    });
    let context = current.context;
    record_provider_context_usage(
        &mut current,
        HarnessId::Claude,
        CWD,
        Some(20_000),
        Some(100_000),
        None,
    );
    assert_eq!(
        provider_binding(&current, HarnessId::Claude, CWD, None)
            .unwrap()
            .context_used,
        Some(20_000)
    );
    assert_eq!(current.context, context);
}

#[test]
fn resumes_the_source_with_only_the_missing_interval_on_switchback() {
    let mut session = preparing();
    record_provider_bound(&mut session, HarnessId::Codex, CWD, "codex-native", None);
    accept_provider_delivery(&mut session, "switch-1");
    session
        .blocks
        .push(Block::new("u2", BlockRole::User, "B request"));
    session
        .blocks
        .push(Block::new("b1", BlockRole::Assistant, "B response"));
    settle_provider_binding(&mut session, HarnessId::Codex, CWD, None);
    let a = provider_binding(&session, HarnessId::Claude, CWD, None).unwrap();
    let b = provider_binding(&session, HarnessId::Codex, CWD, None).unwrap();
    let context = build_portable_context(
        &session,
        &PortableContextOptions {
            after_block_id: a.delivered_through_block_id.as_deref(),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(a.provider_session_id, "claude-native");
    assert_eq!(b.delivered_through_block_id.as_deref(), Some("b1"));
    assert_eq!(
        context
            .items
            .iter()
            .map(|item| item.text.as_str())
            .collect::<Vec<_>>(),
        ["B request", "B response"]
    );
}

#[test]
fn updates_a_source_boundary_that_settles_after_the_picker_changed() {
    let mut session = switched();
    session.blocks.push(Block::new(
        "a2",
        BlockRole::Assistant,
        "Completed source turn",
    ));
    settle_provider_binding(&mut session, HarnessId::Claude, CWD, None);
    assert_eq!(
        provider_binding(&session, HarnessId::Claude, CWD, None)
            .unwrap()
            .delivered_through_block_id
            .as_deref(),
        Some("a2")
    );
    assert_eq!(session.harness, HarnessId::Codex);
    assert!(session.provider_session_id.is_none());
}

#[test]
fn rejects_malformed_records_and_deduplicates_bindings() {
    assert!(sanitize_provider_context(&json!({ "version": 2, "bindings": [] })).is_none());
    let state = sanitize_provider_context(&json!({
        "version": 1,
        "bindings": [
            null,
            { "harness": "unknown", "cwd": CWD, "providerSessionId": "bad" },
            source(),
            { "harness": "claude", "cwd": CWD, "providerSessionId": "latest", "deliveredThroughBlockId": "a1" },
        ],
        "delivery": { "status": "accepted" },
    }))
    .unwrap();
    assert_eq!(
        state.bindings,
        [ProviderBinding {
            provider_session_id: "latest".into(),
            ..source()
        }]
    );
    assert!(state.delivery.is_none());
}

#[test]
fn the_saved_envelope_round_trips_state_and_pending_switch() {
    let session = preparing();
    let stored = stored_provider_context(&session).unwrap();
    assert_eq!(stored["version"], 1);
    let (state, pending) = restore_provider_context(Some(&stored));
    assert_eq!(state, session.provider_context);
    assert_eq!(pending, session.pending_switch);
    assert_eq!(
        restore_provider_context(Some(&json!({ "version": 2 }))),
        (None, None)
    );
    let blank = Session::blank("s", HarnessId::Claude, "claude:sonnet", CWD);
    assert!(stored_provider_context(&blank).is_none());
}

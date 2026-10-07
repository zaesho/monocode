//! engine.test.ts: "host-owned provider switching". One scripted provider
//! stands in for both Codex and Claude, so each case sets the capabilities
//! of the provider it is about to use.

use std::sync::atomic::AtomicBool;

use monocode_core::provider_context::mark_provider_request_submitted;

use super::*;

/// Codex imports history natively and can append to its own conversation.
const NATIVE: ContextTransferCapabilities = ContextTransferCapabilities {
    native_messages: true,
    resumed_append: true,
    explicit_acceptance: false,
};

/// Claude takes history as text and reports acceptance itself.
const CLAUDE: ContextTransferCapabilities = ContextTransferCapabilities {
    native_messages: false,
    resumed_append: true,
    explicit_acceptance: true,
};

fn switch_command(s: &Setup, harness: HarnessId, command_id: &str) -> Value {
    json!({
        "type": "switchProvider", "commandId": command_id, "sessionId": s.id,
        "expectedRevision": s.session().revision,
        "harness": harness.as_str(), "model": format!("{}:test", harness.as_str()),
        "modelSettings": {}, "runtimeMode": "supervised",
    })
}

fn confirm_command(s: &Setup, command_id: &str, revision: i64) -> Value {
    json!({
        "type": "confirmProviderInspection", "commandId": command_id,
        "sessionId": s.id, "expectedRevision": revision,
    })
}

/// `finishTurn`: the provider binds its conversation, answers, and ends.
fn finish_turn(s: &Setup, index: usize, provider_id: &str, reply: &str) {
    s.provider.emit(
        index,
        json!({ "type": "session.providerBound", "providerSessionId": provider_id }),
    );
    s.provider
        .emit(index, json!({ "type": "message.delta", "text": reply }));
    s.provider.finish(index);
    s.wait_for_status(HostSessionStatus::Idle);
}

/// A first Codex turn bound to `source-native`.
fn with_source_turn(s: &Setup) {
    s.send("source", "Original").unwrap();
    s.wait_for_turns(1);
    finish_turn(s, 0, "source-native", "Source answer");
}

fn texts(context: &PortableContext) -> Vec<String> {
    context.items.iter().map(|item| item.text.clone()).collect()
}

fn delivery(s: &Setup) -> Option<monocode_core::provider_context::ProviderContextDelivery> {
    s.session()
        .session
        .provider_context
        .as_ref()
        .and_then(|context| context.delivery.clone())
}

fn binding_ids(s: &Setup) -> Vec<String> {
    s.session()
        .session
        .provider_context
        .as_ref()
        .map(|context| {
            context
                .bindings
                .iter()
                .map(|binding| binding.provider_session_id.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn binding(s: &Setup, provider_session_id: &str) -> ProviderBinding {
    s.session()
        .session
        .provider_context
        .as_ref()
        .and_then(|context| {
            context
                .bindings
                .iter()
                .find(|binding| binding.provider_session_id == provider_session_id)
                .cloned()
        })
        .unwrap()
}

fn block(s: &Setup, id: &str) -> Option<Block> {
    s.session()
        .session
        .blocks
        .iter()
        .find(|block| block.id == id)
        .cloned()
}

fn bound(s: &Setup, provider_session_id: &str) -> bool {
    let cwd = s.session().session.cwd.clone();
    s.provider
        .binds
        .lock()
        .contains(&(s.id.clone(), provider_session_id.to_string(), cwd))
}

#[test]
fn transfers_exact_visible_history_once_and_restores_the_source_with_only_its_missing_interval() {
    let s = setup();
    let original = format!(
        "Keep this requirement {} end",
        "long visible text ".repeat(180)
    );
    s.send("source-turn", &original).unwrap();
    s.wait_for_turns(1);
    finish_turn(&s, 0, "source-native", "Original answer");
    let selected = switch_command(&s, HarnessId::Claude, "choose-claude");
    let receipt = s.engine.command(&selected).unwrap();
    assert_eq!(s.engine.command(&selected).unwrap(), receipt);
    assert_eq!(
        s.session()
            .session
            .pending_switch
            .as_ref()
            .and_then(|pending| pending.from_provider_session_id.as_deref()),
        Some("source-native")
    );
    assert_eq!(s.provider.turn_count(), 1);
    s.send("target-turn", "Continue here").unwrap();
    s.wait_for_turns(2);
    let text = s.provider.input(1).text;
    assert!(text.contains(&original));
    assert!(text.contains("Original answer"));
    assert_eq!(text.matches("Continue here").count(), 1);
    let users = |s: &Setup| -> Vec<Block> {
        s.session()
            .session
            .blocks
            .iter()
            .filter(|block| block.role == BlockRole::User)
            .cloned()
            .collect()
    };
    assert_eq!(
        users(&s)
            .iter()
            .map(|block| block.text.clone())
            .collect::<Vec<_>>(),
        vec![original.clone(), "Continue here".to_string()]
    );
    finish_turn(&s, 1, "target-native", "Target answer");
    let accepted = delivery(&s).unwrap();
    assert_eq!(accepted.status, TransferStatus::Accepted);
    assert_eq!(accepted.mode, TransferMode::Inline);
    assert!(s.session().session.pending_switch.is_none());
    assert_eq!(
        users(&s)
            .iter()
            .map(|block| block.turn_model.as_ref().map(|model| model.harness))
            .collect::<Vec<_>>(),
        vec![Some(HarnessId::Codex), Some(HarnessId::Claude)]
    );
    assert_eq!(
        block(&s, "target-turn-context")
            .unwrap()
            .handoff
            .unwrap()
            .status,
        HandoffStatus::Ready
    );

    s.provider.set_capabilities(Some(NATIVE));
    s.engine
        .command(&switch_command(&s, HarnessId::Codex, "return-codex"))
        .unwrap();
    s.send("return-turn", "Now compare").unwrap();
    s.wait_for_turns(3);
    assert_eq!(s.provider.input(2).text, "Now compare");
    let returning = s.provider.transfer(2).unwrap();
    assert_eq!(
        texts(&returning.context),
        ["Continue here", "Target answer"]
    );
    assert!(
        texts(returning.fallback_context.as_ref().unwrap()).contains(&original),
        "the fallback carries the whole history"
    );
    assert!(bound(&s, "source-native"));
    s.provider.deliver(2, "source-native").unwrap();
    s.provider.accept(2);
    finish_turn(&s, 2, "source-native", "Comparison");
    let accepted = delivery(&s).unwrap();
    assert_eq!(accepted.status, TransferStatus::Accepted);
    assert_eq!(accepted.mode, TransferMode::Native);
    s.provider.emit(
        0,
        json!({ "type": "session.providerBound", "providerSessionId": "late-source" }),
    );
    assert_eq!(
        s.session().session.provider_session_id.as_deref(),
        Some("source-native")
    );
}

#[test]
fn rejects_stale_selection_and_running_turn_switches_before_changing_durable_state() {
    let s = setup();
    let stale = switch_command(&s, HarnessId::Claude, "stale");
    s.engine
        .command(&json!({
            "type": "configure", "commandId": "config", "sessionId": s.id,
            "model": "codex:other", "modelSettings": {}, "runtimeMode": "supervised",
        }))
        .unwrap();
    assert_eq!(
        s.engine.command(&stale).unwrap_err(),
        "Session changed on the host. Reload it before changing providers"
    );
    assert_eq!(s.session().session.harness, HarnessId::Codex);
    s.send("running", "Work").unwrap();
    assert_eq!(
        s.engine
            .command(&switch_command(&s, HarnessId::Claude, "running-switch"))
            .unwrap_err(),
        "Wait for the current turn before changing providers"
    );
    s.wait_for_turns(1);
    s.provider.finish(0);
}

#[test]
fn starts_fresh_with_surviving_history_when_the_saved_target_boundary_disappears() {
    let s = setup();
    with_source_turn(&s);
    s.provider.set_capabilities(Some(NATIVE));
    s.provider.count_forgets();
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-other"))
        .unwrap();
    s.send("other", "Other provider").unwrap();
    s.wait_for_turns(2);
    s.provider.deliver(1, "target-native").unwrap();
    s.provider.accept(1);
    finish_turn(&s, 1, "target-native", "Target answer");
    let mut before = (*s.session()).clone();
    before.revision += 1;
    for binding in &mut before.session.provider_context.as_mut().unwrap().bindings {
        if binding.harness == HarnessId::Codex {
            binding.delivered_through_block_id = Some("removed-boundary".into());
        }
    }
    s.store
        .transaction(|| s.store.save(before, &json!({ "type": "fixture" })))
        .unwrap();
    s.engine
        .command(&switch_command(&s, HarnessId::Codex, "return-source"))
        .unwrap();
    s.provider.binds.lock().clear();
    s.provider.count_forgets();
    s.send("fresh-return", "Continue safely").unwrap();
    s.wait_for_turns(3);
    assert_eq!(s.provider.forget_count(), 1);
    assert!(s.provider.binds.lock().is_empty());
    assert_eq!(
        texts(&s.provider.transfer(2).unwrap().context),
        [
            "Original",
            "Source answer",
            "Other provider",
            "Target answer"
        ]
    );
    s.provider.deliver(2, "fresh-native").unwrap();
    s.provider.accept(2);
    finish_turn(&s, 2, "fresh-native", "Recovered");
}

#[test]
fn retains_the_source_after_failed_startup_and_requires_inspection_before_returning() {
    let s = setup();
    with_source_turn(&s);
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-failing"))
        .unwrap();
    s.provider.script(|_| "Login required".into());
    s.send("failed", "Next request").unwrap();
    wait_for("the failed turn to settle", || {
        s.session().status == HostSessionStatus::Idle
    });
    assert_eq!(delivery(&s).unwrap().status, TransferStatus::Uncertain);
    assert_eq!(
        s.session()
            .session
            .pending_switch
            .as_ref()
            .and_then(|pending| pending.from_provider_session_id.as_deref()),
        Some("source-native")
    );
    s.engine
        .command(&confirm_command(
            &s,
            "inspect-failure",
            s.session().revision,
        ))
        .unwrap();
    s.provider.set_capabilities(Some(NATIVE));
    s.engine
        .command(&switch_command(
            &s,
            HarnessId::Codex,
            "return-after-failure",
        ))
        .unwrap();
    let session = s.session();
    assert_eq!(
        session.session.provider_session_id.as_deref(),
        Some("source-native")
    );
    assert_eq!(
        session
            .session
            .pending_switch
            .as_ref()
            .map(|pending| pending.from),
        Some(HarnessId::Claude)
    );
    s.send("continue-source", "Continue after inspection")
        .unwrap();
    s.wait_for_turns(2);
    assert_eq!(s.provider.input(1).text, "Continue after inspection");
    let context = s.provider.transfer(1).unwrap().context;
    assert_eq!(
        texts(&context)
            .iter()
            .filter(|text| *text == "Next request")
            .count(),
        1
    );
    s.provider.deliver(1, "source-native").unwrap();
    s.provider.accept(1);
    finish_turn(&s, 1, "source-native", "Continued");
}

#[test]
fn does_not_accept_a_resumed_claude_request_from_a_startup_plan_before_initialization_fails() {
    let s = setup();
    with_source_turn(&s);
    s.provider.set_capabilities(Some(CLAUDE));
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-claude"))
        .unwrap();
    s.send("claude-turn", "First Claude request").unwrap();
    s.wait_for_turns(2);
    s.provider.emit(
        1,
        json!({ "type": "session.providerBound", "providerSessionId": "target-native" }),
    );
    s.provider.accept(1);
    finish_turn(&s, 1, "target-native", "Claude answer");

    s.provider.set_capabilities(Some(NATIVE));
    s.engine
        .command(&switch_command(&s, HarnessId::Codex, "return-source"))
        .unwrap();
    s.send("returned-source", "Back on source").unwrap();
    s.wait_for_turns(3);
    s.provider.deliver(2, "source-native").unwrap();
    s.provider.accept(2);
    finish_turn(&s, 2, "source-native", "Returned source answer");

    s.provider.set_capabilities(Some(CLAUDE));
    s.provider.count_forgets();
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "resume-claude"))
        .unwrap();
    s.provider.binds.lock().clear();
    let before_failure = s.session().revision;
    s.provider.script(|turn| {
        (turn.on_event)(
            serde_json::from_value(json!({ "type": "plan", "text": "# Startup plan" })).unwrap(),
        );
        "Claude resumed conversation differs from the requested session".into()
    });
    s.send("failed-resume", "Submit this exactly once").unwrap();
    wait_for("the failed resume to settle", || {
        s.session().status == HostSessionStatus::Idle
    });
    assert!(bound(&s, "target-native"));
    let failed = delivery(&s).unwrap();
    assert_eq!(failed.status, TransferStatus::Uncertain);
    assert_eq!(failed.needs_inspection, Some(true));
    assert_eq!(binding_ids(&s), ["target-native", "source-native"]);
    let session = s.session();
    assert_eq!(
        session
            .session
            .pending_switch
            .as_ref()
            .and_then(|pending| pending.from_provider_session_id.as_deref()),
        Some("source-native")
    );
    assert_eq!(
        session.session.provider_session_id.as_deref(),
        Some("target-native")
    );
    let requests: Vec<&Block> = session
        .session
        .blocks
        .iter()
        .filter(|block| block.role == BlockRole::User && block.text == "Submit this exactly once")
        .collect();
    assert_eq!(requests.len(), 1);
    assert_ne!(requests[0].draft, Some(true));
    let events = s
        .store
        .events(&s.id, before_failure)
        .unwrap()
        .events
        .unwrap();
    let types: Vec<String> = events
        .iter()
        .filter_map(|entry| entry.event["type"].as_str().map(str::to_string))
        .collect();
    assert!(!types.iter().any(|kind| kind == "providerContext.accepted"));
    assert!(!types.iter().any(|kind| kind == "providerContext.delivered"));
    assert_eq!(s.provider.forget_count(), 0);
}

#[test]
fn preserves_an_imported_target_for_inspection_when_the_current_request_was_not_acknowledged() {
    let s = setup();
    s.provider.set_capabilities(Some(NATIVE));
    s.provider.count_forgets();
    with_source_turn(&s);
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-import"))
        .unwrap();
    s.provider.script(|turn| {
        (turn.on_event)(HarnessEvent::SessionProviderBound {
            provider_session_id: "ambiguous-target".into(),
        });
        let delivered = turn
            .transfer
            .as_ref()
            .and_then(|transfer| transfer.on_delivered.clone())
            .unwrap();
        smol::block_on(delivered(native_receipt("ambiguous-target"))).unwrap();
        "Acknowledgement lost".into()
    });
    s.send("uncertain", "Uncertain request").unwrap();
    wait_for("the unacknowledged turn to settle", || {
        s.session().status == HostSessionStatus::Idle
    });
    assert_eq!(
        s.session().session.provider_session_id.as_deref(),
        Some("ambiguous-target")
    );
    let failed = delivery(&s).unwrap();
    assert_eq!(failed.status, TransferStatus::Uncertain);
    assert_eq!(failed.needs_inspection, Some(true));
    assert_eq!(binding_ids(&s), ["source-native", "ambiguous-target"]);
    s.engine
        .command(&confirm_command(&s, "inspect-import", s.session().revision))
        .unwrap();
    assert_eq!(
        s.session()
            .session
            .pending_switch
            .as_ref()
            .map(|pending| pending.from),
        Some(HarnessId::Codex)
    );
    assert!(
        binding(&s, "ambiguous-target")
            .delivered_through_block_id
            .is_none()
    );
    s.provider.binds.lock().clear();
    s.send("retry", "Inspect and continue").unwrap();
    s.wait_for_turns(2);
    assert_eq!(s.provider.input(1).text, "Inspect and continue");
    let context = texts(&s.provider.transfer(1).unwrap().context);
    for expected in ["Original", "Source answer", "Uncertain request"] {
        assert!(context.iter().any(|text| text == expected), "{expected}");
    }
    assert_eq!(
        context
            .iter()
            .filter(|text| *text == "Uncertain request")
            .count(),
        1
    );
    assert!(s.provider.binds.lock().is_empty());
    assert_eq!(s.provider.forget_count(), 2);
    s.provider.emit(
        1,
        json!({ "type": "session.providerBound", "providerSessionId": "fresh-target" }),
    );
    s.provider.deliver(1, "fresh-target").unwrap();
    s.provider.accept(1);
    finish_turn(&s, 1, "fresh-target", "Recovered");
}

#[test]
fn preserves_an_acknowledged_request_when_saving_its_acceptance_receipt_fails() {
    let s = setup();
    with_source_turn(&s);
    s.provider.set_capabilities(Some(NATIVE));
    s.provider.count_forgets();
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-target"))
        .unwrap();
    s.send("acknowledged-request", "Execute exactly once")
        .unwrap();
    s.wait_for_turns(2);
    assert_eq!(delivery(&s).unwrap().request_submitted, Some(true));
    s.provider.emit(
        1,
        json!({ "type": "session.providerBound", "providerSessionId": "target-native" }),
    );
    s.provider.deliver(1, "target-native").unwrap();
    s.provider.count_forgets();
    let acceptance_failed = Arc::new(AtomicBool::new(false));
    let settlement_failed = Arc::new(AtomicBool::new(false));
    let (acceptance, settlement) = (acceptance_failed.clone(), settlement_failed.clone());
    s.engine.set_save_fault(Some(Box::new(move |event| {
        let kind = event["type"].as_str();
        if kind == Some("providerContext.accepted") && !acceptance.swap(true, Ordering::SeqCst) {
            return true;
        }
        kind == Some("settled") && !settlement.swap(true, Ordering::SeqCst)
    })));
    s.provider.accept(1);
    wait_for("the failed settlement", || {
        settlement_failed.load(Ordering::SeqCst)
    });
    assert!(
        s.send("blocked-during-storage", "Follow up")
            .unwrap_err()
            .contains("already running")
    );
    wait_for_within("the retried settlement", Duration::from_secs(3), || {
        s.session().status == HostSessionStatus::Interrupted
    });
    assert!(acceptance_failed.load(Ordering::SeqCst));
    let recovered = s.session();
    assert_eq!(delivery(&s).unwrap().status, TransferStatus::Accepted);
    assert_eq!(
        recovered.session.provider_session_id.as_deref(),
        Some("target-native")
    );
    assert_eq!(binding_ids(&s), ["source-native", "target-native"]);
    let requests: Vec<&Block> = recovered
        .session
        .blocks
        .iter()
        .filter(|block| block.id == "acknowledged-request")
        .collect();
    assert_eq!(requests.len(), 1);
    assert_ne!(requests[0].draft, Some(true));
    assert_eq!(s.provider.forget_count(), 0);
    assert_eq!(s.provider.turn_count(), 2);
}

#[test]
fn requires_explicit_inspection_after_restarting_a_submitted_target_request() {
    let s = setup();
    with_source_turn(&s);
    s.provider.set_capabilities(Some(NATIVE));
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-target"))
        .unwrap();
    s.send("submitted-request", "Inspect before continuing")
        .unwrap();
    s.wait_for_turns(2);
    s.provider.emit(
        1,
        json!({ "type": "session.providerBound", "providerSessionId": "target-native" }),
    );
    s.provider.deliver(1, "target-native").unwrap();
    let mut before = (*s.session()).clone();
    before.revision += 1;
    mark_provider_request_submitted(&mut before.session, "submitted-request");
    s.store
        .transaction(|| s.store.save(before, &json!({ "type": "fixture" })))
        .unwrap();
    let restarted = engine_over(
        s.store.clone(),
        s.provider.clone(),
        &[HarnessId::Codex, HarnessId::Claude],
        &s.runtime,
    );
    let recovered = s.session();
    assert_eq!(recovered.status, HostSessionStatus::Interrupted);
    let uncertain = delivery(&s).unwrap();
    assert_eq!(uncertain.status, TransferStatus::Uncertain);
    assert_eq!(uncertain.request_submitted, Some(true));
    assert_eq!(uncertain.needs_inspection, Some(true));
    assert_eq!(
        recovered.session.provider_session_id.as_deref(),
        Some("target-native")
    );
    assert_ne!(block(&s, "submitted-request").unwrap().draft, Some(true));
    let blocked = |command: Value| restarted.command(&command).unwrap_err();
    for command in [
        json!({ "type": "send", "commandId": "blocked", "sessionId": s.id, "text": "Continue" }),
        json!({ "type": "compact", "commandId": "blocked-compact", "sessionId": s.id }),
        switch_command(&s, HarnessId::Codex, "blocked-switch"),
    ] {
        assert_eq!(
            blocked(command),
            "Inspect the interrupted provider request and confirm inspection before continuing"
        );
    }
    assert_eq!(
        blocked(confirm_command(
            &s,
            "stale-confirmation",
            recovered.revision - 1
        )),
        "Session changed on the host. Reload it before confirming inspection"
    );
    let confirmation = confirm_command(&s, "confirm-inspection", recovered.revision);
    let receipt = restarted.command(&confirmation).unwrap();
    assert_eq!(restarted.command(&confirmation).unwrap(), receipt);
    let inspected = s.session();
    assert!(delivery(&s).is_none());
    assert_eq!(
        inspected
            .session
            .pending_switch
            .as_ref()
            .map(|pending| pending.from),
        Some(HarnessId::Codex)
    );
    assert_eq!(
        inspected.session.provider_session_id.as_deref(),
        Some("target-native")
    );
    assert!(
        binding(&s, "target-native")
            .delivered_through_block_id
            .is_none()
    );
    assert_ne!(block(&s, "submitted-request").unwrap().draft, Some(true));
    assert_eq!(s.provider.turn_count(), 2);
    assert_eq!(
        blocked(confirm_command(&s, "again", inspected.revision)),
        "This session does not need inspection confirmation"
    );
    restarted
        .command(&json!({
            "type": "send", "commandId": "after-inspection", "sessionId": s.id,
            "text": "A new request",
        }))
        .unwrap();
    s.wait_for_turns(3);
    assert_eq!(s.provider.input(2).text, "A new request");
    let context = texts(&s.provider.transfer(2).unwrap().context);
    for expected in ["Original", "Source answer", "Inspect before continuing"] {
        assert!(context.iter().any(|text| text == expected), "{expected}");
    }
    assert_eq!(
        context
            .iter()
            .filter(|text| *text == "Inspect before continuing")
            .count(),
        1
    );
    s.provider.emit(
        2,
        json!({ "type": "session.providerBound", "providerSessionId": "fresh-target" }),
    );
    s.provider.deliver(2, "fresh-target").unwrap();
    s.provider.accept(2);
    finish_turn(&s, 2, "fresh-target", "Continued");
    restarted.close();
}

#[test]
fn stores_omitted_visible_history_on_the_owning_host_and_excludes_private_reasoning() {
    let s = setup();
    let original = "Oversized complete message ".repeat(2_000);
    s.send("source", &original).unwrap();
    s.wait_for_turns(1);
    s.provider.emit(
        0,
        json!({ "type": "reasoning.delta", "text": "Private reasoning must stay excluded" }),
    );
    finish_turn(&s, 0, "source-native", "Source answer");
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-budget"))
        .unwrap();
    s.send("budget", "Continue").unwrap();
    s.wait_for_turns(2);
    let rendered = s.provider.input(1).text;
    assert!(!rendered.contains(&original));
    let manifest: Value = serde_json::from_str(rendered.split("\n\n").nth(1).unwrap()).unwrap();
    let path = manifest["retrievalPath"].as_str().unwrap().to_string();
    let history = s.directory.path().join("context-history").join(&s.id);
    assert!(path.starts_with(history.to_str().unwrap()), "{path}");
    let snapshot = std::fs::read_to_string(&path).unwrap();
    assert!(snapshot.contains(&original));
    assert!(!snapshot.contains("Private reasoning must stay excluded"));
    assert_eq!(block(&s, "source").unwrap().text, original);
    finish_turn(&s, 1, "target-native", "Done");
}

#[test]
fn keeps_picker_intent_and_command_receipts_after_host_restart_without_resending() {
    let s = setup();
    with_source_turn(&s);
    let selection = switch_command(&s, HarnessId::Claude, "durable-selection");
    let receipt = s.engine.command(&selection).unwrap();
    let restarted = engine_over(
        s.store.clone(),
        s.provider.clone(),
        &[HarnessId::Codex, HarnessId::Claude],
        &s.runtime,
    );
    assert_eq!(restarted.command(&selection).unwrap(), receipt);
    let session = s.session();
    assert_eq!(session.session.harness, HarnessId::Claude);
    let pending = session.session.pending_switch.clone().unwrap();
    assert_eq!(pending.from, HarnessId::Codex);
    assert_eq!(
        pending.from_provider_session_id.as_deref(),
        Some("source-native")
    );
    assert_eq!(s.provider.turn_count(), 1);
    restarted
        .command(&json!({
            "type": "send", "commandId": "after-restart", "sessionId": s.id, "text": "Continue",
        }))
        .unwrap();
    s.wait_for_turns(2);
    assert!(s.provider.input(1).text.contains("Original"));
    finish_turn(&s, 1, "target-native", "Continued");
    restarted.close();
}

#[test]
fn recovers_an_imported_receipt_as_uncertain_and_retains_a_retryable_request() {
    let s = setup();
    let before = s.session();
    let cwd = before.session.cwd.clone();
    let mut fixture = serde_json::to_value(&*before).unwrap();
    fixture["revision"] = json!(before.revision + 1);
    fixture["status"] = json!("running");
    fixture["runId"] = json!("lost-run");
    let session = &mut fixture["session"];
    session["harness"] = json!("claude");
    session["model"] = json!("claude:test");
    session["providerSessionId"] = json!("uncertain-native");
    session["busy"] = json!(true);
    session["pendingSwitch"] = json!({
        "from": "codex", "fromModel": "codex:test", "fromSettings": {},
        "fromProviderSessionId": "retained-source",
    });
    session["blocks"] = json!([
        { "id": "source", "role": "user", "text": "Original" },
        {
            "id": "lost-context", "role": "handoff", "text": "Imported history",
            "handoff": {
                "from": "codex", "to": "claude", "status": "preparing", "pending": true,
                "transfer": {
                    "switchId": "lost", "status": "imported", "mode": "native",
                    "included": 1, "omitted": 0, "historicalAttachments": 0,
                },
            },
        },
        { "id": "lost-user", "role": "user", "text": "Unacknowledged request" },
    ]);
    session["providerContext"] = json!({
        "version": 1,
        "bindings": [
            { "harness": "codex", "cwd": cwd, "providerSessionId": "retained-source", "deliveredThroughBlockId": "source" },
            { "harness": "claude", "cwd": cwd, "providerSessionId": "uncertain-native" },
        ],
        "delivery": {
            "switchId": "lost", "status": "imported", "mode": "native",
            "from": "codex", "to": "claude", "cwd": cwd,
            "currentUserBlockId": "lost-user", "sourceThroughBlockId": "source",
            "includedBlockIds": ["source"], "omittedBlockIds": [],
            "targetProviderSessionId": "uncertain-native",
        },
    });
    let fixture: HostSession = serde_json::from_value(fixture).unwrap();
    s.store
        .transaction(|| s.store.save(fixture, &json!({ "type": "fixture" })))
        .unwrap();
    let restarted = engine_over(
        s.store.clone(),
        s.provider.clone(),
        &[HarnessId::Codex, HarnessId::Claude],
        &s.runtime,
    );
    let recovered = s.session();
    assert_eq!(recovered.status, HostSessionStatus::Interrupted);
    assert!(recovered.session.provider_session_id.is_none());
    assert_eq!(delivery(&s).unwrap().status, TransferStatus::Uncertain);
    let request = block(&s, "lost-user").unwrap();
    assert_eq!(request.draft, Some(true));
    assert_eq!(request.text, "Unacknowledged request");
    assert_eq!(
        block(&s, "lost-context")
            .unwrap()
            .handoff
            .unwrap()
            .transfer
            .unwrap()
            .status,
        TransferStatus::Uncertain
    );
    assert_eq!(s.provider.sends.load(Ordering::SeqCst), 0);
    assert!(!bound(&s, "uncertain-native"));
    restarted
        .command(&switch_command(&s, HarnessId::Codex, "return-source"))
        .unwrap();
    assert_eq!(
        s.session().session.provider_session_id.as_deref(),
        Some("retained-source")
    );
    restarted.close();
}

#[test]
fn delivers_historical_attachments_through_immutable_host_references() {
    let s = setup();
    let file_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    s.upload(file_id, b"notes");
    s.engine
        .command(&json!({
            "type": "send", "commandId": "source-file", "sessionId": s.id,
            "text": "Read the attached notes",
            "attachments": [{ "id": file_id, "name": "notes.txt", "mimeType": "text/plain", "kind": "file", "size": 5 }],
        }))
        .unwrap();
    s.wait_for_turns(1);
    finish_turn(&s, 0, "source-native", "Read notes");
    let original_path = s.session().session.blocks[0].attachments.as_ref().unwrap()[0]
        .path
        .clone();
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-files"))
        .unwrap();
    s.send("target-files", "Continue").unwrap();
    s.wait_for_turns(2);
    let input = s.provider.input(1);
    let history: Value = serde_json::from_str(input.text.split("\n\n").nth(3).unwrap()).unwrap();
    let reference = &history[0]["attachments"][0];
    let assets = s
        .directory
        .path()
        .join("context-history")
        .join(&s.id)
        .join("assets");
    assert!(
        reference["path"]
            .as_str()
            .unwrap()
            .starts_with(assets.to_str().unwrap())
    );
    assert_eq!(reference["sha256"].as_str().unwrap().len(), 64);
    assert_eq!(reference["delivery"], "reference-only");
    assert_eq!(input.attachments, Some(Vec::new()));
    assert_eq!(
        s.session().session.blocks[0].attachments.as_ref().unwrap()[0].path,
        original_path
    );
    assert_eq!(
        block(&s, "target-files-context")
            .unwrap()
            .handoff
            .unwrap()
            .transfer
            .unwrap()
            .historical_attachments,
        1
    );
    finish_turn(&s, 1, "target-native", "Continued");
}

#[test]
fn reserves_the_actual_request_capacity_before_accepting_a_target_turn() {
    for request in ["image", "file references", "edited draft", "approved plan"] {
        let s = setup();
        s.provider.set_capabilities(Some(NATIVE));
        s.send("source", "Original").unwrap();
        s.wait_for_turns(1);
        s.provider.emit(
            0,
            json!({ "type": "session.providerBound", "providerSessionId": "source-native" }),
        );
        s.provider.emit(
            0,
            json!({ "type": "context", "used": 36_000, "window": 40_000 }),
        );
        finish_turn(&s, 0, "source-native", "Source answer");
        s.engine
            .command(&switch_command(&s, HarnessId::Claude, "choose-other"))
            .unwrap();
        s.send("other", "Other provider").unwrap();
        s.wait_for_turns(2);
        s.provider.deliver(1, "target-native").unwrap();
        s.provider.accept(1);
        finish_turn(&s, 1, "target-native", "Target answer");
        s.engine
            .command(&switch_command(&s, HarnessId::Codex, "return-full"))
            .unwrap();
        let mut command = json!({ "type": "send", "commandId": "capacity", "sessionId": s.id, "text": "Continue" });
        match request {
            "image" => {
                let id = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
                s.upload(id, b"image");
                command["attachments"] = json!([{ "id": id, "name": "image.png", "mimeType": "image/png", "kind": "image", "size": 5 }]);
            }
            "file references" => {
                let attachments: Vec<Value> = (0..20)
                    .map(|index| {
                        let id = format!("{index:08x}-dddd-4ddd-8ddd-dddddddddddd");
                        s.upload(&id, b"");
                        json!({
                            "id": id, "name": format!("{}-{index}.txt", "資料".repeat(80)),
                            "mimeType": "text/plain", "kind": "file", "size": 0,
                        })
                    })
                    .collect();
                command["attachments"] = json!(attachments);
            }
            "edited draft" => {
                s.engine
                    .command(&json!({
                        "type": "draft", "commandId": "short-draft", "sessionId": s.id,
                        "text": "Short draft",
                    }))
                    .unwrap();
                command["draftBlockId"] = json!("short-draft");
                command["text"] = json!("Edited request ".repeat(400));
            }
            _ => {
                let plan = "Approved step ".repeat(400);
                let mut before = (*s.session()).clone();
                before.revision += 1;
                let mut block = Block::new("reviewed-plan", BlockRole::Plan, plan.clone());
                block.plan = Some(PlanBlockMeta {
                    status: PlanStatus::Ready,
                    ..Default::default()
                });
                before.session.blocks.push(block);
                s.store
                    .transaction(|| s.store.save(before, &json!({ "type": "fixture" })))
                    .unwrap();
                command["planBlockId"] = json!("reviewed-plan");
                command["intent"] = json!("build");
                command["text"] = json!(format!("Build the approved plan:\n\n{plan}"));
            }
        }
        let stops = s.provider.stops.load(Ordering::SeqCst);
        let error = s.engine.command(&command).unwrap_err();
        assert!(
            error.contains("enough remaining context"),
            "{request}: {error}"
        );
        assert_eq!(s.session().status, HostSessionStatus::Idle);
        assert!(block(&s, "capacity").is_none());
        assert_eq!(s.provider.turn_count(), 2);
        assert_eq!(s.provider.stops.load(Ordering::SeqCst), stops);
    }
}

#[test]
fn refuses_compaction_until_a_turn_delivers_the_shared_history() {
    let s = setup();
    s.provider.compact.store(true, Ordering::SeqCst);
    with_source_turn(&s);
    s.engine
        .command(&switch_command(&s, HarnessId::Claude, "choose-claude"))
        .unwrap();
    assert_eq!(
        s.engine
            .command(&json!({ "type": "compact", "commandId": "compact", "sessionId": s.id }))
            .unwrap_err(),
        "Send a turn with shared history before compacting this provider"
    );
}

#[test]
fn marks_settled_system_rows_as_interruptions_or_errors() {
    let s = setup();
    s.send("stopped", "Work").unwrap();
    s.wait_for_turns(1);
    s.engine
        .command(&json!({
            "type": "cancel", "commandId": "cancel", "sessionId": s.id,
            "runId": s.session().run_id,
        }))
        .unwrap();
    s.wait_for_status(HostSessionStatus::Idle);
    let last = s.session().session.blocks.last().cloned().unwrap();
    assert_eq!(last.text, "Stopped by you.");
    assert_eq!(last.notice, Some(BlockNotice::Interrupt));
    s.provider.script(|_| "Login required".into());
    s.send("failed", "Again").unwrap();
    wait_for("the failed turn", || {
        s.session()
            .session
            .blocks
            .last()
            .is_some_and(|block| block.text == "Login required")
    });
    assert_eq!(
        s.session().session.blocks.last().unwrap().notice,
        Some(BlockNotice::Error)
    );
}

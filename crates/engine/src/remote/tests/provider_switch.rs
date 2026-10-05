//! RemoteSession.test.ts: moving a started host session to another
//! provider, and confirming inspection of a provider request that may
//! already have run.

use super::*;

const SWITCH: &str = "sessionProviderSwitchV1";
const INSPECTION: &str = "sessionProviderInspectionV1";

/// A tab bound to `host-1`, whose host advertises `capabilities`.
fn switch_setup(
    cx: &mut TestAppContext,
    capabilities: &[&'static str],
    value: Value,
) -> (Setup, Arc<Mutex<Host>>, Entity<RemoteSession>) {
    let (s, host) = remote_setup(cx, Some("host-1"));
    {
        let mut host = host.lock();
        host.capabilities = capabilities.to_vec();
        host.sessions.insert("host-1".into(), value);
    }
    let tab = s.open(cx, "tab-1");
    (s, host, tab)
}

fn started(revision: i64, busy: bool) -> Value {
    host_session(
        "host-1",
        revision,
        busy,
        json!([user_block("first", "First")]),
    )
}

/// `inspectionHost`: a submitted target request the host could not
/// confirm.
fn inspection_host() -> Value {
    let mut value = host_session(
        "host-1",
        9,
        false,
        json!([user_block("submitted", "This request may already have run")]),
    );
    value["status"] = json!("interrupted");
    value["session"]["providerSessionId"] = json!("retained-native");
    value["session"]["providerContext"] = json!({
        "version": 1,
        "bindings": [{ "harness": "codex", "cwd": "/home/me/repo", "providerSessionId": "retained-native" }],
        "delivery": {
            "switchId": "switch", "from": "claude", "to": "codex", "cwd": "/home/me/repo",
            "currentUserBlockId": "submitted", "includedBlockIds": [], "omittedBlockIds": [],
            "status": "uncertain", "mode": "native", "requestSubmitted": true, "needsInspection": true,
        },
    });
    value
}

fn submit(tab: &Entity<RemoteSession>, cx: &mut TestAppContext, text: &str) -> bool {
    tab.update(cx, |tab, cx| {
        tab.submit(text, Vec::new(), &RemoteTurnOptions::default(), cx)
    })
}

#[gpui::test]
fn continues_a_started_remote_session_with_another_provider_when_the_host_supports_it(
    cx: &mut TestAppContext,
) {
    let (s, host, tab) = switch_setup(cx, &[SWITCH], started(3, false));
    tab.read_with(cx, |tab, _| {
        assert!(tab.can_switch_provider());
        assert_eq!(
            tab.allowed_model_harnesses(),
            [HarnessId::Codex, HarnessId::Claude]
        );
        assert!(tab.model_available(HarnessId::Claude));
    });
    tab.update(cx, |tab, cx| {
        tab.set_model(HarnessId::Claude, "claude:opus", cx)
    });
    cx.run_until_parked();
    let dispatched = s.dispatched();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0]["type"], "switchProvider");
    assert_eq!(dispatched[0]["sessionId"], "host-1");
    assert_eq!(dispatched[0]["expectedRevision"], 3);
    assert_eq!(dispatched[0]["harness"], "claude");
    assert_eq!(dispatched[0]["model"], "claude:opus");
    // The host's copy now runs Claude, so the change is settled.
    assert_eq!(
        host.lock().sessions["host-1"]["session"]["harness"],
        "claude"
    );
    s.advance(cx, 5_000);
    tab.read_with(cx, |tab, cx| {
        assert_eq!(tab.session(cx).harness, HarnessId::Claude);
        assert_eq!(tab.configuration().harness, HarnessId::Claude);
    });
    assert_eq!(s.dispatched().len(), 1);
    assert!(submit(&tab, cx, "Second"));
    cx.run_until_parked();
    let last = s.dispatched().last().cloned().unwrap();
    assert_eq!(last["type"], "send");
    assert_eq!(last["sessionId"], "host-1");
    assert_eq!(last["text"], "Second");
}

#[gpui::test]
fn keeps_started_sessions_on_their_provider_when_the_host_lacks_switch_support(
    cx: &mut TestAppContext,
) {
    let (s, _host, tab) = switch_setup(cx, &[], started(3, false));
    tab.read_with(cx, |tab, _| {
        assert!(!tab.can_switch_provider());
        assert_eq!(tab.allowed_model_harnesses(), [HarnessId::Codex]);
        assert!(!tab.model_available(HarnessId::Claude));
    });
    tab.update(cx, |tab, cx| {
        tab.set_model(HarnessId::Claude, "claude:opus", cx)
    });
    cx.run_until_parked();
    assert!(s.dispatched().is_empty());
    tab.read_with(cx, |tab, _| {
        assert_eq!(tab.configuration().harness, HarnessId::Codex)
    });
}

#[gpui::test]
fn waits_for_a_running_turn_before_applying_the_remote_provider_selection(cx: &mut TestAppContext) {
    let (s, host, tab) = switch_setup(cx, &[SWITCH], started(4, true));
    tab.update(cx, |tab, cx| {
        tab.set_model(HarnessId::Claude, "claude:opus", cx)
    });
    cx.run_until_parked();
    assert!(s.dispatched().is_empty());
    tab.read_with(cx, |tab, _| {
        assert_eq!(tab.configuration().harness, HarnessId::Claude)
    });
    host.lock()
        .sessions
        .insert("host-1".into(), started(5, false));
    s.advance(cx, 5_000);
    let dispatched = s.dispatched();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0]["type"], "switchProvider");
    assert_eq!(dispatched[0]["harness"], "claude");
    assert_eq!(dispatched[0]["expectedRevision"], 5);
}

#[gpui::test]
fn does_not_resend_a_provider_selection_the_host_refused_until_the_session_changes(
    cx: &mut TestAppContext,
) {
    let (s, host, tab) = switch_setup(cx, &[SWITCH], started(3, false));
    host.lock().dispatch_error = Some(
        "Host rejected request: Session changed on the host. Reload it before changing providers"
            .into(),
    );
    tab.update(cx, |tab, cx| {
        tab.set_model(HarnessId::Claude, "claude:opus", cx)
    });
    cx.run_until_parked();
    assert_eq!(s.dispatched().len(), 1);
    s.advance(cx, 5_000);
    assert_eq!(s.dispatched().len(), 1);
    host.lock()
        .sessions
        .insert("host-1".into(), started(4, false));
    s.advance(cx, 5_000);
    let dispatched = s.dispatched();
    assert_eq!(dispatched.len(), 2);
    assert_eq!(dispatched[1]["expectedRevision"], 4);
}

#[gpui::test]
fn requires_a_revision_checked_remote_inspection_confirmation_without_resubmitting_the_request(
    cx: &mut TestAppContext,
) {
    let (s, host, tab) = switch_setup(cx, &[INSPECTION, SWITCH], inspection_host());
    tab.read_with(cx, |tab, _| {
        assert!(tab.needs_inspection());
        assert!(!tab.can_switch_provider());
        assert_eq!(tab.allowed_model_harnesses(), [HarnessId::Codex]);
        let notice = tab.notice().unwrap();
        assert!(notice.text.contains("Inspect its work before continuing"));
        assert_eq!(notice.action, NoticeAction::ConfirmInspection);
        assert_eq!(notice.action.label(), "Confirm inspection");
        assert_eq!(tab.status().inspection, Some(true));
    });
    assert!(!submit(&tab, cx, "A future follow-up"));
    assert!(!tab.update(cx, |tab, cx| tab.compact(cx)));
    cx.run_until_parked();
    assert!(s.dispatched().is_empty());
    tab.update(cx, |tab, cx| {
        tab.run_notice_action(NoticeAction::ConfirmInspection, cx)
    });
    cx.run_until_parked();
    let dispatched = s.dispatched();
    assert_eq!(dispatched.len(), 1);
    assert_eq!(dispatched[0]["type"], "confirmProviderInspection");
    assert_eq!(dispatched[0]["sessionId"], "host-1");
    assert_eq!(dispatched[0]["expectedRevision"], 9);
    s.advance(cx, 5_000);
    let session = host.lock().sessions["host-1"]["session"].clone();
    assert!(session["providerContext"].get("delivery").is_none());
    assert_eq!(session["providerSessionId"], "retained-native");
    assert_eq!(session["blocks"].as_array().unwrap().len(), 1);
    tab.read_with(cx, |tab, _| {
        assert!(!tab.needs_inspection());
        assert!(tab.notice().is_none());
    });
    assert!(
        !s.dispatched()
            .iter()
            .any(|command| command["type"] == "send")
    );
}

#[gpui::test]
fn keeps_inspection_commands_unavailable_on_older_hosts(cx: &mut TestAppContext) {
    let (s, _host, tab) = switch_setup(cx, &[SWITCH], inspection_host());
    tab.read_with(cx, |tab, _| {
        let notice = tab.notice().unwrap();
        assert_eq!(
            notice.detail.as_deref(),
            Some("Update this host to confirm inspection.")
        );
        assert_ne!(notice.action, NoticeAction::ConfirmInspection);
        assert_eq!(tab.status().inspection, Some(false));
    });
    tab.update(cx, |tab, cx| tab.confirm_inspection(cx));
    assert!(!submit(&tab, cx, "A future follow-up"));
    cx.run_until_parked();
    assert!(s.dispatched().is_empty());
}

#[gpui::test]
fn retries_a_lost_inspection_receipt_with_its_original_command_id(cx: &mut TestAppContext) {
    let (s, host, tab) = switch_setup(cx, &[INSPECTION], inspection_host());
    host.lock().dispatch_error =
        Some("The host request did not complete. Retry to confirm its result.".into());
    tab.update(cx, |tab, cx| tab.confirm_inspection(cx));
    cx.run_until_parked();
    let first = s.dispatched()[0].clone();
    assert_eq!(first["type"], "confirmProviderInspection");
    tab.read_with(cx, |tab, _| {
        let notice = tab.notice().unwrap();
        assert_eq!(notice.text, "Waiting for the host to confirm your request.");
        assert_eq!(notice.action, NoticeAction::RetryPending);
    });
    // The outbox keeps the command for a restarted tab.
    assert!(
        pending_remote_command(
            &s.kv,
            PROJECT,
            "env",
            PendingScope::Session("host-1"),
            Some("tab-1")
        )
        .is_some()
    );
    tab.update(cx, |tab, cx| {
        tab.run_notice_action(NoticeAction::RetryPending, cx)
    });
    cx.run_until_parked();
    let dispatched = s.dispatched();
    assert_eq!(dispatched.len(), 2);
    assert_eq!(dispatched[1], first);
    assert!(!dispatched.iter().any(|command| command["type"] == "send"));
}

#[gpui::test]
fn retries_a_lost_provider_selection_receipt_with_its_original_command_id(cx: &mut TestAppContext) {
    let (s, host, tab) = switch_setup(cx, &[SWITCH], started(3, false));
    host.lock().dispatch_error =
        Some("The host request did not complete. Retry to confirm its result.".into());
    tab.update(cx, |tab, cx| {
        tab.set_model(HarnessId::Claude, "claude:opus", cx)
    });
    cx.run_until_parked();
    let first = s.dispatched()[0].clone();
    assert_eq!(first["type"], "switchProvider");
    tab.read_with(cx, |tab, _| {
        assert_eq!(tab.notice().unwrap().action, NoticeAction::RetryPending)
    });
    tab.update(cx, |tab, cx| {
        tab.run_notice_action(NoticeAction::RetryPending, cx)
    });
    cx.run_until_parked();
    let dispatched = s.dispatched();
    assert_eq!(dispatched.len(), 2);
    assert_eq!(dispatched[1], first);
}

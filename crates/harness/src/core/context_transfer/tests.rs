//! Port of src/features/sessions/model/contextTransfer.test.ts.

use std::sync::atomic::AtomicUsize;

use monocode_core::block::{Block, BlockRole};
use monocode_core::harness::{HarnessId, RuntimeMode};
use monocode_core::harness_event::HarnessSessionInput;
use monocode_core::portable_context::{PortableContextOptions, build_portable_context};
use monocode_core::session::Session;

use super::*;

struct Probe {
    accepted: Arc<AtomicUsize>,
    receipts: Arc<Mutex<Vec<ContextTransferReceipt>>>,
    events: Arc<Mutex<Vec<HarnessEvent>>>,
}

fn input() -> (SendTurnInput, ContextTransferInput, Probe) {
    let mut session = Session::blank("s1", HarnessId::Claude, "claude:opus", "/repo");
    session.blocks = vec![Block::new("u1", BlockRole::User, "Prior instruction")];
    let receipts: Arc<Mutex<Vec<ContextTransferReceipt>>> = Arc::default();
    let on_delivered: DeliveredHook = {
        let receipts = receipts.clone();
        Arc::new(move |receipt| {
            receipts.lock().push(receipt);
            async { Ok(()) }.boxed()
        })
    };
    let transfer = ContextTransferInput {
        context: build_portable_context(&session, &PortableContextOptions::default()).unwrap(),
        fallback_context: None,
        on_delivered: Some(on_delivered),
    };
    let input = SendTurnInput {
        session: HarnessSessionInput {
            session_id: "s1".into(),
            cwd: "/repo".into(),
            model: "claude:opus".into(),
            model_settings: None,
            provider_account_id: None,
            runtime_mode: RuntimeMode::Supervised,
            intent: None,
            controls_agents: None,
            app_access: None,
        },
        text: "Current unique request".into(),
        attachments: None,
    };
    (
        input,
        transfer,
        Probe {
            accepted: Arc::default(),
            receipts,
            events: Arc::default(),
        },
    )
}

fn prepare(capabilities: Option<ContextTransferCapabilities>) -> (PreparedTurn, Probe) {
    let (input, transfer, probe) = input();
    let accepted = probe.accepted.clone();
    let events = probe.events.clone();
    // Run the receipt report inline, so the test sees it at once.
    let spawner: SharedSpawner =
        Arc::new(|future: BoxFuture<'static, ()>| futures::executor::block_on(future));
    let prepared = prepare_context_transfer_input(
        input,
        Some(transfer),
        capabilities,
        Arc::new(move |event| events.lock().push(event)),
        Some(Arc::new(move || {
            accepted.fetch_add(1, Ordering::SeqCst);
        })),
        spawner,
    );
    (prepared, probe)
}

fn delta(text: &str) -> HarnessEvent {
    serde_json::from_value(serde_json::json!({ "type": "message.delta", "text": text })).unwrap()
}

#[test]
fn inlines_context_for_text_adapters_and_accepts_only_on_target_evidence() {
    let (prepared, probe) = prepare(None);
    assert!(prepared.input.text.contains("Prior instruction"));
    assert_eq!(
        prepared
            .input
            .text
            .matches("Current unique request")
            .count(),
        1
    );
    assert!(prepared.transfer.is_none());
    (prepared.on_event)(HarnessEvent::SessionStarted);
    (prepared.on_event)(HarnessEvent::SessionError {
        message: "Auth failed".into(),
    });
    assert_eq!(probe.accepted.load(Ordering::SeqCst), 0);
    assert!(probe.receipts.lock().is_empty());
    (prepared.on_event)(HarnessEvent::SessionProviderBound {
        provider_session_id: "native-1".into(),
    });
    (prepared.on_event)(delta("Response"));
    (prepared.on_accepted.unwrap())();
    assert_eq!(probe.accepted.load(Ordering::SeqCst), 1);
    let receipts = probe.receipts.lock();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].mode, DeliveryMode::Inline);
    assert_eq!(receipts[0].provider_session_id.as_deref(), Some("native-1"));
}

#[test]
fn leaves_native_messages_to_capable_adapters() {
    let (prepared, probe) = prepare(Some(ContextTransferCapabilities {
        native_messages: true,
        resumed_append: true,
        explicit_acceptance: false,
    }));
    assert_eq!(prepared.input.text, "Current unique request");
    assert!(prepared.transfer.is_some());
    (prepared.on_event)(
        serde_json::from_value(serde_json::json!({
            "type": "turn.started", "providerTurnId": "previous-thread-turn"
        }))
        .unwrap(),
    );
    assert_eq!(probe.accepted.load(Ordering::SeqCst), 0);
    let on_accepted = prepared.on_accepted.unwrap();
    on_accepted();
    on_accepted();
    assert_eq!(probe.accepted.load(Ordering::SeqCst), 1);
    assert!(probe.receipts.lock().is_empty());
}

#[test]
fn waits_for_explicit_acceptance_through_startup_activity() {
    let (prepared, probe) = prepare(Some(ContextTransferCapabilities {
        native_messages: false,
        resumed_append: true,
        explicit_acceptance: true,
    }));
    (prepared.on_event)(HarnessEvent::SessionProviderBound {
        provider_session_id: "native-1".into(),
    });
    (prepared.on_event)(
        serde_json::from_value(serde_json::json!({
            "type": "plan", "text": "A plan restored during initialization"
        }))
        .unwrap(),
    );
    (prepared.on_event)(delta("Startup output"));
    assert_eq!(probe.accepted.load(Ordering::SeqCst), 0);
    assert!(probe.receipts.lock().is_empty());
    assert_eq!(probe.events.lock().len(), 3);
    let on_accepted = prepared.on_accepted.unwrap();
    on_accepted();
    on_accepted();
    assert_eq!(probe.accepted.load(Ordering::SeqCst), 1);
    let receipts = probe.receipts.lock();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].mode, DeliveryMode::Inline);
    assert_eq!(receipts[0].provider_session_id.as_deref(), Some("native-1"));
}

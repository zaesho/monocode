//! Port of the shared-history cases in codexLive.test.ts: native history
//! import through `thread/inject_items`, the inline fallback, and the
//! uncertain-delivery error.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::FutureExt;
use parking_lot::Mutex;
use serde_json::{Value, json};

use monocode_core::block::{Block, BlockRole};
use monocode_core::harness::{HarnessId, RuntimeMode};
use monocode_core::harness_event::SendTurnInput;
use monocode_core::portable_context::{
    PortableContext, PortableContextOptions, build_portable_context,
};
use monocode_core::session::Session;

use crate::core::context_transfer::{
    ContextTransferError, ContextTransferInput, ContextTransferReceipt, DeliveredHook, DeliveryMode,
};

use super::fake::*;

const S: &str = "codex-live";

fn run(test: impl std::future::Future<Output = ()>) {
    smol::block_on(test);
}

struct ContextRun {
    turn: smol::Task<anyhow::Result<()>>,
    events: Events,
    accepted: Arc<AtomicUsize>,
    receipts: Arc<Mutex<Vec<ContextTransferReceipt>>>,
    full: PortableContext,
    delta: PortableContext,
}

#[derive(Default)]
struct ContextOptions {
    resume: bool,
    stale_resume: bool,
    /// Hold the receipt save until this channel sends.
    hold_receipt: Option<async_channel::Receiver<()>>,
}

fn source() -> Session {
    let mut session = Session::blank("portable-thread", HarnessId::Claude, "claude:opus", "/repo");
    session.blocks = vec![
        Block::new("u1", BlockRole::User, "Early unique instruction"),
        Block::new("a1", BlockRole::Assistant, "First exact reply"),
        Block::new("u2", BlockRole::User, "The later question"),
        Block::new("a2", BlockRole::Assistant, "The later answer"),
    ];
    session
}

fn input(text: &str) -> SendTurnInput {
    SendTurnInput {
        session: session_input(S, RuntimeMode::Supervised),
        text: text.into(),
        attachments: Some(Vec::new()),
    }
}

fn receipt_hook(
    receipts: &Arc<Mutex<Vec<ContextTransferReceipt>>>,
    hold: Option<async_channel::Receiver<()>>,
) -> DeliveredHook {
    let receipts = receipts.clone();
    Arc::new(move |receipt| {
        receipts.lock().push(receipt);
        let hold = hold.clone();
        async move {
            if let Some(hold) = hold {
                let _ = hold.recv().await;
            }
            Ok(())
        }
        .boxed()
    })
}

async fn context_turn(h: &Harness, options: ContextOptions) -> ContextRun {
    let session = source();
    let full = build_portable_context(&session, &PortableContextOptions::default()).unwrap();
    let delta = build_portable_context(
        &session,
        &PortableContextOptions {
            after_block_id: Some("a1"),
            ..Default::default()
        },
    )
    .unwrap();
    let receipts: Arc<Mutex<Vec<ContextTransferReceipt>>> = Arc::default();
    let accepted = Arc::new(AtomicUsize::new(0));
    let events = Events::default();
    if options.resume {
        h.adapter
            .sessions()
            .bind_session(S, "native-prior", "/repo", None);
    }
    let transfer = ContextTransferInput {
        context: if options.resume {
            delta.clone()
        } else {
            full.clone()
        },
        fallback_context: Some(full.clone()),
        on_delivered: Some(receipt_hook(&receipts, options.hold_receipt)),
    };
    let adapter = h.adapter.clone();
    let sink = events.sink();
    let count = accepted.clone();
    let turn = smol::spawn(async move {
        adapter
            .sessions()
            .send_turn_with_context(
                input("Current unique request"),
                Some(transfer),
                sink,
                Some(Arc::new(move || {
                    count.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .await
    });
    wait_for("initialize", || h.find_method("initialize").is_some()).await;
    h.reply(S, &h.find_method("initialize").unwrap()["id"], json!({}));
    let method = if options.resume {
        "thread/resume"
    } else {
        "thread/start"
    };
    wait_for(method, || h.find_method(method).is_some()).await;
    let opening = h.find_method(method).unwrap();
    if options.stale_resume {
        h.push(
            S,
            json!({ "id": opening["id"], "error": { "code": -32000, "message": "thread not found" } }),
        );
        wait_for("fresh thread/start", || {
            h.find_method("thread/start").is_some()
        })
        .await;
        let start = h.find_method("thread/start").unwrap();
        h.reply(
            S,
            &start["id"],
            json!({ "thread": { "id": "native-fresh" } }),
        );
    } else {
        let id = if options.resume {
            "native-prior"
        } else {
            "native-new"
        };
        h.reply(S, &opening["id"], json!({ "thread": { "id": id } }));
    }
    wait_for("history import", || {
        h.find_method("thread/inject_items").is_some()
    })
    .await;
    ContextRun {
        turn,
        events,
        accepted,
        receipts,
        full,
        delta,
    }
}

async fn accept_context_turn(h: &Harness) -> Value {
    wait_for("turn/start", || h.find_method("turn/start").is_some()).await;
    let starting = h.find_method("turn/start").unwrap();
    h.reply(
        S,
        &starting["id"],
        json!({ "turn": { "id": "context-turn", "status": "inProgress" } }),
    );
    h.notify(
        S,
        "turn/completed",
        json!({ "turn": { "id": "context-turn", "status": "completed" } }),
    );
    starting
}

fn ids(context: &PortableContext) -> Vec<String> {
    context.items.iter().map(|item| item.id.clone()).collect()
}

#[test]
fn imports_native_history_before_the_untouched_request() {
    run(async {
        let h = Harness::new();
        let run = context_turn(&h, ContextOptions::default()).await;
        let importing = h.find_method("thread/inject_items").unwrap();
        let items = importing["params"]["items"].as_array().unwrap();
        let roles: Vec<&str> = items
            .iter()
            .map(|item| item["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "user", "assistant", "user", "assistant"]);
        let first: Value =
            serde_json::from_str(items[1]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(first["text"], "Early unique instruction");
        assert!(
            !importing["params"]
                .to_string()
                .contains("Current unique request")
        );
        h.notify(
            S,
            "item/agentMessage/delta",
            json!({ "itemId": "historical-event", "delta": "Old imported assistant text" }),
        );
        settle().await;
        assert!(!run.events.has("message.delta"));
        assert!(h.find_method("turn/start").is_none());
        h.reply(S, &importing["id"], json!({}));
        let starting = accept_context_turn(&h).await;
        run.turn.await.unwrap();
        assert_eq!(
            starting["params"]["input"][0]["text"],
            "Current unique request"
        );
        let receipts = run.receipts.lock();
        assert_eq!(
            receipts.as_slice(),
            [ContextTransferReceipt {
                mode: DeliveryMode::Native,
                provider_session_id: Some("native-new".into()),
                included_ids: Some(ids(&run.full)),
                omitted_ids: Some(Vec::new()),
                through_block_id: Some("a2".into()),
            }]
        );
        assert_eq!(run.accepted.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn waits_for_the_saved_receipt_before_starting_the_turn() {
    run(async {
        let h = Harness::new();
        let (release, hold) = async_channel::bounded(1);
        let run = context_turn(
            &h,
            ContextOptions {
                hold_receipt: Some(hold),
                ..Default::default()
            },
        )
        .await;
        let importing = h.find_method("thread/inject_items").unwrap();
        h.reply(S, &importing["id"], json!({}));
        wait_for("receipt", || run.receipts.lock().len() == 1).await;
        settle().await;
        assert!(h.find_method("turn/start").is_none());
        release.send(()).await.unwrap();
        accept_context_turn(&h).await;
        run.turn.await.unwrap();
    });
}

#[test]
fn falls_back_inline_only_for_method_not_found_and_remembers_it() {
    run(async {
        let h = Harness::new();
        let run = context_turn(&h, ContextOptions::default()).await;
        let importing = h.find_method("thread/inject_items").unwrap();
        h.push(
            S,
            json!({ "id": importing["id"], "error": { "code": -32601, "message": "Unknown method" } }),
        );
        wait_for("inline turn", || h.find_method("turn/start").is_some()).await;
        assert!(run.receipts.lock().is_empty());
        let starting = accept_context_turn(&h).await;
        run.turn.await.unwrap();
        let text = starting["params"]["input"][0]["text"].as_str().unwrap();
        assert!(text.contains("Early unique instruction"));
        assert_eq!(text.matches("Current unique request").count(), 1);
        wait_for("inline receipt", || run.receipts.lock().len() == 1).await;
        assert_eq!(run.receipts.lock()[0].mode, DeliveryMode::Inline);

        h.clear_sent();
        let adapter = h.adapter.clone();
        let delta = run.delta.clone();
        let next = smol::spawn(async move {
            adapter
                .sessions()
                .send_turn_with_context(
                    input("Second request"),
                    Some(ContextTransferInput {
                        context: delta,
                        fallback_context: None,
                        on_delivered: None,
                    }),
                    Arc::new(|_| {}),
                    None,
                )
                .await
        });
        accept_context_turn(&h).await;
        next.await.unwrap();
        assert!(h.find_method("thread/inject_items").is_none());
    });
}

#[test]
fn abandons_an_uncertain_native_thread_without_a_current_turn() {
    run(async {
        let h = Harness::new();
        let run = context_turn(&h, ContextOptions::default()).await;
        let importing = h.find_method("thread/inject_items").unwrap();
        h.push(
            S,
            json!({ "id": importing["id"], "error": { "code": -32000, "message": "History mutation may have failed" } }),
        );
        let error = run.turn.await.unwrap_err();
        assert!(error.downcast_ref::<ContextTransferError>().is_some());
        assert!(h.find_method("turn/start").is_none());
        assert!(run.receipts.lock().is_empty());
        assert_eq!(run.accepted.load(Ordering::SeqCst), 0);
        // The thread was forgotten, so the next turn starts a fresh one.
        h.clear_sent();
        let adapter = h.adapter.clone();
        let next = smol::spawn(async move {
            adapter
                .sessions()
                .send_turn(input("Again"), Arc::new(|_| {}), None)
                .await
        });
        wait_for("initialize", || h.find_method("initialize").is_some()).await;
        h.reply(S, &h.find_method("initialize").unwrap()["id"], json!({}));
        wait_for("thread/start", || h.find_method("thread/start").is_some()).await;
        assert!(h.find_method("thread/resume").is_none());
        let start = h.find_method("thread/start").unwrap();
        h.reply(
            S,
            &start["id"],
            json!({ "thread": { "id": "native-fresh" } }),
        );
        accept_context_turn(&h).await;
        next.await.unwrap();
    });
}

#[test]
fn rebuilds_full_history_after_a_stale_resume() {
    run(async {
        let h = Harness::new();
        let run = context_turn(
            &h,
            ContextOptions {
                resume: true,
                stale_resume: true,
                ..Default::default()
            },
        )
        .await;
        let importing = h.find_method("thread/inject_items").unwrap();
        assert!(
            importing["params"]
                .to_string()
                .contains("Early unique instruction")
        );
        h.reply(S, &importing["id"], json!({}));
        accept_context_turn(&h).await;
        run.turn.await.unwrap();
        let receipts = run.receipts.lock();
        assert_eq!(
            receipts[0].provider_session_id.as_deref(),
            Some("native-fresh")
        );
        assert_eq!(receipts[0].included_ids, Some(ids(&run.full)));
    });
}

#[test]
fn imports_only_the_missing_interval_on_a_resumed_thread() {
    run(async {
        let h = Harness::new();
        let run = context_turn(
            &h,
            ContextOptions {
                resume: true,
                ..Default::default()
            },
        )
        .await;
        let importing = h.find_method("thread/inject_items").unwrap();
        let params = importing["params"].to_string();
        assert!(!params.contains("Early unique instruction"));
        assert!(params.contains("The later question"));
        h.reply(S, &importing["id"], json!({}));
        accept_context_turn(&h).await;
        run.turn.await.unwrap();
        let receipts = run.receipts.lock();
        assert_eq!(
            receipts[0].provider_session_id.as_deref(),
            Some("native-prior")
        );
        assert_eq!(receipts[0].included_ids, Some(ids(&run.delta)));
    });
}

#[test]
fn keeps_a_successful_import_apart_from_a_refused_turn() {
    run(async {
        let h = Harness::new();
        let run = context_turn(&h, ContextOptions::default()).await;
        let importing = h.find_method("thread/inject_items").unwrap();
        h.reply(S, &importing["id"], json!({}));
        wait_for("turn/start", || h.find_method("turn/start").is_some()).await;
        let starting = h.find_method("turn/start").unwrap();
        h.push(
            S,
            json!({ "id": starting["id"], "error": { "code": -32000, "message": "Current turn refused" } }),
        );
        let error = run.turn.await.unwrap_err();
        assert!(error.to_string().contains("Current turn refused"));
        assert_eq!(run.receipts.lock()[0].mode, DeliveryMode::Native);
        assert_eq!(run.accepted.load(Ordering::SeqCst), 0);
    });
}

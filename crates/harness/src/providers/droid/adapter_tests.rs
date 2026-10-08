//! Port of droidLive.test.ts. The TypeScript mocked `core/child` and the
//! catalog module; these run the real adapter over `core::testing::Fake`
//! with a scripted ACP peer, and see the catalog probe as its spawn.

use super::*;
use crate::core::testing::{Call, Fake};
use crate::providers::grok::test_support::{Events, Peer, fake_resolving, settings, turn};

fn config(model: &str, efforts: &[&str], effort: &str) -> Value {
    json!([
        {
            "id": "autonomy_level",
            "category": "mode",
            "currentValue": "normal",
            "options": [
                { "value": "normal", "name": "Auto (Off)" },
                { "value": "spec", "name": "Spec" },
                { "value": "auto-low", "name": "Auto (Low)" },
            ],
        },
        {
            "id": "model",
            "category": "model",
            "currentValue": model,
            "options": [
                { "value": "gpt-6-luna", "name": "GPT-6 Luna" },
                { "value": "claude-opus-5-5", "name": "Opus 5.5" },
            ],
        },
        {
            "id": "reasoning_effort",
            "category": "thought_level",
            "currentValue": effort,
            "options": efforts.iter().map(|value| json!({ "value": value, "name": value })).collect::<Vec<_>>(),
        },
    ])
}

fn update(peer: &Peer, session_id: &str, update: Value) {
    peer.notify(
        session_id,
        "session/update",
        json!({ "sessionId": "droid-session-1", "update": update }),
    );
}

async fn start_session(peer: &Peer, session_id: &str) {
    peer.answer(session_id, "initialize", json!({ "protocolVersion": 1 }))
        .await;
    peer.answer(
        session_id,
        "session/new",
        json!({
            "sessionId": "droid-session-1",
            "models": { "currentModelId": "gpt-6-luna", "availableModels": [] },
            "configOptions": config("gpt-6-luna", &["none", "low", "medium"], "medium"),
        }),
    )
    .await;
}

fn start(adapter: &DroidAdapter, input: SendTurnInput, events: &Events) -> smol::Task<Result<()>> {
    let adapter = adapter.clone();
    let sink = events.sink();
    smol::spawn(async move { adapter.send_turn(input, sink, None).await })
}

fn probe_spawns(peer: &Peer) -> usize {
    peer.calls()
        .iter()
        .filter(|call| matches!(call, Call::Spawn(request) if request.session_id.starts_with("monocode-droid-probe-")))
        .count()
}

#[test]
fn spawns_droid_acp_switches_model_then_effort_sets_autonomy_and_prompts() {
    smol::block_on(async {
        let peer = Peer::with_fake(fake_resolving(HarnessId::Droid, "/fake/droid"));
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-live-new",
                "droid:claude-opus-5-5",
                RuntimeMode::AutoAcceptEdits,
                "inspect this",
                Some(settings(&[("effort", "xhigh")])),
            ),
            &events,
        );
        start_session(&peer, "droid-live-new").await;
        assert_eq!(
            peer.spawned()[0],
            (
                "/fake/droid".to_string(),
                vec!["exec".to_string(), "--output-format".into(), "acp".into()]
            )
        );

        let set_model = peer
            .next("session/set_config_option", |message| {
                message["params"]["configId"] == "model"
            })
            .await;
        assert_eq!(set_model["params"]["value"], "claude-opus-5-5");
        // Droid answers with `{}` and announces the new per-model levels.
        update(
            &peer,
            "droid-live-new",
            json!({
                "sessionUpdate": "config_option_update",
                "configOptions": config("claude-opus-5-5", &["low", "high", "xhigh", "max"], "high"),
            }),
        );
        peer.reply("droid-live-new", &set_model["id"], json!({}));

        let set_effort = peer
            .next("session/set_config_option", |message| {
                message["params"]["configId"] == "reasoning_effort"
            })
            .await;
        assert_eq!(set_effort["params"]["value"], "xhigh");
        peer.reply("droid-live-new", &set_effort["id"], json!({}));

        let set_mode = peer.next("session/set_mode", |_| true).await;
        assert_eq!(set_mode["params"]["modeId"], "auto-low");
        peer.reply("droid-live-new", &set_mode["id"], json!({}));

        let prompt = peer.next("session/prompt", |_| true).await;
        assert_eq!(
            prompt["params"],
            json!({
                "sessionId": "droid-session-1",
                "prompt": [{ "type": "text", "text": "inspect this" }],
            })
        );
        update(
            &peer,
            "droid-live-new",
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "done" } }),
        );
        peer.reply(
            "droid-live-new",
            &prompt["id"],
            json!({ "stopReason": "end_turn" }),
        );

        running.await.unwrap();
        let all = events.all();
        assert!(all.contains(&HarnessEvent::SessionProviderBound {
            provider_session_id: "droid-session-1".into()
        }));
        assert!(all.contains(&HarnessEvent::MessageDelta {
            text: "done".into(),
            append: None,
        }));
        // The first live session kicks off the effort probe.
        peer.wait_for("catalog probe", |peer| probe_spawns(peer) == 1)
            .await;
        adapter.stop_session("droid-live-new".into()).await.unwrap();
    });
}

#[test]
fn loads_a_bound_droid_session_instead_of_creating_a_new_one() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        adapter.bind_session("droid-live-load", "persisted-session", "/repo", None);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-live-load",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "continue",
                None,
            ),
            &events,
        );
        peer.answer(
            "droid-live-load",
            "initialize",
            json!({ "protocolVersion": 1 }),
        )
        .await;
        let load = peer.next("session/load", |_| true).await;
        assert_eq!(load["params"]["sessionId"], "persisted-session");
        peer.reply(
            "droid-live-load",
            &load["id"],
            json!({
                "models": { "currentModelId": "gpt-6-luna" },
                "configOptions": config("gpt-6-luna", &["low"], "low"),
            }),
        );
        let set_mode = peer.next("session/set_mode", |_| true).await;
        assert_eq!(set_mode["params"]["modeId"], "normal");
        peer.reply("droid-live-load", &set_mode["id"], json!({}));
        peer.answer(
            "droid-live-load",
            "session/prompt",
            json!({ "stopReason": "end_turn" }),
        )
        .await;

        running.await.unwrap();
        assert!(peer.find("session/new").is_none());
        assert!(peer.find("session/set_config_option").is_none());
        adapter
            .stop_session("droid-live-load".into())
            .await
            .unwrap();
    });
}

#[test]
fn asks_monocode_before_running_a_command_in_supervised_mode() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-live-approval",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "run tests",
                None,
            ),
            &events,
        );
        start_session(&peer, "droid-live-approval").await;
        peer.answer("droid-live-approval", "session/set_mode", json!({}))
            .await;
        let prompt = peer.next("session/prompt", |_| true).await;

        peer.line(
            "droid-live-approval",
            json!({
                "jsonrpc": "2.0",
                "id": 900,
                "method": "session/request_permission",
                "params": {
                    "sessionId": "droid-session-1",
                    "toolCall": {
                        "toolCallId": "call-1",
                        "title": "npm test",
                        "kind": "execute",
                        "rawInput": { "command": "npm test" },
                    },
                    "options": [
                        { "optionId": "proceed_once", "name": "Allow", "kind": "allow_once" },
                        { "optionId": "cancel", "name": "Deny", "kind": "reject_once" },
                    ],
                },
            }),
        );
        let seen = events.clone();
        peer.wait_for("approval.requested", move |_| {
            seen.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
        })
        .await;
        adapter.respond_approval("droid-live-approval", 900, ApprovalDecision::Allow);
        let answer = |peer: &Peer| {
            peer.sent()
                .into_iter()
                .find(|message| message["id"] == 900 && message.get("result").is_some())
        };
        peer.wait_for("permission reply", |peer| answer(peer).is_some())
            .await;
        assert_eq!(
            answer(&peer).unwrap()["result"]["outcome"]["outcome"],
            "selected"
        );

        peer.reply(
            "droid-live-approval",
            &prompt["id"],
            json!({ "stopReason": "end_turn" }),
        );
        running.await.unwrap();
        adapter
            .stop_session("droid-live-approval".into())
            .await
            .unwrap();
    });
}

#[test]
fn reports_droids_hidden_error_detail_once_without_the_streamed_echo() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-live-limit",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "hi",
                None,
            ),
            &events,
        );
        start_session(&peer, "droid-live-limit").await;
        peer.answer("droid-live-limit", "session/set_mode", json!({}))
            .await;
        let prompt = peer.next("session/prompt", |_| true).await;
        let data = "402 {\"detail\":\"You've reached your 5-hour Droid Core usage limit.\",\"status\":402}";
        update(
            &peer,
            "droid-live-limit",
            json!({ "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": format!("Error: {data}") } }),
        );
        peer.fail(
            "droid-live-limit",
            &prompt["id"],
            json!({ "code": -32603, "message": "Internal error: Agent error", "data": data }),
        );

        assert!(running.await.is_err());
        assert!(!events.any(|event| matches!(event, HarnessEvent::MessageDelta { .. })));
        assert!(events.all().contains(&HarnessEvent::SessionError {
            message: "You've reached your 5-hour Droid Core usage limit.".into()
        }));
    });
}

#[test]
fn registers_once_and_refuses_to_steer() {
    let peer = Peer::new();
    register(&peer.ctx);
    let first = peer.ctx.registry.get_harness(HarnessId::Droid).unwrap();
    register(&peer.ctx);
    let second = peer.ctx.registry.get_harness(HarnessId::Droid).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(!second.can_steer());
}

// The regressions from the Droid review (zaesho/monocode#1).

const OPTIONS: [(&str, &str); 2] = [("proceed_once", "allow_once"), ("cancel", "reject_once")];

fn permission(peer: &Peer, session_id: &str, id: Value, kind: Option<&str>) {
    let mut tool = json!({ "toolCallId": "call-1", "title": "test" });
    if let Some(kind) = kind {
        tool["kind"] = json!(kind);
    }
    peer.line(
        session_id,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/request_permission",
            "params": {
                "toolCall": tool,
                "options": OPTIONS
                    .iter()
                    .map(|(id, kind)| json!({ "optionId": id, "kind": kind, "name": id }))
                    .collect::<Vec<_>>(),
            },
        }),
    );
}

/// The reply sent for request `id`.
fn answer(peer: &Peer, id: &Value) -> Option<Value> {
    peer.sent()
        .into_iter()
        .find(|message| &message["id"] == id && message.get("result").is_some())
}

async fn answered(peer: &Peer, id: Value) -> Value {
    peer.wait_for("permission reply", |peer| answer(peer, &id).is_some())
        .await;
    answer(peer, &id).unwrap()["result"]["outcome"].clone()
}

/// Start a supervised turn and wait for its prompt.
async fn ready(
    peer: &Peer,
    adapter: &DroidAdapter,
    session_id: &str,
    sink: EventSink,
) -> (smol::Task<Result<()>>, Value) {
    let adapter = adapter.clone();
    let input = turn(
        session_id,
        "droid:gpt-6-luna",
        RuntimeMode::Supervised,
        "test",
        None,
    );
    let running = smol::spawn(async move { adapter.send_turn(input, sink, None).await });
    start_session(peer, session_id).await;
    peer.answer(session_id, "session/set_mode", json!({})).await;
    let prompt = peer.next("session/prompt", |_| true).await;
    (running, prompt)
}

/// Poll `future` once right here, then finish it on the executor. A stop
/// started from an event sink sets its flags before the sink returns, as
/// the synchronous start of the TypeScript `stopDroidSession` did.
fn start_now(mut future: BoxFuture<'static, Result<()>>) -> smol::Task<Result<()>> {
    let waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&waker);
    match future.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(result) => smol::spawn(async move { result }),
        std::task::Poll::Pending => smol::spawn(future),
    }
}

fn killed(peer: &Peer, session_id: &str) -> bool {
    peer.calls()
        .iter()
        .any(|call| matches!(call, Call::Kill(id) if id == session_id))
}

#[test]
fn a_tool_update_that_stops_the_session_still_drains_the_permission_reply() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let stopping: Arc<Mutex<Option<smol::Task<Result<()>>>>> = Arc::default();
        let sink: EventSink = {
            let adapter = adapter.clone();
            let stopping = stopping.clone();
            let record = events.sink();
            Arc::new(move |event: HarnessEvent| {
                if matches!(event, HarnessEvent::ToolUpdated { .. }) {
                    let adapter = adapter.clone();
                    *stopping.lock() = Some(start_now(Box::pin(async move {
                        adapter.stop_session("droid-stop-drain".into()).await
                    })));
                }
                record(event);
            })
        };
        let (running, _) = ready(&peer, &adapter, "droid-stop-drain", sink).await;
        permission(&peer, "droid-stop-drain", json!(907), Some("execute"));
        assert_eq!(
            answered(&peer, json!(907)).await,
            json!({ "outcome": "cancelled" })
        );
        let stop = stopping.lock().take().expect("stop started");
        stop.await.unwrap();
        running.await.unwrap();
        // The reply went out before the child was killed.
        let calls = peer.calls();
        let reply = calls
            .iter()
            .position(|call| matches!(call, Call::Write(_, line) if line.contains("\"id\":907")))
            .unwrap();
        let kill = calls
            .iter()
            .position(|call| matches!(call, Call::Kill(id) if id == "droid-stop-drain"))
            .unwrap();
        assert!(reply < kill);
        assert!(!events.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. })));
    });
}

#[test]
fn a_tool_update_that_cancels_the_turn_cancels_the_permission_in_every_mode() {
    for runtime_mode in [RuntimeMode::Supervised, RuntimeMode::FullAccess] {
        smol::block_on(async {
            let session_id = format!("droid-cancel-tool-{runtime_mode:?}");
            let peer = Peer::new();
            let adapter = DroidAdapter::new(&peer.ctx);
            let events = Events::default();
            let cancelling: Arc<Mutex<Option<smol::Task<Result<()>>>>> = Arc::default();
            let sink: EventSink = {
                let adapter = adapter.clone();
                let cancelling = cancelling.clone();
                let record = events.sink();
                let session_id = session_id.clone();
                Arc::new(move |event: HarnessEvent| {
                    if matches!(event, HarnessEvent::ToolUpdated { .. }) {
                        let adapter = adapter.clone();
                        let session_id = session_id.clone();
                        *cancelling.lock() = Some(start_now(Box::pin(async move {
                            adapter.cancel_turn(session_id).await
                        })));
                    }
                    record(event);
                })
            };
            let running = {
                let adapter = adapter.clone();
                let input = turn(&session_id, "droid:gpt-6-luna", runtime_mode, "test", None);
                smol::spawn(async move { adapter.send_turn(input, sink, None).await })
            };
            start_session(&peer, &session_id).await;
            peer.answer(&session_id, "session/set_mode", json!({}))
                .await;
            peer.next("session/prompt", |_| true).await;
            permission(&peer, &session_id, json!(906), Some("execute"));
            assert_eq!(
                answered(&peer, json!(906)).await,
                json!({ "outcome": "cancelled" })
            );
            let cancel = cancelling.lock().take().expect("cancel started");
            cancel.await.unwrap();
            running.await.unwrap();
            assert!(!events.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. })));
            assert!(peer.find("session/cancel").is_some());
        });
    }
}

#[test]
fn a_cancel_during_spawn_abandons_the_startup() {
    smol::block_on(async {
        let (pids, pid_rx) = async_channel::unbounded();
        let fake = Fake {
            pids: parking_lot::Mutex::new(Some(pid_rx)),
            ..Fake::default()
        };
        let peer = Peer::with_fake(fake);
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-deleted",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                None,
            ),
            &events,
        );
        peer.wait_for("spawn", |peer| !peer.spawned().is_empty())
            .await;
        adapter.cancel_turn("droid-deleted".into()).await.unwrap();
        adapter
            .forget_session("droid-deleted".into())
            .await
            .unwrap();
        pids.send(7).await.unwrap();
        running.await.unwrap();
        assert!(peer.find("initialize").is_none());
        assert!(events.all().is_empty());
    });
}

#[test]
fn an_unexpected_exit_fails_the_turn() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let (running, _) = ready(&peer, &adapter, "droid-crashed", events.sink()).await;
        peer.exit("droid-crashed", 1);
        assert!(running.await.is_err());
        assert!(events.any(|event| matches!(event, HarnessEvent::SessionError { .. })));
        assert!(events.any(|event| matches!(event, HarnessEvent::SessionEnded { code: Some(1) })));
    });
}

#[test]
fn registers_an_approval_before_asking_and_replies_with_the_string_id() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let sink: EventSink = {
            let adapter = adapter.clone();
            Arc::new(move |event: HarnessEvent| {
                if let HarnessEvent::ApprovalRequested { request_id, .. } = event {
                    adapter.respond_approval("droid-sync", request_id, ApprovalDecision::Deny);
                }
            })
        };
        let (running, prompt) = ready(&peer, &adapter, "droid-sync", sink).await;
        permission(
            &peer,
            "droid-sync",
            json!("permission-abc"),
            Some("execute"),
        );
        assert_eq!(
            answered(&peer, json!("permission-abc")).await,
            json!({ "outcome": "selected", "optionId": "cancel" })
        );
        peer.reply("droid-sync", &prompt["id"], json!({}));
        running.await.unwrap();
        adapter.stop_session("droid-sync".into()).await.unwrap();
    });
}

#[test]
fn edit_mode_asks_about_a_permission_whose_kind_came_from_the_tool_call() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-sparse",
                "droid:gpt-6-luna",
                RuntimeMode::AutoAcceptEdits,
                "test",
                None,
            ),
            &events,
        );
        start_session(&peer, "droid-sparse").await;
        peer.answer("droid-sparse", "session/set_mode", json!({}))
            .await;
        let prompt = peer.next("session/prompt", |_| true).await;
        update(
            &peer,
            "droid-sparse",
            json!({ "sessionUpdate": "tool_call", "toolCallId": "call-1", "kind": "execute", "title": "test" }),
        );
        permission(&peer, "droid-sparse", json!(901), None);
        let seen = events.clone();
        peer.wait_for("approval.requested", move |_| {
            seen.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
        })
        .await;
        assert!(events.any(|event| matches!(
            event,
            HarnessEvent::ApprovalRequested { kind: Some(kind), .. } if kind == "execute"
        )));
        adapter.respond_approval("droid-sparse", 901, ApprovalDecision::Deny);
        peer.reply("droid-sparse", &prompt["id"], json!({}));
        running.await.unwrap();
        adapter.stop_session("droid-sparse".into()).await.unwrap();
    });
}

#[test]
fn cancel_answers_pending_permissions_cancelled_and_retires_the_child() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let (running, _) = ready(&peer, &adapter, "droid-cancel-approval", events.sink()).await;
        permission(&peer, "droid-cancel-approval", json!(902), Some("execute"));
        let seen = events.clone();
        peer.wait_for("approval.requested", move |_| {
            seen.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
        })
        .await;
        adapter
            .cancel_turn("droid-cancel-approval".into())
            .await
            .unwrap();
        running.await.unwrap();
        assert_eq!(
            answered(&peer, json!(902)).await,
            json!({ "outcome": "cancelled" })
        );
        let asked = |events: &Events| {
            events
                .all()
                .iter()
                .filter(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
                .count()
        };
        let before = asked(&events);
        permission(&peer, "droid-cancel-approval", json!(903), Some("edit"));
        smol::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(asked(&events), before);
        assert!(killed(&peer, "droid-cancel-approval"));
    });
}

#[test]
fn stop_answers_a_pending_permission_cancelled() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let (running, _) = ready(&peer, &adapter, "droid-stop-pending", events.sink()).await;
        permission(&peer, "droid-stop-pending", json!(904), Some("execute"));
        let seen = events.clone();
        peer.wait_for("approval.requested", move |_| {
            seen.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
        })
        .await;
        adapter
            .stop_session("droid-stop-pending".into())
            .await
            .unwrap();
        running.await.unwrap();
        assert_eq!(
            answer(&peer, &json!(904)).unwrap()["result"]["outcome"],
            json!({ "outcome": "cancelled" })
        );
        assert!(killed(&peer, "droid-stop-pending"));
    });
}

#[test]
fn a_failed_resume_fails_the_turn_and_keeps_the_binding() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        adapter.bind_session("droid-resume-error", "retained", "/repo", None);
        let events = Events::default();
        let input = || {
            turn(
                "droid-resume-error",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                None,
            )
        };
        let running = start(&adapter, input(), &events);
        peer.answer("droid-resume-error", "initialize", json!({}))
            .await;
        let load = peer.next("session/load", |_| true).await;
        peer.fail(
            "droid-resume-error",
            &load["id"],
            json!({ "code": -32603, "message": "Temporary storage failure" }),
        );
        assert!(running.await.is_err());
        assert!(peer.find("session/new").is_none());

        peer.clear();
        let retry = start(&adapter, input(), &events);
        peer.answer("droid-resume-error", "initialize", json!({}))
            .await;
        let load = peer.next("session/load", |_| true).await;
        assert_eq!(load["params"]["sessionId"], "retained");
        peer.fail(
            "droid-resume-error",
            &load["id"],
            json!({ "code": -32603, "message": "Still failing" }),
        );
        assert!(retry.await.is_err());
    });
}

#[test]
fn waits_for_the_new_model_config_before_applying_effort() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-delayed-config",
                "droid:claude-opus-5-5",
                RuntimeMode::Supervised,
                "test",
                Some(settings(&[("effort", "xhigh")])),
            ),
            &events,
        );
        start_session(&peer, "droid-delayed-config").await;
        let model = peer.next("session/set_config_option", |_| true).await;
        peer.reply("droid-delayed-config", &model["id"], json!({}));
        smol::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(peer.count("session/set_config_option"), 1);
        assert!(peer.find("session/prompt").is_none());
        update(
            &peer,
            "droid-delayed-config",
            json!({
                "sessionUpdate": "config_option_update",
                "configOptions": config("claude-opus-5-5", &["high", "xhigh"], "high"),
            }),
        );
        let effort = peer
            .next("session/set_config_option", |message| {
                message["params"]["configId"] == "reasoning_effort"
            })
            .await;
        assert_eq!(effort["params"]["value"], "xhigh");
        peer.reply("droid-delayed-config", &effort["id"], json!({}));
        peer.answer(
            "droid-delayed-config",
            "session/prompt",
            json!({ "stopReason": "end_turn" }),
        )
        .await;
        running.await.unwrap();
        adapter
            .stop_session("droid-delayed-config".into())
            .await
            .unwrap();
    });
}

#[test]
fn keeps_a_mode_notification_that_arrives_before_the_control_reply() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let input = || {
            turn(
                "droid-mode-update",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                None,
            )
        };
        let running = start(&adapter, input(), &events);
        start_session(&peer, "droid-mode-update").await;
        let mode = peer.next("session/set_mode", |_| true).await;
        update(
            &peer,
            "droid-mode-update",
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "auto-low" }),
        );
        peer.reply("droid-mode-update", &mode["id"], json!({}));
        let prompt = peer.next("session/prompt", |_| true).await;
        peer.reply("droid-mode-update", &prompt["id"], json!({}));
        running.await.unwrap();

        let second = start(&adapter, input(), &events);
        let restore = peer
            .next("session/set_mode", |message| message["id"] != mode["id"])
            .await;
        assert_eq!(restore["params"]["modeId"], "normal");
        peer.reply("droid-mode-update", &restore["id"], json!({}));
        let again = peer
            .next("session/prompt", |message| message["id"] != prompt["id"])
            .await;
        peer.reply("droid-mode-update", &again["id"], json!({}));
        second.await.unwrap();
        adapter
            .stop_session("droid-mode-update".into())
            .await
            .unwrap();
    });
}

#[test]
fn switches_back_after_droid_falls_back_to_another_model() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let (running, prompt) = ready(&peer, &adapter, "droid-fallback", events.sink()).await;
        peer.reply("droid-fallback", &prompt["id"], json!({}));
        running.await.unwrap();
        update(
            &peer,
            "droid-fallback",
            json!({
                "sessionUpdate": "config_option_update",
                "configOptions": config("fallback", &["low"], "low"),
            }),
        );
        let second = start(
            &adapter,
            turn(
                "droid-fallback",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                None,
            ),
            &events,
        );
        let model = peer
            .next("session/set_config_option", |message| {
                message["params"]["configId"] == "model"
            })
            .await;
        assert_eq!(model["params"]["value"], "gpt-6-luna");
        peer.reply("droid-fallback", &model["id"], json!({}));
        let again = peer
            .next("session/prompt", |message| message["id"] != prompt["id"])
            .await;
        peer.reply("droid-fallback", &again["id"], json!({}));
        second.await.unwrap();
        adapter.stop_session("droid-fallback".into()).await.unwrap();
    });
}

#[test]
fn a_follow_up_after_cancel_resumes_on_a_fresh_process() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let (running, _) = ready(&peer, &adapter, "droid-retire", events.sink()).await;
        adapter.cancel_turn("droid-retire".into()).await.unwrap();
        running.await.unwrap();
        assert!(killed(&peer, "droid-retire"));

        peer.clear();
        let following = start(
            &adapter,
            turn(
                "droid-retire",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "next",
                None,
            ),
            &events,
        );
        peer.answer("droid-retire", "initialize", json!({})).await;
        let load = peer.next("session/load", |_| true).await;
        assert_eq!(load["params"]["sessionId"], "droid-session-1");
        peer.reply(
            "droid-retire",
            &load["id"],
            json!({
                "models": { "currentModelId": "gpt-6-luna" },
                "configOptions": config("gpt-6-luna", &["low", "medium"], "medium"),
            }),
        );
        peer.answer("droid-retire", "session/set_mode", json!({}))
            .await;
        peer.answer("droid-retire", "session/prompt", json!({}))
            .await;
        following.await.unwrap();
        assert_eq!(peer.spawned().len(), 1);
        adapter.stop_session("droid-retire".into()).await.unwrap();
    });
}

#[test]
fn uses_the_offered_option_ids_for_automatic_and_planning_replies() {
    for planning in [false, true] {
        smol::block_on(async {
            let session_id = format!("droid-semantic-{planning}");
            let peer = Peer::new();
            let adapter = DroidAdapter::new(&peer.ctx);
            let events = Events::default();
            let mut input = turn(
                &session_id,
                "droid:gpt-6-luna",
                if planning {
                    RuntimeMode::Supervised
                } else {
                    RuntimeMode::FullAccess
                },
                "test",
                None,
            );
            if planning {
                input.session.intent = Some(TurnIntent::Plan);
            }
            let running = start(&adapter, input, &events);
            start_session(&peer, &session_id).await;
            peer.answer(&session_id, "session/set_mode", json!({}))
                .await;
            let prompt = peer.next("session/prompt", |_| true).await;
            permission(
                &peer,
                &session_id,
                json!(905),
                Some(if planning { "switch_mode" } else { "execute" }),
            );
            assert_eq!(
                answered(&peer, json!(905)).await,
                json!({
                    "outcome": "selected",
                    "optionId": if planning { "cancel" } else { "proceed_once" },
                })
            );
            peer.reply(&session_id, &prompt["id"], json!({}));
            running.await.unwrap();
            adapter.stop_session(session_id.clone()).await.unwrap();
        });
    }
}

#[test]
fn reports_a_rejected_reasoning_effort_without_prompting() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-effort-rejected",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                Some(settings(&[("effort", "low")])),
            ),
            &events,
        );
        start_session(&peer, "droid-effort-rejected").await;
        let effort = peer.next("session/set_config_option", |_| true).await;
        peer.fail(
            "droid-effort-rejected",
            &effort["id"],
            json!({ "code": -32602, "message": "Effort rejected" }),
        );
        assert!(running.await.is_err());
        assert!(peer.find("session/prompt").is_none());
    });
}

#[test]
fn reports_an_effort_the_selected_model_does_not_offer() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-effort-missing",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                Some(settings(&[("effort", "max")])),
            ),
            &events,
        );
        start_session(&peer, "droid-effort-missing").await;
        let error = running.await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not support reasoning effort max"),
            "{error:#}"
        );
        assert!(peer.find("session/set_config_option").is_none());
        assert!(peer.find("session/prompt").is_none());
    });
}

#[test]
fn a_config_reply_that_keeps_the_old_effort_is_an_error() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "droid-effort-ignored",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                Some(settings(&[("effort", "low")])),
            ),
            &events,
        );
        start_session(&peer, "droid-effort-ignored").await;
        let effort = peer.next("session/set_config_option", |_| true).await;
        peer.reply(
            "droid-effort-ignored",
            &effort["id"],
            json!({ "configOptions": config("gpt-6-luna", &["none", "low", "medium"], "medium") }),
        );
        let error = running.await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("did not apply the requested reasoning_effort"),
            "{error:#}"
        );
    });
}

#[test]
fn a_remote_host_skips_the_local_catalog_probe() {
    smol::block_on(async {
        let peer = Peer::with_fake(Fake {
            headless: true,
            ..Fake::default()
        });
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let (running, prompt) = ready(&peer, &adapter, "droid-remote", events.sink()).await;
        peer.reply("droid-remote", &prompt["id"], json!({}));
        running.await.unwrap();
        smol::Timer::after(Duration::from_millis(20)).await;
        assert_eq!(probe_spawns(&peer), 0);
        // The session's own model list shows, but as an incomplete catalog.
        assert!(!peer.ctx.catalog.has_live_catalog(HarnessId::Droid));
        adapter.stop_session("droid-remote".into()).await.unwrap();
    });
}

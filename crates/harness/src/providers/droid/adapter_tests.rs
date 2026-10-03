//! Port of droidLive.test.ts. The TypeScript mocked `core/child` and the
//! catalog module; these run the real adapter over `core::testing::Fake`
//! with a scripted ACP peer, and see the catalog probe as its spawn.

use super::*;
use crate::core::testing::Call;
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
            text: "done".into()
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
            answer(&peer).unwrap()["result"]["outcome"],
            json!({ "outcome": "selected", "optionId": "proceed_once" })
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

async fn ready(
    peer: &Peer,
    adapter: &DroidAdapter,
    id: &str,
    events: &Events,
) -> (smol::Task<Result<()>>, Value) {
    let running = start(
        adapter,
        turn(
            id,
            "droid:gpt-6-luna",
            RuntimeMode::Supervised,
            "test",
            None,
        ),
        events,
    );
    start_session(peer, id).await;
    peer.answer(id, "session/set_mode", json!({})).await;
    let prompt = peer.next("session/prompt", |_| true).await;
    (running, prompt)
}
fn permission(peer: &Peer, session: &str, id: Value, kind: Option<&str>) {
    let mut tool = json!({ "toolCallId": "call-1", "title": "test" });
    if let Some(kind) = kind {
        tool["kind"] = json!(kind);
    }
    peer.line(
        session,
        json!({ "jsonrpc": "2.0", "id": id, "method": "session/request_permission", "params": {
        "toolCall": tool, "options": [
            { "optionId": "proceed_once", "kind": "allow_once", "name": "Allow" },
            { "optionId": "cancel", "kind": "reject_once", "name": "Deny" }
        ]
    } }),
    );
}

#[test]
fn forgetting_during_resolution_prevents_spawn_and_binding() {
    smol::block_on(async {
        let (tx, rx) = async_channel::bounded(1);
        let fake = crate::core::testing::Fake {
            resolve_gate: Mutex::new(Some(rx)),
            ..Default::default()
        };
        let peer = Peer::with_fake(fake);
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "deleted",
                "droid:default",
                RuntimeMode::Supervised,
                "test",
                None,
            ),
            &events,
        );
        peer.wait_for("resolution", |peer| {
            peer.calls()
                .iter()
                .any(|call| matches!(call, Call::ResolveDefault(HarnessId::Droid)))
        })
        .await;
        adapter.cancel_turn("deleted".into()).await.unwrap();
        adapter.forget_session("deleted".into()).await.unwrap();
        tx.send(()).await.unwrap();
        running.await.unwrap();
        assert!(peer.spawned().is_empty());
        assert!(events.all().is_empty());
    });
}

#[test]
fn unexpected_exit_rejects_turn_and_releases_pending_permission_tasks() {
    smol::block_on(async {
        for _ in 0..3 {
            let peer = Peer::new();
            let adapter = DroidAdapter::new(&peer.ctx);
            let events = Events::default();
            let (running, _) = ready(&peer, &adapter, "crashed", &events).await;
            permission(&peer, "crashed", json!(900), Some("execute"));
            peer.wait_for("approval", |_| {
                events.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
            })
            .await;
            let live = adapter
                .inner
                .threads
                .lock()
                .live_by_thread
                .get("crashed")
                .unwrap()
                .clone();
            let retained = Arc::downgrade(&live);
            drop(live);
            peer.exit("crashed", 1);
            assert!(running.await.is_err());
            assert!(events.any(|event| matches!(event, HarnessEvent::SessionError { .. })));
            peer.wait_for("released live state", |_| retained.upgrade().is_none())
                .await;
        }
    });
}

#[test]
fn synchronous_denial_preserves_string_rpc_identifier() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let callback = adapter.clone();
        let task_adapter = adapter.clone();
        let running = smol::spawn(async move {
            task_adapter
                .send_turn(
                    turn(
                        "sync",
                        "droid:gpt-6-luna",
                        RuntimeMode::Supervised,
                        "test",
                        None,
                    ),
                    Arc::new(move |event| {
                        if let HarnessEvent::ApprovalRequested { request_id, .. } = event {
                            callback.respond_approval("sync", request_id, ApprovalDecision::Deny);
                        }
                    }),
                    None,
                )
                .await
        });
        start_session(&peer, "sync").await;
        peer.answer("sync", "session/set_mode", json!({})).await;
        let prompt = peer.next("session/prompt", |_| true).await;
        permission(&peer, "sync", json!("permission-abc"), Some("execute"));
        peer.wait_for("permission reply", |peer| {
            peer.sent()
                .iter()
                .any(|msg| msg["id"] == "permission-abc" && msg.get("result").is_some())
        })
        .await;
        let response = peer
            .sent()
            .into_iter()
            .find(|msg| msg["id"] == "permission-abc")
            .unwrap();
        assert_eq!(
            response["result"]["outcome"],
            json!({ "outcome": "selected", "optionId": "cancel" })
        );
        peer.reply("sync", &prompt["id"], json!({}));
        running.await.unwrap();
        adapter.stop_session("sync".into()).await.unwrap();
    });
}

#[test]
fn cancellation_replies_cancelled_and_retires_the_old_connection() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let (running, _) = ready(&peer, &adapter, "cancel", &events).await;
        permission(&peer, "cancel", json!(900), Some("execute"));
        peer.wait_for("approval", |_| {
            events.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
        })
        .await;
        adapter.cancel_turn("cancel".into()).await.unwrap();
        running.await.unwrap();
        assert_eq!(
            peer.sent()
                .into_iter()
                .find(|msg| msg["id"] == 900)
                .unwrap()["result"]["outcome"],
            json!({ "outcome": "cancelled" })
        );
        assert!(peer.calls().contains(&Call::Kill("cancel".into())));
        assert!(
            !adapter
                .inner
                .threads
                .lock()
                .live_by_thread
                .contains_key("cancel")
        );
        permission(&peer, "cancel", json!(901), Some("edit"));
        assert_eq!(
            events
                .all()
                .iter()
                .filter(|event| matches!(event, HarnessEvent::ApprovalRequested { .. }))
                .count(),
            1
        );
    });
}

#[test]
fn sparse_execute_permission_requires_review_in_edit_mode() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "sparse",
                "droid:gpt-6-luna",
                RuntimeMode::AutoAcceptEdits,
                "test",
                None,
            ),
            &events,
        );
        start_session(&peer, "sparse").await;
        peer.answer("sparse", "session/set_mode", json!({})).await;
        let prompt = peer.next("session/prompt", |_| true).await;
        update(
            &peer,
            "sparse",
            json!({ "sessionUpdate": "tool_call", "toolCallId": "call-1", "kind": "execute", "title": "test" }),
        );
        permission(&peer, "sparse", json!(900), None);
        peer.wait_for("sparse approval", |_| events.any(|event| matches!(event, HarnessEvent::ApprovalRequested { kind: Some(kind), .. } if kind == "execute"))).await;
        adapter.respond_approval("sparse", 900, ApprovalDecision::Deny);
        peer.reply("sparse", &prompt["id"], json!({}));
        running.await.unwrap();
        adapter.stop_session("sparse".into()).await.unwrap();
    });
}

#[test]
fn resume_storage_failure_preserves_the_original_binding() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        adapter.bind_session("resume", "retained", "/repo", None);
        let running = start(
            &adapter,
            turn(
                "resume",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                None,
            ),
            &events,
        );
        peer.answer("resume", "initialize", json!({})).await;
        let load = peer.next("session/load", |_| true).await;
        peer.fail(
            "resume",
            &load["id"],
            json!({ "code": -32603, "message": "Temporary storage failure" }),
        );
        assert!(running.await.is_err());
        assert!(peer.find("session/new").is_none());
        assert_eq!(
            adapter.inner.threads.lock().resume_by_thread["resume"].acp_session_id,
            "retained"
        );
    });
}

#[test]
fn delayed_model_config_applies_requested_effort_before_prompt() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "config",
                "droid:claude-opus-5-5",
                RuntimeMode::Supervised,
                "test",
                Some(settings(&[("effort", "xhigh")])),
            ),
            &events,
        );
        start_session(&peer, "config").await;
        let model = peer.next("session/set_config_option", |_| true).await;
        peer.reply("config", &model["id"], json!({}));
        crate::core::task::sleep(crate::core::task::ms(20)).await;
        assert!(peer.find("session/prompt").is_none());
        update(
            &peer,
            "config",
            json!({ "sessionUpdate": "config_option_update", "configOptions": config("claude-opus-5-5", &["high", "xhigh"], "high") }),
        );
        let effort = peer
            .next("session/set_config_option", |msg| {
                msg["params"]["configId"] == "reasoning_effort"
            })
            .await;
        assert_eq!(effort["params"]["value"], "xhigh");
        peer.reply("config", &effort["id"], json!({}));
        peer.answer("config", "session/prompt", json!({})).await;
        running.await.unwrap();
        adapter.stop_session("config".into()).await.unwrap();
    });
}

#[test]
fn retains_mode_notification_received_before_the_control_reply() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let running = start(
            &adapter,
            turn(
                "mode-notification",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "test",
                None,
            ),
            &events,
        );
        start_session(&peer, "mode-notification").await;
        let mode = peer.next("session/set_mode", |_| true).await;
        update(
            &peer,
            "mode-notification",
            json!({ "sessionUpdate": "current_mode_update", "currentModeId": "auto-low" }),
        );
        peer.reply("mode-notification", &mode["id"], json!({}));
        peer.answer("mode-notification", "session/prompt", json!({}))
            .await;
        running.await.unwrap();
        assert_eq!(
            adapter.inner.threads.lock().live_by_thread["mode-notification"]
                .state
                .lock()
                .mode_id,
            "auto-low"
        );
        adapter
            .stop_session("mode-notification".into())
            .await
            .unwrap();
    });
}

#[test]
fn follow_up_waits_for_the_cancelled_child_to_finish_retiring() {
    smol::block_on(async {
        let peer = Peer::new();
        let adapter = DroidAdapter::new(&peer.ctx);
        let events = Events::default();
        let (running, _) = ready(&peer, &adapter, "retire", &events).await;
        let (tx, rx) = async_channel::bounded(1);
        *peer.fake.kill_gate.lock() = Some(rx);
        let cancelling_adapter = adapter.clone();
        let cancelling =
            smol::spawn(async move { cancelling_adapter.cancel_turn("retire".into()).await });
        peer.wait_for("kill", |peer| {
            peer.calls().contains(&Call::Kill("retire".into()))
        })
        .await;
        let following = start(
            &adapter,
            turn(
                "retire",
                "droid:gpt-6-luna",
                RuntimeMode::Supervised,
                "next",
                None,
            ),
            &events,
        );
        crate::core::task::sleep(crate::core::task::ms(20)).await;
        assert_eq!(
            peer.calls()
                .iter()
                .filter(
                    |call| matches!(call, Call::Spawn(request) if request.session_id == "retire")
                )
                .count(),
            1
        );
        *peer.fake.kill_gate.lock() = None;
        tx.send(()).await.unwrap();
        cancelling.await.unwrap();
        running.await.unwrap();
        peer.clear();
        peer.answer("retire", "initialize", json!({})).await;
        peer.answer("retire", "session/load", json!({ "models": { "currentModelId": "gpt-6-luna" }, "configOptions": config("gpt-6-luna", &["low", "medium"], "medium") })).await;
        peer.answer("retire", "session/set_mode", json!({})).await;
        peer.answer("retire", "session/prompt", json!({})).await;
        following.await.unwrap();
        adapter.stop_session("retire".into()).await.unwrap();
    });
}

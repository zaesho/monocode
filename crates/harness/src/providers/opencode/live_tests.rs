//! Port of src/integrations/harness/providers/opencode/opencodeLive.test.ts.
//! The TypeScript mocked the child backend, so these run on [`FakeHost`]
//! instead of a real CLI.
//!
//! The TypeScript reduced the events with `applyHarnessEvent` to check the
//! transcript. That reducer is not ported yet, so these tests read the
//! events directly: agent steps merge by step id, and the pending question is
//! the last one asked and not yet resolved.
// TODO(port): reduce through monocode_core::reducer once it lands.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use serde_json::{Value, json};

use monocode_core::block::{AgentStepKind, ApprovalDecided, ModelSettings, TurnIntent};
use monocode_core::harness::RuntimeMode;
use monocode_core::harness_event::{
    ApprovalDecision, HarnessEvent, HarnessSessionInput, QuestionDecision, RewindLastTurnInput,
    RewindLastTurnResult, SendTurnInput, SteerTurnInput,
};
use monocode_core::user_question::{QuestionAnswers, UserQuestion, UserQuestionReply};

use super::adapter::OpenCodeAdapter;
use super::test_support::{FakeHost, live_test_handler, path_of, wait_for};
use crate::core::catalog::SharedCatalog;
use crate::core::child::HttpRequest;
use crate::core::registry::{EventSink, HarnessAdapter, event_sink, ignore_events};

const THREAD: &str = "opencode-live";
const ROOT: &str = "session_1";

type Events = Arc<Mutex<Vec<HarnessEvent>>>;

struct Harness {
    host: FakeHost,
    adapter: OpenCodeAdapter,
    catalog: SharedCatalog,
    events: Events,
    injected: usize,
    session: Arc<Mutex<FakeSession>>,
}

/// The durable side of the fake OpenCode session.
struct FakeSession {
    messages: Vec<Value>,
    /// The `messageID` of the last `prompt_async`.
    prompt_id: Option<String>,
    status: &'static str,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// `defaultHarnessHttp`: a prompt adds its user message to the history and
/// announces it, and `/session/status` reports the fake status.
fn session_handler(
    host: &FakeHost,
    session: &Arc<Mutex<FakeSession>>,
) -> impl Fn(&HttpRequest) -> (u16, String) + Send + Sync + 'static {
    let host = host.clone();
    let session = session.clone();
    move |request: &HttpRequest| {
        let path = path_of(&request.url);
        if path.ends_with("/prompt_async") {
            let body = body(request);
            let info = json!({
                "id": body["messageID"].clone(),
                "sessionID": ROOT,
                "role": "user",
                "agent": body["agent"].clone(),
                "time": { "created": now() },
            });
            {
                let mut session = session.lock();
                session.prompt_id = body["messageID"].as_str().map(str::to_string);
                session.status = "busy";
                session
                    .messages
                    .push(json!({ "info": info.clone(), "parts": body["parts"].clone() }));
            }
            host.sse(
                THREAD,
                json!({ "type": "message.updated", "properties": { "info": info } }),
            );
            return (204, String::new());
        }
        match (request.method.as_str(), path.as_str()) {
            ("GET", "/session/status") => (
                200,
                json!({ ROOT: { "type": session.lock().status } }).to_string(),
            ),
            ("GET", "/session/session_1/message") => (
                200,
                Value::Array(session.lock().messages.clone()).to_string(),
            ),
            _ => live_test_handler(json!([]))(request),
        }
    }
}

fn sink(events: &Events) -> EventSink {
    let events = events.clone();
    event_sink(move |event| events.lock().push(event))
}

fn session_input(runtime_mode: RuntimeMode) -> HarnessSessionInput {
    HarnessSessionInput {
        session_id: THREAD.into(),
        cwd: "/repo".into(),
        model: "opencode:openrouter/anthropic/claude-sonnet-4.6".into(),
        model_settings: None,
        provider_account_id: None,
        runtime_mode,
        intent: None,
        controls_agents: None,
        app_access: None,
    }
}

fn turn_input(runtime_mode: RuntimeMode) -> SendTurnInput {
    SendTurnInput {
        session: session_input(runtime_mode),
        text: "delegate the investigation".into(),
        attachments: Some(Vec::new()),
    }
}

impl Harness {
    fn new() -> Self {
        Self::with_messages(json!([]))
    }

    /// `sessionMessages` is what `GET /session/session_1/message` returns
    /// before any prompt.
    fn with_messages(session_messages: Value) -> Self {
        let host = FakeHost::new();
        let session = Arc::new(Mutex::new(FakeSession {
            messages: session_messages.as_array().cloned().unwrap_or_default(),
            prompt_id: None,
            status: "idle",
        }));
        host.respond_with(session_handler(&host, &session));
        let catalog = SharedCatalog::new();
        let adapter = OpenCodeAdapter::new(host.children(), catalog.clone(), host.spawner(), None);
        Self {
            host,
            adapter,
            catalog,
            events: Arc::default(),
            injected: 0,
            session,
        }
    }

    /// Put back the default handler after a test replaced it.
    fn restore_handler(&self) {
        self.host
            .respond_with(session_handler(&self.host, &self.session));
    }

    /// The handler a test wraps: the default reply for `request`.
    fn default_handler(&self) -> impl Fn(&HttpRequest) -> (u16, String) + Send + Sync + 'static {
        session_handler(&self.host, &self.session)
    }

    /// The id of the last prompt OpenCode accepted.
    fn prompt_id(&self) -> Option<String> {
        self.session.lock().prompt_id.clone()
    }

    /// Add a durable message and announce it on the stream.
    fn emit_message(&mut self, info: Value, parts: Value) {
        let mut info = info;
        if info.get("time").is_none() {
            info["time"] = json!({ "created": now() });
        }
        self.session
            .lock()
            .messages
            .push(json!({ "info": info.clone(), "parts": parts }));
        info["sessionID"] = json!(ROOT);
        self.sse(json!({ "type": "message.updated", "properties": { "info": info } }));
    }

    /// `session.status` idle with no durable change.
    fn raw_idle(&mut self) {
        self.session.lock().status = "idle";
        self.sse(json!({
            "type": "session.status",
            "properties": { "sessionID": ROOT, "status": { "type": "idle" } },
        }));
    }

    fn send(&self, input: SendTurnInput, events: &Events) -> smol::Task<anyhow::Result<()>> {
        let adapter = self.adapter.clone();
        let on_event = sink(events);
        smol::spawn(async move { adapter.send_turn(input, on_event, None).await })
    }

    /// `turn(events)`.
    fn turn(&self) -> smol::Task<anyhow::Result<()>> {
        self.send(turn_input(RuntimeMode::Supervised), &self.events)
    }

    fn prompts(&self) -> usize {
        self.host.calls_to("/prompt_async").len()
    }

    /// `startTurn`: send, then wait for the prompt to reach OpenCode.
    async fn start_turn(&self) -> smol::Task<anyhow::Result<()>> {
        let before = self.prompts();
        let done = self.turn();
        wait_for("prompt", || self.prompts() > before).await;
        done
    }

    fn sse(&mut self, event: Value) {
        self.host.sse(THREAD, event);
        self.injected += 1;
    }

    fn sse_end(&mut self) {
        self.host.sse_end(THREAD, None);
        self.injected += 1;
    }

    /// Wait until the stream has handled every frame sent so far. The
    /// TypeScript handlers ran as each frame arrived.
    async fn settle(&self) {
        let injected = self.injected;
        wait_for("events handled", || {
            self.adapter.processed_events() >= injected
        })
        .await;
    }

    fn events(&self) -> Vec<HarnessEvent> {
        self.events.lock().clone()
    }

    fn session_created(&mut self, id: &str, parent_id: Option<&str>) {
        let mut info = json!({ "id": id, "directory": "/repo" });
        if let Some(parent_id) = parent_id {
            info["parentID"] = json!(parent_id);
        }
        self.sse(json!({
            "type": "session.created",
            "properties": { "sessionID": id, "info": info },
        }));
    }

    fn ask_permission(&mut self, session_id: &str, id: &str) {
        self.sse(json!({
            "type": "permission.asked",
            "properties": {
                "id": id,
                "sessionID": session_id,
                "permission": "external_directory",
                "patterns": ["/home/user/*"],
                "metadata": { "filepath": "/home/user/.gitconfig" },
                "tool": { "messageID": "message_child", "callID": format!("call_{id}") },
            },
        }));
    }

    /// `idle`: for the root session, the turn's reply lands in the history
    /// first.
    fn idle(&mut self, session_id: &str) {
        if session_id == ROOT {
            let mut session = self.session.lock();
            session.status = "idle";
            if let Some(prompt_id) = session.prompt_id.clone() {
                let id = format!("assistant_{}", session.messages.len());
                session.messages.push(json!({
                    "info": {
                        "id": id, "role": "assistant", "agent": "build", "parentID": prompt_id,
                        "finish": "stop", "time": { "completed": now() },
                    },
                    "parts": [],
                }));
            }
        }
        self.sse(json!({
            "type": "session.status",
            "properties": { "sessionID": session_id, "status": { "type": "idle" } },
        }));
    }

    fn part(&mut self, session_id: &str, mut value: Value) {
        value["sessionID"] = json!(session_id);
        self.sse(json!({ "type": "message.part.updated", "properties": { "part": value } }));
    }

    fn message(
        &mut self,
        session_id: &str,
        id: &str,
        role: &str,
        agent: Option<&str>,
        model_id: Option<&str>,
    ) {
        let mut info = json!({ "sessionID": session_id, "id": id, "role": role });
        if let Some(agent) = agent {
            info["agent"] = json!(agent);
        }
        if let Some(model_id) = model_id {
            info["modelID"] = json!(model_id);
        }
        self.sse(json!({ "type": "message.updated", "properties": { "info": info } }));
    }

    fn task(&mut self, call_id: &str, child: &str) {
        self.part(
            ROOT,
            json!({
                "id": format!("part_{call_id}"), "type": "tool", "tool": "task", "callID": call_id,
                "state": { "status": "running", "title": format!("Task {call_id}"), "metadata": { "sessionId": child } },
            }),
        );
    }

    fn question(&mut self, session_id: &str, id: &str, prompt: &str, option: Value) {
        self.sse(json!({
            "type": "question.asked",
            "properties": {
                "id": id,
                "sessionID": session_id,
                "questions": [{ "question": prompt, "options": [option] }],
            },
        }));
    }

    async fn wait_for_call(
        &self,
        label: &str,
        matches: impl Fn(&HttpRequest) -> bool,
    ) -> HttpRequest {
        wait_for(label, || self.host.http_calls().iter().any(&matches)).await;
        self.host
            .http_calls()
            .into_iter()
            .find(|call| matches(call))
            .unwrap()
    }

    async fn wait_for_event(
        &self,
        label: &str,
        matches: impl Fn(&HarnessEvent) -> bool,
    ) -> HarnessEvent {
        wait_for(label, || self.events.lock().iter().any(&matches)).await;
        self.events()
            .into_iter()
            .find(|event| matches(event))
            .unwrap()
    }
}

fn is_approval(event: &HarnessEvent) -> bool {
    matches!(event, HarnessEvent::ApprovalRequested { .. })
}

fn approvals(events: &[HarnessEvent]) -> Vec<i64> {
    events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::ApprovalRequested { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .collect()
}

fn completed(events: &[HarnessEvent]) -> bool {
    events.contains(&HarnessEvent::MessageCompleted)
}

fn has_error(events: &[HarnessEvent]) -> bool {
    events
        .iter()
        .any(|event| matches!(event, HarnessEvent::SessionError { .. }))
}

/// A merged agent step, as the reducer would keep it.
#[derive(Debug, Clone, PartialEq)]
struct Step {
    id: String,
    kind: AgentStepKind,
    text: String,
    tool_kind: Option<String>,
    status: Option<String>,
    detail: Option<String>,
}

/// Agent steps on `call_id`'s row: repeats of a step id replace it in place.
fn steps(events: &[HarnessEvent], call: &str) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    for event in events {
        let HarnessEvent::AgentStep {
            call_id,
            step_id,
            kind,
            text,
            tool_kind,
            status,
            detail,
            ..
        } = event
        else {
            continue;
        };
        if call_id != call {
            continue;
        }
        let step = Step {
            id: step_id.clone(),
            kind: *kind,
            text: text.clone(),
            tool_kind: tool_kind.clone(),
            status: status.clone(),
            detail: detail.clone(),
        };
        match steps.iter_mut().find(|existing| existing.id == step.id) {
            Some(existing) => *existing = step,
            None => steps.push(step),
        }
    }
    steps
}

/// The model reported for `call_id`'s subagent.
fn agent_model(events: &[HarnessEvent], call: &str) -> Option<String> {
    events.iter().rev().find_map(|event| match event {
        HarnessEvent::ToolUpdated {
            call_id,
            agent_model: Some(model),
            ..
        } if call_id == call => Some(model.clone()),
        _ => None,
    })
}

/// `session.pendingQuestion`: the last question asked and not yet resolved.
fn pending_question(events: &[HarnessEvent]) -> Option<(i64, Vec<UserQuestion>)> {
    let mut pending = None;
    for event in events {
        match event {
            HarnessEvent::QuestionAsked {
                request_id,
                questions,
                ..
            } => pending = Some((*request_id, questions.clone())),
            HarnessEvent::QuestionResolved { request_id, .. }
                if pending.as_ref().is_some_and(|(id, _)| id == request_id) =>
            {
                pending = None
            }
            _ => {}
        }
    }
    pending
}

fn body(call: &HttpRequest) -> Value {
    serde_json::from_str(call.body.as_deref().unwrap_or("null")).unwrap()
}

#[test]
fn reports_when_opencode_accepts_a_turn() {
    smol::block_on(async {
        let mut h = Harness::new();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        let adapter = h.adapter.clone();
        let on_event = sink(&h.events);
        let done = smol::spawn(async move {
            let on_accepted: crate::core::registry::AcceptedHook = Arc::new(move || {
                counter.fetch_add(1, Ordering::SeqCst);
            });
            adapter
                .send_turn(
                    turn_input(RuntimeMode::Supervised),
                    on_event,
                    Some(on_accepted),
                )
                .await
        });
        wait_for("turn acceptance", || accepted.load(Ordering::SeqCst) == 1).await;
        h.idle(ROOT);
        done.await.unwrap();
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
        let events = h.events();
        assert_eq!(
            events[..2],
            [
                HarnessEvent::SessionProviderBound {
                    provider_session_id: ROOT.into()
                },
                HarnessEvent::SessionStarted,
            ]
        );
        let prompt = h.host.calls_to("/prompt_async")[0].body.clone().unwrap();
        let message_id = body(&h.host.calls_to("/prompt_async")[0])["messageID"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            prompt,
            format!(
                r#"{{"messageID":"{message_id}","model":{{"providerID":"openrouter","modelID":"anthropic/claude-sonnet-4.6"}},"agent":"build","parts":[{{"type":"text","text":"delegate the investigation"}}]}}"#
            )
        );
        let spawn = &h.host.spawns()[0];
        assert_eq!(spawn.args, ["serve", "--hostname=127.0.0.1", "--port=4096"]);
        assert_eq!(spawn.cwd, "/repo");
        assert_eq!(
            h.host.sse_opens(),
            [(
                THREAD.to_string(),
                "http://127.0.0.1:4096/event?directory=%2Frepo".to_string()
            )]
        );
    });
}

// describe("OpenCode subagent trails")

#[test]
fn pairs_concurrent_children_by_metadata_and_replays_their_latest_parts_after_creating_the_row() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.session_created("child_b", Some(ROOT));
        h.session_created("child_a", Some(ROOT));
        h.message(
            "child_a",
            "msg_a",
            "assistant",
            None,
            Some("claude-haiku-4-5"),
        );
        h.message("child_b", "msg_b", "assistant", None, None);
        h.part(
            "child_b",
            json!({ "id": "prose_b", "messageID": "msg_b", "type": "text", "text": "Second child" }),
        );
        for i in 0..70 {
            h.part(
                "child_a",
                json!({ "id": "prose_a", "messageID": "msg_a", "type": "text", "text": format!("First child {i}") }),
            );
        }
        h.task("a", "child_a");
        h.task("b", "child_b");
        h.idle("child_a");
        h.settle().await;
        assert!(!completed(&h.events()));
        h.idle(ROOT);
        done.await.unwrap();

        let events = h.events();
        assert_eq!(
            agent_model(&events, "a").as_deref(),
            Some("claude-haiku-4-5")
        );
        let texts = |call: &str| -> Vec<String> {
            steps(&events, call).into_iter().map(|s| s.text).collect()
        };
        assert_eq!(texts("a"), ["First child 69"]);
        assert_eq!(texts("b"), ["Second child"]);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, HarnessEvent::MessageDelta { .. }))
        );
    });
}

#[test]
fn streams_child_reasoning_and_tools_including_nested_tasks_without_user_or_hidden_text() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.task("a", "child");
        h.session_created("child", Some(ROOT));
        h.message("child", "user_msg", "user", None, None);
        h.message("child", "hidden_msg", "assistant", Some("compaction"), None);
        h.message("child", "assistant_msg", "assistant", None, None);
        h.part("child", json!({ "id": "input", "type": "text", "messageID": "user_msg", "text": "Private prompt" }));
        h.part("child", json!({ "id": "hidden", "type": "text", "messageID": "hidden_msg", "text": "Hidden summary" }));
        h.part("child", json!({ "id": "think", "type": "reasoning", "messageID": "assistant_msg", "text": "Trace " }));
        h.sse(json!({
            "type": "message.part.delta",
            "properties": { "sessionID": "child", "partID": "think", "field": "text", "delta": "imports" },
        }));
        h.part("child", json!({
            "id": "read", "type": "tool", "tool": "read", "callID": "read", "messageID": "assistant_msg",
            "state": { "status": "running", "input": { "filePath": "auth.ts" } },
        }));
        h.part("child", json!({
            "id": "read", "type": "tool", "tool": "read", "callID": "read", "messageID": "assistant_msg",
            "state": { "status": "error", "input": { "filePath": "auth.ts" }, "error": "File missing" },
        }));
        h.session_created("grandchild", Some("child"));
        h.message("grandchild", "nested_msg", "assistant", None, None);
        h.part("grandchild", json!({ "id": "nested_text", "type": "text", "messageID": "nested_msg", "text": "Nested answer" }));
        h.part("child", json!({
            "id": "nested_call", "type": "tool", "tool": "task", "callID": "nested_call", "messageID": "assistant_msg",
            "state": { "status": "running", "metadata": { "sessionId": "grandchild" } },
        }));
        // Another session on the same server is not part of this run.
        h.session_created("unrelated", None);
        h.message("unrelated", "other_msg", "assistant", None, None);
        h.part("unrelated", json!({ "id": "other", "type": "text", "messageID": "other_msg", "text": "Other session" }));
        h.idle(ROOT);
        done.await.unwrap();

        let events = h.events();
        let steps = steps(&events, "a");
        assert_eq!(
            steps.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            [
                "child:think",
                "child:read",
                "child:nested_call",
                "grandchild:nested_text"
            ]
        );
        assert_eq!(
            steps.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["Trace imports", "Read auth.ts", "Subagent", "Nested answer"]
        );
        assert_eq!(steps[0].text, "Trace imports");
        assert_eq!(steps[0].kind, AgentStepKind::Reasoning);
        assert_eq!(steps[3].text, "Nested answer");
        let read = &steps[1];
        assert_eq!(read.tool_kind.as_deref(), Some("read"));
        assert_eq!(read.status.as_deref(), Some("failed"));
        assert_eq!(read.detail.as_deref(), Some("File missing"));
        assert_eq!(steps[2].tool_kind.as_deref(), Some("agent"));
        assert_eq!(steps[2].status.as_deref(), Some("in_progress"));
        assert!(!events.iter().any(|event| matches!(
            event,
            HarnessEvent::MessageDelta { .. } | HarnessEvent::ReasoningDelta { .. }
        )));
    });
}

// describe("OpenCode event stream recovery")

fn rejected_file_history() -> Value {
    json!([
        {
            "info": { "id": "message_bad_file", "sessionID": ROOT, "role": "user", "time": { "created": 1 } },
            "parts": [{ "type": "file", "mime": "application/octet-stream", "filename": "Info.plist" }],
        },
        {
            "info": {
                "id": "message_bad_reply", "parentID": "message_bad_file", "sessionID": ROOT,
                "role": "assistant", "time": { "created": 2 },
                "error": { "data": { "message": "'file part media type application/octet-stream' functionality not supported." } },
            },
            "parts": [],
        },
    ])
}

#[test]
fn reverts_a_rejected_file_turn_before_continuing_a_resumed_session() {
    smol::block_on(async {
        let mut h = Harness::with_messages(rejected_file_history());
        h.adapter.bind_session(THREAD, ROOT, "/repo", None);
        let done = h.turn();
        h.wait_for_call("attachment turn recovery", |call| {
            call.url.contains("/session/session_1/revert")
        })
        .await;
        wait_for("resumed prompt", || h.prompts() == 1).await;

        let calls = h.host.http_calls();
        let revert = calls
            .iter()
            .position(|call| call.url.contains("/session/session_1/revert"))
            .unwrap();
        let prompt = calls
            .iter()
            .position(|call| call.url.contains("/prompt_async"))
            .unwrap();
        assert!(prompt > revert);
        assert_eq!(calls[revert].method, "POST");
        assert_eq!(
            calls[revert].body.as_deref(),
            Some(r#"{"messageID":"message_bad_file"}"#)
        );

        h.idle(ROOT);
        done.await.unwrap();
        assert!(completed(&h.events()));
    });
}

#[test]
fn preserves_history_when_a_later_assistant_turn_succeeded() {
    smol::block_on(async {
        let mut history = rejected_file_history();
        history.as_array_mut().unwrap().push(json!({
            "info": { "id": "message_recovered_reply", "role": "assistant", "time": { "created": 3 } },
            "parts": [{ "type": "text", "text": "Recovered" }],
        }));
        let mut h = Harness::with_messages(history);
        h.adapter.bind_session(THREAD, ROOT, "/repo", None);
        let done = h.turn();
        wait_for("resumed prompt", || h.prompts() == 1).await;
        assert!(h.host.calls_to("/revert").is_empty());
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn fails_a_cleanly_ended_stream_and_reconnects_on_the_next_turn() {
    smol::block_on(async {
        let mut h = Harness::new();
        let first = h.start_turn().await;
        h.sse_end();
        let error = first.await.unwrap_err();
        assert_eq!(
            error.to_string(),
            "OpenCode event stream ended unexpectedly."
        );
        assert!(h.events().contains(&HarnessEvent::SessionError {
            message: "OpenCode event stream ended unexpectedly.".into()
        }));

        let second_events: Events = Arc::default();
        let second = h.send(turn_input(RuntimeMode::Supervised), &second_events);
        wait_for("fresh transport", || h.host.spawns().len() == 2).await;
        wait_for("second prompt", || h.prompts() == 2).await;
        h.idle(ROOT);
        second.await.unwrap();
        assert!(completed(&second_events.lock()));
    });
}

// describe("OpenCode edit recovery")

fn rewind(h: &Harness) -> smol::Task<anyhow::Result<RewindLastTurnResult>> {
    let adapter = h.adapter.clone();
    smol::spawn(async move {
        adapter
            .rewind_last_turn(
                RewindLastTurnInput {
                    session: session_input(RuntimeMode::Supervised),
                    provider_turn_id: None,
                    text: None,
                    attachments: None,
                },
                ignore_events(),
            )
            .await
    })
}

#[test]
fn reverts_the_latest_user_message_before_resending_an_edited_prompt() {
    smol::block_on(async {
        let h = Harness::with_messages(json!([
            { "info": { "id": "message_first", "role": "user", "time": { "created": 1 } } },
            { "info": { "id": "message_first_reply", "role": "assistant", "time": { "created": 2 } } },
            { "info": { "id": "message_latest", "role": "user", "time": { "created": 3 } } },
        ]));
        h.adapter.bind_session(THREAD, ROOT, "/repo", None);
        let result = rewind(&h);
        let request = h
            .wait_for_call("edited message revert", |call| {
                call.url.contains("/session/session_1/revert")
            })
            .await;
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.body.as_deref(),
            Some(r#"{"messageID":"message_latest"}"#)
        );
        assert_eq!(
            result.await.unwrap(),
            RewindLastTurnResult { submitted: false }
        );
    });
}

#[test]
fn uses_response_order_when_a_user_timestamp_is_missing() {
    smol::block_on(async {
        let h = Harness::with_messages(json!([
            { "info": { "id": "message_older", "role": "user", "time": { "created": 100 } } },
            { "info": { "id": "message_latest", "role": "user" } },
        ]));
        h.adapter.bind_session(THREAD, ROOT, "/repo", None);
        let result = rewind(&h);
        let request = h
            .wait_for_call("edited message revert", |call| {
                call.url.contains("/session/session_1/revert")
            })
            .await;
        assert_eq!(
            request.body.as_deref(),
            Some(r#"{"messageID":"message_latest"}"#)
        );
        assert_eq!(
            result.await.unwrap(),
            RewindLastTurnResult { submitted: false }
        );
    });
}

// describe("OpenCode access modes")

#[test]
fn restarts_a_live_session_when_access_changes_and_auto_allows_residual_full_access_prompts() {
    smol::block_on(async {
        let mut h = Harness::new();
        let first = h.start_turn().await;
        h.idle(ROOT);
        first.await.unwrap();

        let second = h.send(turn_input(RuntimeMode::FullAccess), &h.events);
        let update = h
            .wait_for_call("permission update", |call| {
                call.method == "PATCH" && path_of(&call.url) == "/session/session_1"
            })
            .await;
        assert_eq!(
            update.url,
            "http://127.0.0.1:4096/session/session_1?directory=%2Frepo"
        );
        assert_eq!(
            update.body.as_deref(),
            Some(r#"{"permission":[{"permission":"*","pattern":"*","action":"allow"}]}"#)
        );
        wait_for("second prompt", || h.prompts() == 2).await;
        // The access mode is server configuration, so the change restarts it.
        assert_eq!(h.host.spawns().len(), 2);

        h.ask_permission(ROOT, "permission_residual");
        let reply = h
            .wait_for_call("automatic full-access reply", |call| {
                path_of(&call.url) == "/permission/permission_residual/reply"
            })
            .await;
        assert_eq!(reply.method, "POST");
        assert_eq!(reply.body.as_deref(), Some(r#"{"reply":"once"}"#));
        assert!(!h.events().iter().any(is_approval));

        h.idle(ROOT);
        second.await.unwrap();
    });
}

// describe("OpenCode child permission routing")

#[test]
fn queues_simultaneous_child_questions_so_each_stays_reachable() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        for id in ["child_a", "child_b"] {
            h.session_created(id, Some(ROOT));
            h.question(
                id,
                &format!("question_{id}"),
                &format!("Question from {id}"),
                json!({ "label": "Proceed" }),
            );
        }
        h.settle().await;
        let asked = h
            .events()
            .iter()
            .filter(|event| matches!(event, HarnessEvent::QuestionAsked { .. }))
            .count();
        assert_eq!(asked, 1);
        for id in ["child_a", "child_b"] {
            let (request_id, questions) = pending_question(&h.events()).unwrap();
            assert_eq!(questions[0].prompt, format!("Question from {id}"));
            h.adapter
                .respond_question(THREAD, request_id, UserQuestionReply::Skipped);
            h.wait_for_call("question response", |call| {
                call.url
                    .contains(&format!("/question/question_{id}/reject"))
            })
            .await;
        }
        assert_eq!(pending_question(&h.events()), None);
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn ends_the_turn_visibly_when_a_child_approval_reply_fails() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.session_created("session_child", Some(ROOT));
        h.ask_permission("session_child", "permission_child");
        let approval = h.wait_for_event("child approval", is_approval).await;
        let HarnessEvent::ApprovalRequested { request_id, .. } = approval else {
            unreachable!()
        };
        h.host.respond_once(500, "Permission reply failed");
        h.adapter
            .respond_approval(THREAD, request_id, ApprovalDecision::Allow);
        h.wait_for_event("permission failure", |event| {
            matches!(event, HarnessEvent::SessionError { .. })
        })
        .await;
        done.await.unwrap();
        assert!(h.events().contains(&HarnessEvent::SessionError {
            message: "Could not route OpenCode event: Permission reply failed".into()
        }));
    });
}

#[test]
fn routes_permission_from_the_session_and_its_descendants_with_each_decision() {
    for (session_id, decision, reply) in [
        (ROOT, ApprovalDecision::Allow, "once"),
        (ROOT, ApprovalDecision::Deny, "reject"),
        ("session_child", ApprovalDecision::Allow, "once"),
        ("session_child", ApprovalDecision::Deny, "reject"),
        ("session_grandchild", ApprovalDecision::Allow, "once"),
        ("session_grandchild", ApprovalDecision::Deny, "reject"),
    ] {
        smol::block_on(async {
            let mut h = Harness::new();
            let done = h.start_turn().await;
            h.session_created("session_child", Some(ROOT));
            h.session_created("session_grandchild", Some("session_child"));
            h.ask_permission(session_id, "permission_child");

            let approval = h.wait_for_event("approval", is_approval).await;
            let HarnessEvent::ApprovalRequested {
                request_id,
                title,
                kind,
                call_id,
                ..
            } = approval
            else {
                unreachable!()
            };
            assert_eq!(kind.as_deref(), Some("external_directory"));
            assert_eq!(call_id.as_deref(), Some("call_permission_child"));
            assert!(title.contains("/home/user"), "{title}");
            // The approval lands on the tool row it belongs to.
            let events = h.events();
            let row = events.iter().position(|event| {
                matches!(event, HarnessEvent::ToolUpdated { call_id, kind, .. }
                    if call_id == "call_permission_child" && kind.as_deref() == Some("external_directory"))
            });
            let asked = events.iter().position(is_approval);
            assert!(row.is_some() && row < asked, "{session_id}");
            assert!(!completed(&events));

            h.adapter.respond_approval(THREAD, request_id, decision);
            let call = h
                .wait_for_call("permission reply", |call| {
                    path_of(&call.url) == "/permission/permission_child/reply"
                })
                .await;
            assert_eq!(call.method, "POST");
            assert_eq!(
                call.url,
                "http://127.0.0.1:4096/permission/permission_child/reply?directory=%2Frepo"
            );
            assert_eq!(body(&call), json!({ "reply": reply }));
            let decided = match decision {
                ApprovalDecision::Allow => ApprovalDecided::Allow,
                ApprovalDecision::Deny => ApprovalDecided::Deny,
            };
            assert!(h.events().contains(&HarnessEvent::ApprovalResolved {
                request_id,
                decision: decided
            }));

            h.idle("session_child");
            h.settle().await;
            assert!(!completed(&h.events()));
            h.idle(ROOT);
            done.await.unwrap();
            assert!(completed(&h.events()));
            assert!(!has_error(&h.events()));
        });
    }
}

#[test]
fn looks_up_ancestry_for_an_existing_child_whose_creation_was_not_observed() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.host.respond_once(
            200,
            r#"{"id":"session_grandchild","parentID":"session_child"}"#,
        );
        h.host
            .respond_once(200, r#"{"id":"session_child","parentID":"session_1"}"#);
        h.ask_permission("session_grandchild", "permission_child");
        h.wait_for_event("existing child approval", is_approval)
            .await;
        for session_id in ["session_grandchild", "session_child"] {
            let url = format!("http://127.0.0.1:4096/session/{session_id}?directory=%2Frepo");
            assert!(
                h.host
                    .http_calls()
                    .iter()
                    .any(|call| call.method == "GET" && call.url == url),
                "{url}"
            );
        }
        h.adapter.cancel_turn(THREAD.into()).await.unwrap();
        done.await.unwrap();
        let reply = h
            .wait_for_call("rejected reply", |call| {
                call.url
                    == "http://127.0.0.1:4096/permission/permission_child/reply?directory=%2Frepo"
            })
            .await;
        assert_eq!(reply.body.as_deref(), Some(r#"{"reply":"reject"}"#));
    });
}

#[test]
fn ignores_unrelated_sessions_and_child_transcript_status_and_error_events() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.session_created("session_child", Some(ROOT));
        h.session_created("session_other", None);
        h.session_created("session_other_child", Some("session_other"));
        h.settle().await;
        let before = h.events();
        h.ask_permission("session_other_child", "permission_child");
        for session_id in ["session_child", "session_other"] {
            h.sse(json!({
                "type": "message.updated",
                "properties": { "info": {
                    "id": "message_child", "sessionID": session_id, "role": "assistant", "tokens": { "input": 123 },
                } },
            }));
            h.sse(json!({
                "type": "message.part.updated",
                "properties": { "part": { "id": "part_child", "sessionID": session_id, "type": "text", "text": "Child-only text" } },
            }));
            h.sse(json!({
                "type": "message.part.updated",
                "properties": { "part": {
                    "id": "tool_child", "sessionID": session_id, "type": "tool", "tool": "read", "state": { "status": "completed" },
                } },
            }));
            h.idle(session_id);
            h.sse(json!({
                "type": "session.error",
                "properties": { "sessionID": session_id, "error": { "message": "Child failed" } },
            }));
        }
        h.idle(ROOT);
        done.await.unwrap();
        let mut expected = before;
        expected.extend([
            HarnessEvent::MessageCompleted,
            HarnessEvent::ReasoningCompleted,
        ]);
        assert_eq!(h.events(), expected);
        assert!(h.host.calls_to("/permission/").is_empty());
    });
}

#[test]
fn keeps_concurrent_child_requests_distinct_and_deduplicates_repeated_events() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.session_created("session_child", Some(ROOT));
        h.sse(json!({
            "type": "session.updated",
            "properties": { "info": { "id": "session_sibling", "parentID": ROOT } },
        }));
        h.ask_permission("session_child", "permission_a");
        h.ask_permission("session_sibling", "permission_b");
        h.ask_permission("session_child", "permission_a");
        h.settle().await;
        let ids = approvals(&h.events());
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        // The TypeScript answered both at once; microtasks kept the replies in
        // answer order. Here each reply runs on its own task, so wait for the
        // first before answering the second.
        h.adapter
            .respond_approval(THREAD, ids[1], ApprovalDecision::Deny);
        h.wait_for_call("first reply", |call| {
            call.url.contains("/permission/permission_b/")
        })
        .await;
        h.adapter
            .respond_approval(THREAD, ids[0], ApprovalDecision::Allow);
        h.wait_for_call("second reply", |call| {
            call.url.contains("/permission/permission_a/")
        })
        .await;
        h.idle(ROOT);
        done.await.unwrap();
        let replies: Vec<(String, Value)> = h
            .host
            .calls_to("/permission/")
            .iter()
            .map(|call| (path_of(&call.url), body(call)))
            .collect();
        assert_eq!(
            replies,
            [
                (
                    "/permission/permission_b/reply".to_string(),
                    json!({ "reply": "reject" })
                ),
                (
                    "/permission/permission_a/reply".to_string(),
                    json!({ "reply": "once" })
                ),
            ]
        );
    });
}

#[test]
fn surfaces_ancestry_lookup_errors_instead_of_silently_losing_requests() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.host.respond_once(500, "Session lookup failed");
        h.ask_permission("session_child", "permission_child");
        done.await.unwrap();
        assert!(h.events().contains(&HarnessEvent::SessionError {
            message: "Could not route OpenCode event: Session lookup failed".into()
        }));
        assert!(!h.events().iter().any(is_approval));
    });
}

#[test]
fn does_not_show_a_late_child_approval_after_cancellation() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        let lookup = h.host.respond_once_later();
        h.ask_permission("session_child", "permission_child");
        h.wait_for_call("ancestry lookup", |call| {
            call.method == "GET" && path_of(&call.url) == "/session/session_child"
        })
        .await;
        h.adapter.cancel_turn(THREAD.into()).await.unwrap();
        lookup
            .send((
                200,
                r#"{"id":"session_child","parentID":"session_1"}"#.into(),
            ))
            .unwrap();
        done.await.unwrap();
        smol::Timer::after(Duration::from_millis(50)).await;
        assert!(!h.events().iter().any(is_approval));
    });
}

#[test]
fn routes_child_questions_when_answered_or_skipped() {
    for answered in [true, false] {
        smol::block_on(async {
            let mut h = Harness::new();
            let done = h.start_turn().await;
            h.session_created("session_child", Some(ROOT));
            h.question(
                "session_child",
                "question_child",
                "Which directory?",
                json!({ "label": "Repo", "description": "Use the repository" }),
            );
            let asked = h
                .wait_for_event("child question", |event| {
                    matches!(event, HarnessEvent::QuestionAsked { .. })
                })
                .await;
            let HarnessEvent::QuestionAsked {
                request_id,
                questions,
                ..
            } = asked
            else {
                unreachable!()
            };
            let question = &questions[0];
            let reply = if answered {
                UserQuestionReply::Answered {
                    answers: QuestionAnswers::from([(
                        question.id.clone(),
                        vec![question.options[0].id.clone()],
                    )]),
                    custom: None,
                }
            } else {
                UserQuestionReply::Skipped
            };
            h.adapter.respond_question(THREAD, request_id, reply);
            h.idle(ROOT);
            done.await.unwrap();
            let route = if answered { "reply" } else { "reject" };
            let call = h
                .wait_for_call("question reply", |call| {
                    call.url
                        == format!("http://127.0.0.1:4096/question/question_child/{route}?directory=%2Frepo")
                })
                .await;
            assert_eq!(call.method, "POST");
            assert_eq!(
                body(&call),
                if answered {
                    json!({ "answers": [["Repo"]] })
                } else {
                    json!({})
                }
            );
            assert!(h.events().contains(&HarnessEvent::QuestionResolved {
                request_id,
                decision: if answered {
                    QuestionDecision::Answered
                } else {
                    QuestionDecision::Skipped
                },
            }));
        });
    }
}

// Cases the TypeScript covered only indirectly.

#[test]
fn ends_the_session_and_fails_the_turn_when_the_server_exits() {
    smol::block_on(async {
        let h = Harness::new();
        let done = h.start_turn().await;
        h.host.exit(THREAD, Some(1));
        let error = done.await.unwrap_err();
        assert_eq!(error.to_string(), "OpenCode server exited");
        let events = h.events();
        assert!(events.contains(&HarnessEvent::SessionEnded { code: Some(1) }));
        assert!(events.contains(&HarnessEvent::SessionError {
            message: "OpenCode server exited".into()
        }));
    });
}

#[test]
fn answers_permissions_itself_while_planning() {
    smol::block_on(async {
        let mut h = Harness::new();
        let mut input = turn_input(RuntimeMode::Supervised);
        input.session.intent = Some(TurnIntent::Plan);
        let done = h.send(input, &h.events);
        wait_for("prompt", || h.prompts() == 1).await;
        let prompt = &h.host.calls_to("/prompt_async")[0];
        assert!(
            prompt
                .body
                .as_deref()
                .unwrap()
                .contains(r#""agent":"plan""#)
        );
        h.sse(json!({
            "type": "permission.asked",
            "properties": { "id": "read_request", "sessionID": ROOT, "permission": "read", "patterns": ["src/a.ts"] },
        }));
        h.sse(json!({
            "type": "permission.asked",
            "properties": { "id": "bash_request", "sessionID": ROOT, "permission": "bash", "patterns": ["rm -rf build"] },
        }));
        let read = h
            .wait_for_call("read reply", |call| {
                call.url.contains("/permission/read_request/")
            })
            .await;
        let bash = h
            .wait_for_call("bash reply", |call| {
                call.url.contains("/permission/bash_request/")
            })
            .await;
        assert_eq!(body(&read), json!({ "reply": "once" }));
        assert_eq!(body(&bash), json!({ "reply": "reject" }));
        assert!(!h.events().iter().any(is_approval));
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn reports_streamed_text_tools_retries_usage_and_session_errors() {
    smol::block_on(async {
        let mut h = Harness::new();
        h.adapter.set_error_grace(Duration::from_millis(20));
        let done = h.start_turn().await;
        h.message(ROOT, "assistant_1", "assistant", None, None);
        h.part(
            ROOT,
            json!({ "id": "text_1", "messageID": "assistant_1", "type": "text", "text": "Hel" }),
        );
        h.sse(json!({
            "type": "message.part.delta",
            "properties": { "sessionID": ROOT, "partID": "text_1", "delta": "lo" },
        }));
        h.part(
            ROOT,
            json!({ "id": "text_1", "messageID": "assistant_1", "type": "text", "text": "Hello" }),
        );
        h.part(ROOT, json!({
            "id": "todo", "messageID": "assistant_1", "type": "tool", "tool": "todowrite", "callID": "todo_call",
            "state": { "status": "pending", "input": { "todos": [{ "content": "Write tests", "status": "in_progress" }] } },
        }));
        h.sse(json!({
            "type": "session.status",
            "properties": { "sessionID": ROOT, "status": { "type": "retry", "message": "Rate limited, retrying" } },
        }));
        h.sse(json!({
            "type": "message.updated",
            "properties": { "info": {
                "id": "assistant_1", "sessionID": ROOT, "role": "assistant", "providerID": "openrouter", "modelID": "m",
                "tokens": { "input": 100, "output": 20, "reasoning": 5, "cache": { "read": 300, "write": 0 } },
            } },
        }));
        // The error ends the turn once the session stays idle without
        // progress for the grace period.
        h.session.lock().status = "idle";
        h.sse(json!({
            "type": "session.error",
            "properties": { "sessionID": ROOT, "error": { "data": { "message": "Provider overloaded" } } },
        }));
        done.await.unwrap();

        let events = h.events();
        let parts: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                HarnessEvent::MessagePart { part_id, text, .. } if part_id == "text_1" => {
                    Some(text.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(parts, ["Hel", "Hello"]);
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, HarnessEvent::MessageDelta { .. }))
        );
        assert!(events.iter().any(|event| matches!(
            event,
            HarnessEvent::TasksUpdated { items, .. } if items.len() == 1 && items[0].text == "Write tests"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            HarnessEvent::ToolStarted { call_id, kind, status, .. }
                if call_id == "todo_call" && kind.as_deref() == Some("tasks") && status.as_deref() == Some("pending")
        )));
        assert!(events.contains(&HarnessEvent::Status {
            text: "Rate limited, retrying".into()
        }));
        let metrics = events.iter().find_map(|event| match event {
            HarnessEvent::TurnMetrics(metrics) => Some(metrics.clone()),
            _ => None,
        });
        let metrics = serde_json::to_value(metrics.unwrap()).unwrap();
        assert_eq!(
            metrics,
            json!({
                "inputTokens": 100, "outputTokens": 25, "cacheReadTokens": 300, "cacheWriteTokens": 0,
                "cacheHitPercent": 75.0,
            })
        );
        assert!(events.contains(&HarnessEvent::Context {
            used: Some(425),
            window: None
        }));
        assert!(events.contains(&HarnessEvent::SessionError {
            message: "Provider overloaded".into()
        }));
    });
}

#[test]
fn steers_only_an_active_turn_with_the_model_settings() {
    smol::block_on(async {
        let mut h = Harness::new();
        let steer = |h: &Harness| {
            let adapter = h.adapter.clone();
            smol::spawn(async move {
                adapter
                    .steer_turn(SteerTurnInput {
                        session_id: THREAD.into(),
                        cwd: "/repo".into(),
                        model: "opencode:openai/gpt-5.4".into(),
                        model_settings: Some(
                            [("agent", "review"), ("variant", "high")]
                                .into_iter()
                                .map(|(k, v)| (k.to_string(), v.to_string()))
                                .collect(),
                        ),
                        text: "also check the tests".into(),
                        attachments: None,
                    })
                    .await
            })
        };
        assert_eq!(
            steer(&h).await.unwrap_err().to_string(),
            "No active turn to steer"
        );
        let done = h.start_turn().await;
        steer(&h).await.unwrap();
        let steered = body(&h.host.calls_to("/prompt_async")[1]);
        let message_id = steered["messageID"].as_str().unwrap().to_string();
        assert!(message_id.starts_with("msg_"));
        assert_ne!(
            Some(message_id),
            h.host
                .calls_to("/prompt_async")
                .first()
                .map(|first| body(first)["messageID"].as_str().unwrap().to_string())
        );
        // A steer joins the turn, so it keeps the turn's agent.
        assert_eq!(
            steered,
            json!({
                "messageID": steered["messageID"].clone(),
                "model": { "providerID": "openai", "modelID": "gpt-5.4" },
                "agent": "build",
                "variant": "high",
                "parts": [{ "type": "text", "text": "also check the tests" }],
            })
        );
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn registers_once_with_every_optional_capability() {
    use crate::core::register::HarnessContext;
    use crate::core::registry::{HarnessRegistry, RegistryOptions};
    use monocode_core::harness::HarnessId;

    let host = FakeHost::new();
    let registry = HarnessRegistry::new(host.spawner(), RegistryOptions::default());
    let ctx = HarnessContext::new(registry.clone(), host.children(), SharedCatalog::new());
    super::register(&ctx);
    let first = registry.get_harness(HarnessId::Opencode).unwrap();
    super::register(&ctx);
    let second = registry.get_harness(HarnessId::Opencode).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(registry.can_compact_harness_context(HarnessId::Opencode));
    assert!(registry.can_rewind_harness_last_turn(HarnessId::Opencode));
    assert!(registry.can_run_harness_text_prompt(HarnessId::Opencode));
    assert!(registry.can_steer_harness(HarnessId::Opencode));
    let error = smol::block_on(registry.generate_harness_commit_message(
        HarnessId::Opencode,
        "/repo",
        None,
    ))
    .unwrap_err();
    assert_eq!(error.to_string(), "Git context is not available");
}

#[test]
fn closes_an_event_stream_that_ended_on_its_own_when_the_session_stops() {
    smol::block_on(async {
        let h = Harness::new();
        let done = h.start_turn().await;
        h.host.sse_end(THREAD, Some("stream closed"));
        let _ = done.await;
        h.host.clear_sse_closes_and_kills();

        h.adapter.stop_session(THREAD.into()).await.unwrap();
        assert_eq!(h.host.sse_closes(), [THREAD]);
        assert_eq!(h.host.watched_streams(), 0);
    });
}

#[test]
fn closes_the_event_stream_after_the_server_exits_on_its_own() {
    smol::block_on(async {
        let h = Harness::new();
        let done = h.start_turn().await;
        h.host.exit(THREAD, Some(1));
        assert_eq!(
            done.await.unwrap_err().to_string(),
            "OpenCode server exited"
        );
        assert!(
            h.events()
                .contains(&HarnessEvent::SessionEnded { code: Some(1) })
        );

        h.adapter.stop_session(THREAD.into()).await.unwrap();
        assert_eq!(h.host.sse_closes(), [THREAD]);
        assert_eq!(h.host.kills(), [THREAD]);
        assert_eq!(h.host.watched_streams(), 0);
    });
}

#[test]
fn still_kills_the_child_when_closing_an_ended_stream_fails() {
    smol::block_on(async {
        let h = Harness::new();
        let done = h.start_turn().await;
        h.host.sse_end(THREAD, Some("stream closed"));
        assert_eq!(done.await.unwrap_err().to_string(), "stream closed");
        h.host.clear_sse_closes_and_kills();
        h.host.fail_next_sse_close("SSE close failed");

        h.adapter.stop_session(THREAD.into()).await.unwrap();
        assert_eq!(h.host.sse_closes(), [THREAD]);
        assert_eq!(h.host.kills(), [THREAD]);
    });
}

// describe("OpenCode review regressions")

fn plan_input(runtime_mode: RuntimeMode) -> SendTurnInput {
    let mut input = turn_input(runtime_mode);
    input.session.intent = Some(TurnIntent::Plan);
    input
}

#[test]
fn rejects_opencode_2_before_spawning_a_v1_server() {
    smol::block_on(async {
        let h = Harness::new();
        h.host.exec_once("opencode v2.0.20");
        let error = h.turn().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("OpenCode 2 uses a different API")
        );
        assert!(h.host.spawns().is_empty());
    });
}

#[test]
fn rejects_effective_higher_priority_permissions_before_creating_a_prompt() {
    smol::block_on(async {
        let h = Harness::new();
        let handler = live_test_handler(json!([]));
        h.host.respond_with(move |request| {
            if path_of(&request.url) == "/agent" {
                return (
                    200,
                    json!([{
                        "name": "injected",
                        "permission": [
                            { "permission": "*", "pattern": "*", "action": "ask" },
                            { "permission": "bash", "pattern": "*", "action": "allow" },
                        ],
                    }])
                    .to_string(),
                );
            }
            handler(request)
        });
        let error = h.turn().await.unwrap_err();
        assert!(error.to_string().contains("grants tools beyond"));
        assert_eq!(h.prompts(), 0);
        assert!(h.host.kills().contains(&THREAD.to_string()));
    });
}

#[test]
fn rejects_a_higher_priority_primary_child_tool_grant() {
    smol::block_on(async {
        let h = Harness::new();
        let handler = live_test_handler(json!([]));
        h.host.respond_with(move |request| {
            if path_of(&request.url) == "/config" {
                return (
                    200,
                    json!({ "experimental": { "primary_tools": ["bash"] } }).to_string(),
                );
            }
            handler(request)
        });
        let error = h.turn().await.unwrap_err();
        assert!(error.to_string().contains("grants tools beyond"));
        assert_eq!(h.prompts(), 0);
    });
}

#[test]
fn rejects_an_effective_subagent_explore_grant_before_creating_a_prompt() {
    smol::block_on(async {
        let h = Harness::new();
        let handler = live_test_handler(json!([]));
        h.host.respond_with(move |request| {
            if path_of(&request.url) == "/agent" {
                return (
                    200,
                    json!([{
                        "name": "explore",
                        "mode": "subagent",
                        "permission": [
                            { "permission": "*", "pattern": "*", "action": "deny" },
                            { "permission": "task", "pattern": "*", "action": "deny" },
                            { "permission": "task", "pattern": "explore", "action": "allow" },
                        ],
                    }])
                    .to_string(),
                );
            }
            handler(request)
        });
        let error = h
            .send(plan_input(RuntimeMode::Supervised), &h.events)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("grants tools beyond"));
        assert_eq!(h.prompts(), 0);
        assert!(h.host.kills().contains(&THREAD.to_string()));
    });
}

#[test]
fn starts_plan_and_its_subagents_with_the_managed_permission_policy() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.send(plan_input(RuntimeMode::FullAccess), &h.events);
        wait_for("Plan prompt", || h.prompts() == 1).await;
        let config = h.host.spawned_config().unwrap();
        assert_eq!(config["permission"]["*"], "deny");
        assert_eq!(config["permission"]["task"], "deny");
        assert_eq!(
            config["agent"]["plan"]["permission"]["task"],
            json!({ "*": "deny", "explore": "allow" })
        );
        assert_eq!(config["agent"]["general"]["permission"]["task"], "deny");
        assert_eq!(config["agent"]["explore"]["permission"]["task"], "deny");
        assert_eq!(config["agent"]["general"]["permission"]["*"], "deny");
        assert_eq!(
            config["permission"]["external_directory"],
            json!({ "*": "deny", "/isolated/data/opencode/tool-output/*": "allow" })
        );
        assert_eq!(config["experimental"]["primary_tools"], json!([]));
        let session = h
            .host
            .http_calls()
            .into_iter()
            .find(|call| call.method == "POST" && path_of(&call.url) == "/session")
            .unwrap();
        assert!(
            body(&session)["permission"]
                .as_array()
                .unwrap()
                .contains(&json!({
                    "permission": "external_directory",
                    "pattern": "/isolated/data/opencode/tool-output/*",
                    "action": "allow",
                }))
        );
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn rejects_an_unsafe_tool_output_directory_before_starting_the_server() {
    smol::block_on(async {
        let h = Harness::new();
        h.host.set_debug_paths("data       /other/*");
        let error = h.turn().await.unwrap_err();
        assert!(error.to_string().contains("safe data directory"));
        assert!(h.host.spawns().is_empty());
    });
}

#[test]
fn replies_to_a_plan_task_by_its_scope_and_session() {
    let scopes: [(Option<Value>, &str); 14] = [
        (None, "reject"),
        (Some(Value::Null), "reject"),
        (Some(json!([])), "reject"),
        (Some(json!("explore")), "reject"),
        (Some(json!({ "agent": "explore" })), "reject"),
        (Some(json!([42])), "reject"),
        (Some(json!([""])), "reject"),
        (Some(json!([" explore "])), "reject"),
        (Some(json!(["explore", 42])), "reject"),
        (Some(json!(["explore", null])), "reject"),
        (Some(json!(["general"])), "reject"),
        (Some(json!(["explore", "general"])), "reject"),
        (Some(json!(["explore"])), "once"),
        (Some(json!(["explore", "explore"])), "once"),
    ];
    for (patterns, root_reply) in scopes {
        for session_id in [ROOT, "session_child", "session_grandchild"] {
            smol::block_on(async {
                let mut h = Harness::new();
                let done = h.send(plan_input(RuntimeMode::FullAccess), &h.events);
                wait_for("Plan prompt", || h.prompts() == 1).await;
                if session_id != ROOT {
                    h.session_created("session_child", Some(ROOT));
                }
                if session_id == "session_grandchild" {
                    h.session_created(session_id, Some("session_child"));
                }
                let mut properties = json!({
                    "id": "plan_task_request",
                    "sessionID": session_id,
                    "permission": "task",
                    "metadata": { "subagent_type": "explore" },
                });
                if let Some(patterns) = &patterns {
                    properties["patterns"] = patterns.clone();
                }
                h.sse(json!({ "type": "permission.asked", "properties": properties }));
                let request = h
                    .wait_for_call("Plan task reply", |call| {
                        call.url.contains("/permission/plan_task_request/reply")
                    })
                    .await;
                assert_eq!(request.method, "POST");
                assert_eq!(
                    request.url,
                    "http://127.0.0.1:4096/permission/plan_task_request/reply?directory=%2Frepo"
                );
                let reply = if session_id == ROOT {
                    root_reply
                } else {
                    "reject"
                };
                assert_eq!(
                    body(&request),
                    json!({ "reply": reply }),
                    "{patterns:?} {session_id}"
                );
                assert!(!h.events().iter().any(is_approval));
                h.idle(ROOT);
                done.await.unwrap();
            });
        }
    }
}

#[test]
fn does_not_submit_a_resumed_prompt_after_its_permission_patch_fails() {
    smol::block_on(async {
        let h = Harness::new();
        h.adapter.bind_session(THREAD, ROOT, "/repo", None);
        let handler = live_test_handler(json!([]));
        h.host.respond_with(move |request| {
            if request.method == "PATCH" {
                return (500, "Permission update failed".into());
            }
            handler(request)
        });
        let error = h.turn().await.unwrap_err();
        assert!(error.to_string().contains("Permission update failed"));
        assert_eq!(h.prompts(), 0);
        assert!(!h.host.kills().is_empty());
    });
}

/// The texts of `part_id`'s snapshots, in order.
fn part_texts(events: &[HarnessEvent], part: &str) -> Vec<(String, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            HarnessEvent::MessagePart {
                part_id,
                text,
                streaming,
                ..
            } if part_id == part => Some((text.clone(), *streaming)),
            _ => None,
        })
        .collect()
}

#[test]
fn applies_final_corrections_to_one_part_and_ignores_its_late_deltas() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        let part = |text: &str, end: Option<i64>| {
            let mut time = json!({ "start": 1 });
            if let Some(end) = end {
                time["end"] = json!(end);
            }
            json!({ "id": "text_part", "messageID": "assistant_text", "type": "text", "text": text, "time": time })
        };
        // A part that arrives before its message's role waits for it.
        h.part(ROOT, part("Hello worle", None));
        h.settle().await;
        assert!(part_texts(&h.events(), "text_part").is_empty());
        h.message(ROOT, "assistant_text", "assistant", None, None);
        h.part(ROOT, part("Hello world", Some(2)));
        h.sse(json!({
            "type": "message.part.delta",
            "properties": { "sessionID": ROOT, "partID": "text_part", "field": "text", "delta": "ld" },
        }));
        h.part(ROOT, part("Hello", Some(3)));
        h.settle().await;
        assert_eq!(
            part_texts(&h.events(), "text_part"),
            [
                ("Hello worle".to_string(), true),
                ("Hello world".to_string(), false),
                ("Hello".to_string(), false),
            ]
        );
        h.idle(ROOT);
        done.await.unwrap();
    });
}

// Turn recovery: the durable history settles a turn, not the idle event.

fn compact(h: &Harness) -> smol::Task<anyhow::Result<()>> {
    let adapter = h.adapter.clone();
    smol::spawn(async move {
        adapter
            .compact_context(session_input(RuntimeMode::Supervised), ignore_events())
            .await
    })
}

/// Give spawned work a moment, then report whether `task` settled.
async fn settled<T>(task: &smol::Task<T>) -> bool {
    smol::Timer::after(Duration::from_millis(40)).await;
    task.is_finished()
}

fn session_error(name: &str, message: &str) -> Value {
    json!({
        "type": "session.error",
        "properties": { "sessionID": ROOT, "error": { "name": name, "data": { "message": message } } },
    })
}

#[test]
fn waits_through_recoverable_context_overflow_and_returns_its_resumed_answer() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.sse(session_error("ContextOverflowError", "Context exceeded"));
        h.emit_message(
            json!({ "id": "compact_user", "role": "user" }),
            json!([{ "type": "compaction", "auto": true }]),
        );
        h.emit_message(
            json!({ "id": "summary", "role": "assistant", "agent": "compaction", "parentID": "compact_user", "finish": "stop" }),
            json!([]),
        );
        let replayed = h.session.lock().messages[0]["parts"].clone();
        h.emit_message(json!({ "id": "replayed_user", "role": "user" }), replayed);
        assert!(!settled(&done).await);
        assert!(!has_error(&h.events()));
        assert!(h.events().contains(&HarnessEvent::Status {
            text: "OpenCode is compacting context after the provider rejected its size.".into()
        }));
        h.emit_message(
            json!({ "id": "recovered", "role": "assistant", "agent": "build", "parentID": "replayed_user", "finish": "stop" }),
            json!([]),
        );
        h.part(
            ROOT,
            json!({ "id": "recovered_text", "messageID": "recovered", "type": "text", "text": "Recovered after compaction", "time": { "start": 1, "end": 2 } }),
        );
        h.raw_idle();
        done.await.unwrap();
        assert_eq!(
            part_texts(&h.events(), "recovered_text"),
            [("Recovered after compaction".to_string(), false)]
        );
        assert!(!has_error(&h.events()));
    });
}

#[test]
fn reports_context_overflow_when_the_compaction_itself_fails() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.sse(session_error("ContextOverflowError", "Context exceeded"));
        h.emit_message(
            json!({ "id": "compact_user", "role": "user" }),
            json!([{ "type": "compaction", "auto": true }]),
        );
        h.emit_message(
            json!({
                "id": "summary", "role": "assistant", "agent": "compaction", "parentID": "compact_user",
                "error": { "name": "ContextOverflowError", "data": { "message": "Too large to compact" } },
            }),
            json!([]),
        );
        h.raw_idle();
        done.await.unwrap();
        assert!(h.events().contains(&HarnessEvent::SessionError {
            message: "Too large to compact".into()
        }));
    });
}

#[test]
fn ignores_late_users_and_unrelated_newer_users_when_checking_idle() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.emit_message(
            json!({ "id": "old_user", "role": "user", "time": { "created": 1 } }),
            json!([{ "type": "text", "text": "Old request" }]),
        );
        h.emit_message(
            json!({ "id": "old_reply", "role": "assistant", "parentID": "old_user", "agent": "build", "finish": "stop" }),
            json!([]),
        );
        h.emit_message(
            json!({ "id": "unrelated_user", "role": "user" }),
            json!([{ "type": "text", "text": "Unrelated request" }]),
        );
        h.emit_message(
            json!({ "id": "unrelated_reply", "role": "assistant", "parentID": "unrelated_user", "agent": "build", "finish": "stop" }),
            json!([]),
        );
        h.raw_idle();
        assert!(!settled(&done).await);
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn rechecks_an_idle_that_arrives_during_an_older_status_request() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        let stale = h.host.defer("GET", "/session/status");
        h.raw_idle();
        wait_for("status request", || {
            !h.host.calls_to("/session/status").is_empty()
        })
        .await;
        h.idle(ROOT);
        h.settle().await;
        stale
            .send((200, json!({ ROOT: { "type": "busy" } }).to_string()))
            .unwrap();
        done.await.unwrap();
        assert_eq!(h.host.calls_to("/session/status").len(), 2);
    });
}

fn steer_input(text: &str) -> SteerTurnInput {
    SteerTurnInput {
        session_id: THREAD.into(),
        cwd: "/repo".into(),
        model: "opencode:openrouter/anthropic/claude-sonnet-4.6".into(),
        model_settings: Some(ModelSettings::from([(
            "agent".to_string(),
            "build".to_string(),
        )])),
        text: text.into(),
        attachments: None,
    }
}

#[test]
fn waits_for_the_latest_owned_steering_prompt_at_idle() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        let original = h.prompt_id().unwrap();
        h.adapter
            .steer_turn(steer_input("Current steering request"))
            .await
            .unwrap();
        h.emit_message(
            json!({ "id": "first_reply", "role": "assistant", "parentID": original, "agent": "build", "finish": "stop" }),
            json!([]),
        );
        h.raw_idle();
        assert!(!settled(&done).await);
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn keeps_the_plan_agent_on_a_steered_follow_up() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.send(plan_input(RuntimeMode::Supervised), &h.events);
        wait_for("Plan prompt", || h.prompts() == 1).await;
        h.adapter
            .steer_turn(steer_input("Also account for Windows"))
            .await
            .unwrap();
        let agents: Vec<Value> = h
            .host
            .calls_to("/prompt_async")
            .iter()
            .map(|call| body(call)["agent"].clone())
            .collect();
        assert_eq!(agents, [json!("plan"), json!("plan")]);
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn correlates_a_synthetic_continuation_only_after_its_current_compaction() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        h.emit_message(
            json!({ "id": "compact_user", "role": "user" }),
            json!([{ "type": "compaction", "auto": true }]),
        );
        h.emit_message(
            json!({ "id": "summary", "role": "assistant", "parentID": "compact_user", "agent": "compaction", "finish": "stop" }),
            json!([]),
        );
        h.emit_message(
            json!({ "id": "continue_user", "role": "user" }),
            json!([{ "type": "text", "text": "Continue", "synthetic": true, "metadata": { "compaction_continue": true } }]),
        );
        h.emit_message(
            json!({ "id": "final_reply", "role": "assistant", "parentID": "continue_user", "agent": "build", "finish": "stop" }),
            json!([]),
        );
        h.raw_idle();
        done.await.unwrap();
        assert!(completed(&h.events()));
    });
}

#[test]
fn does_not_use_a_delayed_manual_compaction_idle_to_complete_a_new_prompt() {
    smol::block_on(async {
        let mut h = Harness::new();
        compact(&h).await.unwrap();
        let done = h.start_turn().await;
        h.raw_idle();
        assert!(!settled(&done).await);
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn fails_manual_compaction_on_a_durable_summary_error() {
    smol::block_on(async {
        let h = Harness::new();
        let handler = h.default_handler();
        let session = h.session.clone();
        h.host.respond_with(move |request| {
            if request.url.contains("/summarize") {
                session.lock().messages.push(json!({
                    "info": {
                        "id": "failed_summary", "role": "assistant", "agent": "compaction",
                        "error": { "data": { "message": "Summary failed" } },
                    },
                }));
            }
            handler(request)
        });
        let error = compact(&h).await.unwrap_err();
        assert!(error.to_string().contains("Summary failed"));
    });
}

#[test]
fn keeps_a_failed_attachment_read_warning_nonfatal_through_a_successful_reply() {
    smol::block_on(async {
        let mut h = Harness::new();
        let handler = h.default_handler();
        let host = h.host.clone();
        h.host.respond_with(move |request| {
            if request.url.contains("/prompt_async") {
                host.sse(
                    THREAD,
                    session_error("UnknownError", "ENOENT: no such file"),
                );
            }
            handler(request)
        });
        let done = h.start_turn().await;
        h.wait_for_event("warning status", |event| {
            *event
                == HarnessEvent::Status {
                    text: "ENOENT: no such file".into(),
                }
        })
        .await;
        assert!(!has_error(&h.events()));
        h.idle(ROOT);
        done.await.unwrap();
        assert!(!has_error(&h.events()));
    });
}

#[test]
fn reports_terminal_setup_failures_without_waiting_for_a_user_or_idle_event() {
    for message in ["Agent not found: missing", "Model not found: local/missing"] {
        smol::block_on(async {
            let mut h = Harness::new();
            let handler = h.default_handler();
            let host = h.host.clone();
            h.host.respond_with(move |request| {
                if request.url.contains("/prompt_async") {
                    host.sse(THREAD, session_error("UnknownError", message));
                    return (204, String::new());
                }
                handler(request)
            });
            h.turn().await.unwrap();
            assert!(h.events().contains(&HarnessEvent::SessionError {
                message: message.into()
            }));
            h.restore_handler();
            let next = h.start_turn().await;
            h.idle(ROOT);
            next.await.unwrap();
        });
    }
}

#[test]
fn bounds_an_anonymous_preparation_error_with_no_durable_progress() {
    smol::block_on(async {
        let h = Harness::new();
        h.adapter.set_error_grace(Duration::from_millis(150));
        let handler = h.default_handler();
        let host = h.host.clone();
        h.host.respond_with(move |request| {
            if request.url.contains("/prompt_async") {
                host.sse(
                    THREAD,
                    session_error("UnknownError", "Preparation hook failed"),
                );
                return (204, String::new());
            }
            handler(request)
        });
        let done = h.turn();
        assert!(!settled(&done).await);
        done.await.unwrap();
        assert!(h.events().contains(&HarnessEvent::SessionError {
            message: "Preparation hook failed".into()
        }));
    });
}

#[test]
fn does_not_classify_an_attachment_warning_while_its_assistant_is_still_running() {
    smol::block_on(async {
        let mut h = Harness::new();
        h.adapter.set_error_grace(Duration::from_millis(20));
        let done = h.start_turn().await;
        h.sse(session_error("UnknownError", "Attachment read failed"));
        let prompt = h.prompt_id().unwrap();
        h.emit_message(
            json!({ "id": "running_reply", "role": "assistant", "parentID": prompt, "agent": "build" }),
            json!([]),
        );
        h.raw_idle();
        smol::Timer::after(Duration::from_millis(150)).await;
        assert!(!has_error(&h.events()));
        h.idle(ROOT);
        done.await.unwrap();
    });
}

#[test]
fn clears_rejected_prompt_state_so_rewind_and_later_prompts_still_work() {
    smol::block_on(async {
        let h = Harness::with_messages(json!([{
            "info": { "id": "previous_user", "role": "user", "time": { "created": 1 } },
            "parts": [{ "type": "text", "text": "Previous request" }],
        }]));
        let handler = h.default_handler();
        h.host.respond_with(move |request| {
            if request.url.contains("/prompt_async") {
                return (500, "Prompt rejected".into());
            }
            handler(request)
        });
        let error = h.turn().await.unwrap_err();
        assert!(error.to_string().contains("Prompt rejected"));
        let error = h
            .adapter
            .steer_turn(steer_input("Follow-up"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("No active turn"));
        assert!(!rewind(&h).await.unwrap().submitted);
        h.restore_handler();
        let mut h = h;
        let next = h.start_turn().await;
        h.idle(ROOT);
        next.await.unwrap();
    });
}

#[test]
fn rewinds_the_visible_request_before_internal_messages() {
    for internal in [
        json!([{ "type": "compaction", "auto": false }]),
        json!([{ "type": "text", "text": "Continue", "synthetic": true }]),
    ] {
        smol::block_on(async {
            let h = Harness::with_messages(json!([
                {
                    "info": { "id": "visible_user", "role": "user", "time": { "created": 1 } },
                    "parts": [{ "type": "text", "text": "Original request" }],
                },
                { "info": { "id": "internal", "role": "user", "time": { "created": 2 } }, "parts": internal },
            ]));
            rewind(&h).await.unwrap();
            let revert = h
                .host
                .http_calls()
                .into_iter()
                .find(|call| call.url.contains("/revert"))
                .unwrap();
            assert_eq!(body(&revert), json!({ "messageID": "visible_user" }));
        });
    }
}

// Cancel and stop lifecycle.

fn cancel(h: &Harness) -> smol::Task<anyhow::Result<()>> {
    let adapter = h.adapter.clone();
    smol::spawn(async move { adapter.cancel_turn(THREAD.into()).await })
}

#[test]
fn reports_a_failed_abort_and_kills_the_owned_server() {
    smol::block_on(async {
        let h = Harness::new();
        let done = h.start_turn().await;
        let handler = h.default_handler();
        h.host.respond_with(move |request| {
            if request.url.contains("/abort") {
                return (500, "Abort rejected".into());
            }
            handler(request)
        });
        let error = cancel(&h).await.unwrap_err();
        assert!(error.to_string().contains("Abort rejected"));
        done.await.unwrap();
        assert!(h.host.kills().contains(&THREAD.to_string()));
        assert!(h.events().contains(&HarnessEvent::SessionError {
            message: "Could not confirm OpenCode cancellation: Abort rejected".into()
        }));
    });
}

#[test]
fn kills_the_server_and_releases_the_turn_if_closing_its_stream_fails() {
    smol::block_on(async {
        let h = Harness::new();
        let done = h.start_turn().await;
        h.host.fail_next_sse_close("Stream close rejected");
        cancel(&h).await.unwrap();
        done.await.unwrap();
        assert!(h.host.kills().contains(&THREAD.to_string()));
    });
}

#[test]
fn cancels_a_resumed_session_during_startup_before_submitting_a_prompt() {
    smol::block_on(async {
        let h = Harness::new();
        h.adapter.bind_session(THREAD, ROOT, "/repo", None);
        let version = h.host.exec_once_later();
        let done = h.turn();
        wait_for("version lookup", || !h.host.exec_calls().is_empty()).await;
        let cancelled = cancel(&h);
        smol::Timer::after(Duration::from_millis(20)).await;
        version.send("opencode 1.14.19".into()).unwrap();
        cancelled.await.unwrap();
        done.await.unwrap();
        assert_eq!(h.prompts(), 0);
        assert!(h.host.kills().contains(&THREAD.to_string()));
    });
}

#[test]
fn opens_a_fresh_stream_after_cancellation_even_if_the_old_stream_ends() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        cancel(&h).await.unwrap();
        done.await.unwrap();
        h.sse_end();
        let next = h.turn();
        wait_for("replacement server", || h.host.spawns().len() == 2).await;
        wait_for("next prompt", || h.prompts() == 2).await;
        h.idle(ROOT);
        next.await.unwrap();
    });
}

#[test]
fn serializes_concurrent_startup_before_running_both_queued_prompts() {
    smol::block_on(async {
        let mut h = Harness::new();
        let first = h.turn();
        let second = h.turn();
        wait_for("first prompt", || h.prompts() == 1).await;
        assert_eq!(h.host.spawns().len(), 1);
        h.idle(ROOT);
        first.await.unwrap();
        wait_for("queued prompt", || h.prompts() == 2).await;
        h.idle(ROOT);
        second.await.unwrap();
        assert_eq!(h.host.spawns().len(), 1);
    });
}

#[test]
fn does_not_submit_queued_turns_compaction_or_rewind_after_cancellation() {
    smol::block_on(async {
        let h = Harness::new();
        let done = h.start_turn().await;
        let queued_turn = h.turn();
        let queued_compaction = compact(&h);
        let queued_rewind = rewind(&h);
        smol::Timer::after(Duration::from_millis(20)).await;
        cancel(&h).await.unwrap();
        done.await.unwrap();
        queued_turn.await.unwrap();
        queued_compaction.await.unwrap();
        assert!(!queued_rewind.await.unwrap().submitted);
        assert_eq!(h.prompts(), 1);
        assert!(
            !h.host
                .http_calls()
                .iter()
                .any(|call| call.url.contains("/summarize") || call.url.contains("/revert"))
        );
    });
}

#[test]
fn rejects_a_queued_operation_on_an_ended_stream_before_it_submits() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        let queued = h.turn();
        smol::Timer::after(Duration::from_millis(20)).await;
        h.sse_end();
        assert!(done.await.is_err());
        assert_eq!(
            queued.await.unwrap_err().to_string(),
            "OpenCode session ended before this operation could start. Retry the request."
        );
        assert_eq!(h.prompts(), 1);
    });
}

#[test]
fn does_not_carry_repeated_idle_cancellation_into_the_next_prompt() {
    smol::block_on(async {
        let mut h = Harness::new();
        let done = h.start_turn().await;
        cancel(&h).await.unwrap();
        done.await.unwrap();
        cancel(&h).await.unwrap();
        let next = h.turn();
        wait_for("next prompt", || h.prompts() == 2).await;
        assert!(!settled(&next).await);
        h.idle(ROOT);
        next.await.unwrap();
    });
}

#[test]
fn reports_the_current_projects_custom_model_context_window() {
    use monocode_core::harness::HarnessId;
    use monocode_core::models::AgentModel;
    smol::block_on(async {
        let mut h = Harness::new();
        let mut model = AgentModel::new("opencode:openai/review", HarnessId::Opencode, "Review");
        model.context_window = Some(100_000);
        h.catalog
            .set_harness_models(HarnessId::Opencode, vec![model.clone()]);
        model.context_window = Some(8_192);
        h.catalog
            .set_project_harness_models(HarnessId::Opencode, "/repo", vec![model]);
        let done = h.start_turn().await;
        let prompt = h.prompt_id().unwrap();
        h.emit_message(
            json!({
                "id": "context_reply", "role": "assistant", "parentID": prompt,
                "providerID": "openai", "modelID": "review",
                "tokens": { "input": 250, "output": 20, "cache": { "read": 0, "write": 0 } },
            }),
            json!([]),
        );
        h.wait_for_event("context", |event| {
            matches!(event, HarnessEvent::Context { .. })
        })
        .await;
        assert!(h.events().contains(&HarnessEvent::Context {
            used: Some(270),
            window: Some(8_192),
        }));
        h.idle(ROOT);
        done.await.unwrap();
    });
}

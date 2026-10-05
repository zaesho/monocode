//! Port of src/integrations/harness/providers/claude/claudeLive.test.ts.
//!
//! Despite its name, the TypeScript file never spawned Claude Code: it
//! replaced `../../core/child` with a mock and scripted the CLI's stream-json
//! lines. These tests do the same through a fake [`ClaudeChildIo`], so they
//! run by default. The real CLI is exercised by the `#[ignore]` test in
//! `live_tests.rs`.
//!
//! The TypeScript reduced events with `applyHarnessEvent` from a
//! `newSession("claude", "/repo")`; these use `monocode_core::reducer`. Its
//! checks on `groupTurnItems`, `foldableWork`, and `workSummaryLine`
//! (transcriptActivity.ts, not ported yet) are left out.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::FutureExt;
use futures::future::BoxFuture;
use monocode_core::block::{
    AgentStepKind, BlockRole, InterjectionStatus, ModelSettings, PlanStatus, TaskListItem,
    TaskListItemStatus, TaskListMeta, TurnIntent,
};
use monocode_core::harness::{HarnessId, RuntimeMode};
use monocode_core::harness_event::{
    ApprovalDecision, HarnessEvent, HarnessSessionInput, SendTurnInput,
};
use monocode_core::reducer::apply::apply_harness_event;
use monocode_core::session::Session;
use monocode_core::user_question::UserQuestionReply;
use parking_lot::Mutex;
use serde_json::{Value, json};

use crate::core::child::ChildAccount;
use crate::core::registry::EventSink;
use crate::core::task::SmolSpawner;

use super::io::{ClaudeChildIo, ExitHandler, LineHandler};
use super::session::{ClaudeSessionOptions, ClaudeSessions};

/// The TypeScript's 15 s grace, shortened so the tests do not need fake timers.
const GRACE: Duration = Duration::from_millis(300);

#[derive(Default)]
struct FakeState {
    sent: Vec<String>,
    spawned: Vec<Vec<String>>,
    on_line: Option<LineHandler>,
    on_exit: Option<ExitHandler>,
    reject_writes: VecDeque<String>,
    kills: usize,
    /// The working directory and account of every spawn.
    spawn_places: Vec<(String, Option<ChildAccount>)>,
    /// Holds binary resolution until the test sends or drops the gate.
    binary_gate: Option<async_channel::Receiver<()>>,
}

/// The `vi.mock("../../core/child")` of the TypeScript test.
#[derive(Default)]
struct FakeIo {
    state: Mutex<FakeState>,
}

impl ClaudeChildIo for FakeIo {
    fn resolve_claude_binary(&self) -> BoxFuture<'static, Result<String>> {
        let gate = self.state.lock().binary_gate.take();
        async move {
            if let Some(gate) = gate {
                let _ = gate.recv().await;
            }
            Ok("/fake/claude".to_string())
        }
        .boxed()
    }

    fn spawn_child(
        &self,
        _child_id: &str,
        _command: &str,
        args: Vec<String>,
        cwd: &str,
        account: Option<ChildAccount>,
    ) -> BoxFuture<'static, Result<()>> {
        let mut state = self.state.lock();
        state.spawned.push(args);
        state.spawn_places.push((cwd.to_string(), account));
        async { Ok(()) }.boxed()
    }

    fn watch_child(&self, _child_id: &str, on_line: LineHandler, on_exit: ExitHandler) {
        let mut state = self.state.lock();
        state.on_line = Some(on_line);
        state.on_exit = Some(on_exit);
    }

    fn unwatch_child(&self, _child_id: &str) {}

    fn write_child(&self, _child_id: &str, line: String) -> BoxFuture<'static, Result<()>> {
        let mut state = self.state.lock();
        if let Some(error) = state.reject_writes.pop_front() {
            return async move { Err(anyhow!(error)) }.boxed();
        }
        state.sent.push(line);
        async { Ok(()) }.boxed()
    }

    fn kill_child(&self, _child_id: &str) -> BoxFuture<'static, Result<()>> {
        self.state.lock().kills += 1;
        async { Ok(()) }.boxed()
    }

    fn exec_child(
        &self,
        _command: &str,
        _args: Vec<String>,
        _cwd: Option<&str>,
    ) -> BoxFuture<'static, Result<String>> {
        async { Ok("2.1.300 (Claude Code)".to_string()) }.boxed()
    }

    fn home_dir(&self) -> BoxFuture<'static, Result<String>> {
        async { Ok("/home/user".to_string()) }.boxed()
    }
}

#[derive(Clone, Default)]
struct Events(Arc<Mutex<Vec<HarnessEvent>>>);

impl Events {
    fn sink(&self) -> EventSink {
        let events = self.0.clone();
        Arc::new(move |event| events.lock().push(event))
    }

    fn all(&self) -> Vec<HarnessEvent> {
        self.0.lock().clone()
    }

    fn any(&self, pred: impl Fn(&HarnessEvent) -> bool) -> bool {
        self.0.lock().iter().any(pred)
    }

    fn extend(&self, other: &Events) {
        let mut copied = other.all();
        self.0.lock().append(&mut copied);
    }

    /// `events.reduce(applyHarnessEvent, newSession("claude", "/repo"))`.
    fn reduce(&self) -> Session {
        let mut session = Session::blank("s", HarnessId::Claude, "claude:sonnet-5", "/repo");
        for event in self.all() {
            session = apply_harness_event(&session, &event);
        }
        session
    }

    fn background_updates(&self) -> Vec<Vec<String>> {
        self.all()
            .into_iter()
            .filter_map(|event| match event {
                HarnessEvent::BackgroundUpdated { tasks } => Some(tasks),
                _ => None,
            })
            .collect()
    }

    fn last_task_items(&self) -> Option<Vec<TaskListItem>> {
        self.reduce()
            .blocks
            .iter()
            .rev()
            .find(|block| block.role == BlockRole::Tasks)
            .and_then(|block| block.task_list.as_ref())
            .map(|list| list.items.clone())
    }
}

#[derive(Default)]
struct TurnOptions {
    runtime_mode: Option<RuntimeMode>,
    intent: Option<TurnIntent>,
    provider_account_id: Option<String>,
    model: Option<&'static str>,
    text: Option<&'static str>,
}

type Turn = smol::Task<Result<()>>;

struct Harness {
    io: Arc<FakeIo>,
    sessions: ClaudeSessions,
}

impl Harness {
    fn new() -> Self {
        let io = Arc::new(FakeIo::default());
        let options = ClaudeSessionOptions {
            init_timeout: Duration::from_secs(2),
            resume_grace: GRACE,
            ..Default::default()
        };
        let sessions = ClaudeSessions::new(io.clone(), Arc::new(SmolSpawner), options);
        Self { io, sessions }
    }

    fn emit(&self, rec: Value) {
        let on_line = self
            .io
            .state
            .lock()
            .on_line
            .clone()
            .expect("no watched child");
        on_line(rec.to_string());
    }

    fn exit(&self, code: Option<i64>) {
        let on_exit = self
            .io
            .state
            .lock()
            .on_exit
            .clone()
            .expect("no watched child");
        on_exit(code);
    }

    fn parse(&self) -> Vec<Value> {
        self.io
            .state
            .lock()
            .sent
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn clear_sent(&self) {
        self.io.state.lock().sent.clear();
    }

    fn spawned(&self) -> Vec<Vec<String>> {
        self.io.state.lock().spawned.clone()
    }

    fn reject_next_write(&self, error: &str) {
        self.io.state.lock().reject_writes.push_back(error.into());
    }

    fn user_count(&self) -> usize {
        self.parse().iter().filter(|m| m["type"] == "user").count()
    }

    fn response_for(&self, request_id: &str) -> Option<Value> {
        self.parse()
            .into_iter()
            .find(|m| m["response"]["request_id"] == request_id)
    }

    fn wait_for(&self, pred: impl Fn() -> bool, label: &str) {
        for _ in 0..400 {
            if pred() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("timed out waiting for {label}; sent={:?}", self.parse());
    }

    fn send(&self, session_id: &str, options: TurnOptions) -> (Events, Turn) {
        let events = Events::default();
        let turn = self.send_with(session_id, options, events.sink());
        (events, turn)
    }

    fn send_with(&self, session_id: &str, options: TurnOptions, sink: EventSink) -> Turn {
        let model_settings = ModelSettings::new();
        let input = SendTurnInput {
            session: HarnessSessionInput {
                session_id: session_id.into(),
                cwd: "/repo".into(),
                model: options.model.unwrap_or("claude:claude-sonnet-5").into(),
                model_settings: Some(model_settings),
                provider_account_id: options.provider_account_id,
                runtime_mode: options.runtime_mode.unwrap_or(RuntimeMode::Supervised),
                intent: options.intent,
                controls_agents: None,
                app_access: None,
            },
            text: options.text.unwrap_or("explore the codebase").into(),
            attachments: Some(Vec::new()),
        };
        let sessions = self.sessions.clone();
        smol::spawn(async move { sessions.send_turn(input, sink).await })
    }

    /// `startTurn`: a first turn through the initialize handshake.
    fn start_turn(&self, session_id: &str, options: TurnOptions) -> (Events, Turn) {
        let (events, turn) = self.send(session_id, options);
        self.wait_for(
            || {
                self.parse()
                    .iter()
                    .any(|m| m["request"]["subtype"] == "initialize")
            },
            "initialize",
        );
        self.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
        self.emit(json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": "monocode_2" },
        }));
        self.wait_for(|| self.user_count() > 0, "user prompt");
        (events, turn)
    }

    /// Acknowledge the latest process's `initialize` once it has asked.
    fn ack_init(&self) {
        let processes = self.spawned().len();
        self.wait_for(
            || {
                self.parse()
                    .iter()
                    .filter(|m| m["request"]["subtype"] == "initialize")
                    .count()
                    >= processes
            },
            "initialize",
        );
        self.emit(json!({
            "type": "control_response",
            "response": { "subtype": "success", "request_id": "monocode_2" },
        }));
    }

    /// `restartedTurn`: a later turn that must launch a new Claude process.
    fn restarted_turn(&self, options: TurnOptions, provider_session_id: &str) -> (Events, Turn) {
        let spawn_count = self.spawned().len();
        let user_count = self.user_count();
        let (events, turn) = self.send(
            "s1",
            TurnOptions {
                text: Some("finish it"),
                ..options
            },
        );
        self.wait_for(
            || self.spawned().len() == spawn_count + 1,
            "replacement Claude process",
        );
        self.emit(
            json!({ "type": "system", "subtype": "init", "session_id": provider_session_id }),
        );
        self.ack_init();
        self.wait_for(|| self.user_count() > user_count, "follow-up prompt");
        (events, turn)
    }

    /// `emitFollowUpTurn`: what Claude streams when a finished task wakes it
    /// for another turn.
    fn emit_follow_up_turn(&self, text: &str) {
        self.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
        self.emit(json!({
            "type": "stream_event",
            "session_id": "sess_1",
            "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } },
        }));
        self.emit(json!({
            "type": "assistant",
            "session_id": "sess_1",
            "message": { "content": [{ "type": "text", "text": text }] },
        }));
        self.result("sess_1");
    }

    fn result(&self, session_id: &str) {
        self.emit(json!({ "type": "result", "subtype": "success", "session_id": session_id }));
    }

    /// `emitInlineSubagent`: a subagent Claude ran inline; its report comes
    /// back on the parent's stream.
    fn emit_inline_subagent(&self) {
        self.emit(json!({
            "type": "assistant",
            "session_id": "sess_1",
            "message": { "content": [{
                "type": "tool_use",
                "id": "toolu_agent",
                "name": "Task",
                "input": { "description": "Explore the auth module", "subagent_type": "explore" },
            }] },
        }));
        self.emit(json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": "t1",
            "tool_use_id": "toolu_agent",
            "description": "Explore the auth module",
            "task_type": "local_agent",
        }));
        self.emit(json!({
            "type": "user",
            "session_id": "sess_1",
            "message": { "content": [{
                "type": "tool_result", "tool_use_id": "toolu_agent", "content": "Auth lives in src/auth.",
            }] },
        }));
    }

    fn emit_background_bash_launch(&self, task_id: &str) {
        self.emit(json!({
            "type": "assistant",
            "session_id": "sess_1",
            "message": { "content": [{
                "type": "tool_use",
                "id": "toolu_bash",
                "name": "Bash",
                "input": { "command": "sleep 30 && echo done", "run_in_background": true },
            }] },
        }));
        self.emit(json!({
            "type": "system",
            "subtype": "background_tasks_changed",
            "tasks": [{ "task_id": task_id, "task_type": "local_bash", "description": "Wait 30 seconds then print done" }],
        }));
        self.emit(json!({
            "type": "system",
            "subtype": "task_started",
            "task_id": task_id,
            "tool_use_id": "toolu_bash",
            "description": "Wait 30 seconds then print done",
            "task_type": "local_bash",
        }));
        self.emit(json!({
            "type": "user",
            "session_id": "sess_1",
            "message": { "content": [{
                "type": "tool_result",
                "tool_use_id": "toolu_bash",
                "content": format!("Command running in background with ID: {task_id}"),
            }] },
        }));
    }

    fn emit_background_bash(&self, task_id: &str) {
        self.emit_background_bash_launch(task_id);
        self.emit(json!({
            "type": "stream_event",
            "session_id": "sess_1",
            "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "waiting" } },
        }));
        self.emit(json!({
            "type": "assistant",
            "session_id": "sess_1",
            "message": { "content": [{ "type": "text", "text": "waiting" }] },
        }));
        self.result("sess_1");
    }

    fn emit_bash_finished(&self, task_id: &str) {
        self.emit(json!({ "type": "system", "subtype": "background_tasks_changed", "tasks": [] }));
        self.emit(json!({
            "type": "system", "subtype": "task_updated", "task_id": task_id, "patch": { "status": "completed" },
        }));
        self.emit(json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": task_id,
            "tool_use_id": "toolu_bash",
            "status": "completed",
            "summary": "Background command \"sleep 30 && echo done\" completed (exit code 0)",
        }));
    }

    fn emit_task_tool(
        &self,
        id: &str,
        name: &str,
        input: Value,
        result: &str,
        provider_session_id: &str,
    ) {
        self.emit(json!({
            "type": "assistant",
            "session_id": provider_session_id,
            "message": { "content": [{ "type": "tool_use", "id": id, "name": name, "input": input }] },
        }));
        self.emit(json!({
            "type": "user",
            "session_id": provider_session_id,
            "message": { "content": [{ "type": "tool_result", "tool_use_id": id, "content": result }] },
        }));
    }
}

/// `await turn`, with a deadline so a hung turn fails the test.
fn finish(turn: Turn) -> Result<()> {
    smol::block_on(async {
        crate::core::task::timeout(Duration::from_secs(5), turn)
            .await
            .expect("turn did not settle")
    })
}

fn settles_within(turn: &Turn, wait: Duration) -> bool {
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if turn.is_finished() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    turn.is_finished()
}

fn item(id: &str, text: &str, status: TaskListItemStatus) -> TaskListItem {
    TaskListItem {
        id: Some(id.into()),
        text: text.into(),
        status,
        extra: Default::default(),
    }
}

fn contains_all(args: &[String], items: &[&str]) -> bool {
    items.iter().all(|item| args.iter().any(|arg| arg == item))
}

fn tasks_updates(events: &Events) -> Vec<HarnessEvent> {
    events
        .all()
        .into_iter()
        .filter(|event| matches!(event, HarnessEvent::TasksUpdated { .. }))
        .collect()
}

// describe("claude streamed tool inputs")

#[test]
fn replaces_an_empty_shell_row_with_the_complete_assistant_tool_input() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "stream_event",
        "session_id": "sess_1",
        "event": {
            "type": "content_block_start",
            "index": 0,
            "content_block": { "type": "tool_use", "id": "toolu_shell", "name": "Bash", "input": {} },
        },
    }));
    h.emit(json!({
        "type": "stream_event",
        "session_id": "sess_1",
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "input_json_delta", "partial_json": "{\"command\":\"git status" },
        },
    }));
    h.emit(json!({
        "type": "assistant",
        "session_id": "sess_1",
        "message": { "content": [{
            "type": "tool_use",
            "id": "toolu_shell",
            "name": "Bash",
            "input": { "command": "git status --short", "description": "Check changes" },
        }] },
    }));
    h.emit(json!({
        "type": "user",
        "session_id": "sess_1",
        "message": { "content": [{ "type": "tool_result", "tool_use_id": "toolu_shell", "content": "clean" }] },
    }));
    h.result("sess_1");
    finish(turn).unwrap();

    assert!(events.any(|event| matches!(
        event,
        HarnessEvent::ToolUpdated { call_id, title: Some(title), status: Some(status), .. }
            if call_id == "toolu_shell" && title == "git status --short" && status == "pending"
    )));
    let session = events.reduce();
    let tool = session
        .blocks
        .iter()
        .find(|block| {
            block.tool.as_ref().and_then(|tool| tool.call_id.as_deref()) == Some("toolu_shell")
        })
        .unwrap();
    assert_eq!(tool.text, "git status --short");
    assert_eq!(
        tool.tool.as_ref().unwrap().status.as_deref(),
        Some("completed")
    );
}

// describe("claude task tools")

#[test]
fn builds_the_task_list_from_task_create_and_task_update_not_subagent_rows() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_task_tool(
        "toolu_c1",
        "TaskCreate",
        json!({ "subject": "Write tests", "description": "Cover the parser" }),
        "Task #1 created successfully: Write tests",
        "sess_1",
    );
    h.emit_task_tool(
        "toolu_c2",
        "TaskCreate",
        json!({ "subject": "Ship it", "description": "Open the PR" }),
        "Task #2 created successfully: Ship it",
        "sess_1",
    );
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "1", "status": "in_progress" }),
        "Updated task #1 status",
        "sess_1",
    );
    h.emit_task_tool(
        "toolu_u2",
        "TaskUpdate",
        json!({ "taskId": "1", "status": "completed" }),
        "Updated task #1 status",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();

    let session = events.reduce();
    let lists: Vec<_> = session
        .blocks
        .iter()
        .filter(|block| block.role == BlockRole::Tasks)
        .collect();
    assert_eq!(lists.len(), 1);
    assert_eq!(
        lists[0].task_list.as_ref().unwrap().items,
        [
            item("1", "Write tests", TaskListItemStatus::Completed),
            item("2", "Ship it", TaskListItemStatus::Pending),
        ]
    );
    assert!(!session.blocks.iter().any(|block| {
        block.tool.as_ref().and_then(|tool| tool.kind.as_deref()) == Some("agent")
            || block.agent_run.is_some()
    }));
}

#[test]
fn keeps_earlier_tasks_updatable_after_a_restart_resumes_the_conversation() {
    let h = Harness::new();
    let (first, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_task_tool(
        "toolu_c1",
        "TaskCreate",
        json!({ "subject": "Write tests", "description": "Cover the parser" }),
        "Task #1 created successfully: Write tests",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();

    let events = Events::default();
    events.extend(&first);
    let (second, turn) = h.restarted_turn(
        TurnOptions {
            model: Some("claude:opus-5"),
            ..Default::default()
        },
        "sess_1",
    );
    assert!(contains_all(&h.spawned()[1], &["--resume", "sess_1"]));
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "1", "status": "completed" }),
        "Updated task #1 status",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    events.extend(&second);

    assert_eq!(
        events.last_task_items(),
        Some(vec![item(
            "1",
            "Write tests",
            TaskListItemStatus::Completed
        )])
    );
}

#[test]
fn shows_a_task_update_subject_rename_in_the_panel() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_task_tool(
        "toolu_c1",
        "TaskCreate",
        json!({ "subject": "Write tests", "description": "Cover the parser" }),
        "Task #1 created successfully: Write tests",
        "sess_1",
    );
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "1", "subject": "Write parser tests", "status": "in_progress" }),
        "Updated task #1 subject, status",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();

    assert_eq!(
        events.last_task_items(),
        Some(vec![item(
            "1",
            "Write parser tests",
            TaskListItemStatus::InProgress
        )])
    );
}

#[test]
fn keeps_earlier_tasks_across_a_plan_to_build_restart() {
    let h = Harness::new();
    let (first, turn) = h.start_turn(
        "s1",
        TurnOptions {
            intent: Some(TurnIntent::Plan),
            ..Default::default()
        },
    );
    h.emit_task_tool(
        "toolu_c1",
        "TaskCreate",
        json!({ "subject": "Write tests" }),
        "Task #1 created successfully: Write tests",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();

    let events = Events::default();
    events.extend(&first);
    let (second, turn) = h.restarted_turn(
        TurnOptions {
            intent: Some(TurnIntent::Build),
            ..Default::default()
        },
        "sess_1",
    );
    assert!(contains_all(&h.spawned()[1], &["--resume", "sess_1"]));
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "1", "status": "completed" }),
        "Updated task #1 status",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    events.extend(&second);

    assert_eq!(
        events.last_task_items(),
        Some(vec![item(
            "1",
            "Write tests",
            TaskListItemStatus::Completed
        )])
    );
}

#[test]
fn keeps_earlier_tasks_after_the_claude_child_exits() {
    let h = Harness::new();
    let (first, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_task_tool(
        "toolu_c1",
        "TaskCreate",
        json!({ "subject": "Write tests" }),
        "Task #1 created successfully: Write tests",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    h.exit(Some(1));

    let events = Events::default();
    events.extend(&first);
    let (second, turn) = h.restarted_turn(TurnOptions::default(), "sess_1");
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "1", "status": "completed" }),
        "Updated task #1 status",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    events.extend(&second);

    assert_eq!(
        events.last_task_items(),
        Some(vec![item(
            "1",
            "Write tests",
            TaskListItemStatus::Completed
        )])
    );
}

fn task_lists(session: &Session) -> Vec<TaskListMeta> {
    session
        .blocks
        .iter()
        .filter_map(|block| block.task_list.clone())
        .collect()
}

#[test]
fn rehydrates_tasks_from_the_persisted_panel_after_an_app_restart() {
    let h = Harness::new();
    let (first, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_task_tool(
        "toolu_c1",
        "TaskCreate",
        json!({ "subject": "Write tests" }),
        "Task #1 created successfully: Write tests",
        "sess_1",
    );
    h.emit_task_tool(
        "toolu_c2",
        "TaskCreate",
        json!({ "subject": "Ship it" }),
        "Task #2 created successfully: Ship it",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    let restored = first.reduce();

    // App restart: all module state is gone; only the saved transcript remains.
    smol::block_on(h.sessions.stop_session("s1")).unwrap();
    h.sessions.reset_for_tests();
    h.sessions.bind_session("s1", "sess_1", "/repo", None);
    h.sessions.restore_task_lists("s1", &task_lists(&restored));

    let events = Events::default();
    events.extend(&first);
    let (second, turn) = h.restarted_turn(TurnOptions::default(), "sess_1");
    assert!(contains_all(
        h.spawned().last().unwrap(),
        &["--resume", "sess_1"]
    ));
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "1", "status": "completed" }),
        "Updated task #1 status",
        "sess_1",
    );
    h.emit_task_tool(
        "toolu_c3",
        "TaskCreate",
        json!({ "subject": "Tag release" }),
        "Task #3 created successfully: Tag release",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    events.extend(&second);

    assert_eq!(
        events.last_task_items(),
        Some(vec![
            item("1", "Write tests", TaskListItemStatus::Completed),
            item("2", "Ship it", TaskListItemStatus::Pending),
            item("3", "Tag release", TaskListItemStatus::Pending),
        ])
    );
}

/// Conversation A: tasks #1 and #2 in sess_1, reduced like the saved transcript.
fn conversation_with_tasks(h: &Harness) -> Session {
    let (first, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_task_tool(
        "toolu_c1",
        "TaskCreate",
        json!({ "subject": "Write tests" }),
        "Task #1 created successfully: Write tests",
        "sess_1",
    );
    h.emit_task_tool(
        "toolu_c2",
        "TaskCreate",
        json!({ "subject": "Ship it" }),
        "Task #2 created successfully: Ship it",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    first.reduce()
}

/// Conversation B updates A's #1, then creates its own #1.
fn emit_conversation_b(h: &Harness) {
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "1", "status": "completed" }),
        "Updated task #1 status",
        "sess_2",
    );
    h.emit_task_tool(
        "toolu_c3",
        "TaskCreate",
        json!({ "subject": "Fresh task" }),
        "Task #1 created successfully: Fresh task",
        "sess_2",
    );
    h.result("sess_2");
}

fn is_fresh_sess_2_list(event: &HarnessEvent) -> bool {
    matches!(
        event,
        HarnessEvent::TasksUpdated { provider_session_id: Some(id), items, .. }
            if id == "sess_2" && items == &[item("1", "Fresh task", TaskListItemStatus::Pending)]
    )
}

#[test]
fn rehydrates_the_bound_conversations_panel_past_a_later_conversations_panel() {
    let h = Harness::new();
    let restored = conversation_with_tasks(&h);
    let mut lists = task_lists(&restored);
    lists.push(TaskListMeta {
        key: Some("claude-tasks".into()),
        provider_session_id: Some("sess_2".into()),
        explanation: None,
        items: vec![item("1", "Other conversation", TaskListItemStatus::Pending)],
        extra: Default::default(),
    });

    smol::block_on(h.sessions.stop_session("s1")).unwrap();
    h.sessions.reset_for_tests();
    h.sessions.bind_session("s1", "sess_1", "/repo", None);
    h.sessions.restore_task_lists("s1", &lists);

    let (events, turn) = h.restarted_turn(TurnOptions::default(), "sess_1");
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "2", "status": "completed" }),
        "Updated task #2 status",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();

    assert_eq!(
        events.last_task_items(),
        Some(vec![
            item("1", "Write tests", TaskListItemStatus::Pending),
            item("2", "Ship it", TaskListItemStatus::Completed),
        ])
    );
}

#[test]
fn does_not_carry_tasks_to_another_conversation_bound_to_the_same_thread() {
    let h = Harness::new();
    let restored = conversation_with_tasks(&h);
    let first_list = restored
        .blocks
        .iter()
        .find(|block| block.role == BlockRole::Tasks)
        .and_then(|block| block.task_list.as_ref())
        .unwrap();
    assert_eq!(first_list.provider_session_id.as_deref(), Some("sess_1"));

    smol::block_on(h.sessions.stop_session("s1")).unwrap();
    h.sessions.bind_session("s1", "sess_2", "/repo", None);
    h.sessions.restore_task_lists("s1", &task_lists(&restored));

    let (events, turn) = h.restarted_turn(TurnOptions::default(), "sess_2");
    assert!(contains_all(
        h.spawned().last().unwrap(),
        &["--resume", "sess_2"]
    ));
    emit_conversation_b(&h);
    finish(turn).unwrap();

    let updates = tasks_updates(&events);
    assert_eq!(updates.len(), 1);
    assert!(is_fresh_sess_2_list(&updates[0]));
}

#[test]
fn starts_a_clean_task_map_when_claude_reports_a_different_conversation() {
    let h = Harness::new();
    conversation_with_tasks(&h);

    let (events, turn) = h.send(
        "s1",
        TurnOptions {
            text: Some("keep going"),
            ..Default::default()
        },
    );
    h.wait_for(|| h.user_count() > 1, "follow-up prompt");
    emit_conversation_b(&h);
    finish(turn).unwrap();

    assert!(is_fresh_sess_2_list(tasks_updates(&events).last().unwrap()));
}

#[test]
fn drops_the_task_map_when_the_conversation_cannot_resume() {
    let h = Harness::new();
    let (_, turn) = h.start_turn(
        "s1",
        TurnOptions {
            provider_account_id: Some("work".into()),
            ..Default::default()
        },
    );
    h.emit_task_tool(
        "toolu_c1",
        "TaskCreate",
        json!({ "subject": "Write tests" }),
        "Task #1 created successfully: Write tests",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();

    let (events, turn) = h.restarted_turn(
        TurnOptions {
            provider_account_id: Some("home".into()),
            ..Default::default()
        },
        "sess_1",
    );
    assert!(
        !h.spawned()
            .last()
            .unwrap()
            .iter()
            .any(|arg| arg == "--resume")
    );
    h.emit_task_tool(
        "toolu_u1",
        "TaskUpdate",
        json!({ "taskId": "1", "status": "completed" }),
        "Updated task #1 status",
        "sess_1",
    );
    h.result("sess_1");
    finish(turn).unwrap();

    assert!(tasks_updates(&events).is_empty());
}

// describe("claude assistant message boundaries")

#[test]
fn keeps_a_follow_up_paragraph_separate_and_does_not_replay_its_snapshot() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    let progress = "- update the notes and commit";
    let update = "Connect returned an empty file for one image on one post. The catch-up skips it and carries on, and I'll include it in the final tally.";
    for text in [progress, update] {
        h.emit(json!({
            "type": "stream_event",
            "session_id": "sess_1",
            "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } },
        }));
        h.emit(json!({
            "type": "assistant",
            "session_id": "sess_1",
            "message": { "content": [{ "type": "text", "text": text }] },
        }));
    }
    h.result("sess_1");
    finish(turn).unwrap();

    let texts: Vec<String> = events
        .reduce()
        .blocks
        .iter()
        .map(|block| block.text.clone())
        .collect();
    assert_eq!(texts, [progress, update]);
    let deltas: Vec<HarnessEvent> = events
        .all()
        .into_iter()
        .filter(|event| matches!(event, HarnessEvent::MessageDelta { .. }))
        .collect();
    assert_eq!(
        deltas,
        [
            HarnessEvent::MessageDelta {
                text: progress.into(),
                append: Some(true),
            },
            HarnessEvent::MessageDelta {
                text: update.into(),
                append: Some(true),
            },
        ]
    );
}

// describe("claude model switching")

#[test]
fn restarts_a_named_account_with_the_new_model_while_resuming_the_provider_conversation() {
    let h = Harness::new();
    let (_, turn) = h.start_turn(
        "s1",
        TurnOptions {
            provider_account_id: Some("account-work".into()),
            ..Default::default()
        },
    );
    h.result("sess_1");
    finish(turn).unwrap();

    let user_count = h.user_count();
    let (_, second) = h.send(
        "s1",
        TurnOptions {
            model: Some("claude:opus-5"),
            provider_account_id: Some("account-work".into()),
            text: Some("what did I ask before?"),
            ..Default::default()
        },
    );
    h.wait_for(|| h.spawned().len() == 2, "replacement Claude process");
    let args = &h.spawned()[1];
    assert!(contains_all(
        args,
        &["--model", "claude-opus-5", "--resume", "sess_1"]
    ));
    assert!(!args.iter().any(|arg| arg == "--session-id"));

    h.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
    h.ack_init();
    h.wait_for(|| h.user_count() > user_count, "follow-up prompt");
    h.result("sess_1");
    finish(second).unwrap();
}

// describe("claude conversation binding")

fn provider_bindings(events: &Events) -> Vec<String> {
    events
        .all()
        .into_iter()
        .filter_map(|event| match event {
            HarnessEvent::SessionProviderBound {
                provider_session_id,
            } => Some(provider_session_id),
            _ => None,
        })
        .collect()
}

fn has_arg(args: &[String], arg: &str) -> bool {
    args.iter().any(|item| item == arg)
}

/// Answer the replacement process's initialize and finish its turn.
fn finish_replacement_turn(h: &Harness, turn: Turn, user_count: usize, provider_session_id: &str) {
    h.ack_init();
    h.wait_for(|| h.user_count() == user_count, "retried prompt");
    h.result(provider_session_id);
    finish(turn).unwrap();
}

#[test]
fn does_not_bind_a_new_conversation_until_claude_saves_the_first_prompt() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    assert!(provider_bindings(&events).is_empty());
    smol::block_on(h.sessions.cancel_turn("s1")).unwrap();
    finish(turn).unwrap();
    h.exit(Some(1));

    let (_, turn) = h.send(
        "s1",
        TurnOptions {
            text: Some("try again"),
            ..Default::default()
        },
    );
    h.wait_for(|| h.spawned().len() == 2, "replacement Claude process");
    let args = &h.spawned()[1];
    assert!(!has_arg(args, "--resume"));
    assert!(has_arg(args, "--session-id"));
    finish_replacement_turn(&h, turn, 2, "sess_1");
}

#[test]
fn binds_the_conversation_once_claude_streams_the_reply() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "stream_event",
        "session_id": "sess_1",
        "event": {
            "type": "content_block_delta",
            "index": 0,
            "delta": { "type": "text_delta", "text": "hi" },
        },
    }));
    assert_eq!(provider_bindings(&events), vec!["sess_1".to_string()]);
    h.result("sess_1");
    finish(turn).unwrap();
}

fn missing_conversation_result() -> Value {
    json!({
        "type": "result",
        "subtype": "error_during_execution",
        "is_error": true,
        "session_id": "gone",
        "errors": ["No conversation found with session ID: gone"],
    })
}

#[test]
fn starts_a_new_conversation_when_the_saved_one_does_not_exist() {
    let h = Harness::new();
    h.sessions.bind_session("s1", "gone", "/repo", None);
    let (events, turn) = h.send(
        "s1",
        TurnOptions {
            text: Some("keep going"),
            ..Default::default()
        },
    );
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request"]["subtype"] == "initialize")
        },
        "resumed Claude process",
    );
    assert!(contains_all(&h.spawned()[0], &["--resume", "gone"]));
    h.emit(missing_conversation_result());
    h.exit(Some(1));

    h.wait_for(|| h.spawned().len() == 2, "replacement Claude process");
    let args = &h.spawned()[1];
    assert!(!has_arg(args, "--resume"));
    assert!(has_arg(args, "--session-id"));
    finish_replacement_turn(&h, turn, 1, "sess_2");

    assert!(!events.any(|event| matches!(event, HarnessEvent::SessionError { .. })));
    assert!(!events.any(|event| matches!(event, HarnessEvent::SessionEnded { .. })));
    assert!(events.any(|event| matches!(
        event,
        HarnessEvent::Status { text } if text.contains("no saved conversation")
    )));
    assert_eq!(provider_bindings(&events), vec!["sess_2".to_string()]);
}

// describe("claude missing conversation during a turn")

#[test]
fn fails_the_turn_and_starts_a_new_conversation_next_time() {
    let h = Harness::new();
    h.sessions.bind_session("s1", "gone", "/repo", None);
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    assert!(contains_all(&h.spawned()[0], &["--resume", "gone"]));
    h.emit(missing_conversation_result());
    let _ = finish(turn);
    assert!(events.all().contains(&HarnessEvent::SessionError {
        message: "No conversation found with session ID: gone".into(),
    }));
    h.exit(Some(1));

    let (_, turn) = h.send(
        "s1",
        TurnOptions {
            text: Some("again"),
            ..Default::default()
        },
    );
    h.wait_for(|| h.spawned().len() == 2, "replacement Claude process");
    assert!(!has_arg(&h.spawned()[1], "--resume"));
    finish_replacement_turn(&h, turn, 2, "sess_2");
}

// describe("claude legacy account resume")

#[test]
fn resumes_a_legacy_thread_when_the_missing_account_resolves_to_default() {
    let h = Harness::new();
    h.sessions
        .bind_session("s1", "legacy-session", "/repo", None);
    let (_, turn) = h.start_turn(
        "s1",
        TurnOptions {
            provider_account_id: Some("default".into()),
            ..Default::default()
        },
    );
    let args = &h.spawned()[0];
    assert!(contains_all(args, &["--resume", "legacy-session"]));
    assert!(!args.iter().any(|arg| arg == "--session-id"));
    h.result("legacy-session");
    finish(turn).unwrap();
}

#[test]
fn does_not_resume_a_legacy_default_thread_under_a_named_account() {
    let h = Harness::new();
    h.sessions
        .bind_session("s1", "legacy-session", "/repo", None);
    let (_, turn) = h.start_turn(
        "s1",
        TurnOptions {
            provider_account_id: Some("account-work".into()),
            ..Default::default()
        },
    );
    let args = &h.spawned()[0];
    assert!(!args.iter().any(|arg| arg == "--resume"));
    assert!(args.iter().any(|arg| arg == "--session-id"));
    h.result("sess_1");
    finish(turn).unwrap();
}

// describe("claude subagents")

fn approval_request_id(events: &Events) -> (i64, Option<String>) {
    events
        .all()
        .into_iter()
        .find_map(|event| match event {
            HarnessEvent::ApprovalRequested {
                request_id,
                call_id,
                ..
            } => Some((request_id, call_id)),
            _ => None,
        })
        .expect("no approval.requested")
}

#[test]
fn routes_a_child_permission_decision() {
    for decision in [ApprovalDecision::Allow, ApprovalDecision::Deny] {
        let h = Harness::new();
        let (events, turn) = h.start_turn("s1", TurnOptions::default());
        h.emit(json!({
            "type": "control_request",
            "request_id": "child_permission",
            "session_id": "sess_child",
            "parent_tool_use_id": "toolu_agent",
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "Read",
                "tool_use_id": "child_read",
                "input": { "file_path": "/home/user/.gitconfig" },
            },
        }));
        let (request_id, call_id) = approval_request_id(&events);
        assert_eq!(call_id.as_deref(), Some("child_read"));
        h.sessions.respond_approval("s1", request_id, decision);
        h.wait_for(
            || h.response_for("child_permission").is_some(),
            "child decision",
        );
        let response = h.response_for("child_permission").unwrap();
        assert_eq!(response["type"], "control_response");
        let expected = match decision {
            ApprovalDecision::Allow => "allow",
            ApprovalDecision::Deny => "deny",
        };
        assert_eq!(response["response"]["response"]["behavior"], expected);
        // A new conversation binds once Claude saves it, here on the result.
        h.result("sess_1");
        finish(turn).unwrap();
        let bound = events
            .all()
            .into_iter()
            .rev()
            .find_map(|event| match event {
                HarnessEvent::SessionProviderBound {
                    provider_session_id,
                } => Some(provider_session_id),
                _ => None,
            });
        assert_eq!(bound.as_deref(), Some("sess_1"));
    }
}

fn emit_child_questions(h: &Harness, prompt: impl Fn(&str) -> String) {
    for id in ["child_a", "child_b"] {
        h.emit(json!({
            "type": "control_request",
            "request_id": id,
            "parent_tool_use_id": format!("agent_{id}"),
            "request": {
                "subtype": "can_use_tool",
                "tool_name": "AskUserQuestion",
                "input": { "questions": [{ "question": prompt(id), "options": [{ "label": "Proceed" }] }] },
            },
        }));
    }
}

#[test]
fn keeps_simultaneous_child_questions_reachable_in_the_single_question_ui() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    emit_child_questions(&h, |id| format!("Question from {id}"));
    let asked = events
        .all()
        .into_iter()
        .filter(|event| matches!(event, HarnessEvent::QuestionAsked { .. }))
        .count();
    assert_eq!(asked, 1);
    for id in ["child_a", "child_b"] {
        h.wait_for(
            || {
                events.reduce().pending_question.is_some_and(|request| {
                    request.questions[0].prompt == format!("Question from {id}")
                })
            },
            "pending question",
        );
        let request = events.reduce().pending_question.unwrap();
        let question = &request.questions[0];
        let reply = if id == "child_a" {
            UserQuestionReply::Answered {
                answers: std::collections::BTreeMap::from([(
                    question.id.clone(),
                    vec![question.options[0].id.clone()],
                )]),
                custom: None,
            }
        } else {
            UserQuestionReply::Skipped
        };
        h.sessions.respond_question("s1", request.request_id, reply);
        h.wait_for(|| h.response_for(id).is_some(), "question response");
        let behavior = if id == "child_a" { "allow" } else { "deny" };
        assert_eq!(
            h.response_for(id).unwrap()["response"]["response"]["behavior"],
            behavior
        );
    }
    h.wait_for(
        || events.reduce().pending_question.is_none(),
        "no pending question",
    );
    h.result("sess_1");
    finish(turn).unwrap();
}

#[test]
fn preserves_the_remaining_question_when_one_is_cancelled_by_the_server() {
    for cancelled in ["child_a", "child_b"] {
        let h = Harness::new();
        let (events, turn) = h.start_turn("s1", TurnOptions::default());
        emit_child_questions(&h, str::to_string);
        h.emit(json!({ "type": "control_cancel_request", "request_id": cancelled }));
        h.wait_for(
            || events.any(|event| matches!(event, HarnessEvent::QuestionResolved { .. })),
            "cancelled question",
        );
        let remaining = if cancelled == "child_a" {
            "child_b"
        } else {
            "child_a"
        };
        h.wait_for(
            || {
                events
                    .reduce()
                    .pending_question
                    .is_some_and(|request| request.questions[0].prompt == remaining)
            },
            "remaining question",
        );
        let request = events.reduce().pending_question.unwrap();
        h.sessions
            .respond_question("s1", request.request_id, UserQuestionReply::Skipped);
        h.wait_for(
            || h.response_for(remaining).is_some(),
            "remaining question response",
        );
        assert!(h.response_for(cancelled).is_none());
        h.result("sess_1");
        finish(turn).unwrap();
    }
}

#[test]
fn fails_the_active_turn_if_a_child_permission_reply_cannot_be_delivered() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "control_request",
        "request_id": "child_permission",
        "parent_tool_use_id": "toolu_agent",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "Read",
            "input": { "file_path": "/home/user/.gitconfig" },
        },
    }));
    let (request_id, _) = approval_request_id(&events);
    h.reject_next_write("Broken pipe");
    h.sessions
        .respond_approval("s1", request_id, ApprovalDecision::Allow);
    let error = finish(turn).unwrap_err();
    assert_eq!(error.to_string(), "Broken pipe");
    assert!(events.any(|event| event
        == &HarnessEvent::SessionError {
            message: "Broken pipe".into()
        }));
}

fn tool_of(session: &Session, call_id: &str) -> monocode_core::block::BlockTool {
    session
        .blocks
        .iter()
        .find_map(|block| {
            block
                .tool
                .as_ref()
                .filter(|tool| tool.call_id.as_deref() == Some(call_id))
                .cloned()
        })
        .unwrap_or_else(|| panic!("no tool block {call_id}"))
}

#[test]
fn shows_one_row_per_background_subagent_when_the_task_list_comes_first() {
    for descriptions in [
        ["Explore the auth module", "Review the tests"],
        ["Explore the auth module", "Explore the auth module"],
    ] {
        let h = Harness::new();
        let (events, turn) = h.start_turn("s1", TurnOptions::default());
        let agents = [
            ("toolu_a", "t1", descriptions[0]),
            ("toolu_b", "t2", descriptions[1]),
        ];
        h.emit(json!({
            "type": "assistant",
            "session_id": "sess_1",
            "message": { "content": agents.iter().map(|(id, _, description)| json!({
                "type": "tool_use",
                "id": id,
                "name": "Agent",
                "input": { "description": description, "subagent_type": "explore", "run_in_background": true },
            })).collect::<Vec<_>>() },
        }));
        // Claude lists the tasks, with no tool_use_id, before it announces them.
        h.emit(json!({
            "type": "system",
            "subtype": "background_tasks_changed",
            "tasks": agents.iter().map(|(_, task, description)| json!({
                "task_id": task, "task_type": "local_agent", "description": description,
            })).collect::<Vec<_>>(),
        }));
        // Each listed task must claim a different call, even before
        // task_started supplies the authoritative ids for agents with
        // identical descriptions.
        let mut updated: Vec<String> = events
            .all()
            .into_iter()
            .filter_map(|event| match event {
                HarnessEvent::ToolUpdated { call_id, .. } => Some(call_id),
                _ => None,
            })
            .collect();
        updated.sort();
        updated.dedup();
        assert_eq!(updated, ["toolu_a", "toolu_b"]);
        for (id, task, description) in agents {
            h.emit(json!({
                "type": "system",
                "subtype": "task_started",
                "task_id": task,
                "tool_use_id": id,
                "description": description,
                "task_type": "local_agent",
                "is_backgrounded": true,
            }));
        }
        let rows: Vec<String> = events
            .all()
            .into_iter()
            .filter_map(|event| match event {
                HarnessEvent::ToolStarted {
                    call_id,
                    kind: Some(kind),
                    ..
                } if kind == "agent" => Some(call_id),
                _ => None,
            })
            .collect();
        assert_eq!(rows, ["toolu_a", "toolu_b"]);

        h.emit(json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": "t1",
            "tool_use_id": "toolu_a",
            "status": "completed",
            "summary": "First agent finished",
        }));
        let session = events.reduce();
        let first = tool_of(&session, "toolu_a");
        assert_eq!(first.status.as_deref(), Some("completed"));
        assert_eq!(first.detail.as_deref(), Some("First agent finished"));
        assert_eq!(
            tool_of(&session, "toolu_b").status.as_deref(),
            Some("in_progress")
        );
        h.emit(json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": "t2",
            "tool_use_id": "toolu_b",
            "status": "completed",
            "summary": "Second agent finished",
        }));
        // Both agents finished before the result, so the turn waits for the
        // follow-up Claude takes to read them, which this fixture never sends.
        h.result("sess_1");
        finish(turn).unwrap();
    }
}

#[test]
fn keeps_an_unmatched_background_subagent_visible_until_it_finishes() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    let description = "Explore the auth module";
    // A task can be listed without an Agent call in the parent transcript.
    for _ in 0..2 {
        h.emit(json!({
            "type": "system",
            "subtype": "background_tasks_changed",
            "tasks": [{ "task_id": "t1", "task_type": "local_agent", "description": description }],
        }));
    }
    h.emit(json!({
        "type": "system",
        "subtype": "task_progress",
        "task_id": "t1",
        "description": description,
        "summary": "Reading the auth module",
    }));
    let call_id = format!("agent:{description}");
    let session = events.reduce();
    let agent_rows = |session: &Session| {
        session
            .blocks
            .iter()
            .filter(|block| {
                block.tool.as_ref().and_then(|tool| tool.kind.as_deref()) == Some("agent")
            })
            .count()
    };
    assert_eq!(agent_rows(&session), 1);
    let row = tool_of(&session, &call_id);
    assert_eq!(row.status.as_deref(), Some("in_progress"));
    assert_eq!(row.detail.as_deref(), Some("Reading the auth module"));

    h.emit(json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": "t1",
        "status": "completed",
        "summary": "Found the auth entry points",
    }));
    // The task finished before the result, so the turn waits for the
    // follow-up Claude takes to read it, which this fixture never sends.
    h.result("sess_1");
    finish(turn).unwrap();

    let finished = events.reduce();
    assert_eq!(agent_rows(&finished), 1);
    let row = tool_of(&finished, &call_id);
    assert_eq!(row.status.as_deref(), Some("completed"));
    assert_eq!(row.detail.as_deref(), Some("Found the auth entry points"));
}

fn emit_agent_call(h: &Harness, input: Value) {
    h.emit(json!({
        "type": "assistant",
        "session_id": "sess_1",
        "message": { "content": [{ "type": "tool_use", "id": "toolu_agent", "name": "Agent", "input": input }] },
    }));
}

fn has_message_completed(events: &Events) -> bool {
    events.any(|event| event == &HarnessEvent::MessageCompleted)
}

#[test]
fn stays_busy_after_a_parent_result_while_a_background_subagent_is_running() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    emit_agent_call(
        &h,
        json!({ "description": "Explore the auth module", "subagent_type": "explore" }),
    );
    h.emit(json!({
        "type": "system",
        "subtype": "task_started",
        "task_id": "t1",
        "tool_use_id": "toolu_agent",
        "description": "Explore the auth module",
        "task_type": "local_agent",
        "is_backgrounded": true,
    }));
    h.emit(json!({
        "type": "user",
        "session_id": "sess_1",
        "message": { "content": [{ "type": "tool_result", "tool_use_id": "toolu_agent", "content": "Backgrounded" }] },
    }));
    h.result("sess_1");

    assert!(!settles_within(&turn, Duration::from_millis(30)));
    assert!(events.any(|event| matches!(
        event,
        HarnessEvent::ToolStarted { kind: Some(kind), title, .. }
            if kind == "agent" && title == "Explore the auth module"
    )));
    assert!(!has_message_completed(&events));

    h.emit(json!({
        "type": "system",
        "subtype": "task_notification",
        "task_id": "t1",
        "tool_use_id": "toolu_agent",
        "status": "completed",
        "summary": "Found the tokens",
    }));
    // The notification wakes Claude for a follow-up turn; that turn's result
    // is what ends the MonoCode turn.
    assert!(!settles_within(&turn, Duration::from_millis(30)));
    h.emit_follow_up_turn("The explorer found the tokens.");
    finish(turn).unwrap();
    assert!(has_message_completed(&events));
}

#[test]
fn does_not_end_the_turn_on_a_subagent_result() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    emit_agent_call(
        &h,
        json!({ "description": "Explore", "subagent_type": "explore" }),
    );
    h.emit(json!({
        "type": "result",
        "subtype": "success",
        "session_id": "sess_sub",
        "parent_tool_use_id": "toolu_agent",
    }));
    assert!(!settles_within(&turn, Duration::from_millis(30)));
    assert!(!has_message_completed(&events));
    h.result("sess_1");
    finish(turn).unwrap();
}

#[test]
fn does_not_dump_subagent_assistant_text_into_the_parent_transcript() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    emit_agent_call(
        &h,
        json!({ "description": "Explore", "subagent_type": "explore" }),
    );
    h.emit(json!({
        "type": "assistant",
        "parent_tool_use_id": "toolu_agent",
        "message": { "content": [{ "type": "text", "text": "I will grep for tokens" }] },
    }));
    h.result("sess_1");
    finish(turn).unwrap();
    assert!(!events.any(|event| matches!(
        event,
        HarnessEvent::MessageDelta { text, .. } if text.contains("I will grep for tokens")
    )));
}

#[test]
fn mirrors_a_subagents_tools_thinking_and_prose_onto_its_own_row() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    emit_agent_call(
        &h,
        json!({ "description": "Correctness review", "subagent_type": "explore" }),
    );
    h.emit(json!({
        "type": "assistant",
        "parent_tool_use_id": "toolu_agent",
        "message": {
            "id": "msg_sub_1",
            "model": "claude-haiku-4-5",
            "content": [
                { "type": "thinking", "thinking": "Start with the reducer." },
                { "type": "text", "text": "I will grep for tokens" },
                { "type": "tool_use", "id": "toolu_sub_read", "name": "Read", "input": { "file_path": "/repo/src/App.tsx" } },
            ],
        },
    }));
    h.emit(json!({
        "type": "user",
        "parent_tool_use_id": "toolu_agent",
        "message": { "content": [{
            "type": "tool_result", "tool_use_id": "toolu_sub_read", "content": "export function App() {}",
        }] },
    }));
    h.result("sess_1");
    finish(turn).unwrap();

    let session = events.reduce();
    let agent = session
        .blocks
        .iter()
        .find(|block| {
            block.tool.as_ref().and_then(|tool| tool.call_id.as_deref()) == Some("toolu_agent")
        })
        .unwrap();
    assert_eq!(
        agent
            .agent_run
            .as_ref()
            .and_then(|run| run.model.as_deref()),
        Some("claude-haiku-4-5")
    );
    let steps: Vec<(String, String, AgentStepKind, String, Option<String>)> = events
        .all()
        .into_iter()
        .filter_map(|event| match event {
            HarnessEvent::AgentStep {
                call_id,
                step_id,
                kind,
                text,
                status,
                ..
            } => Some((call_id, step_id, kind, text, status)),
            _ => None,
        })
        .collect();
    assert!(steps.iter().all(|step| step.0 == "toolu_agent"));
    let steps: Vec<_> = steps
        .into_iter()
        .map(|(_, step_id, kind, text, status)| (step_id, kind, text, status))
        .collect();
    assert_eq!(
        steps,
        [
            (
                "msg_sub_1:thinking".to_string(),
                AgentStepKind::Reasoning,
                "Start with the reducer.".to_string(),
                None
            ),
            (
                "msg_sub_1:text".to_string(),
                AgentStepKind::Message,
                "I will grep for tokens".to_string(),
                None
            ),
            (
                "toolu_sub_read".to_string(),
                AgentStepKind::Tool,
                "Read /repo/src/App.tsx".to_string(),
                Some("in_progress".to_string())
            ),
            (
                "toolu_sub_read".to_string(),
                AgentStepKind::Tool,
                String::new(),
                Some("completed".to_string())
            ),
        ]
    );
}

#[test]
fn keeps_a_failed_subagent_tool_result_on_its_tool_row() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    emit_agent_call(&h, json!({ "description": "Run tests" }));
    h.emit(json!({
        "type": "assistant",
        "parent_tool_use_id": "toolu_agent",
        "message": { "content": [{ "type": "tool_use", "id": "toolu_sub_bash", "name": "Bash", "input": { "command": "npm test" } }] },
    }));
    h.emit(json!({
        "type": "user",
        "parent_tool_use_id": "toolu_agent",
        "message": { "content": [{
            "type": "tool_result",
            "tool_use_id": "toolu_sub_bash",
            "is_error": true,
            "content": [{ "type": "text", "text": "Tests failed: assertion error" }],
        }] },
    }));
    h.result("sess_1");
    finish(turn).unwrap();

    let session = events.reduce();
    let steps = &session
        .blocks
        .iter()
        .find(|block| {
            block.tool.as_ref().and_then(|tool| tool.call_id.as_deref()) == Some("toolu_agent")
        })
        .and_then(|block| block.agent_run.as_ref())
        .unwrap()
        .steps;
    assert_eq!(steps.len(), 1);
    let step = &steps[0];
    assert_eq!(step.id, "toolu_sub_bash");
    assert_eq!(step.kind, AgentStepKind::Tool);
    assert_eq!(step.text, "npm test");
    assert_eq!(step.tool_kind.as_deref(), Some("execute"));
    assert_eq!(step.status.as_deref(), Some("failed"));
    assert_eq!(
        step.detail.as_deref(),
        Some("Tests failed: assertion error")
    );
}

#[test]
fn does_not_mirror_a_subagent_result_onto_the_parent_tool_row() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    emit_agent_call(&h, json!({ "description": "Correctness review" }));
    h.emit(json!({
        "type": "user",
        "parent_tool_use_id": "toolu_agent",
        "message": { "content": [{
            "type": "tool_result", "tool_use_id": "toolu_sub_read", "content": "export function App() {}",
        }] },
    }));
    h.result("sess_1");
    finish(turn).unwrap();

    // The parent stays in flight: only the subagent's own row settles.
    assert!(!events.any(|event| matches!(
        event,
        HarnessEvent::ToolUpdated { call_id, status: Some(status), .. }
            if call_id == "toolu_agent" && status == "completed"
    )));
}

#[test]
fn ends_the_turn_once_a_subagent_has_reported_back_inline() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_inline_subagent();
    h.result("sess_1");
    finish(turn).unwrap();
}

#[test]
fn routes_an_unexpected_provider_exit_to_the_turn_that_is_actually_running() {
    let h = Harness::new();
    let (first, turn) = h.start_turn("s1", TurnOptions::default());
    h.result("sess_1");
    finish(turn).unwrap();

    let user_messages = h.user_count();
    let (second, turn) = h.send(
        "s1",
        TurnOptions {
            text: Some("try again"),
            ..Default::default()
        },
    );
    h.wait_for(|| h.user_count() > user_messages, "second user prompt");

    h.exit(Some(1));
    let error = finish(turn).unwrap_err();
    assert!(error.to_string().contains("Claude Code exited"));
    assert!(!first.any(|event| matches!(event, HarnessEvent::SessionEnded { .. })));
    assert!(second.any(|event| event == &HarnessEvent::SessionEnded { code: Some(1) }));
    assert!(second.any(|event| event
        == &HarnessEvent::SessionError {
            message: "Claude Code exited".into()
        }));
}

// describe("claude background tasks")

#[test]
fn keeps_the_turn_working_through_a_background_command_and_claudes_follow_up() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_background_bash("b1");
    assert!(!settles_within(&turn, Duration::from_millis(30)));
    assert!(!has_message_completed(&events));
    assert_eq!(
        events.background_updates(),
        [vec!["Wait 30 seconds then print done".to_string()]]
    );

    h.emit_bash_finished("b1");
    assert!(!settles_within(&turn, Duration::from_millis(30)));

    h.emit_follow_up_turn("It finished and printed done.");
    finish(turn).unwrap();
    assert_eq!(
        events.background_updates(),
        [vec!["Wait 30 seconds then print done".to_string()], vec![]]
    );
    // The command waited on sits under the message Claude left off with as a
    // row of its own, and the reply is a new message after it, once.
    let session = events.reduce();
    let background = session
        .blocks
        .iter()
        .find(|block| {
            block
                .tool
                .as_ref()
                .is_some_and(|tool| tool.background == Some(true))
        })
        .unwrap();
    let tool = background.tool.as_ref().unwrap();
    assert_eq!(tool.call_id.as_deref(), Some("background:b1"));
    assert_eq!(tool.status.as_deref(), Some("completed"));
    assert_eq!(
        tool.detail.as_deref(),
        Some("Background command \"sleep 30 && echo done\" completed (exit code 0)")
    );
    assert!(background.text.contains("sleep 30"));
    let order: Vec<String> = session
        .blocks
        .iter()
        .filter(|block| {
            block.role == BlockRole::Assistant
                || block
                    .tool
                    .as_ref()
                    .is_some_and(|tool| tool.background == Some(true))
        })
        .map(|block| {
            if block
                .tool
                .as_ref()
                .is_some_and(|tool| tool.background == Some(true))
            {
                "[background]".to_string()
            } else {
                block.text.clone()
            }
        })
        .collect();
    assert_eq!(
        order,
        ["waiting", "[background]", "It finished and printed done."]
    );
}

#[test]
fn shows_the_waited_on_command_as_a_live_row_under_claudes_last_message() {
    let h = Harness::new();
    let (events, _turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_background_bash("b1");

    let session = events.reduce();
    let count = session.blocks.len();
    assert_eq!(session.blocks[count - 2].text, "waiting");
    let last = session.blocks[count - 1].tool.as_ref().unwrap();
    assert_eq!(last.background, Some(true));
    assert_eq!(last.status.as_deref(), Some("in_progress"));
    assert_eq!(last.kind.as_deref(), Some("execute"));
}

#[test]
fn lets_the_turn_go_if_a_finished_task_never_wakes_claude() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_background_bash("b1");
    h.emit_bash_finished("b1");
    assert!(!settles_within(&turn, GRACE / 2));
    assert!(settles_within(&turn, GRACE * 3));
    finish(turn).unwrap();
}

#[test]
fn waits_for_claudes_follow_up_when_a_command_finishes_before_the_result() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_background_bash_launch("b1");
    h.emit_bash_finished("b1");
    h.result("sess_1");
    assert!(!settles_within(&turn, Duration::from_millis(30)));
    h.emit_follow_up_turn("It finished and printed done.");
    finish(turn).unwrap();
}

#[test]
fn ends_at_the_result_when_a_later_tool_result_already_carried_the_notice() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_background_bash_launch("b1");
    h.emit_bash_finished("b1");
    h.emit(json!({
        "type": "assistant",
        "session_id": "sess_1",
        "message": { "content": [{ "type": "tool_use", "id": "toolu_ls", "name": "Bash", "input": { "command": "ls" } }] },
    }));
    h.emit(json!({
        "type": "user",
        "session_id": "sess_1",
        "message": { "content": [{ "type": "tool_result", "tool_use_id": "toolu_ls", "content": "" }] },
    }));
    h.result("sess_1");
    assert!(settles_within(&turn, GRACE / 2));
    finish(turn).unwrap();
}

#[test]
fn lets_the_turn_go_if_a_command_finished_before_the_result_never_wakes_claude() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_background_bash_launch("b1");
    h.emit_bash_finished("b1");
    h.result("sess_1");
    assert!(!settles_within(&turn, GRACE / 2));
    assert!(settles_within(&turn, GRACE * 3));
    finish(turn).unwrap();
}

#[test]
fn stops_background_commands_when_the_turn_is_stopped() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit_background_bash("b7");

    smol::block_on(h.sessions.cancel_turn("s1")).unwrap();
    finish(turn).unwrap();
    let requests: Vec<Value> = h
        .parse()
        .into_iter()
        .filter_map(|message| message.get("request").cloned())
        .collect();
    // Stop ends the process, and its background commands with it.
    assert_eq!(requests.last(), Some(&json!({ "subtype": "interrupt" })));
    assert!(
        !requests
            .iter()
            .any(|request| request["subtype"] == "stop_task")
    );
    assert_eq!(h.io.state.lock().kills, 1);
    assert!(has_message_completed(&events));
}

// describe("claude plan permissions")

#[test]
fn answers_residual_plan_mode_permissions_without_prompting_the_user() {
    let h = Harness::new();
    let (events, turn) = h.start_turn(
        "s1",
        TurnOptions {
            runtime_mode: Some(RuntimeMode::Auto),
            intent: Some(TurnIntent::Plan),
            ..Default::default()
        },
    );
    h.emit(json!({
        "type": "control_request",
        "request_id": "read_1",
        "request": { "subtype": "can_use_tool", "tool_name": "Read", "input": { "file_path": "/repo/src/App.tsx" } },
    }));
    h.emit(json!({
        "type": "control_request",
        "request_id": "write_1",
        "request": { "subtype": "can_use_tool", "tool_name": "Write", "input": { "file_path": "/repo/src/new.ts" } },
    }));
    h.wait_for(
        || {
            h.parse()
                .iter()
                .filter(|m| m["type"] == "control_response")
                .count()
                >= 2
        },
        "plan permission responses",
    );
    assert_eq!(
        h.response_for("read_1").unwrap()["response"]["response"]["behavior"],
        "allow"
    );
    assert_eq!(
        h.response_for("write_1").unwrap()["response"]["response"]["behavior"],
        "deny"
    );
    assert!(!events.any(|event| matches!(event, HarnessEvent::ApprovalRequested { .. })));
    h.result("sess_1");
    finish(turn).unwrap();
}

#[test]
fn leaves_the_captured_plan_ready_to_build_after_a_subagent_explored_for_it() {
    let h = Harness::new();
    let (events, turn) = h.start_turn(
        "s1",
        TurnOptions {
            intent: Some(TurnIntent::Plan),
            ..Default::default()
        },
    );
    h.emit_inline_subagent();
    h.emit(json!({
        "type": "control_request",
        "request_id": "exit_1",
        "request": {
            "subtype": "can_use_tool",
            "tool_name": "ExitPlanMode",
            "tool_use_id": "toolu_exit",
            "input": { "plan": "# Plan\n\nRewrite the auth module." },
        },
    }));
    h.wait_for(
        || h.response_for("exit_1").is_some(),
        "exit plan mode response",
    );
    h.result("sess_1");

    // The turn must end for the session to stop being busy; until it does,
    // the plan's Build control stays disabled however the plan block reads.
    finish(turn).unwrap();
    let session = events.reduce();
    let plan = session
        .blocks
        .iter()
        .find(|block| block.role == BlockRole::Plan)
        .unwrap();
    assert!(plan.text.contains("Rewrite the auth module."));
    assert!(!plan.is_streaming());
    assert_eq!(
        plan.plan.as_ref().map(|plan| plan.status),
        Some(PlanStatus::Ready)
    );
}

// describe("claude manual compaction")

#[test]
fn runs_the_built_in_command_and_requires_a_compact_boundary() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    h.result("sess_1");
    finish(turn).unwrap();
    h.clear_sent();

    let events = Events::default();
    let sessions = h.sessions.clone();
    let sink = events.sink();
    let compact = smol::spawn(async move {
        sessions
            .compact_context(
                HarnessSessionInput {
                    session_id: "s1".into(),
                    cwd: "/repo".into(),
                    model: "claude:claude-sonnet-5".into(),
                    model_settings: None,
                    provider_account_id: None,
                    runtime_mode: RuntimeMode::Supervised,
                    intent: None,
                    controls_agents: None,
                    app_access: None,
                },
                sink,
            )
            .await
    });
    h.wait_for(|| h.user_count() > 0, "compact command");
    let command = h.parse().into_iter().find(|m| m["type"] == "user").unwrap();
    assert_eq!(
        command["message"]["content"],
        json!([{ "type": "text", "text": "/compact" }])
    );

    h.emit(json!({
        "type": "assistant",
        "session_id": "sess_1",
        "message": { "content": [{ "type": "text", "text": "not transcript output" }] },
    }));
    h.emit(json!({ "type": "system", "subtype": "compact_boundary", "session_id": "sess_1" }));
    h.result("sess_1");
    finish(compact).unwrap();

    assert!(events.any(|event| event
        == &HarnessEvent::Status {
            text: "Compacted context".into()
        }));
    assert!(!events.any(|event| matches!(event, HarnessEvent::MessageDelta { .. })));
}

// Not in the TypeScript suite: the sink runs after the session unlocks.

#[test]
fn a_sink_can_answer_an_approval_from_inside_its_callback() {
    let h = Harness::new();
    let events = Events::default();
    let sink: EventSink = {
        let record = events.sink();
        let sessions = h.sessions.clone();
        Arc::new(move |event: HarnessEvent| {
            if let HarnessEvent::ApprovalRequested { request_id, .. } = &event {
                sessions.respond_approval("s1", *request_id, ApprovalDecision::Allow);
            }
            record(event);
        })
    };
    let turn = h.send_with("s1", TurnOptions::default(), sink);
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request"]["subtype"] == "initialize")
        },
        "initialize",
    );
    h.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
    h.ack_init();
    h.wait_for(|| h.user_count() > 0, "user prompt");
    h.emit(json!({
        "type": "control_request",
        "request_id": "bash_1",
        "request": { "subtype": "can_use_tool", "tool_name": "Bash", "input": { "command": "ls" } },
    }));
    h.wait_for(|| h.response_for("bash_1").is_some(), "approval response");
    assert_eq!(
        h.response_for("bash_1").unwrap()["response"]["response"]["behavior"],
        "allow"
    );
    h.result("sess_1");
    finish(turn).unwrap();
    assert!(events.any(|event| matches!(
        event,
        HarnessEvent::ApprovalResolved {
            decision: monocode_core::block::ApprovalDecided::Allow,
            ..
        }
    )));
}

// describe("claude advisor consults")

fn advisor_interjections(events: &Events) -> Vec<(String, String, Option<String>, Option<String>)> {
    events
        .all()
        .into_iter()
        .filter_map(|event| match event {
            HarnessEvent::Interjection {
                id: Some(id),
                text,
                custom_type,
                model,
                status,
                ..
            } if custom_type == "advisor" => Some((
                id,
                text,
                model,
                status.map(|status| {
                    serde_json::to_value(status)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_string()
                }),
            )),
            _ => None,
        })
        .collect()
}

#[test]
fn shows_a_captured_advisor_consult_as_one_interjection() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    for line in include_str!("fixtures/advisor_redacted.jsonl").lines() {
        h.emit(serde_json::from_str(line).unwrap());
    }
    finish(turn).unwrap();

    let id = "advisor-srvtoolu_01Y9zw3xsSCZyzHMr91psFJL";
    let forwarded = "Claude Code sent the full conversation to the advisor.";
    let redacted = "The provider encrypts this advisor's advice, so MonoCode can't show it.";
    assert_eq!(
        advisor_interjections(&events),
        vec![
            (id.into(), forwarded.into(), None, Some("running".into())),
            (
                id.into(),
                format!("{redacted}\n\n{forwarded}"),
                None,
                Some("completed".into())
            ),
            (
                id.into(),
                format!("{redacted}\n\n{forwarded} 39,219 tokens in, 128 out."),
                Some("claude-fable-5-1".into()),
                Some("completed".into())
            ),
        ]
    );
    assert!(!events.any(|event| matches!(
        event,
        HarnessEvent::ToolStarted { call_id, .. } if call_id.starts_with("srvtoolu_")
    )));
    // The turn's context reading skips the advisor's own window.
    assert!(events.any(|event| matches!(
        event,
        HarnessEvent::Context {
            used: Some(37826),
            ..
        }
    )));

    let session = events.reduce();
    let advisor: Vec<_> = session
        .blocks
        .iter()
        .filter(|block| block.interjection.is_some())
        .collect();
    assert_eq!(advisor.len(), 1);
    assert_eq!(advisor[0].id, id);
    let meta = advisor[0].interjection.as_ref().unwrap();
    assert_eq!(meta.model.as_deref(), Some("claude-fable-5-1"));
    assert_eq!(meta.status, Some(InterjectionStatus::Completed));
    assert!(session.blocks.iter().any(|block| block.text
        == "I checked with the advisor first, as you asked. 2 + 2 = 4."));
}

#[test]
fn reads_advisor_advice_and_failures_from_the_assistant_snapshot() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "assistant",
        "session_id": "sess_1",
        "message": { "id": "msg_1", "content": [
            { "type": "server_tool_use", "id": "srvtoolu_ok", "name": "advisor", "input": {} },
            { "type": "advisor_tool_result", "tool_use_id": "srvtoolu_ok",
              "content": { "type": "advisor_result", "text": "Check the fallback.", "stop_reason": "end_turn" } },
            { "type": "server_tool_use", "id": "srvtoolu_err", "name": "advisor", "input": {} },
            { "type": "advisor_tool_result", "tool_use_id": "srvtoolu_err",
              "content": { "type": "advisor_tool_result_error", "error_code": "max_uses_exceeded" } },
        ] },
    }));
    h.result("sess_1");
    finish(turn).unwrap();

    let forwarded = "Claude Code sent the full conversation to the advisor.";
    let session = events.reduce();
    let advisor: Vec<_> = session
        .blocks
        .iter()
        .filter(|block| block.interjection.is_some())
        .collect();
    assert_eq!(advisor.len(), 2);
    assert_eq!(advisor[0].id, "advisor-srvtoolu_ok");
    assert_eq!(
        advisor[0].text,
        format!("Check the fallback.\n\n{forwarded}")
    );
    assert_eq!(
        advisor[0].interjection.as_ref().unwrap().status,
        Some(InterjectionStatus::Completed)
    );
    assert_eq!(advisor[1].id, "advisor-srvtoolu_err");
    assert_eq!(
        advisor[1].text,
        format!("The advisor call failed: max uses exceeded.\n\n{forwarded}")
    );
    assert_eq!(
        advisor[1].interjection.as_ref().unwrap().status,
        Some(InterjectionStatus::Failed)
    );
    assert!(!session.blocks.iter().any(|block| block.tool.is_some()));
}

// describe("official v0.7.0 lifecycle audit")

fn steer(h: &Harness, text: &str) -> Result<()> {
    smol::block_on(
        h.sessions
            .steer_turn(monocode_core::harness_event::SteerTurnInput {
                session_id: "s1".into(),
                cwd: "/repo".into(),
                model: "claude:claude-sonnet-5".into(),
                model_settings: None,
                text: text.into(),
                attachments: None,
            }),
    )
}

#[test]
fn cancels_binary_discovery_without_cancelling_the_next_send() {
    let h = Harness::new();
    let (gate, held) = async_channel::bounded::<()>(1);
    h.io.state.lock().binary_gate = Some(held);
    let (_, first) = h.send("s1", TurnOptions::default());
    std::thread::sleep(Duration::from_millis(20));
    smol::block_on(h.sessions.cancel_turn("s1")).unwrap();
    drop(gate);
    finish(first).unwrap();
    assert!(h.spawned().is_empty());

    let (_, second) = h.start_turn("s1", TurnOptions::default());
    h.result("sess_1");
    finish(second).unwrap();
}

#[test]
fn does_not_send_a_prompt_that_was_cancelled_during_initialization() {
    let h = Harness::new();
    let (_, turn) = h.send("s1", TurnOptions::default());
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request"]["subtype"] == "initialize")
        },
        "initialize",
    );
    smol::block_on(h.sessions.cancel_turn("s1")).unwrap();
    h.emit(json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": "monocode_2" },
    }));
    finish(turn).unwrap();
    assert_eq!(h.user_count(), 0);
}

#[test]
fn does_not_complete_a_new_turn_with_the_cancelled_turns_delayed_result() {
    let h = Harness::new();
    let (_, first) = h.start_turn("s1", TurnOptions::default());
    let old_line = h.io.state.lock().on_line.clone().unwrap();
    smol::block_on(h.sessions.cancel_turn("s1")).unwrap();
    finish(first).unwrap();

    let (_, second) = h.send(
        "s1",
        TurnOptions {
            text: Some("second"),
            ..Default::default()
        },
    );
    h.wait_for(|| h.spawned().len() == 2, "replacement Claude process");
    h.ack_init();
    h.wait_for(|| h.user_count() == 2, "second prompt");
    old_line(
        json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": ["Interrupted"],
            "terminal_reason": "aborted_streaming",
        })
        .to_string(),
    );
    assert!(!settles_within(&second, Duration::from_millis(100)));
    h.result("sess_1");
    finish(second).unwrap();
}

#[test]
fn reports_api_errors_carried_by_a_success_result() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "result",
        "subtype": "success",
        "is_error": true,
        "result": "API Error: 529 Overloaded",
        "errors": [],
    }));
    finish(turn).unwrap();
    assert!(events.all().contains(&HarnessEvent::SessionError {
        message: "API Error: 529 Overloaded".into(),
    }));
}

#[test]
fn keeps_the_monocode_turn_active_for_a_queued_steer_message() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    steer(&h, "second").unwrap();
    h.emit(json!({ "type": "result", "subtype": "success", "result": "FIRST", "is_error": false }));
    assert!(!settles_within(&turn, Duration::from_millis(100)));
    h.emit(
        json!({ "type": "result", "subtype": "success", "result": "SECOND", "is_error": false }),
    );
    finish(turn).unwrap();
}

#[test]
fn releases_the_result_a_steer_message_owed_when_it_cannot_be_written() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    h.reject_next_write("Write failed");
    assert!(steer(&h, "second").is_err());
    h.result("sess_1");
    finish(turn).unwrap();
}

#[test]
fn sums_token_totals_across_the_parent_and_steer_results() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    steer(&h, "second").unwrap();
    for (input, output) in [(10, 5), (20, 7)] {
        h.emit(json!({
            "type": "result",
            "subtype": "success",
            "usage": { "input_tokens": input, "output_tokens": output, "cache_read_input_tokens": 30 },
        }));
    }
    finish(turn).unwrap();
    let last = events
        .all()
        .into_iter()
        .rev()
        .find_map(|event| match event {
            HarnessEvent::TurnMetrics(metrics) => Some(metrics),
            _ => None,
        })
        .unwrap();
    assert_eq!(last.input_tokens, Some(30));
    assert_eq!(last.output_tokens, Some(12));
    assert_eq!(last.cache_read_tokens, Some(60));
    assert_eq!(last.cache_write_tokens, Some(0));
    assert_eq!(last.cache_hit_percent, Some(60.0 / 90.0 * 100.0));
}

#[test]
fn reads_the_window_of_the_model_claude_reported() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "assistant",
        "session_id": "sess_1",
        "message": { "model": "claude-opus-5", "content": [{ "type": "text", "text": "hi" }] },
    }));
    h.emit(json!({
        "type": "result",
        "subtype": "success",
        "usage": { "iterations": [{ "input_tokens": 5, "output_tokens": 5 }] },
        "modelUsage": {
            "claude-haiku-4-5": { "contextWindow": 200_000 },
            "claude-opus-5": { "contextWindow": 1_000_000 },
        },
    }));
    finish(turn).unwrap();
    assert!(events.all().contains(&HarnessEvent::Context {
        used: Some(10),
        window: Some(1_000_000),
    }));
}

#[test]
fn waits_for_the_follow_up_of_a_background_task_that_finished_before_the_first_result() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "system", "subtype": "task_started", "task_id": "short-bash",
        "task_type": "local_bash", "is_backgrounded": true, "description": "Audit verification",
    }));
    h.emit(json!({ "type": "system", "subtype": "background_tasks_changed", "tasks": [] }));
    h.emit(json!({
        "type": "system", "subtype": "task_updated", "task_id": "short-bash",
        "patch": { "status": "completed" },
    }));
    h.emit(json!({
        "type": "system", "subtype": "task_notification", "task_id": "short-bash",
        "status": "completed", "summary": "AUDIT_DONE",
    }));
    h.result("sess_1");
    assert!(!settles_within(&turn, Duration::from_millis(100)));
    h.emit_follow_up_turn("done");
    finish(turn).unwrap();
}

#[test]
fn rejects_an_initialization_error_rather_than_sending_the_prompt() {
    let h = Harness::new();
    let (_, turn) = h.send("s1", TurnOptions::default());
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request"]["subtype"] == "initialize")
        },
        "initialize",
    );
    h.emit(json!({
        "type": "control_response",
        "response": { "subtype": "error", "request_id": "monocode_2", "error": "Initialization failed" },
    }));
    let error = finish(turn).unwrap_err();
    assert!(format!("{error:#}").contains("Initialization failed"));
    assert_eq!(h.user_count(), 0);
}

#[test]
fn ignores_a_control_response_for_another_request_during_initialization() {
    let h = Harness::new();
    let (events, turn) = h.send("s1", TurnOptions::default());
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request"]["subtype"] == "initialize")
        },
        "initialize",
    );
    h.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
    h.emit(json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": "someone_else" },
    }));
    assert!(!settles_within(&turn, Duration::from_millis(100)));
    assert_eq!(h.user_count(), 0);
    assert!(!events.any(|event| matches!(event, HarnessEvent::SessionStarted)));
    h.ack_init();
    h.wait_for(|| h.user_count() == 1, "user prompt");
    h.result("sess_1");
    finish(turn).unwrap();
}

#[test]
fn does_not_report_initialized_when_no_initialize_acknowledgement_arrives() {
    let io = Arc::new(FakeIo::default());
    let options = ClaudeSessionOptions {
        init_timeout: Duration::from_millis(50),
        resume_grace: GRACE,
        ..Default::default()
    };
    let h = Harness {
        sessions: ClaudeSessions::new(io.clone(), Arc::new(SmolSpawner), options),
        io,
    };
    let (events, turn) = h.send("s1", TurnOptions::default());
    let error = finish(turn).unwrap_err();
    assert!(format!("{error:#}").contains("timed out"));
    assert!(!events.any(|event| matches!(event, HarnessEvent::SessionStarted)));
    assert_eq!(h.user_count(), 0);
}

#[test]
fn does_not_mark_a_child_that_exited_just_after_initialization_as_started() {
    let h = Harness::new();
    let (events, turn) = h.send("s1", TurnOptions::default());
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request"]["subtype"] == "initialize")
        },
        "initialize",
    );
    h.exit(Some(1));
    let error = finish(turn).unwrap_err();
    assert!(format!("{error:#}").contains("exited during initialization"));
    assert!(
        events
            .all()
            .contains(&HarnessEvent::SessionEnded { code: Some(1) })
    );
    assert!(!events.any(|event| matches!(event, HarnessEvent::SessionStarted)));
}

#[test]
fn does_not_silently_succeed_when_an_interrupt_cannot_reach_claude() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.reject_next_write("Write failed");
    let error = smol::block_on(h.sessions.cancel_turn("s1")).unwrap_err();
    assert!(format!("{error:#}").contains("Write failed"));
    finish(turn).unwrap();
    assert!(events.all().contains(&HarnessEvent::SessionError {
        message: "Write failed".into(),
    }));
    assert_eq!(h.io.state.lock().kills, 1);
}

// describe("review probes for the official v0.7.0 transcript")

fn delta(h: &Harness, text: &str) {
    h.emit(json!({
        "type": "stream_event",
        "event": { "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } },
    }));
}

fn start_agent(h: &Harness) {
    h.emit(json!({
        "type": "assistant",
        "message": { "id": "parent-message", "content": [{
            "type": "tool_use", "id": "agent-call", "name": "Agent", "input": { "description": "Review" },
        }] },
    }));
}

fn agent_steps(events: &Events) -> Vec<monocode_core::block::AgentStep> {
    events
        .reduce()
        .blocks
        .into_iter()
        .find(|block| {
            block.tool.as_ref().and_then(|tool| tool.call_id.as_deref()) == Some("agent-call")
        })
        .and_then(|block| block.agent_run)
        .map(|run| run.steps)
        .unwrap_or_default()
}

#[test]
fn preserves_repeated_incremental_text_chunks() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    for text in ["ha", "ha", "!"] {
        delta(&h, text);
    }
    h.emit(json!({
        "type": "assistant",
        "message": { "id": "answer", "content": [{ "type": "text", "text": "haha!" }] },
    }));
    h.result("sess_1");
    finish(turn).unwrap();
    let text: String = events
        .reduce()
        .blocks
        .iter()
        .filter(|block| block.role == BlockRole::Assistant)
        .map(|block| block.text.as_str())
        .collect();
    assert_eq!(text, "haha!");
}

#[test]
fn does_not_replace_the_final_requests_context_with_aggregate_turn_usage() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "assistant",
        "message": {
            "id": "last-request",
            "content": [{ "type": "text", "text": "Done" }],
            "usage": { "input_tokens": 5, "cache_read_input_tokens": 40_000, "output_tokens": 1 },
        },
    }));
    h.emit(json!({
        "type": "result",
        "subtype": "success",
        "usage": { "input_tokens": 20, "cache_read_input_tokens": 100_000, "output_tokens": 500 },
        "modelUsage": { "claude-sonnet-5": { "contextWindow": 200_000 } },
    }));
    finish(turn).unwrap();
    let context = events.reduce().context.unwrap();
    assert!(context.used < 50_000);
    assert_eq!(context.window, Some(200_000));
}

#[test]
fn does_not_use_a_subagents_larger_context_window_for_the_main_model() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "result",
        "subtype": "success",
        "usage": { "input_tokens": 10, "output_tokens": 5 },
        "modelUsage": {
            "claude-sonnet-5": { "contextWindow": 200_000 },
            "claude-opus-5[1m]": { "contextWindow": 1_000_000 },
        },
    }));
    finish(turn).unwrap();
    assert_eq!(events.reduce().context.unwrap().window, Some(200_000));
}

#[test]
fn keeps_successful_subagent_tool_output_available_for_inspection() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    start_agent(&h);
    h.emit(json!({
        "type": "assistant",
        "parent_tool_use_id": "agent-call",
        "message": { "id": "sub-message", "content": [{
            "type": "tool_use", "id": "shell-call", "name": "Bash", "input": { "command": "npm test" },
        }] },
    }));
    h.emit(json!({
        "type": "user",
        "parent_tool_use_id": "agent-call",
        "message": { "content": [{ "type": "tool_result", "tool_use_id": "shell-call", "content": "42 tests passed" }] },
    }));
    h.result("sess_1");
    finish(turn).unwrap();
    let step = agent_steps(&events)
        .into_iter()
        .find(|step| step.id == "shell-call")
        .unwrap();
    assert_eq!(step.detail.as_deref(), Some("42 tests passed"));
}

#[test]
fn keeps_all_subagent_text_blocks_that_share_one_api_message_id() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    start_agent(&h);
    for text in ["First observation.", "Second observation."] {
        h.emit(json!({
            "type": "assistant",
            "parent_tool_use_id": "agent-call",
            "message": { "id": "sub-message", "content": [{ "type": "text", "text": text }] },
        }));
    }
    h.result("sess_1");
    finish(turn).unwrap();
    let text: Vec<String> = agent_steps(&events)
        .into_iter()
        .map(|step| step.text)
        .collect();
    assert_eq!(text, ["First observation.\nSecond observation."]);
}

#[test]
fn counts_the_task_follow_up_work_in_the_same_monocode_user_turn() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "system", "subtype": "task_started", "task_id": "background-shell",
        "task_type": "local_bash", "description": "Run tests",
    }));
    h.emit(json!({
        "type": "result", "subtype": "success",
        "usage": { "input_tokens": 50, "cache_read_input_tokens": 5000, "output_tokens": 1000 },
    }));
    h.emit(json!({
        "type": "system", "subtype": "task_notification", "task_id": "background-shell",
        "status": "completed", "summary": "Tests passed",
    }));
    h.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
    delta(&h, "The tests passed.");
    h.emit(json!({
        "type": "assistant",
        "message": { "content": [{ "type": "text", "text": "The tests passed." }] },
    }));
    h.emit(json!({
        "type": "result", "subtype": "success",
        "usage": { "input_tokens": 5, "cache_read_input_tokens": 0, "output_tokens": 20 },
    }));
    finish(turn).unwrap();
    let mut session = Session::blank("s", HarnessId::Claude, "claude:sonnet-5", "/repo");
    session.blocks.push(monocode_core::block::Block::new(
        "user-turn",
        BlockRole::User,
        "Run tests",
    ));
    for event in events.all() {
        session = apply_harness_event(&session, &event);
    }
    let metrics = session.blocks[0].turn_metrics.clone().unwrap();
    assert_eq!(metrics.input_tokens, Some(55));
    assert_eq!(metrics.output_tokens, Some(1020));
}

#[test]
fn passes_an_explicit_thinking_off_to_claude() {
    let h = Harness::new();
    let mut settings = ModelSettings::new();
    settings.insert("thinking".into(), "false".into());
    let input = SendTurnInput {
        session: HarnessSessionInput {
            session_id: "s1".into(),
            cwd: "/repo".into(),
            model: "claude:claude-sonnet-5".into(),
            model_settings: Some(settings),
            provider_account_id: None,
            runtime_mode: RuntimeMode::Supervised,
            intent: None,
            controls_agents: None,
            app_access: None,
        },
        text: "hi".into(),
        attachments: Some(Vec::new()),
    };
    let sessions = h.sessions.clone();
    let events = Events::default();
    let sink = events.sink();
    let turn = smol::spawn(async move { sessions.send_turn(input, sink).await });
    h.wait_for(|| h.spawned().len() == 1, "Claude process");
    h.ack_init();
    h.wait_for(|| h.user_count() == 1, "user prompt");
    let args = &h.spawned()[0];
    let index = args.iter().position(|arg| arg == "--settings").unwrap();
    let settings: Value = serde_json::from_str(&args[index + 1]).unwrap();
    assert_eq!(settings["alwaysThinkingEnabled"], json!(false));
    h.result("sess_1");
    finish(turn).unwrap();
}

// describe("audit transcript text helper")

fn text_prompt(
    h: &Harness,
    intent: Option<TurnIntent>,
    events: &Events,
) -> smol::Task<Result<String>> {
    let text = super::text::ClaudeText::with_init_timeout(
        h.io.clone(),
        Arc::new(Vec::new),
        Duration::from_secs(2),
    );
    let input = crate::core::registry::TextPromptInput {
        cwd: "/repo".into(),
        provider_account_id: None,
        model: Some("claude-haiku-4-5".into()),
        model_settings: None,
        thread_id: None,
        on_thread_id: None,
        intent,
        prompt: "Where is auth?".into(),
        timeout_ms: Some(5_000),
        signal: None,
        on_event: Some(events.sink()),
    };
    smol::spawn(async move { text.run(input).await })
}

#[test]
fn the_read_only_helper_finishes_a_tool_loop_and_publishes_tool_results() {
    let h = Harness::new();
    let events = Events::default();
    let prompt = text_prompt(&h, Some(TurnIntent::Plan), &events);
    h.wait_for(|| h.spawned().len() == 1, "text helper");
    let args = &h.spawned()[0];
    assert!(contains_all(args, &["--tools", "Read,Glob,Grep"]));
    assert!(contains_all(args, &["--max-turns", "12"]));
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request_id"] == "monocode_text_init")
        },
        "helper initialize",
    );
    // `system init` alone does not make the helper ready.
    h.emit(json!({ "type": "system", "subtype": "init" }));
    std::thread::sleep(Duration::from_millis(30));
    assert_eq!(h.user_count(), 0);
    h.emit(json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": "monocode_text_init" },
    }));
    h.wait_for(|| h.user_count() == 1, "helper prompt");
    h.emit(json!({
        "type": "stream_event",
        "event": { "type": "content_block_start", "index": 0, "content_block": {
            "type": "tool_use", "id": "read-1", "name": "Read", "input": { "file_path": "/repo/auth.rs" },
        } },
    }));
    h.emit(json!({
        "type": "user",
        "message": { "content": [{ "type": "tool_result", "tool_use_id": "read-1", "content": "fn login() {}" }] },
    }));
    h.emit(json!({
        "type": "assistant",
        "message": { "content": [{ "type": "text", "text": "Auth is in auth.rs." }] },
    }));
    h.emit(json!({ "type": "result", "subtype": "success" }));
    let output = smol::block_on(prompt).unwrap();
    assert_eq!(output, "Auth is in auth.rs.");
    assert!(events.any(|event| matches!(
        event,
        HarnessEvent::ToolUpdated { call_id, status: Some(status), detail: Some(detail), .. }
            if call_id == "read-1" && status == "completed" && detail == "fn login() {}"
    )));
}

#[test]
fn automatic_helpers_launch_with_no_tools_and_fail_a_rejected_initialize() {
    let h = Harness::new();
    let events = Events::default();
    let prompt = text_prompt(&h, None, &events);
    h.wait_for(|| h.spawned().len() == 1, "text helper");
    assert!(contains_all(
        &h.spawned()[0],
        &["--tools", "", "--max-turns", "1"]
    ));
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request_id"] == "monocode_text_init")
        },
        "helper initialize",
    );
    h.emit(json!({
        "type": "control_response",
        "response": { "subtype": "error", "request_id": "monocode_text_init", "error": "bad flags" },
    }));
    let error = smol::block_on(prompt).unwrap_err();
    assert!(format!("{error:#}").contains("bad flags"));
    assert_eq!(h.user_count(), 0);
}

// describe("MCP form elicitation")

fn last_question_id(events: &Events) -> i64 {
    events
        .all()
        .into_iter()
        .rev()
        .find_map(|event| match event {
            HarnessEvent::QuestionAsked { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("no form shown")
}

fn form_answer(count: &str) -> UserQuestionReply {
    UserQuestionReply::Answered {
        answers: [("__mcp_action".to_string(), vec!["accept".to_string()])].into(),
        custom: Some([("count".to_string(), count.to_string())].into()),
    }
}

#[test]
fn keeps_an_mcp_form_open_after_invalid_input_and_sends_the_corrected_typed_answer() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "control_request",
        "request_id": "form",
        "request": {
            "subtype": "elicitation",
            "mode": "form",
            "requested_schema": {
                "type": "object",
                "properties": { "count": { "type": "integer", "minimum": 1 } },
                "required": ["count"],
            },
        },
    }));
    h.wait_for(
        || events.any(|e| matches!(e, HarnessEvent::QuestionAsked { .. })),
        "form",
    );
    let first = last_question_id(&events);
    h.sessions.respond_question("s1", first, form_answer("0"));
    h.wait_for(|| last_question_id(&events) != first, "form shown again");
    assert!(h.response_for("form").is_none());
    h.sessions
        .respond_question("s1", last_question_id(&events), form_answer("2"));
    h.wait_for(|| h.response_for("form").is_some(), "form response");
    assert_eq!(
        h.response_for("form").unwrap()["response"]["response"],
        json!({ "action": "accept", "content": { "count": 2 } })
    );
    h.result("sess_1");
    finish(turn).unwrap();
}

#[test]
fn declines_an_mcp_form_the_question_ui_cannot_show() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({
        "type": "control_request",
        "request_id": "url-form",
        "request": { "subtype": "elicitation", "mode": "url", "url": "https://example.com" },
    }));
    h.wait_for(|| h.response_for("url-form").is_some(), "decline");
    assert_eq!(
        h.response_for("url-form").unwrap()["response"]["response"],
        json!({ "action": "decline" })
    );
    assert!(!events.any(|e| matches!(e, HarnessEvent::QuestionAsked { .. })));
    h.result("sess_1");
    finish(turn).unwrap();
}

// describe("Claude's own Plan Mode")

fn plan_tool_request(h: &Harness, request_id: &str, tool_name: &str) {
    h.emit(json!({
        "type": "control_request",
        "request_id": request_id,
        "request": {
            "subtype": "can_use_tool",
            "tool_name": tool_name,
            "tool_use_id": format!("{request_id}-tool"),
            "input": if tool_name == "ExitPlanMode" {
                json!({ "plan": "# Plan\n\n1. Modify the file" })
            } else {
                json!({})
            },
        },
    }));
}

fn approval_ids(events: &Events) -> Vec<i64> {
    events
        .all()
        .into_iter()
        .filter_map(|event| match event {
            HarnessEvent::ApprovalRequested { request_id, .. } => Some(request_id),
            _ => None,
        })
        .collect()
}

#[test]
fn allows_exit_plan_mode_during_a_full_access_implementation_turn() {
    let h = Harness::new();
    let (events, turn) = h.start_turn(
        "s1",
        TurnOptions {
            runtime_mode: Some(RuntimeMode::FullAccess),
            ..Default::default()
        },
    );
    plan_tool_request(&h, "exit-plan", "ExitPlanMode");
    h.wait_for(
        || h.response_for("exit-plan").is_some(),
        "exit plan response",
    );
    assert_eq!(
        h.response_for("exit-plan").unwrap()["response"]["response"]["behavior"],
        "allow"
    );
    assert!(approval_ids(&events).is_empty());
    h.result("sess_1");
    finish(turn).unwrap();
}

#[test]
fn restarts_in_build_mode_after_an_autonomous_plan() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    plan_tool_request(&h, "enter-plan", "EnterPlanMode");
    h.wait_for(|| approval_ids(&events).len() == 1, "enter approval");
    h.sessions
        .respond_approval("s1", approval_ids(&events)[0], ApprovalDecision::Allow);
    h.wait_for(|| h.response_for("enter-plan").is_some(), "enter response");
    plan_tool_request(&h, "exit-plan", "ExitPlanMode");
    h.wait_for(|| approval_ids(&events).len() == 2, "exit approval");
    h.sessions
        .respond_approval("s1", approval_ids(&events)[1], ApprovalDecision::Deny);
    h.wait_for(|| h.response_for("exit-plan").is_some(), "exit response");
    h.result("sess_1");
    finish(turn).unwrap();

    let (_, build) = h.send(
        "s1",
        TurnOptions {
            intent: Some(TurnIntent::Build),
            text: Some("Build approved plan"),
            ..Default::default()
        },
    );
    h.wait_for(|| h.spawned().len() == 2, "Build process");
    h.ack_init();
    h.wait_for(|| h.user_count() == 2, "build prompt");
    h.result("sess_1");
    finish(build).unwrap();
}

#[test]
fn reuses_the_process_for_build_when_claude_left_plan_mode() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    plan_tool_request(&h, "enter-plan", "EnterPlanMode");
    h.wait_for(|| approval_ids(&events).len() == 1, "enter approval");
    h.sessions
        .respond_approval("s1", approval_ids(&events)[0], ApprovalDecision::Allow);
    plan_tool_request(&h, "exit-plan", "ExitPlanMode");
    h.wait_for(|| approval_ids(&events).len() == 2, "exit approval");
    h.sessions
        .respond_approval("s1", approval_ids(&events)[1], ApprovalDecision::Allow);
    h.wait_for(|| h.response_for("exit-plan").is_some(), "exit response");
    h.result("sess_1");
    finish(turn).unwrap();

    let (_, build) = h.send(
        "s1",
        TurnOptions {
            intent: Some(TurnIntent::Build),
            ..Default::default()
        },
    );
    h.wait_for(|| h.user_count() == 2, "build prompt");
    assert_eq!(h.spawned().len(), 1);
    h.result("sess_1");
    finish(build).unwrap();
}

// describe("native activity after an ordinary result")

/// A harness whose clock the test sets, in epoch ms.
fn clocked(now: i64) -> (Harness, Arc<std::sync::atomic::AtomicI64>) {
    let clock = Arc::new(std::sync::atomic::AtomicI64::new(now));
    let io = Arc::new(FakeIo::default());
    let read = clock.clone();
    let options = ClaudeSessionOptions {
        init_timeout: Duration::from_secs(2),
        resume_grace: GRACE,
        now_ms: Arc::new(move || read.load(std::sync::atomic::Ordering::SeqCst)),
        ..Default::default()
    };
    let sessions = ClaudeSessions::new(io.clone(), Arc::new(SmolSpawner), options);
    (Harness { io, sessions }, clock)
}

fn complete_tool(
    h: &Harness,
    id: &str,
    name: &str,
    input: Value,
    result: Option<Value>,
    text: &str,
) {
    h.emit(json!({
        "type": "assistant",
        "message": { "content": [{ "type": "tool_use", "id": id, "name": name, "input": input }] },
    }));
    let mut record = json!({
        "type": "user",
        "message": { "content": [{ "type": "tool_result", "tool_use_id": id, "content": text }] },
    });
    if let Some(result) = result {
        record["tool_use_result"] = result;
    }
    h.emit(record);
}

fn local_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    use chrono::TimeZone;
    chrono::Local
        .with_ymd_and_hms(year, month, day, hour, minute, 0)
        .earliest()
        .unwrap()
        .timestamp_millis()
}

#[test]
fn reads_a_native_cron_id_from_the_result_text() {
    for recurring in [false, true] {
        let h = Harness::new();
        let (_, turn) = h.start_turn("s1", TurnOptions::default());
        let kind = if recurring {
            "recurring job"
        } else {
            "one-shot task"
        };
        complete_tool(
            &h,
            "create",
            "CronCreate",
            json!({ "cron": "* * * * *", "recurring": recurring }),
            None,
            &format!("Scheduled {kind} native-job (* * * * *)."),
        );
        h.result("sess_1");
        finish(turn).unwrap();
        assert!(h.sessions.needs_process("s1"));
        complete_tool(
            &h,
            "delete",
            "CronDelete",
            json!({ "id": "native-job" }),
            Some(json!({})),
            "ok",
        );
        h.result("sess_1");
        assert!(!h.sessions.needs_process("s1"), "recurring={recurring}");
    }
}

#[test]
fn uses_the_structured_cron_id_when_a_job_is_deleted() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    complete_tool(
        &h,
        "create",
        "CronCreate",
        json!({ "cron": "* * * * *", "recurring": true }),
        Some(json!({ "id": "cron-1", "recurring": true })),
        "Scheduled",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    assert!(h.sessions.needs_process("s1"));
    complete_tool(
        &h,
        "delete",
        "CronDelete",
        json!({ "id": "cron-1" }),
        Some(json!({})),
        "ok",
    );
    h.result("sess_1");
    assert!(!h.sessions.needs_process("s1"));
}

#[test]
fn removes_only_due_one_shot_jobs_when_claude_wakes_up() {
    let (h, clock) = clocked(local_ms(2026, 10, 3, 12, 3));
    let set = |ms: i64| clock.store(ms, std::sync::atomic::Ordering::SeqCst);
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    complete_tool(
        &h,
        "once",
        "CronCreate",
        json!({ "cron": "5 12 * * *" }),
        Some(json!({ "id": "once", "recurring": false })),
        "ok",
    );
    complete_tool(
        &h,
        "later",
        "CronCreate",
        json!({ "cron": "5 13 * * *" }),
        Some(json!({ "id": "later", "recurring": false })),
        "ok",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    set(local_ms(2026, 10, 3, 12, 5));
    h.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
    h.result("sess_1");
    assert!(h.sessions.needs_process("s1"));
    set(local_ms(2026, 10, 3, 13, 5));
    h.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
    h.result("sess_1");
    assert!(!h.sessions.needs_process("s1"));
}

#[test]
fn reconciles_the_scheduled_jobs_reported_by_cron_list() {
    let h = Harness::new();
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    complete_tool(
        &h,
        "create",
        "CronCreate",
        json!({ "cron": "* * * * *", "recurring": true }),
        Some(json!({ "id": "expired", "recurring": true })),
        "ok",
    );
    complete_tool(
        &h,
        "list",
        "CronList",
        json!({}),
        Some(json!({ "jobs": [] })),
        "No jobs",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    assert!(!h.sessions.needs_process("s1"));
}

#[test]
fn releases_recurring_jobs_after_their_expiry_and_final_fire_jitter() {
    let start = local_ms(2026, 10, 3, 12, 0);
    let (h, clock) = clocked(start);
    let (_, turn) = h.start_turn("s1", TurnOptions::default());
    complete_tool(
        &h,
        "create",
        "CronCreate",
        json!({ "cron": "* * * * *", "recurring": true }),
        Some(json!({ "id": "recurring", "recurring": true })),
        "ok",
    );
    h.result("sess_1");
    finish(turn).unwrap();
    assert!(h.sessions.needs_process("s1"));
    clock.store(
        start + 7 * 86_400_000 + 31 * 60_000,
        std::sync::atomic::Ordering::SeqCst,
    );
    assert!(!h.sessions.needs_process("s1"));
}

#[test]
fn releases_finished_ambient_work() {
    for subtype in ["task_notification", "task_updated"] {
        let h = Harness::new();
        let (_, turn) = h.start_turn("s1", TurnOptions::default());
        h.emit(json!({ "type": "system", "subtype": "task_started", "task_id": "ambient-1", "ambient": true }));
        h.result("sess_1");
        finish(turn).unwrap();
        assert!(h.sessions.needs_process("s1"));
        h.emit(json!({
            "type": "system", "subtype": subtype, "task_id": "ambient-1", "ambient": true,
            "status": "completed", "patch": { "status": "completed" },
        }));
        assert!(!h.sessions.needs_process("s1"), "{subtype}");
    }
}

#[test]
fn gives_an_unsolicited_wakeup_a_native_turn_of_its_own() {
    let h = Harness::new();
    let (events, turn) = h.start_turn("s1", TurnOptions::default());
    h.emit(json!({ "type": "result", "subtype": "success", "result": "Scheduled" }));
    finish(turn).unwrap();
    assert!(!h.sessions.needs_process("s1"));

    h.emit(json!({ "type": "system", "subtype": "init", "session_id": "sess_1" }));
    delta(&h, "The scheduled check has started.");
    assert!(h.sessions.needs_process("s1"));
    h.result("sess_1");
    let all = events.all();
    let started = all
        .iter()
        .position(|event| {
            matches!(
                event,
                HarnessEvent::TurnStarted {
                    native: Some(true),
                    ..
                }
            )
        })
        .expect("native turn.started");
    let finished = all
        .iter()
        .position(|event| *event == HarnessEvent::TurnFinished { native: Some(true) })
        .expect("native turn.finished");
    assert!(started < finished);
    assert!(all[started..finished].iter().any(|event| matches!(
        event,
        HarnessEvent::MessageDelta { text, .. } if text == "The scheduled check has started."
    )));
    assert!(!h.sessions.needs_process("s1"));
}

// describe("catalog discovery scope")

fn discover_in(h: &Harness, catalog: &super::catalog::ClaudeCatalog, cwd: &str, account: &str) {
    let spawns = h.spawned().len();
    let refresh = catalog.refresh_in(crate::core::registry::CatalogScope {
        cwd: Some(cwd.into()),
        provider_account_id: Some(account.into()),
        force: false,
    });
    let running = smol::spawn(refresh);
    h.wait_for(|| h.spawned().len() == spawns + 1, "catalog probe");
    h.wait_for(
        || {
            h.parse()
                .iter()
                .filter(|m| m["request"]["subtype"] == "initialize")
                .count()
                > spawns
        },
        "probe initialize",
    );
    h.emit(json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": "monocode_init", "response": {} },
    }));
    h.wait_for(
        || {
            h.parse()
                .iter()
                .filter(|m| m["request"]["subtype"] == "list_models")
                .count()
                > spawns
        },
        "list_models",
    );
    h.emit(json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": "monocode_list_models", "response": {
            "models": [{ "value": "sonnet", "resolvedModel": "claude-sonnet-5", "displayName": "Sonnet" }],
        } },
    }));
    smol::block_on(running);
}

#[test]
fn discovers_models_in_the_selected_project_with_the_selected_account() {
    let h = Harness::new();
    let set: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let catalog = super::catalog::ClaudeCatalog::new(h.io.clone(), Arc::new(SmolSpawner), {
        let set = set.clone();
        Arc::new(move |models: Vec<monocode_core::models::AgentModel>| {
            set.lock()
                .push(models.into_iter().map(|model| model.name).collect());
        })
    });
    discover_in(&h, &catalog, "/audit-project", "account-work");
    discover_in(&h, &catalog, "/audit-project", "default");
    let places = h.io.state.lock().spawn_places.clone();
    assert_eq!(places.len(), 2);
    assert_eq!(places[0].0, "/audit-project");
    assert_eq!(places[0].1.as_ref().unwrap().id, "account-work");
    assert_eq!(places[1].1.as_ref().unwrap().id, "default");
    assert_eq!(set.lock().len(), 2);
}

// describe("Git helpers forward the account")

#[test]
fn names_a_branch_under_the_selected_account() {
    let h = Harness::new();
    let text = super::text::ClaudeText::with_init_timeout(
        h.io.clone(),
        Arc::new(Vec::new),
        Duration::from_secs(2),
    );
    let branch = smol::spawn(async move {
        super::git::generate_claude_branch_name(&text, "/repo", "Fix login", Some("account-work"))
            .await
    });
    h.wait_for(|| h.spawned().len() == 1, "text helper");
    h.wait_for(
        || {
            h.parse()
                .iter()
                .any(|m| m["request_id"] == "monocode_text_init")
        },
        "helper initialize",
    );
    h.emit(json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": "monocode_text_init" },
    }));
    h.wait_for(|| h.user_count() == 1, "helper prompt");
    h.emit(json!({
        "type": "assistant",
        "message": { "content": [{ "type": "text", "text": "fix-login" }] },
    }));
    h.emit(json!({ "type": "result", "subtype": "success" }));
    assert_eq!(smol::block_on(branch).as_deref(), Some("fix-login"));
    let places = h.io.state.lock().spawn_places.clone();
    assert_eq!(places[0].1.as_ref().unwrap().id, "account-work");
}

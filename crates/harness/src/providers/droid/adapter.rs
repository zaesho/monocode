//! Port of src/integrations/harness/providers/droid/droid.ts and
//! droidAdapter.ts: the live Factory Droid adapter. It spawns
//! `droid exec --output-format acp`.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::Shared;
use parking_lot::Mutex;
use regex::Regex;
use serde_json::{Value, json};

use monocode_core::block::TurnIntent;
use monocode_core::harness::{HarnessId, RuntimeMode};
use monocode_core::harness_event::{
    ApprovalDecision, HarnessEvent, HarnessSessionInput, SendTurnInput, SteerTurnInput,
};
use monocode_core::js;

use crate::core::acp::{AcpClient, AcpHandlers};
use crate::core::acp_subagents::AcpSubagents;
use crate::core::catalog::SharedCatalog;
use crate::core::child::{ChildHandlers, Children};
use crate::core::register::HarnessContext;
use crate::core::registry::{AcceptedHook, AdapterCapabilities, EventSink, HarnessAdapter};
use crate::core::task::{self, BoxFuture, SharedSpawner};
use crate::providers::grok::adapter::decided;
use crate::providers::grok::protocol::{
    events_from_acp_update, permission_option_id, permission_request_from_acp, pick_auto_option,
};
use crate::providers::grok::shared::{
    Wiring, initialize_params, permission_outcome, respond_method_not_found, spawn_method_not_found,
};

use super::catalog::DroidCatalog;
use super::protocol::{
    DROID_ACP_ARGS, DROID_AUTH_HELP, DroidConfigOption, droid_config_options_from,
    droid_current_model_id, droid_effort_config, droid_effort_value, droid_error_message,
    droid_mode_id, droid_model_config, droid_prompt_blocks, droid_session_id, droid_spec_plan,
    droid_startup_error, is_droid_auth_error, is_droid_error_echo, models_from_droid_session,
};

const INIT_TIMEOUT_MS: i64 = 20_000;
const SESSION_TIMEOUT_MS: i64 = 45_000;
const CONTROL_TIMEOUT_MS: i64 = 20_000;
const PROMPT_TIMEOUT_MS: i64 = 30 * 60_000;

struct LiveState {
    model_id: String,
    mode_id: String,
    /// Counts mode changes Droid reported, so a control reply does not
    /// overwrite a newer `current_mode_update`.
    mode_revision: u64,
    mute_updates: bool,
    cancelled: bool,
    /// The connection is gone. Permission replies are skipped.
    closed: bool,
    runtime_mode: RuntimeMode,
    planning: bool,
    on_event: EventSink,
    /// The kind of each tool call this turn, for permission requests that
    /// omit it.
    tool_kinds: HashMap<String, String>,
}

/// Droid's config options, with a count of whole-list replacements. The
/// count is the TypeScript `configRevision`.
#[derive(Default)]
struct ConfigOptions {
    generation: u64,
    options: Vec<DroidConfigOption>,
}

/// `Live`.
struct Live {
    subagents: Mutex<AcpSubagents>,
    acp: AcpClient,
    acp_session_id: String,
    cwd: String,
    config: Mutex<ConfigOptions>,
    state: Mutex<LiveState>,
    approvals: Mutex<HashMap<i64, oneshot::Sender<Option<ApprovalDecision>>>>,
    /// `permissionTasks`. Each request handler holds a read guard, and
    /// stopping takes the write lock to wait for their replies.
    permission_tasks: Arc<smol::lock::RwLock<()>>,
}

impl Live {
    fn emit(&self, event: HarnessEvent) {
        let sink = self.state.lock().on_event.clone();
        sink(event);
    }

    fn cancelled(&self) -> bool {
        self.state.lock().cancelled
    }

    /// Cancelled or closed.
    fn retired(&self) -> bool {
        let state = self.state.lock();
        state.cancelled || state.closed
    }

    fn options(&self) -> Vec<DroidConfigOption> {
        self.config.lock().options.clone()
    }

    fn config_generation(&self) -> u64 {
        self.config.lock().generation
    }

    /// `resolveApprovals`: pending approvals end without a decision.
    fn resolve_approvals(&self) {
        for (_, waiter) in self.approvals.lock().drain() {
            let _ = waiter.send(None);
        }
    }

    /// `syncConfig`: a config snapshot also carries the model and mode in
    /// effect.
    fn sync_config(&self, options: Vec<DroidConfigOption>) {
        let model = droid_model_config(&options).and_then(|config| config.current_value.clone());
        let mode = options
            .iter()
            .find(|option| {
                option.category.as_deref() == Some("mode") || option.id == "autonomy_level"
            })
            .and_then(|option| option.current_value.clone());
        {
            let mut config = self.config.lock();
            config.generation += 1;
            config.options = options;
        }
        let mut state = self.state.lock();
        if let Some(model) = model {
            state.model_id = model;
        }
        if let Some(mode) = mode {
            state.mode_id = mode;
            state.mode_revision += 1;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Resume {
    acp_session_id: String,
    cwd: String,
}

type Tail = Shared<BoxFuture<'static, ()>>;

#[derive(Default)]
struct Threads {
    live_by_thread: HashMap<String, Arc<Live>>,
    /// `startingByThread`: clients whose session is not live yet, so a stop
    /// can close them.
    starting_by_thread: HashMap<String, AcpClient>,
    resume_by_thread: HashMap<String, Resume>,
    /// `generations`. Queued turns share their thread's generation, and a
    /// stop or cancel drops it so every queued step bails out.
    generations: HashMap<String, u64>,
    next_generation: u64,
    /// `queues`: the last turn or stop per thread, tagged so its owner can
    /// remove only its own entry.
    queues: HashMap<String, (u64, Tail)>,
    next_queue: u64,
}

/// A place in a thread's queue. Dropping `done` lets the next entry run.
struct Queued {
    id: u64,
    previous: Option<Tail>,
    tail: Tail,
    done: oneshot::Sender<()>,
}

impl Threads {
    fn generation(&mut self, session_id: &str) -> u64 {
        if let Some(generation) = self.generations.get(session_id) {
            return *generation;
        }
        self.next_generation += 1;
        let generation = self.next_generation;
        self.generations.insert(session_id.to_string(), generation);
        generation
    }

    /// Queue behind the thread's last entry.
    fn enqueue(&mut self, session_id: &str) -> Queued {
        let previous = self.queues.get(session_id).map(|(_, tail)| tail.clone());
        let (done, finished) = oneshot::channel::<()>();
        let before = previous.clone();
        let tail: Tail = async move {
            if let Some(before) = before {
                before.await;
            }
            let _ = finished.await;
        }
        .boxed()
        .shared();
        self.next_queue += 1;
        let id = self.next_queue;
        self.queues
            .insert(session_id.to_string(), (id, tail.clone()));
        Queued {
            id,
            previous,
            tail,
            done,
        }
    }

    fn dequeue(&mut self, session_id: &str, id: u64) {
        if self
            .queues
            .get(session_id)
            .is_some_and(|(entry, _)| *entry == id)
        {
            self.queues.remove(session_id);
        }
    }
}

struct Inner {
    children: Children,
    spawner: SharedSpawner,
    catalog: SharedCatalog,
    threads: Arc<Mutex<Threads>>,
    models: DroidCatalog,
}

/// `droidAdapter`.
#[derive(Clone)]
pub struct DroidAdapter {
    inner: Arc<Inner>,
}

impl DroidAdapter {
    pub fn new(ctx: &HarnessContext) -> Self {
        Self {
            inner: Arc::new(Inner {
                children: ctx.children.clone(),
                spawner: ctx.spawner.clone(),
                catalog: ctx.catalog.clone(),
                threads: Arc::default(),
                models: DroidCatalog::new(
                    ctx.children.clone(),
                    ctx.spawner.clone(),
                    ctx.catalog.clone(),
                ),
            }),
        }
    }

    /// The model catalog probe.
    pub fn catalog(&self) -> &DroidCatalog {
        &self.inner.models
    }
}

/// `ensureDroidRegistered`. A second call keeps the live adapter.
pub fn register(ctx: &HarnessContext) {
    if ctx.registry.is_registered(HarnessId::Droid) {
        return;
    }
    ctx.registry
        .register_harness(Arc::new(DroidAdapter::new(ctx)));
}

impl Inner {
    fn is_current(&self, session_id: &str, generation: u64) -> bool {
        self.threads.lock().generations.get(session_id) == Some(&generation)
    }

    /// `checkGeneration`.
    fn check_generation(&self, session_id: &str, generation: u64) -> Result<()> {
        if self.is_current(session_id, generation) {
            Ok(())
        } else {
            Err(anyhow!("cancelled"))
        }
    }

    /// `sendDroidTurn`. Turns on one thread run in order, and a stop or
    /// cancel drops every turn still waiting.
    async fn send_turn(&self, input: SendTurnInput, on_event: EventSink) -> Result<()> {
        let session_id = input.session.session_id.clone();
        let (generation, queued) = {
            let mut threads = self.threads.lock();
            (
                threads.generation(&session_id),
                threads.enqueue(&session_id),
            )
        };
        if let Some(previous) = queued.previous {
            previous.await;
        }
        let result = self.run_turn(&input, on_event, generation).await;
        drop(queued.done);
        self.threads.lock().dequeue(&session_id, queued.id);
        result
    }

    async fn run_turn(
        &self,
        input: &SendTurnInput,
        on_event: EventSink,
        generation: u64,
    ) -> Result<()> {
        let session_id = &input.session.session_id;
        if !self.is_current(session_id, generation) {
            return Ok(());
        }
        let live = match self
            .ensure_live(&input.session, on_event.clone(), generation)
            .await
        {
            Ok(live) => live,
            Err(error) => return self.fail_turn(session_id, generation, None, error).await,
        };
        let run = async {
            self.check_generation(session_id, generation)?;
            {
                let mut state = live.state.lock();
                state.on_event = on_event;
                state.runtime_mode = input.session.runtime_mode;
                state.planning = input.session.intent == Some(TurnIntent::Plan);
                state.cancelled = false;
                state.mute_updates = false;
                state.tool_kinds.clear();
            }
            self.apply_model_selection(&live, &input.session).await?;
            self.check_generation(session_id, generation)?;
            let (runtime_mode, planning) = {
                let state = live.state.lock();
                (state.runtime_mode, state.planning)
            };
            self.apply_runtime_mode(&live, runtime_mode, planning)
                .await?;
            self.check_generation(session_id, generation)?;
            prompt(&live, input).await
        };
        match run.await {
            Ok(()) => Ok(()),
            Err(error) => {
                self.fail_turn(session_id, generation, Some(&live), error)
                    .await
            }
        }
    }

    /// The `catch` of `sendDroidTurn`: a cancelled turn ends quietly, and a
    /// failed one stops the connection it owns.
    async fn fail_turn(
        &self,
        session_id: &str,
        generation: u64,
        live: Option<&Arc<Live>>,
        error: anyhow::Error,
    ) -> Result<()> {
        if !self.is_current(session_id, generation) || live.is_some_and(|live| live.cancelled()) {
            return Ok(());
        }
        let current = self.threads.lock().live_by_thread.get(session_id).cloned();
        let owned = match (live, current) {
            (None, _) => true,
            (Some(live), Some(current)) => Arc::ptr_eq(live, &current),
            (Some(_), None) => false,
        };
        if owned {
            self.stop_session(session_id, true, false).await;
        }
        Err(error)
    }

    fn respond_approval(&self, session_id: &str, request_id: i64, decision: ApprovalDecision) {
        let live = self.threads.lock().live_by_thread.get(session_id).cloned();
        if let Some(waiter) = live.and_then(|live| live.approvals.lock().remove(&request_id)) {
            let _ = waiter.send(Some(decision));
        }
    }

    /// `stopDroidSession`. `invalidate` drops the thread's queued turns.
    /// Turns queued after this call wait for the connection to close.
    async fn stop_session(&self, session_id: &str, invalidate: bool, send_cancel: bool) {
        let queued = {
            let mut threads = self.threads.lock();
            if invalidate {
                threads.generations.remove(session_id);
            }
            threads.enqueue(session_id)
        };
        self.stop_connection(session_id, send_cancel).await;
        drop(queued.done);
        let threads = self.threads.clone();
        let session_id = session_id.to_string();
        let tail = queued.tail;
        self.spawner.spawn(Box::pin(async move {
            tail.await;
            threads.lock().dequeue(&session_id, queued.id);
        }));
    }

    /// `stopConnection`. The flags are set before the first await, so a
    /// permission handler running now sees the stop.
    async fn stop_connection(&self, session_id: &str, send_cancel: bool) {
        let (live, starting) = {
            let mut threads = self.threads.lock();
            (
                threads.live_by_thread.remove(session_id),
                threads.starting_by_thread.remove(session_id),
            )
        };
        if let Some(starting) = starting {
            starting.close(None);
        }
        if let Some(live) = &live {
            {
                let mut state = live.state.lock();
                state.mute_updates = true;
                state.cancelled = true;
            }
            live.resolve_approvals();
            if send_cancel {
                let _ = live
                    .acp
                    .notify(
                        "session/cancel",
                        Some(json!({ "sessionId": live.acp_session_id })),
                    )
                    .await;
            }
            // Let in-flight permission handlers send their replies first.
            drop(live.permission_tasks.write().await);
            live.state.lock().closed = true;
            live.acp.close(None);
        }
        self.children.unwatch_child(session_id);
        let _ = self.children.kill_child(session_id).await;
    }

    async fn forget_session(&self, session_id: &str) {
        self.threads.lock().resume_by_thread.remove(session_id);
        self.stop_session(session_id, true, false).await;
    }

    fn bind_session(&self, thread_id: &str, acp_session_id: &str, cwd: &str) {
        let session_id = js::trim(acp_session_id);
        if thread_id.is_empty() || session_id.is_empty() || js::trim(cwd).is_empty() {
            return;
        }
        self.threads.lock().resume_by_thread.insert(
            thread_id.to_string(),
            Resume {
                acp_session_id: session_id.to_string(),
                cwd: cwd.to_string(),
            },
        );
    }

    /// `ensureLive`.
    async fn ensure_live(
        &self,
        input: &HarnessSessionInput,
        on_event: EventSink,
        generation: u64,
    ) -> Result<Arc<Live>> {
        let session_id = input.session_id.clone();
        let existing = self.threads.lock().live_by_thread.get(&session_id).cloned();
        if let Some(existing) = &existing
            && existing.cwd == input.cwd
        {
            return Ok(existing.clone());
        }
        if existing.is_some() {
            self.threads.lock().resume_by_thread.remove(&session_id);
            self.stop_session(&session_id, false, false).await;
            self.check_generation(&session_id, generation)?;
        }

        let resume = {
            let mut threads = self.threads.lock();
            let resume = threads.resume_by_thread.get(&session_id).cloned();
            if resume
                .as_ref()
                .is_some_and(|resume| resume.cwd != input.cwd)
            {
                threads.resume_by_thread.remove(&session_id);
            }
            resume.filter(|resume| resume.cwd == input.cwd)
        };

        let path = self.children.resolve_droid_binary().await?.path;
        self.check_generation(&session_id, generation)?;
        let wiring: Arc<Wiring<Live>> = Wiring::new();
        let handlers = AcpHandlers::default()
            .on_notification({
                let wiring = wiring.clone();
                move |method: &str, params: Value| {
                    let live = wiring.live();
                    // Config snapshots matter even while a session/load replay is muted.
                    if let Some(live) = &live
                        && method == "session/update"
                        && let Some(options) = droid_config_options_from(&params)
                    {
                        live.sync_config(options);
                        return;
                    }
                    if wiring.muted() {
                        return;
                    }
                    let Some(live) = live else {
                        return;
                    };
                    if live.state.lock().mute_updates {
                        return;
                    }
                    handle_notification(&live, method, &params);
                }
            })
            .on_request({
                let wiring = wiring.clone();
                let spawner = self.spawner.clone();
                move |id: i64, method: &str, params: Value| match wiring.live() {
                    None => {
                        if let Some(acp) = wiring.acp() {
                            spawn_method_not_found(&spawner, acp, id, method);
                        }
                    }
                    Some(live) => {
                        // Taken here, before the task runs, so a stop that
                        // starts now still waits for this reply.
                        let guard = live.permission_tasks.try_read_arc();
                        let method = method.to_string();
                        spawner.spawn(Box::pin(async move {
                            let _guard = guard;
                            if let Err(error) = handle_request(&live, id, &method, &params).await
                                && !live.retired()
                            {
                                live.emit(HarnessEvent::SessionError {
                                    message: droid_error_message(&error),
                                });
                            }
                        }));
                    }
                }
            });
        let acp = AcpClient::new(&session_id, Arc::new(self.children.clone()), handlers);
        wiring.set_acp(&acp);

        let emit = {
            let wiring = wiring.clone();
            let fallback = on_event.clone();
            Arc::new(move |event: HarnessEvent| match wiring.live() {
                Some(live) => live.emit(event),
                None => fallback(event),
            })
        };
        self.children.watch_child_with(
            &session_id,
            ChildHandlers {
                on_line: Box::new({
                    let acp = acp.clone();
                    move |line| acp.push_line(&line)
                }),
                on_exit: Box::new({
                    let acp = acp.clone();
                    let threads = self.threads.clone();
                    let session_id = session_id.clone();
                    let emit = emit.clone();
                    let wiring = wiring.clone();
                    move |code| {
                        let live = wiring.live();
                        if let Some(live) = &live {
                            {
                                let mut state = live.state.lock();
                                state.closed = true;
                                state.mute_updates = true;
                            }
                            live.resolve_approvals();
                        }
                        acp.close(Some("Factory Droid exited"));
                        if let Some(live) = &live {
                            let mut threads = threads.lock();
                            if threads
                                .live_by_thread
                                .get(&session_id)
                                .is_some_and(|current| Arc::ptr_eq(current, live))
                            {
                                threads.live_by_thread.remove(&session_id);
                            }
                        }
                        let ended = HarnessEvent::SessionEnded {
                            code: code.map(i64::from),
                        };
                        match &live {
                            Some(live) => live.emit(ended),
                            None => emit(ended),
                        }
                    }
                }),
                on_stderr: Some(Box::new({
                    let emit = emit.clone();
                    move |line| {
                        log::debug!("[monocode] droid stderr {line}");
                        if is_droid_auth_error(&line) {
                            emit(HarnessEvent::SessionError {
                                message: format!("{}\n\n{DROID_AUTH_HELP}", js::trim(&line)),
                            });
                        }
                    }
                })),
            },
        );

        self.threads
            .lock()
            .starting_by_thread
            .insert(session_id.clone(), acp.clone());
        let started = async {
            self.children
                .spawn_child(
                    &session_id,
                    &path,
                    DROID_ACP_ARGS.iter().map(|arg| arg.to_string()).collect(),
                    &input.cwd,
                    None,
                    Some(HarnessId::Droid),
                )
                .await?;
            self.start_session(input, &acp, &wiring, resume.as_ref(), on_event, generation)
                .await
        };
        match started.await {
            Ok(live) => Ok(live),
            Err(error) => {
                acp.close(Some(&error.to_string()));
                wiring.clear();
                // Keep the resume binding: the next turn retries the load.
                self.stop_session(&session_id, false, false).await;
                Err(error)
            }
        }
    }

    /// The `try` block of `ensureLive`, after the spawn.
    async fn start_session(
        &self,
        input: &HarnessSessionInput,
        acp: &AcpClient,
        wiring: &Arc<Wiring<Live>>,
        resume: Option<&Resume>,
        on_event: EventSink,
        generation: u64,
    ) -> Result<Arc<Live>> {
        let session_id = &input.session_id;
        self.check_generation(session_id, generation)?;
        acp.request_value(
            "initialize",
            Some(initialize_params("monocode")),
            INIT_TIMEOUT_MS,
        )
        .await
        .map_err(|error| droid_startup_error(&error))?;

        self.check_generation(session_id, generation)?;
        let mut setup = Value::Null;
        let mut acp_session_id: Option<String> = None;
        let mut did_load = false;
        if let Some(resume) = resume {
            // A failed load fails the turn instead of starting a new session,
            // so a transient error does not lose the conversation.
            wiring.set_muted(true);
            let loaded = acp
                .request_value(
                    "session/load",
                    Some(json!({
                        "sessionId": resume.acp_session_id,
                        "cwd": input.cwd,
                        "mcpServers": [],
                    })),
                    SESSION_TIMEOUT_MS,
                )
                .await;
            wiring.set_muted(false);
            let result = loaded?;
            acp_session_id =
                Some(droid_session_id(&result).unwrap_or_else(|| resume.acp_session_id.clone()));
            setup = result;
            did_load = true;
        }

        self.check_generation(session_id, generation)?;
        if acp_session_id.is_none() {
            setup = acp
                .request_value(
                    "session/new",
                    Some(json!({ "cwd": input.cwd, "mcpServers": [] })),
                    SESSION_TIMEOUT_MS,
                )
                .await
                .map_err(|error| droid_startup_error(&error))?;
            acp_session_id = droid_session_id(&setup);
        }
        self.check_generation(session_id, generation)?;
        let Some(acp_session_id) = acp_session_id else {
            return Err(anyhow!("Factory Droid did not return a session id"));
        };

        let live = Arc::new(Live {
            subagents: Mutex::new(AcpSubagents::new()),
            acp: acp.clone(),
            acp_session_id: acp_session_id.clone(),
            cwd: input.cwd.clone(),
            config: Mutex::new(ConfigOptions {
                generation: 0,
                options: droid_config_options_from(&setup).unwrap_or_default(),
            }),
            state: Mutex::new(LiveState {
                model_id: droid_current_model_id(&setup).unwrap_or_default(),
                mode_id: String::new(),
                mode_revision: 0,
                mute_updates: did_load,
                cancelled: false,
                closed: false,
                runtime_mode: input.runtime_mode,
                planning: input.intent == Some(TurnIntent::Plan),
                on_event,
                tool_kinds: HashMap::new(),
            }),
            approvals: Mutex::default(),
            permission_tasks: Arc::new(smol::lock::RwLock::new(())),
        });
        {
            let mut threads = self.threads.lock();
            threads.starting_by_thread.remove(session_id);
            threads
                .live_by_thread
                .insert(session_id.clone(), live.clone());
            threads.resume_by_thread.insert(
                session_id.clone(),
                Resume {
                    acp_session_id: acp_session_id.clone(),
                    cwd: input.cwd.clone(),
                },
            );
        }
        wiring.set_live(&live);
        live.emit(HarnessEvent::SessionProviderBound {
            provider_session_id: acp_session_id,
        });
        self.check_generation(session_id, generation)?;
        live.emit(HarnessEvent::SessionStarted);
        self.check_generation(session_id, generation)?;
        if !self.catalog.has_live_catalog(HarnessId::Droid) {
            // A running session already knows Droid's models; show them now
            // and let the catalog probe fill in per-model reasoning levels.
            let models = models_from_droid_session(&setup, &HashMap::new());
            if !models.is_empty() {
                self.catalog
                    .set_harness_catalog(HarnessId::Droid, models, false);
            }
            // Remote hosts discover catalogs with their project directory.
            if !self.children.has_headless_child_backend() {
                let probe = self.models.clone();
                self.spawner
                    .spawn(Box::pin(async move { probe.refresh().await }));
            }
        }
        Ok(live)
    }

    /// `setConfigOption`. A snapshot in the reply must show the requested
    /// value for any setting other than the model.
    async fn set_config_option(&self, live: &Live, config_id: &str, value: &str) -> Result<()> {
        let (generation, current) = {
            let config = live.config.lock();
            let current = config
                .options
                .iter()
                .find(|option| option.id == config_id)
                .cloned();
            (config.generation, current)
        };
        if current
            .as_ref()
            .is_some_and(|current| current.current_value.as_deref() == Some(value))
        {
            return Ok(());
        }
        let result = live
            .acp
            .request_value(
                "session/set_config_option",
                Some(json!({ "sessionId": live.acp_session_id, "configId": config_id, "value": value })),
                CONTROL_TIMEOUT_MS,
            )
            .await?;
        if let Some(options) = droid_config_options_from(&result) {
            let is_model = droid_model_config(&options).is_some_and(|model| model.id == config_id);
            let effective = options
                .iter()
                .find(|option| option.id == config_id)
                .and_then(|option| option.current_value.clone());
            live.sync_config(options);
            if !is_model && effective.is_some_and(|effective| effective != value) {
                return Err(anyhow!(
                    "Factory Droid did not apply the requested {config_id}"
                ));
            }
            return Ok(());
        }
        let mut config = live.config.lock();
        let is_model =
            droid_model_config(&config.options).is_some_and(|model| model.id == config_id);
        if current.is_some()
            && config.generation == generation
            && !is_model
            && let Some(option) = config
                .options
                .iter_mut()
                .find(|option| option.id == config_id)
        {
            option.current_value = Some(value.to_string());
        }
        Ok(())
    }

    /// `applyModelSelection`.
    async fn apply_model_selection(&self, live: &Live, input: &HarnessSessionInput) -> Result<()> {
        let native = self.catalog.read().native_model_id_for(&input.model);
        let model_id = js::trim(&native).to_string();
        let requested_effort = input.model_settings.as_ref().and_then(|settings| {
            settings
                .get("effort")
                .or_else(|| settings.get("reasoning"))
                .cloned()
        });
        let current = live.state.lock().model_id.clone();
        let switching = !model_id.is_empty() && model_id != "default" && model_id != current;
        if switching {
            let generation = live.config_generation();
            let config_id = droid_model_config(&live.options())
                .map(|config| config.id.clone())
                .unwrap_or_else(|| "model".into());
            if let Err(error) = self.set_config_option(live, &config_id, &model_id).await {
                // Older Droid builds only have session/set_model.
                if !UNSUPPORTED_CONTROL.is_match(&error.to_string()) {
                    return Err(error);
                }
                live.acp
                    .request_value(
                        "session/set_model",
                        Some(json!({ "sessionId": live.acp_session_id, "modelId": model_id })),
                        CONTROL_TIMEOUT_MS,
                    )
                    .await?;
            }
            if live.config_generation() == generation {
                live.state.lock().model_id = model_id.clone();
            }
        }

        // Reasoning levels are per model, so apply effort once Droid reports
        // the new model's options.
        if switching && requested_effort.is_some() {
            let deadline = Instant::now() + task::ms(CONTROL_TIMEOUT_MS);
            while droid_model_config(&live.options())
                .and_then(|config| config.current_value.clone())
                .as_deref()
                != Some(model_id.as_str())
            {
                if live.retired() {
                    return Ok(());
                }
                if Instant::now() >= deadline {
                    return Err(anyhow!(
                        "Factory Droid did not confirm the selected model configuration"
                    ));
                }
                task::sleep(Duration::from_millis(10)).await;
            }
        }
        let options = live.options();
        let effort = droid_effort_config(&options);
        let value = droid_effort_value(effort, input.model_settings.as_ref());
        if let Some(requested) = &requested_effort
            && value.is_none()
        {
            return Err(anyhow!(
                "Factory Droid does not support reasoning effort {requested} for the selected model"
            ));
        }
        if let (Some(effort), Some(value)) = (effort, value) {
            self.set_config_option(live, &effort.id, &value).await?;
        }
        Ok(())
    }

    /// `applyRuntimeMode`.
    async fn apply_runtime_mode(
        &self,
        live: &Live,
        runtime_mode: RuntimeMode,
        planning: bool,
    ) -> Result<()> {
        let mode_id = droid_mode_id(runtime_mode, planning).as_str();
        let revision = {
            let state = live.state.lock();
            if state.mode_id == mode_id {
                return Ok(());
            }
            state.mode_revision
        };
        let set = live
            .acp
            .request_value(
                "session/set_mode",
                Some(json!({ "sessionId": live.acp_session_id, "modeId": mode_id })),
                CONTROL_TIMEOUT_MS,
            )
            .await;
        if set.is_err() {
            self.set_config_option(live, "autonomy_level", mode_id)
                .await?;
        }
        let mut state = live.state.lock();
        if state.mode_revision == revision {
            state.mode_id = mode_id.to_string();
        }
        Ok(())
    }
}

static UNSUPPORTED_CONTROL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)method not found|unsupported").unwrap());

/// `prompt`.
async fn prompt(live: &Live, input: &SendTurnInput) -> Result<()> {
    let run = async {
        let attachments = input.attachments.as_deref().unwrap_or(&[]);
        let blocks =
            droid_prompt_blocks(&input.text, attachments).map_err(|error| anyhow!(error))?;
        if blocks.is_empty() {
            return Ok::<(), anyhow::Error>(());
        }
        live.acp
            .request_value(
                "session/prompt",
                Some(json!({ "sessionId": live.acp_session_id, "prompt": blocks })),
                PROMPT_TIMEOUT_MS,
            )
            .await?;
        Ok(())
    };
    match run.await {
        Ok(()) => Ok(()),
        Err(_) if live.cancelled() => Ok(()),
        Err(error) => {
            let detail = droid_error_message(&error);
            live.emit(HarnessEvent::SessionError {
                message: if is_droid_auth_error(&detail) {
                    format!("{detail}\n\n{DROID_AUTH_HELP}")
                } else {
                    detail
                },
            });
            Err(error)
        }
    }
}

/// `handleNotification`.
fn handle_notification(live: &Live, method: &str, params: &Value) {
    if method != "session/update" {
        return;
    }
    let update = &params["update"];
    if update["sessionUpdate"] == "current_mode_update"
        && let Some(mode) = update["currentModeId"].as_str()
    {
        let mut state = live.state.lock();
        state.mode_id = mode.to_string();
        state.mode_revision += 1;
    }
    if is_droid_error_echo(params) {
        return;
    }
    let events = events_from_acp_update(params);
    let routed = live.subagents.lock().route(params, events);
    for event in routed {
        if let HarnessEvent::ToolStarted {
            call_id,
            kind: Some(kind),
            ..
        }
        | HarnessEvent::ToolUpdated {
            call_id,
            kind: Some(kind),
            ..
        } = &event
        {
            live.state
                .lock()
                .tool_kinds
                .insert(call_id.clone(), kind.clone());
        }
        live.emit(event);
    }
}

/// `handleRequest`.
async fn handle_request(live: &Live, id: i64, method: &str, params: &Value) -> Result<()> {
    if method == "session/request_permission" {
        return handle_permission(live, id, params).await;
    }
    respond_method_not_found(&live.acp, id, method).await;
    Ok(())
}

/// `handlePermission`. A cancelled turn answers `cancelled`, and a closed
/// connection gets no reply.
async fn handle_permission(live: &Live, id: i64, params: &Value) -> Result<()> {
    let (closed, cancelled) = {
        let state = live.state.lock();
        (state.closed, state.cancelled)
    };
    if closed {
        return Ok(());
    }
    if cancelled {
        return respond_permission(live, id, None).await;
    }
    let mut request = permission_request_from_acp(params);
    if request.kind.is_none()
        && let Some(call_id) = &request.call_id
    {
        request.kind = live.state.lock().tool_kinds.get(call_id).cloned();
    }
    if let Some(call_id) = &request.call_id {
        live.emit(HarnessEvent::ToolUpdated {
            agent_model: None,
            call_id: call_id.clone(),
            title: Some(request.title.clone()),
            kind: request.kind.clone(),
            status: None,
            detail: None,
            preview: request.preview.clone(),
            paths: None,
        });
    }
    if live.retired() {
        return respond_permission(live, id, None).await;
    }

    let (planning, runtime_mode) = {
        let state = live.state.lock();
        (state.planning, state.runtime_mode)
    };
    if planning {
        // Spec mode ends by asking to leave it. Surface the spec as MonoCode's
        // plan and stay read-only; the user decides whether to build it.
        if let Some(plan) = droid_spec_plan(params) {
            live.emit(HarnessEvent::Plan {
                text: plan,
                key: None,
                append: None,
                streaming: Some(false),
            });
        }
        let read_only = matches!(request.kind.as_deref(), Some("read" | "search"));
        let decision = if read_only {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        };
        let option_id = permission_option_id(decision, &request.option_ids, &request.option_kinds);
        return respond_permission(live, id, option_id).await;
    }

    if let Some(automatic) = pick_auto_option(
        runtime_mode,
        request.kind.as_deref(),
        &request.option_ids,
        &request.option_kinds,
    ) {
        return respond_permission(live, id, Some(automatic)).await;
    }

    // Register the waiter under the state lock, so a stop either sees it or
    // this sees the stop.
    let waiter = {
        let state = live.state.lock();
        if state.cancelled || state.closed {
            None
        } else {
            let (tx, rx) = oneshot::channel();
            live.approvals.lock().insert(id, tx);
            Some(rx)
        }
    };
    let Some(waiter) = waiter else {
        return respond_permission(live, id, None).await;
    };
    live.emit(HarnessEvent::ApprovalRequested {
        request_id: id,
        title: request.title.clone(),
        kind: request.kind.clone(),
        call_id: request.call_id.clone(),
        preview: request.preview.clone(),
    });
    let decision = waiter.await.ok().flatten();
    live.approvals.lock().remove(&id);
    live.emit(HarnessEvent::ApprovalResolved {
        request_id: id,
        decision: decided(decision.unwrap_or(ApprovalDecision::Deny)),
    });
    let option_id = match decision {
        Some(decision) if !live.cancelled() => {
            permission_option_id(decision, &request.option_ids, &request.option_kinds)
        }
        _ => None,
    };
    respond_permission(live, id, option_id).await
}

/// `respondPermission`.
async fn respond_permission(live: &Live, id: i64, option_id: Option<String>) -> Result<()> {
    let option_id = {
        let state = live.state.lock();
        if state.closed {
            return Ok(());
        }
        option_id.filter(|_| !state.cancelled)
    };
    live.acp
        .respond(id, permission_outcome(option_id.as_deref()))
        .await
}

impl HarnessAdapter for DroidAdapter {
    fn id(&self) -> HarnessId {
        HarnessId::Droid
    }

    /// Droid runs one prompt at a time; MonoCode queues follow-ups.
    fn can_steer(&self) -> bool {
        false
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            refresh_catalog: true,
            ..AdapterCapabilities::default()
        }
    }

    fn send_turn(
        &self,
        input: SendTurnInput,
        on_event: EventSink,
        _on_accepted: Option<AcceptedHook>,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move { self.inner.send_turn(input, on_event).await })
    }

    fn steer_turn(&self, _input: SteerTurnInput) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Err(anyhow!("Factory Droid cannot accept a message mid-turn")) })
    }

    fn cancel_turn(&self, session_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // `cancelDroidTurn`: tell Droid, then retire the connection. The
            // next turn resumes the session on a fresh process.
            self.inner.stop_session(&session_id, true, true).await;
            Ok(())
        })
    }

    fn respond_approval(&self, session_id: &str, request_id: i64, decision: ApprovalDecision) {
        self.inner
            .respond_approval(session_id, request_id, decision);
    }

    fn stop_session(&self, session_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.inner.stop_session(&session_id, true, false).await;
            Ok(())
        })
    }

    fn forget_session(&self, session_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.inner.forget_session(&session_id).await;
            Ok(())
        })
    }

    fn bind_session(
        &self,
        thread_id: &str,
        provider_session_id: &str,
        cwd: &str,
        _provider_account_id: Option<&str>,
    ) {
        self.inner.bind_session(thread_id, provider_session_id, cwd);
    }

    fn refresh_catalog(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.inner.models.refresh().await;
            Ok(())
        })
    }
}

#[cfg(test)]
#[path = "adapter_tests.rs"]
mod tests;

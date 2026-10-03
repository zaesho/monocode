//! Port of src/integrations/harness/providers/droid/droid.ts and
//! droidAdapter.ts: the live Factory Droid adapter. It spawns
//! `droid exec --output-format acp`.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use parking_lot::Mutex;
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
use crate::core::task::{BoxFuture, SharedSpawner};
use crate::providers::grok::adapter::decided;
use crate::providers::grok::protocol::{
    events_from_acp_update, permission_option_id_with_kinds, permission_outcome,
    permission_request_from_acp, pick_auto_option_with_kinds,
};
use crate::providers::grok::shared::{
    Wiring, initialize_params, respond_method_not_found, spawn_method_not_found,
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
    mode_generation: u64,
    mute_updates: bool,
    cancelled: bool,
    runtime_mode: RuntimeMode,
    planning: bool,
    on_event: EventSink,
}

/// Droid's config options, with a count of whole-list replacements. The
/// TypeScript updated `current.currentValue` on the option object it read
/// before awaiting, which only shows if the list was not replaced meanwhile.
#[derive(Default)]
struct ConfigOptions {
    generation: u64,
    options: Vec<DroidConfigOption>,
}

impl ConfigOptions {
    fn replace(&mut self, options: Vec<DroidConfigOption>) {
        self.generation += 1;
        self.options = options;
    }
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
    tool_kinds: Mutex<HashMap<String, String>>,
    permission_tasks: AtomicUsize,
}

impl Live {
    fn emit(&self, event: HarnessEvent) {
        let sink = self.state.lock().on_event.clone();
        sink(event);
    }

    fn cancelled(&self) -> bool {
        self.state.lock().cancelled
    }

    fn options(&self) -> Vec<DroidConfigOption> {
        self.config.lock().options.clone()
    }

    /// `resolveApprovals`.
    fn resolve_approvals(&self) {
        for (_, waiter) in self.approvals.lock().drain() {
            let _ = waiter.send(None);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Resume {
    acp_session_id: String,
    cwd: String,
}

#[derive(Default)]
struct Threads {
    live_by_thread: HashMap<String, Arc<Live>>,
    resume_by_thread: HashMap<String, Resume>,
    generations: HashMap<String, Arc<AtomicBool>>,
    queues: HashMap<String, Arc<smol::lock::Mutex<()>>>,
    starting: HashMap<String, AcpClient>,
    retiring: HashMap<String, Arc<AtomicUsize>>,
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
    /// `sendDroidTurn`.
    async fn send_turn(&self, input: SendTurnInput, on_event: EventSink) -> Result<()> {
        let session_id = input.session.session_id.clone();
        let (generation, queue) = {
            let mut threads = self.threads.lock();
            let generation = threads
                .generations
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(AtomicBool::new(true)))
                .clone();
            let queue = threads
                .queues
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(smol::lock::Mutex::new(())))
                .clone();
            (generation, queue)
        };
        let _turn = queue.lock().await;
        let retiring = self.threads.lock().retiring.get(&session_id).cloned();
        if let Some(retiring) = retiring {
            while retiring.load(Ordering::SeqCst) > 0 {
                crate::core::task::sleep(crate::core::task::ms(1)).await;
            }
        }
        if !generation.load(Ordering::SeqCst) {
            return Ok(());
        }
        let live = match self
            .ensure_live(&input.session, on_event.clone(), &generation)
            .await
        {
            Ok(live) => live,
            Err(_) if !generation.load(Ordering::SeqCst) => return Ok(()),
            Err(error) => return Err(error),
        };
        if !generation.load(Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut state = live.state.lock();
            state.on_event = on_event;
            state.runtime_mode = input.session.runtime_mode;
            state.planning = input.session.intent == Some(TurnIntent::Plan);
        }
        let result = {
            {
                let mut state = live.state.lock();
                state.cancelled = false;
                state.mute_updates = false;
            }
            let run = async {
                self.apply_model_selection(&live, &input.session).await?;
                if live.cancelled() {
                    return Ok(());
                }
                let (runtime_mode, planning) = {
                    let state = live.state.lock();
                    (state.runtime_mode, state.planning)
                };
                self.apply_runtime_mode(&live, runtime_mode, planning)
                    .await?;
                if live.cancelled() {
                    return Ok(());
                }
                prompt(&live, &input).await
            };
            match run.await {
                Err(_) if live.cancelled() => Ok(()),
                other => other,
            }
        };
        if let Err(error) = result {
            let current = self.threads.lock().live_by_thread.get(&session_id).cloned();
            if current.is_some_and(|current| Arc::ptr_eq(&current, &live)) {
                self.stop_session(&session_id).await;
            }
            return Err(error);
        }
        Ok(())
    }

    fn respond_approval(&self, session_id: &str, request_id: i64, decision: ApprovalDecision) {
        let live = self.threads.lock().live_by_thread.get(session_id).cloned();
        if let Some(waiter) = live.and_then(|live| live.approvals.lock().remove(&request_id)) {
            let _ = waiter.send(Some(decision));
        }
    }

    /// `cancelDroidTurn`.
    async fn cancel_turn(&self, session_id: &str) {
        self.retire_session(session_id, true).await;
    }

    async fn stop_session(&self, session_id: &str) {
        self.retire_session(session_id, false).await;
    }

    async fn retire_session(&self, session_id: &str, send_cancel: bool) {
        let retiring = {
            let mut threads = self.threads.lock();
            if let Some(generation) = threads.generations.remove(session_id) {
                generation.store(false, Ordering::SeqCst);
            }
            let retiring = threads
                .retiring
                .entry(session_id.to_string())
                .or_default()
                .clone();
            retiring.fetch_add(1, Ordering::SeqCst);
            retiring
        };
        self.stop_connection_with_cancel(session_id, send_cancel)
            .await;
        retiring.fetch_sub(1, Ordering::SeqCst);
    }

    async fn stop_connection(&self, session_id: &str) {
        self.stop_connection_with_cancel(session_id, false).await;
    }

    async fn stop_connection_with_cancel(&self, session_id: &str, send_cancel: bool) {
        let (live, starting) = {
            let mut threads = self.threads.lock();
            (
                threads.live_by_thread.remove(session_id),
                threads.starting.remove(session_id),
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
            let _ = crate::core::task::timeout(crate::core::task::ms(1000), async {
                while live.permission_tasks.load(Ordering::SeqCst) > 0 {
                    crate::core::task::sleep(crate::core::task::ms(1)).await;
                }
            })
            .await;
            live.acp.close(None);
        }
        self.children.unwatch_child(session_id);
        let _ = self.children.kill_child(session_id).await;
    }

    async fn forget_session(&self, session_id: &str) {
        self.threads.lock().resume_by_thread.remove(session_id);
        self.stop_session(session_id).await;
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
        generation: &Arc<AtomicBool>,
    ) -> Result<Arc<Live>> {
        let session_id = input.session_id.clone();
        let existing = self.threads.lock().live_by_thread.get(&session_id).cloned();
        if let Some(existing) = &existing
            && existing.cwd == input.cwd
        {
            let mut state = existing.state.lock();
            state.on_event = on_event;
            state.runtime_mode = input.runtime_mode;
            state.planning = input.intent == Some(TurnIntent::Plan);
            return Ok(existing.clone());
        }
        if existing.is_some() {
            self.threads.lock().resume_by_thread.remove(&session_id);
            self.stop_connection(&session_id).await;
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
        check_generation(generation)?;
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
                        sync_config(live, options);
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
                        let method = method.to_string();
                        live.permission_tasks.fetch_add(1, Ordering::SeqCst);
                        spawner.spawn(Box::pin(async move {
                            handle_request(&live, id, &method, &params).await;
                            live.permission_tasks.fetch_sub(1, Ordering::SeqCst);
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
                    let children = self.children.clone();
                    move |code| {
                        let live = wiring.live();
                        if let Some(live) = &live {
                            {
                                let mut state = live.state.lock();
                                state.mute_updates = true;
                                acp.close(Some("Factory Droid exited"));
                            }
                            live.resolve_approvals();
                        } else {
                            acp.close(Some("Factory Droid exited"));
                        }
                        threads.lock().live_by_thread.remove(&session_id);
                        wiring.clear();
                        children.unwatch_child(&session_id);
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
            .starting
            .insert(session_id.clone(), acp.clone());
        let spawned = self
            .children
            .spawn_child(
                &session_id,
                &path,
                DROID_ACP_ARGS.iter().map(|arg| arg.to_string()).collect(),
                &input.cwd,
                None,
                Some(HarnessId::Droid),
            )
            .await;
        if let Err(error) = spawned {
            acp.close(Some(&error.to_string()));
            wiring.clear();
            self.stop_connection(&session_id).await;
            return Err(error);
        }
        if let Err(error) = check_generation(generation) {
            acp.close(None);
            wiring.clear();
            self.stop_connection(&session_id).await;
            return Err(error);
        }

        match self
            .start_session(input, &acp, &wiring, resume.as_ref(), on_event, generation)
            .await
        {
            Ok(live) => Ok(live),
            Err(error) => {
                acp.close(Some(&error.to_string()));
                wiring.clear();
                self.stop_connection(&session_id).await;
                Err(error)
            }
        }
    }

    /// The `try` block of `ensureLive`.
    async fn start_session(
        &self,
        input: &HarnessSessionInput,
        acp: &AcpClient,
        wiring: &Arc<Wiring<Live>>,
        resume: Option<&Resume>,
        on_event: EventSink,
        generation: &Arc<AtomicBool>,
    ) -> Result<Arc<Live>> {
        acp.request_value(
            "initialize",
            Some(initialize_params("monocode")),
            INIT_TIMEOUT_MS,
        )
        .await
        .map_err(|error| droid_startup_error(&error))?;

        check_generation(generation)?;
        let mut setup = Value::Null;
        let mut acp_session_id: Option<String> = None;
        let mut did_load = false;
        if let Some(resume) = resume {
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
            if let Ok(result) = &loaded {
                acp_session_id =
                    Some(droid_session_id(result).unwrap_or_else(|| resume.acp_session_id.clone()));
                setup = result.clone();
                did_load = true;
            }
            loaded?;
        }

        check_generation(generation)?;
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
        check_generation(generation)?;
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
                mode_generation: 0,
                mute_updates: did_load,
                cancelled: false,
                runtime_mode: input.runtime_mode,
                planning: input.intent == Some(TurnIntent::Plan),
                on_event,
            }),
            approvals: Mutex::default(),
            tool_kinds: Mutex::default(),
            permission_tasks: AtomicUsize::new(0),
        });
        wiring.set_live(&live);
        {
            let mut threads = self.threads.lock();
            check_generation(generation)?;
            threads.starting.remove(&input.session_id);
            threads
                .live_by_thread
                .insert(input.session_id.clone(), live.clone());
            threads.resume_by_thread.insert(
                input.session_id.clone(),
                Resume {
                    acp_session_id: acp_session_id.clone(),
                    cwd: input.cwd.clone(),
                },
            );
        }
        live.emit(HarnessEvent::SessionProviderBound {
            provider_session_id: acp_session_id,
        });
        check_generation(generation)?;
        live.emit(HarnessEvent::SessionStarted);
        check_generation(generation)?;
        if !self.catalog.has_live_catalog(HarnessId::Droid) {
            // A running session already knows Droid's models; show them now
            // and let the catalog probe fill in per-model reasoning levels.
            let models = models_from_droid_session(&setup, &HashMap::new());
            if !models.is_empty() {
                self.catalog
                    .set_harness_models_complete(HarnessId::Droid, models, false);
            }
            let probe = self.models.clone();
            self.spawner
                .spawn(Box::pin(async move { probe.refresh().await }));
        }
        Ok(live)
    }

    /// `setConfigOption`.
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
        let mut config = live.config.lock();
        if let Some(options) = droid_config_options_from(&result) {
            drop(config);
            let effective = options
                .iter()
                .find(|option| option.id == config_id)
                .and_then(|option| option.current_value.clone());
            let model = droid_model_config(&options).map(|option| option.id.clone());
            sync_config(live, options);
            if model.as_deref() != Some(config_id)
                && effective
                    .as_deref()
                    .is_some_and(|effective| effective != value)
            {
                return Err(anyhow!(
                    "Factory Droid did not apply the requested {config_id}"
                ));
            }
        } else if current.is_some()
            && droid_model_config(&config.options).is_none_or(|option| option.id != config_id)
            && config.generation == generation
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
        let current = live.state.lock().model_id.clone();
        let requested_effort = input
            .model_settings
            .as_ref()
            .and_then(|settings| settings.get("effort").or_else(|| settings.get("reasoning")));
        let switching = !model_id.is_empty() && model_id != "default" && model_id != current;
        if switching {
            let generation = live.config.lock().generation;
            let config_id = droid_model_config(&live.options())
                .map(|config| config.id.clone())
                .unwrap_or_else(|| "model".into());
            if let Err(error) = self.set_config_option(live, &config_id, &model_id).await {
                let detail = error.to_string().to_lowercase();
                if !detail.contains("method not found") && !detail.contains("unsupported") {
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
            if live.config.lock().generation == generation {
                live.state.lock().model_id = model_id.clone();
            }
        }

        // Reasoning levels are per model, so apply effort after the model switch.
        if switching && requested_effort.is_some() {
            let deadline = std::time::Instant::now() + crate::core::task::ms(CONTROL_TIMEOUT_MS);
            while droid_model_config(&live.options())
                .and_then(|option| option.current_value.as_deref())
                != Some(&model_id)
            {
                if live.cancelled() || live.acp.is_closed() {
                    return Ok(());
                }
                if std::time::Instant::now() >= deadline {
                    return Err(anyhow!(
                        "Factory Droid did not confirm the selected model configuration"
                    ));
                }
                crate::core::task::sleep(crate::core::task::ms(10)).await;
            }
        }
        let options = live.options();
        let effort = droid_effort_config(&options);
        let value = droid_effort_value(effort, input.model_settings.as_ref());
        if requested_effort.is_some() && value.is_none() {
            return Err(anyhow!(
                "Factory Droid does not support the requested reasoning effort for the selected model"
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
        if live.state.lock().mode_id == mode_id {
            return Ok(());
        }
        let generation = live.state.lock().mode_generation;
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
        if state.mode_generation == generation {
            state.mode_id = mode_id.to_string();
        }
        Ok(())
    }
}

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
    if method != "session/update" || is_droid_error_echo(params) {
        return;
    }
    if params["update"]["sessionUpdate"] == "current_mode_update"
        && let Some(mode) = params["update"]["currentModeId"].as_str()
    {
        let mut state = live.state.lock();
        state.mode_id = mode.to_string();
        state.mode_generation += 1;
    }
    let events = events_from_acp_update(params);
    let routed = live.subagents.lock().route(params, events);
    for event in routed {
        match &event {
            HarnessEvent::ToolStarted {
                call_id,
                kind: Some(kind),
                ..
            }
            | HarnessEvent::ToolUpdated {
                call_id,
                kind: Some(kind),
                ..
            } => {
                live.tool_kinds.lock().insert(call_id.clone(), kind.clone());
            }
            _ => {}
        }
        live.emit(event);
    }
}

/// `handleRequest`.
async fn handle_request(live: &Live, id: i64, method: &str, params: &Value) {
    if method == "session/request_permission" {
        if let Err(error) = handle_permission(live, id, params).await {
            log::debug!("[monocode] droid permission reply {error:#}");
        }
        return;
    }
    respond_method_not_found(&live.acp, id, method).await;
}

/// `handlePermission`.
async fn handle_permission(live: &Live, id: i64, params: &Value) -> Result<()> {
    if live.acp.is_closed() {
        return Ok(());
    }
    if live.cancelled() {
        return respond_permission(live, id, None).await;
    }
    let mut request = permission_request_from_acp(params);
    if request.kind.is_none() {
        request.kind = request
            .call_id
            .as_ref()
            .and_then(|id| live.tool_kinds.lock().get(id).cloned());
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
    if live.cancelled() || live.acp.is_closed() {
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
        let option =
            permission_option_id_with_kinds(decision, &request.option_ids, &request.option_kinds);
        return respond_permission(live, id, option).await;
    }

    if let Some(automatic) = pick_auto_option_with_kinds(
        runtime_mode,
        request.kind.as_deref(),
        &request.option_ids,
        &request.option_kinds,
    ) {
        return respond_permission(live, id, Some(automatic)).await;
    }

    let receiver = {
        let state = live.state.lock();
        if state.cancelled || live.acp.is_closed() {
            None
        } else {
            let (tx, rx) = oneshot::channel();
            live.approvals.lock().insert(id, tx);
            Some(rx)
        }
    };
    let Some(rx) = receiver else {
        return respond_permission(live, id, None).await;
    };
    live.emit(HarnessEvent::ApprovalRequested {
        request_id: id,
        title: request.title.clone(),
        kind: request.kind.clone(),
        call_id: request.call_id.clone(),
        preview: request.preview.clone(),
    });
    let decision = rx.await.unwrap_or(None);
    live.approvals.lock().remove(&id);
    live.emit(HarnessEvent::ApprovalResolved {
        request_id: id,
        decision: decided(decision.unwrap_or(ApprovalDecision::Deny)),
    });
    let option = decision.and_then(|decision| {
        permission_option_id_with_kinds(decision, &request.option_ids, &request.option_kinds)
    });
    respond_permission(live, id, option).await
}

async fn respond_permission(live: &Live, id: i64, option: Option<String>) -> Result<()> {
    if live.acp.is_closed() {
        return Ok(());
    }
    let option = option.filter(|_| !live.cancelled());
    live.acp.respond(id, permission_outcome(option)).await
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
            self.inner.cancel_turn(&session_id).await;
            Ok(())
        })
    }

    fn respond_approval(&self, session_id: &str, request_id: i64, decision: ApprovalDecision) {
        self.inner
            .respond_approval(session_id, request_id, decision);
    }

    fn stop_session(&self, session_id: String) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.inner.stop_session(&session_id).await;
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

fn check_generation(generation: &AtomicBool) -> Result<()> {
    if generation.load(Ordering::SeqCst) {
        Ok(())
    } else {
        Err(anyhow!("cancelled"))
    }
}

fn sync_config(live: &Live, options: Vec<DroidConfigOption>) {
    {
        let mut state = live.state.lock();
        if let Some(model) =
            droid_model_config(&options).and_then(|option| option.current_value.clone())
        {
            state.model_id = model;
        }
        if let Some(mode) = options
            .iter()
            .find(|option| {
                option.category.as_deref() == Some("mode") || option.id == "autonomy_level"
            })
            .and_then(|option| option.current_value.clone())
        {
            state.mode_id = mode;
            state.mode_generation += 1;
        }
    }
    live.config.lock().replace(options);
}

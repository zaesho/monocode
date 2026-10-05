//! Port of host/engine.ts: runs provider turns for host-owned sessions,
//! applies their events with the transcript reducer, batches streamed
//! output, and saves `HostSession` snapshots that desktops sync.
//!
//! The Node host did this on one event loop. Here one lock over the
//! engine's state stands in for that loop: commands, provider events,
//! flushes, and settlement each run under it, and provider calls run after
//! it is released. The lock is always taken before the store's own lock,
//! never inside it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use base64::Engine as _;
use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use monocode_core::attachment::is_vision_image;
use monocode_core::block::{
    ApprovalDecided, Block, BlockNotice, BlockRole, HandoffMeta, HandoffStatus, PlanBlockMeta,
    PlanStatus, TransferMode, TransferStatus, TurnIntent, TurnModel,
};
use monocode_core::handoff::ComposerSwitchPlan;
use monocode_core::harness_event::{HarnessEvent, HarnessSessionInput, SendTurnInput};
use monocode_core::portable_context::{
    ContextAssetSnapshot, OmissionReason, PortableContext, PortableContextOptions,
    build_portable_context, build_portable_context_snapshot, current_attachment_tokens,
    historical_context_attachments,
};
use monocode_core::provider_context::{
    DeliveryCoverage, DeliveryStart, ProviderBinding, accept_provider_delivery,
    begin_provider_delivery, can_resume_provider_binding, confirm_provider_delivery_inspection,
    fail_provider_delivery, mark_provider_context_delivered, mark_provider_request_submitted,
    provider_binding, record_provider_bound, record_provider_context_usage,
    recover_submitted_provider_delivery, remember_provider_binding,
    requires_fresh_provider_binding, settle_provider_binding, update_provider_handoff,
};
use monocode_core::reducer::{SystemEnv, apply_harness_event_mut, now_ms, stop_streaming};
use monocode_core::session::{
    PendingHarnessSwitch, can_replace_session_title, format_session_title, title_from_prompt,
};
use monocode_core::{Attachment, HarnessId, Session};
use monocode_harness::core::SharedCatalog;
use monocode_harness::core::context_transfer::{
    ContextTransferInput, ContextTransferReceipt, DeliveredHook, DeliveryMode,
    prepare_context_transfer_input,
};
use monocode_harness::core::registry::{AcceptedHook, EventSink, TitleInput};
use monocode_harness::core::task::SharedSpawner;
use monocode_remote::host::HostStore;
use monocode_remote::host::attachments::resolve_attachments;
use monocode_remote::host::protocol::{
    ApprovalDecision, CommandReceipt, HostCommand, HostProject, HostSession, HostSessionStatus,
    HostSessionSummary, RemoteProvider, SendIntent, provider_name,
};
use monocode_remote::host::store::SessionPatch;
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::backend::{HostEngineOptions, HostHarness};
use crate::commands::parse_command;
use crate::context_assets::{ContextAssetLimits, snapshot_host_context_assets};
use crate::git_worktrees::{
    named_worktree_branch, rename_host_worktree_branch, resolve_host_worktree,
};
use crate::providers::{HostProvider, HostProviders};
use crate::workspace_commands::WorkspaceCommands;

/// Streamed output is written in batches. Anything a user may need to act
/// on (approvals, questions, errors, completion) is written immediately.
pub const FLUSH_MS: u64 = 120;

const PLACEHOLDER_TITLE: &str = "New remote session";
/// How long `close` waits for stopped turns to settle.
const CLOSE_WAIT: Duration = Duration::from_secs(10);
/// How long a persistent provider stays running after its session goes
/// idle, so a scheduled wakeup can still reach it.
const IDLE_PARK: Duration = Duration::from_secs(5 * 60);
const STORAGE_FAILED: &str =
    "Session storage failed during this turn. Inspect its work before continuing.";

/// `BATCHED`.
fn batched(event: &HarnessEvent) -> bool {
    matches!(
        event,
        HarnessEvent::MessageDelta { .. }
            | HarnessEvent::ReasoningDelta { .. }
            | HarnessEvent::ToolUpdated { .. }
            | HarnessEvent::AgentStep { .. }
            | HarnessEvent::Status { .. }
    )
}

/// `runningSessionsMessage`: names the running sessions a branch change
/// would disturb. The desktop recognizes the last sentence and asks before
/// retrying with `force`.
pub fn running_sessions_message(sessions: &[HostSessionSummary]) -> String {
    let names = sessions
        .iter()
        .take(3)
        .map(|session| format!("\"{}\"", session.title))
        .collect::<Vec<_>>()
        .join(", ");
    let more = if sessions.len() > 3 {
        format!(" and {} more", sessions.len() - 3)
    } else {
        String::new()
    };
    if sessions.len() == 1 {
        format!(
            "{names} is running on the host. Switching branches changes the files it is working on."
        )
    } else {
        format!(
            "{} sessions are running on the host: {names}{more}. Switching branches changes the files they are working on.",
            sessions.len()
        )
    }
}

type Done = Shared<BoxFuture<'static, ()>>;

/// A running turn.
struct Active {
    done: Done,
    cancelled: bool,
    persistence_failed: bool,
    /// The provider acknowledged the request, even if saving that failed.
    accepted: bool,
}

/// The shared history a turn delivers after a provider switch, or after a
/// delivery that left the target's conversation uncertain.
#[derive(Clone)]
struct Transfer {
    context: PortableContext,
    /// The full eligible history, for a target whose resume fails.
    fallback_context: PortableContext,
    switch_id: String,
    /// The target starts a new conversation instead of resuming its own.
    fresh: bool,
}

/// What the handoff row says about a delivery (`contextStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextStatus {
    Imported,
    Accepted,
    Uncertain,
}

/// `contextStatus`: word the handoff row for `switch_id` after the
/// delivery's state changed.
fn context_status(session: &mut Session, switch_id: &str, status: ContextStatus) {
    let Some(delivery) = session
        .provider_context
        .as_ref()
        .and_then(|state| state.delivery.as_ref())
        .filter(|delivery| delivery.switch_id == switch_id)
    else {
        return;
    };
    let mode = if delivery.mode == TransferMode::Native {
        "native messages"
    } else {
        "attributed history"
    };
    let detail = format!(
        "Delivered {} transcript items as {mode}. Omitted {}. Historical attachments remain file references.",
        delivery.included_block_ids.len(),
        delivery.omitted_block_ids.len()
    );
    let text = match status {
        ContextStatus::Accepted => format!("Continued with shared history. {detail}"),
        ContextStatus::Imported => format!("{detail} The current request awaits acceptance."),
        ContextStatus::Uncertain if delivery.needs_inspection() => "The provider request may already have run. Inspect its work, then confirm inspection before continuing.".to_string(),
        ContextStatus::Uncertain => "The provider did not acknowledge this transfer. The next turn will use a fresh conversation with shared history. You can also return to the source provider.".to_string(),
    };
    let row = format!("{switch_id}-context");
    for block in &mut session.blocks {
        if block.id != row {
            continue;
        }
        block.text = text.clone();
        if let Some(handoff) = &mut block.handoff {
            let accepted = status == ContextStatus::Accepted;
            handoff.status = if accepted {
                HandoffStatus::Ready
            } else {
                HandoffStatus::Preparing
            };
            handoff.pending = Some(!accepted);
        }
    }
}

/// Ends an unaccepted delivery after its turn stopped. A submitted request
/// may have run, so it waits for inspection. Otherwise the request goes
/// back to a draft. Returns whether the target's conversation should be
/// forgotten.
fn end_unaccepted_delivery(session: &mut Session, switch_id: &str) -> bool {
    let submitted = session
        .provider_context
        .as_ref()
        .and_then(|state| state.delivery.as_ref())
        .is_some_and(|delivery| delivery.is_submitted());
    if submitted {
        recover_submitted_provider_delivery(session, switch_id);
    } else {
        fail_provider_delivery(session, switch_id, false);
    }
    context_status(session, switch_id, ContextStatus::Uncertain);
    !session
        .provider_context
        .as_ref()
        .and_then(|state| state.delivery.as_ref())
        .is_some_and(|delivery| delivery.needs_inspection())
}

/// The session's provider conversation may be rebound: it is not a target
/// that may hold partial history.
fn can_rebind(session: &Session) -> Option<&str> {
    session
        .provider_session_id
        .as_deref()
        .filter(|_| !requires_fresh_provider_binding(session, session.harness, &session.cwd, None))
}

/// `planComposerSwitch` from src/features/sessions/model/handoff.ts. The
/// desktop's copy lives in monocode-engine, which this crate cannot use.
fn plan_composer_switch(session: &Session, next: HarnessId) -> ComposerSwitchPlan {
    if session.harness == next {
        return ComposerSwitchPlan::Model;
    }
    // History may include a target request that ran. Returning to its source
    // then needs a new transfer, before or after inspection is confirmed.
    let last = session
        .blocks
        .iter()
        .rev()
        .find(|block| block.role == BlockRole::Handoff)
        .and_then(|block| block.handoff.as_ref());
    let unknown_request_intent = session.pending_switch.as_ref().is_some_and(|pending| {
        last.is_some_and(|last| {
            last.from == pending.from
                && last.transfer.as_ref().is_some_and(|transfer| {
                    transfer.needs_inspection == Some(true)
                        || transfer.inspection_confirmed == Some(true)
                })
        })
    });
    let pending = session
        .pending_switch
        .as_ref()
        .filter(|_| !unknown_request_intent);
    if let Some(pending) = pending
        && next == pending.from
    {
        return ComposerSwitchPlan::Revert {
            restore_provider_session_id: pending
                .from_provider_session_id
                .clone()
                .filter(|id| !id.is_empty()),
            restore_provider_account_id: pending
                .from_provider_account_id
                .clone()
                .filter(|id| !id.is_empty()),
        };
    }
    if !session
        .blocks
        .iter()
        .any(|block| block.role == BlockRole::User)
        && session.pending_switch.is_none()
    {
        return ComposerSwitchPlan::Empty {
            forget: session.harness,
        };
    }
    ComposerSwitchPlan::Arm {
        pending: pending.cloned().unwrap_or_else(|| PendingHarnessSwitch {
            from: session.harness,
            from_model: session.model.clone(),
            from_settings: session.model_settings.clone(),
            from_provider_session_id: session
                .provider_session_id
                .clone()
                .filter(|id| !id.is_empty()),
            from_provider_account_id: session
                .provider_account_id
                .clone()
                .filter(|id| !id.is_empty()),
        }),
    }
}

/// A running session, including streamed events not yet written to disk.
struct Live {
    value: HostSession,
    events: Vec<HarnessEvent>,
    timer: Option<u64>,
}

#[derive(Default)]
struct State {
    switching_projects: HashSet<String>,
    running: HashMap<String, Active>,
    live: HashMap<String, Live>,
    retry_timers: HashMap<String, u64>,
    /// Persistent providers waiting to be parked, by session id.
    idle_timers: HashMap<String, u64>,
    /// Providers being parked. A new turn waits for this before it starts,
    /// so the stop cannot kill the turn's new child.
    parking: HashMap<String, (u64, Done)>,
    next_token: u64,
}

impl State {
    fn token(&mut self) -> u64 {
        self.next_token += 1;
        self.next_token
    }
}

/// The provider half of a `switchProvider` command: remember the binding
/// the session leaves, then arm, revert, or drop the pending switch.
/// `resumable` means the target can append to its own saved conversation.
fn switch_provider(session: &mut Session, harness: HarnessId, resumable: bool) {
    let cwd = session.cwd.clone();
    if let Some(source) = provider_binding(session, session.harness, &cwd, None) {
        remember_provider_binding(session, source);
    }
    let plan = plan_composer_switch(session, harness);
    let target = provider_binding(session, harness, &cwd, None);
    session.harness = harness;
    session.context = None;
    session.provider_session_id = target
        .filter(|_| resumable)
        .map(|target| target.provider_session_id);
    session.provider_account_id = None;
    match plan {
        ComposerSwitchPlan::Arm { pending } => session.pending_switch = Some(pending),
        ComposerSwitchPlan::Revert {
            restore_provider_session_id,
            ..
        } => {
            session.pending_switch = None;
            session.provider_session_id = restore_provider_session_id;
        }
        ComposerSwitchPlan::Empty { .. } => session.pending_switch = None,
        ComposerSwitchPlan::Model => {}
    }
}

/// A test seam: return true to fail the next matching save.
#[cfg(test)]
pub(crate) type SaveFault = Box<dyn FnMut(&Value) -> bool + Send>;

struct Inner {
    store: Arc<HostStore>,
    providers: HostProviders,
    catalog: SharedCatalog,
    spawner: SharedSpawner,
    state: Mutex<State>,
    closing: AtomicBool,
    retry_delay: Duration,
    idle_park: Mutex<Duration>,
    /// The real provider processes, when this engine serves a host.
    harness: Option<Arc<HostHarness>>,
    workspace: WorkspaceCommands,
    #[cfg(test)]
    save_fault: Mutex<Option<SaveFault>>,
}

/// What a command does after its transaction commits.
enum Effect {
    Run {
        prompt: Option<String>,
        intent: Option<SendIntent>,
        attachments: Vec<Attachment>,
        first_turn: Option<(String, bool)>,
        transfer: Option<Box<Transfer>>,
    },
    Cancel,
    Approve {
        request_id: i64,
        decision: ApprovalDecision,
    },
    Answer {
        request_id: i64,
        reply: monocode_core::user_question::UserQuestionReply,
    },
}

/// `HostEngine`. Clones share one engine.
#[derive(Clone)]
pub struct HostEngine {
    inner: Arc<Inner>,
}

impl Inner {
    fn provider(&self, id: RemoteProvider) -> Result<Arc<dyn HostProvider>, String> {
        self.providers
            .get(&id)
            .cloned()
            .ok_or_else(|| format!("{} is not available on this host", provider_name(id)))
    }

    /// `store.save`, with the test seam.
    fn persist(&self, value: HostSession, event: &Value) -> Result<Arc<HostSession>, String> {
        #[cfg(test)]
        if let Some(fault) = self.save_fault.lock().as_mut()
            && fault(event)
        {
            return Err("temporary storage error".into());
        }
        self.store.save(value, event)
    }

    /// `save`: bumps the revision and the update time in one transaction.
    fn save(&self, mut value: HostSession, event: &Value) -> Result<Arc<HostSession>, String> {
        value.revision += 1;
        value.updated_at = now_ms();
        self.store.transaction(|| self.persist(value, event))
    }

    /// `flush`: writes batched events.
    fn flush(&self, state: &mut State, id: &str) -> Result<(), String> {
        let Some(live) = state.live.get_mut(id) else {
            return Ok(());
        };
        live.timer = None;
        if live.events.is_empty() {
            return Ok(());
        }
        let event = json!({ "type": "events", "events": live.events });
        let saved = self.save(live.value.clone(), &event)?;
        live.value = (*saved).clone();
        live.events.clear();
        Ok(())
    }

    /// `scheduledFlush`: a failed write stops the provider.
    fn scheduled_flush(self: &Arc<Self>, state: &mut State, id: &str) {
        let Err(error) = self.flush(state, id) else {
            return;
        };
        let active = match state.running.get_mut(id) {
            Some(active) => {
                active.persistence_failed = true;
                true
            }
            None => false,
        };
        log::error!("Session persistence failed; stopping its provider: {error}");
        let live = state
            .live
            .get(id)
            .map(|live| (live.value.session.harness, live.value.run_id.clone()));
        if let Some((harness, run_id)) = live
            && let Ok(provider) = self.provider(harness)
        {
            let stopping = provider.clone();
            let stop_id = id.to_string();
            self.spawner.spawn(
                async move {
                    let _ = stopping.stop(&stop_id).await;
                }
                .boxed(),
            );
            // A native turn has no send that would settle it.
            if !active && let Some(run_id) = run_id {
                let engine = self.clone();
                let id = id.to_string();
                self.spawner
                    .spawn(async move { engine.retry_settlement(&id, &run_id, provider) }.boxed());
            }
        }
    }

    /// `settled`: ends streaming and closes a building plan.
    fn settled(
        value: &HostSession,
        status: HostSessionStatus,
        message: Option<&str>,
        ended_at: i64,
    ) -> HostSession {
        let message = message.filter(|message| !message.is_empty());
        let mut session = stop_streaming(&value.session, ended_at);
        for block in &mut session.blocks {
            if block.role == BlockRole::Plan
                && let Some(plan) = block.plan.as_mut()
                && plan.status == PlanStatus::Building
            {
                plan.status = if status == HostSessionStatus::Idle && message.is_none() {
                    PlanStatus::Built
                } else {
                    PlanStatus::Ready
                };
            }
        }
        if let Some(message) = message {
            let mut block =
                Block::new(uuid::Uuid::new_v4().to_string(), BlockRole::System, message);
            block.notice = Some(
                if status == HostSessionStatus::Interrupted || message == "Stopped by you." {
                    BlockNotice::Interrupt
                } else {
                    BlockNotice::Error
                },
            );
            block.streaming = Some(false);
            session.blocks.push(block);
        }
        HostSession {
            status,
            session,
            ..value.clone()
        }
    }

    /// `event`: one provider event for the current run.
    fn event(self: &Arc<Self>, id: &str, run_id: &str, event: HarnessEvent) {
        let mut state = self.state.lock();
        let state = &mut *state;
        if matches!(
            event,
            HarnessEvent::TurnStarted {
                native: Some(true),
                ..
            }
        ) && !self.closing.load(Ordering::SeqCst)
        {
            self.start_native_turn(state, id, run_id);
        }
        let Some(live) = state.live.get_mut(id) else {
            return;
        };
        if live.value.run_id.as_deref() != Some(run_id)
            || live.value.status != HostSessionStatus::Running
        {
            return;
        }
        if matches!(event, HarnessEvent::TurnFinished { native: Some(true) }) {
            self.settle_native_turn(state, id, run_id);
            return;
        }
        let mut changed = apply_harness_event_mut(&mut SystemEnv, &mut live.value.session, &event);
        let session = &mut live.value.session;
        let identity = |session: &Session| {
            (
                session.provider_context.clone(),
                session.pending_switch.clone(),
                session.provider_session_id.clone(),
            )
        };
        match &event {
            HarnessEvent::SessionProviderBound {
                provider_session_id,
            } => {
                let before = identity(session);
                let (harness, cwd) = (session.harness, session.cwd.clone());
                record_provider_bound(session, harness, &cwd, provider_session_id, None);
                changed |= identity(session) != before;
            }
            HarnessEvent::Context { used, window } => {
                let before = session.provider_context.clone();
                let (harness, cwd) = (session.harness, session.cwd.clone());
                record_provider_context_usage(session, harness, &cwd, *used, *window, None);
                changed |= session.provider_context != before;
            }
            _ => {}
        }
        if !changed {
            return;
        }
        let batch = batched(&event);
        live.events.push(event);
        if !batch {
            self.scheduled_flush(state, id);
        } else if live.timer.is_none() {
            let token = state.token();
            if let Some(live) = state.live.get_mut(id) {
                live.timer = Some(token);
            }
            let engine = Arc::downgrade(self);
            let id = id.to_string();
            self.spawner.spawn(
                async move {
                    smol::Timer::after(Duration::from_millis(FLUSH_MS)).await;
                    let Some(engine) = engine.upgrade() else {
                        return;
                    };
                    let mut state = engine.state.lock();
                    if state.live.get(&id).and_then(|live| live.timer) == Some(token) {
                        engine.scheduled_flush(&mut state, &id);
                    }
                }
                .boxed(),
            );
        }
    }

    /// `startNativeTurn`: Claude woke on its own, for example for a job it
    /// scheduled. The idle session runs again under the provider's turn id.
    fn start_native_turn(self: &Arc<Self>, state: &mut State, id: &str, run_id: &str) {
        state.idle_timers.remove(id);
        let Ok(current) = self.store.session(id) else {
            return;
        };
        if current.session.harness != RemoteProvider::Claude
            || (current.status != HostSessionStatus::Idle && !state.running.contains_key(id))
        {
            return;
        }
        let previous = Self::settled(&current, HostSessionStatus::Idle, None, now_ms());
        let mut session = previous.session.clone();
        session.busy = Some(true);
        let value = HostSession {
            run_id: Some(run_id.to_string()),
            status: HostSessionStatus::Running,
            session,
            ..previous
        };
        match self.save(value, &json!({ "type": "native.started" })) {
            Ok(saved) => {
                state.live.insert(
                    id.to_string(),
                    Live {
                        value: (*saved).clone(),
                        events: Vec::new(),
                        timer: None,
                    },
                );
            }
            Err(error) => log::error!("Could not start a native turn: {error}"),
        }
    }

    /// `settleNativeTurn`: the turn Claude started on its own ended.
    fn settle_native_turn(self: &Arc<Self>, state: &mut State, id: &str, run_id: &str) {
        let Some(harness) = state.live.get(id).map(|live| live.value.session.harness) else {
            return;
        };
        let Ok(provider) = self.provider(harness) else {
            return;
        };
        let settled = self.flush(state, id).and_then(|()| {
            let latest = self.store.session(id)?;
            self.save(
                Self::settled(&latest, HostSessionStatus::Idle, None, now_ms()),
                &json!({ "type": "native.settled" }),
            )
        });
        match settled {
            Ok(_) => {
                state.live.remove(id);
                self.park_idle_provider(state, id, provider);
            }
            Err(error) => {
                log::error!("Could not settle a native turn: {error}");
                let engine = self.clone();
                let (id, run_id) = (id.to_string(), run_id.to_string());
                self.spawner
                    .spawn(async move { engine.retry_settlement(&id, &run_id, provider) }.boxed());
            }
        }
    }

    /// `parkIdleProvider`: stop a persistent provider once its session has
    /// been idle for a while, unless it still has work that can wake it.
    /// Its conversation stays bound for the next turn.
    fn park_idle_provider(
        self: &Arc<Self>,
        state: &mut State,
        id: &str,
        provider: Arc<dyn HostProvider>,
    ) {
        if self.closing.load(Ordering::SeqCst) || state.idle_timers.contains_key(id) {
            return;
        }
        let token = state.token();
        state.idle_timers.insert(id.to_string(), token);
        let engine = Arc::downgrade(self);
        let id = id.to_string();
        let delay = *self.idle_park.lock();
        self.spawner.spawn(
            async move {
                smol::Timer::after(delay).await;
                let Some(engine) = engine.upgrade() else {
                    return;
                };
                let parking = {
                    let mut state = engine.state.lock();
                    if state.idle_timers.get(&id) != Some(&token) {
                        return;
                    }
                    state.idle_timers.remove(&id);
                    let running = engine
                        .store
                        .session(&id)
                        .is_ok_and(|value| value.status == HostSessionStatus::Running);
                    if running || provider.needs_process(&id) {
                        engine.park_idle_provider(&mut state, &id, provider);
                        return;
                    }
                    let stopping = provider.clone();
                    let store = engine.store.clone();
                    let stop_id = id.clone();
                    let parking: Done = async move {
                        if let Err(error) = stopping.stop(&stop_id).await {
                            log::error!("Could not park provider: {error}");
                            return;
                        }
                        if let Ok(value) = store.session(&stop_id)
                            && let Some(provider_session_id) = &value.session.provider_session_id
                        {
                            stopping.bind(&stop_id, provider_session_id, &value.session.cwd);
                        }
                    }
                    .boxed()
                    .shared();
                    let token = state.token();
                    state.parking.insert(id.clone(), (token, parking.clone()));
                    (token, parking)
                };
                parking.1.await;
                let mut state = engine.state.lock();
                if state
                    .parking
                    .get(&id)
                    .is_some_and(|(token, _)| *token == parking.0)
                {
                    state.parking.remove(&id);
                }
            }
            .boxed(),
        );
    }

    /// `updateProviderState`: apply `update` to the running session and save
    /// it with `event`. The change stays in memory even when saving fails,
    /// so settlement can still use it; the failure stops the provider.
    /// Returns false when `run_id` no longer runs.
    fn update_provider_state(
        self: &Arc<Self>,
        id: &str,
        run_id: &str,
        update: impl FnOnce(&mut Session),
        event: &Value,
    ) -> Result<bool, String> {
        let mut state = self.state.lock();
        let state = &mut *state;
        let Some(live) = state.live.get_mut(id).filter(|live| {
            live.value.run_id.as_deref() == Some(run_id)
                && live.value.status == HostSessionStatus::Running
        }) else {
            return Ok(false);
        };
        let before = live.value.session.clone();
        update(&mut live.value.session);
        let changed = live.value.session != before;
        let harness = live.value.session.harness;
        let saved = self.flush(state, id).and_then(|()| {
            if changed && let Some(live) = state.live.get_mut(id) {
                live.value = (*self.save(live.value.clone(), event)?).clone();
            }
            Ok(())
        });
        if let Err(error) = saved {
            if let Some(active) = state.running.get_mut(id) {
                active.persistence_failed = true;
            }
            if let Ok(provider) = self.provider(harness) {
                let id = id.to_string();
                self.spawner.spawn(
                    async move {
                        let _ = provider.stop(&id).await;
                    }
                    .boxed(),
                );
            }
            return Err(error);
        }
        Ok(true)
    }

    /// Whether `run_id` is still the session's live run.
    fn live_run(&self, id: &str, run_id: &str) -> bool {
        self.state
            .lock()
            .live
            .get(id)
            .is_some_and(|live| live.value.run_id.as_deref() == Some(run_id))
    }

    fn mark_accepted(&self, id: &str) {
        if let Some(active) = self.state.lock().running.get_mut(id) {
            active.accepted = true;
        }
    }

    /// `onAccepted`: the provider took the request. A failed save must not
    /// reach the provider's reader; settlement saves the retained
    /// acknowledgment again.
    fn accepted_hook(
        self: &Arc<Self>,
        id: &str,
        run_id: &str,
        switch_id: Option<String>,
    ) -> AcceptedHook {
        let engine = Arc::downgrade(self);
        let (id, run_id) = (id.to_string(), run_id.to_string());
        Arc::new(move || {
            let Some(engine) = engine.upgrade() else {
                return;
            };
            if !engine.live_run(&id, &run_id) {
                return;
            }
            engine.mark_accepted(&id);
            let mut event = json!({ "type": "providerContext.accepted" });
            if let Some(switch_id) = &switch_id {
                event["switchId"] = json!(switch_id);
            }
            let accepted = engine.update_provider_state(
                &id,
                &run_id,
                |session| {
                    if let Some(switch_id) = &switch_id {
                        accept_provider_delivery(session, switch_id);
                        context_status(session, switch_id, ContextStatus::Accepted);
                    }
                },
                &event,
            );
            if let Err(error) = accepted {
                log::error!("Could not save provider acceptance: {error}");
            }
        })
    }

    /// `onDelivered`: the history reached the target. Inline history travels
    /// in the acknowledged request itself, so it also counts as acceptance.
    fn delivered_hook(
        self: &Arc<Self>,
        id: &str,
        run_id: &str,
        transfer: &Transfer,
    ) -> DeliveredHook {
        let engine = Arc::downgrade(self);
        let (id, run_id) = (id.to_string(), run_id.to_string());
        let switch_id = transfer.switch_id.clone();
        let known: Vec<(String, String)> = transfer
            .context
            .items
            .iter()
            .chain(&transfer.fallback_context.items)
            .map(|item| (item.id.clone(), item.source_block_id.clone()))
            .collect();
        Arc::new(move |receipt: ContextTransferReceipt| {
            let result = (|| {
                let Some(engine) = engine.upgrade() else {
                    return Ok(());
                };
                if !engine.live_run(&id, &run_id) {
                    return Ok(());
                }
                let inline = receipt.mode == DeliveryMode::Inline;
                if inline {
                    engine.mark_accepted(&id);
                }
                let mode = if inline {
                    TransferMode::Inline
                } else {
                    TransferMode::Native
                };
                let included = receipt.included_ids.as_ref().map(|ids| {
                    ids.iter()
                        .map(|id| {
                            known
                                .iter()
                                .find(|(item, _)| item == id)
                                .map_or_else(|| id.clone(), |(_, block)| block.clone())
                        })
                        .collect::<Vec<_>>()
                });
                let coverage = DeliveryCoverage {
                    omitted_block_ids: included.as_ref().and_then(|_| receipt.omitted_ids.clone()),
                    included_block_ids: included,
                    source_through_block_id: receipt.through_block_id.clone(),
                };
                let event = json!({
                    "type": "providerContext.delivered",
                    "switchId": switch_id,
                    "mode": if inline { "inline" } else { "native" },
                });
                let saved = engine.update_provider_state(
                    &id,
                    &run_id,
                    |session| {
                        mark_provider_context_delivered(
                            session,
                            &switch_id,
                            mode,
                            receipt.provider_session_id.as_deref(),
                            coverage,
                        );
                        context_status(session, &switch_id, ContextStatus::Imported);
                    },
                    &event,
                );
                match saved {
                    Err(error) if !inline => Err(error),
                    _ => Ok(()),
                }
            })();
            async move { result }.boxed()
        })
    }

    fn sink(self: &Arc<Self>, id: &str, run_id: &str) -> EventSink {
        let engine: Weak<Self> = Arc::downgrade(self);
        let id = id.to_string();
        // A turn Claude starts on its own runs under its own id.
        let run_id = Mutex::new(run_id.to_string());
        Arc::new(move |event| {
            if let Some(engine) = engine.upgrade() {
                let current = {
                    let mut run_id = run_id.lock();
                    if let HarnessEvent::TurnStarted {
                        provider_turn_id,
                        native: Some(true),
                    } = &event
                    {
                        *run_id = provider_turn_id.clone();
                    }
                    run_id.clone()
                };
                engine.event(&id, &current, event);
            }
        })
    }

    /// `run`: starts the provider turn for a saved `running` value.
    fn run(
        self: &Arc<Self>,
        state: &mut State,
        value: &HostSession,
        prompt: Option<String>,
        intent: Option<SendIntent>,
        attachments: Vec<Attachment>,
        transfer: Option<Transfer>,
    ) {
        let session = value.session.clone();
        let run_id = value.run_id.clone().unwrap_or_default();
        let Ok(provider) = self.provider(session.harness) else {
            return;
        };
        state.idle_timers.remove(&session.id);
        let (done, finished) = futures::channel::oneshot::channel::<()>();
        state.running.insert(
            session.id.clone(),
            Active {
                done: finished.map(|_| ()).boxed().shared(),
                cancelled: false,
                persistence_failed: false,
                accepted: false,
            },
        );
        state.live.insert(
            session.id.clone(),
            Live {
                value: value.clone(),
                events: Vec::new(),
                timer: None,
            },
        );
        let engine = self.clone();
        self.spawner.spawn(
            async move {
                let outcome = engine
                    .turn(
                        &session,
                        &run_id,
                        &provider,
                        prompt,
                        intent,
                        attachments,
                        transfer,
                    )
                    .await;
                if let Err(error) = outcome {
                    if let Some(live) = engine.state.lock().live.get_mut(&session.id) {
                        live.timer = None;
                    }
                    log::error!("Session persistence failed; stopping its provider: {error}");
                    let stopping = provider.clone();
                    let id = session.id.clone();
                    engine.spawner.spawn(
                        async move {
                            let _ = stopping.stop(&id).await;
                        }
                        .boxed(),
                    );
                    engine.retry_settlement(&session.id, &run_id, provider);
                }
                drop(done);
            }
            .boxed(),
        );
    }

    /// The body of `run`: the provider call, then settlement.
    #[allow(clippy::too_many_arguments)]
    async fn turn(
        self: &Arc<Self>,
        session: &Session,
        run_id: &str,
        provider: &Arc<dyn HostProvider>,
        prompt: Option<String>,
        intent: Option<SendIntent>,
        attachments: Vec<Attachment>,
        transfer: Option<Transfer>,
    ) -> Result<(), String> {
        let id = session.id.as_str();
        let mut error: Option<String> = None;
        let parking = self
            .state
            .lock()
            .parking
            .get(id)
            .map(|(_, parking)| parking.clone());
        if let Some(parking) = parking {
            parking.await;
        }
        let cancelled = self
            .state
            .lock()
            .running
            .get(id)
            .is_some_and(|active| active.cancelled);
        if !self.closing.load(Ordering::SeqCst) && !cancelled {
            let input = HarnessSessionInput {
                session_id: id.to_string(),
                cwd: session.cwd.clone(),
                model: session.model.clone(),
                model_settings: Some(session.model_settings.clone()),
                provider_account_id: None,
                runtime_mode: session.runtime_mode,
                intent: intent.map(|intent| match intent {
                    SendIntent::Default => TurnIntent::Default,
                    SendIntent::Plan => TurnIntent::Plan,
                    SendIntent::Build => TurnIntent::Build,
                }),
                controls_agents: None,
                app_access: None,
            };
            let on_event = self.sink(id, run_id);
            let result = async {
                // The target takes over from the provider the switch left.
                if let Some(pending) = &session.pending_switch
                    && pending.from != session.harness
                {
                    self.provider(pending.from)?.stop(id).await?;
                }
                if let Some(transfer) = &transfer {
                    if transfer.fresh {
                        provider.forget(id).await?;
                    } else if let Some(provider_session_id) = &session.provider_session_id {
                        provider.bind(id, provider_session_id, &session.cwd);
                    }
                }
                match prompt {
                    None => provider.compact(input, on_event).await,
                    Some(text) => {
                        let input = SendTurnInput {
                            session: input,
                            text,
                            attachments: Some(with_image_data(attachments)?),
                        };
                        let on_accepted = self.accepted_hook(
                            id,
                            run_id,
                            transfer.as_ref().map(|transfer| transfer.switch_id.clone()),
                        );
                        let transfer = transfer.as_ref().map(|transfer| ContextTransferInput {
                            context: transfer.context.clone(),
                            fallback_context: Some(transfer.fallback_context.clone()),
                            on_delivered: Some(self.delivered_hook(id, run_id, transfer)),
                        });
                        provider
                            .send(prepare_context_transfer_input(
                                input,
                                transfer,
                                provider.context_transfer_capabilities(),
                                on_event,
                                Some(on_accepted),
                                self.spawner.clone(),
                            ))
                            .await
                    }
                }
            }
            .await;
            error = result.err();
        }
        // A persistent provider keeps its child for later wakeups unless the
        // turn ended badly. Otherwise keep the session running until the old
        // process has stopped, so a follow-up cannot race cleanup and have
        // its newly spawned child killed.
        let (cancelled, failed) = self
            .state
            .lock()
            .running
            .get(id)
            .map(|active| (active.cancelled, active.persistence_failed))
            .unwrap_or_default();
        let stop = !provider.persistent()
            || self.closing.load(Ordering::SeqCst)
            || cancelled
            || failed
            || error.is_some();
        if stop {
            provider.stop(id).await?;
        }
        let (persisted, forget) = {
            let mut state = self.state.lock();
            let state = &mut *state;
            self.flush(state, id)?;
            let mut forget = false;
            if let Some(mut latest) =
                Self::running_value(state, &*self.store.session(id)?, id, run_id)
            {
                let closing = self.closing.load(Ordering::SeqCst);
                let (cancelled, failed, accepted) = state
                    .running
                    .get(id)
                    .map(|active| (active.cancelled, active.persistence_failed, active.accepted))
                    .unwrap_or_default();
                if accepted {
                    if let Some(transfer) = &transfer {
                        accept_provider_delivery(&mut latest.session, &transfer.switch_id);
                        context_status(
                            &mut latest.session,
                            &transfer.switch_id,
                            ContextStatus::Accepted,
                        );
                    }
                    settle_provider_binding(
                        &mut latest.session,
                        session.harness,
                        &session.cwd,
                        None,
                    );
                } else if let Some(transfer) = &transfer {
                    forget = end_unaccepted_delivery(&mut latest.session, &transfer.switch_id);
                }
                let message = if closing {
                    Some("Host stopped. This turn was interrupted.")
                } else if failed {
                    Some(STORAGE_FAILED)
                } else if cancelled {
                    Some("Stopped by you.")
                } else {
                    error.as_deref()
                };
                let status = if closing || failed {
                    HostSessionStatus::Interrupted
                } else {
                    HostSessionStatus::Idle
                };
                let mut event = json!({ "type": "settled", "cancelled": cancelled });
                if let Some(error) = &error {
                    event["error"] = json!(error);
                }
                self.save(Self::settled(&latest, status, message, now_ms()), &event)?;
            }
            // A native turn may already own the session.
            if state
                .live
                .get(id)
                .is_some_and(|live| live.value.run_id.as_deref() == Some(run_id))
            {
                state.live.remove(id);
            }
            state.running.remove(id);
            if provider.persistent() {
                self.park_idle_provider(state, id, provider.clone());
            }
            (self.store.session(id)?.session.clone(), forget)
        };
        if forget {
            provider.forget(id).await?;
        }
        // Stopping released the provider's callbacks; binding keeps only its
        // conversation for an explicit follow-up. A target that may hold
        // partial history is not rebound.
        if let Some(provider_session_id) = can_rebind(&persisted) {
            provider.bind(id, provider_session_id, &persisted.cwd);
        }
        Ok(())
    }

    /// The value to settle for `run_id`: the live copy, which keeps
    /// provider evidence whose save failed, while the saved session still
    /// runs that turn. `None` once another run or a settlement replaced it.
    fn running_value(
        state: &State,
        stored: &HostSession,
        id: &str,
        run_id: &str,
    ) -> Option<HostSession> {
        if stored.run_id.as_deref() != Some(run_id) || stored.status != HostSessionStatus::Running {
            return None;
        }
        Some(
            state
                .live
                .get(id)
                .filter(|live| live.value.run_id.as_deref() == Some(run_id))
                .map_or_else(|| stored.clone(), |live| live.value.clone()),
        )
    }

    /// `retrySettlement`: settles a turn whose final write failed, every
    /// second until it succeeds.
    fn retry_settlement(self: &Arc<Self>, id: &str, run_id: &str, provider: Arc<dyn HostProvider>) {
        let token = {
            let mut state = self.state.lock();
            if self.closing.load(Ordering::SeqCst) || state.retry_timers.contains_key(id) {
                return;
            }
            let token = state.token();
            state.retry_timers.insert(id.to_string(), token);
            token
        };
        let engine = Arc::downgrade(self);
        let (id, run_id) = (id.to_string(), run_id.to_string());
        let delay = self.retry_delay;
        self.spawner.spawn(
            async move {
                smol::Timer::after(delay).await;
                let Some(engine) = engine.upgrade() else {
                    return;
                };
                {
                    let mut state = engine.state.lock();
                    if state.retry_timers.get(&id) != Some(&token) {
                        return;
                    }
                    state.retry_timers.remove(&id);
                }
                if let Err(error) = engine.retry(&id, &run_id, &provider).await {
                    log::error!("Retrying session persistence: {error}");
                    engine.retry_settlement(&id, &run_id, provider);
                }
            }
            .boxed(),
        );
    }

    async fn retry(
        self: &Arc<Self>,
        id: &str,
        run_id: &str,
        provider: &Arc<dyn HostProvider>,
    ) -> Result<(), String> {
        provider.stop(id).await?;
        let (latest, forget) = {
            let mut state = self.state.lock();
            let state = &mut *state;
            self.flush(state, id)?;
            let stored = self.store.session(id)?;
            let mut forget = false;
            let latest = match Self::running_value(state, &stored, id, run_id) {
                Some(mut latest) => {
                    let accepted = state.running.get(id).is_some_and(|active| active.accepted);
                    let delivery = latest
                        .session
                        .provider_context
                        .as_ref()
                        .and_then(|context| context.delivery.as_ref())
                        .map(|delivery| (delivery.switch_id.clone(), delivery.status));
                    if accepted {
                        if let Some((switch_id, _)) = &delivery {
                            accept_provider_delivery(&mut latest.session, switch_id);
                            context_status(&mut latest.session, switch_id, ContextStatus::Accepted);
                        }
                        let (harness, cwd) = (latest.session.harness, latest.session.cwd.clone());
                        settle_provider_binding(&mut latest.session, harness, &cwd, None);
                    } else if let Some((switch_id, status)) = &delivery
                        && *status != TransferStatus::Accepted
                    {
                        forget = end_unaccepted_delivery(&mut latest.session, switch_id);
                    }
                    let updated_at = latest.updated_at;
                    (*self.save(
                        Self::settled(
                            &latest,
                            HostSessionStatus::Interrupted,
                            Some(STORAGE_FAILED),
                            updated_at,
                        ),
                        &json!({ "type": "interrupted", "reason": "persistence failure" }),
                    )?)
                    .clone()
                }
                None => (*stored).clone(),
            };
            state.live.remove(id);
            state.running.remove(id);
            (latest, forget)
        };
        if forget {
            provider.forget(id).await?;
        }
        if let Some(provider_session_id) = can_rebind(&latest.session) {
            provider.bind(id, provider_session_id, &latest.session.cwd);
        }
        Ok(())
    }

    /// `generateFirstTurnNames`: an LLM title, and a branch name for an
    /// automatically created worktree.
    fn generate_first_turn_names(
        self: &Arc<Self>,
        value: &HostSession,
        message: &str,
        generate_title: bool,
    ) {
        let Ok(provider) = self.provider(value.session.harness) else {
            return;
        };
        let id = value.session.id.clone();
        let cwd = value.session.cwd.clone();
        let harness = value.session.harness;
        if generate_title && provider.can_generate_title() {
            let engine = self.clone();
            let title = value.session.title.clone();
            let request = provider.generate_title(TitleInput {
                session_id: id.clone(),
                cwd: cwd.clone(),
                message: message.to_string(),
                provider_account_id: None,
            });
            let id = id.clone();
            self.spawner.spawn(
                async move {
                    let result: Result<(), String> = async {
                        let Some(generated) = request.await? else {
                            return Ok(());
                        };
                        let mut state = engine.state.lock();
                        engine.flush(&mut state, &id)?;
                        let current = engine.store.session(&id)?;
                        if current.session.title != title {
                            return Ok(());
                        }
                        let mut next = (*current).clone();
                        next.session.title = format_session_title(harness, &generated.title);
                        let saved =
                            engine.save(next, &json!({ "type": "session.generatedTitle" }))?;
                        if let Some(live) = state.live.get_mut(&id) {
                            live.value = (*saved).clone();
                        }
                        Ok(())
                    }
                    .await;
                    if let Err(error) = result {
                        log::debug!("[monocode] remote session title {error}");
                    }
                }
                .boxed(),
            );
        }
        let Some(temporary) = value.auto_worktree_branch.clone() else {
            return;
        };
        if !provider.can_generate_branch_name() {
            return;
        }
        let engine = self.clone();
        let project_id = value.project_id.clone();
        let request = provider.generate_branch_name(&cwd, message);
        self.spawner.spawn(
            async move {
                let result: Result<(), String> = async {
                    let Some(branch) = request.await?.as_deref().and_then(named_worktree_branch)
                    else {
                        return Ok(());
                    };
                    // A title or branch request may finish after the
                    // conversation was deleted.
                    let before = engine.store.session(&id)?;
                    if before.auto_worktree_branch.as_deref() != Some(temporary.as_str()) {
                        return Ok(());
                    }
                    let project = engine.store.project(&project_id)?;
                    {
                        let (engine, id, temporary, branch, cwd) = (
                            engine.clone(),
                            id.clone(),
                            temporary.clone(),
                            branch.clone(),
                            cwd.clone(),
                        );
                        smol::unblock(move || {
                            let owned = || {
                                engine.store.session(&id).is_ok_and(|current| {
                                    current.auto_worktree_branch.as_deref()
                                        == Some(temporary.as_str())
                                })
                            };
                            rename_host_worktree_branch(
                                &project.cwd,
                                &cwd,
                                &temporary,
                                &branch,
                                &owned,
                            )
                        })
                        .await?;
                    }
                    let mut state = engine.state.lock();
                    engine.flush(&mut state, &id)?;
                    let current = engine.store.session(&id)?;
                    let mut next = (*current).clone();
                    next.auto_worktree_branch = None;
                    next.session.branch = Some(branch.clone());
                    let saved = engine.save(
                        next,
                        &json!({ "type": "session.generatedBranch", "branch": branch }),
                    )?;
                    if let Some(live) = state.live.get_mut(&id) {
                        live.value = (*saved).clone();
                    }
                    Ok(())
                }
                .await;
                if let Err(error) = result {
                    log::debug!("[monocode] remote worktree branch {error}");
                }
            }
            .boxed(),
        );
    }
}

fn create_private_dir(directory: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .map_err(|error| error.to_string())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(directory).map_err(|error| error.to_string())
    }
}

/// `writeFileSync(path, data, { mode: 0o600 })`.
fn write_private_file(path: &std::path::Path, data: &[u8]) -> Result<(), String> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|mut file| file.write_all(data))
        .map_err(|error| error.to_string())
}

/// Adds base64 data to vision images, as the provider sends them inline.
fn with_image_data(attachments: Vec<Attachment>) -> Result<Vec<Attachment>, String> {
    attachments
        .into_iter()
        .map(|mut file| {
            if is_vision_image(&file.mime_type)
                && file.size <= 20 * 1024 * 1024
                && let Some(path) = file.path.as_deref().filter(|path| !path.is_empty())
            {
                let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
                file.data = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
            }
            Ok(file)
        })
        .collect()
}

/// `value.session.model.replace(/^[^:]+:/, "")`.
fn model_slug(model: &str) -> &str {
    match model.find(':') {
        Some(index) if index > 0 => &model[index + 1..],
        _ => model,
    }
}

impl HostEngine {
    /// Builds the engine over `store` and recovers turns a previous host
    /// left running. Provider dispatch is not transactional with SQLite, so
    /// a send is never replayed after a crash; its effects may already
    /// exist.
    pub fn new(
        store: Arc<HostStore>,
        providers: HostProviders,
        catalog: SharedCatalog,
        spawner: SharedSpawner,
    ) -> Result<Self, String> {
        Self::build(store, providers, catalog, spawner, None)
    }

    /// The engine over the real harness: every provider adapter, run by
    /// this host's process supervisor. This is what `serve` uses.
    pub fn start(store: Arc<HostStore>, options: HostEngineOptions) -> Result<Self, String> {
        let harness = Arc::new(HostHarness::start(&store, options));
        let engine = Self::build(
            store,
            harness.providers(),
            harness.catalog().clone(),
            harness.spawner(),
            Some(harness.clone()),
        );
        if engine.is_err() {
            harness.close();
        }
        engine
    }

    fn build(
        store: Arc<HostStore>,
        providers: HostProviders,
        catalog: SharedCatalog,
        spawner: SharedSpawner,
        harness: Option<Arc<HostHarness>>,
    ) -> Result<Self, String> {
        let engine = Self {
            inner: Arc::new(Inner {
                workspace: WorkspaceCommands::new(store.clone()),
                store,
                providers,
                catalog,
                spawner,
                state: Mutex::new(State::default()),
                closing: AtomicBool::new(false),
                retry_delay: Duration::from_secs(1),
                idle_park: Mutex::new(IDLE_PARK),
                harness,
                #[cfg(test)]
                save_fault: Mutex::new(None),
            }),
        };
        let inner = &engine.inner;
        for value in inner.store.sessions(None)? {
            let mut recovered = value;
            if recovered.status == HostSessionStatus::Running {
                recovered = (*inner.save(
                    Inner::settled(
                        &recovered,
                        HostSessionStatus::Interrupted,
                        Some("Host restarted. This turn was interrupted; inspect its work before continuing."),
                        recovered.updated_at,
                    ),
                    &json!({ "type": "interrupted" }),
                )?)
                .clone();
            }
            // A delivery the previous host left open cannot be completed. A
            // submitted request waits for inspection; any other goes back to
            // a draft.
            let open = recovered
                .session
                .provider_context
                .as_ref()
                .and_then(|context| context.delivery.as_ref())
                .filter(|delivery| {
                    !matches!(
                        delivery.status,
                        TransferStatus::Accepted | TransferStatus::Uncertain
                    )
                })
                .map(|delivery| delivery.switch_id.clone());
            if let Some(switch_id) = open {
                end_unaccepted_delivery(&mut recovered.session, &switch_id);
                recovered = (*inner.save(
                    recovered,
                    &json!({ "type": "providerContext.recovered", "switchId": switch_id }),
                )?)
                .clone();
            }
            if let Some(provider_session_id) = can_rebind(&recovered.session) {
                inner.provider(recovered.session.harness)?.bind(
                    &recovered.session.id,
                    provider_session_id,
                    &recovered.session.cwd,
                );
            }
        }
        Ok(engine)
    }

    pub fn store(&self) -> &HostStore {
        &self.inner.store
    }

    pub fn store_arc(&self) -> Arc<HostStore> {
        self.inner.store.clone()
    }

    pub fn catalog(&self) -> &SharedCatalog {
        &self.inner.catalog
    }

    #[cfg(test)]
    pub(crate) fn set_idle_park(&self, delay: Duration) {
        *self.inner.idle_park.lock() = delay;
    }

    #[cfg(test)]
    pub(crate) fn set_save_fault(&self, fault: Option<SaveFault>) {
        *self.inner.save_fault.lock() = fault;
    }

    /// `openProject`: registers an absolute directory.
    pub fn open_project(&self, path: &str) -> Result<HostProject, String> {
        if !std::path::Path::new(path).is_absolute() || path.contains('\0') {
            return Err("Choose an absolute directory path on the host".into());
        }
        let cwd = dunce::canonicalize(path).map_err(|error| error.to_string())?;
        if !std::fs::metadata(&cwd).is_ok_and(|meta| meta.is_dir()) {
            return Err("Project path is not a directory".into());
        }
        #[cfg(windows)]
        for project in self.inner.store.projects()? {
            if dunce::canonicalize(&project.cwd).is_ok_and(|existing| existing == cwd) {
                return Ok(project);
            }
        }
        let name = cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.inner.store.add_project(&cwd.to_string_lossy(), &name)
    }

    /// `withIdleProject`: runs a branch change while no session in the
    /// project is running. With `force`, the change goes ahead anyway, after
    /// the desktop has asked; the running agents then see their files
    /// change, as they would locally.
    pub fn with_idle_project<T>(
        &self,
        project_id: &str,
        force: bool,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        {
            let mut state = self.inner.state.lock();
            if state.switching_projects.contains(project_id) {
                return Err("A branch switch is already in progress".into());
            }
            let running: Vec<HostSessionSummary> = self
                .inner
                .store
                .summaries(project_id)?
                .into_iter()
                .filter(|session| session.status == HostSessionStatus::Running)
                .collect();
            if !running.is_empty() && !force {
                return Err(running_sessions_message(&running));
            }
            state.switching_projects.insert(project_id.to_string());
        }
        let result = action();
        self.inner
            .state
            .lock()
            .switching_projects
            .remove(project_id);
        result
    }

    /// `updateSession`: flushes batched output, then applies the metadata
    /// change.
    pub fn update_session(
        &self,
        id: &str,
        patch: &SessionPatch,
    ) -> Result<HostSessionSummary, String> {
        let inner = &self.inner;
        let mut state = inner.state.lock();
        inner.flush(&mut state, id)?;
        let summary = inner.store.update_session(id, patch)?;
        if let Some(live) = state.live.get_mut(id) {
            live.value = (*inner.store.session(id)?).clone();
        }
        Ok(summary)
    }

    /// `command`: validates and applies one `HostCommand`. A receipt means
    /// durable host acceptance, not provider completion.
    pub fn command(&self, raw: &Value) -> Result<CommandReceipt, String> {
        let inner = &self.inner;
        if inner.closing.load(Ordering::SeqCst) {
            return Err("Host is stopping".into());
        }
        let command = parse_command(raw)?;
        let serialized = serde_json::to_string(&command).map_err(|error| error.to_string())?;
        let signature: String = Sha256::digest(serialized.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let command_id = command_id(&command).to_string();
        if let Some(previous) = inner.store.receipt(&command_id, &signature)? {
            return Ok(previous);
        }
        let mut guard = inner.state.lock();
        let state = &mut *guard;
        // Commands apply to the latest state, including batched output.
        if let Some(session_id) = session_id(&command) {
            inner.flush(state, session_id)?;
        }
        let mut effect: Option<Effect> = None;
        let (receipt, saved) = inner.store.transaction(|| {
            let mut value = self.prepare(state, &command, &mut effect)?;
            value.revision += 1;
            if !matches!(command, HostCommand::Create { .. }) {
                value.updated_at = now_ms();
            }
            let saved = inner.persist(value, &json!({ "type": "command", "command": command }))?;
            let receipt = CommandReceipt {
                command_id: command_id.clone(),
                session_id: saved.session.id.clone(),
                revision: saved.revision,
            };
            inner.store.record_receipt(&signature, &receipt)?;
            Ok((receipt, saved))
        })?;
        let id = saved.session.id.clone();
        if let Some(live) = state.live.get_mut(&id) {
            live.value = (*saved).clone();
        }
        let provider = inner.provider(saved.session.harness);
        match effect {
            None => {}
            Some(Effect::Run {
                prompt,
                intent,
                attachments,
                first_turn,
                transfer,
            }) => {
                inner.run(
                    state,
                    &saved,
                    prompt.clone(),
                    intent,
                    attachments,
                    transfer.map(|transfer| *transfer),
                );
                drop(guard);
                if let Some((message, placeholder_title)) = first_turn {
                    inner.generate_first_turn_names(&saved, &message, placeholder_title);
                }
            }
            Some(Effect::Cancel) => {
                if let Some(active) = state.running.get_mut(&id) {
                    active.cancelled = true;
                }
                drop(guard);
                let provider = provider?;
                inner.spawner.spawn(
                    async move {
                        if provider.cancel(&id).await.is_err() {
                            let _ = provider.stop(&id).await;
                        }
                    }
                    .boxed(),
                );
            }
            Some(Effect::Approve {
                request_id,
                decision,
            }) => {
                drop(guard);
                let decision = match decision {
                    ApprovalDecision::Allow => {
                        monocode_core::harness_event::ApprovalDecision::Allow
                    }
                    ApprovalDecision::Deny => monocode_core::harness_event::ApprovalDecision::Deny,
                };
                provider?.approve(&id, request_id, decision)?;
            }
            Some(Effect::Answer { request_id, reply }) => {
                drop(guard);
                // TODO(port): as in TypeScript, a provider without questions
                // fails here after the command was recorded.
                provider?.answer(&id, request_id, reply)?;
            }
        }
        Ok(receipt)
    }

    /// The new session value for `command`, inside the command transaction.
    fn prepare(
        &self,
        state: &mut State,
        command: &HostCommand,
        effect: &mut Option<Effect>,
    ) -> Result<HostSession, String> {
        let inner = &self.inner;
        let store = &inner.store;
        if let HostCommand::Create {
            project_id,
            worktree_cwd,
            auto_worktree_branch,
            harness,
            model,
            model_settings,
            runtime_mode,
            ..
        } = command
        {
            let project = store.project(project_id)?;
            if state.switching_projects.contains(&project.id) {
                return Err("Wait for the branch switch to finish".into());
            }
            inner.provider(*harness)?;
            let cwd = resolve_host_worktree(
                &project.cwd,
                worktree_cwd
                    .as_ref()
                    .map(|cwd| Value::String(cwd.clone()))
                    .as_ref(),
            )?;
            let now = now_ms();
            let mut session = Session::blank(
                uuid::Uuid::new_v4().to_string(),
                *harness,
                model.clone(),
                cwd.clone(),
            );
            session.runtime_mode = *runtime_mode;
            session.model_settings = model_settings.clone().unwrap_or_default();
            session.title = PLACEHOLDER_TITLE.into();
            if auto_worktree_branch.is_some() {
                session.branch = auto_worktree_branch.clone();
                session.worktree_cwd = Some(cwd);
            }
            return Ok(HostSession {
                session,
                project_id: project.id,
                revision: 0,
                run_id: None,
                status: HostSessionStatus::Idle,
                created_at: Some(now),
                updated_at: now,
                archived: None,
                pinned: None,
                auto_worktree_branch: auto_worktree_branch.clone(),
                block_revisions: None,
                extra: Default::default(),
            });
        }
        let session_id = session_id(command).unwrap_or_default();
        let mut value = (*store.session(session_id)?).clone();
        let starts_turn = matches!(
            command,
            HostCommand::Send { .. } | HostCommand::Compact { .. }
        );
        if starts_turn && state.switching_projects.contains(&value.project_id) {
            return Err("Wait for the branch switch to finish".into());
        }
        let provider = inner.provider(value.session.harness)?;
        let running = value.status == HostSessionStatus::Running;
        let needs_inspection = value
            .session
            .provider_context
            .as_ref()
            .and_then(|context| context.delivery.as_ref())
            .is_some_and(|delivery| delivery.needs_inspection());
        if needs_inspection
            && matches!(
                command,
                HostCommand::Send { .. }
                    | HostCommand::Compact { .. }
                    | HostCommand::SwitchProvider { .. }
            )
        {
            return Err(
                "Inspect the interrupted provider request and confirm inspection before continuing"
                    .into(),
            );
        }
        match command {
            HostCommand::Create { .. } => unreachable!("handled above"),
            HostCommand::ConfirmProviderInspection {
                expected_revision, ..
            } => {
                if running {
                    return Err(
                        "Wait for host storage to reconcile before confirming inspection".into(),
                    );
                }
                if value.revision != *expected_revision {
                    return Err(
                        "Session changed on the host. Reload it before confirming inspection"
                            .into(),
                    );
                }
                if !needs_inspection {
                    return Err("This session does not need inspection confirmation".into());
                }
                confirm_provider_delivery_inspection(&mut value.session);
            }
            HostCommand::SwitchProvider {
                expected_revision,
                harness,
                model,
                model_settings,
                runtime_mode,
                ..
            } => {
                if running {
                    return Err("Wait for the current turn before changing providers".into());
                }
                if value.revision != *expected_revision {
                    return Err(
                        "Session changed on the host. Reload it before changing providers".into(),
                    );
                }
                let target_provider = inner.provider(*harness)?;
                switch_provider(
                    &mut value.session,
                    *harness,
                    target_provider
                        .context_transfer_capabilities()
                        .is_some_and(|capabilities| capabilities.resumed_append),
                );
                value.session.model = model.clone();
                value.session.model_settings = model_settings.clone();
                value.session.runtime_mode = *runtime_mode;
            }
            HostCommand::Configure {
                model,
                model_settings,
                runtime_mode,
                ..
            } => {
                if running {
                    return Err("Wait for the current turn before changing settings".into());
                }
                value.session.model = model.clone();
                value.session.model_settings = model_settings.clone();
                value.session.runtime_mode = *runtime_mode;
            }
            HostCommand::Draft {
                command_id,
                text,
                attachments,
                ..
            } => {
                if running || value.session.blocks.iter().any(Block::is_draft) {
                    return Err("This session cannot save another draft right now".into());
                }
                let attachments =
                    resolve_attachments(store, attachments.as_deref().unwrap_or_default())?;
                if value.session.blocks.is_empty() {
                    value.session.title =
                        title_from_prompt(text, value.session.harness, &attachments);
                }
                let mut block = Block::new(command_id.clone(), BlockRole::User, text.clone());
                if !attachments.is_empty() {
                    block.attachments = Some(attachments);
                }
                block.draft = Some(true);
                value.session.blocks.push(block);
            }
            HostCommand::RemoveDraft { draft_block_id, .. } => {
                let index = value
                    .session
                    .blocks
                    .iter()
                    .position(|block| block.id == *draft_block_id && block.is_draft())
                    .ok_or("Draft not found")?;
                let draft_id = value.session.blocks[index].id.clone();
                value.session.blocks.retain(|block| block.id != draft_id);
            }
            HostCommand::Send { .. } | HostCommand::Compact { .. } => {
                self.prepare_turn(&mut value, command, &provider, effect)?;
            }
            HostCommand::Cancel { run_id, .. }
            | HostCommand::Approve { run_id, .. }
            | HostCommand::Answer { run_id, .. } => {
                if value.run_id.as_deref() != Some(run_id.as_str()) || !running {
                    return Err("This request belongs to a finished or replaced turn".into());
                }
                match command {
                    HostCommand::Cancel { .. } => *effect = Some(Effect::Cancel),
                    HostCommand::Approve {
                        request_id,
                        decision,
                        ..
                    } => {
                        let pending = value.session.blocks.iter().any(|block| {
                            block.approval.as_ref().is_some_and(|approval| {
                                approval.request_id == *request_id && approval.decided.is_none()
                            })
                        });
                        if !pending {
                            return Err("Approval is already resolved".into());
                        }
                        apply_harness_event_mut(
                            &mut SystemEnv,
                            &mut value.session,
                            &HarnessEvent::ApprovalResolved {
                                request_id: *request_id,
                                decision: match decision {
                                    ApprovalDecision::Allow => ApprovalDecided::Allow,
                                    ApprovalDecision::Deny => ApprovalDecided::Deny,
                                },
                            },
                        );
                        *effect = Some(Effect::Approve {
                            request_id: *request_id,
                            decision: *decision,
                        });
                    }
                    HostCommand::Answer {
                        request_id, reply, ..
                    } => {
                        if value
                            .session
                            .pending_question
                            .as_ref()
                            .map(|question| question.request_id)
                            != Some(*request_id)
                        {
                            return Err("Question is already resolved".into());
                        }
                        value.session.pending_question = None;
                        *effect = Some(Effect::Answer {
                            request_id: *request_id,
                            reply: reply.clone(),
                        });
                    }
                    _ => unreachable!("matched above"),
                }
            }
        }
        Ok(value)
    }

    /// The `send` and `compact` half of `command`.
    fn prepare_turn(
        &self,
        value: &mut HostSession,
        command: &HostCommand,
        provider: &Arc<dyn HostProvider>,
        effect: &mut Option<Effect>,
    ) -> Result<(), String> {
        let inner = &self.inner;
        let (command_id, text, attachments, intent, draft_block_id, plan_block_id, send) =
            match command {
                HostCommand::Send {
                    command_id,
                    text,
                    attachments,
                    intent,
                    draft_block_id,
                    plan_block_id,
                    ..
                } => (
                    command_id,
                    text.as_str(),
                    attachments.as_deref(),
                    *intent,
                    draft_block_id.as_deref(),
                    plan_block_id.as_deref(),
                    true,
                ),
                HostCommand::Compact { command_id, .. } => {
                    (command_id, "", None, None, None, None, false)
                }
                _ => unreachable!("only send and compact start turns"),
            };
        if value.status == HostSessionStatus::Running {
            return Err("This session is already running".into());
        }
        if !send && !provider.can_compact() {
            return Err("Context compaction is unavailable for this provider".into());
        }
        if !send
            && (value.session.pending_switch.is_some()
                || requires_fresh_provider_binding(
                    &value.session,
                    value.session.harness,
                    &value.session.cwd,
                    None,
                ))
        {
            return Err("Send a turn with shared history before compacting this provider".into());
        }
        let blocks = &value.session.blocks;
        let draft = draft_block_id.and_then(|id| {
            blocks
                .iter()
                .find(|block| block.id == id && block.is_draft())
        });
        if draft_block_id.is_some() && draft.is_none() {
            return Err("Draft not found".into());
        }
        let plan_index = plan_block_id.and_then(|id| {
            blocks
                .iter()
                .position(|block| block.id == id && block.role == BlockRole::Plan)
        });
        if plan_block_id.is_some() {
            let ready = plan_index.map(|index| &blocks[index]).is_some_and(|plan| {
                !monocode_core::js::trim(&plan.text).is_empty()
                    && !plan.is_streaming()
                    && !plan.plan.as_ref().is_some_and(|meta| {
                        matches!(meta.status, PlanStatus::Building | PlanStatus::Built)
                    })
            });
            if !ready {
                return Err("Plan is not ready to build".into());
            }
        }
        let attachments = if send {
            match draft.and_then(|draft| draft.attachments.clone()) {
                Some(attachments) => attachments,
                None => resolve_attachments(&inner.store, attachments.unwrap_or_default())?,
            }
        } else {
            Vec::new()
        };
        let transfer = if send {
            self.prepare_transfer(value, command_id, text, &attachments, provider)?
        } else {
            None
        };
        let blocks = &value.session.blocks;
        let run_id = uuid::Uuid::new_v4().to_string();
        let first_turn = send && !blocks.iter().any(|block| !block.is_draft());
        let harness = value.session.harness;
        let placeholder_title = value.session.title == PLACEHOLDER_TITLE
            || can_replace_session_title(&value.session.title, harness, harness.label());
        let model = inner
            .catalog
            .read()
            .resolve_model(harness, Some(&value.session.model));
        let turn_model = TurnModel {
            harness,
            id: value.session.model.clone(),
            name: if model.id == value.session.model {
                model.name.clone()
            } else {
                model_slug(&value.session.model).to_string()
            },
            extra: Default::default(),
        };
        let mut next_blocks: Vec<Block> = Vec::with_capacity(blocks.len() + 1);
        for (index, block) in blocks.iter().enumerate() {
            if block.is_draft() {
                continue;
            }
            let mut block = block.clone();
            if Some(index) == plan_index {
                let mut plan = block.plan.clone().unwrap_or(PlanBlockMeta {
                    status: PlanStatus::Ready,
                    ..Default::default()
                });
                plan.status = PlanStatus::Building;
                plan.approved_text = Some(block.text.clone());
                block.plan = Some(plan);
            }
            next_blocks.push(block);
        }
        let mut user = Block::new(
            command_id.clone(),
            BlockRole::User,
            if send { text } else { "/compact" },
        );
        if !attachments.is_empty() {
            user.attachments = Some(attachments.clone());
        }
        user.started_at = Some(now_ms());
        user.turn_model = Some(turn_model);
        next_blocks.push(user);
        value.status = HostSessionStatus::Running;
        value.run_id = Some(run_id);
        value.session.busy = Some(true);
        value.session.pending_question = None;
        // A new turn retries after a usage limit, as it does locally.
        value.session.usage_limit = None;
        if first_turn && placeholder_title {
            value.session.title = title_from_prompt(text, harness, &attachments);
        }
        value.session.blocks = next_blocks;
        // Saved before dispatch: after a crash the request may have run.
        if let Some(transfer) = &transfer {
            mark_provider_request_submitted(&mut value.session, &transfer.switch_id);
        }
        *effect = Some(Effect::Run {
            prompt: send.then(|| text.to_string()),
            intent: if send { intent } else { None },
            attachments,
            first_turn: (first_turn && send).then(|| (text.to_string(), placeholder_title)),
            transfer: transfer.map(Box::new),
        });
        Ok(())
    }

    /// The shared history a send delivers after a provider switch, or after
    /// a delivery left the target's conversation uncertain. Adds the
    /// handoff row and starts the delivery receipt. An error, such as a
    /// request too large for the target's context, leaves the session
    /// unchanged.
    fn prepare_transfer(
        &self,
        value: &mut HostSession,
        command_id: &str,
        text: &str,
        attachments: &[Attachment],
        provider: &Arc<dyn HostProvider>,
    ) -> Result<Option<Transfer>, String> {
        let session = &value.session;
        let (harness, cwd) = (session.harness, session.cwd.clone());
        let uncertain = requires_fresh_provider_binding(session, harness, &cwd, None);
        let switching = session
            .pending_switch
            .as_ref()
            .is_some_and(|pending| pending.from != harness);
        if !switching && !uncertain {
            return Ok(None);
        }
        let switch_id = command_id.to_string();
        let binding = provider_binding(session, harness, &cwd, None);
        let resumable = !uncertain
            && provider
                .context_transfer_capabilities()
                .is_some_and(|capabilities| capabilities.resumed_append)
            && can_resume_provider_binding(session, binding.as_ref());
        let resumed = binding.filter(|_| resumable);
        let directory = self.context_directory(&session.id)?;
        let assets = snapshot_host_context_assets(
            &directory.join("assets"),
            &historical_context_attachments(session, None),
            ContextAssetLimits::default(),
        )?;
        let attachment_tokens = current_attachment_tokens(attachments);
        let context = self.portable_history(
            session,
            &switch_id,
            text,
            resumed.as_ref(),
            &assets,
            attachment_tokens,
        )?;
        let fallback_context = match &resumed {
            Some(_) => self.portable_history(
                session,
                &format!("{switch_id}-fallback"),
                text,
                None,
                &assets,
                attachment_tokens,
            )?,
            None => context.clone(),
        };
        let from = session
            .pending_switch
            .as_ref()
            .map(|pending| pending.from)
            .or_else(|| {
                session
                    .provider_context
                    .as_ref()
                    .and_then(|context| context.delivery.as_ref())
                    .map(|delivery| delivery.from)
            })
            .unwrap_or(harness);
        let session = &mut value.session;
        session.provider_session_id = resumed
            .as_ref()
            .map(|binding| binding.provider_session_id.clone());
        let mut row = Block::new(
            format!("{command_id}-context"),
            BlockRole::Handoff,
            format!(
                "Preparing shared history. Selected {} transcript items and omitted {}. Historical attachments remain file references.",
                context.items.len(),
                context.omitted.len()
            ),
        );
        row.handoff = Some(HandoffMeta {
            from,
            to: harness,
            status: HandoffStatus::Preparing,
            pending: Some(true),
            transfer: None,
            extra: Default::default(),
        });
        session.blocks.push(row);
        begin_provider_delivery(
            session,
            DeliveryStart {
                switch_id: switch_id.clone(),
                from: Some(from),
                to: Some(harness),
                cwd,
                provider_account_id: None,
                current_user_block_id: command_id.to_string(),
                source_through_block_id: context.through_block_id.clone(),
                included_block_ids: context
                    .items
                    .iter()
                    .map(|item| item.source_block_id.clone())
                    .collect(),
                omitted_block_ids: context.omitted.iter().map(|item| item.id.clone()).collect(),
                target_provider_session_id: resumed
                    .as_ref()
                    .map(|binding| binding.provider_session_id.clone()),
            },
        );
        let historical: u64 = context
            .items
            .iter()
            .map(|item| item.attachments.as_ref().map_or(0, Vec::len) as u64)
            .sum();
        let retrieval_path = context.retrieval_path.clone();
        update_provider_handoff(session, &switch_id, |transfer| {
            transfer.historical_attachments = historical;
            transfer.retrieval_path = retrieval_path.clone();
        });
        Ok(Some(Transfer {
            context,
            fallback_context,
            switch_id,
            fresh: !resumable,
        }))
    }

    /// `contextDirectory`: where this host keeps a session's shared
    /// history. Fails once the session was deleted.
    fn context_directory(&self, id: &str) -> Result<std::path::PathBuf, String> {
        let store = &self.inner.store;
        store.assert_context_writable(id)?;
        store.context_directory(id)
    }

    /// `portableHistory`: the history `binding` lacks, or all of it. When
    /// the budget leaves items out, the full history is saved as a file the
    /// target can read.
    fn portable_history(
        &self,
        session: &Session,
        switch_id: &str,
        current_request: &str,
        binding: Option<&ProviderBinding>,
        assets: &[ContextAssetSnapshot],
        attachment_tokens: usize,
    ) -> Result<PortableContext, String> {
        let window = self
            .inner
            .catalog
            .read()
            .model_context_window(&session.model)
            .or_else(|| binding.and_then(|binding| binding.context_window))
            .map(|window| window.max(0) as usize);
        let mut context = build_portable_context(
            session,
            &PortableContextOptions {
                after_block_id: binding
                    .and_then(|binding| binding.delivered_through_block_id.as_deref()),
                current_request: Some(current_request),
                window_tokens: window,
                occupied_tokens: binding
                    .and_then(|binding| binding.context_used)
                    .map(|used| used.max(0) as usize),
                attachment_tokens: Some(attachment_tokens),
                asset_snapshots: assets,
                ..Default::default()
            },
        )?;
        if context
            .omitted
            .iter()
            .any(|item| item.reason == OmissionReason::Budget)
        {
            let directory = self.context_directory(&session.id)?;
            create_private_dir(&directory)?;
            let name = format!("{:x}.md", Sha256::digest(switch_id.as_bytes()));
            let path = directory.join(name);
            let snapshot = build_portable_context_snapshot(
                session,
                context.through_block_id.as_deref(),
                assets,
            )?;
            write_private_file(&path, snapshot.as_bytes())?;
            context.retrieval_path = Some(path.to_string_lossy().into_owned());
        }
        Ok(context)
    }

    /// `close`: stops running providers and waits for their turns to
    /// settle.
    pub fn close(&self) {
        let inner = &self.inner;
        if inner.closing.swap(true, Ordering::SeqCst) {
            return;
        }
        let (stops, dones) = {
            let mut state = inner.state.lock();
            state.retry_timers.clear();
            state.idle_timers.clear();
            // Persistent providers keep children for idle sessions too.
            let mut ids: HashSet<String> = state.running.keys().cloned().collect();
            ids.extend(
                inner
                    .store
                    .sessions(None)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|value| value.session.id),
            );
            let stops: Vec<_> = ids
                .iter()
                .filter_map(|id| {
                    let harness = inner.store.session(id).ok()?.session.harness;
                    let provider = inner.provider(harness).ok()?;
                    Some(provider.stop(id))
                })
                .collect();
            let dones: Vec<Done> = state
                .running
                .values()
                .map(|active| active.done.clone())
                .collect();
            (stops, dones)
        };
        smol::block_on(async {
            for result in futures::future::join_all(stops).await {
                if let Err(error) = result {
                    log::error!("Could not stop a provider: {error}");
                }
            }
            // TODO(port): TypeScript waited for every turn without a limit.
            // A provider whose send never returns after it was stopped (Codex
            // while it starts) would keep the host from exiting, so the wait
            // is bounded. A turn left running is marked interrupted when the
            // host starts again.
            let settled = smol::future::or(
                async {
                    futures::future::join_all(dones).await;
                    true
                },
                async {
                    smol::Timer::after(CLOSE_WAIT).await;
                    false
                },
            )
            .await;
            if !settled {
                log::error!("A provider turn did not stop; closing the host anyway");
            }
        });
        // A native turn has no send to settle it.
        {
            let mut state = inner.state.lock();
            let state = &mut *state;
            let native: Vec<String> = state
                .live
                .keys()
                .filter(|id| !state.running.contains_key(*id))
                .cloned()
                .collect();
            for id in native {
                let saved = inner.flush(state, &id).and_then(|()| {
                    let latest = inner.store.session(&id)?;
                    inner.save(
                        Inner::settled(
                            &latest,
                            HostSessionStatus::Interrupted,
                            Some("Host stopped. This turn was interrupted."),
                            now_ms(),
                        ),
                        &json!({ "type": "interrupted" }),
                    )
                });
                if let Err(error) = saved {
                    log::error!("Could not settle a native turn on close: {error}");
                }
                state.live.remove(&id);
            }
        }
        if let Some(harness) = &inner.harness {
            harness.close();
        }
    }

    /// The real harness, when this engine runs providers.
    pub fn harness(&self) -> Option<&Arc<HostHarness>> {
        self.inner.harness.as_ref()
    }

    pub fn workspace(&self) -> &WorkspaceCommands {
        &self.inner.workspace
    }

    /// Whether `id` has a turn the engine is still running.
    pub fn is_running(&self, id: &str) -> bool {
        self.inner.state.lock().running.contains_key(id)
    }
}

fn command_id(command: &HostCommand) -> &str {
    match command {
        HostCommand::Create { command_id, .. }
        | HostCommand::Configure { command_id, .. }
        | HostCommand::SwitchProvider { command_id, .. }
        | HostCommand::ConfirmProviderInspection { command_id, .. }
        | HostCommand::Compact { command_id, .. }
        | HostCommand::Send { command_id, .. }
        | HostCommand::Draft { command_id, .. }
        | HostCommand::RemoveDraft { command_id, .. }
        | HostCommand::Cancel { command_id, .. }
        | HostCommand::Approve { command_id, .. }
        | HostCommand::Answer { command_id, .. } => command_id,
    }
}

fn session_id(command: &HostCommand) -> Option<&str> {
    match command {
        HostCommand::Create { .. } => None,
        HostCommand::Configure { session_id, .. }
        | HostCommand::SwitchProvider { session_id, .. }
        | HostCommand::ConfirmProviderInspection { session_id, .. }
        | HostCommand::Compact { session_id, .. }
        | HostCommand::Send { session_id, .. }
        | HostCommand::Draft { session_id, .. }
        | HostCommand::RemoveDraft { session_id, .. }
        | HostCommand::Cancel { session_id, .. }
        | HostCommand::Approve { session_id, .. }
        | HostCommand::Answer { session_id, .. } => Some(session_id),
    }
}

#[cfg(test)]
mod tests;

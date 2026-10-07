//! The asynchronous half of `submitSession`: the draft worktree, the
//! handoff recap, the provider rewind for an edited resend, the turn itself
//! with its event routing, orchestration proposals, and the cleanup when the
//! turn ends.
//!
//! Harness adapters report events on any thread. They go through a channel
//! to this foreground task, which applies them in order and drains the rest
//! once the adapter's future resolves, so the order matches the TypeScript
//! callbacks.

use std::rc::Rc;

use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::BoxFuture;
use gpui::{AsyncApp, WeakEntity};
use monocode_core::block::TransferMode;
use monocode_core::block::{Block, ModelTarget, PlanStatus, TurnIntent};
use monocode_core::handoff::HandoffComposerCard;
use monocode_core::harness_event::{HarnessSessionInput, RewindLastTurnInput, SendTurnInput};
use monocode_core::orchestration::{
    OrchestrationProposal, OrchestrationProposalStatus, with_orchestration_proposal,
};
use monocode_core::paths::path_key;
use monocode_core::plan::{
    build_plan_prompt, is_provider_failure_text, plan_turn_key, plan_turn_prompt,
};
use monocode_core::provider_context::{
    DeliveryCoverage, accept_provider_delivery, can_apply_running_configuration,
    fail_provider_delivery, mark_provider_context_delivered, mark_provider_request_submitted,
    record_provider_bound, record_provider_context_usage, recover_submitted_provider_delivery,
    settle_provider_binding,
};
use monocode_core::reducer::{
    MessageParts, now_ms, promote_last_assistant_to_plan_mut, stop_streaming_mut,
};
use monocode_core::session::{PendingHarnessSwitch, session_work_cwd};
use monocode_core::{Attachment, HarnessEvent, HarnessId, Session, js};
use monocode_harness::core::context_transfer::{
    ContextTransferInput, ContextTransferReceipt, DeliveredHook, DeliveryMode,
};
use monocode_harness::core::registry::{AcceptedHook, EventSink, TitleInput};
use monocode_harness::core::text_harness::pick_text_harness;

use super::actions::nudge_after_turn;
use super::session_edits::{
    keep_tail, named_worktree_branch, shell_path, temporary_worktree_branch_name, with_plan_status,
};
use super::submit::{CommitTurn, reject_edited, report};
use super::{Submit, SubmitConfig, SubmitOptions, settle};
use crate::runtime::checkpoint::{begin_session_turn, notify_review_changed};
use crate::runtime::edits::{nudge_open_editors, track_session_edits};
use crate::runtime::engine::Engine;
use crate::runtime::in_flight::CONTINUE_PROMPT;
use crate::submit::acceptance::{ControlOutcome, ControlStatus};
use crate::submit::app_access::TurnAppAccess;
use crate::submit::attachments::prepare_attachments;
use crate::submit::edit_last_turn::EditedResendAttempt;
use crate::submit::handoff::{
    complete_handoff, consume_handoff, is_preparing_handoff, user_messages_after_handoff,
    wrap_handoff_prompt,
};
use crate::submit::hooks::SubmitPeers;
use crate::submit::prompt::prepare_prompt;
use crate::submit::provider_switch::{
    ACCEPTANCE_SAVE_FAILED, TRANSFER_FAILED, save_provider_context_session,
};
use crate::submit::session_context::expand_dropped_sessions;
use crate::submit::skills::{SkillCatalog, SkillCatalogContext};

/// What an adapter reports while a harness call runs.
pub(crate) enum TurnSignal {
    Event(Box<HarnessEvent>),
    /// `onAccepted`: the provider took the user turn.
    Accepted,
    /// `onDelivered`: shared history reached the target. The reply says
    /// whether its receipt was saved.
    Delivered(ContextTransferReceipt, oneshot::Sender<Result<(), String>>),
}

/// An event sink and accepted hook that feed one channel.
pub(crate) fn signal_channel() -> (EventSink, AcceptedHook, async_channel::Receiver<TurnSignal>) {
    let (sink, accepted, _, receiver) = signal_channel_with_delivery();
    (sink, accepted, receiver)
}

/// [`signal_channel`] with the delivery hook a shared-history transfer
/// reports through.
pub(crate) fn signal_channel_with_delivery() -> (
    EventSink,
    AcceptedHook,
    DeliveredHook,
    async_channel::Receiver<TurnSignal>,
) {
    let (sender, receiver) = async_channel::unbounded();
    let events = sender.clone();
    let sink: EventSink = std::sync::Arc::new(move |event| {
        let _ = events.try_send(TurnSignal::Event(Box::new(event)));
    });
    let accepts = sender.clone();
    let accepted: AcceptedHook = std::sync::Arc::new(move || {
        let _ = accepts.try_send(TurnSignal::Accepted);
    });
    let delivered: DeliveredHook = std::sync::Arc::new(move |receipt| {
        let (reply, saved) = oneshot::channel();
        let _ = sender.try_send(TurnSignal::Delivered(receipt, reply));
        async move {
            saved
                .await
                .unwrap_or_else(|_| Err("The turn ended before the receipt was saved.".into()))
        }
        .boxed()
    });
    (sink, accepted, delivered, receiver)
}

/// Run a harness call and hand each signal to `on_signal` as it arrives.
/// Signals sent before the call resolved are handled before it returns.
pub(crate) async fn drive<T>(
    operation: BoxFuture<'static, T>,
    signals: &async_channel::Receiver<TurnSignal>,
    mut on_signal: impl FnMut(TurnSignal),
) -> T {
    let mut operation = operation.fuse();
    loop {
        let next = signals.recv().fuse();
        futures::pin_mut!(next);
        futures::select_biased! {
            signal = next => match signal {
                Ok(signal) => on_signal(signal),
                Err(_) => {
                    let result = operation.await;
                    return result;
                }
            },
            result = operation => {
                while let Ok(signal) = signals.try_recv() {
                    on_signal(signal);
                }
                return result;
            }
        }
    }
}

/// `preparePrompt` with the prompt hooks.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare(
    text: &str,
    harness: HarnessId,
    session_id: &str,
    cwd: &str,
    account_id: Option<&str>,
    resolve_context: &super::SkillContextResolver,
    skills: &SkillCatalog,
    peers: &SubmitPeers,
    cx: &AsyncApp,
) -> String {
    let mut context = SkillCatalogContext::new(harness, cwd).with_session(session_id);
    if let Some(account_id) = account_id {
        context = context.with_account(account_id);
    }
    let context = resolve_context(context);
    let prompt = prepare_prompt(
        text,
        &context,
        skills,
        |text| cx.update(|cx| peers.prompt.apply_file_mentions(text, cwd, cx)),
        |text| cx.update(|cx| peers.prompt.apply_notes(text, cx)),
    )
    .await;
    // Expand dropped sessions last, so mention and note expansion never
    // rewrites another session's history.
    expand_dropped_sessions(prompt, cx).await
}

/// A recap waiting to reach the incoming provider.
#[derive(Debug, Clone)]
pub(crate) struct Wrap {
    pub from: HarnessId,
    pub text: String,
}

/// Everything the turn task needs, captured when the turn starts.
pub(crate) struct TurnRun {
    pub session_id: String,
    pub generation: u64,
    pub current: Session,
    pub attachments: Vec<Attachment>,
    pub options: SubmitOptions,
    pub intent: TurnIntent,
    pub harness_text: String,
    pub raw_command: bool,
    pub operator_matched: bool,
    pub operator_access: bool,
    /// Every app action the turn may use, `/operator` or not.
    pub app_access: TurnAppAccess,
    /// The short `<monocode_app>` block, when the agent has not seen it.
    pub app_note: Option<String>,
    pub provider_account_id: Option<String>,
    pub initial_work_cwd: String,
    pub create_draft_worktree: bool,
    pub is_first_turn: bool,
    #[allow(dead_code)]
    pub placeholder_title: bool,
    pub title_seed: String,
    pub generate_title: bool,
    pub approved_plan: Option<Block>,
    pub edited: Option<Rc<EditedResendAttempt>>,
    pub edited_provider_turn_id: Option<String>,
    pub pending_switch: Option<PendingHarnessSwitch>,
    pub queued_handoff: Option<Wrap>,
    pub handoff_card: Option<HandoffComposerCard>,
    pub proposal_id: Option<String>,
    pub proposal_draft: Option<OrchestrationProposal>,
    pub commit: Rc<CommitTurn>,
    pub config: SubmitConfig,
    pub peers: SubmitPeers,
    pub skills: SkillCatalog,
    /// The picker revision when the turn started.
    pub selection_revision: u64,
    /// The `Submit` entity, set when the turn task starts.
    pub submit: Option<WeakEntity<Submit>>,
}

/// The turn's mutable state (the `let` bindings in the TypeScript closure).
pub(super) struct TurnState {
    control_text: String,
    /// Provider parts behind `control_text` and `proposal_text`, so a
    /// corrected part replaces its text.
    message_parts: MessageParts,
    control_error: Option<String>,
    proposal: Option<OrchestrationProposal>,
    proposal_text: String,
    native_proposal_text: String,
    completed_proposal: Option<OrchestrationProposal>,
    provider_failure_seen: bool,
    native_plan_seen: bool,
    build_succeeded: bool,
    pending_edited_events: Vec<HarnessEvent>,
    pub(super) work_cwd: String,
    pub(super) wrap: Option<Wrap>,
    plan_event_key: String,
    /// The prepared attachments and the exact first request, composed
    /// before any history is exported.
    pub(super) prepared: Vec<Attachment>,
    pub(super) send_text: String,
    /// Shared history for a provider switch, until the target accepts.
    pub(super) transfer: Option<ContextTransferInput>,
    pub(super) transfer_switch_id: Option<String>,
    provider_accepted: bool,
    /// The request went to the provider, so it may have run.
    provider_dispatched: bool,
    provider_turn_completed: bool,
    acceptance_persistence_failed: bool,
}

impl TurnState {
    fn proposal_response(&self) -> &str {
        if self.native_proposal_text.is_empty() {
            &self.proposal_text
        } else {
            &self.native_proposal_text
        }
    }
}

/// Run the turn and report the outcome to a managed caller.
pub(crate) async fn run_turn(this: WeakEntity<Submit>, mut run: TurnRun, cx: &mut AsyncApp) {
    run.submit = Some(this.clone());
    let cx: &AsyncApp = cx;
    let mut state = TurnState {
        control_text: String::new(),
        message_parts: MessageParts::default(),
        control_error: Some("Turn did not complete".into()),
        proposal: run.proposal_draft.clone(),
        proposal_text: String::new(),
        native_proposal_text: String::new(),
        completed_proposal: None,
        provider_failure_seen: false,
        native_plan_seen: false,
        build_succeeded: false,
        pending_edited_events: Vec::new(),
        work_cwd: run.initial_work_cwd.clone(),
        wrap: None,
        plan_event_key: String::new(),
        prepared: Vec::new(),
        send_text: String::new(),
        transfer: None,
        transfer_switch_id: None,
        provider_accepted: false,
        provider_dispatched: false,
        provider_turn_completed: false,
        acceptance_persistence_failed: false,
    };
    let mut outcome = ControlOutcome::failed("Turn did not complete");
    match run.outer(&mut state, cx).await {
        Ok(Some(finished)) => outcome = finished,
        Ok(None) => {}
        Err(message) => {
            outcome = ControlOutcome {
                status: ControlStatus::Failed,
                text: state.control_text.clone(),
                error: Some(message.clone()),
            };
            if run.gen_current(cx) {
                cx.update(|cx| {
                    report(
                        &run.session_id,
                        HarnessEvent::SessionError {
                            message: message.clone(),
                        },
                        cx,
                    )
                });
                run.fail_session(&state, &message, cx);
            }
        }
    }
    let cancelled = !run.gen_current(cx);
    if !cancelled {
        let id = run.session_id.clone();
        let _ = this.update(&mut cx.clone(), |this, _| {
            this.running_selections.remove(&id)
        });
    }
    cx.update(|cx| {
        reject_edited(run.edited.as_deref(), &run.options, cx);
        let outcome = if cancelled {
            ControlOutcome {
                status: ControlStatus::Cancelled,
                text: state.control_text.clone(),
                error: None,
            }
        } else {
            outcome
        };
        settle(&run.options, outcome, cx);
    });
    if run.edited.is_some() {
        let id = run.session_id.clone();
        let _ = this.update(&mut cx.clone(), |this, _| this.edited_resends.finish(&id));
    }
}

impl TurnRun {
    fn harness(&self) -> HarnessId {
        self.current.harness
    }

    pub(super) fn gen_current(&self, cx: &AsyncApp) -> bool {
        cx.update(|cx| Engine::sessions(cx).read(cx).turn_gen(&self.session_id)) == self.generation
    }

    fn enqueue(&self, event: HarnessEvent, cx: &AsyncApp) {
        cx.update(|cx| {
            Engine::sessions(cx).update(cx, |sessions, cx| {
                sessions.enqueue_event(&self.session_id, event, cx)
            });
        });
    }

    pub(super) fn flush(&self, cx: &AsyncApp) {
        cx.update(|cx| Engine::sessions(cx).update(cx, |sessions, cx| sessions.flush(cx)));
    }

    pub(super) fn update_session(&self, cx: &AsyncApp, update: impl FnOnce(&mut Session)) {
        cx.update(|cx| {
            Engine::sessions(cx).update(cx, |sessions, cx| {
                sessions.update(&self.session_id, cx, update);
            });
        });
    }

    pub(super) fn latest(&self, cx: &AsyncApp) -> Option<Session> {
        cx.update(|cx| Engine::sessions(cx).read(cx).get(&self.session_id).cloned())
    }

    /// The outer `catch`: end the turn with the error on the transcript. A
    /// provider switch that did not reach the target goes back to a draft,
    /// and one that was dispatched waits for inspection.
    fn fail_session(&self, state: &TurnState, message: &str, cx: &AsyncApp) {
        let proposal = self.proposal_id.clone().zip(state.proposal.clone());
        let peers = self.peers.clone();
        let switching = self.pending_switch.is_some() && !state.provider_accepted;
        let switch_id = state.transfer_switch_id.clone();
        let dispatched = state.provider_dispatched;
        let unbuilt_plan = self
            .approved_plan
            .as_ref()
            .filter(|_| self.intent == TurnIntent::Build && !state.provider_accepted)
            .map(|plan| plan.id.clone());
        self.update_session(cx, |session| {
            stop_streaming_mut(session, now_ms());
            if switching {
                if is_preparing_handoff(session) {
                    *session = complete_handoff(session, TRANSFER_FAILED);
                }
                if let Some(switch_id) = &switch_id {
                    if dispatched {
                        recover_submitted_provider_delivery(session, switch_id);
                    } else {
                        fail_provider_delivery(session, switch_id, true);
                    }
                }
                if !dispatched
                    && let Some(user) = session
                        .blocks
                        .iter_mut()
                        .rev()
                        .find(|block| block.role == monocode_core::BlockRole::User)
                {
                    user.draft = Some(true);
                }
            }
            session.worktree_preparing = None;
            if let Some((id, draft)) = proposal {
                let failed = peers
                    .orchestration
                    .complete_proposal(&draft, "", Some(message));
                *session = with_orchestration_proposal(session, &id, &failed);
            }
            if let Some(plan_id) = &unbuilt_plan {
                with_plan_status(session, plan_id, PlanStatus::Ready);
            }
        });
    }

    /// Everything after the optimistic commit. `Err` is the outer `catch`;
    /// `Ok(Some(outcome))` is the turn's outcome when it ran to the end.
    async fn outer(
        &self,
        state: &mut TurnState,
        cx: &AsyncApp,
    ) -> Result<Option<ControlOutcome>, String> {
        let id = self.session_id.as_str();
        if self.create_draft_worktree {
            let branch = temporary_worktree_branch_name(&uuid::Uuid::new_v4().to_string());
            let base = self
                .current
                .worktree_base
                .clone()
                .filter(|base| !base.is_empty())
                .unwrap_or_else(|| "HEAD".into());
            let tree = cx
                .update(|cx| {
                    self.peers.projects.create_worktree(
                        &self.current.cwd,
                        &branch,
                        &base,
                        false,
                        cx,
                    )
                })
                .await?;
            state.work_cwd = tree.path.clone();
            if let Some(proposal) = &mut state.proposal {
                proposal.checkout_cwd = Some(tree.path.clone());
            }
            let (path, tree_branch) = (tree.path.clone(), tree.branch.clone());
            self.update_session(cx, |session| {
                session.worktree_cwd = Some(path);
                session.branch = tree_branch;
                session.workspace_mode = None;
                session.worktree_base = None;
                session.worktree_preparing = None;
            });
            cx.update(|cx| notify_review_changed(Some(id), cx));
            self.name_worktree_branch(&tree.path, cx);
        }
        self.launch_title_generation(&state.work_cwd, cx);
        if !self.gen_current(cx) {
            return Ok(None);
        }
        if let (Some(proposal_id), Some(proposal)) = (&self.proposal_id, &mut state.proposal) {
            let settings = cx
                .update(|cx| self.peers.orchestration.discover_settings(cx))
                .await?;
            if !self.gen_current(cx) {
                return Ok(None);
            }
            proposal.settings = settings;
            let discovering = proposal.clone();
            self.update_session(cx, |session| {
                *session = with_orchestration_proposal(session, proposal_id, &discovering);
            });
        }
        state.wrap = self
            .handoff_card
            .as_ref()
            .map(|card| Wrap {
                from: card.from,
                text: card.brief.clone(),
            })
            .or_else(|| self.queued_handoff.clone());
        self.prepare_request(state, cx).await?;
        if !self.gen_current(cx) {
            return Ok(None);
        }
        if self.pending_switch.is_some() && !self.raw_command {
            if self.prepare_transfer(state, cx).await?.is_none() {
                return Ok(None);
            }
            state.wrap = None;
        }

        state.plan_event_key =
            plan_turn_key(self.generation as i64, &uuid::Uuid::new_v4().to_string());
        // Workers get their checkpoint in `create_worker`, before any turn. A
        // turn must not create one later: it would count earlier worker edits
        // as the baseline and drop them from the accepted result.
        let in_orchestration = cx
            .update(|cx| self.peers.orchestration.run_status_for_session(id, cx))
            .is_some();
        if self.current.inbox_ask.is_none() && !in_orchestration {
            let _ = cx
                .update(|cx| begin_session_turn(id, &state.work_cwd, cx))
                .await;
        }
        if !self.gen_current(cx) {
            return Ok(None);
        }

        if let Err(message) = self.attempt(state, cx).await {
            self.recover_edited_resend(state, cx);
            if !self.gen_current(cx) {
                return Ok(None);
            }
            if state.acceptance_persistence_failed {
                // The error is already on the transcript, and the provider
                // finished its turn.
                state.control_error = Some(message);
                state.build_succeeded =
                    state.provider_turn_completed && !state.provider_failure_seen;
                return Ok(Some(self.finish(state, cx).await));
            }
            if let Some(wrap) = state.wrap.clone() {
                self.reveal_handoff(&wrap.text, cx);
            }
            let message = if message.is_empty() {
                format!("{} adapter failed", self.harness())
            } else {
                message
            };
            state.control_error = Some(message.clone());
            if !state.provider_failure_seen {
                self.enqueue(HarnessEvent::SessionError { message }, cx);
            }
            state.provider_failure_seen = true;
        }
        if !self.gen_current(cx) {
            return Ok(None);
        }
        Ok(Some(self.finish(state, cx).await))
    }

    /// `firstProviderRequest`: compose the exact first request before any
    /// history is exported or a provider process changes, so the capacity
    /// check sees the whole request.
    async fn prepare_request(&self, state: &mut TurnState, cx: &AsyncApp) -> Result<(), String> {
        let id = self.session_id.as_str();
        let harness = self.harness();
        state.prepared =
            prepare_attachments(self.config.attachment_io.as_ref(), &self.attachments).await;
        let prompt = match (&self.approved_plan, self.intent) {
            (Some(plan), TurnIntent::Build) => build_plan_prompt(&plan.text),
            _ => {
                prepare(
                    &self.harness_text,
                    harness,
                    id,
                    &state.work_cwd,
                    self.provider_account_id.as_deref(),
                    &self.config.skill_context,
                    &self.skills,
                    &self.peers,
                    cx,
                )
                .await
            }
        };
        let turn_prompt = match &state.proposal {
            Some(proposal) => match self.options.orchestration_retry.as_ref().filter(|retry| {
                retry
                    .response
                    .as_deref()
                    .is_some_and(|response| !response.is_empty())
            }) {
                Some(retry) => self
                    .peers
                    .orchestration
                    .repair_prompt(&OrchestrationProposal {
                        error: retry.error.clone(),
                        response: retry.response.clone(),
                        ..proposal.clone()
                    }),
                None => self.peers.orchestration.planning_prompt(
                    &prompt,
                    &proposal.settings,
                    proposal.checkout_cwd.as_deref().unwrap_or(&proposal.cwd),
                ),
            },
            None if self.intent == TurnIntent::Plan && !self.raw_command => {
                plan_turn_prompt(&prompt)
            }
            None => prompt,
        };
        let earlier = if self.queued_handoff.is_some() {
            user_messages_after_handoff(&self.current)
        } else {
            Vec::new()
        };
        let inbox_ask = (!self.raw_command)
            .then(|| self.current.inbox_ask.clone())
            .flatten();
        // A provider switch carries shared history instead of a recap.
        let wrap = state.wrap.clone().filter(|_| self.pending_switch.is_none());
        let body = match &wrap {
            Some(wrap) if !self.raw_command => {
                let request = js::trim(&turn_prompt);
                wrap_handoff_prompt(
                    &wrap.text,
                    wrap.from,
                    if request.is_empty() {
                        CONTINUE_PROMPT
                    } else {
                        request
                    },
                    &earlier,
                )
            }
            _ => turn_prompt,
        };
        let body = self.peers.inbox.ask_prompt(inbox_ask.as_ref(), body);
        let mut send_text = cx.update(|cx| self.peers.orchestration.prompt(id, body, cx));
        if self.operator_matched {
            let cli = format!("{} app", shell_path(&(self.config.app_cli_path)()?));
            send_text.push_str(&format!(
                "\n\n<monocode_app>\nThe user's Operator command enables app access in this thread, including later turns without the command. You can start session tabs or split session panes right or down, list and create project worktrees, choose a new session's checkout, read and continue other project sessions, save unsent drafts, organize session folders, and read or write saved notes through its local CLI. Run `{cli} --help` for exact commands and JSON fields, then use it as needed for the user's request. When reading another session, start with its latest two or three user/assistant exchanges. Request older exchanges with nextBefore or a larger excerpt only if needed. The CLI uses a session credential already in your environment; never print it. New sessions inherit this session's permission mode unless runtimeMode is set explicitly. For a new session with a draft, call sessions.start with its prompt and draft:true; do not submit a seed prompt. The returned ID can be used as besideSessionId to split its pane again or moved into a folder immediately. A normal sessions.start submits its prompt but returns after acceptance, so do not wait for that agent to finish before organizing it.\n</monocode_app>"
            ));
        }
        if let Some(note) = &self.app_note {
            let cli = format!("{} app", shell_path(&(self.config.app_cli_path)()?));
            send_text.push_str("\n\n");
            send_text.push_str(&note.replace("{cli}", &cli));
        }
        state.send_text = send_text;
        Ok(())
    }

    /// The inner `try`. `Err` carries the message the `catch` reports.
    async fn attempt(&self, state: &mut TurnState, cx: &AsyncApp) -> Result<(), String> {
        let id = self.session_id.as_str();
        let registry = self.config.registry.clone();
        let harness = self.harness();
        if let Some(edited) = &self.edited
            && registry.can_rewind_harness_last_turn(harness)
        {
            let (sink, _accepted, signals) = signal_channel();
            let rewind = registry.rewind_harness_last_turn(
                harness,
                RewindLastTurnInput {
                    session: self.session_input(&state.work_cwd),
                    provider_turn_id: self.edited_provider_turn_id.clone(),
                    text: None,
                    attachments: None,
                },
                sink,
            );
            let rewound = drive(rewind, &signals, |signal| {
                if let TurnSignal::Event(event) = signal
                    && self.gen_current(cx)
                {
                    self.enqueue(*event, cx);
                }
            })
            .await;
            if let Err(error) = rewound {
                self.flush(cx);
                cx.update(|cx| reject_edited(Some(edited), &self.options, cx));
                return Err(error.to_string());
            }
            edited.mark_provider_rewound();
            self.flush(cx);
            if !self.gen_current(cx) {
                let latest = self.latest(cx);
                let untouched = latest.as_ref().is_none_or(|latest| {
                    !latest.is_busy()
                        && latest.provider_session_id == self.current.provider_session_id
                });
                if untouched {
                    let _ = registry.forget_harness_session(harness, id).await;
                    if latest.is_some() {
                        let provider = self.current.provider_session_id.clone();
                        self.update_session(cx, |session| {
                            if session.provider_session_id == provider {
                                session.provider_session_id = None;
                            }
                        });
                    }
                }
                self.recover_edited_resend(state, cx);
                return Ok(());
            }
        }

        let send_text = state.send_text.clone();
        let prepared = std::mem::take(&mut state.prepared);
        let sent = match state.transfer_switch_id.clone() {
            Some(switch_id) => {
                // `dispatchAfterContextSave`: the submission marker is saved
                // before the request goes out, so a restart knows it may
                // have run.
                self.update_session(cx, |session| {
                    mark_provider_request_submitted(session, &switch_id)
                });
                let latest = self.latest(cx);
                let save = cx.update(|cx| {
                    save_provider_context_session(
                        latest,
                        "The provider submission marker could not be saved. The request was not sent.",
                        cx,
                    )
                });
                match save.await {
                    Err(error) => Err(error),
                    Ok(()) if !self.gen_current(cx) => Ok(()),
                    Ok(()) => {
                        state.provider_dispatched = true;
                        self.send_turn(send_text, prepared, state, cx).await
                    }
                }
            }
            None => self.send_turn(send_text, prepared, state, cx).await,
        };
        if sent.is_ok() {
            state.provider_turn_completed = true;
        }
        let acceptance = match state.transfer_switch_id.as_deref() {
            Some(switch_id) => self.acceptance_save(switch_id, cx),
            None => None,
        };
        let acceptance = match acceptance {
            Some(save) => save.await,
            None => Ok(()),
        };
        sent?;
        if acceptance.is_err() {
            state.acceptance_persistence_failed = true;
            return Err(ACCEPTANCE_SAVE_FAILED.into());
        }
        if !self.gen_current(cx) {
            return Ok(());
        }
        self.accept_edited_resend(state, cx);
        if let Some(draft) = state.proposal.clone()
            && !state.provider_failure_seen
        {
            // `completeOrRepairOrchestrationProposal`.
            let first =
                self.peers
                    .orchestration
                    .complete_proposal(&draft, state.proposal_response(), None);
            let can_repair =
                self.gen_current(cx) && !is_provider_failure_text(state.proposal_response());
            let completed = if first.status != OrchestrationProposalStatus::Invalid || !can_repair {
                first
            } else {
                state.proposal_text.clear();
                state.message_parts.clear();
                state.native_proposal_text.clear();
                let repair = self.peers.orchestration.repair_prompt(&first);
                self.send_turn(repair, Vec::new(), state, cx).await?;
                if state.provider_failure_seen {
                    return Err(state
                        .control_error
                        .clone()
                        .unwrap_or_else(|| "The lead could not repair the proposal.".into()));
                }
                self.peers
                    .orchestration
                    .complete_proposal(&draft, state.proposal_response(), None)
            };
            state.completed_proposal = Some(completed);
        }
        if !self.gen_current(cx) {
            return Ok(());
        }
        if let Some(wrap) = state.wrap.clone() {
            let raw = self.raw_command;
            self.update_session(cx, |session| {
                if is_preparing_handoff(session) {
                    *session = complete_handoff(session, &wrap.text);
                }
                // A command owns its arguments; deliver the recap with the next chat prompt.
                if !raw {
                    *session = consume_handoff(session);
                }
            });
        }
        state.build_succeeded = true;
        Ok(())
    }

    fn session_input(&self, work_cwd: &str) -> HarnessSessionInput {
        HarnessSessionInput {
            session_id: self.session_id.clone(),
            cwd: work_cwd.to_string(),
            model: self.current.model.clone(),
            model_settings: Some(self.current.model_settings.clone()),
            provider_account_id: self.provider_account_id.clone(),
            runtime_mode: self.current.runtime_mode,
            intent: None,
            controls_agents: None,
            app_access: None,
        }
    }

    /// `sendTurn`.
    async fn send_turn(
        &self,
        text: String,
        attachments: Vec<Attachment>,
        state: &mut TurnState,
        cx: &AsyncApp,
    ) -> Result<(), String> {
        let lead_active = cx
            .update(|cx| {
                self.peers
                    .orchestration
                    .led_run_status(&self.session_id, cx)
            })
            .is_some_and(|status| status == "active");
        let input = SendTurnInput {
            session: HarnessSessionInput {
                intent: Some(if self.intent == TurnIntent::Orchestrate {
                    TurnIntent::Plan
                } else {
                    self.intent
                }),
                // A /operator user turn enables app access for this thread;
                // orchestration leads retain their separate control access.
                // Linked sessions need the loopback socket too. The open
                // session actions alone keep the provider's network policy,
                // so a sandboxed Codex thread asks before it calls the CLI.
                controls_agents: Some(
                    self.operator_access || lead_active || !self.app_access.peers.is_empty(),
                ),
                app_access: Some(self.app_access.any()),
                ..self.session_input(&state.work_cwd)
            },
            text,
            attachments: Some(attachments),
        };
        let (sink, accepted, delivered, signals) = signal_channel_with_delivery();
        let transfer = state.transfer.clone().map(|transfer| ContextTransferInput {
            on_delivered: Some(delivered),
            ..transfer
        });
        let send = self.config.registry.send_harness_turn_with_context(
            self.harness(),
            input,
            transfer,
            sink,
            Some(accepted),
        );
        drive(send, &signals, |signal| match signal {
            TurnSignal::Accepted => self.on_provider_accepted(state, cx),
            TurnSignal::Event(event) => self.route_turn_event(*event, state, cx),
            TurnSignal::Delivered(receipt, reply) => {
                self.on_context_delivered(receipt, reply, state, cx)
            }
        })
        .await
        .map_err(|error| error.to_string())
    }

    /// `onAccepted`: the provider took the request. A provider switch is
    /// complete, and its acceptance is saved at once.
    fn on_provider_accepted(&self, state: &mut TurnState, cx: &AsyncApp) {
        if !self.gen_current(cx) || state.provider_accepted {
            return;
        }
        state.provider_accepted = true;
        self.accept_edited_resend(state, cx);
        let Some(switch_id) = state.transfer_switch_id.clone() else {
            return;
        };
        self.update_session(cx, |session| {
            accept_provider_delivery(session, &switch_id);
            *session = consume_handoff(session);
        });
        if let Some(submit) = &self.submit {
            let id = self.session_id.clone();
            let _ = submit.update(&mut cx.clone(), |submit, cx| {
                submit.start_acceptance_save(&id, &switch_id, cx)
            });
        }
        // Later repair prompts belong to this accepted native turn.
        state.transfer = None;
    }

    /// `onDelivered`: record which history reached the target. A native
    /// import is saved before the current request starts.
    fn on_context_delivered(
        &self,
        receipt: ContextTransferReceipt,
        reply: oneshot::Sender<Result<(), String>>,
        state: &mut TurnState,
        cx: &AsyncApp,
    ) {
        let Some(switch_id) = state.transfer_switch_id.clone() else {
            let _ = reply.send(Ok(()));
            return;
        };
        if !self.gen_current(cx) {
            let _ = reply.send(Ok(()));
            return;
        }
        self.flush(cx);
        let prefix = format!("{}:", self.session_id);
        let mode = match receipt.mode {
            DeliveryMode::Native => TransferMode::Native,
            DeliveryMode::Inline => TransferMode::Inline,
        };
        let coverage = DeliveryCoverage {
            included_block_ids: receipt.included_ids.map(|ids| {
                ids.into_iter()
                    .map(|id| id.strip_prefix(&prefix).map(str::to_string).unwrap_or(id))
                    .collect()
            }),
            omitted_block_ids: receipt.omitted_ids,
            source_through_block_id: receipt.through_block_id,
        };
        let bound = receipt.provider_session_id;
        self.update_session(cx, |session| {
            mark_provider_context_delivered(session, &switch_id, mode, bound.as_deref(), coverage)
        });
        if mode != TransferMode::Native {
            let _ = reply.send(Ok(()));
            return;
        }
        let latest = self.latest(cx);
        let save = cx.update(|cx| {
            save_provider_context_session(
                latest,
                "The native context import could not be saved.",
                cx,
            )
        });
        cx.spawn(async move |_| {
            let _ = reply.send(save.await);
        })
        .detach();
    }

    /// `acceptancePersistence.wait`: the open acceptance save for this
    /// switch.
    fn acceptance_save(
        &self,
        switch_id: &str,
        cx: &AsyncApp,
    ) -> Option<crate::submit::provider_switch::SharedSave> {
        let submit = self.submit.as_ref()?.upgrade()?;
        let id = self.session_id.clone();
        cx.update(|cx| submit.read(cx).acceptance.wait(&id, Some(switch_id)))
    }

    /// The picker revision now, to compare with the turn's.
    fn current_selection_revision(&self, cx: &AsyncApp) -> u64 {
        let id = self.session_id.clone();
        self.submit
            .as_ref()
            .and_then(|submit| submit.upgrade())
            .map(|submit| cx.update(|cx| submit.read(cx).selection_revision(&id)))
            .unwrap_or(self.selection_revision)
    }

    /// `routeTurnEvent`: drop events from a superseded turn, and hold an
    /// edited resend's events until the provider accepts it.
    fn route_turn_event(&self, event: HarnessEvent, state: &mut TurnState, cx: &AsyncApp) {
        if !self.gen_current(cx) {
            return;
        }
        if let Some(edited) = &self.edited
            && !edited.is_accepted()
        {
            state.pending_edited_events.push(event);
            return;
        }
        self.apply_turn_event(event, state, cx);
    }

    /// `applyTurnEvent`.
    fn apply_turn_event(&self, event: HarnessEvent, state: &mut TurnState, cx: &AsyncApp) {
        let harness = self.harness();
        let account = self.provider_account_id.clone();
        match &event {
            HarnessEvent::SessionProviderBound {
                provider_session_id,
            } => {
                // A startup identity is not proof that the target accepted.
                self.flush(cx);
                let work_cwd = state.work_cwd.clone();
                self.update_session(cx, |session| {
                    record_provider_bound(
                        session,
                        harness,
                        &work_cwd,
                        provider_session_id,
                        account.as_deref(),
                    )
                });
                return;
            }
            HarnessEvent::Context { used, window } => {
                self.flush(cx);
                let work_cwd = state.work_cwd.clone();
                self.update_session(cx, |session| {
                    record_provider_context_usage(
                        session,
                        harness,
                        &work_cwd,
                        *used,
                        *window,
                        account.as_deref(),
                    )
                });
            }
            _ => {}
        }
        if matches!(
            event,
            HarnessEvent::SessionConfigChanged { .. } | HarnessEvent::Context { .. }
        ) {
            // A picker change while the turn runs owns the selection now.
            let running = ModelTarget {
                harness,
                model: self.current.model.clone(),
                model_settings: self.current.model_settings.clone(),
            };
            let revisions = (self.selection_revision, self.current_selection_revision(cx));
            // Context reports arrive several times a turn, so read the
            // session in place instead of copying its transcript.
            let applies = cx.update(|cx| {
                Engine::sessions(cx)
                    .read(cx)
                    .get(&self.session_id)
                    .is_some_and(|selected| {
                        can_apply_running_configuration(selected, &running, Some(revisions))
                    })
            });
            if !applies {
                return;
            }
        }
        if let HarnessEvent::SessionConfigChanged {
            model,
            model_settings,
        } = &event
            && let Some(submit) = self.submit.as_ref().and_then(|submit| submit.upgrade())
        {
            let id = self.session_id.clone();
            let (model, model_settings) = (model.clone(), model_settings.clone());
            cx.update(|cx| {
                submit.update(cx, |submit, _| {
                    if let Some(running) = submit.running_selections.get_mut(&id) {
                        if let Some(model) = model {
                            running.model = model;
                        }
                        running
                            .model_settings
                            .extend(model_settings.unwrap_or_default());
                    }
                })
            });
        }
        cx.update(|cx| {
            self.peers
                .orchestration
                .observe(&self.session_id, &event, cx)
        });
        if self.options.on_settled.is_some() {
            match &event {
                HarnessEvent::MessageDelta { text, .. } => {
                    state.control_text.push_str(text);
                    keep_tail(&mut state.control_text, 20_000);
                }
                HarnessEvent::MessagePart {
                    part_id,
                    text,
                    reasoning: false,
                    ..
                } => {
                    state.control_text = state.message_parts.update(part_id, text);
                    keep_tail(&mut state.control_text, 20_000);
                }
                HarnessEvent::MessageCompleted => state.control_text.push('\n'),
                _ => {}
            }
        }
        if let HarnessEvent::SessionError { message } = &event {
            state.control_error = Some(message.clone());
        }
        if let Some(wrap) = state.wrap.clone()
            && matches!(event, HarnessEvent::SessionStarted)
        {
            self.reveal_handoff(&wrap.text, cx);
        }
        let work_cwd = state.work_cwd.clone();
        cx.update(|cx| {
            nudge_open_editors(&event, &work_cwd, cx);
            // Workers need their edits recorded so an accepted task can be
            // applied to the lead checkout. Only the lead is excluded.
            if self
                .peers
                .orchestration
                .led_run_status(&self.session_id, cx)
                .is_none()
            {
                track_session_edits(&self.session_id, &work_cwd, &event, cx);
            }
        });
        if let Some(routed) = self.route_plan_event(event, state) {
            self.enqueue(routed, cx);
        }
    }

    /// `routePlanEvent`: proposal text stays out of the transcript, and a
    /// plan turn's native plan gets this turn's key.
    fn route_plan_event(&self, event: HarnessEvent, state: &mut TurnState) -> Option<HarnessEvent> {
        if matches!(event, HarnessEvent::SessionError { .. }) {
            state.provider_failure_seen = true;
        }
        if state.proposal.is_some() {
            match &event {
                HarnessEvent::MessageDelta { text, .. } => {
                    state.proposal_text.push_str(text);
                    keep_tail(&mut state.proposal_text, 200_000);
                    return None;
                }
                HarnessEvent::MessagePart {
                    part_id,
                    text,
                    reasoning: false,
                    ..
                } => {
                    state.proposal_text = state.message_parts.update(part_id, text);
                    keep_tail(&mut state.proposal_text, 200_000);
                    return None;
                }
                HarnessEvent::MessageCompleted => {
                    state.proposal_text.push('\n');
                    return None;
                }
                HarnessEvent::Plan { text, append, .. } => {
                    if *append == Some(true) {
                        state.native_proposal_text.push_str(text);
                    } else {
                        state.native_proposal_text = text.clone();
                    }
                    return None;
                }
                _ => {}
            }
        }
        if self.intent != TurnIntent::Plan {
            return Some(event);
        }
        if let HarnessEvent::Plan {
            text,
            append,
            streaming,
            ..
        } = event
        {
            state.native_plan_seen = true;
            return Some(HarnessEvent::Plan {
                text,
                key: Some(state.plan_event_key.clone()),
                append,
                streaming,
            });
        }
        Some(event)
    }

    /// `acceptEditedResend`: the provider took the replacement, so show it
    /// and the events it already sent.
    fn accept_edited_resend(&self, state: &mut TurnState, cx: &AsyncApp) {
        let Some(edited) = &self.edited else {
            return;
        };
        if edited.is_accepted() {
            return;
        }
        cx.update(|cx| self.commit.apply(&self.config.catalog, cx));
        edited.mark_accepted();
        for event in std::mem::take(&mut state.pending_edited_events) {
            self.apply_turn_event(event, state, cx);
        }
    }

    /// `recoverEditedResend`.
    fn recover_edited_resend(&self, state: &mut TurnState, cx: &AsyncApp) {
        let Some(edited) = &self.edited else {
            return;
        };
        if edited.is_accepted() {
            return;
        }
        state.pending_edited_events.clear();
        if let Some(previous) = self.latest(cx) {
            let recovered = edited.recover_after_failure(&previous);
            self.update_session(cx, |session| *session = recovered);
        }
    }

    /// `revealHandoff`.
    fn reveal_handoff(&self, brief: &str, cx: &AsyncApp) {
        self.update_session(cx, |session| {
            if is_preparing_handoff(session) {
                *session = complete_handoff(session, brief);
                session.busy = Some(true);
            }
        });
    }

    /// The inner `finally`: settle the transcript and say how the turn ended.
    async fn finish(&self, state: &mut TurnState, cx: &AsyncApp) -> ControlOutcome {
        let id = self.session_id.as_str();
        self.flush(cx);
        let failed = state.provider_failure_seen
            || state.acceptance_persistence_failed
            || is_provider_failure_text(&state.control_text)
            || !state.build_succeeded;
        let outcome = ControlOutcome {
            status: if failed {
                ControlStatus::Failed
            } else {
                ControlStatus::Completed
            },
            text: js::trim(&state.control_text).to_string(),
            error: if state.provider_failure_seen || state.acceptance_persistence_failed {
                state.control_error.clone()
            } else {
                None
            },
        };
        // A failed provider can leave its process alive with a dead event
        // stream or poisoned turn state. Park it now; the next prompt will
        // reconnect and resume through a fresh transport.
        if state.provider_failure_seen {
            // A switch that never reached the target leaves no conversation
            // worth resuming.
            let unsent = state.transfer_switch_id.is_some()
                && !state.provider_accepted
                && !state.provider_dispatched;
            let registry = &self.config.registry;
            let _ = if unsent {
                registry.forget_harness_session(self.harness(), id).await
            } else {
                registry.stop_harness_session(self.harness(), id).await
            };
        }
        let flush = cx.update(|cx| Engine::checkpoints(cx).flush_session_checkpoint(id));
        flush.await;

        let provider_failure_seen = state.provider_failure_seen;
        let acceptance_failed = state.acceptance_persistence_failed;
        let build_succeeded = state.build_succeeded;
        let transfer = state.transfer_switch_id.clone();
        let accepted = state.provider_accepted;
        let dispatched = state.provider_dispatched;
        let harness = self.harness();
        let account = self.provider_account_id.clone();
        let settle_cwd = state.work_cwd.clone();
        let response = state.proposal_response().to_string();
        let proposal = self.proposal_id.clone().zip(state.proposal.clone());
        let completed = state.completed_proposal.clone();
        let error = outcome.error.clone();
        let native_plan_seen = state.native_plan_seen;
        let plan_key = state.plan_event_key.clone();
        let intent = self.intent;
        let approved = self.approved_plan.as_ref().map(|plan| plan.id.clone());
        let peers = self.peers.clone();
        self.update_session(cx, |session| {
            stop_streaming_mut(session, now_ms());
            match &transfer {
                Some(switch_id) if !accepted => {
                    if dispatched {
                        recover_submitted_provider_delivery(session, switch_id);
                    } else {
                        fail_provider_delivery(session, switch_id, true);
                    }
                }
                _ if accepted => {
                    settle_provider_binding(session, harness, &settle_cwd, account.as_deref())
                }
                _ => {}
            }
            let provider_failed = provider_failure_seen
                || is_provider_failure_text(&super::session_edits::last_assistant_text_in_turn(
                    session,
                ));
            if let Some((proposal_id, draft)) = proposal {
                let finalized = match completed {
                    Some(completed) if !provider_failed && build_succeeded => completed,
                    _ => {
                        let error = (provider_failed || !build_succeeded || acceptance_failed)
                            .then(|| {
                                error
                                    .unwrap_or_else(|| "The lead could not finish planning.".into())
                            });
                        peers
                            .orchestration
                            .complete_proposal(&draft, &response, error.as_deref())
                    }
                };
                *session = with_orchestration_proposal(session, &proposal_id, &finalized);
            } else if intent == TurnIntent::Plan && !native_plan_seen && !provider_failed {
                promote_last_assistant_to_plan_mut(session, Some(&plan_key));
            }
            if let Some(plan_id) = &approved
                && intent == TurnIntent::Build
            {
                let status = if build_succeeded && !provider_failed {
                    PlanStatus::Built
                } else {
                    PlanStatus::Ready
                };
                with_plan_status(session, plan_id, status);
            }
        });
        let work_cwd = state.work_cwd.clone();
        cx.update(|cx| {
            // Next tick: the notifier waits until the flush above has applied,
            // so the banner quotes the reply's final text.
            self.peers.attention.announce_finished_later(id, cx);
            notify_review_changed(Some(id), cx);
            nudge_after_turn(Some(&work_cwd), cx);
        });
        outcome
    }

    /// The title request for a first turn (`launchTitleGeneration`).
    fn launch_title_generation(&self, work_cwd: &str, cx: &AsyncApp) {
        if !self.config.registry.is_live_harness(self.harness()) || !self.generate_title {
            return;
        }
        let message = if self.harness_text.is_empty() {
            self.attachments
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            self.harness_text.clone()
        };
        let registry = self.config.registry.clone();
        let harness = self.harness();
        let input = TitleInput {
            session_id: self.session_id.clone(),
            cwd: work_cwd.to_string(),
            message: message.clone(),
            provider_account_id: self.provider_account_id.clone(),
        };
        let refresh = self.options.refresh_title;
        let is_first_turn = self.is_first_turn;
        let generation = self.generation;
        let session_id = self.session_id.clone();
        let title_seed = self.title_seed.clone();
        let peers = self.peers.clone();
        let work_cwd = work_cwd.to_string();
        cx.spawn(async move |cx| {
            let Ok(generated) = registry.generate_harness_title(harness, input).await else {
                return;
            };
            if refresh
                && !is_first_turn
                && cx.update(|cx| Engine::sessions(cx).read(cx).turn_gen(&session_id)) != generation
            {
                return;
            }
            let hint = generated.as_ref().and_then(|generated| generated.work_item);
            let linked = cx
                .update(|cx| {
                    peers
                        .inbox
                        .resolve_linked_work_item(&message, &work_cwd, hint, cx)
                })
                .await;
            if generated.is_none() && linked.is_none() {
                return;
            }
            cx.update(|cx| {
                Engine::sessions(cx).update(cx, |sessions, cx| {
                    sessions.update(&session_id, cx, |session| {
                        if let Some(generated) = &generated
                            && (refresh
                                || monocode_core::session::can_replace_session_title(
                                    &session.title,
                                    session.harness,
                                    &title_seed,
                                ))
                        {
                            session.title = monocode_core::session::format_session_title(
                                session.harness,
                                &generated.title,
                            );
                        }
                        if let Some(linked) = linked
                            && session.linked_work_item.is_none()
                        {
                            session.linked_work_item = Some(linked);
                        }
                    });
                });
            });
        })
        .detach();
    }

    /// Name a new draft worktree's branch from the message.
    fn name_worktree_branch(&self, tree_path: &str, cx: &AsyncApp) {
        let message = if self.harness_text.is_empty() {
            self.attachments
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            self.harness_text.clone()
        };
        let registry = self.config.registry.clone();
        let available = self.config.is_harness_available.clone();
        let text_harness = pick_text_harness(Some(self.harness()), |id| available(id));
        let cwd = self.current.cwd.clone();
        let tree_path = tree_path.to_string();
        let session_id = self.session_id.clone();
        let peers = self.peers.clone();
        // The session's account names its branch when its own harness runs
        // the helper. Another harness's account means nothing there.
        let account = self
            .provider_account_id
            .clone()
            .filter(|_| text_harness == self.harness());
        cx.spawn(async move |cx| {
            let Ok(Some(fragment)) = registry
                .generate_harness_branch_name(
                    text_harness,
                    &tree_path,
                    &message,
                    account.as_deref(),
                )
                .await
            else {
                return;
            };
            let Some(branch) = named_worktree_branch(&fragment) else {
                return;
            };
            let rename = cx.update(|cx| {
                peers
                    .projects
                    .rename_worktree_branch(&cwd, &tree_path, &branch, cx)
            });
            let Ok(renamed) = rename.await else {
                return;
            };
            cx.update(|cx| {
                Engine::sessions(cx).update(cx, |sessions, cx| {
                    sessions.update(&session_id, cx, |session| {
                        if path_key(session_work_cwd(session)) == path_key(&tree_path) {
                            session.branch = renamed.branch.clone();
                        }
                    });
                });
            });
        })
        .detach();
    }
}
